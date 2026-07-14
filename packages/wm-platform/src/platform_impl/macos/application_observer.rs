use std::{
  cell::RefCell,
  ptr::NonNull,
  sync::{Arc, Mutex},
};

use objc2_application_services::{AXError, AXObserver, AXUIElement};
use objc2_core_foundation::{
  kCFRunLoopDefaultMode, CFRetained, CFRunLoop, CFRunLoopSource, CFString,
};
use tokio::sync::mpsc;

use crate::{
  platform_impl::{
    Application, NativeWindow, ProcessId, WindowEventNotificationInner,
  },
  Dispatcher, NativeWindowExtMacOs, ThreadBound, WindowEvent, WindowId,
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

  /// Pointer to the [`ApplicationEventContext`] leaked via
  /// `Box::into_raw` in [`ApplicationObserver::new`]. Reclaimed on drop.
  context_ptr: *mut ApplicationEventContext,

  /// Dispatcher for reclaiming `context_ptr` on the event-loop thread.
  dispatcher: Dispatcher,
}

// TODO: Remove this.
unsafe impl Send for ApplicationObserver {}

impl ApplicationObserver {
  /// Creates a new `ApplicationObserver` for the given application.
  ///
  /// Registers notifications and emits `WindowEvent::Shown` for existing
  /// windows.
  ///
  /// Window enumeration failure is tolerated so future windows are still
  /// observed.
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

    // Seed after notification registration to avoid startup races.
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

    // Re-scan after app-level notifications are registered.
    let windows = app.windows().unwrap_or_default();

    // `Shown` is idempotent and adopts windows missed during startup.
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

      if let Err(err) = events_tx.send(WindowEvent::Shown {
        window: window.clone(),
        notification: crate::WindowEventNotification(None),
      }) {
        tracing::warn!(
          "Failed to send window event for PID {}: {}",
          app.pid,
          err
        );
      }
    }

    *app_windows.lock().unwrap() = windows;

    Ok(Self {
      pid: app.pid,
      app_windows,
      events_tx,
      _observer: observer,
      observer_source,
      context_ptr: context,
      dispatcher: app.dispatcher.clone(),
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
    let element_cell = window.ax_ui_element().get_ref()?;
    let element = element_cell.borrow();

    for notification in AX_WINDOW_NOTIFICATIONS {
      unsafe {
        let notification_cfstr = CFString::from_static_str(notification);
        let result = observer.add_notification(
          &element,
          &notification_cfstr,
          context.cast::<std::ffi::c_void>(),
        );

        if result != AXError::Success {
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
      if let Err(err) = self.events_tx.send(WindowEvent::Destroyed {
        window_id: window.id(),
        notification: crate::WindowEventNotification(None),
      }) {
        tracing::warn!(
          "Failed to send window event for PID {}: {}",
          self.pid,
          err
        );
      }
    }
  }

  pub(crate) fn emit_all_windows_hidden(&self) {
    for window in self.app_windows.lock().unwrap().iter() {
      if let Err(err) = self.events_tx.send(WindowEvent::Hidden {
        window: window.clone(),
        notification: crate::WindowEventNotification(None),
      }) {
        tracing::warn!(
          "Failed to send window event for PID {}: {}",
          self.pid,
          err
        );
      }
    }
  }

  pub(crate) fn emit_all_windows_shown(&self) {
    for window in self.app_windows.lock().unwrap().iter() {
      if let Err(err) = self.events_tx.send(WindowEvent::Shown {
        window: window.clone(),
        notification: crate::WindowEventNotification(None),
      }) {
        tracing::warn!(
          "Failed to send window event for PID {}: {}",
          self.pid,
          err
        );
      }
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

    let context = &mut *context.cast::<ApplicationEventContext>();
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
        // Some apps swap the `AXUIElement` backing a live window. Confirm
        // the stable `WindowId` is gone before emitting `Destroyed`.
        let live_element = match context.application.windows() {
          Ok(windows) => windows
            .into_iter()
            .find(|w| w.id() == window.id())
            .and_then(|w| w.inner.element_clone().ok()),
          Err(err) => {
            tracing::warn!(
              "Failed to re-query windows for PID {}: {}",
              context.application.pid,
              err
            );
            return;
          }
        };

        if let Some(live_element) = live_element {
          // Rebind the shared element and keep observing the live window.
          if let Err(err) = window.inner.set_element(live_element) {
            tracing::warn!(
              "Failed to refresh window element for PID {}: {}",
              context.application.pid,
              err
            );
            return;
          }

          if let Err(err) = Self::register_window_notifications(
            window,
            &context.observer.clone(),
            context,
          ) {
            tracing::warn!(
              "Failed to register refreshed window notifications for PID {}: {}",
              context.application.pid,
              err
            );
          }
        } else {
          context
            .app_windows
            .lock()
            .unwrap()
            .retain(|w| w.id() != window.id());

          if let Err(err) =
            context.events_tx.send(WindowEvent::Destroyed {
              window_id: window.id(),
              notification: crate::WindowEventNotification(Some(
                notification,
              )),
            })
          {
            tracing::warn!(
              "Failed to send window event for PID {}: {}",
              context.application.pid,
              err
            );
          }
        }
      }

      return;
    }

    let is_new_window = found_window.is_none();
    let window = if let Some(window) = found_window {
      window
    } else {
      // Ignore unresolved elements; `WindowId(0)` would collide.
      let Some(window_id) = WindowId::from_window_element(&ax_element)
      else {
        return;
      };
      let ax_element = ThreadBound::new(
        RefCell::new(ax_element),
        context.application.dispatcher.clone(),
      );
      NativeWindow::new(window_id, ax_element, context.application.clone())
        .into()
    };

    if is_new_window {
      context.app_windows.lock().unwrap().push(window.clone());
      if let Err(err) = Self::register_window_notifications(
        &window,
        &context.observer.clone(),
        context,
      ) {
        tracing::warn!(
          "Failed to register window notifications for PID {}: {}",
          context.application.pid,
          err
        );
      }

      if let Err(err) = context.events_tx.send(WindowEvent::Shown {
        window: window.clone(),
        notification: crate::WindowEventNotification(Some(
          notification.clone(),
        )),
      }) {
        tracing::warn!(
          "Failed to send window event for PID {}: {}",
          context.application.pid,
          err
        );
      }
    }

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

    if let Err(err) = context.events_tx.send(window_event) {
      tracing::warn!(
        "Failed to send window event for PID {}: {}",
        context.application.pid,
        err
      );
    }
  }
}

impl Drop for ApplicationObserver {
  fn drop(&mut self) {
    // Invalidate the source and reclaim the context on the event-loop
    // thread. The observer callback runs on that same thread, so this
    // serializes with any in-flight callback — no callback can be
    // executing while the context is freed.
    //
    // Ownership is moved into the closure as raw addresses (an extra
    // retain on the source, plus the context box), since `CFRetained`
    // and raw pointers are not `Send`. `dispatch_async` either runs the
    // closure (now or later) or fails without ever enqueueing it — there
    // is no ambiguous timeout state, unlike `dispatch_sync`.
    let source_addr =
      CFRetained::into_raw(self.observer_source.clone()).as_ptr() as usize;
    let context_addr = self.context_ptr as usize;

    let result = self.dispatcher.dispatch_async(move || {
      // SAFETY: `source_addr` came from `CFRetained::into_raw` above;
      // ownership of that retain is reconstructed exactly once here.
      let source = unsafe {
        CFRetained::from_raw(NonNull::new_unchecked(
          source_addr as *mut CFRunLoopSource,
        ))
      };

      // Invalidate the runloop source to stop further callbacks from
      // being dispatched. Any callback already dispatched has completed,
      // since this closure runs on the same thread.
      source.invalidate();

      // Reclaim the context box. Dropping it releases the retained
      // `AXObserver` clone, which detaches the observer's notifications
      // and runloop source.
      // SAFETY: `context_addr` came from `Box::into_raw` in `new` and is
      // reclaimed exactly once.
      drop(unsafe {
        Box::from_raw(context_addr as *mut ApplicationEventContext)
      });
    });

    // The closure was never enqueued, which means the event loop has
    // stopped and no callback can be running — safe to clean up on the
    // current thread instead.
    if result.is_err() {
      // SAFETY: Reconstructs the retain and box that the unenqueued
      // closure never consumed (its captures are plain integers).
      let source = unsafe {
        CFRetained::from_raw(NonNull::new_unchecked(
          source_addr as *mut CFRunLoopSource,
        ))
      };
      source.invalidate();

      // SAFETY: See above; reclaimed exactly once.
      drop(unsafe { Box::from_raw(self.context_ptr) });
    }
  }
}
