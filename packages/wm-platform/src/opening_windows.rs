//! Transfers provisional visibility ownership from `WinEvent` delivery to
//! placement, without revealing the source between the two.

use std::{
  collections::HashMap,
  sync::{
    atomic::{AtomicBool, AtomicIsize, Ordering},
    Arc, LazyLock, Mutex, MutexGuard, PoisonError, Weak,
  },
};

use windows::Win32::{
  Foundation::HWND,
  UI::WindowsAndMessaging::{IsWindowVisible, KillTimer, SetTimer},
};

use crate::{
  ConcealMethod, NativeSession, NativeWindow, NativeWindowWindowsExt,
  OpacityValue, WindowId,
};

static ENABLED: AtomicBool = AtomicBool::new(false);
static NEXT_TOKEN: AtomicIsize = AtomicIsize::new(-1);
static PENDING: LazyLock<Mutex<HashMap<WindowId, Pending>>> =
  LazyLock::new(|| Mutex::new(HashMap::new()));

struct Pending {
  session: NativeSession,
  token: isize,
  timer: usize,
  notification: Weak<OpeningGuard>,
}

/// Locks the registry, tolerating a poisoned mutex.
///
/// Callers include a `Drop` impl and a timer callback the shell invokes
/// across an FFI boundary, where unwinding is undefined behavior. The
/// guarded map stays usable after a panic, so recovery beats a cascade
/// of failed locks that would strand concealed windows.
fn registry() -> MutexGuard<'static, HashMap<WindowId, Pending>> {
  PENDING.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Last notification owner releases a rejected or unprocessed opening.
#[derive(Debug)]
pub(crate) struct OpeningGuard {
  id: WindowId,
  token: isize,
}

impl Drop for OpeningGuard {
  fn drop(&mut self) {
    let pending = {
      let mut windows = registry();
      if windows.get(&self.id).is_some_and(|p| {
        p.token == self.token && p.notification.strong_count() == 0
      }) {
        windows.remove(&self.id)
      } else {
        None
      }
    };
    if let Some(pending) = pending {
      restore(&pending);
    }
  }
}

pub(crate) fn enabled() -> bool {
  ENABLED.load(Ordering::Acquire)
}

pub(crate) fn set_enabled(enabled: bool) {
  if ENABLED.swap(enabled, Ordering::AcqRel) && !enabled {
    let windows = std::mem::take(&mut *registry());
    for pending in windows.into_values() {
      restore(&pending);
    }
  }
}

/// Whether the app has shown a window provisionally concealed by us.
pub(crate) fn is_held(window: &NativeWindow) -> bool {
  // SAFETY: Queries visibility without changing the application's state.
  unsafe { IsWindowVisible(window.hwnd()) }.as_bool()
    && registry()
      .get(&window.id())
      .is_some_and(|p| p.session.validate().is_ok())
}

/// Claims a pending source before normal session recovery can reveal it.
pub(crate) fn take(window: &NativeWindow) -> Option<NativeSession> {
  registry().remove(&window.id()).map(|p| p.session)
}

/// A destroyed HWND has no remaining native state to restore.
pub(crate) fn forget_window(id: WindowId) {
  registry().remove(&id);
}

/// Retains a CREATE reservation even if the app changed styles at SHOW.
pub(crate) fn guard(id: WindowId) -> Option<Arc<OpeningGuard>> {
  let mut windows = registry();
  let pending = windows.get_mut(&id)?;
  if let Some(guard) = pending.notification.upgrade() {
    return Some(guard);
  }
  let guard = Arc::new(OpeningGuard {
    id,
    token: pending.token,
  });
  pending.notification = Arc::downgrade(&guard);
  Some(guard)
}

/// Acquires concealment on the event thread. Never holds the registry lock
/// across native mutations, which can reenter `WinEvent` delivery.
pub(crate) fn conceal(window: NativeWindow) -> Option<Arc<OpeningGuard>> {
  let id = window.id();
  acquire(window)?;
  guard(id)
}

/// Keeps a CREATE reservation until SHOW, its timer, or listener shutdown.
pub(crate) fn reserve(window: NativeWindow) {
  let _ = acquire(window);
}

fn acquire(window: NativeWindow) -> Option<()> {
  let id = window.id();
  // CREATE can precede SHOW. Keep the original surface capabilities and
  // refresh concealment if the application changed its alpha meanwhile.
  let existing = registry().get(&id).map(|p| p.session.clone());
  if let Some(session) = existing {
    return conceal_session(&session).ok();
  }
  if NativeSession::is_owned(&window) {
    return None;
  }
  let token = NEXT_TOKEN.fetch_sub(1, Ordering::Relaxed);
  let session = NativeSession::new(window, token).ok()?;
  // A missed SHOW or stalled consumer must not strand a hidden window.
  // SAFETY: The listener thread dispatches timer callbacks.
  let timer = unsafe { SetTimer(None, 0, 750, Some(expire)) };
  if timer == 0 || conceal_session(&session).is_err() {
    let _ = session.release();
    if timer != 0 {
      // SAFETY: This timer belongs to the calling thread.
      let _ = unsafe { KillTimer(None, timer) };
    }
    return None;
  }
  registry().insert(
    id,
    Pending {
      session,
      token,
      timer,
      notification: Weak::new(),
    },
  );
  tracing::debug!(window = ?id, "Opening source concealed at WinEvent delivery.");
  Some(())
}

/// Hides an opening source by whichever method its surface allows.
///
/// Parking never applies here; this runs inside `WinEvent` delivery, so
/// the unsupported case reports an error rather than unwinding into the
/// shell.
fn conceal_session(session: &NativeSession) -> crate::Result<()> {
  // Border renderers must also withhold the app's original-position ring.
  session.present(true)?;
  match session.conceal_method() {
    ConcealMethod::Alpha => session.opacity(Some(OpacityValue(0.0))),
    ConcealMethod::Cloak => session.cloak(true),
    ConcealMethod::Park => Err(crate::Error::Platform(
      "Windows never parks openings.".into(),
    )),
  }
}

fn restore(pending: &Pending) {
  if pending.session.validate().is_ok() {
    if let Err(err) = pending.session.release() {
      tracing::warn!("Opening source recovery failed: {err}");
    }
  }
}

unsafe extern "system" fn expire(_: HWND, _: u32, timer: usize, _: u32) {
  // SAFETY: Timer callbacks run on the thread that registered the timer.
  let _ = unsafe { KillTimer(None, timer) };
  let pending = {
    let mut windows = registry();
    let id = windows
      .iter()
      .find_map(|(id, p)| (p.timer == timer).then_some(*id));
    id.and_then(|id| windows.remove(&id))
  };
  if let Some(pending) = pending {
    restore(&pending);
  }
}
