use super::*;

/// Constructs one test binding.
fn binding(keys: &[Key]) -> Keybinding {
  Keybinding::new(keys.to_vec()).expect("Valid test binding.")
}

/// Creates a fixed physical key snapshot.
fn pressed(keys: &[Key]) -> KeySnapshot {
  let mut snapshot = KeySnapshot::default();
  for &key in keys {
    snapshot
      .record(KeyCode::try_from(key).expect("Physical key.").0, i16::MIN);
  }
  snapshot
}

/// Matches against a supplied physical snapshot.
fn matched(
  table: &CompiledBindings,
  trigger: Key,
  keys: &[Key],
) -> Option<Keybinding> {
  table
    .matching(
      KeyCode::try_from(trigger).expect("Trigger key.").0,
      &pressed(keys),
    )
    .map(|binding| (**binding).clone())
}

/// Reads only the documented down-state bit.
#[test]
fn input_matcher_down_bit() {
  let mut keys = KeySnapshot::default();
  keys.record(0xa2, 0x80);
  assert!(!keys.contains(0xa2));
  keys.record(0xa2, 1);
  assert!(!keys.contains(0xa2));
  keys.record(0xa2, i16::MIN);
  assert!(keys.contains(0xa2));
}

/// Preserves longest matches and last equal winners.
#[test]
fn input_matcher_priority() {
  let generic = binding(&[Key::Ctrl, Key::A]);
  let sided = binding(&[Key::LCtrl, Key::A]);
  let longest = binding(&[Key::Ctrl, Key::B, Key::A]);
  let table = CompiledBindings::new(&[
    generic.clone(),
    sided.clone(),
    longest.clone(),
  ]);
  assert_eq!(matched(&table, Key::A, &[Key::LCtrl]), Some(sided.clone()));
  assert_eq!(
    matched(&table, Key::A, &[Key::RCtrl]),
    Some(generic.clone())
  );
  assert_eq!(
    matched(&table, Key::A, &[Key::LCtrl, Key::B]),
    Some(longest)
  );
  let reversed = CompiledBindings::new(&[sided, generic.clone()]);
  assert_eq!(matched(&reversed, Key::A, &[Key::LCtrl]), Some(generic));
}

/// Rejects extra modifiers without choosing shorter fallbacks.
#[test]
fn input_matcher_extra_modifiers() {
  let plain = binding(&[Key::A, Key::A, Key::B]);
  let control = binding(&[Key::Ctrl, Key::B]);
  let table = CompiledBindings::new(&[control, plain]);
  assert!(matched(&table, Key::B, &[Key::LCtrl, Key::A]).is_none());
  let table = CompiledBindings::new(&[binding(&[Key::Ctrl, Key::A])]);
  for extra in [
    Key::LShift,
    Key::RShift,
    Key::LAlt,
    Key::RAlt,
    Key::LWin,
    Key::RWin,
  ] {
    assert!(matched(&table, Key::A, &[Key::LCtrl, extra]).is_none());
  }
  assert!(matched(&table, Key::A, &[Key::LCtrl, Key::RCtrl]).is_some());
  assert!(matched(&table, Key::A, &[Key::LCtrl, Key::B]).is_some());
}

/// Preserves generic, sided, and unsupported aliases.
#[test]
fn input_matcher_aliases() {
  for (generic, left, right) in [
    (Key::Ctrl, Key::LCtrl, Key::RCtrl),
    (Key::Alt, Key::LAlt, Key::RAlt),
    (Key::Shift, Key::LShift, Key::RShift),
    (Key::Win, Key::LWin, Key::RWin),
    (Key::Cmd, Key::LWin, Key::RWin),
  ] {
    let table = CompiledBindings::new(&[binding(&[generic, Key::A])]);
    assert!(matched(&table, Key::A, &[left]).is_some());
    assert!(matched(&table, Key::A, &[right]).is_some());
    assert!(matched(&table, Key::A, &[]).is_none());
  }
  let table = CompiledBindings::new(&[binding(&[Key::LCmd, Key::A])]);
  assert!(matched(&table, Key::A, &[Key::LWin]).is_none());
  let table = CompiledBindings::new(&[binding(&[Key::LWin])]);
  assert!(matched(&table, Key::LWin, &[Key::LWin]).is_none());
  let table = CompiledBindings::new(&[binding(&[Key::Win])]);
  assert!(matched(&table, Key::Win, &[Key::LWin]).is_some());
  assert!(table.matching(256, &KeySnapshot::default()).is_none());
}
