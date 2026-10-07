#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::missing_errors_doc)]
#![feature(iterator_try_collect)]

mod animation_window;
// LINT: Each platform uses part of the companion math; all of it
// compiles everywhere so it stays testable.
#[allow(dead_code)]
mod companion;
mod dispatcher;
mod display;
mod display_listener;
mod error;
mod event_loop;
#[cfg(target_os = "windows")]
mod floating_stacking;
mod frame_clock;
#[cfg(target_os = "windows")]
pub use floating_stacking::*;
#[cfg(all(test, target_os = "windows"))]
mod input_test_allocator;
mod keybinding_listener;
mod models;
mod mouse_listener;
mod native_call_stats;
mod native_window;
#[cfg(target_os = "windows")]
mod opening_windows;
mod placement_session;
mod platform_event;
mod platform_impl;
mod qos;
mod single_instance;
mod thread_bound;
mod thumbnail_layout;
pub use thumbnail_layout::thumbnail_rects;
mod window_listener;
mod window_liveness;
#[cfg(target_os = "windows")]
mod windows_session;
#[cfg(target_os = "windows")]
pub use windows_session::{
  alpha_is_safe, composition_frame, opacity_action, recover_owned_windows,
  NativeSession, OpacityAction,
};

#[cfg(feature = "test_utils")]
pub mod test_utils;

pub use animation_window::*;
pub use companion::COMPANION_MARGIN_PX;
pub use dispatcher::*;
pub use display::*;
pub use display_listener::*;
pub use error::*;
pub use event_loop::*;
pub use frame_clock::*;
pub use keybinding_listener::*;
pub use models::*;
pub use mouse_listener::*;
pub use native_call_stats::*;
pub use native_window::*;
pub use placement_session::*;
pub use platform_event::*;
pub use qos::*;
pub use single_instance::*;
pub use thread_bound::*;
pub use window_listener::*;
pub use window_liveness::*;
// TODO: Avoid exposing `windows` crate types in the public API.
#[cfg(target_os = "windows")]
pub use windows::Win32::UI::WindowsAndMessaging::{
  SET_WINDOW_POS_FLAGS, SWP_ASYNCWINDOWPOS, SWP_FRAMECHANGED,
  SWP_NOACTIVATE, SWP_NOCOPYBITS, SWP_NOSENDCHANGING, WINDOW_EX_STYLE,
  WINDOW_STYLE, WS_CAPTION, WS_CHILD, WS_EX_LAYERED, WS_EX_NOACTIVATE,
  WS_EX_TOOLWINDOW, WS_MAXIMIZEBOX,
};
