use std::{pin::Pin, time::Instant};

use tokio::time::Sleep;

/// One pinned tokio sleep, reset in place for every arm.
///
/// Mach timers are already precise, so no dedicated thread is needed.
pub(crate) struct NativeDeadlineTimer {
  /// Created on first arm, since a sleep needs a runtime context.
  sleep: Option<Pin<Box<Sleep>>>,
  /// Generation of the pending arm, or `None` when cancelled or fired.
  generation: Option<u64>,
}

impl NativeDeadlineTimer {
  /// Creates an idle timer.
  #[allow(clippy::unnecessary_wraps)]
  pub(crate) fn new() -> crate::Result<Self> {
    Ok(Self {
      sleep: None,
      generation: None,
    })
  }

  /// Programs the sleep to complete at `due`, replacing any pending one.
  #[allow(clippy::unnecessary_wraps)]
  pub(crate) fn arm(
    &mut self,
    due: Instant,
    generation: u64,
  ) -> crate::Result<()> {
    let due = tokio::time::Instant::from_std(due);

    match self.sleep.as_mut() {
      Some(sleep) => sleep.as_mut().reset(due),
      None => self.sleep = Some(Box::pin(tokio::time::sleep_until(due))),
    }

    self.generation = Some(generation);
    Ok(())
  }

  /// Stops a pending fire.
  pub(crate) fn cancel(&mut self) {
    self.generation = None;
  }

  /// Resolves with the generation of the pending arm once it is due.
  ///
  /// Pends forever when nothing is armed. Cancel-safe: the arm is consumed
  /// only in the step that returns it.
  #[allow(clippy::unnecessary_wraps)]
  pub(crate) async fn next_fire(&mut self) -> crate::Result<u64> {
    let (Some(sleep), Some(generation)) =
      (self.sleep.as_mut(), self.generation)
    else {
      return std::future::pending().await;
    };

    sleep.as_mut().await;
    self.generation = None;
    Ok(generation)
  }
}
