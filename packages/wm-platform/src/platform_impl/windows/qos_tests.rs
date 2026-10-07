use super::*;

/// Opts out of execution-speed throttling and nothing else.
#[test]
fn opt_out_state_controls_only_execution_speed() {
  let state = opt_out_state();
  assert_eq!(state.Version, 1);
  assert_eq!(state.ControlMask, PROCESS_POWER_THROTTLING_EXECUTION_SPEED);
  assert_eq!(state.StateMask, 0);
}

/// The process accepts the policy, repeatedly.
///
/// Affects only the test process; it creates no window and installs no
/// hook.
#[test]
fn disable_power_throttling_is_idempotent() {
  disable_power_throttling().expect("First call applies the policy.");
  disable_power_throttling().expect("Second call applies it again.");
}
