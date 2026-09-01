use std::time::Duration;

use objc2::{
  rc::Retained, runtime::AnyObject, MainThreadMarker, MainThreadOnly,
};
use objc2_app_kit::{
  NSBackingStoreType, NSColor, NSFloatingWindowLevel, NSWindow,
  NSWindowAnimationBehavior, NSWindowOrderingMode, NSWindowStyleMask,
};
use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::CGImage;
#[allow(deprecated)]
use objc2_core_graphics::{
  CGWindowImageOption, CGWindowListCreateImage, CGWindowListOption,
};
use objc2_quartz_core::{CALayer, CAMediaTimingFunction, CATransaction};

use crate::{
  Dispatcher, EasingFunction, NativeWindow, OpacityValue, Rect,
  ThreadBound, WindowId,
};

/// Cubic Bézier control points for an [`EasingFunction`].
///
/// Core Animation expresses easing as a timing curve rather than a
/// per-frame function, so each variant maps to the curve that matches
/// `EasingFunction::apply` most closely.
const fn control_points(easing: &EasingFunction) -> (f32, f32, f32, f32) {
  match easing {
    EasingFunction::Linear => (0.0, 0.0, 1.0, 1.0),
    EasingFunction::EaseIn => (0.55, 0.085, 0.68, 0.53),
    EasingFunction::EaseOut => (0.25, 0.46, 0.45, 0.94),
    EasingFunction::EaseInOut => (0.455, 0.03, 0.515, 0.955),
    EasingFunction::EaseInCubic => (0.55, 0.055, 0.675, 0.19),
    EasingFunction::EaseOutCubic => (0.215, 0.61, 0.355, 1.0),
    EasingFunction::EaseInOutCubic => (0.645, 0.045, 0.355, 1.0),
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
}

impl AnimationWindow {
  /// Implements [`AnimationWindow::new`].
  pub(crate) fn new(
    _context: &AnimationContext,
    window: &NativeWindow,
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

    let captured = CapturedFrame::new(window.id())?;

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
      Self::update_layer(layer, inner_rect, &self.outer_rect, opacity);
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
  pub(crate) fn animate_to(
    &self,
    target_rect: &Rect,
    duration: Duration,
    easing: &EasingFunction,
    opacity: Option<&OpacityValue>,
  ) -> crate::Result<()> {
    let outer_rect = self.outer_rect.clone();
    let target_rect = target_rect.clone();
    let easing = easing.clone();
    let opacity = opacity.copied();

    self.layer.with(move |layer| {
      let (c1x, c1y, c2x, c2y) = control_points(&easing);

      CATransaction::begin();
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
        opacity.as_ref(),
      );
      CATransaction::commit();
    })
  }

  /// Implements [`AnimationWindow::destroy`].
  pub(crate) fn destroy(self) -> crate::Result<()> {
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
    opacity: Option<&OpacityValue>,
  ) {
    // `inner_rect` needs to be positioned relative to the window's frame.
    let offset_rect = Rect::from_xy(
      inner_rect.x() - outer_rect.x(),
      inner_rect.y() - outer_rect.y(),
      inner_rect.width(),
      inner_rect.height(),
    );

    // `setFrame` expects AppKit coordinates (bottom-left origin).
    layer.setFrame(offset_rect.flip_y(outer_rect.height()).into());

    if let Some(opacity) = opacity {
      layer.setOpacity(opacity.0);
    }
  }
}

/// A screen capture of a window via `CGWindowListCreateImage`.
struct CapturedFrame {
  cg_image: CFRetained<CGImage>,
}

impl CapturedFrame {
  /// Captures a single frame of a given window.
  #[allow(deprecated)]
  fn new(window_id: WindowId) -> crate::Result<Self> {
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

    Ok(Self { cg_image: image })
  }
}
