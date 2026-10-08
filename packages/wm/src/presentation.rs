use std::time::{Duration, Instant};

/// Maximum time to retain a prepared presentation without readiness.
///
/// Also bounds an overlay whose creation never completes.
pub const PREPARATION_LIMIT: Duration = Duration::from_millis(750);

/// Whether a stationary overlay has been created and composed.
///
/// A source is concealed only behind a cover that has been presented, so
/// the gate says when that is true. Creation can run on another thread, so
/// the gate is refreshed from the platform each pass; see
/// `MotionPreparation::refresh_overlay`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverlayGate {
  /// The overlay was already on screen before this preparation, such as a
  /// retained overlay being retargeted. It waits for no compositor frame.
  Presented,
  /// The platform has not finished creating the overlay. Nothing is known
  /// about when it will be composed, so it never counts as ready.
  Creating,
  /// The overlay was shown when the compositor was at this frame. It is
  /// composed only in a frame after it.
  CreatedAt(u64),
}

impl OverlayGate {
  /// Whether the overlay has been composed by compositor frame `frame`.
  ///
  /// Requires an actual compositor notification: a frame later than the
  /// one the overlay was shown at.
  #[must_use]
  pub fn ready(self, frame: u64) -> bool {
    match self {
      Self::Presented => true,
      Self::Creating => false,
      Self::CreatedAt(created) => frame > created,
    }
  }

  /// The compositor frame readiness waits to pass, if it is known.
  fn frame(self) -> Option<u64> {
    match self {
      Self::CreatedAt(created) => Some(created),
      Self::Presented | Self::Creating => None,
    }
  }
}

/// Whether the mutations asked of an overlay have reached the screen.
///
/// Where overlay work is queued to another thread, a resize or a frame
/// asked for is not yet drawn when the call returns. The cover is
/// trustworthy only once what was asked of it is applied and a compositor
/// frame has passed since.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CoverSync {
  /// Nothing queued is outstanding.
  #[default]
  Synced,
  /// Work is queued and not yet applied.
  Queued,
  /// All queued work was applied when the compositor was at this frame.
  /// It is composed in any later frame.
  AppliedAt(u64),
}

impl CoverSync {
  /// Whether everything asked of the cover has been composed by
  /// compositor frame `frame`.
  #[must_use]
  pub fn ready(self, frame: u64) -> bool {
    match self {
      Self::Synced => true,
      Self::Queued => false,
      Self::AppliedAt(applied) => frame > applied,
    }
  }

  /// The compositor frame readiness waits to pass, if it is known.
  fn frame(self) -> Option<u64> {
    match self {
      Self::AppliedAt(applied) => Some(applied),
      Self::Synced | Self::Queued => None,
    }
  }
}

/// Readiness of a stationary overlay, independent of motion and
/// visibility.
pub struct MotionPreparation {
  pub batch: u64,
  pub deadline: Instant,
  /// Whether the overlay is created and composed.
  overlay: OverlayGate,
  /// Whether what was asked of the overlay since (a resize, a held start
  /// frame) is drawn. Refreshed each pass.
  cover: CoverSync,
  /// The compositor frame sampled before the overlay was requested. A
  /// creation that reports an earlier frame cannot have been composed
  /// before it.
  requested_at: u64,
  /// Existing content needs a presented replacement. A new window starts
  /// invisible, so retaining its source would flash before the fade-in.
  requires_cover: bool,
  geometry: Option<(u64, u64)>,
}

impl MotionPreparation {
  /// Starts a preparation whose overlay is in the state `overlay`.
  ///
  /// `overlay` is `OverlayGate::Presented` when a retained overlay is
  /// retargeted: it is already on screen, so it waits for no new
  /// boundary.
  pub fn new(
    batch: u64,
    overlay: OverlayGate,
    now: Instant,
    requires_cover: bool,
  ) -> Self {
    Self {
      batch,
      deadline: now + PREPARATION_LIMIT,
      overlay,
      cover: CoverSync::Synced,
      requested_at: 0,
      requires_cover,
      geometry: None,
    }
  }

  /// Records the compositor frame sampled before the overlay was
  /// requested.
  ///
  /// The overlay's own report is raised to it, which only matters where a
  /// platform reports no frame of its own.
  #[must_use]
  pub fn requested_at(mut self, frame: u64) -> Self {
    self.requested_at = frame;
    self
  }

  /// Takes the platform's report of a creating overlay.
  ///
  /// `shown` is the compositor frame the overlay was shown at, or `None`
  /// while it is still being created. A presented or already created
  /// overlay is not changed: creation completes once.
  pub fn refresh_overlay(&mut self, shown: Option<u64>) {
    if self.overlay == OverlayGate::Creating {
      if let Some(frame) = shown {
        self.overlay =
          OverlayGate::CreatedAt(frame.max(self.requested_at));
      }
    }
  }

  /// Takes the platform's report of how much of what was asked of the
  /// overlay is drawn.
  ///
  /// A source is concealed, and motion released, only behind a cover that
  /// shows where it was told to be.
  pub fn refresh_cover(&mut self, cover: CoverSync) {
    self.cover = cover;
  }

  /// The state of the overlay.
  #[must_use]
  pub fn overlay(&self) -> OverlayGate {
    self.overlay
  }

  /// Requires an actual compositor notification before replacing the
  /// source, after the overlay is created and everything asked of it is
  /// drawn.
  pub fn overlay_ready(&self, frame: u64) -> bool {
    self.overlay.ready(frame) && self.cover.ready(frame)
  }

  /// Keeps existing content visible until its replacement is presented.
  /// Opening sources can hide immediately without releasing motion early.
  pub fn retains_source(&self, frame: u64) -> bool {
    self.requires_cover && !self.overlay_ready(frame)
  }

  /// Invalidates readiness whenever placement changes or diverges.
  pub fn observe(&mut self, generation: u64, converged: bool, frame: u64) {
    if !converged {
      self.geometry = None;
    } else if self.geometry.is_none_or(|(old, _)| old != generation) {
      self.geometry = Some((generation, frame));
    }
  }

  /// The lowest compositor frame this preparation still waits to pass,
  /// among those at or above `from`.
  ///
  /// Readiness needs a frame later than the overlay's, and one later than
  /// the frame that first saw the current geometry converged. A gate
  /// below `from` was passed by the pass that ran at `from`, so it holds
  /// nothing up. Over-reporting is harmless: a stale geometry gate only
  /// wakes one pass that finds nothing to do.
  ///
  /// A creating overlay, or one with queued work, has no frame gate yet.
  /// Its completion wakes the window manager, and `deadline` bounds the
  /// wait.
  pub fn frame_gate(&self, from: u64) -> Option<u64> {
    [
      self.overlay.frame(),
      self.cover.frame(),
      self.geometry.map(|(_, after)| after),
    ]
    .into_iter()
    .flatten()
    .filter(|gate| *gate >= from)
    .min()
  }

  /// Starts only after current geometry and a subsequent compositor
  /// boundary.
  pub fn ready(&self, generation: u64, frame: u64) -> bool {
    self.overlay_ready(frame)
      && self.geometry.is_some_and(|(observed, after)| {
        observed == generation && frame > after
      })
  }
}

/// Source visibility ownership survives motion completion and failed
/// recovery.
#[derive(Default)]
pub struct SourceLease {
  pub restoring: bool,
  /// Whether a parked source has been sent to its destination while its
  /// cover is still moving, because the cover now stands over it.
  ///
  /// The application then lays out at its new size during the last of
  /// the motion instead of after it. Cleared with `restoring` when a new
  /// motion takes the source over.
  pub landing: bool,
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Builds a preparation whose overlay was shown at `frame`.
  fn created_at(frame: u64, requires_cover: bool) -> MotionPreparation {
    MotionPreparation::new(
      1,
      OverlayGate::CreatedAt(frame),
      Instant::now(),
      requires_cover,
    )
  }

  /// Builds a preparation whose overlay is still being created.
  fn creating(requires_cover: bool) -> MotionPreparation {
    MotionPreparation::new(
      1,
      OverlayGate::Creating,
      Instant::now(),
      requires_cover,
    )
  }

  /// Builds a preparation whose overlay is already on screen.
  fn presented(requires_cover: bool) -> MotionPreparation {
    MotionPreparation::new(
      1,
      OverlayGate::Presented,
      Instant::now(),
      requires_cover,
    )
  }

  #[test]
  fn elapsed_time_never_establishes_readiness() {
    let now = Instant::now();
    let mut preparation =
      MotionPreparation::new(1, OverlayGate::CreatedAt(10), now, true);
    preparation.observe(1, true, 10);
    assert!(now + Duration::from_secs(1) > preparation.deadline);
    assert!(!preparation.overlay_ready(10));
    assert!(!preparation.ready(1, 10));
    assert!(preparation.ready(1, 11));
  }

  #[test]
  fn newer_geometry_requires_a_new_presentation_boundary() {
    let mut preparation = created_at(10, true);
    preparation.observe(1, true, 11);
    assert!(!preparation.ready(1, 11));
    assert!(preparation.ready(1, 12));
    assert!(!preparation.ready(2, 12));
    preparation.observe(2, true, 12);
    assert!(!preparation.ready(2, 12));
    assert!(preparation.ready(2, 13));
    preparation.observe(2, false, 13);
    assert!(!preparation.ready(2, 14));
  }

  #[test]
  fn opening_hides_before_first_frame_but_waits_for_placement() {
    let mut preparation = created_at(10, false);
    assert!(!preparation.retains_source(10));
    assert!(!preparation.ready(1, 10));
    assert!(!preparation.ready(1, 11));
    preparation.observe(1, true, 11);
    assert!(!preparation.ready(1, 11));
    assert!(preparation.ready(1, 12));
    preparation.observe(2, false, 12);
    assert!(!preparation.retains_source(12));
    assert!(!preparation.ready(2, 13));
  }

  #[test]
  fn existing_content_stays_visible_until_covered() {
    let preparation = created_at(10, true);
    assert!(preparation.retains_source(9));
    assert!(preparation.retains_source(10));
    assert!(!preparation.retains_source(11));
    assert!(!preparation.ready(1, 11));
  }

  /// A retarget keeps its cover and still waits for settled geometry.
  #[test]
  fn retained_overlay_waits_only_for_geometry() {
    let mut preparation = presented(true);
    assert!(preparation.overlay_ready(10));
    assert!(!preparation.retains_source(10));
    assert!(!preparation.ready(1, 10));
    preparation.observe(1, true, 10);
    assert!(!preparation.ready(1, 10));
    assert!(preparation.ready(1, 11));
  }

  /// A gate is ready exactly when the overlay is on screen, whatever the
  /// frame: never while creating, and only after the frame it was shown
  /// at once created.
  #[test]
  fn overlay_gate_is_ready_only_once_composed() {
    for frame in 0..40 {
      assert!(OverlayGate::Presented.ready(frame));
      assert!(!OverlayGate::Creating.ready(frame));
      for created in 0..40 {
        assert_eq!(
          OverlayGate::CreatedAt(created).ready(frame),
          frame > created
        );
      }
    }
  }

  /// Existing content is never released from its source before the
  /// overlay covering it is composed, in any state of the gate.
  #[test]
  fn a_source_is_retained_until_its_cover_is_presented() {
    for frame in 0..40 {
      // Not yet created: held at every frame, however late.
      assert!(creating(true).retains_source(frame));
      assert!(!creating(true).ready(1, frame));
      for created in 0..40 {
        let preparation = created_at(created, true);
        assert_eq!(preparation.retains_source(frame), frame <= created);
        // Whatever holds the source holds the batch.
        if preparation.retains_source(frame) {
          assert!(!preparation.ready(1, frame));
        }
      }
      assert!(!presented(true).retains_source(frame));
    }
  }

  /// A creation that is not complete keeps motion held, and an opening
  /// source, which has nothing on screen to protect, is not retained.
  #[test]
  fn creating_holds_motion_but_not_an_opening_source() {
    let mut opening = creating(false);
    opening.observe(1, true, 5);
    assert!(!opening.retains_source(5));
    assert!(!opening.ready(1, 6));
    // Only the geometry gate is known while the overlay is created.
    assert_eq!(opening.frame_gate(0), Some(5));
  }

  /// Completion moves a creating overlay to the frame it was shown at,
  /// once, and a later report changes nothing.
  #[test]
  fn completion_gates_on_the_frame_the_overlay_was_shown_at() {
    let mut preparation = creating(true);
    assert_eq!(preparation.overlay(), OverlayGate::Creating);
    preparation.refresh_overlay(None);
    assert_eq!(preparation.overlay(), OverlayGate::Creating);
    assert!(preparation.frame_gate(0).is_none());

    preparation.refresh_overlay(Some(20));
    assert_eq!(preparation.overlay(), OverlayGate::CreatedAt(20));
    assert!(preparation.retains_source(20));
    assert!(!preparation.retains_source(21));
    assert_eq!(preparation.frame_gate(0), Some(20));

    preparation.refresh_overlay(Some(5));
    preparation.refresh_overlay(None);
    assert_eq!(preparation.overlay(), OverlayGate::CreatedAt(20));
  }

  /// A platform that reports no frame of its own (zero) is gated on the
  /// frame sampled before the overlay was requested, as a synchronous
  /// creation always was.
  #[test]
  fn a_report_never_precedes_the_request() {
    let mut preparation = creating(true).requested_at(30);
    preparation.refresh_overlay(Some(0));
    assert_eq!(preparation.overlay(), OverlayGate::CreatedAt(30));
    assert!(preparation.retains_source(30));
    assert!(!preparation.retains_source(31));
  }

  /// Queued work on the cover holds the source and the batch until it is
  /// applied and a later frame has passed, whatever the overlay's gate.
  #[test]
  fn queued_cover_work_holds_the_source_until_drawn_and_composed() {
    for frame in 0..30 {
      let mut preparation = presented(true);
      preparation.refresh_cover(CoverSync::Queued);
      assert!(preparation.retains_source(frame));
      assert!(!preparation.ready(1, frame));
      assert_eq!(preparation.frame_gate(0), None);

      for applied in 0..30 {
        preparation.refresh_cover(CoverSync::AppliedAt(applied));
        assert_eq!(preparation.retains_source(frame), frame <= applied);
      }
      preparation.refresh_cover(CoverSync::AppliedAt(12));
      assert_eq!(preparation.frame_gate(0), Some(12));

      preparation.refresh_cover(CoverSync::Synced);
      assert!(!preparation.retains_source(frame));
    }
  }

  /// A cover is as late as the later of its creation and its queued work.
  #[test]
  fn creation_and_queued_work_both_gate_readiness() {
    let mut preparation = created_at(10, true);
    preparation.refresh_cover(CoverSync::AppliedAt(15));
    assert!(preparation.retains_source(15));
    assert!(!preparation.retains_source(16));
    preparation.refresh_cover(CoverSync::AppliedAt(5));
    assert!(preparation.retains_source(10));
    assert!(!preparation.retains_source(11));
    assert_eq!(preparation.frame_gate(0), Some(5));
  }

  /// A presented overlay is not gated again by a refresh.
  #[test]
  fn a_presented_overlay_stays_presented() {
    let mut preparation = presented(true);
    preparation.refresh_overlay(Some(50));
    assert_eq!(preparation.overlay(), OverlayGate::Presented);
  }

  /// A creation that never completes cannot hold a preparation past its
  /// deadline: the deadline is set when it starts, whatever the gate.
  #[test]
  fn a_creation_that_never_completes_is_bounded_by_the_deadline() {
    let now = Instant::now();
    let preparation =
      MotionPreparation::new(1, OverlayGate::Creating, now, true);
    assert_eq!(preparation.deadline, now + PREPARATION_LIMIT);
    assert!(!preparation.overlay_ready(u64::MAX));
  }
}
