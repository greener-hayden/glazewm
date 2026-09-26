use std::{f64::consts::PI, time::Duration};

/// Largest accepted `bounce` magnitude.
///
/// A bounce of `1.0` has no damping and never settles, and `-1.0` has
/// infinite damping and never moves. At `±0.9` the damping ratio is
/// `0.1` or `10`, which still settles in bounded time.
pub const SPRING_BOUNCE_LIMIT: f32 = 0.9;

/// Shortest accepted perceptual duration.
///
/// Shorter durations would make the stiffness unbounded.
pub const SPRING_MIN_DURATION: Duration = Duration::from_millis(1);

/// Longest accepted perceptual duration.
///
/// Longer durations would make the settle time unbounded in practice.
pub const SPRING_MAX_DURATION: Duration = Duration::from_secs(10);

/// Largest distance from the target, in px, at which a spring counts as
/// settled.
pub const SPRING_SETTLE_DISTANCE: f64 = 0.5;

/// Largest speed, in px per second, at which a spring counts as settled.
///
/// At the stiffness of a 300 ms spring, this speed carries a window at
/// most 0.5 px further, so snapping to the target is invisible.
pub const SPRING_SETTLE_SPEED: f64 = 10.0;

/// Damping ratios this close to `1.0` use the critically damped solution.
///
/// The under-damped and over-damped solutions divide by a term that
/// vanishes at critical damping and lose precision near it.
const CRITICAL_EPSILON: f64 = 1e-4;

/// Scaled time at which a critically damped spring from rest has
/// `e^-s (1 + s) = 0.001` of its travel left.
///
/// Used by the parameterless `EasingFunction::Spring` curve.
const CRITICAL_UNIT_SETTLE: f64 = 9.233_413;

/// Largest number of samples in the backward settle search.
const SETTLE_SEARCH_STEPS: f64 = 4096.0;

/// Smallest sample step in the backward settle search, in seconds.
const SETTLE_SEARCH_MIN_STEP: f64 = 0.000_25;

/// Bisection rounds that refine the settle boundary within one step.
const SETTLE_BISECT_ROUNDS: u32 = 32;

/// Position and velocity of one axis of a spring, relative to its
/// target.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SpringState {
  /// Position minus target, in px.
  pub displacement: f64,
  /// Velocity in px per second.
  pub velocity: f64,
}

impl SpringState {
  /// Whether the state is within the settle tolerances.
  #[must_use]
  pub fn is_settled(&self) -> bool {
    self.displacement.abs() <= SPRING_SETTLE_DISTANCE
      && self.velocity.abs() <= SPRING_SETTLE_SPEED
  }
}

/// A damped harmonic oscillator with unit mass.
///
/// Evaluation is closed-form and allocation-free for the under-damped,
/// critically damped and over-damped cases, so a spring can be sampled
/// at any time without stepping through the ones before it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Spring {
  stiffness: f64,
  damping: f64,
}

impl Spring {
  /// Creates a spring from a perceptual duration and a bounce, as
  /// `SwiftUI`'s `Spring(duration:bounce:)` does.
  ///
  /// Uses stiffness `(2π / duration)²` and, for `bounce >= 0`, damping
  /// `4π (1 - bounce) / duration`. A negative bounce is over-damped with
  /// damping `4π / (duration (1 + bounce))`. `bounce` is clamped to
  /// `±SPRING_BOUNCE_LIMIT` (a non-finite bounce is `0`), and `duration`
  /// to `SPRING_MIN_DURATION..=SPRING_MAX_DURATION`.
  ///
  /// The duration is perceptual: the motion looks finished around then,
  /// but settling takes longer. See `settle_time`.
  #[must_use]
  pub fn new(duration: Duration, bounce: f32) -> Self {
    let duration = duration
      .clamp(SPRING_MIN_DURATION, SPRING_MAX_DURATION)
      .as_secs_f64();

    let bounce = if bounce.is_finite() {
      f64::from(bounce.clamp(-SPRING_BOUNCE_LIMIT, SPRING_BOUNCE_LIMIT))
    } else {
      0.0
    };

    let stiffness = (2.0 * PI / duration).powi(2);
    let damping = if bounce >= 0.0 {
      4.0 * PI * (1.0 - bounce) / duration
    } else {
      4.0 * PI / (duration * (1.0 + bounce))
    };

    Self { stiffness, damping }
  }

  /// Stiffness, in newtons per px for unit mass.
  #[must_use]
  pub const fn stiffness(&self) -> f64 {
    self.stiffness
  }

  /// Damping coefficient, per second for unit mass.
  #[must_use]
  pub const fn damping(&self) -> f64 {
    self.damping
  }

  /// Undamped angular frequency, in radians per second.
  fn natural_frequency(&self) -> f64 {
    self.stiffness.sqrt()
  }

  /// Ratio of damping to critical damping.
  ///
  /// Below `1.0` the spring overshoots, at `1.0` it is critically damped,
  /// and above `1.0` it is over-damped.
  #[must_use]
  pub fn damping_ratio(&self) -> f64 {
    self.damping / (2.0 * self.natural_frequency())
  }

  /// Returns the state at `time` seconds after `initial`.
  ///
  /// A retarget keeps its velocity by starting a new evaluation from the
  /// current position relative to the new target and the current
  /// velocity.
  #[must_use]
  pub fn state_at(&self, initial: SpringState, time: f64) -> SpringState {
    let t = time.max(0.0);
    let (x0, v0) = (initial.displacement, initial.velocity);

    match self.solution(initial) {
      Solution::Under {
        omega0,
        alpha,
        omegad,
      } => {
        let decay = (-alpha * t).exp();
        let (sin, cos) = (omegad * t).sin_cos();
        let sin_x = (v0 + alpha * x0) / omegad;
        let sin_v = (omega0 * omega0 * x0 + alpha * v0) / omegad;

        SpringState {
          displacement: decay * (x0 * cos + sin_x * sin),
          velocity: decay * (v0 * cos - sin_v * sin),
        }
      }
      Solution::Critical { omega0, slope } => {
        let decay = (-omega0 * t).exp();

        SpringState {
          displacement: decay * (x0 + slope * t),
          velocity: decay * (v0 - omega0 * slope * t),
        }
      }
      Solution::Over {
        slow_rate,
        fast_rate,
        slow,
        fast,
      } => {
        let slow_term = slow * (slow_rate * t).exp();
        let fast_term = fast * (fast_rate * t).exp();

        SpringState {
          displacement: slow_term + fast_term,
          velocity: slow_rate * slow_term + fast_rate * fast_term,
        }
      }
    }
  }

  /// Returns the time after which a spring started at `initial` stays
  /// within `SPRING_SETTLE_DISTANCE` of its target and below
  /// `SPRING_SETTLE_SPEED`.
  ///
  /// An exponential envelope gives a time after which the spring is
  /// provably settled. A bounded backward scan from there finds the last
  /// unsettled sample, and bisection refines the boundary, so the result
  /// is deterministic and within a fraction of a millisecond of the true
  /// settle time.
  ///
  /// Returns `Duration::ZERO` for a non-finite state.
  #[must_use]
  pub fn settle_time(&self, initial: SpringState) -> Duration {
    if !initial.displacement.is_finite() || !initial.velocity.is_finite() {
      return Duration::ZERO;
    }

    let upper = self.settle_upper_bound(initial);
    if !upper.is_finite() || upper <= 0.0 {
      return Duration::ZERO;
    }

    let is_unsettled = |t: f64| !self.state_at(initial, t).is_settled();
    let step = (upper / SETTLE_SEARCH_STEPS).max(SETTLE_SEARCH_MIN_STEP);

    // Scan back from the bound to the last unsettled sample. The sample
    // after it, or the bound itself, is settled.
    let mut settled = upper;
    let mut unsettled = None;
    while settled > 0.0 {
      let t = (settled - step).max(0.0);
      if is_unsettled(t) {
        unsettled = Some(t);
        break;
      }
      settled = t;
    }

    let Some(mut low) = unsettled else {
      return Duration::ZERO;
    };

    let mut high = settled;
    for _ in 0..SETTLE_BISECT_ROUNDS {
      let mid = f64::midpoint(low, high);
      if is_unsettled(mid) {
        low = mid;
      } else {
        high = mid;
      }
    }

    // `Duration` truncates to whole nanoseconds, which can land just
    // before the boundary. A microsecond past it is still settled.
    Duration::try_from_secs_f64(high + 1e-6).unwrap_or(Duration::ZERO)
  }

  /// Returns a time, in seconds, after which the spring started at
  /// `initial` is guaranteed to stay settled.
  ///
  /// Each solution is bounded by an exponential `K e^(-rate t)` for
  /// position and for velocity, which is solved for the tolerance.
  fn settle_upper_bound(&self, initial: SpringState) -> f64 {
    let (x0, v0) = (initial.displacement, initial.velocity);

    let (position_bound, velocity_bound, rate) =
      match self.solution(initial) {
        Solution::Under {
          omega0,
          alpha,
          omegad,
        } => (
          x0.hypot((v0 + alpha * x0) / omegad),
          v0.hypot((omega0 * omega0 * x0 + alpha * v0) / omegad),
          alpha,
        ),
        Solution::Critical { omega0, slope } => {
          // `t e^(-w t) <= 2 / (e w) e^(-w t / 2)` bounds the linear
          // term by an exponential at half the rate.
          let linear = 2.0 * slope.abs() / std::f64::consts::E;
          (x0.abs() + linear / omega0, v0.abs() + linear, 0.5 * omega0)
        }
        Solution::Over {
          slow_rate,
          fast_rate,
          slow,
          fast,
        } => (
          slow.abs() + fast.abs(),
          (slow_rate * slow).abs() + (fast_rate * fast).abs(),
          -slow_rate,
        ),
      };

    let position_time = (position_bound / SPRING_SETTLE_DISTANCE).ln();
    let velocity_time = (velocity_bound / SPRING_SETTLE_SPEED).ln();

    position_time.max(velocity_time).max(0.0) / rate
  }

  /// Returns the coefficients of the closed-form solution for `initial`.
  fn solution(&self, initial: SpringState) -> Solution {
    let (x0, v0) = (initial.displacement, initial.velocity);
    let omega0 = self.natural_frequency();
    let zeta = self.damping_ratio();

    if (zeta - 1.0).abs() < CRITICAL_EPSILON {
      Solution::Critical {
        omega0,
        slope: v0 + omega0 * x0,
      }
    } else if zeta < 1.0 {
      Solution::Under {
        omega0,
        alpha: zeta * omega0,
        omegad: omega0 * (1.0 - zeta * zeta).sqrt(),
      }
    } else {
      let root = (zeta * zeta - 1.0).sqrt();
      let slow_rate = -omega0 * (zeta - root);
      let fast_rate = -omega0 * (zeta + root);
      let slow = (v0 - fast_rate * x0) / (slow_rate - fast_rate);

      Solution::Over {
        slow_rate,
        fast_rate,
        slow,
        fast: x0 - slow,
      }
    }
  }
}

/// Coefficients of the closed-form solution for one damping regime.
#[derive(Clone, Copy, Debug)]
enum Solution {
  /// `x = e^(-alpha t) (x0 cos(wd t) + (v0 + alpha x0) / wd sin(wd t))`.
  Under {
    omega0: f64,
    alpha: f64,
    omegad: f64,
  },
  /// `x = e^(-w0 t) (x0 + slope t)`.
  Critical { omega0: f64, slope: f64 },
  /// `x = slow e^(slow_rate t) + fast e^(fast_rate t)`.
  Over {
    slow_rate: f64,
    fast_rate: f64,
    slow: f64,
    fast: f64,
  },
}

/// A spring motion over a known travel, normalized for use in place of
/// `EasingFunction::apply`.
///
/// Progress `0.0` is the start and `1.0` is the settle time, where the
/// curve returns exactly `1.0`. Values in between may exceed `1.0` when
/// the spring overshoots.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpringCurve {
  spring: Spring,
  initial: SpringState,
  travel: f64,
  settle_time: Duration,
}

impl SpringCurve {
  /// Creates a curve that travels `travel` px, starting with
  /// `initial_velocity` px per second towards the target.
  ///
  /// A travel within `SPRING_SETTLE_DISTANCE` of zero has no normalized
  /// form; its curve is `1.0` throughout and settles at once. Evaluate
  /// such an axis with `Spring::state_at` instead.
  #[must_use]
  pub fn new(spring: Spring, travel: f64, initial_velocity: f64) -> Self {
    let travel = travel.abs();
    let initial = SpringState {
      displacement: -travel,
      velocity: initial_velocity,
    };

    let settle_time = if travel <= SPRING_SETTLE_DISTANCE {
      Duration::ZERO
    } else {
      spring.settle_time(initial)
    };

    Self {
      spring,
      initial,
      travel,
      settle_time,
    }
  }

  /// Time for the motion to settle, which maps to progress `1.0`.
  #[must_use]
  pub const fn settle_time(&self) -> Duration {
    self.settle_time
  }

  /// Returns the fraction of the travel covered at `progress`, the
  /// elapsed time as a fraction of the settle time.
  #[must_use]
  #[allow(clippy::cast_possible_truncation)]
  pub fn apply(&self, progress: f32) -> f32 {
    if progress >= 1.0 || self.settle_time.is_zero() {
      return 1.0;
    }

    let time =
      f64::from(progress.max(0.0)) * self.settle_time.as_secs_f64();
    let state = self.spring.state_at(self.initial, time);

    (1.0 + state.displacement / self.travel) as f32
  }
}

/// Evaluates the parameterless spring curve used by
/// `EasingFunction::Spring`.
///
/// This is a critically damped spring from rest, scaled so progress
/// `1.0` is where 0.1% of the travel remains, and snapped to `1.0`
/// there.
#[must_use]
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn critical_unit_curve(progress: f32) -> f32 {
  if progress >= 1.0 {
    return 1.0;
  }

  let s = f64::from(progress.max(0.0)) * CRITICAL_UNIT_SETTLE;
  (1.0 - (-s).exp() * (1.0 + s)) as f32
}

#[cfg(test)]
mod tests {
  use super::*;

  /// One frame at 60 Hz, in seconds.
  const FRAME: f64 = 1.0 / 60.0;

  /// Integrates the spring with RK4, independent of the closed form.
  fn integrate(
    spring: &Spring,
    initial: SpringState,
    time: f64,
    dt: f64,
  ) -> SpringState {
    let accel =
      |x: f64, v: f64| -spring.stiffness() * x - spring.damping() * v;
    let (mut x, mut v) = (initial.displacement, initial.velocity);
    let mut t = 0.0;

    while t < time {
      let h = dt.min(time - t);
      let (k1x, k1v) = (v, accel(x, v));
      let (k2x, k2v) = (
        v + 0.5 * h * k1v,
        accel(x + 0.5 * h * k1x, v + 0.5 * h * k1v),
      );
      let (k3x, k3v) = (
        v + 0.5 * h * k2v,
        accel(x + 0.5 * h * k2x, v + 0.5 * h * k2v),
      );
      let (k4x, k4v) = (v + h * k3v, accel(x + h * k3x, v + h * k3v));
      x += h / 6.0 * (k1x + 2.0 * k2x + 2.0 * k3x + k4x);
      v += h / 6.0 * (k1v + 2.0 * k2v + 2.0 * k3v + k4v);
      t += h;
    }

    SpringState {
      displacement: x,
      velocity: v,
    }
  }

  /// Returns the last time a numerically integrated spring is
  /// unsettled.
  fn simulated_settle_time(spring: &Spring, initial: SpringState) -> f64 {
    let dt = 0.000_05;
    let (mut x, mut v) = (initial.displacement, initial.velocity);
    let mut t = 0.0;
    let mut last_unsettled = 0.0;

    while t < 60.0 {
      let state = SpringState {
        displacement: x,
        velocity: v,
      };
      if !state.is_settled() {
        last_unsettled = t;
      }
      // Semi-implicit Euler is stable and accurate at this step size.
      v += dt * (-spring.stiffness() * x - spring.damping() * v);
      x += dt * v;
      t += dt;
    }

    last_unsettled
  }

  fn from_rest(travel: f64) -> SpringState {
    SpringState {
      displacement: -travel,
      velocity: 0.0,
    }
  }

  #[test]
  fn converts_duration_and_bounce_like_swiftui() {
    let spring = Spring::new(Duration::from_millis(500), 0.25);

    assert!((spring.stiffness() - (4.0 * PI).powi(2)).abs() < 1e-9);
    assert!((spring.damping() - 6.0 * PI).abs() < 1e-9);
    assert!((spring.damping_ratio() - 0.75).abs() < 1e-9);

    let over = Spring::new(Duration::from_millis(500), -0.5);
    assert!((over.damping_ratio() - 2.0).abs() < 1e-9);
  }

  #[test]
  fn closed_form_matches_integration_in_every_regime() {
    let initial = SpringState {
      displacement: -640.0,
      velocity: 1800.0,
    };

    for bounce in [0.4, 0.15, 0.0, -0.3, -0.8] {
      let spring = Spring::new(Duration::from_millis(300), bounce);

      for time in [0.02, 0.1, 0.25, 0.6, 1.2] {
        let exact = spring.state_at(initial, time);
        let numeric = integrate(&spring, initial, time, 0.000_1);

        assert!(
          (exact.displacement - numeric.displacement).abs() < 1e-3,
          "bounce {bounce} at {time}: {exact:?} vs {numeric:?}",
        );
        assert!(
          (exact.velocity - numeric.velocity).abs() < 1e-2,
          "bounce {bounce} at {time}: {exact:?} vs {numeric:?}",
        );
      }
    }
  }

  #[test]
  fn critically_damped_reaches_target_without_overshoot() {
    let spring = Spring::new(Duration::from_millis(300), 0.0);
    let initial = from_rest(1000.0);
    let settle = spring.settle_time(initial).as_secs_f64();

    let mut previous = initial.displacement;
    for step in 0..=2000 {
      let time = settle * 1.5 * f64::from(step) / 2000.0;
      let state = spring.state_at(initial, time);

      assert!(state.displacement <= 0.0, "overshot at {time}");
      assert!(state.displacement >= previous, "reversed at {time}");
      previous = state.displacement;
    }

    assert!(spring.state_at(initial, settle).is_settled());
  }

  #[test]
  fn bounce_overshoots_by_the_expected_amount() {
    let spring = Spring::new(Duration::from_millis(300), 0.15);
    let travel = 1000.0;
    let initial = from_rest(travel);

    // A spring from rest overshoots by `e^(-ζπ / sqrt(1 - ζ²))` of its
    // travel, at half a damped period.
    let zeta = spring.damping_ratio();
    let expected = (-zeta * PI / (1.0 - zeta * zeta).sqrt()).exp();

    let peak = (0..=20_000)
      .map(|step| {
        let time = 1.0 * f64::from(step) / 20_000.0;
        spring.state_at(initial, time).displacement
      })
      .fold(f64::MIN, f64::max);

    assert!((zeta - 0.85).abs() < 1e-6);
    assert!((peak / travel - expected).abs() < 1e-5, "{peak}");
    assert!(peak > 5.0);
  }

  #[test]
  fn settle_time_is_finite_and_matches_simulation() {
    let cases = [
      (300u16, 0.0, from_rest(1000.0)),
      (250, 0.1, from_rest(1920.0)),
      (350, 0.15, from_rest(400.0)),
      (300, 0.9, from_rest(2560.0)),
      (300, -0.5, from_rest(800.0)),
      (300, -0.9, from_rest(800.0)),
      (
        300,
        0.15,
        SpringState {
          displacement: 200.0,
          velocity: -4000.0,
        },
      ),
      (
        300,
        0.0,
        SpringState {
          displacement: 0.0,
          velocity: 3000.0,
        },
      ),
    ];

    for (millis, bounce, initial) in cases {
      let spring =
        Spring::new(Duration::from_millis(u64::from(millis)), bounce);
      let settle = spring.settle_time(initial).as_secs_f64();
      let simulated = simulated_settle_time(&spring, initial);

      assert!(settle.is_finite() && settle > 0.0);
      assert!(
        (settle - simulated).abs() < FRAME,
        "{millis} ms, bounce {bounce}: {settle} vs {simulated}",
      );
      assert!(settle > f64::from(millis) * 1e-3);
    }
  }

  #[test]
  fn settled_state_settles_at_once() {
    let spring = Spring::new(Duration::from_millis(300), 0.15);
    let state = SpringState {
      displacement: 0.2,
      velocity: 1.0,
    };

    assert_eq!(spring.settle_time(state), Duration::ZERO);
    assert_eq!(
      spring.settle_time(SpringState {
        displacement: f64::NAN,
        velocity: 0.0,
      }),
      Duration::ZERO,
    );
  }

  #[test]
  fn retarget_keeps_velocity() {
    let spring = Spring::new(Duration::from_millis(250), 0.1);
    let first = from_rest(900.0);
    let mid = spring.state_at(first, 0.08);
    assert!(mid.velocity.abs() > 1000.0);

    // The target moves 300 px further mid-flight. The new spring starts
    // from the same position and velocity, measured from the new target.
    let retarget = SpringState {
      displacement: mid.displacement - 300.0,
      velocity: mid.velocity,
    };
    let start = spring.state_at(retarget, 0.0);
    assert!((start.velocity - mid.velocity).abs() < 1e-9);
    assert!((start.displacement - retarget.displacement).abs() < 1e-9);

    // The position's slope just after the retarget matches the velocity
    // carried over, so motion has no kink.
    let h = 1e-6;
    let after = spring.state_at(retarget, h);
    let slope = (after.displacement - start.displacement) / h;
    assert!((slope - mid.velocity).abs() / mid.velocity.abs() < 1e-3);

    // The old motion's slope just before the retarget matches too.
    let before = spring.state_at(first, 0.08 - h);
    let old_slope = (mid.displacement - before.displacement) / h;
    assert!((old_slope - slope).abs() / slope.abs() < 1e-3);
  }

  #[test]
  fn curve_keeps_initial_velocity() {
    let spring = Spring::new(Duration::from_millis(250), 0.1);
    let curve = SpringCurve::new(spring, 600.0, 1500.0);
    let settle = curve.settle_time().as_secs_f64();

    #[allow(clippy::cast_possible_truncation)]
    let progress = (1e-4 / settle) as f32;
    let speed = f64::from(curve.apply(progress)) * 600.0
      / (f64::from(progress) * settle);

    assert!((speed - 1500.0).abs() / 1500.0 < 0.02, "{speed}");
    assert!(curve.apply(0.0).abs() < f32::EPSILON);
    assert!((curve.apply(1.0) - 1.0).abs() < f32::EPSILON);
  }

  #[test]
  fn curve_without_travel_is_complete() {
    let spring = Spring::new(Duration::from_millis(250), 0.1);
    let curve = SpringCurve::new(spring, 0.0, 0.0);

    assert_eq!(curve.settle_time(), Duration::ZERO);
    assert!((curve.apply(0.0) - 1.0).abs() < f32::EPSILON);
  }

  #[test]
  fn extreme_bounce_and_duration_are_clamped() {
    let duration = Duration::from_millis(300);

    assert_eq!(
      Spring::new(duration, 5.0),
      Spring::new(duration, SPRING_BOUNCE_LIMIT),
    );
    assert_eq!(
      Spring::new(duration, -5.0),
      Spring::new(duration, -SPRING_BOUNCE_LIMIT),
    );
    assert_eq!(
      Spring::new(duration, f32::NAN),
      Spring::new(duration, 0.0)
    );
    assert_eq!(
      Spring::new(Duration::ZERO, 0.0),
      Spring::new(SPRING_MIN_DURATION, 0.0),
    );
    assert_eq!(
      Spring::new(Duration::from_secs(3600), 0.0),
      Spring::new(SPRING_MAX_DURATION, 0.0),
    );

    for bounce in [-1.0, -SPRING_BOUNCE_LIMIT, SPRING_BOUNCE_LIMIT, 1.0] {
      let spring = Spring::new(duration, bounce);
      let settle = spring.settle_time(from_rest(2000.0));

      assert!(spring.damping_ratio() > 0.0);
      assert!(settle > Duration::ZERO && settle < Duration::from_secs(10));
    }
  }

  #[test]
  fn critical_unit_curve_is_monotonic_and_ends_at_one() {
    let remaining =
      (-CRITICAL_UNIT_SETTLE).exp() * (1.0 + CRITICAL_UNIT_SETTLE);
    assert!((remaining - 0.001).abs() < 1e-7);

    let mut previous = 0.0;
    for step in 0..=1000u16 {
      let value = critical_unit_curve(f32::from(step) / 1000.0);
      assert!((0.0..=1.0).contains(&value));
      assert!(value >= previous);
      previous = value;
    }
    assert!((critical_unit_curve(1.0) - 1.0).abs() < f32::EPSILON);
  }
}
