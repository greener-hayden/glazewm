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
  previous: Option<ObservedFrame>,
  stable_since: Option<Instant>,
  /// The last frame write, until the application is seen to apply it.
  ///
  /// Survives `restart`: a retarget does not recall a write already
  /// queued on the application.
  unapplied: Option<UnappliedWrite>,
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
      previous: None,
      stable_since: None,
      unapplied: None,
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
    self.previous = None;
    self.stable_since = None;
  }

  /// Whether a request is still awaiting its outcome.
  pub fn in_flight(&self) -> bool {
    matches!(
      self.phase,
      ReconcilePhase::Pending | ReconcilePhase::Accepted
    )
  }

  /// Whether reconciliation reached a terminal outcome.
  pub fn settled(&self) -> bool {
    matches!(
      self.phase,
      ReconcilePhase::Converged | ReconcilePhase::Failed
    )
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
  pub fn failed(&mut self) {
    self.phase = ReconcilePhase::Failed;
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

  /// Chooses state before dependent geometry.
  fn mutation(&self, observed: &ObservedFrame) -> Option<NativeMutation> {
    match (self.desired.state, observed.state) {
      (NativeState::Minimized, NativeState::Minimized) => None,
      (NativeState::Minimized, _) => Some(NativeMutation::Minimize),
      (
        NativeState::Normal,
        NativeState::Minimized | NativeState::Maximized,
      ) => Some(NativeMutation::Restore(self.desired.rect.clone())),
      (NativeState::Maximized, NativeState::Maximized) => (!self
        .desired
        .monitor
        .contains_point(&observed.rect.center_point()))
      .then(|| NativeMutation::Restore(self.desired.rect.clone())),
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
          None => frames_match(&self.desired.rect, &observed.rect),
        };
        (!placed || self.desired.dpi != observed.dpi)
          .then(|| NativeMutation::Frame(self.desired.rect.clone()))
      }
    }
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
        None if actual > desired + 1 => actual,
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
    assert!(sync.in_flight() && !sync.settled());
    observed.state = NativeState::Normal;
    assert!(sync.next(&observed, now).is_none());
    assert_eq!(sync.phase, ReconcilePhase::Converged);
    assert!(sync.settled() && !sync.in_flight());
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
    assert_eq!(sync.deadline(), None);
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
