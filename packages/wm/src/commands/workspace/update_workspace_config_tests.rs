use tokio::sync::mpsc::UnboundedReceiver;
use wm_common::{ParsedConfig, WmEvent};
use wm_platform::Rect;

use super::*;
use crate::{
  commands::container::set_focused_descendant,
  models::{Monitor, NonTilingWindow, TilingWindow},
  test_utils::{attach_monitor, mock_state},
  traits::{PositionGetters, WindowGetters},
};

fn workspace_config(
  name: &str,
  bind_to_monitor: Option<u32>,
) -> WorkspaceConfig {
  WorkspaceConfig {
    name: name.to_string(),
    display_name: None,
    bind_to_monitor,
    keep_alive: false,
  }
}

fn mock_config(workspaces: Vec<WorkspaceConfig>) -> UserConfig {
  UserConfig::mock(ParsedConfig {
    workspaces,
    ..Default::default()
  })
}

/// State with two monitors: monitor 0 with workspace "1" and monitor 1
/// with workspace "2".
fn mock_two_monitor_state(
) -> anyhow::Result<(WmState, UnboundedReceiver<WmEvent>)> {
  let (state, event_rx) = mock_state();

  let first_monitor = Monitor::mock()
    .workspaces(vec![Workspace::mock().name("1".to_string()).call()])
    .call();
  let second_monitor = Monitor::mock()
    .workspaces(vec![Workspace::mock().name("2".to_string()).call()])
    .call();

  attach_monitor(&state, &first_monitor)?;
  attach_monitor(&state, &second_monitor)?;

  Ok((state, event_rx))
}

fn update_binding(
  workspace: &Workspace,
  bind_to_monitor: Option<u32>,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  update_workspace_config(
    workspace,
    state,
    config,
    &InvokeUpdateWorkspaceConfig {
      name: None,
      display_name: None,
      bind_to_monitor,
      keep_alive: None,
    },
  )
}

/// Drains queued events, returning how many `WorkspaceUpdated` events
/// were emitted.
fn count_workspace_updates(
  event_rx: &mut UnboundedReceiver<WmEvent>,
) -> usize {
  let mut count = 0;

  while let Ok(event) = event_rx.try_recv() {
    if matches!(event, WmEvent::WorkspaceUpdated { .. }) {
      count += 1;
    }
  }

  count
}

/// A workspace with a changed binding moves to the bound monitor, sorted
/// by config order, with its origin monitor refilled and a single update
/// event emitted. Moved from a monitor without focus, it doesn't replace
/// the workspace displayed on the bound monitor.
#[test]
fn moves_workspace_to_newly_bound_monitor() -> anyhow::Result<()> {
  let (mut state, mut event_rx) = mock_two_monitor_state()?;

  // Focus is on the bound monitor. A workspace moved from the focused
  // monitor takes focus with it and is displayed on arrival, which
  // `moved_workspace_preserves_focus` covers.
  let displayed = state
    .workspace_by_name("2")
    .context("Workspace 2 is missing.")?;
  set_focused_descendant(&displayed.into(), None);
  let config = mock_config(vec![
    workspace_config("1", None),
    workspace_config("2", None),
    workspace_config("3", None),
  ]);

  let workspace = state
    .workspace_by_name("1")
    .context("Workspace 1 is missing.")?;
  let origin_monitor_id = workspace.monitor().context("Monitor.")?.id();

  update_binding(&workspace, Some(1), &mut state, &config)?;

  // Workspace moved to monitor 1, sorted into config order (before
  // "2").
  let target_monitor = state
    .monitors()
    .into_iter()
    .find(|monitor| monitor.index() == 1)
    .context("Monitor 1 is missing.")?;
  assert_eq!(
    workspace.monitor().context("Monitor.")?.id(),
    target_monitor.id()
  );
  let target_names = target_monitor
    .workspaces()
    .into_iter()
    .map(|workspace| workspace.config().name)
    .collect::<Vec<_>>();
  assert_eq!(target_names, vec!["1".to_string(), "2".to_string()]);

  // Origin monitor refilled with an inactive workspace config.
  let origin_monitor = state
    .monitors()
    .into_iter()
    .find(|monitor| monitor.id() == origin_monitor_id)
    .context("Origin monitor is missing.")?;
  assert_eq!(origin_monitor.child_count(), 1);
  assert_eq!(origin_monitor.workspaces()[0].config().name, "3");

  // Displayed workspace on the target monitor is unchanged.
  assert_eq!(
    target_monitor
      .displayed_workspace()
      .context("Displayed workspace is missing.")?
      .config()
      .name,
    "2"
  );

  // Exactly one workspace update event is emitted (any activation of the
  // refill workspace emits a separate event).
  assert_eq!(count_workspace_updates(&mut event_rx), 1);

  Ok(())
}

/// A binding whose monitor doesn't exist keeps the workspace where it is
/// and retains the configured binding.
#[test]
fn keeps_workspace_when_bound_monitor_is_absent() -> anyhow::Result<()> {
  let (mut state, mut event_rx) = mock_two_monitor_state()?;
  let config = mock_config(vec![
    workspace_config("1", None),
    workspace_config("2", None),
  ]);

  let workspace = state
    .workspace_by_name("1")
    .context("Workspace 1 is missing.")?;
  let origin_monitor_id = workspace.monitor().context("Monitor.")?.id();

  update_binding(&workspace, Some(9), &mut state, &config)?;

  // Workspace stayed put and kept its binding.
  assert_eq!(
    workspace.monitor().context("Monitor.")?.id(),
    origin_monitor_id
  );
  assert_eq!(workspace.config().bind_to_monitor, Some(9));
  assert_eq!(count_workspace_updates(&mut event_rx), 1);

  Ok(())
}

/// Updating a binding to the monitor the workspace is already on doesn't
/// move it.
#[test]
fn keeps_workspace_already_on_bound_monitor() -> anyhow::Result<()> {
  let (mut state, mut event_rx) = mock_two_monitor_state()?;
  let config = mock_config(vec![
    workspace_config("1", None),
    workspace_config("2", None),
  ]);

  let workspace = state
    .workspace_by_name("1")
    .context("Workspace 1 is missing.")?;

  update_binding(&workspace, Some(0), &mut state, &config)?;

  // Workspace stayed put and no monitor gained or lost a workspace.
  assert_eq!(workspace.config().bind_to_monitor, Some(0));
  assert_eq!(state.monitors()[0].child_count(), 1);
  assert_eq!(state.monitors()[1].child_count(), 1);
  assert_eq!(count_workspace_updates(&mut event_rx), 1);

  Ok(())
}

/// A workspace that would leave its monitor without a workspace that
/// can't be refilled stays put and retains its binding.
#[test]
fn keeps_workspace_when_origin_monitor_cannot_be_refilled(
) -> anyhow::Result<()> {
  let (mut state, mut event_rx) = mock_two_monitor_state()?;

  // Only workspaces "1" and "2" exist in config and both are active, so
  // emptying monitor 0 can't be refilled.
  let config = mock_config(vec![
    workspace_config("1", None),
    workspace_config("2", None),
  ]);

  let workspace = state
    .workspace_by_name("1")
    .context("Workspace 1 is missing.")?;
  let origin_monitor_id = workspace.monitor().context("Monitor.")?.id();

  update_binding(&workspace, Some(1), &mut state, &config)?;

  // Workspace stayed put and kept its binding.
  assert_eq!(
    workspace.monitor().context("Monitor.")?.id(),
    origin_monitor_id
  );
  assert_eq!(workspace.config().bind_to_monitor, Some(1));
  assert_eq!(origin_child_count(&state, origin_monitor_id), 1);
  assert_eq!(count_workspace_updates(&mut event_rx), 1);

  Ok(())
}

fn origin_child_count(state: &WmState, monitor_id: uuid::Uuid) -> usize {
  state
    .monitors()
    .into_iter()
    .find(|monitor| monitor.id() == monitor_id)
    .map_or_default(|monitor| monitor.child_count())
}

/// Windows on a moved workspace are flagged for DPI adjustment and have
/// their floating placement recentered into the moved workspace.
#[test]
fn moved_workspace_windows_get_dpi_and_floating_updates(
) -> anyhow::Result<()> {
  let (mut state, _event_rx) = mock_state();

  let window = NonTilingWindow::mock().call();
  let workspace = Workspace::mock()
    .name("1".to_string())
    .non_tiling_windows(vec![window.clone()])
    .call();

  let first_monitor =
    Monitor::mock().workspaces(vec![workspace.clone()]).call();
  let second_monitor = Monitor::mock()
    .bounds(Rect::from_xy(1920, 0, 1680, 1050))
    .working_area(Rect::from_xy(1920, 0, 1680, 1000))
    .workspaces(vec![Workspace::mock().name("2".to_string()).call()])
    .call();

  attach_monitor(&state, &first_monitor)?;
  attach_monitor(&state, &second_monitor)?;

  let config = mock_config(vec![
    workspace_config("1", None),
    workspace_config("2", None),
    workspace_config("3", None),
  ]);

  update_binding(&workspace, Some(1), &mut state, &config)?;

  // Window is flagged for a DPI adjustment and its floating placement is
  // centered within the moved workspace.
  assert!(window.has_pending_dpi_adjustment());

  let placement = window.floating_placement();
  let workspace_rect = workspace.to_rect()?;
  assert_eq!(placement.center_point().x, workspace_rect.center_point().x);
  assert_eq!(placement.center_point().y, workspace_rect.center_point().y);

  Ok(())
}

/// Focusing the moved workspace's window survives the reassignment, and
/// the focused workspace becomes displayed on the target monitor.
#[test]
fn moved_workspace_preserves_focus() -> anyhow::Result<()> {
  let (mut state, _event_rx) = mock_state();

  let window = TilingWindow::mock().call();
  let workspace = Workspace::mock()
    .name("1".to_string())
    .tiling_containers(vec![window.clone().into()])
    .call();

  let first_monitor =
    Monitor::mock().workspaces(vec![workspace.clone()]).call();
  let second_monitor = Monitor::mock()
    .workspaces(vec![Workspace::mock().name("2".to_string()).call()])
    .call();

  attach_monitor(&state, &first_monitor)?;
  attach_monitor(&state, &second_monitor)?;

  set_focused_descendant(&window.clone().into(), None);

  let config = mock_config(vec![
    workspace_config("1", None),
    workspace_config("2", None),
    workspace_config("3", None),
  ]);

  update_binding(&workspace, Some(1), &mut state, &config)?;

  // Focus is preserved onto the moved window, now displayed on the
  // target monitor.
  assert_eq!(state.focused_container(), Some(window.clone().into()));
  assert_eq!(
    second_monitor
      .displayed_workspace()
      .context("Displayed workspace is missing.")?
      .id(),
    workspace.id()
  );

  Ok(())
}
