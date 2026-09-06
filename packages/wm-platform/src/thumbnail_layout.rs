use crate::Rect;

/// Clips destination and corresponding source pixels.
#[must_use]
pub fn thumbnail_rects(
  inner: &Rect,
  outer: &Rect,
  source: (i32, i32),
) -> Option<(Rect, Rect)> {
  if inner.width() <= 0
    || inner.height() <= 0
    || source.0 <= 0
    || source.1 <= 0
  {
    return None;
  }
  let visible = inner.crop(outer);
  if visible.width() <= 0 || visible.height() <= 0 {
    return None;
  }
  let map = |offset: i32, length: i32, pixels: i32| {
    i32::try_from(
      i64::from(offset) * i64::from(pixels) / i64::from(length),
    )
    .unwrap_or(pixels)
    .clamp(0, pixels)
  };
  let source_rect = Rect::from_ltrb(
    map(visible.left - inner.left, inner.width(), source.0),
    map(visible.top - inner.top, inner.height(), source.1),
    map(visible.right - inner.left, inner.width(), source.0),
    map(visible.bottom - inner.top, inner.height(), source.1),
  );
  let destination = visible.translate_to_coordinates(
    visible.left - outer.left,
    visible.top - outer.top,
  );
  Some((destination, source_rect))
}

#[cfg(test)]
mod tests {
  use super::thumbnail_rects;
  use crate::Rect;

  /// Clips source pixels without stretching content.
  #[test]
  fn clips_workspace_thumbnail() {
    let monitor = Rect::from_xy(-1920, -200, 1920, 1080);
    let moving = Rect::from_xy(-2020, -100, 400, 300);
    let (destination, source) =
      thumbnail_rects(&moving, &monitor, (800, 600))
        .expect("Visible thumbnail.");
    assert_eq!(destination, Rect::from_xy(0, 100, 300, 300));
    assert_eq!(source, Rect::from_xy(200, 0, 600, 600));
    assert!(thumbnail_rects(
      &Rect::from_xy(100, 100, 200, 200),
      &monitor,
      (400, 400)
    )
    .is_none());
  }
}
