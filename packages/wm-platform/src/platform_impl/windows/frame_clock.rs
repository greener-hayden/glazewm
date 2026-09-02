use std::time::Duration;

use windows::Win32::{
  Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT},
  Graphics::DirectComposition::DCompositionWaitForCompositorClock,
};

/// Implements the platform half of [`FrameClock`](crate::FrameClock).
///
/// Blocks until the compositor's next frame. Sleeps for `frame` instead
/// if the compositor clock cannot be waited on.
pub(crate) fn wait_for_frame(frame: Duration) {
  // Bound the wait so a stalled compositor (a display mode change, say)
  // cannot freeze the clock. Progress is wall-clock, so a late tick is
  // harmless.
  #[allow(clippy::cast_possible_truncation)]
  let timeout_ms = (frame.as_millis() as u32).saturating_mul(4).max(1);

  // SAFETY: No handles are passed, so the call only waits on the clock.
  let result =
    unsafe { DCompositionWaitForCompositorClock(None, timeout_ms) };

  if result != WAIT_OBJECT_0.0 && result != WAIT_TIMEOUT.0 {
    std::thread::sleep(frame);
  }
}
