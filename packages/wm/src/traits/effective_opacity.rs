use wm_platform::OpacityValue;

/// Computes opacity through one presentation owner.
pub fn effective_opacity(
  concealed: bool,
  desired: Option<OpacityValue>,
) -> Option<OpacityValue> {
  if concealed {
    Some(OpacityValue(0.0))
  } else {
    desired
  }
}
