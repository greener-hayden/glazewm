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
use wm_common::{
  AnimationEffectConfig, AnimationsConfig, WindowMoveAnimationConfig,
  WindowState,
};
#[cfg(target_os = "macos")]
use wm_platform::DispatcherExtMacOs;
use wm_platform::{
  AnimationCapture, AnimationContext, AnimationWindow, CompanionOverlay,
  Dispatcher, EasingFunction, FrameClock, FrameSignal, NativeWindow,
  OpacityValue, Rect, Spring, SpringState, WindowId,
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

/// Distinct steps of an 8-bit alpha channel.
///
/// A spring measures opacity in these steps, so its 0.5 px settle
/// tolerance becomes half an alpha step, below what a display shows.
const ALPHA_STEPS: f64 = 255.0;

/// How far an overlay extends past its path so companion thumbnails that
/// reach outside the window's frame are not clipped.
///
/// A border ring reaches a few pixels out at 100% and about three times
/// that at 200% with a thick stroke. The overlay has no redirection
/// bitmap, so the transparent margin costs nothing to compose.
#[cfg(target_os = "windows")]
const COMPANION_MARGIN_PX: i32 = 32;

/// Velocity of each rect edge, in px per second, ordered left, top,
/// right, bottom.
type EdgeVelocity = [f64; 4];

/// Platform-neutral policy describing why a window changes presentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnimationTrigger {
  WindowOpened,
  WindowMoved,
  /// A visible window sent to a hidden workspace. It fades out where it
  /// stands and is hidden once the fade ends, as a leaving slide is.
  WindowSent,
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
    change: WindowChange,
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
      (change == WindowChange::Sent).then_some(Self::WindowSent)
    } else if change == WindowChange::Opened {
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
      // `target` is where the window stands, not its new tile, which is
      // on a workspace nobody can see.
      Self::WindowSent => AnimationPath {
        start: Some(target.clone()),
        target: target.clone(),
        opacity: Some((OpacityValue(1.0), OpacityValue(0.0))),
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

/// What happened to a window this commit, apart from its geometry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowChange {
  /// Newly managed and shown for the first time.
  Opened,
  /// Was visible, and now belongs to a hidden workspace.
  Sent,
  /// Neither; only its placement may have changed.
  Placed,
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

/// How a prepared overlay motion begins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MotionStart {
  /// Holds its start frame until the coordinator releases its batch.
  Held,
  /// Starts at once, continuing the motion it replaces.
  ///
  /// Only for a cover already presented over a concealed source. Such a
  /// retarget has no readiness to wait for, and holding it froze a
  /// running slide for a whole commit: a quick double workspace switch
  /// stopped for about 30ms before reversing.
  Immediate,
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

/// Whether moving from `from` to `to` travels far enough to animate.
///
/// The distance is the sum of the edge and size differences, in pixels.
fn exceeds_threshold(
  from: &Rect,
  to: &Rect,
  config: &WindowMoveAnimationConfig,
) -> bool {
  let distance = (from.x() - to.x()).abs()
    + (from.y() - to.y()).abs()
    + (from.width() - to.width()).abs()
    + (from.height() - to.height()).abs();

  // TODO: Validate config to only allow pixel values.
  #[allow(clippy::cast_possible_truncation)]
  let threshold_px = config.trigger_threshold.amount as i32;

  distance > threshold_px
}

/// Selects the owner of animated pixels.
#[derive(Clone, Debug)]
enum MotionTarget {
  Native(u32),
  Overlay,
}

/// Returns the edges of `rect`, ordered as in `EdgeVelocity`.
fn edges(rect: &Rect) -> [i32; 4] {
  [rect.left, rect.top, rect.right, rect.bottom]
}

/// Per-edge springs for `easing: spring`, each measured from its target.
#[derive(Clone, Copy, Debug)]
struct SpringMotion {
  spring: Spring,
  /// Starting state of each edge, ordered as in `EdgeVelocity`.
  edges: [SpringState; 4],
  /// Starting opacity state, in alpha steps. Always starts at rest.
  opacity: SpringState,
}

impl SpringMotion {
  /// Creates the springs between two rects and opacities, with each edge
  /// starting at `velocity`.
  fn new(
    spring: Spring,
    start_rect: &Rect,
    target_rect: &Rect,
    opacity: Option<(OpacityValue, OpacityValue)>,
    velocity: EdgeVelocity,
  ) -> Self {
    let (start, target) = (edges(start_rect), edges(target_rect));
    let edges = std::array::from_fn(|index| SpringState {
      displacement: f64::from(start[index] - target[index]),
      velocity: velocity[index],
    });
    let opacity = SpringState {
      displacement: opacity.map_or(0.0, |(start, target)| {
        f64::from(start.0 - target.0) * ALPHA_STEPS
      }),
      velocity: 0.0,
    };

    Self {
      spring,
      edges,
      opacity,
    }
  }

  /// Time until every edge and the opacity have settled.
  fn settle_time(&self) -> Duration {
    self
      .edges
      .iter()
      .chain(std::iter::once(&self.opacity))
      .map(|state| self.spring.settle_time(*state))
      .max()
      .unwrap_or(Duration::ZERO)
  }
}

/// Prepared geometry and effect, independent of motion and overlay
/// lifetime.
#[derive(Clone, Debug)]
struct AnimationSpec {
  target: MotionTarget,
  /// Length of the motion. For a spring, its settle time.
  duration: Duration,
  easing: EasingFunction,
  /// Springs for `easing: spring`, or `None` for a curve easing.
  spring: Option<SpringMotion>,

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

  /// Whether the real window stays where the cover stands until motion
  /// ends. A window fading out to a hidden workspace would otherwise move
  /// to its new tile under the cover, and a live thumbnail would stretch
  /// it to the old frame.
  holds_source: bool,

  /// Start and target opacity for the animation, or `None` if no opacity
  /// animation is active.
  start_opacity: Option<OpacityValue>,
  target_opacity: Option<OpacityValue>,
}

impl AnimationSpec {
  /// Creates a new animation between two rects, and optionally between
  /// two opacities.
  ///
  /// A spring starts each edge at `velocity`, so a retarget carries the
  /// motion on. A curve easing ignores it.
  fn new(
    start_rect: Rect,
    target_rect: Rect,
    opacity: Option<(OpacityValue, OpacityValue)>,
    config: &AnimationEffectConfig,
    is_slide: bool,
    velocity: EdgeVelocity,
  ) -> Self {
    let spring = config.spring().map(|spring| {
      SpringMotion::new(
        spring,
        &start_rect,
        &target_rect,
        opacity,
        velocity,
      )
    });
    let duration = spring.as_ref().map_or_else(
      || Duration::from_millis(u64::from(config.duration_ms)),
      SpringMotion::settle_time,
    );
    let (start_opacity, target_opacity) = opacity.unzip();

    Self {
      target: MotionTarget::Overlay,
      duration,
      easing: config.easing.clone(),
      spring,
      start_rect,
      target_rect,
      is_slide,
      holds_source: false,
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
  ///
  /// A spring's duration is its settle time, so it completes once every
  /// edge is within 0.5 px of its target and nearly at rest.
  fn is_complete(&self, elapsed: Duration) -> bool {
    elapsed >= self.duration
  }

  /// Returns the smallest rect enclosing every frame of the motion.
  ///
  /// A curve easing stays between its start and target. A spring does
  /// not: it overshoots with bounce, and a retarget that carries velocity
  /// away from the new target keeps travelling before it turns. An
  /// overlay sized to the endpoints alone clipped the window for as long
  /// as it was outside them, so the spring's path is sampled.
  fn path_bounds(&self) -> Rect {
    /// Samples across the settle time; one per frame at 144 Hz for a
    /// 400 ms motion, and a spring's excursion is smooth between them.
    const SAMPLES: u32 = 64;

    let mut bounds = self.start_rect.union(&self.target_rect);
    if self.spring.is_some() {
      for step in 1..SAMPLES {
        bounds =
          bounds.union(&self.rect_at(self.duration * step / SAMPLES));
      }
    }
    bounds
  }

  /// Returns the interpolated rect at the current animation progress.
  fn rect_at(&self, elapsed: Duration) -> Rect {
    let Some(motion) = self.spring.filter(|_| !self.is_complete(elapsed))
    else {
      let eased_progress = self.easing.apply(self.progress(elapsed));
      return self
        .start_rect
        .interpolate(&self.target_rect, eased_progress);
    };

    let time = elapsed.as_secs_f64();
    let target = edges(&self.target_rect);
    // LINT: Edges stay within a few screens of their targets.
    #[allow(clippy::cast_possible_truncation)]
    let edge = |index: usize| {
      let state = motion.spring.state_at(motion.edges[index], time);
      target[index] + state.displacement.round() as i32
    };

    Rect::from_ltrb(edge(0), edge(1), edge(2), edge(3))
  }

  /// Returns the interpolated opacity at the current animation progress,
  /// or `None` if no opacity animation is active.
  fn opacity_at(&self, elapsed: Duration) -> Option<OpacityValue> {
    let (start, end) =
      (self.start_opacity.as_ref()?, self.target_opacity.as_ref()?);

    let Some(motion) = self.spring.filter(|_| !self.is_complete(elapsed))
    else {
      let eased_progress = self.easing.apply(self.progress(elapsed));
      return Some(start.interpolate(end, eased_progress));
    };

    let state = motion
      .spring
      .state_at(motion.opacity, elapsed.as_secs_f64());
    // LINT: Opacity is a fraction, well within `f32`.
    #[allow(clippy::cast_possible_truncation)]
    let offset = (state.displacement / ALPHA_STEPS) as f32;

    // A bouncing fade would otherwise pass fully clear or fully opaque.
    Some(OpacityValue((end.0 + offset).clamp(0.0, 1.0)))
  }

  /// Returns the velocity of each edge at `elapsed`, in px per second.
  ///
  /// Zero once the motion has completed.
  fn velocity_at(&self, elapsed: Duration) -> EdgeVelocity {
    if self.is_complete(elapsed) {
      return [0.0; 4];
    }

    if let Some(motion) = self.spring {
      let time = elapsed.as_secs_f64();
      return motion
        .edges
        .map(|edge| motion.spring.state_at(edge, time).velocity);
    }

    // A curve's velocity is its slope, sampled across a short span of
    // progress, times the distance each edge travels.
    let span = 0.001;
    let progress = self.progress(elapsed);
    let (before, after) =
      ((progress - span).max(0.0), (progress + span).min(1.0));
    let slope = f64::from(
      (self.easing.apply(after) - self.easing.apply(before))
        / (after - before),
    ) / self.duration.as_secs_f64();

    let (start, target) =
      (edges(&self.start_rect), edges(&self.target_rect));
    std::array::from_fn(|index| {
      f64::from(target[index] - start[index]) * slope
    })
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

/// Draws a window's companions during its native motion and holds them
/// through their reveal. See `CompanionOverlay`.
struct Decoration {
  overlay: CompanionOverlay,
  frame_rate: u32,
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

  /// Companion-only overlays of windows in native motion, keyed by window
  /// ID. Kept apart from `windows`: the real window stays visible and
  /// owns its motion, so none of the cover rules apply.
  decorations: HashMap<Uuid, Decoration>,

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

  /// When `tick_update` last submitted overlay frames.
  last_submitted: Option<Instant>,

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
      decorations: HashMap::new(),
      pending_captures: HashMap::new(),
      context: None,
      clock: None,
      pending_starts: Vec::new(),
      last_submitted: None,
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
  ///
  /// Tick-driven motion completes at the last sampled tick, not by wall
  /// clock. A commit between ticks would otherwise hand off before the
  /// final frame was drawn, and the cover would freeze a remainder short.
  pub fn is_complete(&self, id: &Uuid) -> bool {
    self.running.get(id).is_some_and(|motion| {
      if AnimationWindow::SELF_ANIMATING {
        motion.completed.load(Ordering::Acquire)
      } else {
        self
          .animations
          .get(id)
          .is_some_and(|spec| spec.is_complete(motion.sampled))
      }
    })
  }

  /// Whether a workspace slide, running or complete, owns the window.
  pub fn owns_slide(&self, id: &Uuid) -> bool {
    self.animations.get(id).is_some_and(|spec| spec.is_slide)
  }

  /// Returns the move effect for a window whose completed slide ended
  /// away from its current tile.
  pub fn deferred_move_effect<'a>(
    &self,
    id: &Uuid,
    tile: &Rect,
    config: &'a UserConfig,
  ) -> Option<&'a AnimationEffectConfig> {
    let move_config = config.value.animations.window_move.as_ref()?;
    let spec = self.animations.get(id)?;
    exceeds_threshold(&spec.target_rect, tile, move_config)
      .then_some(&move_config.effect)
  }

  /// Whether the frame clock runs for this window's presentation, so
  /// every tick already commits it.
  pub fn drives_frames(&self, id: &Uuid) -> bool {
    self.windows.contains_key(id)
      || self.decorations.contains_key(id)
      || self
        .animations
        .get(id)
        .is_some_and(|spec| matches!(spec.target, MotionTarget::Native(_)))
  }

  /// Returns where a cover holds its real window, if it does.
  pub fn held_frame(&self, id: &Uuid) -> Option<Rect> {
    self
      .animations
      .get(id)
      .filter(|spec| spec.holds_source)
      .map(|spec| spec.target_rect.clone())
  }

  /// Returns where a window's cover ends: its running motion's target, or
  /// where a finished cover stands.
  pub fn cover_target(&self, id: &Uuid) -> Option<Rect> {
    let overlay = self.windows.get(id)?;
    Some(
      self
        .animations
        .get(id)
        .filter(|spec| matches!(spec.target, MotionTarget::Overlay))
        .map_or_else(
          || overlay.frame.clone(),
          |spec| spec.target_rect.clone(),
        ),
    )
  }

  /// Whether any overlay or motion still depends on the frame clock.
  pub fn has_presentations(&self) -> bool {
    !self.windows.is_empty()
      || !self.animations.is_empty()
      || !self.decorations.is_empty()
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

  /// Returns the per-edge velocity of a window's current motion, so a
  /// replacement carries it on.
  ///
  /// Zero without motion. A prepared motion that is not yet released
  /// reports its starting velocity, which it would have set off with.
  ///
  /// # Platform-specific
  ///
  /// - macOS: the compositor runs released motion, so elapsed time is read
  ///   from the wall clock rather than the last sampled tick.
  fn carried_velocity(&self, id: &Uuid) -> EdgeVelocity {
    let Some(spec) = self.animations.get(id) else {
      return [0.0; 4];
    };
    let elapsed = self.running.get(id).map_or(Duration::ZERO, |motion| {
      if AnimationWindow::SELF_ANIMATING {
        motion.started.elapsed()
      } else {
        motion.sampled
      }
    });

    spec.velocity_at(elapsed)
  }

  /// Returns when a window's current motion was last drawn, so a
  /// replacement started from that frame continues its clock.
  ///
  /// Now without motion, or when the last draw is older than
  /// `MAX_CONTINUATION`.
  ///
  /// # Platform-specific
  ///
  /// - macOS: the compositor draws continuously, so this is always now.
  fn last_drawn(&self, id: &Uuid) -> Instant {
    /// A draw older than this means the frame clock stalled. Continuing
    /// from it would start the replacement far along its path.
    const MAX_CONTINUATION: Duration = Duration::from_millis(100);

    let now = Instant::now();
    match self.running.get(id) {
      Some(motion) if !AnimationWindow::SELF_ANIMATING => {
        let drawn = motion.started + motion.sampled;
        if now.saturating_duration_since(drawn) > MAX_CONTINUATION {
          now
        } else {
          drawn.min(now)
        }
      }
      _ => now,
    }
  }

  /// Returns where a window's cover is presented, if it has one, and the
  /// velocity its motion carries.
  ///
  /// Only a cover that is still presented has motion to carry on, so the
  /// velocity is zero without one.
  fn cover_motion(
    &self,
    id: &Uuid,
  ) -> anyhow::Result<(Option<Rect>, EdgeVelocity)> {
    let Some(overlay) = self.windows.get(id) else {
      return Ok((None, [0.0; 4]));
    };
    let frame = overlay
      .window
      .current_frame()?
      .unwrap_or_else(|| overlay.frame.clone());
    Ok((Some(frame), self.carried_velocity(id)))
  }

  /// Whether a window's cover is on screen.
  ///
  /// A cover whose motion is held for release may not be composed yet, so
  /// it does not count.
  fn is_presented(&self, id: &Uuid) -> bool {
    self.windows.contains_key(id)
      && (self.running.contains_key(id)
        || !self.animations.contains_key(id))
  }

  /// Prepares visible motion without creating a cover.
  ///
  /// The window holds its start frame until the coordinator releases its
  /// batch, so it starts together with any overlays prepared beside it.
  /// A retarget starts from `start`, the observed frame, with the
  /// velocity of the motion it replaces.
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
      self.carried_velocity(&id),
    );
    spec.target = MotionTarget::Native(frame_rate);
    self.running.remove(&id);
    self.pending_starts.retain(|pending| *pending != id);
    self.pending_captures.remove(&id);
    self.failed_updates.remove(&id);
    self.animations.insert(id, spec);
    self.update_clock();
  }

  /// Whether retained visuals still cover a native source.
  pub fn has_overlay(&self, id: &Uuid) -> bool {
    self.windows.contains_key(id)
  }

  /// Starts or continues drawing a window's companions for its native
  /// motion.
  ///
  /// Called after `prepare_native`, with the same `start`. The overlay
  /// covers the motion's path grown by the companion margin. A retarget
  /// keeps the existing overlay and the companion rects it recorded when
  /// it was created, so the decoration does not jump; the overlay only
  /// grows.
  ///
  /// Returns whether the window is decorated. A window without companions
  /// costs one shared companion search and gets no overlay. Failures are
  /// logged and leave the window undecorated; they never affect its
  /// motion.
  pub fn decorate_native(
    &mut self,
    id: Uuid,
    window: &NativeWindow,
    start: &Rect,
    frame_rate: u32,
    dispatcher: &Dispatcher,
  ) -> bool {
    let Some(target) = self.native_target(&id) else {
      return false;
    };
    // The spring's path, not only its endpoints: an overshoot or a
    // carried velocity would otherwise draw the ring outside the overlay.
    let bounds = self.animations.get(&id).map_or_else(
      || start.union(target),
      |spec| spec.path_bounds().union(start),
    );
    #[cfg(target_os = "windows")]
    let bounds = bounds.inset(-COMPANION_MARGIN_PX);
    if let Some(decoration) = self.decorations.get_mut(&id) {
      decoration.frame_rate = frame_rate;
      let Err(err) = decoration.overlay.retarget(&bounds) else {
        self.update_clock();
        return true;
      };
      tracing::warn!(window = %id, "Companion overlay retarget failed: {err}");
      if let Err(err) = self.retire_decoration(&id) {
        tracing::warn!("Companion overlay cleanup failed: {err}");
      }
      return false;
    }
    match CompanionOverlay::new(window, &bounds, dispatcher) {
      Ok(Some(overlay)) => {
        self.decorations.insert(
          id,
          Decoration {
            overlay,
            frame_rate,
          },
        );
        self.update_clock();
        tracing::debug!(window = %id, "Companion overlay prepared.");
        true
      }
      Ok(None) => false,
      Err(err) => {
        tracing::warn!(window = %id, "Companion overlay failed: {err}");
        false
      }
    }
  }

  /// Draws a decorated window's companions against `frame`, the window
  /// rect just requested for it.
  ///
  /// Called straight after the request, so both reach the compositor
  /// together. Does nothing for an undecorated window. A failed update
  /// retires the decoration; the window's own motion continues.
  pub fn draw_decoration(&mut self, id: &Uuid, frame: &Rect) {
    let Some(decoration) = self.decorations.get(id) else {
      return;
    };
    if let Err(err) = decoration.overlay.update(frame) {
      tracing::warn!(window = %id, "Companion overlay update failed: {err}");
      if let Err(err) = self.retire_decoration(id) {
        tracing::warn!("Companion overlay cleanup failed: {err}");
      }
    }
  }

  /// Whether a companion overlay draws this window's companions.
  pub fn has_decoration(&self, id: &Uuid) -> bool {
    self.decorations.contains_key(id)
  }

  /// Whether every companion drawn by a window's companion overlay shows
  /// itself again. `true` without one.
  pub fn decoration_revealed(&self, id: &Uuid) -> bool {
    self
      .decorations
      .get(id)
      .is_none_or(|decoration| decoration.overlay.companions_revealed())
  }

  /// Destroys a window's companion overlay, if it has one.
  pub fn retire_decoration(&mut self, id: &Uuid) -> anyhow::Result<()> {
    let Some(mut decoration) = self.decorations.remove(id) else {
      return Ok(());
    };
    self.update_clock();
    tracing::debug!(window = %id, "Companion overlay retired.");
    decoration.overlay.destroy()?;
    Ok(())
  }

  /// Whether every companion drawn by a window's overlay shows itself
  /// again. `true` without an overlay or companions.
  pub fn companions_revealed(&self, id: &Uuid) -> bool {
    self
      .windows
      .get(id)
      .is_none_or(|overlay| overlay.window.companions_revealed())
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
      // Companions are drawn through the reveal hold, so the overlay keeps
      // the margin they reach into. Its path usually encloses the landing
      // frame already; resizing anyway moves its origin a compositor frame
      // before the thumbnail follows, and the window blinks at the reveal.
      #[cfg(target_os = "windows")]
      let bounds = rect.inset(-COMPANION_MARGIN_PX);
      #[cfg(not(target_os = "windows"))]
      let bounds = rect.clone();
      if !overlay.window.covers(&bounds) {
        overlay.window.resize(&bounds)?;
      }
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
  ///
  /// Also retires the window's companion overlay, which a released
  /// window no longer needs.
  pub fn retire_overlay(&mut self, id: &Uuid) -> anyhow::Result<()> {
    self.retire_decoration(id)?;
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
    self.last_submitted = Some(Instant::now());
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

  /// Submits a frame for running overlay motion if a frame period has
  /// passed since the last one.
  ///
  /// Called by the coordinator between windows of a long commit. Native
  /// calls for a few windows can outlast several frames, and without a
  /// tick in between every running cover stood still until the commit
  /// ended. Safe between windows: nothing here is mid-mutation, and the
  /// update only reads motion and writes overlay frames. Failed overlays
  /// are queued for the frame clock's own tick, as `tick_update` always
  /// does.
  pub fn tick_if_due(
    &mut self,
    dispatcher: &Dispatcher,
  ) -> anyhow::Result<()> {
    let Some((frame_rate, _)) = &self.clock else {
      return Ok(());
    };
    let period = Duration::from_secs(1) / (*frame_rate).max(1);
    if self
      .last_submitted
      .is_some_and(|submitted| submitted.elapsed() < period)
    {
      return Ok(());
    }
    self.tick_update(dispatcher)
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
    //    They get the hard cut a desktop normally gives them. Workspace
    //    slides exclude them too. The coordinator cuts the way out of
    //    fullscreen, which this check cannot see.
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
        AnimationTrigger::WindowSent,
        AnimationsConfig {
          window_close: Some(close_config),
          ..
        },
      ) => Some(close_config),
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
        // A slide owns its windows for its whole duration; the
        // coordinator holds a move asked for meanwhile until it ends.
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

        exceeds_threshold(prev_rect, target_rect, move_config)
          .then_some(&move_config.effect)
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
    // A capture that returns at once gains nothing from a thread.
    let results = if AnimationContext::CAPTURE_BLOCKS {
      std::thread::scope(|scope| {
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
      })
    } else {
      windows
        .iter()
        .map(|(id, window_id)| (id, Ok(context.capture_frame(*window_id))))
        .collect::<Vec<_>>()
    };

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

  /// Prepares an overlay and effect, preserving any retained capture.
  ///
  /// The plan's path decides where the animation begins. Without a start
  /// of its own it begins where the window is. A window whose cover is
  /// still up begins where the cover is presented, with the velocity of
  /// the motion it replaces, so a spring carries on rather than
  /// restarting from rest.
  ///
  /// Pass `MotionStart::Immediate` only for a concealed source. Returns
  /// how the motion actually starts: a cover that is not yet presented is
  /// always held, and the coordinator must release it. An immediate
  /// motion is running on return and needs no release.
  pub fn prepare_animation(
    &mut self,
    window: &WindowContainer,
    plan: &AnimationPlan,
    monitor_properties: &NativeMonitorProperties,
    dispatcher: &Dispatcher,
    start: MotionStart,
  ) -> anyhow::Result<MotionStart> {
    // The monitor's refresh rate, for platforms that pace the clock by
    // sleeping rather than by the compositor.
    let frame_rate = monitor_properties.refresh_rate.unwrap_or(60);
    let start = if self.is_presented(&window.id()) {
      start
    } else {
      MotionStart::Held
    };
    let continued_from = self.last_drawn(&window.id());
    let (presented_rect, velocity) = self.cover_motion(&window.id())?;
    let start_rect = presented_rect
      .or_else(|| plan.path.start.clone())
      .unwrap_or_else(|| window.native_properties().frame.clone());

    let mut animation = AnimationSpec::new(
      start_rect,
      plan.path.target.clone(),
      plan.path.opacity,
      plan.effect,
      plan.trigger.is_slide(),
      velocity,
    );
    animation.holds_source = plan.trigger == AnimationTrigger::WindowSent;

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
      let outer_rect = animation.path_bounds();
      // Companions such as border rings extend past the window's frame,
      // and the overlay clips their thumbnails to its own bounds.
      // Without the margin a ring loses its outer half, or all of a
      // band drawn wholly outside the frame, for the whole
      // animation.
      #[cfg(target_os = "windows")]
      let outer_rect = outer_rect.inset(-COMPANION_MARGIN_PX);

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
    // changes mid-flight, preserving the screenshot and z-order. A slide's
    // overlay already spans its monitor, so a reversal usually fits; the
    // resize is skipped then, sparing a round trip per window while no
    // frame is ticking, and the origin move that lands a frame before the
    // thumbnail follows it.
    if let Some(anim_window) = self.windows.get_mut(&window.id()) {
      if !anim_window.window.covers(&outer_rect) {
        anim_window.window.resize(&outer_rect)?;
      }
      anim_window.frame_rate = frame_rate;

      // A held replacement freezes the sampled presentation until its
      // batch is released. An immediate one on a tick-driven backend
      // already shows its start frame, and its next tick draws the new
      // motion. A compositor-driven one must first stop the running
      // animation, so the next begins where it is.
      if start == MotionStart::Held || AnimationWindow::SELF_ANIMATING {
        anim_window.window.stop_at(
          &animation.start_rect,
          animation.start_opacity.as_ref(),
        )?;
        anim_window.frame = animation.start_rect.clone();
      }
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
    if start == MotionStart::Immediate {
      self.start_now(window.id(), continued_from);
    }
    self.update_clock();
    Ok(start)
  }

  /// Starts a prepared motion at once, without waiting for its batch.
  ///
  /// `started` is when the motion it replaces was last drawn, which is
  /// where the replacement begins. The next tick then draws it as far
  /// along as the time since, rather than from rest. A failed launch is
  /// reported as a failed release is.
  fn start_now(&mut self, id: Uuid, started: Instant) {
    self.pending_starts.retain(|pending| *pending != id);
    let completed = Arc::new(AtomicBool::new(false));
    match self.launch(&id, &completed) {
      Ok(()) => self.run(id, started, completed),
      Err(err) => {
        tracing::warn!("Animation launch failed: {err}");
        self.failed_updates.insert(id);
      }
    }
  }

  /// Hands a prepared motion to whatever draws it.
  ///
  /// Nothing to do for native motion or on a tick-driven backend, where
  /// the frame clock draws every frame. Fails for an overlay motion
  /// without an overlay.
  fn launch(
    &self,
    id: &Uuid,
    completed: &Arc<AtomicBool>,
  ) -> anyhow::Result<()> {
    let anim = self.animations.get(id).context("No prepared motion.")?;
    if matches!(anim.target, MotionTarget::Native(_)) {
      return Ok(());
    }
    let overlay = self
      .windows
      .get(id)
      .context("Prepared animation has no overlay.")?;
    if !AnimationWindow::SELF_ANIMATING {
      return Ok(());
    }
    let completion = completed.clone();
    let tick_tx = self.tick_tx.clone();
    overlay
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
  }

  /// Gives a launched motion its clock, with its first frame at
  /// `started`.
  fn run(
    &mut self,
    id: Uuid,
    started: Instant,
    completed: Arc<AtomicBool>,
  ) {
    self.running.insert(
      id,
      RunningMotion {
        started,
        sampled: Duration::ZERO,
        completed,
      },
    );
  }

  /// Releases ready effects together, preserving later launches on
  /// failure.
  pub fn begin_pending(&mut self) {
    let mut launched = Vec::new();
    for window_id in std::mem::take(&mut self.pending_starts) {
      if !self.animations.contains_key(&window_id)
        || self.running.contains_key(&window_id)
      {
        continue;
      }
      let completed = Arc::new(AtomicBool::new(false));
      match self.launch(&window_id, &completed) {
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
    for (id, completed) in launched {
      self.run(id, started, completed);
    }
    self.update_clock();
  }

  /// Keeps compositor signals alive while any overlay awaits handoff.
  ///
  /// A companion overlay counts too: its reveal hold waits on compositor
  /// frames after the motion itself has ended.
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
      .chain(
        self
          .decorations
          .values()
          .map(|decoration| decoration.frame_rate),
      )
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
      WindowMoved, WindowOpened, WindowSent, WorkspaceEntering,
      WorkspaceLeaving,
    };
    let changes = [
      WindowChange::Opened,
      WindowChange::Sent,
      WindowChange::Placed,
    ];
    for direction in [SlideDirection::Left, SlideDirection::Right] {
      for change in changes {
        assert_eq!(
          AnimationTrigger::select(false, Some(direction), true, change),
          Some(WorkspaceEntering(direction))
        );
        assert_eq!(
          AnimationTrigger::select(false, Some(direction), false, change),
          Some(WorkspaceLeaving(direction))
        );
        for visible in [false, true] {
          assert!(AnimationTrigger::select(
            true,
            Some(direction),
            visible,
            change
          )
          .is_none());
        }
      }
    }
    assert_eq!(
      AnimationTrigger::select(false, None, true, WindowChange::Opened),
      Some(WindowOpened)
    );
    assert_eq!(
      AnimationTrigger::select(false, None, true, WindowChange::Placed),
      Some(WindowMoved)
    );
    assert_eq!(
      AnimationTrigger::select(false, None, false, WindowChange::Sent),
      Some(WindowSent)
    );
    for change in [WindowChange::Opened, WindowChange::Placed] {
      assert!(
        AnimationTrigger::select(false, None, false, change).is_none()
      );
    }
    for change in changes {
      for visible in [false, true] {
        assert!(
          AnimationTrigger::select(true, None, visible, change).is_none()
        );
      }
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
      decorations: HashMap::new(),
      pending_captures: HashMap::new(),
      context: None,
      clock: None,
      pending_starts: Vec::new(),
      last_submitted: None,
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
      [0.0; 4],
    )
  }

  /// Creates a spring effect with a perceptual duration and bounce.
  fn spring_effect(
    duration_ms: u32,
    bounce: f32,
  ) -> AnimationEffectConfig {
    AnimationEffectConfig {
      duration_ms,
      easing: EasingFunction::Spring,
      bounce,
      ..AnimationEffectConfig::default()
    }
  }

  /// A spring runs until it settles and then lands exactly.
  #[test]
  fn spring_completes_once_settled() {
    let effect = spring_effect(250, 0.1);
    let spec = AnimationSpec::new(
      Rect::from_xy(0, 0, 400, 300),
      Rect::from_xy(1200, 200, 600, 400),
      None,
      &effect,
      false,
      [0.0; 4],
    );

    assert!(spec.duration > Duration::from_millis(250));
    assert!(!spec.is_complete(Duration::from_millis(250)));
    assert!(spec.is_complete(spec.duration));
    assert_eq!(spec.rect_at(Duration::ZERO), spec.start_rect);
    assert_eq!(spec.rect_at(spec.duration), spec.target_rect);

    // Just before settling, every edge is already within half a pixel.
    let almost =
      spec.rect_at(spec.duration.saturating_sub(Duration::from_millis(1)));
    for (edge, target) in
      edges(&almost).into_iter().zip(edges(&spec.target_rect))
    {
      assert!((edge - target).abs() <= 1, "{almost:?}");
    }

    // A bouncing spring passes its target before settling.
    let overshoot = (1..100)
      .map(|step| spec.rect_at(spec.duration * step / 100).left)
      .max()
      .unwrap_or_default();
    assert!(overshoot > spec.target_rect.left);
  }

  /// A fade without travel still lasts until its opacity settles.
  #[test]
  fn spring_fade_settles_its_opacity() {
    let effect = spring_effect(300, 0.15);
    let frame = Rect::from_xy(100, 100, 800, 600);
    let spec = AnimationSpec::new(
      frame.clone(),
      frame,
      Some((OpacityValue(1.0), OpacityValue(0.0))),
      &effect,
      false,
      [0.0; 4],
    );

    assert!(spec.duration > Duration::from_millis(300));
    let opacity = |elapsed| spec.opacity_at(elapsed).map(|value| value.0);
    assert_eq!(opacity(Duration::ZERO), Some(1.0));
    assert_eq!(opacity(spec.duration), Some(0.0));
    for step in 0..=100 {
      let value = opacity(spec.duration * step / 100).unwrap_or(-1.0);
      assert!((0.0..=1.0).contains(&value));
    }
  }

  /// A curve's velocity is its slope over the travel.
  #[test]
  fn curve_velocity_follows_its_slope() {
    let effect = AnimationEffectConfig {
      duration_ms: 200,
      easing: EasingFunction::Linear,
      ..AnimationEffectConfig::default()
    };
    let spec = AnimationSpec::new(
      Rect::from_xy(0, 0, 100, 100),
      Rect::from_xy(400, 0, 100, 100),
      None,
      &effect,
      false,
      [0.0; 4],
    );

    let velocity = spec.velocity_at(Duration::from_millis(100));
    assert!((velocity[0] - 2000.0).abs() < 1.0, "{velocity:?}");
    assert!(velocity[1].abs() < f64::EPSILON);
    assert_eq!(spec.velocity_at(spec.duration), [0.0; 4]);
  }

  /// A retarget mid-flight carries each edge's velocity into the new
  /// spring, so the window neither stops nor jerks.
  #[test]
  fn spring_retarget_keeps_velocity() {
    if AnimationWindow::SELF_ANIMATING {
      return;
    }
    let mut manager = manager_without_overlays();
    let id = Uuid::new_v4();
    let effect = spring_effect(250, 0.1);
    let plan = move_plan(&effect, Rect::from_xy(1600, 200, 800, 600));
    manager.prepare_native(id, &plan, Rect::from_xy(0, 0, 800, 600), 60);
    manager.release_animation(&id);
    manager.begin_pending();

    let started = manager.running[&id].started;
    manager.sample_frames(started + Duration::from_millis(60));
    let before = manager.carried_velocity(&id);
    let observed = manager.native_frame(&id).expect("Native frame.");
    assert!(before[0] > 1000.0, "{before:?}");

    let retarget = move_plan(&effect, Rect::from_xy(2400, 400, 1000, 700));
    manager.prepare_native(id, &retarget, observed.clone(), 60);
    let spec = &manager.animations[&id];

    assert_eq!(spec.rect_at(Duration::ZERO), observed);
    let after = spec.velocity_at(Duration::ZERO);
    for (old, new) in before.iter().zip(after) {
      assert!((old - new).abs() < 1e-9, "{before:?} vs {after:?}");
    }
  }

  /// Starts a leaving slide's overlay motion without platform resources,
  /// and samples it `elapsed` after it started.
  fn sampled_slide(
    manager: &mut AnimationManager,
    id: Uuid,
    effect: &AnimationEffectConfig,
    elapsed: Duration,
  ) -> Instant {
    let spec = AnimationSpec::new(
      Rect::from_xy(0, 0, 800, 600),
      Rect::from_xy(-3440, 0, 800, 600),
      None,
      effect,
      true,
      [0.0; 4],
    );
    manager.animations.insert(id, spec);
    let started = Instant::now()
      .checked_sub(Duration::from_millis(50))
      .expect("Clock predates the test.");
    manager.run(id, started, Arc::new(AtomicBool::new(false)));
    manager.sample_frames(started + elapsed);
    started
  }

  /// A covered retarget continues from the last drawn frame, with its
  /// velocity, on the replaced motion's clock. It runs without a release,
  /// and the next frame moves as far as the carried velocity takes it
  /// rather than standing still.
  #[test]
  fn covered_retarget_continues_without_release() {
    if AnimationWindow::SELF_ANIMATING {
      return;
    }
    let mut manager = manager_without_overlays();
    let id = Uuid::new_v4();
    let effect = spring_effect(200, 0.0);
    let elapsed = Duration::from_millis(30);
    let started = sampled_slide(&mut manager, id, &effect, elapsed);
    let drawn = manager.animations[&id].rect_at(elapsed);
    let before = manager.carried_velocity(&id);
    assert!(before[0] < -1000.0, "{before:?}");

    // What `prepare_animation` does for `MotionStart::Immediate`.
    let continued_from = manager.last_drawn(&id);
    assert_eq!(continued_from, started + elapsed);
    let reverse = AnimationSpec::new(
      drawn.clone(),
      Rect::from_xy(0, 0, 800, 600),
      None,
      &effect,
      true,
      before,
    );
    manager.running.remove(&id);
    manager.animations.insert(id, reverse);
    manager.run(id, continued_from, Arc::new(AtomicBool::new(false)));

    // Running at once; a release and a batch start leave it alone.
    assert!(manager.is_running(&id));
    manager.release_animation(&id);
    assert_eq!(manager.pending_starts, Vec::<Uuid>::new());
    manager.begin_pending();
    assert_eq!(manager.running[&id].started, continued_from);

    // One 144Hz frame later it has carried on at the old velocity.
    let frame = Duration::from_micros(6944);
    manager.sample_frames(continued_from + frame);
    let spec = &manager.animations[&id];
    assert_eq!(spec.rect_at(Duration::ZERO), drawn);
    let after = spec.velocity_at(Duration::ZERO);
    for (old, new) in before.iter().zip(after) {
      assert!((old - new).abs() < 1e-9, "{before:?} vs {after:?}");
    }
    let sampled = manager.running[&id].sampled;
    let moved = f64::from(spec.rect_at(sampled).left - drawn.left);
    let expected = before[0] * frame.as_secs_f64();
    assert!(moved < expected / 2.0, "{moved} vs {expected}");
  }

  /// A continuation never reaches back past a stalled clock, and a window
  /// without motion continues from now.
  #[test]
  fn continuation_starts_no_earlier_than_a_recent_draw() {
    if AnimationWindow::SELF_ANIMATING {
      return;
    }
    let mut manager = manager_without_overlays();
    let id = Uuid::new_v4();
    let before = Instant::now();
    assert!(manager.last_drawn(&id) >= before);

    let effect = spring_effect(200, 0.0);
    let started =
      sampled_slide(&mut manager, id, &effect, Duration::from_millis(10));
    assert_eq!(
      manager.last_drawn(&id),
      started + Duration::from_millis(10)
    );

    manager.running.get_mut(&id).expect("Running.").started =
      Instant::now()
        .checked_sub(Duration::from_secs(1))
        .expect("Clock predates the test.");
    let stalled = Instant::now();
    assert!(manager.last_drawn(&id) >= stalled);
  }

  /// An immediate start without a cover to draw it is reported like a
  /// failed release, and never runs.
  #[test]
  fn immediate_start_without_overlay_fails_like_a_release() {
    let mut manager = manager_without_overlays();
    let id = Uuid::new_v4();
    manager.animations.insert(id, test_spec());
    manager.start_now(id, Instant::now());
    assert!(!manager.is_running(&id));
    assert_eq!(manager.take_failures(), HashSet::from([id]));
    assert_eq!(manager.pending_starts, Vec::<Uuid>::new());
  }

  /// A retarget that carries velocity away from its new target travels
  /// past its start, and the overlay's bounds must include that stretch.
  #[test]
  fn spring_path_bounds_include_carried_overshoot() {
    if AnimationWindow::SELF_ANIMATING {
      return;
    }
    let mut manager = manager_without_overlays();
    let id = Uuid::new_v4();
    let effect = spring_effect(180, 0.0);
    let plan = move_plan(&effect, Rect::from_xy(-3000, 0, 800, 600));
    manager.prepare_native(id, &plan, Rect::from_xy(0, 0, 800, 600), 60);
    manager.release_animation(&id);
    manager.begin_pending();
    let started = manager.running[&id].started;
    manager.sample_frames(started + Duration::from_millis(30));
    let observed = manager.native_frame(&id).expect("Native frame.");

    // Reverse while moving left at speed.
    let reverse = move_plan(&effect, Rect::from_xy(0, 0, 800, 600));
    manager.prepare_native(id, &reverse, observed.clone(), 60);
    let spec = &manager.animations[&id];
    let bounds = spec.path_bounds();
    let leftmost = (0..=64)
      .map(|step| spec.rect_at(spec.duration * step / 64).left)
      .min()
      .expect("Samples.");

    assert!(leftmost < observed.left, "{leftmost} vs {observed:?}");
    assert!(bounds.left <= leftmost, "{bounds:?} vs {leftmost}");
    assert!(bounds.contains_rect(&observed));
    assert!(bounds.contains_rect(&Rect::from_xy(0, 0, 800, 600)));
  }

  /// A settled spring carries no velocity into a retarget.
  #[test]
  fn finished_motion_carries_no_velocity() {
    if AnimationWindow::SELF_ANIMATING {
      return;
    }
    let mut manager = manager_without_overlays();
    let id = Uuid::new_v4();
    let effect = spring_effect(250, 0.0);
    let plan = move_plan(&effect, Rect::from_xy(900, 0, 800, 600));
    manager.prepare_native(id, &plan, Rect::from_xy(0, 0, 800, 600), 60);
    manager.release_animation(&id);
    manager.begin_pending();

    let started = manager.running[&id].started;
    let duration = manager.animations[&id].duration;
    manager.sample_frames(started + duration);
    assert_eq!(manager.carried_velocity(&id), [0.0; 4]);
    assert!(manager.is_complete(&id));
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
    assert!(manager.windows.is_empty());
    assert!(manager.context.is_none());
    assert_eq!(manager.clock.as_ref().map(|(rate, _)| *rate), Some(144));
    // Preparation alone never starts motion; its batch releases it.
    manager.begin_pending();
    assert!(!manager.is_running(&id));
    manager.release_animation(&id);
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
    manager.release_animation(&id);
    manager.begin_pending();
    let observed = Rect::from_xy(37, 14, 420, 310);
    let replacement =
      move_plan(&effect, Rect::from_xy(-500, -200, 300, 200));
    manager.prepare_native(id, &replacement, observed.clone(), 120);
    assert_eq!(manager.native_frame(&id), Some(observed));
    assert_eq!(manager.native_target(&id), Some(&replacement.path.target));
    assert!(!manager.is_running(&id));
    assert_eq!(manager.pending_starts, Vec::<Uuid>::new());
    manager.release_animation(&id);
    assert_eq!(manager.pending_starts, vec![id]);
    manager.finish_animation(&id).expect("Cancellation.");
    manager.begin_pending();
    assert!(!manager.is_animating(&id));
    assert!(manager.clock.is_none());
  }

  /// Tick-driven motion completes only once a tick has sampled its end.
  #[test]
  fn completion_follows_the_sampled_tick() {
    if AnimationWindow::SELF_ANIMATING {
      return;
    }
    let mut manager = manager_without_overlays();
    let id = Uuid::new_v4();
    let spec = test_spec();
    let duration = spec.duration;
    manager.animations.insert(id, spec);
    let started = Instant::now()
      .checked_sub(duration * 2)
      .expect("Clock predates the test.");
    manager.running.insert(
      id,
      RunningMotion {
        started,
        sampled: Duration::ZERO,
        completed: Arc::new(AtomicBool::new(false)),
      },
    );
    assert!(!manager.is_complete(&id));
    manager.sample_frames(started + duration);
    assert!(manager.is_complete(&id));
  }

  /// A slide owns its window until it is handed off.
  #[test]
  fn slide_ownership_follows_the_spec() {
    let mut manager = manager_without_overlays();
    let id = Uuid::new_v4();
    manager.animations.insert(id, test_spec());
    assert!(!manager.owns_slide(&id));
    let mut slide = test_spec();
    slide.is_slide = true;
    manager.animations.insert(id, slide);
    assert!(manager.owns_slide(&id));
    assert!(manager.has_presentations());
    manager.finish_animation(&id).expect("Handoff.");
    assert!(!manager.owns_slide(&id));
    assert!(!manager.has_presentations());
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

  /// A sent window fades in place and never slides or moves natively.
  #[test]
  fn sent_window_fades_where_it_stands() {
    let frame = Rect::from_xy(100, 100, 1000, 500);
    let monitor = Rect::from_xy(0, 0, 3440, 1440);

    let path = AnimationTrigger::WindowSent.path(&frame, &monitor);

    assert_eq!(path.start, Some(frame.clone()));
    assert_eq!(path.target, frame);
    assert_eq!(path.opacity, Some((OpacityValue(1.0), OpacityValue(0.0))));
    assert!(!AnimationTrigger::WindowSent.is_slide());
    assert!(!AnimationTrigger::WindowSent.uses_native());
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
