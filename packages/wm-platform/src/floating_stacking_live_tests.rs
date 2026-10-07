//! Opt-in checks using unmanaged, nonactivating native fixtures.

use std::{sync::mpsc, time::Duration};

use windows::{
  core::w,
  Win32::{
    Graphics::Gdi::{BLACK_BRUSH, WHITE_BRUSH},
    UI::{
      Input::KeyboardAndMouse::EnableWindow,
      WindowsAndMessaging::{
        GetForegroundWindow, GetGUIThreadInfo, GetTopWindow, GetWindow,
        GetWindowLongPtrW, GetWindowThreadProcessId, SetForegroundWindow,
        SetWindowLongPtrW, ShowWindow, GUITHREADINFO, GWLP_HWNDPARENT,
        GW_HWNDNEXT, GW_OWNER, SW_HIDE, WINDOW_EX_STYLE, WS_EX_NOACTIVATE,
        WS_EX_TOPMOST,
      },
    },
  },
};

use super::{fixture_pixel, next_frame, Fixture};
use crate::{
  native_stacking_context, FrameClock, NativeSession,
  NativeStackingWorker, NativeWindow, NativeWindowWindowsExt, Rect,
  WindowId, WindowZOrder,
};

/// Observes the actual foreground and selected keyboard control.
fn focus_identity() -> (isize, isize) {
  let mut info = GUITHREADINFO {
    cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
    ..Default::default()
  };
  // SAFETY: Reads only the current foreground thread's live GUI metadata.
  unsafe {
    GetGUIThreadInfo(0, &raw mut info).expect("foreground GUI metadata");
    (GetForegroundWindow().0, info.hwndFocus.0)
  }
}

/// Submits one group and waits for completion, never treating it as a
/// frame.
fn run(
  actors: &[&NativeSession],
  floats: Vec<WindowId>,
  tiles: Vec<WindowId>,
  expected: Vec<crate::NativeStackingEntry>,
) {
  let (sender, receiver) = mpsc::channel();
  let worker = NativeStackingWorker::start(move || {
    let _ = sender.send(());
  })
  .expect("native stacking worker");
  worker
    .submit(
      actors.iter().map(|actor| (*actor).clone()).collect(),
      vec![(floats, tiles)],
      expected,
    )
    .expect("submit one native group");
  let deadline = std::time::Instant::now() + Duration::from_secs(3);
  loop {
    let mut message =
      windows::Win32::UI::WindowsAndMessaging::MSG::default();
    // SAFETY: Pumps notifications on our fixture UI thread while native
    // work runs.
    unsafe {
      while windows::Win32::UI::WindowsAndMessaging::PeekMessageW(
        &raw mut message,
        None,
        0,
        0,
        windows::Win32::UI::WindowsAndMessaging::PM_REMOVE,
      )
      .as_bool()
      {
        let _ = windows::Win32::UI::WindowsAndMessaging::TranslateMessage(
          &raw const message,
        );
        windows::Win32::UI::WindowsAndMessaging::DispatchMessageW(
          &raw const message,
        );
      }
    }
    match receiver.recv_timeout(Duration::from_millis(2)) {
      Ok(()) => break,
      Err(mpsc::RecvTimeoutError::Timeout)
        if std::time::Instant::now() < deadline => {}
      error => panic!("native completion, not presentation: {error:?}"),
    }
  }
  assert!(!worker.is_busy());
}

/// Builds an independent, owner-disabled, or hidden-owner fixture and
/// checks pixels.
pub(super) fn owner_modal_and_pixels() {
  for mode in 0..3 {
    eprintln!("floating native owner mode={mode}");
    let focus = focus_identity();
    let owner = Fixture::new(
      w!("GlazeWMFloatOwnerOracle"),
      BLACK_BRUSH,
      &Rect::from_xy(140, 60, 60, 60),
      WINDOW_EX_STYLE(0),
    );
    let source = Fixture::new(
      w!("GlazeWMFloatSourceOracle"),
      WHITE_BRUSH,
      &Rect::from_xy(60, 60, 60, 60),
      WINDOW_EX_STYLE(0),
    );
    let tile = Fixture::new(
      w!("GlazeWMFloatTileOracle"),
      BLACK_BRUSH,
      &Rect::from_xy(60, 60, 60, 60),
      WINDOW_EX_STYLE(0),
    );
    let source_window = NativeWindow::from_handle(source.0 .0);
    let tile_window = NativeWindow::from_handle(tile.0 .0);
    NativeWindow::from_handle(owner.0 .0)
      .set_z_order(&WindowZOrder::Normal)
      .expect("normal owner");
    source_window
      .set_z_order(&WindowZOrder::Normal)
      .expect("normal float");
    tile_window
      .set_z_order(&WindowZOrder::Normal)
      .expect("tile initially ahead");
    if mode != 0 {
      // SAFETY: Assigns ownership only between our live tool fixtures.
      unsafe {
        SetWindowLongPtrW(source.0, GWLP_HWNDPARENT, owner.0 .0);
        EnableWindow(owner.0, false);
        if mode == 2 {
          let _ = ShowWindow(owner.0, SW_HIDE);
        }
      }
    }
    let source_session =
      NativeSession::new(source_window.clone(), 12_301 + mode)
        .expect("claim float");
    let tile_session =
      NativeSession::new(tile_window.clone(), 12_401 + mode)
        .expect("claim tile");
    let expected =
      native_stacking_context().expect("complete native context");
    let before_family = expected
      .iter()
      .filter(|entry| {
        entry.id == WindowId(owner.0 .0) || entry.id == source_window.id()
      })
      .cloned()
      .collect::<Vec<_>>();
    let (sender, receiver) = mpsc::channel();
    let _clock =
      FrameClock::start(60, move |signal| sender.send(signal).is_ok());
    let frame = next_frame(&receiver, 0);
    let pixel = source_window
      .frame()
      .expect("physical DWM source bounds")
      .center_point();
    assert_eq!(
      fixture_pixel(pixel.x, pixel.y),
      0,
      "tile's black pixels precede float"
    );
    run(
      &[&source_session, &tile_session],
      vec![source_window.id()],
      vec![tile_window.id()],
      expected,
    );
    next_frame(&receiver, frame);
    assert_eq!(
      fixture_pixel(pixel.x, pixel.y),
      0x00ff_ffff,
      "float's white pixels were presented"
    );
    assert!(!source_window.has_window_style_ex(WS_EX_TOPMOST));
    assert!(!tile_window.has_window_style_ex(WS_EX_TOPMOST));
    assert_eq!(
      focus_identity(),
      focus,
      "foreground or selected input control changed"
    );
    let after = native_stacking_context().expect("post-operation context");
    for prior in before_family {
      assert_eq!(
        after.iter().find(|entry| entry.id == prior.id),
        Some(&prior)
      );
    }
    source_session.release().expect("release source recovery");
    tile_session.release().expect("release tile recovery");
  }
}

/// Keeps an active edit control selected and verifies real Unicode input.
pub(super) fn focused_dialog_typing() {
  use windows::Win32::{
    Foundation::HWND,
    UI::{
      Input::KeyboardAndMouse::{
        SendInput, SetFocus, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT,
        KEYEVENTF_KEYUP, KEYEVENTF_UNICODE,
      },
      WindowsAndMessaging::{
        CreateWindowExW, GetWindowTextW, SetForegroundWindow, WS_CHILD,
        WS_EX_NOACTIVATE, WS_VISIBLE,
      },
    },
  };
  let prior_focus = focus_identity();
  let prior = prior_focus.0;
  let owner = Fixture::new(
    w!("GlazeWMFloatTypingOwner"),
    BLACK_BRUSH,
    &Rect::from_xy(140, 60, 60, 60),
    WINDOW_EX_STYLE(0),
  );
  let source = Fixture::new(
    w!("GlazeWMFloatTypingDialog"),
    WHITE_BRUSH,
    &Rect::from_xy(60, 60, 120, 80),
    WINDOW_EX_STYLE(0),
  );
  let tile = Fixture::new(
    w!("GlazeWMFloatTypingTile"),
    BLACK_BRUSH,
    &Rect::from_xy(60, 60, 120, 80),
    WINDOW_EX_STYLE(0),
  );
  let source_window = NativeWindow::from_handle(source.0 .0);
  let tile_window = NativeWindow::from_handle(tile.0 .0);
  NativeWindow::from_handle(owner.0 .0)
    .set_z_order(&WindowZOrder::Normal)
    .expect("normal modal owner");
  source_window
    .set_z_order(&WindowZOrder::Normal)
    .expect("normal dialog");
  tile_window
    .set_z_order(&WindowZOrder::Normal)
    .expect("normal tile");
  source_window.remove_window_style_ex(WS_EX_NOACTIVATE);
  // SAFETY: Creates a standard edit control and classic modal relationship
  // entirely within our unmanaged fixtures, without changing user windows.
  let edit = unsafe {
    SetWindowLongPtrW(source.0, GWLP_HWNDPARENT, owner.0 .0);
    EnableWindow(owner.0, false);
    CreateWindowExW(
      WINDOW_EX_STYLE(0),
      w!("EDIT"),
      w!(""),
      WS_CHILD | WS_VISIBLE,
      10,
      10,
      80,
      30,
      source.0,
      None,
      None,
      None,
    )
  };
  assert_ne!(edit.0, 0, "create fixture edit control");
  // SAFETY: Temporarily activates only our fixture and selects its own
  // edit.
  source_window
    .focus()
    .expect("activate fixture with normal WM foreground helper");
  unsafe {
    SetFocus(edit);
  }
  let expected_focus = focus_identity();
  assert_eq!(expected_focus, (source.0 .0, edit.0));
  tile_window
    .set_z_order(&WindowZOrder::Top)
    .expect("tile overlaps without activation");
  let source_session = NativeSession::new(source_window.clone(), 12_601)
    .expect("dialog recovery");
  let tile_session = NativeSession::new(tile_window.clone(), 12_602)
    .expect("tile recovery");
  run(
    &[&source_session, &tile_session],
    vec![source_window.id()],
    vec![tile_window.id()],
    native_stacking_context().expect("focused modal context"),
  );
  assert_eq!(focus_identity(), expected_focus);
  let input = [KEYEVENTF_UNICODE, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP]
    .map(|flags| INPUT {
      r#type: INPUT_KEYBOARD,
      Anonymous: INPUT_0 {
        ki: KEYBDINPUT {
          wScan: u16::from(b'x'),
          dwFlags: flags,
          ..Default::default()
        },
      },
    });
  // SAFETY: Foreground and selected edit were checked immediately above;
  // sends only one harmless character, never Enter or a command shortcut.
  let delivered =
    unsafe { SendInput(&input, std::mem::size_of::<INPUT>() as i32) };
  assert_eq!(delivered, 2);
  let deadline = std::time::Instant::now() + Duration::from_secs(2);
  let mut text = [0u16; 8];
  loop {
    let mut message =
      windows::Win32::UI::WindowsAndMessaging::MSG::default();
    // SAFETY: Pumps only our fixture messages and reads its own edit text.
    unsafe {
      while windows::Win32::UI::WindowsAndMessaging::PeekMessageW(
        &raw mut message,
        None,
        0,
        0,
        windows::Win32::UI::WindowsAndMessaging::PM_REMOVE,
      )
      .as_bool()
      {
        let _ = windows::Win32::UI::WindowsAndMessaging::TranslateMessage(
          &raw const message,
        );
        windows::Win32::UI::WindowsAndMessaging::DispatchMessageW(
          &raw const message,
        );
      }
      if GetWindowTextW(edit, &mut text) == 1 {
        break;
      }
    }
    assert!(
      std::time::Instant::now() < deadline,
      "Unicode input did not reach fixture edit"
    );
    std::thread::yield_now();
  }
  assert_eq!(text[0], u16::from(b'x'));
  assert_eq!(focus_identity(), expected_focus);
  source_session.release().expect("release dialog");
  tile_session.release().expect("release tile");
  // SAFETY: Restores prior foreground only while our own fixture still has
  // it.
  unsafe {
    if GetForegroundWindow() == source.0 {
      let _ = SetForegroundWindow(HWND(prior));
    }
  }
  let restore_deadline =
    std::time::Instant::now() + Duration::from_secs(2);
  while focus_identity() != prior_focus {
    assert!(
      std::time::Instant::now() < restore_deadline,
      "prior foreground/control did not finish restoring"
    );
    std::thread::sleep(Duration::from_millis(2));
  }
}

/// Captures a bounded native z-order walk independent of `EnumWindows`.
fn native_order() -> Vec<isize> {
  let mut order = Vec::new();
  let mut seen = std::collections::HashSet::new();
  // SAFETY: Queries desktop z-order and stops on null, a cycle, or a
  // bound.
  let mut window = unsafe { GetTopWindow(None) };
  while window.0 != 0 && order.len() < 4096 && seen.insert(window.0) {
    order.push(window.0);
    // SAFETY: Reads the next top-level window in the current z-order
    // chain.
    window = unsafe { GetWindow(window, GW_HWNDNEXT) };
  }
  order
}

/// Reports independent native, enumeration, pixel, identity, and focus
/// observations.
fn report_boundary(label: &str, source: isize, tile: isize) {
  report_boundary_with_caller(label, source, tile, None);
}

/// Reports a native boundary together with the caller thread identity.
fn report_caller_boundary(
  label: &str,
  source: isize,
  tile: isize,
  caller: (u32, u32),
) {
  report_boundary_with_caller(label, source, tile, Some(caller));
}

/// Reports one desktop boundary and optional caller identity.
fn report_boundary_with_caller(
  label: &str,
  source: isize,
  tile: isize,
  caller: Option<(u32, u32)>,
) {
  let order = native_order();
  let rank = |id| order.iter().position(|window| *window == id);
  let neighbors = |id| {
    rank(id).map(|index| {
      (
        index.checked_sub(1).map(|previous| order[previous]),
        order.get(index + 1).copied(),
      )
    })
  };
  let identity = |id| {
    let mut process = 0;
    // SAFETY: Reads identity for a still-live unmanaged test fixture.
    let thread = unsafe {
      GetWindowThreadProcessId(
        windows::Win32::Foundation::HWND(id),
        Some(&raw mut process),
      )
    };
    (process, thread)
  };
  let context = native_stacking_context().ok();
  let (style_source, style_tile, exstyle_source, exstyle_tile) = unsafe {
    (
      GetWindowLongPtrW(
        windows::Win32::Foundation::HWND(source),
        windows::Win32::UI::WindowsAndMessaging::GWL_STYLE,
      ),
      GetWindowLongPtrW(
        windows::Win32::Foundation::HWND(tile),
        windows::Win32::UI::WindowsAndMessaging::GWL_STYLE,
      ),
      GetWindowLongPtrW(
        windows::Win32::Foundation::HWND(source),
        windows::Win32::UI::WindowsAndMessaging::GWL_EXSTYLE,
      ),
      GetWindowLongPtrW(
        windows::Win32::Foundation::HWND(tile),
        windows::Win32::UI::WindowsAndMessaging::GWL_EXSTYLE,
      ),
    )
  };
  let source_owner = unsafe {
    GetWindow(windows::Win32::Foundation::HWND(source), GW_OWNER).0
  };
  let tile_owner = unsafe {
    GetWindow(windows::Win32::Foundation::HWND(tile), GW_OWNER).0
  };
  let foreground_focus = focus_identity();
  let point = NativeWindow::from_handle(source)
    .frame()
    .ok()
    .map(|frame| frame.center_point());
  let pixel = point.map(|point| fixture_pixel(point.x, point.y));
  eprintln!(
    "native-completion {label}: order-ranks source={:?} tile={:?}, source-neighbors={:?} tile-neighbors={:?}, enum-ranks source={:?} tile={:?}, identities-PID-TID source={:?} tile={:?}, owners source={source_owner:#x} tile={tile_owner:#x}, styles source={style_source:#x} tile={style_tile:#x}, exstyles source={exstyle_source:#x} tile={exstyle_tile:#x}, pixel={pixel:?}, foreground-focus={foreground_focus:?} ids-PID-TID={:?}/{:?}, caller-PID-TID={caller:?}",
    rank(source),
    rank(tile),
    neighbors(source),
    neighbors(tile),
    context.as_ref().and_then(|items| items
      .iter()
      .position(|entry| entry.id == WindowId(source))),
    context.as_ref().and_then(|items| items
      .iter()
      .position(|entry| entry.id == WindowId(tile))),
    identity(source),
    identity(tile),
    identity(foreground_focus.0),
    identity(foreground_focus.1),
  );
  let (messages, dropped) = super::take_window_position_messages();
  let fixture_messages = messages
    .into_iter()
    .filter(|message| message.window == source || message.window == tile)
    .map(|message| {
      format!(
        "hwnd={:#x} kind={} flags={:#x} insert-after={:#x}",
        message.window,
        message.message,
        message.flags,
        message.insert_after
      )
    })
    .collect::<Vec<_>>();
  eprintln!(
    "native-completion {label}: fixture-window-position-count={}, messages={fixture_messages:?}, dropped={dropped}",
    fixture_messages.len(),
  );
}

/// Completes the production worker while its target UI thread is blocked.
pub(super) fn blocked_worker() {
  let _message_observation = super::observe_window_positions();
  let (ready_tx, ready_rx) = mpsc::channel();
  let (block_tx, block_rx) = mpsc::channel();
  let (blocked_tx, blocked_rx) = mpsc::channel();
  let (unblock_tx, unblock_rx) = mpsc::channel();
  let (stop_tx, stop_rx) = mpsc::channel();
  let (resumed_tx, resumed_rx) = mpsc::channel();
  let app = std::thread::spawn(move || {
    let fixture = Fixture::new(
      w!("GlazeWMFloatBlockedWorker"),
      WHITE_BRUSH,
      &Rect::from_xy(60, 60, 60, 60),
      WINDOW_EX_STYLE(0),
    );
    let window = NativeWindow::from_handle(fixture.0 .0);
    window
      .set_z_order(&WindowZOrder::Normal)
      .expect("normal blocked source");
    let session = NativeSession::new(window.clone(), 12_701)
      .expect("blocked source recovery");
    ready_tx
      .send((window.id(), session.clone()))
      .expect("ready source");
    loop {
      let mut message =
        windows::Win32::UI::WindowsAndMessaging::MSG::default();
      // SAFETY: Pumps fixture creation and async-demotion messages before
      // intentionally blocking this unmanaged source UI.
      unsafe {
        while windows::Win32::UI::WindowsAndMessaging::PeekMessageW(
          &raw mut message,
          None,
          0,
          0,
          windows::Win32::UI::WindowsAndMessaging::PM_REMOVE,
        )
        .as_bool()
        {
          let _ =
            windows::Win32::UI::WindowsAndMessaging::TranslateMessage(
              &raw const message,
            );
          windows::Win32::UI::WindowsAndMessaging::DispatchMessageW(
            &raw const message,
          );
        }
      }
      match block_rx.recv_timeout(Duration::from_millis(2)) {
        Ok(()) => break,
        Err(mpsc::RecvTimeoutError::Disconnected) => return,
        Err(mpsc::RecvTimeoutError::Timeout) => {}
      }
    }
    blocked_tx.send(()).expect("confirm source blocked");
    unblock_rx.recv().expect("unblock source");
    let mut announced_resume = false;
    loop {
      let mut message =
        windows::Win32::UI::WindowsAndMessaging::MSG::default();
      // SAFETY: Pumps only this unmanaged fixture thread's own messages.
      unsafe {
        while windows::Win32::UI::WindowsAndMessaging::PeekMessageW(
          &raw mut message,
          None,
          0,
          0,
          windows::Win32::UI::WindowsAndMessaging::PM_REMOVE,
        )
        .as_bool()
        {
          let _ =
            windows::Win32::UI::WindowsAndMessaging::TranslateMessage(
              &raw const message,
            );
          windows::Win32::UI::WindowsAndMessaging::DispatchMessageW(
            &raw const message,
          );
        }
      }
      if !announced_resume {
        let _ = resumed_tx.send(());
        announced_resume = true;
      }
      match stop_rx.recv_timeout(Duration::from_millis(2)) {
        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
        Err(mpsc::RecvTimeoutError::Timeout) => {}
      }
    }
    session.release().expect("release blocked source");
  });
  let (source_id, source_session) = ready_rx
    .recv_timeout(Duration::from_secs(3))
    .expect("blocked source ready");
  let tile = Fixture::new(
    w!("GlazeWMFloatBlockedWorkerTile"),
    BLACK_BRUSH,
    &Rect::from_xy(60, 60, 60, 60),
    WINDOW_EX_STYLE(0),
  );
  let tile_window = NativeWindow::from_handle(tile.0 .0);
  tile_window
    .set_z_order(&WindowZOrder::Normal)
    .expect("tile ahead of blocked source");
  let tile_session = NativeSession::new(tile_window.clone(), 12_702)
    .expect("tile recovery");
  let (frame_tx, frame_rx) = mpsc::channel();
  let _clock =
    FrameClock::start(60, move |signal| frame_tx.send(signal).is_ok());
  let setup_frame = next_frame(&frame_rx, 0);
  let source_window = NativeWindow::from_handle(source_id.0);
  assert!(
    !source_window.has_window_style_ex(WS_EX_TOPMOST)
      && !tile_window.has_window_style_ex(WS_EX_TOPMOST),
    "fixture setup invalid: source or tile remains topmost"
  );
  report_boundary("settled-setup", source_id.0, tile_window.id().0);
  let setup_order = native_order();
  let setup_ranks = setup_order
    .iter()
    .position(|window| *window == source_id.0)
    .zip(
      setup_order
        .iter()
        .position(|window| *window == tile_window.id().0),
    );
  let enum_ranks = native_stacking_context().ok().and_then(|context| {
    context
      .iter()
      .position(|entry| entry.id == WindowId(source_id.0))
      .zip(
        context
          .iter()
          .position(|entry| entry.id == tile_window.id()),
      )
  });
  assert!(
    setup_ranks.is_some_and(|(source, tile)| tile < source)
      && enum_ranks.is_some_and(|(source, tile)| tile < source),
    "fixture setup invalid: source/tile native order was not established"
  );
  let source_frame = source_window.frame().expect("source pixel bounds");
  let setup_pixel = fixture_pixel(
    source_frame.center_point().x,
    source_frame.center_point().y,
  );
  assert_eq!(
    setup_pixel, 0,
    "fixture setup invalid: tile does not cover source"
  );
  block_tx.send(()).expect("block settled source UI");
  blocked_rx
    .recv_timeout(Duration::from_secs(2))
    .expect("source UI entered blocked phase");
  let (done_tx, done_rx) = mpsc::channel();
  let worker = NativeStackingWorker::start(move || {
    let _ = done_tx.send(());
  })
  .expect("production stacking worker");
  let expected =
    native_stacking_context().expect("blocked source context");
  report_boundary("pre-call", source_id.0, tile_window.id().0);
  worker
    .submit(
      vec![source_session, tile_session.clone()],
      vec![(vec![source_id], vec![tile_window.id()])],
      expected,
    )
    .expect("submit one blocked source request");
  let deadline = std::time::Instant::now() + Duration::from_secs(2);
  let early = loop {
    let mut message =
      windows::Win32::UI::WindowsAndMessaging::MSG::default();
    // SAFETY: Pumps only this main fixture thread, never the blocked
    // source UI.
    unsafe {
      while windows::Win32::UI::WindowsAndMessaging::PeekMessageW(
        &raw mut message,
        None,
        0,
        0,
        windows::Win32::UI::WindowsAndMessaging::PM_REMOVE,
      )
      .as_bool()
      {
        let _ = windows::Win32::UI::WindowsAndMessaging::TranslateMessage(
          &raw const message,
        );
        windows::Win32::UI::WindowsAndMessaging::DispatchMessageW(
          &raw const message,
        );
      }
    }
    match done_rx.recv_timeout(Duration::from_millis(2)) {
      Ok(()) => break true,
      Err(mpsc::RecvTimeoutError::Timeout)
        if std::time::Instant::now() < deadline => {}
      _ => break false,
    }
  };
  if early {
    report_boundary("api-return-blocked", source_id.0, tile_window.id().0);
  }
  let applied = if early {
    let context = native_stacking_context()
      .expect("post-worker order while UI still blocked");
    eprintln!(
      "blocked worker source rank={:?}, tile rank={:?}",
      context.iter().position(|entry| entry.id == source_id),
      context
        .iter()
        .position(|entry| entry.id == tile_window.id())
    );
    context
      .iter()
      .position(|entry| entry.id == source_id)
      .zip(
        context
          .iter()
          .position(|entry| entry.id == tile_window.id()),
      )
      .is_some_and(|(source, tile)| source < tile)
  } else {
    false
  };
  if early {
    let compositor_frame = next_frame(&frame_rx, setup_frame);
    eprintln!(
      "native-completion compositor-progress frame={compositor_frame} while source UI blocked"
    );
    report_boundary(
      "compositor-progress-blocked",
      source_id.0,
      tile_window.id().0,
    );
  }
  unblock_tx
    .send(())
    .expect("unblock fixture even on failure");
  let source_resumed =
    resumed_rx.recv_timeout(Duration::from_secs(1)).is_ok();
  eprintln!("native-completion source-ui-pump-resumed={source_resumed}");
  if !early {
    done_rx
      .recv_timeout(Duration::from_secs(3))
      .expect("drain native work after unblock");
  }
  let resumed_frame = next_frame(&frame_rx, setup_frame);
  eprintln!(
    "native-completion source-resumed compositor-frame={resumed_frame}"
  );
  report_boundary("after-source-resume", source_id.0, tile_window.id().0);
  stop_tx.send(()).expect("stop fixture UI");
  app.join().expect("join fixture UI");
  tile_session.release().expect("release tile");
  assert!(
    early,
    "production native worker waited for a blocked source UI"
  );
  assert!(
    source_resumed,
    "source UI did not resume its fixture message pump"
  );
  assert!(
    applied,
    "worker completion did not establish the requested native order"
  );
}

/// Tests one exact z-order request on a queue-matched caller thread.
pub(super) fn caller_queue_processing() {
  caller_queue_diagnostic(false);
}

/// Tests one native stacking request after activating the tile fixture.
pub(super) fn foreground_owner_permission() {
  caller_queue_diagnostic(true);
}

/// Runs the bounded caller-queue diagnostic, optionally making the
/// source-owning process the foreground process first.
fn caller_queue_diagnostic(foreground_owner: bool) {
  use windows::Win32::{
    Foundation::{BOOL, HWND},
    System::Threading::{GetCurrentProcessId, GetCurrentThreadId},
    UI::WindowsAndMessaging::{
      DispatchMessageW, GetQueueStatus, PeekMessageW, TranslateMessage,
      MSG, PM_NOREMOVE, PM_REMOVE, QS_ALLINPUT, SET_WINDOW_POS_FLAGS,
    },
  };

  const REQUEST_FLAGS: u32 = 0x061b;
  const MAX_CALLER_MESSAGES: usize = 128;
  const CALL_TIMEOUT: Duration = Duration::from_secs(2);

  #[link(name = "user32")]
  unsafe extern "system" {
    /// Returns the raw `SetWindowPos` `BOOL` without binding conversion.
    #[link_name = "SetWindowPos"]
    fn set_window_pos_raw(
      window: HWND,
      insert_after: HWND,
      x: i32,
      y: i32,
      width: i32,
      height: i32,
      flags: SET_WINDOW_POS_FLAGS,
    ) -> BOOL;
  }

  #[link(name = "kernel32")]
  unsafe extern "system" {
    /// Reads the caller thread's last Win32 error.
    #[link_name = "GetLastError"]
    fn raw_get_last_error() -> u32;
  }

  let _message_observation = super::observe_window_positions();
  let (source_ready_tx, source_ready_rx) = mpsc::channel();
  let (block_source_tx, block_source_rx) = mpsc::channel();
  let (source_blocked_tx, source_blocked_rx) = mpsc::channel();
  let (resume_source_tx, resume_source_rx) = mpsc::channel();
  let (stop_source_tx, stop_source_rx) = mpsc::channel();
  let (source_resumed_tx, source_resumed_rx) = mpsc::channel();
  let (source_cleanup_tx, source_cleanup_rx) = mpsc::channel();
  let source_thread = std::thread::spawn(move || {
    let fixture = Fixture::new(
      w!("GlazeWMCallerQueueSource"),
      WHITE_BRUSH,
      &Rect::from_xy(60, 60, 60, 60),
      WINDOW_EX_STYLE(0),
    );
    let window = NativeWindow::from_handle(fixture.0 .0);
    window
      .set_z_order(&WindowZOrder::Normal)
      .expect("normal caller-queue source");
    let session = NativeSession::new(window.clone(), 12_801)
      .expect("claim caller-queue source");
    source_ready_tx
      .send((window.id(), session.clone()))
      .expect("publish source identity");
    loop {
      let mut message =
        windows::Win32::UI::WindowsAndMessaging::MSG::default();
      // SAFETY: Pumps only this fixture UI while async demotion settles.
      unsafe {
        while windows::Win32::UI::WindowsAndMessaging::PeekMessageW(
          &raw mut message,
          None,
          0,
          0,
          windows::Win32::UI::WindowsAndMessaging::PM_REMOVE,
        )
        .as_bool()
        {
          let _ =
            windows::Win32::UI::WindowsAndMessaging::TranslateMessage(
              &raw const message,
            );
          windows::Win32::UI::WindowsAndMessaging::DispatchMessageW(
            &raw const message,
          );
        }
      }
      match block_source_rx.recv_timeout(Duration::from_millis(2)) {
        Ok(()) => break,
        Err(mpsc::RecvTimeoutError::Disconnected) => return,
        Err(mpsc::RecvTimeoutError::Timeout) => {}
      }
    }
    let _ = source_blocked_tx.send(());
    // A dropped controller is also an unconditional resume request.
    let _ = resume_source_rx.recv();
    let mut announced_resume = false;
    loop {
      let mut message =
        windows::Win32::UI::WindowsAndMessaging::MSG::default();
      // SAFETY: Resumes and pumps only the unmanaged source fixture UI.
      unsafe {
        while windows::Win32::UI::WindowsAndMessaging::PeekMessageW(
          &raw mut message,
          None,
          0,
          0,
          windows::Win32::UI::WindowsAndMessaging::PM_REMOVE,
        )
        .as_bool()
        {
          let _ =
            windows::Win32::UI::WindowsAndMessaging::TranslateMessage(
              &raw const message,
            );
          windows::Win32::UI::WindowsAndMessaging::DispatchMessageW(
            &raw const message,
          );
        }
      }
      if !announced_resume {
        let _ = source_resumed_tx.send(());
        announced_resume = true;
      }
      match stop_source_rx.recv_timeout(Duration::from_millis(2)) {
        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
        Err(mpsc::RecvTimeoutError::Timeout) => {}
      }
    }
    let released = session.release().is_ok();
    drop(fixture);
    let _ = source_cleanup_tx.send(released);
  });

  let (source_id, source_session) = source_ready_rx
    .recv_timeout(Duration::from_secs(3))
    .expect("caller-queue source ready");
  let source_window = NativeWindow::from_handle(source_id.0);
  let tile = Fixture::new(
    w!("GlazeWMCallerQueueTile"),
    BLACK_BRUSH,
    &Rect::from_xy(60, 60, 60, 60),
    WINDOW_EX_STYLE(0),
  );
  let tile_window = NativeWindow::from_handle(tile.0 .0);
  tile_window
    .set_z_order(&WindowZOrder::Normal)
    .expect("normal caller-queue tile");
  let tile_session = NativeSession::new(tile_window.clone(), 12_802)
    .expect("claim caller-queue tile");

  let (caller_ready_tx, caller_ready_rx) = mpsc::channel();
  let (call_tx, call_rx) = mpsc::channel();
  let (call_result_tx, call_result_rx) = mpsc::channel();
  let (pump_tx, pump_rx) = mpsc::channel();
  let (pump_result_tx, pump_result_rx) = mpsc::channel();
  let caller_thread = std::thread::spawn(move || {
    let mut message = MSG::default();
    // SAFETY: Matches production worker queue initialization exactly.
    unsafe {
      PeekMessageW(&raw mut message, None, 0, 0, PM_NOREMOVE);
    }
    // SAFETY: Reads only this caller thread's process/thread identity.
    let caller_identity =
      unsafe { (GetCurrentProcessId(), GetCurrentThreadId()) };
    let _ = caller_ready_tx.send(caller_identity);
    if call_rx.recv_timeout(Duration::from_secs(5)).is_err() {
      return;
    }
    let queue_before = unsafe { GetQueueStatus(QS_ALLINPUT) };
    // SAFETY: Exactly one HWND_TOP request using the approved production
    // flags; this caller owns a queue but no fixture window.
    let result = unsafe {
      set_window_pos_raw(
        HWND(source_id.0),
        HWND(0),
        0,
        0,
        0,
        0,
        SET_WINDOW_POS_FLAGS(REQUEST_FLAGS),
      )
    };
    let last_error = if result.0 == 0 {
      Some(unsafe { raw_get_last_error() })
    } else {
      None
    };
    let queue_after = unsafe { GetQueueStatus(QS_ALLINPUT) };
    let queue_held = unsafe { GetQueueStatus(QS_ALLINPUT) };
    let _ = call_result_tx.send((
      result.0,
      last_error,
      queue_before,
      queue_after,
      queue_held,
      caller_identity,
    ));
    if pump_rx.recv_timeout(Duration::from_secs(5)).is_err() {
      return;
    }
    let queue_before_pump = unsafe { GetQueueStatus(QS_ALLINPUT) };
    let deadline = std::time::Instant::now() + Duration::from_millis(150);
    let mut messages = Vec::new();
    while std::time::Instant::now() < deadline
      && messages.len() < MAX_CALLER_MESSAGES
    {
      let mut message = MSG::default();
      // SAFETY: Processes only this caller thread's own queue.
      unsafe {
        while PeekMessageW(&raw mut message, None, 0, 0, PM_REMOVE)
          .as_bool()
          && messages.len() < MAX_CALLER_MESSAGES
        {
          messages.push((message.hwnd.0, message.message));
          let _ = TranslateMessage(&raw const message);
          DispatchMessageW(&raw const message);
        }
      }
      std::thread::yield_now();
    }
    let queue_after_pump = unsafe { GetQueueStatus(QS_ALLINPUT) };
    let _ = pump_result_tx.send((
      queue_before_pump,
      queue_after_pump,
      messages,
      caller_identity,
    ));
  });

  let caller_identity = caller_ready_rx
    .recv_timeout(Duration::from_secs(2))
    .expect("fixture caller queue ready");
  let (frame_tx, frame_rx) = mpsc::channel();
  let _clock =
    FrameClock::start(60, move |signal| frame_tx.send(signal).is_ok());
  let setup_frame = next_frame(&frame_rx, 0);
  assert!(
    !source_window.has_window_style_ex(WS_EX_TOPMOST)
      && !tile_window.has_window_style_ex(WS_EX_TOPMOST),
    "caller-queue setup invalid: topmost fixture"
  );
  report_boundary("caller-queue-settled", source_id.0, tile_window.id().0);
  let setup_order = native_order();
  let source_rank = setup_order.iter().position(|id| *id == source_id.0);
  let tile_rank =
    setup_order.iter().position(|id| *id == tile_window.id().0);
  let enum_ranks = native_stacking_context().ok().and_then(|context| {
    context.iter().position(|entry| entry.id == source_id).zip(
      context
        .iter()
        .position(|entry| entry.id == tile_window.id()),
    )
  });
  assert!(
    source_rank
      .zip(tile_rank)
      .is_some_and(|(source, tile)| tile < source)
      && enum_ranks.is_some_and(|(source, tile)| tile < source),
    "caller-queue setup invalid: tile must precede source"
  );
  let setup_point = source_window
    .frame()
    .expect("caller-queue source bounds")
    .center_point();
  assert_eq!(
    fixture_pixel(setup_point.x, setup_point.y),
    0,
    "caller-queue setup invalid: tile must cover source"
  );
  block_source_tx.send(()).expect("block source fixture UI");
  source_blocked_rx
    .recv_timeout(Duration::from_secs(2))
    .expect("source fixture blocked");
  let prior_focus = focus_identity();
  let mut source_owner_pid = 0;
  // SAFETY: Reads the process identity of our still-live source fixture.
  let source_owner_tid = unsafe {
    GetWindowThreadProcessId(
      windows::Win32::Foundation::HWND(source_id.0),
      Some(&raw mut source_owner_pid),
    )
  };
  report_caller_boundary(
    "foreground-owner-before-activation",
    source_id.0,
    tile_window.id().0,
    caller_identity,
  );
  let activation_permitted = if foreground_owner {
    tile_window.remove_window_style_ex(WS_EX_NOACTIVATE);
    // SAFETY: Attempts plain foreground activation of only our fixture
    // tile.
    let activation_result =
      unsafe { SetForegroundWindow(tile.0) }.as_bool();
    let activated_identity = focus_identity();
    let mut foreground_pid = 0;
    // SAFETY: Reads the process identity of the current foreground window.
    let foreground_tid = unsafe {
      GetWindowThreadProcessId(
        windows::Win32::Foundation::HWND(activated_identity.0),
        Some(&raw mut foreground_pid),
      )
    };
    let permitted = activation_result
      && activated_identity.0 == tile.0 .0
      && foreground_pid == source_owner_pid;
    eprintln!(
      "foreground-owner activation: tile={:#x} source-owner-PID={source_owner_pid} source-owner-TID={source_owner_tid}, SetForegroundWindow={activation_result}, foreground-focus={activated_identity:?} foreground-PID-TID={foreground_pid}/{foreground_tid}, permitted={permitted}",
      tile.0 .0,
    );
    report_caller_boundary(
      "foreground-owner-after-activation",
      source_id.0,
      tile_window.id().0,
      caller_identity,
    );
    permitted
  } else {
    true
  };
  if foreground_owner && !activation_permitted {
    eprintln!(
      "foreground-owner activation denied or foreground PID mismatch; skipping the sole HWND_TOP request without retry or bypass"
    );
    drop(call_tx);
  } else {
    call_tx.send(()).expect("request one caller SetWindowPos");
  }
  let returned_while_blocked = if activation_permitted {
    match call_result_rx.recv_timeout(CALL_TIMEOUT) {
      Ok(result) => Some(result),
      Err(mpsc::RecvTimeoutError::Timeout) => None,
      Err(mpsc::RecvTimeoutError::Disconnected) => None,
    }
  } else {
    None
  };

  let mut source_resume_requested = false;
  let pump_observation = if let Some((
    raw_bool,
    last_error,
    queue_before,
    queue_after,
    queue_held,
    identity,
  )) = returned_while_blocked
  {
    eprintln!(
      "caller-queue native-return: hwnd={:#x} insert_after=0 x=0 y=0 cx=0 cy=0 flags={REQUEST_FLAGS:#06x}, raw-BOOL={raw_bool}, last-error={last_error:?}, queue-before={queue_before:#x} queue-after={queue_after:#x} held={queue_held:#x}, caller={identity:?}",
      source_id.0,
    );
    report_caller_boundary(
      "caller-queue-api-return-held",
      source_id.0,
      tile_window.id().0,
      caller_identity,
    );
    let compositor_frame = next_frame(&frame_rx, setup_frame);
    eprintln!("caller-queue held compositor frame={compositor_frame}");
    report_caller_boundary(
      "caller-queue-held-after-compositor",
      source_id.0,
      tile_window.id().0,
      caller_identity,
    );
    let observation = if pump_tx.send(()).is_ok() {
      pump_result_rx.recv_timeout(Duration::from_secs(2)).ok()
    } else {
      None
    };
    if observation.is_some() {
      report_caller_boundary(
        "caller-queue-after-pump-source-still-blocked",
        source_id.0,
        tile_window.id().0,
        caller_identity,
      );
    } else {
      eprintln!("caller-queue processing result unavailable; will resume source before cleanup");
    }
    observation
  } else if !activation_permitted {
    eprintln!("caller-queue request and pump not run because fixture activation was denied");
    None
  } else {
    eprintln!("caller-queue native return timed out while source blocked; resuming source, not treating timeout as completion");
    let _ = resume_source_tx.send(());
    source_resume_requested = true;
    let result_after_resume = call_result_rx.recv().ok();
    eprintln!(
      "caller-queue native-return-after-resume={result_after_resume:?}"
    );
    if pump_tx.send(()).is_ok() {
      pump_result_rx.recv_timeout(Duration::from_secs(2)).ok()
    } else {
      None
    }
  };

  if let Some((queue_before, queue_after, messages, identity)) =
    pump_observation
  {
    eprintln!(
      "caller-queue pump: queue-before={queue_before:#x} queue-after={queue_after:#x}, messages-processed={} entries={messages:?}, caller={identity:?}",
      messages.len(),
    );
  } else {
    eprintln!("caller-queue pump observations unavailable");
  }
  if !source_resume_requested {
    let _ = resume_source_tx.send(());
    source_resume_requested = true;
  }
  let source_resume_ack = match source_resumed_rx
    .recv_timeout(Duration::from_secs(2))
  {
    Ok(()) => true,
    Err(mpsc::RecvTimeoutError::Timeout) => {
      eprintln!("caller-queue resume acknowledgement delayed; timeout is not treated as completion");
      source_resumed_rx.recv().is_ok()
    }
    Err(mpsc::RecvTimeoutError::Disconnected) => false,
  };
  eprintln!("caller-queue source-resume-ack={source_resume_ack}");
  if source_resume_ack {
    let resumed_frame = next_frame(&frame_rx, setup_frame);
    eprintln!(
      "caller-queue after-resume compositor-frame={resumed_frame}"
    );
    report_caller_boundary(
      "caller-queue-after-source-resume",
      source_id.0,
      tile_window.id().0,
      caller_identity,
    );
  } else {
    eprintln!("caller-queue after-source-resume observations unavailable");
  }
  if foreground_owner {
    let current_focus = focus_identity();
    if current_focus.0 == tile.0 .0 {
      // SAFETY: Restores the captured foreground window only while our
      // tile still owns foreground, matching the existing fixture
      // focus helper.
      let restore_result = unsafe {
        SetForegroundWindow(windows::Win32::Foundation::HWND(
          prior_focus.0,
        ))
      }
      .as_bool();
      let restore_deadline =
        std::time::Instant::now() + Duration::from_secs(2);
      while focus_identity() != prior_focus
        && std::time::Instant::now() < restore_deadline
      {
        std::thread::yield_now();
      }
      eprintln!(
        "foreground-owner restore: SetForegroundWindow={restore_result}, prior={prior_focus:?}, actual={:?}",
        focus_identity(),
      );
    } else if current_focus == prior_focus {
      eprintln!(
        "foreground-owner restore not needed: prior foreground and focused control remained unchanged ({prior_focus:?})"
      );
    } else {
      eprintln!(
        "foreground-owner restore skipped: fixture tile is no longer foreground; prior={prior_focus:?}, actual={current_focus:?}"
      );
    }
  }
  let caller_joined = caller_thread.join().is_ok();
  eprintln!("caller-queue caller-thread-joined={caller_joined}");
  let _ = stop_source_tx.send(());
  let source_joined = source_thread.join().is_ok();
  let source_released = source_cleanup_rx
    .recv_timeout(Duration::from_secs(1))
    .unwrap_or(false);
  eprintln!(
    "caller-queue cleanup: source-resume-requested={source_resume_requested}, source-ack={source_resume_ack}, caller-joined={caller_joined}, source-joined={source_joined}, source-session-released={source_released}"
  );
  let tile_released = tile_session.release().is_ok();
  drop(tile);
  eprintln!("caller-queue cleanup: tile-session-released={tile_released}, tile-fixture-destroyed=true");
  let _ = source_session;
  let _ = stop_source_tx;
}

/// Rejects new modal families or overlay ownership and preserves a late
/// topmost flag.
pub(super) fn changed_context_and_overlay() {
  let focus = focus_identity();
  let source = Fixture::new(
    w!("GlazeWMFloatRaceSource"),
    WHITE_BRUSH,
    &Rect::from_xy(60, 60, 60, 60),
    WINDOW_EX_STYLE(0),
  );
  let tile = Fixture::new(
    w!("GlazeWMFloatRaceTile"),
    BLACK_BRUSH,
    &Rect::from_xy(60, 60, 60, 60),
    WINDOW_EX_STYLE(0),
  );
  let source_window = NativeWindow::from_handle(source.0 .0);
  let tile_window = NativeWindow::from_handle(tile.0 .0);
  source_window
    .set_z_order(&WindowZOrder::Normal)
    .expect("normal float");
  tile_window
    .set_z_order(&WindowZOrder::Normal)
    .expect("normal tile");
  let source_session = NativeSession::new(source_window.clone(), 12_501)
    .expect("float recovery");
  let tile_session = NativeSession::new(tile_window.clone(), 12_502)
    .expect("tile recovery");
  let expected = native_stacking_context().expect("pre-overlay snapshot");
  source_session
    .present(true)
    .expect("publish active overlay");
  run(
    &[&source_session, &tile_session],
    vec![source_window.id()],
    vec![tile_window.id()],
    expected,
  );
  assert!(crate::floating_raise_plan(
    &native_stacking_context().expect("post-overlay context"),
    &[source_window.id()],
    &[tile_window.id()]
  )
  .expect("still unsatisfied without mutation")
  .contains(&source_window.id()));
  source_session
    .present(false)
    .expect("release overlay ownership");
  let expected = native_stacking_context().expect("pre-modal snapshot");
  let dialog = Fixture::new(
    w!("GlazeWMFloatUnmanagedModal"),
    WHITE_BRUSH,
    &Rect::from_xy(140, 60, 60, 60),
    WINDOW_EX_STYLE(0),
  );
  // SAFETY: Creates a classic modal-family relation on our tile fixture
  // only.
  unsafe {
    SetWindowLongPtrW(dialog.0, GWLP_HWNDPARENT, tile.0 .0);
    EnableWindow(tile.0, false);
  }
  run(
    &[&source_session, &tile_session],
    vec![source_window.id()],
    vec![tile_window.id()],
    expected,
  );
  assert!(crate::floating_raise_plan(
    &native_stacking_context().expect("new modal retained"),
    &[source_window.id()],
    &[tile_window.id()]
  )
  .expect("ownership overrides layer")
  .is_empty());
  drop(dialog);
  // SAFETY: Re-enables only our still-live tile fixture.
  unsafe {
    EnableWindow(tile.0, true);
  }
  let expected =
    native_stacking_context().expect("before independent topmost change");
  source_window
    .set_z_order(&WindowZOrder::TopMost)
    .expect("app promotes itself");
  run(
    &[&source_session, &tile_session],
    vec![source_window.id()],
    vec![tile_window.id()],
    expected,
  );
  assert!(
    source_window.has_window_style_ex(WS_EX_TOPMOST),
    "late topmost flag was demoted"
  );
  assert_eq!(focus_identity(), focus);
  source_session.release().expect("release source");
  tile_session.release().expect("release tile");
}
