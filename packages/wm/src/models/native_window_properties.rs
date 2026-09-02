use std::time::Instant;

#[cfg(target_os = "macos")]
use wm_platform::NativeWindowExtMacOs;
use wm_platform::{NativeWindow, Rect};
#[cfg(target_os = "windows")]
use wm_platform::{NativeWindowWindowsExt, RectDelta};

#[derive(Debug, Clone)]
pub struct NativeWindowProperties {
  pub title: String,

  /// When a read of `title` was last attempted.
  ///
  /// Reading a title is a blocking call into the owning app, and a
  /// terminal rewrites its title several times a second while busy, so
  /// the reads land exactly when that app is slowest to answer. Attempts
  /// rather than successes, because a read that times out is the
  /// expensive one and must still count against the next.
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

  /// Smallest frame the window has been observed to accept.
  ///
  /// Learned, not read: an accessibility resize reports success and is
  /// then clamped inside the owning app, so the only way to know a
  /// window's floor is to have asked for less and been refused. `None`
  /// until that happens, and cleared again the moment the window takes
  /// something smaller — a browser collapsing its sidebar lowers its own
  /// minimum, and a floor kept from before that would waste the space
  /// forever.
  pub min_size: Option<(i32, i32)>,
  #[cfg(target_os = "windows")]
  pub shadow_borders: RectDelta,
  /// What the WM has done to the window's alpha, so it can be undone.
  #[cfg(target_os = "windows")]
  pub alpha_state: AlphaState,
}

/// What the WM has done to a window's alpha.
///
/// Recorded on the first write and cleared on restore, so the WM undoes
/// exactly what it did and nothing the app did itself.
#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AlphaState {
  /// The WM has not written the window's alpha.
  #[default]
  Untouched,
  /// The WM wrote alpha to a window its app had already made layered.
  Written,
  /// The WM made the window layered in order to write alpha.
  Layered,
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
      min_size: None,
      #[cfg(target_os = "windows")]
      shadow_borders: native_window.shadow_borders()?,
      #[cfg(target_os = "windows")]
      alpha_state: AlphaState::default(),
    })
  }
}
