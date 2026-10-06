use std::collections::HashSet;

use crate::{platform_impl, NativeWindow, WindowId};

/// Tells which of many windows are still alive.
///
/// [`NativeWindow::is_valid`] asks the window itself, which on macOS is a
/// request to the owning application and can take as long as that
/// application is busy. A `WindowLiveness` made with
/// [`WindowLiveness::from_window_server`] first takes one listing from the
/// window server, which answers without the application, and only asks
/// the windows that listing lacks. Make a fresh one for each round of
/// checks: it is a snapshot and does not follow windows that appear or
/// close after it was taken.
///
/// A window the server lists is reported valid without being asked, so a
/// window whose application has dropped it while the server still lists
/// it is not caught by a listing-backed check.
/// [`WindowLiveness::per_window`] catches it.
///
/// # Example usage
///
/// ```no_run
/// # use wm_platform::{NativeWindow, WindowLiveness};
/// # fn example(windows: &[NativeWindow]) {
/// let liveness = WindowLiveness::from_window_server();
/// let alive = windows.iter().filter(|window| liveness.is_valid(window));
/// # }
/// ```
///
/// # Platform-specific
///
/// - **macOS:** [`WindowLiveness::from_window_server`] lists every window
///   the window server has, including minimized, hidden and other-space
///   ones.
/// - **Windows:** there is no listing to take, since `IsWindow` answers
///   without the owning process; every check asks the window.
#[derive(Debug, Default)]
pub struct WindowLiveness {
  /// IDs the window server listed, or `None` to ask every window.
  listed: Option<HashSet<WindowId>>,
}

impl WindowLiveness {
  /// Takes one listing from the window server to check windows against.
  ///
  /// Falls back to asking every window if the listing is unavailable.
  #[must_use]
  pub fn from_window_server() -> Self {
    Self {
      listed: platform_impl::listed_window_ids(),
    }
  }

  /// Asks every window for itself, with no listing taken.
  #[must_use]
  pub fn per_window() -> Self {
    Self::default()
  }

  /// Whether `window` is still alive.
  ///
  /// Asks the window only if the listing does not have it.
  #[must_use]
  pub fn is_valid(&self, window: &NativeWindow) -> bool {
    self.is_valid_with(window.id(), || window.is_valid())
  }

  /// Whether the window `id` is listed, or else `ask` says it is alive.
  fn is_valid_with(
    &self,
    id: WindowId,
    ask: impl FnOnce() -> bool,
  ) -> bool {
    self
      .listed
      .as_ref()
      .is_some_and(|listed| listed.contains(&id))
      || ask()
  }
}

#[cfg(test)]
mod tests {
  use std::cell::Cell;

  use super::*;

  #[cfg(target_os = "macos")]
  fn id(raw: u8) -> WindowId {
    WindowId(u32::from(raw))
  }

  #[cfg(target_os = "windows")]
  fn id(raw: u8) -> WindowId {
    WindowId(isize::from(raw))
  }

  /// A `WindowLiveness` over a listing of the given window IDs.
  fn listing(ids: &[u8]) -> WindowLiveness {
    WindowLiveness {
      listed: Some(ids.iter().copied().map(id).collect()),
    }
  }

  /// Runs the check for `window`, returning whether it is valid and how
  /// many times the window itself was asked.
  fn check(
    liveness: &WindowLiveness,
    window: u8,
    answer: bool,
  ) -> (bool, u32) {
    let asked = Cell::new(0);
    let valid = liveness.is_valid_with(id(window), || {
      asked.set(asked.get() + 1);
      answer
    });

    (valid, asked.get())
  }

  /// A listed window is valid without being asked, however its own
  /// answer would go; this is what spares the application the request.
  #[test]
  fn listed_windows_are_not_asked() {
    let liveness = listing(&[1, 2, 3]);

    assert_eq!(check(&liveness, 2, true), (true, 0));
    assert_eq!(check(&liveness, 2, false), (true, 0));
  }

  /// A window the listing lacks is asked, and removed if it says it is
  /// gone.
  #[test]
  fn unlisted_windows_are_asked() {
    let liveness = listing(&[1, 2, 3]);

    assert_eq!(check(&liveness, 9, true), (true, 1));
    assert_eq!(check(&liveness, 9, false), (false, 1));
  }

  /// A window the application has dropped while the server still lists it
  /// survives a listing-backed check, and only a per-window check finds
  /// it. The periodic full sweep relies on the second half.
  #[test]
  fn per_window_check_finds_a_dead_window_the_server_still_lists() {
    let dead_but_listed = false;

    assert_eq!(check(&listing(&[7]), 7, dead_but_listed), (true, 0));
    assert_eq!(
      check(&WindowLiveness::per_window(), 7, dead_but_listed),
      (false, 1)
    );
  }

  /// With no listing every window is asked, as it would be without a
  /// `WindowLiveness`.
  #[test]
  fn without_a_listing_every_window_is_asked() {
    let liveness = WindowLiveness::per_window();

    assert_eq!(check(&liveness, 1, true), (true, 1));
    assert_eq!(check(&liveness, 1, false), (false, 1));
  }

  /// Taking the listing is one window server call, and a per-window
  /// `WindowLiveness` makes none.
  ///
  /// Reads a process-wide counter, so this holds only under
  /// `--test-threads=1`.
  #[cfg(target_os = "macos")]
  #[test]
  fn from_window_server_lists_once_and_per_window_lists_nothing() {
    use crate::NativeCallStats;

    let before = NativeCallStats::snapshot();
    let _ = WindowLiveness::per_window();
    let spent = NativeCallStats::snapshot().since(&before);
    assert_eq!(spent.window_list_full, 0);

    let before = NativeCallStats::snapshot();
    let _ = WindowLiveness::from_window_server();
    let spent = NativeCallStats::snapshot().since(&before);
    assert_eq!(spent.window_list_full, 1);
    assert_eq!(spent.ax_reads, 0);
    assert_eq!(spent.hops, 0);
  }
}
