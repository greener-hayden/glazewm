//! Completion tickets and request tracking for native work that blocks on
//! another process.
//!
//! Cloaking, taskbar membership and style changes each wait on explorer or
//! on the window's own application. The window manager thread must never
//! wait for them, so it queues a request, keeps going, and learns the
//! outcome from a [`ShellTicket`] on a later pass. A [`ShellSlot`] is that
//! bookkeeping for one kind of request on one window.
//!
//! Nothing here touches the platform, so all of it is testable natively.

use std::{
  sync::{
    atomic::{AtomicU8, Ordering},
    Arc, Mutex, PoisonError,
  },
  time::{Duration, Instant},
};

const PENDING: u8 = 0;
const RUNNING: u8 = 1;
const OK: u8 = 2;
const ERR: u8 = 3;
const CANCELLED: u8 = 4;

/// How long a failed request is left alone before it is issued again.
pub const SHELL_RETRY: Duration = Duration::from_millis(250);

/// How long a request the shell accepted may go unobserved before it is
/// issued again.
///
/// The observation normally agrees at once. Waiting only guards against a
/// shell that reported success without acting.
const REOBSERVE: Duration = Duration::from_millis(100);

/// How many times an accepted but unobserved request is issued again
/// before the shell is taken at its word.
const MAX_REISSUES: u32 = 3;

/// Where a [`ShellTicket`]'s work stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TicketState {
  /// Queued behind earlier work.
  Pending,
  /// A worker is executing it.
  Running,
  /// The worker finished it successfully.
  Ok,
  /// The worker finished it with an error.
  Err,
  /// Withdrawn before a worker started it.
  Cancelled,
}

/// Shared completion state of one queued native request.
///
/// The requester keeps one clone and the worker the other. Cancelling
/// succeeds only while the work is still queued, so a request that a
/// worker has started always runs to its end.
#[derive(Clone, Debug)]
pub struct ShellTicket(Arc<AtomicU8>);

impl ShellTicket {
  /// Creates a ticket for work that has not started.
  #[must_use]
  pub fn new() -> Self {
    Self(Arc::new(AtomicU8::new(PENDING)))
  }

  /// Reads where the work stands.
  #[must_use]
  pub fn state(&self) -> TicketState {
    match self.0.load(Ordering::Acquire) {
      PENDING => TicketState::Pending,
      RUNNING => TicketState::Running,
      OK => TicketState::Ok,
      ERR => TicketState::Err,
      _ => TicketState::Cancelled,
    }
  }

  /// Withdraws work no worker has started.
  ///
  /// Returns whether the work was withdrawn.
  #[must_use]
  pub fn cancel(&self) -> bool {
    self
      .0
      .compare_exchange(
        PENDING,
        CANCELLED,
        Ordering::AcqRel,
        Ordering::Acquire,
      )
      .is_ok()
  }

  /// Claims the work for a worker.
  ///
  /// Returns `false` for work that was cancelled, which the worker skips.
  #[must_use]
  pub fn claim(&self) -> bool {
    self
      .0
      .compare_exchange(
        PENDING,
        RUNNING,
        Ordering::AcqRel,
        Ordering::Acquire,
      )
      .is_ok()
  }

  /// Records a worker's outcome.
  pub fn complete(&self, success: bool) {
    self
      .0
      .store(if success { OK } else { ERR }, Ordering::Release);
  }
}

impl Default for ShellTicket {
  fn default() -> Self {
    Self::new()
  }
}

/// Where an opacity change stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpacityProgress {
  /// The window shows the requested opacity.
  Settled,
  /// A style change is still queued or running; ask again on a later
  /// pass.
  Pending,
}

/// What one look at a [`ShellSlot`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellPoll {
  /// The window already is as desired, and no request was outstanding.
  Settled,
  /// An outstanding request has taken effect.
  Applied,
  /// A request is queued, running, or awaiting observation.
  InFlight,
  /// The outstanding request failed, which this look reports once.
  Failed,
  /// A recent failure is being waited out before another request.
  Backoff,
}

/// One outstanding request.
#[derive(Debug)]
struct Flight<T> {
  desired: T,
  ticket: ShellTicket,
  /// When the worker was first seen done without the state following.
  accepted_at: Option<Instant>,
}

/// Tracks one kind of asynchronous request for one window.
///
/// The slot holds at most one request. A newer desire withdraws an older
/// request that has not started. One that a worker has already started
/// cannot be withdrawn, so the slot keeps tracking it until it ends and
/// only then looks at the window again: its value may or may not have
/// landed, and only an observation made afterwards can say.
#[derive(Debug)]
pub struct ShellSlot<T> {
  flight: Option<Flight<T>>,
  failed: Option<(T, Instant)>,
  /// Consecutive failures, which stretch the pause before a retry.
  failures: u32,
  /// Times an accepted request went unobserved and was issued again.
  reissues: u32,
}

impl<T> Default for ShellSlot<T> {
  fn default() -> Self {
    Self {
      flight: None,
      failed: None,
      failures: 0,
      reissues: 0,
    }
  }
}

impl<T: Copy + PartialEq> ShellSlot<T> {
  /// Whether a request is outstanding.
  #[must_use]
  pub fn in_flight(&self) -> bool {
    self.flight.is_some()
  }

  /// How many requests in a row have failed.
  ///
  /// Callers can log the first quietly or loudly and the rest at a lower
  /// level, since each retry waits longer than the last.
  #[must_use]
  pub fn failures(&self) -> u32 {
    self.failures
  }

  /// How long to leave the window alone after the latest failure.
  ///
  /// Doubles with each consecutive failure, from `SHELL_RETRY` up to
  /// sixteen times that.
  fn backoff(&self) -> Duration {
    SHELL_RETRY * (1 << self.failures.saturating_sub(1).min(4))
  }

  /// Drives the window toward `desired`.
  ///
  /// `observe` reads the window's actual value where the platform can be
  /// asked, and gives `None` where only the worker's report is available.
  /// It is called at most once, and only when the answer can be trusted:
  /// never while a contrary request is still running, and always after
  /// the ticket state it is judged against has been read.
  ///
  /// When nothing is outstanding and the value differs, `submit` queues
  /// the work. It returns `None` for a desire that needs no work.
  ///
  /// A request the worker finished successfully but the observation does
  /// not yet show is waited on briefly, then issued again, up to
  /// `MAX_REISSUES` times; after that it is taken as applied and left
  /// alone for a pause.
  ///
  /// `Applied` means a request for `desired` took effect, or one for the
  /// opposite was withdrawn before it ran. A request for the opposite
  /// that did run is never reported as applied, even when the window now
  /// matches, because its record is not this desire's to settle.
  pub fn converge(
    &mut self,
    desired: T,
    observe: impl FnOnce() -> crate::Result<Option<T>>,
    now: Instant,
    submit: impl FnOnce() -> crate::Result<Option<ShellTicket>>,
  ) -> crate::Result<ShellPoll> {
    let mut withdrawn = false;
    if let Some(flight) = self
      .flight
      .as_ref()
      .filter(|flight| flight.desired != desired)
    {
      let started = !flight.ticket.cancel();
      if started
        && matches!(
          flight.ticket.state(),
          TicketState::Pending | TicketState::Running
        )
      {
        // It will land its own value, so nothing seen now says whether
        // this desire holds.
        return Ok(ShellPoll::InFlight);
      }
      withdrawn = !started;
      self.flight = None;
      self.reissues = 0;
    }

    let observed = observe()?;
    if observed == Some(desired) {
      self.failed = None;
      self.failures = 0;
      self.reissues = 0;
      return Ok(match self.flight.take() {
        Some(flight) => {
          let _ = flight.ticket.cancel();
          ShellPoll::Applied
        }
        None if withdrawn => ShellPoll::Applied,
        None => ShellPoll::Settled,
      });
    }

    if let Some(flight) = &mut self.flight {
      match flight.ticket.state() {
        TicketState::Pending | TicketState::Running => {
          return Ok(ShellPoll::InFlight);
        }
        TicketState::Ok if observed.is_none() => {
          self.flight = None;
          self.failures = 0;
          return Ok(ShellPoll::Applied);
        }
        TicketState::Ok => {
          let accepted = *flight.accepted_at.get_or_insert(now);
          if now.saturating_duration_since(accepted) < REOBSERVE {
            return Ok(ShellPoll::InFlight);
          }
          self.flight = None;
          if self.reissues >= MAX_REISSUES {
            // The shell keeps saying yes and the window keeps saying no.
            // Stop asking for a while rather than hammer it.
            self.reissues = 0;
            self.failures += 1;
            self.failed = Some((desired, now));
            return Ok(ShellPoll::Applied);
          }
          self.reissues += 1;
        }
        TicketState::Err => {
          self.failures += 1;
          self.failed = Some((desired, now));
          self.flight = None;
          return Ok(ShellPoll::Failed);
        }
        TicketState::Cancelled => self.flight = None,
      }
    }

    if let Some((failed, at)) = self.failed {
      if failed == desired
        && now.saturating_duration_since(at) < self.backoff()
      {
        return Ok(ShellPoll::Backoff);
      }
      self.failed = None;
    }
    Ok(match submit()? {
      None => ShellPoll::Settled,
      Some(ticket) => {
        self.flight = Some(Flight {
          desired,
          ticket,
          accepted_at: None,
        });
        ShellPoll::InFlight
      }
    })
  }
}

/// Wakes the window manager when native work completes.
static WAKE: Mutex<Option<Box<dyn Fn() + Send>>> = Mutex::new(None);

/// Registers the callback workers run after finishing a request.
///
/// A completion only matters once a pass sees it, so the callback should
/// do no more than ask for one. It runs on a worker thread, and replaces
/// any earlier callback.
pub fn set_native_wake(wake: impl Fn() + Send + 'static) {
  *WAKE.lock().unwrap_or_else(PoisonError::into_inner) =
    Some(Box::new(wake));
}

/// Runs the registered completion callback, if there is one.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn notify_native_wake() {
  if let Some(wake) =
    WAKE.lock().unwrap_or_else(PoisonError::into_inner).as_ref()
  {
    wake();
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Submits a fresh ticket and keeps a handle to it.
  fn submit(
    tickets: &mut Vec<ShellTicket>,
  ) -> impl FnOnce() -> crate::Result<Option<ShellTicket>> + '_ {
    move || {
      let ticket = ShellTicket::new();
      tickets.push(ticket.clone());
      Ok(Some(ticket))
    }
  }

  /// A submission that must not happen.
  fn forbidden() -> crate::Result<Option<ShellTicket>> {
    panic!("No request was expected.");
  }

  /// An observation of `value`.
  fn seen(
    value: Option<bool>,
  ) -> impl FnOnce() -> crate::Result<Option<bool>> {
    move || Ok(value)
  }

  /// An observation that must not be made.
  fn untrusted() -> crate::Result<Option<bool>> {
    panic!("The window was observed while a contrary call was running.");
  }

  #[test]
  fn ticket_runs_once_and_cannot_be_cancelled_after_it_starts() {
    let ticket = ShellTicket::new();
    assert_eq!(ticket.state(), TicketState::Pending);
    assert!(ticket.claim());
    assert!(!ticket.claim());
    assert!(!ticket.cancel());
    ticket.complete(true);
    assert_eq!(ticket.state(), TicketState::Ok);
  }

  #[test]
  fn cancelled_ticket_is_skipped_by_the_worker() {
    let ticket = ShellTicket::new();
    assert!(ticket.cancel());
    assert!(!ticket.claim());
    assert_eq!(ticket.state(), TicketState::Cancelled);
  }

  #[test]
  fn matching_observation_issues_nothing() {
    let mut slot = ShellSlot::<bool>::default();
    let poll =
      slot.converge(true, seen(Some(true)), Instant::now(), forbidden);
    assert_eq!(poll.unwrap(), ShellPoll::Settled);
    assert!(!slot.in_flight());
  }

  #[test]
  fn a_request_is_issued_once_and_applied_when_observed() {
    let now = Instant::now();
    let mut tickets = Vec::new();
    let mut slot = ShellSlot::<bool>::default();
    let poll =
      slot.converge(true, seen(Some(false)), now, submit(&mut tickets));
    assert_eq!(poll.unwrap(), ShellPoll::InFlight);
    // Asking again while it is queued issues nothing.
    let poll = slot.converge(true, seen(Some(false)), now, forbidden);
    assert_eq!(poll.unwrap(), ShellPoll::InFlight);
    assert!(tickets[0].claim());
    let poll = slot.converge(true, seen(Some(false)), now, forbidden);
    assert_eq!(poll.unwrap(), ShellPoll::InFlight);
    tickets[0].complete(true);
    let poll = slot.converge(true, seen(Some(true)), now, forbidden);
    assert_eq!(poll.unwrap(), ShellPoll::Applied);
    assert!(!slot.in_flight());
  }

  #[test]
  fn a_newer_desire_withdraws_the_queued_request() {
    let now = Instant::now();
    let mut tickets = Vec::new();
    let mut slot = ShellSlot::<bool>::default();
    slot
      .converge(true, seen(Some(false)), now, submit(&mut tickets))
      .unwrap();
    let poll =
      slot.converge(false, seen(Some(true)), now, submit(&mut tickets));
    assert_eq!(poll.unwrap(), ShellPoll::InFlight);
    assert_eq!(tickets[0].state(), TicketState::Cancelled);
    assert_eq!(tickets[1].state(), TicketState::Pending);
  }

  /// A request withdrawn before it ran leaves the window as it was, so a
  /// window that already matches the newer desire is settled by it.
  #[test]
  fn withdrawing_an_unstarted_contrary_request_applies_the_desire() {
    let now = Instant::now();
    let mut tickets = Vec::new();
    let mut slot = ShellSlot::<bool>::default();
    slot
      .converge(true, seen(Some(false)), now, submit(&mut tickets))
      .unwrap();
    let poll = slot.converge(false, seen(Some(false)), now, forbidden);
    assert_eq!(poll.unwrap(), ShellPoll::Applied);
    assert_eq!(tickets[0].state(), TicketState::Cancelled);
    assert!(!slot.in_flight());
  }

  /// The call has already been claimed: it will land its value whatever
  /// the window shows now, so the slot must keep tracking it, and must not
  /// report the newer desire as applied.
  #[test]
  fn a_running_contrary_request_is_tracked_until_it_ends() {
    let now = Instant::now();
    let mut tickets = Vec::new();
    let mut slot = ShellSlot::<bool>::default();
    slot
      .converge(true, seen(Some(false)), now, submit(&mut tickets))
      .unwrap();
    assert!(tickets[0].claim());
    // The window still reads uncloaked, which is the newer desire, but
    // the running cloak has not landed yet.
    let poll = slot.converge(false, untrusted, now, forbidden);
    assert_eq!(poll.unwrap(), ShellPoll::InFlight);
    assert!(slot.in_flight());
    assert_eq!(tickets[0].state(), TicketState::Running);
  }

  /// Running contrary call, window reads as desired, the call completes,
  /// then the window flips: the reverse must be issued.
  #[test]
  fn a_contrary_request_that_lands_is_reversed() {
    let now = Instant::now();
    let mut tickets = Vec::new();
    let mut slot = ShellSlot::<bool>::default();
    slot
      .converge(true, seen(Some(false)), now, submit(&mut tickets))
      .unwrap();
    assert!(tickets[0].claim());
    let poll = slot.converge(false, untrusted, now, forbidden);
    assert_eq!(poll.unwrap(), ShellPoll::InFlight);

    tickets[0].complete(true);
    // The cloak landed: the window now reads cloaked, not as desired.
    let poll =
      slot.converge(false, seen(Some(true)), now, submit(&mut tickets));
    assert_eq!(poll.unwrap(), ShellPoll::InFlight);
    assert_eq!(tickets.len(), 2);
    assert!(tickets[1].claim());
    tickets[1].complete(true);
    let poll = slot.converge(false, seen(Some(false)), now, forbidden);
    assert_eq!(poll.unwrap(), ShellPoll::Applied);
  }

  /// A contrary call that ran but whose value did not stick is never
  /// reported as applied: its record is not this desire's to settle.
  #[test]
  fn a_contrary_request_that_ran_is_never_reported_applied() {
    let now = Instant::now();
    let mut tickets = Vec::new();
    let mut slot = ShellSlot::<bool>::default();
    slot
      .converge(true, seen(Some(false)), now, submit(&mut tickets))
      .unwrap();
    assert!(tickets[0].claim());
    tickets[0].complete(true);
    let poll = slot.converge(false, seen(Some(false)), now, forbidden);
    assert_eq!(poll.unwrap(), ShellPoll::Settled);
    assert!(!slot.in_flight());
  }

  #[test]
  fn a_contrary_request_that_failed_lets_the_desire_through() {
    let now = Instant::now();
    let mut tickets = Vec::new();
    let mut slot = ShellSlot::<bool>::default();
    slot
      .converge(true, seen(Some(false)), now, submit(&mut tickets))
      .unwrap();
    assert!(tickets[0].claim());
    tickets[0].complete(false);
    let poll =
      slot.converge(false, seen(Some(true)), now, submit(&mut tickets));
    assert_eq!(poll.unwrap(), ShellPoll::InFlight);
    assert_eq!(tickets.len(), 2);
  }

  #[test]
  fn failure_is_reported_once_then_backed_off_then_retried() {
    let now = Instant::now();
    let mut tickets = Vec::new();
    let mut slot = ShellSlot::<bool>::default();
    slot
      .converge(true, seen(Some(false)), now, submit(&mut tickets))
      .unwrap();
    assert!(tickets[0].claim());
    tickets[0].complete(false);
    let poll = slot.converge(true, seen(Some(false)), now, forbidden);
    assert_eq!(poll.unwrap(), ShellPoll::Failed);
    assert_eq!(slot.failures(), 1);
    let soon = now + SHELL_RETRY / 2;
    let poll = slot.converge(true, seen(Some(false)), soon, forbidden);
    assert_eq!(poll.unwrap(), ShellPoll::Backoff);
    let later = now + SHELL_RETRY;
    let poll =
      slot.converge(true, seen(Some(false)), later, submit(&mut tickets));
    assert_eq!(poll.unwrap(), ShellPoll::InFlight);
    assert_eq!(tickets.len(), 2);
  }

  #[test]
  fn each_consecutive_failure_waits_longer_and_success_resets_it() {
    let mut now = Instant::now();
    let mut tickets = Vec::new();
    let mut slot = ShellSlot::<bool>::default();
    let mut pause = SHELL_RETRY;
    for failure in 1..=3 {
      let poll =
        slot.converge(true, seen(Some(false)), now, submit(&mut tickets));
      assert_eq!(poll.unwrap(), ShellPoll::InFlight);
      let ticket = tickets.last().unwrap().clone();
      assert!(ticket.claim());
      ticket.complete(false);
      let poll = slot.converge(true, seen(Some(false)), now, forbidden);
      assert_eq!(poll.unwrap(), ShellPoll::Failed);
      assert_eq!(slot.failures(), failure);
      // Just short of the pause it is still waited out.
      let almost = now + pause / 2;
      let poll = slot.converge(true, seen(Some(false)), almost, forbidden);
      assert_eq!(poll.unwrap(), ShellPoll::Backoff);
      now += pause;
      pause *= 2;
    }
    let poll =
      slot.converge(true, seen(Some(false)), now, submit(&mut tickets));
    assert_eq!(poll.unwrap(), ShellPoll::InFlight);
    let poll = slot.converge(true, seen(Some(true)), now, forbidden);
    assert_eq!(poll.unwrap(), ShellPoll::Applied);
    assert_eq!(slot.failures(), 0);
  }

  #[test]
  fn backoff_does_not_outlast_a_different_desire() {
    let now = Instant::now();
    let mut tickets = Vec::new();
    let mut slot = ShellSlot::<bool>::default();
    slot
      .converge(true, seen(Some(false)), now, submit(&mut tickets))
      .unwrap();
    assert!(tickets[0].claim());
    tickets[0].complete(false);
    slot
      .converge(true, seen(Some(false)), now, forbidden)
      .unwrap();
    let poll =
      slot.converge(false, seen(Some(true)), now, submit(&mut tickets));
    assert_eq!(poll.unwrap(), ShellPoll::InFlight);
  }

  #[test]
  fn an_accepted_but_unobserved_request_is_reissued_after_a_grace() {
    let now = Instant::now();
    let mut tickets = Vec::new();
    let mut slot = ShellSlot::<bool>::default();
    slot
      .converge(true, seen(Some(false)), now, submit(&mut tickets))
      .unwrap();
    assert!(tickets[0].claim());
    tickets[0].complete(true);
    let poll = slot.converge(true, seen(Some(false)), now, forbidden);
    assert_eq!(poll.unwrap(), ShellPoll::InFlight);
    let poll = slot.converge(
      true,
      seen(Some(false)),
      now + REOBSERVE,
      submit(&mut tickets),
    );
    assert_eq!(poll.unwrap(), ShellPoll::InFlight);
    assert_eq!(tickets.len(), 2);
  }

  /// A shell that keeps accepting what the window never shows is asked a
  /// bounded number of times, then taken at its word and left alone.
  #[test]
  fn unobserved_reissues_are_capped() {
    let mut now = Instant::now();
    let mut tickets = Vec::new();
    let mut slot = ShellSlot::<bool>::default();
    slot
      .converge(true, seen(Some(false)), now, submit(&mut tickets))
      .unwrap();
    for _ in 0..MAX_REISSUES {
      let ticket = tickets.last().unwrap().clone();
      assert!(ticket.claim());
      ticket.complete(true);
      slot
        .converge(true, seen(Some(false)), now, forbidden)
        .unwrap();
      now += REOBSERVE;
      let poll =
        slot.converge(true, seen(Some(false)), now, submit(&mut tickets));
      assert_eq!(poll.unwrap(), ShellPoll::InFlight);
    }
    assert_eq!(tickets.len(), 1 + MAX_REISSUES as usize);
    let ticket = tickets.last().unwrap().clone();
    assert!(ticket.claim());
    ticket.complete(true);
    slot
      .converge(true, seen(Some(false)), now, forbidden)
      .unwrap();
    now += REOBSERVE;
    let poll = slot.converge(true, seen(Some(false)), now, forbidden);
    assert_eq!(poll.unwrap(), ShellPoll::Applied);
    assert!(!slot.in_flight());
    // It then pauses instead of asking again at once.
    let poll = slot.converge(true, seen(Some(false)), now, forbidden);
    assert_eq!(poll.unwrap(), ShellPoll::Backoff);
  }

  #[test]
  fn an_unobservable_request_is_applied_on_the_workers_report() {
    let now = Instant::now();
    let mut tickets = Vec::new();
    let mut slot = ShellSlot::<bool>::default();
    slot
      .converge(true, seen(None), now, submit(&mut tickets))
      .unwrap();
    let poll = slot.converge(true, seen(None), now, forbidden);
    assert_eq!(poll.unwrap(), ShellPoll::InFlight);
    assert!(tickets[0].claim());
    tickets[0].complete(true);
    let poll = slot.converge(true, seen(None), now, forbidden);
    assert_eq!(poll.unwrap(), ShellPoll::Applied);
    assert!(!slot.in_flight());
  }

  #[test]
  fn an_unobservable_contrary_request_is_followed_by_the_reverse() {
    let now = Instant::now();
    let mut tickets = Vec::new();
    let mut slot = ShellSlot::<bool>::default();
    slot
      .converge(true, seen(None), now, submit(&mut tickets))
      .unwrap();
    assert!(tickets[0].claim());
    let poll = slot.converge(false, untrusted, now, forbidden);
    assert_eq!(poll.unwrap(), ShellPoll::InFlight);
    tickets[0].complete(true);
    let poll = slot.converge(false, seen(None), now, submit(&mut tickets));
    assert_eq!(poll.unwrap(), ShellPoll::InFlight);
    assert_eq!(tickets.len(), 2);
  }

  #[test]
  fn work_that_is_not_needed_settles_without_a_flight() {
    let mut slot = ShellSlot::<bool>::default();
    let poll =
      slot.converge(false, seen(Some(true)), Instant::now(), || Ok(None));
    assert_eq!(poll.unwrap(), ShellPoll::Settled);
    assert!(!slot.in_flight());
  }

  #[test]
  fn a_submission_error_leaves_the_slot_idle() {
    let mut slot = ShellSlot::<bool>::default();
    let poll =
      slot.converge(true, seen(Some(false)), Instant::now(), || {
        Err(crate::Error::Platform("worker gone".into()))
      });
    assert!(poll.is_err());
    assert!(!slot.in_flight());
  }

  #[test]
  fn an_observation_error_leaves_the_flight_alone() {
    let now = Instant::now();
    let mut tickets = Vec::new();
    let mut slot = ShellSlot::<bool>::default();
    slot
      .converge(true, seen(Some(false)), now, submit(&mut tickets))
      .unwrap();
    let poll = slot.converge(
      true,
      || Err(crate::Error::Platform("gone".into())),
      now,
      forbidden,
    );
    assert!(poll.is_err());
    assert!(slot.in_flight());
  }
}
