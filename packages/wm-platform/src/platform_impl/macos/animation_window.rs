use std::time::Duration;

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
  kCGWindowBounds, kCGWindowNumber, kCGWindowOwnerName, CGImage,
  CGRectMakeWithDictionaryRepresentation, CGWindowImageOption,
  CGWindowListCopyWindowInfo, CGWindowListCreateImage,
  CGWindowListCreateImageFromArray, CGWindowListOption,
};
use objc2_foundation::{ns_string, NSArray, NSNumber, NSString, NSValue};
use objc2_quartz_core::{
  kCAAnimationLinear, CAKeyframeAnimation, CALayer, CAMediaTiming,
  CAMediaTimingFunction, CATransaction,
};

use crate::{
  companion, Dispatcher, EasingFunction, NativeWindow, OpacityValue, Rect,
  ThreadBound, WindowId,
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

/// Platform-specific implementation of [`AnimationContext`].
pub(crate) struct AnimationContext;

impl AnimationContext {
  /// Implements [`AnimationContext::new`].
  #[allow(clippy::unnecessary_wraps)]
  pub(crate) fn new(_dispatcher: &Dispatcher) -> crate::Result<Self> {
    Ok(Self)
  }

  /// Implements [`AnimationContext::capture_frame`].
  #[allow(clippy::unused_self)]
  pub(crate) fn capture_frame(
    &self,
    window_id: WindowId,
  ) -> crate::Result<AnimationCapture> {
    Ok(AnimationCapture {
      frame: CapturedFrame::new(window_id)?,
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

/// Platform-specific implementation of [`AnimationWindow`].
pub(crate) struct AnimationWindow {
  ns_window: ThreadBound<Retained<NSWindow>>,
  layer: ThreadBound<Retained<CALayer>>,

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
    type DispatchResult = crate::Result<(
      ThreadBound<Retained<NSWindow>>,
      ThreadBound<Retained<CALayer>>,
      i32,
    )>;

    let captured = capture.frame;
    let extent = captured.extent.clone();
    let layer_extent = extent.clone();

    let (ns_window, layer, display_height) =
      dispatcher.dispatch_sync(|| -> DispatchResult {
        // SAFETY: `Dispatcher::dispatch_sync` runs on the main thread.
        let mtm = unsafe { MainThreadMarker::new_unchecked() };

        // Get height of the primary display, needed for CG<->AppKit
        // coordinate conversion.
        let display_height =
          dispatcher.primary_display()?.bounds()?.height();

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

        Ok((
          ThreadBound::new(ns_window, dispatcher.clone()),
          ThreadBound::new(layer, dispatcher.clone()),
          display_height,
        ))
      })??;

    Ok(Self {
      ns_window,
      layer,
      display_height,
      outer_rect: outer_rect.clone(),
      extent,
      source_id: window.id(),
    })
  }

  /// Implements [`AnimationWindow::resize`].
  pub(crate) fn resize(&mut self, outer_rect: &Rect) -> crate::Result<()> {
    self.outer_rect = outer_rect.clone();

    self.ns_window.with(|ns_window| {
      ns_window.setFrame_display(
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
    self.layer.with(|layer| {
      Self::update_layer(
        layer,
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

    self.layer.with(move |layer| {
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

    self.layer.with(move |layer| {
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
    self.layer.with(|layer| {
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
    })
  }

  /// Samples the presentation layer in global screen coordinates.
  pub(crate) fn current_frame(&self) -> crate::Result<Option<Rect>> {
    self.layer.with(|layer| {
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
        self.extent.as_ref().map_or_else(
          || image.clone(),
          |extent| extent.source_rect(&image),
        )
      })
    })
  }

  /// Implements [`AnimationWindow::companions_revealed`].
  ///
  /// `true` for an image without companions, which has nothing to hand
  /// back. Otherwise `true` once a companion decorates the source where
  /// it now stands, or when the source or the window list is gone, so a
  /// failure never holds the overlay.
  pub(crate) fn companions_revealed(&self) -> bool {
    if self.extent.is_none() {
      return true;
    }
    let Some(windows) = on_screen_windows() else {
      return true;
    };
    let Some(source) =
      windows.iter().find(|window| window.id == self.source_id.0)
    else {
      return true;
    };
    windows.iter().any(|window| {
      window.owner == COMPANION_OWNER
        && companion::decorates(&window.bounds, &source.bounds)
    })
  }

  /// Implements [`AnimationWindow::destroy`].
  pub(crate) fn destroy(&mut self) -> crate::Result<()> {
    self.ns_window.with(|ns_window| ns_window.close())
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
  fn new(window_id: WindowId) -> crate::Result<Self> {
    if let Some(captured) = Self::with_companions(window_id) {
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
  fn with_companions(window_id: WindowId) -> Option<Self> {
    let windows = on_screen_windows()?;
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

/// Lists every on-screen window, front to back.
///
/// Windows the server describes incompletely are skipped. Returns `None`
/// when the list itself is unavailable.
fn on_screen_windows() -> Option<Vec<ListedWindow>> {
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
}
