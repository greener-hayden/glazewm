use wm_common::BindingModeConfig;
use wm_platform::{Key, Keybinding, KeybindingEvent};

use super::*;

/// Creates configuration without touching user files.
fn config() -> UserConfig {
  UserConfig {
    path: PathBuf::new(),
    value: serde_yaml::from_str(SAMPLE_CONFIG)
      .expect("Sample config parses."),
    value_str: String::new(),
    window_rules_by_event: HashMap::new(),
  }
}

/// Creates an isolated temporary config directory.
fn temporary_config_path() -> PathBuf {
  std::env::temp_dir()
    .join(format!("glazewm-config-test-{}", uuid::Uuid::new_v4()))
    .join("config.yaml")
}

/// Creates a binding with caller-selected commands.
fn shortcut(command: InvokeCommand) -> KeybindingConfig {
  KeybindingConfig {
    bindings: vec![
      Keybinding::new(vec![Key::F24]).expect("Valid binding.")
    ],
    commands: vec![command],
  }
}

/// Verifies sample config defaults and config reload transitions.
#[test]
fn floating_above_tiled_config_reload() -> anyhow::Result<()> {
  let sample: ParsedConfig = serde_yaml::from_str(SAMPLE_CONFIG)?;
  assert!(!sample.window_behavior.floating_above_tiled);

  let path = temporary_config_path();
  let directory = path.parent().expect("Temporary config has a parent.");
  fs::create_dir_all(directory)?;
  let false_config = "window_behavior:\n  floating_above_tiled: false\n";
  let true_config = "window_behavior:\n  floating_above_tiled: true\n";
  fs::write(&path, false_config)?;
  let mut config = UserConfig::new(Some(path.clone()))?;
  assert!(!config.value.window_behavior.floating_above_tiled);

  fs::write(&path, true_config)?;
  config.reload()?;
  assert!(config.value.window_behavior.floating_above_tiled);
  assert_eq!(config.value_str, true_config);

  fs::write(&path, false_config)?;
  config.reload()?;
  assert!(!config.value.window_behavior.floating_above_tiled);

  fs::write(&path, "window_behavior: {}\n")?;
  config.reload()?;
  assert!(!config.value.window_behavior.floating_above_tiled);

  fs::write(&path, true_config)?;
  config.reload()?;
  let prior_value = config.value.clone();
  let prior_string = config.value_str.clone();
  fs::write(&path, "window_behavior:\n  floating_above_tiled: invalid\n")?;
  assert!(config.reload().is_err());
  assert!(config.value.window_behavior.floating_above_tiled);
  assert_eq!(config.value_str, prior_string);
  assert_eq!(
    config.value.window_behavior.floating_above_tiled,
    prior_value.window_behavior.floating_above_tiled
  );

  drop(config);
  fs::remove_dir_all(directory)?;
  Ok(())
}

/// Preserves interception while paused execution remains filtered.
#[test]
fn input_compatibility_paused() {
  let mut config = config();
  let shortcut = shortcut(InvokeCommand::WmExit);
  let event = KeybindingEvent::new(shortcut.bindings[0].clone());
  config.value.keybindings = vec![shortcut];
  assert!(config.listener_bindings(&[]).contains(&event.binding));
  assert!(config.keybinding_commands(&event, &[], true).is_none());
  config.value.keybindings[0].commands =
    vec![InvokeCommand::WmTogglePause];
  assert_eq!(
    config.keybinding_commands(&event, &[], true),
    Some(vec![InvokeCommand::WmTogglePause])
  );
}

/// Resolves queued bindings using consumption-time configuration.
#[test]
fn input_compatibility_consumption() {
  let mut config = config();
  let shortcut = shortcut(InvokeCommand::WmExit);
  let queued = KeybindingEvent::new(shortcut.bindings[0].clone());
  config.value.keybindings = vec![shortcut];
  config.value.keybindings[0].commands =
    vec![InvokeCommand::WmReloadConfig];
  assert_eq!(
    config.keybinding_commands(&queued, &[], false),
    Some(vec![InvokeCommand::WmReloadConfig])
  );
  let mode = BindingModeConfig {
    name: "test".into(),
    display_name: None,
    keybindings: vec![self::shortcut(InvokeCommand::WmRedraw)],
  };
  assert_eq!(
    config.keybinding_commands(&queued, &[mode], false),
    Some(vec![InvokeCommand::WmRedraw])
  );
  config.value.keybindings.clear();
  assert!(config.keybinding_commands(&queued, &[], false).is_none());
}

/// Resolution as it was before configs were borrowed: copies the whole
/// table per call. Kept as an oracle for the by-reference path.
fn oracle_active_configs(
  config: &UserConfig,
  modes: &[BindingModeConfig],
  paused: bool,
) -> Vec<KeybindingConfig> {
  let source = if let Some(first) = modes.first() {
    &first.keybindings
  } else {
    &config.value.keybindings
  }
  .clone();
  source
    .into_iter()
    .filter(|kb| {
      !paused || kb.commands.contains(&InvokeCommand::WmTogglePause)
    })
    .collect()
}

/// Oracle for `UserConfig::keybinding_commands`.
fn oracle_commands(
  config: &UserConfig,
  event: &KeybindingEvent,
  modes: &[BindingModeConfig],
  paused: bool,
) -> Option<Vec<InvokeCommand>> {
  oracle_active_configs(config, modes, paused)
    .into_iter()
    .find(|kb| kb.bindings.contains(&event.binding))
    .map(|kb| kb.commands)
}

/// Oracle for `UserConfig::listener_bindings`.
fn oracle_listener_bindings(
  config: &UserConfig,
  modes: &[BindingModeConfig],
) -> Vec<Keybinding> {
  oracle_active_configs(config, modes, false)
    .into_iter()
    .flat_map(|kb| kb.bindings)
    .collect()
}

/// Creates a binding config of two chords with the given commands.
fn chord_config(
  prefix: Key,
  key: Key,
  commands: Vec<InvokeCommand>,
) -> KeybindingConfig {
  KeybindingConfig {
    bindings: vec![
      Keybinding::new(vec![prefix, key]).expect("Valid binding."),
      Keybinding::new(vec![Key::F4, prefix, key]).expect("Valid binding."),
    ],
    commands,
  }
}

/// Creates a 56-entry table shaped like a full user config. Entry 50
/// repeats the binding of entry 3 so first-match precedence is covered.
fn realistic_table() -> Vec<KeybindingConfig> {
  let letters = [
    Key::A,
    Key::B,
    Key::C,
    Key::D,
    Key::E,
    Key::F,
    Key::G,
    Key::H,
    Key::I,
    Key::J,
    Key::K,
    Key::L,
    Key::M,
    Key::N,
    Key::O,
    Key::P,
    Key::Q,
    Key::R,
    Key::S,
    Key::T,
    Key::U,
    Key::V,
    Key::W,
    Key::X,
    Key::Y,
    Key::Z,
  ];
  let digits = [
    Key::D0,
    Key::D1,
    Key::D2,
    Key::D3,
    Key::D4,
    Key::D5,
    Key::D6,
    Key::D7,
    Key::D8,
    Key::D9,
  ];
  let function_keys = [
    Key::F1,
    Key::F2,
    Key::F3,
    Key::F4,
    Key::F5,
    Key::F6,
    Key::F7,
    Key::F8,
    Key::F9,
  ];
  let chords = letters
    .iter()
    .map(|key| (Key::F1, *key))
    .chain(digits.iter().map(|key| (Key::F2, *key)))
    .chain(function_keys.iter().map(|key| (Key::F3, *key)))
    .chain(letters.iter().take(11).map(|key| (Key::F5, *key)));
  let mut table = chords
    .enumerate()
    .map(|(index, (prefix, key))| {
      let commands = match index % 5 {
        0 => vec![InvokeCommand::WmRedraw],
        1 => vec![InvokeCommand::WmTogglePause],
        2 => vec![InvokeCommand::Close, InvokeCommand::WmReloadConfig],
        3 => vec![InvokeCommand::Ignore],
        _ => vec![InvokeCommand::WmExit],
      };
      chord_config(prefix, key, commands)
    })
    .collect::<Vec<_>>();
  assert_eq!(table.len(), 56);
  table[50] = KeybindingConfig {
    bindings: table[3].bindings.clone(),
    commands: vec![InvokeCommand::WmExit, InvokeCommand::WmExit],
  };
  table
}

/// Resolving by reference returns exactly what copying the table did, for
/// every binding, mode and pause state.
#[test]
fn keybinding_resolution_matches_copying_oracle() {
  let mut config = config();
  config.value.keybindings = realistic_table();
  let mode = BindingModeConfig {
    name: "resize".into(),
    display_name: None,
    keybindings: config.value.keybindings[10..24].to_vec(),
  };
  let mode_sets: [&[BindingModeConfig]; 2] =
    [&[], std::slice::from_ref(&mode)];

  let mut events = config
    .value
    .keybindings
    .iter()
    .flat_map(|kb| kb.bindings.iter())
    .map(|binding| KeybindingEvent::new(binding.clone()))
    .collect::<Vec<_>>();
  // A binding no table contains.
  events.push(KeybindingEvent::new(
    Keybinding::new(vec![Key::F24, Key::Z]).expect("Valid binding."),
  ));

  let mut resolved = 0;
  for modes in mode_sets {
    assert_eq!(
      config.listener_bindings(modes),
      oracle_listener_bindings(&config, modes)
    );
    for paused in [false, true] {
      for event in &events {
        let actual = config.keybinding_commands(event, modes, paused);
        assert_eq!(
          actual,
          oracle_commands(&config, event, modes, paused),
          "{:?} modes={} paused={paused}",
          event.binding,
          modes.len()
        );
        resolved += usize::from(actual.is_some());
      }
    }
  }
  assert!(resolved > 100, "Fixture resolves real bindings.");

  // The duplicate binding resolves to the earlier config.
  let duplicate =
    KeybindingEvent::new(config.value.keybindings[3].bindings[0].clone());
  assert_eq!(
    config.keybinding_commands(&duplicate, &[], false),
    Some(config.value.keybindings[3].commands.clone())
  );
}

/// The active table borrows from the configuration; it is not a copy.
#[test]
fn active_keybinding_configs_borrow_the_table() {
  let mut config = config();
  config.value.keybindings = realistic_table();
  let mode = BindingModeConfig {
    name: "mode".into(),
    display_name: None,
    keybindings: realistic_table(),
  };

  let global = config.active_keybinding_configs(&[], false);
  for (borrowed, owned) in global.zip(&config.value.keybindings) {
    assert!(std::ptr::eq(borrowed, owned));
  }
  let modes = std::slice::from_ref(&mode);
  let in_mode = config.active_keybinding_configs(modes, false);
  for (borrowed, owned) in in_mode.zip(&mode.keybindings) {
    assert!(std::ptr::eq(borrowed, owned));
  }
  let paused = config
    .active_keybinding_configs(&[], true)
    .collect::<Vec<_>>();
  assert!(!paused.is_empty());
  assert!(paused
    .iter()
    .all(|kb| { kb.commands.contains(&InvokeCommand::WmTogglePause) }));
}
