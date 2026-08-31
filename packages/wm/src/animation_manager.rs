use std::{
  collections::{HashMap, HashSet},
  time::{Duration, Instant},
};

use anyhow::Context;
use tokio::sync::mpsc;
use uuid::Uuid;
use wm_common::{AnimationEffectConfig, AnimationsConfig};
#[cfg(target_os = "macos")]
use wm_platform::DispatcherExtMacOs;
use wm_platform::{
  AnimationContext, AnimationWindow, Dispatcher, EasingFunction,
  OpacityValue, Rect,
};

use crate::{
  models::{NativeMonitorProperties, WindowContainer},
  traits::{CommonGetters, WindowGetters},
  user_config::UserConfig,
};

#[derive(Clone, Copy, Debug)]
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

/// State of an individual window animation.
///
/// A window corresponds to a maximum of one [`WindowAnimationState`] at a
/// time.
#[derive(Clone, Debug)]
struct WindowAnimationState {
  start_time: Instant,
  duration: Duration,
  easing: EasingFunction,

  /// Target frame rate for the animation.
  frame_rate: u32,

  /// Start and target positions for the animation.
  start_rect: Rect,
  target_rect: Rect,

  /// Start and target opacity for the animation, or `None` if no opacity
  /// animation is active.
  start_opacity: Option<OpacityValue>,
  target_opacity: Option<OpacityValue>,

  /// Whether this animation belongs to a workspace switch.
  ///
  /// A leaving window is travelling off screen, so a later sync measures
  /// it a whole monitor from the tile it would occupy and starts a
  /// second animation hauling it back — the outgoing workspace appears
  /// to slide into the incoming one before vanishing.
  is_workspace_slide: bool,
}

impl WindowAnimationState {
  /// Creates a new movement animation between two rects.
  fn new(
    start_rect: Rect,
    target_rect: Rect,
    config: &AnimationEffectConfig,
    frame_rate: u32,
    is_workspace_slide: bool,
  ) -> Self {
    Self {
      start_time: Instant::now(),
      duration: Duration::from_millis(u64::from(config.duration_ms)),
      frame_rate,
      easing: config.easing.clone(),
      start_rect,
      target_rect,
      start_opacity: None,
      target_opacity: None,
      is_workspace_slide,
    }
  }

  /// Returns the normalized animation progress in `[0.0, 1.0]`.
  fn progress(&self) -> f32 {
    let elapsed = self.start_time.elapsed();

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
  // LINT: Progress is clamped to [0.0, 1.0], so exact comparison is safe.
  #[allow(clippy::float_cmp)]
  fn is_complete(&self) -> bool {
    self.progress() == 1.0
  }

  /// Returns the interpolated rect at the current animation progress.
  fn current_rect(&self) -> Rect {
    let eased_progress = self.easing.apply(self.progress());
    self
      .start_rect
      .interpolate(&self.target_rect, eased_progress)
  }

  /// Returns the interpolated opacity at the current animation progress,
  /// or `None` if no opacity animation is active.
  fn current_opacity(&self) -> Option<OpacityValue> {
    let (start, end) =
      (self.start_opacity.as_ref()?, self.target_opacity.as_ref()?);

    let eased_progress = self.easing.apply(self.progress());
    Some(start.interpolate(end, eased_progress))
  }
}

/// Manages animations for all windows.
pub struct AnimationManager {
  /// Active animations keyed by window ID.
  animations: HashMap<Uuid, WindowAnimationState>,

  /// Sender for animation tick events.
  tick_tx: mpsc::UnboundedSender<()>,

  /// Receiver for animation tick events.
  pub tick_rx: mpsc::UnboundedReceiver<()>,

  /// Per-window overlay windows keyed by window ID.
  windows: HashMap<Uuid, AnimationWindow>,

  /// Shared GPU context for animation overlay windows. Lazily
  /// initialized on the first animation.
  context: Option<AnimationContext>,

  /// Handle to the running tick task, if any.
  tick_task: Option<tokio::task::JoinHandle<()>>,

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
  pub fn new(
    // LINT: `dispatcher` is only used on macOS.
    #[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
    dispatcher: &Dispatcher,
  ) -> Self {
    let (tick_tx, tick_rx) = mpsc::unbounded_channel();

    Self {
      animations: HashMap::new(),
      tick_tx,
      tick_rx,
      windows: HashMap::new(),
      context: None,
      tick_task: None,
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

  /// Gets the window IDs of animations that have completed.
  pub fn completed_ids(&self) -> HashSet<Uuid> {
    self
      .animations
      .iter()
      .filter(|(_, anim)| anim.is_complete())
      .map(|(id, _)| *id)
      .collect::<HashSet<_>>()
  }

  /// Destroys the animation window and clears animation state.
  pub fn destroy_animation(&mut self, window_id: &Uuid) {
    self.animations.remove(window_id);
    self.update_tick_rate();

    if let Some(anim_window) = self.windows.remove(window_id) {
      std::thread::spawn(|| {
        // Briefly keep animation windows up to hide flicker during sync.
        std::thread::sleep(std::time::Duration::from_millis(20));
        if let Err(err) = anim_window.destroy() {
          tracing::warn!("Failed to destroy animation window: {err}");
        }
      });
    }
  }

  /// Updates all active animations during a single tick.
  ///
  /// Updates get batched into a single compositor transaction.
  ///
  /// Does nothing where the platform animates for itself: the compositor
  /// is already producing frames, and the transaction below would put a
  /// synchronous hop to the event loop thread on every one of them. Ticks
  /// still fire, but only so `completed_ids` can drive the handover.
  pub fn tick_update(
    &mut self,
    dispatcher: &Dispatcher,
  ) -> anyhow::Result<()> {
    if AnimationWindow::SELF_ANIMATING || self.animations.is_empty() {
      return Ok(());
    }

    self
      .context
      .as_ref()
      .context("Animation context not initialized.")?
      .transaction(
        || {
          for (id, anim) in &self.animations {
            // A completed animation still needs its final frame drawn.
            // Completion falls between ticks, so skipping it leaves the
            // overlay up to a frame short of the target while the
            // handover puts the real window at full travel — two copies,
            // offset by the remainder, for as long as the overlay lives.
            if let Some(anim_window) = self.windows.get(id) {
              anim_window.update(
                &anim.current_rect(),
                anim.current_opacity().as_ref(),
              )?;
            }
          }
          anyhow::Ok(())
        },
        dispatcher,
      )?
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
        // A workspace slide owns its window until it completes. Its
        // target is off screen, so measuring against the tile the window
        // would otherwise occupy always clears the threshold.
        if self
          .animations
          .get(&window.id())
          .is_some_and(|anim| anim.is_workspace_slide)
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

  /// Starts a new animation, or extends an existing animation.
  #[allow(clippy::too_many_arguments)]
  /// `start_override` forces where the animation begins, for a window that
  /// is not currently where it should appear to come from. A workspace
  /// entering from off screen is the case that needs it: its real frame is
  /// already the tile it will occupy, so without an override there is
  /// nothing to travel.
  pub fn start_animation(
    &mut self,
    window: &WindowContainer,
    effect_config: &AnimationEffectConfig,
    target_rect: Rect,
    monitor_properties: &NativeMonitorProperties,
    dispatcher: &Dispatcher,
    start_override: Option<Rect>,
    is_workspace_slide: bool,
  ) -> anyhow::Result<()> {
    let existing_animation = self.animations.get(&window.id());

    // Sync the frame rate to the monitor's refresh rate. Since ticks are
    // skipped if the animation is behind, the frame rate is variable.
    let frame_rate = monitor_properties.refresh_rate.unwrap_or(60);

    let start_rect = start_override.unwrap_or_else(|| {
      existing_animation.map_or_else(
        || window.native_properties().frame.clone(),
        WindowAnimationState::current_rect,
      )
    });

    let animation = WindowAnimationState::new(
      start_rect,
      target_rect,
      effect_config,
      frame_rate,
      is_workspace_slide,
    );

    self.animations.insert(window.id(), animation.clone());

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
      if self.displays_have_separate_spaces || is_workspace_slide {
        let display_bounds =
          dispatcher.nearest_display(&window.native())?.bounds()?;

        outer_rect.crop(&display_bounds)
      } else {
        outer_rect
      }

      #[cfg(not(target_os = "macos"))]
      outer_rect
    };

    let context = match &self.context {
      Some(ctx) => ctx,
      None => self
        .context
        .get_or_insert(AnimationContext::new(dispatcher)?),
    };

    // Resize existing overlay to the new bounding box when the target
    // changes mid-flight, preserving the screenshot and z-order.
    if let Some(anim_window) = self.windows.get_mut(&window.id()) {
      anim_window.resize(&outer_rect)?;

      // Immediately redraw the animation after resizing. The animation is
      // scaled relative to the window's frame, so it would otherwise be
      // incorrect until the next tick.
      //
      // Skipped where the platform animates for itself: there is no next
      // tick to be wrong until, and pinning the layer to its current rect
      // here would fight the retarget issued below.
      if !AnimationWindow::SELF_ANIMATING {
        context.transaction(
          || {
            anim_window.update(
              &animation.current_rect(),
              animation.current_opacity().as_ref(),
            )
          },
          dispatcher,
        )??;
      }
    } else {
      let anim_window = AnimationWindow::new(
        context,
        &window.native(),
        &animation.start_rect,
        &outer_rect,
        animation.current_opacity(),
        dispatcher,
      )?;

      self.windows.insert(window.id(), anim_window);
    }

    if AnimationWindow::SELF_ANIMATING {
      // Queue rather than start: see `begin_pending`.
      self.pending_starts.push(window.id());
    } else if let Some(animation) = self.animations.get_mut(&window.id()) {
      animation.start_time = Instant::now();
    }

    self.update_tick_rate();

    Ok(())
  }

  /// Hands every animation prepared this sync to the compositor.
  ///
  /// Called once the sync has finished preparing them, so a workspace
  /// switch releases as one sheet instead of a window at a time.
  ///
  /// The clock starts here too: `is_complete` retires the animation and
  /// hands the real window back, so it must not lead the compositor, or
  /// the window is restored to its tile while its overlay is still
  /// travelling and both are on screen at once.
  pub fn begin_pending(&mut self) -> anyhow::Result<()> {
    for window_id in std::mem::take(&mut self.pending_starts) {
      let Some(anim) = self.animations.get(&window_id) else {
        continue;
      };

      if let Some(anim_window) = self.windows.get(&window_id) {
        anim_window.animate_to(
          &anim.target_rect,
          anim.duration,
          &anim.easing,
          anim.target_opacity.as_ref(),
        )?;
      }

      if let Some(anim) = self.animations.get_mut(&window_id) {
        anim.start_time = Instant::now();
      }
    }

    Ok(())
  }

  /// Spawns a task for emitting ticks at the target frame rate.
  ///
  /// Cancels existing tick task if there is one. The ticks are emitted at
  /// the highest frame rate among the animated windows.
  ///
  /// Called on animation start and completion.
  fn update_tick_rate(&mut self) {
    if let Some(handle) = self.tick_task.take() {
      handle.abort();
    }

    // Get the highest frame rate among the animated windows.
    let Some(frame_rate) =
      self.animations.values().map(|anim| anim.frame_rate).max()
    else {
      return;
    };

    // `1000 / 60` truncates to 16ms, which beats against a 60Hz vsync
    // and drops a frame roughly every 400ms. Guard against a 0Hz rate,
    // which some macOS displays report and which would panic here.
    let frame_rate = if frame_rate == 0 { 60 } else { frame_rate };

    let frame_time =
      Duration::from_nanos(1_000_000_000 / u64::from(frame_rate));
    let tick_tx = self.tick_tx.clone();

    self.tick_task = Some(tokio::spawn(async move {
      let mut interval = tokio::time::interval(frame_time);
      interval
        .set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

      loop {
        interval.tick().await;
        if tick_tx.send(()).is_err() {
          break;
        }
      }
    }));
  }
}
