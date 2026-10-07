use std::{
  fmt,
  sync::atomic::{AtomicU64, Ordering},
  time::Duration,
};

/// A kind of native call counted by [`NativeCallStats`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeCall {
  /// An accessibility attribute read.
  AxRead,
  /// An accessibility attribute write.
  AxWrite,
  /// An accessibility action (e.g. raise or press).
  AxAction,
  /// A window server lookup of a single window.
  WindowListSingle,
  /// A window server listing of many windows at once, such as every
  /// on-screen window or the IDs of every window.
  WindowListFull,
  /// A walk over every screen.
  ScreenEnumeration,
  /// A window screenshot.
  ScreenCapture,
}

/// Process-wide counters for native calls.
///
/// Lock-free relaxed atomics, so recording costs a few nanoseconds and the
/// counters are always on. They are not scoped to a thread or a task: two
/// snapshots delimit everything the process did between them, including
/// work on other threads.
struct Counters {
  hops: AtomicU64,
  hop_blocked_ns: AtomicU64,
  hop_wait_ns: AtomicU64,
  hop_run_ns: AtomicU64,
  ax_reads: AtomicU64,
  ax_writes: AtomicU64,
  ax_actions: AtomicU64,
  window_list_single: AtomicU64,
  window_list_full: AtomicU64,
  screen_enumerations: AtomicU64,
  screen_captures: AtomicU64,
}

impl Counters {
  /// Creates counters that all read zero.
  const fn new() -> Self {
    Self {
      hops: AtomicU64::new(0),
      hop_blocked_ns: AtomicU64::new(0),
      hop_wait_ns: AtomicU64::new(0),
      hop_run_ns: AtomicU64::new(0),
      ax_reads: AtomicU64::new(0),
      ax_writes: AtomicU64::new(0),
      ax_actions: AtomicU64::new(0),
      window_list_single: AtomicU64::new(0),
      window_list_full: AtomicU64::new(0),
      screen_enumerations: AtomicU64::new(0),
      screen_captures: AtomicU64::new(0),
    }
  }

  /// Counts one native call.
  fn record(&self, call: NativeCall) {
    let counter = match call {
      NativeCall::AxRead => &self.ax_reads,
      NativeCall::AxWrite => &self.ax_writes,
      NativeCall::AxAction => &self.ax_actions,
      NativeCall::WindowListSingle => &self.window_list_single,
      NativeCall::WindowListFull => &self.window_list_full,
      NativeCall::ScreenEnumeration => &self.screen_enumerations,
      NativeCall::ScreenCapture => &self.screen_captures,
    };
    counter.fetch_add(1, Ordering::Relaxed);
  }

  /// Counts one cross-thread hop, the time its caller waited, and how
  /// that time split between queue wait and run time.
  fn record_hop(&self, blocked: Duration, wait: Duration, run: Duration) {
    /// Saturates after about 584 years.
    fn nanos(duration: Duration) -> u64 {
      u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
    }

    self.hops.fetch_add(1, Ordering::Relaxed);
    self
      .hop_blocked_ns
      .fetch_add(nanos(blocked), Ordering::Relaxed);
    self.hop_wait_ns.fetch_add(nanos(wait), Ordering::Relaxed);
    self.hop_run_ns.fetch_add(nanos(run), Ordering::Relaxed);
  }

  /// Reads every counter.
  ///
  /// The counters are read one after another, so a snapshot taken while
  /// other threads record may mix instants by a few calls.
  fn snapshot(&self) -> NativeCallSnapshot {
    NativeCallSnapshot {
      hops: self.hops.load(Ordering::Relaxed),
      hop_blocked_ns: self.hop_blocked_ns.load(Ordering::Relaxed),
      hop_wait_ns: self.hop_wait_ns.load(Ordering::Relaxed),
      hop_run_ns: self.hop_run_ns.load(Ordering::Relaxed),
      ax_reads: self.ax_reads.load(Ordering::Relaxed),
      ax_writes: self.ax_writes.load(Ordering::Relaxed),
      ax_actions: self.ax_actions.load(Ordering::Relaxed),
      window_list_single: self.window_list_single.load(Ordering::Relaxed),
      window_list_full: self.window_list_full.load(Ordering::Relaxed),
      screen_enumerations: self
        .screen_enumerations
        .load(Ordering::Relaxed),
      screen_captures: self.screen_captures.load(Ordering::Relaxed),
    }
  }
}

static COUNTERS: Counters = Counters::new();

/// Counts the native calls that cost the window manager its latency.
///
/// Counts attempts, whether or not the call succeeds. Read it with
/// [`NativeCallStats::snapshot`] before and after the work being measured
/// and take [`NativeCallSnapshot::since`] of the two.
///
/// # Example usage
///
/// ```
/// use wm_platform::NativeCallStats;
///
/// let before = NativeCallStats::snapshot();
/// // ... work that makes native calls ...
/// let spent = NativeCallStats::snapshot().since(&before);
/// println!("{spent}");
/// ```
///
/// # Platform-specific
///
/// - **macOS:** every counter is filled.
/// - **Windows:** only `hops`, `hop_blocked`, `hop_wait` and `hop_run`
///   are; the rest stay zero.
#[derive(Clone, Copy, Debug)]
pub struct NativeCallStats;

impl NativeCallStats {
  /// Reads the process-wide counters.
  #[must_use]
  pub fn snapshot() -> NativeCallSnapshot {
    COUNTERS.snapshot()
  }

  /// Counts one native call.
  #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
  pub(crate) fn record(call: NativeCall) {
    COUNTERS.record(call);
  }

  /// Counts one cross-thread hop, the time its caller waited, and how
  /// that time split between queue wait and run time.
  pub(crate) fn record_hop(
    blocked: Duration,
    wait: Duration,
    run: Duration,
  ) {
    COUNTERS.record_hop(blocked, wait, run);
  }
}

/// A reading of [`NativeCallStats`], or the difference of two.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NativeCallSnapshot {
  /// `Dispatcher::dispatch_sync` calls that crossed to the event loop
  /// thread. Calls made on that thread run in place and are not counted.
  pub hops: u64,
  /// Nanoseconds callers spent blocked waiting on those hops.
  pub hop_blocked_ns: u64,
  /// Nanoseconds those hops' closures sat queued before the event loop
  /// started them. A hop that never ran counts all its blocked time here.
  pub hop_wait_ns: u64,
  /// Nanoseconds the event loop spent running those hops' closures.
  /// `hop_wait_ns + hop_run_ns` is at most `hop_blocked_ns`.
  pub hop_run_ns: u64,
  /// Accessibility attribute reads.
  pub ax_reads: u64,
  /// Accessibility attribute writes.
  pub ax_writes: u64,
  /// Accessibility actions.
  pub ax_actions: u64,
  /// Window server lookups of a single window.
  pub window_list_single: u64,
  /// Window server listings of many windows at once.
  pub window_list_full: u64,
  /// Walks over every screen.
  pub screen_enumerations: u64,
  /// Window screenshots.
  pub screen_captures: u64,
}

impl NativeCallSnapshot {
  /// Gets what was counted since `earlier`.
  ///
  /// Saturates at zero, so a stale `earlier` cannot underflow.
  #[must_use]
  pub fn since(&self, earlier: &Self) -> Self {
    Self {
      hops: self.hops.saturating_sub(earlier.hops),
      hop_blocked_ns: self
        .hop_blocked_ns
        .saturating_sub(earlier.hop_blocked_ns),
      hop_wait_ns: self.hop_wait_ns.saturating_sub(earlier.hop_wait_ns),
      hop_run_ns: self.hop_run_ns.saturating_sub(earlier.hop_run_ns),
      ax_reads: self.ax_reads.saturating_sub(earlier.ax_reads),
      ax_writes: self.ax_writes.saturating_sub(earlier.ax_writes),
      ax_actions: self.ax_actions.saturating_sub(earlier.ax_actions),
      window_list_single: self
        .window_list_single
        .saturating_sub(earlier.window_list_single),
      window_list_full: self
        .window_list_full
        .saturating_sub(earlier.window_list_full),
      screen_enumerations: self
        .screen_enumerations
        .saturating_sub(earlier.screen_enumerations),
      screen_captures: self
        .screen_captures
        .saturating_sub(earlier.screen_captures),
    }
  }

  /// Gets the time callers spent blocked on hops.
  #[must_use]
  pub fn hop_blocked(&self) -> Duration {
    Duration::from_nanos(self.hop_blocked_ns)
  }

  /// Gets the time hops sat queued before the event loop started them.
  #[must_use]
  pub fn hop_wait(&self) -> Duration {
    Duration::from_nanos(self.hop_wait_ns)
  }

  /// Gets the time the event loop spent running hops.
  #[must_use]
  pub fn hop_run(&self) -> Duration {
    Duration::from_nanos(self.hop_run_ns)
  }
}

impl fmt::Display for NativeCallSnapshot {
  /// Writes `key=value` pairs, so a log line can be searched by field.
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(
      f,
      "hops={} hop_blocked_ms={:.2} hop_wait_ms={:.2} hop_run_ms={:.2} \
       ax_reads={} ax_writes={} ax_actions={} window_list_single={} \
       window_list_full={} screen_enumerations={} screen_captures={}",
      self.hops,
      self.hop_blocked().as_secs_f64() * 1000.0,
      self.hop_wait().as_secs_f64() * 1000.0,
      self.hop_run().as_secs_f64() * 1000.0,
      self.ax_reads,
      self.ax_writes,
      self.ax_actions,
      self.window_list_single,
      self.window_list_full,
      self.screen_enumerations,
      self.screen_captures,
    )
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn record_increments_only_its_counter() {
    let counters = Counters::new();

    counters.record(NativeCall::AxRead);
    counters.record(NativeCall::AxRead);
    counters.record(NativeCall::AxWrite);
    counters.record(NativeCall::AxAction);
    counters.record(NativeCall::WindowListSingle);
    counters.record(NativeCall::WindowListFull);
    counters.record(NativeCall::ScreenEnumeration);
    counters.record(NativeCall::ScreenCapture);
    counters.record_hop(
      Duration::from_micros(1500),
      Duration::from_micros(1000),
      Duration::from_micros(400),
    );

    assert_eq!(
      counters.snapshot(),
      NativeCallSnapshot {
        hops: 1,
        hop_blocked_ns: 1_500_000,
        hop_wait_ns: 1_000_000,
        hop_run_ns: 400_000,
        ax_reads: 2,
        ax_writes: 1,
        ax_actions: 1,
        window_list_single: 1,
        window_list_full: 1,
        screen_enumerations: 1,
        screen_captures: 1,
      }
    );
  }

  #[test]
  fn since_is_the_difference_and_saturates() {
    let counters = Counters::new();
    counters.record(NativeCall::AxRead);
    let before = counters.snapshot();

    counters.record(NativeCall::AxRead);
    counters.record(NativeCall::AxWrite);
    let after = counters.snapshot();

    let spent = after.since(&before);
    assert_eq!(spent.ax_reads, 1);
    assert_eq!(spent.ax_writes, 1);
    assert_eq!(spent.hops, 0);

    // Reversed, nothing underflows.
    assert_eq!(before.since(&after), NativeCallSnapshot::default());
  }

  #[test]
  fn global_stats_increment() {
    let before = NativeCallStats::snapshot();

    NativeCallStats::record(NativeCall::AxRead);
    NativeCallStats::record_hop(
      Duration::from_nanos(7),
      Duration::from_nanos(3),
      Duration::from_nanos(4),
    );

    // Other tests share the counters, so only a lower bound holds.
    let spent = NativeCallStats::snapshot().since(&before);
    assert!(spent.ax_reads >= 1);
    assert!(spent.hops >= 1);
    assert!(spent.hop_blocked_ns >= 7);
    assert!(spent.hop_wait_ns >= 3);
    assert!(spent.hop_run_ns >= 4);
  }

  #[test]
  fn display_names_every_field() {
    let text = NativeCallSnapshot::default().to_string();

    for key in [
      "hops=",
      "hop_blocked_ms=",
      "hop_wait_ms=",
      "hop_run_ms=",
      "ax_reads=",
      "ax_writes=",
      "ax_actions=",
      "window_list_single=",
      "window_list_full=",
      "screen_enumerations=",
      "screen_captures=",
    ] {
      assert!(text.contains(key), "Missing `{key}` in `{text}`.");
    }
  }
}
