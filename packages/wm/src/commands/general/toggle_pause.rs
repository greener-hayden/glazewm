use wm_common::WmEvent;

use crate::wm_state::WmState;

/// Pauses or unpauses the WM.
pub fn toggle_pause(state: &mut WmState) {
  let is_paused = !state.is_paused;
  state.is_paused = is_paused;
  if is_paused {
    state.native_sync.release_all();
    state.native_sync.cleanup(&mut state.animation_manager);
  }

  // Redraw full container tree on unpause.
  if !is_paused {
    state
      .pending_sync
      .queue_container_to_redraw(state.root_container.clone());
  }

  state.emit_event(WmEvent::PauseChanged { is_paused });
}
