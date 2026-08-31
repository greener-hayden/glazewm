use anyhow::Context;
use tracing::info;

use super::activate_workspace;
use crate::{
  animation_manager::SlideDirection,
  commands::{
    container::set_focused_descendant, workspace::deactivate_workspace,
  },
  models::{Workspace, WorkspaceTarget},
  traits::CommonGetters,
  user_config::UserConfig,
  wm_state::WmState,
};

/// Focuses a workspace by a given target.
///
/// This target can be a workspace name, the most recently focused
/// workspace, the next workspace, the previous workspace, or the workspace
/// in a given direction from the currently focused workspace.
///
/// The workspace will be activated if it isn't already active.
pub fn focus_workspace(
  target: WorkspaceTarget,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  // User-initiated workspace focus supersedes any deferred follow.
  state.cancel_pending_follow();

  let focused_workspace = state
    .focused_container()
    .and_then(|focused| focused.workspace())
    .context("No workspace is currently focused.")?;

  let (target_workspace_name, target_workspace) =
    state.workspace_by_target(&focused_workspace, target, config)?;

  // Retrieve or activate the target workspace by its name.
  let target_workspace = match target_workspace {
    Some(_) => anyhow::Ok(target_workspace),
    _ => match target_workspace_name {
      Some(name) => {
        activate_workspace(Some(&name), None, state, config)?;

        Ok(state.workspace_by_name(&name))
      }
      _ => Ok(None),
    },
  }?;

  if let Some(target_workspace) = target_workspace {
    info!("Focusing workspace: {target_workspace}");

    // Get the currently displayed workspace on the same monitor that the
    // workspace to focus is on.
    let monitor = target_workspace.monitor().context("No monitor.")?;

    let displayed_workspace = monitor
      .displayed_workspace()
      .context("No workspace is currently displayed.")?;

    // Set focus to whichever window last had focus in workspace. If the
    // workspace has no windows, then set focus to the workspace itself.
    let container_to_focus = target_workspace
      .descendant_focus_order()
      .next()
      .unwrap_or_else(|| target_workspace.clone().into());

    set_focused_descendant(&container_to_focus, None);
    state.pending_sync.queue_focus_change();

    // Refocusing the workspace already on screen moves focus without
    // changing what is displayed. Sliding it, redrawing both workspaces
    // and collecting an empty one are all answers to a switch that did
    // not happen — and `SlideDirection::between` yields a direction even
    // for equal indices, so the slide fires and every window on the
    // monitor travels a screen width and back for a no-op.
    if displayed_workspace.id() != target_workspace.id() {
      // Which way the content travels, ordered the way `Next`/`Previous`
      // order workspaces: by position in the configured list, not by
      // activation order. Moving to a later workspace sends the old
      // content left and brings the new one in from the right, as a pager
      // does.
      //
      // `None` when either workspace is absent from the config, which is
      // the case for a workspace created on the fly. There is no
      // defensible direction to slide then, so that switch stays a cut.
      let slide = config
        .value
        .animations
        .workspace_switch
        .as_ref()
        .and_then(|_| {
          let names = &config.value.workspaces;
          let index_of = |workspace: &Workspace| {
            let name = workspace.config().name.clone();
            names.iter().position(|entry| entry.name == name)
          };

          Some(SlideDirection::between(
            index_of(&displayed_workspace)?,
            index_of(&target_workspace)?,
          ))
        });

      // Display the workspace to switch focus to. A switch is not a move,
      // so the move animation is still wrong for it: with no slide
      // configured this stays a hard cut, which is what upstream does
      // unconditionally.
      state
        .pending_sync
        .set_skip_animations(slide.is_none())
        .set_workspace_slide(slide, Some(monitor.id()))
        .queue_container_to_redraw(displayed_workspace)
        .queue_container_to_redraw(target_workspace.clone());

      // Get empty workspace to destroy (if one is found). Cannot destroy
      // empty workspaces if they're the only workspace on the monitor.
      //
      // Scoped to the monitor that switched. Searching every monitor lets
      // a switch here collect a stray empty workspace over there, and
      // that deactivation is a second workspace transition — which the
      // other monitor's windows answer by sliding off screen and back,
      // for a switch they were never part of.
      let workspace_to_destroy =
        monitor.workspaces().into_iter().find(|workspace| {
          !workspace.config().keep_alive
            && !workspace.has_children()
            && !workspace.is_displayed()
        });

      if let Some(workspace) = workspace_to_destroy {
        deactivate_workspace(workspace, state)?;
      }
    }

    // Save the currently focused workspace as recent.
    state.recent_workspace_name = Some(focused_workspace.config().name);
    state.pending_sync.queue_cursor_jump();
  }

  Ok(())
}
