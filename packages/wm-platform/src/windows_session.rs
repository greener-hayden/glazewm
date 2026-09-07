use windows::{
  core::{w, PCWSTR},
  Win32::{
    Foundation::{BOOL, HANDLE, HWND, LPARAM, RECT, WPARAM},
    Graphics::{
      Dwm::{DwmGetCompositionTimingInfo, DWM_TIMING_INFO},
      Gdi::{MonitorFromRect, MONITOR_DEFAULTTONEAREST},
    },
    UI::{
      HiDpi::{
        GetAwarenessFromDpiAwarenessContext, GetDpiForWindow,
        GetWindowDpiAwarenessContext, DPI_AWARENESS_PER_MONITOR_AWARE,
      },
      WindowsAndMessaging::{
        EnumWindows, GetClassLongPtrW, GetPropW, GetWindowThreadProcessId,
        RemovePropW, SendMessageTimeoutW, SetPropW, CS_CLASSDC, CS_OWNDC,
        GCL_STYLE, MINMAXINFO, SMTO_ABORTIFHUNG, SMTO_BLOCK,
        WM_GETMINMAXINFO, WS_DLGFRAME, WS_EX_LAYERED,
        WS_EX_NOREDIRECTIONBITMAP,
      },
    },
  },
};

use crate::{
  ConcealMethod, NativeWindow, NativeWindowWindowsExt, OpacityValue, Rect,
};

const OWNER: PCWSTR = w!("GlazeWM.greener.owner.v1");
/// Recovery flags, also a contract for other processes.
///
/// Border renderers read `PRESENTED` to hide a ring while an overlay
/// stands in for the window. The property name and bit values are stable.
const CHANGES: PCWSTR = w!("GlazeWM.greener.changes.v1");
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
      recover_window(&window)?;
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

  /// Applies only WM-owned opacity changes.
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

  /// Marks the window as stood in for by an overlay.
  ///
  /// Nothing native changes; the flag tells other processes that the
  /// pixels at this window's frame are not what the user sees.
  pub fn present(&self, presenting: bool) -> crate::Result<()> {
    self.validate()?;
    let changes = property(&self.window, CHANGES);
    let updated = if presenting {
      changes | PRESENTED
    } else {
      changes & !PRESENTED
    };
    if updated != changes {
      put_property(&self.window, CHANGES, updated)?;
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
  pub fn release(&self) -> crate::Result<()> {
    self.validate()?;
    recover_window(&self.window)
  }
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
fn recover_window(window: &NativeWindow) -> crate::Result<()> {
  if property(window, OWNER) == 0 {
    return Ok(());
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
    window.remove_window_style_ex(WS_EX_LAYERED);
    if window.has_window_style_ex(WS_EX_LAYERED) {
      return Err(crate::Error::Platform(
        "Opacity restoration failed.".into(),
      ));
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
  Ok(())
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
    if let Err(err) = recover_window(&NativeWindow::from_handle(handle)) {
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
