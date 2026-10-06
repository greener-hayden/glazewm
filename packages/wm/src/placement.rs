use std::{
  collections::{HashMap, HashSet},
  time::{Duration, Instant},
};

use anyhow::Context;
use uuid::Uuid;
use wm_common::{
  CursorJumpTrigger, DisplayState, HideCorner, HideMethod,
  WindowEffectConfig, WindowState, WmEvent,
};
use wm_platform::{
  Color, ConcealMethod, CornerStyle, Delta, NativeCallSnapshot,
  NativeCallStats, OpacityValue, PlacementSession, Rect, WindowId,
  WindowZOrder,
};

use crate::{
  animation_manager::{
    AnimationPlan, AnimationTrigger, MotionStart, WindowChange,
  },
  models::{Monitor, NativeMonitorProperties, WindowContainer},
  native_reconciler::{
    DesiredFrame, FrameReconciler, NativeMutation, NativeState,
    ObservePlan, ObservedFrame, ReconcilePhase,
  },
  perf::{self, PerfSpan, SyncDetail, SyncOrigin, SLOW_CALL, SLOW_SPAN},
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

  frame_decorations(&window.state(), effects)
}

/// Resolves the border and corners a window state allows.
///
/// A fullscreen window owns every pixel of its monitor. The compositor
/// clips the corners of a rounded window and rings it with a translucent
/// edge, and both show the desktop behind a window that fills the screen.
fn frame_decorations(
  state: &WindowState,
  effects: &WindowEffectConfig,
) -> (Option<Color>, CornerStyle) {
  if matches!(state, WindowState::Fullscreen(_)) {
    return (None, CornerStyle::Square);
  }

  (
    effects.border.enabled.then(|| effects.border.color.clone()),
    if effects.corner_style.enabled {
      effects.corner_style.style.clone()
    } else {
      CornerStyle::Default
    },
  )
}

/// Resolves the opacity a window state allows, before any concealment.
///
/// A fullscreen window takes no opacity from the focus effects. An
/// explicit command still applies.
fn state_alpha(
  state: &WindowState,
  commanded: Option<OpacityValue>,
  effects: &WindowEffectConfig,
) -> Option<OpacityValue> {
  commanded.or_else(|| {
    (effects.transparency.enabled
      && !matches!(state, WindowState::Fullscreen(_)))
    .then_some(effects.transparency.opacity)
  })
}

/// Resolves the stacking the WM holds a window to.
///
/// A fullscreen window covers its workspace only while it leads it, which
/// is while it was the last window focused there. Otherwise it sits behind
/// the workspace so the window that leads is visible.
///
/// Returns `None` to leave the stacking to the window. A fullscreen window
/// that leads its workspace may keep an always-on-top flag it set itself.
fn desired_stacking(
  state: &WindowState,
  leads_workspace: bool,
) -> Option<WindowZOrder> {
  match state {
    WindowState::Floating(floating) if floating.shown_on_top => {
      Some(WindowZOrder::TopMost)
    }
    WindowState::Fullscreen(_) if !leads_workspace => {
      Some(WindowZOrder::Bottom)
    }
    WindowState::Fullscreen(fullscreen) if fullscreen.shown_on_top => {
      Some(WindowZOrder::TopMost)
    }
    WindowState::Fullscreen(_) => None,
    _ => Some(WindowZOrder::Normal),
  }
}

/// Whether the window was the last one focused in its workspace.
fn leads_workspace(window: &WindowContainer) -> bool {
  window
    .workspace()
    .and_then(|workspace| {
      workspace
        .descendant_focus_order()
        .find_map(|container| container.as_window_container().ok())
    })
    .is_some_and(|lead| lead.id() == window.id())
}

/// Gets the native ids of the other windows shown in the window's
/// workspace.
fn workspace_peers(window: &WindowContainer) -> Vec<WindowId> {
  let id = window.id();
  window
    .workspace()
    .map(|workspace| {
      workspace
        .descendants()
        .filter_map(|container| container.as_window_container().ok())
        .filter(|peer| {
          peer.id() != id && peer.state() != WindowState::Minimized
        })
        .map(|peer| {
          let native_id = peer.native().id();
          native_id
        })
        .collect()
    })
    .unwrap_or_default()
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
  state_alpha(
    &window.state(),
    intent.and_then(|intent| intent.opacity),
    effects,
  )
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

/// What one window contributes to choosing the windows a sync touches.
#[derive(Clone, Copy, Debug)]
#[allow(clippy::struct_excessive_bools)]
struct ReconcileCandidate {
  id: Uuid,
  workspace: Uuid,
  /// Released windows wait for native recovery and are never reconciled.
  retired: bool,
  /// The layout changed this window's geometry or state.
  redraw: bool,
  /// A transaction, presentation or visibility change is still settling.
  pending: bool,
  /// A native event or command marked the window for another pass.
  dirty: bool,
  /// An animation starts or continues this window's motion.
  planned: bool,
  /// Native focus is still waiting to be applied to this window.
  ///
  /// Applying it needs a settled frame, so the window has to be
  /// reconciled even when nothing else asks for it. A frame suspended by
  /// a finished drag, for one, only restarts in a pass.
  focus_pending: bool,
  /// The applied effects differ from what the new focus asks for.
  ///
  /// Only read when `ReconcileSignals::focus_effects_changed` is set.
  effects_stale: bool,
}

/// What the sync as a whole asks for, apart from any one window.
struct ReconcileSignals<'a> {
  /// Workspaces whose windows need restacking.
  reorder_workspaces: &'a HashSet<Uuid>,
  /// Every window's effects are stale, such as after a config reload.
  all_effects_changed: bool,
  /// Focus moved, so only the windows whose effects it changes are stale.
  focus_effects_changed: bool,
}

/// Gathers each window's facts for `select_reconcile_set`.
///
/// Reads only WM state. `effects_stale` is compared only when focus is
/// the sole reason a window could be stale.
#[allow(clippy::too_many_arguments)]
fn reconcile_candidates(
  windows: &[WindowContainer],
  redraw: &HashSet<Uuid>,
  dirty: &HashSet<Uuid>,
  plans: &HashMap<Uuid, AnimationPlan<'_>>,
  all_effects_changed: bool,
  focus_effects_changed: bool,
  state: &WmState,
  config: &UserConfig,
) -> anyhow::Result<Vec<ReconcileCandidate>> {
  windows
    .iter()
    .map(|window| {
      let id = window.id();
      let entry = state.native_sync.windows.get(&id);
      Ok(ReconcileCandidate {
        id,
        workspace: window.workspace().context("No workspace.")?.id(),
        retired: entry.is_some_and(|entry| entry.retired),
        redraw: redraw.contains(&id),
        pending: entry.is_some_and(|entry| {
          entry.settling() || entry.visibility_pending
        }),
        dirty: dirty.contains(&id),
        planned: plans.contains_key(&id),
        focus_pending: state.native_sync.focus == Some(id),
        effects_stale: focus_effects_changed
          && !all_effects_changed
          && focus_effects_stale(window, &state.native_sync, config),
      })
    })
    .collect()
}

/// Chooses the windows a sync reconciles.
///
/// Returns each chosen window's id, mapped to whether it is restacked.
/// Reconciling a window costs native calls, so a focus change must not
/// reach every window on its workspace when only restacking asks for it.
/// Platforms without native z-order have nothing to restack. A focus
/// change there reaches only the window gaining focus, which native focus
/// waits on, and the windows whose effects it changes.
fn select_reconcile_set(
  candidates: &[ReconcileCandidate],
  signals: &ReconcileSignals<'_>,
  has_native_z_order: bool,
) -> HashMap<Uuid, bool> {
  candidates
    .iter()
    .filter(|candidate| !candidate.retired)
    .filter_map(|candidate| {
      let reorder = has_native_z_order
        && signals.reorder_workspaces.contains(&candidate.workspace);
      // Restacking reconciled the focus target along with its workspace.
      let focus_target = !has_native_z_order && candidate.focus_pending;
      let effects_changed = signals.all_effects_changed
        || (signals.focus_effects_changed && candidate.effects_stale);

      (candidate.redraw
        || effects_changed
        || reorder
        || focus_target
        || candidate.pending
        || candidate.dirty
        || candidate.planned)
        .then_some((candidate.id, reorder))
    })
    .collect()
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

  /// The lowest compositor frame this fence still waits to pass, among
  /// those at or above `from`.
  ///
  /// The fence waits for a frame after `after`, then after the frame that
  /// saw the companions revealed. Between the two, and past the hold, it
  /// is rechecked by `recheck_at` instead.
  fn frame_gate(&self, from: u64) -> Option<u64> {
    [Some(self.after), self.revealed_at]
      .into_iter()
      .flatten()
      .filter(|gate| *gate >= from)
      .min()
  }

  /// When a fence that is not yet due should be checked again, without
  /// relying on compositor ticks.
  ///
  /// `None` while the source awaits composition; that wait is a frame
  /// gate (see `frame_gate`), which `PlacementCoordinator::tick_due`
  /// watches.
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
  ///
  /// An in-flight frame counts: a frame write is confirmed by a later
  /// pass, which this keeps scheduled for a window whose echo is not
  /// marked (see `reconcile_frame`).
  fn settling(&self) -> bool {
    self.settling_view(false).settling()
  }

  /// Borrows what keeps the window settling.
  ///
  /// `running` is whether the animation manager owns released motion for
  /// the window.
  fn settling_view(&self, running: bool) -> SettlingView<'_> {
    SettlingView {
      frame: &self.frame,
      motion: self.motion.as_ref(),
      source: self.source.as_ref(),
      retire_after: self.retire_after.as_ref(),
      decoration: self.decoration.as_ref(),
      visibility_pending: self.visibility_pending,
      cancelled: self.cancelled,
      running,
      retry_at: self.retry_at,
    }
  }
}

/// Everything that keeps one window settling, apart from its native
/// session.
///
/// Holding no session, it can be built in a test, so what wakes each
/// settling state is checked without a native window.
#[derive(Clone, Copy)]
struct SettlingView<'a> {
  frame: &'a FrameReconciler,
  motion: Option<&'a MotionOwner>,
  source: Option<&'a SourceLease>,
  retire_after: Option<&'a RetireFence>,
  decoration: Option<&'a DecorationPhase>,
  visibility_pending: bool,
  cancelled: bool,
  /// The animation manager owns released motion for the window.
  running: bool,
  /// When the pass that last ran asked to see the window again.
  retry_at: Option<Instant>,
}

/// What brings a settling window back for its next pass.
///
/// A window that falls asleep on none of these is stranded until some
/// unrelated sync. See `SettlingView::wake`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Wake {
  /// Nothing else is guaranteed to bring the window back, so the next
  /// tick must.
  immediate: bool,
  /// The lowest compositor frame the window waits to pass.
  gate: Option<u64>,
  /// Released motion that ends by setting its completion flag.
  completion: bool,
  /// A time deadline brings the window back, with no tick needed.
  deadline: bool,
}

impl Wake {
  /// Whether the window is certain to be reconciled again.
  #[cfg(test)]
  fn covered(self) -> bool {
    self.immediate
      || self.gate.is_some()
      || self.completion
      || self.deadline
  }
}

impl SettlingView<'_> {
  /// Whether reconciliation still owes the window work.
  fn settling(&self) -> bool {
    self.motion.is_some()
      || self.source.is_some()
      || self.retire_after.is_some()
      || self.decoration.is_some()
      || self.frame.in_flight()
  }

  /// Finds what ends the window's wait, given that its last pass saw
  /// compositor frame `synced`.
  ///
  /// Every settling state must report at least one source (`covered`).
  /// A state that is not a known wait reports `immediate`, which costs a
  /// pass per tick but never a stranded window.
  fn wake(&self, synced: u64) -> Wake {
    let preparation = self.motion.and_then(MotionOwner::preparation);
    let fence = self.retire_after;
    let revealing = match self.decoration {
      Some(DecorationPhase::Revealing(fence)) => Some(fence),
      _ => None,
    };
    let gate = [
      preparation.and_then(|motion| motion.frame_gate(synced)),
      fence.and_then(|fence| fence.frame_gate(synced)),
      revealing.and_then(|fence| fence.frame_gate(synced)),
    ]
    .into_iter()
    .flatten()
    .min();
    // Motion that only its completion ends. A source concealed behind a
    // cover with no preparation left is the cover's running motion, and
    // a decoration being drawn is a native motion's.
    let awaits_completion =
      matches!(self.motion, Some(MotionOwner::Native { .. }))
        || matches!(self.decoration, Some(DecorationPhase::Drawing))
        || (preparation.is_none()
          && self.source.is_some_and(|source| !source.restoring));
    Wake {
      // A write or a visibility change is confirmed only by a later
      // pass, and a window whose clock drives it queues no echo for it.
      immediate: self.cancelled
        || self.frame.in_flight()
        || self.visibility_pending
        || (awaits_completion && !self.running),
      gate,
      completion: self.running,
      deadline: preparation.is_some()
        || self.frame.deadline().is_some()
        || self.retry_at.is_some(),
    }
  }
}

/// Whether a frame tick has anything for a pass to do.
///
/// Where the platform animates for itself, the tick carries no motion, so
/// a pass is due only when a wait can have ended. A pass at `synced` left
/// every frame gate below it passed, so the tick is due once `presented`
/// is beyond the lowest gate not passed then. Time deadlines and queued
/// events start passes of their own.
///
/// `completed` is whether any motion has set its completion flag. It is
/// read rather than inferred from a `Wake` signal, which a full tick
/// channel can drop.
fn tick_due<'a>(
  presented: u64,
  synced: u64,
  clock_failed: bool,
  completed: bool,
  windows: impl IntoIterator<Item = SettlingView<'a>>,
) -> bool {
  if clock_failed || completed {
    return true;
  }
  let mut lowest_gate = None;
  for window in windows.into_iter().filter(SettlingView::settling) {
    let wake = window.wake(synced);
    if wake.immediate {
      return true;
    }
    lowest_gate = lowest_gate.into_iter().chain(wake.gate).min();
  }
  lowest_gate.is_some_and(|gate| presented > gate)
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
  /// The compositor frame the last completed sync read, which every frame
  /// gate below it had already been passed by.
  synced_frame: u64,
  clock_failed: bool,
}

impl PlacementCoordinator {
  /// Records a real compositor boundary, independent of any motion clock.
  pub fn presented(&mut self, frame: u64) {
    self.presented_frame = self.presented_frame.max(frame);
  }

  /// Returns the last compositor boundary recorded by `presented`.
  pub fn presented_frame(&self) -> u64 {
    self.presented_frame
  }

  /// Whether a tick at `presented_frame` can change what a sync does.
  ///
  /// For platforms that animate for themselves, where a tick carries no
  /// motion. Otherwise every tick syncs, because it draws the motion.
  /// Due when any of these holds:
  /// - A running motion set its completion flag.
  /// - The frame passed the lowest frame gate left open by the last sync:
  ///   overlay readiness, a retire fence or a companion hold.
  /// - A settling window's frame write or visibility change is
  ///   unconfirmed, or its presentation was cancelled. Their echoes queue
  ///   no sync of their own for a window the clock drives, so the tick
  ///   confirms them. A window that is not settling never makes a tick
  ///   due: a hidden application cannot change state on a tick, and its
  ///   retry deadline polls it.
  /// - The clock failed.
  ///
  /// Anything else that settles waits on a deadline, which starts its
  /// own sync.
  pub fn tick_due(
    &self,
    presented_frame: u64,
    animations: &crate::animation_manager::AnimationManager,
  ) -> bool {
    tick_due(
      presented_frame,
      self.synced_frame,
      self.clock_failed,
      animations.has_completed_motion(),
      self
        .windows
        .iter()
        .filter(|(_, window)| !window.retired)
        .map(|(id, window)| {
          window.settling_view(animations.is_running(id))
        }),
    )
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
  origin: SyncOrigin,
) -> anyhow::Result<()> {
  let span = PerfSpan::start("platform_sync");
  let key_queued = origin.key_received_at.map(|at| at.elapsed());
  // A tiling size that cannot be laid out would collapse its tile, so a
  // corrupt one is repaired before anything reads the layout.
  let repaired = crate::commands::container::repair_tiling_sizes(
    &state.root_container.clone().into(),
  );
  let layout_changed =
    std::mem::take(&mut state.native_sync.layout_dirty) || repaired;
  // Every commit solves the layout again. A queued redraw is not the only
  // thing that moves a rect: a dropped floating window writes its own
  // placement and asks for no redraw, and reconciling that window against
  // a snapshot from before the drag puts it back where it started.
  state.layout_snapshot = crate::layout_snapshot::LayoutSnapshot::capture(
    &state.root_container,
  )?;
  let t_snapshot = span.elapsed();
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
  let mut reorder_workspaces = state
    .pending_sync
    .workspaces_to_reorder()
    .iter()
    .map(CommonGetters::id)
    .collect::<HashSet<_>>();
  if state.pending_sync.needs_focus_update() {
    if let Some(workspace) = focused.workspace() {
      reorder_workspaces.insert(workspace.id());
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
  let t_order = span.elapsed();
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

  let selected = select_reconcile_set(
    &reconcile_candidates(
      &windows,
      &redraw,
      &dirty,
      &plans,
      all_effects_changed,
      focus_effects_changed,
      state,
      config,
    )?,
    &ReconcileSignals {
      reorder_workspaces: &reorder_workspaces,
      all_effects_changed,
      focus_effects_changed,
    },
    PlacementSession::HAS_NATIVE_Z_ORDER,
  );

  let hide_corners = state.monitors_by_hide_corner();
  // Listed on the first retire check that needs it, then shared by the
  // rest of the pass.
  let on_screen = wm_platform::OnScreenWindows::new();
  let mut reconciled = 0;
  for window in &windows {
    let id = window.id();
    let Some(&needs_reorder) = selected.get(&id) else {
      continue;
    };
    let monitor = window.monitor().context("No monitor.")?;
    let pass = ReconcilePass {
      owner: &monitor,
      now,
      compositor_frame,
      sampled_frame,
      on_screen: &on_screen,
      reorder: needs_reorder,
      hide_corner: hide_corners
        .get(&monitor.id())
        .copied()
        .unwrap_or(HideCorner::BottomRight),
    };
    reconciled += 1;
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
        let focus_span = PerfSpan::start("native_focus");
        if let Err(err) = entry
          .native
          .window()
          .and_then(wm_platform::NativeWindow::focus)
        {
          tracing::warn!("Window focus failed: {err}");
        }
        focus_span.finish(SLOW_CALL, format_args!("window={id}"));
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
  // Only a sync that ran to here has passed every gate below this frame.
  state.native_sync.synced_frame = compositor_frame;
  let t_total = span.elapsed();
  span.finish(
    SLOW_SPAN,
    format_args!(
      "{}",
      SyncDetail {
        origin,
        key_queued,
        windows: windows.len(),
        reconciled,
        settling: state
          .native_sync
          .windows
          .values()
          .filter(|entry| entry.settling())
          .count(),
        snapshot: t_snapshot,
        order: t_order.saturating_sub(t_snapshot),
        reconcile: t_total.saturating_sub(t_order),
      }
    ),
  );
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
    let capture_span = PerfSpan::start("pre_capture");
    let result = state
      .animation_manager
      .pre_capture(&captures, &state.dispatcher);
    capture_span
      .finish(SLOW_SPAN, format_args!("windows={}", captures.len()));
    result?;
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
  /// The on-screen windows, listed at most once for the whole commit.
  ///
  /// Each retire check needs them to find companions, and every window
  /// listing is a round-trip to the window server, so the checks of one
  /// commit share a single listing instead of paying for one each. It is
  /// a snapshot from the first check of the commit; a companion that
  /// shows itself later in the commit is seen by the next one, which the
  /// fence's recheck deadline guarantees.
  on_screen: &'a wm_platform::OnScreenWindows,
  reorder: bool,
  hide_corner: HideCorner,
}

/// Records where one window's reconciliation spends its time.
///
/// Always on, so a slow pass is caught without debug logging. A pass that
/// takes longer than `REPORT_AFTER` logs every span, so a slow commit
/// names its native calls: at `INFO` from `perf::SLOW_CALL` on, otherwise
/// at `DEBUG`.
struct PassTimer {
  started: Instant,
  calls: NativeCallSnapshot,
  last: Duration,
  marks: [(&'static str, Duration); Self::MAX_MARKS],
  len: usize,
}

impl PassTimer {
  /// A pass shorter than this is not reported.
  const REPORT_AFTER: Duration = Duration::from_millis(1);

  /// Most marks a pass records; a pass makes fewer, and extra marks are
  /// dropped. Fixed so that timing a pass never allocates.
  const MAX_MARKS: usize = 32;

  /// Starts timing a pass.
  fn start() -> Self {
    Self {
      started: Instant::now(),
      calls: NativeCallStats::snapshot(),
      last: Duration::ZERO,
      marks: [("", Duration::ZERO); Self::MAX_MARKS],
      len: 0,
    }
  }

  /// Ends the span since the previous mark, naming it `label`.
  fn mark(&mut self, label: &'static str) {
    let elapsed = self.started.elapsed();
    if let Some(slot) = self.marks.get_mut(self.len) {
      *slot = (label, elapsed.saturating_sub(self.last));
      self.len += 1;
    }
    self.last = elapsed;
  }

  /// Logs the spans of a slow pass.
  ///
  /// `app` names the window's process. It is read only for a pass that is
  /// reported, and from cached properties, so it never reaches the app.
  fn report(&self, id: &Uuid, app: impl FnOnce() -> String) {
    let slow = self.last >= SLOW_CALL;
    if self.last < Self::REPORT_AFTER
      || (!slow
        && !tracing::enabled!(
          target: perf::PERF_TARGET,
          tracing::Level::DEBUG
        ))
    {
      return;
    }
    let calls = NativeCallStats::snapshot().since(&self.calls);
    let spans = self.marks[..self.len]
      .iter()
      .filter(|(_, span)| !span.is_zero())
      .map(|(label, span)| format!("{label}={}us", span.as_micros()))
      .collect::<Vec<_>>()
      .join(" ");
    perf::emit(
      slow,
      format_args!(
        "reconcile_slow window={id} app={} total_ms={:.2} {spans} {calls}",
        app(),
        perf::millis(self.last),
      ),
    );
  }
}

/// Reads current geometry rather than treating a successful write as
/// completion.
fn observe(native: &PlacementSession) -> anyhow::Result<ObservedFrame> {
  let observation = native.observe()?;
  Ok(ObservedFrame {
    rect: observation.rect,
    dpi: observation.dpi,
    state: if observation.minimized {
      NativeState::Minimized
    } else if observation.maximized {
      NativeState::Maximized
    } else {
      NativeState::Normal
    },
  })
}

/// Reads a window back after a native write, as `plan` allows.
///
/// A plan that keeps the state from before the write skips the reads that
/// ask the application, so a write whose relayout is still running does
/// not hold up the pass: geometry comes from the window server alone.
fn observe_planned(
  native: &PlacementSession,
  plan: ObservePlan,
) -> anyhow::Result<ObservedFrame> {
  match plan {
    ObservePlan::Full => observe(native),
    ObservePlan::Geometry { state } => Ok(ObservedFrame {
      rect: native.observed_frame()?,
      dpi: native.dpi()?,
      state,
    }),
  }
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
    working_area: monitor.working_area.clone(),
    dpi: native.expected_dpi(monitor.dpi)?,
    state,
    parking_clamp: parked
      .then(|| PlacementSession::parking_clamp(target.height())),
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
/// After a frame write the observation is geometry only (see
/// `ObservePlan`), so it usually still shows the old frame: the write is
/// confirmed by the application's move event, or by the next frame tick
/// for a window the clock drives, rather than in this pass.
///
/// A window the clock drives gets no echo-marked sync (see
/// `handle_window_moved_or_resized`), so the per-tick sync of a window
/// with `frame.in_flight()` is what confirms its write. Any change that
/// stops syncing on frame ticks (such as syncing only when a tick is due)
/// must keep a tick due for such a window, or its write waits out the
/// 250 ms retry.
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
    let plan = ObservePlan::after_write(
      &request.mutation,
      &observed,
      PlacementSession::STATE_READS_ASK_APP,
    );
    // The display the request belongs to, so macOS need not enumerate
    // screens to find it.
    let monitor = entry.frame.desired.monitor.clone();
    entry.frame.apply(&request, now, |mutation| {
      match mutation {
        NativeMutation::Frame(rect) => {
          entry.native.set_frame_on_display(rect, &monitor)?;
          placed(rect);
        }
        NativeMutation::Restore(rect) => entry.native.restore(rect)?,
        NativeMutation::Minimize => entry.native.minimize()?,
        NativeMutation::Maximize => entry.native.maximize()?,
      }
      Ok(())
    })?;
    let observed = observe_planned(&entry.native, plan)?;
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
    on_screen,
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
          tracing::warn!(
            window = %id,
            ?minimum,
            observed = ?observed.rect,
            desired = ?entry.frame.desired.rect,
            "Learned size floor."
          );
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
      PlacementSession::parking_clamp(observed.rect.height()),
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
        state.animation_manager.companions_revealed(&id, on_screen)
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
  let z_order = desired_stacking(&window.state(), leads_workspace(window));
  if visible
    && native_state != NativeState::Minimized
    && (entry.stacking != z_order || reorder)
  {
    if let Some(order) = &z_order {
      // Apps restack themselves, so the last write says nothing about
      // where the window is now. Leaving the always-on-top band lifts a
      // window over every normal window, and sending one to the bottom
      // drops it under them; neither may repeat on a window already in
      // place. Entering the band also raises the window within it, which
      // a reorder asks for, so that write always goes out.
      let settled = match order {
        WindowZOrder::Normal => entry.native.has_z_order(order, &[])?,
        WindowZOrder::Bottom => {
          entry.native.has_z_order(order, &workspace_peers(window))?
        }
        _ => false,
      };
      if !settled {
        entry.native.set_z_order(order)?;
      }
    }
    entry.stacking = z_order;
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
  timer.report(&id, || window.native_properties().process_name);
  Ok(())
}

#[cfg(test)]
mod tests {
  use wm_common::{FloatingStateConfig, FullscreenStateConfig};

  use super::*;

  /// Builds a fullscreen state with the given always-on-top preference.
  fn fullscreen(shown_on_top: bool) -> WindowState {
    WindowState::Fullscreen(FullscreenStateConfig {
      maximized: true,
      shown_on_top,
    })
  }

  /// Builds focus effects that round corners, draw a border and fade.
  fn decorated_effects() -> WindowEffectConfig {
    let mut effects = WindowEffectConfig::default();
    effects.border.enabled = true;
    effects.corner_style.enabled = true;
    effects.corner_style.style = CornerStyle::Rounded;
    effects.transparency.enabled = true;
    effects
  }

  /// Builds a workspace of idle windows, none of which ask for a pass.
  fn idle_workspace(count: usize) -> (Uuid, Vec<ReconcileCandidate>) {
    let workspace = Uuid::new_v4();
    let candidates = (0..count)
      .map(|_| ReconcileCandidate {
        id: Uuid::new_v4(),
        workspace,
        retired: false,
        redraw: false,
        pending: false,
        dirty: false,
        planned: false,
        focus_pending: false,
        effects_stale: false,
      })
      .collect();

    (workspace, candidates)
  }

  #[test]
  fn focus_change_reconciles_two_windows_without_native_z_order() {
    let (workspace, mut candidates) = idle_workspace(10);
    // Focus moved from the first window to the second.
    candidates[0].effects_stale = true;
    candidates[1].effects_stale = true;
    let reorder_workspaces = HashSet::from([workspace]);
    let signals = ReconcileSignals {
      reorder_workspaces: &reorder_workspaces,
      all_effects_changed: false,
      focus_effects_changed: true,
    };

    let without = select_reconcile_set(&candidates, &signals, false);
    let with = select_reconcile_set(&candidates, &signals, true);

    assert_eq!(
      without,
      HashMap::from([
        (candidates[0].id, false),
        (candidates[1].id, false)
      ])
    );
    assert_eq!(with.len(), 10);
    assert!(with.values().all(|reorder| *reorder));
  }

  #[test]
  fn keyboard_focus_reconciles_only_the_window_gaining_focus() {
    // A focus command queues no effects update; the echo of the
    // application's own focus event does that later.
    let (workspace, mut candidates) = idle_workspace(10);
    candidates[3].focus_pending = true;
    let reorder_workspaces = HashSet::from([workspace]);
    let signals = ReconcileSignals {
      reorder_workspaces: &reorder_workspaces,
      all_effects_changed: false,
      focus_effects_changed: false,
    };

    assert_eq!(
      select_reconcile_set(&candidates, &signals, false),
      HashMap::from([(candidates[3].id, false)])
    );
    assert_eq!(
      select_reconcile_set(&candidates, &signals, true).len(),
      10
    );
  }

  #[test]
  fn focus_echo_without_effects_reconciles_nothing_without_z_order() {
    // Native focus was applied by the earlier sync, so none is pending.
    let (workspace, candidates) = idle_workspace(10);
    let reorder_workspaces = HashSet::from([workspace]);
    let signals = ReconcileSignals {
      reorder_workspaces: &reorder_workspaces,
      all_effects_changed: false,
      focus_effects_changed: true,
    };

    assert!(select_reconcile_set(&candidates, &signals, false).is_empty());
    assert_eq!(
      select_reconcile_set(&candidates, &signals, true).len(),
      10
    );
  }

  #[test]
  fn other_workspaces_are_not_restacked() {
    let (_, mut candidates) = idle_workspace(3);
    let (other, others) = idle_workspace(3);
    candidates.extend(others);
    let reorder_workspaces = HashSet::from([other]);
    let signals = ReconcileSignals {
      reorder_workspaces: &reorder_workspaces,
      all_effects_changed: false,
      focus_effects_changed: false,
    };

    let selected = select_reconcile_set(&candidates, &signals, true);

    assert_eq!(selected.len(), 3);
    assert!(candidates[3..]
      .iter()
      .all(|candidate| selected.get(&candidate.id) == Some(&true)));
  }

  #[test]
  fn capability_does_not_hide_other_reasons_to_reconcile() {
    let (workspace, mut candidates) = idle_workspace(6);
    candidates[0].redraw = true;
    candidates[1].pending = true;
    candidates[2].dirty = true;
    candidates[3].planned = true;
    candidates[4].retired = true;
    candidates[4].redraw = true;
    let reorder_workspaces = HashSet::from([workspace]);
    let signals = ReconcileSignals {
      reorder_workspaces: &reorder_workspaces,
      all_effects_changed: false,
      focus_effects_changed: false,
    };

    let selected = select_reconcile_set(&candidates, &signals, false);

    assert_eq!(
      selected,
      candidates[..4]
        .iter()
        .map(|candidate| (candidate.id, false))
        .collect()
    );
  }

  #[test]
  fn all_effects_update_still_reaches_every_window() {
    let (_, candidates) = idle_workspace(10);
    let reorder_workspaces = HashSet::new();
    let signals = ReconcileSignals {
      reorder_workspaces: &reorder_workspaces,
      all_effects_changed: true,
      focus_effects_changed: false,
    };

    assert_eq!(
      select_reconcile_set(&candidates, &signals, false).len(),
      10
    );
  }

  /// The selection as it was written inline before it became a function.
  fn selected_before_extraction(
    candidate: &ReconcileCandidate,
    signals: &ReconcileSignals<'_>,
  ) -> Option<bool> {
    let needs_reorder =
      signals.reorder_workspaces.contains(&candidate.workspace);
    if candidate.retired {
      return None;
    }
    let effects_changed = signals.all_effects_changed
      || (signals.focus_effects_changed && candidate.effects_stale);

    (candidate.redraw
      || effects_changed
      || needs_reorder
      || candidate.pending
      || candidate.dirty
      || candidate.planned)
      .then_some(needs_reorder)
  }

  #[test]
  fn native_z_order_selection_matches_the_inline_original() {
    let workspace = Uuid::new_v4();
    // Every combination of the ten boolean inputs, with the focus target
    // ignored by the oracle.
    for bits in 0u32..1 << 10 {
      let flag = |bit: u32| bits & (1 << bit) != 0;
      let candidate = ReconcileCandidate {
        id: Uuid::new_v4(),
        workspace: if flag(0) { workspace } else { Uuid::new_v4() },
        retired: flag(1),
        redraw: flag(2),
        pending: flag(3),
        dirty: flag(4),
        planned: flag(5),
        focus_pending: flag(9),
        effects_stale: flag(6),
      };
      let reorder_workspaces = HashSet::from([workspace]);
      let signals = ReconcileSignals {
        reorder_workspaces: &reorder_workspaces,
        all_effects_changed: flag(7),
        focus_effects_changed: flag(8),
      };

      let expected = selected_before_extraction(&candidate, &signals)
        .map(|reorder| (candidate.id, reorder));
      let actual = select_reconcile_set(
        std::slice::from_ref(&candidate),
        &signals,
        true,
      );

      assert_eq!(actual.into_iter().next(), expected, "bits={bits:#b}");
    }
  }

  #[test]
  fn fullscreen_window_takes_no_frame_effects() {
    let effects = decorated_effects();

    assert_eq!(
      frame_decorations(&fullscreen(false), &effects),
      (None, CornerStyle::Square)
    );
    assert_eq!(state_alpha(&fullscreen(false), None, &effects), None);
  }

  #[test]
  fn other_states_keep_configured_frame_effects() {
    let effects = decorated_effects();

    assert_eq!(
      frame_decorations(&WindowState::Tiling, &effects),
      (Some(effects.border.color.clone()), CornerStyle::Rounded)
    );
    assert_eq!(
      state_alpha(&WindowState::Tiling, None, &effects),
      Some(effects.transparency.opacity)
    );
  }

  #[test]
  fn commanded_opacity_applies_to_a_fullscreen_window() {
    let commanded = decorated_effects().transparency.opacity;

    assert_eq!(
      state_alpha(
        &fullscreen(false),
        Some(commanded),
        &WindowEffectConfig::default()
      ),
      Some(commanded)
    );
  }

  #[test]
  fn fullscreen_window_steps_behind_a_workspace_it_does_not_lead() {
    for shown_on_top in [false, true] {
      assert_eq!(
        desired_stacking(&fullscreen(shown_on_top), false),
        Some(WindowZOrder::Bottom)
      );
    }
  }

  #[test]
  fn leading_fullscreen_window_keeps_its_own_stacking() {
    assert_eq!(desired_stacking(&fullscreen(false), true), None);
    assert_eq!(
      desired_stacking(&fullscreen(true), true),
      Some(WindowZOrder::TopMost)
    );
  }

  #[test]
  fn tiling_and_floating_stacking_ignores_the_workspace_lead() {
    let floating = |shown_on_top| {
      WindowState::Floating(FloatingStateConfig {
        centered: true,
        shown_on_top,
      })
    };

    for leads in [false, true] {
      assert_eq!(
        desired_stacking(&WindowState::Tiling, leads),
        Some(WindowZOrder::Normal)
      );
      assert_eq!(
        desired_stacking(&floating(false), leads),
        Some(WindowZOrder::Normal)
      );
      assert_eq!(
        desired_stacking(&floating(true), leads),
        Some(WindowZOrder::TopMost)
      );
    }
  }

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

/// Tests for when a frame tick has work for a pass.
#[cfg(test)]
mod tick_due_tests {
  use super::*;

  /// Where the application has a window, before and after the slide.
  const HOME: (i32, i32) = (100, 100);
  const PARKED: (i32, i32) = (1919, 1079);
  const TILE: (i32, i32) = (400, 100);

  /// Builds a frame target at `at`.
  fn desired(at: (i32, i32)) -> DesiredFrame {
    DesiredFrame {
      rect: Rect::from_xy(at.0, at.1, 500, 400),
      monitor: Rect::from_xy(0, 0, 1920, 1080),
      working_area: Rect::from_xy(0, 0, 1920, 1040),
      dpi: 96,
      state: NativeState::Normal,
      parking_clamp: None,
    }
  }

  /// Builds an observation of a window at `at`.
  fn observed(at: (i32, i32)) -> ObservedFrame {
    ObservedFrame {
      rect: desired(at).rect,
      dpi: 96,
      state: NativeState::Normal,
    }
  }

  /// Builds a frame that has converged on `at`.
  fn converged(at: (i32, i32), now: Instant) -> FrameReconciler {
    let mut frame = FrameReconciler::new(desired(at));
    frame.observe(&observed(at), now);
    assert!(!frame.in_flight());
    frame
  }

  /// A window's settling state, owned so a view can borrow it.
  struct Parts {
    frame: FrameReconciler,
    motion: Option<MotionOwner>,
    source: Option<SourceLease>,
    retire_after: Option<RetireFence>,
    decoration: Option<DecorationPhase>,
    visibility_pending: bool,
    cancelled: bool,
    running: bool,
    retry_at: Option<Instant>,
  }

  impl Parts {
    /// Builds a window with nothing settling.
    fn idle(now: Instant) -> Self {
      Self {
        frame: converged(HOME, now),
        motion: None,
        source: None,
        retire_after: None,
        decoration: None,
        visibility_pending: false,
        cancelled: false,
        running: false,
        retry_at: None,
      }
    }

    /// Borrows the state as a view.
    fn view(&self) -> SettlingView<'_> {
      SettlingView {
        frame: &self.frame,
        motion: self.motion.as_ref(),
        source: self.source.as_ref(),
        retire_after: self.retire_after.as_ref(),
        decoration: self.decoration.as_ref(),
        visibility_pending: self.visibility_pending,
        cancelled: self.cancelled,
        running: self.running,
        retry_at: self.retry_at,
      }
    }

    /// Builds a window parked behind its running cover.
    fn sliding(now: Instant) -> Self {
      Self {
        frame: converged(PARKED, now),
        source: Some(SourceLease::default()),
        running: true,
        ..Self::idle(now)
      }
    }

    /// Builds a window whose overlay waits for compositor frame `gate`.
    fn preparing(gate: u64, now: Instant) -> Self {
      Self {
        motion: Some(MotionOwner::Overlay(MotionPreparation::new(
          1,
          Some(gate),
          now,
          true,
        ))),
        ..Self::idle(now)
      }
    }

    /// Builds a window whose retire fence waits for frame `after`.
    fn fenced(after: u64, now: Instant) -> Self {
      Self {
        retire_after: Some(RetireFence::new(after, now)),
        ..Self::idle(now)
      }
    }
  }

  /// Runs `tick_due` over `windows`.
  fn due(
    presented: u64,
    synced: u64,
    completed: bool,
    windows: &[Parts],
  ) -> bool {
    tick_due(
      presented,
      synced,
      false,
      completed,
      windows.iter().map(Parts::view),
    )
  }

  #[test]
  fn nothing_settling_is_never_due() {
    let now = Instant::now();
    assert!(!due(9, 0, false, &[]));
    assert!(!due(9, 0, false, &[Parts::idle(now)]));
  }

  #[test]
  fn a_failed_clock_is_always_due() {
    assert!(tick_due(1, 1, true, false, []));
  }

  /// A completion whose `Wake` was dropped from the full tick channel
  /// still shows in its flag, so the next tick of any kind is due.
  #[test]
  fn a_lost_wake_is_found_by_its_completion_flag() {
    let now = Instant::now();
    let windows = [Parts::sliding(now)];
    assert!(!due(5, 5, false, &windows));
    assert!(due(5, 5, true, &windows));
  }

  #[test]
  fn a_converged_slide_is_not_due_between_gates() {
    let now = Instant::now();
    let windows = [Parts::sliding(now), Parts::sliding(now)];
    for presented in 1..=40 {
      assert!(!due(presented, 1, false, &windows));
    }
  }

  #[test]
  fn an_unconfirmed_frame_write_is_due_every_tick() {
    let now = Instant::now();
    let mut window = Parts::sliding(now);
    window.frame = FrameReconciler::new(desired(PARKED));
    assert!(window.frame.in_flight());
    assert!(due(2, 2, false, &[window]));
  }

  #[test]
  fn an_unconfirmed_visibility_change_is_due_every_tick() {
    let now = Instant::now();
    let mut window = Parts::sliding(now);
    window.visibility_pending = true;
    assert!(due(2, 2, false, &[window]));
  }

  #[test]
  fn a_cancelled_presentation_is_due_only_while_settling() {
    let now = Instant::now();
    let mut idle = Parts::idle(now);
    idle.cancelled = true;
    assert!(!due(2, 2, false, &[idle]));
    let mut sliding = Parts::sliding(now);
    sliding.cancelled = true;
    assert!(due(2, 2, false, &[sliding]));
  }

  #[test]
  fn overlay_readiness_is_due_once_its_frame_is_passed() {
    let now = Instant::now();
    let windows = [Parts::sliding(now), Parts::preparing(10, now)];
    assert!(!due(9, 8, false, &windows));
    assert!(!due(10, 8, false, &windows));
    assert!(due(11, 8, false, &windows));
  }

  /// A gate the last sync had already passed holds nothing up.
  #[test]
  fn a_gate_passed_by_the_last_sync_is_not_due_again() {
    let now = Instant::now();
    let windows = [Parts::preparing(10, now)];
    assert!(due(11, 9, false, &windows));
    assert!(!due(11, 11, false, &windows));
  }

  #[test]
  fn the_lowest_open_gate_decides() {
    let now = Instant::now();
    let windows = [
      Parts::preparing(20, now),
      Parts::fenced(12, now),
      Parts::fenced(30, now),
    ];
    assert!(!due(12, 5, false, &windows));
    assert!(due(13, 5, false, &windows));
  }

  #[test]
  fn a_retire_fence_is_due_after_composition_then_after_the_reveal() {
    let now = Instant::now();
    let mut window = Parts::fenced(10, now);
    assert!(!due(10, 10, false, &[Parts::fenced(10, now)]));
    assert!(due(11, 10, false, &[Parts::fenced(10, now)]));
    // The pass at frame 11 sees the companions revealed.
    let mut fence = window.retire_after.take().expect("Fence.");
    assert!(!fence.due(11, now, || true));
    window.retire_after = Some(fence);
    window.retry_at = Some(now);
    assert!(!due(11, 11, false, &[Parts::idle(now), window]));
  }

  #[test]
  fn a_revealed_fence_is_due_one_frame_later() {
    let now = Instant::now();
    let mut fence = RetireFence::new(10, now);
    assert!(!fence.due(11, now, || true));
    let window = Parts {
      retire_after: Some(fence),
      ..Parts::idle(now)
    };
    assert!(!due(11, 11, false, std::slice::from_ref(&window)));
    assert!(due(12, 11, false, std::slice::from_ref(&window)));
  }

  /// A fence past its composition gate with no reveal yet is rechecked at
  /// its own deadline, never by ticks.
  #[test]
  fn a_polled_fence_does_not_wake_ticks() {
    let now = Instant::now();
    let fence = RetireFence::new(10, now);
    assert!(fence.frame_gate(11).is_none());
    assert!(fence.recheck_at(11, now).is_some());
    let window = Parts {
      retire_after: Some(fence),
      retry_at: fence.recheck_at(11, now),
      ..Parts::idle(now)
    };
    assert!(!due(30, 11, false, &[window]));
  }

  #[test]
  fn a_revealing_companion_hold_follows_the_same_gates() {
    let now = Instant::now();
    let window = Parts {
      decoration: Some(DecorationPhase::Revealing(RetireFence::new(
        10, now,
      ))),
      ..Parts::idle(now)
    };
    assert!(!due(10, 10, false, std::slice::from_ref(&window)));
    assert!(due(11, 10, false, std::slice::from_ref(&window)));
  }

  /// Every combination of the fields that keep a window settling.
  ///
  /// `synced` is the frame the last sync read. A fence either waits for a
  /// frame after it, or has passed its source frame and seen the
  /// companions revealed.
  fn every_state(now: Instant, synced: u64) -> Vec<Parts> {
    let waiting = RetireFence::new(synced + 3, now);
    let mut revealed = RetireFence::new(synced.saturating_sub(2), now);
    assert!(!revealed.due(synced, now, || true));
    let mut states = Vec::new();
    for in_flight in [false, true] {
      for motion in 0..4 {
        for source in 0..3 {
          for retire in 0..3 {
            for decoration in 0..3 {
              for flags in 0..16_u8 {
                let fence = if retire == 2 { revealed } else { waiting };
                states.push(Parts {
                  frame: if in_flight {
                    FrameReconciler::new(desired(HOME))
                  } else {
                    converged(HOME, now)
                  },
                  motion: match motion {
                    0 => None,
                    1 => Some(MotionOwner::Native { batch: 1 }),
                    2 => {
                      Some(MotionOwner::Overlay(MotionPreparation::new(
                        1,
                        Some(synced + 1),
                        now,
                        true,
                      )))
                    }
                    _ => Some(MotionOwner::Overlay(
                      MotionPreparation::new(1, None, now, true),
                    )),
                  },
                  source: match source {
                    0 => None,
                    1 => Some(SourceLease { restoring: false }),
                    _ => Some(SourceLease { restoring: true }),
                  },
                  retire_after: (retire > 0).then_some(fence),
                  decoration: match decoration {
                    0 => None,
                    1 => Some(DecorationPhase::Drawing),
                    _ => Some(DecorationPhase::Revealing(fence)),
                  },
                  visibility_pending: flags & 1 != 0,
                  cancelled: flags & 2 != 0,
                  running: flags & 4 != 0,
                  retry_at: None,
                });
              }
            }
          }
        }
      }
    }
    states
  }

  /// Whether the pass that last ran would have asked for a deadline: the
  /// rule at the end of `reconcile_managed`.
  fn pass_sets_retry(state: &Parts, synced: u64, now: Instant) -> bool {
    let rechecks = |fence: Option<&RetireFence>| {
      fence.is_some_and(|fence| fence.recheck_at(synced, now).is_some())
    };
    state.visibility_pending
      || state.source.as_ref().is_some_and(|source| source.restoring)
      || rechecks(state.retire_after.as_ref())
      || rechecks(match state.decoration.as_ref() {
        Some(DecorationPhase::Revealing(fence)) => Some(fence),
        _ => None,
      })
  }

  /// Every settling state has a deadline, a frame gate, a completion flag
  /// or an immediate wake, so none can fall asleep with no way back.
  #[test]
  fn every_settling_state_has_a_way_back() {
    let now = Instant::now();
    let synced = 10;
    let mut settling = 0;
    for state in every_state(now, synced) {
      if !state.view().settling() {
        continue;
      }
      settling += 1;
      let wake = state.view().wake(synced);
      assert!(
        wake.covered() || pass_sets_retry(&state, synced, now),
        "Stranded settling state: {wake:?}",
      );
    }
    assert!(settling > 1000, "Enumerated {settling} settling states.");
  }

  /// A fence waits for a frame or for its own recheck, never for neither.
  #[test]
  fn a_fence_always_has_a_gate_or_a_recheck() {
    let now = Instant::now();
    for after in 0..6 {
      for synced in 0..9 {
        let mut fence = RetireFence::new(after, now);
        for revealed_at in [None, Some(after + 1)] {
          fence.revealed_at = revealed_at;
          assert!(
            fence.frame_gate(synced).is_some()
              || fence.recheck_at(synced, now).is_some(),
            "after {after}, synced {synced}, revealed {revealed_at:?}",
          );
        }
      }
    }
  }

  /// One window of a simulated workspace slide.
  struct SimWindow {
    parts: Parts,
    /// Where the application has the window.
    app: ObservedFrame,
    /// A frame write the application applies when the frame is reached.
    landing: Option<(u64, Rect)>,
    /// Frames the application takes to apply a write.
    delay: u64,
    /// The frame at which the running motion sets its completion flag.
    completes_at: Option<u64>,
  }

  /// How many frames a simulated slide runs.
  const SLIDE_FRAMES: u64 = 20;

  /// A workspace slide over several windows, one pass at a time.
  ///
  /// Mirrors the order of `reconcile_managed`, `release_ready` and
  /// `begin_pending` on the pieces that decide timing: `FrameReconciler`,
  /// `MotionPreparation` and `RetireFence`. The application lands its
  /// writes on its own schedule, so both ways of driving it see the same
  /// world. Time stands still; deadlines are covered by their own tests.
  struct Slide {
    now: Instant,
    windows: Vec<SimWindow>,
    presented: u64,
    synced: u64,
    /// Every transition, with the pass frame it happened at.
    log: Vec<(u64, usize, &'static str)>,
    syncs: u64,
  }

  impl Slide {
    /// Starts a slide of windows whose applications take `delays` frames
    /// to apply a write.
    fn start(delays: &[u64]) -> Self {
      let now = Instant::now();
      let windows = delays
        .iter()
        .map(|delay| {
          let mut parts = Parts::preparing(0, now);
          parts.frame = converged(HOME, now);
          SimWindow {
            parts,
            app: observed(HOME),
            landing: None,
            delay: *delay,
            completes_at: None,
          }
        })
        .collect();
      let mut slide = Self {
        now,
        windows,
        presented: 0,
        synced: 0,
        log: Vec::new(),
        syncs: 0,
      };
      slide.sync();
      slide
    }

    /// Whether any window still needs the frame clock.
    fn clock_runs(&self) -> bool {
      self
        .windows
        .iter()
        .any(|window| window.parts.view().settling())
    }

    /// Whether any motion has set its completion flag.
    fn completed(&self) -> bool {
      self.windows.iter().any(|window| {
        window.parts.running
          && window.completes_at.is_some_and(|at| at <= self.presented)
      })
    }

    /// Whether a tick at the current frame is due.
    fn due(&self) -> bool {
      tick_due(
        self.presented,
        self.synced,
        false,
        self.completed(),
        self.windows.iter().map(|window| window.parts.view()),
      )
    }

    /// Runs one commit at the presented frame.
    fn sync(&mut self) {
      let frame = self.presented;
      let now = self.now;
      self.syncs += 1;
      for (index, window) in self.windows.iter_mut().enumerate() {
        let completed = window.completes_at.is_some_and(|at| at <= frame);
        Self::pass(window, index, frame, completed, now, &mut self.log);
      }
      // `release_ready`: a batch starts together.
      let blocked = self.windows.iter().any(|window| {
        window
          .parts
          .motion
          .as_ref()
          .and_then(MotionOwner::preparation)
          .is_some_and(|motion| {
            !motion.ready(window.parts.frame.generation, frame)
          })
      });
      if !blocked {
        for (index, window) in self.windows.iter_mut().enumerate() {
          if window.parts.motion.take().is_some() {
            window.parts.running = true;
            window.completes_at = Some(frame + SLIDE_FRAMES);
            self.log.push((frame, index, "released"));
          }
        }
      }
      self.synced = frame;
    }

    /// Runs one window's pass at `frame`.
    fn pass(
      window: &mut SimWindow,
      index: usize,
      frame: u64,
      completed: bool,
      now: Instant,
      log: &mut Vec<(u64, usize, &'static str)>,
    ) {
      let parts = &mut window.parts;
      parts.retry_at = None;
      if let Some((_, rect)) =
        window.landing.take_if(|(at, _)| *at <= frame)
      {
        window.app.rect = rect;
      }
      let observed = window.app.clone();
      if parts.running && completed {
        parts.running = false;
        window.completes_at = None;
        if let Some(source) = &mut parts.source {
          source.restoring = true;
        }
        log.push((frame, index, "handoff"));
      }
      let preparation =
        parts.motion.as_ref().and_then(MotionOwner::preparation);
      let retaining =
        preparation.is_some_and(|motion| motion.retains_source(frame));
      if !retaining && preparation.is_some() && parts.source.is_none() {
        parts.source = Some(SourceLease::default());
        log.push((frame, index, "leased"));
      }
      let suppressing = parts
        .source
        .as_ref()
        .is_some_and(|source| !source.restoring);
      if retaining {
        parts.frame.observe(&observed, now);
      } else {
        parts.frame.retarget(desired(if suppressing {
          PARKED
        } else {
          TILE
        }));
        let mut landing = None;
        if let Some(request) = parts.frame.next(&observed, now) {
          let delay = window.delay;
          parts
            .frame
            .apply(&request, now, |mutation| {
              if let NativeMutation::Frame(rect) = mutation {
                landing = Some((frame + delay, rect.clone()));
              }
              Ok(())
            })
            .expect("Write.");
          window.landing = landing;
          // The read after a write shows the frame from before it.
          parts.frame.observe(&observed, now);
        }
      }
      let converged = parts.frame.phase == ReconcilePhase::Converged
        && parts.frame.converged(&observed);
      if let Some(MotionOwner::Overlay(motion)) = &mut parts.motion {
        motion.observe(
          parts.frame.generation,
          suppressing && converged,
          frame,
        );
      }
      let restoring =
        parts.source.as_ref().is_some_and(|source| source.restoring);
      if restoring && converged {
        parts.source = None;
        parts.retire_after = Some(RetireFence::new(frame, now));
        log.push((frame, index, "restored"));
      }
      if let Some(fence) = parts.retire_after.as_mut() {
        if fence.due(frame, now, || true) {
          parts.retire_after = None;
          log.push((frame, index, "retired"));
        } else if let Some(at) = fence.recheck_at(frame, now) {
          parts.retry_at = Some(at);
        }
      }
      if parts.source.as_ref().is_some_and(|source| source.restoring) {
        parts.retry_at = Some(now + Duration::from_millis(250));
      }
    }

    /// Drives ticks until the clock stops, syncing on each tick when
    /// `only_when_due` is not set and on due ticks otherwise.
    ///
    /// Returns the syncs between the slide's release and its completion,
    /// where the cover runs alone.
    fn run(&mut self, only_when_due: bool) -> u64 {
      let mut mid_slide = 0;
      while self.clock_runs() {
        self.presented += 1;
        assert!(self.presented < 200, "The slide never ends.");
        if !only_when_due || self.due() {
          let sliding = self.windows.iter().all(|window| {
            window.parts.running
              && window.completes_at.is_some_and(|at| at > self.presented)
          });
          self.sync();
          if sliding {
            mid_slide += 1;
          }
        }
      }
      mid_slide
    }
  }

  /// Syncing only on due ticks moves every state at the frame syncing on
  /// every tick does, in a fraction of the syncs.
  #[test]
  fn due_ticks_transition_at_the_same_frames_as_every_tick() {
    for delays in [
      vec![1],
      vec![1, 2, 3],
      vec![3, 3, 1, 2],
      vec![5, 1],
      vec![2, 2, 2, 2, 2],
    ] {
      let mut every = Slide::start(&delays);
      let mut sparse = Slide::start(&delays);
      let every_mid = every.run(false);
      let sparse_mid = sparse.run(true);
      assert_eq!(every.log, sparse.log, "delays {delays:?}");
      assert!(
        every.log.iter().any(|(_, _, what)| *what == "retired"),
        "The slide retires its covers."
      );
      assert!(sparse.syncs < every.syncs, "delays {delays:?}");
      assert_eq!(
        sparse_mid, 0,
        "A converged slide needs no mid-slide sync."
      );
      assert!(
        every_mid >= SLIDE_FRAMES - 2,
        "Every tick syncs through the slide: {every_mid}.",
      );
    }
  }

  /// Syncs a simulated 20-frame slide of three windows takes.
  #[test]
  fn sparse_ticks_cut_the_syncs_of_a_slide() {
    let delays = [1, 2, 3];
    let mut every = Slide::start(&delays);
    let mut sparse = Slide::start(&delays);
    let every_mid = every.run(false);
    let sparse_mid = sparse.run(true);
    eprintln!(
      "20-frame slide, 3 windows: every tick = {} syncs ({every_mid} while \
       the covers run), due ticks = {} syncs ({sparse_mid} while the \
       covers run)",
      every.syncs, sparse.syncs
    );
    assert!(sparse.syncs * 2 < every.syncs);
  }
}
