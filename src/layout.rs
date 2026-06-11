// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::pty::PtySize;

// Below these content dimensions a pane is not usable: the header row alone
// consumes the first line, and a shell cannot render meaningfully in
// fewer than a handful of columns. Splits that would push any pane under
// the minimum are refused by the caller.
pub const MIN_PANE_WIDTH: u16 = 4;
pub const MIN_PANE_HEIGHT: u16 = 3;

pub type PaneId = usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl Rect {
    pub fn contains(&self, col: u16, row: u16) -> bool {
        col >= self.x
            && col < self.x.saturating_add(self.width)
            && row >= self.y
            && row < self.y.saturating_add(self.height)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneLayout {
    pub frame: Rect,
    pub content: Rect,
}

impl PaneLayout {
    pub fn from_frame(frame: Rect) -> Self {
        // Row 0 of each frame is the header; the rest is content. A
        // frame squeezed to 0 or 1 rows gets zero-height content —
        // NOT a 1-row minimum, which would escape the frame and draw
        // over whatever pane sits below. `pty_size()` still clamps
        // the PTY itself to 1x1 so the child process stays alive.
        let header_height = 1u16;
        let content = Rect {
            x: frame.x,
            y: frame.y.saturating_add(header_height),
            width: frame.width,
            height: frame.height.saturating_sub(header_height),
        };
        Self { frame, content }
    }

    pub fn content_contains(&self, col: u16, row: u16) -> bool {
        self.content.contains(col, row)
    }

    pub fn content_position(&self, col: u16, row: u16) -> Option<(u16, u16)> {
        if !self.content_contains(col, row) {
            return None;
        }
        Some((col - self.content.x, row - self.content.y))
    }

    pub fn pty_size(&self) -> PtySize {
        PtySize::new(self.content.height.max(1), self.content.width.max(1))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitOrientation {
    // Children arranged left-to-right; each child gets a slice of the
    // parent's width. This is what `Ctrl-a |` produces.
    Columns,
    // Children stacked top-to-bottom; each child gets a slice of the
    // parent's height. This is what `Ctrl-a -` produces.
    Rows,
}

// Default weight given to every new child in a Split; total width/height
// within the split is divided proportionally. Starting at 10 (rather than
// 1) means one resize step is 1/(10*N) of the split — granular enough to
// feel smooth without making the data structure exotic.
pub const DEFAULT_CHILD_WEIGHT: u16 = 10;
// A child is not allowed to shrink below this; callers that would push it
// lower should treat the resize as a no-op.
pub const MIN_CHILD_WEIGHT: u16 = 1;

// The tree is the mutable source of truth for workspace structure.
// `Leaf` holds a stable `PaneId` that outlives any Vec reorg. `weights`
// always has the same length as `children` — a missing weight is a bug.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutNode {
    Leaf(PaneId),
    Split {
        orientation: SplitOrientation,
        children: Vec<LayoutNode>,
        weights: Vec<u16>,
    },
}

// Direction that a resize request moves the active pane's border. Only
// the axis implied by the name matters; the sign is captured here so the
// tree walk can look for the appropriate sibling direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeDirection {
    Left,  // shift active's left edge leftward → steal from previous column
    Right, // shift active's right edge rightward → steal from next column
    Up,    // shift active's top edge upward → steal from previous row
    Down,  // shift active's bottom edge downward → steal from next row
}

// Line-segment description for a divider between two sibling children of a
// single Split. Rendering code draws '|' or '-' along the segment; at
// intersections with perpendicular separators the caller overdraws '+'.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Separator {
    pub orientation: SplitOrientation,
    pub x: u16,
    pub y: u16,
    pub length: u16,
}

// Flat renderable snapshot computed from a tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceLayout {
    pub size: PtySize,
    pub status_row: u16,
    pub panes: Vec<(PaneId, PaneLayout)>,
    pub separators: Vec<Separator>,
}

impl WorkspaceLayout {
    pub fn pane_count(&self) -> usize {
        self.panes.len()
    }

    // Finds the pane that contains the given global coordinate — by frame,
    // not content. Useful for mouse focus routing.
    pub fn pane_at(&self, col: u16, row: u16) -> Option<PaneId> {
        self.panes
            .iter()
            .find(|(_, pane)| pane.frame.contains(col, row))
            .map(|(id, _)| *id)
    }

    pub fn content_position(&self, col: u16, row: u16) -> Option<(PaneId, u16, u16)> {
        self.panes
            .iter()
            .find_map(|(id, pane)| pane.content_position(col, row).map(|(c, r)| (*id, c, r)))
    }

    pub fn pane_frame(&self, pane_id: PaneId) -> Option<PaneLayout> {
        self.panes
            .iter()
            .find(|(id, _)| *id == pane_id)
            .map(|(_, pane)| *pane)
    }

    // Is `(col, row)` on top of one of the border separators? Used by
    // the drag-resize mouse handler so clicks on a pane boundary grab
    // the border rather than starting a selection in the pane below.
    // Vertical separators occupy one column at (sep.x, sep.y..sep.y+length);
    // row separators one row at (sep.x..sep.x+length, sep.y).
    pub fn separator_at(&self, col: u16, row: u16) -> Option<Separator> {
        self.separators
            .iter()
            .copied()
            .find(|sep| match sep.orientation {
                SplitOrientation::Columns => {
                    col == sep.x && row >= sep.y && row < sep.y.saturating_add(sep.length)
                }
                SplitOrientation::Rows => {
                    row == sep.y && col >= sep.x && col < sep.x.saturating_add(sep.length)
                }
            })
    }
}

impl LayoutNode {
    pub fn leaf(pane_id: PaneId) -> Self {
        Self::Leaf(pane_id)
    }

    // Walk the tree and return all pane ids in pre-order. This is the order
    // used by `cycle_active` / `cycle_active_backward`.
    pub fn leaves(&self) -> Vec<PaneId> {
        let mut ids = Vec::new();
        self.collect_leaves(&mut ids);
        ids
    }

    fn collect_leaves(&self, ids: &mut Vec<PaneId>) {
        match self {
            LayoutNode::Leaf(id) => ids.push(*id),
            LayoutNode::Split { children, .. } => {
                for child in children {
                    child.collect_leaves(ids);
                }
            }
        }
    }

    // Insert `new_pane` next to `target` with the requested orientation.
    // If target's parent Split already has the same orientation, the new
    // pane is added as a sibling directly after target (keeps the tree
    // flat when possible). Otherwise, target is replaced with a new Split
    // whose children are [target, new_pane]. Returns true if target was
    // found and the insertion happened.
    pub fn split_at(
        &mut self,
        target: PaneId,
        new_pane: PaneId,
        orientation: SplitOrientation,
    ) -> bool {
        // Special-case the root being the target Leaf: replace root with
        // a 2-child Split. This mirrors the sibling-insertion logic below
        // when there's no enclosing Split yet.
        if let LayoutNode::Leaf(id) = self {
            if *id == target {
                *self = LayoutNode::Split {
                    orientation,
                    children: vec![LayoutNode::Leaf(target), LayoutNode::Leaf(new_pane)],
                    weights: vec![DEFAULT_CHILD_WEIGHT, DEFAULT_CHILD_WEIGHT],
                };
                return true;
            }
            return false;
        }

        if let LayoutNode::Split {
            orientation: self_orientation,
            children,
            weights,
        } = self
        {
            // Check if target is a direct child and we can insert as a
            // sibling (when orientations match). The new sibling inherits
            // the default weight so existing children keep their relative
            // sizes and the newcomer gets an even share.
            if *self_orientation == orientation {
                for index in 0..children.len() {
                    if let LayoutNode::Leaf(id) = &children[index]
                        && *id == target
                    {
                        children.insert(index + 1, LayoutNode::Leaf(new_pane));
                        weights.insert(index + 1, DEFAULT_CHILD_WEIGHT);
                        return true;
                    }
                }
            }

            // Otherwise walk into each child. If a child is the target
            // leaf, replace it with a sub-Split; if it's a non-leaf,
            // recurse into it.
            for child in children.iter_mut() {
                match child {
                    LayoutNode::Leaf(id) if *id == target => {
                        *child = LayoutNode::Split {
                            orientation,
                            children: vec![LayoutNode::Leaf(target), LayoutNode::Leaf(new_pane)],
                            weights: vec![DEFAULT_CHILD_WEIGHT, DEFAULT_CHILD_WEIGHT],
                        };
                        return true;
                    }
                    LayoutNode::Leaf(_) => continue,
                    node => {
                        if node.split_at(target, new_pane, orientation) {
                            return true;
                        }
                    }
                }
            }
        }

        false
    }

    // Nudges the active pane's border in the requested direction by one
    // weight unit. Walks the tree looking for the nearest ancestor Split
    // whose orientation matches the requested axis AND where the target
    // is a direct child with a neighbor we can steal weight from. Returns
    // true iff the adjustment happened; false when there's nothing to
    // resize (e.g. root is a single leaf, or neighbor is already at
    // MIN_CHILD_WEIGHT).
    pub fn resize_pane(&mut self, target: PaneId, direction: ResizeDirection) -> bool {
        let axis = match direction {
            ResizeDirection::Left | ResizeDirection::Right => SplitOrientation::Columns,
            ResizeDirection::Up | ResizeDirection::Down => SplitOrientation::Rows,
        };
        let steal_forward = matches!(direction, ResizeDirection::Right | ResizeDirection::Down,);

        if let LayoutNode::Split {
            orientation,
            children,
            weights,
        } = self
            && *orientation == axis
            && let Some(target_index) = children
                .iter()
                .position(|child| matches!(child, LayoutNode::Leaf(id) if *id == target))
        {
            // Neighbor is the immediate next (or previous) sibling.
            let neighbor_index = if steal_forward {
                if target_index + 1 >= children.len() {
                    // No neighbor in this direction; try ancestors instead.
                    return self.resize_pane_in_children(target, direction);
                }
                target_index + 1
            } else {
                if target_index == 0 {
                    return self.resize_pane_in_children(target, direction);
                }
                target_index - 1
            };

            if weights[neighbor_index] <= MIN_CHILD_WEIGHT {
                return false;
            }
            weights[neighbor_index] -= 1;
            weights[target_index] += 1;
            return true;
        }

        self.resize_pane_in_children(target, direction)
    }

    fn resize_pane_in_children(&mut self, target: PaneId, direction: ResizeDirection) -> bool {
        if let LayoutNode::Split { children, .. } = self {
            for child in children.iter_mut() {
                if matches!(child, LayoutNode::Split { .. }) && child.resize_pane(target, direction)
                {
                    return true;
                }
            }
        }
        false
    }

    // Remove the leaf for `target`. If removing it leaves a Split with
    // only one child, that Split is collapsed into its surviving child.
    // Returns true if the target was found and removed.
    // Swap two leaves' PaneIds in-place. Tree structure and weights stay
    // exactly the same — only which PaneId sits at each slot changes. Used
    // by the pane swap bindings so the user can reposition a pane in the
    // layout without destroying and recreating splits.
    pub fn swap_leaves(&mut self, a: PaneId, b: PaneId) -> bool {
        if a == b {
            return false;
        }
        // Two-pass: verify BOTH leaves exist before mutating. An earlier
        // version rewrote eagerly and could corrupt the tree when only
        // one of the two ids was present (rewrote it to the missing
        // id, then reported failure).
        let leaves = self.leaves();
        if !leaves.contains(&a) || !leaves.contains(&b) {
            return false;
        }
        self.rewrite_leaves(&mut |id| {
            if *id == a {
                *id = b;
            } else if *id == b {
                *id = a;
            }
        });
        true
    }

    fn rewrite_leaves(&mut self, rewrite: &mut dyn FnMut(&mut PaneId)) {
        match self {
            LayoutNode::Leaf(id) => rewrite(id),
            LayoutNode::Split { children, .. } => {
                for child in children.iter_mut() {
                    child.rewrite_leaves(rewrite);
                }
            }
        }
    }

    pub fn remove_leaf(&mut self, target: PaneId) -> bool {
        // A Leaf at the root is a degenerate case — there's nothing left
        // to host a workspace, so refuse the removal.
        if let LayoutNode::Leaf(_) = self {
            return false;
        }

        let removed = self.remove_leaf_recursive(target);
        if removed {
            self.collapse_single_child_splits();
        }
        removed
    }

    fn remove_leaf_recursive(&mut self, target: PaneId) -> bool {
        if let LayoutNode::Split {
            children, weights, ..
        } = self
        {
            // First check direct children for a Leaf match.
            if let Some(index) = children
                .iter()
                .position(|child| matches!(child, LayoutNode::Leaf(id) if *id == target))
            {
                children.remove(index);
                weights.remove(index);
                return true;
            }

            // Then recurse into non-leaf children.
            for child in children.iter_mut() {
                if matches!(child, LayoutNode::Split { .. }) && child.remove_leaf_recursive(target)
                {
                    return true;
                }
            }
        }
        false
    }

    // Walk the tree and replace any Split with exactly one child with
    // that child. Applied after a remove to keep the tree tight.
    fn collapse_single_child_splits(&mut self) {
        loop {
            if let LayoutNode::Split { children, .. } = self
                && children.len() == 1
            {
                let only = children.remove(0);
                *self = only;
                continue; // re-check: the promoted node might also need collapsing
            }

            if let LayoutNode::Split { children, .. } = self {
                for child in children.iter_mut() {
                    child.collapse_single_child_splits();
                }
            }
            break;
        }
    }

    // Compute a flat snapshot from this tree for the given terminal size.
    // Rows is split into body + status row; the tree is laid out within
    // the body, and the caller overlays the status bar on `status_row`.
    pub fn compute(&self, size: PtySize) -> WorkspaceLayout {
        let body_rows = size.rows.saturating_sub(1).max(2);
        let body = Rect {
            x: 0,
            y: 0,
            width: size.cols.max(MIN_PANE_WIDTH),
            height: body_rows,
        };

        let mut panes = Vec::new();
        let mut separators = Vec::new();
        self.lay_out(body, &mut panes, &mut separators);

        WorkspaceLayout {
            size,
            status_row: body_rows,
            panes,
            separators,
        }
    }

    fn lay_out(
        &self,
        rect: Rect,
        panes: &mut Vec<(PaneId, PaneLayout)>,
        separators: &mut Vec<Separator>,
    ) {
        match self {
            LayoutNode::Leaf(id) => {
                panes.push((*id, PaneLayout::from_frame(rect)));
            }
            LayoutNode::Split {
                orientation,
                children,
                weights,
            } => {
                if children.is_empty() {
                    return;
                }

                let n = children.len() as u16;
                let divider_count = n.saturating_sub(1);

                // Distribute the available space across children in
                // proportion to their weights. Remainder is spread left
                // to right so the final pane ends exactly on the right
                // edge.
                let allocate = |available: u16, weights: &[u16]| -> Vec<u16> {
                    let total: u32 = weights.iter().map(|w| *w as u32).sum();
                    if total == 0 {
                        return vec![1; weights.len()];
                    }
                    let mut sizes: Vec<u16> = weights
                        .iter()
                        .map(|weight| {
                            (u32::from(available) * u32::from(*weight) / total).min(u16::MAX as u32)
                                as u16
                        })
                        .collect();
                    let consumed: u16 = sizes.iter().sum();
                    let mut remainder = available.saturating_sub(consumed);
                    let mut index = 0;
                    while remainder > 0 && !sizes.is_empty() {
                        sizes[index] = sizes[index].saturating_add(1);
                        remainder -= 1;
                        index = (index + 1) % sizes.len();
                    }
                    for size in sizes.iter_mut() {
                        *size = (*size).max(1);
                    }
                    sizes
                };

                match orientation {
                    SplitOrientation::Columns => {
                        let available = rect.width.saturating_sub(divider_count);
                        let widths = allocate(available, weights);
                        // Children are clipped to the parent rect: the
                        // allocator's 1-cell floor can hand out more
                        // total width than `available` on an over-
                        // constrained split, and an unclipped child
                        // would spill into the parent's siblings (panes
                        // drawing over each other). Trailing children
                        // degrade to zero-width instead.
                        let parent_right = rect.x.saturating_add(rect.width);
                        let mut cursor_x = rect.x;
                        for (index, (child, width)) in
                            children.iter().zip(widths.iter()).enumerate()
                        {
                            let width = (*width).min(parent_right.saturating_sub(cursor_x));
                            let child_rect = Rect {
                                x: cursor_x,
                                y: rect.y,
                                width,
                                height: rect.height,
                            };
                            child.lay_out(child_rect, panes, separators);
                            cursor_x = cursor_x.saturating_add(width).min(parent_right);
                            if index + 1 < children.len() {
                                separators.push(Separator {
                                    orientation: SplitOrientation::Columns,
                                    x: cursor_x,
                                    y: rect.y,
                                    length: rect.height,
                                });
                                cursor_x = cursor_x.saturating_add(1).min(parent_right);
                            }
                        }
                    }
                    SplitOrientation::Rows => {
                        let available = rect.height.saturating_sub(divider_count);
                        let heights = allocate(available, weights);
                        // Same clipping as the Columns branch above.
                        let parent_bottom = rect.y.saturating_add(rect.height);
                        let mut cursor_y = rect.y;
                        for (index, (child, height)) in
                            children.iter().zip(heights.iter()).enumerate()
                        {
                            let height = (*height).min(parent_bottom.saturating_sub(cursor_y));
                            let child_rect = Rect {
                                x: rect.x,
                                y: cursor_y,
                                width: rect.width,
                                height,
                            };
                            child.lay_out(child_rect, panes, separators);
                            cursor_y = cursor_y.saturating_add(height).min(parent_bottom);
                            if index + 1 < children.len() {
                                separators.push(Separator {
                                    orientation: SplitOrientation::Rows,
                                    x: rect.x,
                                    y: cursor_y,
                                    length: rect.width,
                                });
                                cursor_y = cursor_y.saturating_add(1).min(parent_bottom);
                            }
                        }
                    }
                }
            }
        }
    }

    // Returns true if adding another leaf next to `target` with the given
    // orientation would still leave every pane at or above the minimum
    // dimensions. The caller uses this to refuse splits on terminals that
    // are too narrow / short for another pane.
    pub fn fits_after_split(
        &self,
        size: PtySize,
        target: PaneId,
        orientation: SplitOrientation,
    ) -> bool {
        let mut hypothetical = self.clone();
        // Use a sentinel id that is guaranteed to be unique in this
        // hypothetical tree (caller-provided ids are non-negative and
        // small; usize::MAX is reserved here).
        let probe = usize::MAX;
        if !hypothetical.split_at(target, probe, orientation) {
            return false;
        }
        let layout = hypothetical.compute(size);
        layout.panes.iter().all(|(_, pane)| {
            pane.content.width >= MIN_PANE_WIDTH
                && pane.content.height >= 1
                && pane.frame.height >= MIN_PANE_HEIGHT
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{LayoutNode, ResizeDirection, SplitOrientation, WorkspaceLayout};
    use crate::PtySize;

    fn ids_in(layout: &WorkspaceLayout) -> Vec<usize> {
        layout.panes.iter().map(|(id, _)| *id).collect()
    }

    #[test]
    fn single_leaf_compute_produces_one_pane() {
        let tree = LayoutNode::leaf(1);
        let layout = tree.compute(PtySize::new(24, 80));

        assert_eq!(layout.panes.len(), 1);
        assert_eq!(layout.separators.len(), 0);
        let (id, pane) = layout.panes[0];
        assert_eq!(id, 1);
        assert_eq!(pane.frame.x, 0);
        assert_eq!(pane.frame.y, 0);
        assert_eq!(pane.frame.width, 80);
        assert_eq!(pane.frame.height, 23); // 24 - 1 status row
    }

    #[test]
    fn column_split_at_root_produces_two_panes_side_by_side() {
        let mut tree = LayoutNode::leaf(1);
        assert!(tree.split_at(1, 2, SplitOrientation::Columns));

        let layout = tree.compute(PtySize::new(24, 80));
        assert_eq!(ids_in(&layout), vec![1, 2]);
        assert_eq!(layout.separators.len(), 1);

        let [left, right] = [layout.panes[0].1, layout.panes[1].1];
        assert_eq!(left.frame.x, 0);
        assert_eq!(right.frame.x, left.frame.width + 1); // +1 for separator
    }

    #[test]
    fn row_split_at_root_stacks_panes() {
        let mut tree = LayoutNode::leaf(1);
        assert!(tree.split_at(1, 2, SplitOrientation::Rows));

        let layout = tree.compute(PtySize::new(24, 80));
        assert_eq!(ids_in(&layout), vec![1, 2]);
        assert_eq!(layout.separators.len(), 1);

        let [top, bottom] = [layout.panes[0].1, layout.panes[1].1];
        assert_eq!(top.frame.y, 0);
        assert_eq!(bottom.frame.y, top.frame.height + 1);
    }

    #[test]
    fn matching_orientation_flattens_siblings_instead_of_nesting() {
        let mut tree = LayoutNode::leaf(1);
        tree.split_at(1, 2, SplitOrientation::Columns);
        tree.split_at(2, 3, SplitOrientation::Columns);

        // Expect a single Split with three Leaf children, not a deeply
        // nested tree.
        match &tree {
            LayoutNode::Split {
                orientation,
                children,
                weights,
            } => {
                assert_eq!(*orientation, SplitOrientation::Columns);
                assert_eq!(children.len(), 3);
                assert_eq!(weights.len(), 3, "weights track children 1:1");
                for (expected, child) in [1, 2, 3].iter().zip(children) {
                    match child {
                        LayoutNode::Leaf(id) => assert_eq!(id, expected),
                        other => panic!("expected leaf, got {other:?}"),
                    }
                }
            }
            other => panic!("expected Split at root, got {other:?}"),
        }
    }

    #[test]
    fn mixed_orientation_creates_a_nested_split() {
        let mut tree = LayoutNode::leaf(1);
        tree.split_at(1, 2, SplitOrientation::Columns);
        // Now split pane 2 horizontally (Rows) — introduces a nested Split.
        tree.split_at(2, 3, SplitOrientation::Rows);

        let layout = tree.compute(PtySize::new(24, 80));
        assert_eq!(ids_in(&layout), vec![1, 2, 3]);
        // Two separators: the outer column separator, and the inner row
        // separator inside the right column.
        assert_eq!(layout.separators.len(), 2);
    }

    #[test]
    fn remove_leaf_collapses_single_child_splits() {
        let mut tree = LayoutNode::leaf(1);
        tree.split_at(1, 2, SplitOrientation::Columns);
        tree.split_at(2, 3, SplitOrientation::Rows);
        // Tree is: Split(Cols, [Leaf(1), Split(Rows, [Leaf(2), Leaf(3)])]).

        assert!(tree.remove_leaf(3));
        // The inner Split should have collapsed back into just Leaf(2),
        // so the outer Split now has two direct Leaf children.
        match &tree {
            LayoutNode::Split { children, .. } => {
                assert_eq!(children.len(), 2);
                for (expected, child) in [1, 2].iter().zip(children) {
                    match child {
                        LayoutNode::Leaf(id) => assert_eq!(id, expected),
                        other => panic!("expected leaf after collapse, got {other:?}"),
                    }
                }
            }
            other => panic!("expected Split after collapse, got {other:?}"),
        }
    }

    #[test]
    fn remove_leaf_can_shrink_root_to_a_single_leaf() {
        let mut tree = LayoutNode::leaf(1);
        tree.split_at(1, 2, SplitOrientation::Columns);
        assert!(tree.remove_leaf(2));
        assert_eq!(tree, LayoutNode::Leaf(1));
    }

    #[test]
    fn remove_leaf_refuses_to_empty_the_root() {
        let mut tree = LayoutNode::leaf(1);
        // Cannot remove the only leaf; caller must reject the close.
        assert!(!tree.remove_leaf(1));
        assert_eq!(tree, LayoutNode::Leaf(1));
    }

    #[test]
    fn leaves_walk_in_pre_order() {
        let mut tree = LayoutNode::leaf(1);
        tree.split_at(1, 2, SplitOrientation::Columns);
        tree.split_at(1, 3, SplitOrientation::Rows);
        // Tree: Split(Cols, [Split(Rows, [Leaf(1), Leaf(3)]), Leaf(2)])
        assert_eq!(tree.leaves(), vec![1, 3, 2]);
    }

    #[test]
    fn content_position_translates_into_the_correct_pane() {
        let mut tree = LayoutNode::leaf(1);
        tree.split_at(1, 2, SplitOrientation::Columns);
        let layout = tree.compute(PtySize::new(24, 80));

        // y=0 is the pane header row — not content. y=1 is the first
        // content row.
        assert_eq!(layout.content_position(0, 1).map(|(id, _, _)| id), Some(1));
        assert_eq!(layout.content_position(60, 1).map(|(id, _, _)| id), Some(2));
        assert_eq!(layout.content_position(0, 0), None);
    }

    #[test]
    fn resize_shifts_weight_from_neighbor_toward_target() {
        let mut tree = LayoutNode::leaf(1);
        tree.split_at(1, 2, SplitOrientation::Columns);
        // Default weights are (10, 10). Growing pane 1 rightward (steals
        // from pane 2) should bump 1 to 11 and drop 2 to 9.
        assert!(tree.resize_pane(1, ResizeDirection::Right));
        match &tree {
            LayoutNode::Split { weights, .. } => assert_eq!(weights, &vec![11, 10 - 1]),
            other => panic!("expected split at root, got {other:?}"),
        }
    }

    #[test]
    fn resize_refuses_when_neighbor_would_drop_below_minimum() {
        let mut tree = LayoutNode::leaf(1);
        tree.split_at(1, 2, SplitOrientation::Columns);
        // Repeatedly growing pane 1 eats into pane 2 until pane 2 hits
        // MIN_CHILD_WEIGHT; the next call must refuse rather than drive
        // the layout to zero.
        for _ in 0..100 {
            if !tree.resize_pane(1, ResizeDirection::Right) {
                break;
            }
        }
        assert!(!tree.resize_pane(1, ResizeDirection::Right));
        match &tree {
            LayoutNode::Split { weights, .. } => {
                assert_eq!(weights[1], super::MIN_CHILD_WEIGHT);
            }
            other => panic!("expected split, got {other:?}"),
        }
    }

    #[test]
    fn resize_along_wrong_axis_is_a_noop_at_root_column_split() {
        let mut tree = LayoutNode::leaf(1);
        tree.split_at(1, 2, SplitOrientation::Columns);
        // Up/Down is a row-axis request on a column-only tree — there's
        // no row-split anywhere, so the call should refuse.
        assert!(!tree.resize_pane(1, ResizeDirection::Up));
        assert!(!tree.resize_pane(1, ResizeDirection::Down));
    }

    #[test]
    fn resize_descends_into_nested_row_split() {
        // Tree: Split(Cols, [Leaf(1), Split(Rows, [Leaf(2), Leaf(3)])])
        let mut tree = LayoutNode::leaf(1);
        tree.split_at(1, 2, SplitOrientation::Columns);
        tree.split_at(2, 3, SplitOrientation::Rows);

        // Growing pane 2 downward should steal from the Rows sibling 3.
        assert!(tree.resize_pane(2, ResizeDirection::Down));
        // Growing pane 1 upward finds no Rows ancestor → refused.
        assert!(!tree.resize_pane(1, ResizeDirection::Up));
    }

    #[test]
    fn fits_after_split_rejects_too_narrow_or_too_short_terminals() {
        let mut tree = LayoutNode::leaf(1);
        tree.split_at(1, 2, SplitOrientation::Columns);

        // 9 cols can fit exactly 2 panes (2 * 4 + 1 divider = 9). Adding
        // a third column would need 14+, so this must fail.
        assert!(!tree.fits_after_split(PtySize::new(24, 9), 2, SplitOrientation::Columns));

        // Row splits need at least MIN_PANE_HEIGHT per pane plus dividers.
        // A 6-row terminal gives 5 body rows — not enough for 2 Rows
        // panes (2 * 3 + 1 = 7). Should refuse.
        assert!(!tree.fits_after_split(PtySize::new(6, 80), 1, SplitOrientation::Rows));

        // A comfortable terminal should accept.
        assert!(tree.fits_after_split(PtySize::new(40, 120), 1, SplitOrientation::Rows));
    }

    #[test]
    fn swap_leaves_exchanges_pane_ids_in_place() {
        let mut tree = LayoutNode::leaf(1);
        tree.split_at(1, 2, SplitOrientation::Columns);
        tree.split_at(2, 3, SplitOrientation::Rows);
        assert_eq!(tree.leaves(), vec![1, 2, 3]);

        // Swap 1 and 3 — tree structure unchanged, only ids rewrite.
        assert!(tree.swap_leaves(1, 3));
        assert_eq!(tree.leaves(), vec![3, 2, 1]);

        // Swap back; idempotent paired with itself.
        assert!(tree.swap_leaves(1, 3));
        assert_eq!(tree.leaves(), vec![1, 2, 3]);

        // Same id on both sides is a no-op false (prevents accidental
        // self-swap from claiming to have changed the tree).
        assert!(!tree.swap_leaves(2, 2));

        // Asking to swap with a missing id fails cleanly without
        // corrupting the tree.
        assert!(!tree.swap_leaves(2, 99));
        assert_eq!(tree.leaves(), vec![1, 2, 3]);
    }
}
