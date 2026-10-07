use windows::{
  core::{w, PCWSTR},
  Win32::{
    Foundation::{BOOL, HANDLE, HWND, LPARAM, RECT, WPARAM},
    Graphics::{
      Dwm::{DwmGetCompositionTimingInfo, DWM_TIMING_INFO},
      Gdi::{MonitorFromRect, MONITOR_DEFAULTTONEAREST},
    },
    UI::{
      Accessibility::NotifyWinEvent,
      HiDpi::{
        GetAwarenessFromDpiAwarenessContext, GetDpiForWindow,
        GetWindowDpiAwarenessContext, DPI_AWARENESS_PER_MONITOR_AWARE,
      },
      WindowsAndMessaging::{
        EnumWindows, GetClassLongPtrW, GetPropW, GetWindowThreadProcessId,
        RemovePropW, SendMessageTimeoutW, SetPropW, CHILDID_SELF,
        CS_CLASSDC, CS_OWNDC, EVENT_OBJECT_STATECHANGE, GCL_STYLE,
        MINMAXINFO, OBJID_WINDOW, SMTO_ABORTIFHUNG, SMTO_BLOCK,
        WM_GETMINMAXINFO, WS_DLGFRAME, WS_EX_LAYERED,
        WS_EX_NOREDIRECTIONBITMAP,
      },
    },
  },
};

use crate::{
  opening_windows::{submit_shell, ShellCall},
  style_worker::{Change, Completion, STYLES},
  ConcealMethod, NativeWindow, NativeWindowWindowsExt, OpacityProgress,
  OpacityValue, Rect, ShellTicket,
};

const OWNER: PCWSTR = w!("GlazeWM.greener.owner.v1");
/// Recovery flags, also a contract for other processes.
///
/// Border renderers read `PRESENTED` to hide a ring while an overlay
/// stands in for the window, `DECORATED` to hide it while the WM draws
/// the ring itself during native motion, and `FOCUSED` to colour a ring
/// for the WM's focus before the OS foreground follows it. Every change
/// to `PRESENTED`, `DECORATED` or `FOCUSED` is followed by
/// `EVENT_OBJECT_STATECHANGE` on the window (`OBJID_WINDOW`,
/// `CHILDID_SELF`), so readers can react without polling. The property
/// name and bit values are stable.
const CHANGES: PCWSTR = w!("GlazeWM.greener.changes.v1");
/// Companion registration, a contract for other processes.
///
/// A companion is a top-level window of another process that decorates a
/// managed window, such as one band of a border ring. It opts in by
/// setting this property on itself, with the managed window's `HWND` as
/// the value. The WM only reads the property; the companion owns it.
///
/// The WM draws companions itself in two cases, each flagged in
/// `CHANGES`. The companion should hide itself, by cloaking itself or at
/// alpha 0, while either flag is set; its thumbnail keeps rendering
/// either way.
///
/// - Overlay motion (`PRESENTED`): an overlay stands in for the window and
///   draws each companion as a DWM thumbnail above the window's own
///   thumbnail. Each update maps the companion's live window rect through
///   the transform that maps the window's frame to its animated rect: the
///   same translation and scale, clipped to the overlay, at the same
///   opacity. Both thumbnails move in one compositor batch.
/// - Native motion (`DECORATED`): the WM moves the real window, and a
///   transparent overlay directly above it in the z-order draws only the
///   companions. When the motion starts the WM records each companion's
///   window rect and the window's rect. Each frame the WM requests for the
///   window, it redraws every companion from that record with each edge
///   anchored to the nearer parallel window edge, keeping its offset. A
///   band therefore keeps its thickness and stretches with the window. The
///   thumbnails are updated right after the window's own move request, so
///   the decoration follows the WM's requests rather than trailing the
///   move events. A companion that changes shape relative to the window
///   during the motion is drawn with its recorded shape.
///
/// - Companions are found by enumerating top-level windows when the
///   overlay is prepared. During overlay motion, until one is found, the
///   search repeats at most every 16ms, and one found late fades in over
///   80ms. Native motion searches only when it starts; a window without
///   companions gets no overlay and no flag.
/// - A companion that is destroyed, or whose property no longer names the
///   window, is dropped. A failing companion never fails or stalls the
///   window's own motion.
/// - At the reveal, the flag is cleared and the overlay is held until
///   every companion reports `DWMWA_CLOAKED` as 0 or is gone, for at most
///   50ms after the flag cleared. A companion that uncloaks itself when
///   the flag clears is therefore never missing for a frame.
///
/// The property name and value format are stable.
pub(crate) const COMPANION: PCWSTR = w!("GlazeWM.greener.companion.v1");
const FRAME: [PCWSTR; 4] = [
  w!("GlazeWM.greener.left.v1"),
  w!("GlazeWM.greener.top.v1"),
  w!("GlazeWM.greener.right.v1"),
  w!("GlazeWM.greener.bottom.v1"),
];
const ALPHA: isize = 1;
const CLOAK: isize = 2;
const HIDDEN: isize = 4;
const PARKED: isize = 8;
const TITLE: isize = 16;
const ORIGINAL_TITLE: isize = 32;
const PRESENTED: isize = 64;
const FOCUSED: isize = 128;
/// Set while the WM draws the window's companions itself during native
/// motion; the window's own pixels are real. Companions treat it like
/// `PRESENTED`: stay drawn, self-cloaked, and uncloak when it clears.
const DECORATED: isize = 256;

/// Describes safe opacity mutations.
#[derive(Debug, PartialEq)]
pub enum OpacityAction {
  Unchanged,
  Unsupported,
  Set(OpacityValue),
  Restore,
}

/// Preserves application-owned layered rendering.
#[must_use]
pub fn opacity_action(
  supported: bool,
  owned: bool,
  requested: Option<OpacityValue>,
) -> OpacityAction {
  match (supported, owned, requested) {
    (false, _, Some(_)) => OpacityAction::Unsupported,
    (true, _, Some(value)) => OpacityAction::Set(value),
    (true, true, None) => OpacityAction::Restore,
    _ => OpacityAction::Unchanged,
  }
}

/// Whether attribute alpha leaves the application's rendering intact.
///
/// Layered and no-redirection-bitmap windows own their surface, and
/// class-owned DCs skip the redirection path alpha depends on.
#[must_use]
pub fn alpha_is_safe(owns_surface: bool, class_style: usize) -> bool {
  !owns_surface && class_style & (CS_CLASSDC.0 | CS_OWNDC.0) as usize == 0
}

/// Guards mutations against reused window handles.
#[derive(Clone)]
pub struct NativeSession {
  window: NativeWindow,
  token: isize,
  process: u32,
  thread: u32,
  supports_alpha: bool,
  opening_concealed: bool,
}

impl NativeSession {
  /// Claims a window's recovery record.
  pub fn new(window: NativeWindow, token: isize) -> crate::Result<Self> {
    if let Some(mut opening) = crate::opening_windows::take(&window) {
      if let Err(err) = opening.validate().and_then(|()| {
        if token == 0 {
          return Err(crate::Error::Platform(
            "Invalid window identity.".into(),
          ));
        }
        put_property(&window, OWNER, token)
      }) {
        let _ = opening.release();
        return Err(err);
      }
      opening.token = token;
      opening.opening_concealed = true;
      return Ok(opening);
    }
    if property(&window, OWNER) != 0 {
      recover_window(&window, None)?;
    }
    let mut process = 0;
    // SAFETY: Process output lives through call.
    let thread = unsafe {
      GetWindowThreadProcessId(window.hwnd(), Some(&raw mut process))
    };
    if thread == 0 || token == 0 {
      return Err(crate::Error::Platform(
        "Invalid window identity.".into(),
      ));
    }
    // SAFETY: Reads the live window class.
    let class_style =
      unsafe { GetClassLongPtrW(window.hwnd(), GCL_STYLE) };
    let supports_alpha = alpha_is_safe(
      window
        .has_window_style_ex(WS_EX_LAYERED | WS_EX_NOREDIRECTIONBITMAP),
      class_style,
    );
    put_property(&window, OWNER, token)?;
    Ok(Self {
      window,
      token,
      process,
      thread,
      supports_alpha,
      opening_concealed: false,
    })
  }

  /// Checks ownership before the listener attempts a provisional claim.
  pub(crate) fn is_owned(window: &NativeWindow) -> bool {
    property(window, OWNER) != 0
  }

  /// Whether this session inherited concealment from the event listener.
  #[must_use]
  pub fn opening_concealed(&self) -> bool {
    self.opening_concealed
  }

  /// Checks ownership before each native mutation.
  pub fn validate(&self) -> crate::Result<()> {
    let mut process = 0;
    // SAFETY: Process output lives through call.
    let thread = unsafe {
      GetWindowThreadProcessId(self.window.hwnd(), Some(&raw mut process))
    };
    if thread != self.thread
      || process != self.process
      || property(&self.window, OWNER) != self.token
    {
      return Err(crate::Error::Platform(
        "Window ownership expired.".into(),
      ));
    }
    Ok(())
  }

  /// Checks that a live source has no pending concealment or presentation.
  pub fn stacking_ready(&self) -> crate::Result<bool> {
    self.validate()?;
    Ok(
      property(&self.window, CHANGES)
        & (PRESENTED | HIDDEN | CLOAK | PARKED | DECORATED)
        == 0,
    )
  }

  /// Returns the guarded native window.
  pub fn window(&self) -> crate::Result<&NativeWindow> {
    self.validate()?;
    Ok(&self.window)
  }

  /// Reports safe attribute-based alpha support.
  #[must_use]
  pub fn supports_alpha(&self) -> bool {
    self.supports_alpha
      && !self.window.has_window_style_ex(WS_EX_NOREDIRECTIONBITMAP)
      && (!self.window.has_window_style_ex(WS_EX_LAYERED)
        || property(&self.window, CHANGES) & ALPHA != 0)
  }

  /// Chooses how an overlay conceals this source.
  #[must_use]
  pub fn conceal_method(&self) -> ConcealMethod {
    if self.supports_alpha() {
      ConcealMethod::Alpha
    } else {
      ConcealMethod::Cloak
    }
  }

  /// Reads the application's current DPI.
  pub fn dpi(&self) -> crate::Result<u32> {
    self.validate()?;
    // SAFETY: Validated handle supplies window DPI.
    Ok(unsafe { GetDpiForWindow(self.window.hwnd()) })
  }

  /// Resolves DPI without penalizing legacy applications.
  pub fn expected_dpi(&self, monitor_dpi: u32) -> crate::Result<u32> {
    self.validate()?;
    // SAFETY: Validated window supplies awareness context.
    let awareness = unsafe {
      GetAwarenessFromDpiAwarenessContext(GetWindowDpiAwarenessContext(
        self.window.hwnd(),
      ))
    };
    if awareness == DPI_AWARENESS_PER_MONITOR_AWARE {
      Ok(monitor_dpi)
    } else {
      self.dpi()
    }
  }

  /// Applies only WM-owned opacity changes, waiting on the application.
  ///
  /// Adding or removing the layered bit blocks until the application's
  /// thread answers, for as long as it takes. Only a thread that may block
  /// should call this; the window manager thread calls
  /// [`Self::request_opacity`].
  pub fn opacity(&self, value: Option<OpacityValue>) -> crate::Result<()> {
    self.validate()?;
    let changes = property(&self.window, CHANGES);
    match opacity_action(
      self.supports_alpha(),
      changes & ALPHA != 0,
      value,
    ) {
      OpacityAction::Unchanged => {}
      OpacityAction::Unsupported => {
        return Err(crate::Error::Platform(
          "Application owns window transparency.".into(),
        ))
      }
      OpacityAction::Set(value) => {
        put_property(&self.window, CHANGES, changes | ALPHA)?;
        self.window.set_transparency(&value)?;
      }
      OpacityAction::Restore => {
        self.window.remove_window_style_ex(WS_EX_LAYERED);
        if self.window.has_window_style_ex(WS_EX_LAYERED) {
          return Err(crate::Error::Platform(
            "Opacity restoration failed.".into(),
          ));
        }
        put_property(&self.window, CHANGES, changes & !ALPHA)?;
      }
    }
    Ok(())
  }

  /// Requests an opacity without waiting on the application.
  ///
  /// Adding or removing `WS_EX_LAYERED` makes the application's own thread
  /// answer a style message, so those changes run on the target process's
  /// style lane (see `style_worker`). Setting attribute alpha on a window
  /// that already has the bit is done here, inline, and removing the bit
  /// first sets alpha to opaque: a reveal reaches the screen at once and
  /// the removal follows.
  ///
  /// Idempotent. Call it each pass until it reports
  /// [`OpacityProgress::Settled`]; a later request for another opacity
  /// supersedes one still queued. `Settled` means the window shows the
  /// requested opacity and, for a restore, that the bit is gone and its
  /// recovery record cleared.
  pub fn request_opacity(
    &self,
    value: Option<OpacityValue>,
  ) -> crate::Result<OpacityProgress> {
    self.validate()?;
    let changes = property(&self.window, CHANGES);
    let layered = self.window.has_window_style_ex(WS_EX_LAYERED);
    match opacity_action(
      self.supports_alpha(),
      changes & ALPHA != 0,
      value,
    ) {
      OpacityAction::Unchanged => Ok(OpacityProgress::Settled),
      OpacityAction::Unsupported => Err(crate::Error::Platform(
        "Application owns window transparency.".into(),
      )),
      OpacityAction::Set(value) => {
        let alpha = value.to_alpha();
        if changes & ALPHA == 0 {
          put_property(&self.window, CHANGES, changes | ALPHA)?;
        }
        self.queue_layer(true, alpha, layered)?;
        let applied = layered && self.apply_alpha(alpha);
        Ok(if applied && !self.layer_busy() {
          OpacityProgress::Settled
        } else {
          OpacityProgress::Pending
        })
      }
      OpacityAction::Restore => {
        if layered {
          self.apply_alpha(u8::MAX);
        }
        self.queue_layer(false, u8::MAX, layered)?;
        if layered || self.layer_busy() {
          return Ok(OpacityProgress::Pending);
        }
        put_property(&self.window, CHANGES, changes & !ALPHA)?;
        Ok(OpacityProgress::Settled)
      }
    }
  }

  /// Queues a change of the window's `WS_EX_LAYERED` bit.
  fn queue_layer(
    &self,
    on: bool,
    alpha: u8,
    observed_on: bool,
  ) -> crate::Result<()> {
    self
      .queue_layer_with(on, alpha, observed_on, None)
      .map(|_| ())
  }

  /// Queues a change of the `WS_EX_LAYERED` bit, with a hook that runs
  /// when it ends.
  ///
  /// Returns whether work is outstanding, which is whether `done` will
  /// run: a window that already is as asked has nothing to wait for.
  fn queue_layer_with(
    &self,
    on: bool,
    alpha: u8,
    observed_on: bool,
    done: Option<Completion>,
  ) -> crate::Result<bool> {
    STYLES.request(
      self.process,
      self.window.hwnd().0,
      self,
      Change {
        on,
        alpha,
        observed_on,
      },
      done,
    )
  }

  /// Conceals an opening window by attribute alpha without waiting on its
  /// application.
  ///
  /// For the shell worker. Adding the layered bit is queued on the target
  /// process's own lane, so a hung application cannot hold up the shell
  /// thread, and `done` runs when that ends, with whether it worked. It
  /// does not run when nothing needed queueing. Setting alpha on a window
  /// that already has the bit is inline.
  pub(crate) fn conceal_queued(
    &self,
    done: Completion,
  ) -> crate::Result<()> {
    self.validate()?;
    let changes = property(&self.window, CHANGES);
    let layered = self.window.has_window_style_ex(WS_EX_LAYERED);
    match opacity_action(
      self.supports_alpha(),
      changes & ALPHA != 0,
      Some(OpacityValue(0.0)),
    ) {
      OpacityAction::Set(value) => {
        if changes & ALPHA == 0 {
          put_property(&self.window, CHANGES, changes | ALPHA)?;
        }
        let alpha = value.to_alpha();
        if layered {
          self.apply_alpha(alpha);
        }
        self.queue_layer_with(true, alpha, layered, Some(done))?;
        Ok(())
      }
      OpacityAction::Unsupported => Err(crate::Error::Platform(
        "Application owns window transparency.".into(),
      )),
      OpacityAction::Unchanged | OpacityAction::Restore => Ok(()),
    }
  }

  /// Whether a style change is queued or running for this window.
  fn layer_busy(&self) -> bool {
    STYLES.busy(self.process, self.window.hwnd().0)
  }

  /// Sets attribute alpha on a window that has the layered bit.
  ///
  /// Returns whether the window shows the alpha afterwards. A window whose
  /// bit a queued removal took away meanwhile rejects the call, and that
  /// is not an error: the removal is what the caller asked for next.
  fn apply_alpha(&self, alpha: u8) -> bool {
    let native = &self.window.inner;
    if native.layered_alpha() == Some(alpha) {
      return true;
    }
    match native.set_layered_alpha(alpha) {
      Ok(()) => true,
      Err(err) => {
        tracing::debug!("Inline alpha was rejected: {err}");
        false
      }
    }
  }

  /// Adds or removes `WS_EX_LAYERED` on the style lane's thread.
  ///
  /// Raw mutation only: recovery records belong to whoever asked for the
  /// change. Ownership is checked again once the add has landed. The call
  /// can take as long as the application likes, and `release` may have
  /// run meanwhile, so an add that outlived its ownership is undone
  /// rather than left on a window nobody will restore.
  pub(crate) fn execute_layered(
    &self,
    on: bool,
    alpha: &dyn Fn() -> u8,
  ) -> crate::Result<()> {
    self.validate()?;
    if on {
      self.window.add_window_style_ex(WS_EX_LAYERED);
      if !self.window.has_window_style_ex(WS_EX_LAYERED) {
        return Err(crate::Error::Platform(
          "Opacity application failed.".into(),
        ));
      }
      // A layered window draws nothing until it has attributes, so this
      // follows the add at once, with the alpha wanted now.
      let applied = self.window.inner.set_layered_alpha(alpha());
      if self.validate().is_err() {
        self.window.remove_window_style_ex(WS_EX_LAYERED);
        return Err(crate::Error::Platform(
          "Window ownership expired.".into(),
        ));
      }
      applied
    } else {
      self.window.remove_window_style_ex(WS_EX_LAYERED);
      if self.window.has_window_style_ex(WS_EX_LAYERED) {
        return Err(crate::Error::Platform(
          "Opacity restoration failed.".into(),
        ));
      }
      Ok(())
    }
  }

  /// Preserves application-owned title-bar state.
  pub fn title_bar(&self, visible: Option<bool>) -> crate::Result<()> {
    self.validate()?;
    let mut changes = property(&self.window, CHANGES);
    if let Some(visible) = visible {
      if changes & TITLE == 0 {
        if self.window.has_window_style(WS_DLGFRAME) {
          changes |= ORIGINAL_TITLE;
        }
        changes |= TITLE;
        put_property(&self.window, CHANGES, changes)?;
      }
      self.window.set_title_bar_visibility(visible)?;
    } else if changes & TITLE != 0 {
      self
        .window
        .set_title_bar_visibility(changes & ORIGINAL_TITLE != 0)?;
      put_property(
        &self.window,
        CHANGES,
        changes & !(TITLE | ORIGINAL_TITLE),
      )?;
    }
    Ok(())
  }

  /// Applies recoverable workspace cloaking.
  ///
  /// A failed cloak leaves no recovery flag behind, so a later uncloak
  /// stays a no-op instead of failing the same way again.
  pub fn cloak(&self, hidden: bool) -> crate::Result<()> {
    self.validate()?;
    let changes = property(&self.window, CHANGES);
    if !hidden && changes & CLOAK == 0 {
      return Ok(());
    }
    if hidden {
      put_property(&self.window, CHANGES, changes | CLOAK)?;
    }
    if let Err(err) = self.window.set_cloaked(hidden) {
      if hidden {
        put_property(&self.window, CHANGES, changes & !CLOAK)?;
      }
      return Err(err);
    }
    if !hidden {
      put_property(&self.window, CHANGES, changes & !CLOAK)?;
    }
    Ok(())
  }

  /// Queues recoverable workspace cloaking without waiting on the shell.
  ///
  /// Returns the ticket of the queued call, or `None` when there is
  /// nothing to do because the window was not cloaked by us. The recovery
  /// record is written here, before the call can run, and the call itself
  /// only makes the shell request; the caller reports the outcome to
  /// [`Self::finish_cloak`].
  pub fn request_cloak(
    &self,
    hidden: bool,
  ) -> crate::Result<Option<ShellTicket>> {
    self.validate()?;
    let changes = property(&self.window, CHANGES);
    if !hidden && changes & CLOAK == 0 {
      return Ok(None);
    }
    if hidden && changes & CLOAK == 0 {
      put_property(&self.window, CHANGES, changes | CLOAK)?;
    }
    let ticket = ShellTicket::new();
    if let Err(err) = submit_shell(self, ShellCall::Cloak(hidden), &ticket)
    {
      if hidden {
        put_property(&self.window, CHANGES, changes & !CLOAK)?;
      }
      return Err(err);
    }
    Ok(Some(ticket))
  }

  /// Settles the recovery record of a cloak request that has ended.
  ///
  /// A cloak that failed leaves no record behind, so a later uncloak
  /// stays a no-op instead of failing the same way again, and an uncloak
  /// that took effect no longer needs one. A cloak that took effect, and
  /// an uncloak that failed, keep it.
  pub fn finish_cloak(
    &self,
    hidden: bool,
    applied: bool,
  ) -> crate::Result<()> {
    if hidden == applied {
      return Ok(());
    }
    self.validate()?;
    let changes = property(&self.window, CHANGES);
    put_property(&self.window, CHANGES, changes & !CLOAK)
  }

  /// Queues a taskbar tab change without waiting on the shell.
  pub fn request_taskbar_visibility(
    &self,
    visible: bool,
  ) -> crate::Result<Option<ShellTicket>> {
    self.validate()?;
    let ticket = ShellTicket::new();
    submit_shell(self, ShellCall::TaskbarTab(visible), &ticket)?;
    Ok(Some(ticket))
  }

  /// Queues the taskbar's fullscreen mark without waiting on the shell.
  pub fn request_fullscreen_mark(
    &self,
    fullscreen: bool,
  ) -> crate::Result<Option<ShellTicket>> {
    self.validate()?;
    let ticket = ShellTicket::new();
    submit_shell(self, ShellCall::MarkFullscreen(fullscreen), &ticket)?;
    Ok(Some(ticket))
  }

  /// Makes the shell cloak call. The shell worker's thread only.
  ///
  /// Ownership is checked again once a cloak has landed. The call waits
  /// on explorer and `release` may have run meanwhile, so a cloak that
  /// outlived its ownership is undone rather than left on a window nobody
  /// will restore.
  pub(crate) fn execute_cloak(&self, hidden: bool) -> crate::Result<()> {
    self.validate()?;
    self.window.inner.set_cloaked_cached(hidden)?;
    if hidden && self.validate().is_err() {
      if let Err(err) = self.window.inner.set_cloaked_cached(false) {
        tracing::warn!("Expired cloak could not be undone: {err}");
      }
      return Err(crate::Error::Platform(
        "Window ownership expired.".into(),
      ));
    }
    Ok(())
  }

  /// Marks the window as stood in for by an overlay.
  ///
  /// Nothing native changes; the flag tells other processes that the
  /// pixels at this window's frame are not what the user sees.
  pub fn present(&self, presenting: bool) -> crate::Result<()> {
    self.set_flag(PRESENTED, presenting)
  }

  /// Marks the window as the WM's focus.
  ///
  /// The OS foreground follows only once the window is revealed, so a
  /// window arriving with a workspace is shown before it is foreground.
  /// The flag lets other processes know its focus from the start.
  pub fn mark_focused(&self, focused: bool) -> crate::Result<()> {
    self.set_flag(FOCUSED, focused)
  }

  /// Marks the window as decorated by the WM during native motion.
  ///
  /// Nothing native changes; the flag tells companions that the WM draws
  /// them above the moving window, so they should hide themselves until
  /// it clears. See `COMPANION`.
  pub fn mark_decorated(&self, decorated: bool) -> crate::Result<()> {
    self.set_flag(DECORATED, decorated)
  }

  /// Sets or clears one published bit of the shared `CHANGES` property.
  ///
  /// Only for `PRESENTED`, `DECORATED` and `FOCUSED`: an actual change
  /// raises `EVENT_OBJECT_STATECHANGE` so readers need not poll.
  /// Recovery bits are private and change without notice.
  fn set_flag(&self, flag: isize, set: bool) -> crate::Result<()> {
    self.validate()?;
    let changes = property(&self.window, CHANGES);
    let updated = if set { changes | flag } else { changes & !flag };
    if updated != changes {
      put_property(&self.window, CHANGES, updated)?;
      let child = i32::try_from(CHILDID_SELF)?;
      // SAFETY: Raises an event for a live window; no memory is passed.
      unsafe {
        NotifyWinEvent(
          EVENT_OBJECT_STATECHANGE,
          self.window.hwnd(),
          OBJID_WINDOW.0,
          child,
        );
      }
    }
    Ok(())
  }

  /// Observes compositor cloaking, whoever applied it.
  pub fn is_cloaked(&self) -> crate::Result<bool> {
    self.validate()?;
    self.window.inner.is_cloaked()
  }

  /// Applies recoverable native visibility.
  pub fn show(&self, visible: bool) -> crate::Result<()> {
    self.validate()?;
    let changes = property(&self.window, CHANGES);
    if visible {
      self.window.show()?;
      put_property(&self.window, CHANGES, changes & !HIDDEN)?;
    } else {
      put_property(&self.window, CHANGES, changes | HIDDEN)?;
      self.window.hide()?;
    }
    Ok(())
  }

  /// Records the recovery destination before temporary placement.
  pub fn remember_restore(&self, home: &Rect) -> crate::Result<()> {
    self.validate()?;
    for (key, coordinate) in
      FRAME
        .iter()
        .zip([home.left, home.top, home.right, home.bottom])
    {
      put_property(&self.window, *key, coordinate as isize)?;
    }
    put_property(
      &self.window,
      CHANGES,
      property(&self.window, CHANGES) | PARKED,
    )
  }

  /// Records recovery before corner parking.
  pub fn park(&self, home: &Rect, corner: &Rect) -> crate::Result<()> {
    self.remember_restore(home)?;
    self.window.set_frame(corner)
  }

  /// Clears confirmed corner parking.
  pub fn unpark(&self) -> crate::Result<()> {
    self.validate()?;
    put_property(
      &self.window,
      CHANGES,
      property(&self.window, CHANGES) & !PARKED,
    )
  }

  /// Queries responsive native minimum-size evidence.
  pub fn minimum_size(&self) -> crate::Result<Option<(i32, i32)>> {
    self.validate()?;
    let mut info = MINMAXINFO::default();
    // SAFETY: System message marshals this structure.
    let result = unsafe {
      SendMessageTimeoutW(
        self.window.hwnd(),
        WM_GETMINMAXINFO,
        WPARAM(0),
        LPARAM(std::ptr::from_mut(&mut info) as isize),
        SMTO_ABORTIFHUNG | SMTO_BLOCK,
        20,
        None,
      )
    };
    let size = (info.ptMinTrackSize.x, info.ptMinTrackSize.y);
    Ok((result.0 != 0 && size.0 > 0 && size.1 > 0).then_some(size))
  }

  /// Restores owned changes before relinquishing ownership.
  ///
  /// Waits on the shell and on the window's application, so it belongs to
  /// a thread that may block.
  ///
  /// Work a worker already holds for this window cannot be recalled, and
  /// is settled from the other side rather than waited for. Queued style
  /// changes are dropped here; queued shell calls fail their ownership
  /// check once the record is gone; and a call that is already running
  /// undoes itself when it lands and finds ownership expired (see
  /// `execute_cloak` and `execute_layered`). The recovery below looks
  /// again after the record is removed, so a change that landed between
  /// its first look and the removal is undone too.
  pub fn release(&self) -> crate::Result<()> {
    self.validate()?;
    STYLES.forget(self.window.hwnd().0);
    recover_window(&self.window, None).map(|_| ())
  }

  /// Restores owned changes without waiting on the window's application.
  ///
  /// For the shell worker. Removing the layered bit is queued on the
  /// target process's lane instead, after revealing the window by setting
  /// alpha opaque inline, and the recovery record is retained until it is
  /// gone. Returns `true` when recovery is complete. Returns `false` when
  /// the removal was queued, in which case `done` runs once it ends, with
  /// whether it worked, for the caller to try again. A caller should not
  /// retry a failed removal, since that would only fail the same way; the
  /// record stays for the next claim of the window. `done` does not run
  /// when this returns `true`.
  pub(crate) fn release_queued(
    &self,
    done: Completion,
  ) -> crate::Result<bool> {
    self.validate()?;
    recover_window(
      &self.window,
      Some(QueuedRemoval {
        session: self,
        done,
      }),
    )
    .map(|recovery| recovery == Recovery::Complete)
  }
}

/// How far `recover_window` got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Recovery {
  /// Every change is undone and the record is removed.
  Complete,
  /// The layered bit's removal is queued, and the record is retained
  /// until it lands.
  Queued,
}

/// A layered-bit removal that recovery may queue rather than perform.
struct QueuedRemoval<'a> {
  session: &'a NativeSession,
  /// Runs once the removal has ended.
  done: Completion,
}

/// Reads a pointer-sized recovery value.
fn property(window: &NativeWindow, key: PCWSTR) -> isize {
  // SAFETY: Named property contains integer data.
  unsafe { GetPropW(window.hwnd(), key).0 }
}

/// Writes recovery before changing native state.
fn put_property(
  window: &NativeWindow,
  key: PCWSTR,
  value: isize,
) -> crate::Result<()> {
  if value == 0 {
    if property(window, key) != 0 {
      // SAFETY: Removes only our named property.
      unsafe {
        RemovePropW(window.hwnd(), key)?;
      }
    }
  } else {
    // SAFETY: Integer properties are never dereferenced.
    unsafe { SetPropW(window.hwnd(), key, HANDLE(value)) }?;
  }
  Ok(())
}

/// Reads the home rect recorded before parking.
fn parked_home(
  window: &NativeWindow,
  changes: isize,
) -> crate::Result<Option<Rect>> {
  if changes & PARKED == 0 || window.is_minimized()? {
    return Ok(None);
  }
  let [left, top, right, bottom] =
    FRAME.map(|key| i32::try_from(property(window, key)));
  Ok(Some(Rect::from_ltrb(left?, top?, right?, bottom?)))
}

/// Restores exclusively tagged WM changes.
///
/// Concealment is lifted before placement settles. Placement writes are
/// asynchronous, so the first attempt almost always reports a pending
/// frame; returning early with the window still cloaked or transparent
/// strands it invisible whenever no caller retries. A misplaced window
/// is recoverable by the user, an unseen one is not.
///
/// With `queued`, removing the layered bit does not wait on the
/// application; see `NativeSession::release_queued`.
fn recover_window(
  window: &NativeWindow,
  queued: Option<QueuedRemoval<'_>>,
) -> crate::Result<Recovery> {
  if property(window, OWNER) == 0 {
    return Ok(Recovery::Complete);
  }
  let changes = property(window, CHANGES);
  // Start the move first, so concealment lifts over a travelling window.
  let home = parked_home(window, changes)?;
  if let Some(target) = &home {
    if window.frame_with_shadows()? != *target {
      window.set_frame(target)?;
    }
  }
  if changes & CLOAK != 0 && window.inner.is_cloaked()? {
    window.set_cloaked(false)?;
  }
  if changes & HIDDEN != 0 && !window.is_visible()? {
    window.show()?;
  }
  if changes & ALPHA != 0 {
    match queued {
      Some(QueuedRemoval { session, done })
        if window.has_window_style_ex(WS_EX_LAYERED) =>
      {
        // Reveal now; the bit follows. The record stays until it does.
        session.apply_alpha(u8::MAX);
        session.queue_layer_with(false, u8::MAX, true, Some(done))?;
        return Ok(Recovery::Queued);
      }
      Some(_) => {}
      None => {
        window.remove_window_style_ex(WS_EX_LAYERED);
        if window.has_window_style_ex(WS_EX_LAYERED) {
          return Err(crate::Error::Platform(
            "Opacity restoration failed.".into(),
          ));
        }
      }
    }
  }
  if changes & TITLE != 0 {
    window.set_title_bar_visibility(changes & ORIGINAL_TITLE != 0)?;
  }
  // Retain the recovery record until the window reaches its home.
  if let Some(target) = &home {
    if window.frame_with_shadows()? != *target {
      return Err(crate::Error::Platform(
        "Window recovery pending.".into(),
      ));
    }
  }
  for key in FRAME.into_iter().chain([CHANGES, OWNER]) {
    // SAFETY: Removes only our named properties.
    if property(window, key) != 0 {
      unsafe {
        RemovePropW(window.hwnd(), key)?;
      }
    }
  }
  // A worker's call that was already running when the checks above ran
  // may have landed since. Its own ownership check fails now that the
  // record is gone, so it undoes itself; this look catches one that
  // landed before its check. Best effort: nothing is left to retry with.
  if changes & CLOAK != 0 && window.inner.is_cloaked().unwrap_or(false) {
    if let Err(err) = window.set_cloaked(false) {
      tracing::warn!("Late cloak could not be undone: {err}");
    }
  }
  if changes & ALPHA != 0 && window.has_window_style_ex(WS_EX_LAYERED) {
    window.remove_window_style_ex(WS_EX_LAYERED);
  }
  Ok(Recovery::Complete)
}

/// Recovers windows missed by IPC delivery.
pub fn recover_owned_windows() -> crate::Result<()> {
  /// Collects handles without mutating windows.
  unsafe extern "system" fn collect(hwnd: HWND, data: LPARAM) -> BOOL {
    // SAFETY: Enumeration retains this vector exclusively.
    unsafe {
      (*(data.0 as *mut Vec<isize>)).push(hwnd.0);
    }
    BOOL(1)
  }
  let mut handles = Vec::<isize>::new();
  // SAFETY: Vector outlives synchronous enumeration.
  unsafe {
    EnumWindows(
      Some(collect),
      LPARAM(std::ptr::from_mut(&mut handles) as isize),
    )?;
  }
  let mut failure = None;
  for handle in handles {
    if let Err(err) =
      recover_window(&NativeWindow::from_handle(handle), None)
    {
      tracing::warn!("Window recovery failed: {err}");
      failure = Some(err);
    }
  }
  failure.map_or(Ok(()), Err)
}

/// Observes composition, not application paint completion.
pub fn composition_frame() -> crate::Result<u64> {
  let mut timing = DWM_TIMING_INFO {
    cbSize: u32::try_from(std::mem::size_of::<DWM_TIMING_INFO>())?,
    ..Default::default()
  };
  // SAFETY: Initialized output receives compositor timing.
  unsafe {
    DwmGetCompositionTimingInfo(HWND(0), &raw mut timing)?;
  }
  Ok(timing.cFrame)
}

/// Converts screen rectangles to placement coordinates.
pub(crate) fn placement_rect(
  rect: &Rect,
  tool_window: bool,
) -> crate::Result<Rect> {
  if tool_window {
    return Ok(rect.clone());
  }
  let native = RECT {
    left: rect.left,
    top: rect.top,
    right: rect.right,
    bottom: rect.bottom,
  };
  // SAFETY: Reads a monitor handle from a caller-owned rect.
  let monitor = unsafe {
    MonitorFromRect(&raw const native, MONITOR_DEFAULTTONEAREST)
  };
  let display = crate::platform_impl::Display::new(monitor.0);
  Ok(rect.to_workspace(&display.bounds()?, &display.working_area()?))
}

#[cfg(test)]
mod tests {
  use windows::Win32::UI::WindowsAndMessaging::CS_OWNDC;

  use super::{alpha_is_safe, opacity_action, OpacityAction};
  use crate::OpacityValue;

  #[test]
  fn plain_window_takes_alpha() {
    assert!(alpha_is_safe(false, 0));
  }

  #[test]
  fn composition_window_is_cloaked() {
    assert!(!alpha_is_safe(true, 0));
  }

  #[test]
  fn class_owned_dc_is_cloaked() {
    assert!(!alpha_is_safe(false, CS_OWNDC.0 as usize));
  }

  /// Never mutates application-owned transparency attributes.
  #[test]
  fn preserves_application_transparency() {
    assert_eq!(
      opacity_action(false, false, Some(OpacityValue(0.0))),
      OpacityAction::Unsupported
    );
    assert_eq!(
      opacity_action(false, false, None),
      OpacityAction::Unchanged
    );
    assert_eq!(
      opacity_action(true, false, None),
      OpacityAction::Unchanged
    );
    assert_eq!(opacity_action(true, true, None), OpacityAction::Restore);
  }
}
