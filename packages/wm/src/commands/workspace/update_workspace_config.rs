use std::collections::HashSet;

use anyhow::Context;
use tracing::warn;
use uuid::Uuid;
use wm_common::{InvokeUpdateWorkspaceConfig, WmEvent, WorkspaceConfig};

use super::sort_workspaces;
use crate::{
  commands::monitor::{
    move_workspace_between_monitors, move_workspace_to_monitor,
  },
  models::{Monitor, Workspace},
  traits::CommonGetters,
  user_config::UserConfig,
  wm_state::WmState,
};

/// Applies a partial config update to an active workspace.
///
/// A changed `bind_to_monitor` moves the workspace to that monitor when
/// it can (see `reassign_workspaces_to_bound_monitors`). Emits a single
/// `WmEvent::WorkspaceUpdated` either way.
pub fn update_workspace_config(
  workspace: &Workspace,
  state: &mut WmState,
  config: &UserConfig,
  new_config: &InvokeUpdateWorkspaceConfig,
) -> anyhow::Result<()> {
  let current_config = workspace.config();

  // Validate the workspace name change.
  if let Some(new_name) = &new_config.name {
    if new_name != &current_config.name {
      if let Some(_other_workspace) = state.workspace_by_name(new_name) {
        anyhow::bail!("The workspace \"{}\" already exists", new_name);
      }
    }
  }

  // Update the config with the incoming values.
  let updated_config = WorkspaceConfig {
    name: new_config
      .name
      .clone()
      .unwrap_or(current_config.name.clone()),
    display_name: new_config
      .display_name
      .clone()
      .or(current_config.display_name.clone()),
    bind_to_monitor: new_config
      .bind_to_monitor
      .or(current_config.bind_to_monitor),
    keep_alive: new_config.keep_alive.unwrap_or(current_config.keep_alive),
  };

  workspace.set_config(updated_config);

  // A workspace with a move target is sorted and announced by the
  // re-assignment, so each update emits a single event.
  if bound_reassignment_target(workspace, state).is_none() {
    let monitor = workspace.monitor().context("No monitor.")?;
    sort_workspaces(&monitor, config)?;

    state.emit_event(WmEvent::WorkspaceUpdated {
      updated_workspace: workspace.to_dto()?,
    });
  }

  reassign_workspaces_to_bound_monitors(
    std::slice::from_ref(workspace),
    state,
    config,
  )
}

/// Moves active workspaces to the monitors named by their
/// `bind_to_monitor` configs.
///
/// A move must never leave a monitor without a workspace, so moves are
/// taken in order of what they cost the monitor left behind:
///
/// 1. A workspace with siblings moves freely.
/// 2. Workspaces that are alone on their monitors and move into each
///    other's places (e.g. two bound workspaces swapping monitors) are
///    exchanged as a group.
/// 3. A workspace that is alone moves if an inactive workspace config can
///    be activated in its place.
///
/// Any other workspace stays where it is and keeps its binding, to be
/// honoured by a later re-assignment (e.g. a config reload, or the bound
/// monitor being added).
///
/// Emits one `WmEvent::WorkspaceUpdated` per workspace that has a move
/// target, whether it moved or stayed. Workspaces without a move target
/// (see `bound_reassignment_target`) are ignored.
pub fn reassign_workspaces_to_bound_monitors(
  workspaces: &[Workspace],
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let mut pending = workspaces
    .iter()
    .filter_map(|workspace| {
      let target_monitor = bound_reassignment_target(workspace, state)?;
      Some((workspace.clone(), target_monitor))
    })
    .collect::<Vec<_>>();

  // Every move can unblock another (it gives its target monitor a
  // sibling for a workspace waiting to leave), so the cheapest available
  // move is re-evaluated after each one.
  loop {
    let free_move = pending
      .iter()
      .position(|(workspace, _)| has_sibling_workspace(workspace));

    if let Some(index) = free_move {
      let (workspace, target_monitor) = pending.remove(index);
      move_workspace_to_monitor(
        &workspace,
        &target_monitor,
        state,
        config,
      )?;
      continue;
    }

    // Every pending workspace is now alone on its monitor.
    let exchange = take_exchange(&mut pending);

    if !exchange.is_empty() {
      exchange_workspaces(&exchange, state, config)?;
      continue;
    }

    let refilled_move = pending.iter().position(|(workspace, _)| {
      can_refill_monitor(workspace, state, config)
    });

    let Some(index) = refilled_move else {
      break;
    };

    let (workspace, target_monitor) = pending.remove(index);
    move_workspace_to_monitor(&workspace, &target_monitor, state, config)?;
  }

  for (workspace, _) in &pending {
    warn!(
      "Keeping workspace \"{}\" on its monitor. No workspace is available \
       to take its place.",
      workspace.config().name
    );

    let monitor = workspace.monitor().context("No monitor.")?;
    sort_workspaces(&monitor, config)?;

    state.emit_event(WmEvent::WorkspaceUpdated {
      updated_workspace: workspace.to_dto()?,
    });
  }

  Ok(())
}

/// Removes and returns the pending moves that refill each other's
/// monitors.
///
/// Expects every pending workspace to be alone on its monitor. A move
/// belongs to the exchange when its origin monitor is the target of
/// another move in the exchange. A move that would only be refilled by
/// one left out (an open chain) is left out as well, so membership is
/// narrowed until it stops changing.
fn take_exchange(
  pending: &mut Vec<(Workspace, Monitor)>,
) -> Vec<(Workspace, Monitor)> {
  let mut exchange = std::mem::take(pending);

  loop {
    let targets = exchange
      .iter()
      .map(|(_, target_monitor)| target_monitor.id())
      .collect::<HashSet<Uuid>>();

    let (refilled, unfilled): (Vec<_>, Vec<_>) =
      exchange.into_iter().partition(|(workspace, _)| {
        workspace
          .monitor()
          .is_some_and(|origin| targets.contains(&origin.id()))
      });

    let is_settled = unfilled.is_empty();
    pending.extend(unfilled);
    exchange = refilled;

    if is_settled {
      return exchange;
    }
  }
}

/// Moves workspaces that refill each other's monitors (see
/// `take_exchange`).
///
/// A monitor is empty between its workspace leaving and the next one
/// arriving, which `move_workspace_to_monitor` cannot pass through: it
/// would activate a workspace there, or fail for want of one.
fn exchange_workspaces(
  exchange: &[(Workspace, Monitor)],
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  for (workspace, target_monitor) in exchange {
    let origin_monitor =
      move_workspace_between_monitors(workspace, target_monitor, state)?;

    // An origin still empty is redrawn by the move that refills it.
    if let Some(displayed_workspace) = origin_monitor.displayed_workspace()
    {
      state
        .pending_sync
        .queue_container_to_redraw(displayed_workspace);
    }

    sort_workspaces(target_monitor, config)?;

    state.emit_event(WmEvent::WorkspaceUpdated {
      updated_workspace: workspace.to_dto()?,
    });
  }

  // Later commands assume a displayed workspace per monitor, so a broken
  // exchange is reported here rather than wherever that first surfaces.
  for (_, monitor) in exchange {
    anyhow::ensure!(
      monitor.child_count() > 0,
      "Monitor {} was left without a workspace.",
      monitor.id()
    );
  }

  Ok(())
}

/// Resolves the monitor a workspace should move to per its
/// `bind_to_monitor` config.
///
/// Returns `None` when the workspace has no binding, is already on the
/// bound monitor, or no monitor with the bound index exists.
pub fn bound_reassignment_target(
  workspace: &Workspace,
  state: &WmState,
) -> Option<Monitor> {
  let current_monitor = workspace.monitor()?;
  let bound_monitor_index = workspace.config().bind_to_monitor?;

  state
    .monitors()
    .into_iter()
    .find(|monitor| monitor.index() == bound_monitor_index as usize)
    .filter(|monitor| monitor.id() != current_monitor.id())
}

/// Whether another workspace shares the workspace's monitor.
fn has_sibling_workspace(workspace: &Workspace) -> bool {
  workspace
    .monitor()
    .is_some_and(|monitor| monitor.child_count() > 1)
}

/// Whether an inactive workspace config could replace the workspace on
/// its monitor.
///
/// Mirrors how `activate_workspace` picks a config when given a monitor
/// and no name, which is what `move_workspace_to_monitor` does for a
/// monitor it emptied.
fn can_refill_monitor(
  workspace: &Workspace,
  state: &WmState,
  config: &UserConfig,
) -> bool {
  let active_workspaces = state.workspaces();

  workspace.monitor().is_some_and(|monitor| {
    config
      .workspace_config_for_monitor(&monitor, &active_workspaces)
      .or_else(|| {
        config.next_inactive_workspace_config(&active_workspaces)
      })
      .is_some()
  })
}

#[cfg(test)]
#[path = "update_workspace_config_tests.rs"]
mod tests;
