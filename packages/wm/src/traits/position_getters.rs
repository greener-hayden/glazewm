use ambassador::delegatable_trait;
use wm_platform::Rect;

#[delegatable_trait]
pub trait PositionGetters {
  fn to_rect(&self) -> anyhow::Result<Rect>;
}

/// Splits `total` between children that each insist on a floor.
///
/// A tiling size is a fraction of the parent, so a busy workspace can
/// hand a window a slot narrower than the app will ever accept. Left
/// alone the window keeps its own size and overhangs its neighbour; the
/// space it took has to come off the siblings instead, or the layout and
/// the screen disagree about where every window after it begins.
///
/// Water-filling: pin whoever falls below their floor, re-share what is
/// left over the rest, and repeat, since pinning one can push the next
/// under. Each pass pins at least one child, so it ends in at most
/// `mins.len()` passes.
///
/// Returns a length per child, in the order given. When the floors alone
/// exceed `total` every child gets its floor and the overflow is
/// accepted — no arrangement satisfies everyone, and spreading the
/// shortfall would leave every window wrong instead of one.
pub fn resolve_lengths(
  sizes: &[f32],
  mins: &[i32],
  total: i32,
) -> Vec<i32> {
  let mut pinned = vec![false; sizes.len()];

  loop {
    let free_total = total
      - mins
        .iter()
        .zip(&pinned)
        .filter(|(_, is_pinned)| **is_pinned)
        .map(|(min, _)| *min)
        .sum::<i32>();

    let free_size = sizes
      .iter()
      .zip(&pinned)
      .filter(|(_, is_pinned)| !**is_pinned)
      .map(|(size, _)| *size)
      .sum::<f32>();

    // Every child is pinned, or there is no share left to divide.
    if free_size <= 0. {
      break;
    }

    let mut pinned_any = false;

    for (index, is_pinned) in pinned.iter_mut().enumerate() {
      if *is_pinned {
        continue;
      }

      #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation
      )]
      let length =
        (free_total as f32 * (sizes[index] / free_size)).round() as i32;

      if length < mins[index] {
        *is_pinned = true;
        pinned_any = true;
      }
    }

    if !pinned_any {
      break;
    }
  }

  let free_total = total
    - mins
      .iter()
      .zip(&pinned)
      .filter(|(_, is_pinned)| **is_pinned)
      .map(|(min, _)| *min)
      .sum::<i32>();

  let free_size = sizes
    .iter()
    .zip(&pinned)
    .filter(|(_, is_pinned)| !**is_pinned)
    .map(|(size, _)| *size)
    .sum::<f32>();

  sizes
    .iter()
    .enumerate()
    .map(|(index, size)| {
      if pinned[index] || free_size <= 0. {
        mins[index]
      } else {
        #[allow(
          clippy::cast_precision_loss,
          clippy::cast_possible_truncation
        )]
        let length =
          (free_total as f32 * (size / free_size)).round() as i32;
        length
      }
    })
    .collect()
}

/// Implements the `PositionGetters` trait for tiling containers that can
/// be resized. This is used by `SplitContainer` and `TilingWindow`.
///
/// Expects that the struct has a wrapping `RefCell` containing a struct
/// with an `id` and a `parent` field.
#[macro_export]
macro_rules! impl_position_getters_as_resizable {
  ($struct_name:ident) => {
    impl PositionGetters for $struct_name {
      fn to_rect(&self) -> anyhow::Result<Rect> {
        let parent = self
          .parent()
          .and_then(|parent| parent.as_direction_container().ok())
          .context("Parent does not have a tiling direction.")?;

        let parent_rect = parent.to_rect()?;

        let (horizontal_gap, vertical_gap) = self.inner_gaps()?;
        let inner_gap = match parent.tiling_direction() {
          TilingDirection::Vertical => vertical_gap,
          TilingDirection::Horizontal => horizontal_gap,
        };

        // Siblings are resolved together rather than each from its own
        // tiling size, because a child pinned to its minimum changes what
        // is left for the others.
        let siblings = parent.tiling_children().collect::<Vec<_>>();
        let sizes = siblings
          .iter()
          .map(TilingSizeGetters::tiling_size)
          .collect::<Vec<_>>();

        let is_horizontal =
          matches!(parent.tiling_direction(), TilingDirection::Horizontal);

        let mins = siblings
          .iter()
          .map(|sibling| sibling.min_length(is_horizontal))
          .collect::<Vec<_>>();

        let index = siblings
          .iter()
          .position(|sibling| sibling.id() == self.id())
          .context("Container is not among its parent's children.")?;

        #[allow(
          clippy::cast_possible_truncation,
          clippy::cast_possible_wrap
        )]
        let (width, height) = {
          let available = match parent.tiling_direction() {
            TilingDirection::Vertical => parent_rect.height(),
            TilingDirection::Horizontal => parent_rect.width(),
          } - inner_gap
            * (siblings.len().saturating_sub(1)) as i32;

          let length =
            $crate::traits::resolve_lengths(&sizes, &mins, available)
              .get(index)
              .copied()
              .unwrap_or(0);

          match parent.tiling_direction() {
            TilingDirection::Vertical => (parent_rect.width(), length),
            TilingDirection::Horizontal => (length, parent_rect.height()),
          }
        };

        let (x, y) = {
          let mut prev_siblings = self
            .prev_siblings()
            .filter_map(|sibling| sibling.as_tiling_container().ok());

          match prev_siblings.next() {
            None => (parent_rect.x(), parent_rect.y()),
            Some(sibling) => {
              let sibling_rect = sibling.to_rect()?;

              match parent.tiling_direction() {
                TilingDirection::Vertical => (
                  parent_rect.x(),
                  sibling_rect.y() + sibling_rect.height() + inner_gap,
                ),
                TilingDirection::Horizontal => (
                  sibling_rect.x() + sibling_rect.width() + inner_gap,
                  parent_rect.y(),
                ),
              }
            }
          }
        };

        Ok(Rect::from_xy(x, y, width, height))
      }
    }
  };
}

#[cfg(test)]
mod tests {
  use super::resolve_lengths;

  #[test]
  fn splits_evenly_without_minimums() {
    assert_eq!(
      resolve_lengths(&[0.5, 0.5], &[0, 0], 1000),
      vec![500, 500]
    );
  }

  #[test]
  fn pins_a_constrained_child_and_pays_from_the_rest() {
    // The first wants 250 but cannot go under 400, so the other three
    // share the remaining 600.
    let lengths = resolve_lengths(&[0.25; 4], &[400, 0, 0, 0], 1000);
    assert_eq!(lengths[0], 400);
    assert_eq!(lengths[1..].iter().sum::<i32>(), 600);
  }

  #[test]
  fn pinning_one_can_pin_the_next() {
    // Pinning the 500 leaves 500 for two, which puts the 300 under too.
    let lengths =
      resolve_lengths(&[0.34, 0.33, 0.33], &[500, 300, 0], 1000);
    assert_eq!(lengths[0], 500);
    assert_eq!(lengths[1], 300);
    assert_eq!(lengths[2], 200);
  }

  #[test]
  fn gives_every_child_its_floor_when_they_cannot_all_fit() {
    // 600 + 600 needs 1200 and there is 1000: overflow is unavoidable,
    // so each keeps its floor rather than every window being wrong.
    assert_eq!(
      resolve_lengths(&[0.5, 0.5], &[600, 600], 1000),
      vec![600, 600]
    );
  }

  #[test]
  fn leaves_a_child_alone_when_its_share_already_clears_its_floor() {
    assert_eq!(
      resolve_lengths(&[0.5, 0.5], &[100, 100], 1000),
      vec![500, 500]
    );
  }

  #[test]
  fn honours_uneven_sizes() {
    assert_eq!(
      resolve_lengths(&[0.75, 0.25], &[0, 0], 1000),
      vec![750, 250]
    );
  }
}
