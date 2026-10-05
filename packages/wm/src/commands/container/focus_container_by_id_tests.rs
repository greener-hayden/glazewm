use tokio::sync::mpsc::UnboundedReceiver;
use wm_common::{ParsedConfig, WmEvent};

use super::*;
use crate::{
  models::{Monitor, TilingWindow, Workspace},
  test_utils::{attach_monitor, mock_state},
  traits::CommonGetters,
  user_config::UserConfig,
};

/// State with a single monitor: displayed workspace "1" with windows "a"
/// and "d" ("d" focused), hidden workspace "2" with windows "b" and "c"
/// ("c" focused within the hidden workspace).
///
/// Returns the unfocused window of each workspace: "b" (hidden), then "a"
/// (displayed).
fn mock_single_monitor_state() -> anyhow::Result<(
  WmState,
  UnboundedReceiver<WmEvent>,
  Monitor,
  TilingWindow,
  TilingWindow,
)> {
  let (state, event_rx) = mock_state();

  let window_a = TilingWindow::mock().title("a".to_string()).call();
  let window_d = TilingWindow::mock().title("d".to_string()).call();
  let window_b = TilingWindow::mock().title("b".to_string()).call();
  let window_c = TilingWindow::mock().title("c".to_string()).call();

  let first = Workspace::mock()
    .name("1".to_string())
    .tiling_containers(vec![
      window_a.clone().into(),
      window_d.clone().into(),
    ])
    .call();
  let second = Workspace::mock()
    .name("2".to_string())
    .tiling_containers(vec![
      window_b.clone().into(),
      window_c.clone().into(),
    ])
    .call();

  let monitor = Monitor::mock().workspaces(vec![first, second]).call();

  attach_monitor(&state, &monitor)?;

  // Focus window "c" within the hidden workspace, then window "d" on the
  // displayed workspace.
  set_focused_descendant(&window_c.clone().into(), None);
  set_focused_descendant(&window_d.clone().into(), None);

  Ok((state, event_rx, monitor, window_b, window_a))
}

fn mock_config() -> UserConfig {
  UserConfig::mock(ParsedConfig::default())
}

/// Focusing a container on a hidden workspace displays the workspace
/// through the established switch path, then focuses the requested
/// container, not the workspace's previously focused window.
#[test]
fn hidden_workspace_target_switches_then_focuses_request(
) -> anyhow::Result<()> {
  let (mut state, _event_rx, monitor, window_b, _window_a) =
    mock_single_monitor_state()?;
  let config = mock_config();

  let first_id = state
    .workspace_by_name("1")
    .context("Workspace 1 is missing.")?
    .id();
  let second = state
    .workspace_by_name("2")
    .context("Workspace 2 is missing.")?;
  let second_id = second.id();

  focus_container_by_id(&window_b.id(), &mut state, &config)?;

  // The requested container is the final focus target, not window "c".
  assert_eq!(state.focused_container(), Some(window_b.clone().into()));

  // The target workspace became displayed via the switch path.
  assert!(second.is_displayed());

  // The displaced and target workspaces are both queued for redraw.
  let redraw = state.pending_sync.containers_to_redraw();
  assert!(redraw.contains_key(&first_id));
  assert!(redraw.contains_key(&second_id));

  // A workspace switch is recorded for the monitor. With no slide
  // animation configured, the switch cuts.
  assert!(state.pending_sync.should_skip_animations_for(monitor.id()));

  assert!(state.pending_sync.needs_focus_update());
  assert!(state.pending_sync.needs_cursor_jump());

  Ok(())
}

/// Focusing a hidden workspace container itself by id switches to it
/// through the established switch path; focus lands on the workspace's
/// focused leaf.
#[test]
fn hidden_workspace_container_target_switches_to_it() -> anyhow::Result<()>
{
  let (mut state, _event_rx, monitor, _window_b, _window_a) =
    mock_single_monitor_state()?;
  let config = mock_config();

  let second = state
    .workspace_by_name("2")
    .context("Workspace 2 is missing.")?;
  let first_id = state
    .workspace_by_name("1")
    .context("Workspace 1 is missing.")?
    .id();

  focus_container_by_id(&second.id(), &mut state, &config)?;

  // The hidden workspace container is displayed via the switch path.
  assert!(second.is_displayed());

  // Focus lands within the requested workspace.
  let focused =
    state.focused_container().context("No focused container.")?;
  assert_eq!(
    focused
      .workspace()
      .context("Focused container has no workspace.")?
      .id(),
    second.id()
  );

  // The displaced and target workspaces are both queued for redraw with
  // a recorded switch.
  let redraw = state.pending_sync.containers_to_redraw();
  assert!(redraw.contains_key(&first_id));
  assert!(redraw.contains_key(&second.id()));
  assert!(state.pending_sync.should_skip_animations_for(monitor.id()));

  Ok(())
}

/// Focusing a container within the already displayed workspace doesn't
/// switch or queue a redraw.
#[test]
fn displayed_workspace_target_focuses_without_switch() -> anyhow::Result<()>
{
  let (mut state, _event_rx, monitor, _window_b, window_a) =
    mock_single_monitor_state()?;
  let config = mock_config();

  focus_container_by_id(&window_a.id(), &mut state, &config)?;

  let first = state
    .workspace_by_name("1")
    .context("Workspace 1 is missing.")?;

  // Focus moved without a workspace switch.
  assert_eq!(state.focused_container(), Some(window_a.clone().into()));
  assert!(first.is_displayed());
  assert!(state.pending_sync.containers_to_redraw().is_empty());
  assert!(!state.pending_sync.should_skip_animations_for(monitor.id()));
  assert!(state.pending_sync.needs_focus_update());

  Ok(())
}

/// Focusing a container on a hidden workspace of another monitor switches
/// only that monitor's displayed workspace.
#[test]
fn hidden_workspace_on_other_monitor_switches_only_that_monitor(
) -> anyhow::Result<()> {
  let (mut state, _event_rx) = mock_state();

  let window_a = TilingWindow::mock().title("a".to_string()).call();
  let window_c = TilingWindow::mock().title("c".to_string()).call();
  let window_b = TilingWindow::mock().title("b".to_string()).call();

  let first = Workspace::mock()
    .name("1".to_string())
    .tiling_containers(vec![window_a.clone().into()])
    .call();
  let third = Workspace::mock().name("3".to_string()).call();
  let second = Workspace::mock()
    .name("2".to_string())
    .tiling_containers(vec![window_c.clone().into()])
    .call();
  let fourth = Workspace::mock()
    .name("4".to_string())
    .tiling_containers(vec![window_b.clone().into()])
    .call();

  let first_monitor = Monitor::mock()
    .workspaces(vec![first.clone(), third])
    .call();
  let second_monitor = Monitor::mock()
    .workspaces(vec![second.clone(), fourth.clone()])
    .call();

  attach_monitor(&state, &first_monitor)?;
  attach_monitor(&state, &second_monitor)?;

  set_focused_descendant(&window_a.clone().into(), None);

  let config = mock_config();

  focus_container_by_id(&window_b.id(), &mut state, &config)?;

  // Requested container focused on the now displayed target workspace.
  assert_eq!(state.focused_container(), Some(window_b.clone().into()));
  assert!(fourth.is_displayed());

  // The other monitor's displayed workspace is untouched.
  assert!(first.is_displayed());

  // Redraws and the switch are scoped to the switching monitor.
  let redraw = state.pending_sync.containers_to_redraw();
  assert!(redraw.contains_key(&second.id()));
  assert!(redraw.contains_key(&fourth.id()));
  assert!(!redraw.contains_key(&first.id()));
  assert!(!state
    .pending_sync
    .should_skip_animations_for(first_monitor.id()));
  assert!(state
    .pending_sync
    .should_skip_animations_for(second_monitor.id()));

  Ok(())
}

/// Focusing a container without a workspace (a monitor) doesn't switch
/// workspaces.
#[test]
fn monitor_target_focuses_without_workspace_switch() -> anyhow::Result<()>
{
  let (mut state, _event_rx, monitor, _window_b, _window_a) =
    mock_single_monitor_state()?;
  let config = mock_config();

  let first = state
    .workspace_by_name("1")
    .context("Workspace 1 is missing.")?;

  focus_container_by_id(&monitor.id(), &mut state, &config)?;

  // No switch happened; the displayed workspace is unchanged and no
  // redraw was queued.
  assert!(first.is_displayed());
  assert!(state.pending_sync.containers_to_redraw().is_empty());
  assert!(state.pending_sync.needs_focus_update());

  Ok(())
}
