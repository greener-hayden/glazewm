use wm_platform::{Display, Rect};
#[cfg(target_os = "windows")]
use wm_platform::{DisplayDeviceExtWindows, DisplayExtWindows};

#[derive(Debug, Clone, PartialEq)]
pub struct NativeMonitorProperties {
  #[cfg(target_os = "macos")]
  pub device_uuid: String,
  #[cfg(target_os = "windows")]
  pub handle: isize,
  #[cfg(target_os = "windows")]
  pub hardware_id: Option<String>,
  #[cfg(target_os = "windows")]
  pub device_path: Option<String>,
  pub refresh_rate: Option<u32>,
  pub device_name: String,
  pub working_area: Rect,
  pub bounds: Rect,
  pub dpi: u32,
  pub scale_factor: f32,
}

impl NativeMonitorProperties {
  pub fn try_from(native_display: &Display) -> anyhow::Result<Self> {
    let display_device = native_display.main_device()?;

    // Read in one go, which on macOS is a single hop to the main thread.
    let properties = native_display.properties()?;

    Ok(Self {
      #[cfg(target_os = "macos")]
      device_uuid: display_device.id().0,
      #[cfg(target_os = "windows")]
      handle: native_display.hmonitor().0,
      #[cfg(target_os = "windows")]
      hardware_id: display_device.hardware_id(),
      #[cfg(target_os = "windows")]
      device_path: display_device.device_path(),
      #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
      refresh_rate: display_device
        .refresh_rate()
        .ok()
        .map(|rate| rate as u32),
      device_name: properties.name,
      working_area: properties.working_area,
      bounds: properties.bounds,
      dpi: properties.dpi,
      scale_factor: properties.scale_factor,
    })
  }
}
