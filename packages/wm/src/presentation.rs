use std::time::{Duration, Instant};

/// Maximum time to retain a prepared presentation without readiness.
const PREPARATION_LIMIT: Duration = Duration::from_millis(750);

/// Readiness of a stationary overlay, independent of motion and
/// visibility.
pub struct MotionPreparation {
  pub batch: u64,
  pub deadline: Instant,
  /// The compositor boundary preceding overlay creation, or `None` when
  /// the overlay was already presented before this preparation.
  overlay_frame: Option<u64>,
  /// Existing content needs a presented replacement. A new window starts
  /// invisible, so retaining its source would flash before the fade-in.
  requires_cover: bool,
  geometry: Option<(u64, u64)>,
}

impl MotionPreparation {
  /// Records the compositor boundary preceding overlay creation.
  ///
  /// `overlay_frame` is `None` when a retained overlay is retargeted: it
  /// is already on screen, so it waits for no new boundary.
  pub fn new(
    batch: u64,
    overlay_frame: Option<u64>,
    now: Instant,
    requires_cover: bool,
  ) -> Self {
    Self {
      batch,
      deadline: now + PREPARATION_LIMIT,
      overlay_frame,
      requires_cover,
      geometry: None,
    }
  }

  /// Requires an actual compositor notification before replacing the
  /// source.
  pub fn overlay_ready(&self, frame: u64) -> bool {
    self.overlay_frame.is_none_or(|created| frame > created)
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
  pub fn frame_gate(&self, from: u64) -> Option<u64> {
    [self.overlay_frame, self.geometry.map(|(_, after)| after)]
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
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn elapsed_time_never_establishes_readiness() {
    let now = Instant::now();
    let mut preparation = MotionPreparation::new(1, Some(10), now, true);
    preparation.observe(1, true, 10);
    assert!(now + Duration::from_secs(1) > preparation.deadline);
    assert!(!preparation.overlay_ready(10));
    assert!(!preparation.ready(1, 10));
    assert!(preparation.ready(1, 11));
  }

  #[test]
  fn newer_geometry_requires_a_new_presentation_boundary() {
    let mut preparation =
      MotionPreparation::new(1, Some(10), Instant::now(), true);
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
    let mut preparation =
      MotionPreparation::new(1, Some(10), Instant::now(), false);
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
    let preparation =
      MotionPreparation::new(1, Some(10), Instant::now(), true);
    assert!(preparation.retains_source(9));
    assert!(preparation.retains_source(10));
    assert!(!preparation.retains_source(11));
    assert!(!preparation.ready(1, 11));
  }

  /// A retarget keeps its cover and still waits for settled geometry.
  #[test]
  fn retained_overlay_waits_only_for_geometry() {
    let mut preparation =
      MotionPreparation::new(1, None, Instant::now(), true);
    assert!(preparation.overlay_ready(10));
    assert!(!preparation.retains_source(10));
    assert!(!preparation.ready(1, 10));
    preparation.observe(1, true, 10);
    assert!(!preparation.ready(1, 10));
    assert!(preparation.ready(1, 11));
  }
}
