use std::time::{Duration, Instant};

use super::*;
use crate::{EventLoop, Key};

#[path = "keyboard_live_tests.rs"]
mod live;

/// Waits with a bounded test deadline.
fn wait_until(mut condition: impl FnMut() -> bool) {
  let deadline = Instant::now() + Duration::from_secs(5);
  while !condition() {
    assert!(Instant::now() < deadline, "Input test timed out.");
    thread::sleep(Duration::from_millis(1));
  }
}

/// Constructs one test binding.
fn binding(keys: &[Key]) -> Keybinding {
  Keybinding::new(keys.to_vec()).expect("Valid binding.")
}

/// Submits a typed, test-only input probe.
fn probe(control: &InputControl, key: u32) {
  let before = control.probes.load(Ordering::Acquire);
  control.probe.store(key, Ordering::Release);
  assert!(control.changed.set());
  wait_until(|| control.probes.load(Ordering::Acquire) != before);
}

/// Creates input state without installing global hooks.
fn local_input(
  bindings: &[Keybinding],
  capacity: usize,
) -> (InputState, Consumer<QueuedBinding>) {
  let (producer, consumer) = RingBuffer::new(capacity);
  (
    InputState {
      table: Box::new(CompiledBindings::new(bindings)),
      producer,
      control: Arc::new(InputControl::new().expect("Controls start.")),
    },
    consumer,
  )
}

/// Preserves repeats, releases, enabling, and replacements.
#[test]
fn input_matcher_transitions() {
  let first = binding(&[Key::F24]);
  let second = binding(&[Key::F23]);
  let (mut input, mut output) =
    local_input(std::slice::from_ref(&first), 8);
  let keys = KeySnapshot::default();
  assert!(input.handle(0x87, true, &keys));
  assert!(input.handle(0x87, true, &keys));
  assert!(!input.handle(0x87, false, &keys));
  assert_eq!(*output.pop().expect("First keydown.").binding, first);
  assert_eq!(*output.pop().expect("Repeated keydown.").binding, first);
  assert!(output.pop().is_err());
  input.control.enabled.store(false, Ordering::Release);
  assert!(!input.handle(0x87, true, &keys));
  assert!(!input.handle(0x87, false, &keys));
  input.control.enabled.store(true, Ordering::Release);
  input
    .control
    .publish(Box::new(CompiledBindings::new(std::slice::from_ref(
      &second,
    ))))
    .expect("Replacement publishes.");
  assert!(input.handle(0x87, true, &keys));
  input.adopt();
  assert!(!input.handle(0x87, true, &keys));
  assert!(input.handle(0x86, true, &keys));
  assert!(!input.handle(0x86, false, &keys));
  assert_eq!(
    *output.pop().expect("Previous binding survives.").binding,
    first
  );
  assert_eq!(
    *output.pop().expect("Replacement binding fires.").binding,
    second
  );
}

/// Preserves accepted commands under bounded saturation.
#[test]
fn input_transport_saturation() {
  let first = binding(&[Key::F24]);
  let second = binding(&[Key::F23]);
  let (mut input, mut output) =
    local_input(&[first.clone(), second.clone()], 2);
  let keys = KeySnapshot::default();
  crate::input_test_allocator::start();
  let accepted = [
    input.handle(0x87, true, &keys),
    input.handle(0x86, true, &keys),
  ];
  let discarded = [
    input.handle(0x86, true, &keys),
    input.handle(0x87, true, &keys),
  ];
  let releases = [
    input.handle(0x87, false, &keys),
    input.handle(0x86, false, &keys),
  ];
  let counts = crate::input_test_allocator::finish();
  assert_eq!(accepted, [true, true]);
  assert_eq!(discarded, [true, true]);
  assert_eq!(releases, [false, false]);
  assert_eq!(counts, (0, 0));
  assert_eq!(input.control.overflow.load(Ordering::Relaxed), 2);
  assert_eq!(
    *output.pop().expect("First accepted command.").binding,
    first
  );
  assert_eq!(
    *output.pop().expect("Second accepted command.").binding,
    second
  );
  assert!(output.pop().is_err());
  assert!(input.handle(0x87, true, &keys));
  assert_eq!(*output.pop().expect("Capacity recovers.").binding, first);
}

/// Bounds buffering even when Tokio stops consuming.
#[test]
fn input_transport_relay_stall() {
  let mut input =
    KeyboardInput::new(&[binding(&[Key::F24])]).expect("Input starts.");
  let control = Arc::clone(&input.control);
  for _ in 0..CAPACITY + 16 {
    probe(&control, 0x87);
  }
  assert_eq!(input.receiver.len(), 1);
  assert!(input.take_dropped() >= 14);
  assert_eq!(input.take_dropped(), 0);
  assert_eq!(
    control.suppressed.load(Ordering::Acquire),
    (CAPACITY + 16) as u64
  );
  input.terminate().expect("Saturated input terminates.");
  assert_eq!(control.allocations.load(Ordering::Acquire), 0);
  assert_eq!(control.deallocations.load(Ordering::Acquire), 0);
}

/// Coalesces pending tables and defers all reclamation.
#[test]
fn input_publication_coalesces() {
  let first = binding(&[Key::F24]);
  let last = binding(&[Key::F23]);
  let (mut input, mut output) =
    local_input(std::slice::from_ref(&first), 8);
  let keys = KeySnapshot::default();
  let original = Arc::downgrade(
    input
      .table
      .matching(0x87, &keys)
      .expect("Original binding exists."),
  );
  assert!(input.handle(0x87, true, &keys));
  input
    .control
    .publish(Box::new(CompiledBindings::new(&[binding(&[Key::F22])])))
    .expect("Intermediate table publishes.");
  input
    .control
    .publish(Box::new(CompiledBindings::new(std::slice::from_ref(&last))))
    .expect("Latest table publishes.");
  crate::input_test_allocator::start();
  input.adopt();
  let counts = crate::input_test_allocator::finish();
  assert_eq!(counts, (0, 0));
  assert!(!input.handle(0x87, true, &keys));
  assert!(!input.handle(0x85, true, &keys));
  assert!(input.handle(0x86, true, &keys));
  input
    .control
    .publish(Box::new(CompiledBindings::new(&[])))
    .expect("Retirement is reclaimed.");
  let queued = output.pop().expect("Old command remains queued.");
  assert_eq!(*queued.binding, first);
  assert!(original.upgrade().is_some());
  drop(queued);
  assert!(original.upgrade().is_none());
  assert_eq!(
    *output.pop().expect("New command remains queued.").binding,
    last
  );
}

/// Adopts complete tables during concurrent reload floods.
#[test]
fn input_publication_concurrent() {
  let mut input = KeyboardInput::new(&[]).expect("Input starts.");
  let control = Arc::clone(&input.control);
  let publisher_control = Arc::clone(&control);
  let publisher = thread::spawn(move || {
    for index in 0..1000 {
      let key = if index % 2 == 0 { Key::F24 } else { Key::F23 };
      publisher_control
        .publish(Box::new(CompiledBindings::new(&[binding(&[key])])))
        .expect("Reload publishes.");
    }
  });
  for _ in 0..32 {
    probe(&control, 0x87);
  }
  publisher.join().expect("Publisher joins.");
  input
    .update(&[binding(&[Key::F23])])
    .expect("Final table publishes.");
  let before = control.suppressed.load(Ordering::Acquire);
  probe(&control, 0x86);
  assert_eq!(control.suppressed.load(Ordering::Acquire), before + 1);
  input.terminate().expect("Input joins.");
  assert_eq!(control.allocations.load(Ordering::Acquire), 0);
  assert_eq!(control.deallocations.load(Ordering::Acquire), 0);
}

/// Reports native installation failure and joins threads.
#[test]
fn input_lifecycle_install_failure() {
  let control = Arc::new(InputControl::new().expect("Controls start."));
  control.invalid_hook.store(true, Ordering::Release);
  assert!(KeyboardInput::start(&[], Arc::clone(&control)).is_err());
  assert_ne!(control.failure.load(Ordering::Acquire), 0);
  assert_eq!(control.installed.load(Ordering::Acquire), 0);
  assert!(control.tls_cleared.load(Ordering::Acquire));
  assert_eq!(Arc::strong_count(&control), 1);
}

/// Closes pending receives after terminal input failure.
#[test]
fn input_lifecycle_terminal_failure() {
  let mut input = KeyboardInput::new(&[]).expect("Input starts.");
  let control = Arc::clone(&input.control);
  let failure_control = Arc::clone(&control);
  let failure = thread::spawn(move || {
    thread::sleep(Duration::from_millis(20));
    failure_control.fail(FAILED);
  });
  let runtime = tokio::runtime::Runtime::new().expect("Runtime starts.");
  let event = runtime.block_on(async {
    tokio::time::timeout(Duration::from_secs(5), input.next_event())
      .await
      .expect("Terminal receive wakes.")
  });
  assert!(event.is_none());
  failure.join().expect("Failure thread joins.");
  assert!(input.terminate().is_err());
  assert!(input.input.is_none());
  assert!(input.relay.is_none());
  assert!(control.tls_cleared.load(Ordering::Acquire));
  assert_eq!(
    control.owner.load(Ordering::Acquire),
    control.removed.load(Ordering::Acquire)
  );
}

/// Repeated shutdown cannot unregister or join twice.
#[test]
fn input_lifecycle_idempotent() {
  let mut input =
    KeyboardInput::new(&[binding(&[Key::F24])]).expect("Input starts.");
  let control = Arc::clone(&input.control);
  for _ in 0..CAPACITY + 8 {
    probe(&control, 0x87);
  }
  assert!(control.overflow.load(Ordering::Acquire) > 0);
  for _ in 0..3 {
    input.terminate().expect("Shutdown succeeds.");
  }
  assert!(input.input.is_none());
  assert!(input.relay.is_none());
  assert_eq!(control.installed.load(Ordering::Acquire), 1);
  assert!(control.tls_cleared.load(Ordering::Acquire));
  assert_eq!(
    control.owner.load(Ordering::Acquire),
    control.removed.load(Ordering::Acquire)
  );
  drop(input);
  assert_eq!(Arc::strong_count(&control), 1);
}

/// Owns an independently stalled window dispatcher.
struct StalledDispatcher {
  release: Arc<Signal>,
  dispatcher: crate::Dispatcher,
  thread: Option<JoinHandle<()>>,
}

impl StalledDispatcher {
  /// Stalls only the existing window dispatcher.
  fn new() -> Self {
    let (sender, receiver) = std::sync::mpsc::channel();
    let thread = thread::spawn(move || {
      let (event_loop, dispatcher) =
        EventLoop::new().expect("Dispatcher starts.");
      sender.send(dispatcher).expect("Dispatcher receiver lives.");
      event_loop.run().expect("Dispatcher exits.");
    });
    let dispatcher = receiver
      .recv_timeout(Duration::from_secs(5))
      .expect("Dispatcher becomes ready.");
    let release =
      Arc::new(Signal::new(true).expect("Release event starts."));
    let signal = Arc::clone(&release);
    let (sender, receiver) = std::sync::mpsc::channel();
    dispatcher
      .dispatch_async(move || {
        sender.send(()).expect("Stall receiver lives.");
        assert!(signal.wait());
      })
      .expect("Stall queued.");
    receiver
      .recv_timeout(Duration::from_secs(5))
      .expect("Dispatcher stalls.");
    Self {
      release,
      dispatcher,
      thread: Some(thread),
    }
  }
}

impl Drop for StalledDispatcher {
  /// Releases owned work even after assertion failures.
  fn drop(&mut self) {
    self.release.set();
    self
      .dispatcher
      .stop_event_loop()
      .expect("Dispatcher stop succeeds.");
    if let Some(thread) = self.thread.take() {
      thread.join().expect("Dispatcher joins.");
    }
  }
}

/// Proves input ignores dispatcher and publication stalls.
#[test]
fn input_dispatch_isolation() {
  let stalled = StalledDispatcher::new();
  let mut input = KeyboardInput::new(&[binding(&[Key::F24])])
    .expect("Input starts independently.");
  let control = Arc::clone(&input.control);
  assert_ne!(control.owner.load(Ordering::Acquire), 0);
  let lock = control.mailbox.lock().expect("Publisher lock acquired.");
  for _ in 0..4 {
    probe(&control, 0x87);
  }
  drop(lock);
  input
    .update(&[binding(&[Key::F23])])
    .expect("Replacement publishes.");
  probe(&control, 0x86);
  input.terminate().expect("Input stops independently.");
  assert_eq!(control.installed.load(Ordering::Acquire), 1);
  assert_eq!(
    control.owner.load(Ordering::Acquire),
    control.removed.load(Ordering::Acquire)
  );
  assert_eq!(control.allocations.load(Ordering::Acquire), 0);
  assert_eq!(control.deallocations.load(Ordering::Acquire), 0);
  assert!(control.suppressed.load(Ordering::Acquire) >= 5);
  drop(stalled);
}
