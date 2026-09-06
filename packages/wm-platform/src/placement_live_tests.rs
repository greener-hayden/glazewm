//! Desktop integration checks using only explicitly unmanaged fixtures.

use std::{sync::mpsc, time::Duration};

use windows::{
  core::{w, PCWSTR},
  Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    Graphics::Gdi::{
      GetDC, GetPixel, GetStockObject, ReleaseDC, UpdateWindow,
      BLACK_BRUSH, GET_STOCK_OBJECT_FLAGS, HBRUSH, WHITE_BRUSH,
    },
    UI::WindowsAndMessaging::{
      CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
      GetLayeredWindowAttributes, PeekMessageW, RegisterClassW,
      TranslateMessage, MSG, PM_REMOVE, WINDOW_EX_STYLE, WNDCLASSW,
      WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_NOREDIRECTIONBITMAP,
      WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
    },
  },
};

use crate::{
  AnimationContext, AnimationWindow, ConcealMethod, EventLoop, FrameClock,
  FrameSignal, NativeWindow, NativeWindowWindowsExt, OpacityValue,
  PlacementSession, Rect,
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
        WS_POPUP | WS_VISIBLE,
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
      "live_composition_source_is_cloaked",
      live_composition_source_is_cloaked as fn(),
    ),
    (
      "live_tool_window_cloak_declines",
      live_tool_window_cloak_declines,
    ),
    (
      "live_opening_cancel_before_overlay",
      live_opening_cancel_before_overlay,
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
    .capture_frame(native.id())
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
