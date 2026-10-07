use std::{cell::RefCell, collections::HashSet, sync::Arc};

use objc2::MainThreadMarker;
use objc2_app_kit::{
  NSApplicationActivationOptions, NSWindow, NSWorkspace,
};
use objc2_application_services::{AXError, AXValue};
use objc2_core_foundation::{
  CFBoolean, CFNumber, CFRetained, CFString, CFType, CGPoint, CGSize,
};
#[allow(deprecated)]
use objc2_core_graphics::{
  CGDisplayIsAsleep, CGError, CGWindowListCopyWindowInfo,
  CGWindowListCreate, CGWindowListOption,
};

use crate::{
  platform_impl::{
    self, ffi, is_stall, AXUIElement, AXUIElementExt, AXValueExt,
    AXValueTypeMarker, Application,
  },
  Dispatcher, NativeCall, NativeCallStats, Point, Rect, ThreadBound,
  WindowFrameState, WindowId,
};

/// Platform-specific implementation of [`NativeWindow`].
#[derive(Clone, Debug)]
pub(crate) struct NativeWindow {
  pub(crate) id: WindowId,
  /// Shared accessibility element for the window.
  ///
  /// Refreshable in place because some apps swap the `AXUIElement` for a
  /// still-live window.
  pub(crate) element: Arc<ThreadBound<RefCell<CFRetained<AXUIElement>>>>,
  pub(crate) application: Application,
}

impl NativeWindow {
  /// Creates an instance of `NativeWindow`.
  #[must_use]
  pub(crate) fn new(
    id: WindowId,
    element: ThreadBound<RefCell<CFRetained<AXUIElement>>>,
    application: Application,
  ) -> Self {
    Self {
      element: Arc::new(element),
      id,
      application,
    }
  }

  /// Runs `f` with the window's current accessibility element.
  ///
  /// Centralizes borrowing the `RefCell` so call sites can treat the
  /// element as if it were stored directly.
  ///
  /// Borrows are scoped to `f` and all element access happens on the
  /// event loop thread, so a borrow conflict is never expected. If one
  /// occurs regardless, an error is returned instead of panicking, since
  /// unwinding out of an accessibility callback would abort the process.
  pub(crate) fn with_element<F, R>(&self, f: F) -> crate::Result<R>
  where
    F: Send + FnOnce(&CFRetained<AXUIElement>) -> R,
    R: Send,
  {
    self.element.with(|cell| {
      let element =
        cell.try_borrow().map_err(|_| element_borrow_error())?;
      Ok(f(&element))
    })?
  }

  /// Replaces the window's accessibility element in place.
  ///
  /// Used when an application swaps the element backing a still-live
  /// window. All clones share the same `Arc`, so the new element is seen
  /// by every holder.
  ///
  /// Must be called on the event loop thread, and never from within a
  /// `with_element` closure on the same window.
  pub(crate) fn set_element(
    &self,
    element: CFRetained<AXUIElement>,
  ) -> crate::Result<()> {
    let cell = self.element.get_ref()?;
    let mut current =
      cell.try_borrow_mut().map_err(|_| element_borrow_error())?;
    *current = element;
    Ok(())
  }

  /// Clones (retains) the window's current accessibility element.
  ///
  /// Must be called on the event loop thread.
  pub(crate) fn element_clone(
    &self,
  ) -> crate::Result<CFRetained<AXUIElement>> {
    let cell = self.element.get_ref()?;
    let element = cell
      .try_borrow()
      .map_err(|_| element_borrow_error())?
      .clone();
    Ok(element)
  }

  /// Whether `element` is the window's current accessibility element.
  ///
  /// Returns `false` if the element is inaccessible (called off the
  /// event loop thread, or the element is concurrently borrowed).
  pub(crate) fn matches_element(
    &self,
    element: &CFRetained<AXUIElement>,
  ) -> bool {
    self.element.get_ref().is_ok_and(|cell| {
      cell.try_borrow().is_ok_and(|current| &*current == element)
    })
  }

  /// Implements [`NativeWindow::id`].
  pub(crate) fn id(&self) -> WindowId {
    self.id
  }

  /// Implements [`NativeWindow::title`].
  pub(crate) fn title(&self) -> crate::Result<String> {
    self.with_element(|el| {
      el.get_attribute::<CFString>("AXTitle")
        .map(|cf_string| cf_string.to_string())
    })?
  }

  /// Implements [`NativeWindow::process_name`].
  pub(crate) fn process_name(&self) -> crate::Result<String> {
    self
      .application
      .process_name()
      .ok_or(crate::Error::Platform(
        "Failed to get application process name.".to_string(),
      ))
  }

  /// Implements [`NativeWindow::frame`].
  ///
  /// Position and size are read in one request, so the frame costs a
  /// single hop and a single round trip to the application.
  pub(crate) fn frame(&self) -> crate::Result<Rect> {
    // TODO: Would `AXFrame` work instead?
    self.with_element(|el| {
      let (position, size) = read_position_and_size(el)?;
      Ok(truncated_frame(position, size))
    })?
  }

  /// Implements [`NativeWindow::frame_and_state`].
  ///
  /// Frame, minimized flag and full-screen flag are read in one request,
  /// where [`Self::frame`], [`Self::is_minimized`] and
  /// [`Self::is_maximized`] cost one hop and one round trip each.
  ///
  /// Only the frame and the request as a whole must succeed. Each flag
  /// carries its own result, so one unreadable attribute fails only the
  /// question about it, as it would from the separate reads. As in
  /// [`Self::window_state`], a window read as minimized is reported as
  /// not maximized, and its full-screen attribute is not required to be
  /// readable.
  pub(crate) fn frame_and_state(&self) -> crate::Result<WindowFrameState> {
    self.with_element(|el| {
      let mut values = el
        .get_attributes(&[
          "AXPosition",
          "AXSize",
          "AXMinimized",
          "AXFullScreen",
        ])?
        .into_iter();

      let position = next_value(&mut values, "AXPosition")?;
      let size = next_value(&mut values, "AXSize")?;
      let minimized = next_value(&mut values, "AXMinimized");
      let maximized = next_value(&mut values, "AXFullScreen");

      let frame = truncated_frame(
        ax_value_from("AXPosition", &position)?,
        ax_value_from("AXSize", &size)?,
      );
      let (is_minimized, is_maximized) = flag_results(
        minimized.and_then(|value| flag_from_value("AXMinimized", &value)),
        || {
          maximized
            .and_then(|value| flag_from_value("AXFullScreen", &value))
        },
      );

      Ok(WindowFrameState {
        frame,
        is_minimized,
        is_maximized,
      })
    })?
  }

  /// Implements [`NativeWindow::position`].
  pub(crate) fn position(&self) -> crate::Result<(f64, f64)> {
    self.with_element(move |el| {
      el.get_attribute::<AXValue>("AXPosition")
        .and_then(|ax_value| ax_value.value_strict::<CGPoint>())
        .map(|point| (point.x, point.y))
    })?
  }

  /// Implements [`NativeWindow::size`].
  pub(crate) fn size(&self) -> crate::Result<(f64, f64)> {
    self.with_element(move |el| {
      el.get_attribute::<AXValue>("AXSize")
        .and_then(|ax_value| ax_value.value_strict::<CGSize>())
        .map(|size| (size.width, size.height))
    })?
  }

  /// Implements [`NativeWindow::is_valid`].
  pub(crate) fn is_valid(&self) -> bool {
    // Query `AXRole`, which is present on all valid `AXUIElement`s.
    self
      .with_element(|el| match el.get_attribute::<CFString>("AXRole") {
        Err(crate::Error::Accessibility(_, code))
          if code == AXError::InvalidUIElement.0 =>
        {
          let has_login_window = NSWorkspace::sharedWorkspace()
            .frontmostApplication()
            .and_then(|app| app.bundleIdentifier())
            .is_some_and(|id| id.to_string() == "com.apple.loginwindow");

          // AX calls transiently fail with `InvalidUIElement` during
          // sleep/wake. The window should still be considered valid.
          //
          // Events during sleep:
          //   1. Display goes asleep.
          //   2. AX calls fail with `InvalidUIElement`.
          //   3. Login window activates.
          //
          // Events during wake:
          //   1. Display wakes up.
          //   2. Login window deactivates and AX calls succeed again.
          //
          // Perf: `CGDisplayIsAsleep` ~1-5µs, login window check ~1-2ms.
          CGDisplayIsAsleep(0) || has_login_window
        }
        Ok(_) => true,
        // No answer (e.g. -25204) says nothing about the window: the app
        // may be busy, or gone along with its windows. The window server
        // knows either way, without the app.
        //
        // Perf: `AXRole` ~25us p50 from a responsive app; the window
        // server lookup ~100us p50, so it only runs when AX cannot answer.
        Err(_) => self.exists_on_window_server(),
      })
      .unwrap_or(false)
  }

  /// Whether the window server still has this window.
  ///
  /// Answers without the owning application, so it holds while the
  /// application is busy or after it has exited. Trails the application's
  /// own destroy notification by a moment.
  fn exists_on_window_server(&self) -> bool {
    NativeCallStats::record(NativeCall::WindowListSingle);
    CGWindowListCopyWindowInfo(
      CGWindowListOption::OptionIncludingWindow,
      self.id.0,
    )
    .is_some_and(|windows| windows.count() > 0)
  }

  /// Implements [`NativeWindow::is_visible`].
  #[allow(clippy::unnecessary_wraps)]
  pub(crate) fn is_visible(&self) -> crate::Result<bool> {
    Ok(!self.application.is_hidden())
  }

  /// Implements [`NativeWindow::is_minimized`].
  pub(crate) fn is_minimized(&self) -> crate::Result<bool> {
    self.with_element(|el| {
      el.get_attribute::<CFBoolean>("AXMinimized")
        .map(|cf_bool| cf_bool.value())
    })?
  }

  /// Implements [`NativeWindow::is_maximized`].
  pub(crate) fn is_maximized(&self) -> crate::Result<bool> {
    self.with_element(|el| {
      el.get_attribute::<CFBoolean>("AXFullScreen")
        .map(|cf_bool| cf_bool.value())
    })?
  }

  /// Reads whether the window is minimized and whether it is maximized,
  /// in one request.
  ///
  /// Asking for both costs the application a single round trip, where
  /// [`Self::is_minimized`] and [`Self::is_maximized`] cost one each. A
  /// minimized window is reported as not maximized, and its full-screen
  /// attribute is not required to be readable.
  pub(crate) fn window_state(&self) -> crate::Result<(bool, bool)> {
    self.with_element(|el| {
      let mut values = el
        .get_attributes(&["AXMinimized", "AXFullScreen"])?
        .into_iter();
      let mut flag = |attribute: &str| -> crate::Result<bool> {
        flag_from_value(attribute, &next_value(&mut values, attribute)?)
      };

      let minimized = flag("AXMinimized")?;
      Ok((minimized, !minimized && flag("AXFullScreen")?))
    })?
  }

  /// Implements [`NativeWindow::is_resizable`].
  #[allow(clippy::unnecessary_wraps, clippy::unused_self)]
  pub(crate) fn is_resizable(&self) -> crate::Result<bool> {
    // TODO: Not sure if this is even available via the AX API.
    Ok(true)
  }

  /// Implements [`NativeWindow::is_desktop_window`].
  ///
  /// Finder exposes the desktop as an `AXScrollArea`, while folder windows
  /// are `AXStandardWindow`.
  pub(crate) fn is_desktop_window(&self) -> crate::Result<bool> {
    if self.application.bundle_id() != Some("com.apple.finder".to_string())
    {
      return Ok(false);
    }

    // Unreadable subroles are treated as the desktop.
    let subrole = self.with_element(|el| {
      el.get_attribute::<CFString>("AXSubrole")
        .map(|subrole| subrole.to_string())
    })?;

    Ok(subrole.map_or(true, |subrole| subrole != "AXStandardWindow"))
  }

  /// Implements [`NativeWindow::set_frame`].
  ///
  /// Returns once the writes are delivered, not once they are applied:
  /// see [`with_deferred_writes`]. Callers observe the result.
  ///
  /// # Platform-specific
  ///
  /// - macOS returns success for a size write it then ignores while the
  ///   window is parked to a 1px sliver or straddles two displays. The
  ///   window is first staged fully onto the target's display at its
  ///   current size, then resized, then moved into place. A size still
  ///   refused is corrected by the caller's next request, once the window
  ///   is fully on one display.
  pub(crate) fn set_frame(&self, rect: &Rect) -> crate::Result<()> {
    self.write_frame(rect, None)
  }

  /// Implements [`NativeWindow::set_frame_on_display`].
  pub(crate) fn set_frame_on_display(
    &self,
    rect: &Rect,
    display: &Rect,
  ) -> crate::Result<()> {
    self.write_frame(rect, Some(display))
  }

  /// Writes `rect`, staging the window onto a display first if needed.
  ///
  /// `display` is the bounds of the display `rect` belongs to. It is used
  /// when it wholly contains `rect`; otherwise (or when `None`) the
  /// display is looked up by walking every screen. A bare move needs no
  /// display.
  fn write_frame(
    &self,
    rect: &Rect,
    display: Option<&Rect>,
  ) -> crate::Result<()> {
    let rect = rect.clone();
    let display = display.cloned();
    let dispatcher = self.application.dispatcher.clone();

    self.with_enhanced_ui_disabled(move |el| -> crate::Result<()> {
      let current = read_frame(el)?;

      let is_same_size = current.width() == rect.width()
        && current.height() == rect.height();

      // A bare move is safe from anywhere.
      if is_same_size {
        return with_deferred_writes(el, || {
          delivered(write_position(el, rect.x(), rect.y()))?;
          delivered(write_size(el, rect.width(), rect.height()))
        });
      }

      let display = platform_impl::display_bounds_for_rect(
        &rect,
        display.as_ref(),
        &dispatcher,
      )?;

      // The application applies these in order, so the size lands while
      // the window is staged and the move follows it.
      with_deferred_writes(el, || {
        if !display.contains_rect(&current) {
          let staging = staging_origin(&current, &rect, &display);
          delivered(write_position(el, staging.x, staging.y))?;
        }

        delivered(write_size(el, rect.width(), rect.height()))?;
        delivered(write_position(el, rect.x(), rect.y()))
      })
    })
  }

  /// Implements [`NativeWindow::resize`].
  pub(crate) fn resize(
    &self,
    width: i32,
    height: i32,
  ) -> crate::Result<()> {
    self.with_enhanced_ui_disabled(move |el| write_size(el, width, height))
  }

  /// Implements [`NativeWindow::reposition`].
  pub(crate) fn reposition(&self, x: i32, y: i32) -> crate::Result<()> {
    self.with_enhanced_ui_disabled(move |el| write_position(el, x, y))
  }

  /// Implements [`NativeWindow::minimize`].
  pub(crate) fn minimize(&self) -> crate::Result<()> {
    self.with_element(move |el| -> crate::Result<()> {
      let ax_bool = CFBoolean::new(true);
      el.set_attribute::<CFBoolean>("AXMinimized", &ax_bool.into())
    })?
  }

  /// Implements [`NativeWindow::maximize`].
  pub(crate) fn maximize(&self) -> crate::Result<()> {
    self.with_element(move |el| -> crate::Result<()> {
      let ax_bool = CFBoolean::new(true);
      el.set_attribute::<CFBoolean>("AXFullScreen", &ax_bool.into())
    })?
  }

  /// Implements [`NativeWindow::focus`].
  pub(crate) fn focus(&self) -> crate::Result<()> {
    self.focus_with(RaiseWait::Acknowledged)
  }

  /// Implements [`NativeWindow::focus_deferred_raise`].
  pub(crate) fn focus_deferred_raise(&self) -> crate::Result<()> {
    self.focus_with(RaiseWait::Deferred)
  }

  /// Focuses the window, then raises it as `raise_wait` says.
  fn focus_with(&self, raise_wait: RaiseWait) -> crate::Result<()> {
    let psn = self.application.psn()?;
    self.set_front_process(&psn)?;
    self.set_key_window(&psn)?;
    self.raise(raise_wait)
  }

  /// Implements [`NativeWindow::close`].
  pub(crate) fn close(&self) -> crate::Result<()> {
    self.with_element(|el| -> crate::Result<()> {
      let close_button =
        el.get_attribute::<AXUIElement>("AXCloseButton")?;

      // Simulate pressing the window's close button.
      NativeCallStats::record(NativeCall::AxAction);
      let result = unsafe {
        close_button.perform_action(&CFString::from_str("AXPress"))
      };

      if result != AXError::Success {
        return Err(crate::Error::Accessibility(
          "AXPress".to_string(),
          result.0,
        ));
      }

      Ok(())
    })?
  }

  /// Executes a callback with the `AXEnhancedUserInterface` attribute
  /// temporarily disabled on the application `AXUIElement`.
  ///
  /// This is to prevent inconsistent window resizing and repositioning
  /// for certain applications (e.g. Firefox).
  ///
  /// References:
  /// - <https://github.com/koekeishiya/yabai/commit/3fe4c77b001e1a4f613c26f01ea68c0f09327f3a>
  /// - <https://github.com/rxhanson/Rectangle/pull/285>
  fn with_enhanced_ui_disabled<F, R>(
    &self,
    callback: F,
  ) -> crate::Result<R>
  where
    F: FnOnce(&CFRetained<AXUIElement>) -> crate::Result<R> + Send,
    R: Send,
  {
    self.application.ax_element.with(|app_el| {
      // Get whether enhanced UI is currently enabled, once per app. The
      // read is a blocking cross-process call on the event loop thread,
      // and this gates every frame set, so re-reading it made each move
      // pay for a value that effectively never changes.
      let was_enabled = *self.application.enhanced_ui.get_or_init(|| {
        app_el
          .get_attribute::<CFBoolean>("AXEnhancedUserInterface")
          .is_ok_and(|cf_bool| cf_bool.value())
      });

      // Disable enhanced UI if it was enabled. The application handles
      // requests in order, so it is off for the writes that follow and on
      // again after them without waiting on either.
      if was_enabled {
        let ax_bool = CFBoolean::new(false);
        let _ = with_deferred_writes(app_el, || {
          delivered(app_el.set_attribute::<CFBoolean>(
            "AXEnhancedUserInterface",
            &ax_bool.into(),
          ))
        });
      }

      // Execute the callback with the window element.
      let result = self.with_element(callback);

      // Restore enhanced UI if it was originally enabled.
      if was_enabled {
        let ax_bool = CFBoolean::new(true);
        let _ = with_deferred_writes(app_el, || {
          delivered(app_el.set_attribute::<CFBoolean>(
            "AXEnhancedUserInterface",
            &ax_bool.into(),
          ))
        });
      }

      result
    })??
  }

  fn raise(&self, raise_wait: RaiseWait) -> crate::Result<()> {
    self.with_element(move |el| -> crate::Result<()> {
      // This has a couple of caveats:
      // - Some windows do not get raised without first calling
      //   `_SLPSSetFrontProcessWithOptions`.
      // - This changes focus if raising a window of the frontmost (active)
      //   application. For example, if 2 Chrome windows are open and one
      //   is focused, raising the other will change focus to the other
      //   window.
      //
      // Because of these caveats, this method is not exposed as a public
      // API. It's also the reason why the GlazeWM feature of bringing all
      // tiling/floating windows to the front on focus change is not
      // implemented for macOS.
      let raise = || {
        NativeCallStats::record(NativeCall::AxAction);
        let result =
          unsafe { el.perform_action(&CFString::from_str("AXRaise")) };

        if result == AXError::Success {
          Ok(())
        } else {
          Err(crate::Error::Accessibility("AXRaise".to_string(), result.0))
        }
      };

      match raise_wait {
        RaiseWait::Acknowledged => raise(),
        RaiseWait::Deferred => {
          with_deferred_writes(el, || delivered(raise()))
        }
      }
    })?
  }

  fn set_front_process(
    &self,
    psn: &ffi::ProcessSerialNumber,
  ) -> crate::Result<()> {
    let result = unsafe {
      #[allow(clippy::cast_possible_wrap)]
      ffi::_SLPSSetFrontProcessWithOptions(
        psn,
        self.id.0 as i32,
        ffi::CPS_USER_GENERATED,
      )
    };

    if result != CGError::Success {
      return Err(crate::Error::Platform(
        "Failed to set front process.".to_string(),
      ));
    }

    Ok(())
  }

  fn set_key_window(
    &self,
    psn: &ffi::ProcessSerialNumber,
  ) -> crate::Result<()> {
    // Ref: https://github.com/Hammerspoon/hammerspoon/issues/370#issuecomment-545545468
    let window_id = self.id.0.to_ne_bytes();
    let mut event1 = [0; 0x100];
    event1[0x04] = 0xf8;
    event1[0x08] = 0x01;
    event1[0x3a] = 0x10;
    event1[0x3c..(0x3c + window_id.len())].copy_from_slice(&window_id);
    event1[0x20..(0x20 + 0x10)].fill(0xff);

    let mut event2 = event1;
    event2[0x08] = 0x02;

    for event in [event1, event2] {
      let result =
        unsafe { ffi::SLPSPostEventRecordTo(psn, event.as_ptr().cast()) };

      if result != CGError::Success {
        return Err(crate::Error::Platform(
          "Failed to set key window.".to_string(),
        ));
      }
    }

    Ok(())
  }
}

impl From<NativeWindow> for crate::NativeWindow {
  fn from(window: NativeWindow) -> Self {
    crate::NativeWindow { inner: window }
  }
}

/// Implements [`WindowLiveness::from_window_server`].
///
/// Lists every window the window server has, whether on-screen or not, so
/// windows that are minimized, hidden or on another space are included.
/// Returns `None` when the list is unavailable.
///
/// Perf: `CGWindowListCreate` returns the IDs alone, ~0.2ms p50 for 170
/// windows. `CGWindowListCopyWindowInfo` builds a dictionary for each
/// window, ~3ms p50 for the same list.
///
/// [`WindowLiveness::from_window_server`]: crate::WindowLiveness::from_window_server
pub(crate) fn listed_window_ids() -> Option<HashSet<WindowId>> {
  NativeCallStats::record(NativeCall::WindowListFull);
  let windows = CGWindowListCreate(CGWindowListOption::OptionAll, 0)?;

  Some(
    (0..windows.count())
      .filter_map(|index| {
        // SAFETY: The array is retained and `index` is within its count.
        let value = unsafe { windows.value_at_index(index) };

        // The array holds each `CGWindowID` as the pointer-sized value
        // itself, not as an object, and is never dereferenced.
        u32::try_from(value.addr()).ok().map(WindowId)
      })
      .collect(),
  )
}

/// Reports native stacking order as unsupported on macOS.
pub(crate) fn stacking_order(
  _ids: &[WindowId],
  _dispatcher: &Dispatcher,
) -> crate::Result<Vec<WindowId>> {
  Err(crate::Error::Platform(
    "Native stacking-order observation is unsupported on macOS.".into(),
  ))
}

/// Implements [`Dispatcher::visible_windows`].
pub(crate) fn visible_windows(
  dispatcher: &Dispatcher,
) -> crate::Result<Vec<crate::NativeWindow>> {
  Ok(
    platform_impl::all_applications(dispatcher)?
      .iter()
      .filter_map(|app| app.windows().ok())
      .flat_map(std::iter::IntoIterator::into_iter)
      .collect(),
  )
}

/// Implements [`Dispatcher::window_by_id`].
pub(crate) fn window_by_id(
  id: WindowId,
  dispatcher: &Dispatcher,
) -> crate::Result<Option<crate::NativeWindow>> {
  // TODO: The performance of this is terrible. A better solution would be
  // to have a cache of window ID <-> `NativeWindow` instances.
  for app in platform_impl::all_applications(dispatcher)? {
    if let Ok(windows) = app.windows() {
      if let Some(win) = windows.into_iter().find(|w| w.id() == id) {
        return Ok(Some(win));
      }
    }
  }

  Ok(None)
}

/// Implements [`Dispatcher::window_from_point`].
pub(crate) fn window_from_point(
  point: &Point,
  dispatcher: &Dispatcher,
) -> crate::Result<Option<crate::NativeWindow>> {
  // Get the top-most window ID at the given point.
  let window_id = dispatcher.dispatch_sync(|| {
    let cg_point = CGPoint {
      x: f64::from(point.x),
      y: f64::from(point.y),
    };

    let window_id = unsafe {
      NSWindow::windowNumberAtPoint_belowWindowWithWindowNumber(
        cg_point,
        // 0 for all windows.
        0,
        MainThreadMarker::new_unchecked(),
      )
    };

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    WindowId(window_id as u32)
  })?;

  // No window found at the given point.
  if window_id.0 == 0 {
    return Ok(None);
  }

  window_by_id(window_id, dispatcher)
    .map_err(|_| crate::Error::WindowNotFound)
}

/// Implements [`Dispatcher::focused_window`].
pub(crate) fn focused_window(
  dispatcher: &Dispatcher,
) -> crate::Result<crate::NativeWindow> {
  dispatcher
    .dispatch_sync(|| {
      // Get the frontmost (active) application.
      let frontmost_app = NSWorkspace::sharedWorkspace()
        .frontmostApplication()
        .map(|app| Application::new(app, dispatcher.clone()));

      // Get the focused window of the frontmost application.
      frontmost_app.and_then(|app| app.focused_window().ok().flatten())
    })?
    .ok_or(crate::Error::WindowNotFound)
}

/// Implements [`Dispatcher::reset_focus`].
// TODO: Move this to a better-suited module.
pub(crate) fn reset_focus(dispatcher: &Dispatcher) -> crate::Result<()> {
  let Some(application) = platform_impl::application_for_bundle_id(
    "com.apple.finder",
    dispatcher,
  )?
  else {
    return Err(crate::Error::Platform(
      "Failed to get desktop application.".to_string(),
    ));
  };

  let success = application.ns_app.activateWithOptions(
    NSApplicationActivationOptions::ActivateAllWindows,
  );

  if !success {
    return Err(crate::Error::Platform(
      "Failed to activate desktop application.".to_string(),
    ));
  }

  Ok(())
}

/// Error for a conflicting borrow of a window's accessibility element.
fn element_borrow_error() -> crate::Error {
  crate::Error::Platform(
    "Window accessibility element is already borrowed.".to_string(),
  )
}

/// Reads the window's position and size in one request.
fn read_position_and_size(
  el: &CFRetained<AXUIElement>,
) -> crate::Result<(CGPoint, CGSize)> {
  let mut values =
    el.get_attributes(&["AXPosition", "AXSize"])?.into_iter();

  let position = next_value(&mut values, "AXPosition")?;
  let size = next_value(&mut values, "AXSize")?;

  Ok((
    ax_value_from("AXPosition", &position)?,
    ax_value_from("AXSize", &size)?,
  ))
}

/// Reads the window's frame via `AXPosition` and `AXSize`, rounded to
/// pixels.
#[allow(clippy::cast_possible_truncation)]
fn read_frame(el: &CFRetained<AXUIElement>) -> crate::Result<Rect> {
  let (position, size) = read_position_and_size(el)?;

  Ok(Rect::from_xy(
    position.x.round() as i32,
    position.y.round() as i32,
    size.width.round() as i32,
    size.height.round() as i32,
  ))
}

/// Converts an accessibility position and size into a `Rect`, truncating
/// to whole pixels.
#[allow(clippy::cast_possible_truncation)]
fn truncated_frame(position: CGPoint, size: CGSize) -> Rect {
  Rect::from_xy(
    position.x as i32,
    position.y as i32,
    size.width as i32,
    size.height as i32,
  )
}

/// Takes the next result of a multiple-attribute read, which holds one per
/// requested attribute.
fn next_value(
  values: &mut std::vec::IntoIter<crate::Result<CFRetained<CFType>>>,
  attribute: &str,
) -> crate::Result<CFRetained<CFType>> {
  values.next().ok_or_else(|| {
    crate::Error::InvalidPointer(format!(
      "No value returned for {attribute}."
    ))
  })?
}

/// Decodes a value read for `attribute` as an `AXValue` holding a `T`.
fn ax_value_from<T: AXValueTypeMarker>(
  attribute: &str,
  value: &CFRetained<CFType>,
) -> crate::Result<T> {
  value
    .downcast_ref::<AXValue>()
    .ok_or_else(|| {
      crate::Error::Platform(format!("{attribute} is not an AXValue."))
    })?
    .value_strict::<T>()
}

/// Settles the two state flags of a window from their own results.
///
/// A window read as minimized is reported as not maximized, without
/// reading its full-screen attribute (`maximized` is not called). Any
/// other flag keeps the result of its own read, so one that failed fails
/// only the question about it.
fn flag_results(
  minimized: crate::Result<bool>,
  maximized: impl FnOnce() -> crate::Result<bool>,
) -> (crate::Result<bool>, crate::Result<bool>) {
  let maximized = if matches!(minimized, Ok(true)) {
    Ok(false)
  } else {
    maximized()
  };

  (minimized, maximized)
}

/// Decodes a value read for `attribute` as a flag.
///
/// Some applications answer a flag with a number, which the
/// single-attribute reads always took as its truth value.
fn flag_from_value(
  attribute: &str,
  value: &CFRetained<CFType>,
) -> crate::Result<bool> {
  value
    .downcast_ref::<CFBoolean>()
    .map(CFBoolean::value)
    .or_else(|| {
      value
        .downcast_ref::<CFNumber>()
        .and_then(CFNumber::as_i64)
        .map(|number| number != 0)
    })
    .ok_or_else(|| {
      crate::Error::Platform(format!("{attribute} is not a boolean."))
    })
}

/// Messaging timeout for writes whose outcome is observed afterwards.
///
/// An AX write blocks until the application has handled it, and a resize
/// first makes the application lay out and draw at the new size (~31ms
/// for kitty, ~97ms for Zen). Waiting buys only a receipt: a request that
/// times out is still delivered and applied. Placement confirms the
/// result from the window server and the application's move events, so
/// several applications apply their writes in parallel instead of one
/// after another on the event loop thread.
///
/// 2ms covers a write that needs no relayout (p50 0.2-0.4ms, p99 up to
/// 2.2ms), so those still report their errors.
const DEFERRED_WRITE_TIMEOUT_SECS: f32 = 0.002;

/// How long a raise waits for the application to acknowledge it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RaiseWait {
  /// Waits for the application, up to the global messaging timeout.
  Acknowledged,
  /// Waits only for [`DEFERRED_WRITE_TIMEOUT_SECS`]. A raise that times
  /// out is still delivered, so it counts as sent (see [`delivered`]).
  Deferred,
}

/// Runs `writes` against `el` without waiting for the application to
/// apply them.
///
/// Each write in `writes` must go through [`delivered`]: a slow write
/// times out by design, and the writes after it still have to be sent.
fn with_deferred_writes(
  el: &AXUIElement,
  writes: impl FnOnce() -> crate::Result<()>,
) -> crate::Result<()> {
  // SAFETY: `el` is a valid element. The timeout is client-side state
  // for this element only.
  unsafe { el.set_messaging_timeout(DEFERRED_WRITE_TIMEOUT_SECS) };

  let result = writes();

  // SAFETY: As above. `0` returns the element to the global timeout.
  unsafe { el.set_messaging_timeout(0.0) };

  result
}

/// Treats a write that timed out as delivered (see [`is_stall`]).
///
/// Any other error still fails the write.
fn delivered(result: crate::Result<()>) -> crate::Result<()> {
  match result {
    Err(err) if is_stall(&err) => Ok(()),
    result => result,
  }
}

/// Writes `AXSize`.
fn write_size(
  el: &CFRetained<AXUIElement>,
  width: i32,
  height: i32,
) -> crate::Result<()> {
  let size = CGSize::new(width.into(), height.into());
  el.set_attribute("AXSize", &AXValue::new_strict(&size)?)
}

/// Writes `AXPosition`.
fn write_position(
  el: &CFRetained<AXUIElement>,
  x: i32,
  y: i32,
) -> crate::Result<()> {
  let point = CGPoint::new(x.into(), y.into());
  el.set_attribute("AXPosition", &AXValue::new_strict(&point)?)
}

/// Where to put a window, at its current size, so it lies fully on
/// `display` and as close to `target` as it can.
///
/// A window larger than the display on an axis is pinned to the near edge.
fn staging_origin(current: &Rect, target: &Rect, display: &Rect) -> Point {
  let fit = |start: i32, length: i32, low: i32, high: i32| {
    if length >= high - low {
      low
    } else {
      start.clamp(low, high - length)
    }
  };

  Point {
    x: fit(target.x(), current.width(), display.left, display.right),
    y: fit(target.y(), current.height(), display.top, display.bottom),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const DISPLAY: Rect = Rect {
    left: -3008,
    top: -368,
    right: 0,
    bottom: 1324,
  };

  #[test]
  fn delivered_maps_a_stall_to_success() {
    let stall = crate::Error::Accessibility(
      "AXRaise".to_string(),
      AXError::CannotComplete.0,
    );
    assert!(delivered(Err(stall)).is_ok());
    assert!(delivered(Ok(())).is_ok());
  }

  #[test]
  fn delivered_keeps_every_other_error() {
    let refused = crate::Error::Accessibility(
      "AXRaise".to_string(),
      AXError::InvalidUIElement.0,
    );
    assert!(matches!(
      delivered(Err(refused)),
      Err(crate::Error::Accessibility(_, code))
        if code == AXError::InvalidUIElement.0
    ));
    assert!(matches!(
      delivered(Err(crate::Error::WindowNotFound)),
      Err(crate::Error::WindowNotFound)
    ));
  }

  fn refused() -> crate::Error {
    crate::Error::Accessibility(
      "AXFullScreen".to_string(),
      AXError::AttributeUnsupported.0,
    )
  }

  #[test]
  fn a_minimized_window_is_not_maximized_and_is_not_asked() {
    let (minimized, maximized) =
      flag_results(Ok(true), || panic!("Read the full-screen flag."));
    assert!(matches!(minimized, Ok(true)));
    assert!(matches!(maximized, Ok(false)));
  }

  #[test]
  fn each_flag_keeps_the_result_of_its_own_read() {
    let (minimized, maximized) = flag_results(Ok(false), || Ok(true));
    assert!(matches!(minimized, Ok(false)));
    assert!(matches!(maximized, Ok(true)));

    let (minimized, maximized) =
      flag_results(Ok(false), || Err(refused()));
    assert!(matches!(minimized, Ok(false)));
    assert!(maximized.is_err());

    // An unreadable minimized flag does not hide the full-screen one.
    let (minimized, maximized) = flag_results(Err(refused()), || Ok(true));
    assert!(minimized.is_err());
    assert!(matches!(maximized, Ok(true)));
  }

  #[test]
  fn keeps_a_target_that_already_fits() {
    let current = Rect::from_xy(-2000, -320, 841, 1628);
    let target = Rect::from_xy(-1500, -320, 632, 1628);
    let origin = staging_origin(&current, &target, &DISPLAY);
    assert_eq!((origin.x, origin.y), (-1500, -320));
  }

  #[test]
  fn pulls_an_overhang_back_from_the_right() {
    let current = Rect::from_xy(-1263, -320, 841, 1628);
    let target = Rect::from_xy(-648, -320, 632, 1628);
    let origin = staging_origin(&current, &target, &DISPLAY);
    assert_eq!((origin.x, origin.y), (-841, -320));
  }

  #[test]
  fn pulls_an_overhang_back_from_the_left() {
    let current = Rect::from_xy(-3848, 1296, 841, 1628);
    let target = Rect::from_xy(-3100, -320, 632, 1628);
    let origin = staging_origin(&current, &target, &DISPLAY);
    assert_eq!((origin.x, origin.y), (-3008, -320));
  }

  #[test]
  fn pins_a_window_larger_than_the_display() {
    let current = Rect::from_xy(-100, 0, 4000, 2000);
    let target = Rect::from_xy(-2992, -320, 2976, 1628);
    let origin = staging_origin(&current, &target, &DISPLAY);
    assert_eq!((origin.x, origin.y), (-3008, -368));
  }

  #[test]
  fn lifts_a_parked_sliver_onto_the_display() {
    let current = Rect::from_xy(-1, 1296, 841, 1628);
    let target = Rect::from_xy(-648, 1000, 632, 1628);
    let origin = staging_origin(&current, &target, &DISPLAY);
    assert_eq!((origin.x, origin.y), (-841, -304));
  }
}
