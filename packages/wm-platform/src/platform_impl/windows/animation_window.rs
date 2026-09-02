use std::sync::Once;

use windows::{
  core::{w, PCWSTR},
  Win32::{
    Foundation::{BOOL, HWND, LPARAM, LRESULT, RECT, WPARAM},
    Graphics::Dwm::{
      DwmRegisterThumbnail, DwmUnregisterThumbnail,
      DwmUpdateThumbnailProperties, DWM_THUMBNAIL_PROPERTIES,
      DWM_TNP_OPACITY, DWM_TNP_RECTDESTINATION,
      DWM_TNP_SOURCECLIENTAREAONLY, DWM_TNP_VISIBLE,
    },
    UI::WindowsAndMessaging::{
      CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW,
      SetWindowPos, HTTRANSPARENT, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
      SWP_SHOWWINDOW, WM_NCHITTEST, WNDCLASSW, WS_EX_NOACTIVATE,
      WS_EX_NOREDIRECTIONBITMAP, WS_EX_TRANSPARENT, WS_POPUP,
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
    let props =
      Self::thumbnail_properties(inner_rect, outer_rect, opacity.as_ref());

    let (handle, thumbnail) = dispatcher.dispatch_sync(|| {
      // Window is spawned on the main thread - avoids having to create a
      // new message loop.
      let handle = Self::create_window(outer_rect)?;

      // SAFETY: Both handles are live windows; the destination is ours.
      let thumbnail =
        unsafe { DwmRegisterThumbnail(HWND(handle), source_hwnd)? };

      // SAFETY: The thumbnail was registered just above.
      unsafe {
        DwmUpdateThumbnailProperties(thumbnail, &raw const props)?;
      }

      // Configured before it is shown, so its first composed frame
      // already carries the window.
      Self::show_beneath(handle, source_hwnd)?;

      Ok::<_, crate::Error>((handle, thumbnail))
    })??;

    Ok(Self {
      handle,
      thumbnail,
      outer_rect: outer_rect.clone(),
      dispatcher: dispatcher.clone(),
    })
  }

  /// Implements [`AnimationWindow::resize`].
  pub(crate) fn resize(&mut self, outer_rect: &Rect) -> crate::Result<()> {
    self.outer_rect = outer_rect.clone();

    // SAFETY: The handle is a window this instance created.
    unsafe {
      SetWindowPos(
        HWND(self.handle),
        None,
        outer_rect.x(),
        outer_rect.y(),
        outer_rect.width(),
        outer_rect.height(),
        SWP_NOACTIVATE,
      )
    }
    .map_err(crate::Error::from)
  }

  /// Implements [`AnimationWindow::update`].
  pub(crate) fn update(
    &self,
    inner_rect: &Rect,
    opacity: Option<&OpacityValue>,
  ) -> crate::Result<()> {
    let props =
      Self::thumbnail_properties(inner_rect, &self.outer_rect, opacity);
    let thumbnail = self.thumbnail;

    self.dispatcher.dispatch_sync(move || {
      // SAFETY: The thumbnail stays registered until `destroy`.
      unsafe { DwmUpdateThumbnailProperties(thumbnail, &raw const props) }
        .map_err(crate::Error::from)
    })?
  }

  /// Implements [`AnimationWindow::destroy`].
  pub(crate) fn destroy(self) -> crate::Result<()> {
    let handle = HWND(self.handle);
    let thumbnail = self.thumbnail;

    self.dispatcher.dispatch_sync(move || {
      // SAFETY: Both were created by this instance and not yet released.
      unsafe {
        if let Err(err) = DwmUnregisterThumbnail(thumbnail) {
          tracing::warn!("Failed to unregister thumbnail: {err}");
        }

        if let Err(err) = DestroyWindow(handle) {
          tracing::warn!("Failed to destroy overlay HWND: {err}");
        }
      }
    })
  }

  /// Where and how opaque DWM draws the thumbnail within the window.
  ///
  /// `inner_rect` is in screen coordinates and lands relative to
  /// `outer_rect`, the window's frame. The source is scaled to fit.
  fn thumbnail_properties(
    inner_rect: &Rect,
    outer_rect: &Rect,
    opacity: Option<&OpacityValue>,
  ) -> DWM_THUMBNAIL_PROPERTIES {
    let mut props = DWM_THUMBNAIL_PROPERTIES {
      dwFlags: DWM_TNP_RECTDESTINATION
        | DWM_TNP_VISIBLE
        | DWM_TNP_SOURCECLIENTAREAONLY,
      rcDestination: RECT {
        left: inner_rect.left - outer_rect.left,
        top: inner_rect.top - outer_rect.top,
        right: inner_rect.right - outer_rect.left,
        bottom: inner_rect.bottom - outer_rect.top,
      },
      rcSource: RECT::default(),
      opacity: u8::MAX,
      fVisible: BOOL(1),
      // The whole frame, title bar included, matching the frame rect the
      // animation is computed from.
      fSourceClientAreaOnly: BOOL(0),
    };

    if let Some(opacity) = opacity {
      props.dwFlags |= DWM_TNP_OPACITY;
      props.opacity = opacity.to_alpha();
    }

    props
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
