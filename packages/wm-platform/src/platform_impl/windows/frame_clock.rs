use std::time::Duration;

use windows::Win32::{
  Foundation::{HWND, WAIT_OBJECT_0, WAIT_TIMEOUT},
  Graphics::{
    DirectComposition::DCompositionWaitForCompositorClock,
    Dwm::{DwmGetCompositionTimingInfo, DWM_TIMING_INFO},
  },
};

use crate::FrameSignal;

/// Shortest wait for one compositor frame, in milliseconds.
///
/// Four frame periods alone are 16 ms at 240 Hz, which an ordinary
/// compositor hiccup under load outlasts.
const MIN_TIMEOUT_MS: u32 = 100;

/// How many waits in a row may time out before the clock is reported
/// unavailable.
const MAX_TIMEOUTS: u8 = 3;

/// A DirectComposition wait paired with an observed DWM frame serial.
pub(crate) struct NativeFrameClock {
  timeout_ms: u32,
}

impl NativeFrameClock {
  /// Creates a bounded native compositor wait.
  pub(crate) fn new(frame: Duration) -> crate::Result<Self> {
    Ok(Self {
      timeout_ms: u32::try_from(frame.as_millis())?
        .saturating_mul(4)
        .max(MIN_TIMEOUT_MS),
    })
  }

  /// Waits for real compositor progress; failures never advance readiness.
  pub(crate) fn wait(&mut self) -> FrameSignal {
    // A late frame is waited for again. Reporting the clock unavailable
    // ends its thread and cuts every running animation, which is right
    // for a compositor that has stopped and wrong for one that is slow.
    for _ in 0..MAX_TIMEOUTS {
      // SAFETY: No handles are passed, so this only waits on the clock.
      let result = unsafe {
        DCompositionWaitForCompositorClock(None, self.timeout_ms)
      };

      if result == WAIT_OBJECT_0.0 {
        return match Self::current_frame() {
          Ok(frame) => FrameSignal::Presented(frame),
          Err(_) => FrameSignal::Unavailable,
        };
      }

      if result != WAIT_TIMEOUT.0 {
        break;
      }
    }

    FrameSignal::Unavailable
  }

  /// Samples the native compositor serial for a mutation boundary.
  pub(crate) fn current_frame() -> crate::Result<u64> {
    let mut timing = DWM_TIMING_INFO {
      cbSize: u32::try_from(std::mem::size_of::<DWM_TIMING_INFO>())?,
      ..Default::default()
    };
    // SAFETY: The initialized output structure has the declared size.
    unsafe { DwmGetCompositionTimingInfo(HWND(0), &raw mut timing)? };
    Ok(timing.cFrame)
  }
}
