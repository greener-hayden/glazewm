use std::cmp::Ordering;

use anyhow::Context;
use wm_common::{GapsConfig, TilingDirection};
use wm_platform::Rect;

use super::{attach_container, insert_container, wrap_in_split_container};
use crate::{
  models::{Container, SplitContainer, TilingContainer},
  traits::{
    CommonGetters, PositionGetters, TilingDirectionGetters,
    TilingSizeGetters, MIN_TILING_SIZE,
  },
};

/// Attaches `child` beside `tile`, giving each half of the tile's space.
///
/// Only the tile is split; its siblings keep their sizes. This is the
/// bspwm and Hyprland dwindle model, as against joining the parent row
/// and re-dividing it, which squeezes every window on it. Where the
/// split runs the way the tile's parent already does, `child` becomes
/// the tile's next sibling; otherwise the tile is first wrapped in a
/// split container running that way.
///
/// A tile alone in its container joins it instead, at half, whichever
/// way that container runs: its direction is what
/// `toggle-tiling-direction` last set, and a container of one is also
/// what a closed pair leaves behind.
pub fn attach_to_tile(
  child: &Container,
  tile: &TilingContainer,
  direction: TilingDirection,
  gaps_config: &GapsConfig,
) -> anyhow::Result<()> {
  let parent = tile
    .parent()
    .and_then(|parent| parent.as_direction_container().ok())
    .context("Tile has no direction container.")?;

  if tile.tiling_siblings().next().is_none() {
    return attach_container(child, &parent.into(), None);
  }

  let half = tile.tiling_size() / 2.0;

  if direction == parent.tiling_direction() && half >= MIN_TILING_SIZE {
    insert_container(child, &parent.into(), Some(tile.index() + 1))?;
    tile.set_tiling_size(half);
    child.as_tiling_container()?.set_tiling_size(half);

    return Ok(());
  }

  let split = SplitContainer::new(direction, gaps_config.clone());
  wrap_in_split_container(
    &split,
    &parent.into(),
    std::slice::from_ref(tile),
  )?;

  // The tile is the split's only child, so the even division is a half.
  attach_container(child, &split.into(), None)
}

/// The way a new tile splits `tile`, from its shape on screen.
pub fn tile_split_direction(
  tile: &TilingContainer,
) -> anyhow::Result<TilingDirection> {
  let parent_direction = tile
    .parent()
    .and_then(|parent| parent.as_direction_container().ok())
    .context("Tile has no direction container.")?
    .tiling_direction();

  Ok(split_direction(&tile.to_rect()?, parent_direction))
}

/// The way a new tile splits one of `rect`: across its longer side, so
/// a wide tile gains a column and a tall one a row.
///
/// `fallback` breaks a tie. Passing the parent's direction keeps a
/// square tile from nesting.
#[must_use]
pub fn split_direction(
  rect: &Rect,
  fallback: TilingDirection,
) -> TilingDirection {
  match rect.height().cmp(&rect.width()) {
    Ordering::Greater => TilingDirection::Vertical,
    Ordering::Less => TilingDirection::Horizontal,
    Ordering::Equal => fallback,
  }
}

#[cfg(test)]
mod tests {
  use super::{super::test_tree, *};

  #[test]
  fn alone_tile_joins_its_container_at_half() {
    let workspace: Container = test_tree::workspace().into();
    let first = test_tree::window();
    attach_container(&first.clone().into(), &workspace, None).unwrap();

    let second = test_tree::window();
    attach_to_tile(
      &second.clone().into(),
      &first.into(),
      TilingDirection::Vertical,
      &GapsConfig::default(),
    )
    .unwrap();

    assert_eq!(workspace.child_count(), 2);
    assert!(workspace.children().iter().all(Container::is_tiling_window));
    test_tree::assert_sizes(&workspace, &[0.5, 0.5]);
  }

  #[test]
  fn same_direction_halves_the_tile_only() {
    let workspace: Container = test_tree::workspace().into();
    let first = test_tree::window();
    let second = test_tree::window();
    attach_container(&first.clone().into(), &workspace, None).unwrap();
    attach_container(&second.clone().into(), &workspace, None).unwrap();

    let third = test_tree::window();
    attach_to_tile(
      &third.clone().into(),
      &second.clone().into(),
      TilingDirection::Horizontal,
      &GapsConfig::default(),
    )
    .unwrap();

    let ids = workspace
      .children()
      .iter()
      .map(CommonGetters::id)
      .collect::<Vec<_>>();
    assert_eq!(ids, vec![first.id(), second.id(), third.id()]);
    test_tree::assert_sizes(&workspace, &[0.5, 0.25, 0.25]);
  }

  #[test]
  fn cross_direction_wraps_the_tile() {
    let workspace: Container = test_tree::workspace().into();
    let first = test_tree::window();
    let second = test_tree::window();
    attach_container(&first.clone().into(), &workspace, None).unwrap();
    attach_container(&second.clone().into(), &workspace, None).unwrap();

    let third = test_tree::window();
    attach_to_tile(
      &third.clone().into(),
      &second.clone().into(),
      TilingDirection::Vertical,
      &GapsConfig::default(),
    )
    .unwrap();

    test_tree::assert_sizes(&workspace, &[0.5, 0.5]);

    let split = workspace.children()[1]
      .as_split()
      .cloned()
      .expect("The tile was wrapped in a split.");
    assert_eq!(split.tiling_direction(), TilingDirection::Vertical);

    let ids = split
      .children()
      .iter()
      .map(CommonGetters::id)
      .collect::<Vec<_>>();
    assert_eq!(ids, vec![second.id(), third.id()]);
    test_tree::assert_sizes(&split.into(), &[0.5, 0.5]);
  }

  #[test]
  fn split_runs_across_the_longer_side() {
    let wide = Rect::from_xy(0, 0, 1705, 1380);
    let tall = Rect::from_xy(0, 0, 852, 1380);
    let square = Rect::from_xy(0, 0, 900, 900);

    assert_eq!(
      split_direction(&wide, TilingDirection::Vertical),
      TilingDirection::Horizontal
    );
    assert_eq!(
      split_direction(&tall, TilingDirection::Horizontal),
      TilingDirection::Vertical
    );
    assert_eq!(
      split_direction(&square, TilingDirection::Vertical),
      TilingDirection::Vertical
    );
  }
}
