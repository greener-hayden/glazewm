#[cfg(target_os = "macos")]
use std::cell::RefCell;

#[cfg(target_os = "macos")]
use objc2_core_foundation::{
  CFBoolean, CFDictionary, CFString, CFType, CGRect,
};
#[cfg(target_os = "macos")]
use objc2_core_graphics::{
  kCGWindowBounds, CGRectMakeWithDictionaryRepresentation,
  CGWindowListCopyWindowInfo, CGWindowListOption,
};

#[cfg(target_os = "macos")]
use crate::platform_impl::AXUIElementExt;
use crate::{
  Color, CornerStyle, NativeWindow, OpacityValue, Rect, WindowId,
  WindowZOrder,
};
#[cfg(target_os = "windows")]
use crate::{NativeSession, NativeWindowWindowsExt};

/// How the real window is hidden while an overlay presents it.
///
/// Eligibility for presentation is independent of the method; the method
/// only decides which native mutation conceals the source.
///
/// # Platform-specific
///
/// - Windows: `Alpha` for windows with a redirection surface, `Cloak` for
///   DirectComposition or layered windows that reject attribute alpha.
/// - macOS: always `Park`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConcealMethod {
  /// Attribute alpha is set to zero on the source.
  Alpha,
  /// The source is cloaked by the shell while staying composed.
  Cloak,
  /// The source is moved to a corner sliver of the working area.
  Park,
}

/// Owns recoverable native placement and source visibility mutations.
///
/// Overlay lifetime and animation clocks belong to the presentation
/// coordinator; this adapter only implements native operations.
pub struct PlacementSession {
  #[cfg(target_os = "windows")]
  native: NativeSession,
  #[cfg(target_os = "macos")]
  native: NativeWindow,
  #[cfg(target_os = "macos")]
  recovery_frame: RefCell<Option<Rect>>,
}

impl PlacementSession {
  /// Whether the event listener already concealed this source.
  #[must_use]
  pub fn opening_concealed(&self) -> bool {
    #[cfg(target_os = "windows")]
    {
      self.native.opening_concealed()
    }
    #[cfg(target_os = "macos")]
    {
      false
    }
  }

  /// Claims the native window and its recovery ownership.
  pub fn new(window: NativeWindow, token: isize) -> crate::Result<Self> {
    #[cfg(target_os = "windows")]
    {
      Ok(Self {
        native: NativeSession::new(window, token)?,
      })
    }
    #[cfg(target_os = "macos")]
    {
      if token == 0 || !window.is_valid() {
        return Err(crate::Error::WindowNotFound);
      }
      Ok(Self {
        native: window,
        recovery_frame: RefCell::new(None),
      })
    }
  }

  /// Checks that this session still owns a live native window.
  pub fn validate(&self) -> crate::Result<()> {
    #[cfg(target_os = "windows")]
    {
      self.native.validate()
    }
    #[cfg(target_os = "macos")]
    {
      if self.native.is_valid() {
        Ok(())
      } else {
        Err(crate::Error::WindowNotFound)
      }
    }
  }

  /// Returns the native window after validating ownership.
  pub fn window(&self) -> crate::Result<&NativeWindow> {
    #[cfg(target_os = "windows")]
    {
      self.native.window()
    }
    #[cfg(target_os = "macos")]
    {
      self.validate()?;
      Ok(&self.native)
    }
  }

  /// Observes the native rectangle in placement coordinates.
  pub fn observed_frame(&self) -> crate::Result<Rect> {
    #[cfg(target_os = "windows")]
    {
      self.window()?.frame_with_shadows()
    }
    #[cfg(target_os = "macos")]
    {
      let window_id = self.window()?.id();
      let windows = CGWindowListCopyWindowInfo(
        CGWindowListOption::OptionIncludingWindow,
        window_id.0,
      )
      .ok_or(crate::Error::WindowNotFound)?;
      // SAFETY: Window services returns dictionaries with string keys
      // and Core Foundation values, retaining all of those objects.
      let windows = unsafe {
        windows.cast_unchecked::<CFDictionary<CFString, CFType>>()
      };
      let info = windows.get(0).ok_or(crate::Error::WindowNotFound)?;
      // SAFETY: The framework provides this immutable dictionary key.
      let bounds =
        info.get(unsafe { kCGWindowBounds }).ok_or_else(|| {
          crate::Error::Platform("Window server omitted bounds.".into())
        })?;
      let dictionary =
        bounds.downcast_ref::<CFDictionary>().ok_or_else(|| {
          crate::Error::Platform("Invalid window server bounds.".into())
        })?;
      let mut frame = CGRect::ZERO;
      // SAFETY: The checked dictionary is retained and frame is a live
      // output rectangle for the duration of this synchronous call.
      if !unsafe {
        CGRectMakeWithDictionaryRepresentation(
          Some(dictionary),
          &raw mut frame,
        )
      } {
        return Err(crate::Error::Platform(
          "Invalid window server rectangle.".into(),
        ));
      }
      Ok(frame.into())
    }
  }

  /// Observes native visibility, including Windows compositor cloaking.
  pub fn is_visible(&self) -> crate::Result<bool> {
    self.window()?.is_visible()
  }

  /// Reads the window's current placement coordinate scale.
  pub fn dpi(&self) -> crate::Result<u32> {
    #[cfg(target_os = "windows")]
    {
      self.native.dpi()
    }
    #[cfg(target_os = "macos")]
    {
      self.validate()?;
      Ok(1)
    }
  }

  /// Resolves the expected scale at the destination display.
  pub fn expected_dpi(&self, monitor_dpi: u32) -> crate::Result<u32> {
    #[cfg(target_os = "windows")]
    {
      self.native.expected_dpi(monitor_dpi)
    }
    #[cfg(target_os = "macos")]
    {
      let _ = monitor_dpi;
      self.dpi()
    }
  }

  /// Reports whether the native source can safely yield presentation.
  ///
  /// # Platform-specific
  ///
  /// - Windows: every session; a source that rejects alpha is cloaked.
  /// - macOS: only while the window is not maximized.
  #[must_use]
  pub fn supports_presentation(&self) -> bool {
    #[cfg(target_os = "windows")]
    {
      true
    }
    #[cfg(target_os = "macos")]
    {
      self.native.is_maximized().is_ok_and(|maximized| !maximized)
    }
  }

  /// Reports how this source is concealed behind its overlay.
  #[must_use]
  pub fn conceal_method(&self) -> ConcealMethod {
    #[cfg(target_os = "windows")]
    {
      self.native.conceal_method()
    }
    #[cfg(target_os = "macos")]
    {
      ConcealMethod::Park
    }
  }

  /// Reports whether opacity can change without disturbing app rendering.
  #[must_use]
  pub fn supports_opacity(&self) -> bool {
    #[cfg(target_os = "windows")]
    {
      self.native.supports_alpha()
    }
    #[cfg(target_os = "macos")]
    {
      false
    }
  }

  /// Pixels the platform may pull a parked window's top edge back toward
  /// its display, for a window `height` pixels tall.
  ///
  /// # Platform-specific
  ///
  /// - Windows: `0`; parked frames are placed exactly.
  /// - macOS: up to the window's own height. The window server keeps a
  ///   title bar inside the working area, so a window parked below it
  ///   stops short by that bar's height. The app decides that height (27px
  ///   for kitty, 63px for Spotify's toolbar), and it can only be bounded
  ///   by the window itself. The parked column is 1px wide at any height,
  ///   so the lift never exposes the window.
  #[must_use]
  pub const fn parking_clamp(height: i32) -> i32 {
    if cfg!(target_os = "macos") {
      height
    } else {
      0
    }
  }

  /// Reports whether source suppression requires temporary placement.
  #[must_use]
  pub fn uses_parking(&self) -> bool {
    self.conceal_method() == ConcealMethod::Park
  }

  /// Resolves native placement while an overlay owns source visibility.
  #[must_use]
  pub fn suppression_target(&self, target: &Rect, corner: &Rect) -> Rect {
    if self.uses_parking() {
      corner.clone()
    } else {
      target.clone()
    }
  }

  /// Applies recoverable opacity where native attribute alpha is safe.
  pub fn opacity(&self, value: Option<OpacityValue>) -> crate::Result<()> {
    #[cfg(target_os = "windows")]
    {
      self.native.opacity(value)
    }
    #[cfg(target_os = "macos")]
    {
      let _ = value;
      self.validate()
    }
  }

  /// Applies recoverable title-bar visibility where supported.
  pub fn title_bar(&self, visible: Option<bool>) -> crate::Result<()> {
    #[cfg(target_os = "windows")]
    {
      self.native.title_bar(visible)
    }
    #[cfg(target_os = "macos")]
    {
      let _ = visible;
      self.validate()
    }
  }

  /// Applies platform-supported border and corner decoration.
  pub fn set_decorations(
    &self,
    border: Option<&Color>,
    corner: &CornerStyle,
  ) -> crate::Result<()> {
    #[cfg(target_os = "windows")]
    {
      let window = self.window()?;
      window.set_border_color(border)?;
      window.set_corner_style(corner)
    }
    #[cfg(target_os = "macos")]
    {
      let _ = (border, corner);
      self.validate()
    }
  }

  /// Changes taskbar membership where the platform supports it.
  pub fn set_taskbar_visibility(
    &self,
    visible: bool,
  ) -> crate::Result<()> {
    #[cfg(target_os = "windows")]
    {
      self.window()?.set_taskbar_visibility(visible)
    }
    #[cfg(target_os = "macos")]
    {
      let _ = visible;
      self.validate()
    }
  }

  /// Marks taskbar fullscreen ownership where supported.
  pub fn mark_fullscreen(&self, fullscreen: bool) -> crate::Result<()> {
    #[cfg(target_os = "windows")]
    {
      self.window()?.mark_fullscreen(fullscreen)
    }
    #[cfg(target_os = "macos")]
    {
      let _ = fullscreen;
      self.validate()
    }
  }

  /// Applies native z-order where the platform exposes it.
  pub fn set_z_order(&self, order: &WindowZOrder) -> crate::Result<()> {
    #[cfg(target_os = "windows")]
    {
      self.window()?.set_z_order(order)
    }
    #[cfg(target_os = "macos")]
    {
      let _ = order;
      self.validate()
    }
  }

  /// Observes whether native z-order already satisfies `order`.
  ///
  /// `in_front` lists the windows that a `WindowZOrder::Bottom` window
  /// must sit behind.
  ///
  /// # Platform-specific
  ///
  /// - Windows: reads the always-on-top flag and the stacking order.
  /// - macOS: always satisfied; the platform exposes no z-order.
  pub fn has_z_order(
    &self,
    order: &WindowZOrder,
    in_front: &[WindowId],
  ) -> crate::Result<bool> {
    #[cfg(target_os = "windows")]
    {
      Ok(self.window()?.has_z_order(order, in_front))
    }
    #[cfg(target_os = "macos")]
    {
      let _ = (order, in_front);
      self.validate()?;
      Ok(true)
    }
  }

  /// Changes native visibility where supported; macOS uses parking.
  pub fn show(&self, visible: bool) -> crate::Result<()> {
    #[cfg(target_os = "windows")]
    {
      self.native.show(visible)
    }
    #[cfg(target_os = "macos")]
    {
      let _ = visible;
      self.validate()
    }
  }

  /// Publishes whether an overlay currently stands in for the source.
  ///
  /// # Platform-specific
  ///
  /// - Windows: sets a window property that border renderers read.
  /// - macOS: no-op; parking already takes the window off screen.
  pub fn present(&self, presenting: bool) -> crate::Result<()> {
    #[cfg(target_os = "windows")]
    {
      self.native.present(presenting)
    }
    #[cfg(target_os = "macos")]
    {
      let _ = presenting;
      self.validate()
    }
  }

  /// Publishes whether the WM draws the window's companions itself
  /// during native motion.
  ///
  /// # Platform-specific
  ///
  /// - Windows: sets a window property that border renderers read.
  /// - macOS: no-op; the platform has no companions.
  pub fn mark_decorated(&self, decorated: bool) -> crate::Result<()> {
    #[cfg(target_os = "windows")]
    {
      self.native.mark_decorated(decorated)
    }
    #[cfg(target_os = "macos")]
    {
      let _ = decorated;
      self.validate()
    }
  }

  /// Publishes whether the window holds the WM's focus.
  ///
  /// # Platform-specific
  ///
  /// - Windows: sets a window property that border renderers read.
  /// - macOS: no-op; the renderer reads focus from the front window.
  pub fn mark_focused(&self, focused: bool) -> crate::Result<()> {
    #[cfg(target_os = "windows")]
    {
      self.native.mark_focused(focused)
    }
    #[cfg(target_os = "macos")]
    {
      let _ = focused;
      self.validate()
    }
  }

  /// Observes native cloaking, including cloaks we did not apply.
  pub fn is_cloaked(&self) -> crate::Result<bool> {
    #[cfg(target_os = "windows")]
    {
      self.native.is_cloaked()
    }
    #[cfg(target_os = "macos")]
    {
      self.validate()?;
      Ok(false)
    }
  }

  /// Changes workspace visibility using the native hide capability.
  pub fn cloak(&self, hidden: bool) -> crate::Result<()> {
    #[cfg(target_os = "windows")]
    {
      self.native.cloak(hidden)
    }
    #[cfg(target_os = "macos")]
    {
      let _ = hidden;
      self.validate()
    }
  }

  /// Records a recovery destination without issuing a placement request.
  pub fn remember_restore(&self, target: &Rect) -> crate::Result<()> {
    #[cfg(target_os = "windows")]
    {
      self.native.remember_restore(target)
    }
    #[cfg(target_os = "macos")]
    {
      self.validate()?;
      *self.recovery_frame.borrow_mut() = Some(target.clone());
      Ok(())
    }
  }

  /// Records the recovery destination before parking a source window.
  pub fn park(&self, restore: &Rect, corner: &Rect) -> crate::Result<()> {
    #[cfg(target_os = "windows")]
    {
      self.native.park(restore, corner)
    }
    #[cfg(target_os = "macos")]
    {
      self.remember_restore(restore)?;
      self.native.set_frame(corner)
    }
  }

  /// Releases parking recovery ownership after confirmed restoration.
  pub fn unpark(&self) -> crate::Result<()> {
    #[cfg(target_os = "windows")]
    {
      self.native.unpark()
    }
    #[cfg(target_os = "macos")]
    {
      self.validate()?;
      self.recovery_frame.borrow_mut().take();
      Ok(())
    }
  }

  /// Restores owned native changes before releasing this session.
  ///
  /// A pending restoration retains recovery ownership for another
  /// observation; callers must retain the session when this fails.
  pub fn release(&self) -> crate::Result<()> {
    #[cfg(target_os = "windows")]
    {
      self.native.release()
    }
    #[cfg(target_os = "macos")]
    {
      self.validate()?;
      let target = self.recovery_frame.borrow().clone();
      if let Some(target) = target {
        if self.observed_frame()? != target {
          self.native.set_frame(&target)?;
          return Err(crate::Error::Platform(
            "Window recovery pending.".into(),
          ));
        }
        self.recovery_frame.borrow_mut().take();
      }
      self.show(true)
    }
  }

  /// Queries native minimum-size evidence when available.
  pub fn minimum_size(&self) -> crate::Result<Option<(i32, i32)>> {
    #[cfg(target_os = "windows")]
    {
      self.native.minimum_size()
    }
    #[cfg(target_os = "macos")]
    {
      self.validate()?;
      Ok(None)
    }
  }

  /// Restores normal native state and requests the destination frame.
  pub fn restore(&self, rect: &Rect) -> crate::Result<()> {
    #[cfg(target_os = "windows")]
    {
      self.window()?.restore(Some(rect))
    }
    #[cfg(target_os = "macos")]
    {
      self.validate()?;
      if self.native.is_minimized()? {
        self.set_boolean("AXMinimized", false)?;
      }
      if self.native.is_maximized()? {
        self.set_boolean("AXFullScreen", false)?;
      }
      self.native.set_frame(rect)
    }
  }

  /// Requests native minimization after validating the window.
  pub fn minimize(&self) -> crate::Result<()> {
    self.window()?.minimize()
  }

  /// Requests native maximization after validating the window.
  pub fn maximize(&self) -> crate::Result<()> {
    self.window()?.maximize()
  }

  /// Requests a native frame without changing presentation ownership.
  pub fn set_frame(&self, rect: &Rect) -> crate::Result<()> {
    self.window()?.set_frame(rect)
  }

  /// Changes an accessibility boolean on the owning application thread.
  #[cfg(target_os = "macos")]
  fn set_boolean(
    &self,
    attribute: &'static str,
    value: bool,
  ) -> crate::Result<()> {
    self.window()?.inner.with_element(move |element| {
      element.set_attribute::<CFBoolean>(
        attribute,
        &CFBoolean::new(value).into(),
      )
    })?
  }
}
