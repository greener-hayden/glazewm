//! Tree fixtures for container command tests.

use wm_common::{GapsConfig, TilingDirection, WorkspaceConfig};
use wm_platform::{NativeWindow, Rect, RectDelta};

use super::attach_container;
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

  // The builder, not a literal: it carries the platform-specific fields
  // itself, so a new one does not break this fixture on the platform it
  // was not written on.
  let properties =
    NativeWindowProperties::mock().frame(frame.clone()).call();

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

/// A workspace holding `sizes.len()` windows with those tiling sizes.
pub fn row(sizes: &[f32]) -> (Container, Vec<Container>) {
  let workspace: Container = workspace().into();
  let windows = sizes
    .iter()
    .map(|_| {
      let window: Container = window().into();
      attach_container(&window, &workspace, None).unwrap();
      window
    })
    .collect::<Vec<_>>();

  // Sized after every attach, since each attach re-divides the row.
  for (window, &size) in windows.iter().zip(sizes) {
    window.as_tiling_container().unwrap().set_tiling_size(size);
  }

  (workspace, windows)
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
