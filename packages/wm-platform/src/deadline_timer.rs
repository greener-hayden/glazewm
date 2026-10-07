use std::time::{Duration, Instant};

use crate::platform_impl;

/// Minimum idle time between the end of one fire's handler and the next
/// fire.
///
/// The interval is measured from the first [`DeadlineTimer::set`] after a
/// fire is consumed, which is the start of the next loop iteration and so
/// the end of the handler. A caller that keeps reporting an already-passed
/// deadline (for example `Instant::now()` every iteration) therefore
/// leaves the loop idle for at least this long per fire, however long the
/// handler runs, instead of spinning it at 100% CPU. It does not bound the
/// cost of the handler itself.
pub const MIN_FIRE_INTERVAL: Duration = Duration::from_millis(1);

/// Minimum time between two logged native timer failures.
const FAILURE_LOG_INTERVAL: Duration = Duration::from_secs(1);

/// Rate limiter for native timer failure logs.
///
/// A persistent failure fires the deadline immediately on every
/// iteration, so it would otherwise log at the fire rate.
#[derive(Debug, Default)]
struct FailureLog {
  last_logged: Option<Instant>,
  suppressed: u64,
}

impl FailureLog {
  /// Records a failure at `now`.
  ///
  /// Returns the number of failures suppressed since the last logged one
  /// when this failure should be logged, and `None` when it should not.
  fn record(&mut self, now: Instant) -> Option<u64> {
    let due = self.last_logged.is_none_or(|last| {
      now.saturating_duration_since(last) >= FAILURE_LOG_INTERVAL
    });

    if due {
      self.last_logged = Some(now);
      Some(std::mem::take(&mut self.suppressed))
    } else {
      self.suppressed += 1;
      None
    }
  }
}

/// What the owner must do to the native timer after a state change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Command {
  /// The native timer is already correct.
  None,
  /// Program the native timer to fire at `due`, tagged with `generation`.
  Arm { due: Instant, generation: u64 },
  /// Stop the native timer.
  Cancel,
}

/// Result of a native fire notification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativeFire {
  /// The deadline has been reached.
  Fired,
  /// The notification came early; the timer must be re-armed.
  Rearm { due: Instant, generation: u64 },
  /// The notification belongs to a superseded arm and was dropped.
  Stale,
}

/// Position of a [`DeadlineState`] in `Idle -> Armed -> Fired -> Idle`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
  /// No deadline is pending.
  Idle,
  /// The native timer is programmed to fire at `due`.
  Armed {
    requested: Instant,
    due: Instant,
    generation: u64,
  },
  /// The deadline has been reached and awaits
  /// [`DeadlineState::take_fired`].
  Fired { requested: Instant, generation: u64 },
}

/// Platform-independent state machine behind [`DeadlineTimer`].
///
/// Pure: every transition takes the current time as an argument, so it is
/// testable without a clock or a native timer.
#[derive(Debug)]
struct DeadlineState {
  phase: Phase,
  /// Generation of the most recent arm or fire. Never zero.
  last_generation: u64,
  /// Start of the first `set` after the latest consumed fire: the end of
  /// its handler. The spin guard is measured from here.
  last_fire: Option<Instant>,
  /// A fire was consumed and `last_fire` awaits the next `set`.
  fire_unstamped: bool,
}

impl DeadlineState {
  /// Creates an idle state.
  const fn new() -> Self {
    Self {
      phase: Phase::Idle,
      last_generation: 0,
      last_fire: None,
      fire_unstamped: false,
    }
  }

  /// Issues a fresh generation, so earlier native fires become stale.
  fn next_generation(&mut self) -> u64 {
    self.last_generation = self.last_generation.wrapping_add(1).max(1);
    self.last_generation
  }

  /// Returns the deadline last passed to [`Self::set`] that is still
  /// pending, if any.
  const fn requested(&self) -> Option<Instant> {
    match self.phase {
      Phase::Idle => None,
      Phase::Armed { requested, .. } | Phase::Fired { requested, .. } => {
        Some(requested)
      }
    }
  }

  /// Whether `set(deadline)` would change nothing.
  ///
  /// False right after a fire is consumed, so the next `set` can stamp the
  /// end of that fire's handler.
  fn is_current(&self, deadline: Option<Instant>) -> bool {
    !self.fire_unstamped && deadline == self.requested()
  }

  /// Whether the native timer is expected to fire.
  const fn is_armed(&self) -> bool {
    matches!(self.phase, Phase::Armed { .. })
  }

  /// Requests that the timer fire at `deadline`, or cancels it for `None`.
  ///
  /// A deadline equal to the pending one changes nothing. A deadline that
  /// has passed (after the spin guard) goes straight to `Fired` with no
  /// native command.
  fn set(&mut self, deadline: Option<Instant>, now: Instant) -> Command {
    if self.fire_unstamped {
      self.last_fire = Some(now);
      self.fire_unstamped = false;
    }

    let Some(requested) = deadline else {
      let was_armed = self.is_armed();
      self.phase = Phase::Idle;
      return if was_armed {
        Command::Cancel
      } else {
        Command::None
      };
    };

    if self.requested() == Some(requested) {
      return Command::None;
    }

    let was_armed = self.is_armed();
    let due = self
      .last_fire
      .map_or(requested, |last| requested.max(last + MIN_FIRE_INTERVAL));

    // A changed request that resolves to the same native due time (the
    // spin guard clamping an ever-moving `now`) keeps the armed timer.
    if let Phase::Armed {
      due: armed_due,
      generation,
      ..
    } = self.phase
    {
      if armed_due == due {
        self.phase = Phase::Armed {
          requested,
          due,
          generation,
        };
        return Command::None;
      }
    }

    let generation = self.next_generation();
    if due <= now {
      self.phase = Phase::Fired {
        requested,
        generation,
      };
      return if was_armed {
        Command::Cancel
      } else {
        Command::None
      };
    }

    self.phase = Phase::Armed {
      requested,
      due,
      generation,
    };
    Command::Arm { due, generation }
  }

  /// Handles a native fire tagged `fired_generation`.
  ///
  /// A fire from a superseded arm is stale. A fire that arrives before
  /// `due` (a stale fire whose generation was read after a re-arm) leaves
  /// the state armed under a new generation.
  fn on_native_fire(
    &mut self,
    fired_generation: u64,
    now: Instant,
  ) -> NativeFire {
    let Phase::Armed {
      requested,
      due,
      generation,
    } = self.phase
    else {
      return NativeFire::Stale;
    };

    if generation != fired_generation {
      return NativeFire::Stale;
    }

    if now >= due {
      self.phase = Phase::Fired {
        requested,
        generation,
      };
      return NativeFire::Fired;
    }

    let generation = self.next_generation();
    self.phase = Phase::Armed {
      requested,
      due,
      generation,
    };
    NativeFire::Rearm { due, generation }
  }

  /// Fails safe when the native timer cannot be programmed: the deadline
  /// fires now, so deferred work runs early instead of never.
  fn fail_arm(&mut self) {
    if let Phase::Armed {
      requested,
      generation,
      ..
    } = self.phase
    {
      self.phase = Phase::Fired {
        requested,
        generation,
      };
    }
  }

  /// Consumes a reached deadline, returning to `Idle`.
  ///
  /// The spin guard starts at the next [`Self::set`], not here: the
  /// handler runs between the two.
  fn take_fired(&mut self) -> bool {
    if matches!(self.phase, Phase::Fired { .. }) {
      self.phase = Phase::Idle;
      self.fire_unstamped = true;
      true
    } else {
      false
    }
  }
}

/// A single re-armable deadline that fires close to its nominal time.
///
/// Owned by one event loop: call [`set`](Self::set) with the current
/// deadline before each wait, then await [`fired`](Self::fired) as one
/// branch of the wait. Setting the deadline it already holds costs
/// nothing, and a deadline that has already passed fires without a
/// syscall, at most once per [`MIN_FIRE_INTERVAL`].
///
/// # Platform-specific
///
/// - **Windows:** A high-resolution waitable timer waited on by a
///   dedicated thread. Timers are precise to well under a millisecond,
///   where a tokio sleep is quantized to the process timer resolution
///   (15.6 ms by default).
/// - **macOS:** One pinned tokio sleep, reset in place.
///
/// If the native timer fails (it cannot be armed, or its worker died), the
/// deadline fires immediately instead of never: deferred work runs early.
/// The failure is logged at most once per second.
pub struct DeadlineTimer {
  state: DeadlineState,
  native: platform_impl::NativeDeadlineTimer,
  failures: FailureLog,
}

impl DeadlineTimer {
  /// Creates an idle timer.
  ///
  /// Must be called from within a tokio runtime context.
  pub fn new() -> crate::Result<Self> {
    Ok(Self {
      state: DeadlineState::new(),
      native: platform_impl::NativeDeadlineTimer::new()?,
      failures: FailureLog::default(),
    })
  }

  /// Sets the instant at which [`fired`](Self::fired) resolves, or `None`
  /// to cancel.
  ///
  /// Re-arms only when `deadline` differs from the pending one. A fire
  /// that `deadline` supersedes is never delivered.
  pub fn set(&mut self, deadline: Option<Instant>) {
    // `Instant::now` is only read when the state has something to do.
    let command = if self.state.is_current(deadline) {
      Command::None
    } else {
      self.state.set(deadline, Instant::now())
    };
    self.apply(command);
  }

  /// Resolves once the deadline has been reached, then goes idle.
  ///
  /// Pends forever while idle. Cancel-safe: dropping the future before it
  /// resolves loses nothing, so it can be a `select!` branch.
  pub async fn fired(&mut self) {
    loop {
      if self.state.take_fired() {
        return;
      }
      if !self.state.is_armed() {
        std::future::pending::<()>().await;
      }

      match self.native.next_fire().await {
        Ok(generation) => {
          match self.state.on_native_fire(generation, Instant::now()) {
            NativeFire::Fired | NativeFire::Stale => {}
            NativeFire::Rearm { due, generation } => {
              self.apply(Command::Arm { due, generation });
            }
          }
        }
        Err(err) => self.fail_native(&err),
      }
    }
  }

  /// Applies a state-machine command to the native timer.
  fn apply(&mut self, command: Command) {
    match command {
      Command::None => {}
      Command::Cancel => self.native.cancel(),
      Command::Arm { due, generation } => {
        if let Err(err) = self.native.arm(due, generation) {
          self.fail_native(&err);
        }
      }
    }
  }

  /// Fires the pending deadline now after a native timer failure, logging
  /// at a bounded rate.
  fn fail_native(&mut self, err: &crate::Error) {
    if let Some(suppressed) = self.failures.record(Instant::now()) {
      tracing::error!(
        ?err,
        suppressed,
        "Deadline timer failed; firing deadlines immediately."
      );
    }
    self.state.fail_arm();
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// One millisecond, the unit the timing tests are written in.
  const MS: Duration = Duration::from_millis(1);

  /// Returns `n` milliseconds.
  fn ms(n: u32) -> Duration {
    MS * n
  }

  /// Builds a state whose latest fire was consumed and whose handler
  /// ended at `now`.
  fn fired_at(now: Instant) -> DeadlineState {
    let mut state = DeadlineState::new();
    state.set(Some(now), now + ms(10));
    assert!(state.take_fired());
    state.set(None, now);
    state
  }

  /// A future deadline arms the native timer; a firing returns to idle.
  #[test]
  fn arms_fires_and_returns_to_idle() {
    let t0 = Instant::now();
    let mut state = DeadlineState::new();

    let Command::Arm { due, generation } = state.set(Some(t0 + ms(4)), t0)
    else {
      panic!("expected an arm command");
    };
    assert_eq!(due, t0 + ms(4));
    assert!(state.is_armed());
    assert!(!state.take_fired());

    assert_eq!(
      state.on_native_fire(generation, t0 + ms(4)),
      NativeFire::Fired
    );
    assert!(!state.is_armed());
    assert!(state.take_fired());
    assert_eq!(state.requested(), None);
    assert!(!state.take_fired());
  }

  /// The deadline already pending is not re-armed.
  #[test]
  fn same_deadline_is_a_no_op() {
    let t0 = Instant::now();
    let mut state = DeadlineState::new();
    state.set(Some(t0 + ms(4)), t0);
    assert_eq!(state.set(Some(t0 + ms(4)), t0 + ms(1)), Command::None);
    assert_eq!(state.set(Some(t0 + ms(4)), t0 + ms(2)), Command::None);
  }

  /// A changed deadline re-arms under a new generation, and a fire from
  /// the superseded arm is ignored.
  #[test]
  fn re_arm_ignores_stale_generation() {
    let t0 = Instant::now();
    let mut state = DeadlineState::new();
    let Command::Arm {
      generation: old, ..
    } = state.set(Some(t0 + ms(4)), t0)
    else {
      panic!("expected an arm command");
    };
    let Command::Arm {
      due,
      generation: new,
    } = state.set(Some(t0 + ms(20)), t0 + ms(1))
    else {
      panic!("expected a re-arm command");
    };
    assert_ne!(old, new);
    assert_eq!(due, t0 + ms(20));

    assert_eq!(state.on_native_fire(old, t0 + ms(5)), NativeFire::Stale);
    assert!(state.is_armed());
    assert!(!state.take_fired());
    assert_eq!(state.on_native_fire(new, t0 + ms(20)), NativeFire::Fired);
  }

  /// A fire carrying the current generation but arriving before the due
  /// time (the re-arm race) re-arms instead of firing.
  #[test]
  fn early_native_fire_rearms_for_remainder() {
    let t0 = Instant::now();
    let mut state = DeadlineState::new();
    let Command::Arm { generation, .. } = state.set(Some(t0 + ms(4)), t0)
    else {
      panic!("expected an arm command");
    };

    let NativeFire::Rearm {
      due,
      generation: next,
    } = state.on_native_fire(generation, t0 + ms(1))
    else {
      panic!("expected a re-arm");
    };
    assert_eq!(due, t0 + ms(4));
    assert_ne!(generation, next);
    assert!(state.is_armed());
    // The superseded generation is now stale.
    assert_eq!(
      state.on_native_fire(generation, t0 + ms(4)),
      NativeFire::Stale
    );
    assert_eq!(state.on_native_fire(next, t0 + ms(4)), NativeFire::Fired);
  }

  /// `None` cancels an armed timer and is free when nothing is armed.
  #[test]
  fn none_cancels() {
    let t0 = Instant::now();
    let mut state = DeadlineState::new();
    assert_eq!(state.set(None, t0), Command::None);

    let Command::Arm { generation, .. } = state.set(Some(t0 + ms(4)), t0)
    else {
      panic!("expected an arm command");
    };
    assert_eq!(state.set(None, t0 + ms(1)), Command::Cancel);
    assert_eq!(state.requested(), None);
    assert!(!state.is_armed());
    // A fire already in flight when the timer was cancelled is ignored.
    assert_eq!(
      state.on_native_fire(generation, t0 + ms(4)),
      NativeFire::Stale
    );
    assert!(!state.take_fired());
    assert_eq!(state.set(None, t0 + ms(5)), Command::None);
  }

  /// A deadline that has passed fires without a native command.
  #[test]
  fn immediate_deadline_needs_no_native_timer() {
    let t0 = Instant::now();
    let mut state = DeadlineState::new();
    assert_eq!(state.set(Some(t0), t0), Command::None);
    assert!(!state.is_armed());
    assert!(state.take_fired());
  }

  /// A passed deadline that replaces an armed one cancels the native
  /// timer so it cannot fire again later.
  #[test]
  fn immediate_deadline_cancels_armed_timer() {
    let t0 = Instant::now();
    let mut state = DeadlineState::new();
    state.set(Some(t0 + ms(4)), t0);
    assert_eq!(state.set(Some(t0 + ms(1)), t0 + ms(2)), Command::Cancel);
    assert!(state.take_fired());
  }

  /// A reached but unconsumed deadline is replaced by a later one, so a
  /// branch that lost the race cannot cause a spurious fire.
  #[test]
  fn unconsumed_fire_is_superseded() {
    let t0 = Instant::now();
    let mut state = DeadlineState::new();
    assert_eq!(state.set(Some(t0), t0), Command::None);
    // The same deadline stays fired.
    assert_eq!(state.set(Some(t0), t0 + ms(1)), Command::None);
    assert!(matches!(state.phase, Phase::Fired { .. }));

    let Command::Arm { due, .. } =
      state.set(Some(t0 + ms(50)), t0 + ms(1))
    else {
      panic!("expected an arm command");
    };
    assert_eq!(due, t0 + ms(50));
    assert!(!state.take_fired());
  }

  /// A deadline that keeps reporting `now` fires at most once per
  /// `MIN_FIRE_INTERVAL`.
  #[test]
  fn spin_guard_bounds_the_fire_rate() {
    let t0 = Instant::now();
    let mut state = fired_at(t0);

    // Right after a fire, an immediate deadline is pushed out.
    let Command::Arm { due, .. } =
      state.set(Some(t0 + ms(0)), t0 + MS / 2)
    else {
      panic!("expected the spin guard to arm a timer");
    };
    assert_eq!(due, t0 + MIN_FIRE_INTERVAL);

    // Once the guard interval has passed, it is immediate again.
    let mut state = fired_at(t0);
    assert_eq!(
      state.set(Some(t0 + MS / 2), t0 + MIN_FIRE_INTERVAL),
      Command::None
    );
    assert!(state.take_fired());
  }

  /// The guard starts when the handler ends, not when the fire is
  /// consumed: a slow handler cannot make the next fire immediate.
  #[test]
  fn spin_guard_starts_after_the_handler() {
    let t0 = Instant::now();
    let mut state = DeadlineState::new();
    assert_eq!(state.set(Some(t0), t0), Command::None);
    assert!(state.take_fired());
    // The handler runs for 3 ms; `set` is not yet current until the
    // end of the handler is stamped.
    assert!(!state.is_current(None));

    let handler_end = t0 + ms(3);
    let Command::Arm { due, .. } =
      state.set(Some(handler_end), handler_end)
    else {
      panic!("expected the guard to hold the next fire back");
    };
    assert_eq!(due, handler_end + MIN_FIRE_INTERVAL);
    assert!(state.is_current(Some(handler_end)));
  }

  /// The guard never delays a deadline that is already past it.
  #[test]
  fn spin_guard_keeps_later_deadlines() {
    let t0 = Instant::now();
    let mut state = fired_at(t0);
    let Command::Arm { due, .. } = state.set(Some(t0 + ms(4)), t0 + ms(1))
    else {
      panic!("expected an arm command");
    };
    assert_eq!(due, t0 + ms(4));
  }

  /// Simulates 1000 loop iterations, 100 us apart, that each report `now`
  /// as the deadline: the loop idles at least `MIN_FIRE_INTERVAL` between
  /// fires and the native timer is armed once per fire rather than per
  /// iteration.
  #[test]
  fn spin_guard_caps_a_busy_loop() {
    let t0 = Instant::now();
    let mut state = DeadlineState::new();
    let mut fires = Vec::new();
    let mut arms = 0;

    for step in 0..1000u32 {
      let now = t0 + Duration::from_micros(100) * step;
      // The native timer fires once its due time is reached.
      if let Phase::Armed {
        due, generation, ..
      } = state.phase
      {
        if due <= now {
          state.on_native_fire(generation, now);
        }
      }
      if matches!(state.set(Some(now), now), Command::Arm { .. }) {
        arms += 1;
      }
      if state.take_fired() {
        fires.push(now);
      }
    }

    // Each cycle is the 100 us handler iteration plus the 1 ms guard.
    assert_eq!(fires.len(), 91);
    assert!(fires.windows(2).all(
      |w| w[1] - w[0] >= MIN_FIRE_INTERVAL + Duration::from_micros(100)
    ));
    assert_eq!(arms, 91, "one native arm per fire, not per iteration");
  }

  /// Arming failure fails safe by firing instead of dropping the deadline.
  #[test]
  fn failed_arm_fires_immediately() {
    let t0 = Instant::now();
    let mut state = DeadlineState::new();
    state.set(Some(t0 + ms(4)), t0);
    state.fail_arm();
    assert!(!state.is_armed());
    assert!(state.take_fired());
  }

  /// Failure logs are limited to one per interval, counting the rest.
  #[test]
  fn failure_log_is_rate_limited() {
    let t0 = Instant::now();
    let mut log = FailureLog::default();
    assert_eq!(log.record(t0), Some(0));
    assert_eq!(log.record(t0 + ms(1)), None);
    assert_eq!(log.record(t0 + ms(500)), None);
    assert_eq!(
      log.record(t0 + FAILURE_LOG_INTERVAL),
      Some(2),
      "the suppressed count is reported with the next log"
    );
    assert_eq!(log.record(t0 + FAILURE_LOG_INTERVAL + ms(1)), None);
  }

  /// Generations are never zero, even after wrapping.
  #[test]
  fn generations_skip_zero() {
    let mut state = DeadlineState::new();
    state.last_generation = u64::MAX;
    assert_eq!(state.next_generation(), 1);
  }

  /// Builds a current-thread runtime for timer tests.
  fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
      .enable_all()
      .build()
      .expect("tokio runtime")
  }

  /// Returns the `p`-th percentile (0..=100) of `samples`.
  fn percentile(samples: &mut [Duration], p: usize) -> Duration {
    samples.sort_unstable();
    samples[(samples.len() * p / 100).min(samples.len() - 1)]
  }

  /// Measures `count` runs of a 4 ms deadline through the real native
  /// timer, returning overshoot past the nominal 4 ms.
  fn measure_deadline_timer(count: usize) -> Vec<Duration> {
    runtime().block_on(async {
      let mut timer = DeadlineTimer::new().expect("native timer");
      let mut overshoot = Vec::with_capacity(count);
      for _ in 0..count {
        let start = Instant::now();
        timer.set(Some(start + ms(4)));
        timer.fired().await;
        let elapsed = start.elapsed();
        assert!(elapsed >= ms(4), "fired early after {elapsed:?}");
        overshoot.push(elapsed.saturating_sub(ms(4)));
        // Idle gap so each sample begins with a cold wait.
        tokio::time::sleep(ms(2)).await;
      }
      overshoot
    })
  }

  /// A 4 ms deadline fires at 4 ms, not at the next process timer tick.
  ///
  /// The default asserts only the median, below the ~11.5 ms overshoot a
  /// quantized tokio sleep shows: the tail is scheduler noise when tests
  /// run in parallel at Idle priority on a busy machine. Set
  /// `GLAZEWM_STRICT_TIMER_TEST=1` (and `--test-threads=1`) to also
  /// require the 4-6 ms p95 the timer is designed for.
  #[test]
  fn deadline_fires_close_to_nominal() {
    let mut overshoot = measure_deadline_timer(60);
    let p50 = percentile(&mut overshoot, 50);
    let p95 = percentile(&mut overshoot, 95);
    eprintln!("deadline_timer(4ms) overshoot: p50={p50:?} p95={p95:?}");

    assert!(p50 <= ms(8), "p50 overshoot {p50:?} exceeds 8 ms");
    if std::env::var_os("GLAZEWM_STRICT_TIMER_TEST").is_some() {
      assert!(p95 <= ms(2), "p95 overshoot {p95:?} exceeds 2 ms");
    }
  }

  /// Control: documents what a plain tokio sleep delivers on this host.
  ///
  /// Only the lower bound is asserted; the numbers are reported so the
  /// timer's gain over the process timer resolution is visible.
  #[test]
  fn control_tokio_sleep_overshoot() {
    let count = 60;
    let mut overshoot = runtime().block_on(async {
      let mut overshoot = Vec::with_capacity(count);
      for _ in 0..count {
        let start = Instant::now();
        tokio::time::sleep(ms(4)).await;
        let elapsed = start.elapsed();
        assert!(elapsed >= ms(4), "slept short: {elapsed:?}");
        overshoot.push(elapsed.saturating_sub(ms(4)));
      }
      overshoot
    });
    let p50 = percentile(&mut overshoot, 50);
    let p95 = percentile(&mut overshoot, 95);
    eprintln!(
      "control tokio sleep(4ms) overshoot: p50={p50:?} p95={p95:?}"
    );
  }

  /// Re-arming to a later time never delivers the earlier deadline.
  #[test]
  fn rearm_through_native_timer_skips_superseded_deadline() {
    runtime().block_on(async {
      let mut timer = DeadlineTimer::new().expect("native timer");
      let start = Instant::now();
      timer.set(Some(start + ms(5)));
      timer.set(Some(start + ms(60)));
      timer.fired().await;
      let elapsed = start.elapsed();
      assert!(
        elapsed >= ms(60),
        "fired for a superseded deadline: {elapsed:?}"
      );
    });
  }

  /// `set(None)` stops a pending fire; the timer then stays idle.
  #[test]
  fn cancel_through_native_timer_never_fires() {
    runtime().block_on(async {
      let mut timer = DeadlineTimer::new().expect("native timer");
      timer.set(Some(Instant::now() + ms(5)));
      timer.set(None);
      let outcome = tokio::time::timeout(ms(80), timer.fired()).await;
      assert!(outcome.is_err(), "a cancelled timer fired");
    });
  }

  /// A passed deadline resolves at once, without waiting on any timer.
  #[test]
  fn immediate_deadline_resolves_through_facade() {
    runtime().block_on(async {
      let mut timer = DeadlineTimer::new().expect("native timer");
      timer.set(Some(Instant::now()));
      let outcome = tokio::time::timeout(ms(500), timer.fired()).await;
      assert!(outcome.is_ok(), "an immediate deadline did not fire");
    });
  }

  /// Dropping `fired()` mid-wait loses nothing: the next call still fires.
  #[test]
  fn fired_is_cancel_safe() {
    runtime().block_on(async {
      let mut timer = DeadlineTimer::new().expect("native timer");
      let start = Instant::now();
      let deadline = start + ms(200);
      timer.set(Some(deadline));
      // Dropping the first wait must not consume the deadline. If the
      // machine stalled past 200 ms the wait completes instead, which
      // the final assertion still accepts.
      let first = tokio::time::timeout(ms(20), timer.fired()).await;
      if first.is_err() {
        timer.set(Some(deadline));
        timer.fired().await;
      }
      assert!(start.elapsed() >= ms(200));
    });
  }

  /// A dead worker fires armed and future deadlines early rather than
  /// never.
  #[cfg(target_os = "windows")]
  #[test]
  fn dead_worker_fires_deadlines_immediately() {
    runtime().block_on(async {
      let mut timer = DeadlineTimer::new().expect("native timer");
      let start = Instant::now();
      timer.set(Some(start + ms(60_000)));
      timer.native.fail_worker_for_test();
      let first = tokio::time::timeout(ms(2_000), timer.fired()).await;
      assert!(first.is_ok(), "an armed deadline never fired");

      // New deadlines cannot be armed and fire at once as well.
      timer.set(Some(Instant::now() + ms(60_000)));
      let second = tokio::time::timeout(ms(2_000), timer.fired()).await;
      assert!(second.is_ok(), "a later deadline never fired");
      assert!(start.elapsed() < ms(30_000));
    });
  }
}
