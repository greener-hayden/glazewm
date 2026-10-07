//! Timing of the work behind a key press, a redraw or a monitor change.
//!
//! Every span logs under the [`PERF_TARGET`] target: at `INFO` once it
//! runs past its threshold, otherwise at `DEBUG`. Each line holds
//! `key=value` fields, including the deltas of
//! `wm_platform::NativeCallStats`, so a slow span names the native calls
//! it made. Read them by searching for a span name (e.g. `platform_sync`)
//! or a field such as `key_ms=` or `hops=`.
//!
//! # Reading the log
//!
//! `setup_logging` also writes these lines, and only these, to
//! `~/.glzr/glazewm/perf.<date>.log`. The file layer logs at the
//! verbosity level of the run: slow spans (`INFO`) by default, every span
//! (`DEBUG`) under `-v`, and nothing under `-q`. A new file starts
//! each UTC day, so the date in the name may differ from the local date.
//! Old files are deleted only when a running WM rolls over to a new day,
//! so a WM restarted within a day never trims them. Each file is a
//! day's output and is not capped in bytes.
//!
//! Each line is one span with `key=value` fields. To find a slow key
//! press, grep for `trigger=key` and sort on `key_ms=`; `hops=` and
//! `ax_reads=` count the main-thread hops and accessibility reads the span
//! made. For example: `grep 'trigger=key' perf.*.log | grep key_ms=`.
//!
//! `hop_blocked_ms=` is the time callers waited on those hops. It splits
//! into `hop_wait_ms=`, the time the closures sat in the event loop's
//! queue behind other work, and `hop_run_ms=`, the time the loop spent
//! running them. A large wait means the loop was busy; a large run means
//! the closure itself is slow.
//!
//! Native overlay creation (`overlay_new`) logs under the same target,
//! at `INFO` once it takes [`SLOW_CALL`] or longer, with the cost of each
//! step. Shell cloak calls run on a worker and are not timed here.
use std::{
  fmt,
  path::Path,
  time::{Duration, Instant},
};

use tracing::Level;
use tracing_appender::rolling::{
  InitError, RollingFileAppender, Rotation,
};
use tracing_subscriber::filter::Targets;
use wm_platform::{NativeCallSnapshot, NativeCallStats};

/// Log target of every line written by this module.
pub const PERF_TARGET: &str = "perf";

/// How many daily perf log files are kept, the current one included.
pub const PERF_LOG_FILES: usize = 3;

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

/// Creates the writer of the perf log in `directory`.
///
/// Rolls over each UTC day to `perf.<date>.log`. When it rolls over, it
/// deletes all but the newest [`PERF_LOG_FILES`] files. Nothing is deleted
/// at creation, and a day's file is not capped in bytes.
///
/// Fails if the directory or file cannot be created.
pub fn perf_log_appender(
  directory: impl AsRef<Path>,
) -> Result<RollingFileAppender, InitError> {
  RollingFileAppender::builder()
    .rotation(Rotation::DAILY)
    .filename_prefix("perf")
    .filename_suffix("log")
    .max_log_files(PERF_LOG_FILES)
    .build(directory)
}

/// Creates the filter for the perf log layer.
///
/// Passes only events under [`PERF_TARGET`], up to `level`. Being a
/// target filter, it reports `level` as its max level hint, so it does
/// not raise the global max level above `level`.
#[must_use]
pub fn perf_log_filter(level: Level) -> Targets {
  Targets::new().with_target(PERF_TARGET, level)
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

  /// Collects what a `fmt` layer writes.
  #[derive(Clone, Default)]
  struct Capture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

  impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
      self
        .0
        .lock()
        .map_err(|_| std::io::Error::other("poisoned"))?
        .extend_from_slice(buf);
      Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
      Ok(())
    }
  }

  impl Capture {
    fn text(&self) -> String {
      String::from_utf8_lossy(&self.0.lock().expect("lock").clone())
        .into_owned()
    }
  }

  /// Logs one event per target and level, returning what the perf layer
  /// at `level` wrote.
  fn capture_at(level: Level) -> String {
    use tracing_subscriber::{fmt, layer::SubscriberExt, Layer};

    let capture = Capture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::registry().with(
      fmt::Layer::new()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .with_filter(perf_log_filter(level)),
    );

    tracing::subscriber::with_default(subscriber, || {
      tracing::info!(target: PERF_TARGET, "perf_info");
      tracing::debug!(target: PERF_TARGET, "perf_debug");
      tracing::info!("other_info");
      tracing::error!(target: "wm", "other_error");
      emit(true, format_args!("emit_slow"));
      emit(false, format_args!("emit_fast"));
    });

    capture.text()
  }

  #[test]
  fn perf_filter_passes_only_the_perf_target() {
    let text = capture_at(Level::DEBUG);

    assert!(text.contains("perf_info"), "{text}");
    assert!(text.contains("perf_debug"), "{text}");
    assert!(!text.contains("other_info"), "{text}");
    assert!(!text.contains("other_error"), "{text}");
  }

  #[test]
  fn perf_filter_keeps_debug_spans_for_verbose_runs_only() {
    let text = capture_at(Level::INFO);

    assert!(text.contains("perf_info"), "{text}");
    assert!(text.contains("emit_slow"), "{text}");
    assert!(!text.contains("perf_debug"), "{text}");
    assert!(!text.contains("emit_fast"), "{text}");
  }

  #[test]
  fn perf_filter_reports_its_level_as_max_level_hint() {
    use tracing::level_filters::LevelFilter;
    use tracing_subscriber::Layer;

    let filter = perf_log_filter(Level::INFO);
    let hint =
      Layer::<tracing_subscriber::Registry>::max_level_hint(&filter);

    assert_eq!(hint, Some(LevelFilter::INFO));
  }

  #[test]
  fn perf_log_appender_writes_a_dated_file_in_the_directory() {
    use std::io::Write;

    let dir = std::env::temp_dir()
      .join(format!("glazewm-perf-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    {
      let mut appender = perf_log_appender(&dir).expect("appender");
      writeln!(appender, "line").expect("write");
      appender.flush().expect("flush");
    }

    let names = std::fs::read_dir(&dir)
      .expect("read dir")
      .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
      .collect::<Vec<_>>();
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(names.len(), 1, "{names:?}");
    assert!(
      names[0].starts_with("perf.")
        && Path::new(&names[0])
          .extension()
          .is_some_and(|ext| ext == "log"),
      "{names:?}"
    );
  }
}
