use wm_common::TilingDirection;

use crate::{
  models::{DirectionContainer, TilingContainer},
  traits::{
    resolve_lengths_into, CommonGetters, PositionGetters, SolverBuffers,
    TilingDirectionGetters, TilingSizeGetters, MIN_TILING_SIZE,
  },
  wm_state::WmState,
};

/// Edge of a tile along one axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeEdge {
  /// The left edge of a width, or the top edge of a height.
  Start,

  /// The right edge of a width, or the bottom edge of a height.
  End,
}

/// The line between two neighbouring tiles of a row.
///
/// Every resize of a tiling window moves one of these, whether it comes
/// from a command or from a dragged edge. The row can belong to an
/// ancestor of the window: the right edge of the last window in a split
/// is the right edge of that split.
pub struct Boundary {
  /// Container whose tiling children form the row.
  row: DirectionContainer,

  /// Position, among the row's tiling children, of the tile before the
  /// line.
  index: usize,

  /// Which edge of the resized tile the line is.
  edge: ResizeEdge,
}

impl Boundary {
  /// Finds the nearest boundary along an axis that resizes `tile`.
  ///
  /// Climbs from `tile` through its ancestors. At each row along the axis
  /// it looks for a neighbour across each of `edges`, in the order given,
  /// and stops at the first one. A drag passes the edge it moved. A
  /// command passes both, so that it resizes the nearest row in which the
  /// tile has a neighbour at all.
  ///
  /// Returns `None` when no row has such a neighbour, as for an edge on
  /// the edge of the workspace.
  pub fn find(
    tile: &TilingContainer,
    is_width: bool,
    edges: &[ResizeEdge],
  ) -> Option<Self> {
    let axis = if is_width {
      TilingDirection::Horizontal
    } else {
      TilingDirection::Vertical
    };

    let mut current = tile.clone();

    loop {
      let row = current.parent()?.as_direction_container().ok()?;

      if row.tiling_direction() == axis {
        let id = current.id();
        let mut count = 0_usize;
        let mut position = None;

        for child in row.borrow_children().iter() {
          if child.is_tiling() {
            if child.id() == id {
              position = Some(count);
            }
            count += 1;
          }
        }

        let position = position?;

        for &edge in edges {
          let index = match edge {
            ResizeEdge::Start => position.checked_sub(1),
            ResizeEdge::End => (position + 1 < count).then_some(position),
          };

          if let Some(index) = index {
            return Some(Self { row, index, edge });
          }
        }
      }

      current = match row {
        DirectionContainer::Split(split) => split.into(),
        DirectionContainer::Workspace(_) => return None,
      };
    }
  }

  /// Length that the tiles of the row share out, in pixels. This is the
  /// length of the row without the gaps between its tiles.
  pub fn row_length(&self) -> anyhow::Result<i32> {
    Ok(Row::solve(&self.row)?.available)
  }

  /// Grows the resized tile by `px` pixels, or shrinks it when negative.
  ///
  /// Returns whether the layout changed (see `shift`).
  pub fn grow(
    &self,
    px: i32,
    state: &mut WmState,
  ) -> anyhow::Result<bool> {
    self.shift(
      match self.edge {
        ResizeEdge::Start => px.saturating_neg(),
        ResizeEdge::End => px,
      },
      state,
    )
  }

  /// Moves the boundary by `delta` pixels, towards the end of the row
  /// when positive.
  ///
  /// The tile on the side that the boundary moves away from grows. When
  /// that is the resized tile, the space comes from the tiles across the
  /// boundary, nearest first, each down to its floor: a tile with no
  /// space to give slides along and the next one pays. When the resized
  /// tile shrinks, it alone gives up space. An inferred floor that holds
  /// the move back is forgotten, so that the window is asked again; a
  /// floor that the application reported is kept.
  ///
  /// Works in the lengths that the layout shows, and writes every tiling
  /// size of the row back from them. A tiling size can differ from what
  /// is shown while a floor holds a tile wider than its share, and a
  /// resize worked out from such sizes would land somewhere else.
  ///
  /// Returns whether the layout changed, in which case the row is queued
  /// for redraw.
  pub fn shift(
    &self,
    delta: i32,
    state: &mut WmState,
  ) -> anyhow::Result<bool> {
    if delta == 0 {
      return Ok(false);
    }

    let Row {
      tiles,
      mut lengths,
      available,
      is_width,
    } = Row::solve(&self.row)?;

    // Without room to resize into, there is nothing to trade.
    if available <= 0 || self.index + 1 >= tiles.len() {
      return Ok(false);
    }

    let (grower, mut payers) = if delta > 0 {
      (
        self.index,
        (self.index + 1..tiles.len()).collect::<Vec<_>>(),
      )
    } else {
      (self.index + 1, (0..=self.index).rev().collect())
    };

    let grows_resized_tile = (delta > 0) == (self.edge == ResizeEdge::End);
    if !grows_resized_tile {
      payers.truncate(1);
    }

    // No tile is left with less than the smallest tiling size.
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    let least =
      ((available as f32 * MIN_TILING_SIZE).ceil() as i32).max(1);
    let wanted = delta.saturating_abs();

    let mut take = |wanted: i32| {
      let mut taken = 0;

      for &payer in &payers {
        let floor = tiles[payer].min_length(is_width).max(least);
        let given = (lengths[payer] - floor).clamp(0, wanted - taken);
        lengths[payer] -= given;
        taken += given;
      }

      taken
    };

    let mut taken = take(wanted);
    let mut released = false;

    if taken < wanted {
      // Every payer is asked, so this cannot stop at the first.
      for &payer in &payers {
        released |= tiles[payer].release_inferred_floors(is_width);
      }

      if released {
        taken += take(wanted - taken);
      }
    }

    if taken > 0 {
      lengths[grower] += taken;

      let total = lengths.iter().sum::<i32>();

      #[allow(clippy::cast_precision_loss)]
      for (tile, length) in tiles.iter().zip(&lengths) {
        tile.set_tiling_size((*length).max(1) as f32 / total as f32);
      }
    }

    // A released floor moves the layout even where no tiling size does.
    let changed = taken > 0 || released;
    if changed {
      state.pending_sync.queue_containers_to_redraw(tiles);
    }

    Ok(changed)
  }
}

/// The tiles of a row with the lengths that the layout gives them.
struct Row {
  tiles: Vec<TilingContainer>,

  /// Length of each tile along the row, in pixels.
  lengths: Vec<i32>,

  /// Length of the row without the gaps between its tiles.
  available: i32,

  /// Whether the row runs along the width.
  is_width: bool,
}

impl Row {
  /// Solves the row as `LayoutSnapshot` does.
  fn solve(row: &DirectionContainer) -> anyhow::Result<Self> {
    let is_width = row.tiling_direction() == TilingDirection::Horizontal;
    let tiles = row.tiling_children().collect::<Vec<_>>();
    let rect = row.to_rect()?;

    // The first tile's config decides the gap of the row.
    let gap = match tiles.first() {
      Some(tile) => {
        let (horizontal_gap, vertical_gap) = tile.inner_gaps()?;
        if is_width {
          horizontal_gap
        } else {
          vertical_gap
        }
      }
      None => 0,
    };

    let total = if is_width {
      rect.width()
    } else {
      rect.height()
    };
    let gaps = i32::try_from(tiles.len().saturating_sub(1))?;
    let available = total.saturating_sub(gap.saturating_mul(gaps)).max(0);

    let sizes = tiles
      .iter()
      .map(TilingSizeGetters::tiling_size)
      .collect::<Vec<_>>();
    let floors = tiles
      .iter()
      .map(|tile| tile.min_length(is_width))
      .collect::<Vec<_>>();
    let mut lengths = vec![0; tiles.len()];

    resolve_lengths_into(
      &sizes,
      &floors,
      available,
      &mut lengths,
      &mut SolverBuffers::default(),
    );

    Ok(Self {
      tiles,
      lengths,
      available,
      is_width,
    })
  }
}

#[cfg(test)]
mod tests {
  use tokio::sync::mpsc::UnboundedReceiver;
  use wm_common::{GapsConfig, WmEvent};
  use wm_platform::LengthValue;

  use super::*;
  use crate::{
    models::{
      MinSizeSource, Monitor, SplitContainer, TilingWindow, Workspace,
    },
    test_utils::{attach_monitor, mock_state},
    traits::WindowGetters,
  };

  /// A 1680px wide workspace attached to a mock state.
  fn attach(
    workspace: Workspace,
  ) -> (WmState, UnboundedReceiver<WmEvent>) {
    let monitor = Monitor::mock().workspaces(vec![workspace]).call();
    let (state, events) = mock_state();
    attach_monitor(&state, &monitor).expect("Attach monitor.");
    (state, events)
  }

  /// Gives a window a floor along its width.
  fn set_floor(window: &TilingWindow, floor: i32, source: MinSizeSource) {
    window.update_native_properties(|properties| {
      properties.min_size = Some((floor, 0));
      properties.min_size_source = source;
    });
  }

  /// Three windows in a row on a gapless workspace, 560px each.
  fn row_of_three(
  ) -> (WmState, UnboundedReceiver<WmEvent>, [TilingWindow; 3]) {
    let windows = [(); 3].map(|()| TilingWindow::mock().call());
    let workspace = Workspace::mock()
      .tiling_containers(windows.iter().cloned().map(Into::into).collect())
      .call();
    let (state, events) = attach(workspace);

    for window in &windows {
      window.set_tiling_size(1. / 3.);
    }

    (state, events, windows)
  }

  /// Widths of the windows as the layout shows them.
  fn widths(windows: &[TilingWindow]) -> Vec<i32> {
    windows
      .iter()
      .map(|window| window.to_rect().expect("Rect.").width())
      .collect()
  }

  /// Moves the boundary at `edge` of `window` along the width.
  fn shift(
    window: &TilingWindow,
    edge: ResizeEdge,
    delta: i32,
    state: &mut WmState,
  ) -> bool {
    Boundary::find(&window.clone().into(), true, &[edge])
      .expect("Boundary.")
      .shift(delta, state)
      .expect("Shift.")
  }

  #[test]
  fn an_end_edge_trades_with_the_next_tile_only() {
    let (mut state, _events, windows) = row_of_three();

    assert!(shift(&windows[1], ResizeEdge::End, 100, &mut state));

    assert_eq!(widths(&windows), [560, 660, 460]);
  }

  #[test]
  fn a_start_edge_trades_with_the_previous_tile_only() {
    let (mut state, _events, windows) = row_of_three();

    assert!(shift(&windows[1], ResizeEdge::Start, -100, &mut state));

    assert_eq!(widths(&windows), [460, 660, 560]);
  }

  /// The neighbour is at its floor, so it slides along and the tile after
  /// it gives up the space.
  #[test]
  fn growth_passes_a_tile_at_its_floor() {
    let (mut state, _events, windows) = row_of_three();
    set_floor(&windows[1], 560, MinSizeSource::Reported);

    assert!(shift(&windows[0], ResizeEdge::End, 200, &mut state));

    assert_eq!(widths(&windows), [760, 560, 360]);
  }

  /// With every tile across the boundary at a reported floor, nothing
  /// can move, and no tiling size is written.
  #[test]
  fn growth_stops_when_no_tile_has_room() {
    let (mut state, _events, windows) = row_of_three();
    set_floor(&windows[1], 560, MinSizeSource::Reported);
    set_floor(&windows[2], 560, MinSizeSource::Reported);

    assert!(!shift(&windows[0], ResizeEdge::End, 200, &mut state));

    assert_eq!(widths(&windows), [560, 560, 560]);
    assert!(!state.pending_sync.has_changes());
  }

  /// An inferred floor in the way is forgotten, and the move goes on.
  #[test]
  fn growth_releases_an_inferred_floor_in_the_way() {
    let (mut state, _events, windows) = row_of_three();
    set_floor(&windows[1], 560, MinSizeSource::Inferred);
    set_floor(&windows[2], 560, MinSizeSource::Inferred);

    assert!(shift(&windows[0], ResizeEdge::End, 200, &mut state));

    assert_eq!(widths(&windows), [760, 360, 560]);
    assert_eq!(windows[1].min_size(), None);
  }

  /// A tile that shrinks gives up its own space only. The tile behind it
  /// keeps its width, however far the boundary was asked to go.
  #[test]
  fn a_shrinking_tile_does_not_take_from_the_tile_behind_it() {
    let (mut state, _events, windows) = row_of_three();
    set_floor(&windows[1], 500, MinSizeSource::Reported);

    assert!(shift(&windows[1], ResizeEdge::End, -5000, &mut state));

    assert_eq!(widths(&windows), [560, 500, 620]);
  }

  /// No tile is left under the smallest tiling size.
  #[test]
  fn a_tile_keeps_the_smallest_tiling_size() {
    let (mut state, _events, windows) = row_of_three();

    assert!(shift(&windows[0], ResizeEdge::End, 5000, &mut state));

    assert_eq!(widths(&windows), [1646, 17, 17]);
  }

  /// A floor holds the first tile wider than its share. The tiling sizes
  /// are written back from what is shown, so the other boundary stays.
  #[test]
  fn tiling_sizes_follow_what_is_shown() {
    let (mut state, _events, windows) = row_of_three();
    set_floor(&windows[0], 800, MinSizeSource::Reported);
    assert_eq!(widths(&windows), [800, 440, 440]);

    assert!(shift(&windows[1], ResizeEdge::End, 40, &mut state));

    assert_eq!(widths(&windows), [800, 480, 400]);

    windows[0].update_native_properties(|properties| {
      properties.min_size = None;
    });
    assert_eq!(widths(&windows), [800, 480, 400]);
  }

  /// A horizontal workspace of `a` beside a vertical split of `b` over a
  /// horizontal split of `c` and `d`, with 20px gaps.
  fn nested() -> (WmState, UnboundedReceiver<WmEvent>, [TilingWindow; 4]) {
    let gaps = GapsConfig {
      inner_gap: LengthValue::from_px(20),
      ..GapsConfig::default()
    };
    let windows = [(); 4]
      .map(|()| TilingWindow::mock().gaps_config(gaps.clone()).call());
    let [a, b, c, d] = windows.clone();
    let lower = SplitContainer::mock()
      .tiling_direction(TilingDirection::Horizontal)
      .gaps_config(gaps.clone())
      .tiling_containers(vec![c.into(), d.into()])
      .call();
    let column = SplitContainer::mock()
      .tiling_direction(TilingDirection::Vertical)
      .gaps_config(gaps.clone())
      .tiling_containers(vec![b.into(), lower.into()])
      .call();
    let workspace = Workspace::mock()
      .gaps_config(gaps)
      .tiling_containers(vec![a.into(), column.into()])
      .call();
    let (state, events) = attach(workspace);

    (state, events, windows)
  }

  /// The left edge of `c` is the left edge of the whole column, so it
  /// moves the boundary beside `a`. `c` and `d` keep their shares.
  #[test]
  fn an_edge_without_a_neighbour_in_its_row_climbs_to_one_that_has() {
    let (mut state, _events, windows) = nested();
    let before = windows.clone().map(|w| w.to_rect().expect("Rect."));

    assert!(shift(&windows[2], ResizeEdge::Start, -100, &mut state));

    let after = windows.clone().map(|w| w.to_rect().expect("Rect."));
    assert_eq!(after[0].width(), before[0].width() - 100);
    assert_eq!(after[1].width(), before[1].width() + 100);
    assert_eq!(after[2].left, before[2].left - 100);
    assert_eq!(after[3].right, before[3].right);
    assert_eq!(after[2].width(), after[3].width());
  }

  /// The gaps counted are those of the row that is resized, so the
  /// boundary lands on the pixel asked for.
  #[test]
  fn a_boundary_of_an_ancestor_lands_where_it_is_put() {
    let (mut state, _events, windows) = nested();
    let before = windows[1].to_rect().expect("Rect.");

    assert!(shift(&windows[1], ResizeEdge::Start, 137, &mut state));

    assert_eq!(
      windows[1].to_rect().expect("Rect.").left,
      before.left + 137
    );
  }

  /// An edge on the edge of the workspace has no boundary.
  #[test]
  fn an_edge_of_the_workspace_has_no_boundary() {
    let (_state, _events, windows) = nested();
    let find = |window: &TilingWindow, is_width, edge| {
      Boundary::find(&window.clone().into(), is_width, &[edge]).is_some()
    };

    assert!(!find(&windows[0], true, ResizeEdge::Start));
    assert!(!find(&windows[3], true, ResizeEdge::End));
    assert!(!find(&windows[1], false, ResizeEdge::Start));
    assert!(!find(&windows[3], false, ResizeEdge::End));
    assert!(find(&windows[3], false, ResizeEdge::Start));
  }

  /// A command takes the nearest row with a neighbour on either side,
  /// before it climbs: `d` resizes against `c`, not against `a`.
  #[test]
  fn a_command_resizes_within_the_nearest_row() {
    let (mut state, _events, windows) = nested();
    let before = windows.clone().map(|w| w.to_rect().expect("Rect."));

    Boundary::find(
      &windows[3].clone().into(),
      true,
      &[ResizeEdge::End, ResizeEdge::Start],
    )
    .expect("Boundary.")
    .grow(50, &mut state)
    .expect("Grow.");

    let after = windows.clone().map(|w| w.to_rect().expect("Rect."));
    assert_eq!(after[0], before[0]);
    assert_eq!(after[2].width(), before[2].width() - 50);
    assert_eq!(after[3].width(), before[3].width() + 50);
  }

  /// Any run of moves leaves each row summing to one, and the layout
  /// showing each boundary where the move put it.
  #[test]
  fn random_moves_keep_rows_whole_and_land_on_the_pixel() {
    let mut seed = 0x2545_f491_4f6c_dd1d_u64;
    let mut next = move |bound: i32| {
      seed ^= seed << 13;
      seed ^= seed >> 7;
      seed ^= seed << 17;
      i32::try_from(seed % u64::try_from(bound).expect("Bound."))
        .expect("Value.")
    };
    let (mut state, _events, windows) = nested();

    for _ in 0..500 {
      let window = &windows[usize::try_from(next(4)).expect("Index.")];
      let is_width = next(2) == 0;
      let edge = if next(2) == 0 {
        ResizeEdge::Start
      } else {
        ResizeEdge::End
      };
      let Some(boundary) =
        Boundary::find(&window.clone().into(), is_width, &[edge])
      else {
        continue;
      };

      let position = |window: &TilingWindow| {
        let rect = window.to_rect().expect("Rect.");
        match (is_width, edge) {
          (true, ResizeEdge::Start) => rect.left,
          (true, ResizeEdge::End) => rect.right,
          (false, ResizeEdge::Start) => rect.top,
          (false, ResizeEdge::End) => rect.bottom,
        }
      };
      let before = position(window);

      // Small enough that no tile reaches the smallest tiling size.
      let delta = next(41) - 20;
      boundary.shift(delta, &mut state).expect("Shift.");

      assert_eq!(position(window), before + delta);

      for container in state.root_container.descendants() {
        let Ok(row) = container.as_direction_container() else {
          continue;
        };
        let sum = row
          .tiling_children()
          .map(|tile| tile.tiling_size())
          .sum::<f32>();
        assert!((sum - 1.).abs() < 0.0001, "row sums to {sum}");
      }
    }
  }
}
