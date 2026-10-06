use ambassador::delegatable_trait;
use wm_platform::Rect;

#[delegatable_trait]
pub trait PositionGetters {
  fn to_rect(&self) -> anyhow::Result<Rect>;
}

/// Whether a tiling size can be laid out: finite and above zero.
#[must_use]
pub fn is_usable_tiling_size(size: f32) -> bool {
  size.is_finite() && size > 0.
}

/// Replaces every size that cannot be laid out with a usable one.
///
/// A non-finite or non-positive size has no meaning as a share, and left
/// as a zero weight it gives its tile no space at all. Such a size takes
/// the average of the valid sizes, or an equal share when none are valid.
/// Valid sizes are returned as they are.
#[must_use]
pub fn usable_tiling_sizes(sizes: &[f32]) -> Vec<f32> {
  let is_valid = |size: &f32| is_usable_tiling_size(*size);
  let valid = sizes.iter().copied().filter(is_valid);
  let count = valid.clone().count();

  #[allow(clippy::cast_precision_loss)]
  let fallback = if count == 0 {
    1. / sizes.len() as f32
  } else {
    valid.sum::<f32>() / count as f32
  };

  sizes
    .iter()
    .map(|size| if is_valid(size) { *size } else { fallback })
    .collect()
}

/// Splits `total` between children that each insist on a floor.
///
/// A tiling size is a fraction of the parent, so a busy workspace can
/// hand a window a slot narrower than the app will ever accept. Left
/// alone the window keeps its own size and overhangs its neighbour; the
/// space it took has to come off the siblings instead, or the layout and
/// the screen disagree about where every window after it begins.
///
/// Water-filling: pin whoever falls below their floor, re-share what is
/// left over the rest, and repeat, since pinning one can push the next
/// under. Each pass pins at least one child, so it ends in at most
/// `mins.len()` passes.
///
/// A size that cannot be laid out (see `usable_tiling_sizes`) counts as
/// an average one, so a corrupt size never collapses its tile.
///
/// Returns a length per child, in the order given. When the floors alone
/// exceed `total` every child gets its floor and the overflow is
/// accepted here — no arrangement satisfies everyone, and spreading the
/// shortfall would leave every window wrong instead of one. `to_rect`
/// then keeps each child inside its parent, so the overflow shows as
/// overlap rather than a window past the workspace edge.
pub fn resolve_lengths(
  sizes: &[f32],
  mins: &[i32],
  total: i32,
) -> Vec<i32> {
  let total = i64::from(total.max(0));
  let mins = (0..sizes.len())
    .map(|index| mins.get(index).copied().unwrap_or(0).max(0))
    .collect::<Vec<_>>();
  if mins.iter().map(|value| i64::from(*value)).sum::<i64>() >= total {
    return mins;
  }
  let weights = usable_tiling_sizes(sizes)
    .into_iter()
    .map(f64::from)
    .collect::<Vec<_>>();
  let mut pinned = vec![false; sizes.len()];
  let mut exact = vec![0.0; sizes.len()];
  loop {
    let remaining = total
      - mins
        .iter()
        .zip(&pinned)
        .filter(|(_, pinned)| **pinned)
        .map(|(min, _)| i64::from(*min))
        .sum::<i64>();
    let free = pinned.iter().filter(|pinned| !**pinned).count();
    if free == 0 {
      return mins;
    }
    let weight = weights
      .iter()
      .zip(&pinned)
      .filter(|(_, pinned)| !**pinned)
      .map(|(weight, _)| *weight)
      .sum::<f64>();
    let mut changed = false;
    for index in 0..sizes.len() {
      if pinned[index] {
        exact[index] = f64::from(mins[index]);
        continue;
      }
      #[allow(clippy::cast_precision_loss)]
      let share = if weight > 0.0 {
        weights[index] / weight
      } else {
        1.0 / free as f64
      };
      #[allow(clippy::cast_precision_loss)]
      {
        exact[index] = remaining as f64 * share;
      }
      if exact[index] < f64::from(mins[index]) {
        pinned[index] = true;
        changed = true;
      }
    }
    if !changed {
      break;
    }
  }
  #[allow(clippy::cast_possible_truncation)]
  let mut lengths = exact
    .iter()
    .map(|value| value.floor() as i32)
    .collect::<Vec<_>>();
  let mut order = (0..sizes.len())
    .filter(|index| !pinned[*index])
    .collect::<Vec<_>>();
  order.sort_by(|left, right| {
    exact[*right]
      .fract()
      .total_cmp(&exact[*left].fract())
      .then(left.cmp(right))
  });
  let remaining =
    total - lengths.iter().map(|length| i64::from(*length)).sum::<i64>();
  for index in order
    .into_iter()
    .take(usize::try_from(remaining).unwrap_or(0))
  {
    lengths[index] += 1;
  }
  lengths
}

/// Keeps a child's rect inside its parent.
///
/// A floor that does not fit overlaps its neighbour inside the workspace
/// rather than reaching onto the next monitor, where macOS stops
/// honouring size writes.
#[must_use]
pub fn contain_in_parent(rect: &Rect, parent: &Rect) -> Rect {
  let parent_width = parent.width().max(0);
  let parent_height = parent.height().max(0);
  let width = rect.width().clamp(0, parent_width);
  let height = rect.height().clamp(0, parent_height);
  Rect::from_xy(
    rect
      .x()
      .clamp(parent.left, parent.left + parent_width - width),
    rect
      .y()
      .clamp(parent.top, parent.top + parent_height - height),
    width,
    height,
  )
}

/// Implements the `PositionGetters` trait for tiling containers that can
/// be resized. This is used by `SplitContainer` and `TilingWindow`.
///
/// Expects that the struct has a wrapping `RefCell` containing a struct
/// with an `id` and a `parent` field.
#[macro_export]
macro_rules! impl_position_getters_as_resizable {
  ($struct_name:ident) => {
    impl PositionGetters for $struct_name {
      fn to_rect(&self) -> anyhow::Result<Rect> {
        let parent = self
          .parent()
          .and_then(|parent| parent.as_direction_container().ok())
          .context("Parent lacks tiling direction.")?;
        $crate::layout_snapshot::child_rects(&parent, &parent.to_rect()?)?
          .into_iter()
          .find(|(child, _)| child.id() == self.id())
          .map(|(_, rect)| rect)
          .context("Missing tiling child.")
      }
    }
  };
}

#[cfg(test)]
mod tests {
  use super::resolve_lengths;

  /// Distributes rounding residuals deterministically.
  #[test]
  fn distributes_residual_pixels() {
    assert_eq!(resolve_lengths(&[1.0; 3], &[0; 3], 100), vec![34, 33, 33]);
    assert_eq!(resolve_lengths(&[0.0; 3], &[0; 3], 100), vec![34, 33, 33]);
    assert_eq!(resolve_lengths(&[f32::NAN, 0.0], &[0; 2], 3), vec![2, 1]);
  }

  /// A corrupt weight takes an average share rather than none.
  #[test]
  fn gives_a_non_finite_weight_an_average_share() {
    assert_eq!(
      resolve_lengths(&[f32::NAN, 0.5], &[0, 0], 100),
      vec![50, 50]
    );
    assert_eq!(
      resolve_lengths(&[f32::INFINITY, 0.25, 0.75], &[0; 3], 100),
      vec![33, 17, 50]
    );
    assert_eq!(
      resolve_lengths(&[-1.0, 0.0, f32::NAN, 0.0], &[0; 4], 100),
      vec![25, 25, 25, 25]
    );
  }

  /// Conserves available pixels across many layouts.
  #[test]
  fn conserves_layout_pixels() {
    for count in 1..16 {
      for total in 0..1000 {
        let sizes = vec![1.0; count];
        let lengths = resolve_lengths(&sizes, &vec![0; count], total);
        assert_eq!(lengths.iter().sum::<i32>(), total);
        assert!(lengths.iter().all(|length| *length >= 0));
      }
    }
  }

  #[test]
  fn splits_evenly_without_minimums() {
    assert_eq!(
      resolve_lengths(&[0.5, 0.5], &[0, 0], 1000),
      vec![500, 500]
    );
  }

  #[test]
  fn pins_a_constrained_child_and_pays_from_the_rest() {
    // The first wants 250 but cannot go under 400, so the other three
    // share the remaining 600.
    let lengths = resolve_lengths(&[0.25; 4], &[400, 0, 0, 0], 1000);
    assert_eq!(lengths[0], 400);
    assert_eq!(lengths[1..].iter().sum::<i32>(), 600);
  }

  #[test]
  fn pinning_one_can_pin_the_next() {
    // Pinning the 500 leaves 500 for two, which puts the 300 under too.
    let lengths =
      resolve_lengths(&[0.34, 0.33, 0.33], &[500, 300, 0], 1000);
    assert_eq!(lengths[0], 500);
    assert_eq!(lengths[1], 300);
    assert_eq!(lengths[2], 200);
  }

  #[test]
  fn gives_every_child_its_floor_when_they_cannot_all_fit() {
    // 600 + 600 needs 1200 and there is 1000: overflow is unavoidable,
    // so each keeps its floor rather than every window being wrong.
    assert_eq!(
      resolve_lengths(&[0.5, 0.5], &[600, 600], 1000),
      vec![600, 600]
    );
  }

  #[test]
  fn leaves_a_child_alone_when_its_share_already_clears_its_floor() {
    assert_eq!(
      resolve_lengths(&[0.5, 0.5], &[100, 100], 1000),
      vec![500, 500]
    );
  }

  #[test]
  fn honours_uneven_sizes() {
    assert_eq!(
      resolve_lengths(&[0.75, 0.25], &[0, 0], 1000),
      vec![750, 250]
    );
  }
}

#[cfg(test)]
mod contain_tests {
  use wm_platform::Rect;

  use super::contain_in_parent;

  const PARENT: Rect = Rect {
    left: -2992,
    top: -320,
    right: -16,
    bottom: 1308,
  };

  #[test]
  fn leaves_a_fitting_rect_alone() {
    let rect = Rect::from_xy(-2992, -320, 2324, 1628);
    assert_eq!(contain_in_parent(&rect, &PARENT), rect);
  }

  #[test]
  fn pulls_an_overhang_back_to_the_edge() {
    let rect = Rect::from_xy(-648, -320, 841, 1628);
    let contained = contain_in_parent(&rect, &PARENT);
    assert_eq!(contained.right, PARENT.right);
    assert_eq!(contained.width(), 841);
  }

  #[test]
  fn shrinks_a_rect_wider_than_the_parent() {
    let rect = Rect::from_xy(-2992, -320, 4000, 1628);
    let contained = contain_in_parent(&rect, &PARENT);
    assert_eq!(contained, PARENT);
  }
}
