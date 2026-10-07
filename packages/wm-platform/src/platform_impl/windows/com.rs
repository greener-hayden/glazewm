use std::{cell::RefCell, collections::HashMap};

use windows::{
  core::{ComInterface, IUnknown, IUnknown_Vtbl, GUID, HRESULT},
  Win32::{
    System::Com::{
      CoCreateInstance, CoInitializeEx, CoUninitialize, IServiceProvider,
      CLSCTX_ALL, CLSCTX_SERVER, COINIT_APARTMENTTHREADED,
    },
    UI::Shell::{ITaskbarList2, TaskbarList},
  },
};

/// COM class identifier (CLSID) for the Windows Shell that implements the
/// `IServiceProvider` interface.
const CLSID_IMMERSIVE_SHELL: GUID =
  GUID::from_u128(0xC2F03A33_21F5_47FA_B4BB_156362A2F239);

thread_local! {
  /// Manages per-thread COM initialization. COM must be initialized on each
  /// thread that uses it, so we store this in thread-local storage to handle
  /// the setup and cleanup automatically.
  ///
  /// Wrapped in `RefCell` to allow mutation via `COM_INIT.borrow_mut()`.
  pub(crate) static COM_INIT: RefCell<ComInit> = RefCell::new(ComInit::new());
}

/// How many `IApplicationView` proxies a thread keeps.
///
/// Destroyed windows are evicted as they are reported, so this only bounds
/// the cache against reports that never arrive.
const MAX_CACHED_VIEWS: usize = 128;

pub(crate) struct ComInit {
  service_provider: Option<IServiceProvider>,
  application_view_collection: Option<IApplicationViewCollection>,
  taskbar_list: Option<ITaskbarList2>,
  /// Shell views by window handle, so repeat cloaking of a window skips
  /// the lookup round trip into explorer.
  views: HashMap<isize, IApplicationView>,
}

impl ComInit {
  /// Initializes COM on the current thread with apartment threading model.
  /// `COINIT_APARTMENTTHREADED` is required for shell COM objects.
  ///
  /// # Panics
  ///
  /// Panics if COM initialization fails. This is typically only possible
  /// if COM is already initialized with an incompatible threading model.
  #[must_use]
  pub(crate) fn new() -> Self {
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
      .expect("Unable to initialize COM.");

    let service_provider = unsafe {
      CoCreateInstance(&CLSID_IMMERSIVE_SHELL, None, CLSCTX_ALL)
    }
    .ok();

    let application_view_collection = service_provider.as_ref().and_then(
      |provider: &IServiceProvider| unsafe {
        provider.QueryService(&IApplicationViewCollection::IID).ok()
      },
    );

    let taskbar_list =
      unsafe { CoCreateInstance(&TaskbarList, None, CLSCTX_SERVER) }.ok();

    Self {
      service_provider,
      application_view_collection,
      taskbar_list,
      views: HashMap::new(),
    }
  }

  /// Cloaks or uncloaks a window through a cached shell view.
  ///
  /// A cached view that no longer answers is evicted, and the window's
  /// view is looked up again, refreshing the shell interfaces once if
  /// that fails as well. The call is one round trip into explorer when
  /// the view is cached, and two otherwise.
  pub(crate) fn set_cloak_cached(
    &mut self,
    hwnd: isize,
    cloaked: bool,
  ) -> crate::Result<()> {
    let flag = if cloaked { 2 } else { 0 };
    if let Some(view) = self.views.get(&hwnd) {
      // SAFETY: The view is a live shell proxy owned by this thread.
      if unsafe { view.set_cloak(1, flag) }.ok().is_ok() {
        return Ok(());
      }
      self.views.remove(&hwnd);
    }

    let view = self.with_retry(|com| {
      let view_collection = com.application_view_collection()?;

      let mut view: Option<IApplicationView> = None;
      // SAFETY: `view` outlives the call and receives the shell's proxy.
      unsafe { view_collection.get_view_for_hwnd(hwnd, &raw mut view) }
        .ok()?;

      let view = view.ok_or_else(|| {
        crate::Error::Platform(
          "Unable to get application view by window handle.".to_string(),
        )
      })?;

      // Ref: https://github.com/Ciantic/AltTabAccessor/issues/1#issuecomment-1426877843
      // SAFETY: The view was just returned by the shell.
      unsafe { view.set_cloak(1, flag) }.ok().map_err(|_| {
        crate::Error::Platform("Failed to cloak window.".to_string())
      })?;
      Ok(view)
    })?;

    if self.views.len() >= MAX_CACHED_VIEWS {
      self.views.clear();
    }
    self.views.insert(hwnd, view);
    Ok(())
  }

  /// Drops the cached view of a destroyed window.
  pub(crate) fn forget_view(&mut self, hwnd: isize) {
    self.views.remove(&hwnd);
  }

  /// Returns an instance of `IApplicationViewCollection`.
  pub(crate) fn application_view_collection(
    &self,
  ) -> crate::Result<&IApplicationViewCollection> {
    self.application_view_collection.as_ref().ok_or_else(|| {
      crate::Error::Platform(
        "Failed to query for `IApplicationViewCollection` instance."
          .to_string(),
      )
    })
  }

  /// Returns an instance of `ITaskbarList2`.
  pub(crate) fn taskbar_list(&self) -> crate::Result<&ITaskbarList2> {
    self.taskbar_list.as_ref().ok_or_else(|| {
      crate::Error::Platform(
        "Unable to create `ITaskbarList2` instance.".to_string(),
      )
    })
  }

  /// Refreshes cached COM interfaces.
  ///
  /// Called automatically by `with_retry` when COM operations fail due to
  /// stale interface pointers (e.g. after Explorer restarts).
  pub(crate) fn refresh(&mut self) {
    // Views from the previous shell are stale.
    self.views.clear();

    // Re-create the service provider.

    self.service_provider = unsafe {
      CoCreateInstance(&CLSID_IMMERSIVE_SHELL, None, CLSCTX_ALL)
    }
    .ok();

    // Re-create the application view collection.
    self.application_view_collection = self
      .service_provider
      .as_ref()
      .and_then(|provider: &IServiceProvider| unsafe {
        provider.QueryService(&IApplicationViewCollection::IID).ok()
      });

    // Re-create the taskbar list.
    self.taskbar_list =
      unsafe { CoCreateInstance(&TaskbarList, None, CLSCTX_SERVER) }.ok();
  }

  /// Executes a COM operation, refreshing interfaces on failure and
  /// retrying once. Use this for operations that may fail due to stale
  /// COM interfaces.
  pub fn with_retry<T, F>(&mut self, op: F) -> crate::Result<T>
  where
    F: Fn(&Self) -> crate::Result<T>,
  {
    if let Ok(result) = op(self) {
      Ok(result)
    } else {
      self.refresh();
      op(self)
    }
  }
}

impl Default for ComInit {
  fn default() -> Self {
    Self::new()
  }
}

impl Drop for ComInit {
  fn drop(&mut self) {
    // Explicitly drop COM interfaces first.
    self.views.clear();
    drop(self.taskbar_list.take());
    drop(self.application_view_collection.take());
    drop(self.service_provider.take());

    unsafe { CoUninitialize() };
  }
}

/// Undocumented COM interface for Windows shell functionality.
///
/// Note that filler methods are added to match the vtable layout.
#[windows_interface::interface("1841c6d7-4f9d-42c0-af41-8747538f10e5")]
pub unsafe trait IApplicationViewCollection: IUnknown {
  pub unsafe fn m1(&self);
  pub unsafe fn m2(&self);
  pub unsafe fn m3(&self);
  pub unsafe fn get_view_for_hwnd(
    &self,
    window: isize,
    application_view: *mut Option<IApplicationView>,
  ) -> HRESULT;
}

/// Undocumented COM interface for managing views in the Windows shell.
///
/// Note that filler methods are added to match the vtable layout.
#[windows_interface::interface("372E1D3B-38D3-42E4-A15B-8AB2B178F513")]
pub unsafe trait IApplicationView: IUnknown {
  pub unsafe fn m1(&self);
  pub unsafe fn m2(&self);
  pub unsafe fn m3(&self);
  pub unsafe fn m4(&self);
  pub unsafe fn m5(&self);
  pub unsafe fn m6(&self);
  pub unsafe fn m7(&self);
  pub unsafe fn m8(&self);
  pub unsafe fn m9(&self);
  pub unsafe fn set_cloak(
    &self,
    cloak_type: u32,
    cloak_flag: i32,
  ) -> HRESULT;
}
