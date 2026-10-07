use std::{
  cell::RefCell,
  sync::{
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    Arc, Mutex, TryLockError,
  },
  thread::{self, JoinHandle},
  time::Instant,
};

use rtrb::{Consumer, Producer, RingBuffer};
use tokio::sync::mpsc;
use windows::Win32::{
  Foundation::{
    CloseHandle, BOOL, HANDLE, HINSTANCE, LPARAM, LRESULT, WPARAM,
  },
  System::Threading::{CreateEventW, GetCurrentThreadId, INFINITE},
  UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, MsgWaitForMultipleObjectsEx,
    PeekMessageW, HHOOK, HOOKPROC, KBDLLHOOKSTRUCT, MSG,
    MWMO_INPUTAVAILABLE, PM_NOREMOVE, PM_REMOVE, QS_ALLINPUT,
    WH_KEYBOARD_LL, WM_KEYDOWN, WM_QUIT, WM_SYSKEYDOWN,
  },
};

use super::keybinding_matcher::{CompiledBindings, KeySnapshot};
use crate::{Keybinding, KeybindingEvent};

#[cfg(test)]
#[path = "keyboard_hook_tests.rs"]
mod tests;

const CAPACITY: usize = 256;
const FAILED: u32 = 31;

#[link(name = "kernel32")]
extern "system" {
  #[link_name = "GetLastError"]
  fn last_error() -> u32;
  #[link_name = "SetEvent"]
  fn signal_event(handle: HANDLE) -> BOOL;
  #[link_name = "ResetEvent"]
  fn reset_event(handle: HANDLE) -> BOOL;
  #[link_name = "WaitForSingleObject"]
  fn wait_event(handle: HANDLE, timeout: u32) -> u32;
}

#[link(name = "user32")]
extern "system" {
  #[link_name = "SetWindowsHookExW"]
  fn install_hook(
    kind: i32,
    callback: HOOKPROC,
    module: HINSTANCE,
    thread: u32,
  ) -> HHOOK;
  #[link_name = "UnhookWindowsHookEx"]
  fn remove_hook(hook: HHOOK) -> BOOL;
}

/// Owns one kernel notification event.
struct Signal(HANDLE);

impl Signal {
  /// Creates signaling storage before hook installation.
  fn new(manual: bool) -> crate::Result<Self> {
    // SAFETY: Creates an unnamed owned event.
    Ok(Self(unsafe { CreateEventW(None, manual, false, None)? }))
  }

  /// Signals without constructing allocating error objects.
  fn set(&self) -> bool {
    // SAFETY: This owner retains the handle.
    unsafe { signal_event(self.0) }.as_bool()
  }

  /// Resets before checking the publication mailbox.
  fn reset(&self) -> bool {
    // SAFETY: This owner retains the handle.
    unsafe { reset_event(self.0) }.as_bool()
  }

  /// Waits only outside keyboard delivery.
  fn wait(&self) -> bool {
    // SAFETY: This owner retains the handle.
    unsafe { wait_event(self.0, INFINITE) == 0 }
  }
}

impl Drop for Signal {
  /// Closes the final owned event handle.
  fn drop(&mut self) {
    // SAFETY: Threads have released this owner.
    if let Err(error) = unsafe { CloseHandle(self.0) } {
      tracing::error!("Input event cleanup failed: {error}");
    }
  }
}

/// Bounds publication and off-thread table retirement.
#[derive(Default)]
struct BindingMailbox {
  pending: Option<Box<CompiledBindings>>,
  retired: Option<Box<CompiledBindings>>,
}

/// Typed controls; never accepts dispatched closures.
struct InputControl {
  mailbox: Mutex<BindingMailbox>,
  enabled: AtomicBool,
  stopping: AtomicBool,
  failure: AtomicU32,
  installed: AtomicU32,
  owner: AtomicU32,
  removed: AtomicU32,
  callbacks: AtomicU64,
  suppressed: AtomicU64,
  overflow: AtomicU64,
  #[cfg(test)]
  allocations: AtomicU64,
  #[cfg(test)]
  deallocations: AtomicU64,
  #[cfg(test)]
  probe: AtomicU32,
  #[cfg(test)]
  probes: AtomicU32,
  #[cfg(test)]
  invalid_hook: AtomicBool,
  #[cfg(test)]
  tls_cleared: AtomicBool,
  ready: Signal,
  changed: Signal,
  stop: Signal,
  output: Signal,
}

impl InputControl {
  /// Allocates controls before starting either thread.
  fn new() -> crate::Result<Self> {
    Ok(Self {
      mailbox: Mutex::default(),
      enabled: AtomicBool::new(true),
      stopping: AtomicBool::new(false),
      failure: AtomicU32::new(0),
      installed: AtomicU32::new(0),
      owner: AtomicU32::new(0),
      removed: AtomicU32::new(0),
      callbacks: AtomicU64::new(0),
      suppressed: AtomicU64::new(0),
      overflow: AtomicU64::new(0),
      #[cfg(test)]
      allocations: AtomicU64::new(0),
      #[cfg(test)]
      deallocations: AtomicU64::new(0),
      #[cfg(test)]
      probe: AtomicU32::new(0),
      #[cfg(test)]
      probes: AtomicU32::new(0),
      #[cfg(test)]
      invalid_hook: AtomicBool::new(false),
      #[cfg(test)]
      tls_cleared: AtomicBool::new(false),
      ready: Signal::new(true)?,
      changed: Signal::new(true)?,
      stop: Signal::new(true)?,
      output: Signal::new(false)?,
    })
  }

  /// Records errors without formatting or logging.
  fn fail(&self, code: u32) {
    let _previous = self.failure.compare_exchange(
      0,
      code.max(1),
      Ordering::AcqRel,
      Ordering::Acquire,
    );
    self.stopping.store(true, Ordering::Release);
    self.stop.set();
    self.output.set();
  }

  /// Requests shutdown without requiring queue capacity.
  fn stop(&self) {
    self.stopping.store(true, Ordering::Release);
    if !self.stop.set() || !self.output.set() {
      self.fail(FAILED);
    }
  }

  /// Converts errors outside the input thread.
  fn result(&self) -> crate::Result<()> {
    match self.failure.load(Ordering::Acquire) {
      0 => Ok(()),
      code => Err(
        std::io::Error::from_raw_os_error(
          i32::try_from(code).unwrap_or(31),
        )
        .into(),
      ),
    }
  }

  /// Publishes complete tables; reclaims off-thread data.
  fn publish(&self, table: Box<CompiledBindings>) -> crate::Result<()> {
    if self.stopping.load(Ordering::Acquire) {
      return Err(crate::Error::EventLoopStopped);
    }
    let (pending, retired) = {
      let mut mailbox = self.mailbox.lock().map_err(|_| {
        crate::Error::Thread("Input publication failed.".into())
      })?;
      (mailbox.pending.replace(table), mailbox.retired.take())
    };
    if !self.changed.set() {
      self.fail(FAILED);
    }
    drop((pending, retired));
    self.result()
  }
}

/// A matched binding on its way from the hook to Tokio.
struct QueuedBinding {
  binding: Arc<Keybinding>,
  /// When the hook matched the key press.
  received_at: Instant,
}

/// Mutable state accessed only between native callbacks.
struct InputState {
  table: Box<CompiledBindings>,
  producer: Producer<QueuedBinding>,
  control: Arc<InputControl>,
}

impl InputState {
  /// Attempts one whole-table adoption without waiting.
  fn adopt(&mut self) {
    match self.control.mailbox.try_lock() {
      Ok(mut mailbox) => {
        if mailbox.retired.is_none() {
          if let Some(table) = mailbox.pending.take() {
            mailbox.retired =
              Some(std::mem::replace(&mut self.table, table));
          }
        }
      }
      Err(TryLockError::WouldBlock) => {}
      Err(TryLockError::Poisoned(_)) => self.control.fail(FAILED),
    }
  }

  /// Matches against an explicit key snapshot.
  ///
  /// Test entry point; the hook samples live key state through
  /// [`Self::handle_sampled`].
  #[cfg(test)]
  fn handle(
    &mut self,
    code: u16,
    keydown: bool,
    keys: &KeySnapshot,
  ) -> bool {
    self.handle_sampled(code, keydown, |_, _| keys.clone())
  }

  /// Matches locally and never waits for consumers.
  ///
  /// `sample` reads physical key state and runs only for a keydown that
  /// is enabled and has a bound trigger. Any other key is passed on
  /// without a single key-state query, since `matching` over an empty
  /// candidate list yields no binding for any snapshot.
  fn handle_sampled(
    &mut self,
    code: u16,
    keydown: bool,
    sample: impl FnOnce(&CompiledBindings, u16) -> KeySnapshot,
  ) -> bool {
    self.control.callbacks.fetch_add(1, Ordering::Relaxed);
    if !keydown
      || !self.control.enabled.load(Ordering::Acquire)
      || self.control.stopping.load(Ordering::Acquire)
      || !self.table.has_trigger(code)
    {
      return false;
    }
    let keys = sample(&self.table, code);
    let Some(binding) = self.table.matching(code, &keys) else {
      return false;
    };
    self.control.suppressed.fetch_add(1, Ordering::Relaxed);
    if self.producer.is_full() {
      self.control.overflow.fetch_add(1, Ordering::Relaxed);
    } else if self
      .producer
      .push(QueuedBinding {
        binding: Arc::clone(binding),
        received_at: Instant::now(),
      })
      .is_err()
      || !self.control.output.set()
    {
      self.control.fail(FAILED);
    }
    true
  }
}

thread_local! {
  static INPUT: RefCell<Option<InputState>> = const { RefCell::new(None) };
}

/// Forwards events without retaining mutable TLS access.
extern "system" fn keyboard_proc(
  code: i32,
  wparam: WPARAM,
  lparam: LPARAM,
) -> LRESULT {
  if code != 0 {
    // SAFETY: Forwards unchanged native hook parameters.
    return unsafe { CallNextHookEx(None, code, wparam, lparam) };
  }
  // SAFETY: Windows supplies this callback's keyboard payload.
  let event = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
  let intercepted = INPUT
    .try_with(|slot| {
      let Ok(mut slot) = slot.try_borrow_mut() else {
        return false;
      };
      let Some(input) = slot.as_mut() else {
        return false;
      };
      let Ok(key) = u16::try_from(event.vkCode) else {
        return false;
      };
      let keydown = wparam.0 == WM_KEYDOWN as usize
        || wparam.0 == WM_SYSKEYDOWN as usize;
      input.handle_sampled(key, keydown, CompiledBindings::sample)
    })
    .unwrap_or(false);
  if intercepted {
    LRESULT(1)
  } else {
    // SAFETY: No mutable TLS access remains here.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
  }
}

/// Pumps only input and typed control signals.
fn pump_input(control: &InputControl) {
  let handles = [control.stop.0, control.changed.0];
  let mut message = MSG::default();
  while !control.stopping.load(Ordering::Acquire) {
    // SAFETY: Handles and message storage remain live.
    let wake = unsafe {
      MsgWaitForMultipleObjectsEx(
        Some(&handles),
        INFINITE,
        QS_ALLINPUT,
        MWMO_INPUTAVAILABLE,
      )
    }
    .0;
    match wake {
      0 => break,
      1 => {
        if !control.changed.reset() {
          control.fail(FAILED);
          break;
        }
        INPUT.with(|slot| {
          if let Some(input) = slot.borrow_mut().as_mut() {
            input.adopt();
            #[cfg(test)]
            if let Ok(code) =
              u16::try_from(control.probe.swap(0, Ordering::AcqRel))
            {
              if code != 0 {
                let mut keys = KeySnapshot::default();
                keys.record(code, i16::MIN);
                input.handle(code, true, &keys);
                control.probes.fetch_add(1, Ordering::Release);
              }
            }
          }
        });
      }
      2 => {}
      _ => {
        control.fail(FAILED);
        break;
      }
    }
    for _ in 0..64 {
      if control.stopping.load(Ordering::Acquire) {
        break;
      }
      // SAFETY: Pumps without borrowing input state.
      if !unsafe { PeekMessageW(&raw mut message, None, 0, 0, PM_REMOVE) }
        .as_bool()
      {
        break;
      }
      if message.message == WM_QUIT {
        control.fail(FAILED);
        break;
      }
      // SAFETY: Dispatches the retrieved native message.
      unsafe { DispatchMessageW(&raw const message) };
    }
  }
}

/// Returns resources for off-thread destruction.
fn run_input(
  state: InputState,
  control: &InputControl,
) -> Option<InputState> {
  INPUT.with(|slot| *slot.borrow_mut() = Some(state));
  let mut message = MSG::default();
  // SAFETY: Initializes this thread's message queue.
  unsafe { PeekMessageW(&raw mut message, None, 0, 0, PM_NOREMOVE) };
  // SAFETY: Reads the current hook-owning thread identifier.
  control
    .owner
    .store(unsafe { GetCurrentThreadId() }, Ordering::Release);
  let kind = WH_KEYBOARD_LL.0;
  #[cfg(test)]
  let kind = if control.invalid_hook.load(Ordering::Acquire) {
    i32::MAX
  } else {
    kind
  };
  #[cfg(test)]
  crate::input_test_allocator::start();
  // SAFETY: TLS and message queue are ready.
  let hook = unsafe {
    install_hook(kind, Some(keyboard_proc), HINSTANCE::default(), 0)
  };
  if hook.0 == 0 {
    // SAFETY: Reads this thread's installation error.
    control.fail(unsafe { last_error() });
  } else {
    control.installed.fetch_add(1, Ordering::Release);
  }
  if !control.ready.set() {
    control.fail(FAILED);
  }
  if hook.0 != 0 {
    pump_input(control);
    // SAFETY: Removes this thread's own installed hook.
    if !unsafe { remove_hook(hook) }.as_bool() {
      // SAFETY: Reads this thread's removal error.
      control.fail(unsafe { last_error() });
    }
    // SAFETY: Reads the hook-removing thread identifier.
    control
      .removed
      .store(unsafe { GetCurrentThreadId() }, Ordering::Release);
  }
  #[cfg(test)]
  {
    let (allocated, freed) = crate::input_test_allocator::finish();
    control
      .allocations
      .store(allocated as u64, Ordering::Release);
    control.deallocations.store(freed as u64, Ordering::Release);
  }
  control.stop();
  let state = INPUT.with(|slot| slot.borrow_mut().take());
  #[cfg(test)]
  control.tls_cleared.store(true, Ordering::Release);
  state
}

/// Bridges into Tokio away from input delivery.
fn relay_input(
  mut consumer: Consumer<QueuedBinding>,
  sender: &mpsc::Sender<KeybindingEvent>,
  control: &InputControl,
) {
  loop {
    if control.stopping.load(Ordering::Acquire) {
      break;
    }
    if let Ok(queued) = consumer.pop() {
      if sender
        .blocking_send(KeybindingEvent::received_at(
          (*queued.binding).clone(),
          queued.received_at,
        ))
        .is_err()
      {
        if !control.stopping.load(Ordering::Acquire) {
          control.fail(FAILED);
        }
        break;
      }
    } else if !control.output.wait() {
      control.fail(FAILED);
      break;
    }
  }
}

/// Owns the Windows keyboard service lifetime.
pub(crate) struct KeyboardInput {
  control: Arc<InputControl>,
  receiver: mpsc::Receiver<KeybindingEvent>,
  input: Option<JoinHandle<Option<InputState>>>,
  relay: Option<JoinHandle<()>>,
}

impl KeyboardInput {
  /// Starts input without any window dispatcher.
  pub(crate) fn new(bindings: &[Keybinding]) -> crate::Result<Self> {
    Self::start(bindings, Arc::new(InputControl::new()?))
  }

  /// Starts threads with preallocated owned controls.
  fn start(
    bindings: &[Keybinding],
    control: Arc<InputControl>,
  ) -> crate::Result<Self> {
    let (producer, consumer) = RingBuffer::new(CAPACITY);
    let (sender, receiver) = mpsc::channel(1);
    let state = InputState {
      table: Box::new(CompiledBindings::new(bindings)),
      producer,
      control: Arc::clone(&control),
    };
    let mut service = Self {
      control,
      receiver,
      input: None,
      relay: None,
    };
    let relay_control = Arc::clone(&service.control);
    service.relay = Some(
      thread::Builder::new()
        .name("glazewm-key-relay".into())
        .spawn(move || {
          relay_input(consumer, &sender, &relay_control);
        })?,
    );
    let input_control = Arc::clone(&service.control);
    service.input = Some(
      thread::Builder::new()
        .name("glazewm-keyboard".into())
        .spawn(move || run_input(state, &input_control))?,
    );
    if !service.control.ready.wait() {
      service.control.fail(FAILED);
    }
    service.control.result()?;
    Ok(service)
  }

  /// Publishes bindings without touching the dispatcher.
  pub(crate) fn update(
    &self,
    bindings: &[Keybinding],
  ) -> crate::Result<()> {
    self
      .control
      .publish(Box::new(CompiledBindings::new(bindings)))
  }

  /// Takes overflow counts for off-thread reporting.
  pub(crate) fn take_dropped(&self) -> u64 {
    self.control.overflow.swap(0, Ordering::Relaxed)
  }

  /// Changes interception without disabling hook delivery.
  pub(crate) fn enable(&self, enabled: bool) {
    self.control.enabled.store(enabled, Ordering::Release);
  }

  /// Receives commands or reports terminal input closure.
  pub(crate) async fn next_event(&mut self) -> Option<KeybindingEvent> {
    if self.control.stopping.load(Ordering::Acquire) {
      return None;
    }
    self.receiver.recv().await
  }

  /// Stops and joins without draining queued commands.
  pub(crate) fn terminate(&mut self) -> crate::Result<()> {
    self.control.stop();
    self.receiver.close();
    if let Some(input) = self.input.take() {
      match input.join() {
        Ok(state) => drop(state),
        Err(_) => self.control.fail(FAILED),
      }
    }
    if let Some(relay) = self.relay.take() {
      if relay.join().is_err() {
        self.control.fail(FAILED);
      }
    }
    self.control.result()
  }
}

impl Drop for KeyboardInput {
  /// Joins both threads on every exit path.
  fn drop(&mut self) {
    if let Err(error) = self.terminate() {
      tracing::error!("Keyboard shutdown failed: {error}");
    }
  }
}
