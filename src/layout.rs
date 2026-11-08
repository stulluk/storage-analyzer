//! Integer box layout shared by the disk map and the folder treemap.

use std::cmp::Reverse;

/// A terminal cell is about twice as tall as it is wide.
const CELL_ASPECT: usize = 2;

/// A box on the character grid; its outer border cells are included.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
  pub x: usize,
  pub y: usize,
  pub width: usize,
  pub height: usize,
}

impl Rect {
  /// Return the column of the right border.
  fn right(self) -> usize {
    self.x + self.width.saturating_sub(1)
  }

  /// Return the row of the bottom border.
  fn bottom(self) -> usize {
    self.y + self.height.saturating_sub(1)
  }
}

/// A direction of cursor movement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
  Left,
  Right,
  Up,
  Down,
}

/// Split total integer cells proportionally to weights while honouring minimums.
pub fn allocate(total: usize, weights: &[u64], minimums: &[usize]) -> Vec<usize> {
  let count = weights.len();
  let requested: usize = minimums.iter().sum();
  // not enough room: shrink every minimum evenly, so that no box collapses first
  let minimums: Vec<usize> = if requested > total {
    minimums
      .iter()
      .map(|minimum| minimum * total / requested)
      .collect()
  } else {
    minimums.to_vec()
  };
  let mut pinned = vec![false; count];
  let (free_total, free_weight) = loop {
    let free_total = (total
      - (0..count)
        .filter(|&i| pinned[i])
        .map(|i| minimums[i])
        .sum::<usize>()) as u128;
    let free_weight: u128 = (0..count)
      .filter(|&i| !pinned[i])
      .map(|i| weights[i] as u128)
      .sum();
    let newly_pinned: Vec<usize> = (0..count)
      .filter(|&i| {
        !pinned[i]
          && (free_weight == 0
            || free_total * (weights[i] as u128) < (minimums[i] as u128) * free_weight)
      })
      .collect();
    if newly_pinned.is_empty() {
      break (free_total, free_weight);
    }
    for i in newly_pinned {
      pinned[i] = true;
    }
  };
  let mut lengths = minimums.clone();
  let mut remainders: Vec<(u128, usize)> = Vec::new();
  for i in (0..count).filter(|&i| !pinned[i]) {
    let share = free_total * weights[i] as u128;
    lengths[i] = (share / free_weight) as usize;
    remainders.push((share % free_weight, i));
  }
  let leftover = total - lengths.iter().sum::<usize>();
  remainders.sort_unstable_by_key(|&entry| Reverse(entry));
  for &(_, i) in remainders.iter().take(leftover) {
    lengths[i] += 1;
  }
  if remainders.is_empty() && count > 0 {
    let largest = (0..count).max_by_key(|&i| weights[i]).unwrap_or(0);
    lengths[largest] += leftover;
  }
  lengths
}

/// Return how far the least square-looking box is from a visual square.
fn worst_aspect(rects: &[Rect]) -> f64 {
  rects.iter().fold(1.0_f64, |worst, rect| {
    let width = rect.width.max(1) as f64;
    let height = (rect.height.max(1) * CELL_ASPECT) as f64;
    worst.max(width / height).max(height / width)
  })
}

/// Cut a box along one axis; neighbouring boxes share their border line.
fn slice_rect(rect: Rect, lengths: &[usize], horizontal: bool) -> Vec<Rect> {
  let mut position = if horizontal { rect.x } else { rect.y };
  lengths
    .iter()
    .map(|&length| {
      let slice = if horizontal {
        Rect {
          x: position,
          width: length + 1,
          ..rect
        }
      } else {
        Rect {
          y: position,
          height: length + 1,
          ..rect
        }
      };
      position += length;
      slice
    })
    .collect()
}

/// Return whether a (does not fit, aspect) score beats another one.
fn is_better(score: (bool, f64), best: (bool, f64)) -> bool {
  (!score.0 && best.0) || (score.0 == best.0 && score.1 < best.1)
}

/// Place regions side by side or on top of each other, whichever suits the box.
pub fn stack_regions(
  rect: Rect,
  weights: &[u64],
  min_width: usize,
  min_heights: &[usize],
) -> Vec<Rect> {
  let mut best = Vec::new();
  let mut best_score = (true, f64::INFINITY);
  for horizontal in [true, false] {
    let span = if horizontal { rect.width } else { rect.height };
    let slots = span.saturating_sub(1);
    let minimums: Vec<usize> = if horizontal {
      vec![min_width - 1; weights.len()]
    } else {
      min_heights.iter().map(|height| height - 1).collect()
    };
    let rects = slice_rect(rect, &allocate(slots, weights, &minimums), horizontal);
    let score = (minimums.iter().sum::<usize>() > slots, worst_aspect(&rects));
    if is_better(score, best_score) {
      best = rects;
      best_score = score;
    }
  }
  best
}

/// Group descending sizes into count consecutive rows of similar total size.
fn split_rows(sizes: &[u64], count: usize) -> Vec<std::ops::Range<usize>> {
  let mut rows = Vec::with_capacity(count);
  let mut start = 0;
  for rows_left in (2..=count).rev() {
    let target = sizes[start..].iter().sum::<u64>() as f64 / rows_left as f64;
    let mut end = start + 1;
    let mut filled = sizes[start] as f64;
    while end < sizes.len() - (rows_left - 1) && filled + sizes[end] as f64 / 2.0 <= target {
      filled += sizes[end] as f64;
      end += 1;
    }
    rows.push(start..end);
    start = end;
  }
  rows.push(start..sizes.len());
  rows
}

/// Arrange descending disk sizes in rows, using the row count that gives the squarest boxes.
pub fn layout_disks(sizes: &[u64], rect: Rect, min_width: usize, min_height: usize) -> Vec<Rect> {
  let mut best = Vec::new();
  let mut best_score = (true, f64::INFINITY);
  for count in 1..=sizes.len() {
    let rows = split_rows(sizes, count);
    let row_totals: Vec<u64> = rows
      .iter()
      .map(|row| sizes[row.clone()].iter().sum())
      .collect();
    let height_minimums = vec![min_height - 1; count];
    let height_slots = rect.height.saturating_sub(1);
    let heights = allocate(height_slots, &row_totals, &height_minimums);
    let mut fits = height_minimums.iter().sum::<usize>() <= height_slots;
    let mut placed = Vec::with_capacity(sizes.len());
    for (row, row_rect) in rows.iter().zip(slice_rect(rect, &heights, false)) {
      let width_minimums = vec![min_width - 1; row.len()];
      let width_slots = rect.width.saturating_sub(1);
      fits = fits && width_minimums.iter().sum::<usize>() <= width_slots;
      let widths = allocate(width_slots, &sizes[row.clone()], &width_minimums);
      placed.extend(slice_rect(row_rect, &widths, true));
    }
    let score = (!fits, worst_aspect(&placed));
    if is_better(score, best_score) {
      best = placed;
      best_score = score;
    }
  }
  best
}

/// Recursively cut a box in two, giving each half about half of the weight.
fn split_treemap(
  order: &[usize],
  weights: &[u64],
  rect: Rect,
  placed: &mut [Rect],
  minimum: (usize, usize),
) {
  if let [only] = order {
    placed[*only] = rect;
    return;
  }
  let total: u128 = weights.iter().map(|&weight| weight as u128).sum();
  let mut left = 0_u128;
  let mut count = 0;
  for &weight in &weights[..weights.len() - 1] {
    let next = left + weight as u128;
    if count > 0 && next.abs_diff(total - next) >= left.abs_diff(total - left) {
      break;
    }
    left = next;
    count += 1;
  }
  let mut horizontal = rect.width >= rect.height * CELL_ASPECT;
  let slots_of = |horizontal: bool| {
    if horizontal {
      rect.width.saturating_sub(1)
    } else {
      rect.height.saturating_sub(1)
    }
  };
  if slots_of(horizontal) < 2 {
    horizontal = !horizontal;
  }
  let slots = slots_of(horizontal);
  let least = if horizontal { minimum.0 } else { minimum.1 } - 1;
  let least = if slots >= 2 * least { least } else { 1 };
  let cut = ((slots as u128 * left + total / 2) / total) as usize;
  let cut = if slots >= 2 {
    cut.clamp(least, slots - least)
  } else {
    slots.min(1)
  };
  let halves = slice_rect(rect, &[cut, slots - cut], horizontal);
  split_treemap(
    &order[..count],
    &weights[..count],
    halves[0],
    placed,
    minimum,
  );
  split_treemap(
    &order[count..],
    &weights[count..],
    halves[1],
    placed,
    minimum,
  );
}

/// Return one box per weight, areas following the weights and the boxes tiling rect exactly.
pub fn treemap(weights: &[u64], rect: Rect, minimum: (usize, usize)) -> Vec<Rect> {
  if weights.is_empty() {
    return Vec::new();
  }
  let mut order: Vec<usize> = (0..weights.len()).collect();
  order.sort_by_key(|&i| (Reverse(weights[i]), i));
  let sorted: Vec<u64> = order.iter().map(|&i| weights[i].max(1)).collect();
  let mut placed = vec![rect; weights.len()];
  split_treemap(&order, &sorted, rect, &mut placed, minimum);
  placed
}

/// Fit as many descending weights as have a readable box; the others share one final box.
///
/// `extra` is weight that always belongs to the shared box. The result holds the number of
/// weights kept and their boxes, followed by the shared box when there is one.
pub fn fit_treemap(
  weights: &[u64],
  extra: u64,
  rect: Rect,
  minimum: (usize, usize),
) -> (usize, Vec<Rect>) {
  if weights.is_empty() && extra == 0 {
    return (0, Vec::new());
  }
  let area = rect.width.saturating_sub(1) * rect.height.saturating_sub(1);
  let cell = (minimum.0 - 1) * (minimum.1 - 1);
  let total = weights.iter().sum::<u64>() + extra;
  let least_shared = ((total as u128 * cell as u128) / area.max(1) as u128).min(u64::MAX as u128);
  let mut kept = weights.len().min((area / cell).max(1));
  loop {
    let mut boxes: Vec<u64> = weights[..kept].to_vec();
    if kept < weights.len() || extra > 0 {
      let shared = weights[kept..].iter().sum::<u64>() + extra;
      boxes.push(shared.max(least_shared as u64));
    }
    let placed = treemap(&boxes, rect, minimum);
    let readable = placed
      .iter()
      .all(|r| r.width >= minimum.0 && r.height >= minimum.1);
    if readable || kept <= 1 {
      return (kept, placed);
    }
    kept -= (kept / 8).max(1);
  }
}

/// Return the box next to rects[from] in a direction, preferring boxes alongside it.
///
/// Boxes that are not alongside are the fallback: on the disk map only mounted partitions can be
/// selected, so the next selectable box may be further up or down.
pub fn neighbour(rects: &[Rect], from: usize, direction: Direction) -> Option<usize> {
  let origin = rects[from];
  let center = |low: usize, high: usize| (low + high) as i64;
  rects
    .iter()
    .enumerate()
    .filter(|&(index, _)| index != from)
    .filter_map(|(index, rect)| {
      let rect = *rect;
      let (gap, overlap, offset) = match direction {
        Direction::Right | Direction::Left => {
          let gap = if direction == Direction::Right {
            (rect.x >= origin.right()).then(|| rect.x - origin.right())
          } else {
            (rect.right() <= origin.x).then(|| origin.x - rect.right())
          }?;
          let overlap = rect.bottom().min(origin.bottom()) > rect.y.max(origin.y);
          (
            gap,
            overlap,
            center(rect.y, rect.bottom()) - center(origin.y, origin.bottom()),
          )
        }
        Direction::Down | Direction::Up => {
          let gap = if direction == Direction::Down {
            (rect.y >= origin.bottom()).then(|| rect.y - origin.bottom())
          } else {
            (rect.bottom() <= origin.y).then(|| origin.y - rect.bottom())
          }?;
          let overlap = rect.right().min(origin.right()) > rect.x.max(origin.x);
          (
            gap,
            overlap,
            center(rect.x, rect.right()) - center(origin.x, origin.right()),
          )
        }
      };
      Some((!overlap, gap, offset.abs(), index))
    })
    .min()
    .map(|(_, _, _, index)| index)
}

#[cfg(test)]
mod tests {
  use super::*;

  const FULL: Rect = Rect {
    x: 0,
    y: 0,
    width: 121,
    height: 41,
  };

  #[test]
  fn allocate_is_proportional_and_uses_every_cell() {
    assert_eq!(allocate(90, &[2, 1], &[1, 1]), [60, 30]);
    assert_eq!(
      allocate(101, &[3, 3, 3], &[1, 1, 1]).iter().sum::<usize>(),
      101
    );
  }

  #[test]
  fn allocate_honours_minimums() {
    assert_eq!(allocate(40, &[1000, 1], &[3, 3]), [37, 3]);
  }

  #[test]
  fn allocate_survives_impossible_minimums_and_zero_weights() {
    assert_eq!(allocate(5, &[1, 1], &[4, 4]).iter().sum::<usize>(), 5);
    assert_eq!(allocate(0, &[1, 1], &[3, 3]).iter().sum::<usize>(), 0);
    assert_eq!(allocate(10, &[0, 0], &[1, 1]).iter().sum::<usize>(), 10);
  }

  /// Assert that boxes cover every interior slot of the area exactly once.
  fn assert_tiles(boxes: &[Rect], area: Rect) {
    let covered: usize = boxes.iter().map(|r| (r.width - 1) * (r.height - 1)).sum();
    assert_eq!(covered, (area.width - 1) * (area.height - 1));
    for (index, first) in boxes.iter().enumerate() {
      for second in &boxes[index + 1..] {
        let apart = first.right() <= second.x
          || second.right() <= first.x
          || first.bottom() <= second.y
          || second.bottom() <= first.y;
        assert!(apart, "{first:?} overlaps {second:?}");
      }
    }
  }

  #[test]
  fn treemap_tiles_the_area_in_proportion() {
    let boxes = treemap(&[600, 300, 100], FULL, (6, 3));
    assert_tiles(&boxes, FULL);
    let area = |r: Rect| ((r.width - 1) * (r.height - 1)) as f64;
    let ratio = area(boxes[0]) / area(boxes[2]);
    assert!((ratio - 6.0).abs() < 0.8, "ratio {ratio}");
  }

  #[test]
  fn fit_treemap_merges_what_cannot_be_read() {
    let weights: Vec<u64> = (1..=5000).rev().collect();
    let minimum = (12, 3);
    let (kept, boxes) = fit_treemap(&weights, 0, FULL, minimum);
    assert!(kept < weights.len());
    assert_eq!(boxes.len(), kept + 1);
    assert_tiles(&boxes, FULL);
    assert!(
      boxes
        .iter()
        .all(|r| r.width >= minimum.0 && r.height >= minimum.1)
    );
  }

  #[test]
  fn fit_treemap_keeps_everything_when_there_is_room() {
    let (kept, boxes) = fit_treemap(&[500, 400, 300], 0, FULL, (12, 3));
    assert_eq!((kept, boxes.len()), (3, 3));
  }

  #[test]
  fn fit_treemap_gives_extra_weight_a_box() {
    let (kept, boxes) = fit_treemap(&[500], 100, FULL, (12, 3));
    assert_eq!((kept, boxes.len()), (1, 2));
  }

  #[test]
  fn fit_treemap_never_panics_on_tiny_areas() {
    for (width, height) in [(0, 0), (1, 1), (2, 2), (3, 1), (5, 3), (13, 4)] {
      let rect = Rect {
        x: 0,
        y: 1,
        width,
        height,
      };
      let weights: Vec<u64> = (1..=40).rev().collect();
      let _ = fit_treemap(&weights, 7, rect, (12, 3));
    }
  }

  #[test]
  fn layout_is_proportional_to_disk_size() {
    let area = Rect {
      x: 0,
      y: 0,
      width: 151,
      height: 41,
    };
    let boxes = layout_disks(&[2000, 1000], area, 34, 7);
    let size = |r: Rect| ((r.width - 1) * (r.height - 1)) as f64;
    assert!((size(boxes[0]) / size(boxes[1]) - 2.0).abs() < 0.05);
  }

  #[test]
  fn neighbour_reaches_a_box_that_is_not_alongside() {
    let boxes = [
      Rect {
        x: 0,
        y: 0,
        width: 11,
        height: 4,
      },
      Rect {
        x: 10,
        y: 8,
        width: 11,
        height: 4,
      },
    ];
    assert_eq!(neighbour(&boxes, 0, Direction::Right), Some(1));
    assert_eq!(neighbour(&boxes, 1, Direction::Left), Some(0));
  }

  #[test]
  fn neighbour_follows_the_grid() {
    let boxes = [
      Rect {
        x: 0,
        y: 0,
        width: 11,
        height: 11,
      },
      Rect {
        x: 10,
        y: 0,
        width: 11,
        height: 6,
      },
      Rect {
        x: 10,
        y: 5,
        width: 11,
        height: 6,
      },
    ];
    assert_eq!(neighbour(&boxes, 0, Direction::Right), Some(1));
    assert_eq!(neighbour(&boxes, 1, Direction::Down), Some(2));
    assert_eq!(neighbour(&boxes, 2, Direction::Left), Some(0));
    assert_eq!(neighbour(&boxes, 0, Direction::Left), None);
    assert_eq!(neighbour(&boxes, 1, Direction::Up), None);
  }
}
