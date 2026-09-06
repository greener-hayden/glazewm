use std::collections::HashMap;

use anyhow::Context;
use uuid::Uuid;
use wm_common::TilingDirection;
use wm_platform::Rect;

use crate::{
  models::{DirectionContainer, RootContainer, TilingContainer},
  traits::{
    CommonGetters, PositionGetters, TilingDirectionGetters,
    TilingSizeGetters,
  },
};

/// Freezes final rectangles for one commit.
#[derive(Default)]
pub struct LayoutSnapshot {
  rects: HashMap<Uuid, Rect>,
}

impl LayoutSnapshot {
  /// Solves parents before their descendants.
  pub fn capture(root: &RootContainer) -> anyhow::Result<Self> {
    let mut snapshot = Self::default();
    for monitor in root.monitors() {
      snapshot.rects.insert(monitor.id(), monitor.to_rect()?);
      for workspace in monitor.workspaces() {
        let rect = workspace.to_rect()?;
        snapshot.rects.insert(workspace.id(), rect.clone());
        snapshot.visit(&workspace.clone().into(), &rect)?;
        for window in workspace
          .children()
          .into_iter()
          .filter_map(|child| child.as_non_tiling_window().cloned())
        {
          snapshot.rects.insert(window.id(), window.to_rect()?);
        }
      }
    }
    Ok(snapshot)
  }

  /// Returns an immutable committed rectangle.
  pub fn rect(&self, id: Uuid) -> anyhow::Result<&Rect> {
    self.rects.get(&id).context("No layout rectangle.")
  }

  /// Reuses each resolved parent rectangle.
  fn visit(
    &mut self,
    parent: &DirectionContainer,
    rect: &Rect,
  ) -> anyhow::Result<()> {
    for (child, rect) in child_rects(parent, rect)? {
      self.rects.insert(child.id(), rect.clone());
      if let TilingContainer::Split(split) = child {
        self.visit(&split.into(), &rect)?;
      }
    }
    Ok(())
  }
}

/// Resolves siblings together without recursive offsets.
pub fn child_rects(
  parent: &DirectionContainer,
  rect: &Rect,
) -> anyhow::Result<Vec<(TilingContainer, Rect)>> {
  let children = parent.tiling_children().collect::<Vec<_>>();
  let horizontal =
    parent.tiling_direction() == TilingDirection::Horizontal;
  let gap = children
    .first()
    .map(TilingSizeGetters::inner_gaps)
    .transpose()?
    .map_or(0, |(x, y)| if horizontal { x } else { y });
  let sizes = children
    .iter()
    .map(TilingSizeGetters::tiling_size)
    .collect::<Vec<_>>();
  let mins = children
    .iter()
    .map(|child| child.min_length(horizontal))
    .collect::<Vec<_>>();
  let total = if horizontal {
    rect.width()
  } else {
    rect.height()
  };
  let available = total
    .saturating_sub(
      gap.saturating_mul(i32::try_from(children.len().saturating_sub(1))?),
    )
    .max(0);
  let lengths = crate::traits::resolve_lengths(&sizes, &mins, available);
  let mut offset = 0;
  Ok(
    children
      .into_iter()
      .zip(lengths)
      .map(|(child, length)| {
        let candidate = if horizontal {
          Rect::from_xy(
            rect.left + offset,
            rect.top,
            length,
            rect.height(),
          )
        } else {
          Rect::from_xy(rect.left, rect.top + offset, rect.width(), length)
        };
        offset += length + gap;
        (child, crate::traits::contain_in_parent(&candidate, rect))
      })
      .collect(),
  )
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{
    commands::container::attach_container,
    models::{Monitor, TilingWindow, Workspace},
  };

  /// Snapshots ignore later native frame mutations.
  #[test]
  fn freezes_layout_geometry() {
    let first = TilingWindow::mock().call();
    let second = TilingWindow::mock().call();
    let workspace = Workspace::mock()
      .tiling_containers(vec![first.clone().into(), second.into()])
      .call();
    let monitor = Monitor::mock().workspaces(vec![workspace]).call();
    let root = RootContainer::new();
    attach_container(&monitor.into(), &root.clone().into(), None)
      .expect("Attach monitor.");
    let snapshot =
      LayoutSnapshot::capture(&root).expect("Capture layout.");
    let old = snapshot
      .rect(first.id())
      .expect("Window rectangle.")
      .clone();
    first.set_tiling_size(0.1);
    assert_eq!(
      snapshot.rect(first.id()).expect("Frozen rectangle."),
      &old
    );
    assert_ne!(
      LayoutSnapshot::capture(&root)
        .expect("New layout.")
        .rect(first.id())
        .expect("New rectangle."),
      &old
    );
  }
}
