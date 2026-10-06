//! Desktop integration checks using only explicitly unmanaged fixtures.

use std::{sync::mpsc, time::Duration};

use windows::{
  core::{w, PCWSTR},
  Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    Graphics::{
      Dwm::{
        DwmQueryThumbnailSourceSize, DwmRegisterThumbnail,
        DwmUnregisterThumbnail,
      },
      Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC,
        DeleteObject, GetDC, GetPixel, GetStockObject, ReleaseDC,
        SelectObject, UpdateWindow, BLACK_BRUSH, CAPTUREBLT,
        GET_STOCK_OBJECT_FLAGS, HBRUSH, SRCCOPY, WHITE_BRUSH,
      },
    },
    UI::WindowsAndMessaging::{
      CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
      GetLayeredWindowAttributes, PeekMessageW, RegisterClassW,
      TranslateMessage, MSG, PM_REMOVE, WINDOW_EX_STYLE, WINDOW_STYLE,
      WNDCLASSW, WS_CAPTION, WS_EX_LAYERED, WS_EX_NOACTIVATE,
      WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
      WS_POPUP, WS_THICKFRAME, WS_VISIBLE,
    },
  },
};

use crate::{
  AnimationContext, AnimationWindow, ConcealMethod, CornerStyle,
  EventLoop, FrameClock, FrameSignal, NativeWindow,
  NativeWindowWindowsExt, OpacityValue, PlacementSession, Rect,
};

/// Owns a nonactivating tool window that the WM will never manage.
struct Fixture(HWND);

/// Delegates fixture messages to the system's standard painting behavior.
unsafe extern "system" fn fixture_wnd_proc(
  window: HWND,
  message: u32,
  wparam: WPARAM,
  lparam: LPARAM,
) -> LRESULT {
  // SAFETY: Windows supplied this callback's live message parameters.
  unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

impl Fixture {
  /// Creates a solid-color source or backdrop without taking focus.
  fn new(
    class: PCWSTR,
    brush: GET_STOCK_OBJECT_FLAGS,
    rect: &Rect,
    extra_style: WINDOW_EX_STYLE,
  ) -> Self {
    Self::with_frame(class, brush, rect, extra_style, WINDOW_STYLE(0))
  }

  /// Creates fixtures with explicit frame styles.
  fn with_frame(
    class: PCWSTR,
    brush: GET_STOCK_OBJECT_FLAGS,
    rect: &Rect,
    extra_style: WINDOW_EX_STYLE,
    frame_style: WINDOW_STYLE,
  ) -> Self {
    // SAFETY: System stock brushes outlive the registered window class.
    let background = HBRUSH(unsafe { GetStockObject(brush) }.0);
    let class_definition = WNDCLASSW {
      lpszClassName: class,
      lpfnWndProc: Some(fixture_wnd_proc),
      hbrBackground: background,
      ..Default::default()
    };
    // SAFETY: The class description and string remain live for the call.
    unsafe { RegisterClassW(&raw const class_definition) };
    // SAFETY: This process owns the class and creates only test fixtures.
    let handle = unsafe {
      CreateWindowExW(
        WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | extra_style,
        class,
        w!("GlazeWM presentation test fixture"),
        WS_POPUP | WS_VISIBLE | frame_style,
        rect.x(),
        rect.y(),
        rect.width(),
        rect.height(),
        None,
        None,
        None,
        None,
      )
    };
    assert_ne!(handle.0, 0, "Fixture creation failed.");
    // SAFETY: Paints the newly created fixture on its owning thread.
    unsafe { UpdateWindow(handle) }
      .ok()
      .expect("Fixture paint failed.");
    Self(handle)
  }
}

impl Drop for Fixture {
  /// Removes the fixture even when a pixel assertion fails.
  fn drop(&mut self) {
    // SAFETY: This guard owns the live window on its creation thread.
    let _ = unsafe { DestroyWindow(self.0) };
  }
}

/// Pumps fixture work while waiting for genuine compositor progress.
fn next_frame(receiver: &mpsc::Receiver<FrameSignal>, after: u64) -> u64 {
  let after =
    after.max(FrameClock::current_frame().expect("No frame serial."));
  let deadline = std::time::Instant::now() + Duration::from_secs(3);
  loop {
    let mut message = MSG::default();
    // SAFETY: The initialized message belongs to this fixture thread.
    unsafe {
      while PeekMessageW(&raw mut message, None, 0, 0, PM_REMOVE).as_bool()
      {
        let _ = TranslateMessage(&raw const message);
        DispatchMessageW(&raw const message);
      }
    }
    let remaining =
      deadline.saturating_duration_since(std::time::Instant::now());
    match receiver
      .recv_timeout(remaining)
      .expect("No compositor frame within deadline.")
    {
      FrameSignal::Presented(frame) if frame > after => return frame,
      FrameSignal::Unavailable => {
        panic!("Native compositor clock unavailable.")
      }
      _ => {}
    }
  }
}

/// Samples one screen pixel strictly inside our solid-color fixtures.
fn fixture_pixel(x: i32, y: i32) -> u32 {
  // SAFETY: Acquires and releases a screen DC on this thread.
  unsafe {
    let dc = GetDC(HWND(0));
    assert_ne!(dc.0, 0, "Screen DC unavailable.");
    let pixel = GetPixel(dc, x, y).0;
    ReleaseDC(HWND(0), dc);
    assert_ne!(pixel, u32::MAX, "Screen pixel unavailable.");
    pixel
  }
}

/// Registers an opt-in test requiring an unlocked Windows DWM desktop.
pub fn register() {
  for (name, pixels, opening) in [
    ("live_source_overlay_handoff", false, false),
    ("live_source_overlay_pixels", true, false),
    ("live_opening_overlay_handoff", false, true),
    ("live_opening_overlay_pixels", true, true),
  ] {
    libtest_mimic_collect::TestCollection::add_test(
      libtest_mimic_collect::libtest_mimic::Trial::test(name, move || {
        live_source_overlay_handoff(pixels, opening);
        Ok(())
      })
      .with_ignored_flag(true),
    );
  }
  for (name, test) in [
    (
      "live_thumbnail_decorations",
      live_thumbnail_decorations as fn(),
    ),
    ("live_native_decorations", live_native_decorations),
    ("live_opening_decorations", live_opening_decorations),
    ("live_slide_overlay_pixels", live_slide_overlay_pixels),
    (
      "live_composition_source_is_cloaked",
      live_composition_source_is_cloaked,
    ),
    (
      "live_tool_window_cloak_declines",
      live_tool_window_cloak_declines,
    ),
    (
      "live_opening_cancel_before_overlay",
      live_opening_cancel_before_overlay,
    ),
    (
      "live_parked_source_recovers_visible",
      live_parked_source_recovers_visible,
    ),
    ("live_opening_event_adoption", live_opening_event_adoption),
    ("live_opening_event_rejection", live_opening_event_rejection),
    (
      "live_opening_creation_timeout",
      live_opening_creation_timeout,
    ),
  ] {
    libtest_mimic_collect::TestCollection::add_test(
      libtest_mimic_collect::libtest_mimic::Trial::test(name, move || {
        test();
        Ok(())
      })
      .with_ignored_flag(true),
    );
  }
}

/// Reads thumbnail dimensions without showing another copy.
fn thumbnail_size(destination: HWND, source: HWND) -> (i32, i32) {
  // SAFETY: Both handles belong to fixtures.
  let thumbnail = unsafe { DwmRegisterThumbnail(destination, source) }
    .expect("Thumbnail registration failed.");
  // SAFETY: Registration remains live during query.
  let size = unsafe { DwmQueryThumbnailSourceSize(thumbnail) };
  // SAFETY: Releases this probe's registration only.
  unsafe { DwmUnregisterThumbnail(thumbnail) }
    .expect("Thumbnail cleanup failed.");
  let size = size.expect("Thumbnail size unavailable.");
  (size.cx, size.cy)
}

/// Samples straight edges or corner patches.
fn decoration_pixels(rect: &Rect, corners: bool) -> Vec<u32> {
  let mut pixels = Vec::new();
  // SAFETY: Acquires this thread's screen DC.
  let dc = unsafe { GetDC(HWND(0)) };
  assert_ne!(dc.0, 0, "Screen DC unavailable.");
  // SAFETY: Captures fixture pixels into owned resources.
  let (memory, bitmap, previous, copied) = unsafe {
    let memory = CreateCompatibleDC(dc);
    let bitmap = CreateCompatibleBitmap(dc, rect.width(), rect.height());
    let previous = SelectObject(memory, bitmap);
    let copied = BitBlt(
      memory,
      0,
      0,
      rect.width(),
      rect.height(),
      dc,
      rect.x(),
      rect.y(),
      SRCCOPY | CAPTUREBLT,
    );
    (memory, bitmap, previous, copied)
  };
  for y in 0..rect.height() {
    for x in 0..rect.width() {
      let near_x = x < 12 || x >= rect.width() - 12;
      let near_y = y < 12 || y >= rect.height() - 12;
      let edge = x == 0
        || y == 0
        || x == rect.width() - 1
        || y == rect.height() - 1;
      if if corners {
        near_x && near_y
      } else {
        edge && !(near_x && near_y)
      } {
        // SAFETY: Samples only our fixture bounds.
        pixels.push(unsafe { GetPixel(memory, x, y) }.0);
      }
    }
  }
  // SAFETY: Restores selections before releasing owned resources.
  let cleaned = unsafe {
    let restored = SelectObject(memory, previous).0 == bitmap.0;
    let bitmap_deleted = DeleteObject(bitmap).as_bool();
    let memory_deleted = DeleteDC(memory).as_bool();
    let released = ReleaseDC(HWND(0), dc);
    restored && bitmap_deleted && memory_deleted && released == 1
  };
  assert!(cleaned, "Capture cleanup failed.");
  copied.expect("Screen capture failed.");
  assert!(!pixels.contains(&u32::MAX), "Screen pixel unavailable.");
  pixels
}

/// Checks decorated workspace proxies and clipping.
fn live_thumbnail_decorations() {
  check_decorations(false);
}

/// Checks decorated opening proxies and opacity.
fn live_opening_decorations() {
  check_decorations(true);
}

/// Measures decorations across thumbnail concealment states.
fn check_decorations(opening: bool) {
  let (_event_loop, dispatcher) =
    EventLoop::new().expect("Event loop unavailable.");
  let backdrop = Fixture::new(
    w!("GlazeWM.DecorationBackdrop"),
    BLACK_BRUSH,
    &Rect::from_xy(48, 48, 560, 240),
    WINDOW_EX_STYLE(0),
  );
  let source = Fixture::with_frame(
    w!("GlazeWM.DecorationSource"),
    WHITE_BRUSH,
    &Rect::from_xy(64, 64, 200, 180),
    WINDOW_EX_STYLE(0),
    WS_THICKFRAME | WS_CAPTION,
  );
  let native = NativeWindow::from_handle(source.0 .0);
  native
    .set_border_color(None)
    .expect("System border unavailable.");
  native
    .set_corner_style(&CornerStyle::Rounded)
    .expect("Rounded corners unavailable.");
  let session = PlacementSession::new(native.clone(), 1)
    .expect("Source claim failed.");
  let (sender, receiver) = mpsc::channel();
  let _clock =
    FrameClock::start(60, move |signal| sender.send(signal).is_ok());
  let mut frame = next_frame(&receiver, 0);
  frame = next_frame(&receiver, frame);
  let initial = native.frame().expect("Source frame unavailable.");
  let target =
    initial.translate_to_coordinates(initial.x() + 280, initial.y());
  let bounds = initial.union(&target);
  // Isolate images from canvas decorations.
  let outer = Rect::from_ltrb(
    bounds.left - 16,
    bounds.top - 16,
    bounds.right + 16,
    bounds.bottom + 16,
  );
  let baseline = [
    decoration_pixels(&initial, false),
    decoration_pixels(&initial, true),
  ];
  assert!(
    baseline[0]
      .iter()
      .any(|pixel| *pixel != 0 && *pixel != 0x00ff_ffff),
    "Fixture lacks a visible system border."
  );
  assert!(baseline[1].contains(&0), "Fixture lacks rounded corners.");
  let context =
    AnimationContext::new(&dispatcher).expect("Context creation failed.");
  let capture = context
    .capture_frame(native.id(), &crate::OnScreenWindows::new())
    .expect("Capture failed.");
  if opening {
    session
      .opacity(Some(OpacityValue(0.0)))
      .expect("Opening concealment failed.");
  }
  let mut overlay = AnimationWindow::new(
    &context,
    &native,
    capture,
    &target,
    &outer,
    opening.then_some(OpacityValue(0.0)),
    &dispatcher,
  )
  .expect("Overlay creation failed.");
  if opening {
    frame = next_frame(&receiver, frame);
    frame = next_frame(&receiver, frame);
    assert_eq!(
      fixture_pixel(target.center_point().x, target.center_point().y),
      0
    );
    assert_eq!(
      fixture_pixel(initial.center_point().x, initial.center_point().y),
      0
    );
    let opacity = OpacityValue(0.5);
    overlay
      .update(&target, Some(&opacity))
      .expect("Opening fade failed.");
    frame = next_frame(&receiver, frame);
    frame = next_frame(&receiver, frame);
    let faded = fixture_pixel(target.left + 12, target.top);
    for shift in [0, 8, 16] {
      let expected = ((baseline[0][0] >> shift) & 255)
        * u32::from(opacity.to_alpha())
        / 255;
      let actual = (faded >> shift) & 255;
      assert!(
        actual.abs_diff(expected) <= 1,
        "Border did not fade with content."
      );
    }
    let scaled = target.scale_from_center(0.94);
    overlay
      .update(&scaled, Some(&OpacityValue(1.0)))
      .expect("Opening scale failed.");
    frame = next_frame(&receiver, frame);
    frame = next_frame(&receiver, frame);
    assert_ne!(fixture_pixel(scaled.left, scaled.center_point().y), 0);
    assert_eq!(fixture_pixel(scaled.left, scaled.top), 0);
    assert_eq!(fixture_pixel(target.left, target.center_point().y), 0);
  }
  let mut matches = true;
  let mut samples = Vec::new();
  for (label, opacity) in [
    ("visible", opening.then_some(OpacityValue(0.0))),
    ("alpha", Some(OpacityValue(0.0))),
    ("restored", None),
  ] {
    session
      .opacity(opacity)
      .expect("Opacity transition failed.");
    overlay
      .update(&target, Some(&OpacityValue(1.0)))
      .expect("Overlay update failed.");
    frame = next_frame(&receiver, frame);
    frame = next_frame(&receiver, frame);
    let size = thumbnail_size(backdrop.0, source.0);
    println!(
      "{label}: window={:?} frame={:?} thumbnail={size:?} insets={:?}",
      native
        .frame_with_shadows()
        .expect("Window bounds unavailable."),
      native.frame().expect("Frame bounds unavailable."),
      native.shadow_borders().expect("Source insets unavailable."),
    );
    let sample = [
      decoration_pixels(&target, false),
      decoration_pixels(&target, true),
    ];
    for (index, corners) in [false, true].into_iter().enumerate() {
      let actual = &sample[index];
      let differences = baseline[index]
        .iter()
        .zip(actual)
        .filter(|(expected, actual)| expected != actual)
        .count();
      println!(
        "{label}: corners={corners} changed={differences}/{}",
        actual.len()
      );
      matches &= differences == 0;
    }
    samples.push(sample);
  }
  println!("Concealment changed pixels: {}", samples[0] != samples[1]);
  println!("Restoration changed pixels: {}", samples[0] != samples[2]);
  let restored = [
    decoration_pixels(&initial, false),
    decoration_pixels(&initial, true),
  ];
  let stable = baseline == restored;
  println!("Source baseline remained stable: {stable}");
  for index in 0..2 {
    let changes = baseline[index]
      .iter()
      .zip(&restored[index])
      .enumerate()
      .filter(|(_, (before, after))| before != after)
      .collect::<Vec<_>>();
    println!("Source section {index}: changed={}", changes.len());
    for (offset, (before, after)) in changes.iter().take(8) {
      println!("Source pixel {offset}: {before:06x} -> {after:06x}");
    }
  }
  session
    .opacity(Some(OpacityValue(0.0)))
    .expect("Slide concealment failed.");
  let clipped =
    target.translate_to_coordinates(outer.right - 40, target.top);
  overlay
    .update(&clipped, None)
    .expect("Clipped slide failed.");
  frame = next_frame(&receiver, frame);
  frame = next_frame(&receiver, frame);
  assert_eq!(
    fixture_pixel(clipped.left + 12, clipped.top),
    baseline[0][0]
  );
  assert_eq!(fixture_pixel(clipped.left, clipped.top), 0);
  assert_eq!(fixture_pixel(outer.right + 2, clipped.center_point().y), 0);
  assert_eq!(
    fixture_pixel(outer.right - 1, clipped.center_point().y),
    0x00ff_ffff
  );
  overlay
    .update(
      &clipped.translate_to_coordinates(outer.right + 1, clipped.top),
      None,
    )
    .expect("Slide exit failed.");
  frame = next_frame(&receiver, frame);
  next_frame(&receiver, frame);
  assert_eq!(fixture_pixel(clipped.left + 12, clipped.top), 0);
  overlay.destroy().expect("Overlay cleanup failed.");
  session.release().expect("Source recovery failed.");
  assert!(stable, "Source baseline changed during probing.");
  assert!(
    matches,
    "Thumbnail decorations differ; inspect probe output."
  );
}

/// Native movement retains system border and corners.
fn live_native_decorations() {
  let _backdrop = Fixture::new(
    w!("GlazeWM.NativeBackdrop"),
    BLACK_BRUSH,
    &Rect::from_xy(48, 48, 560, 300),
    WINDOW_EX_STYLE(0),
  );
  let source = Fixture::with_frame(
    w!("GlazeWM.NativeSource"),
    WHITE_BRUSH,
    &Rect::from_xy(64, 64, 200, 180),
    WINDOW_EX_STYLE(0),
    WS_THICKFRAME | WS_CAPTION,
  );
  let native = NativeWindow::from_handle(source.0 .0);
  let session = PlacementSession::new(native.clone(), 1)
    .expect("Source claim failed.");
  session
    .set_decorations(None, &CornerStyle::Rounded)
    .expect("Source decorations failed.");
  let (sender, receiver) = mpsc::channel();
  let _clock =
    FrameClock::start(60, move |signal| sender.send(signal).is_ok());
  let mut frame = next_frame(&receiver, 0);
  frame = next_frame(&receiver, frame);
  let initial = session
    .observed_frame()
    .expect("Source bounds unavailable.");
  let visible = native.frame().expect("Visible bounds unavailable.");
  let edges = decoration_pixels(&visible, false);
  let corners = decoration_pixels(&visible, true);
  let edge_colors = |rect: &Rect| {
    let center = rect.center_point();
    [
      fixture_pixel(center.x, rect.top),
      fixture_pixel(center.x, rect.bottom - 1),
      fixture_pixel(rect.left, center.y),
      fixture_pixel(rect.right - 1, center.y),
    ]
  };
  let colors = edge_colors(&visible);
  assert!(
    edges
      .iter()
      .any(|pixel| *pixel != 0 && *pixel != 0x00ff_ffff),
    "Fixture lacks a visible system border."
  );
  assert!(corners.contains(&0), "Fixture lacks rounded corners.");
  for target in [
    initial.translate_to_coordinates(160, 64),
    Rect::from_xy(280, 90, 240, 204),
    initial.clone(),
  ] {
    session.set_frame(&target).expect("Native movement failed.");
    frame = next_frame(&receiver, frame);
    frame = next_frame(&receiver, frame);
    assert_eq!(
      session
        .observed_frame()
        .expect("Native bounds unavailable."),
      target
    );
    assert!(session.is_visible().expect("Visibility unavailable."));
    assert!(!session.is_cloaked().expect("Cloak state unavailable."));
    assert!(!native.has_window_style_ex(WS_EX_LAYERED));
    let visible = native.frame().expect("Visible bounds unavailable.");
    assert_eq!(
      decoration_pixels(&visible, true),
      corners,
      "Native movement changed corners."
    );
    assert_eq!(
      edge_colors(&visible),
      colors,
      "Native resizing changed border colors."
    );
    if target.width() == initial.width()
      && target.height() == initial.height()
    {
      assert_eq!(
        decoration_pixels(&visible, false),
        edges,
        "Native movement changed the border."
      );
    }
  }
  session.release().expect("Source recovery failed.");
}

/// Slides clip pixels at their canvas boundary.
fn live_slide_overlay_pixels() {
  let (_event_loop, dispatcher) =
    EventLoop::new().expect("Event loop unavailable.");
  let _backdrop = Fixture::new(
    w!("GlazeWM.SlideBackdrop"),
    BLACK_BRUSH,
    &Rect::from_xy(48, 48, 400, 160),
    WINDOW_EX_STYLE(0),
  );
  let source = Fixture::new(
    w!("GlazeWM.SlideSource"),
    WHITE_BRUSH,
    &Rect::from_xy(64, 64, 96, 96),
    WINDOW_EX_STYLE(0),
  );
  let native = NativeWindow::from_handle(source.0 .0);
  let session = PlacementSession::new(native.clone(), 1)
    .expect("Source claim failed.");
  let context =
    AnimationContext::new(&dispatcher).expect("Context creation failed.");
  let capture = context
    .capture_frame(native.id(), &crate::OnScreenWindows::new())
    .expect("Capture failed.");
  let (sender, receiver) = mpsc::channel();
  let _clock =
    FrameClock::start(60, move |signal| sender.send(signal).is_ok());
  let mut frame = next_frame(&receiver, 0);
  let mut overlay = AnimationWindow::new(
    &context,
    &native,
    capture,
    &Rect::from_xy(240, 64, 96, 96),
    &Rect::from_xy(48, 48, 240, 144),
    None,
    &dispatcher,
  )
  .expect("Overlay creation failed.");
  session
    .opacity(Some(OpacityValue(0.0)))
    .expect("Source concealment failed.");
  frame = next_frame(&receiver, frame);
  frame = next_frame(&receiver, frame);
  assert_eq!(fixture_pixel(260, 100), 0x00ff_ffff);
  assert_eq!(
    fixture_pixel(310, 100),
    0,
    "Slide crossed its canvas boundary."
  );
  assert_eq!(fixture_pixel(100, 100), 0, "Source remained visible.");
  overlay
    .update(&Rect::from_xy(300, 64, 96, 96), None)
    .expect("Slide update failed.");
  frame = next_frame(&receiver, frame);
  next_frame(&receiver, frame);
  assert_eq!(fixture_pixel(260, 100), 0);
  assert_eq!(fixture_pixel(310, 100), 0);
  overlay.destroy().expect("Overlay cleanup failed.");
  session.release().expect("Source recovery failed.");
}

/// Adoption and stale notifications must never restore alpha mid-opening.
fn live_opening_event_adoption() {
  let source = Fixture::new(
    w!("GlazeWM.LiveEventOpening"),
    WHITE_BRUSH,
    &Rect::from_xy(64, 64, 96, 96),
    WINDOW_EX_STYLE(0),
  );
  let native = NativeWindow::from_handle(source.0 .0);
  let guard = crate::opening_windows::conceal(native.clone())
    .expect("Event concealment failed.");
  assert!(crate::opening_windows::is_held(&native));
  let session = PlacementSession::new(native.clone(), 7)
    .expect("Event source adoption failed.");
  assert!(session.opening_concealed());
  assert!(
    session.supports_opacity(),
    "Adoption mistook owned alpha for app alpha."
  );
  drop(guard);
  let mut alpha = u8::MAX;
  // SAFETY: The fixture remains owned by this test.
  unsafe {
    GetLayeredWindowAttributes(source.0, None, Some(&raw mut alpha), None)
  }
  .expect("Source alpha observation failed.");
  assert_eq!(alpha, 0, "Adoption revealed the source.");
  assert!(!crate::opening_windows::is_held(&native));
  session.release().expect("Adopted source recovery failed.");
  assert!(!native.has_window_style_ex(WS_EX_LAYERED));
}

/// Ignored windows recover as soon as the final notification is dropped.
fn live_opening_event_rejection() {
  let source = Fixture::new(
    w!("GlazeWM.LiveRejectedOpening"),
    WHITE_BRUSH,
    &Rect::from_xy(64, 64, 96, 96),
    WINDOW_EX_STYLE(0),
  );
  let native = NativeWindow::from_handle(source.0 .0);
  let guard = crate::opening_windows::conceal(native.clone())
    .expect("Event concealment failed.");
  let repeated_show = crate::opening_windows::guard(native.id())
    .expect("Repeated SHOW lost its reservation.");
  assert!(std::sync::Arc::ptr_eq(&guard, &repeated_show));
  let batch = guard.clone();
  drop(guard);
  assert!(crate::opening_windows::is_held(&native));
  drop(batch);
  assert!(crate::opening_windows::is_held(&native));
  drop(repeated_show);
  assert!(!crate::opening_windows::is_held(&native));
  assert!(!native.has_window_style_ex(WS_EX_LAYERED));
}

/// Missing SHOW delivery must not leave the application concealed.
fn live_opening_creation_timeout() {
  let source = Fixture::new(
    w!("GlazeWM.LiveOpeningTimeout"),
    WHITE_BRUSH,
    &Rect::from_xy(64, 64, 96, 96),
    WINDOW_EX_STYLE(0),
  );
  let native = NativeWindow::from_handle(source.0 .0);
  crate::opening_windows::reserve(native.clone());
  assert!(crate::opening_windows::is_held(&native));
  let deadline = std::time::Instant::now() + Duration::from_secs(2);
  while crate::opening_windows::is_held(&native)
    && std::time::Instant::now() < deadline
  {
    let mut message = MSG::default();
    // SAFETY: Dispatches only messages belonging to this fixture thread.
    unsafe {
      while PeekMessageW(&raw mut message, None, 0, 0, PM_REMOVE).as_bool()
      {
        let _ = TranslateMessage(&raw const message);
        DispatchMessageW(&raw const message);
      }
    }
    std::thread::sleep(Duration::from_millis(5));
  }
  assert!(
    !crate::opening_windows::is_held(&native),
    "Opening timeout did not restore the source."
  );
  assert!(!native.has_window_style_ex(WS_EX_LAYERED));
}

/// Recovery must reveal a parked source even with geometry outstanding.
///
/// Recovery moves a parked window home and lifts its concealment. If the
/// placement half reports a pending frame first, the source is left
/// invisible, and a clean exit schedules nothing to retry it.
fn live_parked_source_recovers_visible() {
  let source = Fixture::new(
    w!("GlazeWM.LiveParkedRecovery"),
    WHITE_BRUSH,
    &Rect::from_xy(64, 64, 96, 96),
    WINDOW_EX_STYLE(0),
  );
  let native = NativeWindow::from_handle(source.0 .0);
  let session = PlacementSession::new(native.clone(), 1)
    .expect("Source claim failed.");
  let home = session
    .observed_frame()
    .expect("Geometry observation failed.");
  let corner = Rect::from_xy(
    home.x() + 480,
    home.y() + 320,
    home.width(),
    home.height(),
  );
  session
    .park(&home, &corner)
    .expect("Source parking failed.");
  assert_eq!(
    session
      .observed_frame()
      .expect("Geometry observation failed."),
    corner,
    "Source never reached its parking corner."
  );
  session
    .opacity(Some(OpacityValue(0.0)))
    .expect("Source concealment failed.");

  // Recovery still owes this window a move; concealment lifts anyway.
  let recovery = session.release();
  assert!(
    !native.has_window_style_ex(WS_EX_LAYERED),
    "An outstanding frame stranded an invisible source."
  );
  assert!(native.is_visible().expect("Visibility observation failed."));
  recovery.expect("Parked source recovery failed.");
}

/// Cancelling preparation must restore a source even without an overlay.
fn live_opening_cancel_before_overlay() {
  let source = Fixture::new(
    w!("GlazeWM.LiveCancelledOpening"),
    WHITE_BRUSH,
    &Rect::from_xy(64, 64, 96, 96),
    WINDOW_EX_STYLE(0),
  );
  let native = NativeWindow::from_handle(source.0 .0);
  let session = PlacementSession::new(native.clone(), 1)
    .expect("Source claim failed.");
  session
    .opacity(Some(OpacityValue(0.0)))
    .expect("Opening concealment failed.");
  session.release().expect("Opening recovery failed.");
  assert!(!native.has_window_style_ex(WS_EX_LAYERED));
  assert!(native.is_visible().expect("Visibility observation failed."));
}

/// The shell has no view for a tool window, so cloaking it fails; the
/// failure must leave no recovery flag and the window visible.
///
/// Real application windows have views and cloak; that path is checked on
/// the desktop, since an unmanaged fixture cannot be an application
/// window.
fn live_tool_window_cloak_declines() {
  let source = Fixture::new(
    w!("GlazeWM.LiveToolSource"),
    WHITE_BRUSH,
    &Rect::from_xy(64, 64, 96, 96),
    WINDOW_EX_STYLE(0),
  );
  let session =
    PlacementSession::new(NativeWindow::from_handle(source.0 .0), 1)
      .expect("Source claim failed.");
  assert!(session.cloak(true).is_err(), "Tool window cloaked.");
  assert!(
    !session.is_cloaked().expect("Cloak observation failed."),
    "Failed cloak left the source cloaked."
  );
  session
    .cloak(false)
    .expect("Uncloak after a failed cloak was not a no-op.");
  assert!(session
    .is_visible()
    .expect("Visibility observation failed."));
  session.release().expect("Source release failed.");
}

/// A source without a redirection surface is presented through cloaking.
fn live_composition_source_is_cloaked() {
  let source = Fixture::new(
    w!("GlazeWM.LiveComposition"),
    WHITE_BRUSH,
    &Rect::from_xy(64, 64, 96, 96),
    WS_EX_NOREDIRECTIONBITMAP,
  );
  let session =
    PlacementSession::new(NativeWindow::from_handle(source.0 .0), 1)
      .expect("Source claim failed.");
  assert!(session.supports_presentation());
  assert_eq!(session.conceal_method(), ConcealMethod::Cloak);
  assert!(!session.supports_opacity());
  session.release().expect("Source release failed.");
}

/// Checks rendered thumbnail pixels across preparation, motion and
/// handoff.
fn live_source_overlay_handoff(pixels: bool, opening: bool) {
  let verify_pixel = |x, y, expected, reason| {
    if pixels {
      assert_eq!(fixture_pixel(x, y), expected, "{reason}");
    }
  };
  let (_event_loop, dispatcher) =
    EventLoop::new().expect("Event loop unavailable.");
  let outer = Rect::from_xy(48, 48, 320, 144);
  let initial = Rect::from_xy(64, 64, 96, 96);
  let target = Rect::from_xy(240, 64, 96, 96);
  let _backdrop = Fixture::new(
    w!("GlazeWM.LiveBackdrop"),
    BLACK_BRUSH,
    &outer,
    WINDOW_EX_STYLE(0),
  );
  let source = Fixture::new(
    w!("GlazeWM.LiveSource"),
    WHITE_BRUSH,
    &initial,
    WINDOW_EX_STYLE(0),
  );
  let native = NativeWindow::from_handle(source.0 .0);
  let session = PlacementSession::new(native.clone(), 1)
    .expect("Source claim failed.");
  assert!(session.supports_presentation());
  let context =
    AnimationContext::new(&dispatcher).expect("Context creation failed.");
  let capture = context
    .capture_frame(native.id(), &crate::OnScreenWindows::new())
    .expect("Capture preparation failed.");
  let (sender, receiver) = mpsc::channel();
  let _clock =
    FrameClock::start(60, move |signal| sender.send(signal).is_ok());
  let frame = next_frame(
    &receiver,
    FrameClock::current_frame().expect("No frame serial."),
  );
  verify_pixel(100, 100, 0x00ff_ffff, "Source fixture was not painted.");

  let frame = if opening {
    session
      .opacity(Some(OpacityValue(0.0)))
      .expect("Opening concealment failed.");
    let frame = next_frame(&receiver, frame);
    verify_pixel(100, 100, 0, "Opening source leaked before preparation.");
    frame
  } else {
    frame
  };

  let mut overlay = AnimationWindow::new(
    &context,
    &native,
    capture,
    &initial,
    &outer,
    opening.then_some(OpacityValue(0.0)),
    &dispatcher,
  )
  .expect("DWM overlay preparation failed.");
  let frame = next_frame(&receiver, frame);
  session
    .opacity(Some(OpacityValue(0.0)))
    .expect("Source concealment failed.");
  session
    .set_frame(&target)
    .expect("Native placement submission failed.");
  let mut alpha = u8::MAX;
  // SAFETY: Reads attributes of the fixture owned by this session.
  unsafe {
    GetLayeredWindowAttributes(source.0, None, Some(&raw mut alpha), None)
  }
  .expect("Source alpha observation failed.");
  assert_eq!(alpha, 0);
  let frame = next_frame(&receiver, frame);
  assert_eq!(
    session
      .observed_frame()
      .expect("Geometry observation failed."),
    target
  );
  verify_pixel(
    100,
    100,
    if opening { 0 } else { 0x00ff_ffff },
    "Prepared thumbnail has incorrect visibility.",
  );
  verify_pixel(280, 100, 0, "Concealed source leaked at target.");

  overlay
    .update(&target, Some(&OpacityValue(1.0)))
    .expect("Overlay update failed.");
  let frame = next_frame(&receiver, frame);
  verify_pixel(100, 100, 0, "Overlay remained at its initial frame.");
  verify_pixel(280, 100, 0x00ff_ffff, "Moved thumbnail is blank.");

  session.opacity(None).expect("Source restoration failed.");
  let frame = next_frame(&receiver, frame);
  assert!(
    !native.has_window_style_ex(WS_EX_LAYERED),
    "Owned alpha was not removed."
  );
  overlay.destroy().expect("Overlay retirement failed.");
  next_frame(&receiver, frame);
  verify_pixel(
    280,
    100,
    0x00ff_ffff,
    "Restored source is not visible after retirement.",
  );
  session.release().expect("Source release failed.");
}
