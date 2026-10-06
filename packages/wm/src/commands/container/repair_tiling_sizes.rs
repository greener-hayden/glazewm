use crate::{
  models::{Container, TilingContainer},
  traits::{
    is_usable_tiling_size, usable_tiling_sizes, CommonGetters,
    TilingSizeGetters,
  },
};

/// Gives every tiling size that cannot be laid out a usable value.
///
/// Walks `root` and, for each container whose tiling children include a
/// non-finite or non-positive size, replaces such sizes with the average
/// of the valid ones (see `usable_tiling_sizes`) and scales the row to
/// sum to one. Rows with only valid sizes are left exactly as they are.
///
/// Returns whether anything was repaired, in which case the layout has
/// changed.
pub fn repair_tiling_sizes(root: &Container) -> bool {
  let mut repaired = false;

  for parent in root.self_and_descendants() {
    // Checked first, as this runs on every sync and nearly always finds
    // nothing to repair.
    if parent
      .tiling_children()
      .all(|child| is_usable_tiling_size(child.tiling_size()))
    {
      continue;
    }

    let children = parent.tiling_children().collect::<Vec<_>>();
    let sizes = children
      .iter()
      .map(TilingSizeGetters::tiling_size)
      .collect::<Vec<_>>();

    tracing::warn!(
      container = %parent.id(),
      ?sizes,
      "Repairing tiling sizes that cannot be laid out."
    );

    assign_usable_tiling_sizes(&children, &sizes, 1.);
    repaired = true;
  }

  repaired
}

/// Sets `children` to `sizes` made usable, scaled to sum to `total`.
///
/// Sizes that cannot be laid out take the average of the valid ones (see
/// `usable_tiling_sizes`), so every child keeps a share in proportion to
/// what it had. Where no scaling is possible, the children share `total`
/// equally.
pub fn assign_usable_tiling_sizes(
  children: &[TilingContainer],
  sizes: &[f32],
  total: f32,
) {
  let usable = usable_tiling_sizes(sizes);
  let sum = usable.iter().sum::<f32>();

  #[allow(clippy::cast_precision_loss)]
  let equal_share = total / children.len() as f32;

  for (child, size) in children.iter().zip(usable) {
    let scaled = size / sum * total;
    child.set_tiling_size(if scaled.is_finite() {
      scaled
    } else {
      equal_share
    });
  }
}

#[cfg(test)]
mod tests {
  use super::{super::test_tree, *};

  #[test]
  fn leaves_valid_sizes_untouched() {
    let (workspace, _) = test_tree::row(&[0.3, 0.3, 0.3]);

    assert!(!repair_tiling_sizes(&workspace));

    test_tree::assert_sizes(&workspace, &[0.3, 0.3, 0.3]);
  }

  #[test]
  fn replaces_a_non_finite_size_with_the_average() {
    let (workspace, _) = test_tree::row(&[f32::NAN, 0.5, 0.5]);

    assert!(repair_tiling_sizes(&workspace));

    let third = 1. / 3.;
    test_tree::assert_sizes(&workspace, &[third, third, third]);
  }

  #[test]
  fn shares_equally_when_no_size_is_valid() {
    let (workspace, _) = test_tree::row(&[f32::NAN, 0., f32::INFINITY]);

    assert!(repair_tiling_sizes(&workspace));

    let third = 1. / 3.;
    test_tree::assert_sizes(&workspace, &[third, third, third]);
  }

  /// A split holding one corrupt child, as on a live workspace.
  #[test]
  fn repairs_nested_splits() {
    let (workspace, windows) = test_tree::row(&[0.5, 0.5]);
    let split = wm_common::TilingDirection::Vertical;
    let split = crate::models::SplitContainer::new(
      split,
      wm_common::GapsConfig::default(),
    );
    super::super::wrap_in_split_container(
      &split,
      &workspace,
      &[windows[1].as_tiling_container().unwrap()],
    )
    .unwrap();
    windows[1]
      .as_tiling_container()
      .unwrap()
      .set_tiling_size(f32::NAN);

    assert!(repair_tiling_sizes(&workspace));

    test_tree::assert_sizes(&split.into(), &[1.0]);
    test_tree::assert_sizes(&workspace, &[0.5, 0.5]);
  }
}
