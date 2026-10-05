use tokio::sync::mpsc::UnboundedReceiver;
use wm_common::{ParsedConfig, WorkspaceConfig};

use super::*;
use crate::{
  commands::container::{attach_container, set_focused_descendant},
  models::{Monitor, TilingWindow, Workspace},
  test_utils::{attach_monitor, mock_state},
  traits::CommonGetters,
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

fn monitor_by_index(state: &WmState, index: usize) -> Option<Monitor> {
  state
    .monitors()
    .into_iter()
    .find(|monitor| monitor.index() == index)
}

/// State with three monitors, each holding one workspace ("1", "2", "3").
fn mock_three_monitor_state(
) -> anyhow::Result<(WmState, UnboundedReceiver<WmEvent>)> {
  let (state, event_rx) = mock_state();

  let first_monitor = Monitor::mock()
    .workspaces(vec![Workspace::mock().name("1".to_string()).call()])
    .call();
  let second_monitor = Monitor::mock()
    .workspaces(vec![Workspace::mock().name("2".to_string()).call()])
    .call();
  let third_monitor = Monitor::mock()
    .workspaces(vec![Workspace::mock().name("3".to_string()).call()])
    .call();

  attach_monitor(&state, &first_monitor)?;
  attach_monitor(&state, &second_monitor)?;
  attach_monitor(&state, &third_monitor)?;

  Ok((state, event_rx))
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

/// A workspace whose binding changed during reload moves to its bound
/// monitor with its origin monitor refilled.
#[test]
fn reload_moves_workspace_to_newly_bound_monitor() -> anyhow::Result<()> {
  let (mut state, mut event_rx) = mock_two_monitor_state()?;
  let config = mock_config(vec![
    workspace_config("1", Some(1)),
    workspace_config("2", None),
    workspace_config("3", None),
  ]);

  update_workspace_configs(&mut state, &config)?;

  // Workspace "1" moved to monitor 1; monitor 0 refilled with "3".
  let workspace = state
    .workspace_by_name("1")
    .context("Workspace 1 is missing.")?;
  assert_eq!(workspace.monitor().context("Monitor.")?.index(), 1,);
  let origin_monitor =
    monitor_by_index(&state, 0).context("Monitor 0 is missing.")?;
  assert_eq!(origin_monitor.child_count(), 1);
  assert_eq!(origin_monitor.workspaces()[0].config().name, "3");

  // Exactly one update event for the moved workspace (any activation of
  // the refill workspace emits a separate event).
  assert_eq!(count_workspace_updates(&mut event_rx), 1);

  Ok(())
}

/// A binding whose monitor doesn't exist keeps the workspace where it is
/// and retains the configured binding.
#[test]
fn reload_keeps_workspace_when_bound_monitor_is_absent(
) -> anyhow::Result<()> {
  let (mut state, mut event_rx) = mock_two_monitor_state()?;
  let config = mock_config(vec![
    workspace_config("1", Some(9)),
    workspace_config("2", None),
  ]);

  update_workspace_configs(&mut state, &config)?;

  let workspace = state
    .workspace_by_name("1")
    .context("Workspace 1 is missing.")?;
  assert_eq!(workspace.monitor().context("Monitor.")?.index(), 0);
  assert_eq!(workspace.config().bind_to_monitor, Some(9));
  assert_eq!(count_workspace_updates(&mut event_rx), 1);

  Ok(())
}

/// Unchanged workspace configs cause no moves and no events.
#[test]
fn reload_skips_unchanged_workspaces() -> anyhow::Result<()> {
  let (mut state, mut event_rx) = mock_two_monitor_state()?;
  let config = mock_config(vec![
    workspace_config("1", None),
    workspace_config("2", None),
  ]);

  update_workspace_configs(&mut state, &config)?;

  let first = state
    .workspace_by_name("1")
    .context("Workspace 1 is missing.")?;
  let second = state
    .workspace_by_name("2")
    .context("Workspace 2 is missing.")?;
  assert_eq!(first.monitor().context("Monitor.")?.index(), 0);
  assert_eq!(second.monitor().context("Monitor.")?.index(), 1);
  assert_eq!(count_workspace_updates(&mut event_rx), 0);

  Ok(())
}

/// Bound workspaces swapping monitors take each other's places. No
/// inactive workspace config is activated to cover for either of them,
/// though one is available.
#[test]
fn reload_swaps_bound_workspaces_between_monitors() -> anyhow::Result<()> {
  let (mut state, mut event_rx) = mock_two_monitor_state()?;
  let config = mock_config(vec![
    workspace_config("1", Some(1)),
    workspace_config("2", Some(0)),
    workspace_config("3", None),
  ]);

  update_workspace_configs(&mut state, &config)?;

  let first = state
    .workspace_by_name("1")
    .context("Workspace 1 is missing.")?;
  let second = state
    .workspace_by_name("2")
    .context("Workspace 2 is missing.")?;

  // Swapped: "1" now on monitor 1, "2" on monitor 0.
  assert_eq!(first.monitor().context("Monitor.")?.index(), 1);
  assert_eq!(second.monitor().context("Monitor.")?.index(), 0);

  // Each monitor holds only the workspace bound to it.
  for index in 0..2 {
    let monitor =
      monitor_by_index(&state, index).context("Monitor is missing.")?;
    assert_eq!(monitor.child_count(), 1);
  }
  assert!(state.workspace_by_name("3").is_none());

  // One update event per moved workspace.
  assert_eq!(count_workspace_updates(&mut event_rx), 2);

  Ok(())
}

/// A workspace that can't leave its monitor empty moves once another
/// move has given it a sibling, whatever order the two are listed in.
#[test]
fn reload_moves_workspace_unblocked_by_another_move() -> anyhow::Result<()>
{
  let (mut state, mut event_rx) = mock_state();

  let first_monitor = Monitor::mock()
    .workspaces(vec![Workspace::mock().name("1".to_string()).call()])
    .call();
  let second_monitor = Monitor::mock()
    .workspaces(vec![
      Workspace::mock().name("2".to_string()).call(),
      Workspace::mock().name("3".to_string()).call(),
    ])
    .call();

  attach_monitor(&state, &first_monitor)?;
  attach_monitor(&state, &second_monitor)?;

  // Workspace "1" is alone on monitor 0 with nothing to refill it, until
  // workspace "2" arrives from monitor 1.
  let config = mock_config(vec![
    workspace_config("1", Some(1)),
    workspace_config("2", Some(0)),
    workspace_config("3", None),
  ]);

  update_workspace_configs(&mut state, &config)?;

  let names_on = |index: usize| -> anyhow::Result<Vec<String>> {
    Ok(
      monitor_by_index(&state, index)
        .context("Monitor is missing.")?
        .workspaces()
        .into_iter()
        .map(|workspace| workspace.config().name)
        .collect(),
    )
  };

  assert_eq!(names_on(0)?, vec!["2".to_string()]);
  assert_eq!(names_on(1)?, vec!["1".to_string(), "3".to_string()]);

  // One update event per moved workspace.
  assert_eq!(count_workspace_updates(&mut event_rx), 2);

  Ok(())
}

/// Reciprocal bindings on single-workspace monitors with no inactive
/// workspace config resolve as one atomic exchange: each workspace
/// reaches its bound monitor, no monitor is left empty, and focus stays
/// on a displayed workspace.
#[test]
fn reload_swaps_single_workspace_monitors_atomically() -> anyhow::Result<()>
{
  let (mut state, mut event_rx) = mock_state();

  let window_a = TilingWindow::mock().call();
  let window_b = TilingWindow::mock().call();

  let first = Workspace::mock()
    .name("1".to_string())
    .tiling_containers(vec![window_a.clone().into()])
    .call();
  let second = Workspace::mock()
    .name("2".to_string())
    .tiling_containers(vec![window_b.clone().into()])
    .call();

  let first_monitor =
    Monitor::mock().workspaces(vec![first.clone()]).call();
  let second_monitor =
    Monitor::mock().workspaces(vec![second.clone()]).call();

  attach_monitor(&state, &first_monitor)?;
  attach_monitor(&state, &second_monitor)?;

  set_focused_descendant(&window_a.clone().into(), None);

  // Reciprocal bindings; no inactive workspace config exists.
  let config = mock_config(vec![
    workspace_config("1", Some(1)),
    workspace_config("2", Some(0)),
  ]);

  update_workspace_configs(&mut state, &config)?;

  // Each workspace reached its bound monitor.
  let workspace_one = state
    .workspace_by_name("1")
    .context("Workspace 1 is missing.")?;
  let workspace_two = state
    .workspace_by_name("2")
    .context("Workspace 2 is missing.")?;
  assert_eq!(workspace_one.monitor().context("Monitor.")?.index(), 1);
  assert_eq!(workspace_two.monitor().context("Monitor.")?.index(), 0);

  // No monitor was left empty; each displays its (single) workspace.
  let origin_monitor =
    monitor_by_index(&state, 0).context("Monitor 0 is missing.")?;
  let target_monitor =
    monitor_by_index(&state, 1).context("Monitor 1 is missing.")?;
  assert_eq!(origin_monitor.child_count(), 1);
  assert_eq!(target_monitor.child_count(), 1);
  assert_eq!(
    origin_monitor
      .displayed_workspace()
      .context("No displayed workspace.")?
      .id(),
    workspace_two.id()
  );
  assert_eq!(
    target_monitor
      .displayed_workspace()
      .context("No displayed workspace.")?
      .id(),
    workspace_one.id()
  );

  // Focus remains on a displayed workspace.
  let focused =
    state.focused_container().context("No focused container.")?;
  assert!(focused
    .workspace()
    .context("Focused container has no workspace.")?
    .is_displayed());

  // One update event per moved workspace.
  assert_eq!(count_workspace_updates(&mut event_rx), 2);

  Ok(())
}

/// A reload with every live config already matching the config file does
/// nothing: no moves, no events.
#[test]
fn reload_after_swap_is_a_no_op() -> anyhow::Result<()> {
  let (mut state, mut event_rx) = mock_state();

  let first = Workspace::mock()
    .name("1".to_string())
    .tiling_containers(vec![TilingWindow::mock().call().into()])
    .call();
  let second = Workspace::mock()
    .name("2".to_string())
    .tiling_containers(vec![TilingWindow::mock().call().into()])
    .call();

  let first_monitor =
    Monitor::mock().workspaces(vec![first.clone()]).call();
  let second_monitor =
    Monitor::mock().workspaces(vec![second.clone()]).call();

  attach_monitor(&state, &first_monitor)?;
  attach_monitor(&state, &second_monitor)?;

  let config = mock_config(vec![
    workspace_config("1", Some(1)),
    workspace_config("2", Some(0)),
  ]);

  update_workspace_configs(&mut state, &config)?;
  assert_eq!(count_workspace_updates(&mut event_rx), 2);

  // A second reload finds every live config already matching and does
  // nothing.
  update_workspace_configs(&mut state, &config)?;
  assert_eq!(count_workspace_updates(&mut event_rx), 0);

  let workspace_one = state
    .workspace_by_name("1")
    .context("Workspace 1 is missing.")?;
  let workspace_two = state
    .workspace_by_name("2")
    .context("Workspace 2 is missing.")?;
  assert_eq!(workspace_one.monitor().context("Monitor.")?.index(), 1);
  assert_eq!(workspace_two.monitor().context("Monitor.")?.index(), 0);

  Ok(())
}

/// An open chain with no inactive workspace config (no move refills the
/// first monitor) retains every workspace on its monitor with its binding
/// intact: no monitor is emptied and no post-mutation error occurs.
#[test]
fn reload_retains_open_chain_workspaces() -> anyhow::Result<()> {
  let (mut state, mut event_rx) = mock_three_monitor_state()?;
  let config = mock_config(vec![
    workspace_config("1", Some(1)),
    workspace_config("2", Some(2)),
    workspace_config("3", None),
  ]);

  update_workspace_configs(&mut state, &config)?;

  // Every workspace stayed on its monitor and kept its new binding.
  let first = state
    .workspace_by_name("1")
    .context("Workspace 1 is missing.")?;
  let second = state
    .workspace_by_name("2")
    .context("Workspace 2 is missing.")?;
  let third = state
    .workspace_by_name("3")
    .context("Workspace 3 is missing.")?;
  assert_eq!(first.monitor().context("Monitor.")?.index(), 0);
  assert_eq!(second.monitor().context("Monitor.")?.index(), 1);
  assert_eq!(third.monitor().context("Monitor.")?.index(), 2);
  assert_eq!(first.config().bind_to_monitor, Some(1));
  assert_eq!(second.config().bind_to_monitor, Some(2));

  // No monitor lost or gained a workspace.
  for index in 0..3 {
    let monitor =
      monitor_by_index(&state, index).context("Monitor is missing.")?;
    assert_eq!(monitor.child_count(), 1);
  }

  // One update event per changed config; no move events.
  assert_eq!(count_workspace_updates(&mut event_rx), 2);

  Ok(())
}

/// A three-monitor binding cycle resolves as one atomic exchange: every
/// workspace reaches its bound monitor and no monitor is left empty.
#[test]
fn reload_swaps_three_monitor_cycle_atomically() -> anyhow::Result<()> {
  let (mut state, mut event_rx) = mock_three_monitor_state()?;

  let window = TilingWindow::mock().call();
  let first = state
    .workspace_by_name("1")
    .context("Workspace 1 is missing.")?;
  attach_container(&window.clone().into(), &first.clone().into(), None)?;
  set_focused_descendant(&window.clone().into(), None);

  // Three-way cycle; no inactive workspace config exists.
  let config = mock_config(vec![
    workspace_config("1", Some(1)),
    workspace_config("2", Some(2)),
    workspace_config("3", Some(0)),
  ]);

  update_workspace_configs(&mut state, &config)?;

  // Every workspace reached its bound monitor.
  let workspace_one = state
    .workspace_by_name("1")
    .context("Workspace 1 is missing.")?;
  let workspace_two = state
    .workspace_by_name("2")
    .context("Workspace 2 is missing.")?;
  let workspace_three = state
    .workspace_by_name("3")
    .context("Workspace 3 is missing.")?;
  assert_eq!(workspace_one.monitor().context("Monitor.")?.index(), 1);
  assert_eq!(workspace_two.monitor().context("Monitor.")?.index(), 2);
  assert_eq!(workspace_three.monitor().context("Monitor.")?.index(), 0);

  // No monitor was left empty.
  for index in 0..3 {
    let monitor =
      monitor_by_index(&state, index).context("Monitor is missing.")?;
    assert_eq!(monitor.child_count(), 1);
  }

  // Focus remains on a displayed workspace.
  let focused =
    state.focused_container().context("No focused container.")?;
  assert!(focused
    .workspace()
    .context("Focused container has no workspace.")?
    .is_displayed());

  // One update event per moved workspace.
  assert_eq!(count_workspace_updates(&mut event_rx), 3);

  Ok(())
}

/// An open chain with a closed tail (1→2 open, 2⇄3 cycle) keeps the open
/// workspace in place while the closed part still exchanges.
#[test]
fn reload_exchanges_closed_part_of_open_chain() -> anyhow::Result<()> {
  let (mut state, mut event_rx) = mock_three_monitor_state()?;

  // Workspaces "2" and "3" close a cycle through monitor 1; workspace
  // "1"'s origin monitor is refilled by nothing, so it is deferred.
  let config = mock_config(vec![
    workspace_config("1", Some(1)),
    workspace_config("2", Some(2)),
    workspace_config("3", Some(1)),
  ]);

  update_workspace_configs(&mut state, &config)?;

  // The open workspace stayed put and kept its binding.
  let first = state
    .workspace_by_name("1")
    .context("Workspace 1 is missing.")?;
  assert_eq!(first.monitor().context("Monitor.")?.index(), 0);
  assert_eq!(first.config().bind_to_monitor, Some(1));

  // The closed pair exchanged.
  let second = state
    .workspace_by_name("2")
    .context("Workspace 2 is missing.")?;
  let third = state
    .workspace_by_name("3")
    .context("Workspace 3 is missing.")?;
  assert_eq!(second.monitor().context("Monitor.")?.index(), 2);
  assert_eq!(third.monitor().context("Monitor.")?.index(), 1);

  // No monitor was left empty.
  for index in 0..3 {
    let monitor =
      monitor_by_index(&state, index).context("Monitor is missing.")?;
    assert_eq!(monitor.child_count(), 1);
  }

  // One update event per changed config (deferred or exchanged).
  assert_eq!(count_workspace_updates(&mut event_rx), 3);

  Ok(())
}
