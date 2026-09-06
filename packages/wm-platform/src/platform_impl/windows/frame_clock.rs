use std::time::Duration;

use windows::Win32::{
  Foundation::{HWND, WAIT_OBJECT_0},
  Graphics::{
    DirectComposition::DCompositionWaitForCompositorClock,
    Dwm::{DwmGetCompositionTimingInfo, DWM_TIMING_INFO},
  },
};

use crate::FrameSignal;

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
        .max(1),
    })
  }

  /// Waits for real compositor progress; failures never advance readiness.
  pub(crate) fn wait(&mut self) -> FrameSignal {
    // SAFETY: No handles are passed, so this only waits on the clock.
    let result =
      unsafe { DCompositionWaitForCompositorClock(None, self.timeout_ms) };
    if result != WAIT_OBJECT_0.0 {
      return FrameSignal::Unavailable;
    }

    match Self::current_frame() {
      Ok(frame) => FrameSignal::Presented(frame),
      Err(_) => FrameSignal::Unavailable,
    }
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
