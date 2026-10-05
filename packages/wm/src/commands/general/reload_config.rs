use anyhow::Context;
use tracing::{info, warn};
use wm_common::{WindowRuleEvent, WmEvent};

use crate::{
  commands::{
    window::run_window_rules,
    workspace::{
      bound_reassignment_target, reassign_workspaces_to_bound_monitors,
      sort_workspaces,
    },
  },
  traits::{CommonGetters, TilingSizeGetters, WindowGetters},
  user_config::UserConfig,
  wm::WindowManager,
  wm_state::WmState,
};

pub fn reload_config(
  state: &mut WmState,
  config: &mut UserConfig,
) -> anyhow::Result<()> {
  info!("Config reloaded.");

  // Re-evaluate user config file and set its values in state.
  config.reload()?;

  // Re-run window rules on all active windows.
  for window in state.windows() {
    window.set_done_window_rules(Vec::new());
    run_window_rules(window, &WindowRuleEvent::Manage, state, config)?;
  }

  update_workspace_configs(state, config)?;

  update_container_gaps(state, config);

  state.native_sync.invalidate();
  state.pending_sync.queue_all_effects_update();

  // Clear active binding modes.
  state.binding_modes = Vec::new();

  // Redraw full container tree.
  state
    .pending_sync
    .queue_container_to_redraw(state.root_container.clone());

  // Emit the updated config.
  state.emit_event(WmEvent::UserConfigChanged {
    config_path: config
      .path
      .to_str()
      .context("Invalid config path.")?
      .to_string(),
    config_string: config.value_str.clone(),
    parsed_config: config.value.clone(),
  });

  // Run config reload commands.
  WindowManager::run_commands(
    &config.value.general.config_reload_commands.clone(),
    state.focused_container().context("No focused container.")?,
    state,
    config,
  )?;

  Ok(())
}

/// Update configs of active workspaces.
fn update_workspace_configs(
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let workspaces = state.workspaces();

  for workspace in &workspaces {
    let monitor = workspace.monitor().context("No monitor.")?;

    let workspace_config = config
      .value
      .workspaces
      .iter()
      .find(|config| config.name == workspace.config().name)
      .or_else(|| {
        // When the workspace config is not found, the current name of the
        // workspace has been removed. So, we reassign the first suitable
        // workspace config to the workspace.
        config
          .workspace_config_for_monitor(&monitor, &workspaces)
          .or_else(|| config.next_inactive_workspace_config(&workspaces))
      });

    match workspace_config {
      None => {
        warn!(
          "Unable to update workspace config. No available workspace configs."
        );
      }
      Some(workspace_config) => {
        if *workspace_config != workspace.config() {
          workspace.set_config(workspace_config.clone());

          // Workspaces without a move target keep their position and are
          // re-sorted here. Workspaces with a target are re-assigned
          // together after the loop.
          if bound_reassignment_target(workspace, state).is_none() {
            sort_workspaces(&monitor, config)?;

            state.emit_event(WmEvent::WorkspaceUpdated {
              updated_workspace: workspace.to_dto()?,
            });
          }
        }
      }
    }
  }

  // Re-assign workspaces that are not on their bound monitors: those
  // whose configs just changed, and any still waiting from an earlier
  // re-assignment. Handled as one batch so that moves which would empty
  // their origin monitor (e.g. bound workspaces swapping monitors)
  // resolve against each other.
  reassign_workspaces_to_bound_monitors(&workspaces, state, config)?;

  Ok(())
}

/// Updates outer gap of workspaces and inner gaps of tiling containers.
fn update_container_gaps(state: &mut WmState, config: &UserConfig) {
  let tiling_containers = state
    .root_container
    .self_and_descendants()
    .filter_map(|container| container.as_tiling_container().ok());

  for container in tiling_containers {
    container.set_gaps_config(config.value.gaps.clone());
  }

  for workspace in state.workspaces() {
    workspace.set_gaps_config(config.value.gaps.clone());
  }
}

#[cfg(test)]
#[path = "reload_config_tests.rs"]
mod tests;
