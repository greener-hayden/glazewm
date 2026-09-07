use crate::{platform_impl, Dispatcher, Key, KeybindingEvent};

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Keybinding(Vec<Key>);

impl Keybinding {
  /// Creates a new keybinding from a vector of keys.
  ///
  /// # Errors
  ///
  /// Returns [`Error::InvalidKeybinding`] if the keybinding is empty.
  pub fn new(keys: Vec<Key>) -> crate::Result<Self> {
    if keys.is_empty() {
      return Err(crate::Error::InvalidKeybinding);
    }

    Ok(Self(keys))
  }

  /// Returns the keys in the keybinding.
  #[must_use]
  pub fn keys(&self) -> &[Key] {
    &self.0
  }

  /// Returns the trigger key in the keybinding.
  #[must_use]
  #[allow(clippy::missing_panics_doc)]
  pub fn trigger_key(&self) -> &Key {
    // SAFETY: Keys vector is verified to be non-empty in
    // `Keybinding::new`.
    self.0.last().unwrap()
  }
}

/// Platform-specific shortcut listener facade.
pub struct KeybindingListener {
  #[cfg(target_os = "windows")]
  inner: platform_impl::KeyboardInput,
  #[cfg(target_os = "macos")]
  inner: platform_impl::MacKeybindingListener,
}

impl std::fmt::Debug for KeybindingListener {
  /// Describes the listener without exposing platform state.
  fn fmt(
    &self,
    formatter: &mut std::fmt::Formatter<'_>,
  ) -> std::fmt::Result {
    formatter
      .debug_struct("KeybindingListener")
      .finish_non_exhaustive()
  }
}

impl KeybindingListener {
  /// Starts the platform keyboard listener.
  pub fn new(
    bindings: &[Keybinding],
    dispatcher: &Dispatcher,
  ) -> crate::Result<Self> {
    #[cfg(target_os = "windows")]
    let inner = {
      let _ = dispatcher;
      platform_impl::KeyboardInput::new(bindings)?
    };
    #[cfg(target_os = "macos")]
    let inner =
      platform_impl::MacKeybindingListener::new(bindings, dispatcher)?;
    Ok(Self { inner })
  }

  /// Receives bindings without resolving WM commands.
  pub async fn next_event(&mut self) -> Option<KeybindingEvent> {
    self.inner.next_event().await
  }

  /// Publishes a complete replacement binding table.
  pub fn update(&self, bindings: &[Keybinding]) -> crate::Result<()> {
    #[cfg(target_os = "windows")]
    {
      self.inner.update(bindings)
    }
    #[cfg(target_os = "macos")]
    {
      self.inner.update(bindings);
      Ok(())
    }
  }

  /// Takes overflow counts without inspecting hook health.
  #[must_use]
  #[cfg_attr(target_os = "macos", allow(clippy::unused_self))]
  pub fn take_dropped(&self) -> u64 {
    #[cfg(target_os = "windows")]
    {
      self.inner.take_dropped()
    }
    #[cfg(target_os = "macos")]
    {
      0
    }
  }

  /// Enables or disables shortcut interception.
  pub fn enable(&mut self, enabled: bool) {
    self.inner.enable(enabled);
  }

  /// Releases the platform listener exactly once.
  pub fn terminate(&mut self) -> crate::Result<()> {
    self.inner.terminate()
  }
}
