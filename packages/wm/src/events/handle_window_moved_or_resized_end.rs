use anyhow::Context;
use wm_common::{
  ActiveDrag, FullscreenStateConfig, TilingDirection, WindowState,
};
use wm_platform::{Point, Rect};

use crate::{
  commands::{
    container::{
      move_container_within_tree, wrap_in_split_container, Boundary,
      ResizeEdge,
    },
    window::update_window_state,
  },
  events::update_floating_window_position,
  models::{
    DirectionContainer, NonTilingWindow, SplitContainer, TilingContainer,
    TilingWindow, WindowContainer,
  },
  traits::{
    CommonGetters, PositionGetters, TilingDirectionGetters, WindowGetters,
  },
  user_config::UserConfig,
  wm_state::WmState,
};

/// Handles the event for when a window is finished being moved or resized
/// by the user (e.g. via the window's drag handles).
///
/// This resizes the window if it's a tiling window and attach a dragged
/// floating window.
///
/// TODO: Move this to a better location - maybe a new `active_drag_ext`
/// mod.
pub fn handle_window_moved_or_resized_end(
  window: &WindowContainer,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let Some(active_drag) = window.active_drag() else {
    return Ok(());
  };

  let result = finish_drag(window, &active_drag, state, config);

  // A drag that could not be finished must still end. The window would
  // otherwise stay out of the layout, and Windows reports the end of a
  // drag only once.
  if result.is_err() {
    window.set_active_drag(None);
    state.pending_sync.queue_container_to_redraw(window.clone());
  }

  result
}

/// Applies the result of a finished drag and clears the drag state.
fn finish_drag(
  window: &WindowContainer,
  active_drag: &ActiveDrag,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  match &window {
    WindowContainer::NonTilingWindow(window) => {
      // Bind first; a match holds the native borrow.
      let is_maximized = window.native().is_maximized();
      let is_maximized = is_maximized.unwrap_or_else(|err| {
        tracing::warn!("Kept cached maximized state: {err}");
        window.is_native_maximized()
      });

      window.update_native_properties(|properties| {
        properties.is_maximized = is_maximized;
      });

      let nearest_monitor = state
        .nearest_monitor(&window.native())
        .context("Failed to get workspace of nearest monitor.")?;

      let should_fullscreen = window.should_fullscreen(
        &nearest_monitor
          .displayed_workspace()
          .context("No workspace.")?,
      )?;

      if is_maximized || should_fullscreen {
        let fullscreen_state = if let WindowState::Fullscreen(
          fullscreen_state,
        ) = window.state()
        {
          fullscreen_state
        } else {
          config
            .value
            .window_behavior
            .state_defaults
            .fullscreen
            .clone()
        };

        let window = update_window_state(
          window.clone().into(),
          WindowState::Fullscreen(FullscreenStateConfig {
            maximized: is_maximized,
            ..fullscreen_state
          }),
          state,
          config,
        )?;

        window.set_active_drag(None);

        if is_maximized {
          // Dequeue the window from redraw if it's maximized, since the
          // window is already in the correct state.
          state
            .pending_sync
            .dequeue_container_from_redraw(window.clone());
        } else {
          // Force a redraw to snap the window to the monitor edges.
          // TODO: Skip redraw if it's already matches fullscreen frame.
          state.pending_sync.queue_container_to_redraw(window.clone());
        }

        return Ok(());
      }

      if active_drag.is_from_floating {
        update_floating_window_position(
          window,
          window.native_frame(),
          &nearest_monitor,
          state,
        )?;
        window.set_active_drag(None);
      } else {
        // Window is a temporary floating window that should be
        // reverted back to tiling.
        let window = drop_as_tiling_window(window, state, config)?;
        window.set_active_drag(None);
      }
    }
    WindowContainer::TilingWindow(window) => {
      finish_tiling_drag(window, active_drag, state)?;
    }
  }

  Ok(())
}

/// Applies a finished drag of a tiling window and clears the drag state.
///
/// A resize moves the dragged edge in the layout. A move that did not
/// turn the window floating leaves the layout as it was, and the window
/// is put back on its tile.
fn finish_tiling_drag(
  window: &TilingWindow,
  active_drag: &ActiveDrag,
  state: &mut WmState,
) -> anyhow::Result<()> {
  tracing::info!(
    "Tiling window move/resize ended: {}",
    window.as_window_container()?
  );

  let frame = drag_end_frame(window, state);
  let border_delta = window.border_delta();

  apply_dragged_edges(
    window,
    &active_drag.initial_position,
    &frame,
    state,
  )?;

  // The window is already where the layout puts it when the resize
  // was applied in full. It is redrawn otherwise, which snaps it back
  // when:
  // - A size limit held part of the resize back.
  // - The window is the only tiling window in the workspace.
  // - The window is not past the movement threshold for transitioning to
  //   floating while being dragged.
  // - The resize was in a direction that cannot change the window's tiling
  //   size.
  let is_settled =
    window.to_rect()?.apply_delta(&border_delta, None) == frame;

  if is_settled {
    state
      .pending_sync
      .dequeue_container_from_redraw(window.clone());
  } else {
    state.pending_sync.queue_container_to_redraw(window.clone());
  }

  window.set_active_drag(None);

  Ok(())
}

/// Moves, in the layout, the edges of a tiling window that a drag has
/// moved from `start` to `frame`.
///
/// Each moved edge moves the boundary it lies on to where the edge now
/// is (see `Boundary`), and the tiles that changed are queued for
/// redraw. Applying the same frame twice changes nothing the second
/// time, so this serves both the drag while it runs and its end.
///
/// Only an axis whose length the drag changed is applied. The other one
/// can differ from its tile for reasons of its own, such as an
/// application that rounds its size, and applying it would resize a
/// split that nobody touched.
pub(crate) fn apply_dragged_edges(
  window: &TilingWindow,
  start: &Rect,
  frame: &Rect,
  state: &mut WmState,
) -> anyhow::Result<()> {
  // Where the layout has the frame now. A boundary along one axis does
  // not move an edge along the other, so one reading serves both.
  let tile_frame =
    window.to_rect()?.apply_delta(&window.border_delta(), None);

  let axes = [
    (
      (start.left, start.right),
      (frame.left, frame.right),
      (tile_frame.left, tile_frame.right),
      true,
    ),
    (
      (start.top, start.bottom),
      (frame.top, frame.bottom),
      (tile_frame.top, tile_frame.bottom),
      false,
    ),
  ];

  for (start, end, tile, is_width) in axes {
    let Some(edge) = dragged_edge(start, end) else {
      continue;
    };

    let delta = match edge {
      ResizeEdge::Start => end.0 - tile.0,
      ResizeEdge::End => end.1 - tile.1,
    };

    if let Some(boundary) =
      Boundary::find(&window.as_tiling_container()?, is_width, &[edge])
    {
      boundary.shift(delta, state)?;
    }
  }

  Ok(())
}

/// The frame a dragged tiling window ended on, which is also cached as
/// the window's frame.
///
/// The cached frame is from the last move event handled, and an
/// application busy resizing its window delivers those late. The window
/// server has where the drag really ended; the cached frame stands in
/// when it cannot be asked.
fn drag_end_frame(window: &TilingWindow, state: &WmState) -> Rect {
  let frame = state
    .native_sync
    .visible_frame(window.id())
    .unwrap_or_else(|| window.native_frame());

  window.update_native_properties(|properties| {
    properties.frame = frame.clone();
  });

  frame
}

/// The edge that a drag moved along one axis, or `None` when the drag
/// did not change the length of that axis.
///
/// `start` and `end` each hold the positions of the two edges of the
/// axis, before and after. The edge that moved further is the dragged
/// one. The other can be off by a few pixels where the two frames were
/// read in different ways, so it is not required to have stood still.
fn dragged_edge(start: (i32, i32), end: (i32, i32)) -> Option<ResizeEdge> {
  (end.1 - end.0 != start.1 - start.0).then(|| {
    if end.0.abs_diff(start.0) > end.1.abs_diff(start.1) {
      ResizeEdge::Start
    } else {
      ResizeEdge::End
    }
  })
}

/// Handles transition from temporary floating window to tiling window on
/// drag end.
#[allow(clippy::too_many_lines)]
fn drop_as_tiling_window(
  moved_window: &NonTilingWindow,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<WindowContainer> {
  tracing::info!(
    "Tiling window drag ended: {}",
    moved_window.as_window_container()?
  );

  let mouse_pos = state.dispatcher.cursor_position()?;
  let mouse_workspace = state
    .monitor_at_point(&mouse_pos)
    .and_then(|monitor| monitor.displayed_workspace())
    .or_else(|| moved_window.workspace())
    .context("Couldn't find workspace for window drop.")?;

  // Get the workspace, split containers, and other windows under the
  // dragged window.
  let containers_at_pos = state
    .containers_at_point(&mouse_workspace.clone().into(), &mouse_pos)
    .into_iter()
    .filter(|container| container.id() != moved_window.id());

  // Get the deepest direction container under the dragged window.
  let target_parent: DirectionContainer = containers_at_pos
    .filter_map(|container| container.as_direction_container().ok())
    .fold(mouse_workspace.into(), |acc, container| {
      if container.ancestors().count() > acc.ancestors().count() {
        container
      } else {
        acc
      }
    });

  // If the target parent has no children (i.e. an empty workspace), then
  // add the window directly.
  if target_parent.tiling_children().count() == 0 {
    move_container_within_tree(
      &moved_window.clone().into(),
      &target_parent.clone().into(),
      0,
      state,
    )?;

    moved_window.set_insertion_target(None);

    return update_window_state(
      moved_window.as_window_container()?,
      WindowState::Tiling,
      state,
      config,
    );
  }

  let nearest_container = target_parent
    .children()
    .into_iter()
    .filter_map(|container| container.as_tiling_container().ok())
    .try_fold(None, |acc: Option<TilingContainer>, container| match acc {
      Some(acc) => {
        let is_nearer = acc.to_rect()?.distance_to_point(&mouse_pos)
          < container.to_rect()?.distance_to_point(&mouse_pos);

        anyhow::Ok(Some(if is_nearer { acc } else { container }))
      }
      None => Ok(Some(container)),
    })?
    .context("No nearest container.")?;

  let tiling_direction = target_parent.tiling_direction();
  let drop_position =
    drop_position(&mouse_pos, &nearest_container.to_rect()?);

  let moved_window = update_window_state(
    moved_window.clone().into(),
    WindowState::Tiling,
    state,
    config,
  )?;

  let should_split = nearest_container.is_tiling_window()
    && match tiling_direction {
      TilingDirection::Horizontal => {
        drop_position == DropPosition::Top
          || drop_position == DropPosition::Bottom
      }
      TilingDirection::Vertical => {
        drop_position == DropPosition::Left
          || drop_position == DropPosition::Right
      }
    };

  if should_split {
    let split_container = SplitContainer::new(
      tiling_direction.inverse(),
      config.value.gaps.clone(),
    );

    wrap_in_split_container(
      &split_container,
      &target_parent.clone().into(),
      &[nearest_container],
    )?;

    let target_index = match drop_position {
      DropPosition::Top | DropPosition::Left => 0,
      _ => 1,
    };

    move_container_within_tree(
      &moved_window.clone().into(),
      &split_container.into(),
      target_index,
      state,
    )?;
  } else {
    let target_index = match drop_position {
      DropPosition::Top | DropPosition::Left => nearest_container.index(),
      _ => nearest_container.index() + 1,
    };

    move_container_within_tree(
      &moved_window.clone().into(),
      &target_parent.clone().into(),
      target_index,
      state,
    )?;
  }

  state.pending_sync.queue_container_to_redraw(target_parent);

  Ok(moved_window)
}

/// Represents where the window was dropped over another.
#[derive(Debug, Clone, PartialEq)]
enum DropPosition {
  Top,
  Bottom,
  Left,
  Right,
}

/// Gets the drop position for a window based on the mouse position.
///
/// This approach divides the window rect into an "X", creating four
/// triangular quadrants, to determine which side the cursor is closest to.
fn drop_position(mouse_pos: &Point, rect: &Rect) -> DropPosition {
  let delta_x = mouse_pos.x - rect.center_point().x;
  let delta_y = mouse_pos.y - rect.center_point().y;

  if delta_x.abs() > delta_y.abs() {
    // Window is in the left or right triangle.
    if delta_x > 0 {
      DropPosition::Right
    } else {
      DropPosition::Left
    }
  } else {
    // Window is in the top or bottom triangle.
    if delta_y > 0 {
      DropPosition::Bottom
    } else {
      DropPosition::Top
    }
  }
}

#[cfg(test)]
mod tests {
  use tokio::sync::mpsc::UnboundedReceiver;
  use wm_common::{ActiveDragOperation, ParsedConfig, WmEvent};

  use super::*;
  use crate::{
    models::{MinSizeSource, Monitor, Workspace},
    test_utils::{attach_monitor, mock_state},
    traits::TilingSizeGetters,
  };

  /// Three windows in a row on a gapless 1680px workspace, 560px each.
  fn row_of_three(
  ) -> (WmState, UnboundedReceiver<WmEvent>, [TilingWindow; 3]) {
    let windows = [(); 3].map(|()| TilingWindow::mock().call());
    let workspace = Workspace::mock()
      .tiling_containers(windows.iter().cloned().map(Into::into).collect())
      .call();
    let monitor = Monitor::mock().workspaces(vec![workspace]).call();
    let (state, events) = mock_state();
    attach_monitor(&state, &monitor).expect("Attach monitor.");

    for window in &windows {
      window.set_tiling_size(1. / 3.);
    }

    (state, events, windows)
  }

  /// Ends a drag of `window` that began on its tile and left it at
  /// `frame`.
  fn release_at(window: &TilingWindow, frame: Rect, state: &mut WmState) {
    let tile = window.to_rect().expect("Tile.");
    window.update_native_properties(|properties| {
      properties.frame = frame;
    });
    window.set_active_drag(Some(ActiveDrag {
      operation: Some(ActiveDragOperation::Resize),
      is_from_floating: false,
      initial_position: tile,
    }));

    handle_window_moved_or_resized_end(
      &window.clone().into(),
      state,
      &UserConfig::mock(ParsedConfig::default()),
    )
    .expect("Drag end.");
  }

  /// Asserts the tiling sizes of the row, in order.
  fn assert_sizes(windows: &[TilingWindow; 3], expected: [f32; 3]) {
    let sizes = windows
      .iter()
      .map(TilingSizeGetters::tiling_size)
      .collect::<Vec<_>>();

    for (size, expected) in sizes.iter().zip(expected) {
      assert!(
        (size - expected).abs() < 0.0001,
        "sizes: {sizes:?}, expected: {expected:?}"
      );
    }
  }

  /// The window on the left keeps its size when the right edge of the
  /// middle window is dragged, and the drag is over afterwards.
  #[test]
  fn releasing_a_right_edge_resizes_against_the_right_neighbour() {
    let (mut state, _events, windows) = row_of_three();
    let tile = windows[1].to_rect().expect("Tile.");

    release_at(
      &windows[1],
      Rect::from_ltrb(tile.left, tile.top, tile.right + 100, tile.bottom),
      &mut state,
    );

    assert_sizes(&windows, [1. / 3., 660. / 1680., 460. / 1680.]);
    assert!(windows[1].active_drag().is_none());
  }

  /// The window on the right keeps its size when the left edge of the
  /// middle window is dragged.
  #[test]
  fn releasing_a_left_edge_resizes_against_the_left_neighbour() {
    let (mut state, _events, windows) = row_of_three();
    let tile = windows[1].to_rect().expect("Tile.");

    release_at(
      &windows[1],
      Rect::from_ltrb(tile.left - 100, tile.top, tile.right, tile.bottom),
      &mut state,
    );

    assert_sizes(&windows, [460. / 1680., 660. / 1680., 1. / 3.]);
  }

  /// The drag applies its edges as it runs, and the release applies the
  /// same frame again. The second time must change nothing.
  #[test]
  fn applying_the_same_dragged_frame_twice_changes_nothing_more() {
    let (mut state, _events, windows) = row_of_three();
    let tile = windows[1].to_rect().expect("Tile.");
    let frame =
      Rect::from_ltrb(tile.left, tile.top, tile.right + 100, tile.bottom);

    apply_dragged_edges(&windows[1], &tile, &frame, &mut state)
      .expect("First.");
    assert_sizes(&windows, [1. / 3., 660. / 1680., 460. / 1680.]);

    apply_dragged_edges(&windows[1], &tile, &frame, &mut state)
      .expect("Second.");
    assert_sizes(&windows, [1. / 3., 660. / 1680., 460. / 1680.]);
  }

  /// A drag that moved the window and kept its size changes no tile.
  #[test]
  fn releasing_a_moved_window_keeps_every_size() {
    let (mut state, _events, windows) = row_of_three();
    let tile = windows[1].to_rect().expect("Tile.");

    release_at(
      &windows[1],
      Rect::from_ltrb(
        tile.left + 5,
        tile.top + 5,
        tile.right + 5,
        tile.bottom + 5,
      ),
      &mut state,
    );

    assert_sizes(&windows, [1. / 3.; 3]);
    assert!(windows[1].active_drag().is_none());
  }

  /// The right edge of the first window was dragged over a neighbour
  /// that is at its reported floor. The neighbour slides along, and the
  /// last window gives up the space.
  #[test]
  fn releasing_an_edge_over_a_tile_at_its_floor_takes_from_the_next() {
    let (mut state, _events, windows) = row_of_three();
    windows[1].update_native_properties(|properties| {
      properties.min_size = Some((560, 0));
      properties.min_size_source = MinSizeSource::Reported;
    });
    let tile = windows[0].to_rect().expect("Tile.");

    release_at(
      &windows[0],
      Rect::from_ltrb(tile.left, tile.top, tile.right + 200, tile.bottom),
      &mut state,
    );

    assert_sizes(&windows, [760. / 1680., 560. / 1680., 360. / 1680.]);
  }

  #[test]
  fn an_unchanged_length_is_not_a_dragged_edge() {
    // A move changes the position and keeps the length.
    assert_eq!(dragged_edge((100, 900), (140, 940)), None);
  }

  #[test]
  fn a_still_start_edge_means_the_end_edge_moved() {
    assert_eq!(
      dragged_edge((100, 900), (100, 960)),
      Some(ResizeEdge::End)
    );
  }

  #[test]
  fn a_moved_start_edge_is_the_dragged_edge() {
    assert_eq!(
      dragged_edge((100, 900), (40, 900)),
      Some(ResizeEdge::Start)
    );
  }

  /// The start frame was read 7px wider on each side than the end
  /// frame. The right edge still moved further, so it is the dragged one.
  #[test]
  fn a_start_frame_read_wider_does_not_change_the_edge() {
    assert_eq!(
      dragged_edge((93, 907), (100, 1000)),
      Some(ResizeEdge::End)
    );
  }
}
