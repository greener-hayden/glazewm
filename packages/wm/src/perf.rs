//! Timing of the work behind a key press, a redraw or a monitor change.
//!
//! Every span logs under the [`PERF_TARGET`] target: at `INFO` once it
//! runs past its threshold, otherwise at `DEBUG`. Each line holds
//! `key=value` fields, including the deltas of
//! `wm_platform::NativeCallStats`, so a slow span names the native calls
//! it made. Read them by searching for a span name (e.g. `platform_sync`)
//! or a field such as `key_ms=` or `hops=`.
//!
//! The target is the one hook for routing these lines elsewhere, e.g. to a
//! dedicated file layer filtered on [`PERF_TARGET`] in `setup_logging`.
use std::{
  fmt,
  time::{Duration, Instant},
};

use wm_platform::{NativeCallSnapshot, NativeCallStats};

/// Log target of every line written by this module.
pub const PERF_TARGET: &str = "perf";

/// A whole span (e.g. a sync) slower than this is logged at `INFO`.
pub const SLOW_SPAN: Duration = Duration::from_millis(16);

/// A single window pass or native call slower than this is logged at
/// `INFO`.
pub const SLOW_CALL: Duration = Duration::from_millis(8);

/// What set a native sync going.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncTrigger {
  /// A keybinding.
  Key,
  /// A command from the CLI or another IPC client.
  Ipc,
  /// A mouse event.
  Mouse,
  /// A batch of window events.
  WindowEvent,
  /// A presentation frame.
  Tick,
  /// A placement recovery deadline.
  Deadline,
  /// A debounced display topology change.
  Topology,
  /// A debounced off-screen focus follow.
  Follow,
  /// Initial population of the WM state.
  Startup,
  /// Anything else, such as shutdown or tray commands.
  Other,
}

impl SyncTrigger {
  /// Gets the name logged in the `trigger` field.
  #[must_use]
  pub const fn as_str(self) -> &'static str {
    match self {
      Self::Key => "key",
      Self::Ipc => "ipc",
      Self::Mouse => "mouse",
      Self::WindowEvent => "window_event",
      Self::Tick => "tick",
      Self::Deadline => "deadline",
      Self::Topology => "topology",
      Self::Follow => "follow",
      Self::Startup => "startup",
      Self::Other => "other",
    }
  }
}

impl fmt::Display for SyncTrigger {
  /// Writes the name logged in the `trigger` field.
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(self.as_str())
  }
}

/// Why a sync runs, and where its latency is measured from.
///
/// Passed down to the sync rather than stored in `WmState`, so a stale
/// origin cannot label a later sync.
#[derive(Clone, Copy, Debug)]
pub struct SyncOrigin {
  /// What set the sync going.
  pub trigger: SyncTrigger,
  /// When the keyboard hook matched the key press, for key triggers.
  pub key_received_at: Option<Instant>,
  /// Whether the sync is the flush after an event rather than one run by
  /// a command. One key press can log both, and `key_ms` of the flush
  /// includes the command's sync.
  pub flush: bool,
}

impl SyncOrigin {
  /// Creates an origin that has no key press behind it.
  #[must_use]
  pub const fn new(trigger: SyncTrigger) -> Self {
    Self {
      trigger,
      key_received_at: None,
      flush: false,
    }
  }

  /// Creates the origin of a key press matched at `received_at`.
  #[must_use]
  pub const fn key(received_at: Instant) -> Self {
    Self {
      trigger: SyncTrigger::Key,
      key_received_at: Some(received_at),
      flush: false,
    }
  }

  /// Marks the origin as that of an event's closing flush.
  #[must_use]
  pub const fn flushed(mut self) -> Self {
    self.flush = true;
    self
  }
}

/// Times a span of work and the native calls made during it.
///
/// The call counts are process-wide, so work on other threads during the
/// span is included.
pub struct PerfSpan {
  name: &'static str,
  started: Instant,
  calls: NativeCallSnapshot,
}

impl PerfSpan {
  /// Starts timing a span named `name`.
  #[must_use]
  pub fn start(name: &'static str) -> Self {
    Self {
      name,
      started: Instant::now(),
      calls: NativeCallStats::snapshot(),
    }
  }

  /// Gets the time since the span started.
  #[must_use]
  pub fn elapsed(&self) -> Duration {
    self.started.elapsed()
  }

  /// Ends the span and logs it, with `detail` as extra fields.
  ///
  /// Logs at `INFO` when the span took `slow_after` or longer, otherwise
  /// at `DEBUG`. Returns the span's duration.
  pub fn finish(
    self,
    slow_after: Duration,
    detail: fmt::Arguments<'_>,
  ) -> Duration {
    let total = self.started.elapsed();
    let calls = NativeCallStats::snapshot().since(&self.calls);

    emit(
      total >= slow_after,
      format_args!(
        "{} total_ms={:.2} {detail} {calls}",
        self.name,
        millis(total),
      ),
    );

    total
  }
}

/// Logs one line under [`PERF_TARGET`], at `INFO` if `slow`.
pub fn emit(slow: bool, line: fmt::Arguments<'_>) {
  if slow {
    tracing::info!(target: PERF_TARGET, "{line}");
  } else {
    tracing::debug!(target: PERF_TARGET, "{line}");
  }
}

/// Converts a duration to fractional milliseconds for logging.
#[must_use]
pub fn millis(duration: Duration) -> f64 {
  duration.as_secs_f64() * 1000.0
}

/// The fields describing one `platform_sync`.
pub struct SyncDetail {
  /// Why the sync ran.
  pub origin: SyncOrigin,
  /// Time from the key press to the start of the sync.
  pub key_queued: Option<Duration>,
  /// Windows the WM manages.
  pub windows: usize,
  /// Windows the sync reconciled.
  pub reconciled: usize,
  /// Windows still settling afterwards.
  pub settling: usize,
  /// Time spent capturing the layout snapshot.
  pub snapshot: Duration,
  /// Time spent ordering windows.
  pub order: Duration,
  /// Time spent in planning and reconciliation.
  pub reconcile: Duration,
}

impl fmt::Display for SyncDetail {
  /// Writes `key=value` fields; `key_ms` is measured at this moment.
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "trigger={}", self.origin.trigger)?;

    if self.origin.flush {
      write!(f, " flush=1")?;
    }

    // Measured at log time, so it spans the key press to the end of sync.
    if let (Some(at), Some(queued)) =
      (self.origin.key_received_at, self.key_queued)
    {
      write!(
        f,
        " key_ms={:.2} key_queued_ms={:.2}",
        millis(at.elapsed()),
        millis(queued),
      )?;
    }

    write!(
      f,
      " windows={} reconciled={} settling={} snapshot_us={} order_us={} \
       reconcile_us={}",
      self.windows,
      self.reconciled,
      self.settling,
      self.snapshot.as_micros(),
      self.order.as_micros(),
      self.reconcile.as_micros(),
    )
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn trigger_names_are_distinct() {
    let triggers = [
      SyncTrigger::Key,
      SyncTrigger::Ipc,
      SyncTrigger::Mouse,
      SyncTrigger::WindowEvent,
      SyncTrigger::Tick,
      SyncTrigger::Deadline,
      SyncTrigger::Topology,
      SyncTrigger::Follow,
      SyncTrigger::Startup,
      SyncTrigger::Other,
    ];
    let names = triggers
      .iter()
      .map(|trigger| trigger.as_str())
      .collect::<std::collections::HashSet<_>>();

    assert_eq!(names.len(), triggers.len());
  }

  #[test]
  fn key_origin_reports_key_latency() {
    let detail = SyncDetail {
      origin: SyncOrigin::key(Instant::now()),
      key_queued: Some(Duration::from_millis(1)),
      windows: 3,
      reconciled: 2,
      settling: 1,
      snapshot: Duration::ZERO,
      order: Duration::ZERO,
      reconcile: Duration::ZERO,
    };
    let text = detail.to_string();

    assert!(text.starts_with("trigger=key key_ms="), "{text}");
    assert!(!text.contains("flush"), "{text}");
    assert!(text.contains("key_queued_ms=1.00"), "{text}");
    assert!(text.contains("windows=3 reconciled=2 settling=1"), "{text}");
  }

  #[test]
  fn flushed_origin_is_marked() {
    let detail = SyncDetail {
      origin: SyncOrigin::key(Instant::now()).flushed(),
      key_queued: None,
      windows: 0,
      reconciled: 0,
      settling: 0,
      snapshot: Duration::ZERO,
      order: Duration::ZERO,
      reconcile: Duration::ZERO,
    };

    assert!(detail.to_string().starts_with("trigger=key flush=1"));
  }

  #[test]
  fn other_origins_report_no_key_latency() {
    let detail = SyncDetail {
      origin: SyncOrigin::new(SyncTrigger::Tick),
      key_queued: None,
      windows: 0,
      reconciled: 0,
      settling: 0,
      snapshot: Duration::ZERO,
      order: Duration::ZERO,
      reconcile: Duration::ZERO,
    };

    assert!(!detail.to_string().contains("key_ms"));
  }

  #[test]
  fn span_returns_its_duration() {
    let span = PerfSpan::start("test_span");
    std::thread::sleep(Duration::from_millis(2));

    let total = span.finish(Duration::MAX, format_args!("detail=1"));

    assert!(total >= Duration::from_millis(2));
  }
}
