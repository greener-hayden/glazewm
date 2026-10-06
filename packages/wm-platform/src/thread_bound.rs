use core::{
  fmt,
  mem::{self, ManuallyDrop},
};
use std::thread::ThreadId;

use crate::Dispatcher;

/// Binds a value to the current event loop thread.
///
/// `ThreadBound<T>` wraps a value created on an event loop thread and
/// guarantees that all access and destruction of that value happens on
/// the same thread, using the provided [`Dispatcher`]. This allows the
/// wrapper to be used across threads (`Send + Sync`) even when `T` itself
/// is not thread-safe.
///
/// Inspired by:
/// - `threadbound::ThreadBound` <https://github.com/dtolnay/threadbound>
/// - `dispatch2::MainThreadBound` <https://github.com/madsmtm/objc2/tree/main/crates/dispatch2>
///
/// NOTE: Dropping the wrapper schedules the inner value to be dropped on
/// the event loop thread. If the event loop has already stopped, the drop
/// is skipped to avoid running `T`'s destructor on the wrong thread,
/// potentially leaking the value.
///
/// # Example usage
///
/// ```no_run
/// use wm_platform::{EventLoop, Dispatcher, ThreadBound};
///
/// # fn main() -> wm_platform::Result<()> {
/// let (event_loop, dispatcher) = EventLoop::new()?;
///
/// // Create the value on the event loop thread.
/// let bound = dispatcher.dispatch_sync(|| {
///   ThreadBound::new(String::from("hello"), dispatcher.clone())
/// })?;
///
/// // Access from any thread via the dispatcher.
/// let len = bound.with(|s| s.len())?;
/// assert_eq!(len, 5);
///
/// // Direct access only works on the original thread.
/// assert!(bound.get_ref().is_ok());
///
/// drop(bound); // Drop is scheduled on the event loop thread.
/// # Ok(()) }
/// ```
#[derive(Clone)]
pub struct ThreadBound<T> {
  value: ManuallyDrop<T>,
  thread_id: ThreadId,
  dispatcher: Dispatcher,
}

// SAFETY: Access to the inner value is only exposed on the event loop
// thread.
unsafe impl<T> Send for ThreadBound<T> {}
unsafe impl<T> Sync for ThreadBound<T> {}

impl<T> ThreadBound<T> {
  /// Binds a value to the current thread without an event loop.
  ///
  /// For tests, which have no running event loop to bind to. The value is
  /// never handed back and is leaked rather than dropped, because
  /// dropping it would need the thread its dispatcher cannot reach.
  #[cfg(feature = "test_utils")]
  #[must_use]
  pub fn mock(inner: T) -> Self {
    // Marked stopped, so the drop below dispatches nowhere instead of
    // reaching for an event loop source that does not exist.
    let dispatcher = Dispatcher::new(
      None,
      std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
    );

    Self {
      value: ManuallyDrop::new(inner),
      thread_id: std::thread::current().id(),
      dispatcher,
    }
  }

  /// Binds a value to the current event loop thread.
  ///
  /// # Panics
  ///
  /// Panics if `dispatcher` is not tied to an event loop running on the
  /// current thread.
  #[inline]
  pub fn new(inner: T, dispatcher: Dispatcher) -> Self {
    let thread_id = std::thread::current().id();

    // Ensure the dispatcher is tied to the same thread.
    assert_eq!(thread_id, dispatcher.thread_id());

    Self {
      value: ManuallyDrop::new(inner),
      thread_id,
      dispatcher,
    }
  }

  /// Returns `Ok(&T)` if called on the event loop thread.
  ///
  /// # Errors
  ///
  /// Returns `Error::NotMainThread` if called from a different thread.
  #[inline]
  pub fn get_ref(&self) -> crate::Result<&T> {
    if self.is_event_loop_thread() {
      Ok(&self.value)
    } else {
      Err(crate::Error::NotMainThread)
    }
  }

  /// Returns `Ok(&mut T)` if called on the event loop thread.
  ///
  /// # Errors
  ///
  /// Returns `Error::NotMainThread` if called from a different thread.
  #[inline]
  pub fn get_mut(&mut self) -> crate::Result<&mut T> {
    if self.is_event_loop_thread() {
      Ok(&mut self.value)
    } else {
      Err(crate::Error::NotMainThread)
    }
  }

  /// Consumes the wrapper and returns `Ok(T)` if called on the event loop
  /// thread.
  ///
  /// # Errors
  ///
  /// Returns `Error::NotMainThread` if called from a different thread.
  #[inline]
  pub fn into_inner(self) -> crate::Result<T> {
    if self.is_event_loop_thread() {
      // Prevent `Drop` from running.
      let mut this = ManuallyDrop::new(self);

      // SAFETY: `self` is consumed by this function, and wrapped in
      // `ManuallyDrop`, so the item's destructor is never run.
      Ok(unsafe { ManuallyDrop::take(&mut this.value) })
    } else {
      Err(crate::Error::NotMainThread)
    }
  }

  /// Consumes the wrapper and drops the inner value on the event loop
  /// thread, without waiting for it to get there.
  ///
  /// Dropping the wrapper blocks until the event loop thread has dropped
  /// the value. Use this where the caller is not a thread that can afford
  /// that hop, and nothing needs the value to be gone by the time it
  /// returns. The drop runs inline on the event loop thread.
  ///
  /// Like dropping the wrapper, the value is leaked rather than dropped on
  /// the wrong thread if the event loop has stopped or stops before it
  /// reaches the queued drop.
  pub fn drop_async(self)
  where
    T: 'static,
  {
    /// Carries a value to the event loop thread to be dropped there.
    ///
    /// The value stays in a `ManuallyDrop`, so a closure that is dropped
    /// without running leaks it instead of dropping it on whichever
    /// thread dropped the closure.
    struct Deferred<T>(ManuallyDrop<T>);

    // SAFETY: The value is only ever dropped by the closure below, which
    // runs on the event loop thread the value is bound to.
    unsafe impl<T> Send for Deferred<T> {}

    // Prevent `Drop` from running, which would hop synchronously.
    let mut this = ManuallyDrop::new(self);

    // SAFETY: `self` is wrapped in `ManuallyDrop`, so neither field is
    // used or dropped again after being moved out here.
    let (value, dispatcher) = unsafe {
      (
        ManuallyDrop::take(&mut this.value),
        std::ptr::read(&raw const this.dispatcher),
      )
    };

    if !mem::needs_drop::<T>() {
      return;
    }

    let deferred = Deferred(ManuallyDrop::new(value));

    let _ = dispatcher.dispatch_async(move || {
      // Taking the whole struct keeps the closure capturing `Deferred`
      // (which is `Send`) rather than its non-`Send` field.
      let mut deferred = deferred;

      // SAFETY: Runs on the event loop thread the value was created on
      // (guaranteed by `new`), and the value is never used again.
      unsafe { ManuallyDrop::drop(&mut deferred.0) };
    });
  }

  /// Execute a closure with `&T` on the event loop thread.
  ///
  /// Runs synchronously and returns the closure's result.
  #[inline]
  pub fn with<F, R>(&self, f: F) -> crate::Result<R>
  where
    F: Send + FnOnce(&T) -> R,
    R: Send,
  {
    self.dispatcher.dispatch_sync(|| f(&self.value))
  }

  /// Execute a closure with `&mut T` on the event loop thread.
  ///
  /// Runs synchronously and returns the closure's result.
  #[inline]
  #[allow(
    clippy::borrow_as_ptr,
    clippy::ptr_as_ptr,
    clippy::as_conversions
  )]
  pub fn with_mut<F, R>(&mut self, f: F) -> crate::Result<R>
  where
    F: Send + FnOnce(&mut T) -> R,
    R: Send,
  {
    // TODO: This is pretty cursed. Should be a better way.
    let value_ptr =
      std::ptr::from_mut::<ManuallyDrop<T>>(&mut self.value) as usize;
    self.dispatcher.dispatch_sync(|| unsafe {
      // SAFETY: The closure executes on the event loop thread where the
      // value was created, and we only create a unique mutable reference.
      let value_mut: &mut T = &mut *(value_ptr as *mut T);
      f(value_mut)
    })
  }

  /// Returns `true` if called on the event loop thread.
  #[inline]
  #[must_use]
  pub fn is_event_loop_thread(&self) -> bool {
    std::thread::current().id() == self.thread_id
  }
}

impl<T> fmt::Debug for ThreadBound<T> {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("ThreadBound").finish_non_exhaustive()
  }
}

impl<T> Drop for ThreadBound<T> {
  #[allow(
    clippy::borrow_as_ptr,
    clippy::ptr_as_ptr,
    clippy::as_conversions,
    clippy::ref_as_ptr
  )]
  fn drop(&mut self) {
    if mem::needs_drop::<T>() {
      // TODO: This is pretty cursed. Should be a better way.
      let value_ptr =
        std::ptr::from_mut::<ManuallyDrop<T>>(&mut self.value) as usize;

      let _ = self.dispatcher.dispatch_sync(|| unsafe {
        // SAFETY: The value is dropped on the event loop thread, which is
        // the same thread that it originated from (guaranteed by `new`).
        // Additionally, the value is never used again after this point.
        ManuallyDrop::drop(&mut *(value_ptr as *mut ManuallyDrop<T>));
      });
    }
  }
}

#[cfg(test)]
mod tests {
  use std::sync::{Arc, Mutex};

  use super::*;
  use crate::{EventLoop, NativeCallStats};

  /// Records the thread it is dropped on.
  struct DropProbe(Arc<Mutex<Option<ThreadId>>>);

  impl Drop for DropProbe {
    fn drop(&mut self) {
      *self.0.lock().unwrap() = Some(std::thread::current().id());
    }
  }

  /// Binds a probe to the event loop thread from a worker thread.
  fn bound_probe(
    dispatcher: &Dispatcher,
    dropped_on: &Arc<Mutex<Option<ThreadId>>>,
  ) -> ThreadBound<DropProbe> {
    let probe = DropProbe(dropped_on.clone());

    dispatcher
      .dispatch_sync(|| ThreadBound::new(probe, dispatcher.clone()))
      .unwrap()
  }

  #[test]
  fn drop_async_drops_on_the_event_loop_thread_without_a_hop() {
    let (event_loop, dispatcher) = EventLoop::new().unwrap();
    let dropped_on = Arc::new(Mutex::new(None));

    let worker_dropped_on = dropped_on.clone();
    let worker = std::thread::spawn(move || {
      let bound = bound_probe(&dispatcher, &worker_dropped_on);

      let before = NativeCallStats::snapshot();
      bound.drop_async();
      let spent = NativeCallStats::snapshot().since(&before);

      // Queued after the drop, so it only returns once the drop has run.
      dispatcher.dispatch_sync(|| {}).unwrap();
      let event_loop_thread = dispatcher.thread_id();

      dispatcher.stop_event_loop().unwrap();
      (spent.hops, event_loop_thread)
    });

    event_loop.run().unwrap();
    let (hops, event_loop_thread) = worker.join().unwrap();

    // Counters are process-wide and other tests may hop meanwhile, so the
    // exact count only holds where the tests run one at a time (macOS).
    #[cfg(target_os = "macos")]
    assert_eq!(hops, 0);
    #[cfg(not(target_os = "macos"))]
    let _ = hops;
    assert_eq!(*dropped_on.lock().unwrap(), Some(event_loop_thread));
  }

  #[test]
  fn drop_async_leaks_instead_of_dropping_after_the_loop_stopped() {
    let (event_loop, dispatcher) = EventLoop::new().unwrap();
    let dropped_on = Arc::new(Mutex::new(None));

    let worker_dropped_on = dropped_on.clone();
    let worker = std::thread::spawn(move || {
      let bound = bound_probe(&dispatcher, &worker_dropped_on);

      dispatcher.stop_event_loop().unwrap();
      bound.drop_async();
    });

    event_loop.run().unwrap();
    worker.join().unwrap();

    // Dropping on the worker thread instead would be unsound.
    assert_eq!(*dropped_on.lock().unwrap(), None);
  }

  #[test]
  fn drop_waits_for_the_event_loop_thread_to_drop() {
    let (event_loop, dispatcher) = EventLoop::new().unwrap();
    let dropped_on = Arc::new(Mutex::new(None));

    let worker_dropped_on = dropped_on.clone();
    let worker = std::thread::spawn(move || {
      let bound = bound_probe(&dispatcher, &worker_dropped_on);

      let before = NativeCallStats::snapshot();
      drop(bound);
      let spent = NativeCallStats::snapshot().since(&before);

      // Already dropped by the time `drop` returned.
      let dropped = worker_dropped_on.lock().unwrap().is_some();
      dispatcher.stop_event_loop().unwrap();
      (dropped, spent.hops)
    });

    event_loop.run().unwrap();
    let (dropped, hops) = worker.join().unwrap();

    assert!(dropped);
    // Other tests may hop meanwhile, so only a lower bound holds, except
    // where the tests run one at a time (macOS).
    assert!(hops >= 1);
    #[cfg(target_os = "macos")]
    assert_eq!(hops, 1);
  }
}
