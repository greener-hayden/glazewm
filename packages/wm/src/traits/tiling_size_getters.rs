use std::cell::Ref;

use ambassador::delegatable_trait;
use anyhow::Context;
use wm_common::GapsConfig;

use super::CommonGetters;
use crate::models::MonitorMetrics;

pub const MIN_TILING_SIZE: f32 = 0.01;

#[delegatable_trait]
pub trait TilingSizeGetters: CommonGetters {
  fn tiling_size(&self) -> f32;

  fn set_tiling_size(&self, tiling_size: f32);

  fn gaps_config(&self) -> Ref<'_, GapsConfig>;

  fn set_gaps_config(&self, gaps_config: GapsConfig);

  /// Gets the horizontal and vertical gaps between windows in pixels.
  fn inner_gaps(&self) -> anyhow::Result<(i32, i32)> {
    let monitor = self.monitor().context("No monitor.")?;
    Ok(self.inner_gaps_with(&monitor.metrics()))
  }

  /// Gets the horizontal and vertical gaps between windows in pixels,
  /// given the measurements of the monitor that the container is on.
  ///
  /// Same as `inner_gaps`, for callers that already hold the metrics and
  /// so do not need to look the monitor up for every container.
  fn inner_gaps_with(&self, metrics: &MonitorMetrics) -> (i32, i32) {
    let gaps_config = self.gaps_config();

    let scale_factor = if gaps_config.scale_with_dpi {
      metrics.scale_factor
    } else {
      1.
    };

    (
      gaps_config
        .inner_gap
        .to_px(metrics.height, Some(scale_factor)),
      gaps_config
        .inner_gap
        .to_px(metrics.width, Some(scale_factor)),
    )
  }
}

/// Logs a tiling size that can never be laid out, with where it came from.
///
/// Such a size gives its tile no space, so the backtrace names the code
/// path that wrote it. Logged at error level to reach `errors.log`.
pub fn report_invalid_tiling_size(tiling_size: f32) {
  if !tiling_size.is_finite() || tiling_size <= 0. {
    tracing::error!(
      size = tiling_size,
      backtrace = %std::backtrace::Backtrace::force_capture(),
      "Non-finite or non-positive tiling size."
    );
  }
}

/// Implements the `TilingSizeGetters` trait for a given struct.
///
/// Expects that the struct has a wrapping `RefCell` containing a struct
/// with a `tiling_size` field.
#[macro_export]
macro_rules! impl_tiling_size_getters {
  ($struct_name:ident) => {
    impl TilingSizeGetters for $struct_name {
      fn tiling_size(&self) -> f32 {
        self.0.borrow().tiling_size
      }

      fn set_tiling_size(&self, tiling_size: f32) {
        $crate::traits::report_invalid_tiling_size(tiling_size);
        self.0.borrow_mut().tiling_size = tiling_size;
      }

      fn gaps_config(&self) -> Ref<'_, GapsConfig> {
        Ref::map(self.0.borrow(), |inner| &inner.gaps_config)
      }

      fn set_gaps_config(&self, gaps_config: GapsConfig) {
        self.0.borrow_mut().gaps_config = gaps_config;
      }
    }
  };
}
