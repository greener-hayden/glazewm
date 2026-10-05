use wm_common::{
  InvokeUpdateWorkspaceConfig, ParsedConfig, WorkspaceConfig,
};

use super::*;
use crate::{
  commands::workspace::update_workspace_config,
  test_utils::{attach_monitor, mock_state},
};

/// A workspace bound to a monitor index that doesn't exist yet keeps its
/// binding and moves onto the monitor when it appears, including when
/// the binding exists only in the workspace's live config and not in the
/// config file. `keep_alive` activation from the config file still runs.
#[test]
fn bound_workspace_moves_to_monitor_when_it_appears() -> anyhow::Result<()>
{
  let (mut state, _event_rx) = mock_state();

  let first = Workspace::mock().name("1".to_string()).call();
  let first_monitor =
    Monitor::mock().workspaces(vec![first.clone()]).call();

  attach_monitor(&state, &first_monitor)?;

  // The config file binds nothing that is currently active; "keep" is an
  // inactive keep_alive config for monitor 1 and "3" is an inactive
  // spare.
  let config = UserConfig::mock(ParsedConfig {
    workspaces: vec![
      WorkspaceConfig {
        name: "1".to_string(),
        display_name: None,
        bind_to_monitor: None,
        keep_alive: false,
      },
      WorkspaceConfig {
        name: "keep".to_string(),
        display_name: None,
        bind_to_monitor: Some(1),
        keep_alive: true,
      },
      WorkspaceConfig {
        name: "3".to_string(),
        display_name: None,
        bind_to_monitor: None,
        keep_alive: false,
      },
    ],
    ..Default::default()
  });

  // Bind workspace "1" to monitor index 1 through the live config. The
  // monitor doesn't exist yet, so the workspace stays put and keeps its
  // binding.
  update_workspace_config(
    &first,
    &mut state,
    &config,
    &InvokeUpdateWorkspaceConfig {
      name: None,
      display_name: None,
      bind_to_monitor: Some(1),
      keep_alive: None,
    },
  )?;

  assert_eq!(first.monitor().context("Monitor.")?.index(), 0);
  assert_eq!(first.config().bind_to_monitor, Some(1));

  // Monitor with index 1 appears.
  let new_monitor = Monitor::mock().call();
  attach_monitor(&state, &new_monitor)?;

  move_bounded_workspaces_to_new_monitor(
    &new_monitor,
    &mut state,
    &config,
  )?;

  // The live-bound workspace moved onto the new monitor.
  let moved = state
    .workspace_by_name("1")
    .context("Workspace 1 is missing.")?;
  assert_eq!(moved.monitor().context("Monitor.")?.id(), new_monitor.id());

  // The keep_alive workspace from the config file was activated on the
  // new monitor.
  let keep_alive = state
    .workspace_by_name("keep")
    .context("Workspace keep is missing.")?;
  assert_eq!(
    keep_alive.monitor().context("Monitor.")?.id(),
    new_monitor.id()
  );

  // The origin monitor was refilled with an inactive workspace config.
  let origin_monitor = state
    .monitors()
    .into_iter()
    .find(|monitor| monitor.index() == 0)
    .context("Monitor 0 is missing.")?;
  assert_eq!(origin_monitor.child_count(), 1);
  assert_eq!(origin_monitor.workspaces()[0].config().name, "3");

  Ok(())
}
