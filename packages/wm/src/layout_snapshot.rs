use std::{
  collections::HashMap,
  hash::{BuildHasherDefault, Hasher},
  ops::Range,
};

use anyhow::Context;
use uuid::Uuid;
use wm_common::TilingDirection;
use wm_platform::Rect;

use crate::{
  models::{
    Container, DirectionContainer, MonitorMetrics, RootContainer,
    SplitContainer, TilingContainer, TilingWindow, Workspace,
  },
  traits::{
    contain_in_parent, resolve_lengths_into, CommonGetters,
    PositionGetters, SolverBuffers, TilingDirectionGetters,
    TilingSizeGetters, WindowGetters,
  },
};

/// Multiplier of the `UuidHasher` mix, the odd constant used by `FxHash`.
const HASH_SEED: u64 = 0x517c_c1b7_2722_0a95;

/// Hashes a random `Uuid` without the cost of a keyed hash.
///
/// A version 4 `Uuid` is already uniformly random, so the standard
/// hasher's collision resistance buys nothing for ids this process
/// generates itself. Folds the bytes written, eight at a time, with a
/// multiply.
///
/// Only meant for `Uuid` keys. It ignores the length prefix that hashing a
/// byte array writes first, since that is the same for every id.
#[derive(Default)]
struct UuidHasher(u64);

impl Hasher for UuidHasher {
  fn write(&mut self, bytes: &[u8]) {
    for chunk in bytes.chunks(8) {
      let mut word = [0; 8];
      word[..chunk.len()].copy_from_slice(chunk);
      self.0 = (self.0.rotate_left(5) ^ u64::from_le_bytes(word))
        .wrapping_mul(HASH_SEED);
    }
  }

  fn write_usize(&mut self, _length: usize) {}

  fn finish(&self) -> u64 {
    self.0
  }
}

/// Rectangles keyed by container id.
type RectMap = HashMap<Uuid, Rect, BuildHasherDefault<UuidHasher>>;

/// Freezes final rectangles for one commit.
#[derive(Default)]
pub struct LayoutSnapshot {
  rects: RectMap,
  scratch: LayoutScratch,
}

impl LayoutSnapshot {
  /// Solves parents before their descendants.
  ///
  /// Production callers use [`LayoutSnapshot::recapture`] to reuse the
  /// scratch buffers; this constructor stays for tests and the benchmark
  /// harness, which start from a fresh snapshot.
  #[cfg_attr(not(test), allow(dead_code))]
  pub fn capture(root: &RootContainer) -> anyhow::Result<Self> {
    let mut snapshot = Self::default();
    snapshot.recapture(root)?;
    Ok(snapshot)
  }

  /// Solves the layout again, replacing every rectangle.
  ///
  /// Gives the same result as `capture`, but keeps the storage of the
  /// previous snapshot, so a commit does not allocate once the tree has
  /// stopped growing. Leaves the snapshot empty if it fails, rather than
  /// half old and half new.
  pub fn recapture(&mut self, root: &RootContainer) -> anyhow::Result<()> {
    self.rects.clear();
    let result = self.solve(root);
    if result.is_err() {
      self.rects.clear();
    }
    self.scratch.nodes.clear();
    result
  }

  /// Returns an immutable committed rectangle.
  pub fn rect(&self, id: Uuid) -> anyhow::Result<&Rect> {
    self.rects.get(&id).context("No layout rectangle.")
  }

  /// Solves every monitor, workspace and window under `root`.
  fn solve(&mut self, root: &RootContainer) -> anyhow::Result<()> {
    for monitor in root.borrow_children().iter() {
      let Container::Monitor(monitor) = monitor else {
        continue;
      };
      self.rects.insert(monitor.id(), monitor.to_rect()?);
      let metrics = monitor.metrics();

      for workspace in monitor.borrow_children().iter() {
        let Container::Workspace(workspace) = workspace else {
          continue;
        };
        self.solve_workspace(workspace, &metrics)?;

        for child in workspace.borrow_children().iter() {
          if let Container::NonTilingWindow(window) = child {
            self.rects.insert(window.id(), window.to_rect()?);
          }
        }
      }
    }

    Ok(())
  }

  /// Solves a workspace and every tiling container beneath it.
  ///
  /// Builds the tiling tree into an arena first, so each container is
  /// read once however deep it sits. Floors are then resolved from the
  /// leaves up and rectangles from the workspace down, each in a single
  /// pass over the arena.
  fn solve_workspace(
    &mut self,
    workspace: &Workspace,
    metrics: &MonitorMetrics,
  ) -> anyhow::Result<()> {
    let rect = workspace.to_rect()?;
    let scratch = &mut self.scratch;
    scratch.build(workspace, rect, metrics);
    scratch.resolve_floors();
    scratch.resolve_rects()?;

    self.rects.reserve(scratch.nodes.len());
    for node in &scratch.nodes {
      self.rects.insert(node.id, node.rect.clone());
    }

    Ok(())
  }
}

/// One tiling container of a workspace, as read for a single capture.
struct LayoutNode {
  id: Uuid,
  tiling_size: f32,

  /// Floors along the horizontal and vertical axes, in that order.
  floors: [i32; 2],

  /// The inner gaps `(horizontal, vertical)` that the container's own
  /// config resolves to. Only filled in for splits, and for the first
  /// child of a row, which decides the gap of that row.
  gap: (i32, i32),

  /// Whether the container lays its children out left to right.
  horizontal: bool,

  /// Indices of the tiling children, which are contiguous. Empty for a
  /// window.
  children: Range<usize>,

  /// The container, until its children have been read.
  source: Option<DirectionContainer>,
  rect: Rect,
}

/// Storage for solving one workspace, kept between captures.
#[derive(Default)]
struct LayoutScratch {
  /// The workspace first, then each container after its parent, so a
  /// child always has a higher index than its parent.
  nodes: Vec<LayoutNode>,
  row: RowScratch,
}

impl LayoutScratch {
  /// Reads the tiling tree of `workspace` breadth first.
  ///
  /// Reads through `borrow_children`, so no child list is copied. Only
  /// shared borrows are taken, which never conflict with each other.
  fn build(
    &mut self,
    workspace: &Workspace,
    rect: Rect,
    metrics: &MonitorMetrics,
  ) {
    self.nodes.clear();
    self.nodes.push(LayoutNode {
      id: workspace.id(),
      tiling_size: 1.,
      floors: [0; 2],
      gap: (0, 0),
      horizontal: false,
      children: 0..0,
      source: Some(workspace.clone().into()),
      rect,
    });

    let mut next = 0;
    while next < self.nodes.len() {
      if let Some(parent) = self.nodes[next].source.take() {
        let first = self.nodes.len();
        for child in parent.borrow_children().iter() {
          let node = match child {
            Container::TilingWindow(window) => LayoutNode::window(
              window,
              // The first child's config decides the gap of the row.
              self.nodes.len() == first,
              metrics,
            ),
            Container::Split(split) => LayoutNode::split(split, metrics),
            _ => continue,
          };
          self.nodes.push(node);
        }

        let end = self.nodes.len();
        let node = &mut self.nodes[next];
        node.horizontal =
          parent.tiling_direction() == TilingDirection::Horizontal;
        node.children = first..end;
      }
      next += 1;
    }
  }

  /// Resolves the floor of every split along both axes, leaves first.
  ///
  /// Gives what `TilingContainer::min_length` gives for the same
  /// container, without visiting any container more than once.
  fn resolve_floors(&mut self) {
    for index in (0..self.nodes.len()).rev() {
      let (parents, descendants) = self.nodes.split_at_mut(index + 1);
      let parent = &mut parents[index];
      if parent.children.is_empty() {
        continue;
      }

      // `descendants` starts right after `index`.
      let offset = index + 1;
      let children = &descendants
        [parent.children.start - offset..parent.children.end - offset];
      let gaps_between =
        i32::try_from(children.len().saturating_sub(1)).unwrap_or(0);

      for axis in 0..2 {
        let is_horizontal = axis == 0;
        let floors = children.iter().map(|child| child.floors[axis]);
        parent.floors[axis] = if parent.horizontal == is_horizontal {
          let gap = if is_horizontal {
            parent.gap.0
          } else {
            parent.gap.1
          };
          floors.sum::<i32>() + gap * gaps_between
        } else {
          floors.max().unwrap_or(0)
        };
      }
    }
  }

  /// Solves every row from the workspace down.
  fn resolve_rects(&mut self) -> anyhow::Result<()> {
    for index in 0..self.nodes.len() {
      let (parents, descendants) = self.nodes.split_at_mut(index + 1);
      let parent = &parents[index];
      if parent.children.is_empty() {
        continue;
      }

      // `descendants` starts right after `index`.
      let offset = index + 1;
      let children = &mut descendants
        [parent.children.start - offset..parent.children.end - offset];
      let gap = children.first().map_or(0, |first| {
        if parent.horizontal {
          first.gap.0
        } else {
          first.gap.1
        }
      });

      let row = &mut self.row;
      row.clear();
      let axis = usize::from(!parent.horizontal);
      for child in children.iter() {
        row.push(child.tiling_size, child.floors[axis]);
      }
      row.solve(&parent.rect, parent.horizontal, gap)?;

      let mut offset = 0;
      for (child, length) in children.iter_mut().zip(&row.lengths) {
        child.rect =
          row_child_rect(&parent.rect, parent.horizontal, offset, *length);
        offset += length + gap;
      }
    }

    Ok(())
  }
}

impl LayoutNode {
  /// Reads a tiling window.
  ///
  /// Resolves its gap only when `needs_gap`, i.e. when it is first in its
  /// row, since no other window's gap is ever read.
  fn window(
    window: &TilingWindow,
    needs_gap: bool,
    metrics: &MonitorMetrics,
  ) -> Self {
    let (width, height) = window.min_size().unwrap_or((0, 0));
    Self {
      id: window.id(),
      tiling_size: window.tiling_size(),
      floors: [width, height],
      gap: if needs_gap {
        window.inner_gaps_with(metrics)
      } else {
        (0, 0)
      },
      horizontal: false,
      children: 0..0,
      source: None,
      rect: Rect::from_xy(0, 0, 0, 0),
    }
  }

  /// Reads a split container, whose children are read later.
  fn split(split: &SplitContainer, metrics: &MonitorMetrics) -> Self {
    Self {
      id: split.id(),
      tiling_size: split.tiling_size(),
      floors: [0; 2],
      gap: split.inner_gaps_with(metrics),
      horizontal: false,
      children: 0..0,
      source: Some(split.clone().into()),
      rect: Rect::from_xy(0, 0, 0, 0),
    }
  }
}

/// Storage for solving one row of siblings, reused from row to row.
#[derive(Default)]
struct RowScratch {
  sizes: Vec<f32>,
  floors: Vec<i32>,
  lengths: Vec<i32>,
  solver: SolverBuffers,
}

impl RowScratch {
  /// Empties the row, keeping its storage.
  fn clear(&mut self) {
    self.sizes.clear();
    self.floors.clear();
  }

  /// Adds a child with its tiling size and its floor along the row.
  fn push(&mut self, tiling_size: f32, floor: i32) {
    self.sizes.push(tiling_size);
    self.floors.push(floor);
  }

  /// Resolves the length of every child in the row, in `lengths`.
  ///
  /// Shares `rect` between the children less the gaps between them.
  fn solve(
    &mut self,
    rect: &Rect,
    horizontal: bool,
    gap: i32,
  ) -> anyhow::Result<()> {
    let count = self.sizes.len();
    let total = if horizontal {
      rect.width()
    } else {
      rect.height()
    };
    let available = total
      .saturating_sub(
        gap.saturating_mul(i32::try_from(count.saturating_sub(1))?),
      )
      .max(0);

    self.lengths.clear();
    self.lengths.resize(count, 0);
    resolve_lengths_into(
      &self.sizes,
      &self.floors,
      available,
      &mut self.lengths,
      &mut self.solver,
    );

    Ok(())
  }
}

/// Rectangle of a child that starts `offset` along its parent's row.
///
/// Keeps the child inside the parent, which a floor that does not fit
/// would otherwise push out.
fn row_child_rect(
  parent: &Rect,
  horizontal: bool,
  offset: i32,
  length: i32,
) -> Rect {
  let candidate = if horizontal {
    Rect::from_xy(
      parent.left + offset,
      parent.top,
      length,
      parent.height(),
    )
  } else {
    Rect::from_xy(parent.left, parent.top + offset, parent.width(), length)
  };

  contain_in_parent(&candidate, parent)
}

/// Rectangle of a tiling container, solved from its workspace down.
///
/// Solves only the rows on the way down to `container`, each sharing its
/// space exactly as `LayoutSnapshot` does, so the result matches what the
/// snapshot holds for it.
pub fn tiling_rect(container: &TilingContainer) -> anyhow::Result<Rect> {
  let (rect, _) = tiling_rect_in(container, &mut RowScratch::default())?;
  Ok(rect)
}

/// Rectangle of `container` along with the measurements of its monitor.
fn tiling_rect_in(
  container: &TilingContainer,
  row: &mut RowScratch,
) -> anyhow::Result<(Rect, MonitorMetrics)> {
  let parent = container
    .parent()
    .and_then(|parent| parent.as_direction_container().ok())
    .context("Parent lacks tiling direction.")?;

  let (parent_rect, metrics) = match &parent {
    DirectionContainer::Workspace(workspace) => (
      workspace.to_rect()?,
      workspace
        .monitor()
        .context("Workspace has no parent monitor.")?
        .metrics(),
    ),
    DirectionContainer::Split(split) => {
      tiling_rect_in(&split.as_tiling_container()?, row)?
    }
  };

  let horizontal =
    parent.tiling_direction() == TilingDirection::Horizontal;
  let id = container.id();
  let mut index = None;
  let mut gap = 0;
  row.clear();
  for child in parent.borrow_children().iter() {
    let Ok(child) = TilingContainer::try_from(child.clone()) else {
      continue;
    };
    if row.sizes.is_empty() {
      // The first child's config decides the gap of the row.
      let (x, y) = child.inner_gaps_with(&metrics);
      gap = if horizontal { x } else { y };
    }
    if child.id() == id {
      index = Some(row.sizes.len());
    }
    row.push(
      child.tiling_size(),
      child.min_length_with(horizontal, Some(&metrics)),
    );
  }
  let index = index.context("Missing tiling child.")?;

  row.solve(&parent_rect, horizontal, gap)?;
  let offset = row.lengths[..index]
    .iter()
    .map(|length| length + gap)
    .sum::<i32>();

  Ok((
    row_child_rect(&parent_rect, horizontal, offset, row.lengths[index]),
    metrics,
  ))
}

#[cfg(test)]
pub(crate) mod tests {
  #![allow(clippy::cast_precision_loss)]
  use std::collections::HashSet;

  use wm_common::{GapsConfig, WindowState};
  use wm_platform::{LengthValue, RectDelta};

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

  /// Deterministic pseudo-random numbers for building trees.
  struct Random(u64);

  impl Random {
    fn next(&mut self) -> u64 {
      self.0 = self
        .0
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
      self.0 >> 33
    }

    /// A number in `0..bound`.
    fn below(&mut self, bound: u64) -> u64 {
      self.next() % bound
    }
  }

  /// One of a few gap configs, so neighbours disagree about their gaps.
  fn random_gaps(random: &mut Random) -> GapsConfig {
    let outer = |amount| {
      RectDelta::new(
        LengthValue::from_px(amount),
        LengthValue::from_px(amount),
        LengthValue::from_px(amount),
        LengthValue::from_px(amount),
      )
    };
    #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
    let amount = random.below(24) as i32;
    GapsConfig {
      scale_with_dpi: random.below(2) == 0,
      inner_gap: if random.below(4) == 0 {
        LengthValue {
          amount: 0.01 * random.below(5) as f32,
          unit: wm_platform::LengthUnit::Percentage,
        }
      } else {
        LengthValue::from_px(amount)
      },
      outer_gap: outer(amount / 2),
      single_window_outer_gap: (random.below(3) == 0)
        .then(|| outer(amount)),
    }
  }

  /// A random tiling subtree of at most `budget` windows.
  fn random_tiling(
    random: &mut Random,
    budget: usize,
    depth: usize,
  ) -> TilingContainer {
    if budget <= 1 || depth > 5 || random.below(5) == 0 {
      let window =
        TilingWindow::mock().gaps_config(random_gaps(random)).call();
      if random.below(3) != 0 {
        #[allow(
          clippy::cast_possible_wrap,
          clippy::cast_possible_truncation
        )]
        let min_size =
          (random.below(900) as i32, random.below(500) as i32);
        window.update_native_properties(|properties| {
          properties.min_size = Some(min_size);
        });
      }
      return window.into();
    }

    let count = 1 + usize::try_from(random.below(5)).unwrap_or(1);
    let share = budget / count;
    let children = (0..count)
      .map(|_| random_tiling(random, share.max(1), depth + 1))
      .collect::<Vec<_>>();
    SplitContainer::mock()
      .tiling_direction(if random.below(2) == 0 {
        TilingDirection::Horizontal
      } else {
        TilingDirection::Vertical
      })
      .gaps_config(random_gaps(random))
      .tiling_containers(children)
      .call()
      .into()
  }

  /// A root holding monitors of random geometry with random workspaces.
  fn random_root(seed: u64) -> RootContainer {
    use crate::models::NonTilingWindow;

    let mut random = Random(seed);
    let root = RootContainer::new();
    for _ in 0..=random.below(2) {
      #[allow(
        clippy::cast_possible_wrap,
        clippy::cast_possible_truncation
      )]
      let bounds = Rect::from_xy(
        random.below(2000) as i32 - 1000,
        random.below(500) as i32 - 250,
        300 + random.below(3000) as i32,
        200 + random.below(1600) as i32,
      );
      let workspaces = (0..=random.below(2))
        .map(|index| {
          let budget = 1 + usize::try_from(random.below(30)).unwrap_or(1);
          let tiling = if random.below(8) == 0 {
            Vec::new()
          } else {
            (0..=random.below(2))
              .map(|_| random_tiling(&mut random, budget, 0))
              .collect()
          };
          Workspace::mock()
            .name(index.to_string())
            .tiling_direction(if random.below(2) == 0 {
              TilingDirection::Horizontal
            } else {
              TilingDirection::Vertical
            })
            .gaps_config(random_gaps(&mut random))
            .tiling_containers(tiling)
            .non_tiling_windows(vec![NonTilingWindow::mock()
              .state(WindowState::Tiling)
              .call()])
            .call()
        })
        .collect::<Vec<_>>();
      #[allow(clippy::cast_precision_loss)]
      let monitor = Monitor::mock()
        .bounds(bounds.clone())
        .working_area(bounds.apply_delta(
          &RectDelta::new(
            LengthValue::from_px(0),
            LengthValue::from_px(0),
            LengthValue::from_px(0),
            LengthValue::from_px(40),
          ),
          None,
        ))
        .scale_factor(1. + 0.25 * random.below(4) as f32)
        .workspaces(workspaces)
        .call();
      attach_container(&monitor.into(), &root.clone().into(), None)
        .expect("Attach monitor.");
    }

    // Sizes are random after every attach, since each attach re-divides
    // its row.
    for container in root.descendants() {
      if let Ok(tiling) = container.as_tiling_container() {
        let size = 0.05 + 0.01 * random.below(90) as f32;
        tiling.set_tiling_size(size);
      }
    }

    root
  }

  /// The pre-optimisation `min_length`, kept as a differential oracle.
  pub(crate) fn reference_min_length(
    container: &TilingContainer,
    is_horizontal: bool,
  ) -> i32 {
    match container {
      TilingContainer::TilingWindow(window) => window
        .native_properties()
        .min_size
        .map_or(
          0,
          |(width, height)| {
            if is_horizontal {
              width
            } else {
              height
            }
          },
        ),
      TilingContainer::Split(split) => {
        let children = split.tiling_children().collect::<Vec<_>>();

        let mins = children
          .iter()
          .map(|child| reference_min_length(child, is_horizontal));

        let divides_axis =
          matches!(split.tiling_direction(), TilingDirection::Horizontal)
            == is_horizontal;

        if divides_axis {
          let gap =
            split.inner_gaps().map_or(0, |(horizontal, vertical)| {
              if is_horizontal {
                horizontal
              } else {
                vertical
              }
            });

          mins.sum::<i32>()
            + gap
              * i32::try_from(children.len().saturating_sub(1))
                .unwrap_or(0)
        } else {
          mins.max().unwrap_or(0)
        }
      }
    }
  }

  /// The pre-optimisation `child_rects`, kept as a differential oracle.
  fn reference_child_rects(
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
      .map(|child| reference_min_length(child, horizontal))
      .collect::<Vec<_>>();
    let total = if horizontal {
      rect.width()
    } else {
      rect.height()
    };
    let available =
      total
        .saturating_sub(gap.saturating_mul(i32::try_from(
          children.len().saturating_sub(1),
        )?))
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
            Rect::from_xy(
              rect.left,
              rect.top + offset,
              rect.width(),
              length,
            )
          };
          offset += length + gap;
          (child, contain_in_parent(&candidate, rect))
        })
        .collect(),
    )
  }

  /// The pre-optimisation `to_rect` of a tiling container, kept as a
  /// differential oracle.
  pub(crate) fn reference_to_rect(
    container: &TilingContainer,
  ) -> anyhow::Result<Rect> {
    let parent = container
      .parent()
      .and_then(|parent| parent.as_direction_container().ok())
      .context("Parent lacks tiling direction.")?;
    let parent_rect = match &parent {
      DirectionContainer::Workspace(workspace) => workspace.to_rect()?,
      DirectionContainer::Split(split) => {
        reference_to_rect(&split.as_tiling_container()?)?
      }
    };
    reference_child_rects(&parent, &parent_rect)?
      .into_iter()
      .find(|(child, _)| child.id() == container.id())
      .map(|(_, rect)| rect)
      .context("Missing tiling child.")
  }

  /// The pre-optimisation `capture`, kept as a differential oracle.
  fn reference_capture(root: &RootContainer) -> HashMap<Uuid, Rect> {
    fn visit(
      rects: &mut HashMap<Uuid, Rect>,
      parent: &DirectionContainer,
      rect: &Rect,
    ) {
      for (child, rect) in
        reference_child_rects(parent, rect).expect("Reference rows.")
      {
        rects.insert(child.id(), rect.clone());
        if let TilingContainer::Split(split) = child {
          visit(rects, &split.into(), &rect);
        }
      }
    }

    let mut rects = HashMap::new();
    for monitor in root.monitors() {
      rects.insert(monitor.id(), monitor.to_rect().expect("Monitor."));
      for workspace in monitor.workspaces() {
        let rect = workspace.to_rect().expect("Workspace.");
        rects.insert(workspace.id(), rect.clone());
        visit(&mut rects, &workspace.clone().into(), &rect);
        for window in workspace
          .children()
          .into_iter()
          .filter_map(|child| child.as_non_tiling_window().cloned())
        {
          rects.insert(window.id(), window.to_rect().expect("Floating."));
        }
      }
    }
    rects
  }

  /// Every rectangle of the arena capture equals the old recursive one,
  /// and so does the rectangle of each container solved on its own.
  #[test]
  fn capture_and_to_rect_match_the_reference_on_random_trees() {
    let mut tiling = 0;
    for seed in 0..400 {
      let root = random_root(seed);
      let reference = reference_capture(&root);
      let snapshot = LayoutSnapshot::capture(&root).expect("Capture.");
      assert_eq!(snapshot.rects.len(), reference.len(), "seed {seed}");

      for (id, expected) in &reference {
        assert_eq!(
          snapshot.rect(*id).expect("Snapshot rectangle."),
          expected,
          "seed {seed}"
        );
      }

      for container in root.descendants() {
        let Ok(container) = container.as_tiling_container() else {
          continue;
        };
        tiling += 1;
        let expected = reference.get(&container.id()).expect("Reference.");
        assert_eq!(
          &container.to_rect().expect("Solved on its own."),
          expected,
          "seed {seed}"
        );
        assert_eq!(
          &reference_to_rect(&container).expect("Oracle."),
          expected,
          "seed {seed}"
        );
      }
    }
    assert!(tiling > 2000, "trees are too small to mean much: {tiling}");
  }

  /// `min_length` without allocation equals the old recursion on both
  /// axes, with and without a monitor to take gaps from.
  #[test]
  fn min_length_matches_the_reference_on_random_trees() {
    let mut pinned = 0;
    for seed in 0..400 {
      let root = random_root(seed);
      for container in root.descendants() {
        let Ok(container) = container.as_tiling_container() else {
          continue;
        };
        for is_horizontal in [true, false] {
          let floor = container.min_length(is_horizontal);
          assert_eq!(
            floor,
            reference_min_length(&container, is_horizontal),
            "seed {seed}"
          );
          pinned += i32::from(floor > 0);
        }
      }
    }
    assert!(pinned > 1000, "floors are never exercised: {pinned}");
  }

  /// Recapturing over an old snapshot gives the same rectangles as a
  /// fresh capture, whatever the old snapshot held.
  #[test]
  fn recapture_replaces_every_rectangle() {
    let mut snapshot = LayoutSnapshot::default();
    for seed in 0..100 {
      let root = random_root(seed);
      snapshot.recapture(&root).expect("Recapture.");
      let fresh = LayoutSnapshot::capture(&root).expect("Fresh capture.");
      assert_eq!(snapshot.rects, fresh.rects, "seed {seed}");
      assert!(snapshot.scratch.nodes.is_empty(), "nodes kept alive");
    }
  }

  /// Ids drawn at random spread over the table without clashing.
  #[test]
  fn uuid_hasher_spreads_random_ids() {
    use std::hash::BuildHasher;

    let build = BuildHasherDefault::<UuidHasher>::default();
    let hashes = (0..100_000)
      .map(|_| build.hash_one(Uuid::new_v4()))
      .collect::<HashSet<_>>();
    assert_eq!(hashes.len(), 100_000);

    // The low bits pick a bucket, so they have to vary too.
    let buckets = hashes
      .iter()
      .map(|hash| hash & 0xffff)
      .collect::<HashSet<_>>();
    assert!(buckets.len() > 50_000, "poor spread: {}", buckets.len());
  }
}
