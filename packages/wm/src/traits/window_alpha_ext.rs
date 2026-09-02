use wm_platform::{
  Delta, NativeWindowWindowsExt, OpacityValue, WS_EX_LAYERED,
};

use crate::{models::AlphaState, traits::WindowGetters};

/// Alpha writes that remember what the WM changed, so it can be undone.
///
/// Every alpha the WM writes goes through here. The first write records
/// whether the window was already layered, which decides how
/// [`WindowAlphaExt::restore_opacity`] hands the window back.
pub trait WindowAlphaExt: WindowGetters {
  /// Sets the window's alpha.
  fn set_alpha(&self, opacity: OpacityValue) -> anyhow::Result<()>;

  /// Adjusts the window's alpha by a delta.
  fn adjust_alpha(&self, delta: Delta<OpacityValue>)
    -> anyhow::Result<()>;

  /// Undoes every alpha change the WM made to the window.
  ///
  /// Removes the layered style if the WM added it, so the window is
  /// indistinguishable from one the WM never touched. A window its app
  /// made layered keeps the style and only gets full alpha back.
  fn restore_opacity(&self) -> anyhow::Result<()>;
}

impl<T: WindowGetters> WindowAlphaExt for T {
  fn set_alpha(&self, opacity: OpacityValue) -> anyhow::Result<()> {
    claim_alpha(self);
    self.native().set_transparency(&opacity)?;
    Ok(())
  }

  fn adjust_alpha(
    &self,
    delta: Delta<OpacityValue>,
  ) -> anyhow::Result<()> {
    claim_alpha(self);
    self.native().adjust_transparency(&delta)?;
    Ok(())
  }

  fn restore_opacity(&self) -> anyhow::Result<()> {
    match self.native_properties().alpha_state {
      AlphaState::Untouched => return Ok(()),
      AlphaState::Written => self
        .native()
        .set_transparency(&OpacityValue::from_alpha(u8::MAX))?,
      AlphaState::Layered => {
        self.native().remove_window_style_ex(WS_EX_LAYERED);
      }
    }

    self.update_native_properties(|props| {
      props.alpha_state = AlphaState::Untouched;
    });

    Ok(())
  }
}

/// Records the first alpha write, and whether it had to add the layered
/// style to make it.
fn claim_alpha<T: WindowGetters>(window: &T) {
  if window.native_properties().alpha_state != AlphaState::Untouched {
    return;
  }

  let state = if window.native().has_window_style_ex(WS_EX_LAYERED) {
    AlphaState::Written
  } else {
    AlphaState::Layered
  };

  window.update_native_properties(|props| props.alpha_state = state);
}
