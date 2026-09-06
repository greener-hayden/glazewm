use std::{
  cell::{Cell, RefCell},
  collections::HashSet,
  sync::{Arc, OnceLock},
};

use tokio::sync::mpsc;
use windows::Win32::{
  Foundation::{BOOL, HWND, LPARAM},
  UI::{
    Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK},
    WindowsAndMessaging::{
      EnumWindows, EVENT_OBJECT_CLOAKED, EVENT_OBJECT_CREATE,
      EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE,
      EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_NAMECHANGE,
      EVENT_OBJECT_SHOW, EVENT_OBJECT_UNCLOAKED, EVENT_SYSTEM_FOREGROUND,
      EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART,
      EVENT_SYSTEM_MOVESIZEEND, EVENT_SYSTEM_MOVESIZESTART, OBJID_WINDOW,
      WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WS_CAPTION,
      WS_CHILD, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    },
  },
};

use super::NativeWindow;
use crate::{Dispatcher, WindowEvent, WindowId};

thread_local! {
  /// Sender for window events. For use with hook procedure.
  static EVENT_TX: OnceLock<mpsc::UnboundedSender<WindowEvent>> = const { OnceLock::new() };
  static SEEN: RefCell<HashSet<WindowId>> = RefCell::new(HashSet::new());
  static CONCEALING: Cell<bool> = const { Cell::new(false) };
}

/// Platform-specific implementation of [`WindowEventNotification`].
#[derive(Clone, Debug)]
pub struct WindowEventNotificationInner {
  _opening: Arc<crate::opening_windows::OpeningGuard>,
}

/// Platform-specific implementation of [`WindowListener`].
#[derive(Debug)]
pub(crate) struct WindowListener {
  hook_handles: Vec<HWINEVENTHOOK>,
}

impl WindowListener {
  /// Implements [`WindowListener::new`].
  pub(crate) fn new(
    event_tx: mpsc::UnboundedSender<WindowEvent>,
    dispatcher: &Dispatcher,
  ) -> crate::Result<Self> {
    let hook_handles = dispatcher.dispatch_sync(move || {
      // Existing and ignored windows must not fade again on a later SHOW.
      unsafe extern "system" fn remember(hwnd: HWND, _: LPARAM) -> BOOL {
        SEEN.with(|seen| seen.borrow_mut().insert(WindowId(hwnd.0)));
        BOOL(1)
      }
      EVENT_TX.with(|lock| lock.set(event_tx)).map_err(|_| {
        crate::Error::Platform(
          "Window event sender already set.".to_string(),
        )
      })?;

      // SAFETY: Synchronous enumeration calls a callback with no raw
      // state.
      unsafe { EnumWindows(Some(remember), LPARAM(0)) }?;
      Self::hook_win_events()
    })??;

    Ok(Self { hook_handles })
  }

  /// Implements [`WindowListener::terminate`].
  pub(crate) fn terminate(&mut self) {
    crate::opening_windows::set_enabled(false);
    for handle in self.hook_handles.drain(..) {
      let _ = unsafe { UnhookWinEvent(handle) };
    }
  }

  /// Creates several window event hooks via `SetWinEventHook`.
  ///
  /// Separate hooks are created per event range, which is more performant
  /// than a single hook covering all events.
  fn hook_win_events() -> crate::Result<Vec<HWINEVENTHOOK>> {
    let event_ranges = [
      (EVENT_OBJECT_CREATE, EVENT_OBJECT_HIDE),
      (EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND),
      (EVENT_SYSTEM_MOVESIZESTART, EVENT_SYSTEM_MOVESIZEEND),
      (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
      (EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_NAMECHANGE),
      (EVENT_OBJECT_CLOAKED, EVENT_OBJECT_UNCLOAKED),
    ];

    event_ranges
      .iter()
      .try_fold(Vec::new(), |mut handles, (min, max)| {
        // Create a window hook for the event range.
        let hook_handle = unsafe {
          SetWinEventHook(
            *min,
            *max,
            None,
            Some(Self::window_event_proc),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
          )
        };

        if hook_handle.is_invalid() {
          return Err(crate::Error::Platform(
            "Failed to set window event hook.".to_string(),
          ));
        }

        handles.push(hook_handle);
        Ok(handles)
      })
  }

  /// Acquires and forwards provisional ownership at lifecycle boundaries.
  fn opening_notification(
    event_type: u32,
    handle: HWND,
  ) -> crate::WindowEventNotification {
    let id = WindowId(handle.0);
    let mut notification = crate::WindowEventNotification(None);
    if event_type == EVENT_OBJECT_DESTROY {
      SEEN.with(|seen| seen.borrow_mut().remove(&id));
      crate::opening_windows::forget_window(id);
    } else if matches!(event_type, EVENT_OBJECT_CREATE | EVENT_OBJECT_SHOW)
    {
      let seen = SEEN.with(|seen| seen.borrow().contains(&id));
      if !seen
        && crate::opening_windows::enabled()
        && !CONCEALING.with(Cell::get)
      {
        // Shell COM calls can dispatch nested WinEvents. Forward those
        // events without recursively borrowing the shell connection.
        CONCEALING.with(|busy| busy.set(true));
        let window = NativeWindow::new(handle.0);
        // Only ordinary top-level application windows are candidates.
        // Rules still decide management; rejected candidates are restored
        // when their notification is released after the batch.
        if window.has_window_style(WS_CAPTION)
          && !window.has_window_style(WS_CHILD)
          && !window
            .has_window_style_ex(WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW)
          && !window.is_minimized().unwrap_or(true)
          && !window.is_maximized().unwrap_or(true)
        {
          if event_type == EVENT_OBJECT_CREATE {
            crate::opening_windows::reserve(window.into());
          } else if let Some(guard) =
            crate::opening_windows::conceal(window.into())
          {
            notification.0 =
              Some(WindowEventNotificationInner { _opening: guard });
          }
        }
        CONCEALING.with(|busy| busy.set(false));
      }
      if event_type == EVENT_OBJECT_SHOW {
        SEEN.with(|seen| seen.borrow_mut().insert(id));
        if notification.0.is_none() {
          notification.0 = crate::opening_windows::guard(id)
            .map(|guard| WindowEventNotificationInner { _opening: guard });
        }
      }
    }

    notification
  }

  /// Callback passed to `SetWinEventHook`.
  ///
  /// This function is called on selected window events, and forwards them
  /// through an MPSC channel.
  extern "system" fn window_event_proc(
    _hook: HWINEVENTHOOK,
    event_type: u32,
    handle: HWND,
    id_object: i32,
    id_child: i32,
    _event_thread: u32,
    _event_time: u32,
  ) {
    // Check whether the event is associated with a window object rather
    // than a UI control.
    let is_window_event =
      id_object == OBJID_WINDOW.0 && id_child == 0 && handle != HWND(0);

    if !is_window_event {
      return;
    }

    let Some(event_tx) = EVENT_TX.with(|lock| lock.get().cloned()) else {
      return;
    };

    let notification = Self::opening_notification(event_type, handle);

    let event = match event_type {
      EVENT_OBJECT_DESTROY => WindowEvent::Destroyed {
        window_id: WindowId(handle.0),
        notification,
      },
      EVENT_SYSTEM_FOREGROUND => WindowEvent::Focused {
        window: NativeWindow::new(handle.0).into(),
        notification,
      },
      EVENT_OBJECT_HIDE | EVENT_OBJECT_CLOAKED => WindowEvent::Hidden {
        window: NativeWindow::new(handle.0).into(),
        notification,
      },
      EVENT_OBJECT_LOCATIONCHANGE => WindowEvent::MovedOrResized {
        window: NativeWindow::new(handle.0).into(),
        is_interactive_start: false,
        is_interactive_end: false,
        notification,
      },
      EVENT_SYSTEM_MINIMIZESTART => WindowEvent::Minimized {
        window: NativeWindow::new(handle.0).into(),
        notification,
      },
      EVENT_SYSTEM_MINIMIZEEND => WindowEvent::MinimizeEnded {
        window: NativeWindow::new(handle.0).into(),
        notification,
      },
      EVENT_SYSTEM_MOVESIZESTART => WindowEvent::MovedOrResized {
        window: NativeWindow::new(handle.0).into(),
        is_interactive_start: true,
        is_interactive_end: false,
        notification,
      },
      EVENT_SYSTEM_MOVESIZEEND => WindowEvent::MovedOrResized {
        window: NativeWindow::new(handle.0).into(),
        is_interactive_start: false,
        is_interactive_end: true,
        notification,
      },
      EVENT_OBJECT_SHOW | EVENT_OBJECT_UNCLOAKED => WindowEvent::Shown {
        window: NativeWindow::new(handle.0).into(),
        notification,
      },
      EVENT_OBJECT_NAMECHANGE => WindowEvent::TitleChanged {
        window: NativeWindow::new(handle.0).into(),
        notification,
      },
      _ => return,
    };

    if let Err(err) = event_tx.send(event) {
      tracing::warn!("Failed to send window event: {}.", err);
    }
  }
}

impl Drop for WindowListener {
  fn drop(&mut self) {
    self.terminate();
  }
}
