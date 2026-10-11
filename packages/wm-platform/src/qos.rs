use crate::platform_impl;

/// Opts out of power throttling.
///
/// Without this, the system may run the WM on efficiency cores at a
/// reduced clock, which delays the input and window-event paths that have
/// to answer within milliseconds. Call once at startup, on the WM thread.
///
/// # Platform-specific
///
/// - **Windows**: Sets `ProcessPowerThrottling` for the whole process,
///   with the execution-speed policy controlled by the process and
///   switched off. Thread priorities are left untouched.
/// - **macOS**: Gives the calling thread the user-interactive
///   quality-of-service class. The event tap thread asks for its own.
pub fn disable_power_throttling() -> crate::Result<()> {
  #[cfg(target_os = "windows")]
  {
    platform_impl::disable_power_throttling()
  }

  #[cfg(target_os = "macos")]
  {
    platform_impl::set_interactive_qos()
  }
}
