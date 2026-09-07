use std::sync::Arc;

use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;

use crate::{Key, KeyCode, Keybinding};

#[cfg(test)]
#[path = "keybinding_matcher_tests.rs"]
mod tests;

const MODIFIERS: [(u16, u16); 4] =
  [(0xa0, 0xa1), (0xa2, 0xa3), (0xa4, 0xa5), (0x5b, 0x5c)];

/// Fixed-size physical key state.
#[derive(Clone, Default)]
pub(super) struct KeySnapshot([u64; 4]);

impl KeySnapshot {
  /// Records the documented high-order down bit.
  pub(super) fn record(&mut self, code: u16, state: i16) {
    let index = usize::from(code);
    if index < 256 && state.cast_unsigned() & 0x8000 != 0 {
      self.0[index / 64] |= 1 << (index % 64);
    }
  }

  /// Checks one physical key.
  fn contains(&self, code: u16) -> bool {
    let index = usize::from(code);
    self.0[index / 64] & (1 << (index % 64)) != 0
  }

  /// Combines sided modifier states.
  fn modifiers(&self) -> u8 {
    MODIFIERS.iter().enumerate().fold(
      0,
      |bits, (index, &(left, right))| {
        bits
          | (u8::from(self.contains(left) || self.contains(right))
            << index)
      },
    )
  }
}

/// Immutable matching instructions for one binding.
struct Candidate {
  binding: Arc<Keybinding>,
  required: KeySnapshot,
  groups: u8,
  allowed: u8,
}

impl Candidate {
  /// Compiles predicates without changing trigger aliases.
  fn new(binding: &Keybinding) -> Option<Self> {
    let mut required = KeySnapshot::default();
    let mut groups = 0;
    let mut allowed = 0;
    for &key in binding.keys() {
      let group = match key {
        Key::Shift | Key::LShift | Key::RShift => 1,
        Key::Ctrl | Key::LCtrl | Key::RCtrl => 2,
        Key::Alt | Key::LAlt | Key::RAlt => 4,
        Key::Win
        | Key::LWin
        | Key::RWin
        | Key::Cmd
        | Key::LCmd
        | Key::RCmd => 8,
        _ => 0,
      };
      allowed |= group;
      if key == *binding.trigger_key() {
        continue;
      }
      if matches!(
        key,
        Key::Shift | Key::Ctrl | Key::Alt | Key::Win | Key::Cmd
      ) {
        groups |= group;
      } else {
        required.record(KeyCode::try_from(key).ok()?.0, i16::MIN);
      }
    }
    Some(Self {
      binding: Arc::new(binding.clone()),
      required,
      groups,
      allowed,
    })
  }

  /// Checks required keys before modifier exclusion.
  fn matches(&self, keys: &KeySnapshot, modifiers: u8) -> bool {
    modifiers & self.groups == self.groups
      && self
        .required
        .0
        .iter()
        .zip(keys.0)
        .all(|(required, down)| down & required == *required)
  }
}

/// Prepared bindings owned by the input thread.
pub(super) struct CompiledBindings {
  triggers: [Vec<Candidate>; 256],
  queried: KeySnapshot,
}

impl CompiledBindings {
  /// Compiles bindings away from input delivery.
  pub(super) fn new(bindings: &[Keybinding]) -> Self {
    let mut result = Self {
      triggers: std::array::from_fn(|_| Vec::new()),
      queried: KeySnapshot::default(),
    };
    for (left, right) in MODIFIERS {
      result.queried.record(left, i16::MIN);
      result.queried.record(right, i16::MIN);
    }
    for binding in bindings {
      let Ok(code) = KeyCode::try_from(*binding.trigger_key()) else {
        continue;
      };
      if Key::try_from(code).ok().as_ref() != Some(binding.trigger_key()) {
        continue;
      }
      let Some(candidate) = Candidate::new(binding) else {
        continue;
      };
      for (queried, required) in
        result.queried.0.iter_mut().zip(candidate.required.0)
      {
        *queried |= required;
      }
      result.triggers[usize::from(code.0)].push(candidate);
    }
    for candidates in &mut result.triggers {
      candidates.reverse();
      candidates.sort_by_key(|candidate| {
        std::cmp::Reverse(candidate.binding.keys().len())
      });
    }
    result
  }

  /// Samples input state without querying windows.
  pub(super) fn sample(&self, trigger: u16) -> KeySnapshot {
    let mut keys = KeySnapshot::default();
    for code in 0..256u16 {
      if code != trigger && self.queried.contains(code) {
        // SAFETY: Reads system input state only.
        keys.record(code, unsafe { GetAsyncKeyState(i32::from(code)) });
      }
    }
    keys.record(trigger, i16::MIN);
    keys
  }

  /// Selects the existing longest-match winner.
  pub(super) fn matching(
    &self,
    trigger: u16,
    keys: &KeySnapshot,
  ) -> Option<&Arc<Keybinding>> {
    let modifiers = keys.modifiers();
    let candidate = self
      .triggers
      .get(usize::from(trigger))?
      .iter()
      .find(|candidate| candidate.matches(keys, modifiers))?;
    (modifiers & !candidate.allowed == 0).then_some(&candidate.binding)
  }
}
