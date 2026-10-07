use std::{
  cell::RefCell,
  fmt::Write,
  sync::{
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    Arc, Mutex, MutexGuard, Once, PoisonError,
  },
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
  Dispatcher, FrameClock, NativeWindow, OpacityValue, Rect, WindowId,
};

/// Most companions one overlay draws.
///
/// A border ring is four bands. The bound keeps a misbehaving process
/// from making every update of one overlay arbitrarily expensive.
const MAX_COMPANIONS: usize = 8;

/// Shortest spacing between two companion failure warnings of one
/// overlay; failures in between are logged at debug level.
const WARN_INTERVAL: Duration = Duration::from_secs(1);

/// An overlay creation slower than this is logged at `INFO` on the perf
/// target. Mirrors `SLOW_CALL` in the `wm` crate's `perf` module.
const SLOW_CALL: Duration = Duration::from_millis(8);

/// Log target of the perf log. Mirrors `PERF_TARGET` in the `wm` crate's
/// `perf` module.
const PERF_TARGET: &str = "perf";

/// Platform-specific implementation of [`OnScreenWindows`].
///
/// Holds nothing on Windows; companions are found through their own
/// handles, not through a window list.
#[derive(Default)]
pub(crate) struct OnScreenWindows;

/// Locks `mutex`, ignoring poison.
///
/// Every state behind these locks is replaced whole or only read, so a
/// panic in another holder leaves nothing half-written.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
  mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What an overlay's creation tells the window manager once it is done,
/// whether it succeeded or not.
type Wake = Arc<dyn Fn() + Send + Sync + 'static>;

/// Platform-specific implementation of [`AnimationContext`].
///
/// Holds the queue that carries overlay frames to the event loop and the
/// wake that tells the window manager an overlay finished being created.
/// DWM paints the overlay from the source window's own surface, so there
/// is no device to share.
pub(crate) struct AnimationContext {
  frames: Arc<FrameQueue>,
  wake: Wake,
}

impl AnimationContext {
  /// Implements [`AnimationContext::new`].
  #[allow(clippy::unnecessary_wraps)]
  pub(crate) fn new(dispatcher: &Dispatcher) -> crate::Result<Self> {
    Self::with_wake(dispatcher, || {})
  }

  /// Implements [`AnimationContext::with_wake`].
  #[allow(clippy::unnecessary_wraps)]
  pub(crate) fn with_wake(
    _dispatcher: &Dispatcher,
    wake: impl Fn() + Send + Sync + 'static,
  ) -> crate::Result<Self> {
    let wake: Wake = Arc::new(wake);
    Ok(Self {
      frames: Arc::new(FrameQueue::new(
        AnimationWindow::apply,
        Arc::clone(&wake),
      )),
      wake,
    })
  }

  /// Implements [`AnimationContext::capture_frame`].
  ///
  /// Nothing is captured. The overlay shows the window live, so this
  /// returns at once from any thread.
  #[allow(clippy::unnecessary_wraps, clippy::unused_self)]
  pub(crate) fn capture_frame(
    &self,
    _window_id: WindowId,
    _windows: &OnScreenWindows,
  ) -> crate::Result<AnimationCapture> {
    Ok(AnimationCapture)
  }

  /// Implements [`AnimationContext::transaction`].
  ///
  /// Thumbnail updates issued within one frame are composed together by
  /// DWM, so the updates of a transaction are held back and handed to the
  /// event loop as one batch when it ends. Nothing blocks on the event
  /// loop.
  pub(crate) fn transaction<F, R>(
    &self,
    update_fn: F,
    dispatcher: &Dispatcher,
  ) -> crate::Result<R>
  where
    F: FnOnce() -> R + Send,
    R: Send,
  {
    let transaction = FrameQueue::begin(&self.frames);
    let result = update_fn();
    transaction
      .close(&|queued_fn| dispatcher.dispatch_async(queued_fn))?;
    Ok(result)
  }
}

/// How an overlay's creation stands.
#[derive(Clone)]
enum OverlayState {
  /// Creation is queued or running on the event loop.
  Creating,
  /// The overlay is on screen.
  Shown(Live),
  /// Creation failed and was rolled back, for this reason.
  Failed(String),
  /// The overlay was torn down, or never created because it was destroyed
  /// first.
  Destroyed,
}

/// What an overlay owns once created. Only the event loop thread uses the
/// handles.
#[derive(Clone)]
struct Live {
  handle: isize,
  thumbnail: isize,
  /// The source's invisible frame insets, measured once at registration.
  source_origin: (i32, i32),
  /// Frame of the window, as the event loop last placed it.
  outer_rect: Rect,
  /// The compositor frame current once everything the overlay shows was
  /// submitted. It is composed in any later frame.
  shown_frame: u64,
}

/// Where an `OverlayState` is, without what it owns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
  Creating,
  Shown,
  Gone,
}

/// The state of one overlay, shared by the window manager's thread and
/// the event loop.
///
/// The event loop thread creates, draws and destroys. The window
/// manager's thread only reads: whether the overlay is shown and at which
/// frame, a failure the event loop latched, and the companions' reveal.
/// Locks on `state` are held only to read or replace it, never across a
/// native call, so a read never waits for the event loop.
struct OverlayCell {
  /// The source window's handle.
  source: isize,
  state: Mutex<OverlayState>,
  /// Thumbnails of the source's companions. See `COMPANION`.
  ///
  /// Locked on the event loop thread during updates, and briefly by
  /// `companions_revealed` at the reveal; never contended in practice.
  companions: Mutex<Companions>,
  /// The first failure of an operation the window manager did not wait
  /// for, until it takes it.
  failure: Mutex<Option<String>>,
  /// Set when the window manager destroyed the overlay, so a creation not
  /// yet started is skipped.
  cancelled: AtomicBool,
  /// Mutations asked of the overlay so far, numbered from 1.
  requested: AtomicU64,
  /// The highest mutation the event loop has applied (or dropped, for an
  /// overlay that is gone).
  applied: AtomicU64,
  /// The compositor frame current when the event loop last applied a
  /// mutation the window manager waits on. Written before `applied`.
  applied_frame: AtomicU64,
}

impl OverlayCell {
  /// Creates the cell of an overlay for `source` that is being created.
  fn new(source: isize) -> Self {
    Self {
      source,
      state: Mutex::new(OverlayState::Creating),
      companions: Mutex::new(Companions::default()),
      failure: Mutex::new(None),
      cancelled: AtomicBool::new(false),
      requested: AtomicU64::new(0),
      applied: AtomicU64::new(0),
      applied_frame: AtomicU64::new(0),
    }
  }

  /// Numbers a new mutation of the overlay.
  fn next_seq(&self) -> u64 {
    self.requested.fetch_add(1, Ordering::SeqCst) + 1
  }

  /// Records that mutations up to `seq` are applied.
  fn mark_applied(&self, seq: u64) {
    self.applied.fetch_max(seq, Ordering::SeqCst);
  }

  /// The compositor frame at which every mutation asked of the overlay
  /// had been applied, or `None` while some are still queued.
  ///
  /// `Some(0)` when none was waited on: no frame need pass.
  fn applied_frame(&self) -> Option<u64> {
    // Read in the order the event loop writes them in reverse, so a frame
    // seen belongs to at least the mutations seen applied.
    let applied = self.applied.load(Ordering::SeqCst);
    let frame = self.applied_frame.load(Ordering::SeqCst);
    (applied >= self.requested.load(Ordering::SeqCst)).then_some(frame)
  }

  /// Where the overlay is in its life.
  fn phase(&self) -> Phase {
    match &*lock(&self.state) {
      OverlayState::Creating => Phase::Creating,
      OverlayState::Shown(_) => Phase::Shown,
      OverlayState::Failed(_) | OverlayState::Destroyed => Phase::Gone,
    }
  }

  /// What the overlay owns, if it is shown.
  fn live(&self) -> Option<Live> {
    match &*lock(&self.state) {
      OverlayState::Shown(live) => Some(live.clone()),
      _ => None,
    }
  }

  /// Replaces the state, returning the one it had.
  fn replace(&self, state: OverlayState) -> OverlayState {
    std::mem::replace(&mut *lock(&self.state), state)
  }

  /// Records where the event loop placed the window.
  fn set_outer_rect(&self, outer_rect: &Rect) {
    if let OverlayState::Shown(live) = &mut *lock(&self.state) {
      live.outer_rect.clone_from(outer_rect);
    }
  }

  /// Keeps the first failure of an operation nobody waited for.
  ///
  /// Later ones are logged at debug level only, so a failing overlay does
  /// not warn on every frame.
  fn latch(&self, err: &crate::Error) {
    let mut failure = lock(&self.failure);
    if failure.is_none() {
      tracing::warn!("Overlay operation failed: {err}");
      *failure = Some(err.to_string());
    } else {
      tracing::debug!("Overlay operation failed again: {err}");
    }
  }

  /// Ends the overlay's creation with a failure.
  fn fail(&self, err: &crate::Error) {
    self.latch(err);
    self.replace(OverlayState::Failed(err.to_string()));
  }
}

/// The latest frame requested of one overlay.
struct Frame {
  inner_rect: Rect,
  opacity: Option<OpacityValue>,
}

/// What an overlay is waiting to have applied, the latest of each kind.
struct PendingWork {
  cell: Arc<OverlayCell>,
  /// The window frame to place before drawing.
  resize: Option<Rect>,
  frame: Option<Frame>,
  /// The latest mutation of the overlay this work holds.
  seq: u64,
  /// The window manager waits on this work: the compositor frame it lands
  /// at is recorded, and the wake runs once it is applied. The frames of
  /// a running animation are not waited on.
  notify: bool,
}

impl PendingWork {
  /// Creates work for `cell`, numbering it as the newest mutation.
  fn new(
    cell: &Arc<OverlayCell>,
    resize: Option<Rect>,
    frame: Option<Frame>,
    notify: bool,
  ) -> Self {
    Self {
      cell: Arc::clone(cell),
      resize,
      frame,
      seq: cell.next_seq(),
      notify,
    }
  }

  /// Takes `newer`, which replaces what it also asks for.
  ///
  /// A resize is applied before the frame, so the frame is drawn against
  /// the window as it is meant to be, whatever order they were asked in.
  fn absorb(&mut self, newer: Self) {
    self.resize = newer.resize.or(self.resize.take());
    self.frame = newer.frame.or(self.frame.take());
    self.seq = self.seq.max(newer.seq);
    self.notify |= newer.notify;
  }
}

/// Carries overlay frames to the event loop without waiting for it.
///
/// Work is kept per overlay with the latest of each kind winning, and at
/// most one drain is queued on the event loop. An event loop that falls
/// behind therefore finds one batch of the newest frames, not a backlog of
/// stale ones. A transaction holds the drain back so the frames of one
/// tick land together.
struct FrameQueue {
  pending: Mutex<Vec<PendingWork>>,
  /// Whether a drain is queued on the event loop.
  posted: AtomicBool,
  /// Transactions that are open. No drain applies work while one is.
  open: AtomicUsize,
  /// Applies one overlay's work on the event loop.
  apply: fn(&PendingWork),
  /// Runs after a drain applied work the window manager waits on.
  wake: Wake,
}

/// Hands a closure to the event loop.
type Post<'a> =
  &'a dyn Fn(Box<dyn FnOnce() + Send + 'static>) -> crate::Result<()>;

/// Holds back a queue's drain until closed.
struct Transaction<'a> {
  queue: &'a Arc<FrameQueue>,
  closed: bool,
}

impl Transaction<'_> {
  /// Ends the transaction, queueing a drain of what it collected.
  fn close(mut self, post: Post<'_>) -> crate::Result<()> {
    self.closed = true;
    let queue = self.queue;
    queue.open.fetch_sub(1, Ordering::SeqCst);
    if queue.open.load(Ordering::SeqCst) == 0 && queue.has_pending() {
      return FrameQueue::request_drain(queue, post);
    }
    Ok(())
  }
}

impl Drop for Transaction<'_> {
  /// Releases the hold of a transaction that unwound. Its work stays
  /// pending, and the next submission queues it.
  fn drop(&mut self) {
    if !self.closed {
      self.queue.open.fetch_sub(1, Ordering::SeqCst);
    }
  }
}

impl FrameQueue {
  /// Creates a queue whose work is applied by `apply`, and which calls
  /// `wake` once work the window manager waits on is applied.
  fn new(apply: fn(&PendingWork), wake: Wake) -> Self {
    Self {
      pending: Mutex::new(Vec::new()),
      posted: AtomicBool::new(false),
      open: AtomicUsize::new(0),
      apply,
      wake,
    }
  }

  /// Whether work is waiting.
  fn has_pending(&self) -> bool {
    !lock(&self.pending).is_empty()
  }

  /// Holds the queue's drain back until the transaction is closed.
  ///
  /// Opened under the lock a drain checks it under, so a drain either
  /// runs wholly before the transaction or sees it open.
  fn begin(queue: &Arc<Self>) -> Transaction<'_> {
    {
      let _pending = lock(&queue.pending);
      queue.open.fetch_add(1, Ordering::SeqCst);
    }
    Transaction {
      queue,
      closed: false,
    }
  }

  /// Adds `work`, replacing what it supersedes, and queues a drain unless
  /// one is queued or a transaction holds it back.
  fn submit(
    queue: &Arc<Self>,
    work: PendingWork,
    post: Post<'_>,
  ) -> crate::Result<()> {
    {
      let mut pending = lock(&queue.pending);
      match pending
        .iter_mut()
        .find(|queued| Arc::ptr_eq(&queued.cell, &work.cell))
      {
        Some(queued) => queued.absorb(work),
        None => pending.push(work),
      }
    }
    if queue.open.load(Ordering::SeqCst) == 0 {
      return Self::request_drain(queue, post);
    }
    Ok(())
  }

  /// Queues a drain on the event loop unless one is already queued.
  fn request_drain(
    queue: &Arc<Self>,
    post: Post<'_>,
  ) -> crate::Result<()> {
    if queue.posted.swap(true, Ordering::SeqCst) {
      return Ok(());
    }
    let drained = Arc::clone(queue);
    post(Box::new(move || drained.drain())).inspect_err(|_| {
      queue.posted.store(false, Ordering::SeqCst);
    })
  }

  /// Runs a queued drain, on the event loop thread.
  ///
  /// Resets `posted` before taking the work, so a submission that sees a
  /// drain still queued is certain the drain has yet to take its work.
  fn drain(&self) {
    self.posted.store(false, Ordering::SeqCst);
    self.apply_pending();
  }

  /// Applies the work waiting, on the event loop thread.
  ///
  /// Does nothing while a transaction is open: its close queues a drain.
  /// Work for an overlay still being created stays, and is applied once
  /// the creation completes (see `AnimationWindow::create`). Work for one
  /// that failed or is gone is dropped. Every item taken counts as
  /// applied, so nothing waits on work that will never run.
  fn apply_pending(&self) {
    let work = {
      let mut pending = lock(&self.pending);
      // Checked under the lock a transaction opens under.
      if self.open.load(Ordering::SeqCst) > 0 {
        return;
      }
      std::mem::take(&mut *pending)
    };
    let mut waiting = Vec::new();
    let mut notify = false;
    for item in work {
      match item.cell.phase() {
        Phase::Creating => {
          waiting.push(item);
          continue;
        }
        Phase::Shown => (self.apply)(&item),
        Phase::Gone => {}
      }
      item.cell.mark_applied(item.seq);
      notify |= item.notify;
    }
    if !waiting.is_empty() {
      let mut pending = lock(&self.pending);
      for mut older in waiting {
        // Anything submitted since is newer than what waited.
        if let Some(index) = pending
          .iter()
          .position(|queued| Arc::ptr_eq(&queued.cell, &older.cell))
        {
          older.absorb(pending.swap_remove(index));
        }
        pending.push(older);
      }
    }
    if notify {
      (self.wake)();
    }
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
///
/// Nothing here waits for the event loop. Creation, drawing, resizing and
/// destruction are queued to it in order, and the window manager's thread
/// learns how they went from `shown_frame` and `take_failure`.
pub(crate) struct AnimationWindow {
  cell: Arc<OverlayCell>,
  queue: Arc<FrameQueue>,
  /// Frame of the `AnimationWindow`, as last requested.
  outer_rect: Rect,
  destroyed: bool,
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
/// and on every retarget, so a slow creation is logged with the cost of
/// each step: at `INFO` on the perf target from `SLOW_CALL` on, so it
/// reaches the perf log without `-v`, and otherwise at debug level.
struct OverlayTiming {
  started: Instant,
  /// Elapsed time at the end of each step: source origin, queue wait,
  /// window, thumbnail, show, companion search and companion drawing.
  marks: [Duration; 7],
  count: usize,
}

impl OverlayTiming {
  /// Names of the steps the marks end, then the compositor frame sample.
  ///
  /// `hop` is the time the creation sat queued behind other work on the
  /// event loop.
  const STEPS: [&'static str; 8] = [
    "origin",
    "hop",
    "create",
    "thumbnail",
    "show",
    "discover",
    "companions",
    "sample",
  ];

  /// Starts timing now.
  fn start() -> Self {
    Self {
      started: Instant::now(),
      marks: [Duration::ZERO; 7],
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
    if total >= SLOW_CALL {
      tracing::info!(
        target: PERF_TARGET,
        "overlay_new total={}us{steps}",
        total.as_micros()
      );
    } else {
      tracing::debug!(
        target: PERF_TARGET,
        "overlay_new total={}us{steps}",
        total.as_micros()
      );
    }
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

/// What the event loop needs to create an overlay.
struct Creation {
  /// The source window's handle.
  source: isize,
  /// The source's invisible frame insets, measured on the caller's
  /// thread. See `AnimationWindow::source_origin`.
  source_origin: (i32, i32),
  inner_rect: Rect,
  outer_rect: Rect,
  opacity: Option<OpacityValue>,
}

impl AnimationWindow {
  /// Implements [`AnimationWindow::new`].
  ///
  /// Measures the source here, then queues the creation to the event loop
  /// and returns without waiting for it. The overlay reports when it is
  /// shown through `shown_frame`, and the context's wake runs once it is.
  pub(crate) fn new(
    context: &AnimationContext,
    window: &NativeWindow,
    _capture: AnimationCapture,
    inner_rect: &Rect,
    outer_rect: &Rect,
    opacity: Option<OpacityValue>,
    dispatcher: &Dispatcher,
  ) -> crate::Result<Self> {
    let source_hwnd = window.inner.hwnd();
    // Measured here, on the caller's thread and once per overlay. See
    // `source_origin`.
    let mut timing = OverlayTiming::start();
    let source_origin = Self::source_origin(window)?;
    timing.mark();
    let cell = Arc::new(OverlayCell::new(source_hwnd.0));
    let queue = Arc::clone(&context.frames);
    let wake = Arc::clone(&context.wake);
    let creation = Creation {
      source: source_hwnd.0,
      source_origin,
      inner_rect: inner_rect.clone(),
      outer_rect: outer_rect.clone(),
      opacity,
    };
    let (created, drained) = (Arc::clone(&cell), Arc::clone(&queue));
    dispatcher.dispatch_async(move || {
      Self::create(&created, &creation, timing);
      // Work queued while the overlay was being created waited for it.
      drained.apply_pending();
      wake();
    })?;

    Ok(Self {
      cell,
      queue,
      outer_rect: outer_rect.clone(),
      destroyed: false,
      dispatcher: dispatcher.clone(),
    })
  }

  /// Implements [`AnimationWindow::covers`].
  pub(crate) fn covers(&self, rect: &Rect) -> bool {
    self.outer_rect.contains_rect(rect)
  }

  /// Implements [`AnimationWindow::shown_frame`].
  pub(crate) fn shown_frame(&self) -> crate::Result<Option<u64>> {
    match &*lock(&self.cell.state) {
      OverlayState::Creating => Ok(None),
      OverlayState::Shown(live) => Ok(Some(live.shown_frame)),
      OverlayState::Failed(reason) => Err(crate::Error::Platform(
        format!("Overlay creation failed: {reason}"),
      )),
      OverlayState::Destroyed => {
        Err(crate::Error::Platform("Overlay was destroyed.".to_string()))
      }
    }
  }

  /// Implements [`AnimationWindow::take_failure`].
  pub(crate) fn take_failure(&self) -> Option<crate::Error> {
    lock(&self.cell.failure).take().map(crate::Error::Platform)
  }

  /// Implements [`AnimationWindow::applied_frame`].
  pub(crate) fn applied_frame(&self) -> Option<u64> {
    self.cell.applied_frame()
  }

  /// Queues `work` for the overlay.
  fn submit(&self, work: PendingWork) -> crate::Result<()> {
    FrameQueue::submit(&self.queue, work, &|queued_fn| {
      self.dispatcher.dispatch_async(queued_fn)
    })
  }

  /// Implements [`AnimationWindow::resize`].
  ///
  /// Queued with the frames, and applied before them. The window manager
  /// waits on it: see `applied_frame`.
  pub(crate) fn resize(&mut self, outer_rect: &Rect) -> crate::Result<()> {
    self.submit(PendingWork::new(
      &self.cell,
      Some(outer_rect.clone()),
      None,
      true,
    ))?;
    self.outer_rect = outer_rect.clone();
    Ok(())
  }

  /// Implements [`AnimationWindow::update`].
  ///
  /// Queued, and drawn on the event loop. A frame replaces any that has
  /// not been drawn yet. Nothing waits on it.
  pub(crate) fn update(
    &self,
    inner_rect: &Rect,
    opacity: Option<&OpacityValue>,
  ) -> crate::Result<()> {
    if opacity.is_some() {
      tracing::debug!(
        "Overlay frame {inner_rect:?} at {:?}",
        opacity.map(OpacityValue::to_alpha)
      );
    }
    self.submit(PendingWork::new(
      &self.cell,
      None,
      Some(Frame {
        inner_rect: inner_rect.clone(),
        opacity: opacity.copied(),
      }),
      false,
    ))
  }

  /// Implements [`AnimationWindow::stop_at`].
  ///
  /// Like `update`, except that the window manager waits on it: see
  /// `applied_frame`.
  pub(crate) fn stop_at(
    &self,
    inner_rect: &Rect,
    opacity: Option<&OpacityValue>,
  ) -> crate::Result<()> {
    self.submit(PendingWork::new(
      &self.cell,
      None,
      Some(Frame {
        inner_rect: inner_rect.clone(),
        opacity: opacity.copied(),
      }),
      true,
    ))
  }

  /// Implements [`AnimationWindow::companions_revealed`].
  ///
  /// Reads each companion's cloak state from the compositor; callable
  /// from any thread. Needs no window list.
  pub(crate) fn companions_revealed(
    &self,
    _windows: &OnScreenWindows,
  ) -> bool {
    lock(&self.cell.companions).revealed(self.cell.source)
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
  ///
  /// Queued, so it runs after every draw already queued. A creation that
  /// has not started is skipped. Every step of the teardown runs even when
  /// an earlier one fails: a source destroyed mid-animation has already
  /// taken its thumbnail with it, so unregistering fails, and stopping
  /// there leaked the overlay window for good.
  pub(crate) fn destroy(&mut self) -> crate::Result<()> {
    if std::mem::replace(&mut self.destroyed, true) {
      return Ok(());
    }
    self.cell.cancelled.store(true, Ordering::SeqCst);
    let cell = Arc::clone(&self.cell);
    self
      .dispatcher
      .dispatch_async(move || Self::teardown(&cell))
  }

  /// Releases everything the overlay owns, on the event loop thread.
  fn teardown(cell: &OverlayCell) {
    let state = cell.replace(OverlayState::Destroyed);
    let mut registered =
      std::mem::take(&mut lock(&cell.companions).registered);
    for companion in &mut registered {
      companion.unregister();
    }
    if let OverlayState::Shown(live) = state {
      Self::release(live.handle, live.thumbnail);
    }
  }

  /// Unregisters a thumbnail and destroys its window. Both are attempted;
  /// failures are logged.
  fn release(handle: isize, thumbnail: isize) {
    if thumbnail != 0 {
      // SAFETY: Unregisters our still-owned thumbnail.
      if let Err(err) = unsafe { DwmUnregisterThumbnail(thumbnail) } {
        tracing::debug!("Overlay thumbnail release failed: {err}");
      }
    }
    if handle != 0 {
      // SAFETY: Destroys our still-owned window.
      if let Err(err) = unsafe { DestroyWindow(HWND(handle)) } {
        tracing::warn!("Overlay window release failed: {err}");
      }
    }
  }

  /// Applies one overlay's queued work, on the event loop thread.
  ///
  /// A failure is latched for the window manager, never returned: nothing
  /// is waiting for it.
  fn apply(work: &PendingWork) {
    Self::apply_work(work);
    if work.notify {
      // Sampled once the work is submitted: the window manager waits for a
      // later frame, which has composed all of it.
      match FrameClock::current_frame() {
        Ok(frame) => {
          work.cell.applied_frame.store(frame, Ordering::SeqCst);
        }
        Err(err) => work.cell.latch(&err),
      }
    }
  }

  /// Places and draws one overlay's work.
  fn apply_work(work: &PendingWork) {
    let cell = &work.cell;
    let Some(mut live) = cell.live() else {
      return;
    };
    if let Some(outer_rect) = &work.resize {
      if let Err(err) = place_window(live.handle, outer_rect) {
        cell.latch(&err);
        return;
      }
      cell.set_outer_rect(outer_rect);
      live.outer_rect.clone_from(outer_rect);
    }
    if let Some(frame) = &work.frame {
      if let Err(err) = Self::draw(cell, &live, frame) {
        cell.latch(&err);
      }
    }
  }

  /// Draws one frame of the source and its companions.
  fn draw(
    cell: &OverlayCell,
    live: &Live,
    frame: &Frame,
  ) -> crate::Result<()> {
    let source_rect =
      Self::source_rect(live.source_origin, live.thumbnail)?;
    let props = Self::thumbnail_properties(
      &frame.inner_rect,
      &live.outer_rect,
      &source_rect,
      frame.opacity.as_ref(),
    );
    // SAFETY: Updates our registered thumbnail.
    unsafe {
      DwmUpdateThumbnailProperties(live.thumbnail, &raw const props)
    }?;
    // Issued in the same compositor frame as the source's thumbnail, so
    // both move together.
    let mut companions = lock(&cell.companions);
    let now = Instant::now();
    if companions.registered.is_empty() {
      if !companions.throttle.try_begin(now) {
        return Ok(());
      }
      companions.discover(live.handle, cell.source, Some(now));
      if companions.registered.is_empty() {
        return Ok(());
      }
    }
    Self::update_companions(
      &mut companions,
      cell.source,
      &source_rect,
      &frame.inner_rect,
      &live.outer_rect,
      frame.opacity.as_ref(),
      now,
    );
    Ok(())
  }

  /// Creates the overlay, on the event loop thread, and records how it
  /// went.
  fn create(
    cell: &OverlayCell,
    creation: &Creation,
    mut timing: OverlayTiming,
  ) {
    timing.mark();
    if cell.cancelled.load(Ordering::SeqCst) {
      cell.replace(OverlayState::Destroyed);
      return;
    }
    match Self::build(cell, creation, &mut timing) {
      Ok(live) => {
        cell.replace(OverlayState::Shown(live));
      }
      Err(err) => cell.fail(&err),
    }
    timing.log();
  }

  /// Creates the window and registers everything it draws.
  ///
  /// Rolls back whatever it made on failure. The compositor frame is
  /// sampled last, once everything the overlay shows has been submitted:
  /// any later frame has composed all of it.
  fn build(
    cell: &OverlayCell,
    creation: &Creation,
    timing: &mut OverlayTiming,
  ) -> crate::Result<Live> {
    let source_hwnd = HWND(creation.source);
    let handle = Self::create_window(&creation.outer_rect)?;
    timing.mark();
    // SAFETY: Source and destination are live windows.
    let thumbnail =
      match unsafe { DwmRegisterThumbnail(HWND(handle), source_hwnd) } {
        Ok(thumbnail) => thumbnail,
        Err(err) => {
          Self::release(handle, 0);
          return Err(crate::Error::from(err));
        }
      };
    let mut prepare = || -> crate::Result<Rect> {
      let source_rect =
        Self::source_rect(creation.source_origin, thumbnail)?;
      let props = Self::thumbnail_properties(
        &creation.inner_rect,
        &creation.outer_rect,
        &source_rect,
        creation.opacity.as_ref(),
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
    let source_rect = match prepare() {
      Ok(source_rect) => source_rect,
      Err(err) => {
        Self::release(handle, thumbnail);
        return Err(err);
      }
    };
    timing.mark();
    // Companions never fail the overlay; see `Companions`.
    {
      let mut companions = lock(&cell.companions);
      let now = Instant::now();
      if companions.throttle.try_begin(now) {
        companions.discover(handle, creation.source, None);
      }
      timing.mark();
      if !companions.registered.is_empty() {
        Self::update_companions(
          &mut companions,
          creation.source,
          &source_rect,
          &creation.inner_rect,
          &creation.outer_rect,
          creation.opacity.as_ref(),
          now,
        );
      }
    }
    timing.mark();
    match FrameClock::current_frame() {
      Ok(shown_frame) => Ok(Live {
        handle,
        thumbnail,
        source_origin: creation.source_origin,
        outer_rect: creation.outer_rect.clone(),
        shown_frame,
      }),
      Err(err) => {
        let mut registered =
          std::mem::take(&mut lock(&cell.companions).registered);
        for companion in &mut registered {
          companion.unregister();
        }
        Self::release(handle, thumbnail);
        Err(err)
      }
    }
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

/// How a companion overlay's creation stands.
#[derive(Clone)]
enum CompanionState {
  /// Creation is queued or running on the event loop.
  Creating,
  /// The overlay is on screen.
  Shown(Placed),
  /// The source has no companions, or is topmost, so nothing was
  /// created.
  Absent,
  /// Creation failed and was rolled back, for this reason.
  Failed(String),
  /// The overlay was torn down, or never created because it was destroyed
  /// first.
  Destroyed,
}

/// What a created companion overlay owns. Only the event loop thread uses
/// the handle.
#[derive(Clone)]
struct Placed {
  handle: isize,
  /// The source's window rect when the companions' anchors were recorded.
  frame: Rect,
  /// Frame of the overlay window, as the event loop last placed it.
  outer_rect: Rect,
  /// The source rect the companions were last drawn against.
  moved: Rect,
}

/// The state of one companion overlay, shared by the window manager's
/// thread and the event loop. See `OverlayCell`.
struct CompanionCell {
  /// The source window's handle.
  source: isize,
  state: Mutex<CompanionState>,
  /// Locked on the event loop thread during updates, and briefly by
  /// `companions_revealed` at the reveal; never contended in practice.
  companions: Mutex<Companions>,
  /// The latest source rect to draw against, until it is drawn.
  moved: Mutex<Option<Rect>>,
  /// Whether a draw of `moved` is queued on the event loop.
  queued: AtomicBool,
  /// The first failure of an operation the window manager did not wait
  /// for, until it takes it.
  failure: Mutex<Option<String>>,
  /// Set when the window manager destroyed the overlay, so a creation not
  /// yet started is skipped.
  cancelled: AtomicBool,
}

impl CompanionCell {
  /// Creates the cell of an overlay for `source` that is being created.
  fn new(source: isize) -> Self {
    Self {
      source,
      state: Mutex::new(CompanionState::Creating),
      companions: Mutex::new(Companions::default()),
      moved: Mutex::new(None),
      queued: AtomicBool::new(false),
      failure: Mutex::new(None),
      cancelled: AtomicBool::new(false),
    }
  }

  /// What the overlay owns, if it is shown.
  fn placed(&self) -> Option<Placed> {
    match &*lock(&self.state) {
      CompanionState::Shown(placed) => Some(placed.clone()),
      _ => None,
    }
  }

  /// Replaces the state, returning the one it had.
  fn replace(&self, state: CompanionState) -> CompanionState {
    std::mem::replace(&mut *lock(&self.state), state)
  }

  /// Updates what the overlay owns, if it is shown.
  fn update_placed(&self, update: impl FnOnce(&mut Placed)) {
    if let CompanionState::Shown(placed) = &mut *lock(&self.state) {
      update(placed);
    }
  }

  /// Keeps the first failure of an operation nobody waited for.
  fn latch(&self, err: &crate::Error) {
    let mut failure = lock(&self.failure);
    if failure.is_none() {
      tracing::warn!("Companion overlay operation failed: {err}");
      *failure = Some(err.to_string());
    } else {
      tracing::debug!("Companion overlay operation failed again: {err}");
    }
  }
}

/// Platform-specific implementation of [`CompanionOverlay`].
///
/// A transparent popup directly above the source in the z-order, into
/// which DWM paints only the source's companions. See `COMPANION`.
///
/// As for `AnimationWindow`, nothing here waits for the event loop.
pub(crate) struct CompanionOverlay {
  cell: Arc<CompanionCell>,
  /// Frame of the overlay window, as last requested.
  outer_rect: Rect,
  destroyed: bool,
  dispatcher: Dispatcher,
}

impl CompanionOverlay {
  /// Implements [`CompanionOverlay::new`].
  ///
  /// Queues the creation and returns without waiting for it. The event
  /// loop searches first and creates nothing for a window without
  /// companions, which `status` then reports as absent. Every companion is
  /// drawn where it stands before the overlay is shown, so the decoration
  /// is complete from its first frame.
  pub(crate) fn new(
    context: &AnimationContext,
    window: &NativeWindow,
    outer_rect: &Rect,
    dispatcher: &Dispatcher,
  ) -> crate::Result<Option<Self>> {
    let source = window.inner.hwnd();
    // A topmost window's band may not be joined.
    if is_topmost(source) {
      return Ok(None);
    }
    let cell = Arc::new(CompanionCell::new(source.0));
    let wake = Arc::clone(&context.wake);
    let created = Arc::clone(&cell);
    let outer = outer_rect.clone();
    dispatcher.dispatch_async(move || {
      Self::create(&created, &outer);
      wake();
    })?;

    Ok(Some(Self {
      cell,
      outer_rect: outer_rect.clone(),
      destroyed: false,
      dispatcher: dispatcher.clone(),
    }))
  }

  /// Implements [`CompanionOverlay::status`].
  pub(crate) fn status(&self) -> crate::Result<crate::CompanionStatus> {
    match &*lock(&self.cell.state) {
      CompanionState::Creating => Ok(crate::CompanionStatus::Pending),
      CompanionState::Shown(_) => Ok(crate::CompanionStatus::Shown),
      CompanionState::Absent => Ok(crate::CompanionStatus::Absent),
      CompanionState::Failed(reason) => Err(crate::Error::Platform(
        format!("Companion overlay creation failed: {reason}"),
      )),
      CompanionState::Destroyed => Err(crate::Error::Platform(
        "Companion overlay was destroyed.".to_string(),
      )),
    }
  }

  /// Implements [`CompanionOverlay::take_failure`].
  pub(crate) fn take_failure(&self) -> Option<crate::Error> {
    lock(&self.cell.failure).take().map(crate::Error::Platform)
  }

  /// Creates the overlay, on the event loop thread, and records how it
  /// went.
  fn create(cell: &CompanionCell, outer_rect: &Rect) {
    if cell.cancelled.load(Ordering::SeqCst) {
      cell.replace(CompanionState::Destroyed);
      return;
    }
    match Self::build(cell, outer_rect) {
      Ok(state) => {
        cell.replace(state);
      }
      Err(err) => {
        cell.latch(&err);
        cell.replace(CompanionState::Failed(err.to_string()));
      }
    }
  }

  /// Searches for companions and, if there are any, draws them in a new
  /// window above the source. Rolls back on failure.
  fn build(
    cell: &CompanionCell,
    outer_rect: &Rect,
  ) -> crate::Result<CompanionState> {
    let source = HWND(cell.source);
    let mut companions = lock(&cell.companions);
    if !companions.search(source.0, false)
      || companions.found.is_empty()
      || is_topmost(source)
    {
      return Ok(CompanionState::Absent);
    }
    let Some(frame) = window_rect(source.0) else {
      return Ok(CompanionState::Absent);
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
      return Ok(CompanionState::Shown(Placed {
        handle,
        frame: frame.clone(),
        outer_rect: outer_rect.clone(),
        moved: frame,
      }));
    }
    for companion in &mut companions.registered {
      companion.unregister();
    }
    companions.registered.clear();
    // SAFETY: Rolls back our new window.
    if let Err(cleanup) = unsafe { DestroyWindow(HWND(handle)) } {
      tracing::warn!("Companion overlay rollback failed: {cleanup}");
    }
    shown.map(|_| CompanionState::Absent)
  }

  /// Implements [`CompanionOverlay::retarget`].
  ///
  /// Queued after the creation and every draw already queued. The overlay
  /// only grows, so repeated retargets rarely resize it. A resize moves
  /// the overlay's origin, so the companions are redrawn in the same step
  /// to keep the frame they spend misplaced to one at most.
  pub(crate) fn retarget(
    &mut self,
    outer_rect: &Rect,
  ) -> crate::Result<()> {
    let resize = !self.outer_rect.contains_rect(outer_rect);
    let bounds = if resize {
      self.outer_rect.union(outer_rect)
    } else {
      self.outer_rect.clone()
    };
    let cell = Arc::clone(&self.cell);
    let queued_bounds = bounds.clone();
    self.dispatcher.dispatch_async(move || {
      if let Err(err) = Self::restack(&cell, resize, &queued_bounds) {
        cell.latch(&err);
      }
    })?;
    self.outer_rect = bounds;
    Ok(())
  }

  /// Grows the overlay to `bounds` if `resize` is set, redrawing the
  /// companions, and restacks it directly above its source. On the event
  /// loop thread.
  fn restack(
    cell: &CompanionCell,
    resize: bool,
    bounds: &Rect,
  ) -> crate::Result<()> {
    let Some(placed) = cell.placed() else {
      return Ok(());
    };
    if resize {
      place_window(placed.handle, bounds)?;
      cell.update_placed(|placed| placed.outer_rect.clone_from(bounds));
      lock(&cell.companions).update(
        cell.source,
        Mapping::Anchor {
          frame: &placed.frame,
          moved: &placed.moved,
        },
        bounds,
        None,
        Instant::now(),
      );
    }
    show_above(placed.handle, HWND(cell.source))
  }

  /// Implements [`CompanionOverlay::update`].
  ///
  /// Queued, and drawn on the event loop. A rect replaces any that has not
  /// been drawn yet.
  pub(crate) fn update(&self, moved: &Rect) -> crate::Result<()> {
    *lock(&self.cell.moved) = Some(moved.clone());
    if self.cell.queued.swap(true, Ordering::SeqCst) {
      return Ok(());
    }
    let cell = Arc::clone(&self.cell);
    self
      .dispatcher
      .dispatch_async(move || Self::draw(&cell))
      .inspect_err(|_| self.cell.queued.store(false, Ordering::SeqCst))
  }

  /// Draws the companions against the latest requested rect, on the event
  /// loop thread.
  fn draw(cell: &CompanionCell) {
    // Reset before taking the rect, so a request that sees a draw still
    // queued is certain the draw has yet to take it.
    cell.queued.store(false, Ordering::SeqCst);
    let Some(moved) = lock(&cell.moved).take() else {
      return;
    };
    let Some(placed) = cell.placed() else {
      return;
    };
    let mut companions = lock(&cell.companions);
    if companions.registered.is_empty() {
      return;
    }
    companions.update(
      cell.source,
      Mapping::Anchor {
        frame: &placed.frame,
        moved: &moved,
      },
      &placed.outer_rect,
      None,
      Instant::now(),
    );
    drop(companions);
    cell.update_placed(|placed| placed.moved = moved);
  }

  /// Implements [`CompanionOverlay::companions_revealed`].
  ///
  /// Reads each companion's cloak state from the compositor; callable
  /// from any thread.
  pub(crate) fn companions_revealed(&self) -> bool {
    lock(&self.cell.companions).revealed(self.cell.source)
  }

  /// Implements [`CompanionOverlay::destroy`].
  ///
  /// Queued after everything already queued, and releases everything in
  /// one step, as `AnimationWindow::destroy` does.
  pub(crate) fn destroy(&mut self) -> crate::Result<()> {
    if std::mem::replace(&mut self.destroyed, true) {
      return Ok(());
    }
    self.cell.cancelled.store(true, Ordering::SeqCst);
    let cell = Arc::clone(&self.cell);
    self.dispatcher.dispatch_async(move || {
      let state = cell.replace(CompanionState::Destroyed);
      let mut registered =
        std::mem::take(&mut lock(&cell.companions).registered);
      for companion in &mut registered {
        companion.unregister();
      }
      if let CompanionState::Shown(placed) = state {
        // SAFETY: Destroys our still-owned window.
        if let Err(err) = unsafe { DestroyWindow(HWND(placed.handle)) } {
          tracing::warn!("Companion overlay release failed: {err}");
        }
      }
    })
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

  /// Work that was applied: the overlay (by address), the resize and the
  /// frame's rect.
  #[derive(Debug, PartialEq)]
  struct Applied {
    cell: usize,
    resize: Option<Rect>,
    frame: Option<Rect>,
  }

  thread_local! {
    /// Work applied on this thread.
    static APPLIED: RefCell<Vec<Applied>> =
      const { RefCell::new(Vec::new()) };
  }

  /// Records the work instead of drawing it.
  fn record(work: &PendingWork) {
    APPLIED.with(|applied| {
      applied.borrow_mut().push(Applied {
        cell: Arc::as_ptr(&work.cell) as usize,
        resize: work.resize.clone(),
        frame: work.frame.as_ref().map(|frame| frame.inner_rect.clone()),
      });
    });
  }

  /// Takes what was recorded since the last call.
  fn applied() -> Vec<Applied> {
    APPLIED.with(|applied| std::mem::take(&mut *applied.borrow_mut()))
  }

  /// Stands in for the event loop's queue: collects what is posted.
  #[derive(Default)]
  struct Posts(Mutex<Vec<Box<dyn FnOnce() + Send>>>);

  impl Posts {
    /// Hands a closure to the stand-in. Mirrors `Post`, which can fail.
    #[allow(clippy::unnecessary_wraps)]
    fn post(
      &self,
      queued_fn: Box<dyn FnOnce() + Send>,
    ) -> crate::Result<()> {
      lock(&self.0).push(queued_fn);
      Ok(())
    }

    /// How many closures are waiting to run.
    fn len(&self) -> usize {
      lock(&self.0).len()
    }

    /// Runs everything posted, as the event loop would.
    fn run(&self) {
      let posted = std::mem::take(&mut *lock(&self.0));
      for queued_fn in posted {
        queued_fn();
      }
    }
  }

  /// A queue that records its work, starting from a clean record: the
  /// test harness may run several tests on one thread.
  fn fresh_queue(wake: Wake) -> Arc<FrameQueue> {
    applied();
    Arc::new(FrameQueue::new(record, wake))
  }

  /// A wake that does nothing.
  fn quiet() -> Wake {
    Arc::new(|| {})
  }

  /// A cell whose overlay is shown.
  fn shown_cell() -> Arc<OverlayCell> {
    let cell = Arc::new(OverlayCell::new(1));
    cell.replace(OverlayState::Shown(Live {
      handle: 0,
      thumbnail: 0,
      source_origin: (0, 0),
      outer_rect: Rect::from_xy(0, 0, 1, 1),
      shown_frame: 7,
    }));
    cell
  }

  /// Submits a frame for `cell`.
  fn frame(
    queue: &Arc<FrameQueue>,
    posts: &Posts,
    cell: &Arc<OverlayCell>,
    x: i32,
  ) -> crate::Result<()> {
    FrameQueue::submit(
      queue,
      PendingWork::new(
        cell,
        None,
        Some(Frame {
          inner_rect: Rect::from_xy(x, 0, 10, 10),
          opacity: None,
        }),
        false,
      ),
      &|queued_fn| posts.post(queued_fn),
    )
  }

  /// However many frames are submitted, one drain is queued, and it draws
  /// only the latest frame of each overlay.
  #[test]
  fn many_frames_queue_one_drain_and_the_latest_wins() {
    let queue = fresh_queue(quiet());
    let posts = Posts::default();
    let (first, second) = (shown_cell(), shown_cell());
    for x in 0..100 {
      frame(&queue, &posts, &first, x).unwrap();
      frame(&queue, &posts, &second, 1000 + x).unwrap();
    }
    assert_eq!(posts.len(), 1);

    posts.run();
    let mut drawn = applied();
    drawn.sort_by_key(|work| work.frame.as_ref().map(Rect::x));
    assert_eq!(drawn.len(), 2);
    assert_eq!(drawn[0].frame, Some(Rect::from_xy(99, 0, 10, 10)));
    assert_eq!(drawn[1].frame, Some(Rect::from_xy(1099, 0, 10, 10)));

    // The drain is spent, so the next frame queues another.
    frame(&queue, &posts, &first, 5).unwrap();
    assert_eq!(posts.len(), 1);
    posts.run();
    assert_eq!(applied().len(), 1);
  }

  /// A resize and a frame asked for in either order are applied together,
  /// the resize first, as one piece of work.
  #[test]
  fn a_resize_is_applied_with_the_frame_that_follows_it() {
    let queue = fresh_queue(quiet());
    let posts = Posts::default();
    let cell = shown_cell();
    frame(&queue, &posts, &cell, 3).unwrap();
    for width in [20, 30] {
      FrameQueue::submit(
        &queue,
        PendingWork::new(
          &cell,
          Some(Rect::from_xy(0, 0, width, 40)),
          None,
          false,
        ),
        &|queued_fn| posts.post(queued_fn),
      )
      .unwrap();
    }
    assert_eq!(posts.len(), 1);

    posts.run();
    let drawn = applied();
    assert_eq!(drawn.len(), 1);
    assert_eq!(drawn[0].resize, Some(Rect::from_xy(0, 0, 30, 40)));
    assert_eq!(drawn[0].frame, Some(Rect::from_xy(3, 0, 10, 10)));
  }

  /// The frames of one transaction land in one drain, however the event
  /// loop interleaves with the caller.
  #[test]
  fn a_transaction_hands_over_its_frames_together() {
    let queue = fresh_queue(quiet());
    let posts = Posts::default();
    let (first, second) = (shown_cell(), shown_cell());

    // A drain was already queued when the transaction opened, and runs
    // while it is open.
    frame(&queue, &posts, &first, 1).unwrap();
    let transaction = FrameQueue::begin(&queue);
    frame(&queue, &posts, &first, 2).unwrap();
    posts.run();
    assert_eq!(
      applied(),
      Vec::new(),
      "Drew before the transaction closed."
    );

    frame(&queue, &posts, &second, 3).unwrap();
    assert_eq!(posts.len(), 0);
    transaction
      .close(&|queued_fn| posts.post(queued_fn))
      .unwrap();
    assert_eq!(posts.len(), 1);

    posts.run();
    assert_eq!(applied().len(), 2);
  }

  /// A transaction that unwound does not hold the queue back for good.
  #[test]
  fn a_dropped_transaction_releases_the_queue() {
    let queue = fresh_queue(quiet());
    let posts = Posts::default();
    let cell = shown_cell();
    drop(FrameQueue::begin(&queue));
    frame(&queue, &posts, &cell, 1).unwrap();
    assert_eq!(posts.len(), 1);
  }

  /// A transaction with nothing in it queues nothing.
  #[test]
  fn an_empty_transaction_queues_no_drain() {
    let queue = fresh_queue(quiet());
    let posts = Posts::default();
    FrameQueue::begin(&queue)
      .close(&|queued_fn| posts.post(queued_fn))
      .unwrap();
    assert_eq!(posts.len(), 0);
  }

  /// Work for an overlay still being created waits for it, and a newer
  /// request replaces what waited.
  #[test]
  fn work_waits_for_a_creating_overlay() {
    let queue = fresh_queue(quiet());
    let posts = Posts::default();
    let cell = Arc::new(OverlayCell::new(1));
    frame(&queue, &posts, &cell, 1).unwrap();
    posts.run();
    assert_eq!(applied(), Vec::new());
    assert!(queue.has_pending());

    // A newer frame arrives while it still waits.
    frame(&queue, &posts, &cell, 2).unwrap();
    posts.run();
    assert_eq!(applied(), Vec::new());

    // The creation completes, and its own drain draws the newest.
    cell.replace(OverlayState::Shown(Live {
      handle: 0,
      thumbnail: 0,
      source_origin: (0, 0),
      outer_rect: Rect::from_xy(0, 0, 1, 1),
      shown_frame: 4,
    }));
    queue.drain();
    let drawn = applied();
    assert_eq!(drawn.len(), 1);
    assert_eq!(drawn[0].frame, Some(Rect::from_xy(2, 0, 10, 10)));
    assert!(!queue.has_pending());
  }

  /// Work for an overlay that failed or is gone is dropped.
  #[test]
  fn work_for_a_dead_overlay_is_dropped() {
    let queue = fresh_queue(quiet());
    let posts = Posts::default();
    let failed = Arc::new(OverlayCell::new(1));
    failed.fail(&crate::Error::Platform("No window.".to_string()));
    let gone = shown_cell();
    gone.replace(OverlayState::Destroyed);
    frame(&queue, &posts, &failed, 1).unwrap();
    frame(&queue, &posts, &gone, 2).unwrap();
    posts.run();
    assert_eq!(applied(), Vec::new());
    assert!(!queue.has_pending());
  }

  /// A drain that could not be queued does not block the next submission
  /// from queueing one.
  #[test]
  fn a_failed_post_does_not_strand_the_queue() {
    let queue = fresh_queue(quiet());
    let posts = Posts::default();
    let cell = shown_cell();
    let refused = FrameQueue::submit(
      &queue,
      PendingWork::new(&cell, None, None, false),
      &|_| Err(crate::Error::EventLoopStopped),
    );
    assert!(refused.is_err());
    frame(&queue, &posts, &cell, 1).unwrap();
    assert_eq!(posts.len(), 1);
  }

  /// A creation failure is reported by the state, and the first failure
  /// of queued work is kept until taken.
  #[test]
  fn failures_are_latched_once_and_creation_failure_is_sticky() {
    let cell = OverlayCell::new(1);
    assert_eq!(cell.phase(), Phase::Creating);
    cell.latch(&crate::Error::Platform("First.".to_string()));
    cell.latch(&crate::Error::Platform("Second.".to_string()));
    let latched = lock(&cell.failure).take();
    assert!(latched.is_some_and(|reason| reason.contains("First")));
    assert!(lock(&cell.failure).is_none());

    cell.fail(&crate::Error::Platform("Gone.".to_string()));
    assert_eq!(cell.phase(), Phase::Gone);
    assert!(matches!(
      &*lock(&cell.state),
      OverlayState::Failed(reason) if reason.contains("Gone")
    ));
  }

  /// Submits waited-on work (a resize) for `cell`.
  fn waited_on(
    queue: &Arc<FrameQueue>,
    posts: &Posts,
    cell: &Arc<OverlayCell>,
  ) {
    FrameQueue::submit(
      queue,
      PendingWork::new(cell, Some(Rect::from_xy(0, 0, 5, 5)), None, true),
      &|queued_fn| posts.post(queued_fn),
    )
    .unwrap();
  }

  /// The window manager sees work as outstanding from the moment it is
  /// queued until the event loop has applied it.
  #[test]
  fn queued_work_is_outstanding_until_applied() {
    let queue = fresh_queue(quiet());
    let posts = Posts::default();
    let cell = shown_cell();
    assert_eq!(cell.applied_frame(), Some(0));

    waited_on(&queue, &posts, &cell);
    assert_eq!(cell.applied_frame(), None);
    // A frame queued after it keeps it outstanding too.
    frame(&queue, &posts, &cell, 1).unwrap();
    assert_eq!(cell.applied_frame(), None);

    // The platform records the frame it landed at before the queue
    // counts the work as applied.
    cell.applied_frame.store(42, Ordering::SeqCst);
    posts.run();
    assert_eq!(cell.applied_frame(), Some(42));
  }

  /// Work that waits for a creating overlay, or is dropped for a dead
  /// one, never lets the window manager wait on it forever.
  #[test]
  fn waiting_work_is_outstanding_and_dropped_work_is_not() {
    let queue = fresh_queue(quiet());
    let posts = Posts::default();
    let creating = Arc::new(OverlayCell::new(1));
    waited_on(&queue, &posts, &creating);
    posts.run();
    assert_eq!(creating.applied_frame(), None);

    let dead = Arc::new(OverlayCell::new(1));
    dead.fail(&crate::Error::Platform("No window.".to_string()));
    waited_on(&queue, &posts, &dead);
    posts.run();
    assert_eq!(dead.applied_frame(), Some(0));
  }

  /// A drain wakes the window manager only when it applied work the
  /// window manager waits on, so a running animation's frames cause no
  /// extra passes.
  #[test]
  fn a_drain_wakes_only_for_waited_on_work() {
    let woken = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&woken);
    let queue = fresh_queue(Arc::new(move || {
      counter.fetch_add(1, Ordering::SeqCst);
    }));
    let posts = Posts::default();
    let cell = shown_cell();

    frame(&queue, &posts, &cell, 1).unwrap();
    posts.run();
    assert_eq!(woken.load(Ordering::SeqCst), 0);

    waited_on(&queue, &posts, &cell);
    frame(&queue, &posts, &cell, 2).unwrap();
    posts.run();
    assert_eq!(woken.load(Ordering::SeqCst), 1);
  }

  /// A creation applies the work that waited for it directly, leaving the
  /// queued drain's own bookkeeping alone.
  #[test]
  fn creation_applies_waiting_work_without_touching_the_queued_drain() {
    let queue = fresh_queue(quiet());
    let posts = Posts::default();
    let cell = Arc::new(OverlayCell::new(1));
    frame(&queue, &posts, &cell, 1).unwrap();
    assert_eq!(posts.len(), 1);
    cell.replace(OverlayState::Shown(Live {
      handle: 0,
      thumbnail: 0,
      source_origin: (0, 0),
      outer_rect: Rect::from_xy(0, 0, 1, 1),
      shown_frame: 1,
    }));
    queue.apply_pending();
    assert_eq!(applied().len(), 1);
    // The queued drain is still counted as queued: no second one.
    frame(&queue, &posts, &cell, 2).unwrap();
    assert_eq!(posts.len(), 1);
  }
}
