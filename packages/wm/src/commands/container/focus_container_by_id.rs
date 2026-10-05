use anyhow::Context;
use uuid::Uuid;

use super::set_focused_descendant;
use crate::{
  commands::workspace::focus_workspace, models::WorkspaceTarget,
  traits::CommonGetters, user_config::UserConfig, wm_state::WmState,
};

/// Focuses a container by its id.
///
/// When the container sits on a hidden workspace, its workspace is first
/// displayed through the established workspace-switch path (transition
/// and redraw of the displaced and target workspaces), mirroring the
/// OS-focus path. The requested container is focused afterwards, so it
/// remains the final focus target.
pub fn focus_container_by_id(
  container_id: &Uuid,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let focus_target = state
    .container_by_id(*container_id)
    .context("No container with given id")?;

  // Display the target's workspace when it's hidden.
  if let Some(workspace) = focus_target.workspace() {
    if !workspace.is_displayed() {
      focus_workspace(
        WorkspaceTarget::Name(workspace.config().name.clone()),
        state,
        config,
      )?;
    }
  }

  // Set focus to the target container.
  set_focused_descendant(&focus_target, None);
  state.pending_sync.queue_focus_change().queue_cursor_jump();

  Ok(())
}

#[cfg(test)]
#[path = "focus_container_by_id_tests.rs"]
mod tests;
