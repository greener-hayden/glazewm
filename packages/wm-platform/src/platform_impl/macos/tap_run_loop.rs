use std::{
  sync::{atomic::AtomicBool, mpsc, Arc, OnceLock},
  time::Duration,
};

use objc2_core_foundation::CFRunLoop;

use super::event_loop::{EventLoop, StopKind};
use crate::Dispatcher;

/// How long the thread is given to publish its dispatch source.
const READY_TIMEOUT: Duration = Duration::from_secs(5);

/// The process-wide event tap run loop.
static TAP_RUN_LOOP: OnceLock<crate::Result<TapRunLoop>> = OnceLock::new();

/// A thread whose only job is to service the keyboard and mouse event
/// taps.
///
/// A `CGEventTap` needs a run loop, not the main one. Hosting the taps on
/// the main run loop put them behind every accessibility call, which is
/// cross-process and blocks: waiting on a busy application stalled input
/// for the whole system, and macOS answers a long enough stall by
/// disabling the tap outright (`kCGEventTapDisabledByTimeout`), which
/// drops keys and clicks until something re-enables it.
///
/// On its own thread the taps are answerable within microseconds no
/// matter how long a window operation takes, so accessibility work and
/// input are no longer each other's problem.
///
/// One per process, like the main run loop it exists beside. It runs for
/// the lifetime of the process; the taps themselves are invalidated by
/// their own listeners.
pub(crate) struct TapRunLoop {
  dispatcher: Dispatcher,
}

impl TapRunLoop {
  /// Starts the thread and waits for its run loop to accept dispatches.
  fn new() -> crate::Result<Self> {
    let (ready_tx, ready_rx) = mpsc::channel();

    std::thread::Builder::new()
      .name("glazewm-event-taps".to_string())
      .spawn(move || {
        let source = EventLoop::create_dispatch_source(StopKind::RunLoop);
        let is_ready = source.is_ok();

        // A receiver that has gone away means nothing will ever dispatch
        // here, so there is no run loop worth running.
        if ready_tx.send(source).is_err() || !is_ready {
          return;
        }

        // Blocks for the lifetime of the process. The dispatch source
        // keeps the run loop alive even with no taps installed.
        CFRunLoop::run();

        tracing::warn!("Event tap run loop exited.");
      })
      .map_err(|err| {
        crate::Error::Platform(format!(
          "Failed to spawn event tap thread: {err}"
        ))
      })?;

    let source = ready_rx
      .recv_timeout(READY_TIMEOUT)
      .map_err(crate::Error::ChannelRecv)??;

    tracing::info!("Started event tap run loop, off the main thread.");

    Ok(Self {
      dispatcher: Dispatcher::new(
        Some(source),
        Arc::new(AtomicBool::new(false)),
      ),
    })
  }

  /// Dispatcher for the event tap thread.
  ///
  /// Taps are created, enabled, and invalidated through this rather than
  /// through the main dispatcher, so that every one of those calls lands
  /// on the thread whose run loop owns them.
  pub(crate) fn dispatcher(&self) -> &Dispatcher {
    &self.dispatcher
  }
}

/// Gets the process-wide event tap run loop, starting it on first use.
///
/// The thread is started once and never restarted: a failure here means
/// the process could not spawn a thread, which the caller reports rather
/// than retries.
pub(crate) fn tap_run_loop() -> crate::Result<&'static TapRunLoop> {
  TAP_RUN_LOOP
    .get_or_init(TapRunLoop::new)
    .as_ref()
    .map_err(|err| {
      crate::Error::Platform(format!(
        "Event tap run loop is unavailable: {err}"
      ))
    })
}
