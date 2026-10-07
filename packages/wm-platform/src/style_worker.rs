//! Per-process queue for `WS_EX_LAYERED` changes.
//!
//! Setting an extended style on another process's window sends
//! `WM_STYLECHANGING` and `WM_STYLECHANGED` and waits for the owner to
//! answer. A busy or hung application holds the caller for as long as it
//! likes (an earlier build logged stalls of 30 seconds on the window
//! manager thread), so neither the window manager thread nor the ordered
//! shell thread makes the call. Adding or removing the bit is queued here
//! instead.
//!
//! Each target process has its own lane, run by its own thread, so one
//! hung application delays only its own windows. Each window has one slot
//! that holds the latest desired state, so a burst of changes collapses
//! into the last one rather than replaying. Changing the attributes of a
//! window that already has the bit is not queued; the caller does that
//! inline.
//!
//! A request may carry a completion hook, for a caller that must act once
//! the change has ended (an opening reservation that must be dropped if
//! its conceal fails, a release that must finish once the bit is gone).
//! Hooks run on the lane's thread, after the lock is released.
//!
//! The queue holds only raw style mutations. Recovery properties are
//! written by whoever requests the change, never here.

use std::{
  collections::{HashMap, VecDeque},
  sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError},
  time::{Duration, Instant},
};

use crate::{
  shell_request::{notify_native_wake, SHELL_RETRY},
  NativeSession,
};

/// How long a style call may run before it is reported as a stall.
const STALL: Duration = Duration::from_secs(1);

/// Runs once a queued change has ended, with whether it succeeded.
pub(crate) type Completion = Box<dyn FnOnce(bool) + Send>;

/// One queued or running style change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Request {
  /// Whether the window should end up layered.
  on: bool,
}

/// The state of one window's style change.
///
/// `pending` is what makes the slot latest-wins: a newer desire replaces
/// it, and a change that ends while one is still pending has been
/// superseded, so it neither records a failure nor runs hooks.
struct Slot<S> {
  target: S,
  /// What the latest request asked for.
  desired: Option<bool>,
  /// The attribute alpha to apply when the bit is added.
  alpha: u8,
  pending: Option<Request>,
  executing: Option<Request>,
  failure: Option<(String, Instant)>,
  /// Run when the work now outstanding ends.
  hooks: Vec<Completion>,
}

impl<S> Slot<S> {
  /// Whether no work is queued or running and nothing is left to report.
  fn idle(&self) -> bool {
    self.pending.is_none()
      && self.executing.is_none()
      && self.failure.is_none()
  }
}

/// The windows of one target process, run in order by one thread.
struct Lane<S> {
  slots: HashMap<isize, Slot<S>>,
  order: VecDeque<isize>,
  /// Whether a thread is draining the lane.
  running: bool,
}

impl<S> Default for Lane<S> {
  fn default() -> Self {
    Self {
      slots: HashMap::new(),
      order: VecDeque::new(),
      running: false,
    }
  }
}

impl<S> Lane<S> {
  /// Claims the lane's thread, returning whether the caller must start
  /// one.
  fn start(&mut self) -> bool {
    !std::mem::replace(&mut self.running, true)
  }

  /// Whether the lane holds nothing and has no thread.
  fn spent(&self) -> bool {
    self.slots.is_empty() && !self.running
  }
}

/// What a request left behind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Admission {
  /// The caller must start the lane's thread.
  spawn: bool,
  /// Work is queued or running for the window, so a hook given with the
  /// request will run. `false` means the window already was as asked, and
  /// the hook was dropped.
  pub(crate) outstanding: bool,
}

/// Every lane. Pure state, so the rules are testable without threads.
struct Lanes<S> {
  lanes: HashMap<u32, Lane<S>>,
}

impl<S> Default for Lanes<S> {
  fn default() -> Self {
    Self {
      lanes: HashMap::new(),
    }
  }
}

impl<S: Clone> Lanes<S> {
  /// Records that a window should be layered or not.
  ///
  /// A request the window already satisfies queues nothing, and creates
  /// no lane. One that reverses a queued but unstarted change withdraws
  /// it. A change that is running cannot be withdrawn, so a reversal
  /// queues behind it.
  fn request(
    &mut self,
    process: u32,
    hwnd: isize,
    target: &S,
    change: Change,
    hook: Option<Completion>,
    now: Instant,
  ) -> crate::Result<Admission> {
    let Change {
      on,
      alpha,
      observed_on,
    } = change;
    let Some(lane) = self.lanes.get_mut(&process) else {
      if observed_on == on {
        return Ok(Admission::default());
      }
      let lane = self.lanes.entry(process).or_default();
      return Ok(lane.queue_new(hwnd, target, change, hook));
    };
    let Some(slot) = lane.slots.get_mut(&hwnd) else {
      if observed_on == on {
        return Ok(Admission::default());
      }
      return Ok(lane.queue_new(hwnd, target, change, hook));
    };
    slot.target = target.clone();
    slot.alpha = alpha;
    let busy = slot.pending.is_some() || slot.executing.is_some();
    if !busy && observed_on == on {
      lane.slots.remove(&hwnd);
      self.prune(process);
      return Ok(Admission::default());
    }
    if slot.desired == Some(on) {
      match &slot.failure {
        Some((error, at))
          if now.saturating_duration_since(*at) < SHELL_RETRY =>
        {
          return Err(crate::Error::Platform(error.clone()));
        }
        // Already on its way.
        None if busy => {
          slot.hooks.extend(hook);
          return Ok(Admission {
            spawn: false,
            outstanding: true,
          });
        }
        // An old failure, or nothing outstanding: ask again.
        Some(_) | None => {}
      }
    }
    slot.desired = Some(on);
    slot.failure = None;
    let satisfied = match slot.executing {
      Some(executing) => executing.on == on,
      None => observed_on == on,
    };
    let was_queued = slot.pending.is_some();
    slot.pending = (!satisfied).then_some(Request { on });
    slot.hooks.extend(hook);
    if slot.pending.is_none() {
      if slot.executing.is_some() {
        // The change that is running already says what was asked, and
        // its end settles the hooks.
        return Ok(Admission {
          spawn: false,
          outstanding: true,
        });
      }
      lane.slots.remove(&hwnd);
      self.prune(process);
      return Ok(Admission::default());
    }
    if !was_queued {
      lane.order.push_back(hwnd);
    }
    Ok(Admission {
      spawn: lane.start(),
      outstanding: true,
    })
  }

  /// Takes the next queued change of a lane, or ends the lane's thread.
  fn next(&mut self, process: u32) -> Option<(isize, S, Request)> {
    let lane = self.lanes.get_mut(&process)?;
    while let Some(hwnd) = lane.order.pop_front() {
      let Some(slot) = lane.slots.get_mut(&hwnd) else {
        continue;
      };
      let Some(request) = slot.pending.take() else {
        continue;
      };
      slot.executing = Some(request);
      return Some((hwnd, slot.target.clone(), request));
    }
    lane.running = false;
    self.prune(process);
    None
  }

  /// Records a finished change, returning the hooks it settles.
  ///
  /// A change that ends while a newer one is queued has been superseded:
  /// it records no failure and settles nothing, and the newer change, or
  /// the next observation, repairs whatever it left.
  fn complete(
    &mut self,
    process: u32,
    hwnd: isize,
    error: Option<String>,
    now: Instant,
  ) -> Vec<Completion> {
    let Some(lane) = self.lanes.get_mut(&process) else {
      return Vec::new();
    };
    let Some(slot) = lane.slots.get_mut(&hwnd) else {
      return Vec::new();
    };
    slot.executing = None;
    let mut hooks = Vec::new();
    if slot.pending.is_none() {
      slot.failure = error.map(|error| (error, now));
      hooks = std::mem::take(&mut slot.hooks);
    }
    if slot.idle() {
      lane.slots.remove(&hwnd);
    }
    hooks
  }

  /// The attribute alpha to apply to a window that gains the bit.
  ///
  /// Read when the change runs, not when it was queued, so a window whose
  /// reveal has since been requested comes up opaque rather than blank.
  fn alpha(&self, process: u32, hwnd: isize) -> u8 {
    self
      .lanes
      .get(&process)
      .and_then(|lane| lane.slots.get(&hwnd))
      .filter(|slot| slot.desired == Some(true))
      .map_or(u8::MAX, |slot| slot.alpha)
  }

  /// Whether a change is queued or running for the window.
  fn busy(&self, process: u32, hwnd: isize) -> bool {
    self
      .lanes
      .get(&process)
      .and_then(|lane| lane.slots.get(&hwnd))
      .is_some_and(|slot| {
        slot.pending.is_some() || slot.executing.is_some()
      })
  }

  /// Drops a destroyed window's state, and its hooks unrun.
  fn forget(&mut self, hwnd: isize) {
    for lane in self.lanes.values_mut() {
      lane.slots.remove(&hwnd);
    }
    self.lanes.retain(|_, lane| !lane.spent());
  }

  /// Fails every queued change of a lane that has no thread to run them,
  /// returning the hooks that failure settles.
  fn abort(
    &mut self,
    process: u32,
    error: &str,
    now: Instant,
  ) -> Vec<Completion> {
    let Some(lane) = self.lanes.get_mut(&process) else {
      return Vec::new();
    };
    lane.running = false;
    lane.order.clear();
    let mut hooks = Vec::new();
    for slot in lane.slots.values_mut() {
      if slot.pending.take().is_some() {
        slot.failure = Some((error.to_owned(), now));
        hooks.append(&mut slot.hooks);
      }
    }
    self.prune(process);
    hooks
  }

  /// Drops a lane that holds nothing and has no thread.
  fn prune(&mut self, process: u32) {
    if self.lanes.get(&process).is_some_and(Lane::spent) {
      self.lanes.remove(&process);
    }
  }
}

impl<S: Clone> Lane<S> {
  /// Queues a change for a window the lane has no slot for.
  fn queue_new(
    &mut self,
    hwnd: isize,
    target: &S,
    change: Change,
    hook: Option<Completion>,
  ) -> Admission {
    self.slots.insert(
      hwnd,
      Slot {
        target: target.clone(),
        desired: Some(change.on),
        alpha: change.alpha,
        pending: Some(Request { on: change.on }),
        executing: None,
        failure: None,
        hooks: hook.into_iter().collect(),
      },
    );
    self.order.push_back(hwnd);
    Admission {
      spawn: self.start(),
      outstanding: true,
    }
  }
}

/// What a caller wants of one window's `WS_EX_LAYERED` bit.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Change {
  /// Whether the window should be layered.
  pub(crate) on: bool,
  /// The attribute alpha to apply once layered.
  pub(crate) alpha: u8,
  /// Whether the window has the bit now.
  pub(crate) observed_on: bool,
}

/// Runs one change on a window, off the window manager thread.
pub(crate) trait StyleExecutor<S>: Send + Sync {
  /// Adds or removes the bit. `alpha` reads the alpha to apply after an
  /// add, at the moment it is needed.
  fn apply(
    &self,
    target: &S,
    on: bool,
    alpha: &dyn Fn() -> u8,
  ) -> crate::Result<()>;
}

/// The lanes and the threads that drain them.
pub(crate) struct StyleQueue<S> {
  state: Mutex<Lanes<S>>,
  executor: Arc<dyn StyleExecutor<S>>,
}

impl<S: Clone + Send + 'static> StyleQueue<S> {
  /// Creates a queue that runs changes with `executor`.
  pub(crate) fn new(executor: Arc<dyn StyleExecutor<S>>) -> Arc<Self> {
    Arc::new(Self {
      state: Mutex::new(Lanes::default()),
      executor,
    })
  }

  /// Locks the lanes, tolerating a poisoned mutex.
  ///
  /// The state stays consistent across a panic in a hook, which runs
  /// outside the lock, so recovering beats stranding every lane.
  fn lock(&self) -> MutexGuard<'_, Lanes<S>> {
    self.state.lock().unwrap_or_else(PoisonError::into_inner)
  }

  /// Queues a change for a window of `process`, starting the lane's
  /// thread if it has none.
  ///
  /// Returns whether work is outstanding for the window, which is whether
  /// `hook` will run. Fails with the last failure while it is recent, and
  /// otherwise retries.
  pub(crate) fn request(
    self: &Arc<Self>,
    process: u32,
    hwnd: isize,
    target: &S,
    change: Change,
    hook: Option<Completion>,
  ) -> crate::Result<bool> {
    let now = Instant::now();
    let admission = self
      .lock()
      .request(process, hwnd, target, change, hook, now)?;
    if !admission.spawn {
      return Ok(admission.outstanding);
    }
    let queue = Arc::clone(self);
    let spawned = std::thread::Builder::new()
      .name(format!("glazewm-style-{process}"))
      .spawn(move || queue.run(process));
    if let Err(err) = spawned {
      let error = err.to_string();
      let hooks = self.lock().abort(process, &error, now);
      for hook in hooks {
        hook(false);
      }
      return Err(crate::Error::Platform(error));
    }
    Ok(true)
  }

  /// Whether a change is queued or running for the window.
  pub(crate) fn busy(&self, process: u32, hwnd: isize) -> bool {
    self.lock().busy(process, hwnd)
  }

  /// Drops a destroyed window's state.
  pub(crate) fn forget(&self, hwnd: isize) {
    self.lock().forget(hwnd);
  }

  /// Drains one lane, then ends its thread.
  fn run(&self, process: u32) {
    loop {
      let Some((hwnd, target, request)) = self.lock().next(process) else {
        return;
      };
      let started = Instant::now();
      let result = self
        .executor
        .apply(&target, request.on, &|| self.lock().alpha(process, hwnd));
      let elapsed = started.elapsed();
      if elapsed >= STALL {
        tracing::warn!(
          hwnd,
          process,
          on = request.on,
          elapsed_ms = elapsed.as_millis(),
          "Application style call stalled on its isolated lane."
        );
      }
      let success = result.is_ok();
      let error = result.err().map(|err| err.to_string());
      let hooks =
        self.lock().complete(process, hwnd, error, Instant::now());
      for hook in hooks {
        hook(success);
      }
      notify_native_wake();
    }
  }
}

/// Runs changes on real windows.
struct NativeStyles;

impl StyleExecutor<NativeSession> for NativeStyles {
  fn apply(
    &self,
    target: &NativeSession,
    on: bool,
    alpha: &dyn Fn() -> u8,
  ) -> crate::Result<()> {
    target.execute_layered(on, alpha)
  }
}

/// The queue every session shares.
pub(crate) static STYLES: LazyLock<Arc<StyleQueue<NativeSession>>> =
  LazyLock::new(|| StyleQueue::new(Arc::new(NativeStyles)));

#[cfg(test)]
mod tests {
  use std::{
    sync::{
      atomic::{AtomicBool, Ordering},
      mpsc, Mutex,
    },
    time::Duration,
  };

  use super::*;

  /// A change to a window whose bit is `observed_on`.
  fn change(on: bool, observed_on: bool) -> Change {
    Change {
      on,
      alpha: 0,
      observed_on,
    }
  }

  /// Requests `on` of a window that has the opposite bit, with no hook.
  fn queued(lanes: &mut Lanes<()>, process: u32, hwnd: isize, on: bool) {
    lanes
      .request(process, hwnd, &(), change(on, !on), None, Instant::now())
      .unwrap();
  }

  /// Requests a change with no hook, at `now`.
  fn ask(
    lanes: &mut Lanes<()>,
    (process, hwnd): (u32, isize),
    change: Change,
    now: Instant,
  ) -> crate::Result<Admission> {
    lanes.request(process, hwnd, &(), change, None, now)
  }

  /// A hook that records whether it ran, and with what.
  fn recording(ran: &Arc<Mutex<Vec<bool>>>) -> Completion {
    let ran = Arc::clone(ran);
    Box::new(move |success| {
      ran.lock().unwrap().push(success);
    })
  }

  #[test]
  fn a_request_the_window_already_satisfies_queues_nothing() {
    let mut lanes = Lanes::<()>::default();
    let admission =
      ask(&mut lanes, (1, 10), change(true, true), Instant::now())
        .unwrap();
    assert_eq!(admission, Admission::default());
    assert!(!lanes.busy(1, 10));
    assert_eq!(lanes.next(1).map(|job| job.0), None);
  }

  /// A request that needs no work leaves no lane behind, however many
  /// windows or processes ask.
  #[test]
  fn a_satisfied_request_creates_no_lane() {
    let mut lanes = Lanes::<()>::default();
    let now = Instant::now();
    for process in 0..100 {
      ask(&mut lanes, (process, 10), change(true, true), now).unwrap();
    }
    assert!(lanes.lanes.is_empty());
    // Nor does one for a known lane's unknown window.
    ask(&mut lanes, (1, 10), change(true, false), now).unwrap();
    ask(&mut lanes, (1, 11), change(true, true), now).unwrap();
    assert_eq!(lanes.lanes[&1].slots.len(), 1);
  }

  #[test]
  fn only_the_first_request_of_a_lane_starts_its_thread() {
    let mut lanes = Lanes::<()>::default();
    let now = Instant::now();
    let start = |lanes: &mut Lanes<()>, process, hwnd| {
      ask(lanes, (process, hwnd), change(true, false), now)
        .unwrap()
        .spawn
    };
    assert!(start(&mut lanes, 1, 10));
    assert!(!start(&mut lanes, 1, 11));
    assert!(start(&mut lanes, 2, 12));
  }

  #[test]
  fn reversing_a_queued_change_withdraws_it() {
    let mut lanes = Lanes::<()>::default();
    queued(&mut lanes, 1, 10, true);
    // The bit was never added, so removing it needs no work.
    ask(&mut lanes, (1, 10), change(false, false), Instant::now())
      .unwrap();
    assert!(!lanes.busy(1, 10));
    assert_eq!(lanes.next(1).map(|job| job.0), None);
    assert!(lanes.lanes.is_empty());
  }

  #[test]
  fn reversing_a_running_change_queues_behind_it_and_the_last_one_wins() {
    let mut lanes = Lanes::<()>::default();
    let now = Instant::now();
    queued(&mut lanes, 1, 10, true);
    let (hwnd, (), adding) = lanes.next(1).unwrap();
    assert_eq!(hwnd, 10);
    assert!(adding.on);
    // A thousand flips while the add is stuck in the application.
    for flip in 0..1000 {
      let on = flip % 2 == 0;
      ask(&mut lanes, (1, 10), change(on, false), now).unwrap();
    }
    // The last flip asked for the bit to be removed again.
    assert!(lanes.busy(1, 10));
    lanes.complete(1, 10, Some("obsolete".into()), now);
    let (_, (), removing) = lanes.next(1).unwrap();
    assert!(!removing.on);
    lanes.complete(1, 10, None, now);
    assert!(!lanes.busy(1, 10));
    assert_eq!(lanes.next(1).map(|job| job.0), None);
    // The obsolete failure was dropped with the change it belonged to.
    ask(&mut lanes, (1, 10), change(true, false), now).unwrap();
  }

  #[test]
  fn a_superseded_failure_does_not_poison_a_newer_request() {
    let mut lanes = Lanes::<()>::default();
    let now = Instant::now();
    queued(&mut lanes, 1, 10, true);
    lanes.next(1).unwrap();
    queued(&mut lanes, 1, 10, false);
    // The removal is still queued when the add fails, so the add is
    // superseded and its failure means nothing.
    lanes.complete(1, 10, Some("old failure".into()), now);
    assert!(lanes.busy(1, 10));
    lanes.next(1).unwrap();
    lanes.complete(1, 10, None, now);
    assert!(ask(&mut lanes, (1, 10), change(true, false), now).is_ok());
  }

  /// A change that ends with nothing newer queued is the last word on
  /// what was asked, even if the request flipped back to it meanwhile.
  #[test]
  fn a_failure_of_the_change_that_satisfies_the_request_is_kept() {
    let mut lanes = Lanes::<()>::default();
    let now = Instant::now();
    queued(&mut lanes, 1, 10, true);
    lanes.next(1).unwrap();
    queued(&mut lanes, 1, 10, false);
    queued(&mut lanes, 1, 10, true);
    lanes.complete(1, 10, Some("unavailable".into()), now);
    assert!(!lanes.busy(1, 10));
    assert!(ask(&mut lanes, (1, 10), change(true, false), now).is_err());
  }

  #[test]
  fn a_failure_is_reported_while_recent_and_retried_after() {
    let mut lanes = Lanes::<()>::default();
    let now = Instant::now();
    queued(&mut lanes, 1, 10, true);
    lanes.next(1).unwrap();
    lanes.complete(1, 10, Some("unavailable".into()), now);
    assert!(!lanes.busy(1, 10));
    let soon = now + SHELL_RETRY / 2;
    assert!(ask(&mut lanes, (1, 10), change(true, false), soon).is_err());
    let later = now + SHELL_RETRY;
    assert!(ask(&mut lanes, (1, 10), change(true, false), later).is_ok());
    assert!(lanes.busy(1, 10));
  }

  #[test]
  fn alpha_is_read_when_the_change_runs() {
    let mut lanes = Lanes::<()>::default();
    let now = Instant::now();
    let conceal = Change {
      on: true,
      alpha: 0,
      observed_on: false,
    };
    ask(&mut lanes, (1, 10), conceal, now).unwrap();
    assert_eq!(lanes.alpha(1, 10), 0);
    lanes.next(1).unwrap();
    // A reveal arrives while the add is stuck: the window must come up
    // opaque, not blank.
    let reveal = Change {
      on: false,
      alpha: u8::MAX,
      observed_on: false,
    };
    ask(&mut lanes, (1, 10), reveal, now).unwrap();
    assert_eq!(lanes.alpha(1, 10), u8::MAX);
    lanes.complete(1, 10, None, now);
  }

  #[test]
  fn forgetting_a_window_drops_its_queued_change() {
    let mut lanes = Lanes::<()>::default();
    queued(&mut lanes, 1, 10, true);
    lanes.forget(10);
    assert!(!lanes.busy(1, 10));
    assert_eq!(lanes.next(1).map(|job| job.0), None);
  }

  #[test]
  fn an_idle_lane_is_released_with_its_thread() {
    let mut lanes = Lanes::<()>::default();
    queued(&mut lanes, 1, 10, true);
    lanes.next(1).unwrap();
    lanes.complete(1, 10, None, Instant::now());
    assert_eq!(lanes.next(1).map(|job| job.0), None);
    assert!(lanes.lanes.is_empty());
    // A later request starts a new thread.
    let admission =
      ask(&mut lanes, (1, 10), change(true, false), Instant::now())
        .unwrap();
    assert!(admission.spawn);
  }

  /// A lane whose last slot goes away while its thread still runs is
  /// kept for that thread, and dropped once it ends.
  #[test]
  fn a_lane_is_pruned_when_its_last_slot_is_dropped_without_a_thread() {
    let mut lanes = Lanes::<()>::default();
    let now = Instant::now();
    // The queued add is withdrawn by the reversal: nothing is left, and
    // the lane's thread has not yet noticed.
    queued(&mut lanes, 1, 10, true);
    ask(&mut lanes, (1, 10), change(false, false), now).unwrap();
    assert!(lanes.lanes.contains_key(&1));
    assert_eq!(lanes.next(1).map(|job| job.0), None);
    assert!(lanes.lanes.is_empty());
    // With no thread, the same withdrawal drops the lane at once.
    queued(&mut lanes, 2, 20, true);
    lanes.lanes.get_mut(&2).unwrap().running = false;
    ask(&mut lanes, (2, 20), change(false, false), now).unwrap();
    assert!(lanes.lanes.is_empty());
  }

  #[test]
  fn a_hook_runs_when_its_change_ends() {
    let ran = Arc::new(Mutex::new(Vec::new()));
    let mut lanes = Lanes::<()>::default();
    let now = Instant::now();
    let admission = lanes
      .request(1, 10, &(), change(true, false), Some(recording(&ran)), now)
      .unwrap();
    assert!(admission.outstanding);
    lanes.next(1).unwrap();
    for hook in lanes.complete(1, 10, None, now) {
      hook(true);
    }
    assert_eq!(*ran.lock().unwrap(), [true]);
  }

  #[test]
  fn a_hook_for_work_that_is_not_needed_is_not_outstanding() {
    let ran = Arc::new(Mutex::new(Vec::new()));
    let mut lanes = Lanes::<()>::default();
    let admission = lanes
      .request(
        1,
        10,
        &(),
        change(true, true),
        Some(recording(&ran)),
        Instant::now(),
      )
      .unwrap();
    assert!(!admission.outstanding);
    assert!(ran.lock().unwrap().is_empty());
  }

  /// A later request for the same state joins the work already queued, so
  /// both hooks run when it ends.
  #[test]
  fn a_second_request_for_the_same_change_shares_the_work() {
    let ran = Arc::new(Mutex::new(Vec::new()));
    let mut lanes = Lanes::<()>::default();
    let now = Instant::now();
    for _ in 0..2 {
      let admission = lanes
        .request(
          1,
          10,
          &(),
          change(false, true),
          Some(recording(&ran)),
          now,
        )
        .unwrap();
      assert!(admission.outstanding);
    }
    lanes.next(1).unwrap();
    assert_eq!(lanes.next(1).map(|job| job.0), None);
    let hooks = lanes.complete(1, 10, None, now);
    assert_eq!(hooks.len(), 2);
  }

  /// A superseded change settles nothing: its hooks wait for the newer
  /// change that replaced it.
  #[test]
  fn hooks_wait_for_the_change_that_supersedes() {
    let ran = Arc::new(Mutex::new(Vec::new()));
    let mut lanes = Lanes::<()>::default();
    let now = Instant::now();
    lanes
      .request(1, 10, &(), change(true, false), Some(recording(&ran)), now)
      .unwrap();
    lanes.next(1).unwrap();
    // While the add runs, the reverse is queued behind it.
    ask(&mut lanes, (1, 10), change(false, false), now).unwrap();
    assert!(lanes.complete(1, 10, None, now).is_empty());
    lanes.next(1).unwrap();
    let hooks = lanes.complete(1, 10, None, now);
    assert_eq!(hooks.len(), 1);
  }

  #[test]
  fn forgetting_a_window_drops_its_hooks_unrun() {
    let ran = Arc::new(Mutex::new(Vec::new()));
    let mut lanes = Lanes::<()>::default();
    lanes
      .request(
        1,
        10,
        &(),
        change(true, false),
        Some(recording(&ran)),
        Instant::now(),
      )
      .unwrap();
    lanes.forget(10);
    assert!(ran.lock().unwrap().is_empty());
    // The lane's thread has not ended, so it still owns the lane.
    assert_eq!(lanes.next(1).map(|job| job.0), None);
    assert!(lanes.lanes.is_empty());
  }

  #[test]
  fn a_lane_that_cannot_start_fails_its_hooks() {
    let ran = Arc::new(Mutex::new(Vec::new()));
    let mut lanes = Lanes::<()>::default();
    let now = Instant::now();
    lanes
      .request(1, 10, &(), change(true, false), Some(recording(&ran)), now)
      .unwrap();
    let hooks = lanes.abort(1, "no thread", now);
    assert_eq!(hooks.len(), 1);
    assert!(!lanes.busy(1, 10));
  }

  /// Blocks the first window's change until released, and records what
  /// ran.
  struct Gated {
    release: Mutex<mpsc::Receiver<()>>,
    done: Mutex<mpsc::Sender<isize>>,
  }

  impl StyleExecutor<isize> for Gated {
    fn apply(
      &self,
      target: &isize,
      _: bool,
      _: &dyn Fn() -> u8,
    ) -> crate::Result<()> {
      if *target == 10 {
        let release =
          self.release.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = release.recv_timeout(Duration::from_secs(10));
      }
      let done = self.done.lock().unwrap_or_else(PoisonError::into_inner);
      let _ = done.send(*target);
      Ok(())
    }
  }

  /// A hung application delays its own windows and nobody else's.
  #[test]
  fn a_stalled_process_does_not_delay_another() {
    let (release_tx, release_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let queue = StyleQueue::new(Arc::new(Gated {
      release: Mutex::new(release_rx),
      done: Mutex::new(done_tx),
    }));
    // Process 1 hangs on its window; its second window queues behind it.
    queue
      .request(1, 10, &10, change(true, false), None)
      .unwrap();
    queue
      .request(1, 11, &11, change(true, false), None)
      .unwrap();
    // Process 2 is unaffected.
    queue
      .request(2, 20, &20, change(true, false), None)
      .unwrap();
    assert_eq!(
      done_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
      20,
      "The other process waited on the stalled one."
    );
    assert!(done_rx.try_recv().is_err());
    assert!(queue.busy(1, 10));
    assert!(queue.busy(1, 11));
    // Once the application answers, its lane resumes in order.
    release_tx.send(()).unwrap();
    assert_eq!(done_rx.recv_timeout(Duration::from_secs(5)).unwrap(), 10);
    assert_eq!(done_rx.recv_timeout(Duration::from_secs(5)).unwrap(), 11);
    for _ in 0..200 {
      if !queue.busy(1, 11) {
        break;
      }
      std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!queue.busy(1, 10));
    assert!(!queue.busy(1, 11));
  }

  /// A hook given to a real lane runs on the lane's thread after the
  /// change, and a stalled neighbour does not hold it back.
  #[test]
  fn a_hook_runs_on_its_lane_after_the_change() {
    let (release_tx, release_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let queue = StyleQueue::new(Arc::new(Gated {
      release: Mutex::new(release_rx),
      done: Mutex::new(done_tx),
    }));
    // Process 1 hangs, so a hook for process 2 must not wait on it.
    queue
      .request(1, 10, &10, change(true, false), None)
      .unwrap();
    let fired = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&fired);
    let (hook_tx, hook_rx) = mpsc::channel();
    let hook: Completion = Box::new(move |success| {
      flag.store(success, Ordering::SeqCst);
      let _ = hook_tx.send(());
    });
    let outstanding = queue
      .request(2, 20, &20, change(true, false), Some(hook))
      .unwrap();
    assert!(outstanding);
    hook_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(fired.load(Ordering::SeqCst));
    assert_eq!(done_rx.recv_timeout(Duration::from_secs(5)).unwrap(), 20);
    release_tx.send(()).unwrap();
  }
}
