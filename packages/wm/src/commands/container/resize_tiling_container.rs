use super::assign_usable_tiling_sizes;
use crate::{
  models::TilingContainer,
  traits::{
    is_usable_tiling_size, CommonGetters, TilingSizeGetters,
    MIN_TILING_SIZE,
  },
};

/// Resizes a tiling container, taking from or giving to its siblings.
///
/// The target is clamped so that no tiling container falls below
/// `MIN_TILING_SIZE`. A non-finite target is ignored.
pub fn resize_tiling_container(
  container_to_resize: &TilingContainer,
  target_size: f32,
) {
  resize_tiling_container_from(
    container_to_resize,
    container_to_resize.tiling_size(),
    target_size,
  );
}

/// Resizes a tiling container as if it currently had `current_size`.
///
/// Lets a container that has not been given a share yet (e.g. one just
/// attached) claim its space from its siblings without first being
/// written an invalid size.
pub fn resize_tiling_container_from(
  container_to_resize: &TilingContainer,
  current_size: f32,
  target_size: f32,
) {
  if !target_size.is_finite() {
    tracing::error!(
      target_size,
      "Ignoring non-finite tiling size target."
    );
    return;
  }

  let tiling_siblings =
    container_to_resize.tiling_siblings().collect::<Vec<_>>();

  // Ignore cases where the container is the only child.
  if tiling_siblings.is_empty() {
    container_to_resize.set_tiling_size(1.);
    return;
  }

  // Prevent the container from being smaller than the minimum size, and
  // larger than the space available from sibling containers.
  #[allow(clippy::cast_precision_loss)]
  let clamped_target_size = target_size.clamp(
    MIN_TILING_SIZE,
    1. - (tiling_siblings.len() as f32 * MIN_TILING_SIZE),
  );

  let size_delta = clamped_target_size - current_size;
  container_to_resize.set_tiling_size(clamped_target_size);

  share_out_tiling_size(
    &tiling_siblings,
    -size_delta,
    1. - clamped_target_size,
  );
}

/// Adds `amount` of tiling size to `siblings`, or takes it when negative.
///
/// Each sibling's part is in proportion to its size above
/// `MIN_TILING_SIZE`, so larger containers give up (or take) more. When
/// the siblings have no such room to weigh by, or the weighing is not
/// finite, every sibling gets an equal part instead.
///
/// `total` is what the siblings should sum to afterwards. It is used only
/// when a sibling's size or `amount` cannot be laid out: nothing can be
/// shared out then, so the siblings are repaired to sum to `total`
/// instead (see `assign_usable_tiling_sizes`).
pub fn share_out_tiling_size(
  siblings: &[TilingContainer],
  amount: f32,
  total: f32,
) {
  let sizes = siblings
    .iter()
    .map(TilingSizeGetters::tiling_size)
    .collect::<Vec<_>>();

  if !amount.is_finite()
    || !sizes.iter().all(|size| is_usable_tiling_size(*size))
  {
    assign_usable_tiling_sizes(siblings, &sizes, total);
    return;
  }

  let available_size =
    sizes.iter().map(|size| size - MIN_TILING_SIZE).sum::<f32>();

  let proportional = (available_size > f32::EPSILON)
    .then(|| {
      sizes
        .iter()
        .map(|size| {
          size + (size - MIN_TILING_SIZE) / available_size * amount
        })
        .collect::<Vec<_>>()
    })
    .filter(|shared| shared.iter().all(|size| size.is_finite()));

  #[allow(clippy::cast_precision_loss)]
  let shared = proportional.unwrap_or_else(|| {
    let part = amount / siblings.len() as f32;
    sizes.iter().map(|size| size + part).collect()
  });

  for (sibling, size) in siblings.iter().zip(shared) {
    sibling.set_tiling_size(size);
  }
}

#[cfg(test)]
mod tests {
  use super::{
    super::test_tree::{self, row},
    *,
  };

  /// The sibling already sits at the minimum, so 0/0 used to make its
  /// share-out factor NaN.
  #[test]
  fn resizing_beside_a_minimum_tile_keeps_sizes_finite() {
    let (workspace, windows) = row(&[0.99, 0.01]);

    resize_tiling_container(
      &windows[0].as_tiling_container().unwrap(),
      0.99,
    );

    test_tree::assert_sizes(&workspace, &[0.99, 0.01]);
  }

  #[test]
  fn growing_takes_from_siblings_in_proportion() {
    let (workspace, windows) = row(&[0.4, 0.4, 0.2]);

    resize_tiling_container(
      &windows[0].as_tiling_container().unwrap(),
      0.5,
    );

    // The siblings' room above the minimum is 0.39 and 0.19.
    test_tree::assert_sizes(
      &workspace,
      &[0.5, 0.4 - 0.1 * 39. / 58., 0.2 - 0.1 * 19. / 58.],
    );
  }

  #[test]
  fn minimum_siblings_give_up_space_equally() {
    let (workspace, windows) = row(&[0.01, 0.01, 0.98]);

    resize_tiling_container(
      &windows[2].as_tiling_container().unwrap(),
      0.5,
    );

    test_tree::assert_sizes(&workspace, &[0.25, 0.25, 0.5]);
  }

  #[test]
  fn a_non_finite_target_leaves_sizes_alone() {
    let (workspace, windows) = row(&[0.5, 0.5]);

    resize_tiling_container(
      &windows[0].as_tiling_container().unwrap(),
      f32::NAN,
    );

    test_tree::assert_sizes(&workspace, &[0.5, 0.5]);
  }

  /// A corrupt sibling is repaired to the space left, not counted as a
  /// minimum tile that then keeps almost nothing.
  #[test]
  fn a_non_finite_sibling_is_repaired_to_the_space_left() {
    let (workspace, windows) = row(&[0.5, f32::NAN, 0.25]);

    resize_tiling_container(
      &windows[0].as_tiling_container().unwrap(),
      0.6,
    );

    test_tree::assert_sizes(&workspace, &[0.6, 0.2, 0.2]);
  }

  /// With no usable current size there is no delta to share out.
  #[test]
  fn a_non_finite_current_size_leaves_the_siblings_filling_the_rest() {
    let (workspace, windows) = row(&[f32::NAN, 0.5, 0.25]);

    resize_tiling_container(
      &windows[0].as_tiling_container().unwrap(),
      0.3,
    );

    test_tree::assert_sizes(&workspace, &[0.3, 0.7 * 2. / 3., 0.7 / 3.]);
  }
}
