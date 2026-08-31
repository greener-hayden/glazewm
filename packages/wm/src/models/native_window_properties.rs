use std::time::Instant;

#[cfg(target_os = "macos")]
use wm_platform::NativeWindowExtMacOs;
use wm_platform::{NativeWindow, Rect};
#[cfg(target_os = "windows")]
use wm_platform::{NativeWindowWindowsExt, RectDelta};

#[derive(Debug, Clone)]
pub struct NativeWindowProperties {
  pub title: String,

  /// When `title` was last read from the platform.
  ///
  /// Reading a title is a blocking call into the owning app, and a
  /// terminal rewrites its title several times a second while busy, so
  /// the reads land exactly when that app is slowest to answer.
  pub title_read_at: Instant,
  #[cfg(target_os = "windows")]
  pub class_name: String,
  #[cfg(target_os = "macos")]
  pub bundle_id: Option<String>,
  pub process_name: String,
  pub frame: Rect,
  pub is_minimized: bool,
  pub is_maximized: bool,
  pub is_resizable: bool,
  #[cfg(target_os = "windows")]
  pub shadow_borders: RectDelta,
}

impl TryFrom<&NativeWindow> for NativeWindowProperties {
  type Error = anyhow::Error;

  fn try_from(native_window: &NativeWindow) -> Result<Self, Self::Error> {
    Ok(Self {
      title: native_window.title()?,
      title_read_at: Instant::now(),
      #[cfg(target_os = "windows")]
      class_name: native_window.class_name()?,
      #[cfg(target_os = "macos")]
      bundle_id: native_window.bundle_id(),
      process_name: native_window.process_name()?,
      frame: native_window.frame()?,
      is_minimized: native_window.is_minimized()?,
      is_maximized: native_window.is_maximized()?,
      is_resizable: native_window.is_resizable()?,
      #[cfg(target_os = "windows")]
      shadow_borders: native_window.shadow_borders()?,
    })
  }
}
