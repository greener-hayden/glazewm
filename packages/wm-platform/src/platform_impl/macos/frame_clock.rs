use std::time::Duration;

/// Implements the platform half of [`FrameClock`](crate::FrameClock).
///
/// Sleeps for one frame. macOS exposes no compositor clock short of a
/// `CVDisplayLink`, and `thread::sleep` is precise enough there.
pub(crate) fn wait_for_frame(frame: Duration) {
  std::thread::sleep(frame);
}
