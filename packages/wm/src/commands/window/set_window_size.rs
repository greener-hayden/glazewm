use anyhow::Context;
use wm_common::WindowState;
use wm_platform::{LengthValue, Rect};

use crate::{
  commands::container::{Boundary, ResizeEdge},
  models::{NonTilingWindow, TilingWindow, WindowContainer},
  traits::{CommonGetters, PositionGetters, WindowGetters},
  wm_state::WmState,
};

/// Arbitrary defaults for minimum floating window dimensions.
const MIN_FLOATING_WIDTH: i32 = 250;
const MIN_FLOATING_HEIGHT: i32 = 140;

/// Sets the size of a window.
///
/// A tiling window is resized by moving a boundary (see
/// `grow_tiling_window`), so a length is reached only as far as the
/// floors of the tiles that pay for it allow.
pub fn set_window_size(
  window: WindowContainer,
  target_width: Option<LengthValue>,
  target_height: Option<LengthValue>,
  state: &mut WmState,
) -> anyhow::Result<()> {
  match window {
    WindowContainer::TilingWindow(window) => {
      for (target, is_width) in
        [(target_width, true), (target_height, false)]
      {
        let Some(target) = target else {
          continue;
        };

        let rect = window.to_rect()?;
        let current = if is_width {
          rect.width()
        } else {
          rect.height()
        };

        grow_tiling_window(
          &window,
          is_width,
          |row_length| target.to_px(row_length, None) - current,
          state,
        )?;
      }
    }
    WindowContainer::NonTilingWindow(window) => {
      if matches!(window.state(), WindowState::Floating(_)) {
        set_floating_window_size(
          &window,
          target_width,
          target_height,
          state,
        )?;
      }
    }
  }

  Ok(())
}

/// Grows a tiling window along one axis, or shrinks it when negative.
///
/// Moves the boundary of the nearest row in which the window has a
/// neighbour: the one after it, or the one before it for the last tile
/// of a row. Every other boundary stays where it is.
///
/// `px` is given the length that the tiles of that row share out, which
/// a percentage is taken from, and returns the pixels to grow by.
/// Nothing happens where no row has a neighbour, as for the only window
/// of a workspace.
pub(super) fn grow_tiling_window(
  window: &TilingWindow,
  is_width: bool,
  px: impl FnOnce(i32) -> i32,
  state: &mut WmState,
) -> anyhow::Result<()> {
  let Some(boundary) = Boundary::find(
    &window.as_tiling_container()?,
    is_width,
    &[ResizeEdge::End, ResizeEdge::Start],
  ) else {
    return Ok(());
  };

  boundary.grow(px(boundary.row_length()?), state)?;

  Ok(())
}

fn set_floating_window_size(
  window: &NonTilingWindow,
  target_width: Option<LengthValue>,
  target_height: Option<LengthValue>,
  state: &mut WmState,
) -> anyhow::Result<()> {
  let monitor = window.monitor().context("No monitor")?;
  let monitor_rect = monitor.to_rect()?;
  let window_rect = window.to_rect()?;

  // Prevent resize from making the window smaller than minimum dimensions.
  // Always allow the size to be increased, even if the window would still
  // be within minimum dimension values.
  let length_with_clamp =
    |target_length: Option<i32>, current_length, min_length| {
      target_length.map_or(current_length, |target_length| {
        if target_length >= current_length {
          target_length
        } else {
          target_length.max(min_length)
        }
      })
    };

  let target_width_px = target_width
    .map(|target_width| target_width.to_px(monitor_rect.width(), None));

  let new_width = length_with_clamp(
    target_width_px,
    window_rect.width(),
    MIN_FLOATING_WIDTH,
  );

  let target_height_px = target_height
    .map(|target_height| target_height.to_px(monitor_rect.height(), None));

  let new_height = length_with_clamp(
    target_height_px,
    window_rect.height(),
    MIN_FLOATING_HEIGHT,
  );

  window.set_floating_placement(Rect::from_xy(
    window.floating_placement().x(),
    window.floating_placement().y(),
    new_width,
    new_height,
  ));

  state.pending_sync.queue_container_to_redraw(window.clone());

  Ok(())
}

#[cfg(test)]
mod tests {
  use tokio::sync::mpsc::UnboundedReceiver;
  use wm_common::{GapsConfig, TilingDirection, WmEvent};

  use super::*;
  use crate::{
    commands::window::resize_window,
    models::{MinSizeSource, Monitor, SplitContainer, Workspace},
    test_utils::{attach_monitor, mock_state},
    traits::TilingSizeGetters,
  };

  /// A horizontal workspace holding window `a` and a vertical split of
  /// `b` and a horizontal split of `c` and `d`, with 10px inner gaps and a
  /// floor on every window. Keeps the windows that tests resize or read.
  struct Fixture {
    state: WmState,
    _events: UnboundedReceiver<WmEvent>,
    b: TilingWindow,
    c: TilingWindow,
    d: TilingWindow,
  }

  impl Fixture {
    /// The fixture with every floor known from `source`.
    fn new(source: MinSizeSource) -> Self {
      let gaps = GapsConfig {
        inner_gap: LengthValue::from_px(10),
        ..GapsConfig::default()
      };
      let window = |floor: (i32, i32)| {
        let window = TilingWindow::mock().gaps_config(gaps.clone()).call();
        window.update_native_properties(|properties| {
          properties.min_size = Some(floor);
          properties.min_size_source = source;
        });
        window
      };
      let (a, b, c, d) = (
        window((200, 100)),
        window((100, 200)),
        window((300, 150)),
        window((150, 100)),
      );
      let lower = SplitContainer::mock()
        .tiling_direction(TilingDirection::Horizontal)
        .gaps_config(gaps.clone())
        .tiling_containers(vec![c.clone().into(), d.clone().into()])
        .call();
      let upper = SplitContainer::mock()
        .tiling_direction(TilingDirection::Vertical)
        .gaps_config(gaps.clone())
        .tiling_containers(vec![b.clone().into(), lower.into()])
        .call();
      let workspace = Workspace::mock()
        .gaps_config(gaps)
        .tiling_containers(vec![a.into(), upper.into()])
        .call();
      let monitor = Monitor::mock().workspaces(vec![workspace]).call();
      let (state, events) = mock_state();
      attach_monitor(&state, &monitor).expect("Attach monitor.");

      Self {
        state,
        _events: events,
        b,
        c,
        d,
      }
    }

    /// Resizes a window of the fixture by pixel deltas.
    fn resize(
      &mut self,
      window: &TilingWindow,
      width: Option<i32>,
      height: Option<i32>,
    ) {
      resize_window(
        &window.clone().into(),
        width.map(LengthValue::from_px),
        height.map(LengthValue::from_px),
        &mut self.state,
      )
      .expect("Resize.");
    }

    /// Asserts the tiling size of every tiling container, breadth
    /// first: `a`, the vertical split, `b`, the horizontal split, `c`,
    /// `d`.
    fn assert_sizes(&self, expected: [f32; 6]) {
      let sizes = self
        .state
        .root_container
        .descendants()
        .filter_map(|container| container.as_tiling_container().ok())
        .map(|container| container.tiling_size())
        .collect::<Vec<_>>();

      assert_eq!(sizes.len(), expected.len(), "sizes: {sizes:?}");
      for (size, expected) in sizes.iter().zip(expected) {
        assert!(
          (size - expected).abs() < 0.0001,
          "sizes: {sizes:?}, expected: {expected:?}"
        );
      }
    }
  }

  // The expected sizes below follow from the layout of the fixture: the
  // workspace is 1680x1000, so with a 10px gap the top row gives `a` and
  // the vertical split 835px each, which gives `b` and the horizontal
  // split 495px each, which gives `c` 413px and `d` 412px of the 825px
  // between them.

  /// `d` is the last tile of its row, so its left boundary moves: 472px
  /// of 825px for `d`, the rest for `c`.
  #[test]
  fn width_moves_the_boundary_of_the_own_row() {
    let mut fixture = Fixture::new(MinSizeSource::Reported);
    let d = fixture.d.clone();
    fixture.resize(&d, Some(60), None);

    fixture.assert_sizes([0.5, 0.5, 0.5, 0.5, 353. / 825., 472. / 825.]);
  }

  /// The row of `d` runs along the width, so its height is that of the
  /// horizontal split: 555px of 990px, the rest for `b`.
  #[test]
  fn height_moves_the_boundary_of_the_split_above() {
    let mut fixture = Fixture::new(MinSizeSource::Reported);
    let d = fixture.d.clone();
    fixture.resize(&d, None, Some(60));

    fixture.assert_sizes([0.5, 0.5, 435. / 990., 555. / 990., 0.5, 0.5]);
  }

  #[test]
  fn width_and_height_are_resized_independently() {
    let mut fixture = Fixture::new(MinSizeSource::Reported);
    let d = fixture.d.clone();
    fixture.resize(&d, Some(60), Some(60));

    fixture.assert_sizes([
      0.5,
      0.5,
      435. / 990.,
      555. / 990.,
      353. / 825.,
      472. / 825.,
    ]);
  }

  /// `b` is alone in its row along the width, so the vertical split is
  /// what grows, against `a`.
  #[test]
  fn width_of_a_stacked_window_resizes_its_split() {
    let mut fixture = Fixture::new(MinSizeSource::Reported);
    let b = fixture.b.clone();
    fixture.resize(&b, Some(100), None);

    fixture.assert_sizes([735. / 1670., 935. / 1670., 0.5, 0.5, 0.5, 0.5]);
  }

  /// A reported floor on the tile that pays caps how far a window can
  /// grow: `c` keeps its 300px.
  #[test]
  fn growth_stops_at_a_reported_floor() {
    let mut fixture = Fixture::new(MinSizeSource::Reported);
    let d = fixture.d.clone();
    fixture.resize(&d, Some(5000), None);

    fixture.assert_sizes([0.5, 0.5, 0.5, 0.5, 300. / 825., 525. / 825.]);
  }

  /// The same request with inferred floors. The width floor of `c` is
  /// forgotten, so `d` takes all but the smallest tile of 9px. The height
  /// floor of `c` is not in the way and stays.
  #[test]
  fn growth_past_an_inferred_floor_releases_it() {
    let mut fixture = Fixture::new(MinSizeSource::Inferred);
    let d = fixture.d.clone();
    fixture.resize(&d, Some(5000), None);

    assert_eq!(fixture.c.min_size(), Some((0, 150)));
    assert_eq!(fixture.d.min_size(), Some((150, 100)));
    fixture.assert_sizes([0.5, 0.5, 0.5, 0.5, 9. / 825., 816. / 825.]);
  }

  /// `d` -400px wide: 12px of 825px, under its own 150px floor, which is
  /// forgotten.
  #[test]
  fn shrinking_below_an_own_inferred_floor_releases_it() {
    let mut fixture = Fixture::new(MinSizeSource::Inferred);
    let d = fixture.d.clone();
    fixture.resize(&d, Some(-400), None);

    assert_eq!(fixture.d.min_size(), Some((0, 100)));
    assert_eq!(fixture.c.min_size(), Some((300, 150)));
    fixture.assert_sizes([0.5, 0.5, 0.5, 0.5, 813. / 825., 12. / 825.]);
  }

  /// A request that fits within the floors has no reason to re-test
  /// them.
  #[test]
  fn a_request_within_inferred_floors_keeps_them() {
    let mut fixture = Fixture::new(MinSizeSource::Inferred);
    let d = fixture.d.clone();
    fixture.resize(&d, Some(60), None);

    assert_eq!(fixture.c.min_size(), Some((300, 150)));
    assert_eq!(fixture.d.min_size(), Some((150, 100)));
  }

  /// An absolute size is reached by the same boundary: `d` at 40% of
  /// the 825px its row shares out is 330px.
  #[test]
  fn a_size_is_set_through_the_same_boundary() {
    let mut fixture = Fixture::new(MinSizeSource::Reported);
    set_window_size(
      fixture.d.clone().into(),
      Some("40%".parse().expect("Length.")),
      None,
      &mut fixture.state,
    )
    .expect("Size.");

    fixture.assert_sizes([0.5, 0.5, 0.5, 0.5, 495. / 825., 330. / 825.]);
  }

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

  /// Widths of the row as the layout shows them.
  fn widths(windows: &[TilingWindow; 3]) -> Vec<i32> {
    windows
      .iter()
      .map(|window| window.to_rect().expect("Rect.").width())
      .collect()
  }

  /// Growing the middle window moves its right boundary. The window on
  /// its left keeps its width.
  #[test]
  fn a_middle_window_grows_towards_the_end_of_its_row() {
    let (mut state, _events, windows) = row_of_three();

    resize_window(
      &windows[1].clone().into(),
      Some(LengthValue::from_px(100)),
      None,
      &mut state,
    )
    .expect("Resize.");

    assert_eq!(widths(&windows), [560, 660, 460]);
  }

  /// A step out and a step back return to the start exactly, since both
  /// move the same boundary by the same whole number of pixels.
  #[test]
  fn a_resize_step_out_and_back_returns_to_the_start() {
    for step in ["2%", "3%", "7%"] {
      let (mut state, _events, windows) = row_of_three();
      let start = widths(&windows);

      for _ in 0..200 {
        for sign in ["+", "-"] {
          resize_window(
            &windows[1].clone().into(),
            Some(format!("{sign}{step}").parse().expect("Length.")),
            None,
            &mut state,
          )
          .expect("Resize.");
        }

        assert_eq!(widths(&windows), start, "step {step}");
      }
    }
  }
}
