//! Tree fixtures for container command tests.

use wm_common::{GapsConfig, TilingDirection, WorkspaceConfig};
use wm_platform::{NativeWindow, Rect, RectDelta};

#[cfg(target_os = "windows")]
use crate::models::AlphaState;
use crate::{
  models::{Container, NativeWindowProperties, TilingWindow, Workspace},
  traits::{CommonGetters, TilingSizeGetters},
};

/// An empty horizontal workspace.
pub fn workspace() -> Workspace {
  Workspace::new(
    WorkspaceConfig {
      name: "1".to_string(),
      display_name: None,
      bind_to_monitor: None,
      keep_alive: false,
    },
    GapsConfig::default(),
    TilingDirection::Horizontal,
  )
}

/// A detached tiling window.
pub fn window() -> TilingWindow {
  let frame = Rect::from_xy(0, 0, 100, 100);

  let properties = NativeWindowProperties {
    title: String::new(),
    #[cfg(target_os = "windows")]
    class_name: String::new(),
    #[cfg(target_os = "macos")]
    bundle_id: None,
    process_name: String::new(),
    frame: frame.clone(),
    is_minimized: false,
    is_maximized: false,
    is_resizable: true,
    #[cfg(target_os = "windows")]
    shadow_borders: RectDelta::zero(),
    #[cfg(target_os = "windows")]
    alpha_state: AlphaState::default(),
  };

  TilingWindow::new(
    None,
    NativeWindow::mock(),
    properties,
    None,
    RectDelta::zero(),
    frame,
    false,
    GapsConfig::default(),
    Vec::new(),
    None,
  )
}

/// Asserts the tiling sizes of `parent`'s tiling children, in order.
pub fn assert_sizes(parent: &Container, expected: &[f32]) {
  let sizes = parent
    .tiling_children()
    .map(|child| child.tiling_size())
    .collect::<Vec<_>>();

  assert_eq!(sizes.len(), expected.len(), "sizes: {sizes:?}");

  for (size, expected) in sizes.iter().zip(expected) {
    assert!(
      (size - expected).abs() < 0.0001,
      "sizes: {sizes:?}, expected: {expected:?}"
    );
  }
}
