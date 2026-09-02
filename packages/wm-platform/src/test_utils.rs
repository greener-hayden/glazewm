//! Utilities for testing.
//!
//! Available via the `test_utils` Cargo feature.
use std::sync::{atomic::AtomicBool, Arc};

use crate::platform_impl;
#[cfg(target_os = "macos")]
pub use crate::WindowId;
pub use crate::{Dispatcher, Display, NativeWindow};

impl Dispatcher {
  /// Creates a mock `Dispatcher` for use in tests.
  ///
  /// Dispatching through the mock fails rather than runs: it is marked
  /// stopped, so a dispatch returns `EventLoopStopped` instead of
  /// reaching for the event loop source it does not have.
  #[must_use]
  pub fn mock() -> Self {
    Self::new(None, Arc::new(AtomicBool::new(true)))
  }
}

impl NativeWindow {
  /// Creates a mock `NativeWindow` for use in tests.
  ///
  /// Calling any methods on the mock is undefined behavior and may panic.
  #[must_use]
  pub fn mock() -> Self {
    #[cfg(target_os = "windows")]
    {
      platform_impl::NativeWindow::new(0).into()
    }
    #[cfg(target_os = "macos")]
    {
      use std::{
        cell::RefCell,
        sync::{Arc, OnceLock},
      };

      use objc2_app_kit::NSRunningApplication;
      use objc2_application_services::AXUIElement;

      use crate::ThreadBound;

      // Real elements for a pid that owns nothing, rather than zeroed
      // memory. `CFRetained` and `Retained` are non-null, so zeroing them
      // is instant undefined behavior — the mock aborted the moment a
      // test on macOS built one. Creating an element for an invalid pid
      // always succeeds; only operations on it fail, which is all a mock
      // promises anyway.
      let element = || unsafe { AXUIElement::new_application(0) };

      let application = platform_impl::Application {
        pid: 0,
        dispatcher: Dispatcher::mock(),
        ns_app: NSRunningApplication::currentApplication(),
        ax_element: Arc::new(ThreadBound::mock(element())),
        enhanced_ui: Arc::new(OnceLock::new()),
      };

      platform_impl::NativeWindow::new(
        WindowId(0),
        ThreadBound::mock(RefCell::new(element())),
        application,
      )
      .into()
    }
  }
}

impl Display {
  /// Creates a mock `Display` for use in tests.
  ///
  /// Calling any methods on the mock is undefined behavior and may panic.
  #[must_use]
  pub fn mock() -> Self {
    Self {
      #[cfg(target_os = "windows")]
      inner: platform_impl::Display::new(0),
      #[cfg(target_os = "macos")]
      #[allow(invalid_value)]
      inner: unsafe { std::mem::zeroed() },
    }
  }
}
