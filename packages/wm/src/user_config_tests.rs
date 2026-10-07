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
  let event = KeybindingEvent(shortcut.bindings[0].clone());
  config.value.keybindings = vec![shortcut];
  assert!(config.listener_bindings(&[]).contains(&event.0));
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
  let queued = KeybindingEvent(shortcut.bindings[0].clone());
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
