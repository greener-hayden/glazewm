use anyhow::Context;

use super::{flatten_split_container, share_out_tiling_size};
use crate::{
  models::Container,
  traits::{CommonGetters, TilingSizeGetters},
};

/// Removes a container from the tree.
///
/// If the container is a tiling container, the siblings will be resized to
/// fill the freed up space. Will flatten empty parent split containers.
#[allow(clippy::needless_pass_by_value)]
pub fn detach_container(child_to_remove: Container) -> anyhow::Result<()> {
  // Flatten the parent split container if it'll be empty after removing
  // the child.
  if let Some(split_parent) = child_to_remove
    .parent()
    .and_then(|parent| parent.as_split().cloned())
  {
    if split_parent.child_count() == 1 {
      flatten_split_container(split_parent)?;
    }
  }

  let parent = child_to_remove.parent().context("No parent.")?;

  parent
    .borrow_children_mut()
    .retain(|c| c.id() != child_to_remove.id());

  parent
    .borrow_child_focus_order_mut()
    .retain(|id| *id != child_to_remove.id());

  *child_to_remove.borrow_parent_mut() = None;

  // Resize the siblings if it is a tiling container.
  if let Ok(child_to_remove) = child_to_remove.as_tiling_container() {
    let tiling_siblings = parent.tiling_children().collect::<Vec<_>>();

    // Adjust size of the siblings based on the freed up space.
    share_out_tiling_size(
      &tiling_siblings,
      child_to_remove.tiling_size(),
      1.,
    );
  }

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::{
    super::{
      detach_tile,
      test_tree::{self, row},
    },
    *,
  };

  /// Every sibling sits at the minimum, so there is no room to weigh
  /// the share-out by.
  #[test]
  fn closing_a_tile_beside_a_minimum_tile_gives_it_the_space() {
    let (workspace, windows) = row(&[0.99, 0.01]);

    detach_tile(windows[0].clone()).unwrap();

    test_tree::assert_sizes(&workspace, &[1.0]);
  }

  #[test]
  fn closing_a_tile_beside_minimum_tiles_shares_out_equally() {
    let (workspace, windows) = row(&[0.98, 0.01, 0.01]);

    detach_container(windows[0].clone()).unwrap();

    test_tree::assert_sizes(&workspace, &[0.5, 0.5]);
  }

  /// A size already corrupted must not spread to the siblings, which
  /// still have to fill the row.
  #[test]
  fn closing_a_tile_with_a_non_finite_size_keeps_the_row_whole() {
    let (workspace, windows) = row(&[0.5, 0.25, 0.25]);
    windows[0]
      .as_tiling_container()
      .unwrap()
      .set_tiling_size(f32::NAN);

    detach_container(windows[0].clone()).unwrap();

    test_tree::assert_sizes(&workspace, &[0.5, 0.5]);
  }

  /// A corrupt sibling must not be left as a minimum tile.
  #[test]
  fn closing_a_tile_repairs_a_non_finite_sibling() {
    let (workspace, windows) = row(&[f32::NAN, 0.5, 0.5]);

    detach_container(windows[2].clone()).unwrap();

    test_tree::assert_sizes(&workspace, &[0.5, 0.5]);
  }
}
