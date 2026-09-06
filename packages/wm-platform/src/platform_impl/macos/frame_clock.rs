// Core Video supports deployment targets predating AppKit display links.
#![allow(deprecated)]

use std::{
  ptr::NonNull,
  sync::{
    atomic::{AtomicU64, Ordering},
    mpsc::{self, Receiver},
  },
  time::Duration,
};

use block2::RcBlock;
use objc2_core_foundation::CFRetained;
use objc2_core_video::{CVDisplayLink, CVOptionFlags, CVTimeStamp};

use crate::FrameSignal;

/// Process-wide serials remain monotonic when a presentation clock
/// restarts.
static FRAME_SERIAL: AtomicU64 = AtomicU64::new(0);

/// Owns a display link whose callback only enqueues bounded notifications.
pub(crate) struct NativeFrameClock {
  display_link: CFRetained<CVDisplayLink>,
  frames: Receiver<u64>,
  timeout: Duration,
}

impl NativeFrameClock {
  /// Samples the latest callback serial for a presentation mutation.
  #[allow(clippy::unnecessary_wraps)]
  pub(crate) fn current_frame() -> crate::Result<u64> {
    Ok(FRAME_SERIAL.load(Ordering::Acquire))
  }

  /// Creates and starts a real display link on the presentation worker.
  pub(crate) fn new(frame: Duration) -> crate::Result<Self> {
    let mut raw_link = std::ptr::null_mut();
    // SAFETY: The initialized output pointer is writable for this call.
    let status = unsafe {
      CVDisplayLink::create_with_active_cg_displays(NonNull::from(
        &mut raw_link,
      ))
    };
    if status != 0 {
      return Err(crate::Error::Platform(format!(
        "Failed to create display link: {status}."
      )));
    }
    let raw_link = NonNull::new(raw_link).ok_or_else(|| {
      crate::Error::Platform("Display link creation returned null.".into())
    })?;
    // SAFETY: A successful Create call transfers one retained reference.
    let display_link = unsafe { CFRetained::from_raw(raw_link) };
    let (frame_tx, frames) = mpsc::sync_channel(1);
    let handler = RcBlock::new(
      move |_link: NonNull<CVDisplayLink>,
            _now: NonNull<CVTimeStamp>,
            _output: NonNull<CVTimeStamp>,
            _flags: CVOptionFlags,
            _output_flags: NonNull<CVOptionFlags>| {
        let serial = FRAME_SERIAL.fetch_add(1, Ordering::AcqRel) + 1;
        let _ = frame_tx.try_send(serial);
        0
      },
    );
    // SAFETY: Core Video copies the block, which owns its sender. No
    // callback borrows worker state or performs window-system calls.
    let status = unsafe {
      display_link.set_output_handler(RcBlock::as_ptr(&handler))
    };
    if status != 0 {
      return Err(crate::Error::Platform(format!(
        "Failed to configure display link: {status}."
      )));
    }
    let status = display_link.start();
    if status != 0 {
      return Err(crate::Error::Platform(format!(
        "Failed to start display link: {status}."
      )));
    }

    Ok(Self {
      display_link,
      frames,
      timeout: frame.saturating_mul(4),
    })
  }

  /// Waits for a genuine display-link callback, never a synthesized tick.
  pub(crate) fn wait(&mut self) -> FrameSignal {
    match self.frames.recv_timeout(self.timeout) {
      Ok(serial) => FrameSignal::Presented(serial),
      Err(_) => FrameSignal::Unavailable,
    }
  }
}

impl Drop for NativeFrameClock {
  fn drop(&mut self) {
    // Stop outside the callback, before releasing its retained block.
    self.display_link.stop();
  }
}
