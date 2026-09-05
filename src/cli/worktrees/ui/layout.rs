//! Pane-group geometry (issue #1585 §4/§5, Phase 4b): how the terminal
//! side's vertical stack of groups is sized, where the splitters are, and
//! what a splitter drag does.
//!
//! The model is a **stack**, not a recursive split tree
//! ([ADR-0072](../../../../docs/adrs/adr-0072.md) §7): `weights` holds one
//! relative weight per group, top to bottom. That is what the request asked
//! for (top-vs-bottom) at a fraction of the layout, focus-traversal and
//! serialization complexity, and a nested variant stays additive.
//!
//! Everything here is pure `Rect` arithmetic over a weight vector — no
//! terminal, no PTY — so the drag and clamping rules are unit-tested
//! directly.

use ratatui::layout::Rect;

/// The smallest a group may be dragged to: its tab strip, its two border
/// rows, and one row of grid.
pub const MIN_GROUP_HEIGHT: u16 = 4;

/// A splitter's hit box is 3 rows tall — the boundary row and one either
/// side. A 1-row target is unusable with a mouse (issue #1585 §5).
/// [`mouse`](super::mouse) takes its reach from this.
pub const SPLITTER_HIT_HEIGHT: u16 = 3;

/// The boundary row between two adjacent group rects — the row a splitter
/// is drawn on and dragged by. `app.rs` derives the live splitter rows from
/// the rects it actually rendered rather than recomputing a split, so this
/// is the shared definition of "where the boundary is".
pub fn boundary_row(rect: Rect) -> u16 {
    rect.y + rect.height.saturating_sub(1)
}

/// Splits `area` into one rect per weight, top to bottom.
///
/// Heights are proportional to `weights`, then corrected so every group is
/// at least [`MIN_GROUP_HEIGHT`] (taking rows from the tallest groups) and
/// the rects exactly tile `area` with no gap or overlap. When `area` is too
/// short to give everyone the minimum, the groups that do not fit are
/// dropped from the result — the caller renders only what came back.
pub fn split_groups(area: Rect, weights: &[u16]) -> Vec<Rect> {
    if weights.is_empty() || area.height == 0 {
        return Vec::new();
    }
    let capacity = (area.height / MIN_GROUP_HEIGHT).max(1) as usize;
    let count = weights.len().min(capacity);
    let weights = &weights[..count];

    let total: u32 = weights.iter().map(|w| u32::from((*w).max(1))).sum();
    let height = u32::from(area.height);
    let mut heights: Vec<u16> = weights
        .iter()
        .map(|w| {
            let raw = height * u32::from((*w).max(1)) / total;
            u16::try_from(raw).unwrap_or(u16::MAX)
        })
        .collect();

    // Hand the rounding remainder to the first group, then lift anyone
    // below the minimum by taking rows from the tallest group that can
    // spare them. Both loops are bounded by the row count.
    let assigned: u16 = heights.iter().copied().sum();
    heights[0] += area.height.saturating_sub(assigned);
    for i in 0..heights.len() {
        while heights[i] < MIN_GROUP_HEIGHT {
            let Some(donor) = tallest_donor(&heights, i) else {
                break;
            };
            heights[donor] -= 1;
            heights[i] += 1;
        }
    }

    let mut y = area.y;
    let mut rects = Vec::with_capacity(count);
    for height in heights {
        rects.push(Rect::new(area.x, y, area.width, height));
        y += height;
    }
    rects
}

/// The index of the tallest group other than `skip` that can give up a row.
fn tallest_donor(heights: &[u16], skip: usize) -> Option<usize> {
    heights
        .iter()
        .enumerate()
        .filter(|(i, h)| *i != skip && **h > MIN_GROUP_HEIGHT)
        .max_by_key(|(_, h)| **h)
        .map(|(i, _)| i)
}

/// Drags splitter `index` to row `y`, returning the new weights.
///
/// Only the two groups either side of the boundary change, and their
/// combined size is preserved, so a drag never disturbs the rest of the
/// stack. Both are clamped to [`MIN_GROUP_HEIGHT`]: dragging to either
/// extreme pins the opposite group at exactly the minimum, never below.
pub fn drag_splitter(area: Rect, weights: &[u16], index: usize, y: u16) -> Vec<u16> {
    let mut next = weights.to_vec();
    let rects = split_groups(area, weights);
    if index + 1 >= rects.len() {
        return next;
    }
    let (upper, lower) = (rects[index], rects[index + 1]);
    let span = upper.height + lower.height;
    if span < MIN_GROUP_HEIGHT * 2 {
        return next; // no room to move the boundary either way
    }
    // The boundary is the upper group's last row, so its new height is the
    // distance from its top to `y`, inclusive.
    let wanted = y.saturating_sub(upper.y) + 1;
    let upper_height = wanted.clamp(MIN_GROUP_HEIGHT, span - MIN_GROUP_HEIGHT);

    // Weights are relative, so the *rendered heights* are themselves a
    // valid weight vector that reproduces this exact layout. Adopting them
    // keeps every other group's size untouched and — unlike rescaling the
    // pair's existing weights — cannot lose the drag to integer division
    // when those weights are small (`[1, 1]` has no room to express a
    // 4-row group in a 30-row pane).
    for (slot, rect) in next.iter_mut().zip(rects.iter()) {
        *slot = rect.height.max(1);
    }
    next[index] = upper_height;
    next[index + 1] = span - upper_height;
    next
}

/// Equal weights for `count` groups — `alt-0`'s reset, and the shape a new
/// group is added with.
pub fn even_weights(count: usize) -> Vec<u16> {
    vec![1; count]
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn heights(rects: &[Rect]) -> Vec<u16> {
        rects.iter().map(|r| r.height).collect()
    }

    #[test]
    fn one_group_takes_the_whole_area() {
        let area = Rect::new(10, 2, 40, 20);
        let rects = split_groups(area, &[1]);
        assert_eq!(rects, vec![area]);
    }

    #[test]
    fn groups_tile_the_area_exactly_with_no_gap_or_overlap() {
        let area = Rect::new(0, 5, 30, 21);
        for weights in [vec![1, 1], vec![2, 1], vec![1, 3, 1], vec![5, 1, 1, 2]] {
            let rects = split_groups(area, &weights);
            assert_eq!(rects.len(), weights.len(), "{weights:?}");
            assert_eq!(rects[0].y, area.y, "{weights:?}");
            let total: u16 = heights(&rects).iter().sum();
            assert_eq!(total, area.height, "{weights:?}");
            for pair in rects.windows(2) {
                assert_eq!(pair[1].y, pair[0].y + pair[0].height, "{weights:?}");
            }
            assert!(rects.iter().all(|r| r.height >= MIN_GROUP_HEIGHT));
        }
    }

    #[test]
    fn weights_are_respected_in_proportion() {
        let rects = split_groups(Rect::new(0, 0, 20, 30), &[2, 1]);
        assert_eq!(heights(&rects), vec![20, 10]);
    }

    #[test]
    fn a_group_below_the_minimum_is_lifted_out_of_the_tallest() {
        // 1:99 would give the first group 0 rows.
        let rects = split_groups(Rect::new(0, 0, 20, 40), &[1, 99]);
        assert_eq!(rects[0].height, MIN_GROUP_HEIGHT);
        assert_eq!(rects[1].height, 40 - MIN_GROUP_HEIGHT);
    }

    #[test]
    fn groups_that_cannot_fit_the_minimum_are_dropped() {
        // 9 rows fits two groups of 4, not three.
        let rects = split_groups(Rect::new(0, 0, 20, 9), &[1, 1, 1]);
        assert_eq!(rects.len(), 2);
        assert_eq!(heights(&rects).iter().sum::<u16>(), 9);
        // A single row still yields one (clipped) group rather than none.
        assert_eq!(split_groups(Rect::new(0, 0, 20, 1), &[1, 1]).len(), 1);
        assert!(split_groups(Rect::new(0, 0, 20, 0), &[1]).is_empty());
        assert!(split_groups(Rect::new(0, 0, 20, 10), &[]).is_empty());
    }

    #[test]
    fn the_boundary_row_is_a_groups_last_row() {
        let area = Rect::new(0, 0, 20, 30);
        let rects = split_groups(area, &[1, 1]);
        assert_eq!(boundary_row(rects[0]), 14);
        assert_eq!(boundary_row(rects[0]), rects[1].y - 1);
        assert_eq!(boundary_row(Rect::new(0, 5, 10, 0)), 5, "an empty rect");
    }

    #[test]
    fn dragging_a_splitter_moves_only_its_two_groups() {
        let area = Rect::new(0, 0, 20, 30);
        let weights = [1, 1, 1];
        let before = split_groups(area, &weights);
        let after = split_groups(area, &drag_splitter(area, &weights, 0, 3));
        // The third group is untouched; the first shrank and second grew.
        assert_eq!(after[2].height, before[2].height);
        assert!(after[0].height < before[0].height);
        assert_eq!(
            after[0].height + after[1].height,
            before[0].height + before[1].height
        );
    }

    #[test]
    fn dragging_to_either_extreme_clamps_the_opposite_group_to_the_minimum() {
        let area = Rect::new(0, 0, 20, 30);
        let weights = [1, 1];

        let up = split_groups(area, &drag_splitter(area, &weights, 0, 0));
        assert_eq!(up[0].height, MIN_GROUP_HEIGHT);
        assert_eq!(up[0].height + up[1].height, 30);

        let down = split_groups(area, &drag_splitter(area, &weights, 0, 29));
        assert_eq!(down[1].height, MIN_GROUP_HEIGHT);
        assert_eq!(down[0].height + down[1].height, 30);

        // Dragging far past either end clamps identically, never below.
        let way_down = split_groups(area, &drag_splitter(area, &weights, 0, 500));
        assert_eq!(heights(&way_down), heights(&down));
    }

    #[test]
    fn dragging_a_splitter_that_does_not_exist_changes_nothing() {
        let area = Rect::new(0, 0, 20, 30);
        assert_eq!(drag_splitter(area, &[1, 1], 5, 10), vec![1, 1]);
        assert_eq!(drag_splitter(area, &[1], 0, 10), vec![1]);
        assert_eq!(
            drag_splitter(Rect::new(0, 0, 20, 0), &[1, 1], 0, 0),
            vec![1, 1]
        );
    }

    #[test]
    fn a_drag_round_trips_back_to_where_it_started() {
        let area = Rect::new(0, 0, 20, 40);
        let weights = vec![1, 1];
        let original = boundary_row(split_groups(area, &weights)[0]);
        let dragged = drag_splitter(area, &weights, 0, 10);
        let back = drag_splitter(area, &dragged, 0, original);
        assert_eq!(boundary_row(split_groups(area, &back)[0]), original);
    }

    #[test]
    fn even_weights_resets_the_stack() {
        assert_eq!(even_weights(3), vec![1, 1, 1]);
        let area = Rect::new(0, 0, 20, 30);
        let rects = split_groups(area, &even_weights(3));
        assert_eq!(heights(&rects), vec![10, 10, 10]);
    }
}
