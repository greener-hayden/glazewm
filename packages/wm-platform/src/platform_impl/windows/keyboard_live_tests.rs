use std::{
  cell::RefCell,
  sync::atomic::AtomicUsize,
  time::{Duration, Instant},
};

use windows::Win32::UI::Input::KeyboardAndMouse::{
  SendInput, INPUT as NATIVE_INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT,
  KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, VIRTUAL_KEY,
};

use super::{super::*, binding, wait_until, StalledDispatcher};
use crate::Key;

const MARKER: usize = 0x4757_4d49;

/// Counts tagged events reaching the next hook.
#[derive(Default)]
struct Forwarded {
  down: AtomicUsize,
  up: AtomicUsize,
}

thread_local! {
  static OBSERVER: RefCell<Option<Arc<Forwarded>>> = const { RefCell::new(None) };
}

/// Observes forwarding without suppressing any input.
extern "system" fn observer_proc(
  code: i32,
  wparam: WPARAM,
  lparam: LPARAM,
) -> LRESULT {
  if code == 0 {
    // SAFETY: Windows supplies this callback's live payload.
    let event = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
    if event.dwExtraInfo == MARKER && event.vkCode == 0x87 {
      let _result = OBSERVER.try_with(|slot| {
        if let Some(counts) = slot.borrow().as_ref() {
          if wparam.0 == WM_KEYDOWN as usize
            || wparam.0 == WM_SYSKEYDOWN as usize
          {
            counts.down.fetch_add(1, Ordering::Release);
          } else {
            counts.up.fetch_add(1, Ordering::Release);
          }
        }
      });
    }
  }
  // SAFETY: Observers never intercept native keyboard input.
  unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// Owns the independently pumped downstream observer.
struct Observer {
  counts: Arc<Forwarded>,
  control: Arc<InputControl>,
  thread: Option<JoinHandle<()>>,
}

impl Observer {
  /// Installs before the hook under test.
  fn new() -> Self {
    let counts = Arc::new(Forwarded::default());
    let control =
      Arc::new(InputControl::new().expect("Observer controls start."));
    let observer_counts = Arc::clone(&counts);
    let observer_control = Arc::clone(&control);
    let thread = thread::spawn(move || {
      OBSERVER.with(|slot| *slot.borrow_mut() = Some(observer_counts));
      let mut message = MSG::default();
      // SAFETY: Initializes the observer's message queue.
      unsafe { PeekMessageW(&raw mut message, None, 0, 0, PM_NOREMOVE) };
      // SAFETY: Observer state and queue remain live.
      let hook = unsafe {
        install_hook(
          WH_KEYBOARD_LL.0,
          Some(observer_proc),
          HINSTANCE::default(),
          0,
        )
      };
      if hook.0 == 0 {
        observer_control.fail(FAILED);
      }
      observer_control.ready.set();
      if hook.0 != 0 {
        pump_input(&observer_control);
        // SAFETY: Removes this observer thread's own hook.
        assert!(unsafe { remove_hook(hook) }.as_bool());
      }
      OBSERVER.with(|slot| slot.borrow_mut().take());
    });
    let observer = Self {
      counts,
      control,
      thread: Some(thread),
    };
    assert!(observer.control.ready.wait());
    observer.control.result().expect("Observer hook installs.");
    observer
  }

  /// Sends tagged F24 transitions through Windows.
  fn send(&self) {
    let before = self.counts.up.load(Ordering::Acquire);
    let inputs = [false, true].map(|release| NATIVE_INPUT {
      r#type: INPUT_KEYBOARD,
      Anonymous: INPUT_0 {
        ki: KEYBDINPUT {
          wVk: VIRTUAL_KEY(0x87),
          dwFlags: if release {
            KEYEVENTF_KEYUP
          } else {
            KEYBD_EVENT_FLAGS::default()
          },
          dwExtraInfo: MARKER,
          ..Default::default()
        },
      },
    });
    // SAFETY: Injects only the tagged test key.
    assert_eq!(
      unsafe {
        SendInput(
          &inputs,
          i32::try_from(std::mem::size_of::<NATIVE_INPUT>())
            .expect("Input size fits."),
        )
      },
      2
    );
    wait_until(|| self.counts.up.load(Ordering::Acquire) > before);
  }
}

impl Drop for Observer {
  /// Removes the observer even after failed assertions.
  fn drop(&mut self) {
    self.control.stop();
    if let Some(thread) = self.thread.take() {
      thread.join().expect("Observer joins.");
    }
  }
}

/// Stops the reload worker during assertion unwinding.
struct StopPublisher(Arc<AtomicBool>);

impl Drop for StopPublisher {
  /// Signals the scoped worker before joining it.
  fn drop(&mut self) {
    self.0.store(true, Ordering::Release);
  }
}

/// Tests native delivery throughout a dispatcher stall.
fn live_stall(seconds: u64) {
  let observer = Observer::new();
  observer.send();
  assert_eq!(observer.counts.down.load(Ordering::Acquire), 1);
  let mut input =
    KeyboardInput::new(&[binding(&[Key::F24])]).expect("Input starts.");
  let control = Arc::clone(&input.control);
  let stalled = StalledDispatcher::new();
  let started = Instant::now();
  thread::scope(|scope| {
    let stop = StopPublisher(Arc::new(AtomicBool::new(false)));
    let done = Arc::clone(&stop.0);
    let publisher = Arc::clone(&control);
    scope.spawn(move || {
      while !done.load(Ordering::Acquire) {
        publisher
          .publish(Box::new(CompiledBindings::new(&[binding(&[
            Key::F24,
          ])])))
          .expect("Live reload publishes.");
        thread::sleep(Duration::from_millis(2));
      }
    });
    for _ in 0..CAPACITY + 16 {
      observer.send();
    }
    while started.elapsed() < Duration::from_secs(seconds) {
      observer.send();
      assert_eq!(observer.counts.down.load(Ordering::Acquire), 1);
      thread::sleep(Duration::from_millis(20));
    }
    drop(stop);
  });
  assert!(control.overflow.load(Ordering::Acquire) > 0);
  assert_eq!(observer.counts.down.load(Ordering::Acquire), 1);
  assert_eq!(control.installed.load(Ordering::Acquire), 1);
  input.enable(false);
  observer.send();
  assert_eq!(observer.counts.down.load(Ordering::Acquire), 2);
  input.enable(true);
  observer.send();
  assert_eq!(observer.counts.down.load(Ordering::Acquire), 2);
  input
    .terminate()
    .expect("Input stops during compositor stall.");
  assert_eq!(
    control.owner.load(Ordering::Acquire),
    control.removed.load(Ordering::Acquire)
  );
  assert!(control.tls_cleared.load(Ordering::Acquire));
  assert_eq!(control.allocations.load(Ordering::Acquire), 0);
  assert_eq!(control.deallocations.load(Ordering::Acquire), 0);
  observer.send();
  assert_eq!(observer.counts.down.load(Ordering::Acquire), 3);
  drop(stalled);
}

/// Registers explicitly opt-in native keyboard tests.
#[libtest_mimic_collect::ctor]
fn register_live() {
  for (name, seconds) in [
    ("live_keyboard_stall_2s", 2),
    ("live_keyboard_stall_10s", 10),
    ("live_keyboard_stall_60s", 60),
  ] {
    libtest_mimic_collect::TestCollection::add_test(
      libtest_mimic_collect::libtest_mimic::Trial::test(name, move || {
        live_stall(seconds);
        Ok(())
      })
      .with_ignored_flag(true),
    );
  }
}
