use std::{mem::ManuallyDrop, ops::Deref, sync::Arc};

use objc2::{rc::Retained, MainThreadMarker};
use objc2_app_kit::NSScreen;
use objc2_core_foundation::{CFRetained, CGRect, CFUUID};
use objc2_core_graphics::{
  CGDirectDisplayID, CGDisplayBounds, CGDisplayCopyDisplayMode,
  CGDisplayMirrorsDisplay, CGDisplayMode, CGDisplayRotation, CGError,
  CGGetActiveDisplayList, CGGetOnlineDisplayList, CGMainDisplayID,
};
use objc2_foundation::{ns_string, NSNumber};

use crate::{
  platform_impl::ffi, ConnectionState, Dispatcher, DisplayDeviceId,
  DisplayId, DisplayProperties, MirroringState, NativeCall,
  NativeCallStats, Point, Rect, ThreadBound,
};

/// An `NSScreen` that releases without making the dropping thread wait.
///
/// A bare `ThreadBound` hops to the main thread synchronously when it is
/// dropped, which the window manager thread would pay for every display
/// it drops. This hands the release to the main thread and moves on.
#[derive(Debug)]
struct ScreenHandle(ManuallyDrop<ThreadBound<Retained<NSScreen>>>);

impl ScreenHandle {
  /// Takes ownership of a screen bound to the main thread.
  fn new(screen: ThreadBound<Retained<NSScreen>>) -> Self {
    Self(ManuallyDrop::new(screen))
  }
}

impl Deref for ScreenHandle {
  type Target = ThreadBound<Retained<NSScreen>>;

  fn deref(&self) -> &Self::Target {
    &self.0
  }
}

impl Drop for ScreenHandle {
  fn drop(&mut self) {
    // SAFETY: `self.0` is never used again after this point.
    let screen = unsafe { ManuallyDrop::take(&mut self.0) };
    screen.drop_async();
  }
}

/// Converts a Core Graphics rectangle into a `Rect`, truncating to whole
/// pixels.
#[allow(clippy::cast_possible_truncation)]
fn rect_from_cg(rect: CGRect) -> Rect {
  Rect::from_xy(
    rect.origin.x as i32,
    rect.origin.y as i32,
    rect.size.width as i32,
    rect.size.height as i32,
  )
}

/// Gets the height of the primary display.
///
/// Answers in process, without a hop to the main thread or a walk over
/// the screens. Needed for converting between Core Graphics coordinates
/// (top-left origin) and AppKit's (bottom-left origin).
pub(crate) fn primary_display_height() -> i32 {
  rect_from_cg(CGDisplayBounds(CGMainDisplayID())).height()
}

/// Gets the working area of a screen in the same coordinate space as
/// `CGDisplayBounds`.
fn screen_working_area(screen: &NSScreen) -> Rect {
  let primary_display_bounds =
    rect_from_cg(CGDisplayBounds(CGMainDisplayID()));

  // Convert `NSScreen::visibleFrame` into the same coordinate space as
  // `CGDisplayBounds`.
  Rect::from(screen.visibleFrame()).flip_y(primary_display_bounds.height())
}

/// Gets the scale factor of a screen.
#[allow(clippy::cast_possible_truncation)]
fn screen_scale_factor(screen: &NSScreen) -> f32 {
  screen.backingScaleFactor() as f32
}

/// Gets the DPI that corresponds to a scale factor.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn dpi_from_scale_factor(scale_factor: f32) -> u32 {
  (72.0 * scale_factor) as u32
}

/// Platform-specific implementation of [`Display`].
#[derive(Clone, Debug)]
pub(crate) struct Display {
  cg_display_id: CGDirectDisplayID,
  #[cfg(not(feature = "test_utils"))]
  ns_screen: Arc<ScreenHandle>,
  #[cfg(feature = "test_utils")]
  ns_screen: Option<Arc<ScreenHandle>>,
}

impl Display {
  /// Creates an instance of `Display`.
  pub(crate) fn new(
    ns_screen: ThreadBound<Retained<NSScreen>>,
  ) -> crate::Result<Self> {
    let cg_display_id = ns_screen
      .with(|screen| {
        let device_description = screen.deviceDescription();

        device_description
          .objectForKey(ns_string!("NSScreenNumber"))
          .and_then(|val| {
            val.downcast_ref::<NSNumber>().map(NSNumber::as_u32)
          })
      })?
      .ok_or(crate::Error::DisplayNotFound)?;

    let ns_screen = Arc::new(ScreenHandle::new(ns_screen));
    #[cfg(feature = "test_utils")]
    let ns_screen = Some(ns_screen);
    Ok(Self {
      cg_display_id,
      ns_screen,
    })
  }

  /// Creates screenless displays for geometry tests.
  #[cfg(feature = "test_utils")]
  pub(crate) fn mock() -> Self {
    Self {
      cg_display_id: 0,
      ns_screen: None,
    }
  }

  /// Rejects native access on screenless test displays.
  #[cfg_attr(
    not(feature = "test_utils"),
    allow(clippy::unnecessary_wraps)
  )]
  fn screen(&self) -> crate::Result<&ThreadBound<Retained<NSScreen>>> {
    #[cfg(not(feature = "test_utils"))]
    {
      Ok(&self.ns_screen)
    }
    #[cfg(feature = "test_utils")]
    {
      self
        .ns_screen
        .as_deref()
        .map(Deref::deref)
        .ok_or(crate::Error::DisplayNotFound)
    }
  }

  /// Implements [`Display::id`].
  pub(crate) fn id(&self) -> DisplayId {
    DisplayId(self.cg_display_id)
  }

  /// Implements [`Display::name`].
  pub(crate) fn name(&self) -> crate::Result<String> {
    self.screen()?.with(|screen| {
      let name = screen.localizedName();
      Ok(name.to_string())
    })?
  }

  /// Implements [`Display::properties`].
  pub(crate) fn properties(&self) -> crate::Result<DisplayProperties> {
    let bounds = self.bounds()?;

    // Everything that needs the screen is read in one hop.
    self.screen()?.with(|screen| {
      let scale_factor = screen_scale_factor(screen);

      DisplayProperties {
        name: screen.localizedName().to_string(),
        bounds,
        working_area: screen_working_area(screen),
        scale_factor,
        dpi: dpi_from_scale_factor(scale_factor),
      }
    })
  }

  /// Implements [`Display::bounds`].
  #[allow(clippy::unnecessary_wraps)]
  pub(crate) fn bounds(&self) -> crate::Result<Rect> {
    Ok(rect_from_cg(CGDisplayBounds(self.cg_display_id)))
  }

  /// Implements [`Display::working_area`].
  pub(crate) fn working_area(&self) -> crate::Result<Rect> {
    self.screen()?.with(|screen| screen_working_area(screen))
  }

  /// Implements [`Display::scale_factor`].
  pub(crate) fn scale_factor(&self) -> crate::Result<f32> {
    self.screen()?.with(|screen| screen_scale_factor(screen))
  }

  /// Implements [`Display::dpi`].
  pub(crate) fn dpi(&self) -> crate::Result<u32> {
    Ok(dpi_from_scale_factor(self.scale_factor()?))
  }

  /// Implements [`Display::is_primary`].
  #[allow(clippy::unnecessary_wraps)]
  pub(crate) fn is_primary(&self) -> crate::Result<bool> {
    let main_display_id = CGMainDisplayID();
    Ok(self.cg_display_id == main_display_id)
  }

  /// Implements [`Display::devices`].
  pub(crate) fn devices(
    &self,
  ) -> crate::Result<Vec<crate::DisplayDevice>> {
    let main_device = DisplayDevice::new(
      self.cg_display_id,
      cg_display_uuid(self.cg_display_id)?,
    );

    // TODO: Get devices that are mirroring this display as well.
    Ok(vec![main_device.into()])
  }

  /// Implements [`Display::main_device`].
  pub(crate) fn main_device(&self) -> crate::Result<crate::DisplayDevice> {
    self
      .devices()?
      .into_iter()
      .find(|device| {
        matches!(
          device.mirroring_state(),
          Ok(None | Some(MirroringState::Source))
        )
      })
      .ok_or(crate::Error::DisplayNotFound)
  }

  /// Implements [`DisplayExtMacOs::cg_display_id`].
  pub(crate) fn cg_display_id(&self) -> CGDirectDisplayID {
    self.cg_display_id
  }

  /// Implements [`DisplayExtMacOs::ns_screen`].
  pub(crate) fn ns_screen(&self) -> &ThreadBound<Retained<NSScreen>> {
    self.screen().expect("Native screen unavailable.")
  }
}

impl From<Display> for crate::Display {
  fn from(display: Display) -> Self {
    crate::Display { inner: display }
  }
}

impl PartialEq for Display {
  fn eq(&self, other: &Self) -> bool {
    self.id() == other.id()
  }
}

impl Eq for Display {}

/// Platform-specific implementation of [`DisplayDevice`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DisplayDevice {
  cg_display_id: CGDirectDisplayID,
  uuid: CFRetained<CFUUID>,
}

impl DisplayDevice {
  /// Creates an instance of `DisplayDevice`.
  #[must_use]
  pub(crate) fn new(
    cg_display_id: CGDirectDisplayID,
    uuid: CFRetained<CFUUID>,
  ) -> Self {
    Self {
      cg_display_id,
      uuid,
    }
  }

  /// Implements [`DisplayDevice::id`].
  pub(crate) fn id(&self) -> DisplayDeviceId {
    // SAFETY: Can assume that the `CFUUID` is valid regardless of whether
    // the underlying display device is still alive.
    let uuid_string = CFUUID::new_string(None, Some(&self.uuid))
      .unwrap()
      .to_string();

    DisplayDeviceId(uuid_string)
  }

  /// Implements [`DisplayDevice::rotation`].
  #[allow(clippy::unnecessary_wraps)]
  pub(crate) fn rotation(&self) -> crate::Result<f32> {
    #[allow(clippy::cast_possible_truncation)]
    Ok(CGDisplayRotation(self.cg_display_id) as f32)
  }

  /// Implements [`DisplayDevice::refresh_rate`].
  pub(crate) fn refresh_rate(&self) -> crate::Result<f32> {
    // NOTE: Calling `CGDisplayModeRelease` on cleanup is not needed, since
    // it's equivalent to `CFRelease` in this case. Ref: https://developer.apple.com/documentation/coregraphics/cgdisplaymoderelease
    let display_mode = CGDisplayCopyDisplayMode(self.cg_display_id)
      .ok_or(crate::Error::DisplayModeNotFound)?;

    let refresh_rate = CGDisplayMode::refresh_rate(Some(&display_mode));

    #[allow(clippy::cast_possible_truncation)]
    Ok(refresh_rate as f32)
  }

  /// Implements [`DisplayDevice::is_builtin`].
  #[allow(clippy::unnecessary_wraps)]
  pub(crate) fn is_builtin(&self) -> crate::Result<bool> {
    // TODO: Implement this properly.
    let main_display_id = CGMainDisplayID();
    Ok(self.cg_display_id == main_display_id)
  }

  /// Implements [`DisplayDevice::connection_state`].
  #[allow(clippy::unnecessary_wraps)]
  pub(crate) fn connection_state(&self) -> crate::Result<ConnectionState> {
    let display_mode = CGDisplayCopyDisplayMode(self.cg_display_id);

    // TODO: Implement this properly.
    if display_mode.is_none() {
      Ok(ConnectionState::Disconnected)
    } else {
      Ok(ConnectionState::Active)
    }
  }

  /// Implements [`DisplayDevice::mirroring_state`].
  #[allow(clippy::unnecessary_wraps)]
  pub(crate) fn mirroring_state(
    &self,
  ) -> crate::Result<Option<MirroringState>> {
    let mirrored_display = CGDisplayMirrorsDisplay(self.cg_display_id);

    // TODO: Clean this up.
    if mirrored_display == 0 {
      // This display is not mirroring another display
      // Check if another display is mirroring this one by querying active
      // displays
      let mut displays: Vec<CGDirectDisplayID> = vec![0; 32];
      let mut display_count: u32 = 0;

      #[allow(clippy::cast_possible_truncation)]
      let result = unsafe {
        CGGetActiveDisplayList(
          displays.len() as u32,
          displays.as_mut_ptr(),
          &raw mut display_count,
        )
      };

      if result == CGError::Success {
        displays.truncate(display_count as usize);
        for &display_id in &displays {
          if display_id == self.cg_display_id {
            continue; // Skip self
          }
          let other_mirrored = CGDisplayMirrorsDisplay(display_id);
          if other_mirrored == self.cg_display_id {
            // Another display is mirroring this one, so this is the source
            return Ok(Some(MirroringState::Source));
          }
        }
      }
      Ok(None)
    } else {
      // This display is mirroring another display, so it's a target
      Ok(Some(MirroringState::Target))
    }
  }

  /// Implements [`DisplayDeviceExtMacOs::cg_display_id`].
  pub(crate) fn cg_display_id(&self) -> CGDirectDisplayID {
    self.cg_display_id
  }
}

impl From<DisplayDevice> for crate::DisplayDevice {
  fn from(device: DisplayDevice) -> Self {
    crate::DisplayDevice { inner: device }
  }
}

/// Gets the UUID for a display device from its `CGDirectDisplayID`.
///
/// This UUID is stable across reboots, whereas `CGDirectDisplayID` is not.
fn cg_display_uuid(
  cg_display_id: CGDirectDisplayID,
) -> crate::Result<CFRetained<CFUUID>> {
  let ptr =
    unsafe { ffi::CGDisplayCreateUUIDFromDisplayID(cg_display_id) };

  ptr.map(|ptr| unsafe { CFRetained::from_raw(ptr) }).ok_or(
    crate::Error::InvalidPointer(
      "Failed to create UUID for display device".to_string(),
    ),
  )
}

/// Implements [`Dispatcher::displays`].
pub(crate) fn all_displays(
  dispatcher: &Dispatcher,
) -> crate::Result<Vec<crate::Display>> {
  dispatcher.dispatch_sync(|| {
    let mtm =
      MainThreadMarker::new().ok_or(crate::Error::NotMainThread)?;

    let mut displays = Vec::new();

    NativeCallStats::record(NativeCall::ScreenEnumeration);
    for screen in NSScreen::screens(mtm) {
      let ns_screen = ThreadBound::new(screen, dispatcher.clone());
      displays.push(Display::new(ns_screen)?.into());
    }

    Ok(displays)
  })?
}

/// Implements [`Dispatcher::display_devices`].
pub(crate) fn all_display_devices(
  _: &Dispatcher,
) -> crate::Result<Vec<crate::DisplayDevice>> {
  let mut cg_display_ids: Vec<CGDirectDisplayID> = vec![0; 32]; // Max 32 displays
  let mut display_count: u32 = 0;

  #[allow(clippy::cast_possible_truncation)]
  let result = unsafe {
    CGGetOnlineDisplayList(
      cg_display_ids.len() as u32,
      cg_display_ids.as_mut_ptr(),
      &raw mut display_count,
    )
  };

  if result != CGError::Success {
    return Err(crate::Error::DisplayEnumerationFailed);
  }

  cg_display_ids.truncate(display_count as usize);

  cg_display_ids
    .into_iter()
    .map(|cg_display_id| {
      Ok(
        DisplayDevice::new(cg_display_id, cg_display_uuid(cg_display_id)?)
          .into(),
      )
    })
    .collect()
}

/// Implements [`Dispatcher::display_from_point`].
pub(crate) fn display_from_point(
  point: &Point,
  dispatcher: &Dispatcher,
) -> crate::Result<crate::Display> {
  let displays = all_displays(dispatcher)?;

  for display in displays {
    let bounds = display.bounds()?;
    if bounds.contains_point(point) {
      return Ok(display);
    }
  }

  Err(crate::Error::DisplayNotFound)
}

/// Implements [`Dispatcher::primary_display`].
pub(crate) fn primary_display(
  dispatcher: &Dispatcher,
) -> crate::Result<crate::Display> {
  dispatcher.dispatch_sync(|| {
    let mtm =
      MainThreadMarker::new().ok_or(crate::Error::NotMainThread)?;

    // NOTE: `NSScreen::mainScreen` cannot be used as it returns the screen
    // with keyboard focus. The first screen in `NSScreen::screens` is
    // always the primary (i.e. the display containing the menu bar).
    NativeCallStats::record(NativeCall::ScreenEnumeration);
    let ns_screen = ThreadBound::new(
      NSScreen::screens(mtm)
        .into_iter()
        .next()
        .ok_or(crate::Error::DisplayNotFound)?,
      dispatcher.clone(),
    );

    Display::new(ns_screen).map(Into::into)
  })?
}

/// Bounds of the display showing most of `rect`.
///
/// `owning_display` is the bounds of the display the caller already knows
/// `rect` is placed on. It is returned as is when it wholly contains
/// `rect`, which skips walking every screen. A rect that reaches past it
/// takes the display with the largest overlap, falling back to the display
/// nearest the rect's centre, so the result matches a walk without a hint.
pub(crate) fn display_bounds_for_rect(
  rect: &Rect,
  owning_display: Option<&Rect>,
  dispatcher: &Dispatcher,
) -> crate::Result<Rect> {
  if let Some(display) = owning_display {
    if display.contains_rect(rect) {
      return Ok(display.clone());
    }
  }

  let bounds = all_displays(dispatcher)?
    .iter()
    .map(crate::Display::bounds)
    .collect::<crate::Result<Vec<_>>>()?;

  let overlapping = bounds
    .iter()
    .map(|display| (rect.intersection_area(display), display))
    .filter(|(area, _)| *area > 0)
    .max_by_key(|(area, _)| *area)
    .map(|(_, display)| display.clone());

  if let Some(display) = overlapping {
    return Ok(display);
  }

  let center = rect.center_point();

  bounds
    .into_iter()
    .min_by(|a, b| {
      a.distance_to_point(&center)
        .total_cmp(&b.distance_to_point(&center))
    })
    .ok_or(crate::Error::DisplayNotFound)
}

/// Implements [`Dispatcher::nearest_display`].
///
/// NOTE: This was benchmarked to be 400-600µs on initial retrieval and
/// 150-300µs on subsequent retrievals. Using `CGGetDisplaysWithRect` and
/// getting the corresponding `NSScreen` was found to be slightly slower
/// (700-800µs and then 200-300µs on subsequent retrievals).
pub(crate) fn nearest_display(
  native_window: &crate::NativeWindow,
  dispatcher: &Dispatcher,
) -> crate::Result<crate::Display> {
  dispatcher.dispatch_sync(|| {
    // Get the window's frame in screen coordinates.
    let window_frame = native_window.frame()?;

    let screens = all_displays(dispatcher)?;
    let mut best_screen = None;
    let mut max_intersection_area = 0;

    // TODO: Clean this up.
    // Iterate through all screens to find the one with the largest
    // intersection with the window.
    for screen in screens {
      let screen_frame = screen.bounds()?;

      // Calculate intersection area.
      let intersection_x = i32::max(window_frame.x(), screen_frame.x());
      let intersection_y = i32::max(window_frame.y(), screen_frame.y());
      let intersection_width = i32::min(
        window_frame.x() + window_frame.width(),
        screen_frame.x() + screen_frame.width(),
      ) - intersection_x;
      let intersection_height = i32::min(
        window_frame.y() + window_frame.height(),
        screen_frame.y() + screen_frame.height(),
      ) - intersection_y;

      // If there's a valid intersection, calculate its area.
      if intersection_width > 0 && intersection_height > 0 {
        let area = intersection_width * intersection_height;
        if area > max_intersection_area {
          max_intersection_area = area;
          best_screen = Some(screen);
        }
      }
    }

    // If we found a screen with intersection, use it. Otherwise, if the
    // window is off-screen, use the main screen.
    best_screen
      .or_else(|| primary_display(dispatcher).ok())
      .ok_or(crate::Error::DisplayNotFound)
  })?
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{EventLoop, NativeCallStats};

  /// Runs `body` on a worker thread against a live event loop, the way
  /// the window manager calls into the platform.
  fn with_event_loop<T: Send + 'static>(
    body: impl FnOnce(&Dispatcher) -> T + Send + 'static,
  ) -> T {
    let (event_loop, dispatcher) = EventLoop::new().unwrap();

    let thread = std::thread::spawn(move || {
      let result = body(&dispatcher);
      dispatcher.stop_event_loop().unwrap();
      result
    });

    event_loop.run().unwrap();
    thread.join().unwrap()
  }

  #[test]
  fn owning_display_skips_the_screen_walk() {
    let (bounds, spent) = with_event_loop(|dispatcher| {
      let owning = Rect::from_xy(100, 100, 1920, 1080);
      let target = Rect::from_xy(200, 200, 800, 600);

      let before = NativeCallStats::snapshot();
      let bounds =
        display_bounds_for_rect(&target, Some(&owning), dispatcher);
      (bounds, NativeCallStats::snapshot().since(&before))
    });

    assert_eq!(bounds.unwrap(), Rect::from_xy(100, 100, 1920, 1080));
    // Counters are process-wide: run with `--test-threads=1`.
    assert_eq!(spent.screen_enumerations, 0);
    assert_eq!(spent.hops, 0);
  }

  #[test]
  fn straddling_target_falls_back_to_the_screen_walk() {
    let spent = with_event_loop(|dispatcher| {
      // Overlaps the target without containing it.
      let owning = Rect::from_xy(500, 200, 1920, 1080);
      let target = Rect::from_xy(200, 200, 800, 600);

      let before = NativeCallStats::snapshot();
      let _ = display_bounds_for_rect(&target, Some(&owning), dispatcher);
      NativeCallStats::snapshot().since(&before)
    });

    assert_eq!(spent.screen_enumerations, 1);
  }

  #[test]
  fn unrelated_owning_display_falls_back_to_the_screen_walk() {
    let spent = with_event_loop(|dispatcher| {
      // Far from any real display.
      let owning = Rect::from_xy(-90_000, -90_000, 100, 100);
      let target = Rect::from_xy(200, 200, 800, 600);

      let before = NativeCallStats::snapshot();
      let _ = display_bounds_for_rect(&target, Some(&owning), dispatcher);
      NativeCallStats::snapshot().since(&before)
    });

    assert_eq!(spent.screen_enumerations, 1);
  }

  #[test]
  fn no_owning_display_walks_the_screens_once() {
    let spent = with_event_loop(|dispatcher| {
      let target = Rect::from_xy(200, 200, 800, 600);

      let before = NativeCallStats::snapshot();
      let _ = display_bounds_for_rect(&target, None, dispatcher);
      NativeCallStats::snapshot().since(&before)
    });

    assert_eq!(spent.screen_enumerations, 1);
  }

  #[test]
  fn properties_match_the_individual_getters_in_one_hop() {
    let (properties, expected, spent) = with_event_loop(|dispatcher| {
      let display = dispatcher.displays().unwrap().remove(0);

      let before = NativeCallStats::snapshot();
      let properties = display.properties().unwrap();
      let spent = NativeCallStats::snapshot().since(&before);

      let expected = DisplayProperties {
        name: display.name().unwrap(),
        bounds: display.bounds().unwrap(),
        working_area: display.working_area().unwrap(),
        scale_factor: display.scale_factor().unwrap(),
        dpi: display.dpi().unwrap(),
      };

      (properties, expected, spent)
    });

    assert_eq!(properties, expected);
    // Counters are process-wide: run with `--test-threads=1`.
    assert_eq!(spent.hops, 1);
  }

  #[test]
  fn topology_read_takes_one_hop_per_display() {
    let (displays, spent) = with_event_loop(|dispatcher| {
      let before = NativeCallStats::snapshot();

      // What a topology change does: list, read and drop every display.
      let displays = dispatcher.sorted_displays().unwrap();
      let count = displays.len();
      for display in &displays {
        display.properties().unwrap();
      }
      drop(displays);

      (count, NativeCallStats::snapshot().since(&before))
    });

    // One hop to list the displays, one more to read each of them, and
    // none to drop them.
    assert_eq!(spent.hops, 1 + displays as u64);
  }
}
