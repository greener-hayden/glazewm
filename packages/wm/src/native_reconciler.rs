use std::time::{Duration, Instant};

use wm_platform::Rect;

const RETRY_WAIT: Duration = Duration::from_millis(250);
const MAX_ATTEMPTS: u8 = 3;

/// Longest wait for an application to apply a frame write before the
/// next one is issued anyway.
///
/// Windows frame writes are queued on the application's own thread. An
/// application slower than the frame clock (File Explorer takes over a
/// frame per resize) otherwise falls behind by a write every frame: it
/// replays the backlog long after the motion has ended, and a close
/// posted meanwhile waits behind it. Only a hung application reaches
/// this bound.
const APPLY_TIMEOUT: Duration = Duration::from_millis(100);

/// How long a frame that used up its attempts rests before it is tried
/// again.
///
/// An application that is busy drops writes it would otherwise take:
/// Teams has been seen to answer nothing for seconds and then carry on.
/// Giving up for good after three quick attempts leaves such a window
/// wherever it stood, over its neighbours. The rest is long enough for a
/// stall to pass, and for an application that really refuses a frame to
/// have had its minimum size learned.
const REST: Duration = Duration::from_secs(2);

/// How many times a failed frame is tried again after resting. Bounded,
/// so an application that insists on its own frame is not fought forever.
const MAX_RESTS: u8 = 1;

/// Furthest a window's right or bottom edge may stand from the one asked
/// for, in pixels, and still count as the application rounding the size
/// rather than refusing it.
///
/// Some applications do not take every size. A terminal keeps whole rows
/// of text: kitty at full height on a 1200px display keeps its height
/// when asked for 8px less, and takes 16px less. Such a window never
/// matches its frame however often it is asked, so it is accepted where
/// it stands. The bound is a row of large text; past it the window is
/// refusing the size, which is what a minimum size looks like.
pub const ROUNDING_SLACK: u32 = 24;

/// A frame write the application has not been observed to apply.
#[derive(Clone, Debug)]
struct UnappliedWrite {
  rect: Rect,
  /// The observation the write was issued against.
  before: Option<ObservedFrame>,
  at: Instant,
}

impl UnappliedWrite {
  /// Whether `observed` shows the application has acted on the write.
  ///
  /// An application may clamp or adjust the rect, so any change from the
  /// frame the write was issued against also counts.
  fn applied(&self, observed: &ObservedFrame) -> bool {
    frames_match(&self.rect, &observed.rect)
      || self.before.as_ref() != Some(observed)
  }
}

/// Describes native show-state intent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeState {
  Normal,
  Minimized,
  Maximized,
}

/// Holds an operational native target, including temporary source parking.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesiredFrame {
  pub rect: Rect,
  pub monitor: Rect,
  /// The part of the monitor a maximized window is expected to fill.
  pub working_area: Rect,
  pub dpi: u32,
  pub state: NativeState,
  /// Pixels the platform may pull a parked target back toward its
  /// display. `None` when the target is not parked.
  pub parking_clamp: Option<i32>,
}

/// Contains observations, not accepted requests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedFrame {
  pub rect: Rect,
  pub dpi: u32,
  pub state: NativeState,
}

/// Separates state operations from geometry operations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NativeMutation {
  Frame(Rect),
  Restore(Rect),
  Minimize,
  Maximize,
}

/// What to read back from a window straight after a native write.
///
/// Reading a window's state can ask its application, and an application
/// handles requests in order: a state read queued behind a frame write
/// waits for the relayout that write started, one window after another.
/// A frame write cannot change the state, so that read is skipped and the
/// state seen before the write stands. A real change raises its own event
/// and is read by the next pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObservePlan {
  /// Read geometry and state.
  Full,
  /// Read geometry alone, keeping `state` from before the write.
  Geometry { state: NativeState },
}

impl ObservePlan {
  /// Plans the read after `mutation`, issued against `before`.
  ///
  /// `state_reads_ask_app` is whether reading the state reaches the
  /// window's application (`PlacementSession::STATE_READS_ASK_APP`); when
  /// it does not, there is nothing to save. A state mutation changes the
  /// state it writes, and acknowledging it enables the geometry that
  /// follows, so it is always read back in full.
  pub fn after_write(
    mutation: &NativeMutation,
    before: &ObservedFrame,
    state_reads_ask_app: bool,
  ) -> Self {
    match mutation {
      NativeMutation::Frame(_) if state_reads_ask_app => Self::Geometry {
        state: before.state,
      },
      _ => Self::Full,
    }
  }
}

/// Identifies requests within a managed session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeRequest {
  pub generation: u64,
  pub mutation: NativeMutation,
}

/// Tracks acceptance separately from convergence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReconcilePhase {
  Pending,
  Accepted,
  Converged,
  Failed,
  Suspended,
}

/// Reconciles one window without native side-effects.
pub struct FrameReconciler {
  pub desired: DesiredFrame,
  pub generation: u64,
  pub phase: ReconcilePhase,
  attempts: u8,
  accepted_at: Option<Instant>,
  /// Whether the request accepted last was a frame write, rather than a
  /// state change. Meaningful only while `phase` is `Accepted`.
  accepted_frame_write: bool,
  previous: Option<ObservedFrame>,
  stable_since: Option<Instant>,
  /// The last frame write, until the application is seen to apply it.
  ///
  /// Survives `restart`: a retarget does not recall a write already
  /// queued on the application.
  unapplied: Option<UnappliedWrite>,
  /// A frame accepted in place of the desired one, because the
  /// application rounded the size it was asked for. See `ROUNDING_SLACK`.
  rounded: Option<Rect>,
  /// The state the window was last observed in.
  ///
  /// Survives `restart`, unlike `previous`: a new target says nothing
  /// about the window's state.
  last_state: Option<NativeState>,
  /// When the frame used up its attempts, while it can still be tried
  /// again after `REST`.
  failed_at: Option<Instant>,
  /// How many times the frame has been tried again after resting.
  rests: u8,
}

impl FrameReconciler {
  /// Schedules only outstanding request recovery, never periodic polling.
  ///
  /// A pending write held behind an unapplied one waits for the
  /// application's move event, with `APPLY_TIMEOUT` as the fallback.
  pub fn deadline(&self) -> Option<Instant> {
    match self.phase {
      ReconcilePhase::Pending => Some(
        self
          .unapplied
          .as_ref()
          .map_or_else(Instant::now, |write| write.at + APPLY_TIMEOUT),
      ),
      ReconcilePhase::Accepted => {
        self.accepted_at.map(|at| at + RETRY_WAIT)
      }
      ReconcilePhase::Failed => self.failed_at.map(|at| at + REST),
      _ => None,
    }
  }
  /// Starts an independently cancellable operation.
  pub fn new(desired: DesiredFrame) -> Self {
    Self {
      desired,
      generation: 1,
      phase: ReconcilePhase::Pending,
      attempts: 0,
      accepted_at: None,
      accepted_frame_write: false,
      previous: None,
      stable_since: None,
      unapplied: None,
      rounded: None,
      last_state: None,
      failed_at: None,
      rests: 0,
    }
  }

  /// Replaces intent and invalidates stale requests.
  pub fn retarget(&mut self, desired: DesiredFrame) {
    if self.desired == desired {
      return;
    }
    self.desired = desired;
    self.restart();
  }

  /// Restarts verification without replaying old requests.
  pub fn restart(&mut self) {
    self.generation = self.generation.wrapping_add(1);
    self.phase = ReconcilePhase::Pending;
    self.attempts = 0;
    self.accepted_at = None;
    self.accepted_frame_write = false;
    self.previous = None;
    self.stable_since = None;
    self.rounded = None;
    self.failed_at = None;
    self.rests = 0;
  }

  /// Whether a request is still awaiting its outcome.
  pub fn in_flight(&self) -> bool {
    matches!(
      self.phase,
      ReconcilePhase::Pending | ReconcilePhase::Accepted
    )
  }

  /// Whether reconciliation reached a terminal outcome.
  #[cfg(target_os = "windows")]
  pub fn settled(&self) -> bool {
    matches!(
      self.phase,
      ReconcilePhase::Converged | ReconcilePhase::Failed
    )
  }

  /// Whether the request accepted last was a frame write.
  ///
  /// Only an `Accepted` frame has one outstanding; in any other phase
  /// the write, if any, has been confirmed or replaced.
  pub fn frame_write_accepted(&self) -> bool {
    self.phase == ReconcilePhase::Accepted && self.accepted_frame_write
  }

  /// The state the window was last observed in, if it was observed.
  pub fn observed_state(&self) -> Option<NativeState> {
    self.previous.as_ref().map(|observed| observed.state)
  }

  /// The state the window was last observed in under any target, if it
  /// ever was.
  ///
  /// For a pass that does not act on the state and so need not read it,
  /// such as one over a window behind its overlay or under a drag.
  pub fn last_state(&self) -> Option<NativeState> {
    self.last_state
  }

  /// The state to assume for a window whose frame write is neither
  /// confirmed nor due to be issued again at `now`.
  ///
  /// A frame write cannot change the state, so the one last observed
  /// stands and need not be read. `None` once the write is confirmed or
  /// its retry is due, when the state is read afresh.
  pub fn awaited_state(&self, now: Instant) -> Option<NativeState> {
    (self.frame_write_accepted()
      && self
        .accepted_at
        .is_some_and(|at| now.saturating_duration_since(at) < RETRY_WAIT))
    .then(|| self.observed_state())
    .flatten()
  }

  /// Cancels pending mutations during user control.
  pub fn suspend(&mut self) {
    if self.phase != ReconcilePhase::Suspended {
      self.restart();
      self.phase = ReconcilePhase::Suspended;
    }
  }

  /// Rejects responses belonging to obsolete operations.
  pub fn accepts(&self, request: &NativeRequest) -> bool {
    request.generation == self.generation
      && !matches!(
        self.phase,
        ReconcilePhase::Failed | ReconcilePhase::Suspended
      )
  }

  /// Applies current requests through the native executor.
  pub fn apply<F>(
    &mut self,
    request: &NativeRequest,
    now: Instant,
    execute: F,
  ) -> anyhow::Result<bool>
  where
    F: FnOnce(&NativeMutation) -> anyhow::Result<()>,
  {
    if !self.accepts(request) {
      return Ok(false);
    }
    match execute(&request.mutation) {
      Ok(()) => {
        self.accepted(request, now);
        Ok(true)
      }
      Err(err) => {
        self.failed();
        Err(err)
      }
    }
  }

  /// Records only successfully submitted native requests.
  pub fn accepted(&mut self, request: &NativeRequest, now: Instant) {
    if !self.accepts(request) {
      return;
    }
    self.phase = ReconcilePhase::Accepted;
    self.accepted_at = Some(now);
    self.accepted_frame_write =
      matches!(request.mutation, NativeMutation::Frame(_));
    self.attempts = self.attempts.saturating_add(1);
    if let NativeMutation::Frame(rect) = &request.mutation {
      self.unapplied = Some(UnappliedWrite {
        rect: rect.clone(),
        before: self.previous.clone(),
        at: now,
      });
    }
  }

  /// Terminates failed requests without declaring convergence.
  ///
  /// A request the platform rejected outright is not tried again after a
  /// rest; that is for attempts an application did not act on.
  pub fn failed(&mut self) {
    self.phase = ReconcilePhase::Failed;
    self.failed_at = None;
  }

  /// Whether the frame has failed and waits to be tried again.
  pub fn resting(&self) -> bool {
    self.phase == ReconcilePhase::Failed && self.failed_at.is_some()
  }

  /// Accepts an observation without issuing another native request.
  pub fn observe(&mut self, observed: &ObservedFrame, now: Instant) {
    if self.phase == ReconcilePhase::Accepted
      && self
        .previous
        .as_ref()
        .is_some_and(|previous| previous.state != observed.state)
    {
      // A native state acknowledgement enables dependent geometry
      // immediately.
      self.phase = ReconcilePhase::Pending;
      self.accepted_at = None;
      self.attempts = 0;
    }
    if self
      .unapplied
      .as_ref()
      .is_some_and(|write| write.applied(observed))
    {
      self.unapplied = None;
    }
    self.last_state = Some(observed.state);
    if self.previous.as_ref() != Some(observed) {
      self.previous = Some(observed.clone());
      self.stable_since = Some(now);
    }
    if self.phase != ReconcilePhase::Suspended && self.converged(observed)
    {
      self.phase = ReconcilePhase::Converged;
      self.accepted_at = None;
    }
  }

  /// Plans from current native observations only.
  pub fn next(
    &mut self,
    observed: &ObservedFrame,
    now: Instant,
  ) -> Option<NativeRequest> {
    self.observe(observed, now);
    // A failed frame gets its attempts back once it has rested.
    if self.phase == ReconcilePhase::Failed
      && self.failed_at.is_some_and(|at| now >= at + REST)
    {
      self.phase = ReconcilePhase::Pending;
      self.attempts = 0;
      self.accepted_at = None;
      self.failed_at = None;
      self.rests += 1;
    }
    if matches!(
      self.phase,
      ReconcilePhase::Failed | ReconcilePhase::Suspended
    ) {
      return None;
    }
    let mutation = self.mutation(observed);
    if mutation.is_none() {
      self.phase = ReconcilePhase::Converged;
      self.accepted_at = None;
      return None;
    }
    if self
      .accepted_at
      .is_some_and(|at| now.saturating_duration_since(at) < RETRY_WAIT)
    {
      return None;
    }
    // The application has had a full wait to take the last write and
    // stands a rounding away from it. Asking again gets the same answer.
    if self.accepted_at.is_some() && self.arrived(observed) {
      self.rounded = Some(observed.rect.clone());
      self.phase = ReconcilePhase::Converged;
      self.accepted_at = None;
      return None;
    }
    // Keep at most one frame write queued on the application. The next
    // write carries the latest target, so skipped frames cost smoothness
    // on a slow application, never time.
    if matches!(mutation, Some(NativeMutation::Frame(_)))
      && self.unapplied.as_ref().is_some_and(|write| {
        now.saturating_duration_since(write.at) < APPLY_TIMEOUT
      })
    {
      return None;
    }
    if self.attempts >= MAX_ATTEMPTS {
      self.phase = ReconcilePhase::Failed;
      self.failed_at = (self.rests < MAX_RESTS).then_some(now);
      return None;
    }
    mutation.map(|mutation| NativeRequest {
      generation: self.generation,
      mutation,
    })
  }

  /// Checks fresh state without advancing operations.
  pub fn converged(&self, observed: &ObservedFrame) -> bool {
    self.mutation(observed).is_none()
  }

  /// Whether the window stands where it was asked to, at the size asked
  /// for or a rounding away from it (see `ROUNDING_SLACK`).
  ///
  /// Holds only once a frame has been written for the current target, so
  /// a window that merely starts near its target does not count. Unlike
  /// `converged`, this does not wait for an exact match that a rounding
  /// application never gives. A parked target has to match exactly.
  pub fn arrived(&self, observed: &ObservedFrame) -> bool {
    self.attempts > 0
      && self.in_place(observed)
      && self.desired.parking_clamp.is_none()
      && self.desired.rect.right.abs_diff(observed.rect.right)
        <= ROUNDING_SLACK
      && self.desired.rect.bottom.abs_diff(observed.rect.bottom)
        <= ROUNDING_SLACK
  }

  /// Whether the window is in the normal state it was asked for, with
  /// its top-left corner where its frame puts it, whatever its size.
  pub fn in_place(&self, observed: &ObservedFrame) -> bool {
    self.desired.state == NativeState::Normal
      && observed.state == NativeState::Normal
      && self.desired.dpi == observed.dpi
      && self.desired.rect.left.abs_diff(observed.rect.left) <= 1
      && self.desired.rect.top.abs_diff(observed.rect.top) <= 1
  }

  /// Chooses state before dependent geometry.
  fn mutation(&self, observed: &ObservedFrame) -> Option<NativeMutation> {
    match (self.desired.state, observed.state) {
      (NativeState::Minimized, NativeState::Minimized) => None,
      (NativeState::Minimized, _) => Some(NativeMutation::Minimize),
      (
        NativeState::Normal,
        NativeState::Minimized | NativeState::Maximized,
      ) => Some(NativeMutation::Restore(self.desired.rect.clone())),
      (NativeState::Maximized, NativeState::Maximized) => {
        if !self
          .desired
          .monitor
          .contains_point(&observed.rect.center_point())
        {
          Some(NativeMutation::Restore(self.desired.rect.clone()))
        } else if self.fills_working_area(observed) {
          None
        } else {
          // Some windows take the maximized state and keep their size;
          // SDL pins a fixed-size window's maximum to its current size.
          // The state alone then leaves the window where it was.
          Some(NativeMutation::Frame(self.desired.rect.clone()))
        }
      }
      (NativeState::Maximized, NativeState::Minimized) => {
        Some(NativeMutation::Restore(self.desired.rect.clone()))
      }
      (NativeState::Maximized, NativeState::Normal) => {
        if self
          .desired
          .monitor
          .contains_point(&observed.rect.center_point())
        {
          Some(NativeMutation::Maximize)
        } else {
          Some(NativeMutation::Frame(self.desired.rect.clone()))
        }
      }
      (NativeState::Normal, NativeState::Normal) => {
        let placed = match self.desired.parking_clamp {
          Some(clamp) => {
            parked_frames_match(&self.desired.rect, &observed.rect, clamp)
          }
          None => {
            frames_match(&self.desired.rect, &observed.rect)
              || self.rounded.as_ref() == Some(&observed.rect)
          }
        };
        (!placed || self.desired.dpi != observed.dpi)
          .then(|| NativeMutation::Frame(self.desired.rect.clone()))
      }
    }
  }

  /// Whether a maximized window covers the area maximizing should fill.
  ///
  /// The OS can be off by 1px when positioning windows.
  fn fills_working_area(&self, observed: &ObservedFrame) -> bool {
    observed
      .rect
      .contains_rect(&self.desired.working_area.inset(1))
  }

  /// Requires native evidence before learning constraints.
  pub fn constraint(
    &self,
    observed: &ObservedFrame,
    hint: Option<(i32, i32)>,
    now: Instant,
  ) -> Option<(i32, i32)> {
    if self.phase != ReconcilePhase::Failed
      || self.attempts < MAX_ATTEMPTS
      || observed.state != NativeState::Normal
      || observed.dpi != self.desired.dpi
      || !self.stable_since.is_some_and(|at| {
        now.saturating_duration_since(at) >= RETRY_WAIT * 2
      })
    {
      return None;
    }
    let axis =
      |actual: i32, desired: i32, minimum: Option<i32>| match minimum {
        Some(minimum)
          if minimum > desired && (actual - minimum).abs() <= 1 =>
        {
          minimum
        }
        // A miss within the slack is a rounded size, not a floor: the
        // window takes smaller sizes than the one it kept.
        None
          if actual > desired
            && actual.abs_diff(desired) > ROUNDING_SLACK =>
        {
          actual
        }
        _ => 0,
      };
    let floor = (
      axis(
        observed.rect.width(),
        self.desired.rect.width(),
        hint.map(|size| size.0),
      ),
      axis(
        observed.rect.height(),
        self.desired.rect.height(),
        hint.map(|size| size.1),
      ),
    );
    (floor != (0, 0)).then_some(floor)
  }
}

/// Compares normalized native-frame edges consistently.
pub fn frames_match(desired: &Rect, observed: &Rect) -> bool {
  [
    desired.left.abs_diff(observed.left),
    desired.top.abs_diff(observed.top),
    desired.right.abs_diff(observed.right),
    desired.bottom.abs_diff(observed.bottom),
  ]
  .into_iter()
  .all(|difference| difference <= 1)
}

/// Compares a parked frame, letting the platform pull its top edge back
/// toward the display by up to `clamp` pixels.
///
/// Horizontal position and size still match within 1px.
pub fn parked_frames_match(
  desired: &Rect,
  observed: &Rect,
  clamp: i32,
) -> bool {
  let lift = desired.top - observed.top;
  desired.left.abs_diff(observed.left) <= 1
    && desired.width().abs_diff(observed.width()) <= 1
    && desired.height().abs_diff(observed.height()) <= 1
    && (-1..=clamp).contains(&lift)
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Creates an ordinary target and observation.
  fn fixture() -> (FrameReconciler, ObservedFrame) {
    let rect = Rect::from_xy(10, 20, 400, 300);
    (
      FrameReconciler::new(DesiredFrame {
        rect: rect.clone(),
        monitor: Rect::from_xy(0, 0, 1920, 1080),
        working_area: Rect::from_xy(0, 0, 1920, 1040),
        dpi: 96,
        state: NativeState::Normal,
        parking_clamp: None,
      }),
      ObservedFrame {
        rect,
        dpi: 96,
        state: NativeState::Normal,
      },
    )
  }

  /// Restores before issuing dependent geometry.
  #[test]
  fn restores_before_geometry() {
    let (mut sync, mut observed) = fixture();
    observed.state = NativeState::Maximized;
    let now = Instant::now();
    let request = sync.next(&observed, now).expect("Restore request.");
    assert!(matches!(request.mutation, NativeMutation::Restore(_)));
    sync.accepted(&request, now);
    assert!(sync.next(&observed, now).is_none());
    assert_eq!(sync.phase, ReconcilePhase::Accepted);
    assert!(sync.in_flight());
    observed.state = NativeState::Normal;
    assert!(sync.next(&observed, now).is_none());
    assert_eq!(sync.phase, ReconcilePhase::Converged);
    assert!(!sync.in_flight());
  }

  /// Rejects stale completion after retargeting.
  #[test]
  fn rejects_stale_requests() {
    let (mut sync, mut observed) = fixture();
    observed.rect.left += 30;
    let request =
      sync.next(&observed, Instant::now()).expect("Move request.");
    let mut target = sync.desired.clone();
    target.rect.left += 10;
    sync.retarget(target);
    sync.accepted(&request, Instant::now());
    assert!(!sync.accepts(&request));
    assert_eq!(sync.phase, ReconcilePhase::Pending);
  }

  /// Moves `sync` to a target 10px further right each call, as native
  /// motion does every frame.
  fn advance(sync: &mut FrameReconciler) {
    let mut target = sync.desired.clone();
    target.rect = target
      .rect
      .translate_to_coordinates(target.rect.left + 10, target.rect.top);
    sync.retarget(target);
  }

  /// Holds motion writes while the application has not applied the last
  /// one, then writes the latest target.
  #[test]
  fn holds_frame_writes_until_applied() {
    let (mut sync, observed) = fixture();
    let now = Instant::now();
    advance(&mut sync);
    let first = sync.next(&observed, now).expect("First write.");
    sync.accepted(&first, now);
    sync.observe(&observed, now);

    let frame = Duration::from_millis(7);
    advance(&mut sync);
    advance(&mut sync);
    assert!(sync.next(&observed, now + frame).is_none());
    assert_eq!(sync.deadline(), Some(now + APPLY_TIMEOUT));

    let NativeMutation::Frame(written) = first.mutation else {
      panic!("Expected a frame write.");
    };
    let applied = ObservedFrame {
      rect: written,
      ..observed
    };
    let request = sync
      .next(&applied, now + frame * 2)
      .expect("Applied write releases the latest target.");
    assert_eq!(
      request.mutation,
      NativeMutation::Frame(sync.desired.rect.clone())
    );
  }

  /// Writes again once an application has left a write unapplied for
  /// `APPLY_TIMEOUT`.
  #[test]
  fn bounds_unapplied_frame_writes() {
    let (mut sync, observed) = fixture();
    let now = Instant::now();
    advance(&mut sync);
    let request = sync.next(&observed, now).expect("First write.");
    sync.accepted(&request, now);
    advance(&mut sync);
    let early =
      now + APPLY_TIMEOUT.saturating_sub(Duration::from_millis(1));
    assert!(sync.next(&observed, early).is_none());
    assert!(sync.next(&observed, now + APPLY_TIMEOUT).is_some());
  }

  /// An application that clamps a write has still applied it.
  #[test]
  fn clamped_frame_write_counts_as_applied() {
    let (mut sync, observed) = fixture();
    let now = Instant::now();
    advance(&mut sync);
    let request = sync.next(&observed, now).expect("First write.");
    sync.accepted(&request, now);
    advance(&mut sync);
    let clamped = ObservedFrame {
      rect: observed.rect.translate_to_coordinates(
        observed.rect.left + 3,
        observed.rect.top,
      ),
      ..observed
    };
    assert!(sync.next(&clamped, now).is_some());
  }

  /// State requests are not held behind an unapplied frame write.
  #[test]
  fn state_requests_skip_unapplied_frame_writes() {
    let (mut sync, mut observed) = fixture();
    let now = Instant::now();
    advance(&mut sync);
    let request = sync.next(&observed, now).expect("First write.");
    sync.accepted(&request, now);
    sync.desired.state = NativeState::Minimized;
    sync.restart();
    observed.state = NativeState::Normal;
    let request = sync.next(&observed, now).expect("Minimize request.");
    assert_eq!(request.mutation, NativeMutation::Minimize);
  }

  /// Never executes stale accepted operations.
  #[test]
  fn skips_stale_executor_calls() {
    let (mut sync, mut observed) = fixture();
    observed.rect.left += 100;
    let now = Instant::now();
    let request = sync.next(&observed, now).expect("Request.");
    sync.suspend();
    let mut calls = Vec::new();
    assert!(!sync
      .apply(&request, now, |mutation| {
        calls.push(mutation.clone());
        Ok(())
      })
      .expect("Stale rejection."));
    assert_eq!(calls, [] as [NativeMutation; 0]);
  }

  /// Keeps executor failures distinct from convergence.
  #[test]
  fn records_executor_failure() {
    let (mut sync, mut observed) = fixture();
    observed.rect.left += 100;
    let now = Instant::now();
    let request = sync.next(&observed, now).expect("Request.");
    assert!(sync
      .apply(&request, now, |_| anyhow::bail!("Injected failure."))
      .is_err());
    assert_eq!(sync.phase, ReconcilePhase::Failed);
    assert_eq!(sync.constraint(&observed, Some((500, 300)), now), None);
  }

  /// Detects position-only divergence.
  #[test]
  fn verifies_position() {
    let (mut sync, mut observed) = fixture();
    observed.rect = observed.rect.translate_to_coordinates(200, 200);
    assert!(sync.next(&observed, Instant::now()).is_some());
  }

  /// State acknowledgements advance dependent geometry without a timeout.
  #[test]
  fn acknowledged_restore_enables_geometry_immediately() {
    let (mut sync, mut observed) = fixture();
    let now = Instant::now();
    observed.state = NativeState::Minimized;
    observed.rect.left -= 100;
    let restore = sync.next(&observed, now).expect("Restore request.");
    sync.accepted(&restore, now);
    assert_eq!(sync.deadline(), Some(now + RETRY_WAIT));
    observed.state = NativeState::Normal;
    sync.observe(&observed, now);
    let request = sync
      .next(&observed, now)
      .expect("Acknowledged restore enables placement.");
    assert!(matches!(request.mutation, NativeMutation::Frame(_)));
    sync.accepted(&request, now);
    assert!(sync.next(&observed, now).is_none());
    observed.rect = sync.desired.rect.clone();
    sync.observe(&observed, now);
    assert_eq!(sync.phase, ReconcilePhase::Converged);
    assert_eq!(sync.deadline(), None);
  }

  /// A frame write is read back from geometry alone, keeping the state
  /// seen before it; every state write is read back in full.
  #[test]
  fn plans_the_read_after_a_write() {
    let (_, mut before) = fixture();
    before.state = NativeState::Maximized;
    let rect = Rect::from_xy(0, 0, 10, 10);

    let frame = ObservePlan::after_write(
      &NativeMutation::Frame(rect.clone()),
      &before,
      true,
    );
    assert_eq!(
      frame,
      ObservePlan::Geometry {
        state: NativeState::Maximized
      }
    );

    for mutation in [
      NativeMutation::Restore(rect.clone()),
      NativeMutation::Minimize,
      NativeMutation::Maximize,
    ] {
      let plan = ObservePlan::after_write(&mutation, &before, true);
      assert_eq!(plan, ObservePlan::Full, "{mutation:?}");
    }

    // Where a state read is cheap there is nothing to skip.
    assert_eq!(
      ObservePlan::after_write(
        &NativeMutation::Frame(rect),
        &before,
        false
      ),
      ObservePlan::Full
    );
  }

  /// A read taken before the application applied a frame write leaves the
  /// request in flight; the applied frame converges it.
  #[test]
  fn stale_read_after_a_frame_write_stays_accepted() {
    let (mut sync, observed) = fixture();
    let now = Instant::now();
    advance(&mut sync);
    let request = sync.next(&observed, now).expect("Frame write.");
    sync.accepted(&request, now);

    // The window server still shows the old frame, with the state carried
    // over from before the write.
    sync.observe(&observed, now);
    assert_eq!(sync.phase, ReconcilePhase::Accepted);
    assert!(sync.in_flight());
    assert_eq!(sync.deadline(), Some(now + RETRY_WAIT));

    let applied = ObservedFrame {
      rect: sync.desired.rect.clone(),
      ..observed
    };
    sync.observe(&applied, now + Duration::from_millis(20));
    assert_eq!(sync.phase, ReconcilePhase::Converged);
    assert_eq!(sync.deadline(), None);
  }

  /// Only a frame write that is accepted and not yet seen applied counts,
  /// and a retarget forgets it.
  #[test]
  fn tracks_an_accepted_frame_write_until_it_is_confirmed() {
    let (mut sync, observed) = fixture();
    assert!(!sync.frame_write_accepted());
    assert_eq!(sync.observed_state(), None);

    let now = Instant::now();
    advance(&mut sync);
    let request = sync.next(&observed, now).expect("Frame write.");
    assert!(!sync.frame_write_accepted());
    sync.accepted(&request, now);
    assert!(sync.frame_write_accepted());
    assert_eq!(sync.observed_state(), Some(NativeState::Normal));

    advance(&mut sync);
    assert!(!sync.frame_write_accepted());

    let request = sync
      .next(&observed, now + RETRY_WAIT)
      .expect("Frame write.");
    sync.accepted(&request, now + RETRY_WAIT);
    assert!(sync.frame_write_accepted());
    let applied = ObservedFrame {
      rect: sync.desired.rect.clone(),
      ..observed
    };
    sync.observe(&applied, now + RETRY_WAIT);
    assert_eq!(sync.phase, ReconcilePhase::Converged);
    assert!(!sync.frame_write_accepted());
  }

  /// A state change in flight is not a frame write.
  #[test]
  fn a_restore_is_not_an_accepted_frame_write() {
    let (mut sync, mut observed) = fixture();
    observed.state = NativeState::Maximized;
    let now = Instant::now();
    let request = sync.next(&observed, now).expect("Restore request.");
    sync.accepted(&request, now);
    assert_eq!(sync.phase, ReconcilePhase::Accepted);
    assert!(!sync.frame_write_accepted());
    assert_eq!(sync.observed_state(), Some(NativeState::Maximized));
  }

  /// Position refusals cannot manufacture a size constraint.
  #[test]
  fn ignores_position_only_refusals_when_learning_minimums() {
    let (mut sync, mut observed) = fixture();
    observed.rect = observed.rect.translate_to_coordinates(500, 500);
    let now = Instant::now();
    for step in 0..MAX_ATTEMPTS {
      let at = now + RETRY_WAIT * u32::from(step);
      let request = sync.next(&observed, at).expect("Request.");
      sync.accepted(&request, at);
    }
    let at = now + RETRY_WAIT * u32::from(MAX_ATTEMPTS);
    assert!(sync.next(&observed, at).is_none());
    assert_eq!(sync.constraint(&observed, None, at), None);
    observed.rect = sync.desired.rect.clone();
    sync.observe(&observed, at);
    assert_eq!(sync.phase, ReconcilePhase::Converged);
    assert_eq!(sync.deadline(), None);
  }

  /// Drift after convergence spends the same bounded budget.
  #[test]
  fn budgets_drift_after_convergence() {
    let (mut sync, mut observed) = fixture();
    let now = Instant::now();
    assert!(sync.next(&observed, now).is_none());
    assert_eq!(sync.phase, ReconcilePhase::Converged);
    observed.rect.right += 100;
    let mut writes = 0;
    for step in 1..=MAX_ATTEMPTS + 2 {
      let at = now + RETRY_WAIT * u32::from(step);
      if let Some(request) = sync.next(&observed, at) {
        sync.accepted(&request, at);
        writes += 1;
      }
    }
    assert_eq!(writes, MAX_ATTEMPTS);
    assert_eq!(sync.phase, ReconcilePhase::Failed);

    // It rests, and is owed one more round when the rest is over.
    let failed_at = now + RETRY_WAIT * u32::from(MAX_ATTEMPTS + 1);
    assert!(sync.resting());
    assert_eq!(sync.deadline(), Some(failed_at + REST));
    assert!(sync.next(&observed, failed_at + REST / 2).is_none());

    let mut writes = 0;
    for step in 0..=MAX_ATTEMPTS + 1 {
      let at = failed_at + REST + RETRY_WAIT * u32::from(step);
      if let Some(request) = sync.next(&observed, at) {
        sync.accepted(&request, at);
        writes += 1;
      }
    }
    assert_eq!(writes, MAX_ATTEMPTS);

    // After that it has failed for good.
    assert_eq!(sync.phase, ReconcilePhase::Failed);
    assert!(!sync.resting());
    assert_eq!(sync.deadline(), None);
    assert!(sync.next(&observed, failed_at + REST * 4).is_none());
  }

  /// A window that an application placed late, after its frame failed,
  /// converges without another write once it stands where it should.
  #[test]
  fn a_failed_frame_converges_if_the_window_arrives_late() {
    let (mut sync, mut observed) = fixture();
    let target = observed.rect.clone();
    observed.rect.right += 100;
    let now = Instant::now();
    for step in 0..=MAX_ATTEMPTS {
      let at = now + RETRY_WAIT * u32::from(step);
      if let Some(request) = sync.next(&observed, at) {
        sync.accepted(&request, at);
      }
    }
    assert_eq!(sync.phase, ReconcilePhase::Failed);

    observed.rect = target;
    assert!(sync.next(&observed, now + REST * 2).is_none());
    assert_eq!(sync.phase, ReconcilePhase::Converged);
  }

  /// Never positions natively minimized windows.
  #[test]
  fn respects_minimized_state() {
    let (mut sync, mut observed) = fixture();
    sync.desired.state = NativeState::Minimized;
    observed.state = NativeState::Minimized;
    observed.rect.left = -32000;
    assert!(sync.next(&observed, Instant::now()).is_none());
  }

  /// Leaves native maximization geometry untouched.
  #[test]
  fn respects_maximized_geometry() {
    let (mut sync, mut observed) = fixture();
    sync.desired.state = NativeState::Maximized;
    observed.state = NativeState::Maximized;
    observed.rect = Rect::from_xy(-8, -8, 1936, 1056);
    assert!(sync.next(&observed, Instant::now()).is_none());
  }

  /// Frames a window that took the maximized state and kept its size.
  #[test]
  fn frames_a_maximize_that_did_not_fill() {
    let (mut sync, mut observed) = fixture();
    sync.desired.state = NativeState::Maximized;
    sync.desired.rect = sync.desired.monitor.clone();
    observed.state = NativeState::Maximized;
    observed.rect = Rect::from_xy(16, 52, 1888, 976);
    let request = sync
      .next(&observed, Instant::now())
      .expect("Frame request.");
    assert_eq!(
      request.mutation,
      NativeMutation::Frame(sync.desired.monitor.clone())
    );

    observed.rect = sync.desired.monitor.clone();
    assert!(sync.converged(&observed));
  }

  /// Writes a frame for `observed` at `now` and returns the request's
  /// outcome as the window server reports it straight after.
  fn write(
    sync: &mut FrameReconciler,
    observed: &ObservedFrame,
    now: Instant,
  ) {
    let request = sync.next(observed, now).expect("Frame request.");
    sync.accepted(&request, now);
    sync.observe(observed, now);
  }

  /// A window that keeps 8px of height it was asked to give up, as kitty
  /// does at full height, stands for its frame after one retry wait.
  #[test]
  fn accepts_a_rounded_size_after_the_retry_wait() {
    let (mut sync, mut observed) = fixture();
    observed.rect.bottom += 8;
    let now = Instant::now();
    write(&mut sync, &observed, now);
    assert_eq!(sync.phase, ReconcilePhase::Accepted);
    assert!(sync.arrived(&observed));

    // Before the wait is over the application may still be laying out.
    assert!(sync.next(&observed, now + RETRY_WAIT / 2).is_none());
    assert_eq!(sync.phase, ReconcilePhase::Accepted);

    assert!(sync.next(&observed, now + RETRY_WAIT).is_none());
    assert_eq!(sync.phase, ReconcilePhase::Converged);
    assert!(sync.converged(&observed));

    // Only the frame it answered with is accepted, not any near one.
    observed.rect.bottom += 8;
    assert!(!sync.converged(&observed));
  }

  /// A window that starts near its target has not been asked yet.
  #[test]
  fn writes_before_accepting_a_near_frame() {
    let (mut sync, mut observed) = fixture();
    observed.rect.right += 8;
    assert!(!sync.arrived(&observed));
    assert!(sync.next(&observed, Instant::now()).is_some());
  }

  /// A window that drifts after converging is asked again, even within
  /// the slack, rather than accepted where it drifted to.
  #[test]
  fn writes_again_for_drift_within_the_slack() {
    let (mut sync, mut observed) = fixture();
    observed.rect.right += 100;
    let now = Instant::now();
    write(&mut sync, &observed, now);
    observed.rect.right -= 100;
    sync.observe(&observed, now);
    assert_eq!(sync.phase, ReconcilePhase::Converged);

    observed.rect.right += 8;
    assert!(sync.next(&observed, now + RETRY_WAIT * 4).is_some());
  }

  /// Past the slack the window is refusing, and is asked again.
  #[test]
  fn retries_a_miss_past_the_slack() {
    let (mut sync, mut observed) = fixture();
    observed.rect.bottom += 100;
    let now = Instant::now();
    write(&mut sync, &observed, now);
    assert!(!sync.arrived(&observed));
    assert!(sync.next(&observed, now + RETRY_WAIT).is_some());
  }

  /// A window out of place has not arrived, whatever its size.
  #[test]
  fn has_not_arrived_out_of_place() {
    let (mut sync, mut observed) = fixture();
    observed.rect = observed.rect.translate_to_coordinates(500, 400);
    write(&mut sync, &observed, Instant::now());
    assert!(!sync.in_place(&observed));
    assert!(!sync.arrived(&observed));
  }

  /// A new target forgets the last observation but not the state seen.
  #[test]
  fn keeps_the_last_state_across_a_restart() {
    let (mut sync, observed) = fixture();
    assert_eq!(sync.last_state(), None);

    sync.observe(&observed, Instant::now());
    sync.suspend();
    assert_eq!(sync.observed_state(), None);
    assert_eq!(sync.last_state(), Some(NativeState::Normal));
  }

  /// The state is not read again while a frame write is outstanding.
  #[test]
  fn assumes_the_state_while_a_frame_write_is_outstanding() {
    let (mut sync, mut observed) = fixture();
    let now = Instant::now();
    assert_eq!(sync.awaited_state(now), None);

    observed.rect.left += 100;
    write(&mut sync, &observed, now);
    assert_eq!(sync.awaited_state(now), Some(NativeState::Normal));
    assert_eq!(sync.awaited_state(now + RETRY_WAIT), None);

    observed.rect.left -= 100;
    sync.observe(&observed, now);
    assert_eq!(sync.awaited_state(now), None);
  }

  /// Suspends all mutations during interactive control.
  #[test]
  fn cancels_for_drag() {
    let (mut sync, mut observed) = fixture();
    observed.rect.left += 100;
    sync.suspend();
    assert!(sync.next(&observed, Instant::now()).is_none());
  }

  /// Learns stable per-axis refusals after bounded retries.
  #[test]
  fn bounds_unresponsive_requests() {
    let (mut sync, mut observed) = fixture();
    observed.rect.right += 100;
    let now = Instant::now();
    for step in 0..MAX_ATTEMPTS {
      let at = now + RETRY_WAIT * u32::from(step);
      let request = sync.next(&observed, at).expect("Retry request.");
      sync.accepted(&request, at);
    }
    let at = now + RETRY_WAIT * 4;
    assert!(sync.next(&observed, at).is_none());
    assert_eq!(sync.phase, ReconcilePhase::Failed);
    assert_eq!(sync.constraint(&observed, None, at), Some((500, 0)));
    assert_eq!(
      sync.constraint(&observed, Some((500, 300)), at),
      Some((500, 0))
    );
    assert_eq!(sync.constraint(&observed, Some((700, 300)), at), None);
    observed.dpi = 144;
    assert_eq!(sync.constraint(&observed, Some((500, 300)), at), None);
  }

  /// A window that keeps a few pixels and is also out of place fails,
  /// but the pixels it kept are a rounded size and not a floor.
  #[test]
  fn does_not_learn_a_floor_from_a_rounded_size() {
    let (mut sync, mut observed) = fixture();
    observed.rect = Rect::from_xy(40, 20, 400, 308);
    let now = Instant::now();
    for step in 0..MAX_ATTEMPTS {
      let at = now + RETRY_WAIT * u32::from(step);
      let request = sync.next(&observed, at).expect("Retry request.");
      sync.accepted(&request, at);
    }
    let at = now + RETRY_WAIT * 4;
    assert!(sync.next(&observed, at).is_none());
    assert_eq!(sync.phase, ReconcilePhase::Failed);
    assert_eq!(sync.constraint(&observed, None, at), None);
  }

  /// Creates a target parked below a 1080px display, with its clamp.
  fn parked_fixture(clamp: i32) -> (FrameReconciler, ObservedFrame) {
    let (mut sync, mut observed) = fixture();
    sync.desired.rect = Rect::from_xy(-399, 1079, 400, 300);
    sync.desired.parking_clamp = Some(clamp);
    observed.rect = sync.desired.rect.clone();
    (sync, observed)
  }

  /// Accepts a parked window the platform held below its title bar.
  #[test]
  fn converges_on_clamped_parking() {
    let (mut sync, mut observed) = parked_fixture(55);
    observed.rect = observed.rect.translate_to_coordinates(-399, 1027);
    assert!(sync.next(&observed, Instant::now()).is_none());
    assert_eq!(sync.phase, ReconcilePhase::Converged);
  }

  /// Still corrects a parked window lifted beyond the clamp.
  #[test]
  fn rejects_parking_beyond_clamp() {
    let (mut sync, mut observed) = parked_fixture(55);
    observed.rect = observed.rect.translate_to_coordinates(-399, 1020);
    assert!(sync.next(&observed, Instant::now()).is_some());
  }

  /// The clamp is vertical only; horizontal drift is still corrected.
  #[test]
  fn rejects_parked_horizontal_drift() {
    let (mut sync, mut observed) = parked_fixture(55);
    observed.rect = observed.rect.translate_to_coordinates(-390, 1079);
    assert!(sync.next(&observed, Instant::now()).is_some());
  }

  /// Accepts a title bar taller than any fixed allowance, clamped by the
  /// window's own height (Spotify's toolbar holds it 63px up).
  #[test]
  fn converges_on_tall_title_bar() {
    let (mut sync, mut observed) = parked_fixture(300);
    observed.rect = observed.rect.translate_to_coordinates(-399, 1016);
    assert!(sync.next(&observed, Instant::now()).is_none());
    assert_eq!(sync.phase, ReconcilePhase::Converged);
  }

  /// A window left at its tile's height never reached the parking row,
  /// even with a height-sized clamp.
  #[test]
  fn rejects_unapplied_vertical_parking() {
    let (mut sync, mut observed) = parked_fixture(300);
    observed.rect = observed.rect.translate_to_coordinates(-399, 44);
    assert!(sync.next(&observed, Instant::now()).is_some());
  }

  /// Without a clamp, parking matches as exactly as any other frame.
  #[test]
  fn exact_parking_without_clamp() {
    let (mut sync, mut observed) = parked_fixture(0);
    observed.rect = observed.rect.translate_to_coordinates(-399, 1060);
    assert!(sync.next(&observed, Instant::now()).is_some());
  }
}
