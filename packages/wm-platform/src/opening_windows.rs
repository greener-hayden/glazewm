//! Transfers provisional visibility ownership from `WinEvent` delivery to
//! placement, without revealing the source between the two.
//!
//! Also hosts the shell worker. Everything that waits on explorer
//! (cloaking, taskbar membership, the fullscreen mark) runs on its one
//! thread, for openings and for placement alike, so a window's jobs keep
//! their order and the window manager thread never waits on the shell.

use std::{
  collections::HashMap,
  sync::{
    atomic::{AtomicBool, AtomicIsize, Ordering},
    mpsc, Arc, LazyLock, Mutex, MutexGuard, PoisonError, Weak,
  },
};

use windows::Win32::{
  Foundation::HWND,
  UI::WindowsAndMessaging::{IsWindowVisible, KillTimer, SetTimer},
};

use crate::{
  shell_request::notify_native_wake, ConcealMethod, NativeSession,
  NativeWindow, NativeWindowWindowsExt, ShellTicket, WindowId,
};

static ENABLED: AtomicBool = AtomicBool::new(false);
static NEXT_TOKEN: AtomicIsize = AtomicIsize::new(-1);
static PENDING: LazyLock<Mutex<HashMap<WindowId, Pending>>> =
  LazyLock::new(|| Mutex::new(HashMap::new()));

/// Native work that must not run on the thread that delivers `WinEvent`s.
///
/// Concealment and recovery both reach the shell: cloaking goes through
/// `IApplicationViewCollection`, which is a COM call into another process.
/// Under load that blocks for tens of seconds, and the event thread also
/// carries the window hooks and every `dispatch_sync` target, so blocking
/// it stops window management outright rather than merely delaying one
/// window's reveal.
enum Job {
  Conceal(WindowId, NativeSession),
  Restore(NativeSession),
  /// A request from placement, answered through its ticket.
  Shell {
    session: NativeSession,
    call: ShellCall,
    ticket: ShellTicket,
  },
  /// A destroyed window, whose cached shell view is now stale.
  Forget(WindowId),
}

/// A call into explorer that placement queues.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShellCall {
  /// Cloaks (`true`) or uncloaks the window.
  Cloak(bool),
  /// Adds (`true`) or removes the window's taskbar tab.
  TaskbarTab(bool),
  /// Marks or unmarks the window as fullscreen for the taskbar.
  MarkFullscreen(bool),
}

impl ShellCall {
  /// Makes the call. Worker thread only.
  fn run(self, session: &NativeSession) -> crate::Result<()> {
    match self {
      Self::Cloak(hidden) => session.execute_cloak(hidden),
      Self::TaskbarTab(visible) => {
        session.window()?.set_taskbar_visibility(visible)
      }
      Self::MarkFullscreen(fullscreen) => {
        session.window()?.mark_fullscreen(fullscreen)
      }
    }
  }
}

/// Serializes native work onto one thread that is allowed to block.
///
/// A single worker rather than a pool: jobs for one window must keep their
/// order, or a restore can overtake the concealment it is undoing and
/// strand the window invisible.
///
/// It blocks only on explorer. Anything that waits on a window's own
/// application (the layered bit) goes to that process's lane in
/// `style_worker` instead, so a hung application cannot hold up every
/// window's cloak behind it.
///
/// Cloaking is the one thing that must stay here, for its ordering; the
/// layered bit, which is not, is reached through hooks (see
/// `NativeSession::conceal_queued` and `NativeSession::release_queued`).
static WORKER: LazyLock<mpsc::Sender<Job>> = LazyLock::new(|| {
  let (tx, rx) = mpsc::channel::<Job>();
  if let Err(err) = std::thread::Builder::new()
    .name("glazewm-shell".into())
    .spawn(move || {
      for job in rx {
        match job {
          Job::Conceal(id, session) => {
            if let Err(err) = conceal_session(id, &session) {
              // The reservation promised concealment this could not
              // deliver, so drop it rather than leave a window claimed
              // and visible.
              tracing::debug!(window = ?id, "Opening concealment failed: {err}");
              drop_reservation(id);
            } else {
              tracing::debug!(window = ?id, "Opening source concealment requested.");
            }
          }
          Job::Restore(session) => release_session(&session),
          Job::Shell {
            session,
            call,
            ticket,
          } => {
            // Withdrawn while queued, because a newer request replaced it.
            if !ticket.claim() {
              continue;
            }
            let result = call.run(&session);
            if let Err(err) = &result {
              tracing::debug!(?call, "Shell call failed: {err}");
            }
            ticket.complete(result.is_ok());
            notify_native_wake();
          }
          Job::Forget(id) => crate::platform_impl::forget_cloak_view(id.0),
        }
      }
    })
  {
    tracing::error!("Opening concealment worker failed to start: {err}");
  }
  tx
});

/// Queues native work, dropping it only if the worker is gone.
fn submit(job: Job) {
  if WORKER.send(job).is_err() {
    tracing::error!("Opening concealment worker is not running.");
  }
}

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

/// A destroyed HWND has no remaining native state to restore, and its
/// cached shell view and queued style change are dead.
pub(crate) fn forget_window(id: WindowId) {
  registry().remove(&id);
  crate::style_worker::STYLES.forget(id.0);
  submit(Job::Forget(id));
}

/// Queues a call into explorer and returns at once.
///
/// The ticket completes when the worker has made the call. A newer
/// request may withdraw the call while it is still queued.
pub(crate) fn submit_shell(
  session: &NativeSession,
  call: ShellCall,
  ticket: &ShellTicket,
) -> crate::Result<()> {
  WORKER
    .send(Job::Shell {
      session: session.clone(),
      call,
      ticket: ticket.clone(),
    })
    .map_err(|_| {
      crate::Error::Platform("The shell worker is not running.".into())
    })
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

/// Reserves a window and queues its concealment.
///
/// Everything here is cheap and local: property reads and writes on a
/// window handle, a timer registration, and a channel send. The shell call
/// that actually hides the source is handed to [`WORKER`], because this
/// runs inside `WinEvent` delivery and must return promptly.
///
/// Reserving before the concealment lands is deliberate. The reservation
/// is what stops placement revealing the source in the meantime, so it has
/// to exist from the moment the window is claimed; the worker withdraws it
/// if the concealment then fails.
fn acquire(window: NativeWindow) -> Option<()> {
  let id = window.id();
  // CREATE can precede SHOW. Keep the original surface capabilities and
  // refresh concealment if the application changed its alpha meanwhile.
  let existing = registry().get(&id).map(|p| p.session.clone());
  if let Some(session) = existing {
    submit(Job::Conceal(id, session));
    return Some(());
  }
  // Also what keeps the constructor below off the shell: an owned window
  // is the one case where `NativeSession::new` recovers a stale claim, and
  // recovery cloaks. Returning here leaves it only property and class
  // reads. The reservation checked above is likewise the only case that
  // would make it release an inherited opening.
  if NativeSession::is_owned(&window) {
    return None;
  }
  let token = NEXT_TOKEN.fetch_sub(1, Ordering::Relaxed);
  let session = NativeSession::new(window, token).ok()?;
  // A missed SHOW or stalled consumer must not strand a hidden window.
  // SAFETY: The listener thread dispatches timer callbacks.
  let timer = unsafe { SetTimer(None, 0, 750, Some(expire)) };
  if timer == 0 {
    submit(Job::Restore(session));
    return None;
  }
  registry().insert(
    id,
    Pending {
      session: session.clone(),
      token,
      timer,
      notification: Weak::new(),
    },
  );
  submit(Job::Conceal(id, session));
  Some(())
}

/// Hides an opening source by whichever method its surface allows.
///
/// Parking never applies here; this runs inside `WinEvent` delivery, so
/// the unsupported case reports an error rather than unwinding into the
/// shell.
///
/// An alpha conceal is queued on the application's own lane, so success
/// here means it was accepted. If it then fails, the lane's hook drops
/// the reservation, as a failure here would.
fn conceal_session(
  id: WindowId,
  session: &NativeSession,
) -> crate::Result<()> {
  // Border renderers must also withhold the app's original-position ring.
  session.present(true)?;
  match session.conceal_method() {
    ConcealMethod::Alpha => session.conceal_queued(Box::new(move |ok: bool| {
      if !ok {
        tracing::debug!(window = ?id, "Opening alpha concealment failed.");
        drop_reservation(id);
      }
    })),
    ConcealMethod::Cloak => session.cloak(true),
    ConcealMethod::Park => Err(crate::Error::Platform(
      "Windows never parks openings.".into(),
    )),
  }
}

/// Queues a reservation's recovery. Never performs it inline.
///
/// Callers include a `Drop` impl and a shell timer callback, both of which
/// can run on the event thread, and recovery reaches the shell the same
/// way concealment does.
fn restore(pending: &Pending) {
  submit(Job::Restore(pending.session.clone()));
}

/// Drops a reservation whose concealment failed, and queues its
/// recovery. Never blocks, so it can run on a style lane's thread.
fn drop_reservation(id: WindowId) {
  let pending = registry().remove(&id);
  if let Some(pending) = pending {
    restore(&pending);
  }
}

/// Reveals a source the WM no longer owns. Worker thread only.
///
/// Removing the layered bit is queued on the application's lane, and the
/// recovery record stays until it lands. The lane then queues this again
/// to finish, so a hung application delays only its own recovery.
fn release_session(session: &NativeSession) {
  if session.validate().is_ok() {
    let retry = session.clone();
    let done = Box::new(move |ok: bool| {
      if ok {
        submit(Job::Restore(retry));
      } else {
        tracing::warn!(
          "Opening source layered removal failed; its record is kept."
        );
      }
    });
    if let Err(err) = session.release_queued(done) {
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
