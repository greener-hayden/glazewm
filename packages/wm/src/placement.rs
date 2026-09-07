use std::{
  collections::{HashMap, HashSet},
  time::{Duration, Instant},
};

use anyhow::Context;
use uuid::Uuid;
use wm_common::{
  CursorJumpTrigger, DisplayState, HideCorner, HideMethod, WindowState,
  WmEvent,
};
use wm_platform::{
  Color, ConcealMethod, CornerStyle, Delta, OpacityValue,
  PlacementSession, Rect, WindowZOrder,
};

use crate::{
  animation_manager::{AnimationPlan, AnimationTrigger},
  models::{Monitor, NativeMonitorProperties, WindowContainer},
  native_reconciler::{
    DesiredFrame, FrameReconciler, NativeMutation, NativeState,
    ObservedFrame, ReconcilePhase,
  },
  presentation::{MotionPreparation, SourceLease},
  traits::{
    effective_opacity, CommonGetters, PositionGetters, WindowGetters,
  },
  user_config::UserConfig,
  wm_state::WmState,
};

/// Holds commands until native reconciliation.
#[derive(Default)]
struct WindowIntent {
  opacity: Option<OpacityValue>,
  title_bar: Option<bool>,
  minimize: bool,
}

/// Holds successfully applied decoration intent.
#[derive(Clone, PartialEq)]
struct Decorations {
  border: Option<Color>,
  corner: CornerStyle,
  title_bar: Option<bool>,
}

/// Resolves the window's requested border and corners.
fn focus_decorations(
  window: &WindowContainer,
  config: &UserConfig,
) -> (Option<Color>, CornerStyle) {
  let effects = if window.has_focus(None) {
    &config.value.window_effects.focused_window
  } else {
    &config.value.window_effects.other_windows
  };

  (
    effects.border.enabled.then(|| effects.border.color.clone()),
    if effects.corner_style.enabled {
      effects.corner_style.style.clone()
    } else {
      CornerStyle::Default
    },
  )
}

/// Distinguishes untouched and applied opacity.
#[derive(PartialEq)]
enum AppliedOpacity {
  Unknown,
  Value(Option<OpacityValue>),
}

/// Owns motion independently of source concealment.
enum MotionOwner {
  Native,
  Overlay(MotionPreparation),
}

impl MotionOwner {
  /// Returns readiness only for proxy motion.
  fn preparation(&self) -> Option<&MotionPreparation> {
    match self {
      Self::Native => None,
      Self::Overlay(preparation) => Some(preparation),
    }
  }
}

/// Owns native mutations for one window.
struct ManagedWindow {
  native: PlacementSession,
  frame: FrameReconciler,
  motion: Option<MotionOwner>,
  source: Option<SourceLease>,
  retire_after: Option<u64>,
  cancelled: bool,
  retry_at: Option<Instant>,
  last_requested_alpha: Option<OpacityValue>,
  opacity: AppliedOpacity,
  /// How the source hides behind its overlay; `None` after a failure.
  conceal: Option<ConcealMethod>,
  visibility: Option<(bool, HideMethod)>,
  visibility_pending: bool,
  stacking: Option<WindowZOrder>,
  decorations: Option<Decorations>,
  taskbar: Option<bool>,
  fullscreen: Option<bool>,
  retired: bool,
  recovery_at: Option<Instant>,
}

impl ManagedWindow {
  /// Whether reconciliation still owes this window work.
  fn settling(&self) -> bool {
    self.motion.is_some()
      || self.source.is_some()
      || self.retire_after.is_some()
      || self.frame.in_flight()
  }
}

/// Serializes application-window mutation decisions.
#[derive(Default)]
pub struct PlacementCoordinator {
  windows: HashMap<Uuid, ManagedWindow>,
  intents: HashMap<Uuid, WindowIntent>,
  dirty: HashSet<Uuid>,
  cleanup_overlays: HashSet<Uuid>,
  cleanup_at: Option<Instant>,
  focus: Option<Uuid>,
  layout_dirty: bool,
  batch: u64,
  presented_frame: u64,
  clock_failed: bool,
}

impl PlacementCoordinator {
  /// Records a real compositor boundary, independent of any motion clock.
  pub fn presented(&mut self, frame: u64) {
    self.presented_frame = self.presented_frame.max(frame);
  }

  /// Invalidates readiness after loss of compositor service.
  pub fn presentation_failed(&mut self) {
    self.clock_failed = true;
    for window in self.windows.values_mut() {
      window.cancelled = true;
    }
  }

  /// Queues fresh native observations without requesting new animations.
  pub fn observe(&mut self, id: Uuid) {
    if self.windows.contains_key(&id) {
      self.dirty.insert(id);
    }
  }

  /// Returns the earliest outstanding recovery deadline.
  pub fn deadline(&self, paused: bool) -> Option<Instant> {
    if !paused && (self.layout_dirty || !self.dirty.is_empty()) {
      return Some(Instant::now());
    }
    self
      .windows
      .values()
      .flat_map(|window| {
        [
          (!paused).then(|| window.frame.deadline()).flatten(),
          (!paused)
            .then(|| {
              window
                .motion
                .as_ref()
                .and_then(MotionOwner::preparation)
                .map(|motion| motion.deadline)
            })
            .flatten(),
          window.retry_at,
          window
            .retired
            .then_some(window.recovery_at)
            .flatten()
            .map(|at| at + Duration::from_millis(250)),
        ]
      })
      .flatten()
      .chain(self.cleanup_at)
      .min()
  }
  /// Queues opacity without touching applications.
  pub fn opacity(&mut self, id: Uuid, value: OpacityValue) {
    self.intents.entry(id).or_default().opacity = Some(value);
    self.dirty.insert(id);
  }

  /// Adjusts desired, not temporarily concealed, alpha.
  pub fn adjust_opacity(&mut self, id: Uuid, delta: Delta<OpacityValue>) {
    let current = self
      .intents
      .get(&id)
      .and_then(|intent| intent.opacity)
      .or_else(|| {
        self
          .windows
          .get(&id)
          .and_then(|window| window.last_requested_alpha)
      })
      .unwrap_or_default();
    self.opacity(id, current.adjust(&delta));
  }

  /// Queues title-bar style reconciliation.
  pub fn title_bar(&mut self, id: Uuid, visible: bool) {
    self.intents.entry(id).or_default().title_bar = Some(visible);
    self.dirty.insert(id);
  }

  /// Requests native minimization without immediate mutation.
  pub fn minimize(&mut self, id: Uuid) {
    self.intents.entry(id).or_default().minimize = true;
    self.dirty.insert(id);
  }

  /// Drops completed native minimize intent.
  pub fn minimized(&mut self, id: Uuid) {
    if let Some(intent) = self.intents.get_mut(&id) {
      intent.minimize = false;
    }
  }

  /// Returns outstanding verification and command work.
  pub fn has_pending(&self) -> bool {
    self.layout_dirty
      || !self.dirty.is_empty()
      || !self.cleanup_overlays.is_empty()
      || self.focus.is_some()
      || self
        .windows
        .values()
        .any(|window| window.retired || window.settling())
  }

  /// Cancels operations and restores surviving windows.
  pub fn release(&mut self, id: Uuid, alive: bool) {
    if self.focus == Some(id) {
      self.focus = None;
    }
    self.intents.remove(&id);
    self.dirty.remove(&id);
    if let Some(window) = self.windows.get_mut(&id) {
      window.frame.suspend();
      window.retired = true;
      window.recovery_at = Some(Instant::now());
      if alive {
        if let Err(err) = window.native.release() {
          tracing::warn!("Window recovery failed: {err}");
          return;
        }
      }
    }
    self.windows.remove(&id);
    self.cleanup_overlays.insert(id);
    self.cleanup_at = Some(Instant::now());
  }

  /// Releases all owned state during shutdown.
  pub fn release_all(&mut self) {
    let ids = self.windows.keys().copied().collect::<Vec<_>>();
    for id in ids {
      self.release(id, true);
    }
  }

  /// Cancels transactions before native user control.
  pub fn suspend(&mut self, id: Uuid) {
    if let Some(window) = self.windows.get_mut(&id) {
      window.frame.suspend();
      window.cancelled = true;
      self.dirty.insert(id);
    }
  }

  /// Cancels failed visuals without replaying geometry.
  pub fn cancel_presentation(&mut self, id: Uuid) {
    if let Some(window) = self.windows.get_mut(&id) {
      window.cancelled = true;
      self.dirty.insert(id);
    }
  }

  /// Invalidates constraints and in-flight operations.
  pub fn invalidate(&mut self) {
    self.layout_dirty = true;
    for window in self.windows.values_mut() {
      window.frame.restart();
      window.cancelled = true;
    }
    self.intents.clear();
    self.dirty.extend(self.windows.keys().copied());
  }
  /// Releases overlays after native recovery succeeds.
  pub fn cleanup(
    &mut self,
    animations: &mut crate::animation_manager::AnimationManager,
  ) {
    let retired = self
      .windows
      .iter()
      .filter(|(_, window)| {
        window.retired
          && window
            .recovery_at
            .is_none_or(|at| at.elapsed() >= Duration::from_millis(250))
      })
      .map(|(id, _)| *id)
      .collect::<Vec<_>>();
    for id in retired {
      let alive = self
        .windows
        .get(&id)
        .is_some_and(|entry| entry.native.validate().is_ok());
      self.release(id, alive);
    }
    if self.cleanup_at.is_some_and(|at| at > Instant::now()) {
      return;
    }
    for id in self.cleanup_overlays.clone() {
      match animations.retire_overlay(&id) {
        Ok(()) => {
          self.cleanup_overlays.remove(&id);
        }
        Err(err) => tracing::warn!("Overlay cleanup failed: {err}"),
      }
    }
    self.cleanup_at = (!self.cleanup_overlays.is_empty())
      .then(|| Instant::now() + Duration::from_millis(250));
  }

  /// Reports geometry currently owned by reconciliation.
  pub fn owns_geometry(&self, id: Uuid) -> bool {
    self.windows.get(&id).is_some_and(ManagedWindow::settling)
  }

  /// Reports a source cloaked behind its overlay.
  ///
  /// The shell reports that cloak as a hide event; it is ours, not the
  /// application's, so the window must stay managed.
  pub fn owns_visibility(&self, id: Uuid) -> bool {
    self.windows.get(&id).is_some_and(|entry| {
      entry.source.is_some() && entry.conceal == Some(ConcealMethod::Cloak)
    })
  }
}

/// Applies queued state through guarded native sessions.
#[allow(clippy::too_many_lines)]
pub fn platform_sync(
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let layout_changed = std::mem::take(&mut state.native_sync.layout_dirty);
  // Every commit solves the layout again. A queued redraw is not the only
  // thing that moves a rect: a dropped floating window writes its own
  // placement and asks for no redraw, and reconciling that window against
  // a snapshot from before the drag puts it back where it started.
  state.layout_snapshot = crate::layout_snapshot::LayoutSnapshot::capture(
    &state.root_container,
  )?;
  let focused =
    state.focused_container().context("No focused container.")?;
  let redraw = if layout_changed {
    state.windows()
  } else {
    state.windows_to_redraw()
  }
  .iter()
  .map(CommonGetters::id)
  .collect::<HashSet<_>>();
  let mut reorder = state
    .pending_sync
    .workspaces_to_reorder()
    .iter()
    .map(CommonGetters::id)
    .collect::<HashSet<_>>();
  if state.pending_sync.needs_focus_update() {
    if let Some(workspace) = focused.workspace() {
      reorder.insert(workspace.id());
    }
  }
  let effects_changed = state.pending_sync.needs_all_effects_update()
    || state.pending_sync.needs_focused_effect_update();
  let mut windows = state.windows();
  let order = state
    .root_container
    .descendant_focus_order()
    .enumerate()
    .map(|(index, container)| (container.id(), index))
    .collect::<HashMap<_, _>>();
  windows.sort_by_key(|window| {
    std::cmp::Reverse(
      order.get(&window.id()).copied().unwrap_or(usize::MAX),
    )
  });
  let dirty = std::mem::take(&mut state.native_sync.dirty);
  let now = Instant::now();
  state.native_sync.cleanup(&mut state.animation_manager);
  let compositor_frame = state.native_sync.presented_frame;
  let plans = plan_animations(&windows, &redraw, state, config)?;
  if state.pending_sync.needs_focus_update() {
    state.native_sync.focus =
      focused.as_window_container().ok().map(|window| window.id());
  }

  let hide_corners = state.monitors_by_hide_corner();
  for window in &windows {
    let id = window.id();
    let pending = state
      .native_sync
      .windows
      .get(&id)
      .is_some_and(|entry| entry.settling() || entry.visibility_pending);
    let workspace = window.workspace().context("No workspace.")?;
    let needs_reorder = reorder.contains(&workspace.id());
    if state
      .native_sync
      .windows
      .get(&id)
      .is_some_and(|entry| entry.retired)
    {
      continue;
    }
    if !redraw.contains(&id)
      && !effects_changed
      && !needs_reorder
      && !pending
      && !dirty.contains(&id)
    {
      continue;
    }
    let monitor = window.monitor().context("No monitor.")?;
    let pass = ReconcilePass {
      owner: &monitor,
      now,
      compositor_frame,
      reorder: needs_reorder,
      hide_corner: hide_corners
        .get(&monitor.id())
        .copied()
        .unwrap_or(HideCorner::BottomRight),
    };
    if let Err(err) =
      reconcile_window(window, pass, plans.get(&id), state, config)
    {
      tracing::warn!("Window reconciliation failed: {err}");
      state.native_sync.release(id, window.native().is_valid());
    }
  }

  if let Some(id) = state.native_sync.focus {
    if focused.id() != id {
      state.native_sync.focus = None;
    } else if let Some(entry) = state.native_sync.windows.get(&id) {
      if entry.frame.desired.state == NativeState::Minimized {
        state.native_sync.focus = None;
      } else if entry.source.is_none()
        && entry.frame.settled()
        && entry.frame.desired.state != NativeState::Minimized
        && !entry.visibility_pending
        && entry
          .visibility
          .as_ref()
          .is_some_and(|(visible, _)| *visible)
      {
        if let Err(err) = entry
          .native
          .window()
          .and_then(wm_platform::NativeWindow::focus)
        {
          tracing::warn!("Window focus failed: {err}");
        }
        state.native_sync.focus = None;
      }
    }
  }
  if state.pending_sync.needs_focus_update() {
    if focused.as_window_container().is_err() {
      if let Err(err) = state.dispatcher.reset_focus() {
        tracing::warn!("Desktop focus failed: {err}");
      }
    }
    state.emit_event(WmEvent::FocusChanged {
      focused_container: focused.to_dto()?,
    });
  }
  if state.pending_sync.needs_cursor_jump()
    && config.value.general.cursor_jump.enabled
  {
    let monitor = focused.monitor().context("No monitor.")?;
    let should_jump = config.value.general.cursor_jump.trigger
      == CursorJumpTrigger::WindowFocus
      || state
        .dispatcher
        .cursor_position()
        .ok()
        .and_then(|point| state.monitor_at_point(&point))
        .is_none_or(|current| current.id() != monitor.id());
    if should_jump {
      state
        .dispatcher
        .set_cursor_position(&focused.to_rect()?.center_point())?;
    }
  }
  release_ready(state);
  state.animation_manager.begin_pending();
  state.pending_sync.clear();
  Ok(())
}

/// Selects presentation policy once, before any application geometry
/// changes.
fn plan_animations<'a>(
  windows: &[WindowContainer],
  redraw: &HashSet<Uuid>,
  state: &mut WmState,
  config: &'a UserConfig,
) -> anyhow::Result<HashMap<Uuid, AnimationPlan<'a>>> {
  let mut plans = HashMap::new();
  if state.native_sync.clock_failed
    && state.animation_manager.has_overlays()
  {
    return Ok(plans);
  }
  for window in windows
    .iter()
    .filter(|window| redraw.contains(&window.id()))
  {
    let owner = window.monitor().context("No monitor.")?;
    let monitor = owner.native_properties();
    let visible =
      window.workspace().context("No workspace.")?.is_displayed();
    let Some(trigger) = AnimationTrigger::select(
      state.pending_sync.should_skip_animations()
        || window.active_drag().is_some(),
      state.pending_sync.workspace_slide_for(owner.id()),
      visible,
      state.pending_sync.open_animation_windows().contains(window),
    ) else {
      continue;
    };
    let tile = state
      .layout_snapshot
      .rect(window.id())?
      .apply_delta(&window.border_delta(), None);
    if let Some(effect) =
      state.animation_manager.animation_effect_for_window(
        window, trigger, &tile, &monitor, config,
      )
    {
      plans.insert(
        window.id(),
        AnimationPlan {
          effect,
          trigger,
          path: trigger.path(&tile, &monitor.bounds),
        },
      );
    }
  }
  if !plans.is_empty() {
    state.native_sync.clock_failed = false;
    state.native_sync.batch = state.native_sync.batch.wrapping_add(1);
    let captures = windows
      .iter()
      .filter(|window| {
        plans.get(&window.id()).is_some_and(|plan| {
          !plan.trigger.uses_native()
            || state.animation_manager.has_overlay(&window.id())
        })
      })
      .map(|window| (window.id(), window.native().id()))
      .collect::<Vec<_>>();
    state
      .animation_manager
      .pre_capture(&captures, &state.dispatcher)?;
  }
  Ok(plans)
}

/// Releases each ready batch together; failed participants no longer block
/// it.
fn release_ready(state: &mut WmState) {
  let blocked = state
    .native_sync
    .windows
    .values()
    .filter_map(|entry| {
      entry
        .motion
        .as_ref()
        .and_then(MotionOwner::preparation)
        .filter(|motion| {
          !motion.ready(
            entry.frame.generation,
            state.native_sync.presented_frame,
          )
        })
        .map(|motion| motion.batch)
    })
    .collect::<HashSet<_>>();
  for (id, entry) in &mut state.native_sync.windows {
    if entry
      .motion
      .as_ref()
      .and_then(MotionOwner::preparation)
      .is_some_and(|motion| !blocked.contains(&motion.batch))
    {
      entry.motion = None;
      state.animation_manager.release_animation(id);
      tracing::debug!(window = %id, "Presentation motion ready.");
    }
  }
}

/// Carries native-independent constants for one reconciliation pass.
#[derive(Clone, Copy)]
struct ReconcilePass<'a> {
  owner: &'a Monitor,
  now: Instant,
  compositor_frame: u64,
  reorder: bool,
  hide_corner: HideCorner,
}

/// Reads current geometry rather than treating a successful write as
/// completion.
fn observe(native: &PlacementSession) -> anyhow::Result<ObservedFrame> {
  let window = native.window()?;
  Ok(ObservedFrame {
    rect: native.observed_frame()?,
    dpi: native.dpi()?,
    state: if window.is_minimized()? {
      NativeState::Minimized
    } else if window.is_maximized()? {
      NativeState::Maximized
    } else {
      NativeState::Normal
    },
  })
}

/// Resolves the requested native state before its dependent geometry.
fn desired_state(window: &WindowContainer, minimize: bool) -> NativeState {
  if minimize || window.state() == WindowState::Minimized {
    NativeState::Minimized
  } else if matches!(window.state(), WindowState::Fullscreen(ref state) if state.maximized)
  {
    NativeState::Maximized
  } else {
    NativeState::Normal
  }
}

/// Builds a placement request in the backend's normalized coordinate
/// system.
fn desired_frame(
  native: &PlacementSession,
  target: &Rect,
  monitor: &NativeMonitorProperties,
  state: NativeState,
) -> anyhow::Result<DesiredFrame> {
  Ok(DesiredFrame {
    rect: target.clone(),
    monitor: monitor.bounds.clone(),
    dpi: native.expected_dpi(monitor.dpi)?,
    state,
  })
}

/// Keeps a hidden source on its owning display without resizing it.
fn parking_rect(
  frame: &Rect,
  working_area: &Rect,
  corner: HideCorner,
) -> Rect {
  Rect::from_xy(
    match corner {
      HideCorner::BottomLeft => working_area.left + 1 - frame.width(),
      HideCorner::BottomRight => working_area.right - 1,
    },
    working_area.bottom - 1,
    frame.width(),
    frame.height(),
  )
}

/// Applies one generation-checked request, then immediately observes its
/// result.
fn reconcile_frame(
  entry: &mut ManagedWindow,
  now: Instant,
) -> anyhow::Result<ObservedFrame> {
  let observed = observe(&entry.native)?;
  if let Some(request) = entry.frame.next(&observed, now) {
    entry.frame.apply(&request, now, |mutation| {
      match mutation {
        NativeMutation::Frame(rect) => entry.native.set_frame(rect)?,
        NativeMutation::Restore(rect) => entry.native.restore(rect)?,
        NativeMutation::Minimize => entry.native.minimize()?,
        NativeMutation::Maximize => entry.native.maximize()?,
      }
      Ok(())
    })?;
    let observed = observe(&entry.native)?;
    // Observation may acknowledge the write, but never issues a second
    // request.
    entry.frame.observe(&observed, now);
    Ok(observed)
  } else {
    Ok(observed)
  }
}

/// Stops motion without destroying the cover or abandoning source
/// recovery.
/// Resolves the one cloak state both hiding and presentation want.
///
/// Workspace hiding cloaks only under `HideMethod::Cloak`; a parked window
/// is already out of sight and keeps whatever the parking path chose.
fn desired_cloak(
  cloak_for_source: bool,
  native_visible: bool,
  hidden_parking: bool,
  hide_method: &HideMethod,
) -> bool {
  cloak_for_source
    || (!native_visible
      && !hidden_parking
      && *hide_method == HideMethod::Cloak)
}

/// Reads native visibility through our own source cloak.
///
/// A shown window cloaked for its overlay counts as visible: the cloak is
/// ours and expected. A window that is meant to be hidden reports what
/// the shell sees, so a departed workspace can finish its restore.
fn observed_visible(
  actual: bool,
  cloak_for_source: bool,
  native_visible: bool,
) -> bool {
  actual || (cloak_for_source && native_visible)
}

/// Converges observed cloaking on the desired state.
///
/// Observation rather than a cache: a cloak left by a crashed instance or
/// by the shell is healed on the next pass, and only our own cloak is
/// ever removed.
fn apply_cloak(
  entry: &mut ManagedWindow,
  cloaked: bool,
) -> anyhow::Result<()> {
  if entry.native.is_cloaked()? != cloaked {
    entry.native.cloak(cloaked)?;
  }
  Ok(())
}

fn begin_handoff(
  entry: &mut ManagedWindow,
  id: &Uuid,
  state: &mut WmState,
) -> anyhow::Result<()> {
  if matches!(entry.motion, Some(MotionOwner::Native)) {
    entry.frame.restart();
  }
  entry.motion = None;
  entry.cancelled = false;
  state.animation_manager.finish_animation(id)?;
  if let Some(source) = &mut entry.source {
    source.restoring = true;
  } else if state.animation_manager.has_overlay(id) {
    entry.retire_after.get_or_insert(
      wm_platform::FrameClock::current_frame()
        .unwrap_or(state.native_sync.presented_frame),
    );
  }
  Ok(())
}

/// Owns placement, independent overlay readiness, and source visibility.
#[allow(clippy::too_many_lines)]
fn reconcile_window(
  window: &WindowContainer,
  pass: ReconcilePass,
  plan: Option<&AnimationPlan>,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let owner = pass.owner;
  let id = window.id();
  let monitor = owner.native_properties();
  let visible =
    window.workspace().context("No workspace.")?.is_displayed();
  let dragging = window.active_drag().is_some();
  let target = state
    .layout_snapshot
    .rect(id)?
    .apply_delta(&window.total_border_delta()?, None);
  let wants_minimize = state
    .native_sync
    .intents
    .get(&id)
    .is_some_and(|intent| intent.minimize);
  let native_state = desired_state(window, wants_minimize);
  if let std::collections::hash_map::Entry::Vacant(slot) =
    state.native_sync.windows.entry(id)
  {
    let token = isize::from_le_bytes(
      id.as_bytes()[..std::mem::size_of::<isize>()].try_into()?,
    );
    let native = PlacementSession::new(window.native().clone(), token)?;
    let desired =
      match desired_frame(&native, &target, &monitor, native_state) {
        Ok(desired) => desired,
        Err(err) => {
          let _ = native.release();
          return Err(err);
        }
      };
    let conceal = native.conceal_method();
    let source = native
      .opening_concealed()
      .then_some(SourceLease { restoring: true });
    slot.insert(ManagedWindow {
      native,
      frame: FrameReconciler::new(desired),
      motion: None,
      source,
      retire_after: None,
      cancelled: false,
      retry_at: None,
      last_requested_alpha: None,
      opacity: AppliedOpacity::Unknown,
      conceal: Some(conceal),
      visibility: None,
      visibility_pending: false,
      stacking: None,
      decorations: None,
      taskbar: None,
      fullscreen: None,
      retired: false,
      recovery_at: None,
    });
  }
  // Keep the session outside the map while borrowing other coordinator
  // fields.
  let mut entry = state
    .native_sync
    .windows
    .remove(&id)
    .context("No native session.")?;
  let result = reconcile_managed(
    window,
    &mut entry,
    plan,
    pass,
    state,
    config,
    &target,
    native_state,
    visible,
    dragging,
  );
  state.native_sync.windows.insert(id, entry);
  result
}

/// Reconciles a session while retaining it on every failure path.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn reconcile_managed(
  window: &WindowContainer,
  entry: &mut ManagedWindow,
  plan: Option<&AnimationPlan>,
  pass: ReconcilePass,
  state: &mut WmState,
  config: &UserConfig,
  target: &Rect,
  native_state: NativeState,
  visible: bool,
  dragging: bool,
) -> anyhow::Result<()> {
  let ReconcilePass {
    owner,
    now,
    compositor_frame,
    reorder,
    hide_corner,
  } = pass;
  let id = window.id();
  let monitor = owner.native_properties();
  entry.native.validate()?;
  entry.retry_at = None;
  let mut observed = observe(&entry.native)?;
  if let Some(plan) = plan {
    if plan.trigger.uses_native()
      && visible
      && entry.source.is_none()
      && !state.animation_manager.has_overlay(&id)
      && native_state == NativeState::Normal
      && observed.state == NativeState::Normal
      && !dragging
    {
      state.animation_manager.prepare_native(
        id,
        plan,
        entry.native.window()?.frame()?,
        monitor.refresh_rate.unwrap_or(60),
      );
      entry.motion = Some(MotionOwner::Native);
      entry.retire_after = None;
    } else if entry.native.supports_presentation()
      && entry.conceal.is_some()
      && native_state == NativeState::Normal
      && observed.state == NativeState::Normal
      && !dragging
    {
      let opening = plan.trigger == AnimationTrigger::WindowOpened;
      let mut prepare = || -> anyhow::Result<()> {
        // Windows thumbnails stay live while their source is concealed.
        // Hide an opening source before allocating its overlay; a warm
        // application can already have painted its complete window.
        // Parking backends must capture before moving the source.
        if opening && !entry.native.uses_parking() {
          entry
            .source
            .get_or_insert_with(SourceLease::default)
            .restoring = false;
          if entry.conceal == Some(ConcealMethod::Cloak) {
            apply_cloak(entry, true)?;
          } else {
            let alpha = Some(OpacityValue(0.0));
            entry.native.opacity(alpha)?;
            entry.opacity = AppliedOpacity::Value(alpha);
          }
        }
        state.animation_manager.prepare_animation(
          window,
          plan,
          &monitor,
          &state.dispatcher,
        )
      };
      match prepare() {
        Ok(()) => {
          entry.motion =
            Some(MotionOwner::Overlay(MotionPreparation::new(
              state.native_sync.batch,
              wm_platform::FrameClock::current_frame()?,
              now,
              !opening,
            )));
          entry.retire_after = None;
          if let Some(source) = &mut entry.source {
            source.restoring = false;
          }
          tracing::debug!(window = %id, trigger = ?plan.trigger, "Presentation overlay prepared.");
        }
        Err(err) => {
          tracing::warn!(window = %id, "Presentation preparation failed: {err}");
          entry.cancelled = true;
        }
      }
    } else {
      tracing::debug!(
        window = %id,
        trigger = ?plan.trigger,
        supported = entry.native.supports_presentation(),
        conceal = ?entry.conceal,
        desired = ?native_state,
        observed = ?observed.state,
        dragging,
        "Presentation source capability declined."
      );
    }
  }
  let native_motion = matches!(entry.motion, Some(MotionOwner::Native));
  let visible_target = state
    .layout_snapshot
    .rect(id)?
    .apply_delta(&window.border_delta(), None);
  if (native_motion
    && (!visible
      || observed.state != NativeState::Normal
      || state.pending_sync.should_skip_animations()
      || state.animation_manager.native_target(&id)
        != Some(&visible_target)))
    || dragging
    || native_state != NativeState::Normal
    || matches!(window.state(), WindowState::Fullscreen(_))
    || entry
      .motion
      .as_ref()
      .and_then(MotionOwner::preparation)
      .is_some_and(|motion| now >= motion.deadline)
    || state.animation_manager.is_complete(&id)
    || entry.cancelled
  {
    begin_handoff(entry, &id, state)?;
  }
  // Existing content needs a presented cover. Opening content must stay
  // invisible throughout preparation, including its first native frame.
  let preparation =
    entry.motion.as_ref().and_then(MotionOwner::preparation);
  let retaining_source = preparation
    .is_some_and(|motion| motion.retains_source(compositor_frame));
  if !retaining_source && preparation.is_some() && entry.source.is_none() {
    entry.native.present(true)?;
    entry.source = Some(SourceLease::default());
  }
  let suppressing = entry
    .source
    .as_ref()
    .is_some_and(|source| !source.restoring);
  let presenting = suppressing
    || retaining_source
    || state.animation_manager.is_running(&id);
  let hide_method = &config.value.general.hide_method;
  let hidden_parking = !visible
    && !presenting
    && native_state != NativeState::Minimized
    && (entry.native.uses_parking()
      || *hide_method == HideMethod::PlaceInCorner);
  let parked =
    hidden_parking || (suppressing && entry.native.uses_parking());
  let native_frame = state.animation_manager.native_frame(&id);
  let operational_target = if let Some(frame) = native_frame {
    #[cfg(target_os = "windows")]
    let frame =
      frame.apply_delta(&window.native_properties().shadow_borders, None);
    frame
  } else if retaining_source {
    observed.rect.clone()
  } else if parked {
    let corner =
      parking_rect(&observed.rect, &monitor.working_area, hide_corner);
    entry.native.remember_restore(target)?;
    corner
  } else {
    target.clone()
  };
  let desired = desired_frame(
    &entry.native,
    &operational_target,
    &monitor,
    native_state,
  )?;
  if entry.frame.desired.dpi != desired.dpi {
    window
      .update_native_properties(|properties| properties.min_size = None);
  }
  entry.frame.retarget(desired);
  if dragging {
    entry.frame.suspend();
  } else if entry.frame.phase == ReconcilePhase::Suspended {
    entry.frame.restart();
  }

  let effects = if window.has_focus(None) {
    &config.value.window_effects.focused_window
  } else {
    &config.value.window_effects.other_windows
  };
  let intent = state.native_sync.intents.get(&id);
  let requested_alpha =
    intent.and_then(|intent| intent.opacity).or_else(|| {
      effects
        .transparency
        .enabled
        .then_some(effects.transparency.opacity)
    });
  entry.last_requested_alpha = requested_alpha;
  // Retain alpha ownership throughout destination reconciliation,
  // including handoff.
  let concealed = entry.source.is_some() || hidden_parking;
  let alpha = effective_opacity(concealed, requested_alpha);
  if entry.native.supports_opacity()
    && entry.opacity != AppliedOpacity::Value(alpha)
  {
    entry.native.opacity(alpha)?;
    entry.opacity = AppliedOpacity::Value(alpha);
  }
  let (border, corner) = focus_decorations(window, config);
  let decorations = Decorations {
    border,
    corner,
    title_bar: intent.and_then(|intent| intent.title_bar).or_else(|| {
      (config
        .value
        .window_effects
        .focused_window
        .hide_title_bar
        .enabled
        || config
          .value
          .window_effects
          .other_windows
          .hide_title_bar
          .enabled)
        .then_some(!effects.hide_title_bar.enabled)
    }),
  };
  if entry.decorations.as_ref() != Some(&decorations) {
    entry
      .native
      .set_decorations(decorations.border.as_ref(), &decorations.corner)?;
    entry.native.title_bar(decorations.title_bar)?;
    entry.decorations = Some(decorations);
  }
  // Conceal before any placement write. Cloaked sources need the same
  // ordering as alpha sources or the destination can flash on screen.
  let native_visible = visible || presenting;
  let cloak_for_source =
    concealed && entry.conceal == Some(ConcealMethod::Cloak);
  if !retaining_source
    && native_state != NativeState::Minimized
    && cloak_for_source
  {
    if let Err(err) = apply_cloak(entry, true) {
      tracing::warn!(window = %id, "Source cloaking failed: {err}");
      entry.conceal = None;
      entry.cancelled = true;
    }
  }
  if !retaining_source && !dragging {
    observed = reconcile_frame(entry, now)?;
  } else if retaining_source {
    entry.frame.observe(&observed, now);
  }
  let converged = entry.frame.phase == ReconcilePhase::Converged
    && entry.frame.converged(&observed);
  if converged && !parked {
    entry.native.unpark()?;
    window.set_has_pending_dpi_adjustment(false);
  }
  if entry.frame.phase == ReconcilePhase::Failed {
    begin_handoff(entry, &id, state)?;
    if !parked
      && !dragging
      && visible
      && matches!(window.state(), WindowState::Tiling)
    {
      if let Some(mut minimum) = entry.frame.constraint(
        &observed,
        entry.native.minimum_size()?,
        now,
      ) {
        let tile = state.layout_snapshot.rect(id)?;
        if minimum.0 > 0 {
          minimum.0 = (minimum.0 - (target.width() - tile.width())).max(0);
        }
        if minimum.1 > 0 {
          minimum.1 =
            (minimum.1 - (target.height() - tile.height())).max(0);
        }
        if window.native_properties().min_size != Some(minimum) {
          window.update_native_properties(|properties| {
            properties.min_size = Some(minimum);
          });
          state.native_sync.layout_dirty = true;
        }
      }
    }
  }

  // Logical workspace visibility does not hide a departing presentation
  // early.
  if !retaining_source && native_state != NativeState::Minimized {
    // Reveal workspace arrivals only after their placement write.
    if !cloak_for_source {
      apply_cloak(
        entry,
        desired_cloak(false, native_visible, hidden_parking, hide_method),
      )?;
    }
    let was_visible = observed_visible(
      entry.native.is_visible()?,
      cloak_for_source,
      native_visible,
    );
    if native_visible && !was_visible {
      entry.native.show(true)?;
    } else if !native_visible
      && !hidden_parking
      && was_visible
      && *hide_method == HideMethod::Hide
    {
      entry.native.show(false)?;
    }
    entry.visibility =
      Some((native_visible && !hidden_parking, hide_method.clone()));
    let actual_visible = observed_visible(
      entry.native.is_visible()?,
      cloak_for_source,
      native_visible,
    );
    entry.visibility_pending =
      !hidden_parking && actual_visible != native_visible;
    window.set_display_state(if hidden_parking && converged {
      DisplayState::Hidden
    } else {
      match (native_visible, actual_visible) {
        (true, true) => DisplayState::Shown,
        (true, false) => DisplayState::Showing,
        (false, false) => DisplayState::Hidden,
        (false, true) => DisplayState::Hiding,
      }
    });
  }
  if let Some(MotionOwner::Overlay(motion)) = &mut entry.motion {
    motion.observe(
      entry.frame.generation,
      suppressing && converged && !entry.visibility_pending,
      wm_platform::FrameClock::current_frame()?,
    );
  }
  let restoring =
    entry.source.as_ref().is_some_and(|source| source.restoring);
  // Failed restoration cannot relinquish a source that is still parked.
  // The retained native session will retry recovery at its own deadline.
  if restoring
    && visible
    && !dragging
    && entry.frame.phase == ReconcilePhase::Failed
    && entry.native.uses_parking()
    && crate::native_reconciler::frames_match(
      &observed.rect,
      &parking_rect(&observed.rect, &monitor.working_area, hide_corner),
    )
  {
    anyhow::bail!("Source restoration remains parked.");
  }
  if native_state == NativeState::Minimized && converged {
    entry.visibility_pending = false;
  }
  if restoring
    && (converged
      || dragging
      || entry.frame.phase == ReconcilePhase::Failed)
    && !entry.visibility_pending
  {
    if visible && native_state != NativeState::Minimized {
      state
        .animation_manager
        .place_overlay(&id, &entry.native.window()?.frame()?)?;
    }
    let final_alpha = effective_opacity(hidden_parking, requested_alpha);
    if entry.native.supports_opacity() {
      entry.native.opacity(final_alpha)?;
    }
    entry.opacity = AppliedOpacity::Value(final_alpha);
    // The source must be composed again before its overlay retires.
    if cloak_for_source && native_state != NativeState::Minimized {
      apply_cloak(
        entry,
        desired_cloak(false, native_visible, hidden_parking, hide_method),
      )?;
    }
    entry.native.present(false)?;
    entry.source = None;
    // Preparation can fail after concealing but before creating an
    // overlay. There is then nothing to retire and no running frame clock
    // to acknowledge a retirement fence.
    entry.retire_after =
      state.animation_manager.has_overlay(&id).then(|| {
        wm_platform::FrameClock::current_frame()
          .unwrap_or(compositor_frame)
      });
    tracing::debug!(window = %id, "Presentation source restored.");
  }
  if entry.retire_after.is_some_and(|after| {
    compositor_frame > after || state.native_sync.clock_failed
  }) {
    state.animation_manager.retire_overlay(&id)?;
    entry.retire_after = None;
  }
  if entry.visibility_pending || (restoring && entry.source.is_some()) {
    entry.retry_at = Some(now + Duration::from_millis(250));
  }
  let z_order = match window.state() {
    WindowState::Floating(ref state) if state.shown_on_top => {
      WindowZOrder::TopMost
    }
    WindowState::Fullscreen(ref state) if state.shown_on_top => {
      WindowZOrder::TopMost
    }
    _ => WindowZOrder::Normal,
  };
  if visible
    && native_state != NativeState::Minimized
    && (entry.stacking.as_ref() != Some(&z_order) || reorder)
  {
    entry.native.set_z_order(&z_order)?;
    entry.stacking = Some(z_order);
  }
  let taskbar = visible || config.value.general.show_all_in_taskbar;
  if entry.taskbar != Some(taskbar) {
    entry.native.set_taskbar_visibility(taskbar)?;
    entry.taskbar = Some(taskbar);
  }
  let fullscreen = matches!(window.state(), WindowState::Fullscreen(_));
  if entry.fullscreen != Some(fullscreen) {
    entry.native.mark_fullscreen(fullscreen)?;
    entry.fullscreen = Some(fullscreen);
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Native ownership never requests source concealment.
  #[test]
  fn native_excludes_cover_readiness() {
    assert!(MotionOwner::Native.preparation().is_none());
    let owner = MotionOwner::Overlay(MotionPreparation::new(
      1,
      10,
      Instant::now(),
      true,
    ));
    let preparation = owner.preparation().expect("Overlay preparation.");
    assert!(preparation.retains_source(10));
    assert!(!preparation.retains_source(11));
  }

  #[test]
  fn source_concealment_cloaks_regardless_of_hide_method() {
    assert!(desired_cloak(true, true, false, &HideMethod::Hide));
    assert!(desired_cloak(true, true, false, &HideMethod::PlaceInCorner));
  }

  #[test]
  fn hidden_workspace_cloaks_only_under_cloak_method() {
    assert!(desired_cloak(false, false, false, &HideMethod::Cloak));
    assert!(!desired_cloak(false, false, false, &HideMethod::Hide));
    assert!(!desired_cloak(false, false, true, &HideMethod::Cloak));
  }

  #[test]
  fn visible_uncovered_source_is_uncloaked() {
    assert!(!desired_cloak(false, true, false, &HideMethod::Cloak));
  }

  #[test]
  fn presented_source_reads_visible_only_while_shown() {
    assert!(observed_visible(false, true, true));
    assert!(!observed_visible(false, true, false));
    assert!(!observed_visible(false, false, true));
    assert!(observed_visible(true, false, false));
  }
}
