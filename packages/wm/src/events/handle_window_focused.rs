use std::time::Duration;

use anyhow::Context;
use tracing::info;
use wm_common::{
  DisplayState, HideMethod, WindowRuleEvent, WindowState, WmEvent,
};
use wm_platform::NativeWindow;

use crate::{
  commands::{
    container::set_focused_descendant, window::run_window_rules,
    workspace::focus_workspace,
  },
  events::handle_window_minimize_ended,
  models::WorkspaceTarget,
  traits::{CommonGetters, WindowGetters},
  user_config::UserConfig,
  wm_state::WmState,
};

pub fn handle_window_focused(
  native_window: &NativeWindow,
  state: &mut WmState,
  config: &mut UserConfig,
) -> anyhow::Result<()> {
  // Windows can report a restored window's activation before its
  // `MinimizeEnded` event. Settle the state first; otherwise the sync
  // between the two events minimizes the window again.
  if state
    .window_from_native(native_window)
    .is_some_and(|window| window.state() == WindowState::Minimized)
  {
    handle_window_minimize_ended(native_window, state, config)?;
  }

  let found_window = state.window_from_native(native_window);
  let focused_container =
    state.focused_container().context("No focused container.")?;

  // Update the focus sync state. If the OS focused window is not same as
  // the WM's focused container, then the focus is not synced.
  state.is_focus_synced = match focused_container.as_window_container() {
    Ok(window) => *window.native() == *native_window,
    _ => native_window.is_desktop_window().unwrap_or(false),
  };

  // Handle overriding focus on close/minimize. After a window is closed
  // or minimized, the OS or the closed application might automatically
  // switch focus to a different window. To force focus to go to the WM's
  // target focus container, we reassign any focus events 100ms after
  // close/minimize. This will cause focus to briefly flicker to the OS
  // focus target and then to the WM's focus target.
  if should_override_focus(state) {
    state.pending_sync.queue_focus_change();
    return Ok(());
  }

  // Ignore the focus event if window is being hidden by the WM.
  if let Some(window) = &found_window {
    if window.display_state() == DisplayState::Hiding {
      return Ok(());
    }
  }

  // Focus effect should be updated for any change in focus that shouldn't
  // be overwritten. The incoming focus event at this point is either:
  //  1. WM's focus container (window or workspace). This is the desktop
  //     window in the case of a workspace.
  //  2. An ignored window.
  //  3. A window that received manual focus.
  state.pending_sync.queue_focused_effect_update();

  if let Some(window) = found_window {
    let workspace = window.workspace().context("No workspace")?;

    // Displayed focus means any deferred off-screen follow is stale.
    if window.display_state() != DisplayState::Hidden {
      state.cancel_pending_follow();
    }

    // Native focus has been synced to the WM's focused container.
    if focused_container == window.clone().into() {
      state.is_focus_synced = true;
      state.pending_sync.queue_workspace_to_reorder(workspace);
      return Ok(());
    }

    info!("Window manually focused: {window}");

    // Handle focus events from windows on hidden workspaces. For example,
    // if Discord is forcefully shown by the OS when it's on a hidden
    // workspace, switch focus to Discord's workspace.
    if window.display_state() == DisplayState::Hidden {
      // Corner-parked macOS windows stay OS-focusable during close/open
      // churn. Defer the follow so churn can cancel it; genuine
      // force-shows survive the debounce.
      if config.value.general.hide_method == HideMethod::PlaceInCorner {
        info!(
          "Deferring off-screen follow: id={} display_state={:?} wm_focused={}: {window}",
          window.id(),
          window.display_state(),
          focused_container.id(),
        );

        state.defer_follow(window.id());

        // Preserve WM focus until churn cancels or the follow commits.
        return Ok(());
      }

      info!("Focusing off-screen window: {window}");

      focus_workspace(
        WorkspaceTarget::Name(workspace.config().name),
        state,
        config,
      )?;
    }

    // Update the WM's focus state.
    set_focused_descendant(&window.clone().into(), None);

    // Run window rules for focus events.
    run_window_rules(
      window.clone(),
      &WindowRuleEvent::Focus,
      state,
      config,
    )?;

    state.is_focus_synced = true;
    state.pending_sync.queue_workspace_to_reorder(workspace);

    // Broadcast the focus change event.
    state.emit_event(WmEvent::FocusChanged {
      focused_container: window.to_dto()?,
    });
  }

  Ok(())
}

/// How long after a close or minimize the OS focus pick is always
/// overridden.
const OVERRIDE_FOCUS_WINDOW: Duration = Duration::from_millis(100);

/// Upper bound on overriding while the WM's own native focus change is
/// still queued behind a blocker (e.g. a minimize in flight).
const OVERRIDE_FOCUS_PENDING_LIMIT: Duration = Duration::from_millis(1000);

/// Returns true if focus should be reassigned to the WM's focus container.
///
/// The OS picks its own window after a close or minimize. That event can
/// arrive after the fixed window while the WM's native focus change is
/// still pending, and adopting it would bounce focus from the OS pick
/// back to the WM's target.
fn should_override_focus(state: &WmState) -> bool {
  let Some(elapsed) = state
    .unmanaged_or_minimized_timestamp
    .map(|time| time.elapsed())
  else {
    return false;
  };

  let is_recent = elapsed < OVERRIDE_FOCUS_WINDOW
    || (elapsed < OVERRIDE_FOCUS_PENDING_LIMIT
      && state.native_sync.has_pending_focus());

  is_recent && !state.is_focus_synced
}
