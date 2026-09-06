use std::{
  sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
  },
  time::Duration,
};

use crate::platform_impl;

/// A native display scheduling boundary or a non-presentation wakeup.
///
/// A presented frame establishes compositor progress, not evidence that
/// another application has painted its latest requested geometry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameSignal {
  /// Native compositor/display progress, with a monotonic frame serial.
  Presented(u64),
  /// The native clock is unavailable or has stopped producing signals.
  Unavailable,
  /// Work completed without establishing a presentation boundary.
  Wake,
}

/// Observes native display progress on a dedicated worker.
///
/// Windows uses DirectComposition and DWM timing; macOS uses a Core Video
/// display link. A timeout never becomes presentation evidence.
pub struct FrameClock {
  stopped: Arc<AtomicBool>,
}

impl FrameClock {
  /// Samples the native frame serial after issuing a presentation change.
  /// Compare subsequent signals against this value, rather than the last
  /// consumed event: older display notifications can still be queued.
  pub fn current_frame() -> crate::Result<u64> {
    platform_impl::NativeFrameClock::current_frame()
  }

  /// Starts observing native frames until `on_tick` returns `false` or
  /// the clock is stopped. `fallback_rate` only bounds interruptible
  /// waits.
  pub fn start<F>(fallback_rate: u32, mut on_tick: F) -> Self
  where
    F: FnMut(FrameSignal) -> bool + Send + 'static,
  {
    let stopped = Arc::new(AtomicBool::new(false));
    let frame =
      Duration::from_secs_f64(1.0 / f64::from(fallback_rate.max(1)));
    let thread_stopped = stopped.clone();

    std::thread::spawn(move || {
      let mut clock = match platform_impl::NativeFrameClock::new(frame) {
        Ok(clock) => clock,
        Err(err) => {
          tracing::warn!(?err, "Native presentation clock unavailable.");
          if !thread_stopped.load(Ordering::Acquire) {
            on_tick(FrameSignal::Unavailable);
          }
          return;
        }
      };

      while !thread_stopped.load(Ordering::Acquire) {
        let signal = clock.wait();
        if thread_stopped.load(Ordering::Acquire) || !on_tick(signal) {
          break;
        }
        if signal == FrameSignal::Unavailable {
          break;
        }
      }
    });

    Self { stopped }
  }

  /// Requests shutdown; native resources are released by the worker.
  pub fn stop(&self) {
    self.stopped.store(true, Ordering::Release);
  }
}

impl Drop for FrameClock {
  fn drop(&mut self) {
    self.stop();
  }
}
