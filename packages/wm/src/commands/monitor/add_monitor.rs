use anyhow::Context;
use tracing::info;
use wm_common::WmEvent;
use wm_platform::Display;

use crate::{
  commands::{
    container::{attach_container, move_container_within_tree},
    workspace::{
      activate_workspace, reassign_workspaces_to_bound_monitors,
      sort_workspaces,
    },
  },
  models::{Monitor, NativeMonitorProperties, Workspace},
  traits::{CommonGetters, PositionGetters, WindowGetters},
  user_config::UserConfig,
  wm_state::WmState,
};

pub fn add_monitor(
  native_display: Display,
  native_properties: NativeMonitorProperties,
  state: &mut WmState,
) -> anyhow::Result<Monitor> {
  // Create `Monitor` instance. This uses the working area of the monitor
  // instead of the bounds of the display. The working area excludes
  // taskbars and other reserved display space.
  let monitor = Monitor::new(native_display, native_properties);

  attach_container(
    &monitor.clone().into(),
    &state.root_container.clone().into(),
    None,
  )?;

  info!("Monitor added: {monitor}");

  state.emit_event(WmEvent::MonitorAdded {
    added_monitor: monitor.to_dto()?,
  });

  Ok(monitor)
}

/// Gives a newly added monitor the workspaces bound to it.
///
/// Activates the monitor's `keep_alive` workspaces, moves active
/// workspaces bound to it (see `reassign_workspaces_to_bound_monitors`
/// for when one stays behind), and falls back to activating any
/// workspace so the monitor is never left empty.
pub fn move_bounded_workspaces_to_new_monitor(
  monitor: &Monitor,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let keep_alive_configs = config
    .value
    .workspaces
    .iter()
    .filter(|config| {
      config.keep_alive
        && config.bind_to_monitor.is_some_and(|monitor_index| {
          monitor.index() == monitor_index as usize
        })
    })
    .collect::<Vec<_>>();

  // Activate all `keep_alive` workspaces for this monitor.
  for workspace_config in keep_alive_configs {
    if state.workspace_by_name(&workspace_config.name).is_none() {
      activate_workspace(
        Some(&workspace_config.name),
        Some(monitor.clone()),
        state,
        config,
      )?;
    }
  }

  // Move active workspaces that are bound to this monitor. Their own
  // configs decide, not the config file's: the two differ after an
  // `update-workspace-config`, and the workspace's is the newer.
  let bound_workspaces = state
    .workspaces()
    .into_iter()
    .filter(|workspace| {
      workspace
        .config()
        .bind_to_monitor
        .is_some_and(|monitor_index| {
          monitor.index() == monitor_index as usize
        })
    })
    .collect::<Vec<_>>();

  reassign_workspaces_to_bound_monitors(&bound_workspaces, state, config)?;

  // Make sure the monitor has at least one workspace. This will
  // automatically prioritize bound workspace configs and fall back to the
  // first available one if needed.
  if monitor.child_count() == 0 {
    activate_workspace(None, Some(monitor.clone()), state, config)?;
  }

  Ok(())
}

// TODO: Move to its own file once `swap-workspace` PR is merged.
// Ref: https://github.com/glzr-io/glazewm/pull/980.
pub fn move_workspace_to_monitor(
  workspace: &Workspace,
  target_monitor: &Monitor,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let origin_monitor =
    move_workspace_between_monitors(workspace, target_monitor, state)?;

  match origin_monitor.child_count() {
    0 => {
      // Prevent origin monitor from having no workspaces.
      activate_workspace(None, Some(origin_monitor), state, config)?;
    }
    _ => {
      // Redraw the workspace on the origin monitor.
      state.pending_sync.queue_container_to_redraw(
        origin_monitor
          .displayed_workspace()
          .context("No displayed workspace.")?,
      );
    }
  }

  sort_workspaces(target_monitor, config)?;

  state.emit_event(WmEvent::WorkspaceUpdated {
    updated_workspace: workspace.to_dto()?,
  });

  Ok(())
}

/// Moves a workspace to another monitor without sorting, emitting events,
/// or handling the monitor it left behind.
///
/// Moves the workspace's container, flags its windows for a DPI
/// adjustment, recenters their floating placements into the moved
/// workspace, and queues redraws of the moved workspace and of the target
/// monitor's displayed workspace.
///
/// Returns the monitor the workspace was moved from.
pub fn move_workspace_between_monitors(
  workspace: &Workspace,
  target_monitor: &Monitor,
  state: &mut WmState,
) -> anyhow::Result<Monitor> {
  let origin_monitor = workspace.monitor().context("No monitor.")?;

  move_container_within_tree(
    &workspace.clone().into(),
    &target_monitor.clone().into(),
    target_monitor.child_count(),
    state,
  )?;

  let windows = workspace
    .descendants()
    .filter_map(|descendant| descendant.as_window_container().ok());

  for window in windows {
    window.set_has_pending_dpi_adjustment(true);

    window.set_floating_placement(
      window
        .floating_placement()
        .translate_to_center(&workspace.to_rect()?),
    );
  }

  // Get currently displayed workspace on the target monitor.
  let displayed_workspace = target_monitor
    .displayed_workspace()
    .context("No displayed workspace.")?;

  state
    .pending_sync
    .queue_container_to_redraw(workspace.clone())
    .queue_container_to_redraw(displayed_workspace);

  Ok(origin_monitor)
}

#[cfg(test)]
#[path = "add_monitor_tests.rs"]
mod tests;
