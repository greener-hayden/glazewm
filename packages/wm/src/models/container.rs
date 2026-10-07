use std::{
  cell::{Ref, RefMut},
  collections::VecDeque,
};

use ambassador::Delegate;
use enum_as_inner::EnumAsInner;
use uuid::Uuid;
use wm_common::{
  ActiveDrag, ContainerDto, DisplayState, GapsConfig, TilingDirection,
  WindowRuleConfig, WindowState,
};
use wm_platform::{Direction, NativeWindow, Rect, RectDelta};

#[allow(clippy::wildcard_imports)]
use crate::{
  models::{
    Monitor, MonitorMetrics, NativeWindowProperties, NonTilingWindow,
    RootContainer, SplitContainer, TilingWindow, Workspace,
  },
  traits::*,
  user_config::UserConfig,
};

/// A container of any type.
///
/// Uses:
///
///  * [`wm_macros::SubEnum`] to define subtypes of containers.
///  * [`wm_macros::EnumFromInner`] to define conversions between the enum
///    and wrapped types.
///  * [`ambassador::Delegate`] to delegate common getters to the contained
///    types. E.g. implements [`CommonGetters`] for [Container] by
///    forwarding the call to the item contained in the enum variant.
///
/// # Example
/// Conversion between the different container types:
/// ```
/// use wm::models::{Container, DirectionContainer, SplitContainer, TilingContainer};
/// use wm::traits::{TilingSizeGetters, TilingDirectionGetters};
///
/// fn example(split: SplitContainer) {
///   // Convert a `SplitContainer` into a `Container`
///   let container: Container = split.into(); // Will be a `Container::Split`
///
///   // Could also have gone straight to a [TilingContainer] from SplitContainer
///   // let tiling: TilingContainer = split.into(); // Will be a `TilingContainer::Split`
///
///   // Try to convert a [Container] into a sub container type ([TilingContainer] in this case).
///   let tiling: TilingContainer = container.try_into().unwrap(); // Will be a `TilingContainer::Split`
///   tiling.tiling_size(); // Can use methods from the `TilingSizeGetters` trait.
///
///   // Try to convert a one sub container type into another. ([TilingContainer] to [DirectionContainer] in this case).
///   let direction: DirectionContainer = tiling.try_into().unwrap(); // Will be a `DirectionContainer::Split`
///   direction.tiling_direction(); // Can use methods from the `TilingDirectionGetters` trait.
///
///   // Convert a sub container back into a [Container]
///   let container: Container = direction.into(); // Will be a `Container::Split`
/// }
/// ```
#[derive(
  Clone,
  Debug,
  EnumAsInner,
  wm_macros::EnumFromInner,
  Delegate,
  wm_macros::SubEnum,
)]
#[delegate(CommonGetters)]
#[delegate(PositionGetters)]
#[subenum(defaults, {
  /// Subenum of [Container]
  #[derive(Clone, Debug, EnumAsInner, Delegate, wm_macros::EnumFromInner)]
  #[delegate(CommonGetters)]
  #[delegate(PositionGetters)]
})]
#[subenum(TilingContainer, {
  /// Subset of containers that implement the following traits:
  /// * `CommonGetters`
  /// * `PositionGetters`
  /// * `TilingSizeGetters`
  #[delegate(TilingSizeGetters)]
})]
#[subenum(WindowContainer, {
  /// Subset of containers that implement the following traits:
  /// * `CommonGetters`
  /// * `PositionGetters`
  /// * `WindowGetters`
  #[delegate(WindowGetters)]
})]
#[subenum(DirectionContainer, {
  /// Subset of containers that implement the following traits:
  /// * `CommonGetters`
  /// * `PositionGetters`
  /// * `DirectionGetters`
  #[delegate(TilingDirectionGetters)]
})]
pub enum Container {
  Root(RootContainer),
  Monitor(Monitor),
  #[subenum(DirectionContainer)]
  Workspace(Workspace),
  #[subenum(TilingContainer, DirectionContainer)]
  Split(SplitContainer),
  #[subenum(TilingContainer, WindowContainer)]
  TilingWindow(TilingWindow),
  #[subenum(WindowContainer)]
  NonTilingWindow(NonTilingWindow),
}

impl PartialEq for Container {
  fn eq(&self, other: &Self) -> bool {
    self.id() == other.id()
  }
}

impl Eq for Container {}

impl Container {
  /// Whether this container takes part in a tiling layout, i.e. it is a
  /// split container or a tiling window.
  #[must_use]
  pub fn is_tiling(&self) -> bool {
    matches!(self, Self::Split(_) | Self::TilingWindow(_))
  }
}

impl TilingContainer {
  /// Smallest this container can be along an axis, in pixels.
  ///
  /// A window's floor is one it has been observed to insist on. A split
  /// that divides the same axis has to fit all of its children end to
  /// end, gaps included; one that divides the other axis stacks them, so
  /// it only has to be as wide as its widest child.
  ///
  /// Returns 0 where nothing is known, which leaves the layout free.
  ///
  /// Walks the whole subtree without allocating. Callers that solve many
  /// containers on one monitor should use `min_length_with` instead.
  pub fn min_length(&self, is_horizontal: bool) -> i32 {
    match self {
      // A window has no gaps, so it does not need its monitor.
      Self::TilingWindow(window) => {
        window_min_length(window, is_horizontal)
      }
      Self::Split(_) => {
        let metrics = self.monitor().map(|monitor| monitor.metrics());
        self.min_length_with(is_horizontal, metrics.as_ref())
      }
    }
  }

  /// Same as `min_length`, given the measurements of the monitor that
  /// the container is on.
  ///
  /// Every container in a subtree shares its monitor, so the metrics are
  /// looked up once by the caller rather than once per split. Without
  /// metrics (a detached container) splits have no gaps.
  pub fn min_length_with(
    &self,
    is_horizontal: bool,
    metrics: Option<&MonitorMetrics>,
  ) -> i32 {
    match self {
      Self::TilingWindow(window) => {
        window_min_length(window, is_horizontal)
      }
      Self::Split(split) => {
        split_min_length(split, is_horizontal, metrics)
      }
    }
  }
}

/// Floor of a window along an axis, or 0 where none has been observed.
fn window_min_length(window: &TilingWindow, is_horizontal: bool) -> i32 {
  window.min_size().map_or(
    0,
    |(width, height)| if is_horizontal { width } else { height },
  )
}

/// Floor of a split along an axis, recursing over its tiling children in
/// place.
fn split_min_length(
  split: &SplitContainer,
  is_horizontal: bool,
  metrics: Option<&MonitorMetrics>,
) -> i32 {
  let mut count = 0_usize;
  let mut sum = 0_i32;
  let mut widest: Option<i32> = None;

  for child in split.borrow_children().iter() {
    let min = match child {
      Container::TilingWindow(window) => {
        window_min_length(window, is_horizontal)
      }
      Container::Split(split) => {
        split_min_length(split, is_horizontal, metrics)
      }
      _ => continue,
    };

    count += 1;
    sum += min;
    widest = Some(widest.map_or(min, |widest| widest.max(min)));
  }

  let divides_axis =
    matches!(split.tiling_direction(), TilingDirection::Horizontal)
      == is_horizontal;

  if divides_axis {
    let gap = metrics.map_or(0, |metrics| {
      let (horizontal, vertical) = split.inner_gaps_with(metrics);
      if is_horizontal {
        horizontal
      } else {
        vertical
      }
    });

    sum + gap * i32::try_from(count.saturating_sub(1)).unwrap_or(0)
  } else {
    widest.unwrap_or(0)
  }
}

impl PartialEq for TilingContainer {
  fn eq(&self, other: &Self) -> bool {
    self.id() == other.id()
  }
}

impl Eq for TilingContainer {}

impl PartialEq for WindowContainer {
  fn eq(&self, other: &Self) -> bool {
    self.id() == other.id()
  }
}

impl Eq for WindowContainer {}

impl std::fmt::Display for WindowContainer {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    // Truncate title if longer than 20 chars. Need to use `chars()`
    // instead of byte slices to handle invalid byte indices.
    let title = {
      let title = self.native_properties().title;
      if title.len() > 20 {
        format!("{}...", title.chars().take(17).collect::<String>())
      } else {
        title
      }
    };

    let class = {
      #[cfg(target_os = "windows")]
      {
        self.native_properties().class_name
      }
      #[cfg(not(target_os = "windows"))]
      {
        String::new()
      }
    };

    let process = self.native_properties().process_name;

    write!(
      f,
      "Window(id={:?}, process={}, class={}, title={})",
      self.native().id(),
      process,
      class,
      title,
    )?;

    Ok(())
  }
}

impl PartialEq for DirectionContainer {
  fn eq(&self, other: &Self) -> bool {
    self.id() == other.id()
  }
}

impl Eq for DirectionContainer {}

/// Implements the `Debug` trait for a given container struct.
///
/// Expects that the struct has a `to_dto()` method.
#[macro_export]
macro_rules! impl_container_debug {
  ($type:ty) => {
    impl std::fmt::Debug for $type {
      fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        std::fmt::Debug::fmt(
          &self.to_dto().map_err(|_| std::fmt::Error),
          f,
        )
      }
    }
  };
}
