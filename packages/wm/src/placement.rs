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
  animation_manager::{
    AnimationPlan, AnimationTrigger, MotionStart, WindowChange,
  },
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

/// Resolves the window's requested opacity, before any concealment.
fn requested_alpha(
  window: &WindowContainer,
  intent: Option<&WindowIntent>,
  config: &UserConfig,
) -> Option<OpacityValue> {
  let effects = if window.has_focus(None) {
    &config.value.window_effects.focused_window
  } else {
    &config.value.window_effects.other_windows
  };
  intent.and_then(|intent| intent.opacity).or_else(|| {
    effects
      .transparency
      .enabled
      .then_some(effects.transparency.opacity)
  })
}

/// Resolves the window's requested border, corners, and title bar.
fn desired_decorations(
  window: &WindowContainer,
  intent: Option<&WindowIntent>,
  config: &UserConfig,
) -> Decorations {
  let window_effects = &config.value.window_effects;
  let effects = if window.has_focus(None) {
    &window_effects.focused_window
  } else {
    &window_effects.other_windows
  };
  let (border, corner) = focus_decorations(window, config);
  Decorations {
    border,
    corner,
    title_bar: intent.and_then(|intent| intent.title_bar).or_else(|| {
      (window_effects.focused_window.hide_title_bar.enabled
        || window_effects.other_windows.hide_title_bar.enabled)
        .then_some(!effects.hide_title_bar.enabled)
    }),
  }
}

/// Whether a focus change alters what the window should look like.
///
/// A focus change touches two windows at most. Comparing each window's
/// applied effects with what focus now asks for finds them without
/// reconciling, and so querying, every window.
fn focus_effects_stale(
  window: &WindowContainer,
  coordinator: &PlacementCoordinator,
  config: &UserConfig,
) -> bool {
  let id = window.id();
  let intent = coordinator.intents.get(&id);
  coordinator.windows.get(&id).is_none_or(|entry| {
    entry.decorations.as_ref()
      != Some(&desired_decorations(window, intent, config))
      || entry.last_requested_alpha
        != requested_alpha(window, intent, config)
  })
}

/// Distinguishes untouched and applied opacity.
#[derive(PartialEq)]
enum AppliedOpacity {
  Unknown,
  Value(Option<OpacityValue>),
}

/// Owns motion independently of source concealment.
enum MotionOwner {
  /// The real window moves. It has no readiness of its own, but starts
  /// with the rest of its batch.
  Native {
    batch: u64,
  },
  Overlay(MotionPreparation),
}

impl MotionOwner {
  /// Returns readiness only for proxy motion.
  fn preparation(&self) -> Option<&MotionPreparation> {
    match self {
      Self::Native { .. } => None,
      Self::Overlay(preparation) => Some(preparation),
    }
  }
}

/// How long a restored window's overlay waits for its companions.
///
/// A companion, such as a border ring drawn by another process, shows
/// itself again only after it notices the reveal, about 13-20ms later.
/// Until then the overlay still draws it. The cap bounds the wait when a
/// companion never reveals.
const COMPANION_HANDOVER: Duration = Duration::from_millis(50);

/// How often a held overlay rechecks its companions when no compositor
/// tick arrives.
const COMPANION_POLL: Duration = Duration::from_millis(4);

/// Holds an overlay until its source is composed again.
#[derive(Clone, Copy, Debug)]
struct RetireFence {
  /// The overlay retires only after a compositor frame later than this.
  after: u64,
  /// When the source was restored, which starts the companion hold.
  since: Instant,
  /// The compositor frame current when the companions were first seen
  /// uncloaked.
  ///
  /// An uncloak reaches the screen only in a frame composed after it, so
  /// retiring on the pass that observes it could drop the overlay a frame
  /// before the companion is drawn.
  revealed_at: Option<u64>,
}

impl RetireFence {
  /// Creates a fence for a source restored at `since`.
  fn new(after: u64, since: Instant) -> Self {
    Self {
      after,
      since,
      revealed_at: None,
    }
  }

  /// Whether the overlay may retire.
  ///
  /// Requires a compositor frame after the fence, then either an expired
  /// companion hold or a frame composed after the companions were seen
  /// revealed. `revealed` is queried only once the source frame has
  /// passed, the hold is still running, and no reveal has been recorded.
  fn due(
    &mut self,
    compositor_frame: u64,
    now: Instant,
    revealed: impl FnOnce() -> bool,
  ) -> bool {
    if compositor_frame <= self.after {
      return false;
    }
    if now >= self.since + COMPANION_HANDOVER {
      return true;
    }
    if let Some(frame) = self.revealed_at {
      return compositor_frame > frame;
    }
    if revealed() {
      self.revealed_at = Some(compositor_frame);
    }
    false
  }

  /// When a fence that is not yet due should be checked again, without
  /// relying on compositor ticks.
  ///
  /// `None` while the source awaits composition; that wait already
  /// follows the frame clock.
  fn recheck_at(
    &self,
    compositor_frame: u64,
    now: Instant,
  ) -> Option<Instant> {
    (compositor_frame > self.after)
      .then(|| (now + COMPANION_POLL).min(self.since + COMPANION_HANDOVER))
  }
}

/// Where a window's companion overlay is in its life.
///
/// The overlay draws the companions of a window in native motion. See
/// `AnimationManager::decorate_native`.
#[derive(Clone, Copy, Debug)]
enum DecorationPhase {
  /// The motion runs and the published `DECORATED` flag is set.
  Drawing,
  /// The motion has ended and the flag is cleared. The overlay is held
  /// until the companions show themselves again.
  Revealing(RetireFence),
}

/// Owns native mutations for one window.
struct ManagedWindow {
  native: PlacementSession,
  frame: FrameReconciler,
  motion: Option<MotionOwner>,
  source: Option<SourceLease>,
  retire_after: Option<RetireFence>,
  /// The window's companion overlay, if it has one.
  decoration: Option<DecorationPhase>,
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
      || self.decoration.is_some()
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
  /// Windows whose move was asked for while a slide owned them. Each
  /// moves from where its slide ends once the slide completes.
  deferred_moves: HashSet<Uuid>,
  focus: Option<Uuid>,
  /// The window carrying the published focus flag.
  marked_focus: Option<Uuid>,
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

  /// Moves the published focus flag to `focused`.
  ///
  /// Published when focus changes, not when the OS foreground follows:
  /// that waits for the reveal, and a border renderer colouring a ring
  /// from the foreground alone draws it unfocused first.
  fn mark_focus(&mut self, focused: Option<Uuid>) {
    if self.marked_focus == focused {
      return;
    }
    // A window managed this commit has no session yet; a later commit
    // marks it once one exists.
    if focused.is_some_and(|id| !self.windows.contains_key(&id)) {
      return;
    }
    let previous = std::mem::replace(&mut self.marked_focus, focused);
    for (id, flag) in [(previous, false), (focused, true)] {
      let Some(entry) = id.and_then(|id| self.windows.get(&id)) else {
        continue;
      };
      if let Err(err) = entry.native.mark_focused(flag) {
        tracing::debug!("Focus flag update failed: {err}");
      }
    }
  }

  /// Queues fresh native observations without requesting new animations.
  pub fn observe(&mut self, id: Uuid) {
    if self.windows.contains_key(&id) {
      self.dirty.insert(id);
    }
  }

  /// Resumes moves held behind a cover even if its last clock stopped.
  fn overlay_retired(&mut self, id: Uuid) {
    if self.deferred_moves.contains(&id) {
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
    // Recovery removes the whole property, flag included.
    if self.marked_focus == Some(id) {
      self.marked_focus = None;
    }
    self.intents.remove(&id);
    self.dirty.remove(&id);
    self.deferred_moves.remove(&id);
    if let Some(window) = self.windows.get_mut(&id) {
      window.frame.suspend();
      window.retired = true;
      window.recovery_at = Some(Instant::now());
      if alive {
        // Recovery removes the property without notice; clearing the flag
        // first tells companions to show themselves again.
        if matches!(window.decoration, Some(DecorationPhase::Drawing)) {
          if let Err(err) = window.native.mark_decorated(false) {
            tracing::debug!("Decoration flag clear failed: {err}");
          }
          window.decoration = None;
        }
        if let Err(err) = window.native.release() {
          tracing::warn!("Window recovery failed: {err}");
          // Native recovery retries against the retained entry, but the
          // overlay belongs to a window no longer managed. A hidden source
          // has no shell view, so its recovery can fail until the app
          // shows it again; waiting on it would leak the overlay.
          self.cleanup_overlays.insert(id);
          self.cleanup_at = Some(Instant::now());
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
  let timing = tracing::enabled!(tracing::Level::DEBUG);
  let sync_started = timing.then(Instant::now);
  let layout_changed = std::mem::take(&mut state.native_sync.layout_dirty);
  // Every commit solves the layout again. A queued redraw is not the only
  // thing that moves a rect: a dropped floating window writes its own
  // placement and asks for no redraw, and reconciling that window against
  // a snapshot from before the drag puts it back where it started.
  state.layout_snapshot = crate::layout_snapshot::LayoutSnapshot::capture(
    &state.root_container,
  )?;
  let t_snapshot = sync_started.map(|at| at.elapsed());
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
  let all_effects_changed = state.pending_sync.needs_all_effects_update();
  let focus_effects_changed =
    state.pending_sync.needs_focused_effect_update();
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
  let t_order = sync_started.map(|at| at.elapsed());
  let dirty = std::mem::take(&mut state.native_sync.dirty);
  let now = Instant::now();
  // Once per commit; see `ReconcilePass::sampled_frame`.
  let sampled_frame = wm_platform::FrameClock::current_frame()?;
  state.native_sync.cleanup(&mut state.animation_manager);
  let compositor_frame = state.native_sync.presented_frame;
  let plans = plan_animations(&windows, &redraw, state, config)?;
  if state.pending_sync.needs_focus_update() {
    state.native_sync.focus =
      focused.as_window_container().ok().map(|window| window.id());
  }
  // Every commit, not only queued focus changes: focus the user gives a
  // window directly moves the WM's focus without queueing one.
  state.native_sync.mark_focus(
    focused.as_window_container().ok().map(|window| window.id()),
  );

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
    let effects_changed = all_effects_changed
      || (focus_effects_changed
        && focus_effects_stale(window, &state.native_sync, config));
    if !redraw.contains(&id)
      && !effects_changed
      && !needs_reorder
      && !pending
      && !dirty.contains(&id)
      && !plans.contains_key(&id)
    {
      continue;
    }
    let monitor = window.monitor().context("No monitor.")?;
    let pass = ReconcilePass {
      owner: &monitor,
      now,
      compositor_frame,
      sampled_frame,
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
    // Native calls for a few windows can outlast several frames. Running
    // covers, including retargets started above, keep moving meanwhile.
    // See `AnimationManager::tick_if_due`.
    if let Err(err) =
      state.animation_manager.tick_if_due(&state.dispatcher)
    {
      tracing::debug!("Mid-commit presentation update failed: {err}");
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
  if let (Some(started), Some(snapshot), Some(order)) =
    (sync_started, t_snapshot, t_order)
  {
    let total = started.elapsed();
    tracing::debug!(
      "platform_sync {}us snapshot={}us order={}us reconcile={}us \
       windows={} settling={}",
      total.as_micros(),
      snapshot.as_micros(),
      order.saturating_sub(snapshot).as_micros(),
      total.saturating_sub(order).as_micros(),
      windows.len(),
      state
        .native_sync
        .windows
        .values()
        .filter(|entry| entry.settling())
        .count(),
    );
  }
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
  // A failed clock is replaced only once nothing still depends on it. A
  // plan made now would keep the dead clock alive and never tick.
  if state.native_sync.clock_failed
    && state.animation_manager.has_presentations()
  {
    return Ok(plans);
  }
  let candidates = windows
    .iter()
    .filter(|window| {
      redraw.contains(&window.id())
        || state.native_sync.deferred_moves.contains(&window.id())
    })
    .collect::<Vec<_>>();
  for window in candidates {
    if let Some(plan) = plan_window(window, redraw, state, config)? {
      plans.insert(window.id(), plan);
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
        }) && may_present(window, &state.native_sync)
      })
      .map(|window| (window.id(), window.native().id()))
      .collect::<Vec<_>>();
    state
      .animation_manager
      .pre_capture(&captures, &state.dispatcher)?;
  }
  Ok(plans)
}

/// Chooses one window's animation, or none.
///
/// Also records and consumes moves held behind a cover.
fn plan_window<'a>(
  window: &WindowContainer,
  redraw: &HashSet<Uuid>,
  state: &mut WmState,
  config: &'a UserConfig,
) -> anyhow::Result<Option<AnimationPlan<'a>>> {
  let id = window.id();
  // Entering and leaving fullscreen are both cuts. The current state
  // alone would animate the way out but not the way in.
  if state
    .native_sync
    .windows
    .get(&id)
    .is_some_and(|entry| entry.fullscreen == Some(true))
  {
    state.native_sync.deferred_moves.remove(&id);
    return Ok(None);
  }
  let owner = window.monitor().context("No monitor.")?;
  let monitor = owner.native_properties();
  let visible =
    window.workspace().context("No workspace.")?.is_displayed();
  let trigger = AnimationTrigger::select(
    state.pending_sync.should_skip_animations_for(owner.id())
      || window.active_drag().is_some(),
    state.pending_sync.workspace_slide_for(owner.id()),
    visible,
    if state.pending_sync.open_animation_windows().contains(window) {
      WindowChange::Opened
    } else if state.pending_sync.sent_windows().contains(window) {
      WindowChange::Sent
    } else {
      WindowChange::Placed
    },
  );
  // A sent window fades where it stands; its tile is on a workspace
  // nobody can see.
  let tile = if trigger == Some(AnimationTrigger::WindowSent) {
    window.native_properties().frame
  } else {
    state
      .layout_snapshot
      .rect(id)?
      .apply_delta(&window.border_delta(), None)
  };
  let effect = match trigger {
    // A move never switches form partway through. On Windows a move
    // reflows the real window, so one that lands while a cover is up
    // waits for the cover to finish and retire. Reconciliation holds
    // the window where the cover ends meanwhile, and the move then
    // runs natively from there.
    Some(AnimationTrigger::WindowMoved)
      if AnimationTrigger::WindowMoved.uses_native()
        && state.animation_manager.has_overlay(&id) =>
    {
      state.native_sync.deferred_moves.insert(id);
      return Ok(None);
    }
    // A slide owns its windows until it completes. Its targets differ
    // from a redraw's tiles (a leaving window's is off screen), so a
    // move now would restart the slide mid-flight. Where every move
    // scales a cover, the held move continues that cover from where
    // the slide ends.
    Some(AnimationTrigger::WindowMoved)
      if state.animation_manager.owns_slide(&id) =>
    {
      if !state.animation_manager.is_complete(&id) {
        state.native_sync.deferred_moves.insert(id);
        return Ok(None);
      }
      state.native_sync.deferred_moves.remove(&id);
      state
        .animation_manager
        .deferred_move_effect(&id, &tile, config)
    }
    _ => {
      let deferred = state.native_sync.deferred_moves.remove(&id);
      if !redraw.contains(&id) && !deferred {
        return Ok(None);
      }
      trigger.and_then(|trigger| {
        state.animation_manager.animation_effect_for_window(
          window, trigger, &tile, &monitor, config,
        )
      })
    }
  };
  Ok(effect.zip(trigger).map(|(effect, trigger)| AnimationPlan {
    effect,
    trigger,
    path: trigger.path(&tile, &monitor.bounds),
  }))
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
    match &entry.motion {
      Some(MotionOwner::Overlay(motion))
        if !blocked.contains(&motion.batch) =>
      {
        entry.motion = None;
        state.animation_manager.release_animation(id);
        tracing::debug!(window = %id, "Presentation motion ready.");
      }
      // Native motion keeps its owner while it runs; releasing it again
      // is a no-op.
      Some(MotionOwner::Native { batch }) if !blocked.contains(batch) => {
        state.animation_manager.release_animation(id);
      }
      _ => {}
    }
  }
}

/// Whether a window can take an overlay this commit, as far as is known
/// without a native query.
///
/// Reconciliation checks the same conditions, plus the observed native
/// state, before preparing an overlay. Checking them here first spares a
/// capture (a screenshot on macOS) that would only be discarded.
fn may_present(
  window: &WindowContainer,
  coordinator: &PlacementCoordinator,
) -> bool {
  let id = window.id();
  let minimize = coordinator
    .intents
    .get(&id)
    .is_some_and(|intent| intent.minimize);
  desired_state(window, minimize) == NativeState::Normal
    && coordinator.windows.get(&id).is_none_or(|entry| {
      entry.conceal.is_some() && entry.native.supports_presentation()
    })
}

/// Carries native-independent constants for one reconciliation pass.
#[derive(Clone, Copy)]
struct ReconcilePass<'a> {
  owner: &'a Monitor,
  now: Instant,
  compositor_frame: u64,
  /// The compositor's serial, sampled once for the whole commit.
  ///
  /// Reading it is a round-trip to the compositor, and the answer is a
  /// global counter rather than anything about one window, so sampling it
  /// per window paid that round-trip once per settling window per frame
  /// for the same number.
  sampled_frame: u64,
  reorder: bool,
  hide_corner: HideCorner,
}

/// Records where one window's reconciliation spends its time.
///
/// Inert unless debug logging is enabled. A pass that takes longer than
/// `REPORT_AFTER` logs every span, so a slow commit names its native
/// calls.
struct PassTimer {
  started: Option<Instant>,
  last: Duration,
  spans: Vec<(&'static str, Duration)>,
}

impl PassTimer {
  /// A pass shorter than this is not reported.
  const REPORT_AFTER: Duration = Duration::from_millis(1);

  /// Starts timing when debug logging is enabled.
  fn start() -> Self {
    Self {
      started: tracing::enabled!(tracing::Level::DEBUG).then(Instant::now),
      last: Duration::ZERO,
      spans: Vec::new(),
    }
  }

  /// Ends the span since the previous mark, naming it `label`.
  fn mark(&mut self, label: &'static str) {
    if let Some(started) = self.started {
      let elapsed = started.elapsed();
      self.spans.push((label, elapsed.saturating_sub(self.last)));
      self.last = elapsed;
    }
  }

  /// Logs the spans of a slow pass.
  fn report(&self, id: &Uuid) {
    if self.last < Self::REPORT_AFTER {
      return;
    }
    let spans = self
      .spans
      .iter()
      .filter(|(_, span)| !span.is_zero())
      .map(|(label, span)| format!("{label}={}us", span.as_micros()))
      .collect::<Vec<_>>()
      .join(" ");
    tracing::debug!(
      window = %id,
      "reconcile_slow total={}us {spans}",
      self.last.as_micros()
    );
  }
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
  parked: bool,
) -> anyhow::Result<DesiredFrame> {
  Ok(DesiredFrame {
    rect: target.clone(),
    monitor: monitor.bounds.clone(),
    dpi: native.expected_dpi(monitor.dpi)?,
    state,
    parking_clamp: parked.then_some(PlacementSession::PARKING_CLAMP),
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
///
/// `placed` runs straight after a frame request is issued, with the
/// requested rect, so work that must reach the compositor with the move
/// follows it without a native query in between.
fn reconcile_frame(
  entry: &mut ManagedWindow,
  now: Instant,
  mut placed: impl FnMut(&Rect),
) -> anyhow::Result<ObservedFrame> {
  let observed = observe(&entry.native)?;
  if let Some(request) = entry.frame.next(&observed, now) {
    entry.frame.apply(&request, now, |mutation| {
      match mutation {
        NativeMutation::Frame(rect) => {
          entry.native.set_frame(rect)?;
          placed(rect);
        }
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
  let timing = tracing::enabled!(tracing::Level::DEBUG);
  let started = timing.then(Instant::now);
  let observed = entry.native.is_cloaked()?;
  let t_read = started.map(|at| at.elapsed());
  if observed != cloaked {
    entry.native.cloak(cloaked)?;
  }
  if let (Some(at), Some(read)) = (started, t_read) {
    let total = at.elapsed();
    if total >= Duration::from_micros(500) {
      tracing::debug!(
        "cloak_slow total={}us read={}us write={}us changed={}",
        total.as_micros(),
        read.as_micros(),
        total.saturating_sub(read).as_micros(),
        observed != cloaked,
      );
    }
  }
  Ok(())
}

/// Decorates a window whose native motion was just prepared.
///
/// Draws the window's companions from a companion overlay and publishes
/// `DECORATED` once they are drawn. A retarget keeps a running decoration,
/// and resumes one still held for its reveal. Without companions nothing
/// is published.
fn decorate_native(
  entry: &mut ManagedWindow,
  window: &WindowContainer,
  start: &Rect,
  frame_rate: u32,
  state: &mut WmState,
) {
  let id = window.id();
  if !state.animation_manager.decorate_native(
    id,
    &window.native(),
    start,
    frame_rate,
    &state.dispatcher,
  ) {
    return;
  }
  if matches!(entry.decoration, Some(DecorationPhase::Drawing)) {
    return;
  }
  match entry.native.mark_decorated(true) {
    Ok(()) => entry.decoration = Some(DecorationPhase::Drawing),
    // Unflagged companions keep drawing themselves; drawing them here too
    // would show two.
    Err(err) => {
      tracing::warn!(window = %id, "Decoration flag failed: {err}");
      entry.decoration = None;
      if let Err(err) = state.animation_manager.retire_decoration(&id) {
        tracing::warn!("Companion overlay cleanup failed: {err}");
      }
    }
  }
}

/// Ends a window's decoration once its native motion has ended.
///
/// Runs after the pass's frame request, so the companions are already
/// drawn at the landing frame when `DECORATED` clears. The companion
/// overlay is then held like a retiring cover, until the companions show
/// themselves again. Motion handed to a cover retires it at once, because
/// the cover draws the companions itself.
///
/// Returns when a held decoration should be checked again.
fn settle_decoration(
  entry: &mut ManagedWindow,
  id: &Uuid,
  state: &mut WmState,
  compositor_frame: u64,
  sampled_frame: u64,
  now: Instant,
) -> anyhow::Result<Option<Instant>> {
  let decorating = state.animation_manager.has_decoration(id);
  let native = matches!(entry.motion, Some(MotionOwner::Native { .. }));
  if matches!(entry.decoration, Some(DecorationPhase::Drawing))
    && !(native && decorating)
  {
    if let Err(err) = entry.native.mark_decorated(false) {
      tracing::debug!(window = %id, "Decoration flag clear failed: {err}");
    }
    entry.decoration = decorating.then(|| {
      DecorationPhase::Revealing(RetireFence::new(sampled_frame, now))
    });
  }
  let Some(DecorationPhase::Revealing(fence)) = entry.decoration.as_mut()
  else {
    return Ok(None);
  };
  if !decorating
    || matches!(entry.motion, Some(MotionOwner::Overlay(_)))
    || state.native_sync.clock_failed
    || fence.due(compositor_frame, now, || {
      state.animation_manager.decoration_revealed(id)
    })
  {
    entry.decoration = None;
    state.animation_manager.retire_decoration(id)?;
    return Ok(None);
  }
  Ok(fence.recheck_at(compositor_frame, now))
}

fn begin_handoff(
  entry: &mut ManagedWindow,
  id: &Uuid,
  state: &mut WmState,
  sampled_frame: u64,
  now: Instant,
) -> anyhow::Result<()> {
  if matches!(entry.motion, Some(MotionOwner::Native { .. })) {
    entry.frame.restart();
  }
  entry.motion = None;
  entry.cancelled = false;
  state.animation_manager.finish_animation(id)?;
  if let Some(source) = &mut entry.source {
    source.restoring = true;
  } else if state.animation_manager.has_overlay(id) {
    entry
      .retire_after
      .get_or_insert(RetireFence::new(sampled_frame, now));
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
      match desired_frame(&native, &target, &monitor, native_state, false)
      {
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
      decoration: None,
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
    sampled_frame,
    reorder,
    hide_corner,
  } = pass;
  let id = window.id();
  let monitor = owner.native_properties();
  let mut timer = PassTimer::start();
  entry.native.validate()?;
  entry.retry_at = None;
  let mut observed = observe(&entry.native)?;
  timer.mark("observe");
  // A cancellation raised before this commit (invalidation, a lost clock,
  // a failed launch) ends the motion it was raised against. The plan this
  // commit prepares is newer and must survive it.
  if entry.cancelled {
    begin_handoff(entry, &id, state, sampled_frame, now)?;
  }
  timer.mark("cancel");
  if let Some(plan) = plan {
    if plan.trigger.uses_native()
      && visible
      && entry.source.is_none()
      && !state.animation_manager.has_overlay(&id)
      && native_state == NativeState::Normal
      && observed.state == NativeState::Normal
      && !dragging
    {
      let start = entry.native.window()?.frame()?;
      let frame_rate = monitor.refresh_rate.unwrap_or(60);
      state.animation_manager.prepare_native(
        id,
        plan,
        start.clone(),
        frame_rate,
      );
      entry.motion = Some(MotionOwner::Native {
        batch: state.native_sync.batch,
      });
      entry.retire_after = None;
      decorate_native(entry, window, &start, frame_rate, state);
    } else if entry.native.supports_presentation()
      && entry.conceal.is_some()
      && native_state == NativeState::Normal
      && observed.state == NativeState::Normal
      && !dragging
    {
      let opening = plan.trigger == AnimationTrigger::WindowOpened;
      // Only content already on screen needs a presented cover before its
      // source hides. A new window starts invisible, and an entering
      // workspace is still hidden.
      let requires_cover = !opening
        && !matches!(plan.trigger, AnimationTrigger::WorkspaceEntering(_));
      // A retained overlay is already presented. Retargeting it needs no
      // new overlay frame, and the source stays covered throughout.
      let covered = state.animation_manager.has_overlay(&id);
      // A source still concealed behind its cover has nothing to wait for,
      // so a retarget continues the motion at once. One being restored may
      // already show; its cover holds until it is concealed again.
      let start = if entry
        .source
        .as_ref()
        .is_some_and(|source| !source.restoring)
      {
        MotionStart::Immediate
      } else {
        MotionStart::Held
      };
      let mut prepare = || -> anyhow::Result<MotionStart> {
        // Windows thumbnails stay live while their source is concealed.
        // Hide an opening source before allocating its overlay; a warm
        // application can already have painted its complete window.
        // Parking backends must capture before moving the source.
        if opening && !entry.native.uses_parking() {
          entry
            .source
            .get_or_insert_with(SourceLease::default)
            .restoring = false;
          // The lease taken here skips the generic `present(true)` below,
          // which only fires for a source not yet leased. Border renderers
          // would otherwise ring the destination while the overlay is
          // still opening; the handoff clears it with every
          // other lease.
          entry.native.present(true)?;
          if entry.conceal == Some(ConcealMethod::Cloak) {
            apply_cloak(entry, true)?;
          } else {
            let alpha = Some(OpacityValue(0.0));
            if entry.opacity != AppliedOpacity::Value(alpha) {
              entry.native.opacity(alpha)?;
              entry.opacity = AppliedOpacity::Value(alpha);
            }
          }
        }
        state.animation_manager.prepare_animation(
          window,
          plan,
          &monitor,
          &state.dispatcher,
          start,
        )
      };
      let preparing = Instant::now();
      let prepared = prepare();
      let prepare_us = preparing.elapsed().as_micros();
      match prepared {
        Ok(start) => {
          // An immediate motion is already running. It has no readiness,
          // so it neither waits for its batch nor holds the batch back.
          entry.motion = (start == MotionStart::Held).then(|| {
            MotionOwner::Overlay(MotionPreparation::new(
              state.native_sync.batch,
              (!covered).then_some(sampled_frame),
              now,
              requires_cover,
            ))
          });
          entry.retire_after = None;
          if let Some(source) = &mut entry.source {
            source.restoring = false;
          }
          tracing::debug!(window = %id, trigger = ?plan.trigger, covered, ?start, prepare_us, "Presentation overlay prepared.");
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
    timer.mark("prepare");
  }
  let native_motion =
    matches!(entry.motion, Some(MotionOwner::Native { .. }));
  let visible_target = state
    .layout_snapshot
    .rect(id)?
    .apply_delta(&window.border_delta(), None);
  if (native_motion
    && (!visible
      || observed.state != NativeState::Normal
      || state.pending_sync.should_skip_animations_for(owner.id())
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
    begin_handoff(entry, &id, state, sampled_frame, now)?;
  }
  timer.mark("handoff");
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
  timer.mark("lease");
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
  // Visible geometry the real window follows instead of its tile: its own
  // native motion, a cover that holds it in place, or, for a move held
  // behind a cover, where that cover ends. Holding it there lets the
  // cover restore without a jump and the held move start where the cover
  // left off.
  let native_frame = state
    .animation_manager
    .native_frame(&id)
    .or_else(|| state.animation_manager.held_frame(&id))
    .or_else(|| {
      state
        .native_sync
        .deferred_moves
        .contains(&id)
        .then(|| state.animation_manager.cover_target(&id))
        .flatten()
    });
  let (operational_target, parking) = if retaining_source {
    (observed.rect.clone(), false)
  } else if parked {
    let corner =
      parking_rect(&observed.rect, &monitor.working_area, hide_corner);
    entry.native.remember_restore(target)?;
    (corner, true)
  } else if let Some(frame) = native_frame {
    #[cfg(target_os = "windows")]
    let frame =
      frame.apply_delta(&window.native_properties().shadow_borders, None);
    (frame, false)
  } else {
    (target.clone(), false)
  };
  let desired = desired_frame(
    &entry.native,
    &operational_target,
    &monitor,
    native_state,
    parking,
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
  timer.mark("desired");

  let intent = state.native_sync.intents.get(&id);
  let requested_alpha = requested_alpha(window, intent, config);
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
  timer.mark("opacity");
  let decorations = desired_decorations(window, intent, config);
  if entry.decorations.as_ref() != Some(&decorations) {
    entry
      .native
      .set_decorations(decorations.border.as_ref(), &decorations.corner)?;
    entry.native.title_bar(decorations.title_bar)?;
    entry.decorations = Some(decorations);
  }
  timer.mark("decorations");
  // Conceal before any placement write. Cloaked sources need the same
  // ordering as alpha sources or the destination can flash on screen.
  let native_visible = visible || presenting;
  let cloak_for_source =
    concealed && entry.conceal == Some(ConcealMethod::Cloak);
  if !retaining_source
    && native_state != NativeState::Minimized
    && cloak_for_source
  {
    // A failure cancels only this presentation. The shell call fails
    // transiently (a stale view collection, a stalled shell), and dropping
    // the capability would cut every later slide for the window's life.
    if let Err(err) = apply_cloak(entry, true) {
      tracing::warn!(window = %id, "Source cloaking failed: {err}");
      entry.cancelled = true;
    }
  }
  timer.mark("conceal");
  if !retaining_source && !dragging {
    let animations = &mut state.animation_manager;
    observed = reconcile_frame(entry, now, |rect| {
      animations.draw_decoration(&id, rect);
    })?;
  } else if retaining_source {
    entry.frame.observe(&observed, now);
  }
  timer.mark("frame");
  let converged = entry.frame.phase == ReconcilePhase::Converged
    && entry.frame.converged(&observed);
  if converged && !parked {
    entry.native.unpark()?;
    window.set_has_pending_dpi_adjustment(false);
  }
  timer.mark("unpark");
  if entry.frame.phase == ReconcilePhase::Failed {
    begin_handoff(entry, &id, state, sampled_frame, now)?;
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
    // Visibility is read again only after a write, which is rare; an
    // unchanged window costs one query per pass.
    let mut shown = entry.native.is_visible()?;
    let was_visible =
      observed_visible(shown, cloak_for_source, native_visible);
    if native_visible && !was_visible {
      entry.native.show(true)?;
      shown = entry.native.is_visible()?;
    } else if !native_visible
      && !hidden_parking
      && was_visible
      && *hide_method == HideMethod::Hide
    {
      entry.native.show(false)?;
      shown = entry.native.is_visible()?;
    }
    entry.visibility =
      Some((native_visible && !hidden_parking, hide_method.clone()));
    let actual_visible =
      observed_visible(shown, cloak_for_source, native_visible);
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
  timer.mark("visibility");
  if let Some(MotionOwner::Overlay(motion)) = &mut entry.motion {
    motion.observe(
      entry.frame.generation,
      suppressing && converged && !entry.visibility_pending,
      sampled_frame,
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
    && crate::native_reconciler::parked_frames_match(
      &parking_rect(&observed.rect, &monitor.working_area, hide_corner),
      &observed.rect,
      PlacementSession::PARKING_CLAMP,
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
    entry.retire_after = state
      .animation_manager
      .has_overlay(&id)
      .then(|| RetireFence::new(sampled_frame, now));
    tracing::debug!(window = %id, "Presentation source restored.");
  }
  timer.mark("restore");
  // Companions reveal themselves only after the source does; the overlay
  // keeps drawing them until then. See `COMPANION_HANDOVER`.
  let mut companion_recheck = settle_decoration(
    entry,
    &id,
    state,
    compositor_frame,
    sampled_frame,
    now,
  )?;
  if let Some(fence) = entry.retire_after.as_mut() {
    if state.native_sync.clock_failed
      || fence.due(compositor_frame, now, || {
        state.animation_manager.companions_revealed(&id)
      })
    {
      state.animation_manager.retire_overlay(&id)?;
      entry.retire_after = None;
      state.native_sync.overlay_retired(id);
    } else if let Some(at) = fence.recheck_at(compositor_frame, now) {
      companion_recheck = Some(
        companion_recheck.map_or(at, |recheck: Instant| recheck.min(at)),
      );
    }
  }
  if entry.visibility_pending || (restoring && entry.source.is_some()) {
    entry.retry_at = Some(now + Duration::from_millis(250));
  }
  // The frame clock can stop once nothing changes on screen; the recovery
  // deadline ends the hold regardless.
  if let Some(at) = companion_recheck {
    entry.retry_at =
      Some(entry.retry_at.map_or(at, |retry| retry.min(at)));
  }
  timer.mark("retire");
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
  timer.mark("z_order");
  // Taskbar membership is a round trip to the shell, measured at 1-8ms,
  // and blocks every running cover until it returns. It waits while the
  // window is presented, so it lands with the reveal, and a switch that
  // reverses mid-slide makes no call at all. The pass that restores the
  // source clears both conditions, so the wait always ends.
  let taskbar = visible || config.value.general.show_all_in_taskbar;
  let presented = entry.source.is_some()
    || retaining_source
    || state.animation_manager.is_running(&id);
  if entry.taskbar != Some(taskbar) && !presented {
    entry.native.set_taskbar_visibility(taskbar)?;
    entry.taskbar = Some(taskbar);
  }
  timer.mark("taskbar");
  let fullscreen = matches!(window.state(), WindowState::Fullscreen(_));
  if entry.fullscreen != Some(fullscreen) {
    entry.native.mark_fullscreen(fullscreen)?;
    entry.fullscreen = Some(fullscreen);
  }
  timer.mark("fullscreen");
  timer.report(&id);
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn retiring_cover_schedules_deferred_move_without_frame_clock() {
    let mut coordinator = PlacementCoordinator::default();
    let id = Uuid::new_v4();
    coordinator.deferred_moves.insert(id);
    assert!(coordinator.deadline(false).is_none());

    coordinator.overlay_retired(id);

    assert!(coordinator.has_pending());
    assert!(coordinator.deadline(false).unwrap() <= Instant::now());
    assert!(coordinator.deadline(true).is_none());
    assert!(coordinator.deferred_moves.contains(&id));
  }

  /// Focus on a window without a session waits instead of being recorded
  /// as published.
  #[test]
  fn focus_flag_waits_for_a_session() {
    let mut coordinator = PlacementCoordinator::default();
    coordinator.mark_focus(Some(Uuid::new_v4()));
    assert_eq!(coordinator.marked_focus, None);

    // Focus leaving every window clears the record.
    let id = Uuid::new_v4();
    coordinator.marked_focus = Some(id);
    coordinator.mark_focus(None);
    assert_eq!(coordinator.marked_focus, None);
  }

  #[test]
  fn retiring_cover_without_deferred_move_needs_no_followup() {
    let mut coordinator = PlacementCoordinator::default();
    coordinator.overlay_retired(Uuid::new_v4());
    assert!(!coordinator.has_pending());
    assert!(coordinator.deadline(false).is_none());
  }

  /// Native ownership never requests source concealment.
  #[test]
  fn native_excludes_cover_readiness() {
    assert!(MotionOwner::Native { batch: 1 }.preparation().is_none());
    let owner = MotionOwner::Overlay(MotionPreparation::new(
      1,
      Some(10),
      Instant::now(),
      true,
    ));
    let preparation = owner.preparation().expect("Overlay preparation.");
    assert!(preparation.retains_source(10));
    assert!(!preparation.retains_source(11));
  }

  /// The fence waits for composition, then for companions, then gives
  /// up on them at the cap.
  #[test]
  fn retire_fence_holds_for_companions_within_cap() {
    let since = Instant::now();
    let mut fence = RetireFence::new(10, since);
    assert!(!fence.due(10, since, || true));
    assert!(fence.recheck_at(10, since).is_none());
    assert!(!fence.due(11, since, || false));
    assert_eq!(fence.recheck_at(11, since), Some(since + COMPANION_POLL));
    // Seeing the companions uncloaked is not enough: their uncloak is
    // drawn only in a frame composed after it.
    assert!(!fence.due(11, since, || true));
    assert!(!fence.due(11, since, || panic!("reveal already recorded")));
    assert!(fence.due(12, since, || false));

    let mut fence = RetireFence::new(10, since);
    let late =
      since + COMPANION_HANDOVER.saturating_sub(Duration::from_millis(1));
    assert_eq!(
      fence.recheck_at(11, late),
      Some(since + COMPANION_HANDOVER)
    );
    assert!(fence.due(11, since + COMPANION_HANDOVER, || false));
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
