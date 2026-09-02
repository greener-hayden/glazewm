use std::{
  sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
  },
  time::Duration,
};

use crate::platform_impl;

/// Ticks once per composed frame, on a thread of its own.
///
/// Timers are the wrong tool for pacing an animation. A tokio interval
/// on Windows resolves to the 15.6ms system timer whatever it asks for,
/// so a 155Hz monitor got nine frames of a 140ms animation and the odd
/// 31ms hitch. The compositor's own clock is what frames are made to.
///
/// # Platform-specific
///
/// - Windows: waits on the DirectComposition compositor clock, so a tick
///   lands right after each vblank and an update issued from it is
///   composed on the very next frame.
/// - macOS: sleeps for one frame at `fallback_rate`. `thread::sleep` is
///   precise to well under a millisecond there.
pub struct FrameClock {
  stopped: Arc<AtomicBool>,
}

impl FrameClock {
  /// Starts ticking, calling `on_tick` after each frame until it returns
  /// `false` or the clock is stopped.
  ///
  /// `fallback_rate` paces the clock where no compositor clock is
  /// available.
  pub fn start<F>(fallback_rate: u32, mut on_tick: F) -> Self
  where
    F: FnMut() -> bool + Send + 'static,
  {
    let stopped = Arc::new(AtomicBool::new(false));
    let frame =
      Duration::from_secs_f64(1.0 / f64::from(fallback_rate.max(1)));

    let thread_stopped = stopped.clone();
    std::thread::spawn(move || {
      while !thread_stopped.load(Ordering::Relaxed) {
        platform_impl::wait_for_frame(frame);

        if thread_stopped.load(Ordering::Relaxed) || !on_tick() {
          break;
        }
      }
    });

    Self { stopped }
  }

  /// Stops the clock. Its thread exits after the frame it is waiting on.
  pub fn stop(&self) {
    self.stopped.store(true, Ordering::Relaxed);
  }
}

impl Drop for FrameClock {
  fn drop(&mut self) {
    self.stop();
  }
}
