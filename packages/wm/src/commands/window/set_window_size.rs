use anyhow::Context;
use wm_common::WindowState;
use wm_platform::{LengthValue, Rect};

use crate::{
  commands::container::resize_tiling_container,
  models::{
    Container, NonTilingWindow, TilingContainer, TilingWindow,
    WindowContainer,
  },
  traits::{
    CommonGetters, PositionGetters, TilingSizeGetters, WindowGetters,
  },
  wm_state::WmState,
};

/// Arbitrary defaults for minimum floating window dimensions.
const MIN_FLOATING_WIDTH: i32 = 250;
const MIN_FLOATING_HEIGHT: i32 = 140;

/// What resizing a tiling window along one axis starts from.
///
/// A caller that has already resolved these for its own sums passes them
/// on in `ResizeContexts`, so each is solved once per command.
pub struct ResizeContext {
  /// The container whose tiling size changes. Can be an ancestor split.
  container_to_resize: TilingContainer,

  /// Parent of `container_to_resize`.
  parent: Container,

  /// Rectangle of `parent` when this was resolved.
  pub(super) parent_rect: Rect,

  /// Tiling siblings of the window being resized.
  pub(super) sibling_count: usize,
}

impl ResizeContext {
  /// Resolves the context of a resize along an axis.
  ///
  /// Returns `None` when nothing can be resized along that axis.
  pub fn resolve(
    window: &TilingWindow,
    is_width_resize: bool,
  ) -> anyhow::Result<Option<Self>> {
    window
      .container_to_resize(is_width_resize)?
      .map(|container_to_resize| {
        Self::from_container(window, container_to_resize)
      })
      .transpose()
  }

  /// Resolves the context for a known `container_to_resize`.
  pub fn from_container(
    window: &TilingWindow,
    container_to_resize: TilingContainer,
  ) -> anyhow::Result<Self> {
    let parent = container_to_resize.parent().context("No parent.")?;
    let parent_rect = parent.to_rect()?;

    Ok(Self {
      container_to_resize,
      parent,
      parent_rect,
      sibling_count: window.tiling_sibling_count(),
    })
  }
}

/// Contexts that a caller has already resolved for each axis.
///
/// A context is only good while no tiling size has changed since it was
/// resolved, so the height context is dropped once the width has been
/// resized.
#[derive(Default)]
pub struct ResizeContexts {
  pub width: Option<ResizeContext>,
  pub height: Option<ResizeContext>,
}

pub fn set_window_size(
  window: WindowContainer,
  target_width: Option<LengthValue>,
  target_height: Option<LengthValue>,
  state: &mut WmState,
) -> anyhow::Result<()> {
  set_window_size_with_contexts(
    window,
    target_width,
    target_height,
    ResizeContexts::default(),
    state,
  )
}

/// Sets the size of a window, reusing contexts the caller has resolved.
///
/// Gives the same result as `set_window_size`, which resolves each
/// context itself.
pub fn set_window_size_with_contexts(
  window: WindowContainer,
  target_width: Option<LengthValue>,
  target_height: Option<LengthValue>,
  contexts: ResizeContexts,
  state: &mut WmState,
) -> anyhow::Result<()> {
  match window {
    WindowContainer::TilingWindow(window) => {
      set_tiling_window_size(
        &window,
        target_width,
        target_height,
        contexts,
        state,
      )?;
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

fn set_tiling_window_size(
  window: &TilingWindow,
  target_width: Option<LengthValue>,
  target_height: Option<LengthValue>,
  contexts: ResizeContexts,
  state: &mut WmState,
) -> anyhow::Result<()> {
  let ResizeContexts { width, height } = contexts;
  let mut resized = false;

  if let Some(target_width) = target_width {
    resized =
      set_tiling_window_length(window, &target_width, true, width, state)?;
  }

  if let Some(target_height) = target_height {
    // Resizing the width moved tiling sizes, which the height context
    // was resolved before.
    let height = if resized { None } else { height };
    set_tiling_window_length(
      window,
      &target_height,
      false,
      height,
      state,
    )?;
  }

  Ok(())
}

/// Updates either the width or height of a tiling window.
///
/// Resolves the context itself unless given one. Returns whether the
/// layout changed: a tiling size was set, or a floor was forgotten.
fn set_tiling_window_length(
  window: &TilingWindow,
  target_length: &LengthValue,
  is_width_resize: bool,
  context: Option<ResizeContext>,
  state: &mut WmState,
) -> anyhow::Result<bool> {
  // When resizing a tiling window, the container to resize can actually be
  // an ancestor split container.
  let context = match context {
    Some(context) => Some(context),
    None => ResizeContext::resolve(window, is_width_resize)?,
  };

  if let Some(ResizeContext {
    container_to_resize,
    parent,
    parent_rect,
    sibling_count,
  }) = context
  {
    let (horizontal_gap, vertical_gap) =
      container_to_resize.inner_gaps()?;

    #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
    let parent_length = if is_width_resize {
      parent_rect.width() - horizontal_gap * sibling_count as i32
    } else {
      parent_rect.height() - vertical_gap * sibling_count as i32
    };

    // Without room to resize into, a pixel length has no percentage.
    if parent_length <= 0 {
      return Ok(false);
    }

    let requested_px = target_length.to_px(parent_length, None);

    // A sibling pinned to its floor cannot give the space up, and the
    // window cannot take less than its own; asking would only be undone.
    let (mut floor, mut ceiling) =
      length_limits(&container_to_resize, parent_length, is_width_resize);

    // That holds for a floor the application reported. An inferred one
    // is re-tested instead, so the limits are taken again without it.
    let released = release_floors_in_the_way(
      &container_to_resize,
      requested_px,
      floor,
      ceiling,
      is_width_resize,
    );

    if released {
      (floor, ceiling) = length_limits(
        &container_to_resize,
        parent_length,
        is_width_resize,
      );
    }

    let target_px = requested_px.clamp(floor, ceiling);

    // Convert the target length to a tiling size.
    let tiling_size =
      LengthValue::from_px(target_px).to_percentage(parent_length);

    // Skip the resize if the window is already at the target size.
    let resized = container_to_resize.tiling_size() - tiling_size != 0.;
    if resized {
      resize_tiling_container(&container_to_resize, tiling_size);
    }

    // A released floor moves the layout even where no tiling size does.
    if resized || released {
      state
        .pending_sync
        .queue_containers_to_redraw(parent.tiling_children());
      return Ok(true);
    }
  }

  Ok(false)
}

/// The shortest and longest that `container` can be along an axis, in
/// pixels, given its own floor and the floors of its siblings.
fn length_limits(
  container: &TilingContainer,
  parent_length: i32,
  is_width_resize: bool,
) -> (i32, i32) {
  let siblings_floor = container
    .tiling_siblings()
    .map(|sibling| sibling.min_length(is_width_resize))
    .sum::<i32>();

  let floor = container.min_length(is_width_resize);
  (floor, (parent_length - siblings_floor).max(floor))
}

/// Forgets the inferred floors that keep `container` from the length
/// asked for: its own when asked for less than `floor`, its siblings'
/// when asked for more than `ceiling`.
///
/// An inferred floor can be wrong, and it cannot be disproved from
/// outside, since the layout never asks a window for less than its
/// floor. The user asking is the reason to ask the window again. A
/// request within the limits leaves every floor alone.
///
/// Returns whether any floor was forgotten.
fn release_floors_in_the_way(
  container: &TilingContainer,
  requested_px: i32,
  floor: i32,
  ceiling: i32,
  is_width_resize: bool,
) -> bool {
  if requested_px < floor {
    container.release_inferred_floors(is_width_resize)
  } else if requested_px > ceiling {
    // Every sibling is asked, so this cannot stop at the first.
    let mut released = false;
    for sibling in container.tiling_siblings() {
      released |= sibling.release_inferred_floors(is_width_resize);
    }
    released
  } else {
    false
  }
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
    layout_snapshot::tests::{reference_min_length, reference_to_rect},
    models::{MinSizeSource, Monitor, SplitContainer, Workspace},
    test_utils::{attach_monitor, mock_state},
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
    /// The fixture with floors that the applications reported.
    fn new() -> Self {
      Self::with_floors(MinSizeSource::Reported)
    }

    /// The fixture with every floor known from `source`.
    fn with_floors(source: MinSizeSource) -> Self {
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
        .tiling_containers(vec![a.clone().into(), upper.into()])
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

    /// Tiling size of every tiling container, breadth first.
    fn sizes(&self) -> Vec<f32> {
      self
        .state
        .root_container
        .descendants()
        .filter_map(|container| container.as_tiling_container().ok())
        .map(|container| container.tiling_size())
        .collect()
    }
  }

  /// The three-chain resize that this module replaced, kept as an oracle.
  ///
  /// Solves the layout from the workspace down for each rectangle it
  /// reads, and resolves every context again for each axis.
  #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
  fn reference_resize(
    window: &TilingWindow,
    width_delta: Option<LengthValue>,
    height_delta: Option<LengthValue>,
  ) -> anyhow::Result<()> {
    let tiling = window.as_tiling_container()?;
    let window_rect = reference_to_rect(&tiling)?;

    let target = |delta: Option<LengthValue>, is_width: bool| {
      let Some(delta) = delta else {
        return anyhow::Ok(None);
      };
      let parent_length = tiling
        .container_to_resize(is_width)?
        .and_then(|container| container.parent())
        .and_then(|parent| {
          let rect = reference_parent_rect(&parent).ok()?;
          Some(if is_width {
            rect.width()
          } else {
            rect.height()
          })
        })
        .and_then(|length| {
          let (horizontal_gap, vertical_gap) = tiling.inner_gaps().ok()?;
          let gap = if is_width {
            horizontal_gap
          } else {
            vertical_gap
          };
          Some(length - gap * tiling.tiling_siblings().count() as i32)
        });
      let current = if is_width {
        window_rect.width()
      } else {
        window_rect.height()
      };

      Ok(parent_length.map(|length| current + delta.to_px(length, None)))
    };

    let target_width = target(width_delta, true)?;
    let target_height = target(height_delta, false)?;

    for (target, is_width) in
      [(target_width, true), (target_height, false)]
    {
      if let Some(px) = target {
        reference_set_length(window, px, is_width)?;
      }
    }

    Ok(())
  }

  /// Rectangle of a direction container, solved from the workspace down.
  fn reference_parent_rect(parent: &Container) -> anyhow::Result<Rect> {
    match parent {
      Container::Workspace(workspace) => workspace.to_rect(),
      Container::Split(split) => {
        reference_to_rect(&split.as_tiling_container()?)
      }
      _ => anyhow::bail!("Not a direction container."),
    }
  }

  /// The old `set_tiling_window_length` for a length in pixels.
  #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
  fn reference_set_length(
    window: &TilingWindow,
    target_px: i32,
    is_width_resize: bool,
  ) -> anyhow::Result<()> {
    let Some(container_to_resize) =
      window.container_to_resize(is_width_resize)?
    else {
      return Ok(());
    };
    let parent = container_to_resize.parent().context("No parent.")?;
    let (horizontal_gap, vertical_gap) =
      container_to_resize.inner_gaps()?;
    let parent_rect = reference_parent_rect(&parent)?;

    let parent_length = if is_width_resize {
      parent_rect.width()
        - horizontal_gap * window.tiling_siblings().count() as i32
    } else {
      parent_rect.height()
        - vertical_gap * window.tiling_siblings().count() as i32
    };
    if parent_length <= 0 {
      return Ok(());
    }

    let siblings_floor = container_to_resize
      .tiling_siblings()
      .map(|sibling| reference_min_length(&sibling, is_width_resize))
      .sum::<i32>();
    let floor =
      reference_min_length(&container_to_resize, is_width_resize);
    let ceiling = (parent_length - siblings_floor).max(floor);
    let target_px = LengthValue::from_px(target_px)
      .to_px(parent_length, None)
      .clamp(floor, ceiling);
    let tiling_size =
      LengthValue::from_px(target_px).to_percentage(parent_length);

    if container_to_resize.tiling_size() - tiling_size != 0. {
      resize_tiling_container(&container_to_resize, tiling_size);
    }

    Ok(())
  }

  /// Resizes `pick`'s window in two fixtures, once with `resize_window`
  /// and once with the oracle, and returns the sizes the first ends on.
  ///
  /// Panics unless both end on exactly the same sizes.
  fn resize_both(
    pick: fn(&Fixture) -> &TilingWindow,
    width: Option<LengthValue>,
    height: Option<LengthValue>,
  ) -> Vec<f32> {
    let mut fixture = Fixture::new();
    let window = pick(&fixture).clone();
    resize_window(
      &window.into(),
      width.clone(),
      height.clone(),
      &mut fixture.state,
    )
    .expect("Resize.");

    let reference = Fixture::new();
    reference_resize(pick(&reference), width, height)
      .expect("Reference resize.");

    assert_eq!(
      fixture.sizes(),
      reference.sizes(),
      "resize_window diverged from the three-chain algorithm"
    );

    fixture.sizes()
  }

  /// Asserts sizes against ones worked out for the base revision.
  fn assert_close(actual: &[f32], expected: &[f32]) {
    assert_eq!(actual.len(), expected.len(), "sizes: {actual:?}");
    for (size, expected) in actual.iter().zip(expected) {
      assert!(
        (size - expected).abs() < 0.0001,
        "sizes: {actual:?}, expected: {expected:?}"
      );
    }
  }

  /// Sizes in the order of `Fixture::sizes`: `a`, the vertical split, `b`,
  /// the horizontal split, `c`, `d`.
  const UNTOUCHED: [f32; 6] = [0.5, 0.5, 0.5, 0.5, 0.5, 0.5];

  // The expected sizes below follow from the layout the base revision
  // solves for this fixture: the workspace is 1680x1000, so with a 10px
  // gap the top row gives `a` and the vertical split 835px each, which
  // gives `b` and the horizontal split 495px each, which gives `c` 413px
  // and `d` 412px of the 825px between them.

  /// `d` +60px wide: 472px of 825px, under the 525px its sibling's 300px
  /// floor leaves it. 472 / 825 for `d`, the rest for `c`.
  const WIDTH_ONLY: [f32; 6] =
    [0.5, 0.5, 0.5, 0.5, 0.427_878_8, 0.572_121_2];

  /// `d` +60px tall: the horizontal split is resized, since the resize is
  /// across its row. 555px of 990px, under the 790px that `b`'s 200px
  /// floor leaves it. 555 / 990 for the split, the rest for `b`.
  const HEIGHT_ONLY: [f32; 6] =
    [0.5, 0.5, 0.439_393_94, 0.560_606_06, 0.5, 0.5];

  /// Both of the above at once.
  const BOTH_CHANGED: [f32; 6] = [
    0.5,
    0.5,
    0.439_393_94,
    0.560_606_06,
    0.427_878_8,
    0.572_121_2,
  ];

  /// `b` +0px wide leaves the vertical split at 835 / 1670, and +100px
  /// tall takes 595px of 990px, under the 840px that the horizontal
  /// split's 150px floor leaves it.
  const WIDTH_NO_OP: [f32; 6] =
    [0.5, 0.5, 0.601_010_1, 0.398_989_9, 0.5, 0.5];

  /// `d` asking for far more is held at 525px of 825px.
  const CAPPED: [f32; 6] = [0.5, 0.5, 0.5, 0.5, 0.363_636_36, 0.636_363_6];

  #[test]
  fn fixture_starts_even() {
    assert_close(&Fixture::new().sizes(), &UNTOUCHED);
  }

  #[test]
  fn width_only() {
    let sizes =
      resize_both(|f| &f.d, Some(LengthValue::from_px(60)), None);
    assert_close(&sizes, &WIDTH_ONLY);
  }

  #[test]
  fn height_only() {
    let sizes =
      resize_both(|f| &f.d, None, Some(LengthValue::from_px(60)));
    assert_close(&sizes, &HEIGHT_ONLY);
  }

  /// The width changes, so the height must not reuse a context from
  /// before it.
  #[test]
  fn width_and_height_where_the_width_changes() {
    let sizes = resize_both(
      |f| &f.d,
      Some(LengthValue::from_px(60)),
      Some(LengthValue::from_px(60)),
    );
    assert_close(&sizes, &BOTH_CHANGED);
  }

  /// The width asks for exactly what the window has, so nothing moves and
  /// the height keeps the context resolved up front.
  #[test]
  fn width_and_height_where_the_width_is_a_no_op() {
    let sizes = resize_both(
      |f| &f.b,
      Some(LengthValue::from_px(0)),
      Some(LengthValue::from_px(100)),
    );
    assert_close(&sizes, &WIDTH_NO_OP);
  }

  /// A floor on a sibling caps how far a window can grow.
  #[test]
  fn growth_stops_at_the_sibling_floor() {
    let sizes =
      resize_both(|f| &f.d, Some(LengthValue::from_px(5000)), None);
    assert_close(&sizes, &CAPPED);
  }

  /// Resizes `d`'s width in a fixture whose floors are all inferred.
  fn resize_d_with_inferred_floors(width_px: i32) -> Fixture {
    let mut fixture = Fixture::with_floors(MinSizeSource::Inferred);
    resize_window(
      &fixture.d.clone().into(),
      Some(LengthValue::from_px(width_px)),
      None,
      &mut fixture.state,
    )
    .expect("Resize.");

    fixture
  }

  /// The same request as `growth_stops_at_the_sibling_floor`. `c`'s
  /// width floor is forgotten, so `d` takes all but the minimum tile;
  /// `c`'s height floor is not in the way and stays.
  #[test]
  fn growth_past_an_inferred_sibling_floor_releases_it() {
    let fixture = resize_d_with_inferred_floors(5000);

    assert_eq!(fixture.c.min_size(), Some((0, 150)));
    assert_eq!(fixture.d.min_size(), Some((150, 100)));
    assert_close(&fixture.sizes(), &[0.5, 0.5, 0.5, 0.5, 0.01, 0.99]);
  }

  /// `d` -400px wide: 12px of 825px, under its own 150px floor, which is
  /// forgotten. 12 / 825 for `d`, the rest for `c`.
  #[test]
  fn shrinking_below_an_own_inferred_floor_releases_it() {
    let fixture = resize_d_with_inferred_floors(-400);

    assert_eq!(fixture.d.min_size(), Some((0, 100)));
    assert_eq!(fixture.c.min_size(), Some((300, 150)));
    assert_close(
      &fixture.sizes(),
      &[0.5, 0.5, 0.5, 0.5, 0.985_454_56, 0.014_545_455],
    );
  }

  /// A request that fits within the floors has no reason to re-test
  /// them.
  #[test]
  fn a_request_within_inferred_floors_keeps_them() {
    let fixture = resize_d_with_inferred_floors(60);

    assert_eq!(fixture.c.min_size(), Some((300, 150)));
    assert_eq!(fixture.d.min_size(), Some((150, 100)));
    assert_close(&fixture.sizes(), &WIDTH_ONLY);
  }
}
