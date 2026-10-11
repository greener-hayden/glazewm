use crate::platform_impl::ffi;

/// Gives the calling thread the user-interactive quality-of-service
/// class.
///
/// A thread without a class inherits a default one, which the scheduler
/// may run on efficiency cores. The threads that answer input and window
/// events have to answer within milliseconds.
pub(crate) fn set_interactive_qos() -> crate::Result<()> {
  // SAFETY: Takes no pointers, and only changes the scheduling class of
  // the calling thread.
  let status = unsafe {
    ffi::pthread_set_qos_class_self_np(ffi::QOS_CLASS_USER_INTERACTIVE, 0)
  };

  if status == 0 {
    Ok(())
  } else {
    Err(crate::Error::Platform(format!(
      "Failed to set the thread's quality of service: {status}."
    )))
  }
}
