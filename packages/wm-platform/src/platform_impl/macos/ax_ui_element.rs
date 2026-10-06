use std::ptr::{self, NonNull};

use objc2_application_services::{
  AXCopyMultipleAttributeOptions, AXValue, AXValueType,
};
pub use objc2_application_services::{AXError, AXUIElement};
use objc2_core_foundation::{CFArray, CFRetained, CFString, CFType};

use crate::{Error, NativeCall, NativeCallStats};

/// Extension trait for [`AXUIElement`].
pub trait AXUIElementExt {
  /// Retrieves the value of an accessibility attribute.
  ///
  /// # Errors
  ///
  /// Returns an error if:
  /// - The accessibility operation fails (e.g. invalid attribute name).
  /// - The attribute value cannot be cast to the requested type.
  fn get_attribute<T: objc2_core_foundation::Type>(
    &self,
    attribute: &str,
  ) -> crate::Result<CFRetained<T>>;

  /// Retrieves several accessibility attributes in a single request.
  ///
  /// Returns one result per name in `attributes`, in order: an attribute
  /// the element cannot answer fails alone, as it would from
  /// [`Self::get_attribute`]. A busy application answers (or stalls) once
  /// instead of once per attribute.
  ///
  /// # Errors
  ///
  /// Returns an error if the request as a whole fails (e.g. the element
  /// is invalid or does not answer in time).
  fn get_attributes(
    &self,
    attributes: &[&str],
  ) -> crate::Result<Vec<crate::Result<CFRetained<CFType>>>>;

  /// Sets the value of an accessibility attribute.
  ///
  /// # Errors
  ///
  /// Returns an error if the accessibility operation fails.
  fn set_attribute<T: objc2_core_foundation::Type + AsRef<CFType>>(
    &self,
    attribute: &str,
    value: &CFRetained<T>,
  ) -> crate::Result<()>;
}

impl AXUIElementExt for AXUIElement {
  fn get_attribute<T: objc2_core_foundation::Type>(
    &self,
    attribute: &str,
  ) -> crate::Result<CFRetained<T>> {
    NativeCallStats::record(NativeCall::AxRead);
    let mut value: *const CFType = ptr::null();

    let result = unsafe {
      self.copy_attribute_value(
        &CFString::from_str(attribute),
        // SAFETY: Stack address of `value` is guaranteed to be
        // non-null.
        NonNull::new(&raw mut value).unwrap(),
      )
    };

    if result != AXError::Success {
      return Err(Error::Accessibility(attribute.to_string(), result.0));
    }

    NonNull::new(value.cast_mut())
      .map(|ptr| unsafe { CFRetained::from_raw(ptr.cast()) })
      .ok_or_else(|| {
        Error::InvalidPointer(
          "copy_attribute_value returned success but null pointer"
            .to_string(),
        )
      })
  }

  fn get_attributes(
    &self,
    attributes: &[&str],
  ) -> crate::Result<Vec<crate::Result<CFRetained<CFType>>>> {
    // One request, however many attributes it carries.
    NativeCallStats::record(NativeCall::AxRead);
    let names = attributes
      .iter()
      .map(|attribute| CFString::from_str(attribute))
      .collect::<Vec<_>>();
    let names = CFArray::from_retained_objects(&names);
    let mut values: *const CFArray = ptr::null();

    // SAFETY: `names` is an array of strings and `values` is a valid out
    // pointer for the whole call. Without `StopOnError`, an attribute
    // that cannot be read yields an `AXError` value in its slot.
    let result = unsafe {
      self.copy_multiple_attribute_values(
        names.as_opaque(),
        AXCopyMultipleAttributeOptions::empty(),
        NonNull::from(&mut values),
      )
    };

    if result != AXError::Success {
      return Err(Error::Accessibility(attributes.join(","), result.0));
    }

    let values = NonNull::new(values.cast_mut())
      // SAFETY: On success the call returns a retained array we now own.
      .map(|values| unsafe { CFRetained::from_raw(values) })
      .ok_or_else(|| {
        Error::InvalidPointer(
          "copy_multiple_attribute_values returned success but null \
           pointer"
            .to_string(),
        )
      })?;
    // SAFETY: The array holds Core Foundation values, retained by it.
    let values = unsafe { values.cast_unchecked::<CFType>() };

    Ok(
      attributes
        .iter()
        .enumerate()
        .map(|(index, attribute)| {
          let value = values.get(index).ok_or_else(|| {
            Error::InvalidPointer(format!(
              "No value returned for {attribute}."
            ))
          })?;
          match attribute_error(&value) {
            Some(code) => {
              Err(Error::Accessibility((*attribute).to_string(), code))
            }
            None => Ok(value),
          }
        })
        .collect(),
    )
  }

  fn set_attribute<T: objc2_core_foundation::Type + AsRef<CFType>>(
    &self,
    attribute: &str,
    value: &CFRetained<T>,
  ) -> crate::Result<()> {
    NativeCallStats::record(NativeCall::AxWrite);
    let cf_attribute = CFString::from_str(attribute);
    let result =
      unsafe { self.set_attribute_value(&cf_attribute, value.as_ref()) };

    if result != AXError::Success {
      return Err(Error::Accessibility(attribute.to_string(), result.0));
    }

    Ok(())
  }
}

/// The error code a multiple-attribute read stores in place of a value,
/// or `None` for an attribute that was read.
fn attribute_error(value: &CFType) -> Option<i32> {
  let value = value.downcast_ref::<AXValue>()?;

  // SAFETY: `value` is a live `AXValue`.
  if unsafe { value.r#type() } != AXValueType::AXError {
    return None;
  }

  let mut code = AXError::Success;

  // SAFETY: `AXError` is a transparent `i32`, which is what the
  // `AXValueType::AXError` representation holds, and `code` is live for
  // the call.
  unsafe {
    value.value(AXValueType::AXError, NonNull::from(&mut code).cast())
  }
  .then_some(code.0)
  .or(Some(AXError::Failure.0))
}

#[cfg(test)]
mod tests {
  use objc2_core_foundation::{CFBoolean, CGPoint};

  use super::*;
  use crate::platform_impl::AXValueExt;

  #[test]
  fn get_attribute_invalid_attribute_is_err() {
    let pid = i32::try_from(std::process::id()).expect("pid overflow");

    let el = unsafe { AXUIElement::new_application(pid) };
    let result =
      el.get_attribute::<CFString>("AXDefinitelyNotARealAttribute");

    assert!(result.is_err());
  }

  #[test]
  fn set_attribute_invalid_attribute_is_err() {
    let pid = i32::try_from(std::process::id()).expect("pid overflow");

    let el = unsafe { AXUIElement::new_application(pid) };
    let value = CFString::from_str("dummy");
    let result = el.set_attribute("AXDefinitelyNotARealAttribute", &value);

    assert!(result.is_err());
  }

  #[test]
  fn get_attributes_is_one_counted_request() {
    // The system-wide element needs no application to answer for, so
    // this reads nothing of any window. Whether it answers or not, the
    // call was one request.
    let el = unsafe { AXUIElement::new_system_wide() };
    let before = NativeCallStats::snapshot();

    let _ = el.get_attributes(&[
      "AXFocusedApplication",
      "AXDefinitelyNotARealAttribute",
      "AXRole",
    ]);

    let spent = NativeCallStats::snapshot().since(&before);
    assert_eq!(spent.ax_reads, 1);
  }

  #[test]
  fn get_attributes_names_the_attributes_of_a_failed_request() {
    let el = unsafe { AXUIElement::new_application(0) };

    let result = el.get_attributes(&["AXMinimized", "AXFullScreen"]);

    assert!(matches!(
      result,
      Err(Error::Accessibility(attributes, _))
        if attributes == "AXMinimized,AXFullScreen"
    ));
  }

  #[test]
  fn attribute_error_reads_the_code_stored_in_place_of_a_value() {
    let code = AXError::AttributeUnsupported;
    let error = unsafe {
      AXValue::new(AXValueType::AXError, NonNull::from(&code).cast())
    }
    .expect("Failed to create AXValue.");
    let point = AXValue::new_strict(&CGPoint { x: 1.0, y: 2.0 })
      .expect("Failed to create AXValue.");

    assert_eq!(
      attribute_error(error.as_ref()),
      Some(AXError::AttributeUnsupported.0)
    );
    // Neither another kind of `AXValue` nor an ordinary value is an error.
    assert_eq!(attribute_error(point.as_ref()), None);
    assert_eq!(attribute_error(CFBoolean::new(true).as_ref()), None);
    assert_eq!(
      attribute_error(CFString::from_str("AXWindow").as_ref()),
      None
    );
  }

  #[test]
  fn attribute_access_is_counted() {
    let pid = i32::try_from(std::process::id()).expect("pid overflow");
    let el = unsafe { AXUIElement::new_application(pid) };
    let before = NativeCallStats::snapshot();

    // Failures count too: the call was still attempted.
    let _ = el.get_attribute::<CFString>("AXDefinitelyNotARealAttribute");
    let _ = el.set_attribute(
      "AXDefinitelyNotARealAttribute",
      &CFString::from_str("dummy"),
    );

    let spent = NativeCallStats::snapshot().since(&before);
    assert!(spent.ax_reads >= 1);
    assert!(spent.ax_writes >= 1);
  }
}
