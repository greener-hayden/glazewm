use tokio::sync::mpsc;

use crate::{platform_impl, Dispatcher, WindowEvent};

/// A listener for system-wide window events.
pub struct WindowListener {
  event_rx: mpsc::UnboundedReceiver<WindowEvent>,

  /// Inner platform-specific window listener.
  inner: platform_impl::WindowListener,
}

impl WindowListener {
  /// Enables provisional concealment of new Windows application windows.
  pub fn set_opening_concealment(&self, enabled: bool) {
    #[cfg(target_os = "windows")]
    crate::opening_windows::set_enabled(enabled);
    #[cfg(target_os = "macos")]
    let _ = enabled;
  }

  /// Distinguishes our opening concealment from an application hiding.
  #[must_use]
  pub fn is_opening_window(window: &crate::NativeWindow) -> bool {
    #[cfg(target_os = "windows")]
    {
      crate::opening_windows::is_held(window)
    }
    #[cfg(target_os = "macos")]
    {
      let _ = window;
      false
    }
  }

  /// Creates a new window listener.
  pub fn new(dispatcher: &Dispatcher) -> crate::Result<Self> {
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let inner = platform_impl::WindowListener::new(event_tx, dispatcher)?;

    Ok(Self { event_rx, inner })
  }

  /// Returns the next window event from the listener.
  ///
  /// This will block until a window event is available.
  pub async fn next_event(&mut self) -> Option<WindowEvent> {
    self.event_rx.recv().await
  }

  /// Drains queued events without blocking.
  pub fn try_next_event(&mut self) -> Option<WindowEvent> {
    self.event_rx.try_recv().ok()
  }

  /// Terminates the window listener.
  pub fn terminate(&mut self) {
    self.inner.terminate();
  }
}
