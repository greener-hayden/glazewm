use std::{
  collections::HashMap,
  time::{Duration, Instant},
};

use wm_platform::{WindowEvent, WindowId};

/// Coalesces locations without crossing lifecycle boundaries.
#[derive(Default)]
pub struct WindowBatch {
  events: Vec<Option<WindowEvent>>,
  locations: HashMap<WindowId, usize>,
}

impl WindowBatch {
  /// Replaces only unordered location observations.
  pub fn push(&mut self, event: WindowEvent) {
    if let WindowEvent::MovedOrResized {
      window,
      is_interactive_start: false,
      is_interactive_end: false,
      ..
    } = &event
    {
      if let Some(index) =
        self.locations.insert(window.id(), self.events.len())
      {
        self.events[index] = None;
      }
    } else {
      self.locations.clear();
    }
    self.events.push(Some(event));
  }

  /// Returns retained events in original order.
  pub fn finish(self) -> impl Iterator<Item = WindowEvent> {
    self.events.into_iter().flatten()
  }
}

/// Debounces topology with a bounded deadline.
#[derive(Default)]
pub struct TopologyDebounce {
  first: Option<Instant>,
  deadline: Option<Instant>,
}

impl TopologyDebounce {
  /// Delays snapshots until notification churn settles.
  pub fn notify(&mut self, now: Instant) {
    let first = *self.first.get_or_insert(now);
    self.deadline = Some(
      (now + Duration::from_millis(150))
        .min(first + Duration::from_millis(500)),
    );
  }

  /// Exposes the existing deadline without extending it.
  pub fn deadline(&self) -> Option<Instant> {
    self.deadline
  }

  /// Consumes only an eligible snapshot request.
  pub fn take_due(&mut self, now: Instant) -> bool {
    if self.deadline.is_some_and(|deadline| now >= deadline) {
      self.first = None;
      self.deadline = None;
      true
    } else {
      false
    }
  }
}

#[cfg(test)]
mod tests {
  use wm_platform::{NativeWindow, WindowEventNotification};

  use super::*;

  /// Creates location notifications without native writes.
  fn location(start: bool, end: bool) -> WindowEvent {
    WindowEvent::MovedOrResized {
      window: NativeWindow::mock(),
      is_interactive_start: start,
      is_interactive_end: end,
      notification: WindowEventNotification(None),
    }
  }

  /// Preserves interactive and destruction boundaries.
  #[test]
  fn preserves_event_boundaries() {
    let mut batch = WindowBatch::default();
    batch.push(location(false, false));
    batch.push(location(false, false));
    batch.push(location(true, false));
    batch.push(location(false, false));
    batch.push(location(false, false));
    batch.push(location(false, true));
    batch.push(WindowEvent::Destroyed {
      window_id: WindowId(0),
      notification: WindowEventNotification(None),
    });
    batch.push(WindowEvent::Shown {
      window: NativeWindow::mock(),
      notification: WindowEventNotification(None),
    });
    let events = batch.finish().collect::<Vec<_>>();
    assert_eq!(events.len(), 6);
    assert!(matches!(
      events[1],
      WindowEvent::MovedOrResized {
        is_interactive_start: true,
        ..
      }
    ));
    assert!(matches!(
      events[3],
      WindowEvent::MovedOrResized {
        is_interactive_end: true,
        ..
      }
    ));
    assert!(matches!(events[4], WindowEvent::Destroyed { .. }));
    assert!(matches!(events[5], WindowEvent::Shown { .. }));
  }

  /// Bounds endless topology notifications without starvation.
  #[test]
  fn bounds_topology_debounce() {
    let now = Instant::now();
    let mut debounce = TopologyDebounce::default();
    for milliseconds in [0, 100, 200, 300, 400] {
      debounce.notify(now + Duration::from_millis(milliseconds));
    }
    assert_eq!(
      debounce.deadline(),
      Some(now + Duration::from_millis(500))
    );
    assert!(!debounce.take_due(now + Duration::from_millis(499)));
    assert!(debounce.take_due(now + Duration::from_millis(500)));
    assert_eq!(debounce.deadline(), None);
  }
}
