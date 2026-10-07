use super::*;

const MOVE: [MouseEventKind; 1] = [MouseEventKind::Move];
const UP: [MouseEventKind; 1] = [MouseEventKind::LeftButtonUp];

/// Creates a policy for `events`, enabled and unregistered.
fn policy(events: &[MouseEventKind]) -> RawInputPolicy {
  RawInputPolicy::new(Arc::from(events))
}

/// Applies the pending change like `sync_raw_input`, returning the
/// resulting registration.
fn sync(policy: &mut RawInputPolicy) -> bool {
  if let Some(register) = policy.pending_change() {
    policy.mark_registered(register);
  }
  policy.registered
}

/// Applies a new event set like `set_enabled_events`.
fn set_events(policy: &mut RawInputPolicy, events: &[MouseEventKind]) {
  if policy.accepts_events(events) {
    policy.set_events(Arc::from(events));
  }
}

/// Wants raw input only when enabled with at least one event kind.
#[test]
fn raw_input_wanted_only_with_consumer_and_enabled() {
  let mut both =
    policy(&[MouseEventKind::Move, MouseEventKind::LeftButtonUp]);
  assert!(both.wants_raw_input());

  // Paused: events present but disabled.
  let mut paused = policy(&UP);
  paused.set_enabled(false);
  assert!(!paused.wants_raw_input());

  // No consumer: enabled but nothing to listen for.
  assert!(!policy(&[]).wants_raw_input());
  let mut empty_paused = policy(&[]);
  empty_paused.set_enabled(false);
  assert!(!empty_paused.wants_raw_input());

  both.set_enabled(false);
  assert!(!both.wants_raw_input());
}

/// Plans a native call only when the registration has to change.
#[test]
fn raw_input_change_plans_minimal_transitions() {
  let mut state = policy(&MOVE);
  assert_eq!(state.pending_change(), Some(true));
  assert!(sync(&mut state));
  assert_eq!(state.pending_change(), None);

  state.set_enabled(false);
  assert_eq!(state.pending_change(), Some(false));
  assert!(!sync(&mut state));
  assert_eq!(state.pending_change(), None);

  // Re-enable, then empty the set: raw input is removed.
  state.set_enabled(true);
  assert!(sync(&mut state));
  state.set_events(Arc::from([]));
  assert_eq!(state.pending_change(), Some(false));
  assert!(!sync(&mut state));
  assert_eq!(state.pending_change(), None);
}

/// Pausing, changing events, and resuming never register while paused.
///
/// Regression for `set_enabled_events` re-enabling raw input after
/// `enable(false)`.
#[test]
fn raw_input_stays_removed_while_paused() {
  let mut state = policy(&UP);
  assert!(sync(&mut state));

  // Pause, then a config change swaps the event set.
  state.set_enabled(false);
  assert!(!sync(&mut state));
  set_events(&mut state, &MOVE);
  assert!(!sync(&mut state));

  // Resume.
  state.set_enabled(true);
  assert!(sync(&mut state));

  // The set empties while enabled, then refills.
  set_events(&mut state, &[]);
  assert!(!sync(&mut state));
  set_events(&mut state, &UP);
  assert!(sync(&mut state));
}

/// An unchanged event set is not re-applied.
#[test]
fn unchanged_events_are_not_accepted() {
  let state = policy(&MOVE);
  assert!(!state.accepts_events(&MOVE));
  assert!(state.accepts_events(&UP));
  assert!(state.accepts_events(&[]));
}

/// Terminating removes raw input, and nothing re-registers it.
///
/// Regression: `set_enabled_events` after `terminate` saw a differing
/// (empty) set and registered a callback and raw input again.
#[test]
fn terminate_prevents_reenabling() {
  let mut state = policy(&MOVE);
  assert!(sync(&mut state));

  state.terminate();
  assert_eq!(state.pending_change(), Some(false));
  assert!(!sync(&mut state));

  // Neither enabling nor a new event set registers anything.
  state.set_enabled(true);
  assert!(!state.accepts_events(&UP));
  assert!(!state.accepts_events(&MOVE));
  set_events(&mut state, &UP);
  assert!(state.events.is_empty());
  assert!(!state.wants_raw_input());
  assert_eq!(state.pending_change(), None);

  // Terminating again is idempotent.
  state.terminate();
  assert_eq!(state.pending_change(), None);
  assert!(state.terminated);
}
