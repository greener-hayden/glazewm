use std::{
  collections::{HashMap, HashSet},
  sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
  },
  time::{Duration, Instant},
};

use anyhow::Context;
use tokio::sync::mpsc;
use uuid::Uuid;
use wm_common::{AnimationEffectConfig, AnimationsConfig, WindowState};
#[cfg(target_os = "macos")]
use wm_platform::DispatcherExtMacOs;
use wm_platform::{
  AnimationCapture, AnimationContext, AnimationWindow, Dispatcher,
  EasingFunction, FrameClock, FrameSignal, OpacityValue, Rect, WindowId,
};

use crate::{
  models::{NativeMonitorProperties, WindowContainer},
  traits::{CommonGetters, WindowGetters},
  user_config::UserConfig,
};

/// How far an opening window starts from its full size.
///
/// Small on purpose: the overlay is the window's surface scaled, and at
/// 140ms a few percent reads as the window settling in rather than as
/// distortion.
const OPEN_START_SCALE: f32 = 0.94;

/// Platform-neutral policy describing why a window changes presentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnimationTrigger {
  WindowOpened,
  WindowMoved,
  /// A window arriving with the workspace being switched to. It starts
  /// one screen away, on the side it is travelling from.
  WorkspaceEntering(SlideDirection),
  /// A window leaving with the workspace being switched away from. It
  /// ends one screen away and is hidden once it gets there.
  WorkspaceLeaving(SlideDirection),
}

impl AnimationTrigger {
  /// Selects one transition before either native adapter changes a window.
  #[must_use]
  pub fn select(
    skip: bool,
    slide: Option<SlideDirection>,
    visible: bool,
    opening: bool,
  ) -> Option<Self> {
    if skip {
      None
    } else if let Some(direction) = slide {
      Some(if visible {
        Self::WorkspaceEntering(direction)
      } else {
        Self::WorkspaceLeaving(direction)
      })
    } else if !visible {
      None
    } else if opening {
      Some(Self::WindowOpened)
    } else {
      Some(Self::WindowMoved)
    }
  }

  /// Where a window's animation runs between, which is not always where
  /// the real window goes.
  ///
  /// A leaving workspace ends one screen away and is hidden there; an
  /// entering one starts one screen away. An opening window grows and
  /// fades in at its tile rather than sliding over from wherever the OS
  /// spawned it, which read as the window landing in the wrong place and
  /// being dragged into the right one. Everything else animates from
  /// where it is to the tile it occupies.
  #[must_use]
  pub fn path(self, target: &Rect, monitor: &Rect) -> AnimationPath {
    match self {
      Self::WorkspaceLeaving(direction) => AnimationPath {
        start: None,
        target: direction.offset(target, monitor),
        opacity: None,
      },
      Self::WorkspaceEntering(direction) => AnimationPath {
        start: Some(direction.opposite().offset(target, monitor)),
        target: target.clone(),
        opacity: None,
      },
      Self::WindowOpened => AnimationPath {
        start: Some(target.scale_from_center(OPEN_START_SCALE)),
        target: target.clone(),
        opacity: Some((OpacityValue(0.0), OpacityValue(1.0))),
      },
      Self::WindowMoved => AnimationPath {
        start: None,
        target: target.clone(),
        opacity: None,
      },
    }
  }

  /// Keeps Windows moves on their native windows.
  pub fn uses_native(self) -> bool {
    cfg!(target_os = "windows") && self == Self::WindowMoved
  }

  /// Whether the trigger is one side of a workspace slide.
  #[must_use]
  pub fn is_slide(self) -> bool {
    matches!(self, Self::WorkspaceEntering(_) | Self::WorkspaceLeaving(_))
  }
}

/// The geometry of one window's animation.
#[derive(Clone, Debug, PartialEq)]
pub struct AnimationPath {
  /// Where the animation begins, if not where the window currently is.
  pub start: Option<Rect>,
  /// Where the animation ends.
  pub target: Rect,
  /// Opacity to travel between, for an animation that fades.
  pub opacity: Option<(OpacityValue, OpacityValue)>,
}

/// An animation decided for a window, ready to start.
pub struct AnimationPlan<'a> {
  pub effect: &'a AnimationEffectConfig,
  pub trigger: AnimationTrigger,
  pub path: AnimationPath,
}

/// Which way the workspaces travel during a switch.
///
/// Named for the motion of the content, not the key pressed: moving to a
/// higher-numbered workspace sends the old content `Left` and brings the
/// new one in from the right, the way a pager works.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlideDirection {
  Left,
  Right,
}

impl SlideDirection {
  /// Offsets `rect` by one monitor width in this direction.
  ///
  /// A whole monitor rather than the window's own width so every window on
  /// the workspace travels the same distance and the layout moves as one
  /// sheet. Offsetting by each window's width would have them arrive at
  /// different times and read as a scatter.
  #[must_use]
  pub fn offset(self, rect: &Rect, monitor: &Rect) -> Rect {
    let distance = match self {
      Self::Left => -monitor.width(),
      Self::Right => monitor.width(),
    };

    rect.translate_to_coordinates(rect.x() + distance, rect.y())
  }

  /// The side an entering workspace arrives from, which is the side the
  /// outgoing one is heading toward.
  #[must_use]
  pub fn opposite(self) -> Self {
    match self {
      Self::Left => Self::Right,
      Self::Right => Self::Left,
    }
  }

  /// The direction the outgoing workspace travels when moving from
  /// `from_index` to `to_index` in the monitor's workspace order.
  #[must_use]
  pub fn between(from_index: usize, to_index: usize) -> Self {
    if to_index > from_index {
      Self::Left
    } else {
      Self::Right
    }
  }
}

/// Selects the owner of animated pixels.
#[derive(Clone, Debug)]
enum MotionTarget {
  Native(u32),
  Overlay,
}

/// Prepared geometry and effect, independent of motion and overlay
/// lifetime.
#[derive(Clone, Debug)]
struct AnimationSpec {
  target: MotionTarget,
  duration: Duration,
  easing: EasingFunction,

  /// Start and target positions for the animation.
  start_rect: Rect,
  target_rect: Rect,

  /// Whether the animation is part of a workspace slide.
  ///
  /// A slide is never interrupted by a non-slide redraw, which would
  /// otherwise restart a leaving window's travel mid-flight when an
  /// unrelated `wm-redraw` lands during the slide. A leaving window is
  /// also travelling off screen, so a later sync measures it a whole
  /// monitor from the tile it would occupy and starts a second animation
  /// hauling it back — the outgoing workspace appears to slide into the
  /// incoming one before vanishing.
  is_slide: bool,

  /// Start and target opacity for the animation, or `None` if no opacity
  /// animation is active.
  start_opacity: Option<OpacityValue>,
  target_opacity: Option<OpacityValue>,
}

impl AnimationSpec {
  /// Creates a new animation between two rects, and optionally between
  /// two opacities.
  fn new(
    start_rect: Rect,
    target_rect: Rect,
    opacity: Option<(OpacityValue, OpacityValue)>,
    config: &AnimationEffectConfig,
    is_slide: bool,
  ) -> Self {
    let (start_opacity, target_opacity) = opacity.unzip();

    Self {
      target: MotionTarget::Overlay,
      duration: Duration::from_millis(u64::from(config.duration_ms)),
      easing: config.easing.clone(),
      start_rect,
      target_rect,
      is_slide,
      start_opacity,
      target_opacity,
    }
  }

  /// Returns the normalized animation progress in `[0.0, 1.0]`.
  fn progress(&self, elapsed: Duration) -> f32 {
    if elapsed >= self.duration {
      1.0
    } else {
      // `as_millis` truncates, quantising a 180ms animation into whole
      // millisecond steps. Seconds keep the sub-frame precision.
      let progress = elapsed.as_secs_f32() / self.duration.as_secs_f32();

      progress.clamp(0.0, 1.0)
    }
  }

  /// Whether the animation has completed.
  fn is_complete(&self, elapsed: Duration) -> bool {
    elapsed >= self.duration
  }

  /// Returns the interpolated rect at the current animation progress.
  fn rect_at(&self, elapsed: Duration) -> Rect {
    let eased_progress = self.easing.apply(self.progress(elapsed));
    self
      .start_rect
      .interpolate(&self.target_rect, eased_progress)
  }

  /// Returns the interpolated opacity at the current animation progress,
  /// or `None` if no opacity animation is active.
  fn opacity_at(&self, elapsed: Duration) -> Option<OpacityValue> {
    let (start, end) =
      (self.start_opacity.as_ref()?, self.target_opacity.as_ref()?);

    let eased_progress = self.easing.apply(self.progress(elapsed));
    Some(start.interpolate(end, eased_progress))
  }
}

/// A released effect owns elapsed time and its native completion signal.
struct RunningMotion {
  started: Instant,
  sampled: Duration,
  completed: Arc<AtomicBool>,
}

/// A retained native cover and its compositor pacing requirement.
struct Overlay {
  window: AnimationWindow,
  frame_rate: u32,
  /// Last successfully submitted frame on tick-driven backends.
  frame: Rect,
}

/// Manages animations for all windows.
pub struct AnimationManager {
  /// Prepared effects remain until native handoff relinquishes motion.
  animations: HashMap<Uuid, AnimationSpec>,
  /// Only released effects own a running clock.
  running: HashMap<Uuid, RunningMotion>,
  failed_updates: HashSet<Uuid>,

  /// Sender for animation tick events.
  tick_tx: mpsc::Sender<FrameSignal>,

  /// Receiver for animation tick events.
  pub tick_rx: mpsc::Receiver<FrameSignal>,

  /// Per-window overlay windows keyed by window ID.
  windows: HashMap<Uuid, Overlay>,

  /// Pre-captured frames, keyed by window ID, waiting for their
  /// animations to start.
  // LINT: On Windows a capture is a zero-sized token; the map is still
  // what carries a real screenshot on macOS.
  #[allow(clippy::zero_sized_map_values)]
  pending_captures: HashMap<Uuid, AnimationCapture>,

  /// Shared GPU context for animation overlay windows. Lazily
  /// initialized on the first animation.
  context: Option<AnimationContext>,

  /// The running frame clock and the fallback rate it was started with,
  /// if any.
  clock: Option<(u32, FrameClock)>,

  /// Animations prepared this sync but not yet handed to the compositor.
  ///
  /// Preparing one costs a screen capture, so starting each as it is
  /// prepared staggers a workspace switch by roughly that cost per
  /// window — the last window in a five-window switch began 180ms into
  /// a 200ms slide and barely moved. Released together instead.
  pending_starts: Vec<Uuid>,

  /// Whether "Displays have separate Spaces" setting is enabled.
  #[cfg(target_os = "macos")]
  displays_have_separate_spaces: bool,
}

impl AnimationManager {
  // LINT: See `pending_captures`.
  #[allow(clippy::zero_sized_map_values)]
  pub fn new(
    // LINT: `dispatcher` is only used on macOS.
    #[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
    dispatcher: &Dispatcher,
  ) -> Self {
    let (tick_tx, tick_rx) = mpsc::channel(1);

    Self {
      animations: HashMap::new(),
      running: HashMap::new(),
      failed_updates: HashSet::new(),
      tick_tx,
      tick_rx,
      windows: HashMap::new(),
      pending_captures: HashMap::new(),
      context: None,
      clock: None,
      pending_starts: Vec::new(),
      #[cfg(target_os = "macos")]
      displays_have_separate_spaces: dispatcher
        .displays_have_separate_spaces(),
    }
  }

  /// Whether an animation is currently active for a given window.
  pub fn is_animating(&self, window_id: &Uuid) -> bool {
    self.animations.contains_key(window_id)
  }

  /// Whether one released animation has reached its target.
  pub fn is_complete(&self, id: &Uuid) -> bool {
    self.running.get(id).is_some_and(|motion| {
      if AnimationWindow::SELF_ANIMATING {
        motion.completed.load(Ordering::Acquire)
      } else {
        self
          .animations
          .get(id)
          .is_some_and(|spec| spec.is_complete(motion.started.elapsed()))
      }
    })
  }

  /// Whether this presentation owns released motion.
  pub fn is_running(&self, id: &Uuid) -> bool {
    self.running.contains_key(id)
  }

  /// Samples native motion without writing application geometry.
  pub fn native_frame(&self, id: &Uuid) -> Option<Rect> {
    let spec = self.animations.get(id)?;
    if !matches!(spec.target, MotionTarget::Native(_)) {
      return None;
    }
    let elapsed = self
      .running
      .get(id)
      .map_or(Duration::ZERO, |motion| motion.sampled);
    Some(spec.rect_at(elapsed))
  }

  /// Returns the native motion's final visible bounds.
  pub fn native_target(&self, id: &Uuid) -> Option<&Rect> {
    let spec = self.animations.get(id)?;
    matches!(spec.target, MotionTarget::Native(_))
      .then_some(&spec.target_rect)
  }

  /// Prepares visible motion without creating a cover.
  pub fn prepare_native(
    &mut self,
    id: Uuid,
    plan: &AnimationPlan,
    start: Rect,
    frame_rate: u32,
  ) {
    let mut spec = AnimationSpec::new(
      start,
      plan.path.target.clone(),
      None,
      plan.effect,
      false,
    );
    spec.target = MotionTarget::Native(frame_rate);
    self.running.remove(&id);
    self.pending_starts.retain(|pending| *pending != id);
    self.pending_captures.remove(&id);
    self.failed_updates.remove(&id);
    self.animations.insert(id, spec);
    self.release_animation(&id);
    self.update_clock();
  }

  /// Whether retained visuals still cover a native source.
  pub fn has_overlay(&self, id: &Uuid) -> bool {
    self.windows.contains_key(id)
  }

  /// Whether compositor readiness is still required by retained covers.
  pub fn has_overlays(&self) -> bool {
    !self.windows.is_empty()
  }

  /// Takes failed launches or updates for native recovery.
  pub fn take_failures(&mut self) -> HashSet<Uuid> {
    std::mem::take(&mut self.failed_updates)
  }

  /// Aligns retained visuals before revealing sources.
  pub fn place_overlay(
    &mut self,
    id: &Uuid,
    rect: &Rect,
  ) -> anyhow::Result<()> {
    if let Some(overlay) = self.windows.get_mut(id) {
      overlay.window.resize(rect)?;
      overlay.window.stop_at(rect, Some(&OpacityValue(1.0)))?;
      overlay.frame = rect.clone();
    }
    self.clear_motion(id);
    Ok(())
  }

  /// Queues a prepared effect once the source handoff is ready.
  pub fn release_animation(&mut self, id: &Uuid) {
    if self.animations.contains_key(id)
      && !self.running.contains_key(id)
      && !self.pending_starts.contains(id)
    {
      self.pending_starts.push(*id);
    }
  }

  /// Retires visuals only after successful handoff.
  pub fn retire_overlay(&mut self, id: &Uuid) -> anyhow::Result<()> {
    if let Some(overlay) = self.windows.get_mut(id) {
      overlay.window.destroy()?;
    }
    self.windows.remove(id);
    self.clear_motion(id);
    self.pending_captures.remove(id);
    self.failed_updates.remove(id);
    Ok(())
  }

  /// Freezes current presentation before relinquishing motion ownership.
  pub fn finish_animation(&mut self, id: &Uuid) -> anyhow::Result<()> {
    if self.running.contains_key(id) {
      if let Some(overlay) = self.windows.get_mut(id) {
        let frame = overlay
          .window
          .current_frame()?
          .unwrap_or_else(|| overlay.frame.clone());
        overlay.window.stop_at(&frame, None)?;
        overlay.frame = frame;
      }
    }
    self.clear_motion(id);
    Ok(())
  }

  /// Ends timing while preserving the independently owned native cover.
  fn clear_motion(&mut self, id: &Uuid) {
    self.animations.remove(id);
    self.running.remove(id);
    self.pending_starts.retain(|pending| pending != id);
    self.update_clock();
  }

  /// Updates all active animations during a single tick.
  ///
  /// Updates get batched into a single compositor transaction.
  ///
  /// Does nothing where the platform animates for itself: the compositor
  /// is already producing frames, and the transaction below would put a
  /// synchronous hop to the event loop thread on every one of them. Ticks
  /// still fire so completion can drive native handover.
  pub fn tick_update(
    &mut self,
    dispatcher: &Dispatcher,
  ) -> anyhow::Result<()> {
    if AnimationWindow::SELF_ANIMATING || self.running.is_empty() {
      return Ok(());
    }

    self.sample_frames(Instant::now());

    // A completed animation is included, not filtered out. Completion
    // falls between ticks, so skipping it leaves the overlay up to a
    // frame short of the target while the handover puts the real window
    // at full travel — two copies, offset by the remainder, for as long
    // as the overlay lives.
    let rects = self
      .animations
      .iter()
      .filter_map(|(id, anim)| {
        if matches!(anim.target, MotionTarget::Native(_)) {
          return None;
        }
        let elapsed = self.running.get(id)?.sampled;
        Some((*id, anim.rect_at(elapsed), anim.opacity_at(elapsed)))
      })
      .collect::<Vec<_>>();
    if rects.is_empty() {
      return Ok(());
    }

    let mut failed = HashSet::new();
    self
      .context
      .as_ref()
      .context("Animation context not initialized.")?
      .transaction(
        || {
          // One overlay failing must not stall the rest of the frame.
          for (id, rect, opacity) in &rects {
            let Some(anim_window) = self.windows.get(id) else {
              continue;
            };

            if let Err(err) =
              anim_window.window.update(rect, opacity.as_ref())
            {
              tracing::warn!("Failed to update animation window: {err}");
              failed.insert(*id);
            }
          }
        },
        dispatcher,
      )
      .context("Animation update failed.")?;
    for (id, rect, _) in rects {
      if !failed.contains(&id) {
        if let Some(overlay) = self.windows.get_mut(&id) {
          overlay.frame = rect;
        }
      }
    }
    self.failed_updates.extend(failed);

    Ok(())
  }

  /// Advances samples only on animation ticks.
  fn sample_frames(&mut self, now: Instant) {
    for motion in self.running.values_mut() {
      motion.sampled = now.saturating_duration_since(motion.started);
    }
  }

  /// Returns the animation effect config if an animation should be
  /// started for a window, or `None` if no animation is needed.
  pub fn animation_effect_for_window<'a>(
    &self,
    window: &WindowContainer,
    trigger: AnimationTrigger,
    target_rect: &Rect,
    monitor_properties: &NativeMonitorProperties,
    config: &'a UserConfig,
  ) -> Option<&'a AnimationEffectConfig> {
    // Skip animation if:
    //  - The window is minimized.
    //  - The window is fullscreen. Games and video players live here, and
    //    an animation would capture, layer, cloak, overlay and move them.
    //    They get the hard cut a desktop normally gives them.
    //  - The window is maximized (macOS only - can't override the OS's
    //    animation).
    //  - The window is stranded in the corner.
    //
    // The corner serves two purposes: it is where an animating window is
    // parked, and where a hidden workspace keeps its windows. Which one
    // applies is the difference between a window that should animate and
    // one that must not.
    //
    // A slide is the one animation that may legitimately begin at the
    // corner: its windows are parked there precisely because they were
    // hidden, and the entering half has to travel out of it.
    //
    // Every other animation interpolates from the window's last known
    // frame. Starting one while that frame is the corner makes the window
    // fly in from off screen — and if it was `Shown` and cornered it
    // missed a restore, so animating parks it again and loses it for
    // good. Neither is a move the user made.
    let is_slide = matches!(
      trigger,
      AnimationTrigger::WorkspaceEntering(_)
        | AnimationTrigger::WorkspaceLeaving(_)
    );

    let starts_at_corner =
      window.is_in_corner(&monitor_properties.working_area);

    if window.native_properties().is_minimized
      || matches!(window.state(), WindowState::Fullscreen(_))
      || (window.native_properties().is_maximized
        && cfg!(target_os = "macos"))
      || (!self.is_animating(&window.id())
        && starts_at_corner
        && !is_slide)
    {
      return None;
    }

    match (trigger, &config.value.animations) {
      (
        AnimationTrigger::WindowOpened,
        AnimationsConfig {
          window_open: Some(open_config),
          ..
        },
      ) => {
        if self.animations.contains_key(&window.id()) {
          None
        } else {
          Some(open_config)
        }
      }
      (
        AnimationTrigger::WorkspaceEntering(_)
        | AnimationTrigger::WorkspaceLeaving(_),
        AnimationsConfig {
          workspace_switch: Some(switch_config),
          ..
        },
      ) => Some(switch_config),
      (
        AnimationTrigger::WindowMoved,
        AnimationsConfig {
          window_move: Some(move_config),
          ..
        },
      ) => {
        // A slide owns its windows for its whole duration. A redraw
        // landing mid-slide targets the windows' tiles, which differ from
        // the slide targets (a leaving window's target is off screen), so
        // the distance check alone would restart the slide mid-flight.
        // The slide finishes, and the redraw re-runs after it anyway.
        if self
          .animations
          .get(&window.id())
          .is_some_and(|anim| anim.is_slide)
        {
          return None;
        }

        // If the window is mid-animation, compare the previous animation
        // target to the new target.
        let frame = window.native_properties().frame;
        let prev_rect = self
          .animations
          .get(&window.id())
          .map_or(&frame, |anim| &anim.target_rect);

        let distance = (prev_rect.x() - target_rect.x()).abs()
          + (prev_rect.y() - target_rect.y()).abs()
          + (prev_rect.width() - target_rect.width()).abs()
          + (prev_rect.height() - target_rect.height()).abs();

        // TODO: Validate config to only allow pixel values.
        #[allow(clippy::cast_possible_truncation)]
        let threshold_px = move_config.trigger_threshold.amount as i32;

        if distance > threshold_px {
          Some(&move_config.effect)
        } else {
          None
        }
      }
      _ => None,
    }
  }

  /// Captures frames for a batch of windows concurrently.
  ///
  /// The frames are stored until their animations start, so every
  /// animation started by one sync begins from an already-captured frame.
  /// On macOS a capture is a screenshot, so capturing sequentially would
  /// stagger each animation's start time and break the motion of a
  /// workspace switch into a wave. On Windows the overlay is a live
  /// thumbnail and each capture returns at once.
  pub fn pre_capture(
    &mut self,
    windows: &[(Uuid, WindowId)],
    dispatcher: &Dispatcher,
  ) -> anyhow::Result<()> {
    // Drop captures from a sync that never started their animations.
    self.pending_captures.clear();

    // A window whose overlay is still up keeps its frame, so a fresh
    // capture would only be discarded.
    let windows = windows
      .iter()
      .filter(|(id, _)| !self.windows.contains_key(id))
      .collect::<Vec<_>>();

    if windows.is_empty() {
      return Ok(());
    }

    let context = match &self.context {
      Some(ctx) => ctx,
      None => self
        .context
        .get_or_insert(AnimationContext::new(dispatcher)?),
    };

    let capture_t0 = Instant::now();
    let results = std::thread::scope(|scope| {
      // Spawn every capture before joining any, or they run one at a
      // time.
      let handles = windows
        .iter()
        .map(|(id, window_id)| {
          (id, scope.spawn(move || context.capture_frame(*window_id)))
        })
        .collect::<Vec<_>>();

      handles
        .into_iter()
        .map(|(id, handle)| (id, handle.join()))
        .collect::<Vec<_>>()
    });

    tracing::debug!(
      "Captured {} window frames in {:?}.",
      results.len(),
      capture_t0.elapsed()
    );

    for (id, result) in results {
      match result {
        Ok(Ok(capture)) => {
          self.pending_captures.insert(*id, capture);
        }
        Ok(Err(err)) => {
          tracing::warn!("Failed to capture window frame: {err}");
        }
        Err(_) => {
          tracing::warn!("Capture thread panicked for window {id}.");
        }
      }
    }

    Ok(())
  }

  /// Prepares a held overlay and effect, preserving any retained capture.
  ///
  /// The plan's path decides where the animation begins. Without a start
  /// of its own it begins where the window is, or where its running
  /// animation has got to.
  pub fn prepare_animation(
    &mut self,
    window: &WindowContainer,
    plan: &AnimationPlan,
    monitor_properties: &NativeMonitorProperties,
    dispatcher: &Dispatcher,
  ) -> anyhow::Result<()> {
    // The monitor's refresh rate, for platforms that pace the clock by
    // sleeping rather than by the compositor.
    let frame_rate = monitor_properties.refresh_rate.unwrap_or(60);

    let presented_rect = self
      .windows
      .get(&window.id())
      .map(|overlay| {
        overlay
          .window
          .current_frame()
          .map(|frame| frame.unwrap_or_else(|| overlay.frame.clone()))
      })
      .transpose()?;
    let start_rect = presented_rect
      .or_else(|| plan.path.start.clone())
      .unwrap_or_else(|| window.native_properties().frame.clone());

    let animation = AnimationSpec::new(
      start_rect,
      plan.path.target.clone(),
      plan.path.opacity,
      plan.effect,
      plan.trigger.is_slide(),
    );

    // On macOS, windows cannot span across multiple displays when
    // "Displays have separate Spaces" is enabled. Attempting to position a
    // window beyond the display bounds causes it to wrap around on the
    // same display. We therefore crop the animation to only be shown on
    // the source display.
    //
    // A workspace slide is cropped whatever that setting says. It travels
    // a whole monitor width, so its bounding box reaches an entire screen
    // past the window and onto the neighbouring display — where the
    // outgoing workspace is seen sliding across a monitor it was never
    // on. A switch belongs to one display; only a window genuinely moving
    // between them should be drawn across both.
    let outer_rect = {
      let outer_rect = animation.start_rect.union(&animation.target_rect);

      #[cfg(target_os = "macos")]
      if self.displays_have_separate_spaces || plan.trigger.is_slide() {
        let display_bounds =
          dispatcher.nearest_display(&window.native())?.bounds()?;

        outer_rect.crop(&display_bounds)
      } else {
        outer_rect
      }

      #[cfg(not(target_os = "macos"))]
      if plan.trigger.is_slide() {
        outer_rect.crop(&monitor_properties.bounds)
      } else {
        outer_rect
      }
    };

    let capture = self.pending_captures.remove(&window.id());

    let context = match &self.context {
      Some(ctx) => ctx,
      None => self
        .context
        .get_or_insert(AnimationContext::new(dispatcher)?),
    };

    // Resize existing overlay to the new bounding box when the target
    // changes mid-flight, preserving the screenshot and z-order.
    if let Some(anim_window) = self.windows.get_mut(&window.id()) {
      anim_window.window.resize(&outer_rect)?;
      anim_window.frame_rate = frame_rate;

      // A replacement is prepared, not running: freeze the sampled
      // presentation before waiting for the next native readiness signal.
      anim_window.window.stop_at(
        &animation.start_rect,
        animation.start_opacity.as_ref(),
      )?;
      anim_window.frame = animation.start_rect.clone();
    } else {
      let capture = match capture {
        Some(capture) => capture,
        None => context.capture_frame(window.native().id())?,
      };

      let anim_window = AnimationWindow::new(
        context,
        &window.native(),
        capture,
        &animation.start_rect,
        &outer_rect,
        animation.start_opacity,
        dispatcher,
      )?;

      self.windows.insert(
        window.id(),
        Overlay {
          window: anim_window,
          frame_rate,
          frame: animation.start_rect.clone(),
        },
      );
    }

    self.running.remove(&window.id());
    self.pending_starts.retain(|id| *id != window.id());
    self.failed_updates.remove(&window.id());
    self.animations.insert(window.id(), animation);
    self.update_clock();
    Ok(())
  }

  /// Releases ready effects together, preserving later launches on
  /// failure.
  pub fn begin_pending(&mut self) {
    let mut launched = Vec::new();
    for window_id in std::mem::take(&mut self.pending_starts) {
      let Some(anim) = self.animations.get(&window_id) else {
        continue;
      };
      if self.running.contains_key(&window_id) {
        continue;
      }
      let completed = Arc::new(AtomicBool::new(false));
      let completion = completed.clone();
      let tick_tx = self.tick_tx.clone();
      let result = if matches!(anim.target, MotionTarget::Native(_)) {
        Ok(())
      } else {
        self
          .windows
          .get(&window_id)
          .context("Prepared animation has no overlay.")
          .and_then(|window| {
            if AnimationWindow::SELF_ANIMATING {
              window
                .window
                .animate_to(
                  &anim.target_rect,
                  anim.duration,
                  &anim.easing,
                  anim.target_opacity.as_ref(),
                  move || {
                    completion.store(true, Ordering::Release);
                    let _ = tick_tx.try_send(FrameSignal::Wake);
                  },
                )
                .map_err(Into::into)
            } else {
              Ok(())
            }
          })
      };
      match result {
        Ok(()) => launched.push((window_id, completed)),
        Err(err) => {
          tracing::warn!("Animation launch failed: {err}");
          self.failed_updates.insert(window_id);
        }
      }
    }
    // Completion may trail a compositor submission, but must never lead
    // it.
    let started = Instant::now();
    self
      .running
      .extend(launched.into_iter().map(|(id, completed)| {
        (
          id,
          RunningMotion {
            started,
            sampled: Duration::ZERO,
            completed,
          },
        )
      }));
    self.update_clock();
  }

  /// Keeps compositor signals alive while any overlay awaits handoff.
  ///
  /// The clock's fallback rate is the highest refresh rate among the
  /// animated windows' monitors. A running clock is only replaced when
  /// that rate changes: animations of one sync start together, and a
  /// clock started per window would tick once per start and render a
  /// burst of duplicate frames.
  ///
  /// Called on animation start and completion.
  fn update_clock(&mut self) {
    let Some(frame_rate) = self
      .windows
      .values()
      .map(|overlay| overlay.frame_rate)
      .chain(self.animations.values().filter_map(
        |spec| match spec.target {
          MotionTarget::Native(rate) => Some(rate),
          MotionTarget::Overlay => None,
        },
      ))
      .max()
    else {
      self.clock = None;
      return;
    };

    // Some macOS displays report 0Hz. `FrameClock` floors a zero rate at
    // 1Hz, which is a stopped animation rather than a fast one, so take
    // the ordinary refresh rate instead.
    let frame_rate = if frame_rate == 0 { 60 } else { frame_rate };

    if self
      .clock
      .as_ref()
      .is_some_and(|(rate, _)| *rate == frame_rate)
    {
      return;
    }

    let tick_tx = self.tick_tx.clone();
    let clock = FrameClock::start(frame_rate, move |signal| {
      // An unavailable clock exits after this notification. Unlike frame
      // ticks, losing its last signal would strand retained covers.
      if matches!(signal, FrameSignal::Unavailable) {
        return tick_tx.blocking_send(signal).is_ok();
      }
      match tick_tx.try_send(signal) {
        Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => true,
        Err(mpsc::error::TrySendError::Closed(_)) => false,
      }
    });

    self.clock = Some((frame_rate, clock));
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn trigger_selection_is_shared_and_ordered() {
    use AnimationTrigger::{
      WindowMoved, WindowOpened, WorkspaceEntering, WorkspaceLeaving,
    };
    for direction in [SlideDirection::Left, SlideDirection::Right] {
      for opening in [false, true] {
        assert_eq!(
          AnimationTrigger::select(false, Some(direction), true, opening),
          Some(WorkspaceEntering(direction))
        );
        assert_eq!(
          AnimationTrigger::select(false, Some(direction), false, opening),
          Some(WorkspaceLeaving(direction))
        );
        for visible in [false, true] {
          assert!(AnimationTrigger::select(
            true,
            Some(direction),
            visible,
            opening
          )
          .is_none());
        }
      }
    }
    assert_eq!(
      AnimationTrigger::select(false, None, true, true),
      Some(WindowOpened)
    );
    assert_eq!(
      AnimationTrigger::select(false, None, true, false),
      Some(WindowMoved)
    );
    for opening in [false, true] {
      assert!(
        AnimationTrigger::select(false, None, false, opening).is_none()
      );
      assert!(
        AnimationTrigger::select(true, None, true, opening).is_none()
      );
    }
  }

  // LINT: Windows capture tokens are zero-sized, as in `new`.
  #[allow(clippy::zero_sized_map_values)]
  fn manager_without_overlays() -> AnimationManager {
    let (tick_tx, tick_rx) = mpsc::channel(1);
    AnimationManager {
      animations: HashMap::new(),
      running: HashMap::new(),
      failed_updates: HashSet::new(),
      tick_tx,
      tick_rx,
      windows: HashMap::new(),
      pending_captures: HashMap::new(),
      context: None,
      clock: None,
      pending_starts: Vec::new(),
      #[cfg(target_os = "macos")]
      displays_have_separate_spaces: false,
    }
  }

  fn test_spec() -> AnimationSpec {
    AnimationSpec::new(
      Rect::from_xy(0, 0, 100, 100),
      Rect::from_xy(200, 0, 100, 100),
      None,
      &AnimationEffectConfig::default(),
      false,
    )
  }

  #[test]
  fn prepared_effect_has_no_motion_clock_and_release_is_idempotent() {
    let mut manager = manager_without_overlays();
    let id = Uuid::new_v4();
    manager.animations.insert(id, test_spec());
    assert!(manager.is_animating(&id));
    assert!(!manager.is_running(&id));
    assert!(!manager.is_complete(&id));
    manager.release_animation(&id);
    manager.release_animation(&id);
    assert_eq!(manager.pending_starts, vec![id]);
    assert!(manager.running.is_empty());
    manager.finish_animation(&id).unwrap();
    assert_eq!(manager.pending_starts, Vec::<Uuid>::new());
    assert!(!manager.is_animating(&id));
  }

  #[test]
  fn release_does_not_restart_running_motion() {
    let mut manager = manager_without_overlays();
    let id = Uuid::new_v4();
    manager.animations.insert(id, test_spec());
    let started = Instant::now();
    manager.running.insert(
      id,
      RunningMotion {
        started,
        sampled: Duration::ZERO,
        completed: Arc::new(AtomicBool::new(false)),
      },
    );
    manager.release_animation(&id);
    manager.begin_pending();
    assert_eq!(manager.running[&id].started, started);
    assert_eq!(manager.pending_starts, Vec::<Uuid>::new());
  }

  #[test]
  fn compositor_completion_belongs_to_its_released_motion() {
    let mut manager = manager_without_overlays();
    let id = Uuid::new_v4();
    manager.animations.insert(id, test_spec());
    let retired_completion = Arc::new(AtomicBool::new(false));
    manager.running.insert(
      id,
      RunningMotion {
        started: Instant::now(),
        sampled: Duration::ZERO,
        completed: retired_completion.clone(),
      },
    );
    manager.finish_animation(&id).unwrap();
    manager.animations.insert(id, test_spec());
    let current_completion = Arc::new(AtomicBool::new(false));
    manager.running.insert(
      id,
      RunningMotion {
        started: Instant::now(),
        sampled: Duration::ZERO,
        completed: current_completion.clone(),
      },
    );
    retired_completion.store(true, Ordering::Release);
    assert!(!manager.is_complete(&id));
    current_completion.store(true, Ordering::Release);
    if AnimationWindow::SELF_ANIMATING {
      assert!(manager.is_complete(&id));
    }
  }

  #[test]
  fn launch_failures_are_reported_for_the_entire_batch() {
    let mut manager = manager_without_overlays();
    let ids = [Uuid::new_v4(), Uuid::new_v4()];
    for id in ids {
      manager.animations.insert(id, test_spec());
      manager.release_animation(&id);
    }
    manager.begin_pending();
    assert_eq!(manager.take_failures(), HashSet::from(ids));
    assert!(manager.running.is_empty());
    assert_eq!(manager.pending_starts, Vec::<Uuid>::new());
  }

  #[test]
  fn released_motion_reaches_its_exact_target() {
    let spec = test_spec();
    assert!(!spec.is_complete(Duration::ZERO));
    assert!(spec.is_complete(spec.duration));
    assert_eq!(spec.rect_at(spec.duration), spec.target_rect);
    assert_eq!(spec.rect_at(Duration::ZERO), spec.start_rect);
  }

  #[test]
  fn opened_window_grows_in_at_its_tile() {
    let tile = Rect::from_xy(100, 100, 1000, 500);
    let monitor = Rect::from_xy(0, 0, 3440, 1440);

    let path = AnimationTrigger::WindowOpened.path(&tile, &monitor);

    let start = path.start.expect("An opening window has a start.");
    assert_eq!(path.target, tile);
    let (start_center, tile_center) =
      (start.center_point(), tile.center_point());
    assert_eq!(
      (start_center.x, start_center.y),
      (tile_center.x, tile_center.y)
    );
    assert!(start.width() < tile.width());
    assert!(tile.contains_rect(&start));
    assert_eq!(path.opacity, Some((OpacityValue(0.0), OpacityValue(1.0))));
  }

  #[test]
  fn slide_travels_one_monitor_width() {
    let tile = Rect::from_xy(100, 100, 1000, 500);
    let monitor = Rect::from_xy(0, 0, 3440, 1440);

    let leaving = AnimationTrigger::WorkspaceLeaving(SlideDirection::Left)
      .path(&tile, &monitor);
    assert_eq!(leaving.start, None);
    assert_eq!(leaving.target, Rect::from_xy(100 - 3440, 100, 1000, 500));

    let entering =
      AnimationTrigger::WorkspaceEntering(SlideDirection::Left)
        .path(&tile, &monitor);
    assert_eq!(
      entering.start,
      Some(Rect::from_xy(100 + 3440, 100, 1000, 500))
    );
    assert_eq!(entering.target, tile);
    assert_eq!(entering.opacity, None);
  }

  /// Only Windows moves bypass proxy rendering.
  #[test]
  fn native_trigger_policy() {
    assert_eq!(
      AnimationTrigger::WindowMoved.uses_native(),
      cfg!(target_os = "windows")
    );
    assert!(!AnimationTrigger::WindowOpened.uses_native());
    for direction in [SlideDirection::Left, SlideDirection::Right] {
      assert!(
        !AnimationTrigger::WorkspaceEntering(direction).uses_native()
      );
      assert!(!AnimationTrigger::WorkspaceLeaving(direction).uses_native());
    }
  }

  /// Creates a move plan without platform resources.
  fn move_plan(
    effect: &AnimationEffectConfig,
    target: Rect,
  ) -> AnimationPlan<'_> {
    AnimationPlan {
      effect,
      trigger: AnimationTrigger::WindowMoved,
      path: AnimationPath {
        start: None,
        target,
        opacity: None,
      },
    }
  }

  /// Native moves need clocks, never covers.
  #[test]
  fn native_motion_lifecycle() {
    let mut manager = manager_without_overlays();
    let id = Uuid::new_v4();
    let effect = AnimationEffectConfig::default();
    let initial = Rect::from_xy(-800, 50, 400, 300);
    let target = Rect::from_xy(200, 100, 600, 400);
    let plan = move_plan(&effect, target.clone());
    manager.prepare_native(id, &plan, initial.clone(), 144);
    assert_eq!(manager.native_frame(&id), Some(initial.clone()));
    assert_eq!(manager.native_target(&id), Some(&target));
    assert!(!manager.is_running(&id));
    assert!(!manager.has_overlays());
    assert!(manager.context.is_none());
    assert_eq!(manager.clock.as_ref().map(|(rate, _)| *rate), Some(144));
    manager.begin_pending();
    assert!(manager.is_running(&id));
    assert!(manager.take_failures().is_empty());
    let started = manager.running[&id].started;
    let duration = manager.animations[&id].duration;
    assert_eq!(manager.native_frame(&id), Some(initial));
    manager.sample_frames(started + duration / 2);
    let middle = manager.native_frame(&id).expect("Native frame.");
    assert_ne!(middle, target);
    assert_eq!(manager.native_frame(&id), Some(middle));
    manager.sample_frames(started + duration);
    assert_eq!(manager.native_frame(&id), Some(target));
    manager.finish_animation(&id).expect("Motion cleanup.");
    assert!(manager.native_frame(&id).is_none());
    assert!(manager.native_target(&id).is_none());
    assert!(!manager.is_running(&id));
    assert!(manager.clock.is_none());
  }

  /// Retargeting starts from observed native geometry.
  #[test]
  fn native_retarget_uses_observation() {
    let mut manager = manager_without_overlays();
    let id = Uuid::new_v4();
    let effect = AnimationEffectConfig::default();
    let plan = move_plan(&effect, Rect::from_xy(200, 100, 600, 400));
    manager.prepare_native(id, &plan, Rect::from_xy(0, 0, 400, 300), 60);
    manager.begin_pending();
    let observed = Rect::from_xy(37, 14, 420, 310);
    let replacement =
      move_plan(&effect, Rect::from_xy(-500, -200, 300, 200));
    manager.prepare_native(id, &replacement, observed.clone(), 120);
    assert_eq!(manager.native_frame(&id), Some(observed));
    assert_eq!(manager.native_target(&id), Some(&replacement.path.target));
    assert!(!manager.is_running(&id));
    assert_eq!(manager.pending_starts, vec![id]);
    manager.finish_animation(&id).expect("Cancellation.");
    manager.begin_pending();
    assert!(!manager.is_animating(&id));
    assert!(manager.clock.is_none());
  }

  /// Overlay geometry never becomes native placement.
  #[test]
  fn overlay_excludes_native_frames() {
    let mut manager = manager_without_overlays();
    let id = Uuid::new_v4();
    manager.animations.insert(id, test_spec());
    assert!(manager.native_frame(&id).is_none());
    assert!(manager.native_target(&id).is_none());
  }

  #[test]
  fn moved_window_starts_where_it_is() {
    let tile = Rect::from_xy(100, 100, 1000, 500);
    let monitor = Rect::from_xy(0, 0, 3440, 1440);

    let path = AnimationTrigger::WindowMoved.path(&tile, &monitor);

    assert_eq!(path.start, None);
    assert_eq!(path.target, tile);
    assert!(!AnimationTrigger::WindowMoved.is_slide());
    assert!(
      AnimationTrigger::WorkspaceLeaving(SlideDirection::Right).is_slide()
    );
  }
}
