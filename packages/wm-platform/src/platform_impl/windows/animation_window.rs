use std::{
  cell::RefCell,
  fmt::Write,
  sync::{Mutex, Once, PoisonError},
  time::{Duration, Instant},
};

use windows::{
  core::{w, PCWSTR},
  Win32::{
    Foundation::{BOOL, HWND, LPARAM, LRESULT, RECT, WPARAM},
    Graphics::Dwm::{
      DwmGetWindowAttribute, DwmQueryThumbnailSourceSize,
      DwmRegisterThumbnail, DwmUnregisterThumbnail,
      DwmUpdateThumbnailProperties, DWMWA_CLOAKED,
      DWM_THUMBNAIL_PROPERTIES, DWM_TNP_OPACITY, DWM_TNP_RECTDESTINATION,
      DWM_TNP_RECTSOURCE, DWM_TNP_SOURCECLIENTAREAONLY, DWM_TNP_VISIBLE,
    },
    System::DataExchange::GlobalFindAtomW,
    UI::WindowsAndMessaging::{
      CreateWindowExW, DefWindowProcW, DestroyWindow, EnumWindows,
      GetPropW, GetWindow, GetWindowLongPtrW, GetWindowRect,
      RegisterClassW, SetWindowPos, GWL_EXSTYLE, GW_HWNDPREV,
      HTTRANSPARENT, HWND_TOP, SET_WINDOW_POS_FLAGS, SWP_NOACTIVATE,
      SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, WM_NCHITTEST,
      WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_NOREDIRECTIONBITMAP,
      WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
    },
  },
};

use crate::{
  companion::{self, DiscoveryThrottle},
  windows_session::COMPANION,
  Dispatcher, NativeWindow, OpacityValue, Rect, WindowId,
};

/// Most companions one overlay draws.
///
/// A border ring is four bands. The bound keeps a misbehaving process
/// from making every update of one overlay arbitrarily expensive.
const MAX_COMPANIONS: usize = 8;

/// Shortest spacing between two companion failure warnings of one
/// overlay; failures in between are logged at debug level.
const WARN_INTERVAL: Duration = Duration::from_secs(1);

/// Platform-specific implementation of [`AnimationContext`].
///
/// Holds nothing on Windows. DWM paints the overlay from the source
/// window's own surface, so there is no device to share and nothing to
/// commit.
pub(crate) struct AnimationContext;

impl AnimationContext {
  /// Implements [`AnimationContext::new`].
  #[allow(clippy::unnecessary_wraps)]
  pub(crate) fn new(_dispatcher: &Dispatcher) -> crate::Result<Self> {
    Ok(Self)
  }

  /// Implements [`AnimationContext::capture_frame`].
  ///
  /// Nothing is captured. The overlay shows the window live, so this
  /// returns at once from any thread.
  #[allow(clippy::unnecessary_wraps, clippy::unused_self)]
  pub(crate) fn capture_frame(
    &self,
    _window_id: WindowId,
  ) -> crate::Result<AnimationCapture> {
    Ok(AnimationCapture)
  }

  /// Implements [`AnimationContext::transaction`].
  ///
  /// Thumbnail updates issued within one frame are composed together by
  /// DWM, so a transaction is only the hop to the main thread.
  #[allow(clippy::unused_self)]
  pub(crate) fn transaction<F, R>(
    &self,
    update_fn: F,
    dispatcher: &Dispatcher,
  ) -> crate::Result<R>
  where
    F: FnOnce() -> R + Send,
    R: Send,
  {
    dispatcher.dispatch_sync(update_fn)
  }
}

/// Platform-specific implementation of [`AnimationWindow`].
///
/// A popup that DWM paints a live thumbnail of the source window into.
/// A thumbnail is drawn from the source's own surface, so it keeps
/// rendering while the source is transparent or cloaked, costs no
/// capture, and shows the window exactly as it looks when handed back.
/// The screenshot engine this replaces stalled every animated sync by
/// ~60ms of capture and then popped from a stretched still to the real
/// window at the end; the thumbnail does neither.
pub(crate) struct AnimationWindow {
  handle: isize,
  thumbnail: isize,
  /// The source window's handle.
  source: isize,
  /// The source's invisible frame insets, measured once at registration.
  source_origin: (i32, i32),
  /// Frame of the `AnimationWindow`.
  outer_rect: Rect,
  /// Thumbnails of the source's companions. See `COMPANION`.
  ///
  /// Locked only on the event loop thread during updates, and briefly by
  /// `companions_revealed` at the reveal; never contended in practice.
  companions: Mutex<Companions>,
  dispatcher: Dispatcher,
}

/// One companion window drawn into the overlay.
struct Companion {
  hwnd: isize,
  thumbnail: isize,
  /// When a companion found mid-animation started fading in; `None` once
  /// it is fully faded in, or when it was present from the start.
  fading_since: Option<Instant>,
  /// The companion's window rect when native motion began. `None` for an
  /// overlay that stands in for the window, which maps the live rect.
  anchor: Option<Rect>,
}

/// How an overlay places its companions relative to its source.
#[derive(Clone, Copy)]
enum Mapping<'a> {
  /// Overlay motion: the live companion rect moves through the transform
  /// from `frame`, the source's live frame, to `animated`, where the
  /// overlay draws it. See `companion::companion_rect`.
  Scale { frame: &'a Rect, animated: &'a Rect },
  /// Native motion: the recorded companion rect is anchored to the edges
  /// of `moved`, the source's requested window rect, from `frame`, its
  /// rect when the companions were recorded. See
  /// `companion::anchored_rect`.
  Anchor { frame: &'a Rect, moved: &'a Rect },
}

thread_local! {
  /// The last scan for companions, shared by every overlay on the event
  /// loop thread. See `CompanionScan`.
  static COMPANION_SCAN: RefCell<CompanionScan> =
    RefCell::new(CompanionScan::default());
}

/// One pass over the top-level windows, recording every companion.
///
/// A workspace switch prepares an overlay per window in one commit, and a
/// separate scan for each put 1-7ms per window on the path from keypress
/// to first frame. One scan now serves every overlay for as long as late
/// discovery would tolerate anyway: one `DiscoveryThrottle` interval.
/// A late search on an overlay that found nothing always rescans, because
/// its companions may have appeared since.
#[derive(Default)]
struct CompanionScan {
  at: Option<Instant>,
  /// Pairs of companion and the window it decorates.
  pairs: Vec<(isize, isize)>,
}

impl CompanionScan {
  /// Appends the companions of `source` to `found`, scanning only when
  /// `rescan` is set or the shared scan is older than one discovery
  /// interval.
  fn companions_of(
    source: isize,
    rescan: bool,
    found: &mut Vec<isize>,
  ) -> crate::Result<()> {
    COMPANION_SCAN.with(|scan| {
      let mut scan = scan.borrow_mut();
      let now = Instant::now();
      let fresh = !rescan
        && scan.at.is_some_and(|at| {
          now.saturating_duration_since(at) < companion::DISCOVERY_INTERVAL
        });
      if !fresh {
        scan.rescan()?;
        scan.at = Some(now);
      }
      found.extend(
        scan
          .pairs
          .iter()
          .filter(|(hwnd, target)| *target == source && *hwnd != source)
          .map(|(hwnd, _)| *hwnd),
      );
      Ok(())
    })
  }

  /// Rebuilds the pairs from the current top-level windows.
  fn rescan(&mut self) -> crate::Result<()> {
    /// Records windows that carry the companion property.
    ///
    /// Hidden windows are included: an arriving window's companions are
    /// still hidden when its overlay is prepared, and show themselves,
    /// cloaked, only once the window is presented.
    unsafe extern "system" fn collect(hwnd: HWND, data: LPARAM) -> BOOL {
      // SAFETY: `data` points at the search below, which outlives the
      // synchronous enumeration and is not otherwise accessed during it.
      let search =
        unsafe { &mut *(data.0 as *mut (u16, &mut Vec<(isize, isize)>)) };
      // SAFETY: Looks the property up by its atom (MAKEINTATOM), which
      // avoids converting the name on every window. The property holds
      // an integer, never a pointer.
      let target =
        unsafe { GetPropW(hwnd, PCWSTR(search.0 as usize as *const u16)) }
          .0;
      if target != 0 {
        search.1.push((hwnd.0, target));
      }
      BOOL(1)
    }

    self.pairs.clear();
    // A property set by name lives in the global atom table while any
    // window carries it. No atom means no companion exists anywhere, so
    // the enumeration is skipped.
    // SAFETY: Looks up a constant, NUL-terminated name.
    let atom = unsafe { GlobalFindAtomW(COMPANION) };
    if atom == 0 {
      return Ok(());
    }
    let mut search = (atom, &mut self.pairs);
    // SAFETY: The search outlives the synchronous enumeration.
    unsafe {
      EnumWindows(
        Some(collect),
        LPARAM(std::ptr::from_mut(&mut search) as isize),
      )
    }
    .map_err(crate::Error::from)
  }
}

/// Step timings of one overlay's creation.
///
/// Overlay creation sits on the path from a keypress to the first frame,
/// and on every retarget, so a slow creation is logged at debug level
/// with the cost of each step.
struct OverlayTiming {
  started: Instant,
  /// Elapsed time at the end of each step: source origin, thread hop,
  /// window, thumbnail, show and companion search.
  marks: [Duration; 6],
  count: usize,
}

impl OverlayTiming {
  /// Names of the steps the marks end, then the companion update.
  const STEPS: [&'static str; 7] = [
    "origin",
    "hop",
    "create",
    "thumbnail",
    "show",
    "discover",
    "companions",
  ];

  /// Starts timing now.
  fn start() -> Self {
    Self {
      started: Instant::now(),
      marks: [Duration::ZERO; 6],
      count: 0,
    }
  }

  /// Records the end of the next step.
  fn mark(&mut self) {
    if let Some(mark) = self.marks.get_mut(self.count) {
      *mark = self.started.elapsed();
      self.count += 1;
    }
  }

  /// Logs each step's duration when creation took 2ms or more.
  fn log(&self) {
    let total = self.started.elapsed();
    if total < Duration::from_millis(2) {
      return;
    }
    let mut previous = Duration::ZERO;
    let mut steps = String::new();
    let ends = self.marks[..self.count]
      .iter()
      .copied()
      .chain(std::iter::once(total));
    for (name, end) in Self::STEPS.iter().zip(ends) {
      let step = end.saturating_sub(previous);
      // Writing to a `String` cannot fail.
      let _ = write!(steps, " {name}={}us", step.as_micros());
      previous = end;
    }
    tracing::debug!("overlay_new total={}us{steps}", total.as_micros());
  }
}

/// The companions of one overlay and the state of their discovery.
#[derive(Default)]
struct Companions {
  registered: Vec<Companion>,
  throttle: DiscoveryThrottle,
  /// Reused search results, so a search allocates only the first time.
  found: Vec<isize>,
  /// When a companion failure was last logged as a warning.
  warned_at: Option<Instant>,
}

impl Companions {
  /// Registers thumbnails of companions of `source` into `overlay`.
  ///
  /// `late` is when the search ran, if the animation had already started;
  /// companions found then fade in. Failures are logged, never returned.
  fn discover(
    &mut self,
    overlay: isize,
    source: isize,
    late: Option<Instant>,
  ) {
    if self.search(source, late.is_some()) {
      self.register(overlay, late);
    }
  }

  /// Fills `found` with the companions of `source`.
  ///
  /// Returns `false` when the search failed, which is logged.
  fn search(&mut self, source: isize, rescan: bool) -> bool {
    self.found.clear();
    if let Err(err) =
      CompanionScan::companions_of(source, rescan, &mut self.found)
    {
      self.warn(format_args!("Companion search failed: {err}"));
      return false;
    }
    true
  }

  /// Registers thumbnails of the companions in `found` into `overlay`.
  ///
  /// `late` is as for `discover`. Failures are logged, never returned.
  fn register(&mut self, overlay: isize, late: Option<Instant>) {
    for index in 0..self.found.len() {
      let hwnd = self.found[index];
      if self.registered.len() >= MAX_COMPANIONS {
        break;
      }
      if self
        .registered
        .iter()
        .any(|companion| companion.hwnd == hwnd)
      {
        continue;
      }
      // SAFETY: Our overlay is live; a stale companion handle fails the
      // call rather than misbehaving.
      match unsafe { DwmRegisterThumbnail(HWND(overlay), HWND(hwnd)) } {
        Ok(thumbnail) => {
          tracing::debug!("Overlay companion {hwnd:#x} registered.");
          self.registered.push(Companion {
            hwnd,
            thumbnail,
            fading_since: late,
            anchor: None,
          });
        }
        Err(err) => self.warn(format_args!(
          "Companion {hwnd:#x} registration failed: {err}"
        )),
      }
    }
  }

  /// Records each companion's current window rect as its anchor.
  ///
  /// A companion that cannot be measured is unregistered.
  fn anchor(&mut self) {
    self.registered.retain_mut(|companion| {
      if let Some(rect) = window_rect(companion.hwnd) {
        companion.anchor = Some(rect);
        true
      } else {
        companion.unregister();
        false
      }
    });
  }

  /// Updates every companion thumbnail for one overlay frame.
  ///
  /// `mapping` places each companion; see `Mapping`. Companions that are
  /// gone or fail are unregistered; the rest keep drawing.
  fn update(
    &mut self,
    source: isize,
    mapping: Mapping,
    outer_rect: &Rect,
    opacity: Option<&OpacityValue>,
    now: Instant,
  ) {
    let mut failure = None;
    self.registered.retain_mut(|companion| {
      match companion.update(source, mapping, outer_rect, opacity, now) {
        Ok(true) => true,
        Ok(false) => {
          tracing::debug!(
            "Overlay companion {:#x} is gone.",
            companion.hwnd
          );
          companion.unregister();
          false
        }
        Err(err) => {
          failure = Some((companion.hwnd, err));
          companion.unregister();
          false
        }
      }
    });
    if let Some((hwnd, err)) = failure {
      self.warn(format_args!("Companion {hwnd:#x} update failed: {err}"));
    }
  }

  /// Whether every companion shows itself again, or is gone.
  fn revealed(&self, source: isize) -> bool {
    self.registered.iter().all(|companion| {
      if !companion.decorates(source) {
        return true;
      }
      let mut cloaked = 0u32;
      // SAFETY: The output is a live `u32` of the size passed.
      let read = unsafe {
        DwmGetWindowAttribute(
          HWND(companion.hwnd),
          DWMWA_CLOAKED,
          std::ptr::from_mut(&mut cloaked).cast(),
          // LINT: `size_of::<u32>()` is 4.
          #[allow(clippy::cast_possible_truncation)]
          {
            std::mem::size_of::<u32>() as u32
          },
        )
      };
      read.is_err() || cloaked == 0
    })
  }

  /// Logs a companion failure, as a warning at most once per
  /// `WARN_INTERVAL`.
  fn warn(&mut self, message: std::fmt::Arguments) {
    let now = Instant::now();
    if self
      .warned_at
      .is_none_or(|at| now.saturating_duration_since(at) >= WARN_INTERVAL)
    {
      self.warned_at = Some(now);
      tracing::warn!("{message}");
    } else {
      tracing::debug!("{message}");
    }
  }
}

impl Companion {
  /// Whether the window still names `source` as the window it decorates.
  ///
  /// Also rejects a destroyed window, and a reused handle, whose property
  /// reads as 0.
  fn decorates(&self, source: isize) -> bool {
    // SAFETY: The companion property holds an integer, never a pointer.
    unsafe { GetPropW(HWND(self.hwnd), COMPANION) }.0 == source
  }

  /// Moves the thumbnail to the companion's mapped rect.
  ///
  /// The whole live companion is drawn into the mapped rect, so a
  /// companion resized since its anchor was recorded still fits.
  ///
  /// Returns `Ok(false)` when the companion is gone.
  fn update(
    &mut self,
    source: isize,
    mapping: Mapping,
    outer_rect: &Rect,
    opacity: Option<&OpacityValue>,
    now: Instant,
  ) -> crate::Result<bool> {
    if !self.decorates(source) {
      return Ok(false);
    }
    let Some(rect) = window_rect(self.hwnd) else {
      return Ok(false);
    };
    let fade = self.fading_since.map_or(1.0, |since| {
      companion::fade_in(now.saturating_duration_since(since))
    });
    if fade >= 1.0 {
      self.fading_since = None;
    }
    let opacity =
      OpacityValue(opacity.map_or(1.0, |opacity| opacity.0) * fade);
    let destination = match mapping {
      Mapping::Scale { frame, animated } => {
        companion::companion_rect(&rect, frame, animated)
      }
      Mapping::Anchor { frame, moved } => companion::anchored_rect(
        self.anchor.as_ref().unwrap_or(&rect),
        frame,
        moved,
      ),
    }
    .unwrap_or_else(|| Rect::from_xy(0, 0, 0, 0));
    let props = AnimationWindow::thumbnail_properties(
      &destination,
      outer_rect,
      &Rect::from_xy(0, 0, rect.width(), rect.height()),
      Some(&opacity),
    );
    // SAFETY: Updates our registered thumbnail.
    unsafe {
      DwmUpdateThumbnailProperties(self.thumbnail, &raw const props)
    }?;
    Ok(true)
  }

  /// Unregisters the thumbnail, once.
  fn unregister(&mut self) {
    if self.thumbnail == 0 {
      return;
    }
    // SAFETY: Unregisters our still-owned thumbnail.
    if let Err(err) = unsafe { DwmUnregisterThumbnail(self.thumbnail) } {
      tracing::debug!("Companion thumbnail release failed: {err}");
    }
    self.thumbnail = 0;
  }
}

impl AnimationWindow {
  /// Implements [`AnimationWindow::new`].
  pub(crate) fn new(
    _context: &AnimationContext,
    window: &NativeWindow,
    _capture: AnimationCapture,
    inner_rect: &Rect,
    outer_rect: &Rect,
    opacity: Option<OpacityValue>,
    dispatcher: &Dispatcher,
  ) -> crate::Result<Self> {
    let source_hwnd = window.inner.hwnd();
    // Measured here, on the caller's thread and once per overlay. See
    // `source_rect`.
    let mut timing = OverlayTiming::start();
    let source_origin = Self::source_origin(window)?;
    timing.mark();
    let (handle, thumbnail, companions) =
      dispatcher.dispatch_sync(move || {
        timing.mark();
        let handle = Self::create_window(outer_rect)?;
        timing.mark();
        // SAFETY: Source and destination are live windows.
        let thumbnail =
          match unsafe { DwmRegisterThumbnail(HWND(handle), source_hwnd) }
          {
            Ok(thumbnail) => thumbnail,
            Err(err) => {
              // SAFETY: Rolls back our new window.
              if let Err(cleanup) = unsafe { DestroyWindow(HWND(handle)) }
              {
                tracing::warn!("Overlay rollback failed: {cleanup}");
              }
              return Err(crate::Error::from(err));
            }
          };
        let mut prepare = || -> crate::Result<Rect> {
          let source_rect = Self::source_rect(source_origin, thumbnail)?;
          let props = Self::thumbnail_properties(
            inner_rect,
            outer_rect,
            &source_rect,
            opacity.as_ref(),
          );
          // SAFETY: Properties target our registered thumbnail.
          unsafe {
            DwmUpdateThumbnailProperties(thumbnail, &raw const props)?;
          }
          tracing::debug!(
            "Overlay source {source_rect:?} visible {} dest {:?}",
            props.fVisible.0 != 0,
            [
              props.rcDestination.left,
              props.rcDestination.top,
              props.rcDestination.right,
              props.rcDestination.bottom
            ]
          );
          timing.mark();
          Self::show_beneath(handle, source_hwnd)?;
          Ok(source_rect)
        };
        match prepare() {
          Ok(source_rect) => {
            timing.mark();
            // Companions never fail the overlay; see `Companions`.
            let mut companions = Companions::default();
            let now = Instant::now();
            if companions.throttle.try_begin(now) {
              companions.discover(handle, source_hwnd.0, None);
            }
            timing.mark();
            if !companions.registered.is_empty() {
              Self::update_companions(
                &mut companions,
                source_hwnd.0,
                &source_rect,
                inner_rect,
                outer_rect,
                opacity.as_ref(),
                now,
              );
            }
            timing.log();
            Ok((handle, thumbnail, companions))
          }
          Err(err) => {
            // SAFETY: Rolls back both owned resources.
            unsafe {
              if let Err(cleanup) = DwmUnregisterThumbnail(thumbnail) {
                tracing::warn!("Thumbnail rollback failed: {cleanup}");
              }
              if let Err(cleanup) = DestroyWindow(HWND(handle)) {
                tracing::warn!("Overlay rollback failed: {cleanup}");
              }
            }
            Err(err)
          }
        }
      })??;

    Ok(Self {
      handle,
      thumbnail,
      source: source_hwnd.0,
      source_origin,
      outer_rect: outer_rect.clone(),
      companions: Mutex::new(companions),
      dispatcher: dispatcher.clone(),
    })
  }

  /// Implements [`AnimationWindow::covers`].
  pub(crate) fn covers(&self, rect: &Rect) -> bool {
    self.outer_rect.contains_rect(rect)
  }

  /// Implements [`AnimationWindow::resize`].
  pub(crate) fn resize(&mut self, outer_rect: &Rect) -> crate::Result<()> {
    let handle = self.handle;
    self
      .dispatcher
      .dispatch_sync(|| place_window(handle, outer_rect))??;
    self.outer_rect = outer_rect.clone();
    Ok(())
  }

  /// Implements [`AnimationWindow::update`].
  pub(crate) fn update(
    &self,
    inner_rect: &Rect,
    opacity: Option<&OpacityValue>,
  ) -> crate::Result<()> {
    let thumbnail = self.thumbnail;
    if opacity.is_some() {
      tracing::debug!(
        "Overlay frame {inner_rect:?} at {:?}",
        opacity.map(OpacityValue::to_alpha)
      );
    }
    self.dispatcher.dispatch_sync(|| {
      let source_rect = Self::source_rect(self.source_origin, thumbnail)?;
      let props = Self::thumbnail_properties(
        inner_rect,
        &self.outer_rect,
        &source_rect,
        opacity,
      );
      // SAFETY: Updates our registered thumbnail.
      unsafe {
        DwmUpdateThumbnailProperties(thumbnail, &raw const props)
      }?;
      // Issued in the same compositor frame as the source's thumbnail, so
      // both move together.
      let mut companions = self
        .companions
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
      let now = Instant::now();
      if companions.registered.is_empty() {
        if !companions.throttle.try_begin(now) {
          return Ok(());
        }
        companions.discover(self.handle, self.source, Some(now));
        if companions.registered.is_empty() {
          return Ok(());
        }
      }
      Self::update_companions(
        &mut companions,
        self.source,
        &source_rect,
        inner_rect,
        &self.outer_rect,
        opacity,
        now,
      );
      Ok(())
    })?
  }

  /// Implements [`AnimationWindow::companions_revealed`].
  ///
  /// Reads each companion's cloak state from the compositor; callable
  /// from any thread.
  pub(crate) fn companions_revealed(&self) -> bool {
    self
      .companions
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .revealed(self.source)
  }

  /// Draws `companions` for one overlay frame.
  ///
  /// The source's frame is read live, because the transform is anchored
  /// to where the source is, not to the overlay. A source that cannot be
  /// measured leaves the companions where they were.
  fn update_companions(
    companions: &mut Companions,
    source: isize,
    source_rect: &Rect,
    inner_rect: &Rect,
    outer_rect: &Rect,
    opacity: Option<&OpacityValue>,
    now: Instant,
  ) {
    let Some(native) = window_rect(source) else {
      companions.warn(format_args!("Companion source unmeasured."));
      return;
    };
    // The same region `source_rect` selects from the source, in screen
    // coordinates: the overlay draws exactly this at `inner_rect`.
    let frame = Rect::from_xy(
      native.left + source_rect.x(),
      native.top + source_rect.y(),
      source_rect.width(),
      source_rect.height(),
    );
    companions.update(
      source,
      Mapping::Scale {
        frame: &frame,
        animated: inner_rect,
      },
      outer_rect,
      opacity,
      now,
    );
  }

  /// Implements [`AnimationWindow::destroy`].
  pub(crate) fn destroy(&mut self) -> crate::Result<()> {
    let companions = self
      .companions
      .get_mut()
      .unwrap_or_else(PoisonError::into_inner);
    if !companions.registered.is_empty() {
      let mut registered = std::mem::take(&mut companions.registered);
      self.dispatcher.dispatch_sync(move || {
        for companion in &mut registered {
          companion.unregister();
        }
      })?;
    }
    if self.thumbnail != 0 {
      let thumbnail = self.thumbnail;
      self.dispatcher.dispatch_sync(move || {
        // SAFETY: Unregisters our still-owned thumbnail.
        unsafe { DwmUnregisterThumbnail(thumbnail) }
      })??;
      self.thumbnail = 0;
    }
    if self.handle != 0 {
      let handle = HWND(self.handle);
      self.dispatcher.dispatch_sync(move || {
        // SAFETY: Destroys our still-owned window.
        unsafe { DestroyWindow(handle) }
      })??;
      self.handle = 0;
    }
    Ok(())
  }

  /// Reads the source's invisible frame insets, in source coordinates.
  ///
  /// Called once per overlay, off the event loop thread. `shadow_borders`
  /// costs two `GetWindowRect` calls and a `DwmGetWindowAttribute`, and
  /// the last is a round-trip to the compositor.
  fn source_origin(window: &NativeWindow) -> crate::Result<(i32, i32)> {
    let borders = window.inner.shadow_borders()?;
    Ok((borders.left.to_px(0, None), borders.top.to_px(0, None)))
  }

  /// Maps thumbnail dimensions using cached frame insets.
  fn source_rect(
    origin: (i32, i32),
    thumbnail: isize,
  ) -> crate::Result<Rect> {
    // SAFETY: Reads our registered thumbnail's dimensions.
    let size = unsafe { DwmQueryThumbnailSourceSize(thumbnail)? };
    Ok(Rect::from_xy(origin.0, origin.1, size.cx, size.cy))
  }

  /// Where and how opaque DWM draws the thumbnail within the window.
  ///
  /// `inner_rect` is in screen coordinates and lands relative to
  /// `outer_rect`, the window's frame. The source is scaled to fit.
  fn thumbnail_properties(
    inner_rect: &Rect,
    outer_rect: &Rect,
    source_rect: &Rect,
    opacity: Option<&OpacityValue>,
  ) -> DWM_THUMBNAIL_PROPERTIES {
    let clipped = crate::thumbnail_rects(
      inner_rect,
      outer_rect,
      (source_rect.width(), source_rect.height()),
    )
    .map(|(destination, source)| {
      let source = source.translate_to_coordinates(
        source.x() + source_rect.x(),
        source.y() + source_rect.y(),
      );
      (destination, source)
    });
    let visible = clipped.is_some();
    let (destination, source) = clipped.unwrap_or_else(|| {
      (Rect::from_xy(0, 0, 0, 0), Rect::from_xy(0, 0, 0, 0))
    });
    DWM_THUMBNAIL_PROPERTIES {
      dwFlags: DWM_TNP_RECTDESTINATION
        | DWM_TNP_RECTSOURCE
        | DWM_TNP_OPACITY
        | DWM_TNP_VISIBLE
        | DWM_TNP_SOURCECLIENTAREAONLY,
      rcDestination: RECT {
        left: destination.left,
        top: destination.top,
        right: destination.right,
        bottom: destination.bottom,
      },
      rcSource: RECT {
        left: source.left,
        top: source.top,
        right: source.right,
        bottom: source.bottom,
      },
      opacity: opacity.map_or(u8::MAX, OpacityValue::to_alpha),
      fVisible: BOOL(i32::from(visible)),
      fSourceClientAreaOnly: BOOL(0),
    }
  }

  /// Creates the window, hidden, at `rect`.
  fn create_window(rect: &Rect) -> crate::Result<isize> {
    const CLASS_NAME: PCWSTR = w!("AnimationWindow");

    static CLASS_REGISTERED: Once = Once::new();
    CLASS_REGISTERED.call_once(|| {
      let wnd_class = WNDCLASSW {
        lpszClassName: CLASS_NAME,
        lpfnWndProc: Some(AnimationWindow::overlay_wnd_proc),
        ..Default::default()
      };
      // SAFETY: The class struct outlives the call.
      unsafe { RegisterClassW(&raw const wnd_class) };
    });

    // SAFETY: Plain window creation with a registered class.
    let hwnd = unsafe {
      CreateWindowExW(
        WS_EX_NOREDIRECTIONBITMAP | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
        CLASS_NAME,
        w!(""),
        WS_POPUP,
        rect.x(),
        rect.y(),
        rect.width(),
        rect.height(),
        None,
        None,
        None,
        None,
      )
    };

    if hwnd.0 == 0 {
      return Err(crate::Error::Platform(
        "Failed to create animation window.".to_string(),
      ));
    }

    Ok(hwnd.0)
  }

  /// Shows the window directly beneath `source_hwnd` in the z-order.
  ///
  /// Two constraints, both deliberate. The overlay sits at the source's
  /// own depth, never `HWND_TOPMOST`, and it is torn down shortly after
  /// its animation ends (see `AnimationManager::destroy_animation`). A
  /// topmost, long-lived, click-through popup over a game is the shape of
  /// a cheat overlay, and anti-cheat heuristics look for exactly that. A
  /// brief one at the source's own depth is not.
  ///
  /// Beneath rather than above: the source is transparent while the
  /// overlay runs, and the moment it is opaque again it is meant to be
  /// what is seen.
  fn show_beneath(handle: isize, source_hwnd: HWND) -> crate::Result<()> {
    // SAFETY: Both handles are live windows.
    unsafe {
      SetWindowPos(
        HWND(handle),
        source_hwnd,
        0,
        0,
        0,
        0,
        SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
      )?;
    }

    Ok(())
  }

  /// Window procedure for the overlay class.
  unsafe extern "system" fn overlay_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
  ) -> LRESULT {
    // Route all mouse inputs to the window below.
    if msg == WM_NCHITTEST {
      LRESULT(HTTRANSPARENT as isize)
    } else {
      DefWindowProcW(hwnd, msg, wparam, lparam)
    }
  }
}

/// A token standing in for a capture; the overlay draws the window live.
pub(crate) struct AnimationCapture;

/// Platform-specific implementation of [`CompanionOverlay`].
///
/// A transparent popup directly above the source in the z-order, into
/// which DWM paints only the source's companions. See `COMPANION`.
pub(crate) struct CompanionOverlay {
  handle: isize,
  /// The source window's handle.
  source: isize,
  /// The source's window rect when the companions' anchors were recorded.
  frame: Rect,
  /// Frame of the overlay window.
  outer_rect: Rect,
  /// Locked only on the event loop thread during updates, and briefly by
  /// `companions_revealed` at the reveal; never contended in practice.
  state: Mutex<Decoration>,
  dispatcher: Dispatcher,
}

/// The mutable state of a `CompanionOverlay`.
struct Decoration {
  companions: Companions,
  /// The source rect the companions were last drawn against.
  moved: Rect,
}

impl CompanionOverlay {
  /// Implements [`CompanionOverlay::new`].
  ///
  /// Searches first and creates nothing for a window without companions.
  /// Every companion is drawn where it stands before the overlay is
  /// shown, so the decoration is complete from its first frame.
  pub(crate) fn new(
    window: &NativeWindow,
    outer_rect: &Rect,
    dispatcher: &Dispatcher,
  ) -> crate::Result<Option<Self>> {
    /// The overlay's handle, the source's recorded rect and its drawn
    /// companions.
    type Created = Option<(isize, Rect, Companions)>;
    let source = window.inner.hwnd();
    let created =
      dispatcher.dispatch_sync(move || -> crate::Result<Created> {
        let mut companions = Companions::default();
        if !companions.search(source.0, false)
          || companions.found.is_empty()
          || is_topmost(source)
        {
          return Ok(None);
        }
        let Some(frame) = window_rect(source.0) else {
          return Ok(None);
        };
        let handle = AnimationWindow::create_window(outer_rect)?;
        companions.register(handle, None);
        companions.anchor();
        if !companions.registered.is_empty() {
          companions.update(
            source.0,
            Mapping::Anchor {
              frame: &frame,
              moved: &frame,
            },
            outer_rect,
            None,
            Instant::now(),
          );
        }
        let shown = if companions.registered.is_empty() {
          Ok(false)
        } else {
          show_above(handle, source).map(|()| true)
        };
        if let Ok(true) = shown {
          return Ok(Some((handle, frame, companions)));
        }
        for companion in &mut companions.registered {
          companion.unregister();
        }
        // SAFETY: Rolls back our new window.
        if let Err(cleanup) = unsafe { DestroyWindow(HWND(handle)) } {
          tracing::warn!("Companion overlay rollback failed: {cleanup}");
        }
        shown.map(|_| None)
      })??;

    Ok(created.map(|(handle, frame, companions)| Self {
      handle,
      source: source.0,
      frame: frame.clone(),
      outer_rect: outer_rect.clone(),
      state: Mutex::new(Decoration {
        companions,
        moved: frame,
      }),
      dispatcher: dispatcher.clone(),
    }))
  }

  /// Implements [`CompanionOverlay::retarget`].
  ///
  /// The overlay only grows, so repeated retargets rarely resize it. A
  /// resize moves the overlay's origin, so the companions are redrawn in
  /// the same hop to keep the frame they spend misplaced to one at most.
  pub(crate) fn retarget(
    &mut self,
    outer_rect: &Rect,
  ) -> crate::Result<()> {
    let handle = self.handle;
    let source = HWND(self.source);
    let resize = !self.outer_rect.contains_rect(outer_rect);
    let bounds = if resize {
      self.outer_rect.union(outer_rect)
    } else {
      self.outer_rect.clone()
    };
    let frame = &self.frame;
    let state = &self.state;
    self.dispatcher.dispatch_sync(|| {
      if resize {
        place_window(handle, &bounds)?;
        let mut state =
          state.lock().unwrap_or_else(PoisonError::into_inner);
        let Decoration { companions, moved } = &mut *state;
        companions.update(
          source.0,
          Mapping::Anchor { frame, moved },
          &bounds,
          None,
          Instant::now(),
        );
      }
      show_above(handle, source)
    })??;
    self.outer_rect = bounds;
    Ok(())
  }

  /// Implements [`CompanionOverlay::update`].
  pub(crate) fn update(&self, moved: &Rect) -> crate::Result<()> {
    self.dispatcher.dispatch_sync(|| {
      let mut state =
        self.state.lock().unwrap_or_else(PoisonError::into_inner);
      if state.companions.registered.is_empty() {
        return;
      }
      state.companions.update(
        self.source,
        Mapping::Anchor {
          frame: &self.frame,
          moved,
        },
        &self.outer_rect,
        None,
        Instant::now(),
      );
      state.moved.clone_from(moved);
    })
  }

  /// Implements [`CompanionOverlay::companions_revealed`].
  ///
  /// Reads each companion's cloak state from the compositor; callable
  /// from any thread.
  pub(crate) fn companions_revealed(&self) -> bool {
    self
      .state
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .companions
      .revealed(self.source)
  }

  /// Implements [`CompanionOverlay::destroy`].
  pub(crate) fn destroy(&mut self) -> crate::Result<()> {
    let companions = &mut self
      .state
      .get_mut()
      .unwrap_or_else(PoisonError::into_inner)
      .companions;
    if !companions.registered.is_empty() {
      let mut registered = std::mem::take(&mut companions.registered);
      self.dispatcher.dispatch_sync(move || {
        for companion in &mut registered {
          companion.unregister();
        }
      })?;
    }
    if self.handle != 0 {
      let handle = HWND(self.handle);
      self.dispatcher.dispatch_sync(move || {
        // SAFETY: Destroys our still-owned window.
        unsafe { DestroyWindow(handle) }
      })??;
      self.handle = 0;
    }
    Ok(())
  }
}

impl Drop for CompanionOverlay {
  /// Releases resources on every exit path.
  fn drop(&mut self) {
    if let Err(err) = self.destroy() {
      tracing::warn!("Companion overlay cleanup failed: {err}");
    }
  }
}

/// Reads a window's rect in screen coordinates, or `None` when the
/// window is gone.
fn window_rect(hwnd: isize) -> Option<Rect> {
  let mut native = RECT::default();
  // SAFETY: The output is a live `RECT`.
  unsafe { GetWindowRect(HWND(hwnd), &raw mut native) }.ok()?;
  Some(Rect::from_ltrb(
    native.left,
    native.top,
    native.right,
    native.bottom,
  ))
}

/// Moves and resizes one of our windows without restacking it.
fn place_window(handle: isize, rect: &Rect) -> crate::Result<()> {
  // SAFETY: Places our own live window.
  unsafe {
    SetWindowPos(
      HWND(handle),
      None,
      rect.x(),
      rect.y(),
      rect.width(),
      rect.height(),
      SWP_NOACTIVATE | SWP_NOZORDER,
    )
  }?;
  Ok(())
}

/// Whether a window sits in the topmost z-order band.
fn is_topmost(hwnd: HWND) -> bool {
  // SAFETY: Reads the style of a window; a stale handle reads as 0.
  let style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
  // LINT: The style is a bit mask; only the bit pattern matters.
  #[allow(clippy::cast_possible_wrap)]
  let topmost = WS_EX_TOPMOST.0 as isize;
  style & topmost != 0
}

/// Shows `handle` directly above `source` in the z-order.
///
/// A window is inserted below the window named as its predecessor, so the
/// overlay goes below the window that was directly above the source. It
/// is then above its own window and below every window in front of it,
/// the way a border ring is stacked. When nothing non-topmost is above
/// the source, the overlay goes to the top of the non-topmost band, which
/// is again directly above the source; it never joins the topmost band.
/// An overlay already directly above the source is only shown.
///
/// See `AnimationWindow::show_beneath` for why the overlay stays at the
/// source's own depth.
fn show_above(handle: isize, source: HWND) -> crate::Result<()> {
  // SAFETY: Reads the z-order neighbour of a window; a stale handle reads
  // as null.
  let previous = unsafe { GetWindow(source, GW_HWNDPREV) };
  let (insert_after, keep_order) = if previous.0 == handle {
    (HWND_TOP, SWP_NOZORDER)
  } else if previous.0 == 0 || is_topmost(previous) {
    (HWND_TOP, SET_WINDOW_POS_FLAGS(0))
  } else {
    (previous, SET_WINDOW_POS_FLAGS(0))
  };
  // SAFETY: Restacks our own live window; `insert_after` is a live window
  // or `HWND_TOP`.
  unsafe {
    SetWindowPos(
      HWND(handle),
      insert_after,
      0,
      0,
      0,
      0,
      SWP_NOACTIVATE
        | SWP_NOMOVE
        | SWP_NOSIZE
        | SWP_SHOWWINDOW
        | keep_order,
    )
  }?;
  Ok(())
}

impl Drop for AnimationWindow {
  /// Releases resources on every exit path.
  fn drop(&mut self) {
    if let Err(err) = self.destroy() {
      tracing::warn!("Overlay cleanup failed: {err}");
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Preserves the entire visible source frame.
  #[test]
  fn thumbnail_preserves_frame_origin() {
    let properties = AnimationWindow::thumbnail_properties(
      &Rect::from_xy(351, 64, 186, 173),
      &Rect::from_xy(55, 48, 498, 205),
      &Rect::from_xy(7, 0, 186, 173),
      None,
    );
    let source = properties.rcSource;
    let destination = properties.rcDestination;
    let client_only = properties.fSourceClientAreaOnly;
    assert_eq!(source.left, 7);
    assert_eq!(source.right, 193);
    assert_eq!(source.bottom, 173);
    assert_eq!(destination.left, 296);
    assert_eq!(destination.right, 482);
    assert_eq!(client_only, BOOL(0));
  }

  /// Crops scaled content without losing source insets.
  #[test]
  fn thumbnail_clips_offset_source() {
    let properties = AnimationWindow::thumbnail_properties(
      &Rect::from_xy(-100, -50, 200, 100),
      &Rect::from_xy(0, 0, 500, 500),
      &Rect::from_xy(7, 2, 400, 200),
      Some(&OpacityValue(0.5)),
    );
    let source = properties.rcSource;
    let destination = properties.rcDestination;
    let visible = properties.fVisible;
    assert_eq!(source.left, 207);
    assert_eq!(source.top, 102);
    assert_eq!(source.right, 407);
    assert_eq!(source.bottom, 202);
    assert_eq!(destination.left, 0);
    assert_eq!(destination.top, 0);
    assert_eq!(destination.right, 100);
    assert_eq!(destination.bottom, 50);
    assert_eq!(properties.opacity, OpacityValue(0.5).to_alpha());
    assert_eq!(visible, BOOL(1));
  }
}
