use windows::Win32::System::Threading::{
  GetCurrentProcess, ProcessPowerThrottling, SetProcessInformation,
  PROCESS_POWER_THROTTLING_CURRENT_VERSION,
  PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
  PROCESS_POWER_THROTTLING_STATE,
};

#[cfg(test)]
#[path = "qos_tests.rs"]
mod tests;

/// Builds the state that opts execution speed out of power throttling.
///
/// A set bit in `ControlMask` takes control of that policy from the
/// system, and the cleared bit in `StateMask` then turns the policy off.
/// Only execution speed is controlled; timer resolution stays with the
/// system default.
const fn opt_out_state() -> PROCESS_POWER_THROTTLING_STATE {
  PROCESS_POWER_THROTTLING_STATE {
    Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
    ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
    StateMask: 0,
  }
}

/// Implements [`crate::disable_power_throttling`].
pub(crate) fn disable_power_throttling() -> crate::Result<()> {
  let state = opt_out_state();

  #[allow(clippy::cast_possible_truncation)]
  let size = std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32;

  // SAFETY: `state` outlives the call, `size` is its exact size, and the
  // pseudo handle from `GetCurrentProcess` needs no closing.
  unsafe {
    SetProcessInformation(
      GetCurrentProcess(),
      ProcessPowerThrottling,
      std::ptr::from_ref(&state).cast(),
      size,
    )
  }
  .map_err(crate::Error::from)
}
