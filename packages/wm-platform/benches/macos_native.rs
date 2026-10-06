//! Standalone macOS micro-benchmark for the native calls that dominate the
//! window manager's latency.
//!
//! Run with `cargo bench -p wm-platform --bench macos_native`. Needs
//! Accessibility permission for the terminal running cargo; without it the
//! accessibility sections are skipped and the rest still run. Capturing
//! another process's window also wants Screen Recording permission.
//!
//! Every window it moves, resizes or captures belongs to a helper process
//! it starts itself, and the helper is built so the running window manager
//! ignores it (see `helper`). The only thing read about other windows is
//! the window server's list metadata (owner, ID, bounds). Nothing is
//! started, stopped or replaced.
//!
//! It opens a window and writes to it, so it only runs under `cargo bench`
//! (which passes `--bench`). `cargo test --all-targets` builds it and
//! exits without measuring.
//!
//! Costs are per call, measured on the thread the window manager measures
//! them on: reads run on the event loop thread (the same as
//! `ThreadBound`), the hop is measured from a second thread.
//!
//! The pass section runs the window manager's own `PlacementSession`
//! against the helpers' windows. To find them it lists the accessibility
//! windows of every application, which only reads.
//!
//! An empty `main` on other platforms.
#![warn(clippy::all, clippy::pedantic)]

#[cfg(target_os = "macos")]
#[path = "macos_native/helper.rs"]
mod helper;

// `CGWindowListCreateImage` is deprecated, but it is what the window
// manager captures with.
#[cfg(target_os = "macos")]
#[allow(deprecated)]
mod macos {
  use std::{
    error::Error,
    panic::{self, AssertUnwindSafe},
    process::ExitCode,
    ptr::NonNull,
    sync::{
      atomic::{AtomicU64, Ordering},
      Arc,
    },
    time::{Duration, Instant},
  };

  use objc2_application_services::{
    AXCopyMultipleAttributeOptions, AXError, AXUIElement, AXValue,
    AXValueType,
  };
  use objc2_core_foundation::{
    CFArray, CFDictionary, CFNumber, CFRetained, CFString, CFType,
    CGPoint, CGRect, CGSize,
  };
  use objc2_core_graphics::{
    kCGWindowBounds, kCGWindowNumber, kCGWindowOwnerName,
    CGRectMakeWithDictionaryRepresentation, CGWindowImageOption,
    CGWindowListCopyWindowInfo, CGWindowListCreateImage,
    CGWindowListOption,
  };
  use wm_platform::{
    Dispatcher, DispatcherExtMacOs, EventLoop, NativeCallStats,
    NativeWindow, PlacementSession, Rect, ThreadBound, WindowId,
    WindowLiveness,
  };

  use crate::helper::{self, Helper};

  type BenchResult<T> = Result<T, Box<dyn Error>>;

  /// How long each resize blocks the helper, standing in for an
  /// application that lays out and draws before answering AX requests.
  const SLOW_RELAYOUT: Duration = Duration::from_millis(50);

  /// `kAXErrorCannotComplete`, which a write reports when it times out.
  const AX_CANNOT_COMPLETE: i32 = -25204;

  /// Per-element timeout the window manager gives writes it does not wait
  /// for (`DEFERRED_WRITE_TIMEOUT_SECS`).
  const DEFERRED_WRITE_TIMEOUT_SECS: f32 = 0.002;

  /// The helper window's accessibility element, bound to the event loop
  /// thread like the window manager's own elements.
  type BoundElement = ThreadBound<CFRetained<AXUIElement>>;

  /// Entry point: runs the helper or the benchmark.
  pub fn main() -> ExitCode {
    let args = std::env::args().collect::<Vec<_>>();

    if args.iter().any(|arg| arg == "--helper") {
      let slow_ms = args
        .iter()
        .position(|arg| arg == "--slow-ms")
        .and_then(|index| args.get(index + 1))
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);

      helper::run(Duration::from_millis(slow_ms));
      return ExitCode::SUCCESS;
    }

    // `cargo test --all-targets` also executes bench targets, without the
    // `--bench` flag that `cargo bench` adds.
    if !args.iter().any(|arg| arg == "--bench") {
      println!("macos_native: run with `cargo bench`; nothing measured.");
      return ExitCode::SUCCESS;
    }

    match run_benchmark() {
      Ok(()) => ExitCode::SUCCESS,
      Err(err) => {
        eprintln!("macos_native: {err}");
        ExitCode::FAILURE
      }
    }
  }

  /// Starts the event loop on this thread and the measurements on another,
  /// the same split the window manager uses.
  fn run_benchmark() -> BenchResult<()> {
    let (event_loop, dispatcher) = EventLoop::new()?;

    let worker = std::thread::spawn({
      let dispatcher = dispatcher.clone();
      move || {
        let outcome =
          panic::catch_unwind(AssertUnwindSafe(|| run_all(&dispatcher)));

        // The event loop only returns once told to stop, so stop it
        // whatever happened.
        if let Err(err) = dispatcher.stop_event_loop() {
          eprintln!("macos_native: could not stop the event loop: {err}");
          std::process::exit(1);
        }

        outcome
      }
    });

    event_loop.run()?;

    match worker.join() {
      Ok(Ok(())) => Ok(()),
      Ok(Err(panic)) | Err(panic) => panic::resume_unwind(panic),
    }
  }

  /// Runs every section, reporting the ones that cannot run.
  fn run_all(dispatcher: &Dispatcher) {
    let trusted = dispatcher.has_ax_permission(false);

    println!("macos_native: per-call costs, one sample per call.");
    println!("accessibility permission: {trusted}");
    println!();

    section("dispatch_sync hop", || {
      hop(dispatcher);
      Ok(())
    });
    section("screen walk", || screen_walk(dispatcher));

    let helper = match Helper::spawn(SLOW_RELAYOUT) {
      Ok(helper) => helper,
      Err(err) => {
        println!("helper window unavailable: {err}");
        println!("skipping the sections that need it.");
        return;
      }
    };

    println!(
      "helper pid {}, window {}, relayout {} ms",
      helper.pid,
      helper.window_id,
      SLOW_RELAYOUT.as_millis()
    );
    println!();

    section("window server list", || window_lists(&helper));
    section("window capture", || capture(&helper));

    if trusted {
      section("accessibility", || accessibility(dispatcher, &helper));
      section("window cleanup", || cleanup(dispatcher));
    } else {
      println!(
        "accessibility: skipped, the terminal running cargo lacks \
         Accessibility permission."
      );
    }
  }

  /// Runs one section and reports its failure instead of aborting.
  fn section(name: &str, run: impl FnOnce() -> BenchResult<()>) {
    println!("== {name}");

    if let Err(err) = run() {
      println!("  skipped: {err}");
    }

    println!();
  }

  /// Times `call` `iterations` times after `warmup` untimed calls.
  fn sample(
    warmup: usize,
    iterations: usize,
    mut call: impl FnMut(),
  ) -> Vec<Duration> {
    for _ in 0..warmup {
      call();
    }

    (0..iterations)
      .map(|_| {
        let start = Instant::now();
        call();
        start.elapsed()
      })
      .collect()
  }

  /// Prints the distribution of `samples`.
  fn report(label: &str, samples: &[Duration]) {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();

    let at = |percent: usize| {
      sorted
        .get((sorted.len().saturating_sub(1) * percent) / 100)
        .copied()
        .unwrap_or_default()
    };

    println!(
      "  {label:<48} n={:<5} min {:>10}  p50 {:>10}  p90 {:>10}  max {:>10}",
      sorted.len(),
      format_duration(at(0)),
      format_duration(at(50)),
      format_duration(at(90)),
      format_duration(at(100)),
    );
  }

  /// Formats a duration in the unit that reads best.
  fn format_duration(duration: Duration) -> String {
    let micros = duration.as_secs_f64() * 1e6;

    if micros >= 1000.0 {
      format!("{:.2} ms", micros / 1000.0)
    } else {
      format!("{micros:.1} µs")
    }
  }

  /// Prints what the call counter saw while `run` ran.
  fn counted<T>(run: impl FnOnce() -> T) -> T {
    let before = NativeCallStats::snapshot();
    let result = run();

    println!("  counter: {}", NativeCallStats::snapshot().since(&before));

    result
  }

  /// One `dispatch_sync` round trip, with the event loop idle and busy.
  fn hop(dispatcher: &Dispatcher) {
    counted(|| {
      report(
        "hop, event loop idle",
        &sample(200, 2000, || {
          let _ = dispatcher.dispatch_sync(|| {});
        }),
      );
    });

    // A hop queued behind a task that holds the event loop. What the
    // caller pays beyond the task is the hop itself plus the wake-up.
    let task = Duration::from_millis(2);
    let task_ns = Arc::new(AtomicU64::new(0));

    let overhead = (0..100)
      .filter_map(|_| {
        let record = task_ns.clone();
        let start = Instant::now();

        let queued = dispatcher.dispatch_async(move || {
          let began = Instant::now();
          std::thread::sleep(task);
          record.store(
            u64::try_from(began.elapsed().as_nanos()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
          );
        });
        let answered = dispatcher.dispatch_sync(|| {});

        // Skipped samples would skew the distribution toward zero.
        (queued.is_ok() && answered.is_ok()).then(|| {
          start.elapsed().saturating_sub(Duration::from_nanos(
            task_ns.load(Ordering::Relaxed),
          ))
        })
      })
      .collect::<Vec<_>>();

    report("hop beyond a 2 ms task ahead of it", &overhead);
  }

  /// One `NSScreen` walk, as `Dispatcher::displays` does it.
  ///
  /// Includes dropping the displays, which hops once each.
  fn screen_walk(dispatcher: &Dispatcher) -> BenchResult<()> {
    let displays = dispatcher.displays()?.len();
    println!("  displays: {displays}");

    counted(|| {
      report(
        "Dispatcher::displays, with drop",
        &sample(20, 200, || {
          let _ = dispatcher.displays();
        }),
      );
    });

    Ok(())
  }

  /// Copies the window server's description of one window.
  fn list_one_window(window_id: u32) -> usize {
    CGWindowListCopyWindowInfo(
      CGWindowListOption::OptionIncludingWindow,
      window_id,
    )
    .map_or(0, |windows| usize::try_from(windows.count()).unwrap_or(0))
  }

  /// Copies every on-screen window, and optionally decodes each entry the
  /// way the window manager's `on_screen_windows` does.
  fn list_all_windows(decode: bool) -> usize {
    let Some(windows) = CGWindowListCopyWindowInfo(
      CGWindowListOption::OptionOnScreenOnly,
      0,
    ) else {
      return 0;
    };

    if !decode {
      return usize::try_from(windows.count()).unwrap_or(0);
    }

    // SAFETY: Window services returns dictionaries with string keys and
    // Core Foundation values, retaining all of those objects.
    let windows = unsafe {
      windows.cast_unchecked::<CFDictionary<CFString, CFType>>()
    };

    windows
      .iter()
      .filter(|info| {
        // SAFETY: The framework provides these immutable keys.
        let (number, owner, bounds) = unsafe {
          (kCGWindowNumber, kCGWindowOwnerName, kCGWindowBounds)
        };

        let id = info
          .get(number)
          .and_then(|id| {
            id.downcast_ref::<CFNumber>().and_then(CFNumber::as_i64)
          })
          .and_then(|id| u32::try_from(id).ok());
        let owner = info.get(owner).and_then(|owner| {
          owner.downcast_ref::<CFString>().map(ToString::to_string)
        });
        let mut rect = CGRect::ZERO;
        let decoded = info
          .get(bounds)
          .and_then(|bounds| {
            bounds.downcast_ref::<CFDictionary>().map(|dictionary| {
              // SAFETY: The checked dictionary is retained and `rect` is a
              // live output for the duration of this synchronous call.
              unsafe {
                CGRectMakeWithDictionaryRepresentation(
                  Some(dictionary),
                  &raw mut rect,
                )
              }
            })
          })
          .unwrap_or(false);

        id.is_some() && owner.is_some() && decoded
      })
      .count()
  }

  /// A single-window lookup against the full `OnScreenOnly` list.
  fn window_lists(helper: &Helper) -> BenchResult<()> {
    let window_id = helper.window_id;

    // The helper opens its window before its run loop turns, so the
    // window server learns of it a moment later.
    let listed = (0..50).any(|_| {
      let listed = list_one_window(window_id) > 0;
      if !listed {
        std::thread::sleep(Duration::from_millis(100));
      }
      listed
    });

    if !listed {
      return Err(
        "the window server does not list the helper window".into(),
      );
    }

    println!("  on-screen windows: {}", list_all_windows(false));

    report(
      "one window (OptionIncludingWindow)",
      &sample(50, 1000, || {
        let _ = list_one_window(window_id);
      }),
    );
    report(
      "all on-screen windows, copy only",
      &sample(20, 300, || {
        let _ = list_all_windows(false);
      }),
    );
    report(
      "all on-screen windows, copy and decode",
      &sample(20, 300, || {
        let _ = list_all_windows(true);
      }),
    );

    Ok(())
  }

  /// One `CGWindowListCreateImage` capture of the helper window, with the
  /// options the window manager uses for slide ghosts.
  fn capture(helper: &Helper) -> BenchResult<()> {
    let window_id = helper.window_id;

    let capture_once = || {
      let null_rect = CGRect::new(
        CGPoint {
          x: f64::INFINITY,
          y: f64::INFINITY,
        },
        CGSize::ZERO,
      );

      CGWindowListCreateImage(
        null_rect,
        CGWindowListOption::OptionIncludingWindow,
        window_id,
        CGWindowImageOption::NominalResolution
          .union(CGWindowImageOption::BoundsIgnoreFraming),
      )
    };

    let Some(image) = capture_once() else {
      return Err(
        "the capture failed; the terminal may lack Screen Recording \
         permission"
          .into(),
      );
    };

    println!(
      "  image: {} x {} px",
      objc2_core_graphics::CGImage::width(Some(&image)),
      objc2_core_graphics::CGImage::height(Some(&image)),
    );
    drop(image);

    report(
      "capture, nominal resolution",
      &sample(3, 50, || {
        drop(capture_once());
      }),
    );

    Ok(())
  }

  /// Reads one attribute, or `None` if the read fails.
  fn read(el: &AXUIElement, name: &str) -> Option<CFRetained<CFType>> {
    let mut value: *const CFType = std::ptr::null();

    // SAFETY: `value` is a valid out pointer for the whole call.
    let result = unsafe {
      el.copy_attribute_value(
        &CFString::from_str(name),
        NonNull::from(&mut value),
      )
    };

    if result != AXError::Success {
      return None;
    }

    // SAFETY: On success the call returns a retained object we now own.
    NonNull::new(value.cast_mut())
      .map(|value| unsafe { CFRetained::from_raw(value) })
  }

  /// Reads several attributes in one request.
  fn read_many(el: &AXUIElement, names: &[&str]) -> Option<usize> {
    let names = names
      .iter()
      .map(|name| CFString::from_str(name))
      .collect::<Vec<_>>();
    let names = CFArray::from_retained_objects(&names);
    let mut values: *const CFArray = std::ptr::null();

    // SAFETY: `names` is an array of strings and `values` is a valid out
    // pointer for the whole call.
    let result = unsafe {
      el.copy_multiple_attribute_values(
        names.as_opaque(),
        AXCopyMultipleAttributeOptions::empty(),
        NonNull::from(&mut values),
      )
    };

    if result != AXError::Success {
      return None;
    }

    // SAFETY: On success the call returns a retained array we now own.
    NonNull::new(values.cast_mut())
      .map(|values| unsafe { CFRetained::from_raw(values) })
      .map(|values| usize::try_from(values.count()).unwrap_or(0))
  }

  /// Writes `AXSize` under the window manager's deferred-write timeout.
  ///
  /// A timeout counts as delivered, as it does for the window manager.
  fn write_size_deferred(
    el: &AXUIElement,
    width: f64,
    height: f64,
  ) -> bool {
    let size = CGSize::new(width, height);

    // SAFETY: `size` outlives the call, and `AXValueType::CGSize` matches
    // its type.
    let Some(value) = (unsafe {
      AXValue::new(AXValueType::CGSize, NonNull::from(&size).cast())
    }) else {
      return false;
    };
    let value: &CFType = value.as_ref();

    // SAFETY: `el` is a valid element and the timeout is client-side
    // state for this element alone.
    let result = unsafe {
      el.set_messaging_timeout(DEFERRED_WRITE_TIMEOUT_SECS);
      let result =
        el.set_attribute_value(&CFString::from_str("AXSize"), value);
      el.set_messaging_timeout(0.0);
      result
    };

    result == AXError::Success || result.0 == AX_CANNOT_COMPLETE
  }

  /// Finds the helper's window and binds its element to the event loop.
  fn bind_helper_window(
    dispatcher: &Dispatcher,
    pid: i32,
  ) -> BenchResult<BoundElement> {
    for _ in 0..50 {
      let element = dispatcher.dispatch_sync(|| {
        // SAFETY: Creating an application element has no preconditions.
        let app = unsafe { AXUIElement::new_application(pid) };

        let windows =
          read(&app, "AXWindows")?.downcast::<CFArray>().ok()?;

        // SAFETY: `AXWindows` is an array of window elements.
        let windows = unsafe { windows.cast_unchecked::<AXUIElement>() };
        let element = windows.iter().next()?;

        Some(ThreadBound::new(element, dispatcher.clone()))
      })?;

      if let Some(element) = element {
        return Ok(element);
      }

      std::thread::sleep(Duration::from_millis(100));
    }

    Err("the helper window is not exposed to accessibility".into())
  }

  /// Accessibility reads, and a read right after a write.
  fn accessibility(
    dispatcher: &Dispatcher,
    helper: &Helper,
  ) -> BenchResult<()> {
    let window = bind_helper_window(dispatcher, helper.pid)?;

    // Refuse to go on unless the running window manager is guaranteed to
    // ignore this window, since the writes below resize it.
    let (role, subrole) = window.with(|el| {
      let text = |name| {
        read(el, name)
          .and_then(|value| {
            value.downcast_ref::<CFString>().map(ToString::to_string)
          })
          .unwrap_or_default()
      };

      (text("AXRole"), text("AXSubrole"))
    })?;
    println!("  helper window: role {role:?}, subrole {subrole:?}");

    if role.is_empty() || subrole == "AXStandardWindow" {
      return Err(
        "the helper window could be managed by the window manager".into(),
      );
    }

    reads(dispatcher, &window)?;
    read_after_write(&window, helper.window_id)?;
    raise_after_write(&window)?;
    passes(dispatcher, helper)
  }

  /// Single reads against `AXUIElementCopyMultipleAttributeValues`.
  fn reads(
    dispatcher: &Dispatcher,
    window: &BoundElement,
  ) -> BenchResult<()> {
    const NAMES: [&str; 3] = ["AXRole", "AXMinimized", "AXPosition"];

    // Verify the reads work before timing them.
    let working = window.with(|el| {
      NAMES.iter().all(|name| read(el, name).is_some())
        && read_many(el, &NAMES) == Some(NAMES.len())
    })?;

    if !working {
      return Err("the helper window rejects attribute reads".into());
    }

    // On the event loop thread, without a hop per read: the cost of the
    // request itself.
    let on_loop = dispatcher.dispatch_sync(|| {
      let Ok(el) = window.get_ref() else {
        return Vec::new();
      };

      let mut samples = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];

      for round in 0..1200 {
        let keep = round >= 200;

        for (index, name) in NAMES.iter().enumerate() {
          let start = Instant::now();
          let _ = read(el, name);
          if keep {
            samples[index].push(start.elapsed());
          }
        }

        let start = Instant::now();
        for name in NAMES {
          let _ = read(el, name);
        }
        if keep {
          samples[3].push(start.elapsed());
        }
      }

      samples.into_iter().collect::<Vec<_>>()
    })?;

    println!("  on the event loop thread, no hop:");
    for (label, samples) in [
      "AXRole",
      "AXMinimized",
      "AXPosition",
      "the three, one request each",
    ]
    .iter()
    .zip(&on_loop)
    {
      report(label, samples);
    }
    report(
      "the three, one CopyMultiple request",
      &dispatcher.dispatch_sync(|| {
        let Ok(el) = window.get_ref() else {
          return Vec::new();
        };

        sample(200, 1000, || {
          let _ = read_many(el, &NAMES);
        })
      })?,
    );

    // From another thread, as the window manager calls them: every
    // `ThreadBound::with` is a hop.
    println!("  from another thread, through ThreadBound::with:");
    counted(|| {
      report(
        "the three, three hops",
        &sample(100, 1000, || {
          for name in NAMES {
            let _ = window.with(|el| read(el, name).is_some());
          }
        }),
      );
    });
    counted(|| {
      report(
        "the three, one hop and one CopyMultiple",
        &sample(100, 1000, || {
          let _ = window.with(|el| read_many(el, &NAMES));
        }),
      );
    });

    Ok(())
  }

  /// A read right after a resize write, against a helper whose relayout
  /// takes `SLOW_RELAYOUT`.
  fn read_after_write(
    window: &BoundElement,
    window_id: u32,
  ) -> BenchResult<()> {
    let settle = SLOW_RELAYOUT + Duration::from_millis(50);
    let rounds = 20;

    // The size to toggle from, so the window ends where it began.
    let original = window
      .with(|el| {
        read(el, "AXSize").and_then(|value| {
          let value = value.downcast::<AXValue>().ok()?;
          let mut size = CGSize::ZERO;

          // SAFETY: `size` is a live `CGSize` and the type matches.
          unsafe {
            value
              .value(AXValueType::CGSize, NonNull::from(&mut size).cast())
          }
          .then_some(size)
        })
      })?
      .ok_or("could not read the helper window's size")?;

    let mut idle = Vec::new();
    let mut write = Vec::new();
    let mut after = Vec::new();
    let mut after_server = Vec::new();

    let toggled = |round: usize| {
      if round.is_multiple_of(2) {
        (original.width + 20.0, original.height + 20.0)
      } else {
        (original.width, original.height)
      }
    };

    for round in 0..rounds {
      std::thread::sleep(settle);

      let start = Instant::now();
      let _ = window.with(|el| read(el, "AXRole").is_some())?;
      idle.push(start.elapsed());

      let (width, height) = toggled(round);

      let start = Instant::now();
      let delivered =
        window.with(|el| write_size_deferred(el, width, height))?;
      write.push(start.elapsed());

      if !delivered {
        return Err("the helper window rejects size writes".into());
      }

      let start = Instant::now();
      let _ = window.with(|el| read(el, "AXRole").is_some())?;
      after.push(start.elapsed());
    }

    for round in 0..rounds {
      std::thread::sleep(settle);

      let (width, height) = toggled(round);
      let delivered =
        window.with(|el| write_size_deferred(el, width, height))?;

      if !delivered {
        return Err("the helper window rejects size writes".into());
      }

      let start = Instant::now();
      let _ = list_one_window(window_id);
      after_server.push(start.elapsed());
    }

    // Put the helper window back where it began.
    std::thread::sleep(settle);
    let _ = window.with(|el| {
      write_size_deferred(el, original.width, original.height)
    })?;

    println!(
      "  the helper's relayout takes {} ms by construction, so the read \
       after the write shows that the read waits for it, not how long a \
       real application takes; window written {} px larger and back:",
      SLOW_RELAYOUT.as_millis(),
      20
    );
    report("AXRole read, helper idle", &idle);
    report("AXSize write, 2 ms timeout, with hop", &write);
    report("AXRole read right after the write", &after);
    report("window server read right after the write", &after_server);

    Ok(())
  }

  /// Performs `AXRaise`, waiting at most `timeout_secs` for the
  /// application when given, and for the global timeout otherwise.
  ///
  /// Returns the raw result, so a stall can be told from a refusal.
  fn raise(el: &AXUIElement, timeout_secs: Option<f32>) -> AXError {
    // SAFETY: `el` is a valid element and the timeout is client-side
    // state for this element alone. `0` returns it to the global timeout.
    unsafe {
      if let Some(timeout_secs) = timeout_secs {
        el.set_messaging_timeout(timeout_secs);
      }
      let result = el.perform_action(&CFString::from_str("AXRaise"));
      if timeout_secs.is_some() {
        el.set_messaging_timeout(0.0);
      }
      result
    }
  }

  /// `AXRaise` straight after a resize, waited for and under the deferred
  /// write timeout.
  ///
  /// This is the raise the window manager sends when it focuses a
  /// window. The helper holds a resize for `SLOW_RELAYOUT`, so a raise
  /// that waits for the application waits for that relayout; the
  /// deferred one gives up after 2 ms with the request delivered.
  fn raise_after_write(window: &BoundElement) -> BenchResult<()> {
    let settle = SLOW_RELAYOUT + Duration::from_millis(50);
    let rounds = 20;

    let original = window
      .with(|el| {
        read(el, "AXSize").and_then(|value| {
          let value = value.downcast::<AXValue>().ok()?;
          let mut size = CGSize::ZERO;

          // SAFETY: `size` is a live `CGSize` and the type matches.
          unsafe {
            value
              .value(AXValueType::CGSize, NonNull::from(&mut size).cast())
          }
          .then_some(size)
        })
      })?
      .ok_or("could not read the helper window's size")?;

    let mut idle = Vec::new();
    let mut waited = Vec::new();
    let mut deferred = Vec::new();
    let mut stalled = 0;
    let mut refused = 0;

    for round in 0..rounds * 2 {
      std::thread::sleep(settle);

      // Every round resizes: the size toggles each round, so the helper
      // always has a relayout to run. The two raises take turns in pairs,
      // so each follows both the grown and the original size.
      let defer = (round / 2) % 2 == 1;
      let grown = round % 2 == 0;
      let (width, height) = if grown {
        (original.width + 20.0, original.height + 20.0)
      } else {
        (original.width, original.height)
      };

      let start = Instant::now();
      let _ = window.with(|el| raise(el, None))?;
      idle.push(start.elapsed());

      std::thread::sleep(settle);
      let delivered =
        window.with(|el| write_size_deferred(el, width, height))?;

      if !delivered {
        return Err("the helper window rejects size writes".into());
      }

      let start = Instant::now();
      let result = window.with(|el| {
        raise(el, defer.then_some(DEFERRED_WRITE_TIMEOUT_SECS))
      })?;
      let elapsed = start.elapsed();

      if defer {
        deferred.push(elapsed);
        match result {
          AXError::Success => {}
          result if result.0 == AX_CANNOT_COMPLETE => stalled += 1,
          _ => refused += 1,
        }
      } else {
        waited.push(elapsed);
        if result != AXError::Success {
          refused += 1;
        }
      }
    }

    // Put the helper window back where it began.
    std::thread::sleep(settle);
    let _ = window.with(|el| {
      write_size_deferred(el, original.width, original.height)
    })?;

    println!(
      "  AXRaise right after a resize ({} ms relayout by construction):",
      SLOW_RELAYOUT.as_millis()
    );
    report("AXRaise, helper idle", &idle);
    report("AXRaise after the write, waited for", &waited);
    report("AXRaise after the write, 2 ms timeout", &deferred);
    println!(
      "  2 ms raises that timed out: {stalled} of {}; refused: {refused}",
      deferred.len()
    );

    Ok(())
  }

  /// How many times each cleanup round is repeated for timing.
  const CLEANUP_ROUNDS: usize = 20;

  /// Checks every window the window manager would keep, the way its
  /// periodic cleanup does, as it did and as it does now.
  ///
  /// Before: `NativeWindow::is_valid` on every window, one hop and one
  /// `AXRole` read each. After: one window server listing, and the check
  /// on the windows it lacks. Sweep: `WindowLiveness::per_window`, which
  /// the cleanup runs once a minute and which costs what before did.
  ///
  /// Reads `AXRole` of every window accessibility lists, as the window
  /// manager's own cleanup does every few seconds. Nothing is written, so
  /// no window is moved, resized or focused.
  fn cleanup(dispatcher: &Dispatcher) -> BenchResult<()> {
    let windows = dispatcher.visible_windows()?;
    println!("  windows checked: {}", windows.len());

    // Counts what one round of the check spends, taking its listing, if
    // any, inside the measured span.
    let round =
      |label: &str, liveness: &dyn Fn() -> Option<WindowLiveness>| {
        let before = NativeCallStats::snapshot();
        let liveness = liveness();
        let valid = windows
          .iter()
          .filter(|window| {
            liveness.as_ref().map_or_else(
              || window.is_valid(),
              |liveness| liveness.is_valid(window),
            )
          })
          .count();
        let spent = NativeCallStats::snapshot().since(&before);
        println!("  {label}: {valid} valid, one round: {spent}");
      };

    round("before, is_valid per window", &|| None);
    round("after, one listing", &|| {
      Some(WindowLiveness::from_window_server())
    });
    round("sweep, per_window", &|| Some(WindowLiveness::per_window()));

    report(
      "before, is_valid per window",
      &sample(2, CLEANUP_ROUNDS, || {
        for window in &windows {
          let _ = window.is_valid();
        }
      }),
    );
    report(
      "after, listing and the windows it lacks",
      &sample(2, CLEANUP_ROUNDS, || {
        let liveness = WindowLiveness::from_window_server();
        for window in &windows {
          let _ = liveness.is_valid(window);
        }
      }),
    );
    report(
      "the listing alone",
      &sample(2, CLEANUP_ROUNDS, || {
        drop(WindowLiveness::from_window_server());
      }),
    );

    Ok(())
  }

  /// How many windows a pass writes to.
  const PASS_WINDOWS: usize = 3;

  /// How much wider each write makes a window, and back.
  const PASS_GROWTH: i32 = 20;

  /// A helper window under the window manager's own placement session.
  struct PassWindow {
    session: PlacementSession,
    /// The same window, for the reads a session no longer makes in one.
    window: NativeWindow,
  }

  /// Wraps the helper window `window_id` in the window manager's own
  /// placement session.
  fn placement_session(
    dispatcher: &Dispatcher,
    window_id: u32,
  ) -> BenchResult<PassWindow> {
    // The window server lists the window a moment before accessibility
    // exposes it.
    for _ in 0..50 {
      let window = dispatcher
        .visible_windows()?
        .into_iter()
        .find(|window| window.id() == WindowId(window_id));

      if let Some(window) = window {
        return Ok(PassWindow {
          session: PlacementSession::new(window.clone(), 1)?,
          window,
        });
      }

      std::thread::sleep(Duration::from_millis(100));
    }

    Err("the helper window is not listed by accessibility".into())
  }

  /// Refuses to go on if the running window manager could manage the
  /// helper window, since the passes resize it.
  fn ensure_unmanaged(
    dispatcher: &Dispatcher,
    pid: i32,
  ) -> BenchResult<()> {
    let window = bind_helper_window(dispatcher, pid)?;
    let subrole = window.with(|el| {
      read(el, "AXSubrole")
        .and_then(|value| {
          value.downcast_ref::<CFString>().map(ToString::to_string)
        })
        .unwrap_or_default()
    })?;

    if subrole.is_empty() || subrole == "AXStandardWindow" {
      return Err(
        "a helper window could be managed by the window manager".into(),
      );
    }

    Ok(())
  }

  /// What a window's observation read before the state read became one
  /// request: the window server lookup, then `AXMinimized` and
  /// `AXFullScreen` as a request each, with no validation read.
  fn observe_separately(target: &PassWindow) -> bool {
    target.session.observed_frame().is_ok()
      && target.window.is_minimized().is_ok_and(|minimized| {
        minimized || target.window.is_maximized().is_ok()
      })
  }

  /// Which reads a pass makes.
  #[derive(Clone, Copy)]
  enum Reads {
    /// A request per state flag, and the same reads after a frame write.
    Before,
    /// One request for both flags, and the window server alone after a
    /// frame write.
    After,
  }

  impl Reads {
    /// The observation that precedes a write, and that a pass without
    /// one makes.
    fn observe(self, target: &PassWindow) -> bool {
      match self {
        Self::Before => observe_separately(target),
        Self::After => target.session.observe().is_ok(),
      }
    }

    /// The observation straight after a frame write.
    fn observe_after_write(self, target: &PassWindow) -> bool {
      match self {
        Self::Before => observe_separately(target),
        Self::After => target.session.observed_frame().is_ok(),
      }
    }
  }

  /// A write pass over several windows, then the next pass `gap` later.
  ///
  /// The first pass observes each window, writes it, then observes it
  /// again, as a reconcile pass does. The second pass only observes. The
  /// helpers relayout in parallel, so a pass that does not wait for the
  /// first window's relayout before writing the next one overlaps them.
  ///
  /// Returns the time of the first and of the second pass for each round.
  fn run_passes(
    sessions: &[PassWindow],
    sizes: &[Rect],
    display: &Rect,
    gap: Duration,
    reads: Reads,
  ) -> (Vec<Duration>, Vec<Duration>) {
    let settle = SLOW_RELAYOUT + Duration::from_millis(50);
    let mut write_pass = Vec::new();
    let mut next_pass = Vec::new();

    for round in 0..10 {
      std::thread::sleep(settle);

      let grow = if round % 2 == 0 { PASS_GROWTH } else { 0 };

      let first = Instant::now();
      for (window, size) in sessions.iter().zip(sizes) {
        let target = Rect::from_xy(
          size.left,
          size.top,
          size.width() + grow,
          size.height(),
        );

        let observed = reads.observe(window);
        let written =
          window.session.set_frame_on_display(&target, display);
        let read = reads.observe_after_write(window);

        // A failed step would make the timing meaningless.
        assert!(observed && written.is_ok() && read, "a pass step failed");
      }
      write_pass.push(first.elapsed());

      std::thread::sleep(gap);

      let second = Instant::now();
      for window in sessions {
        assert!(reads.observe(window), "a pass step failed");
      }
      next_pass.push(second.elapsed());
    }

    (write_pass, next_pass)
  }

  /// A write pass over several windows and the pass after it, with the
  /// reads after a frame write as they were and as they are now.
  ///
  /// Before: the window server lookup and one request per state flag,
  /// right after the write. After: the window server lookup alone after a
  /// frame write, and one request for both flags before it. Each helper's
  /// relayout takes `SLOW_RELAYOUT`, and the passes use the production
  /// `PlacementSession` against them.
  fn passes(dispatcher: &Dispatcher, first: &Helper) -> BenchResult<()> {
    let extra = (1..PASS_WINDOWS)
      .map(|_| Helper::spawn(SLOW_RELAYOUT))
      .collect::<Result<Vec<_>, _>>()?;
    let helpers = std::iter::once(first).chain(&extra).collect::<Vec<_>>();

    for helper in &helpers {
      ensure_unmanaged(dispatcher, helper.pid)?;
    }

    let sessions = helpers
      .iter()
      .map(|helper| placement_session(dispatcher, helper.window_id))
      .collect::<BenchResult<Vec<_>>>()?;
    let sizes = sessions
      .iter()
      .map(|window| window.session.observed_frame())
      .collect::<Result<Vec<_>, _>>()?;
    // Large enough that no write is pulled back onto a smaller display.
    let display = Rect::from_xy(-20_000, -20_000, 40_000, 40_000);

    println!(
      "  {PASS_WINDOWS} helper windows, each relayout {} ms, written \
       {PASS_GROWTH} px wider and back; time spent by the caller per \
       pass, with the next pass a gap after the write pass:",
      SLOW_RELAYOUT.as_millis()
    );

    for gap in [Duration::from_millis(16), Duration::from_millis(60)] {
      for (name, reads) in
        [("before", Reads::Before), ("after ", Reads::After)]
      {
        println!("  {name}, gap {} ms", gap.as_millis());
        let (write_pass, next_pass) =
          counted(|| run_passes(&sessions, &sizes, &display, gap, reads));
        let total = write_pass
          .iter()
          .zip(&next_pass)
          .map(|(write, next)| *write + *next)
          .collect::<Vec<_>>();

        report("write pass", &write_pass);
        report("next pass", &next_pass);
        report("both", &total);
      }
    }

    // Put the helper windows back where they began.
    std::thread::sleep(SLOW_RELAYOUT + Duration::from_millis(50));
    for (window, size) in sessions.iter().zip(&sizes) {
      let _ = window.session.set_frame_on_display(size, &display);
    }

    Ok(())
  }
}

/// Runs the benchmark, or the helper process it starts.
#[cfg(target_os = "macos")]
fn main() -> std::process::ExitCode {
  macos::main()
}

/// Nothing to measure outside macOS.
#[cfg(not(target_os = "macos"))]
fn main() {}
