use wm_platform::{LengthValue, Rect};

use super::{set_window_size, set_window_size::grow_tiling_window};
use crate::{
  models::WindowContainer,
  traits::{CommonGetters, PositionGetters},
  wm_state::WmState,
};

/// Resizes a window by a change in width, in height, or in both.
///
/// A tiling window moves a boundary (see `grow_tiling_window`), and a
/// percentage is of the length that the tiles of its row share out. For
/// any other window a percentage is of the length of its parent.
pub fn resize_window(
  window: &WindowContainer,
  width_delta: Option<LengthValue>,
  height_delta: Option<LengthValue>,
  state: &mut WmState,
) -> anyhow::Result<()> {
  match window {
    WindowContainer::TilingWindow(tiling_window) => {
      for (delta, is_width) in [(width_delta, true), (height_delta, false)]
      {
        if let Some(delta) = delta {
          grow_tiling_window(
            tiling_window,
            is_width,
            |row_length| delta.to_px(row_length, None),
            state,
          )?;
        }
      }

      Ok(())
    }
    WindowContainer::NonTilingWindow(_) => {
      let rect = window.to_rect()?;
      let parent_rect =
        window.parent().and_then(|parent| parent.to_rect().ok());

      let target = |delta: Option<LengthValue>,
                    length: fn(&Rect) -> i32| {
        let parent_length = length(parent_rect.as_ref()?);
        Some(LengthValue::from_px(
          length(&rect) + delta?.to_px(parent_length, None),
        ))
      };

      set_window_size(
        window.clone(),
        target(width_delta, Rect::width),
        target(height_delta, Rect::height),
        state,
      )
    }
  }
}
