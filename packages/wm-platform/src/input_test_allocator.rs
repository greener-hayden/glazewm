use std::{
  alloc::{GlobalAlloc, Layout, System},
  cell::Cell,
};

thread_local! {
  static COUNTS: Cell<Option<(usize, usize)>> = const { Cell::new(None) };
}

struct InputAllocator;

/// Counts allocations only inside measured input scopes.
#[global_allocator]
static ALLOCATOR: InputAllocator = InputAllocator;

// SAFETY: Delegates all allocation contracts to System.
unsafe impl GlobalAlloc for InputAllocator {
  /// Records allocation without allocating bookkeeping.
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    count(true);
    // SAFETY: Forwards the unchanged allocator request.
    unsafe { System.alloc(layout) }
  }

  /// Records reclamation without allocating bookkeeping.
  unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
    count(false);
    // SAFETY: Forwards the unchanged allocation ownership.
    unsafe { System.dealloc(pointer, layout) };
  }
}

/// Records one operation using initialized TLS.
fn count(allocation: bool) {
  let _result = COUNTS.try_with(|counts| {
    if let Some((allocated, freed)) = counts.get() {
      counts.set(Some((
        allocated + usize::from(allocation),
        freed + usize::from(!allocation),
      )));
    }
  });
}

/// Starts measuring the current thread.
pub(crate) fn start() {
  COUNTS.with(|counts| counts.set(Some((0, 0))));
}

/// Ends measurement before test assertions allocate.
pub(crate) fn finish() -> (usize, usize) {
  COUNTS.with(|counts| counts.replace(None).unwrap_or_default())
}
