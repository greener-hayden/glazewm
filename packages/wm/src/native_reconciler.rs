use std::time::{Duration, Instant};

use wm_platform::Rect;

const RETRY_WAIT: Duration = Duration::from_millis(250);
const MAX_ATTEMPTS: u8 = 3;

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
}

impl FrameReconciler {
  /// Schedules only outstanding request recovery, never periodic polling.
  pub fn deadline(&self) -> Option<Instant> {
    match self.phase {
      ReconcilePhase::Pending => Some(Instant::now()),
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
        (!frames_match(&self.desired.rect, &observed.rect)
          || self.desired.dpi != observed.dpi)
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
}
