use std::{
  sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
  },
  time::Instant,
};

use tokio::sync::Notify;
use windows::Win32::{
  Foundation::{CloseHandle, HANDLE, WAIT_EVENT, WAIT_OBJECT_0},
  System::Threading::{
    CancelWaitableTimer, CreateEventW, CreateWaitableTimerExW, SetEvent,
    SetWaitableTimerEx, WaitForMultipleObjects,
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, INFINITE,
    SYNCHRONIZATION_SYNCHRONIZE, TIMER_MODIFY_STATE,
  },
};

/// Reported when the worker thread is gone and fires can no longer arrive.
const WORKER_STOPPED: &str = "Deadline timer worker stopped.";

/// An owned kernel handle that closes on drop.
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
  /// Closes the handle once the owner and worker have released it.
  fn drop(&mut self) {
    // SAFETY: This is the last reference to a handle created by this
    // module.
    if let Err(err) = unsafe { CloseHandle(self.0) } {
      tracing::error!("Deadline timer handle cleanup failed: {err}");
    }
  }
}

/// State shared between the owner and the worker thread.
struct Shared {
  /// Auto-reset, high-resolution waitable timer.
  timer: OwnedHandle,
  /// Auto-reset event that stops the worker.
  stop: OwnedHandle,
  /// Generation the owner most recently programmed into `timer`.
  armed_generation: AtomicU64,
  /// Generation of the latest fire not yet consumed, or zero.
  fired_generation: AtomicU64,
  /// Set when the worker stopped on a wait failure, so fires are lost.
  worker_failed: AtomicBool,
  /// Wakes the owner when `fired_generation` or `worker_failed` changes.
  notify: Notify,
}

/// A high-resolution waitable timer waited on by a dedicated thread.
///
/// Unlike a tokio sleep, which is rounded up to the process timer
/// resolution (15.6 ms by default), a
/// `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION` timer fires within well under a
/// millisecond of its due time without raising the system-wide tick rate.
pub(crate) struct NativeDeadlineTimer {
  shared: Arc<Shared>,
}

impl NativeDeadlineTimer {
  /// Creates the timer and starts its worker thread.
  pub(crate) fn new() -> crate::Result<Self> {
    let shared = Arc::new(Shared {
      timer: Self::create_timer()?,
      // SAFETY: Creates an unnamed owned event.
      stop: OwnedHandle(unsafe {
        CreateEventW(None, false, false, None)?
      }),
      armed_generation: AtomicU64::new(0),
      fired_generation: AtomicU64::new(0),
      worker_failed: AtomicBool::new(false),
      notify: Notify::new(),
    });

    let worker = shared.clone();
    std::thread::Builder::new()
      .name("glazewm-deadline-timer".into())
      .spawn(move || Self::run_worker(&worker))
      .map_err(|err| crate::Error::Thread(err.to_string()))?;

    Ok(Self { shared })
  }

  /// Creates the timer, falling back to the system resolution where
  /// high-resolution timers are unsupported (before Windows 10 1803).
  fn create_timer() -> crate::Result<OwnedHandle> {
    let access = SYNCHRONIZATION_SYNCHRONIZE.0 | TIMER_MODIFY_STATE.0;

    // SAFETY: Creates an unnamed owned timer; no pointers are retained.
    let high_resolution = unsafe {
      CreateWaitableTimerExW(
        None,
        None,
        CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
        access,
      )
    };

    match high_resolution {
      Ok(handle) => Ok(OwnedHandle(handle)),
      Err(err) => {
        tracing::warn!(
          "High-resolution timer unavailable, using the system \
           resolution: {err}"
        );
        // SAFETY: As above.
        Ok(OwnedHandle(unsafe {
          CreateWaitableTimerExW(None, None, 0, access)?
        }))
      }
    }
  }

  /// Waits for the timer and publishes each fire until stopped.
  fn run_worker(shared: &Shared) {
    let handles = [shared.timer.0, shared.stop.0];
    let stopped = WAIT_EVENT(WAIT_OBJECT_0.0 + 1);

    loop {
      // SAFETY: `shared` keeps both handles open for the whole call.
      let result =
        unsafe { WaitForMultipleObjects(&handles, false, INFINITE) };

      if result == WAIT_OBJECT_0 {
        // A re-arm racing this read is caught by the owner, which checks
        // the due time before honoring the fire.
        let generation = shared.armed_generation.load(Ordering::Acquire);
        shared.fired_generation.store(generation, Ordering::Release);
        shared.notify.notify_one();
      } else {
        if result != stopped {
          // Logged once: the worker exits here. The owner sees the flag,
          // fires its deadlines immediately, and rate-limits its own logs.
          tracing::error!(
            "Deadline timer wait failed: {result:?}; stopping worker."
          );
          shared.worker_failed.store(true, Ordering::Release);
          shared.notify.notify_one();
        }
        return;
      }
    }
  }

  /// Programs the timer to fire at `due`, replacing any pending fire.
  pub(crate) fn arm(
    &mut self,
    due: Instant,
    generation: u64,
  ) -> crate::Result<()> {
    if self.shared.worker_failed.load(Ordering::Acquire) {
      return Err(crate::Error::Thread(WORKER_STOPPED.into()));
    }

    // Negative due times are relative, in 100 ns units.
    let ticks = due
      .saturating_duration_since(Instant::now())
      .as_nanos()
      .div_ceil(100);
    let due_time = -i64::try_from(ticks).unwrap_or(i64::MAX).max(1);

    // Published before the timer is programmed, so a fire of this arm
    // always reads this generation.
    self
      .shared
      .armed_generation
      .store(generation, Ordering::Release);

    // SAFETY: The handle is open and `due_time` outlives the call; no
    // completion routine is registered.
    unsafe {
      SetWaitableTimerEx(
        self.shared.timer.0,
        &raw const due_time,
        0,
        None,
        None,
        None,
        0,
      )?;
    }
    Ok(())
  }

  /// Stops a pending fire.
  pub(crate) fn cancel(&mut self) {
    // SAFETY: The handle is open.
    if let Err(err) = unsafe { CancelWaitableTimer(self.shared.timer.0) } {
      tracing::error!("Deadline timer cancel failed: {err}");
    }
  }

  /// Simulates the worker thread dying on a wait failure.
  #[cfg(test)]
  pub(crate) fn fail_worker_for_test(&self) {
    self.shared.worker_failed.store(true, Ordering::Release);
    self.shared.notify.notify_one();
  }

  /// Resolves with the generation of the latest fire.
  ///
  /// Fails once the worker thread has died, so the caller can fire its
  /// deadline early instead of never. Cancel-safe: a fire is consumed
  /// only in the step that returns it.
  pub(crate) async fn next_fire(&mut self) -> crate::Result<u64> {
    loop {
      let generation =
        self.shared.fired_generation.swap(0, Ordering::AcqRel);
      if generation != 0 {
        return Ok(generation);
      }
      if self.shared.worker_failed.load(Ordering::Acquire) {
        return Err(crate::Error::Thread(WORKER_STOPPED.into()));
      }
      self.shared.notify.notified().await;
    }
  }
}

impl Drop for NativeDeadlineTimer {
  /// Stops the worker, which then releases the handles.
  fn drop(&mut self) {
    // SAFETY: The stop event is open until the worker drops `Shared`.
    if let Err(err) = unsafe { SetEvent(self.shared.stop.0) } {
      tracing::error!("Deadline timer stop failed: {err}");
    }
  }
}
