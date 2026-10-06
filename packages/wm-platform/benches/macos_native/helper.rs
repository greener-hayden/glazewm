//! Helper process that owns the window the benchmark measures.
//!
//! The helper is the benchmark executable itself, re-run with `--helper`.
//! Its window is borderless, so it never has the `AXStandardWindow`
//! subrole that the window manager's `check_is_manageable` requires on
//! macOS (it reports `AXDialog`). The running window manager may observe
//! the helper process, but it ignores the window: nothing is tiled, moved
//! or resized. The benchmark re-checks the subrole before it writes to the
//! window and refuses to go on if it is `AXStandardWindow`.
//!
//! The window is nearly transparent and ignores the mouse, so it neither
//! hides nor intercepts anything on screen. It cannot become key, so it
//! never takes focus.
//!
//! The window is resizable so accessibility can size it. Its delegate
//! makes every resize slow, standing in for an application that lays out
//! and draws on its main thread before answering the requests queued
//! behind the resize.

use std::{
  io::{BufRead, BufReader, Write},
  process::{Child, Command, Stdio},
  sync::mpsc,
  time::Duration,
};

use objc2::{
  define_class, msg_send, rc::Retained, runtime::ProtocolObject,
  DefinedClass, MainThreadMarker, MainThreadOnly,
};
use objc2_app_kit::{
  NSApplication, NSApplicationActivationPolicy, NSBackingStoreType,
  NSWindow, NSWindowDelegate, NSWindowStyleMask,
};
use objc2_foundation::{
  NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize,
};

/// Marker for the line a helper prints once its window is on screen.
const READY_PREFIX: &str = "READY";

/// How long to wait for the helper to report its window.
const READY_TIMEOUT: Duration = Duration::from_secs(10);

/// Content size of the helper window, comparable to a real tiled window.
const WINDOW_SIZE: (f64, f64) = (1280.0, 800.0);

/// Window opacity. Nonzero, so the window server still lists and captures
/// it, but low enough to be barely visible.
const WINDOW_ALPHA: f64 = 0.02;

/// Instance variables of [`SlowDelegate`].
struct SlowDelegateIvars {
  /// How long each resize blocks the helper's main thread.
  relayout: Duration,
}

define_class!(
  /// Window delegate that makes every resize take `relayout` to finish.
  ///
  /// Stands in for an application that lays out and redraws on its main
  /// thread before it answers the accessibility requests queued behind
  /// the resize.
  #[unsafe(super(NSObject))]
  #[thread_kind = MainThreadOnly]
  #[name = "BenchSlowRelayoutDelegate"]
  #[ivars = SlowDelegateIvars]
  struct SlowDelegate;

  unsafe impl NSObjectProtocol for SlowDelegate {}

  unsafe impl NSWindowDelegate for SlowDelegate {
    #[unsafe(method(windowDidResize:))]
    fn window_did_resize(&self, _notification: &NSNotification) {
      std::thread::sleep(self.ivars().relayout);
    }
  }
);

impl SlowDelegate {
  /// Creates a delegate whose resizes take `relayout`.
  fn new(mtm: MainThreadMarker, relayout: Duration) -> Retained<Self> {
    let this = mtm.alloc().set_ivars(SlowDelegateIvars { relayout });

    // SAFETY: `NSObject`'s `init` takes no arguments.
    unsafe { msg_send![super(this), init] }
  }
}

/// Runs the helper process until its parent closes its standard input.
///
/// Opens one window, prints `READY <pid> <window number>` on standard
/// output, then serves the window's accessibility requests. Exits when
/// standard input reaches end-of-file, which is also what happens if the
/// parent dies, so no helper outlives the benchmark.
pub fn run(relayout: Duration) {
  let Some(mtm) = MainThreadMarker::new() else {
    eprintln!("macos_native helper: not on the main thread.");
    std::process::exit(1);
  };

  let app = NSApplication::sharedApplication(mtm);
  app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

  let delegate = SlowDelegate::new(mtm, relayout);

  let frame = NSRect::new(
    NSPoint::new(60.0, 60.0),
    NSSize::new(WINDOW_SIZE.0, WINDOW_SIZE.1),
  );

  // SAFETY: Plain initializer call on a freshly allocated window.
  let window = unsafe {
    NSWindow::initWithContentRect_styleMask_backing_defer(
      mtm.alloc(),
      frame,
      NSWindowStyleMask::Borderless | NSWindowStyleMask::Resizable,
      NSBackingStoreType::Buffered,
      false,
    )
  };

  // SAFETY: The window is owned by this function and outlives the run
  // loop below; it must not be released when closed.
  unsafe { window.setReleasedWhenClosed(false) };
  window.setAlphaValue(WINDOW_ALPHA);
  window.setIgnoresMouseEvents(true);
  window.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
  window.orderFrontRegardless();

  println!(
    "{READY_PREFIX} {} {}",
    std::process::id(),
    window.windowNumber()
  );
  let _ = std::io::stdout().flush();

  std::thread::spawn(|| {
    let _ = std::io::copy(&mut std::io::stdin(), &mut std::io::sink());
    std::process::exit(0);
  });

  app.run();
}

/// A running helper process and the window it owns.
///
/// Dropping it closes the helper's standard input, kills the process and
/// reaps it, which closes the helper's window.
pub struct Helper {
  child: Child,
  /// Process ID of the helper.
  pub pid: i32,
  /// Window server ID of the helper's window.
  pub window_id: u32,
}

impl Helper {
  /// Starts a helper whose resizes take `relayout`, and waits until its
  /// window is on screen.
  pub fn spawn(relayout: Duration) -> Result<Self, String> {
    let exe = std::env::current_exe().map_err(|err| err.to_string())?;

    let mut child = Command::new(exe)
      .arg("--helper")
      .arg("--slow-ms")
      .arg(relayout.as_millis().to_string())
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .spawn()
      .map_err(|err| format!("could not start the helper: {err}"))?;

    let Some(stdout) = child.stdout.take() else {
      let _ = child.kill();
      let _ = child.wait();
      return Err("the helper has no standard output".to_string());
    };

    // The wait runs on a thread so a wedged helper cannot hang the
    // benchmark.
    let (line_tx, line_rx) = mpsc::channel();
    std::thread::spawn(move || {
      let mut line = String::new();
      let read = BufReader::new(stdout).read_line(&mut line);
      let _ = line_tx.send(read.map(|_| line));
    });

    let parsed = line_rx
      .recv_timeout(READY_TIMEOUT)
      .map_err(|_| {
        "the helper did not open its window in time".to_string()
      })
      .and_then(|line| line.map_err(|err| err.to_string()))
      .and_then(|line| parse_ready(&line));

    match parsed {
      Ok((pid, window_id)) => Ok(Self {
        child,
        pid,
        window_id,
      }),
      Err(err) => {
        let _ = child.kill();
        let _ = child.wait();
        Err(err)
      }
    }
  }
}

impl Drop for Helper {
  fn drop(&mut self) {
    drop(self.child.stdin.take());
    let _ = self.child.kill();
    let _ = self.child.wait();
  }
}

/// Parses the helper's `READY <pid> <window number>` line.
fn parse_ready(line: &str) -> Result<(i32, u32), String> {
  let mut parts = line.split_whitespace();

  match (parts.next(), parts.next(), parts.next()) {
    (Some(READY_PREFIX), Some(pid), Some(window_id)) => {
      let pid = pid.parse().map_err(|_| format!("bad pid in {line:?}"))?;
      let window_id = window_id
        .parse()
        .map_err(|_| format!("bad window number in {line:?}"))?;
      Ok((pid, window_id))
    }
    _ => Err(format!("unexpected helper output: {line:?}")),
  }
}
