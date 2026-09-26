use std::collections::HashMap;

use uuid::Uuid;

use crate::{
  animation_manager::SlideDirection,
  models::{Container, WindowContainer, Workspace},
  traits::CommonGetters,
};

#[derive(Debug, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct PendingSync {
  /// Containers (and their descendants) that have a pending redraw.
  containers_to_redraw: HashMap<Uuid, Container>,

  /// Workspaces where z-order should be updated. Windows that match the
  /// focused window's state should be brought to the front.
  workspaces_to_reorder: Vec<Workspace>,

  /// Newly managed windows that should have an opening animation.
  open_animation_windows: Vec<WindowContainer>,

  /// Visible windows sent to a hidden workspace, which fade out where
  /// they are before they are hidden.
  sent_windows: Vec<WindowContainer>,

  /// Whether native focus should be reassigned to the WM's focused
  /// container.
  needs_focus_update: bool,

  /// Whether window effect for the focused window should be updated.
  needs_focused_effect_update: bool,

  /// Whether window effects for all windows should be updated.
  needs_all_effects_update: bool,

  /// Whether to jump the cursor to the focused container (if enabled in
  /// user config).
  needs_cursor_jump: bool,

  /// Whether to skip animations on every monitor for the current sync.
  skip_animations: bool,

  /// Workspace switches in this sync, keyed by the switching monitor.
  ///
  /// The value is the direction the content travels, or `None` for a
  /// switch that cuts. Windows being shown enter from the opposite side;
  /// windows being hidden leave toward it. A switch belongs to one
  /// monitor, so a cut or slide on one display never decides the motion
  /// of windows on another, and two switches in one sync each keep their
  /// own.
  workspace_transitions: HashMap<Uuid, Option<SlideDirection>>,
}

impl PendingSync {
  pub fn has_changes(&self) -> bool {
    !self.containers_to_redraw.is_empty()
      || !self.workspaces_to_reorder.is_empty()
      || !self.open_animation_windows.is_empty()
      || !self.sent_windows.is_empty()
      || self.needs_focus_update
      || self.needs_focused_effect_update
      || self.needs_all_effects_update
      || self.needs_cursor_jump
  }

  pub fn clear(&mut self) -> &mut Self {
    self.containers_to_redraw.clear();
    self.workspaces_to_reorder.clear();
    self.open_animation_windows.clear();
    self.sent_windows.clear();
    self.needs_focus_update = false;
    self.needs_focused_effect_update = false;
    self.needs_all_effects_update = false;
    self.needs_cursor_jump = false;
    self.skip_animations = false;
    self.workspace_transitions.clear();
    self
  }

  pub fn queue_container_to_redraw<T>(&mut self, container: T) -> &mut Self
  where
    T: Into<Container>,
  {
    let container: Container = container.into();
    self.containers_to_redraw.insert(container.id(), container);
    self
  }

  pub fn queue_containers_to_redraw<I, T>(
    &mut self,
    containers: I,
  ) -> &mut Self
  where
    I: IntoIterator<Item = T>,
    T: Into<Container>,
  {
    for container in containers {
      let container: Container = container.into();
      self.containers_to_redraw.insert(container.id(), container);
    }

    self
  }

  pub fn dequeue_container_from_redraw<T>(
    &mut self,
    container: T,
  ) -> &mut Self
  where
    T: Into<Container>,
  {
    self.containers_to_redraw.remove(&container.into().id());
    self
  }

  pub fn queue_workspace_to_reorder(
    &mut self,
    workspace: Workspace,
  ) -> &mut Self {
    self.workspaces_to_reorder.push(workspace);
    self
  }

  pub fn queue_open_animation_window(
    &mut self,
    window: WindowContainer,
  ) -> &mut Self {
    self.open_animation_windows.push(window);
    self
  }

  /// Queues a visible window leaving for a hidden workspace.
  pub fn queue_sent_window(
    &mut self,
    window: WindowContainer,
  ) -> &mut Self {
    self.sent_windows.push(window);
    self
  }

  pub fn queue_focus_change(&mut self) -> &mut Self {
    self.needs_focus_update = true;
    self
  }

  pub fn queue_focused_effect_update(&mut self) -> &mut Self {
    self.needs_focused_effect_update = true;
    self
  }

  pub fn queue_all_effects_update(&mut self) -> &mut Self {
    self.needs_all_effects_update = true;
    self
  }

  pub fn queue_cursor_jump(&mut self) -> &mut Self {
    self.needs_cursor_jump = true;
    self
  }

  pub fn set_skip_animations(&mut self, skip: bool) -> &mut Self {
    self.skip_animations = skip;
    self
  }

  /// Whether windows on `monitor_id` cut rather than animate this sync.
  ///
  /// True when animations are skipped everywhere, or when the monitor
  /// switches workspace without a slide.
  pub fn should_skip_animations_for(&self, monitor_id: Uuid) -> bool {
    self.skip_animations
      || self
        .workspace_transitions
        .get(&monitor_id)
        .is_some_and(Option::is_none)
  }

  /// Marks this sync as a workspace switch on `monitor_id`, travelling
  /// in `direction`, or cutting when `direction` is `None`.
  ///
  /// A later switch on the same monitor replaces an earlier one.
  pub fn set_workspace_transition(
    &mut self,
    monitor_id: Uuid,
    direction: Option<SlideDirection>,
  ) -> &mut Self {
    self.workspace_transitions.insert(monitor_id, direction);
    self
  }

  /// The direction a switch travels for a window on `monitor_id`, or
  /// `None` if that monitor is not sliding.
  pub fn workspace_slide_for(
    &self,
    monitor_id: Uuid,
  ) -> Option<SlideDirection> {
    self
      .workspace_transitions
      .get(&monitor_id)
      .copied()
      .flatten()
  }

  pub fn needs_focus_update(&self) -> bool {
    self.needs_focus_update
  }

  pub fn needs_focused_effect_update(&self) -> bool {
    self.needs_focused_effect_update
  }

  pub fn needs_all_effects_update(&self) -> bool {
    self.needs_all_effects_update
  }

  pub fn needs_cursor_jump(&self) -> bool {
    self.needs_cursor_jump
  }

  pub fn containers_to_redraw(&self) -> &HashMap<Uuid, Container> {
    &self.containers_to_redraw
  }

  pub fn workspaces_to_reorder(&self) -> &Vec<Workspace> {
    &self.workspaces_to_reorder
  }

  pub fn open_animation_windows(&self) -> &Vec<WindowContainer> {
    &self.open_animation_windows
  }

  pub fn sent_windows(&self) -> &Vec<WindowContainer> {
    &self.sent_windows
  }
}
