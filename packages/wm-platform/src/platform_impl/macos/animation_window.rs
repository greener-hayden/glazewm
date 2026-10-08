use std::{
  sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, OnceLock,
  },
  time::{Duration, Instant},
};

use block2::RcBlock;
use objc2::{
  rc::Retained, runtime::AnyObject, MainThreadMarker, MainThreadOnly,
};
use objc2_app_kit::{
  NSBackingStoreType, NSColor, NSFloatingWindowLevel, NSWindow,
  NSWindowAnimationBehavior, NSWindowOrderingMode, NSWindowStyleMask,
};
use objc2_core_foundation::{
  CFArray, CFDictionary, CFNumber, CFRetained, CFString, CFType, CGPoint,
  CGRect, CGSize,
};
#[allow(deprecated)]
use objc2_core_graphics::{
  kCGWindowBounds, kCGWindowLayer, kCGWindowNumber, kCGWindowOwnerName,
  CGImage, CGRectMakeWithDictionaryRepresentation, CGWindowImageOption,
  CGWindowListCopyWindowInfo, CGWindowListCreateImage,
  CGWindowListCreateImageFromArray, CGWindowListOption,
};
use objc2_foundation::{ns_string, NSArray, NSNumber, NSString, NSValue};
use objc2_quartz_core::{
  kCAAnimationLinear, CAKeyframeAnimation, CALayer, CAMediaTiming,
  CAMediaTimingFunction, CATransaction,
};

use crate::{
  companion, platform_impl::primary_display_height, Dispatcher,
  EasingFunction, NativeCall, NativeCallStats, NativeWindow, OpacityValue,
  Rect, ThreadBound, WindowId,
};

/// Process whose on-screen windows decorate managed windows.
///
/// A window of another process carries no property this process can
/// read, so a companion is recognised by its owner and by where it
/// stands. See `companion::decorates`.
const COMPANION_OWNER: &str = "mover-borders";

/// Where a capture that includes companions stood on screen.
///
/// The image covers `bounds`, which reaches past the source's `frame` on
/// every side a companion does. The layer showing it is placed through
/// the transform that takes `frame` to the animated rect, so a ring
/// around the window stays around the animated window.
#[derive(Clone, Debug, PartialEq)]
struct CaptureExtent {
  /// The source's frame when captured.
  frame: Rect,
  /// The captured region: `frame` joined with every companion.
  bounds: Rect,
}

impl CaptureExtent {
  /// Maps the rect the source is animated at to the rect its image
  /// covers.
  fn image_rect(&self, animated: &Rect) -> Rect {
    companion::companion_rect(&self.bounds, &self.frame, animated)
      .unwrap_or_else(|| animated.clone())
  }

  /// Maps a rect the image covers back to the rect of the source within
  /// it. Inverse of `image_rect`, within a pixel of rounding.
  fn source_rect(&self, image: &Rect) -> Rect {
    companion::companion_rect(&self.frame, &self.bounds, image)
      .unwrap_or_else(|| image.clone())
  }
}

/// Cubic Bézier control points for an [`EasingFunction`].
///
/// Core Animation expresses easing as a timing curve rather than a
/// per-frame function, so each variant maps to the curve that matches
/// `EasingFunction::apply` most closely.
///
/// `Spring` is an approximation: the curve is the closest cubic to a
/// critically damped spring over its settle time, within about 2% of the
/// travel. It ignores bounce and velocity, because an implicit layer
/// animation takes one timing function for every property. Callers with
/// a configured spring use `AnimationWindow::animate_along` instead.
const fn control_points(easing: &EasingFunction) -> (f32, f32, f32, f32) {
  match easing {
    EasingFunction::Linear => (0.0, 0.0, 1.0, 1.0),
    EasingFunction::EaseIn => (0.55, 0.085, 0.68, 0.53),
    EasingFunction::EaseOut => (0.25, 0.46, 0.45, 0.94),
    EasingFunction::EaseInOut => (0.455, 0.03, 0.515, 0.955),
    EasingFunction::EaseInCubic => (0.55, 0.055, 0.675, 0.19),
    EasingFunction::EaseOutCubic => (0.215, 0.61, 0.355, 1.0),
    EasingFunction::EaseInOutCubic => (0.645, 0.045, 0.355, 1.0),
    EasingFunction::Spring => (0.29, 0.48, 0.03, 1.0),
  }
}

/// Platform-specific implementation of [`OnScreenWindows`].
///
/// Lists the windows on first use. The `OnceLock` makes concurrent
/// captures share that one listing: the first to ask takes it and the
/// rest wait for it.
#[derive(Default)]
pub(crate) struct OnScreenWindows {
  listed: OnceLock<Option<Vec<ListedWindow>>>,
}

impl OnScreenWindows {
  /// The listed windows, front to back, taking the list if this is the
  /// first use.
  ///
  /// `None` when the window server could not provide the list; that
  /// outcome is kept too, so a failure is not retried per window.
  fn listed(&self) -> Option<&[ListedWindow]> {
    self.listed.get_or_init(on_screen_windows).as_deref()
  }

  /// Bounds of the window `id` in the list, taking the list if this is
  /// the first use. `None` for a window that is not listed.
  pub(crate) fn bounds(&self, id: WindowId) -> Option<Rect> {
    self
      .listed()?
      .iter()
      .find(|window| window.id == id.0)
      .map(|window| window.bounds.clone())
  }
}

/// How long a picture of the desktop is used before it is taken again.
///
/// The desktop seldom changes, and a stale picture shows for one motion
/// at most: every use that finds it old has it retaken off-thread.
const DESKTOP_MAX_AGE: Duration = Duration::from_secs(10);

/// A picture of one display's desktop: its wallpaper and whatever else
/// the system draws beneath every application's windows.
struct DesktopImage {
  /// Bounds of the display, in screen coordinates.
  bounds: Rect,
  image: CFRetained<CGImage>,
}

// SAFETY: A `CGImage` is immutable once created, and Core Foundation
// reference counting is thread-safe.
unsafe impl Send for DesktopImage {}

/// Pictures of every display's desktop, kept so that covering part of
/// the screen costs no capture at the moment it is needed.
///
/// A capture takes about 20ms whatever its size, which is longer than a
/// frame, so it is never taken on the way to a motion: `refresh` runs
/// off-thread and `crop` uses what is there.
#[derive(Default)]
pub(crate) struct DesktopCache {
  /// The pictures, and when they were taken.
  displays: Mutex<(Vec<DesktopImage>, Option<Instant>)>,
  /// Whether a refresh is under way.
  refreshing: AtomicBool,
}

impl DesktopCache {
  /// The desktop beneath `rect`, cut from the picture of the display
  /// most of it lies on, with the part of `rect` that is on that display.
  ///
  /// `None` when `rect` is on no display that has been captured.
  fn crop(&self, rect: &Rect) -> Option<(Rect, CFRetained<CGImage>)> {
    let displays = self.displays.lock().ok()?;
    let display = displays
      .0
      .iter()
      .max_by_key(|display| display.bounds.intersection_area(rect))?;

    let rect = Rect::from_ltrb(
      rect.left.max(display.bounds.left),
      rect.top.max(display.bounds.top),
      rect.right.min(display.bounds.right),
      rect.bottom.min(display.bounds.bottom),
    );
    if rect.width() <= 0 || rect.height() <= 0 {
      return None;
    }

    // The picture is at the display's backing scale.
    #[allow(clippy::cast_precision_loss)]
    let scale = CGImage::width(Some(&display.image)) as f64
      / f64::from(display.bounds.width());

    let image = CGImage::with_image_in_rect(
      Some(&display.image),
      CGRect::new(
        CGPoint {
          x: f64::from(rect.x() - display.bounds.x()) * scale,
          y: f64::from(rect.y() - display.bounds.y()) * scale,
        },
        CGSize {
          width: f64::from(rect.width()) * scale,
          height: f64::from(rect.height()) * scale,
        },
      ),
    )?;

    Some((rect, image))
  }

  /// Takes the pictures again on another thread, if they are missing or
  /// older than `DESKTOP_MAX_AGE` and no refresh is already running.
  fn refresh_if_stale(self: &Arc<Self>) {
    let fresh = self.displays.lock().is_ok_and(|displays| {
      displays
        .1
        .is_some_and(|taken| taken.elapsed() < DESKTOP_MAX_AGE)
    });
    if fresh || self.refreshing.swap(true, Ordering::AcqRel) {
      return;
    }

    let cache = Arc::clone(self);
    std::thread::spawn(move || {
      let images = capture_desktops();
      if let Ok(mut displays) = cache.displays.lock() {
        *displays = (images, Some(Instant::now()));
      }
      cache.refreshing.store(false, Ordering::Release);
    });
  }
}

/// Captures the desktop of every display, without any application's
/// windows.
///
/// Returns nothing when the window server cannot list its windows.
#[allow(deprecated)]
fn capture_desktops() -> Vec<DesktopImage> {
  let Some(windows) =
    CGWindowListCopyWindowInfo(CGWindowListOption::OptionOnScreenOnly, 0)
  else {
    return Vec::new();
  };
  // SAFETY: Window services returns dictionaries with string keys and
  // Core Foundation values, retaining all of those objects.
  let windows =
    unsafe { windows.cast_unchecked::<CFDictionary<CFString, CFType>>() };

  // The desktop is every window on a layer beneath the normal one. Each
  // spans its display, so their bounds are also the displays'.
  let mut ids = Vec::new();
  let mut displays = Vec::<Rect>::new();
  for info in windows.iter() {
    // SAFETY: The framework provides these immutable dictionary keys.
    let (number, layer, bounds) =
      unsafe { (kCGWindowNumber, kCGWindowLayer, kCGWindowBounds) };
    let number_of = |key| {
      info
        .get(key)
        .and_then(|value| value.downcast_ref::<CFNumber>()?.as_i64())
    };
    let (Some(id), Some(layer)) = (number_of(number), number_of(layer))
    else {
      continue;
    };
    if layer >= 0 {
      continue;
    }
    let Some(dictionary) = info.get(bounds) else {
      continue;
    };
    let Some(dictionary) = dictionary.downcast_ref::<CFDictionary>()
    else {
      continue;
    };
    let mut rect = CGRect::ZERO;
    // SAFETY: The checked dictionary is retained and `rect` is a live
    // output rectangle for the duration of this synchronous call.
    if !unsafe {
      CGRectMakeWithDictionaryRepresentation(
        Some(dictionary),
        &raw mut rect,
      )
    } {
      continue;
    }

    // LINT: A window ID is an unsigned 32-bit value.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    ids.push(id as usize as *const std::ffi::c_void);
    let rect = Rect::from(rect);
    if !displays.contains(&rect) {
      displays.push(rect);
    }
  }

  let Ok(count) = isize::try_from(ids.len()) else {
    return Vec::new();
  };
  // SAFETY: `ids` holds `count` values and outlives the call. Null
  // callbacks make the array store them as plain values, with nothing
  // to retain or release.
  let Some(array) = (unsafe {
    CFArray::new(None, ids.as_mut_ptr(), count, std::ptr::null())
  }) else {
    return Vec::new();
  };

  displays
    .into_iter()
    .filter_map(|bounds| {
      NativeCallStats::record(NativeCall::ScreenCapture);
      // SAFETY: `array` holds window IDs, as the call documents.
      let image = unsafe {
        CGWindowListCreateImageFromArray(
          bounds.clone().into(),
          &array,
          // The cover stands still beside real windows, so it has to be
          // as sharp as they are.
          CGWindowImageOption::BestResolution,
        )
      }?;
      Some(DesktopImage { bounds, image })
    })
    .collect()
}

/// Platform-specific implementation of [`AnimationContext`].
pub(crate) struct AnimationContext {
  /// Pictures of the desktop, for covers.
  desktop: Arc<DesktopCache>,
}

impl AnimationContext {
  /// Implements [`AnimationContext::new`].
  #[allow(clippy::unnecessary_wraps)]
  pub(crate) fn new(_dispatcher: &Dispatcher) -> crate::Result<Self> {
    let desktop = Arc::new(DesktopCache::default());
    // Taken now, so the first motion already has them.
    desktop.refresh_if_stale();
    Ok(Self { desktop })
  }

  /// A context that has captured no desktop and so makes no covers, for
  /// tests that count native calls.
  #[cfg(test)]
  pub(crate) fn without_desktop() -> Self {
    Self {
      desktop: Arc::default(),
    }
  }

  /// Implements [`AnimationContext::capture_frame`].
  #[allow(clippy::unused_self)]
  pub(crate) fn capture_frame(
    &self,
    window_id: WindowId,
    windows: &OnScreenWindows,
  ) -> crate::Result<AnimationCapture> {
    Ok(AnimationCapture {
      frame: CapturedFrame::new(window_id, windows)?,
    })
  }

  /// Implements [`AnimationContext::transaction`].
  #[allow(clippy::unused_self)]
  pub(crate) fn transaction<F, R>(
    &self,
    update_fn: F,
    dispatcher: &Dispatcher,
  ) -> crate::Result<R>
  where
    F: FnOnce() -> R + Send,
    R: Send,
  {
    dispatcher.dispatch_sync(|| {
      CATransaction::begin();
      CATransaction::setDisableActions(true);
      let result = update_fn();
      CATransaction::commit();
      result
    })
  }
}

/// The AppKit objects of an overlay, which only the main thread may
/// touch.
///
/// Bound together, so the overlay hops to the main thread once to use or
/// release them, instead of once for each.
struct OverlayViews {
  ns_window: Retained<NSWindow>,
  layer: Retained<CALayer>,
}

impl Drop for OverlayViews {
  /// Closes the window before its last reference goes.
  ///
  /// Runs on the main thread, where the `ThreadBound` holding the views
  /// drops them.
  fn drop(&mut self) {
    self.ns_window.close();
  }
}

/// The image layer of an overlay, kept so that any thread can hide it.
///
/// Closing the overlay's window has to wait its turn on the main thread,
/// which also carries every accessibility call and so stands still
/// whenever an application is slow to answer one. Hiding the layer waits
/// for nothing.
struct ImageLayer(Retained<CALayer>);

// SAFETY: Core Animation lets any thread set a layer's properties, and
// applies them when that thread commits its transaction. Only `hide`
// uses the layer from off the main thread. Releasing the reference is
// safe from any thread.
unsafe impl Send for ImageLayer {}

// SAFETY: A shared reference reaches only `hide`, which is safe from
// several threads at once for the reason above.
unsafe impl Sync for ImageLayer {}

impl ImageLayer {
  /// Takes the image off the screen, from whichever thread calls.
  fn hide(&self) {
    self.set_hidden(true);
  }

  /// Puts the image on the screen, from whichever thread calls.
  fn show(&self) {
    self.set_hidden(false);
  }

  /// Hides or shows the image, from whichever thread calls.
  fn set_hidden(&self, hidden: bool) {
    CATransaction::begin();
    CATransaction::setDisableActions(true);
    self.0.setHidden(hidden);
    CATransaction::commit();
    // A thread without a run loop commits nothing until it is flushed.
    CATransaction::flush();
  }
}

/// A picture of the desktop standing over part of the screen, one level
/// beneath the overlays, so that a real window can be moved and resized
/// under it unseen.
struct Cover {
  views: ThreadBound<OverlayViews>,
  image: ImageLayer,
}

/// Platform-specific implementation of [`AnimationWindow`].
pub(crate) struct AnimationWindow {
  /// `None` once destroyed.
  views: Option<ThreadBound<OverlayViews>>,

  /// The image the window shows. `None` once destroyed.
  image: Option<ImageLayer>,

  /// Cover over where the real window is going. Stays until the overlay
  /// is destroyed.
  destination_cover: Option<Cover>,

  /// Covers over where the real window stood when it was sent on its
  /// way. Removed by `uncover_origin` once the window has left.
  origin_covers: Vec<Cover>,

  /// Frame of the `AnimationWindow` (in CG coordinates).
  outer_rect: Rect,

  /// Height of the primary display.
  display_height: i32,

  /// How the captured image extends past the source, if it includes
  /// companions.
  extent: Option<CaptureExtent>,

  /// The window this overlay stands in for.
  source_id: WindowId,
}

impl AnimationWindow {
  /// Implements [`AnimationWindow::new`].
  pub(crate) fn new(
    _context: &AnimationContext,
    window: &NativeWindow,
    capture: AnimationCapture,
    inner_rect: &Rect,
    outer_rect: &Rect,
    opacity: Option<OpacityValue>,
    dispatcher: &Dispatcher,
  ) -> crate::Result<Self> {
    type DispatchResult =
      crate::Result<(ThreadBound<OverlayViews>, ImageLayer)>;

    let captured = capture.frame;
    let extent = captured.extent.clone();
    let layer_extent = extent.clone();

    // Needed for CG<->AppKit coordinate conversion. Read in process,
    // where asking the primary display for it walks every screen on the
    // main thread.
    let display_height = primary_display_height();

    let (views, image) =
      dispatcher.dispatch_sync(|| -> DispatchResult {
        // SAFETY: `Dispatcher::dispatch_sync` runs on the main thread.
        let mtm = unsafe { MainThreadMarker::new_unchecked() };

        let ns_window = unsafe {
          NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            // `NSWindow` expects AppKit coordinates (bottom-left origin).
            outer_rect.flip_y(display_height).into(),
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            false,
          )
        };

        ns_window.setBackgroundColor(Some(&NSColor::clearColor()));
        ns_window.setOpaque(false);
        ns_window.setIgnoresMouseEvents(true);

        // Disable AppKit's default open/close animations.
        ns_window.setAnimationBehavior(NSWindowAnimationBehavior::None);

        // SAFETY: `NSWindow` is normally released on close, but when the
        // `Retained<NSWindow>` field is dropped, it will also send a
        // release call and segfault.
        unsafe { ns_window.setReleasedWhenClosed(false) };

        let content_view =
          ns_window.contentView().ok_or(crate::Error::Platform(
            "NSWindow must have a content view.".to_string(),
          ))?;

        content_view.setWantsLayer(true);

        let root_layer =
          content_view.layer().ok_or(crate::Error::Platform(
            "Layer must exist after `setWantsLayer`.".to_string(),
          ))?;

        // The root layer fills the content view, so a sublayer is needed
        // to animate within it.
        let layer = CALayer::new();

        // SAFETY: `CGImageRef` is accepted by `CALayer::contents`.
        unsafe {
          layer.setContents(Some(
            &*std::ptr::from_ref::<CGImage>(&captured.cg_image)
              .cast::<AnyObject>(),
          ));
        };

        // Left at the default scale of 1: the capture is taken at
        // logical resolution, so one image pixel is one point. Matching
        // the display's backing scale here would make the layer treat
        // the image as 2x and draw it at half size.

        CATransaction::begin();
        CATransaction::setDisableActions(true);

        Self::update_layer(
          &layer,
          inner_rect,
          outer_rect,
          layer_extent.as_ref(),
          opacity.as_ref(),
        );
        CATransaction::commit();

        root_layer.addSublayer(&layer);

        // Ordering is relative to another process's window, which AppKit
        // does not guarantee. Without a level of its own the overlay
        // stays at the normal level and can land behind whatever else is
        // on screen, so the tween plays invisibly and reads as a snap.
        ns_window.setLevel(NSFloatingWindowLevel);

        #[allow(clippy::cast_possible_wrap)]
        ns_window.orderWindow_relativeTo(
          NSWindowOrderingMode::Above,
          window.id().0 as isize,
        );

        let image = ImageLayer(layer.clone());

        Ok((
          ThreadBound::new(
            OverlayViews { ns_window, layer },
            dispatcher.clone(),
          ),
          image,
        ))
      })??;

    Ok(Self {
      views: Some(views),
      image: Some(image),
      destination_cover: None,
      origin_covers: Vec::new(),
      display_height,
      outer_rect: outer_rect.clone(),
      extent,
      source_id: window.id(),
    })
  }

  /// The overlay's AppKit objects, or an error once it is destroyed.
  fn views(&self) -> crate::Result<&ThreadBound<OverlayViews>> {
    self.views.as_ref().ok_or_else(|| {
      crate::Error::Platform("Overlay is already destroyed.".to_string())
    })
  }

  /// Implements [`AnimationWindow::resize`].
  pub(crate) fn resize(&mut self, outer_rect: &Rect) -> crate::Result<()> {
    self.outer_rect = outer_rect.clone();

    self.views()?.with(|views| {
      views.ns_window.setFrame_display(
        self.outer_rect.flip_y(self.display_height).into(),
        false,
      );
    })
  }

  /// Implements [`AnimationWindow::update`].
  pub(crate) fn update(
    &self,
    inner_rect: &Rect,
    opacity: Option<&OpacityValue>,
  ) -> crate::Result<()> {
    self.views()?.with(|views| {
      Self::update_layer(
        &views.layer,
        inner_rect,
        &self.outer_rect,
        self.extent.as_ref(),
        opacity,
      );
    })
  }

  /// Implements [`AnimationWindow::animate_to`].
  ///
  /// Starts a Core Animation transition to `target_rect` and returns
  /// immediately. The render server interpolates every frame, so the
  /// caller must not tick.
  ///
  /// Called again mid-flight, Core Animation retargets from wherever the
  /// layer is currently presented, so a cancel-and-replace needs no
  /// special handling here.
  ///
  /// # Platform-specific
  ///
  /// - macOS: costs a single hop to the main thread for the whole
  ///   animation, rather than one per frame. That matters because
  ///   accessibility calls are confined to that same thread, and a move
  ///   issues several of them while the animation is running.
  pub(crate) fn animate_to<F>(
    &self,
    target_rect: &Rect,
    duration: Duration,
    easing: &EasingFunction,
    opacity: Option<&OpacityValue>,
    on_complete: F,
  ) -> crate::Result<()>
  where
    F: Fn() + Send + Sync + 'static,
  {
    let outer_rect = self.outer_rect.clone();
    let extent = self.extent.clone();
    let target_rect = target_rect.clone();
    let easing = easing.clone();
    let opacity = opacity.copied();

    self.views()?.with(move |views| {
      let layer = &views.layer;
      let (c1x, c1y, c2x, c2y) = control_points(&easing);

      CATransaction::begin();
      let completion = RcBlock::new(on_complete);
      // SAFETY: The transaction copies the owned, static callback. This
      // reports completion/removal only, never a presentation fence.
      unsafe { CATransaction::setCompletionBlock(Some(&completion)) };
      CATransaction::setAnimationDuration(duration.as_secs_f64());
      CATransaction::setAnimationTimingFunction(Some(
        &CAMediaTimingFunction::functionWithControlPoints(
          c1x, c1y, c2x, c2y,
        ),
      ));

      // Unlike every other write to this layer, actions are left enabled
      // so the change is animated rather than applied outright.
      Self::update_layer(
        layer,
        &target_rect,
        &outer_rect,
        extent.as_ref(),
        opacity.as_ref(),
      );
      CATransaction::commit();
    })
  }

  /// Implements [`AnimationWindow::animate_along`].
  ///
  /// Adds explicit keyframe animations for the layer's position, bounds
  /// and, when every frame has one, opacity. The model values are set to
  /// the last frame first, so the layer rests there once the animations
  /// are removed.
  pub(crate) fn animate_along<F>(
    &self,
    frames: &[(Rect, Option<OpacityValue>)],
    duration: Duration,
    on_complete: F,
  ) -> crate::Result<()>
  where
    F: Fn() + Send + Sync + 'static,
  {
    let Some((target_rect, target_opacity)) = frames.last().cloned()
    else {
      return Err(crate::Error::Platform(
        "Keyframe animation needs at least one frame.".to_string(),
      ));
    };
    let outer_rect = self.outer_rect.clone();
    let extent = self.extent.clone();
    let frames = frames.to_vec();

    self.views()?.with(move |views| {
      let layer = &views.layer;
      let mut positions = Vec::with_capacity(frames.len());
      let mut bounds = Vec::with_capacity(frames.len());
      let mut opacities = Vec::with_capacity(frames.len());

      for (rect, opacity) in &frames {
        let frame = Self::layer_frame(rect, &outer_rect, extent.as_ref());
        // The layer keeps its default anchor point, so its position is
        // the centre of its frame.
        let center = CGPoint::new(
          frame.origin.x + frame.size.width / 2.0,
          frame.origin.y + frame.size.height / 2.0,
        );
        let size = CGRect::new(CGPoint::ZERO, frame.size);
        // SAFETY: Both wrap plain geometry structs by value.
        unsafe {
          positions.push(NSValue::valueWithPoint(center));
          bounds.push(NSValue::valueWithRect(size));
        }
        if let Some(opacity) = opacity {
          opacities.push(NSNumber::new_f32(opacity.0));
        }
      }

      CATransaction::begin();
      let completion = RcBlock::new(on_complete);
      // SAFETY: The transaction copies the owned, static callback. This
      // reports completion/removal only, never a presentation fence.
      unsafe { CATransaction::setCompletionBlock(Some(&completion)) };
      CATransaction::setDisableActions(true);
      layer.removeAllAnimations();
      Self::update_layer(
        layer,
        &target_rect,
        &outer_rect,
        extent.as_ref(),
        target_opacity.as_ref(),
      );

      // A zero duration means "use the default" to Core Animation, which
      // would play a quarter-second animation between frames that are
      // already at the target. The model values above are the whole
      // motion then, and the completion block still runs.
      if !duration.is_zero() {
        Self::add_keyframes(
          layer,
          ns_string!("position"),
          &positions,
          duration,
        );
        Self::add_keyframes(
          layer,
          ns_string!("bounds"),
          &bounds,
          duration,
        );
        if opacities.len() == frames.len() {
          Self::add_keyframes(
            layer,
            ns_string!("opacity"),
            &opacities,
            duration,
          );
        }
      }
      CATransaction::commit();
    })
  }

  /// Adds a linear keyframe animation of `key_path` through `values`,
  /// evenly spaced over `duration`.
  ///
  /// `values` must hold the value type `key_path` animates.
  fn add_keyframes<T: objc2::Message>(
    layer: &Retained<CALayer>,
    key_path: &NSString,
    values: &[Retained<T>],
    duration: Duration,
  ) {
    let values = NSArray::from_retained_slice(values);
    let animation =
      CAKeyframeAnimation::animationWithKeyPath(Some(key_path));
    // SAFETY: `kCAAnimationLinear` is an immutable framework constant.
    animation.setCalculationMode(unsafe { kCAAnimationLinear });
    animation.setDuration(duration.as_secs_f64());
    // SAFETY: Callers pass the value type their key path animates. The
    // cast only erases the array's element type.
    unsafe {
      animation.setValues(Some(values.cast_unchecked::<AnyObject>()));
    }
    layer.addAnimation_forKey(&animation, Some(key_path));
  }

  /// Cancels active animations and writes a stationary frame atomically.
  pub(crate) fn stop_at(
    &self,
    rect: &Rect,
    opacity: Option<&OpacityValue>,
  ) -> crate::Result<()> {
    self.views()?.with(|views| {
      self.stop_layer_at(&views.layer, rect, opacity);
    })
  }

  /// Implements [`AnimationWindow::stop_in_place`].
  ///
  /// Samples the presented frame and stops there in one hop, where
  /// [`Self::current_frame`] then [`Self::stop_at`] take one each.
  pub(crate) fn stop_in_place(
    &self,
    fallback: &Rect,
  ) -> crate::Result<Rect> {
    self.views()?.with(|views| {
      let frame = self
        .presented_frame(&views.layer)
        .unwrap_or_else(|| fallback.clone());
      self.stop_layer_at(&views.layer, &frame, None);
      frame
    })
  }

  /// Samples the presentation layer in global screen coordinates.
  pub(crate) fn current_frame(&self) -> crate::Result<Option<Rect>> {
    self
      .views()?
      .with(|views| self.presented_frame(&views.layer))
  }

  /// Cancels `layer`'s animations and writes a stationary frame.
  ///
  /// Must run on the main thread.
  fn stop_layer_at(
    &self,
    layer: &Retained<CALayer>,
    rect: &Rect,
    opacity: Option<&OpacityValue>,
  ) {
    CATransaction::begin();
    CATransaction::setDisableActions(true);
    layer.removeAllAnimations();
    Self::update_layer(
      layer,
      rect,
      &self.outer_rect,
      self.extent.as_ref(),
      opacity,
    );
    CATransaction::commit();
  }

  /// Reads where `layer` is presented, in global screen coordinates.
  ///
  /// Must run on the main thread.
  fn presented_frame(&self, layer: &Retained<CALayer>) -> Option<Rect> {
    // SAFETY: Access occurs on the owning AppKit thread; the returned
    // presentation layer is read only and retained for this snapshot.
    unsafe { layer.presentationLayer() }.map(|presented| {
      let local =
        Rect::from(presented.frame()).flip_y(self.outer_rect.height());
      let image = Rect::from_xy(
        self.outer_rect.x() + local.x(),
        self.outer_rect.y() + local.y(),
        local.width(),
        local.height(),
      );
      self
        .extent
        .as_ref()
        .map_or_else(|| image.clone(), |extent| extent.source_rect(&image))
    })
  }

  /// Implements [`AnimationWindow::companions_revealed`].
  ///
  /// `true` for an image without companions, which has nothing to hand
  /// back. Otherwise `true` once a companion decorates the source where
  /// it now stands, or when the source or the window list is gone, so a
  /// failure never holds the overlay.
  ///
  /// Lists the windows through `windows` only when this image has
  /// companions to wait for.
  pub(crate) fn companions_revealed(
    &self,
    windows: &OnScreenWindows,
  ) -> bool {
    if self.extent.is_none() {
      return true;
    }
    windows.listed().is_none_or(|windows| {
      companions_revealed_in(windows, self.source_id)
    })
  }

  /// Implements [`AnimationWindow::cover`].
  ///
  /// Each cover is its own window, as a window cannot span displays. The
  /// covers are made hidden; `show_covers` puts them on screen. Makes
  /// none unless every rect can be covered. A rect is covered where it
  /// lies on its display. A destination cover from an earlier call
  /// stands over where the window is now, so it becomes an origin cover.
  pub(crate) fn cover(
    &mut self,
    destination: &Rect,
    origin: Option<&Rect>,
    context: &AnimationContext,
    dispatcher: &Dispatcher,
  ) -> crate::Result<bool> {
    context.desktop.refresh_if_stale();

    let Some(destination) = context.desktop.crop(destination) else {
      return Ok(false);
    };
    let origin = match origin {
      Some(origin) => match context.desktop.crop(origin) {
        Some(patch) => Some(patch),
        None => return Ok(false),
      },
      None => None,
    };

    let destination = self.make_cover(destination, dispatcher)?;
    let origin = origin
      .map(|patch| self.make_cover(patch, dispatcher))
      .transpose()?;

    self
      .origin_covers
      .extend(self.destination_cover.replace(destination));
    self.origin_covers.extend(origin);

    Ok(true)
  }

  /// Makes a hidden cover showing `image` over `rect`.
  fn make_cover(
    &self,
    (rect, image): (Rect, CFRetained<CGImage>),
    dispatcher: &Dispatcher,
  ) -> crate::Result<Cover> {
    let display_height = self.display_height;
    dispatcher.dispatch_sync(|| -> crate::Result<Cover> {
      // SAFETY: `Dispatcher::dispatch_sync` runs on the main thread.
      let mtm = unsafe { MainThreadMarker::new_unchecked() };

      let ns_window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
          NSWindow::alloc(mtm),
          rect.flip_y(display_height).into(),
          NSWindowStyleMask::Borderless,
          NSBackingStoreType::Buffered,
          false,
        )
      };

      // Not opaque, though its image is: an opaque window would count
      // as hiding the application's window beneath it, and an
      // application that knows it is hidden stops drawing.
      ns_window.setBackgroundColor(Some(&NSColor::clearColor()));
      ns_window.setOpaque(false);
      ns_window.setIgnoresMouseEvents(true);
      ns_window.setAnimationBehavior(NSWindowAnimationBehavior::None);

      // SAFETY: As for the overlay's window: the `Retained` field
      // releases it, so closing must not.
      unsafe { ns_window.setReleasedWhenClosed(false) };

      let content_view =
        ns_window.contentView().ok_or(crate::Error::Platform(
          "NSWindow must have a content view.".to_string(),
        ))?;
      content_view.setWantsLayer(true);
      let root_layer =
        content_view.layer().ok_or(crate::Error::Platform(
          "Layer must exist after `setWantsLayer`.".to_string(),
        ))?;

      let layer = CALayer::new();
      // SAFETY: `CGImageRef` is accepted by `CALayer::contents`.
      unsafe {
        layer.setContents(Some(
          &*std::ptr::from_ref::<CGImage>(&image).cast::<AnyObject>(),
        ));
      };

      CATransaction::begin();
      CATransaction::setDisableActions(true);
      layer.setFrame(CGRect::new(
        CGPoint::ZERO,
        CGSize {
          width: f64::from(rect.width()),
          height: f64::from(rect.height()),
        },
      ));
      layer.setHidden(true);
      CATransaction::commit();
      root_layer.addSublayer(&layer);

      // Above every application's window and beneath the overlays.
      ns_window.setLevel(NSFloatingWindowLevel - 1);
      ns_window.orderFrontRegardless();

      let image = ImageLayer(layer.clone());
      Ok(Cover {
        views: ThreadBound::new(
          OverlayViews { ns_window, layer },
          dispatcher.clone(),
        ),
        image,
      })
    })?
  }

  /// Implements [`AnimationWindow::show_covers`].
  pub(crate) fn show_covers(&self) {
    for cover in self.destination_cover.iter().chain(&self.origin_covers) {
      cover.image.show();
    }
  }

  /// Implements [`AnimationWindow::uncover_origin`].
  pub(crate) fn uncover_origin(&mut self) {
    for cover in self.origin_covers.drain(..) {
      cover.image.hide();
      cover.views.drop_async();
    }
  }

  /// Whether companions were captured into this image.
  pub(crate) fn has_companions(&self) -> bool {
    self.extent.is_some()
  }

  /// Implements [`AnimationWindow::destroy`].
  ///
  /// Hides the image at once, then queues the close and release for the
  /// main thread and returns without waiting for it, so a failure to
  /// close is not reported. The overlay is off the screen from the first
  /// step, however long the main thread takes to reach the second.
  /// Destroying an overlay that is already destroyed does nothing.
  #[allow(clippy::unnecessary_wraps)]
  pub(crate) fn destroy(&mut self) -> crate::Result<()> {
    if let Some(image) = self.image.take() {
      image.hide();
    }
    self.uncover_origin();
    if let Some(cover) = self.destination_cover.take() {
      cover.image.hide();
      cover.views.drop_async();
    }

    if let Some(views) = self.views.take() {
      views.drop_async();
    }

    Ok(())
  }

  /// Updates the `CALayer` position and opacity within the window.
  ///
  /// The window's frame isn't changed; only the layer with the screen
  /// screen capture is updated.
  ///
  /// Shared by [`AnimationWindow::new`] and [`AnimationWindow::update`].
  /// Must be called inside `AnimationContext::transaction`.
  fn update_layer(
    layer: &Retained<CALayer>,
    inner_rect: &Rect,
    outer_rect: &Rect,
    extent: Option<&CaptureExtent>,
    opacity: Option<&OpacityValue>,
  ) {
    layer.setFrame(Self::layer_frame(inner_rect, outer_rect, extent));

    if let Some(opacity) = opacity {
      layer.setOpacity(opacity.0);
    }
  }

  /// Converts the screen rect the source is drawn at to the layer's
  /// frame within the window, in AppKit coordinates (bottom-left origin).
  ///
  /// With an `extent` the layer's image reaches past the source, so the
  /// layer covers the rect that image maps to.
  fn layer_frame(
    inner_rect: &Rect,
    outer_rect: &Rect,
    extent: Option<&CaptureExtent>,
  ) -> CGRect {
    let image_rect = extent.map(|extent| extent.image_rect(inner_rect));
    let inner_rect = image_rect.as_ref().unwrap_or(inner_rect);

    // `inner_rect` needs to be positioned relative to the window's frame.
    let offset_rect = Rect::from_xy(
      inner_rect.x() - outer_rect.x(),
      inner_rect.y() - outer_rect.y(),
      inner_rect.width(),
      inner_rect.height(),
    );

    offset_rect.flip_y(outer_rect.height()).into()
  }
}

impl Drop for AnimationWindow {
  /// Hands the views to the main thread to close and release, instead of
  /// waiting there for each of them.
  fn drop(&mut self) {
    let _ = self.destroy();
  }
}

/// A screen capture of a window, taken before its overlay exists.
pub(crate) struct AnimationCapture {
  frame: CapturedFrame,
}

/// A screen capture of a window, with its companions when it has any.
struct CapturedFrame {
  cg_image: CFRetained<CGImage>,

  /// Where the image stood on screen, when it includes companions.
  /// `None` for an image of exactly the window.
  extent: Option<CaptureExtent>,
}

impl CapturedFrame {
  /// Captures a single frame of a given window.
  ///
  /// A window with companions on screen is captured together with them,
  /// so a border ring travels with the window's image. Anything else is
  /// captured alone, as is every window when the companion search fails.
  fn new(
    window_id: WindowId,
    windows: &OnScreenWindows,
  ) -> crate::Result<Self> {
    if let Some(captured) = Self::with_companions(window_id, windows) {
      return Ok(captured);
    }
    Self::alone(window_id)
  }

  /// Captures the window alone, bounded by its own frame.
  #[allow(deprecated)]
  fn alone(window_id: WindowId) -> crate::Result<Self> {
    // Use `CGRectNull` to capture the minimum rectangle that encloses the
    // window. See: https://developer.apple.com/documentation/coregraphics/cgwindowlistcreateimage(_:_:_:_:)
    let cg_rect_null = CGRect::new(
      CGPoint {
        x: f64::INFINITY,
        y: f64::INFINITY,
      },
      CGSize::ZERO,
    );

    // NOTE: `CGWindowListCreateImage` is deprecated, but functional.
    // ScreenCaptureKit is recommended instead, see:
    // https://developer.apple.com/documentation/screencapturekit/scwindow
    NativeCallStats::record(NativeCall::ScreenCapture);
    let image = CGWindowListCreateImage(
      cg_rect_null,
      CGWindowListOption::OptionIncludingWindow,
      window_id.0,
      // `BestResolution` captures at the display's backing scale, so a
      // 1478x1628 window on a 2x panel is ~2956x3256 px — around 38MB,
      // taken synchronously before the animation clock starts. A swap
      // pays it twice in a row, which reads as a stall before the slide.
      // Logical resolution is a quarter of the data; the ghost is softer
      // than the real window for the length of the tween.
      CGWindowImageOption::NominalResolution
        .union(CGWindowImageOption::BoundsIgnoreFraming),
    )
    .ok_or(crate::Error::Platform(
      "Failed to create window screenshot.".to_string(),
    ))?;

    Ok(Self {
      cg_image: image,
      extent: None,
    })
  }

  /// Captures the window and its on-screen companions as one image.
  ///
  /// Returns `None` when the window has no companions on screen or any
  /// step fails; the caller then captures the window alone.
  #[allow(deprecated)]
  fn with_companions(
    window_id: WindowId,
    windows: &OnScreenWindows,
  ) -> Option<Self> {
    let windows = windows.listed()?;
    let frame = windows
      .iter()
      .find(|window| window.id == window_id.0)?
      .bounds
      .clone();

    let companions = windows
      .iter()
      .filter(|window| {
        window.owner == COMPANION_OWNER
          && companion::decorates(&window.bounds, &frame)
      })
      .collect::<Vec<_>>();
    if companions.is_empty() {
      return None;
    }

    let bounds = companions
      .iter()
      .fold(frame.clone(), |bounds, window| bounds.union(&window.bounds));

    // The array holds window IDs themselves, not pointers to them, which
    // is the form `CGWindowListCreateImageFromArray` documents.
    let mut ids = std::iter::once(window_id.0)
      .chain(companions.iter().map(|window| window.id))
      .map(|id| id as usize as *const std::ffi::c_void)
      .collect::<Vec<_>>();
    let count = isize::try_from(ids.len()).ok()?;
    // SAFETY: `ids` holds `count` values and outlives the call. Null
    // callbacks make the array store them as plain values, with nothing
    // to retain or release.
    let array = unsafe {
      CFArray::new(None, ids.as_mut_ptr(), count, std::ptr::null())
    }?;

    // SAFETY: The array holds window IDs, as the function requires.
    // Nominal resolution for the reason given in `alone`.
    NativeCallStats::record(NativeCall::ScreenCapture);
    let image = unsafe {
      CGWindowListCreateImageFromArray(
        bounds.clone().into(),
        &array,
        CGWindowImageOption::NominalResolution,
      )
    }?;

    Some(Self {
      cg_image: image,
      extent: Some(CaptureExtent { frame, bounds }),
    })
  }
}

/// One on-screen window, as the window server lists it.
struct ListedWindow {
  id: u32,
  /// Name of the owning process; empty when the server omits it.
  owner: String,
  /// Bounds in screen coordinates.
  bounds: Rect,
}

/// Whether a companion decorates `source` where it stands in `windows`.
///
/// `true` when `source` is not listed, so a source that is gone never
/// holds its overlay.
fn companions_revealed_in(
  windows: &[ListedWindow],
  source: WindowId,
) -> bool {
  let Some(source) = windows.iter().find(|window| window.id == source.0)
  else {
    return true;
  };
  windows.iter().any(|window| {
    window.owner == COMPANION_OWNER
      && companion::decorates(&window.bounds, &source.bounds)
  })
}

/// Lists every on-screen window, front to back.
///
/// Windows the server describes incompletely are skipped. Returns `None`
/// when the list itself is unavailable.
fn on_screen_windows() -> Option<Vec<ListedWindow>> {
  NativeCallStats::record(NativeCall::WindowListFull);
  let windows =
    CGWindowListCopyWindowInfo(CGWindowListOption::OptionOnScreenOnly, 0)?;
  // SAFETY: Window services returns dictionaries with string keys and
  // Core Foundation values, retaining all of those objects.
  let windows =
    unsafe { windows.cast_unchecked::<CFDictionary<CFString, CFType>>() };

  Some(
    windows
      .iter()
      .filter_map(|info| {
        // SAFETY: The framework provides these immutable dictionary keys.
        let (number, owner, bounds) = unsafe {
          (kCGWindowNumber, kCGWindowOwnerName, kCGWindowBounds)
        };
        let id = info
          .get(number)?
          .downcast_ref::<CFNumber>()?
          .as_i64()
          .and_then(|id| u32::try_from(id).ok())?;
        let owner = info
          .get(owner)
          .and_then(|owner| {
            owner.downcast_ref::<CFString>().map(ToString::to_string)
          })
          .unwrap_or_default();
        let dictionary = info.get(bounds)?;
        let dictionary = dictionary.downcast_ref::<CFDictionary>()?;
        let mut rect = CGRect::ZERO;
        // SAFETY: The checked dictionary is retained and `rect` is a live
        // output rectangle for the duration of this synchronous call.
        unsafe {
          CGRectMakeWithDictionaryRepresentation(
            Some(dictionary),
            &raw mut rect,
          )
        }
        .then(|| ListedWindow {
          id,
          owner,
          bounds: rect.into(),
        })
      })
      .collect(),
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A capture that includes a ring keeps the ring around the window
  /// wherever the window is drawn, and maps back to the window's rect.
  #[test]
  fn extent_carries_companions_through_the_transform() {
    let extent = CaptureExtent {
      frame: Rect::from_xy(100, 100, 800, 600),
      bounds: Rect::from_ltrb(96, 96, 904, 704),
    };

    let moved = Rect::from_xy(1000, 300, 800, 600);
    let image = extent.image_rect(&moved);
    assert_eq!(image, Rect::from_ltrb(996, 296, 1804, 904));
    assert_eq!(extent.source_rect(&image), moved);

    // Scaled to half size, the ring's reach halves with the window.
    let scaled = Rect::from_xy(0, 0, 400, 300);
    assert_eq!(
      extent.image_rect(&scaled),
      Rect::from_ltrb(-2, -2, 402, 302)
    );
  }

  /// A window that no captured source shares an id with, so a capture
  /// stops at the window search and takes no screenshot.
  const ABSENT: WindowId = WindowId(u32::MAX);

  /// The full-list calls made while `work` runs.
  ///
  /// Reads a process-wide counter, so these tests hold only under
  /// `--test-threads=1`.
  fn full_lists_during(work: impl FnOnce()) -> u64 {
    let before = NativeCallStats::snapshot();
    work();
    NativeCallStats::snapshot().since(&before).window_list_full
  }

  /// A batch of captures sharing one snapshot lists the windows once,
  /// not once per capture.
  #[test]
  fn concurrent_captures_share_one_listing() {
    const WINDOWS: u64 = 8;

    let shared = full_lists_during(|| {
      let snapshot = OnScreenWindows::default();
      std::thread::scope(|scope| {
        for _ in 0..WINDOWS {
          scope.spawn(|| {
            assert!(
              CapturedFrame::with_companions(ABSENT, &snapshot).is_none()
            );
          });
        }
      });
    });
    assert_eq!(shared, 1);

    // Without sharing, each capture lists the windows for itself.
    let separate = full_lists_during(|| {
      for _ in 0..WINDOWS {
        let snapshot = OnScreenWindows::default();
        assert!(
          CapturedFrame::with_companions(ABSENT, &snapshot).is_none()
        );
      }
    });
    assert_eq!(separate, WINDOWS);
  }

  /// Checks that share a snapshot list once however many run, and a
  /// snapshot nobody queries lists nothing.
  #[test]
  fn snapshot_lists_once_for_many_checks_and_not_at_all_unused() {
    assert_eq!(full_lists_during(|| drop(OnScreenWindows::default())), 0);

    let snapshot = OnScreenWindows::default();
    let listed = full_lists_during(|| {
      for _ in 0..16 {
        let _ = snapshot.listed();
      }
    });
    assert_eq!(listed, 1);
  }

  /// A companion that decorates the source reveals it; its absence holds
  /// the overlay; a source that is gone never does.
  #[test]
  fn companions_revealed_in_follows_the_decorating_window() {
    let listed = |id, owner: &str, bounds| ListedWindow {
      id,
      owner: owner.to_string(),
      bounds,
    };
    let source = Rect::from_xy(100, 100, 800, 600);
    let band = Rect::from_ltrb(96, 96, 116, 704);

    let with_ring = [
      listed(1, "mover-borders", band.clone()),
      listed(2, "Terminal", source.clone()),
    ];
    assert!(companions_revealed_in(&with_ring, WindowId(2)));

    // A band of another owner is not a companion.
    let other_owner = [
      listed(1, "Other", band),
      listed(2, "Terminal", source.clone()),
    ];
    assert!(!companions_revealed_in(&other_owner, WindowId(2)));

    let bare = [listed(2, "Terminal", source)];
    assert!(!companions_revealed_in(&bare, WindowId(2)));
    assert!(companions_revealed_in(&bare, WindowId(3)));
  }
}
