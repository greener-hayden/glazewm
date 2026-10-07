use wm_platform::LengthValue;

use super::{
  set_window_size::{ResizeContext, ResizeContexts},
  set_window_size_with_contexts,
};
use crate::{
  models::{TilingWindow, WindowContainer},
  traits::{CommonGetters, PositionGetters, TilingSizeGetters},
  wm_state::WmState,
};

pub fn resize_window(
  window: &WindowContainer,
  width_delta: Option<LengthValue>,
  height_delta: Option<LengthValue>,
  state: &mut WmState,
) -> anyhow::Result<()> {
  let window_rect = window.to_rect()?;
  let tiling_window = window.as_tiling_window();

  // Each axis resolves its parent once, here, and hands it on so that
  // `set_window_size` does not solve the same layout again.
  let mut contexts = ResizeContexts::default();

  let target_width = match width_delta {
    Some(delta) => {
      let parent_width = match tiling_window {
        Some(tiling_window) => {
          contexts.width = resolve_context(tiling_window, true)?;
          contexts.width.as_ref().and_then(|context| {
            let (horizontal_gap, _) = tiling_window.inner_gaps().ok()?;

            #[allow(
              clippy::cast_possible_wrap,
              clippy::cast_possible_truncation
            )]
            Some(
              context.parent_rect.width()
                - horizontal_gap * context.sibling_count as i32,
            )
          })
        }
        None => window.parent().and_then(|parent| {
          parent.to_rect().ok().map(|rect| rect.width())
        }),
      };

      parent_width.map(|parent_width| {
        window_rect.width() + delta.to_px(parent_width, None)
      })
    }
    _ => None,
  };

  let target_height = match height_delta {
    Some(delta) => {
      let parent_height = match tiling_window {
        Some(tiling_window) => {
          contexts.height = resolve_context(tiling_window, false)?;
          contexts.height.as_ref().and_then(|context| {
            let (_, vertical_gap) = tiling_window.inner_gaps().ok()?;

            #[allow(
              clippy::cast_possible_wrap,
              clippy::cast_possible_truncation
            )]
            Some(
              context.parent_rect.height()
                - vertical_gap * context.sibling_count as i32,
            )
          })
        }
        None => window.parent().and_then(|parent| {
          parent.to_rect().ok().map(|rect| rect.height())
        }),
      };

      parent_height.map(|parent_height| {
        window_rect.height() + delta.to_px(parent_height, None)
      })
    }
    _ => None,
  };

  set_window_size_with_contexts(
    window.clone(),
    target_width.map(LengthValue::from_px),
    target_height.map(LengthValue::from_px),
    contexts,
    state,
  )?;

  Ok(())
}

/// Resolves what a resize of a tiling window starts from along an axis.
///
/// An error finding the container to resize is returned. A container
/// whose parent has no layout is not one: the resize has no target then,
/// and is skipped.
fn resolve_context(
  window: &TilingWindow,
  is_width_resize: bool,
) -> anyhow::Result<Option<ResizeContext>> {
  Ok(
    window
      .container_to_resize(is_width_resize)?
      .and_then(|container| {
        ResizeContext::from_container(window, container).ok()
      }),
  )
}
