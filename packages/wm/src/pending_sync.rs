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

  /// Whether to skip animations for the current sync.
  skip_animations: bool,

  /// Set when this sync is a workspace switch, to the direction the
  /// content travels. Windows being shown enter from the opposite side;
  /// windows being hidden leave toward it.
  workspace_slide: Option<SlideDirection>,

  /// Monitor whose displayed workspace is changing.
  ///
  /// A switch belongs to one monitor, but the slide is a property of the
  /// whole sync, so without this every window queued for redraw picks up
  /// a slide trigger — including windows on other monitors, which fly off
  /// screen and are dragged back for a switch that never involved them.
  workspace_slide_monitor: Option<Uuid>,
}

impl PendingSync {
  pub fn has_changes(&self) -> bool {
    !self.containers_to_redraw.is_empty()
      || !self.workspaces_to_reorder.is_empty()
      || !self.open_animation_windows.is_empty()
      || self.needs_focus_update
      || self.needs_focused_effect_update
      || self.needs_all_effects_update
      || self.needs_cursor_jump
  }

  pub fn clear(&mut self) -> &mut Self {
    self.containers_to_redraw.clear();
    self.workspaces_to_reorder.clear();
    self.open_animation_windows.clear();
    self.needs_focus_update = false;
    self.needs_focused_effect_update = false;
    self.needs_all_effects_update = false;
    self.needs_cursor_jump = false;
    self.skip_animations = false;
    self.workspace_slide = None;
    self.workspace_slide_monitor = None;
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

  pub fn should_skip_animations(&self) -> bool {
    self.skip_animations
  }

  /// Marks this sync as a workspace switch on `monitor_id`, travelling
  /// in `direction`.
  pub fn set_workspace_slide(
    &mut self,
    direction: Option<SlideDirection>,
    monitor_id: Option<Uuid>,
  ) -> &mut Self {
    self.workspace_slide = direction;
    self.workspace_slide_monitor = monitor_id;
    self
  }

  /// The direction this switch travels for a window on `monitor_id`, or
  /// `None` if that monitor is not the one switching.
  pub fn workspace_slide_for(
    &self,
    monitor_id: Uuid,
  ) -> Option<SlideDirection> {
    (self.workspace_slide_monitor == Some(monitor_id))
      .then_some(self.workspace_slide)
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
}
