//! Platform-independent geometry and timing for companion thumbnails.
//!
//! A companion is a window of another process that decorates a managed
//! window, such as a border ring. While an overlay stands in for the
//! managed window, the overlay draws each companion as a thumbnail
//! beside the window's own, moved by the same transform. While the WM
//! moves the real window, a companion-only overlay draws each companion
//! anchored to the window's edges instead. macOS has no live thumbnails,
//! so its overlay captures the companions into the window's own
//! screenshot. The math lives here so it is testable everywhere.

use std::time::{Duration, Instant};

use crate::Rect;

/// How far a companion may reach past the frame of the window it
/// decorates, and so how far an overlay extends past its path.
///
/// A border ring reaches a few pixels out at 100% and about three times
/// that at 200% with a thick stroke. The margin is transparent, so it
/// costs nothing to compose.
pub const COMPANION_MARGIN_PX: i32 = 32;

/// Whether `candidate`, a window of a decorating process, decorates the
/// window at `frame`.
///
/// For platforms where a companion cannot name its window. A band of a
/// ring lies within the margin around the frame and touches the frame.
/// The ring of a neighbouring window lies within the margin too once the
/// gap between the two is under the margin, but it does not touch the
/// frame unless the gap is under the ring's own reach, where the two
/// rings share pixels on screen anyway.
#[must_use]
pub(crate) fn decorates(candidate: &Rect, frame: &Rect) -> bool {
  frame.inset(-COMPANION_MARGIN_PX).contains_rect(candidate)
    && candidate.intersection_area(&frame.inset(-1)) > 0
}

/// Minimum spacing between two companion searches of one overlay.
pub(crate) const DISCOVERY_INTERVAL: Duration = Duration::from_millis(16);

/// How long a companion found mid-animation takes to fade in.
pub(crate) const FADE_IN: Duration = Duration::from_millis(80);

/// Maps a companion's screen rect through the overlay's transform.
///
/// `frame` is the source's frame in screen coordinates, the region that
/// the overlay draws at `animated`. The companion keeps its offset and
/// size relative to `frame`, scaled by the same factors, so a ring
/// around the window stays around the animated window.
///
/// Returns `None` when `frame` is empty and no transform exists.
#[must_use]
pub(crate) fn companion_rect(
  companion: &Rect,
  frame: &Rect,
  animated: &Rect,
) -> Option<Rect> {
  if frame.width() <= 0 || frame.height() <= 0 {
    return None;
  }
  let x = |value: i32| {
    map_axis(
      value,
      frame.left,
      frame.width(),
      animated.left,
      animated.width(),
    )
  };
  let y = |value: i32| {
    map_axis(
      value,
      frame.top,
      frame.height(),
      animated.top,
      animated.height(),
    )
  };
  Some(Rect::from_ltrb(
    x(companion.left),
    y(companion.top),
    x(companion.right),
    y(companion.bottom),
  ))
}

/// Maps one coordinate from a frame axis onto an animated axis.
///
/// Rounds to the nearest pixel, so an identity transform is exact and a
/// scaled edge lands on the pixel nearest its true position.
fn map_axis(
  value: i32,
  frame_start: i32,
  frame_length: i32,
  start: i32,
  length: i32,
) -> i32 {
  let numerator =
    (i64::from(value) - i64::from(frame_start)) * i64::from(length) * 2
      + i64::from(frame_length);
  let offset = numerator.div_euclid(i64::from(frame_length) * 2);
  saturate(i64::from(start) + offset)
}

/// Maps a companion's rect onto a moved frame by anchoring its edges.
///
/// `companion` and `frame` are snapshots taken together, before the
/// frame moved to `moved`. Each companion edge keeps its offset from the
/// parallel frame edge it was nearer to: its left edge from the frame's
/// left or right edge, and so on. A 4 px band along the top therefore
/// stays 4 px thick while it stretches with the frame's width. This is
/// the mapping for native motion, where the window resizes rather than
/// scales; `companion_rect` is the one for a scaled thumbnail.
///
/// An edge closer to the far side of a shrunken frame than its opposite
/// edge collapses onto it, so the result is never inverted.
///
/// Returns `None` when `frame` or `moved` is empty.
#[must_use]
pub(crate) fn anchored_rect(
  companion: &Rect,
  frame: &Rect,
  moved: &Rect,
) -> Option<Rect> {
  if frame.width() <= 0
    || frame.height() <= 0
    || moved.width() <= 0
    || moved.height() <= 0
  {
    return None;
  }
  let x = |value: i32| {
    anchor_axis(value, frame.left, frame.right, moved.left, moved.right)
  };
  let y = |value: i32| {
    anchor_axis(value, frame.top, frame.bottom, moved.top, moved.bottom)
  };
  let (left, top) = (x(companion.left), y(companion.top));
  Some(Rect::from_ltrb(
    left,
    top,
    x(companion.right).max(left),
    y(companion.bottom).max(top),
  ))
}

/// Moves one coordinate with the nearer of two frame edges.
///
/// A coordinate equally far from both edges follows the start edge.
fn anchor_axis(
  value: i32,
  start: i32,
  end: i32,
  moved_start: i32,
  moved_end: i32,
) -> i32 {
  let value = i64::from(value);
  let (start, end) = (i64::from(start), i64::from(end));
  if (value - start).abs() <= (value - end).abs() {
    saturate(i64::from(moved_start) + value - start)
  } else {
    saturate(i64::from(moved_end) + value - end)
  }
}

/// Clamps a widened coordinate back into `i32`.
fn saturate(value: i64) -> i32 {
  i32::try_from(value).unwrap_or(if value < 0 {
    i32::MIN
  } else {
    i32::MAX
  })
}

/// Opacity factor of a companion found `elapsed` ago, mid-animation.
///
/// Rises linearly from 0 to 1 over [`FADE_IN`]. The result multiplies the
/// animation's own opacity.
#[must_use]
pub(crate) fn fade_in(elapsed: Duration) -> f32 {
  (elapsed.as_secs_f32() / FADE_IN.as_secs_f32()).clamp(0.0, 1.0)
}

/// Spaces companion searches of one overlay at least
/// [`DISCOVERY_INTERVAL`] apart.
#[derive(Debug, Default)]
pub(crate) struct DiscoveryThrottle {
  last: Option<Instant>,
}

impl DiscoveryThrottle {
  /// Claims a search at `now` if the previous one is old enough.
  ///
  /// Returns `true` when the caller should search, and records `now` as
  /// the latest search.
  pub(crate) fn try_begin(&mut self, now: Instant) -> bool {
    if self.last.is_some_and(|last| {
      now.saturating_duration_since(last) < DISCOVERY_INTERVAL
    }) {
      return false;
    }
    self.last = Some(now);
    true
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The four bands of a window's ring decorate it; a neighbour's ring
  /// across a gap and an unrelated window do not.
  #[test]
  fn decorates_pairs_bands_with_their_window() {
    let frame = Rect::from_xy(100, 100, 800, 600);
    for band in [
      Rect::from_ltrb(96, 96, 116, 704),
      Rect::from_ltrb(884, 96, 904, 704),
      Rect::from_ltrb(116, 96, 884, 103),
      Rect::from_ltrb(116, 697, 884, 704),
    ] {
      assert!(decorates(&band, &frame), "{band:?}");
    }
    // The left band of a neighbour 16 px to the right.
    assert!(!decorates(&Rect::from_ltrb(912, 96, 932, 704), &frame));
    // A band of a larger window that encloses this one.
    assert!(!decorates(&Rect::from_ltrb(0, 0, 20, 900), &frame));
  }

  /// A slide moves the ring by the window's translation alone.
  #[test]
  fn slide_translates_companion() {
    let frame = Rect::from_xy(100, 100, 800, 600);
    let animated = Rect::from_xy(-1500, 100, 800, 600);
    let ring = Rect::from_ltrb(96, 96, 904, 100);
    assert_eq!(
      companion_rect(&ring, &frame, &animated),
      Some(Rect::from_ltrb(-1504, 96, -696, 100))
    );
  }

  /// An identity transform leaves the ring where it is.
  #[test]
  fn identity_keeps_companion() {
    let frame = Rect::from_xy(10, 20, 333, 777);
    let ring = Rect::from_ltrb(4, 14, 349, 20);
    assert_eq!(companion_rect(&ring, &frame, &frame), Some(ring));
  }

  /// An opening window scales the ring about the frame, band thickness
  /// included.
  #[test]
  fn open_scales_companion_about_frame() {
    let frame = Rect::from_xy(0, 0, 1000, 800);
    // 85% of the frame, centred on it.
    let animated = Rect::from_xy(75, 60, 850, 680);
    let left_band = Rect::from_ltrb(-20, -20, 0, 820);
    assert_eq!(
      companion_rect(&left_band, &frame, &animated),
      Some(Rect::from_ltrb(58, 43, 75, 757))
    );
  }

  /// An empty frame has no transform.
  #[test]
  fn empty_frame_has_no_transform() {
    let ring = Rect::from_xy(0, 0, 10, 10);
    assert!(
      companion_rect(&ring, &Rect::from_xy(0, 0, 0, 10), &ring).is_none()
    );
  }

  /// A mapped ring is clipped to the overlay like the window is.
  #[test]
  fn mapped_companion_clips_to_overlay() {
    let frame = Rect::from_xy(0, 0, 800, 600);
    let animated = Rect::from_xy(-400, 0, 800, 600);
    let top_band = Rect::from_ltrb(-4, -4, 804, 0);
    let mapped =
      companion_rect(&top_band, &frame, &animated).expect("Mapped band.");
    assert_eq!(mapped, Rect::from_ltrb(-404, -4, 404, 0));
    let monitor = Rect::from_xy(0, -4, 1920, 1084);
    let (destination, source) =
      crate::thumbnail_rects(&mapped, &monitor, (808, 4))
        .expect("Visible band.");
    assert_eq!(destination, Rect::from_ltrb(0, 0, 404, 4));
    assert_eq!(source, Rect::from_ltrb(404, 0, 808, 4));
  }

  /// The four bands of a 4 px ring around `frame`, ordered left, top,
  /// right, bottom.
  fn ring(frame: &Rect) -> [Rect; 4] {
    let (l, t, r, b) = (frame.left, frame.top, frame.right, frame.bottom);
    [
      Rect::from_ltrb(l - 4, t - 4, l, b + 4),
      Rect::from_ltrb(l - 4, t - 4, r + 4, t),
      Rect::from_ltrb(r, t - 4, r + 4, b + 4),
      Rect::from_ltrb(l - 4, b, r + 4, b + 4),
    ]
  }

  /// A translated frame moves every band by the same offset.
  #[test]
  fn anchored_translation_moves_bands() {
    let frame = Rect::from_ltrb(100, 100, 900, 700);
    let moved = Rect::from_ltrb(150, 80, 950, 680);
    for (band, expected) in ring(&frame).iter().zip(ring(&moved)) {
      assert_eq!(anchored_rect(band, &frame, &moved), Some(expected));
    }
  }

  /// A grown frame stretches each band along its edge and keeps its
  /// thickness, with every band hugging its own edge.
  #[test]
  fn anchored_grow_keeps_thickness() {
    let frame = Rect::from_ltrb(100, 100, 900, 700);
    let moved = Rect::from_ltrb(100, 100, 1100, 800);
    for (band, expected) in ring(&frame).iter().zip(ring(&moved)) {
      let mapped =
        anchored_rect(band, &frame, &moved).expect("Mapped band.");
      assert_eq!(mapped, expected);
      assert_eq!(mapped.width().min(mapped.height()), 4);
    }
  }

  /// A shrunk frame shortens each band and keeps its thickness.
  #[test]
  fn anchored_shrink_keeps_thickness() {
    let frame = Rect::from_ltrb(100, 100, 900, 700);
    let moved = Rect::from_ltrb(200, 200, 400, 300);
    for (band, expected) in ring(&frame).iter().zip(ring(&moved)) {
      assert_eq!(anchored_rect(band, &frame, &moved), Some(expected));
    }
  }

  /// A band longer than the frame keeps its overhang past both ends.
  #[test]
  fn anchored_band_wider_than_frame() {
    let frame = Rect::from_ltrb(0, 0, 10, 10);
    let moved = Rect::from_ltrb(0, 0, 20, 10);
    let band = Rect::from_ltrb(-100, -4, 110, 0);
    assert_eq!(
      anchored_rect(&band, &frame, &moved),
      Some(Rect::from_ltrb(-100, -4, 120, 0))
    );
  }

  /// A coordinate midway between two edges follows the start edge.
  #[test]
  fn anchored_midpoint_follows_start() {
    let frame = Rect::from_ltrb(0, 0, 10, 10);
    let moved = Rect::from_ltrb(0, 0, 30, 10);
    let dot = Rect::from_ltrb(5, 5, 6, 6);
    assert_eq!(
      anchored_rect(&dot, &frame, &moved),
      Some(Rect::from_ltrb(5, 5, 26, 6))
    );
  }

  /// An inner band anchored to both sides collapses rather than inverts
  /// when the frame shrinks past it.
  #[test]
  fn anchored_band_collapses_when_frame_shrinks_past_it() {
    let frame = Rect::from_ltrb(0, 0, 100, 100);
    let moved = Rect::from_ltrb(0, 0, 10, 100);
    let band = Rect::from_ltrb(10, 40, 90, 44);
    assert_eq!(
      anchored_rect(&band, &frame, &moved),
      Some(Rect::from_ltrb(10, 40, 10, 44))
    );
  }

  /// An empty snapshot or moved frame has no mapping.
  #[test]
  fn anchored_degenerate_frames_have_no_mapping() {
    let frame = Rect::from_ltrb(0, 0, 100, 100);
    let band = Rect::from_ltrb(-4, -4, 104, 0);
    for empty in [
      Rect::from_ltrb(0, 0, 0, 100),
      Rect::from_ltrb(0, 0, 100, 0),
      Rect::from_ltrb(50, 50, 40, 60),
    ] {
      assert!(anchored_rect(&band, &empty, &frame).is_none());
      assert!(anchored_rect(&band, &frame, &empty).is_none());
    }
  }

  /// Coordinates far outside `i32` saturate instead of wrapping.
  #[test]
  fn anchored_coordinates_saturate() {
    let frame = Rect::from_ltrb(0, 0, 10, 10);
    let moved = Rect::from_ltrb(i32::MAX - 10, 0, i32::MAX, 10);
    let band = Rect::from_ltrb(10, 0, 20, 4);
    assert_eq!(
      anchored_rect(&band, &frame, &moved),
      Some(Rect::from_ltrb(i32::MAX, 0, i32::MAX, 4))
    );
  }

  /// A late companion fades in linearly over the fade duration.
  #[test]
  fn late_companion_fades_in() {
    assert!(fade_in(Duration::ZERO).abs() < f32::EPSILON);
    assert!((fade_in(Duration::from_millis(40)) - 0.5).abs() < 1e-3);
    assert!((fade_in(FADE_IN) - 1.0).abs() < f32::EPSILON);
    assert!((fade_in(Duration::from_secs(1)) - 1.0).abs() < f32::EPSILON);
  }

  /// Searches are spaced at least one interval apart.
  #[test]
  fn discovery_is_throttled() {
    let start = Instant::now();
    let mut throttle = DiscoveryThrottle::default();
    assert!(throttle.try_begin(start));
    assert!(!throttle.try_begin(start));
    assert!(!throttle.try_begin(start + Duration::from_millis(15)));
    assert!(throttle.try_begin(start + DISCOVERY_INTERVAL));
    assert!(!throttle.try_begin(start + Duration::from_millis(20)));
    assert!(throttle.try_begin(start + Duration::from_millis(40)));
  }
}
