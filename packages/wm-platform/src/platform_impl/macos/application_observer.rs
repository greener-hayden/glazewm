use std::{
  ptr::NonNull,
  sync::{Arc, Mutex},
};

use objc2_application_services::{AXError, AXObserver, AXUIElement};
use objc2_core_foundation::{
  kCFRunLoopDefaultMode, CFRetained, CFRunLoop, CFRunLoopSource, CFString,
};
use tokio::sync::mpsc;

use crate::{
  platform_impl::{Application, ProcessId, WindowEventNotificationInner},
  WindowEvent,
};

/// Notifications to register for the `AXUIElement` of an application.
const AX_APP_NOTIFICATIONS: &[&str] =
  &["AXFocusedWindowChanged", "AXWindowCreated"];

/// Notifications to register for the `AXUIElement` of a window.
const AX_WINDOW_NOTIFICATIONS: &[&str] = &[
  "AXTitleChanged",
  "AXUIElementDestroyed",
  "AXWindowMoved",
  "AXWindowResized",
  "AXWindowDeminiaturized",
  "AXWindowMiniaturized",
];

/// Context passed to the application event callback.
#[derive(Debug)]
struct ApplicationEventContext {
  application: Application,
  events_tx: mpsc::UnboundedSender<WindowEvent>,
  app_windows: Arc<Mutex<Vec<crate::NativeWindow>>>,
  observer: CFRetained<AXObserver>,
}

/// Represents an accessibility observer for a specific application.
#[derive(Debug)]
pub(crate) struct ApplicationObserver {
  pub(crate) pid: ProcessId,
  app_windows: Arc<Mutex<Vec<crate::NativeWindow>>>,
  events_tx: mpsc::UnboundedSender<WindowEvent>,
  _observer: CFRetained<AXObserver>,
  observer_source: CFRetained<CFRunLoopSource>,
}

// TODO: Remove this.
unsafe impl Send for ApplicationObserver {}

impl ApplicationObserver {
  /// Creates a new `ApplicationObserver` for the given application.
  ///
  /// Registers application- and window-level accessibility notifications,
  /// then emits `WindowEvent::Shown` for every existing window so the
  /// window manager can adopt windows that are already open. A failed
  /// window query is tolerated so the observer is still created and
  /// future windows are caught.
  pub fn new(
    app: &Application,
    events_tx: mpsc::UnboundedSender<WindowEvent>,
  ) -> crate::Result<Self> {
    let observer = unsafe {
      let mut observer = std::ptr::null_mut();

      let result = AXObserver::create(
        app.pid,
        Some(Self::window_event_callback),
        // SAFETY: Stack address of `observer` is guaranteed to be
        // non-null.
        NonNull::new(&raw mut observer).unwrap(),
      );

      if result != AXError::Success {
        return Err(crate::Error::Accessibility(
          "AXObserverCreate".to_string(),
          result.0,
        ));
      }

      CFRetained::retain(NonNull::new(observer).ok_or_else(|| {
        crate::Error::InvalidPointer("AXObserver is null.".to_string())
      })?)
    };

    // Start empty; the window set is populated by the re-scan below, after
    // notifications are registered.
    let app_windows = Arc::new(Mutex::new(Vec::new()));
    let context = Box::into_raw(Box::new(ApplicationEventContext {
      application: app.clone(),
      events_tx: events_tx.clone(),
      app_windows: app_windows.clone(),
      observer: observer.clone(),
    }));

    let runloop =
      CFRunLoop::current().ok_or(crate::Error::EventLoopStopped)?;

    let observer_source = unsafe { observer.run_loop_source() };
    runloop.add_source(Some(&observer_source), unsafe {
      kCFRunLoopDefaultMode
    });

    // Register for all window notifications.
    // TODO: Remove from runloop if registration fails.
    Self::register_app_notifications(app, &observer, context)?;

    // Re-scan windows *after* registering app-level notifications. This
    // recovers windows that were missed if an earlier query transiently
    // failed, and catches any window created while registration was in
    // progress.
    let windows = app.windows().unwrap_or_default();

    // Emit `WindowEvent::Shown` for every existing window. The window
    // manager's handler is idempotent — it ignores already-managed windows
    // — so this safely adopts windows missed during startup population
    // without duplicating ones that are already tracked.
    for window in &windows {
      if let Err(err) =
        Self::register_window_notifications(window, &observer, context)
      {
        tracing::warn!(
          "Failed to register window notifications for PID {}: {}",
          app.pid,
          err
        );
      }

      Self::send_window_event(
        &events_tx,
        app.pid,
        WindowEvent::Shown {
          window: window.clone(),
          notification: crate::WindowEventNotification(None),
        },
      );
    }

    *app_windows.lock().unwrap() = windows;

    Ok(Self {
      pid: app.pid,
      app_windows,
      events_tx,
      _observer: observer,
      observer_source,
    })
  }

  fn register_app_notifications(
    app: &Application,
    observer: &CFRetained<AXObserver>,
    context: *mut ApplicationEventContext,
  ) -> crate::Result<()> {
    for notification in AX_APP_NOTIFICATIONS {
      unsafe {
        let notification_cfstr = CFString::from_static_str(notification);
        let result = observer.add_notification(
          app.ax_element.get_ref()?,
          &notification_cfstr,
          context.cast::<std::ffi::c_void>(),
        );

        if result != AXError::Success {
          return Err(crate::Error::Platform(format!(
            "Failed to add notification {} for PID {}: {:?}",
            notification, app.pid, result
          )));
        }
      }
    }

    Ok(())
  }

  fn register_window_notifications(
    window: &crate::NativeWindow,
    observer: &CFRetained<AXObserver>,
    context: *mut ApplicationEventContext,
  ) -> crate::Result<()> {
    let element = window.inner.element_clone()?;

    for notification in AX_WINDOW_NOTIFICATIONS {
      unsafe {
        let notification_cfstr = CFString::from_static_str(notification);
        let result = observer.add_notification(
          &element,
          &notification_cfstr,
          context.cast::<std::ffi::c_void>(),
        );

        // The element may already be registered (e.g. when rebinding a
        // window to an element that was previously observed). Treat this
        // as success so the remaining notifications still register.
        if result != AXError::Success
          && result != AXError::NotificationAlreadyRegistered
        {
          return Err(crate::Error::Platform(format!(
            "Failed to add notification {} for window {}: {:?}",
            notification,
            window.id().0,
            result
          )));
        }
      }
    }

    Ok(())
  }

  pub(crate) fn emit_all_windows_destroyed(&self) {
    for window in self.app_windows.lock().unwrap().iter() {
      Self::send_window_event(
        &self.events_tx,
        self.pid,
        WindowEvent::Destroyed {
          window_id: window.id(),
          notification: crate::WindowEventNotification(None),
        },
      );
    }
  }

  pub(crate) fn emit_all_windows_hidden(&self) {
    for window in self.app_windows.lock().unwrap().iter() {
      Self::send_window_event(
        &self.events_tx,
        self.pid,
        WindowEvent::Hidden {
          window: window.clone(),
          notification: crate::WindowEventNotification(None),
        },
      );
    }
  }

  pub(crate) fn emit_all_windows_shown(&self) {
    for window in self.app_windows.lock().unwrap().iter() {
      Self::send_window_event(
        &self.events_tx,
        self.pid,
        WindowEvent::Shown {
          window: window.clone(),
          notification: crate::WindowEventNotification(None),
        },
      );
    }
  }

  /// Sends a window event, logging on failure.
  fn send_window_event(
    events_tx: &mpsc::UnboundedSender<WindowEvent>,
    pid: ProcessId,
    event: WindowEvent,
  ) {
    if let Err(err) = events_tx.send(event) {
      tracing::warn!(
        "Failed to send window event for PID {}: {}",
        pid,
        err
      );
    }
  }

  /// Callback function for accessibility window events.
  #[allow(clippy::too_many_lines)]
  unsafe extern "C-unwind" fn window_event_callback(
    _observer: NonNull<AXObserver>,
    element: NonNull<AXUIElement>,
    notification_name: NonNull<CFString>,
    context: *mut std::ffi::c_void,
  ) {
    if context.is_null() {
      tracing::error!("Window event callback received null context.");
      return;
    }

    let context_ptr = context.cast::<ApplicationEventContext>();
    // SAFETY: The context is valid for the observer's lifetime and the
    // callback only runs on the event loop thread. Only a shared
    // reference is created, so no aliasing occurs even if the callback
    // is ever re-entered.
    let context = unsafe { &*context_ptr };
    let ax_element = unsafe { CFRetained::retain(element) };
    let notification = WindowEventNotificationInner {
      name: notification_name.as_ref().to_string(),
      ax_element_ptr: element.as_ptr().cast::<std::ffi::c_void>(),
    };

    tracing::debug!(
      "Received window event: {} for PID: {}",
      notification.name,
      context.application.pid
    );

    let found_window = {
      let app_windows = context.app_windows.lock().unwrap();

      app_windows
        .iter()
        .find(|window| window.inner.matches_element(&ax_element))
        .cloned()
    };

    if notification.name.as_str() == "AXUIElementDestroyed" {
      if let Some(window) = &found_window {
        Self::handle_element_destroyed(
          window,
          context,
          context_ptr,
          notification,
        );
      }

      return;
    }

    let Some(window) = found_window.or_else(|| {
      Self::resolve_untracked_element(
        ax_element,
        &notification,
        context,
        context_ptr,
      )
    }) else {
      return;
    };

    let window_event = match notification.name.as_str() {
      "AXFocusedWindowChanged" => WindowEvent::Focused {
        window,
        notification: crate::WindowEventNotification(Some(notification)),
      },
      "AXWindowMoved" | "AXWindowResized" => WindowEvent::MovedOrResized {
        window,
        is_interactive_start: false,
        is_interactive_end: false,
        notification: crate::WindowEventNotification(Some(notification)),
      },
      "AXWindowMiniaturized" => WindowEvent::Minimized {
        window,
        notification: crate::WindowEventNotification(Some(notification)),
      },
      "AXWindowDeminiaturized" => WindowEvent::MinimizeEnded {
        window,
        notification: crate::WindowEventNotification(Some(notification)),
      },
      "AXTitleChanged" => WindowEvent::TitleChanged {
        window,
        notification: crate::WindowEventNotification(Some(notification)),
      },
      _ => {
        tracing::debug!(
          "Unhandled window notification: {} for PID: {}",
          notification.name,
          context.application.pid
        );
        return;
      }
    };

    Self::send_window_event(
      &context.events_tx,
      context.application.pid,
      window_event,
    );
  }

  /// Resolves an untracked element to a window.
  ///
  /// An element that resolves to an already-tracked `WindowId` means the
  /// application swapped the element backing a live window (e.g. Finder
  /// on folder navigation): the tracked window is rebound in place
  /// instead of being tracked twice. Otherwise, the window is newly
  /// tracked, registered for notifications, and announced with `Shown`.
  ///
  /// Returns `None` if the element has no resolvable window ID.
  fn resolve_untracked_element(
    ax_element: CFRetained<AXUIElement>,
    notification: &WindowEventNotificationInner,
    context: &ApplicationEventContext,
    context_ptr: *mut ApplicationEventContext,
  ) -> Option<crate::NativeWindow> {
    let new_window =
      context.application.window_from_element(ax_element.clone())?;

    let tracked_window = {
      let mut app_windows = context.app_windows.lock().unwrap();

      let tracked_window = app_windows
        .iter()
        .find(|window| window.id() == new_window.id())
        .cloned();

      if tracked_window.is_none() {
        app_windows.push(new_window.clone());
      }

      tracked_window
    };

    if let Some(window) = tracked_window {
      Self::rebind_window_element(
        &window,
        ax_element,
        context,
        context_ptr,
      );

      return Some(window);
    }

    if let Err(err) = Self::register_window_notifications(
      &new_window,
      &context.observer,
      context_ptr,
    ) {
      tracing::warn!(
        "Failed to register window notifications for window {}: {}",
        new_window.id().0,
        err
      );
    }

    Self::send_window_event(
      &context.events_tx,
      context.application.pid,
      WindowEvent::Shown {
        window: new_window.clone(),
        notification: crate::WindowEventNotification(Some(
          notification.clone(),
        )),
      },
    );

    Some(new_window)
  }

  /// Handles `AXUIElementDestroyed` for a tracked window.
  ///
  /// The element may be destroyed while the window itself lives on under
  /// a new element (e.g. Finder swaps the backing element on folder
  /// navigation), so the window is re-resolved by its stable `WindowId`:
  ///
  /// - Still present: rebind to the new element. No `Destroyed` event.
  /// - Absent: the window is confirmed closed and `Destroyed` is emitted.
  /// - Query failed: keep the window. AX queries fail transiently during
  ///   sleep/wake, and application teardown is covered separately by the
  ///   workspace termination notification.
  fn handle_element_destroyed(
    window: &crate::NativeWindow,
    context: &ApplicationEventContext,
    context_ptr: *mut ApplicationEventContext,
    notification: WindowEventNotificationInner,
  ) {
    let window_id = window.id();

    let windows = match context.application.windows() {
      Ok(windows) => windows,
      Err(err) => {
        tracing::warn!(
          "Keeping window {} after failed window query for PID {}: {}",
          window_id.0,
          context.application.pid,
          err
        );
        return;
      }
    };

    let live_window =
      windows.into_iter().find(|window| window.id() == window_id);

    let Some(live_window) = live_window else {
      // Confirmed closed: the window query succeeded and the ID is gone.
      context
        .app_windows
        .lock()
        .unwrap()
        .retain(|window| window.id() != window_id);

      Self::send_window_event(
        &context.events_tx,
        context.application.pid,
        WindowEvent::Destroyed {
          window_id,
          notification: crate::WindowEventNotification(Some(
            notification,
          )),
        },
      );

      return;
    };

    match live_window.inner.element_clone() {
      Ok(element) => {
        Self::rebind_window_element(window, element, context, context_ptr);
      }
      Err(err) => tracing::warn!(
        "Failed to clone element for window {}: {}",
        window_id.0,
        err
      ),
    }
  }

  /// Rebinds a tracked window to a new backing element and re-registers
  /// its window notifications.
  ///
  /// The rebind is shared via the window's `Arc`, so the window
  /// manager's copy updates too.
  fn rebind_window_element(
    window: &crate::NativeWindow,
    element: CFRetained<AXUIElement>,
    context: &ApplicationEventContext,
    context_ptr: *mut ApplicationEventContext,
  ) {
    if let Err(err) = window.inner.set_element(element) {
      tracing::warn!(
        "Failed to rebind element for window {}: {}",
        window.id().0,
        err
      );
      return;
    }

    tracing::info!(
      "Rebound element for window {} of PID {}.",
      window.id().0,
      context.application.pid
    );

    if let Err(err) = Self::register_window_notifications(
      window,
      &context.observer,
      context_ptr,
    ) {
      tracing::warn!(
        "Failed to re-register notifications for window {}: {}",
        window.id().0,
        err
      );
    }
  }
}

impl Drop for ApplicationObserver {
  fn drop(&mut self) {
    // Invalidate the runloop source. This is thread-safe and is OK to call
    // after the run loop is stopped.
    self.observer_source.invalidate();
  }
}
