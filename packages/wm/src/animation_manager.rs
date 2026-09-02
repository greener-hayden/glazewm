use std::{
  collections::{HashMap, HashSet},
  time::{Duration, Instant},
};

use anyhow::Context;
use tokio::sync::mpsc;
use uuid::Uuid;
#[cfg(target_os = "windows")]
use wm_common::DisplayState;
use wm_common::{AnimationEffectConfig, AnimationsConfig, WindowState};
#[cfg(target_os = "macos")]
use wm_platform::DispatcherExtMacOs;
#[cfg(target_os = "windows")]
use wm_platform::NativeWindowWindowsExt;
use wm_platform::{
  AnimationCapture, AnimationContext, AnimationWindow, Dispatcher,
  EasingFunction, FrameClock, OpacityValue, Rect, WindowId,
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

impl AnimationTrigger {
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

/// State of an individual window animation.
///
/// A window corresponds to a maximum of one [`WindowAnimationState`] at a
/// time.
#[derive(Clone, Debug)]
struct WindowAnimationState {
  start_time: Instant,
  duration: Duration,
  easing: EasingFunction,

  /// Tick rate to fall back to where the platform has no frame clock.
  frame_rate: u32,

  /// Start and target positions for the animation.
  start_rect: Rect,
  target_rect: Rect,

  /// Whether the animation is part of a workspace slide. A slide
  /// animation is never interrupted by a non-slide redraw, which would
  /// otherwise restart a leaving window's travel mid-flight when an
  /// unrelated `wm-redraw` lands during the slide.
  is_slide: bool,

  /// Start and target opacity for the animation, or `None` if no opacity
  /// animation is active.
  start_opacity: Option<OpacityValue>,
  target_opacity: Option<OpacityValue>,
}

impl WindowAnimationState {
  /// Creates a new animation between two rects, and optionally between
  /// two opacities.
  fn new(
    start_rect: Rect,
    target_rect: Rect,
    opacity: Option<(OpacityValue, OpacityValue)>,
    config: &AnimationEffectConfig,
    frame_rate: u32,
    is_slide: bool,
  ) -> Self {
    let (start_opacity, target_opacity) = opacity.unzip();

    Self {
      start_time: Instant::now(),
      duration: Duration::from_millis(u64::from(config.duration_ms)),
      frame_rate,
      easing: config.easing.clone(),
      start_rect,
      target_rect,
      is_slide,
      start_opacity,
      target_opacity,
    }
  }

  /// Returns the normalized animation progress in `[0.0, 1.0]`.
  fn progress(&self) -> f32 {
    let elapsed = self.start_time.elapsed();

    if elapsed >= self.duration {
      1.0
    } else {
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
    let (tick_tx, tick_rx) = mpsc::unbounded_channel();

    Self {
      animations: HashMap::new(),
      tick_tx,
      tick_rx,
      windows: HashMap::new(),
      pending_captures: HashMap::new(),
      context: None,
      clock: None,
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

  /// Discards ticks that queued while the loop was busy.
  ///
  /// The clock keeps ticking through a long sync, and each queued tick
  /// would otherwise redraw the same frame again. One tick is one frame.
  pub fn drain_ticks(&mut self) {
    while self.tick_rx.try_recv().is_ok() {}
  }

  /// Destroys the animation window and clears animation state.
  pub fn destroy_animation(&mut self, window_id: &Uuid) {
    self.animations.remove(window_id);
    self.pending_captures.remove(window_id);
    self.update_clock();

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
  /// `animating_windows` holds the windows with a running animation,
  /// keyed by ID, so their real frames can follow the overlays.
  pub fn tick_update(
    &mut self,
    dispatcher: &Dispatcher,
    // LINT: `animating_windows` is only used on Windows.
    #[cfg_attr(not(target_os = "windows"), allow(unused_variables))]
    animating_windows: &HashMap<Uuid, WindowContainer>,
  ) -> anyhow::Result<()> {
    if self.animations.is_empty() {
      return Ok(());
    }

    let rects = self
      .animations
      .iter()
      .filter(|(_, anim)| !anim.is_complete())
      .map(|(id, anim)| (*id, anim.current_rect(), anim.current_opacity()))
      .collect::<Vec<_>>();

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

            if let Err(err) = anim_window.update(rect, opacity.as_ref()) {
              tracing::warn!("Failed to update animation window: {err}");
            }
          }
        },
        dispatcher,
      )
      .context("Animation update failed.")?;

    // After the overlays commit, keep the real windows under them. They
    // are transparent, so this is invisible; what it buys is that
    // trackers watching the window's bounds (mover-borders) follow the
    // animation's motion instead of jumping to the endpoint.
    #[cfg(target_os = "windows")]
    for (id, rect, _) in &rects {
      let Some(window) = animating_windows.get(id) else {
        continue;
      };

      // Only a shown window follows. A hidden one is cloaked or parked
      // in the corner, and pulling it out of the corner opaque would put
      // it on screen under the overlay.
      if !matches!(
        window.display_state(),
        DisplayState::Showing | DisplayState::Shown
      ) {
        continue;
      }

      // Only a pure translation follows. A size change would resize the
      // app every tick, which games in particular resent.
      let is_translation = self.animations.get(id).is_some_and(|anim| {
        anim.start_rect.width() == anim.target_rect.width()
          && anim.start_rect.height() == anim.target_rect.height()
      });

      if !is_translation {
        continue;
      }

      // Land on the frame the completing redraw will use, borders
      // included, so the handover does not jump by the border delta.
      let result = window.total_border_delta().and_then(|delta| {
        let frame = rect.apply_delta(&delta, None);
        Ok(window.native().set_position_async(&frame)?)
      });

      if let Err(err) = result {
        tracing::warn!("Failed to move window under animation: {err}");
      }
    }

    Ok(())
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
    //  - The window is hidden in the corner, but not animating. Safeguards
    //    against race condition where window finished an animation, but
    //    hasn't been moved to the real window position yet.
    if window.native_properties().is_minimized
      || matches!(window.state(), WindowState::Fullscreen(_))
      || (window.native_properties().is_maximized
        && cfg!(target_os = "macos"))
      || (!self.is_animating(&window.id())
        && window.is_in_corner(&monitor_properties.working_area))
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

  /// Starts a new animation, or extends an existing animation.
  ///
  /// The plan's path decides where the animation begins. Without a start
  /// of its own it begins where the window is, or where its running
  /// animation has got to.
  pub fn start_animation(
    &mut self,
    window: &WindowContainer,
    plan: &AnimationPlan,
    monitor_properties: &NativeMonitorProperties,
    dispatcher: &Dispatcher,
  ) -> anyhow::Result<()> {
    let existing_animation = self.animations.get(&window.id());

    // The monitor's refresh rate, for platforms that pace the clock by
    // sleeping rather than by the compositor.
    let frame_rate = monitor_properties.refresh_rate.unwrap_or(60);

    let start_rect = plan.path.start.clone().unwrap_or_else(|| {
      existing_animation.map_or_else(
        || window.native_properties().frame.clone(),
        WindowAnimationState::current_rect,
      )
    });

    let animation = WindowAnimationState::new(
      start_rect,
      plan.path.target.clone(),
      plan.path.opacity,
      plan.effect,
      frame_rate,
      plan.trigger.is_slide(),
    );

    self.animations.insert(window.id(), animation.clone());

    // On macOS, windows cannot span across multiple displays when
    // "Displays have separate Spaces" is enabled. Attempting to position a
    // window beyond the display bounds causes it to wrap around on the
    // same display. We therefore crop the animation to only be shown on
    // the source display.
    let outer_rect = {
      let outer_rect = animation.start_rect.union(&animation.target_rect);

      #[cfg(target_os = "macos")]
      if self.displays_have_separate_spaces {
        let display_bounds =
          dispatcher.nearest_display(&window.native())?.bounds()?;

        outer_rect.crop(&display_bounds)
      } else {
        outer_rect
      }

      #[cfg(not(target_os = "macos"))]
      outer_rect
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
      anim_window.resize(&outer_rect)?;

      // Immediately redraw the animation after resizing. The animation is
      // scaled relative to the window's frame, so it would otherwise be
      // incorrect until the next tick.
      context.transaction(
        || {
          anim_window.update(
            &animation.current_rect(),
            animation.current_opacity().as_ref(),
          )
        },
        dispatcher,
      )??;
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
        animation.current_opacity(),
        dispatcher,
      )?;

      self.windows.insert(window.id(), anim_window);
    }

    // Start the clock after the window has been created.
    // NOTE: Start times for animations will differ slightly between
    // windows within the same platform sync.
    if let Some(animation) = self.animations.get_mut(&window.id()) {
      animation.start_time = Instant::now();
    }

    self.update_clock();

    Ok(())
  }

  /// Starts, replaces, or stops the frame clock to match the animations.
  ///
  /// The clock's fallback rate is the highest refresh rate among the
  /// animated windows' monitors. A running clock is only replaced when
  /// that rate changes: animations of one sync start together, and a
  /// clock started per window would tick once per start and render a
  /// burst of duplicate frames.
  ///
  /// Called on animation start and completion.
  fn update_clock(&mut self) {
    let Some(frame_rate) =
      self.animations.values().map(|anim| anim.frame_rate).max()
    else {
      self.clock = None;
      return;
    };

    if self
      .clock
      .as_ref()
      .is_some_and(|(rate, _)| *rate == frame_rate)
    {
      return;
    }

    let tick_tx = self.tick_tx.clone();
    let clock =
      FrameClock::start(frame_rate, move || tick_tx.send(()).is_ok());

    self.clock = Some((frame_rate, clock));
  }
}

#[cfg(test)]
mod tests {
  use super::*;

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
