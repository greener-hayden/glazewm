use std::{
  collections::HashMap,
  time::{Duration, Instant},
};

use objc2::rc::Retained;
use objc2_app_kit::{
  NSApplicationActivationPolicy, NSRunningApplication, NSWorkspace,
};
use objc2_application_services::{AXError, AXUIElement};
use objc2_core_foundation::CFString;
use tokio::sync::mpsc;

use crate::{
  platform_impl::{
    self, is_stall, AXUIElementExt, Application, ApplicationObserver,
    NotificationCenter, NotificationEvent, NotificationName,
    NotificationObserver, ProcessId,
  },
  Dispatcher, ThreadBound, WindowEvent,
};

/// Longest wait for an application to answer its first accessibility
/// request. A cold launch measured about 610ms (`AnyDesk`).
const READY_TIMEOUT: Duration = Duration::from_secs(10);

/// Timeout for each readiness request.
///
/// Short enough that termination is noticed promptly; each request only
/// blocks the readiness thread.
const READY_POLL_SECS: f32 = 1.0;

/// Pause after a readiness request fails before its timeout.
const READY_BACKOFF: Duration = Duration::from_millis(50);

/// Observation attempts before an application that keeps stalling is left
/// unobserved.
const MAX_OBSERVE_ATTEMPTS: u8 = 3;

/// Platform-specific implementation of [`WindowEventNotification`].
#[derive(Clone, Debug)]
pub struct WindowEventNotificationInner {
  /// Name of the notification (e.g. `AXWindowMoved`).
  pub name: String,

  /// Pointer to the `AXUIElement` that triggered the notification.
  pub ax_element_ptr: *mut std::ffi::c_void,
}

unsafe impl Send for WindowEventNotificationInner {}

/// Platform-specific implementation of [`WindowListener`].
#[derive(Debug)]
pub(crate) struct WindowListener {
  /// Workspace notification observer, bound to the main thread.
  observer: Option<ThreadBound<Retained<NotificationObserver>>>,
}

impl WindowListener {
  /// Implements [`WindowListener::new`].
  pub(crate) fn new(
    events_tx: mpsc::UnboundedSender<WindowEvent>,
    dispatcher: &Dispatcher,
  ) -> crate::Result<Self> {
    let observer = dispatcher
      .dispatch_sync(|| Self::init(events_tx, dispatcher.clone()))??;

    Ok(Self {
      observer: Some(observer),
    })
  }

  /// Implements [`WindowListener::terminate`].
  pub(crate) fn terminate(&mut self) {
    // On macOS 10.11+, observer subscriptions are cleaned up automatically
    // without calling `removeObserver`.
    // Ref: https://developer.apple.com/documentation/foundation/notificationcenter/removeobserver(_:name:object:)
    //
    // Dropping the `NotificationObserver` also drops its channel sender,
    // causing the listener thread to exit.
    self.observer.take();
  }

  fn init(
    events_tx: mpsc::UnboundedSender<WindowEvent>,
    dispatcher: Dispatcher,
  ) -> crate::Result<ThreadBound<Retained<NotificationObserver>>> {
    let (observer, events_rx) = NotificationObserver::new();

    // Weak, so dropping the observer in `Self::terminate` still closes the
    // channel and ends the listener thread.
    let ready_tx = observer.events_tx().downgrade();

    let workspace = NSWorkspace::sharedWorkspace();
    let mut workspace_center = NotificationCenter::workspace_center();

    for notification in [
      NotificationName::WorkspaceActiveSpaceDidChange,
      NotificationName::WorkspaceDidLaunchApplication,
      NotificationName::WorkspaceDidActivateApplication,
      NotificationName::WorkspaceDidTerminateApplication,
      NotificationName::WorkspaceDidHideApplication,
      NotificationName::WorkspaceDidUnhideApplication,
    ] {
      unsafe {
        workspace_center.add_observer(
          notification,
          &observer,
          Some(&workspace),
        );
      }
    }

    let running_apps = platform_impl::all_applications(&dispatcher)?;

    // Create observers for all running applications. Those that stall
    // are observed once they answer.
    let app_observers = running_apps
      .into_iter()
      .filter_map(|app| match Self::observe(&app, &events_tx) {
        Observation::Observed(app_observer) => Some(app_observer),
        Observation::Stalled => {
          Self::await_ready(app.ns_app.clone(), 1, ready_tx.clone());
          None
        }
        Observation::Ignored | Observation::Failed(_) => None,
      })
      .collect::<Vec<_>>();

    tracing::info!(
      "Registered observers for {} existing applications.",
      app_observers.len()
    );

    let dispatcher_clone = dispatcher.clone();
    std::thread::spawn(move || {
      Self::listen_workspace_events(
        app_observers,
        events_rx,
        &events_tx,
        &ready_tx,
        &dispatcher_clone,
      );
    });

    Ok(ThreadBound::new(observer, dispatcher))
  }

  fn listen_workspace_events(
    app_observers: Vec<ApplicationObserver>,
    mut events_rx: mpsc::UnboundedReceiver<NotificationEvent>,
    events_tx: &mpsc::UnboundedSender<WindowEvent>,
    ready_tx: &mpsc::WeakUnboundedSender<NotificationEvent>,
    dispatcher: &Dispatcher,
  ) {
    // Track window observers for each application by PID.
    let mut app_observers: HashMap<ProcessId, ApplicationObserver> =
      app_observers
        .into_iter()
        .map(|observer| (observer.pid, observer))
        .collect();

    // Loop exits when the sender is dropped in `Self::terminate`.
    while let Some(event) = events_rx.blocking_recv() {
      tracing::debug!("Received workspace event: {event:?}");

      match event {
        // A launching application can take far longer than the event
        // loop's accessibility timeout to answer. Wait for it off the
        // event loop thread, which carries every animation.
        NotificationEvent::WorkspaceDidLaunchApplication(running_app) => {
          Self::await_ready(running_app, 1, ready_tx.clone());
        }
        NotificationEvent::ApplicationReady { app, attempt } => {
          let pid = app.processIdentifier();

          if app_observers.contains_key(&pid) {
            tracing::debug!("Observer already exists for PID {pid}.");
            continue;
          }

          let started = std::time::Instant::now();
          let observation = dispatcher.dispatch_sync(|| {
            let app = Application::new(app.clone(), dispatcher.clone());
            Self::observe(&app, events_tx)
          });

          match observation {
            Ok(Observation::Observed(app_observer)) => {
              tracing::debug!(
                "Observed PID {pid} in {}us on the event loop thread.",
                started.elapsed().as_micros()
              );
              app_observers.insert(pid, app_observer);
            }
            Ok(Observation::Stalled) if attempt < MAX_OBSERVE_ATTEMPTS => {
              Self::await_ready(app, attempt + 1, ready_tx.clone());
            }
            Ok(Observation::Stalled) => {
              tracing::warn!(
                "PID {pid} stalled on {attempt} observation attempts; \
                 its windows are not managed."
              );
            }
            Ok(Observation::Ignored) => {}
            Ok(Observation::Failed(err)) => {
              tracing::warn!("Failed to observe PID {pid}: {err}");
            }
            Err(err) => {
              tracing::warn!("Failed to observe PID {pid}: {err}");
            }
          }
        }
        NotificationEvent::WorkspaceDidTerminateApplication(
          running_app,
        ) => {
          let pid = running_app.processIdentifier();

          if let Some(observer) = app_observers.remove(&pid) {
            tracing::info!(
              "Removed window observer for terminated PID: {}",
              pid
            );

            observer.emit_all_windows_destroyed();
          }
        }
        NotificationEvent::WorkspaceDidActivateApplication(
          running_app,
        ) => {
          let Ok(Ok(Some(focused_window))) =
            dispatcher.dispatch_sync(|| {
              let app = Application::new(running_app, dispatcher.clone());
              app.focused_window()
            })
          else {
            continue;
          };

          let _ = events_tx.send(WindowEvent::Focused {
            window: focused_window,
            notification: crate::WindowEventNotification(None),
          });
        }
        NotificationEvent::WorkspaceDidHideApplication(running_app) => {
          if let Some(app_observer) =
            app_observers.get(&running_app.processIdentifier())
          {
            app_observer.emit_all_windows_hidden();
          }
        }
        NotificationEvent::WorkspaceDidUnhideApplication(running_app) => {
          if let Some(app_observer) =
            app_observers.get(&running_app.processIdentifier())
          {
            app_observer.emit_all_windows_shown();
          }
        }
        _ => {}
      }
    }

    tracing::debug!("Window listener thread exited.");
  }

  /// Observes an application's windows.
  ///
  /// Must be called on the event loop thread.
  fn observe(
    app: &Application,
    events_tx: &mpsc::UnboundedSender<WindowEvent>,
  ) -> Observation {
    if !app.should_observe() {
      tracing::debug!(
        "Skipped observer registration for PID {} (should ignore).",
        app.pid
      );
      return Observation::Ignored;
    }

    match ApplicationObserver::new(app, events_tx.clone()) {
      Ok(app_observer) => Observation::Observed(app_observer),
      Err(err) if is_stall(&err) => {
        tracing::debug!(
          "PID {} stalled during observation: {err}",
          app.pid
        );
        Observation::Stalled
      }
      Err(err) => {
        tracing::debug!(
          "Skipped observer registration for PID {}: {}",
          app.pid,
          err
        );
        Observation::Failed(err)
      }
    }
  }

  /// Waits on a new thread until an application answers accessibility
  /// requests, then queues `NotificationEvent::ApplicationReady`.
  ///
  /// The wait runs under its own element timeout, so neither the event
  /// loop thread nor the listener thread blocks on a busy application.
  /// Gives up silently once the application terminates, and with a warning
  /// after `READY_TIMEOUT`.
  fn await_ready(
    app: Retained<NSRunningApplication>,
    attempt: u8,
    ready_tx: mpsc::WeakUnboundedSender<NotificationEvent>,
  ) {
    let spawned = std::thread::Builder::new()
      .name("glazewm-app-ready".to_string())
      .spawn(move || {
        let pid = app.processIdentifier();

        // SAFETY: Creating an application element has no preconditions;
        // an invalid PID only makes later requests fail.
        let element = unsafe { AXUIElement::new_application(pid) };

        // SAFETY: `element` is a valid application element. The timeout
        // applies to this element alone, not to the event loop's.
        unsafe { element.set_messaging_timeout(READY_POLL_SECS) };

        let deadline = Instant::now() + READY_TIMEOUT;

        loop {
          if app.isTerminated() {
            return;
          }

          // `AXRole` is served by the application's main thread, as are
          // notification registrations, so an answer means it is ready.
          match element.get_attribute::<CFString>("AXRole") {
            Err(err) if is_stall(&err) => {
              if Instant::now() >= deadline {
                // Agents (e.g. Universal Control) may never answer, and
                // have no windows to manage.
                if app.activationPolicy()
                  == NSApplicationActivationPolicy::Regular
                {
                  tracing::warn!(
                    "PID {pid} did not answer accessibility requests \
                     within {READY_TIMEOUT:?}; its windows are not managed."
                  );
                } else {
                  tracing::debug!(
                    "Agent PID {pid} did not answer accessibility requests \
                     within {READY_TIMEOUT:?}."
                  );
                }
                return;
              }

              // A process that is exiting can fail at once rather than
              // after the timeout; don't spin on it.
              std::thread::sleep(READY_BACKOFF);
            }
            Err(crate::Error::Accessibility(_, code))
              if code == AXError::InvalidUIElement.0 =>
            {
              return;
            }
            _ => break,
          }
        }

        if let Some(ready_tx) = ready_tx.upgrade() {
          let _ = ready_tx
            .send(NotificationEvent::ApplicationReady { app, attempt });
        }
      });

    if let Err(err) = spawned {
      tracing::warn!(
        "Failed to spawn application readiness thread: {err}"
      );
    }
  }
}

/// Outcome of trying to observe an application.
enum Observation {
  Observed(ApplicationObserver),
  /// Not an application whose windows are managed.
  Ignored,
  /// The application did not answer in time; see [`is_stall`].
  Stalled,
  Failed(crate::Error),
}

impl Drop for WindowListener {
  fn drop(&mut self) {
    self.terminate();
  }
}
