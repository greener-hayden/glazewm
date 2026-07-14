use objc2_core_foundation::CFMachPort;
use objc2_core_graphics::{CGEvent, CGEventType};

/// Re-enables a `CGEventTap` if `event_type` is a tap-disabled
/// notification.
///
/// macOS disables an event tap when its callback is too slow to respond
/// (`kCGEventTapDisabledByTimeout`) or when a user physically disables it
/// (`kCGEventTapDisabledByUserInput`). A disabled tap stops receiving
/// events until it is explicitly re-enabled, so callbacks must handle
/// these notifications to recover.
///
/// Returns `true` if the event was a tap-disabled notification (and was
/// handled), in which case the caller should return the event immediately
/// without further processing.
pub(super) fn reenable_if_tap_disabled(
  event_type: CGEventType,
  tap_port: *const CFMachPort,
  label: &str,
) -> bool {
  if event_type != CGEventType::TapDisabledByTimeout
    && event_type != CGEventType::TapDisabledByUserInput
  {
    return false;
  }

  tracing::warn!(
    "{} event tap disabled ({:?}); re-enabling.",
    label,
    event_type
  );

  if let Some(port) = std::ptr::NonNull::new(tap_port.cast_mut()) {
    // SAFETY: `port` refers to the live `CFMachPort` owned by the tap's
    // listener struct. This runs on the run-loop thread that created the
    // tap, and the tap is invalidated (stopping callbacks) before the
    // port is released.
    CGEvent::tap_enable(unsafe { port.as_ref() }, true);
  } else {
    tracing::error!(
      "{} tap-disabled event received but no port handle to re-enable.",
      label
    );
  }

  true
}

#[cfg(test)]
mod tests {
  use objc2_core_graphics::CGEventType;

  /// Guards against `objc2-core-graphics` version drift: the tap-disabled
  /// event types must match the values documented by Apple.
  #[test]
  fn tap_disabled_event_type_values() {
    assert_eq!(CGEventType::TapDisabledByTimeout.0, 0xFFFF_FFFE);
    assert_eq!(CGEventType::TapDisabledByUserInput.0, 0xFFFF_FFFF);
  }
}
