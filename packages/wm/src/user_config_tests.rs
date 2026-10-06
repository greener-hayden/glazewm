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

/// Creates a binding with caller-selected commands.
fn shortcut(command: InvokeCommand) -> KeybindingConfig {
  KeybindingConfig {
    bindings: vec![
      Keybinding::new(vec![Key::F24]).expect("Valid binding.")
    ],
    commands: vec![command],
  }
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
