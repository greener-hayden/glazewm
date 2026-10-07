use std::{
  sync::{Arc, Mutex},
  time::{Duration, Instant},
};

use tokio::sync::mpsc;
use windows::Win32::{
  Devices::HumanInterfaceDevice::{
    HID_USAGE_GENERIC_MOUSE, HID_USAGE_PAGE_GENERIC,
  },
  Foundation::{HWND, POINT},
  UI::{
    Input::{
      GetRawInputData, RegisterRawInputDevices, HRAWINPUT, RAWINPUT,
      RAWINPUTDEVICE, RAWINPUTHEADER, RIDEV_INPUTSINK, RIDEV_REMOVE,
      RID_INPUT, RIM_TYPEMOUSE,
    },
    WindowsAndMessaging::{
      GetCursorPos, RI_MOUSE_LEFT_BUTTON_DOWN, RI_MOUSE_LEFT_BUTTON_UP,
      RI_MOUSE_RIGHT_BUTTON_DOWN, RI_MOUSE_RIGHT_BUTTON_UP, WM_INPUT,
    },
  },
};

use super::FOREGROUND_INPUT_IDENTIFIER;
use crate::{
  mouse_listener::MouseEventKind,
  platform_event::{MouseButton, MouseEvent, PressedButtons},
  Dispatcher, DispatcherExtWindows, Point,
};

/// Data shared with the window procedure callback.
struct CallbackData {
  event_tx: mpsc::UnboundedSender<MouseEvent>,

  /// Pressed button state tracked from events.
  pressed: PressedButtons,

  /// Timestamp of the last emitted `Move` event for throttling.
  last_move_emission: Option<Instant>,
}

#[cfg(test)]
#[path = "mouse_listener_tests.rs"]
mod tests;

/// Pure state machine deciding when raw mouse input is registered.
///
/// Registration delivers a `WM_INPUT` message for every mouse packet to
/// the event loop, so it is held only while the listener is enabled, has
/// at least one event kind with a consumer, and is not terminated.
struct RawInputPolicy {
  /// Event kinds the listener is configured for. Empty once terminated.
  events: Arc<[MouseEventKind]>,

  /// Whether the caller wants events (cleared while the WM is paused).
  enabled: bool,

  /// Whether the raw input device is currently registered.
  registered: bool,

  /// Whether the listener has been terminated for good.
  terminated: bool,
}

impl RawInputPolicy {
  /// Creates an enabled policy with nothing registered yet.
  fn new(events: Arc<[MouseEventKind]>) -> Self {
    Self {
      events,
      enabled: true,
      registered: false,
      terminated: false,
    }
  }

  /// Whether raw input should be registered right now.
  fn wants_raw_input(&self) -> bool {
    self.enabled && !self.terminated && !self.events.is_empty()
  }

  /// Returns the registration change needed to reach the wanted state.
  ///
  /// `None` means the device is already in the wanted state, so no native
  /// call is needed.
  fn pending_change(&self) -> Option<bool> {
    let wanted = self.wants_raw_input();
    (wanted != self.registered).then_some(wanted)
  }

  /// Records that the device is now (un)registered.
  fn mark_registered(&mut self, registered: bool) {
    self.registered = registered;
  }

  /// Sets whether the caller wants events. Ignored once terminated.
  fn set_enabled(&mut self, enabled: bool) {
    if !self.terminated {
      self.enabled = enabled;
    }
  }

  /// Whether `events` differ from the current set and may be applied.
  ///
  /// Always `false` once terminated.
  fn accepts_events(&self, events: &[MouseEventKind]) -> bool {
    !self.terminated && *self.events != *events
  }

  /// Replaces the configured events. Ignored once terminated.
  fn set_events(&mut self, events: Arc<[MouseEventKind]>) {
    if !self.terminated {
      self.events = events;
    }
  }

  /// Terminates for good: no events and no re-enabling.
  fn terminate(&mut self) {
    self.terminated = true;
    self.events = Arc::from([]);
  }
}

/// Platform-specific implementation of [`MouseListener`].
pub(crate) struct MouseListener {
  callback_id: Option<usize>,
  callback_data: Arc<Mutex<CallbackData>>,
  dispatcher: Dispatcher,

  /// Decides whether raw input is registered.
  raw_input: RawInputPolicy,
}

impl MouseListener {
  /// Implements [`MouseListener::new`].
  ///
  /// With no enabled events, neither a callback nor raw input is
  /// registered.
  pub(crate) fn new(
    enabled_events: &[MouseEventKind],
    event_tx: mpsc::UnboundedSender<MouseEvent>,
    dispatcher: &Dispatcher,
  ) -> crate::Result<Self> {
    let callback_data = Arc::new(Mutex::new(CallbackData {
      event_tx,
      pressed: PressedButtons::default(),
      last_move_emission: None,
    }));

    let enabled_events: Arc<[MouseEventKind]> = Arc::from(enabled_events);
    let callback_id = Self::register_callback(
      &enabled_events,
      &callback_data,
      dispatcher,
    )?;

    let mut listener = Self {
      callback_id,
      dispatcher: dispatcher.clone(),
      callback_data,
      raw_input: RawInputPolicy::new(enabled_events),
    };

    // On failure, dropping the listener removes the callback again.
    listener.sync_raw_input()?;

    Ok(listener)
  }

  /// Implements [`MouseListener::enable`].
  ///
  /// Enabling a paused listener registers raw input only if there are
  /// events to listen for. Does nothing once terminated.
  pub(crate) fn enable(&mut self, enabled: bool) -> crate::Result<()> {
    self.raw_input.set_enabled(enabled);
    self.sync_raw_input()
  }

  /// Implements [`MouseListener::set_enabled_events`].
  ///
  /// Keeps the current registration when the events are unchanged, and
  /// never re-enables raw input while the listener is disabled. Does
  /// nothing once terminated.
  pub(crate) fn set_enabled_events(
    &mut self,
    enabled_events: &[MouseEventKind],
  ) -> crate::Result<()> {
    if !self.raw_input.accepts_events(enabled_events) {
      return Ok(());
    }

    let enabled_events: Arc<[MouseEventKind]> = Arc::from(enabled_events);

    // Register the replacement first, so that a failure leaves the
    // previous registration intact.
    let callback_id = Self::register_callback(
      &enabled_events,
      &self.callback_data,
      &self.dispatcher,
    )?;

    let previous_id =
      std::mem::replace(&mut self.callback_id, callback_id);
    self.raw_input.set_events(enabled_events);

    let deregistered = previous_id.map_or(Ok(()), |id| {
      self.dispatcher.deregister_wndproc_callback(id)
    });

    self.sync_raw_input()?;
    deregistered
  }

  /// Implements [`MouseListener::terminate`].
  ///
  /// Safe to call repeatedly. After this, [`Self::enable`] and
  /// [`Self::set_enabled_events`] do nothing, so nothing is registered
  /// again.
  pub(crate) fn terminate(&mut self) -> crate::Result<()> {
    self.raw_input.terminate();
    let removed = self.sync_raw_input();

    let deregistered = self.callback_id.take().map_or(Ok(()), |id| {
      self.dispatcher.deregister_wndproc_callback(id)
    });

    removed.and(deregistered)
  }

  /// Registers or removes raw input so that it matches
  /// [`RawInputPolicy::wants_raw_input`].
  fn sync_raw_input(&mut self) -> crate::Result<()> {
    let Some(register) = self.raw_input.pending_change() else {
      return Ok(());
    };

    // Raw input targets the event loop's message window, so it has to be
    // changed from the event loop thread.
    let handle = self.dispatcher.message_window_handle();
    self
      .dispatcher
      .dispatch_sync(move || Self::enable_raw_input(handle, register))??;

    self.raw_input.mark_registered(register);
    Ok(())
  }

  /// Registers a window procedure callback for `WM_INPUT`.
  ///
  /// Returns the ID of the created callback, or `None` if there are no
  /// events to listen for. Raw input itself is registered separately by
  /// [`Self::sync_raw_input`].
  fn register_callback(
    enabled_events: &Arc<[MouseEventKind]>,
    callback_data: &Arc<Mutex<CallbackData>>,
    dispatcher: &Dispatcher,
  ) -> crate::Result<Option<usize>> {
    if enabled_events.is_empty() {
      return Ok(None);
    }

    let enabled_events = Arc::clone(enabled_events);
    let callback_data = Arc::clone(callback_data);

    let callback_id = dispatcher.register_wndproc_callback(Box::new(
      move |_hwnd, msg, _wparam, lparam| {
        if msg != WM_INPUT {
          return None;
        }

        let mut callback_data = callback_data
          .lock()
          .unwrap_or_else(std::sync::PoisonError::into_inner);

        if let Err(err) = Self::handle_wm_input(
          lparam,
          &enabled_events,
          &mut callback_data,
        ) {
          tracing::warn!("Failed to handle WM_INPUT message: {}", err);
        }

        Some(0)
      },
    ))?;

    Ok(Some(callback_id))
  }

  /// Processes a `WM_INPUT` message, extracting raw input data and
  /// sending the appropriate [`MouseEvent`] on the channel.
  fn handle_wm_input(
    lparam: isize,
    enabled_events: &[MouseEventKind],
    callback_data: &mut CallbackData,
  ) -> crate::Result<()> {
    let mut raw_input: RAWINPUT = unsafe { std::mem::zeroed() };
    #[allow(clippy::cast_possible_truncation)]
    let mut raw_input_size = std::mem::size_of::<RAWINPUT>() as u32;

    let res_size = unsafe {
      #[allow(clippy::cast_possible_truncation)]
      GetRawInputData(
        HRAWINPUT(lparam),
        RID_INPUT,
        Some(std::ptr::from_mut(&mut raw_input).cast()),
        &raw mut raw_input_size,
        std::mem::size_of::<RAWINPUTHEADER>() as u32,
      )
    };

    // Ignore if input is invalid or not a mouse event. Inputs from our own
    // process are also ignored, since `NativeWindow::focus` simulates
    // mouse input.
    #[allow(clippy::cast_possible_truncation)]
    if res_size == 0
      || raw_input_size == u32::MAX
      || raw_input.header.dwType != RIM_TYPEMOUSE.0
      || unsafe { raw_input.data.mouse.ulExtraInformation } as u32
        == FOREGROUND_INPUT_IDENTIFIER
    {
      return Ok(());
    }

    // Map button flags to a `MouseEventKind`.
    let event_kind = {
      let button_flags = u32::from(unsafe {
        raw_input.data.mouse.Anonymous.Anonymous.usButtonFlags
      });

      // Button flags indicate a transition in mouse button state.
      // Ref: https://learn.microsoft.com/en-us/windows/win32/api/ntddmou/ns-ntddmou-mouse_input_data#members
      if button_flags & RI_MOUSE_LEFT_BUTTON_DOWN != 0 {
        MouseEventKind::LeftButtonDown
      } else if button_flags & RI_MOUSE_LEFT_BUTTON_UP != 0 {
        MouseEventKind::LeftButtonUp
      } else if button_flags & RI_MOUSE_RIGHT_BUTTON_DOWN != 0 {
        MouseEventKind::RightButtonDown
      } else if button_flags & RI_MOUSE_RIGHT_BUTTON_UP != 0 {
        MouseEventKind::RightButtonUp
      } else {
        MouseEventKind::Move
      }
    };

    if !enabled_events.contains(&event_kind) {
      return Ok(());
    }

    // Throttle mouse move events so that there's a minimum of 50ms between
    // each emission. State change events (button down/up) always get
    // emitted.
    let should_emit = match event_kind {
      MouseEventKind::Move => {
        callback_data.last_move_emission.is_none_or(|timestamp| {
          timestamp.elapsed() >= Duration::from_millis(50)
        })
      }
      _ => true,
    };

    if !should_emit {
      return Ok(());
    }

    callback_data.pressed.update(event_kind);

    let mouse_event = match event_kind {
      MouseEventKind::LeftButtonDown => MouseEvent::ButtonDown {
        position: Self::cursor_pos()?,
        button: MouseButton::Left,
        pressed_buttons: callback_data.pressed,
      },
      MouseEventKind::LeftButtonUp => MouseEvent::ButtonUp {
        position: Self::cursor_pos()?,
        button: MouseButton::Left,
        pressed_buttons: callback_data.pressed,
      },
      MouseEventKind::RightButtonDown => MouseEvent::ButtonDown {
        position: Self::cursor_pos()?,
        button: MouseButton::Right,
        pressed_buttons: callback_data.pressed,
      },
      MouseEventKind::RightButtonUp => MouseEvent::ButtonUp {
        position: Self::cursor_pos()?,
        button: MouseButton::Right,
        pressed_buttons: callback_data.pressed,
      },
      MouseEventKind::Move => MouseEvent::Move {
        position: Self::cursor_pos()?,
        pressed_buttons: callback_data.pressed,
        window_below_cursor: None,
      },
    };

    let _ = callback_data.event_tx.send(mouse_event);

    if event_kind == MouseEventKind::Move {
      callback_data.last_move_emission = Some(Instant::now());
    }

    Ok(())
  }

  /// Gets the current cursor position.
  fn cursor_pos() -> crate::Result<Point> {
    let mut point = POINT { x: 0, y: 0 };
    unsafe { GetCursorPos(&raw mut point) }?;
    Ok(Point {
      x: point.x,
      y: point.y,
    })
  }

  /// Registers or deregisters the raw input device for mouse events.
  fn enable_raw_input(
    target_handle: isize,
    enabled: bool,
  ) -> crate::Result<()> {
    let mode_flag = if enabled {
      RIDEV_INPUTSINK
    } else {
      RIDEV_REMOVE
    };

    let target_hwnd = if enabled {
      HWND(target_handle)
    } else {
      HWND::default()
    };

    let rid = RAWINPUTDEVICE {
      usUsagePage: HID_USAGE_PAGE_GENERIC,
      usUsage: HID_USAGE_GENERIC_MOUSE,
      dwFlags: mode_flag,
      hwndTarget: target_hwnd,
    };

    unsafe {
      #[allow(clippy::cast_possible_truncation)]
      RegisterRawInputDevices(
        &[rid],
        std::mem::size_of::<RAWINPUTDEVICE>() as u32,
      )
    }
    .map_err(crate::Error::from)
  }
}

impl Drop for MouseListener {
  fn drop(&mut self) {
    if let Err(err) = self.terminate() {
      tracing::warn!("Failed to terminate mouse listener: {}", err);
    }
  }
}
