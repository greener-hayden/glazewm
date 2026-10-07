use crate::{
  platform_impl, Dispatcher, NativeWindow, OpacityValue, Rect, WindowId,
};

/// The on-screen windows, listed at most once and shared.
///
/// Nothing is fetched on creation; the first query that needs the list
/// takes it, and every later query reuses it. Share one across the
/// captures of a batch, or the overlay checks of one placement pass, so
/// each pays for a single window-server round trip rather than one per
/// window. The list is a snapshot: it does not follow windows that move
/// after it was taken, so make a fresh one for each batch or pass.
///
/// # Platform-specific
///
/// - macOS: the list of every on-screen window, which companions are found
///   in.
/// - Windows: holds nothing; no query needs a window list.
#[derive(Default)]
pub struct OnScreenWindows {
  inner: platform_impl::OnScreenWindows,
}

impl OnScreenWindows {
  /// Creates a snapshot that has not yet listed any window.
  #[must_use]
  pub fn new() -> Self {
    Self::default()
  }
}

/// Shared context used by [`AnimationWindow`] instances. Holds GPU
/// resources that can be shared between animations.
///
/// Exposes a [`AnimationContext::transaction`] method for batching updates
/// across animation windows.
pub struct AnimationContext {
  inner: platform_impl::AnimationContext,
}

impl AnimationContext {
  /// Creates a new [`AnimationContext`].
  pub fn new(dispatcher: &Dispatcher) -> crate::Result<Self> {
    let inner = platform_impl::AnimationContext::new(dispatcher)?;
    Ok(Self { inner })
  }

  /// Whether [`AnimationContext::capture_frame`] does real work.
  ///
  /// When `false`, a capture returns at once and callers gain nothing
  /// from running captures in parallel.
  ///
  /// # Platform-specific
  ///
  /// - macOS: `true`; each capture is a screenshot.
  /// - Windows: `false`; each capture is a token.
  pub const CAPTURE_BLOCKS: bool = cfg!(target_os = "macos");

  /// Captures a frame of `window` for use in an [`AnimationWindow`].
  ///
  /// The capture is taken from the window's own surface, so the overlay
  /// can be positioned anywhere on screen independently of where the
  /// window currently sits.
  ///
  /// Captures that run together should share one `windows` snapshot,
  /// which is safe to use from several threads.
  ///
  /// # Platform-specific
  ///
  /// - macOS: A screenshot of the window as it is right now, together with
  ///   any companions on screen around it.
  /// - Windows: Nothing is captured. The overlay is a live DWM thumbnail
  ///   of the window, so this returns a token immediately.
  pub fn capture_frame(
    &self,
    window_id: WindowId,
    windows: &OnScreenWindows,
  ) -> crate::Result<AnimationCapture> {
    Ok(AnimationCapture {
      inner: self.inner.capture_frame(window_id, &windows.inner)?,
    })
  }

  /// Captures a frame of each of `windows` as one batch.
  ///
  /// Where a capture blocks, the captures run concurrently, so every
  /// animation started afterwards begins from an equally fresh frame.
  /// The batch lists the on-screen windows once and shares that listing
  /// among its captures.
  ///
  /// Returns one entry per window, in order. An entry is `Err` when its
  /// capture thread panicked.
  #[must_use]
  pub fn capture_frames(
    &self,
    windows: &[WindowId],
  ) -> Vec<std::thread::Result<crate::Result<AnimationCapture>>> {
    let on_screen = OnScreenWindows::new();
    let on_screen = &on_screen;

    // A capture that returns at once gains nothing from a thread.
    if !Self::CAPTURE_BLOCKS {
      return windows
        .iter()
        .map(|window_id| Ok(self.capture_frame(*window_id, on_screen)))
        .collect();
    }

    std::thread::scope(|scope| {
      // Spawn every capture before joining any, or they run one at a
      // time.
      let handles = windows
        .iter()
        .map(|window_id| {
          scope.spawn(move || self.capture_frame(*window_id, on_screen))
        })
        .collect::<Vec<_>>();

      handles
        .into_iter()
        .map(std::thread::ScopedJoinHandle::join)
        .collect()
    })
  }

  /// Executes `update_fn` inside a compositor transaction.
  ///
  /// Used with [`AnimationWindow::update`] to commit all updates together
  /// when `update_fn` returns.
  pub fn transaction<F, R>(
    &self,
    update_fn: F,
    dispatcher: &Dispatcher,
  ) -> crate::Result<R>
  where
    F: FnOnce() -> R + Send,
    R: Send,
  {
    self.inner.transaction(update_fn, dispatcher)
  }
}

/// A captured frame of a [`NativeWindow`], ready to be shown in an
/// [`AnimationWindow`].
///
/// Created via [`AnimationContext::capture_frame`]. Holding the capture
/// apart from the overlay lets a batch of frames be captured before any
/// animation clock starts, so every window of one sync begins from the
/// same instant.
pub struct AnimationCapture {
  inner: platform_impl::AnimationCapture,
}

/// A screenshot of a [`NativeWindow`] that can be animated performantly.
///
/// # Example usage
///
///   1. Swap in the `AnimationWindow` with the `NativeWindow`,
///   2. Perform animation.
///   3. Swap out the `AnimationWindow`.
///
/// ```no_run,compile_fail
/// let frame = real_window.frame()?;
/// let anim_window = AnimationWindow::new(context, real_window, frame, /* .. */)?;
///
/// # Hide the real window at the animation end position.
/// real_window.set_frame(frame.translate_in_direction(Direction::Left, 100))?;
/// real_window.set_transparency(&OpacityValue::from_alpha(0))?;
///
/// for i in 1..100 {
///   context.transaction(|| {
///     anim_window.update(frame.translate_in_direction(Direction::Left, i), None))
///   })??;
/// }
///
/// real_window.set_transparency(&OpacityValue::from_alpha(u8::MAX));
/// anim_window.destroy()?;
/// ```
pub struct AnimationWindow {
  inner: platform_impl::AnimationWindow,
}

impl AnimationWindow {
  /// Creates a new [`AnimationWindow`].
  ///
  /// The `outer_rect` should span the bounds of the start and end
  /// rects of the animation.
  pub fn new(
    context: &AnimationContext,
    window: &NativeWindow,
    capture: AnimationCapture,
    inner_rect: &Rect,
    outer_rect: &Rect,
    opacity: Option<OpacityValue>,
    dispatcher: &Dispatcher,
  ) -> crate::Result<Self> {
    let inner = platform_impl::AnimationWindow::new(
      &context.inner,
      window,
      capture.inner,
      inner_rect,
      outer_rect,
      opacity,
      dispatcher,
    )?;

    Ok(Self { inner })
  }

  /// Whether the window's bounds already enclose `rect`.
  ///
  /// A resize moves the window, while its content is placed relative to
  /// it, and the two changes do not land in the same compositor frame.
  /// Callers skip a resize that is not needed so content never draws for
  /// a frame against the wrong origin.
  ///
  /// # Platform-specific
  ///
  /// - macOS: always `false`, so callers keep resizing as before.
  #[must_use]
  pub fn covers(&self, rect: &Rect) -> bool {
    #[cfg(target_os = "windows")]
    {
      self.inner.covers(rect)
    }
    #[cfg(target_os = "macos")]
    {
      let _ = rect;
      false
    }
  }

  /// Resizes the window.
  ///
  /// Called when an animation's target rect changes mid-flight.
  pub fn resize(&mut self, outer_rect: &Rect) -> crate::Result<()> {
    self.inner.resize(outer_rect)
  }

  /// Updates the layer position and opacity within the window.
  ///
  /// Does not commit; should be called within
  /// `AnimationContext::transaction` for the change to take effect.
  ///
  /// # Platform-specific
  ///
  /// - Windows: also moves the source's companions by the same transform,
  ///   in the same compositor frame, and searches for companions while
  ///   none are found. A failing companion never fails the update.
  pub fn update(
    &self,
    inner_rect: &Rect,
    opacity: Option<&OpacityValue>,
  ) -> crate::Result<()> {
    self.inner.update(inner_rect, opacity)
  }

  /// Whether the platform interpolates an animation for itself.
  ///
  /// When `true`, a caller starts an animation once with
  /// [`AnimationWindow::animate_to`] and does not drive it; the platform
  /// compositor produces every frame. When `false`, the caller ticks and
  /// calls [`AnimationWindow::update`] per frame.
  pub const SELF_ANIMATING: bool = cfg!(target_os = "macos");

  /// Animates the layer to `target_rect` over `duration`, returning as
  /// soon as the animation is handed to the compositor.
  ///
  /// Only meaningful when [`AnimationWindow::SELF_ANIMATING`] is `true`.
  /// `on_complete` runs when the submitted motion completes or is removed;
  /// callers must reject obsolete callbacks after retargeting. Completion
  /// does not establish that source visibility changes were presented.
  ///
  /// # Platform-specific
  ///
  /// - macOS: hands the transition to Core Animation, which interpolates
  ///   on the render server. One hop to the main thread covers the whole
  ///   animation instead of one per frame.
  /// - Windows: unimplemented; the caller ticks `update` instead.
  pub fn animate_to<F>(
    &self,
    target_rect: &Rect,
    duration: std::time::Duration,
    easing: &crate::EasingFunction,
    opacity: Option<&OpacityValue>,
    on_complete: F,
  ) -> crate::Result<()>
  where
    F: Fn() + Send + Sync + 'static,
  {
    #[cfg(target_os = "macos")]
    {
      self.inner.animate_to(
        target_rect,
        duration,
        easing,
        opacity,
        on_complete,
      )
    }

    #[cfg(not(target_os = "macos"))]
    {
      let _ = (target_rect, duration, easing, opacity, on_complete);
      Ok(())
    }
  }

  /// Animates the layer through `frames`, evenly spaced over `duration`,
  /// returning as soon as the animation is handed to the compositor.
  ///
  /// For motion a single timing curve cannot express, such as a spring
  /// that overshoots or carries a different velocity on each edge. The
  /// caller samples its own model, so the motion is the same one a
  /// tick-driven backend draws. `on_complete` behaves as in
  /// [`AnimationWindow::animate_to`].
  ///
  /// Only meaningful when [`AnimationWindow::SELF_ANIMATING`] is `true`.
  /// A frame's opacity is used only when every frame has one.
  ///
  /// # Platform-specific
  ///
  /// - macOS: hands keyframes to Core Animation, which interpolates
  ///   linearly between them on the render server.
  /// - Windows: unimplemented; the caller ticks `update` instead.
  pub fn animate_along<F>(
    &self,
    frames: &[(Rect, Option<OpacityValue>)],
    duration: std::time::Duration,
    on_complete: F,
  ) -> crate::Result<()>
  where
    F: Fn() + Send + Sync + 'static,
  {
    #[cfg(target_os = "macos")]
    {
      self.inner.animate_along(frames, duration, on_complete)
    }

    #[cfg(not(target_os = "macos"))]
    {
      let _ = (frames, duration, on_complete);
      Ok(())
    }
  }

  /// Cancels compositor motion and retains a stationary overlay.
  pub fn stop_at(
    &self,
    rect: &Rect,
    opacity: Option<&OpacityValue>,
  ) -> crate::Result<()> {
    #[cfg(target_os = "macos")]
    {
      self.inner.stop_at(rect, opacity)
    }
    #[cfg(target_os = "windows")]
    {
      self.inner.update(rect, opacity)
    }
  }

  /// Cancels compositor motion and retains a stationary overlay where it
  /// is currently presented, or at `fallback` where the platform cannot
  /// say. Returns the frame the overlay now stands at.
  ///
  /// Does what [`AnimationWindow::current_frame`] followed by
  /// [`AnimationWindow::stop_at`] (without opacity) does.
  ///
  /// # Platform-specific
  ///
  /// - macOS: one hop to the main thread, where the two calls cost one
  ///   each.
  /// - Windows: stops at `fallback`, the frame the caller already knows.
  pub fn stop_in_place(&self, fallback: &Rect) -> crate::Result<Rect> {
    #[cfg(target_os = "macos")]
    {
      self.inner.stop_in_place(fallback)
    }
    #[cfg(target_os = "windows")]
    {
      self.inner.update(fallback, None)?;
      Ok(fallback.clone())
    }
  }

  /// Returns the compositor's currently presented geometry if available.
  /// Windows motion is driven by the caller, which already knows this
  /// frame.
  pub fn current_frame(&self) -> crate::Result<Option<Rect>> {
    #[cfg(target_os = "macos")]
    {
      self.inner.current_frame()
    }
    #[cfg(target_os = "windows")]
    {
      Ok(None)
    }
  }

  /// Whether every companion the overlay draws shows itself again.
  ///
  /// A companion is a window of another process that decorates the
  /// source, such as a border ring. At the reveal the overlay is held
  /// until this is `true`, so the decoration is never missing for a
  /// frame. Also `true` when there are no companions.
  ///
  /// Checks that run together should share one `windows` snapshot.
  ///
  /// # Platform-specific
  ///
  /// - macOS: companions are captured into the source's image. `true` once
  ///   one decorates the source where it now stands.
  /// - Windows: `true` once every companion reports `DWMWA_CLOAKED` as 0
  ///   or is gone. `windows` is ignored.
  #[must_use]
  pub fn companions_revealed(&self, windows: &OnScreenWindows) -> bool {
    self.inner.companions_revealed(&windows.inner)
  }

  /// Destroys the window and releases GPU resources.
  pub fn destroy(&mut self) -> crate::Result<()> {
    self.inner.destroy()
  }
}

/// Draws a window's companions above it while the WM moves the real
/// window.
///
/// A companion is a window of another process that decorates the source,
/// such as a border ring. The source's own pixels are real during native
/// motion, so this draws only its companions, as thumbnails in a
/// transparent window directly above the source in the z-order. Each
/// companion's rect and the source's rect are recorded when the overlay
/// is created. Every update anchors each recorded companion edge to the
/// nearer edge of the frame the WM has just requested, so a band keeps
/// its thickness and stretches with the window.
///
/// Callers issue [`CompanionOverlay::update`] straight after each native
/// move request for the source, so the decoration follows the requests
/// instead of trailing the move events.
///
/// # Platform-specific
///
/// - macOS: never created; the platform has no companions.
pub struct CompanionOverlay {
  #[cfg(target_os = "windows")]
  inner: platform_impl::CompanionOverlay,
}

impl CompanionOverlay {
  /// Creates an overlay for the companions of `window`, drawn where they
  /// stand, within `outer_rect`.
  ///
  /// Returns `None` when the window has no companions, so a window
  /// without them costs only the shared companion search.
  ///
  /// # Platform-specific
  ///
  /// - macOS: always `None`.
  /// - Windows: also `None` for a topmost window, whose band the overlay
  ///   may not join.
  pub fn new(
    window: &NativeWindow,
    outer_rect: &Rect,
    dispatcher: &Dispatcher,
  ) -> crate::Result<Option<Self>> {
    #[cfg(target_os = "windows")]
    {
      Ok(
        platform_impl::CompanionOverlay::new(
          window, outer_rect, dispatcher,
        )?
        .map(|inner| Self { inner }),
      )
    }
    #[cfg(target_os = "macos")]
    {
      let _ = (window, outer_rect, dispatcher);
      Ok(None)
    }
  }

  /// Grows the overlay to enclose `outer_rect` if it does not already,
  /// and restacks it directly above its source.
  ///
  /// Called when the source's motion is retargeted. The recorded
  /// companion rects are kept, so the decoration does not jump.
  pub fn retarget(&mut self, outer_rect: &Rect) -> crate::Result<()> {
    #[cfg(target_os = "windows")]
    {
      self.inner.retarget(outer_rect)
    }
    #[cfg(target_os = "macos")]
    {
      let _ = outer_rect;
      Ok(())
    }
  }

  /// Draws every companion anchored to `frame`, the source's window rect
  /// as just requested.
  ///
  /// Companions that are gone or fail are dropped; neither fails the
  /// update.
  pub fn update(&self, frame: &Rect) -> crate::Result<()> {
    #[cfg(target_os = "windows")]
    {
      self.inner.update(frame)
    }
    #[cfg(target_os = "macos")]
    {
      let _ = frame;
      Ok(())
    }
  }

  /// Whether every companion the overlay draws shows itself again.
  ///
  /// `true` once every companion reports `DWMWA_CLOAKED` as 0 or is gone.
  #[must_use]
  pub fn companions_revealed(&self) -> bool {
    #[cfg(target_os = "windows")]
    {
      self.inner.companions_revealed()
    }
    #[cfg(target_os = "macos")]
    {
      true
    }
  }

  /// Destroys the overlay and releases its thumbnails.
  pub fn destroy(&mut self) -> crate::Result<()> {
    #[cfg(target_os = "windows")]
    {
      self.inner.destroy()
    }
    #[cfg(target_os = "macos")]
    {
      Ok(())
    }
  }
}

/// Call counts here read process-wide counters, so they hold only under
/// `--test-threads=1`.
#[cfg(all(test, target_os = "macos"))]
mod tests {
  use super::*;
  use crate::{NativeCallStats, WindowId};

  /// A batch of captures lists the on-screen windows once, however many
  /// windows it holds.
  #[test]
  fn capture_batch_lists_windows_once() {
    const WINDOWS: usize = 8;

    let context = AnimationContext {
      inner: platform_impl::AnimationContext,
    };
    // Ids no window has: each capture fails after its companion search,
    // which is the part under test.
    let absent = [WindowId(u32::MAX); WINDOWS];

    let before = NativeCallStats::snapshot();
    let results = context.capture_frames(&absent);
    let spent = NativeCallStats::snapshot().since(&before);

    assert_eq!(results.len(), WINDOWS);
    assert_eq!(spent.window_list_full, 1);
  }
}
