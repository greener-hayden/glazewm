#[cfg(target_os = "windows")]
use crate::platform_impl;

/// Opts the current process out of power throttling.
///
/// Without this, Windows may run the process on efficiency cores at a
/// reduced clock (`EcoQoS`), which delays the input and window-event paths
/// that have to answer within milliseconds. Call once at startup. Thread
/// priorities are left untouched.
///
/// # Platform-specific
///
/// - **Windows**: Sets `ProcessPowerThrottling` with the execution-speed
///   policy controlled by the process and switched off.
/// - **macOS**: No-op.
pub fn disable_power_throttling() -> crate::Result<()> {
  #[cfg(target_os = "windows")]
  {
    platform_impl::disable_power_throttling()
  }

  #[cfg(not(target_os = "windows"))]
  {
    Ok(())
  }
}
