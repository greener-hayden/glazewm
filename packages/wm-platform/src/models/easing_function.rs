use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::spring::{critical_unit_curve, Spring};

/// Supported easing functions for animations.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EasingFunction {
  Linear,
  #[default]
  EaseInOut,
  EaseIn,
  EaseOut,
  EaseInOutCubic,
  EaseInCubic,
  EaseOutCubic,
  /// A damped spring, as `SwiftUI`'s `Spring(duration:bounce:)`.
  ///
  /// Its duration is perceptual and its bounce is configured beside it;
  /// see `spring`. Motion ends when the spring settles, not at the
  /// configured duration.
  Spring,
}

impl EasingFunction {
  /// Applies the easing function to a normalized time value (0.0 to 1.0).
  ///
  /// Returns the eased value, also in the range 0.0 to 1.0.
  ///
  /// `Spring` carries no parameters here, so it evaluates a critically
  /// damped spring from rest that spans the progress range. Use `spring`
  /// for bounce, retarget velocity and settling.
  #[must_use]
  pub fn apply(&self, progress: f32) -> f32 {
    match self {
      Self::Linear => progress,
      Self::EaseInOut => {
        if progress < 0.5 {
          2.0 * progress * progress
        } else {
          -1.0 + (4.0 - 2.0 * progress) * progress
        }
      }
      Self::EaseIn => progress * progress,
      Self::EaseOut => progress * (2.0 - progress),
      EasingFunction::EaseInOutCubic => {
        if progress < 0.5 {
          4.0 * progress * progress * progress
        } else {
          1.0 - (-2.0 * progress + 2.0).powi(3) / 2.0
        }
      }
      Self::EaseInCubic => progress * progress * progress,
      Self::EaseOutCubic => 1.0 - (1.0 - progress).powi(3),
      Self::Spring => critical_unit_curve(progress),
    }
  }

  /// Returns the spring for a perceptual `duration` and `bounce`, or
  /// `None` for a curve easing.
  #[must_use]
  pub fn spring(&self, duration: Duration, bounce: f32) -> Option<Spring> {
    matches!(self, Self::Spring).then(|| Spring::new(duration, bounce))
  }
}
