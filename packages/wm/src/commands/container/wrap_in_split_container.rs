use std::collections::VecDeque;

use anyhow::Context;

use super::assign_usable_tiling_sizes;
use crate::{
  models::{Container, SplitContainer, TilingContainer},
  traits::{is_usable_tiling_size, CommonGetters, TilingSizeGetters},
};

pub fn wrap_in_split_container(
  split_container: &SplitContainer,
  target_parent: &Container,
  target_children: &[TilingContainer],
) -> anyhow::Result<()> {
  let starting_index = target_children
    .iter()
    .map(CommonGetters::index)
    .min()
    .context("Failed to get starting index.")?;

  target_parent
    .borrow_children_mut()
    .insert(starting_index, split_container.clone().into());

  let starting_focus_index = target_children
    .iter()
    .map(CommonGetters::focus_index)
    .min()
    .context("Failed to get starting focus index.")?;

  target_parent
    .borrow_child_focus_order_mut()
    .insert(starting_focus_index, split_container.id());

  // Get the total tiling size amongst all children. A corrupt size would
  // make the division below spread NaN or infinity across the children,
  // so the children are given an equal split instead.
  let total_tiling_size = target_children
    .iter()
    .map(TilingSizeGetters::tiling_size)
    .sum::<f32>();
  let has_valid_total = total_tiling_size.is_finite()
    && total_tiling_size > 0.
    && target_children
      .iter()
      .all(|child| is_usable_tiling_size(child.tiling_size()));

  let target_children_ids = target_children
    .iter()
    .map(CommonGetters::id)
    .collect::<Vec<_>>();

  let sorted_focus_ids = target_parent
    .borrow_child_focus_order()
    .iter()
    .filter(|id| target_children_ids.contains(id))
    .copied()
    .collect::<VecDeque<_>>();

  // Set the split container's parent and tiling size.
  *split_container.borrow_parent_mut() = Some(target_parent.clone());
  if has_valid_total {
    split_container.set_tiling_size(total_tiling_size);
  }

  // Move the children from their original parent to the split container.
  for target_child in target_children {
    *target_child.borrow_parent_mut() =
      Some(split_container.clone().into());

    split_container
      .borrow_children_mut()
      .push_back(target_child.clone().into());

    target_parent
      .borrow_children_mut()
      .retain(|child| child != &target_child.clone().into());

    target_parent
      .borrow_child_focus_order_mut()
      .retain(|id| id != &target_child.id());

    // Scale the tiling size to the new split container.
    #[allow(clippy::cast_precision_loss)]
    let scaled_size = if has_valid_total {
      target_child.tiling_size() / total_tiling_size
    } else {
      1. / target_children.len() as f32
    };
    target_child.set_tiling_size(scaled_size);
  }

  if !has_valid_total {
    // The children's total is unknown, so the split takes the average
    // share of its parent's other tiles and the row is scaled to fill it.
    let siblings = target_parent.tiling_children().collect::<Vec<_>>();
    let sizes = siblings
      .iter()
      .map(|sibling| {
        if sibling.id() == split_container.id() {
          f32::NAN
        } else {
          sibling.tiling_size()
        }
      })
      .collect::<Vec<_>>();
    assign_usable_tiling_sizes(&siblings, &sizes, 1.);
  }

  // Add original focus order to split container.
  *split_container.borrow_child_focus_order_mut() = sorted_focus_ids;

  Ok(())
}

#[cfg(test)]
mod tests {
  use wm_common::{GapsConfig, TilingDirection};

  use super::{super::test_tree, *};

  /// Wraps `tile` in a new split and returns it.
  fn wrap(workspace: &Container, tile: &Container) -> SplitContainer {
    let split = SplitContainer::new(
      TilingDirection::Vertical,
      GapsConfig::default(),
    );
    wrap_in_split_container(
      &split,
      workspace,
      &[tile.as_tiling_container().unwrap()],
    )
    .unwrap();

    split
  }

  #[test]
  fn wrapping_scales_children_to_the_split() {
    let (workspace, windows) = test_tree::row(&[0.25, 0.75]);

    let split = wrap(&workspace, &windows[0]);

    test_tree::assert_sizes(&workspace, &[0.25, 0.75]);
    test_tree::assert_sizes(&split.into(), &[1.0]);
  }

  /// A zero total used to make each child's share 0/0.
  #[test]
  fn wrapping_a_zero_sized_tile_keeps_its_share_finite() {
    let (workspace, windows) = test_tree::row(&[0.5, 0.5]);
    windows[0]
      .as_tiling_container()
      .unwrap()
      .set_tiling_size(0.0);

    let split = wrap(&workspace, &windows[0]);

    test_tree::assert_sizes(&split.into(), &[1.0]);
    test_tree::assert_sizes(&workspace, &[0.5, 0.5]);
  }

  #[test]
  fn wrapping_a_non_finite_tile_keeps_its_share_finite() {
    let (workspace, windows) = test_tree::row(&[0.5, 0.5]);
    windows[0]
      .as_tiling_container()
      .unwrap()
      .set_tiling_size(f32::NAN);

    let split = wrap(&workspace, &windows[0]);

    test_tree::assert_sizes(&split.into(), &[1.0]);
    test_tree::assert_sizes(&workspace, &[0.5, 0.5]);
  }
}
