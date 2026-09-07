use std::sync::Once;

use windows::{
  core::{w, PCWSTR},
  Win32::{
    Foundation::{BOOL, HWND, LPARAM, LRESULT, RECT, WPARAM},
    Graphics::Dwm::{
      DwmQueryThumbnailSourceSize, DwmRegisterThumbnail,
      DwmUnregisterThumbnail, DwmUpdateThumbnailProperties,
      DWM_THUMBNAIL_PROPERTIES, DWM_TNP_OPACITY, DWM_TNP_RECTDESTINATION,
      DWM_TNP_RECTSOURCE, DWM_TNP_SOURCECLIENTAREAONLY, DWM_TNP_VISIBLE,
    },
    UI::WindowsAndMessaging::{
      CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW,
      SetWindowPos, HTTRANSPARENT, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
      SWP_NOZORDER, SWP_SHOWWINDOW, WM_NCHITTEST, WNDCLASSW,
      WS_EX_NOACTIVATE, WS_EX_NOREDIRECTIONBITMAP, WS_EX_TRANSPARENT,
      WS_POPUP,
    },
  },
};

use crate::{Dispatcher, NativeWindow, OpacityValue, Rect, WindowId};

/// Platform-specific implementation of [`AnimationContext`].
///
/// Holds nothing on Windows. DWM paints the overlay from the source
/// window's own surface, so there is no device to share and nothing to
/// commit.
pub(crate) struct AnimationContext;

impl AnimationContext {
  /// Implements [`AnimationContext::new`].
  #[allow(clippy::unnecessary_wraps)]
  pub(crate) fn new(_dispatcher: &Dispatcher) -> crate::Result<Self> {
    Ok(Self)
  }

  /// Implements [`AnimationContext::capture_frame`].
  ///
  /// Nothing is captured. The overlay shows the window live, so this
  /// returns at once from any thread.
  #[allow(clippy::unnecessary_wraps, clippy::unused_self)]
  pub(crate) fn capture_frame(
    &self,
    _window_id: WindowId,
  ) -> crate::Result<AnimationCapture> {
    Ok(AnimationCapture)
  }

  /// Implements [`AnimationContext::transaction`].
  ///
  /// Thumbnail updates issued within one frame are composed together by
  /// DWM, so a transaction is only the hop to the main thread.
  #[allow(clippy::unused_self)]
  pub(crate) fn transaction<F, R>(
    &self,
    update_fn: F,
    dispatcher: &Dispatcher,
  ) -> crate::Result<R>
  where
    F: FnOnce() -> R + Send,
    R: Send,
  {
    dispatcher.dispatch_sync(update_fn)
  }
}

/// Platform-specific implementation of [`AnimationWindow`].
///
/// A popup that DWM paints a live thumbnail of the source window into.
/// A thumbnail is drawn from the source's own surface, so it keeps
/// rendering while the source is transparent or cloaked, costs no
/// capture, and shows the window exactly as it looks when handed back.
/// The screenshot engine this replaces stalled every animated sync by
/// ~60ms of capture and then popped from a stretched still to the real
/// window at the end; the thumbnail does neither.
pub(crate) struct AnimationWindow {
  handle: isize,
  thumbnail: isize,
  /// The source's invisible frame insets, measured once at registration.
  source_origin: (i32, i32),
  /// Frame of the `AnimationWindow`.
  outer_rect: Rect,
  dispatcher: Dispatcher,
}

impl AnimationWindow {
  /// Implements [`AnimationWindow::new`].
  pub(crate) fn new(
    _context: &AnimationContext,
    window: &NativeWindow,
    _capture: AnimationCapture,
    inner_rect: &Rect,
    outer_rect: &Rect,
    opacity: Option<OpacityValue>,
    dispatcher: &Dispatcher,
  ) -> crate::Result<Self> {
    let source_hwnd = window.inner.hwnd();
    // Measured here, on the caller's thread and once per overlay. See
    // `source_rect`.
    let source_origin = Self::source_origin(window)?;
    let (handle, thumbnail) = dispatcher.dispatch_sync(move || {
      let handle = Self::create_window(outer_rect)?;
      // SAFETY: Source and destination are live windows.
      let thumbnail =
        match unsafe { DwmRegisterThumbnail(HWND(handle), source_hwnd) } {
          Ok(thumbnail) => thumbnail,
          Err(err) => {
            // SAFETY: Rolls back our new window.
            if let Err(cleanup) = unsafe { DestroyWindow(HWND(handle)) } {
              tracing::warn!("Overlay rollback failed: {cleanup}");
            }
            return Err(crate::Error::from(err));
          }
        };
      let prepare = || -> crate::Result<()> {
        let source_rect = Self::source_rect(source_origin, thumbnail)?;
        let props = Self::thumbnail_properties(
          inner_rect,
          outer_rect,
          &source_rect,
          opacity.as_ref(),
        );
        // SAFETY: Properties target our registered thumbnail.
        unsafe {
          DwmUpdateThumbnailProperties(thumbnail, &raw const props)?;
        }
        tracing::debug!(
          "Overlay source {source_rect:?} visible {} dest {:?}",
          props.fVisible.0 != 0,
          [
            props.rcDestination.left,
            props.rcDestination.top,
            props.rcDestination.right,
            props.rcDestination.bottom
          ]
        );
        Self::show_beneath(handle, source_hwnd)?;
        Ok(())
      };
      match prepare() {
        Ok(()) => Ok((handle, thumbnail)),
        Err(err) => {
          // SAFETY: Rolls back both owned resources.
          unsafe {
            if let Err(cleanup) = DwmUnregisterThumbnail(thumbnail) {
              tracing::warn!("Thumbnail rollback failed: {cleanup}");
            }
            if let Err(cleanup) = DestroyWindow(HWND(handle)) {
              tracing::warn!("Overlay rollback failed: {cleanup}");
            }
          }
          Err(err)
        }
      }
    })??;

    Ok(Self {
      handle,
      thumbnail,
      source_origin,
      outer_rect: outer_rect.clone(),
      dispatcher: dispatcher.clone(),
    })
  }

  /// Implements [`AnimationWindow::resize`].
  pub(crate) fn resize(&mut self, outer_rect: &Rect) -> crate::Result<()> {
    let handle = self.handle;
    self.dispatcher.dispatch_sync(|| {
      // SAFETY: Resizes our window without restacking.
      unsafe {
        SetWindowPos(
          HWND(handle),
          None,
          outer_rect.x(),
          outer_rect.y(),
          outer_rect.width(),
          outer_rect.height(),
          SWP_NOACTIVATE | SWP_NOZORDER,
        )
      }
    })??;
    self.outer_rect = outer_rect.clone();
    Ok(())
  }

  /// Implements [`AnimationWindow::update`].
  pub(crate) fn update(
    &self,
    inner_rect: &Rect,
    opacity: Option<&OpacityValue>,
  ) -> crate::Result<()> {
    let thumbnail = self.thumbnail;
    if opacity.is_some() {
      tracing::debug!(
        "Overlay frame {inner_rect:?} at {:?}",
        opacity.map(OpacityValue::to_alpha)
      );
    }
    self.dispatcher.dispatch_sync(|| {
      let source_rect = Self::source_rect(self.source_origin, thumbnail)?;
      let props = Self::thumbnail_properties(
        inner_rect,
        &self.outer_rect,
        &source_rect,
        opacity,
      );
      // SAFETY: Updates our registered thumbnail.
      unsafe { DwmUpdateThumbnailProperties(thumbnail, &raw const props) }
        .map_err(crate::Error::from)
    })?
  }

  /// Implements [`AnimationWindow::destroy`].
  pub(crate) fn destroy(&mut self) -> crate::Result<()> {
    if self.thumbnail != 0 {
      let thumbnail = self.thumbnail;
      self.dispatcher.dispatch_sync(move || {
        // SAFETY: Unregisters our still-owned thumbnail.
        unsafe { DwmUnregisterThumbnail(thumbnail) }
      })??;
      self.thumbnail = 0;
    }
    if self.handle != 0 {
      let handle = HWND(self.handle);
      self.dispatcher.dispatch_sync(move || {
        // SAFETY: Destroys our still-owned window.
        unsafe { DestroyWindow(handle) }
      })??;
      self.handle = 0;
    }
    Ok(())
  }

  /// Reads the source's invisible frame insets, in source coordinates.
  ///
  /// Called once per overlay, off the event loop thread. `shadow_borders`
  /// costs two `GetWindowRect` calls and a `DwmGetWindowAttribute`, and
  /// the last is a round-trip to the compositor.
  fn source_origin(window: &NativeWindow) -> crate::Result<(i32, i32)> {
    let borders = window.inner.shadow_borders()?;
    Ok((borders.left.to_px(0, None), borders.top.to_px(0, None)))
  }

  /// Maps thumbnail dimensions using cached frame insets.
  fn source_rect(
    origin: (i32, i32),
    thumbnail: isize,
  ) -> crate::Result<Rect> {
    // SAFETY: Reads our registered thumbnail's dimensions.
    let size = unsafe { DwmQueryThumbnailSourceSize(thumbnail)? };
    Ok(Rect::from_xy(origin.0, origin.1, size.cx, size.cy))
  }

  /// Where and how opaque DWM draws the thumbnail within the window.
  ///
  /// `inner_rect` is in screen coordinates and lands relative to
  /// `outer_rect`, the window's frame. The source is scaled to fit.
  fn thumbnail_properties(
    inner_rect: &Rect,
    outer_rect: &Rect,
    source_rect: &Rect,
    opacity: Option<&OpacityValue>,
  ) -> DWM_THUMBNAIL_PROPERTIES {
    let clipped = crate::thumbnail_rects(
      inner_rect,
      outer_rect,
      (source_rect.width(), source_rect.height()),
    )
    .map(|(destination, source)| {
      let source = source.translate_to_coordinates(
        source.x() + source_rect.x(),
        source.y() + source_rect.y(),
      );
      (destination, source)
    });
    let visible = clipped.is_some();
    let (destination, source) = clipped.unwrap_or_else(|| {
      (Rect::from_xy(0, 0, 0, 0), Rect::from_xy(0, 0, 0, 0))
    });
    DWM_THUMBNAIL_PROPERTIES {
      dwFlags: DWM_TNP_RECTDESTINATION
        | DWM_TNP_RECTSOURCE
        | DWM_TNP_OPACITY
        | DWM_TNP_VISIBLE
        | DWM_TNP_SOURCECLIENTAREAONLY,
      rcDestination: RECT {
        left: destination.left,
        top: destination.top,
        right: destination.right,
        bottom: destination.bottom,
      },
      rcSource: RECT {
        left: source.left,
        top: source.top,
        right: source.right,
        bottom: source.bottom,
      },
      opacity: opacity.map_or(u8::MAX, OpacityValue::to_alpha),
      fVisible: BOOL(i32::from(visible)),
      fSourceClientAreaOnly: BOOL(0),
    }
  }

  /// Creates the window, hidden, at `rect`.
  fn create_window(rect: &Rect) -> crate::Result<isize> {
    const CLASS_NAME: PCWSTR = w!("AnimationWindow");

    static CLASS_REGISTERED: Once = Once::new();
    CLASS_REGISTERED.call_once(|| {
      let wnd_class = WNDCLASSW {
        lpszClassName: CLASS_NAME,
        lpfnWndProc: Some(AnimationWindow::overlay_wnd_proc),
        ..Default::default()
      };
      // SAFETY: The class struct outlives the call.
      unsafe { RegisterClassW(&raw const wnd_class) };
    });

    // SAFETY: Plain window creation with a registered class.
    let hwnd = unsafe {
      CreateWindowExW(
        WS_EX_NOREDIRECTIONBITMAP | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
        CLASS_NAME,
        w!(""),
        WS_POPUP,
        rect.x(),
        rect.y(),
        rect.width(),
        rect.height(),
        None,
        None,
        None,
        None,
      )
    };

    if hwnd.0 == 0 {
      return Err(crate::Error::Platform(
        "Failed to create animation window.".to_string(),
      ));
    }

    Ok(hwnd.0)
  }

  /// Shows the window directly beneath `source_hwnd` in the z-order.
  ///
  /// Two constraints, both deliberate. The overlay sits at the source's
  /// own depth, never `HWND_TOPMOST`, and it is torn down shortly after
  /// its animation ends (see `AnimationManager::destroy_animation`). A
  /// topmost, long-lived, click-through popup over a game is the shape of
  /// a cheat overlay, and anti-cheat heuristics look for exactly that. A
  /// brief one at the source's own depth is not.
  ///
  /// Beneath rather than above: the source is transparent while the
  /// overlay runs, and the moment it is opaque again it is meant to be
  /// what is seen.
  fn show_beneath(handle: isize, source_hwnd: HWND) -> crate::Result<()> {
    // SAFETY: Both handles are live windows.
    unsafe {
      SetWindowPos(
        HWND(handle),
        source_hwnd,
        0,
        0,
        0,
        0,
        SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
      )?;
    }

    Ok(())
  }

  /// Window procedure for the overlay class.
  unsafe extern "system" fn overlay_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
  ) -> LRESULT {
    // Route all mouse inputs to the window below.
    if msg == WM_NCHITTEST {
      LRESULT(HTTRANSPARENT as isize)
    } else {
      DefWindowProcW(hwnd, msg, wparam, lparam)
    }
  }
}

/// A token standing in for a capture; the overlay draws the window live.
pub(crate) struct AnimationCapture;

impl Drop for AnimationWindow {
  /// Releases resources on every exit path.
  fn drop(&mut self) {
    if let Err(err) = self.destroy() {
      tracing::warn!("Overlay cleanup failed: {err}");
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Preserves the entire visible source frame.
  #[test]
  fn thumbnail_preserves_frame_origin() {
    let properties = AnimationWindow::thumbnail_properties(
      &Rect::from_xy(351, 64, 186, 173),
      &Rect::from_xy(55, 48, 498, 205),
      &Rect::from_xy(7, 0, 186, 173),
      None,
    );
    let source = properties.rcSource;
    let destination = properties.rcDestination;
    let client_only = properties.fSourceClientAreaOnly;
    assert_eq!(source.left, 7);
    assert_eq!(source.right, 193);
    assert_eq!(source.bottom, 173);
    assert_eq!(destination.left, 296);
    assert_eq!(destination.right, 482);
    assert_eq!(client_only, BOOL(0));
  }

  /// Crops scaled content without losing source insets.
  #[test]
  fn thumbnail_clips_offset_source() {
    let properties = AnimationWindow::thumbnail_properties(
      &Rect::from_xy(-100, -50, 200, 100),
      &Rect::from_xy(0, 0, 500, 500),
      &Rect::from_xy(7, 2, 400, 200),
      Some(&OpacityValue(0.5)),
    );
    let source = properties.rcSource;
    let destination = properties.rcDestination;
    let visible = properties.fVisible;
    assert_eq!(source.left, 207);
    assert_eq!(source.top, 102);
    assert_eq!(source.right, 407);
    assert_eq!(source.bottom, 202);
    assert_eq!(destination.left, 0);
    assert_eq!(destination.top, 0);
    assert_eq!(destination.right, 100);
    assert_eq!(destination.bottom, 50);
    assert_eq!(properties.opacity, OpacityValue(0.5).to_alpha());
    assert_eq!(visible, BOOL(1));
  }
}
