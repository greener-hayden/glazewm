use super::detach_container;
use crate::{
  models::{Container, TilingContainer},
  traits::{CommonGetters, TilingSizeGetters},
};

/// How close two tiling sizes must be to count as the halves of one
/// split.
const PARTNER_TOLERANCE: f32 = 0.001;

/// Detaches a container, handing its space back to the tile it was
/// split from.
///
/// A tile split by `attach_to_tile` and its partner are the two halves
/// of what was one tile, so they are the adjacent pair of equal size.
/// Closing either should restore the other, which sharing the space out
/// to every sibling in proportion would not do: on a row of one half and
/// two quarters, closing a quarter would leave two thirds and a third.
/// Where no single adjacent sibling matches, because the tiles have
/// been resized or because both neighbours match, the space is shared
/// out as `detach_container` does.
#[allow(clippy::needless_pass_by_value)]
pub fn detach_tile(container: Container) -> anyhow::Result<()> {
  let Some(partner) = split_partner(&container) else {
    return detach_container(container);
  };

  let freed = container.as_tiling_container()?.tiling_size();
  let sizes = container
    .tiling_siblings()
    .map(|sibling| (sibling.clone(), sibling.tiling_size()))
    .collect::<Vec<_>>();

  detach_container(container)?;

  // Undo the share-out so only the partner grows.
  for (sibling, size) in sizes {
    sibling.set_tiling_size(size);
  }

  partner.set_tiling_size(partner.tiling_size() + freed);

  Ok(())
}

/// The one adjacent sibling that shares `container`'s tiling size, if
/// there is exactly one.
fn split_partner(container: &Container) -> Option<TilingContainer> {
  let tile = container.as_tiling_container().ok()?;
  let parent = container.parent()?;

  let tiles = parent.tiling_children().collect::<Vec<_>>();
  let index =
    tiles.iter().position(|sibling| sibling.id() == tile.id())?;

  let is_partner = |candidate: &&TilingContainer| {
    (candidate.tiling_size() - tile.tiling_size()).abs()
      <= PARTNER_TOLERANCE
  };

  let before = index
    .checked_sub(1)
    .and_then(|index| tiles.get(index))
    .filter(is_partner);
  let after = tiles.get(index + 1).filter(is_partner);

  match (before, after) {
    (Some(partner), None) | (None, Some(partner)) => Some(partner.clone()),
    _ => None,
  }
}

#[cfg(test)]
mod tests {
  use super::{
    super::{attach_container, test_tree},
    *,
  };

  /// A workspace holding `sizes.len()` windows with those tiling sizes.
  fn row(sizes: &[f32]) -> (Container, Vec<Container>) {
    let workspace: Container = test_tree::workspace().into();
    let windows = sizes
      .iter()
      .map(|_| {
        let window: Container = test_tree::window().into();
        attach_container(&window, &workspace, None).unwrap();
        window
      })
      .collect::<Vec<_>>();

    // Sized after every attach, since each attach re-divides the row.
    for (window, &size) in windows.iter().zip(sizes) {
      window.as_tiling_container().unwrap().set_tiling_size(size);
    }

    (workspace, windows)
  }

  #[test]
  fn closing_a_half_restores_its_partner() {
    let (workspace, windows) = row(&[0.5, 0.25, 0.25]);

    detach_tile(windows[2].clone()).unwrap();

    test_tree::assert_sizes(&workspace, &[0.5, 0.5]);
  }

  #[test]
  fn closing_the_first_half_restores_the_next() {
    let (workspace, windows) = row(&[0.25, 0.25, 0.5]);

    detach_tile(windows[0].clone()).unwrap();

    test_tree::assert_sizes(&workspace, &[0.5, 0.5]);
  }

  #[test]
  fn closing_an_unpaired_tile_shares_out() {
    let (workspace, windows) = row(&[0.25, 0.25, 0.5]);

    detach_tile(windows[2].clone()).unwrap();

    test_tree::assert_sizes(&workspace, &[0.5, 0.5]);
  }

  #[test]
  fn two_matching_neighbours_share_out() {
    let third = 1.0 / 3.0;
    let (workspace, windows) = row(&[third, third, third]);

    detach_tile(windows[1].clone()).unwrap();

    test_tree::assert_sizes(&workspace, &[0.5, 0.5]);
  }
}
