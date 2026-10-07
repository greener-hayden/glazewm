use std::{
  collections::HashMap,
  time::{Duration, Instant},
};

use anyhow::Context;
use tokio::sync::mpsc::{self};
use tracing::warn;
use uuid::Uuid;
use wm_common::{
  BindingModeConfig, DisplayState, HideCorner, WindowState, WmEvent,
};
use wm_platform::{
  Direction, Dispatcher, Display, NativeWindow, Point, Rect,
  WindowLiveness,
};

use crate::{
  animation_manager::AnimationManager,
  cleanup_schedule::{CleanupCheck, CleanupSchedule},
  commands::{
    container::set_focused_descendant,
    general::platform_sync,
    monitor::{add_monitor, move_bounded_workspaces_to_new_monitor},
    window::{manage_window, unmanage_window},
    workspace::focus_workspace,
  },
  models::{
    Container, Monitor, NativeMonitorProperties, RootContainer,
    WindowContainer, Workspace, WorkspaceTarget,
  },
  pending_sync::PendingSync,
  perf::{SyncOrigin, SyncTrigger},
  traits::{CommonGetters, PositionGetters, WindowGetters},
  user_config::UserConfig,
};

/// A debounced off-screen focus follow candidate.
///
/// macOS corner-parked windows remain OS-focusable, so close/open churn
/// can briefly focus a hidden-workspace window. Debouncing lets churn
/// cancel that focus while preserving genuine force-shows.
#[derive(Clone, Copy, Debug)]
struct PendingFollow {
  /// WM container id of the off-screen window to potentially follow.
  window_id: Uuid,

  /// WM focused container id when the follow was requested.
  ///
  /// The follow only commits while focus is unchanged, so any focus
  /// mutation during the debounce supersedes it without every focus
  /// path needing an explicit cancellation.
  focused_at_request: Option<Uuid>,

  /// When the follow was first requested.
  requested_at: Instant,
}

pub struct WmState {
  /// Root node of the container tree. Monitors are the children of the
  /// root node, followed by workspaces, then split containers/windows.
  pub root_container: RootContainer,

  pub dispatcher: Dispatcher,

  pub pending_sync: PendingSync,

  pub layout_snapshot: crate::layout_snapshot::LayoutSnapshot,

  pub native_sync: crate::placement::PlacementCoordinator,

  /// Manager for window animations.
  pub animation_manager: AnimationManager,

  /// Name of the most recently focused workspace.
  ///
  /// Used for the `general.toggle_workspace_on_refocus` option on
  /// workspace focus.
  pub recent_workspace_name: Option<String>,

  /// Time since a previously focused window was unmanaged or minimized.
  ///
  /// Used to decide whether to override incoming focus events.
  pub unmanaged_or_minimized_timestamp: Option<Instant>,

  /// Deferred off-screen focus follow awaiting churn confirmation.
  pending_follow: Option<PendingFollow>,

  /// Configs of currently enabled binding modes.
  pub binding_modes: Vec<BindingModeConfig>,

  /// Windows that the WM should ignore. Windows can be added via the
  /// `ignore` command.
  pub ignored_windows: Vec<NativeWindow>,

  /// Shown windows whose manageability could not be checked, because the
  /// application did not answer. Their next window event checks again.
  pub unresolved_windows: Vec<NativeWindow>,

  /// When cleanup next asks every window whether it is alive, rather
  /// than checking against the window server's listing.
  cleanup_schedule: CleanupSchedule,

  /// Whether the WM is paused.
  pub is_paused: bool,

  /// Whether the OS focused window is the same as the WM focused window.
  pub is_focus_synced: bool,

  /// Whether the initial state has been populated.
  has_initialized: bool,

  /// Sender for emitting WM-related events.
  event_tx: mpsc::UnboundedSender<WmEvent>,

  /// Sender for gracefully shutting down the WM.
  exit_tx: mpsc::UnboundedSender<()>,
}

impl WmState {
  /// Debounce before a deferred off-screen follow commits.
  ///
  /// Must comfortably outlast the macOS close churn, where the closed
  /// window's `Destroyed` event is delayed (~34ms) by a synchronous
  /// accessibility round-trip in the application observer.
  const FOLLOW_DEBOUNCE: Duration = Duration::from_millis(120);

  /// Creates state with the platform-aware animation manager.
  pub fn new(
    dispatcher: Dispatcher,
    event_tx: mpsc::UnboundedSender<WmEvent>,
    exit_tx: mpsc::UnboundedSender<()>,
  ) -> Self {
    let animation_manager = AnimationManager::new(&dispatcher);
    Self::with_animation_manager(
      dispatcher,
      animation_manager,
      event_tx,
      exit_tx,
    )
  }

  /// Creates native-free test state with stopped dispatch and empty
  /// channels.
  #[cfg(test)]
  pub(crate) fn mock() -> Self {
    Self::mock_with_events().0
  }

  /// Creates native-free test state with stopped dispatch, and returns
  /// the receiving end of its event channel.
  ///
  /// Events are emitted only after `mark_initialized`.
  #[cfg(test)]
  pub(crate) fn mock_with_events(
  ) -> (Self, mpsc::UnboundedReceiver<WmEvent>) {
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (exit_tx, _) = mpsc::unbounded_channel();
    let state = Self::with_animation_manager(
      Dispatcher::mock(),
      AnimationManager::mock(),
      event_tx,
      exit_tx,
    );

    (state, event_rx)
  }

  /// Initializes the shared state independently of animation construction.
  fn with_animation_manager(
    dispatcher: Dispatcher,
    animation_manager: AnimationManager,
    event_tx: mpsc::UnboundedSender<WmEvent>,
    exit_tx: mpsc::UnboundedSender<()>,
  ) -> Self {
    Self {
      root_container: RootContainer::new(),
      animation_manager,
      dispatcher,
      pending_sync: PendingSync::default(),
      layout_snapshot: crate::layout_snapshot::LayoutSnapshot::default(),
      native_sync: crate::placement::PlacementCoordinator::default(),
      recent_workspace_name: None,
      unmanaged_or_minimized_timestamp: None,
      pending_follow: None,
      binding_modes: Vec::new(),
      ignored_windows: Vec::new(),
      unresolved_windows: Vec::new(),
      cleanup_schedule: CleanupSchedule::new(Instant::now()),
      is_paused: false,
      is_focus_synced: false,
      has_initialized: false,
      event_tx,
      exit_tx,
    }
  }

  /// Marks the state as initialized.
  ///
  /// Test-only. Enables event emission, which is otherwise gated on
  /// `populate` having run.
  #[cfg(test)]
  pub(crate) fn mark_initialized(&mut self) {
    self.has_initialized = true;
  }

  /// Populates the initial WM state by creating containers for all
  /// existing windows and monitors.
  pub fn populate(
    &mut self,
    config: &mut UserConfig,
  ) -> anyhow::Result<()> {
    // Get the originally focused window when the WM was started.
    let focused_window = self.dispatcher.focused_window().ok();

    // Create a monitor, and consequently a workspace, for each detected
    // native monitor.
    for native_display in self.dispatcher.sorted_displays()? {
      if let Ok(native_properties) =
        NativeMonitorProperties::try_from(&native_display)
      {
        let monitor =
          add_monitor(native_display, native_properties, self)?;
        move_bounded_workspaces_to_new_monitor(&monitor, self, config)?;
      }
    }

    // Manage windows in reverse z-order (bottom to top). This helps to
    // preserve the original stacking order.
    for native_window in
      self.dispatcher.visible_windows()?.into_iter().rev()
    {
      let nearest_workspace = self
        .nearest_monitor(&native_window)
        .and_then(|m| m.displayed_workspace());

      if let Some(workspace) = nearest_workspace {
        manage_window(
          native_window,
          Some(workspace.into()),
          self,
          config,
        )?;
      }
    }

    let container_to_focus = focused_window
      .and_then(|focused_window| {
        self.window_from_native(&focused_window).map(Into::into)
      })
      .or_else(|| self.windows().pop().map(Into::into))
      .or_else(|| self.workspaces().pop().map(Into::into))
      .context("Failed to get container to focus.")?;

    set_focused_descendant(&container_to_focus, None);
    self.is_focus_synced = true;

    self
      .pending_sync
      .queue_focus_change()
      .queue_all_effects_update()
      .set_skip_animations(true);

    for workspace in self.workspaces() {
      self.pending_sync.queue_workspace_to_reorder(workspace);
    }

    platform_sync(self, config, SyncOrigin::new(SyncTrigger::Startup))?;
    self.has_initialized = true;

    Ok(())
  }

  pub fn monitors(&self) -> Vec<Monitor> {
    self.root_container.monitors()
  }

  pub fn workspaces(&self) -> Vec<Workspace> {
    self
      .monitors()
      .iter()
      .flat_map(Monitor::workspaces)
      .collect()
  }

  /// Gets workspaces sorted by their position in the user config.
  pub fn sorted_workspaces(&self, config: &UserConfig) -> Vec<Workspace> {
    let mut workspaces = self.workspaces();
    config.sort_workspaces(&mut workspaces);
    workspaces
  }

  pub fn windows(&self) -> Vec<WindowContainer> {
    self
      .root_container
      .descendants()
      .filter_map(|container| container.try_into().ok())
      .collect()
  }

  /// Gets the monitor that encompasses the largest portion of a given
  /// window.
  ///
  /// Defaults to the first monitor if the nearest monitor is invalid.
  ///
  /// # Platform-specific
  ///
  /// - Windows: asks the OS for the nearest display.
  /// - macOS: reads the window's frame once and compares it with the
  ///   monitor bounds already held, as [`Self::nearest_monitor_for_rect`]
  ///   does. Asking the OS costs a hop, two more reads and a walk over
  ///   every screen.
  pub fn nearest_monitor(
    &self,
    native_window: &NativeWindow,
  ) -> Option<Monitor> {
    #[cfg(target_os = "macos")]
    {
      self.nearest_monitor_for_rect(&native_window.frame().ok()?)
    }
    #[cfg(target_os = "windows")]
    {
      self
        .monitor_from_native(
          &self.dispatcher.nearest_display(native_window).ok()?,
        )
        .or(self.monitors().first().cloned())
    }
  }

  /// Gets the monitor that encompasses the largest portion of `rect`.
  ///
  /// Compares `rect` with the bounds each monitor already holds, so it
  /// makes no native call. A rect that overlaps no monitor belongs to the
  /// primary one, or to the first monitor if none is primary. Returns
  /// `None` only when there are no monitors.
  ///
  /// Of monitors that share the largest overlap, the first wins.
  #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
  pub fn nearest_monitor_for_rect(&self, rect: &Rect) -> Option<Monitor> {
    let (monitors, candidates): (Vec<_>, Vec<_>) = self
      .monitors()
      .into_iter()
      .filter_map(|monitor| {
        let bounds = monitor.to_rect().ok()?;
        let is_primary = monitor.native().is_primary().unwrap_or(false);
        Some((monitor, (bounds, is_primary)))
      })
      .unzip();

    nearest_candidate(rect, &candidates)
      .and_then(|index| monitors.into_iter().nth(index))
  }

  /// Gets monitor that corresponds to the given `Display`.
  // LINT: Only Windows asks the OS for the nearest display (see
  // `Self::nearest_monitor`).
  #[cfg_attr(target_os = "macos", allow(dead_code))]
  pub fn monitor_from_native(
    &self,
    native_display: &Display,
  ) -> Option<Monitor> {
    self
      .monitors()
      .into_iter()
      .find(|monitor| monitor.native() == *native_display)
  }

  /// Gets the closest monitor in a given direction.
  ///
  /// Uses i3wm's algorithm for finding best guess.
  pub fn monitor_in_direction(
    &self,
    origin_monitor: &Monitor,
    direction: &Direction,
  ) -> anyhow::Result<Option<Monitor>> {
    let origin_rect = origin_monitor.native_properties().bounds;

    // Create a tuple of monitors and their rect.
    let monitors_with_rect = self
      .monitors()
      .into_iter()
      .map(|monitor| {
        let rect = monitor.native_properties().bounds;
        anyhow::Ok((monitor, rect))
      })
      .try_collect::<Vec<_>>()?;

    let closest_monitor = monitors_with_rect
      .into_iter()
      .filter(|(_, rect)| match direction {
        Direction::Right => {
          rect.x() > origin_rect.x() && rect.y_overlap(&origin_rect) > 0
        }
        Direction::Left => {
          rect.x() < origin_rect.x() && rect.y_overlap(&origin_rect) > 0
        }
        Direction::Down => {
          rect.y() > origin_rect.y() && rect.x_overlap(&origin_rect) > 0
        }
        Direction::Up => {
          rect.y() < origin_rect.y() && rect.x_overlap(&origin_rect) > 0
        }
      })
      .min_by(|(_, rect_a), (_, rect_b)| match direction {
        Direction::Right => rect_a.x().cmp(&rect_b.x()),
        Direction::Left => rect_b.x().cmp(&rect_a.x()),
        Direction::Down => rect_a.y().cmp(&rect_b.y()),
        Direction::Up => rect_b.y().cmp(&rect_a.y()),
      })
      .map(|(monitor, _)| monitor);

    Ok(closest_monitor)
  }

  /// Determines the preferred hide corner for each monitor. Used for
  /// [`HideMethod::PlaceInCorner`].
  ///
  /// The corner is chosen by simulating a 400x400 window frame in the
  /// bottom-left and bottom-right of the monitor's working area, then
  /// picking the side that overlaps the least with other monitors'
  /// working areas (ties favor bottom-right).
  ///
  /// Returns a map keyed by monitor ID.
  pub fn monitors_by_hide_corner(&self) -> HashMap<Uuid, HideCorner> {
    const TEST_FRAME_SIZE: i32 = 400;
    const VISIBLE_SLIVER: i32 = 1;

    let monitors = self.monitors();
    let working_areas = monitors
      .iter()
      .map(|monitor| monitor.native_properties().working_area)
      .collect::<Vec<_>>();

    monitors
      .into_iter()
      .enumerate()
      .map(|(idx, monitor)| {
        let monitor_rect = &working_areas[idx];
        let test_frame_y = monitor_rect.bottom - TEST_FRAME_SIZE;

        let left_test_frame = Rect::from_xy(
          monitor_rect.left - TEST_FRAME_SIZE + VISIBLE_SLIVER,
          test_frame_y,
          TEST_FRAME_SIZE,
          TEST_FRAME_SIZE,
        );

        let right_test_frame = Rect::from_xy(
          monitor_rect.right - VISIBLE_SLIVER,
          test_frame_y,
          TEST_FRAME_SIZE,
          TEST_FRAME_SIZE,
        );

        let overlap_area = |test_frame: &Rect| -> i32 {
          working_areas
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != idx)
            .map(|(_, rect)| test_frame.intersection_area(rect))
            .sum()
        };

        let left_overlap = overlap_area(&left_test_frame);
        let right_overlap = overlap_area(&right_test_frame);

        let corner = if left_overlap < right_overlap {
          HideCorner::BottomLeft
        } else {
          HideCorner::BottomRight
        };

        (monitor.id(), corner)
      })
      .collect()
  }

  /// Gets window that corresponds to the given `NativeWindow`.
  pub fn window_from_native(
    &self,
    native_window: &NativeWindow,
  ) -> Option<WindowContainer> {
    self
      .root_container
      .descendants()
      .filter_map(|container| WindowContainer::try_from(container).ok())
      .find(|window| &*window.native() == native_window)
  }

  pub fn workspace_by_name(
    &self,
    workspace_name: &str,
  ) -> Option<Workspace> {
    self
      .workspaces()
      .into_iter()
      .find(|workspace| workspace.config().name == workspace_name)
  }

  /// Gets a workspace and its name by the given target.
  ///
  /// Returns a tuple of the workspace name and the `Workspace` instance
  /// if active.
  #[allow(clippy::too_many_lines)]
  pub fn workspace_by_target(
    &self,
    origin_workspace: &Workspace,
    target: WorkspaceTarget,
    config: &UserConfig,
  ) -> anyhow::Result<(Option<String>, Option<Workspace>)> {
    let (name, workspace) = match target {
      WorkspaceTarget::Name(name) => {
        #[allow(clippy::match_bool)]
        match origin_workspace.config().name == name {
          false => (Some(name.clone()), self.workspace_by_name(&name)),
          // Toggle the workspace if it's already focused.
          true if config.value.general.toggle_workspace_on_refocus => (
            self.recent_workspace_name.clone(),
            self
              .recent_workspace_name
              .as_ref()
              .and_then(|name| self.workspace_by_name(name)),
          ),
          true => (None, None),
        }
      }
      WorkspaceTarget::Recent => (
        self.recent_workspace_name.clone(),
        self
          .recent_workspace_name
          .as_ref()
          .and_then(|name| self.workspace_by_name(name)),
      ),
      WorkspaceTarget::NextActive => {
        let active_workspaces = self.sorted_workspaces(config);
        let origin_index = active_workspaces
          .iter()
          .position(|workspace| workspace.id() == origin_workspace.id())
          .context("Failed to get index of given workspace.")?;

        let next_active_workspace = active_workspaces
          .get(origin_index + 1)
          .or_else(|| active_workspaces.first());

        (
          next_active_workspace.map(|workspace| workspace.config().name),
          next_active_workspace.cloned(),
        )
      }
      WorkspaceTarget::PreviousActive => {
        let active_workspaces = self.sorted_workspaces(config);
        let origin_index = active_workspaces
          .iter()
          .position(|workspace| workspace.id() == origin_workspace.id())
          .context("Failed to get index of given workspace.")?;

        let prev_active_workspace = active_workspaces.get(
          origin_index
            .checked_sub(1)
            .unwrap_or(active_workspaces.len() - 1),
        );

        (
          prev_active_workspace.map(|workspace| workspace.config().name),
          prev_active_workspace.cloned(),
        )
      }
      WorkspaceTarget::NextActiveInMonitor => {
        let monitor = origin_workspace
          .monitor()
          .context("No monitor in workspace")?;

        let mut workspace_in_monitor = monitor.workspaces();
        config.sort_workspaces(&mut workspace_in_monitor);

        let origin_index = workspace_in_monitor
          .iter()
          .position(|workspace| workspace.id() == origin_workspace.id())
          .context("Failed to get index of give workspace")?;

        let next_active_workspace_in_monitor = workspace_in_monitor
          .get(origin_index + 1)
          .or_else(|| workspace_in_monitor.first());

        (
          next_active_workspace_in_monitor
            .map(|workspace| workspace.config().name),
          next_active_workspace_in_monitor.cloned(),
        )
      }
      WorkspaceTarget::PreviousActiveInMonitor => {
        let monitor = origin_workspace
          .monitor()
          .context("No monitor in workspace")?;

        let mut workspace_in_monitor = monitor.workspaces();
        config.sort_workspaces(&mut workspace_in_monitor);

        let origin_index = workspace_in_monitor
          .iter()
          .position(|workspace| workspace.id() == origin_workspace.id())
          .context("Failed to get index of give workspace")?;

        let prev_active_workspace_in_monitor = workspace_in_monitor.get(
          origin_index
            .checked_sub(1)
            .unwrap_or(workspace_in_monitor.len() - 1),
        );

        (
          prev_active_workspace_in_monitor
            .map(|workspace| workspace.config().name),
          prev_active_workspace_in_monitor.cloned(),
        )
      }
      WorkspaceTarget::Next => {
        let workspaces = &config.value.workspaces;
        let origin_name = origin_workspace.config().name.clone();
        let origin_index = workspaces
          .iter()
          .position(|workspace| workspace.name == origin_name)
          .context("Failed to get index of given workspace.")?;

        let next_workspace_config = workspaces
          .get(origin_index + 1)
          .or_else(|| workspaces.first());

        let next_workspace_name =
          next_workspace_config.map(|config| config.name.clone());

        let next_workspace = next_workspace_name
          .as_ref()
          .and_then(|name| self.workspace_by_name(name));

        (next_workspace_name, next_workspace)
      }
      WorkspaceTarget::Previous => {
        let workspaces = &config.value.workspaces;
        let origin_name = origin_workspace.config().name.clone();
        let origin_index = workspaces
          .iter()
          .position(|workspace| workspace.name == origin_name)
          .context("Failed to get index of given workspace.")?;

        let previous_workspace_config = workspaces.get(
          origin_index.checked_sub(1).unwrap_or(workspaces.len() - 1),
        );

        let previous_workspace_name =
          previous_workspace_config.map(|config| config.name.clone());

        let previous_workspace = previous_workspace_name
          .as_ref()
          .and_then(|name| self.workspace_by_name(name));

        (previous_workspace_name, previous_workspace)
      }

      WorkspaceTarget::Direction(direction) => {
        let origin_monitor =
          origin_workspace.monitor().context("No focused monitor.")?;

        let target_workspace = self
          .monitor_in_direction(&origin_monitor, &direction)?
          .and_then(|monitor| monitor.displayed_workspace());

        (
          target_workspace
            .as_ref()
            .map(|workspace| workspace.config().name),
          target_workspace,
        )
      }
    };

    Ok((name, workspace))
  }

  /// Gets windows that should be redrawn.
  ///
  /// When redrawing after a command that changes a window's type (e.g.
  /// tiling -> floating), the original detached window might still be
  /// queued for a redraw and should be filtered out.
  pub fn windows_to_redraw(&self) -> Vec<WindowContainer> {
    self
      .pending_sync
      .containers_to_redraw()
      .values()
      .flat_map(CommonGetters::self_and_descendants)
      .filter(|container| !container.is_detached())
      .filter_map(|container| container.try_into().ok())
      .collect()
  }

  /// Gets the currently focused container. This can either be a window or
  /// a workspace without any descendant windows.
  pub fn focused_container(&self) -> Option<Container> {
    self.root_container.descendant_focus_order().next()
  }

  /// Emits a WM event through an MSPC channel.
  ///
  /// Does not emit events while the WM is paused or populating initial
  /// state. This is to prevent events (e.g. workspace activation events)
  /// from being emitted via IPC server before the initial state is
  /// prepared.
  pub fn emit_event(&self, event: WmEvent) {
    if self.has_initialized
      && (!self.is_paused || matches!(event, WmEvent::PauseChanged { .. }))
    {
      if let Err(err) = self.event_tx.send(event) {
        warn!("Failed to send event: {}", err);
      }
    }
  }

  /// Starts graceful shutdown via an MSPC channel.
  pub fn emit_exit(&self) -> anyhow::Result<()> {
    self.exit_tx.send(())?;
    Ok(())
  }

  pub fn container_by_id(&self, id: Uuid) -> Option<Container> {
    self
      .root_container
      .self_and_descendants()
      .find(|container| container.id() == id)
  }

  /// Gets container to focus after the given window is unmanaged,
  /// minimized, or moved to another workspace.
  pub fn focus_target_after_removal(
    &self,
    removed_window: &WindowContainer,
  ) -> Option<Container> {
    // If the removed window is not focused, no need to change focus.
    if self.focused_container() != Some(removed_window.clone().into()) {
      return None;
    }

    // Get descendant focus order excluding the removed container.
    let workspace = removed_window.workspace()?;
    let descendant_focus_order = workspace
      .descendant_focus_order()
      .filter(|descendant| descendant.id() != removed_window.id())
      .collect::<Vec<_>>();

    // Get focus target that matches the removed window type. This applies
    // for windows that aren't in a minimized state.
    let focus_target_of_type = descendant_focus_order
      .iter()
      .filter_map(|descendant| descendant.as_window_container().ok())
      .find(|descendant| {
        matches!(
          (descendant.state(), removed_window.state()),
          (WindowState::Tiling, WindowState::Tiling)
            | (WindowState::Floating(_), WindowState::Floating(_))
            | (WindowState::Fullscreen(_), WindowState::Fullscreen(_))
        )
      })
      .map(Into::into);

    if focus_target_of_type.is_some() {
      return focus_target_of_type;
    }

    let non_minimized_focus_target = descendant_focus_order
      .iter()
      .filter_map(|descendant| descendant.as_window_container().ok())
      .find(|descendant| descendant.state() != WindowState::Minimized)
      .map(Into::into);

    non_minimized_focus_target
      .or(descendant_focus_order.first().cloned())
      .or(Some(workspace.into()))
  }

  /// Returns all containers that contain the given point.
  #[allow(clippy::unused_self)]
  pub fn containers_at_point(
    &self,
    origin_container: &Container,
    point: &Point,
  ) -> Vec<Container> {
    origin_container
      .descendants()
      .filter(|descendant| {
        descendant
          .to_rect()
          .is_ok_and(|rect| rect.contains_point(point))
      })
      .collect()
  }

  /// Returns the monitor that contains the given point.
  pub fn monitor_at_point(&self, point: &Point) -> Option<Monitor> {
    self
      .monitors()
      .iter()
      .find(|monitor| {
        monitor
          .to_rect()
          .is_ok_and(|rect| rect.contains_point(point))
      })
      .cloned()
  }

  /// Defers an off-screen focus follow for the given window.
  ///
  /// The follow commits after `FOLLOW_DEBOUNCE` unless cancelled by
  /// intervening churn.
  pub fn defer_follow(&mut self, window_id: Uuid) {
    self.pending_follow = Some(PendingFollow {
      window_id,
      focused_at_request: self
        .focused_container()
        .map(|container| container.id()),
      requested_at: Instant::now(),
    });
  }

  /// Cancels any pending off-screen follow.
  pub fn cancel_pending_follow(&mut self) {
    self.pending_follow = None;
  }

  /// Returns when a pending off-screen follow becomes eligible to commit.
  pub fn pending_follow_deadline(&self) -> Option<Instant> {
    self
      .pending_follow
      .map(|pending| pending.requested_at + Self::FOLLOW_DEBOUNCE)
  }

  /// Commits a debounced off-screen follow if the candidate is still
  /// valid.
  ///
  /// Queues side effects on `pending_sync`; the caller must flush them.
  pub fn commit_pending_follow(
    &mut self,
    config: &UserConfig,
  ) -> anyhow::Result<()> {
    let Some(pending) = self.pending_follow else {
      return Ok(());
    };

    self.pending_follow = None;

    // Any focus change during the debounce supersedes the follow.
    let focused_id =
      self.focused_container().map(|container| container.id());

    if focused_id != pending.focused_at_request {
      tracing::debug!(
        "Deferred off-screen follow superseded by focus change."
      );
      return Ok(());
    }

    let Some(window) = self
      .container_by_id(pending.window_id)
      .and_then(|container| container.as_window_container().ok())
    else {
      tracing::debug!("Deferred off-screen follow candidate is gone.");
      return Ok(());
    };

    // Churned windows stop being hidden before the debounce commits.
    if window.display_state() != DisplayState::Hidden {
      tracing::debug!(
        "Deferred off-screen follow candidate is on-screen."
      );
      return Ok(());
    }

    let workspace = window.workspace().context("No workspace.")?;

    if workspace.is_displayed() {
      return Ok(());
    }

    tracing::info!("Committing deferred off-screen follow: {window}");
    focus_workspace(
      WorkspaceTarget::Name(workspace.config().name),
      self,
      config,
    )?;

    Ok(())
  }

  /// Cleans up windows that are no longer alive.
  ///
  /// This addresses the "ghost window" issue where applications may
  /// terminate without sending window destroy events, leaving invalid
  /// windows in WM state.
  ///
  /// Most rounds check against one window server listing and only ask
  /// the windows it lacks, so they cost no request to the applications.
  /// Every [`CleanupSchedule::SWEEP_INTERVAL`] a round asks every window
  /// instead, which also finds a window the application has dropped but
  /// the window server still lists.
  ///
  /// See: <https://github.com/glzr-io/glazewm/issues/1219>
  pub fn cleanup_invalid_windows(&mut self) -> anyhow::Result<()> {
    let liveness = match self.cleanup_schedule.next_check(Instant::now()) {
      CleanupCheck::Listing => WindowLiveness::from_window_server(),
      CleanupCheck::Sweep => {
        tracing::debug!("Checking every window for cleanup.");
        WindowLiveness::per_window()
      }
    };

    let invalid_windows = self
      .windows()
      .into_iter()
      .filter(|window| !liveness.is_valid(&window.native()));

    for window in invalid_windows {
      tracing::info!("Removing invalid window: {}", window);
      unmanage_window(window, self)?;
    }

    // Prune ignored windows that are no longer valid.
    self
      .ignored_windows
      .retain(|window| liveness.is_valid(window));
    self
      .unresolved_windows
      .retain(|window| liveness.is_valid(window));

    Ok(())
  }
}

impl Drop for WmState {
  fn drop(&mut self) {
    self.native_sync.release_all();
    self.native_sync.cleanup(&mut self.animation_manager);
  }
}

/// Picks the candidate display that `rect` overlaps the most.
///
/// Each candidate is its bounds and whether it is the primary display.
/// The first candidate wins a tie. A `rect` that overlaps none (e.g. one
/// that is off-screen) takes the primary candidate, or else the first.
/// Returns the index of the pick, or `None` without candidates.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn nearest_candidate(
  rect: &Rect,
  candidates: &[(Rect, bool)],
) -> Option<usize> {
  let mut best = None;
  let mut best_area = 0;

  for (index, (bounds, _)) in candidates.iter().enumerate() {
    let area = rect.intersection_area(bounds);

    if area > best_area {
      best = Some(index);
      best_area = area;
    }
  }

  best
    .or_else(|| candidates.iter().position(|(_, is_primary)| *is_primary))
    .or_else(|| (!candidates.is_empty()).then_some(0))
}

#[cfg(test)]
mod nearest_candidate_tests {
  use super::*;

  /// Two side-by-side displays, the right one primary.
  fn displays() -> Vec<(Rect, bool)> {
    vec![
      (Rect::from_xy(-1920, 0, 1920, 1080), false),
      (Rect::from_xy(0, 0, 2560, 1440), true),
    ]
  }

  #[test]
  fn a_rect_inside_one_display_picks_it() {
    let rect = Rect::from_xy(100, 100, 800, 600);
    assert_eq!(nearest_candidate(&rect, &displays()), Some(1));

    let rect = Rect::from_xy(-1500, 100, 800, 600);
    assert_eq!(nearest_candidate(&rect, &displays()), Some(0));
  }

  #[test]
  fn a_straddling_rect_picks_the_larger_overlap() {
    // 300px on the left display, 500px on the right one.
    let rect = Rect::from_xy(-300, 100, 800, 600);
    assert_eq!(nearest_candidate(&rect, &displays()), Some(1));

    // 500px on the left display, 300px on the right one.
    let rect = Rect::from_xy(-500, 100, 800, 600);
    assert_eq!(nearest_candidate(&rect, &displays()), Some(0));
  }

  #[test]
  fn an_equal_overlap_picks_the_first_candidate() {
    // 400px on each display.
    let rect = Rect::from_xy(-400, 100, 800, 600);
    assert_eq!(nearest_candidate(&rect, &displays()), Some(0));
  }

  #[test]
  fn a_rect_that_only_touches_a_display_overlaps_none() {
    // Shares the edge at x = 0 with the right display, and nothing else.
    let rect = Rect::from_xy(-800, 3000, 800, 600);
    assert_eq!(nearest_candidate(&rect, &displays()), Some(1));

    let rect = Rect::from_xy(-800, 100, 800, 600);
    assert_eq!(nearest_candidate(&rect, &displays()), Some(0));
  }

  #[test]
  fn an_off_screen_rect_picks_the_primary_display() {
    let rect = Rect::from_xy(9000, 9000, 800, 600);
    assert_eq!(nearest_candidate(&rect, &displays()), Some(1));

    let mut displays = displays();
    displays.reverse();
    assert_eq!(nearest_candidate(&rect, &displays), Some(0));
  }

  #[test]
  fn an_off_screen_rect_without_a_primary_picks_the_first() {
    let rect = Rect::from_xy(9000, 9000, 800, 600);
    let displays = vec![
      (Rect::from_xy(0, 0, 1920, 1080), false),
      (Rect::from_xy(1920, 0, 1920, 1080), false),
    ];
    assert_eq!(nearest_candidate(&rect, &displays), Some(0));
  }

  #[test]
  fn a_degenerate_rect_picks_the_primary_display() {
    let rect = Rect::from_xy(100, 100, 0, 0);
    assert_eq!(nearest_candidate(&rect, &displays()), Some(1));
  }

  #[test]
  fn no_displays_pick_nothing() {
    let rect = Rect::from_xy(0, 0, 100, 100);
    assert_eq!(nearest_candidate(&rect, &[]), None);
  }

  /// The walk `Dispatcher::nearest_display` makes on macOS, which
  /// `nearest_candidate` replaces.
  fn oracle(rect: &Rect, displays: &[(Rect, bool)]) -> Option<usize> {
    let mut best = None;
    let mut max_intersection_area = 0;

    for (index, (screen, _)) in displays.iter().enumerate() {
      let intersection_x = i32::max(rect.x(), screen.x());
      let intersection_y = i32::max(rect.y(), screen.y());
      let intersection_width =
        i32::min(rect.x() + rect.width(), screen.x() + screen.width())
          - intersection_x;
      let intersection_height =
        i32::min(rect.y() + rect.height(), screen.y() + screen.height())
          - intersection_y;

      if intersection_width > 0 && intersection_height > 0 {
        let area = intersection_width * intersection_height;
        if area > max_intersection_area {
          max_intersection_area = area;
          best = Some(index);
        }
      }
    }

    best
      .or_else(|| displays.iter().position(|(_, is_primary)| *is_primary))
      .or_else(|| (!displays.is_empty()).then_some(0))
  }

  #[test]
  fn it_matches_the_native_walk_over_a_grid_of_rects() {
    let displays = vec![
      (Rect::from_xy(0, 0, 1512, 982), true),
      (Rect::from_xy(-3008, -368, 3008, 1692), false),
      (Rect::from_xy(1512, 100, 1920, 1080), false),
    ];

    for x in (-4000..4000).step_by(331) {
      for y in (-1200..2000).step_by(277) {
        for (width, height) in [(0, 0), (1, 1), (640, 480), (2400, 1300)] {
          let rect = Rect::from_xy(x, y, width, height);
          assert_eq!(
            nearest_candidate(&rect, &displays),
            oracle(&rect, &displays),
            "{rect:?}"
          );
        }
      }
    }
  }
}
