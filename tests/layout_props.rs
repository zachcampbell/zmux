// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

// Property tests for the layout tree: random sequences of
// split/remove/swap/resize ops, computed against terminal sizes from
// 0x0 up, asserting the structural invariants that the rest of the
// workspace relies on. Same deterministic-RNG approach as
// tests/vt_fuzz.rs; reproduce any failure from the printed seed.
//
// Knobs: ZMUX_LAYOUT_PROP_ITERS (default 2000), ZMUX_LAYOUT_PROP_SEED.

use std::env;

use zmux::layout::{LayoutNode, ResizeDirection, SplitOrientation};
use zmux::{PtySize, WorkspaceLayout};

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len() as u64) as usize]
    }
}

const SIZES: &[(u16, u16)] = &[
    (0, 0),
    (1, 1),
    (2, 2),
    (3, 3),
    (2, 80),
    (24, 2),
    (5, 5),
    (24, 80),
    (50, 200),
    (255, 255),
    (24, 65535),
    (65535, 80),
];

// Recursively verify the structural contract the rest of the code
// depends on: every Split has weights tracking children 1:1, at least
// one child, and no zero weights below MIN_CHILD_WEIGHT... weights of
// 0 are tolerated by `allocate` (it falls back to even sizing) but a
// children/weights length mismatch would panic indexing later.
fn assert_structure(node: &LayoutNode) {
    if let LayoutNode::Split {
        children, weights, ..
    } = node
    {
        assert_eq!(
            children.len(),
            weights.len(),
            "weights must track children 1:1"
        );
        assert!(!children.is_empty(), "Split with no children");
        for child in children {
            assert_structure(child);
        }
    }
}

fn assert_layout_sane(layout: &WorkspaceLayout, leaf_ids: &[usize], size: PtySize) {
    // Every leaf gets exactly one pane, regardless of how degenerate
    // the terminal size is.
    assert_eq!(
        layout.panes.len(),
        leaf_ids.len(),
        "compute() must produce one pane per leaf at size {size:?}"
    );
    for (id, pane) in &layout.panes {
        assert!(leaf_ids.contains(id), "unknown pane id {id} in layout");
        // Content rects may collapse to zero on over-constrained
        // splits (clipped, not spilled), but the PTY handed to the
        // child process must always be at least 1x1.
        let pty = pane.pty_size();
        assert!(pty.rows >= 1 && pty.cols >= 1);
        // Non-empty content stays inside its frame — a content rect
        // escaping the frame draws over the pane below. (Zero-area
        // content may park its origin past a zero-height frame; it
        // draws nothing, so that's fine.)
        if pane.content.height > 0 && pane.content.width > 0 {
            assert!(
                pane.content.y as u32 + pane.content.height as u32
                    <= pane.frame.y as u32 + pane.frame.height as u32,
                "content escapes frame at {size:?}: {pane:?}"
            );
        }
    }

    // Frames stay inside the body and never overlap — at EVERY size,
    // including 0x0 and over-constrained splits. lay_out clips
    // children to their parent rect; trailing children degrade to
    // zero area rather than spilling into siblings.
    let body_cols = size.cols.max(4) as u32; // compute() floors body width at MIN_PANE_WIDTH
    let body_rows = size.rows.saturating_sub(1).max(2) as u32;
    for (i, (id_a, a)) in layout.panes.iter().enumerate() {
        let right = a.frame.x as u32 + a.frame.width as u32;
        let bottom = a.frame.y as u32 + a.frame.height as u32;
        assert!(
            right <= body_cols,
            "pane {id_a} frame exceeds body width at {size:?}: {a:?}"
        );
        assert!(
            bottom <= body_rows,
            "pane {id_a} frame exceeds body height at {size:?}: {a:?}"
        );
        for (id_b, b) in layout.panes.iter().skip(i + 1) {
            let a_empty = a.frame.width == 0 || a.frame.height == 0;
            let b_empty = b.frame.width == 0 || b.frame.height == 0;
            let disjoint = a_empty
                || b_empty
                || right <= b.frame.x as u32
                || b.frame.x as u32 + b.frame.width as u32 <= a.frame.x as u32
                || bottom <= b.frame.y as u32
                || b.frame.y as u32 + b.frame.height as u32 <= a.frame.y as u32;
            assert!(
                disjoint,
                "panes {id_a} and {id_b} overlap at {size:?}: {a:?} vs {b:?}"
            );
        }
    }
}

fn env_u64(key: &str, default: u64) -> u64 {
    env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

#[test]
fn layout_tree_survives_random_op_sequences() {
    let iters = env_u64("ZMUX_LAYOUT_PROP_ITERS", 2000);
    let base_seed = env_u64("ZMUX_LAYOUT_PROP_SEED", 0x1A40);

    for i in 0..iters {
        let seed = base_seed.wrapping_add(i);
        let mut rng = Rng::new(seed);
        let mut tree = LayoutNode::leaf(0);
        let mut leaves: Vec<usize> = vec![0];
        let mut next_id = 1usize;

        let ops = 1 + rng.below(40);
        for _ in 0..ops {
            match rng.below(10) {
                // Split a random existing leaf (or a bogus id — must
                // be refused without corrupting the tree).
                0..=3 => {
                    let target = if rng.below(10) == 0 {
                        9999 + rng.below(10) as usize // bogus
                    } else {
                        rng.pick(&leaves)
                    };
                    let orientation = if rng.below(2) == 0 {
                        SplitOrientation::Columns
                    } else {
                        SplitOrientation::Rows
                    };
                    let id = next_id;
                    if tree.split_at(target, id, orientation) {
                        assert!(
                            leaves.contains(&target),
                            "seed {seed:#x}: split_at claimed success on bogus target {target}"
                        );
                        leaves.push(id);
                        next_id += 1;
                    } else {
                        assert!(
                            !leaves.contains(&target),
                            "seed {seed:#x}: split_at refused a live target {target}"
                        );
                    }
                }
                // Remove a leaf (sometimes bogus, sometimes the last
                // one standing — both must be clean refusals).
                4..=5 => {
                    let target = if rng.below(8) == 0 {
                        7777
                    } else {
                        rng.pick(&leaves)
                    };
                    if tree.remove_leaf(target) {
                        leaves.retain(|id| *id != target);
                        assert!(!leaves.is_empty(), "seed {seed:#x}: removed the last leaf");
                    }
                }
                // Swap two leaves (occasionally one bogus).
                6 => {
                    let a = rng.pick(&leaves);
                    let b = if rng.below(8) == 0 { 8888 } else { rng.pick(&leaves) };
                    let swapped = tree.swap_leaves(a, b);
                    let _ = swapped;
                }
                // Resize in a random direction, possibly many times to
                // drive weights to their floor.
                _ => {
                    let target = rng.pick(&leaves);
                    let dir = rng.pick(&[
                        ResizeDirection::Left,
                        ResizeDirection::Right,
                        ResizeDirection::Up,
                        ResizeDirection::Down,
                    ]);
                    let times = 1 + rng.below(30);
                    for _ in 0..times {
                        if !tree.resize_pane(target, dir) {
                            break;
                        }
                    }
                }
            }

            assert_structure(&tree);
            let mut tree_leaves = tree.leaves();
            tree_leaves.sort_unstable();
            let mut expected = leaves.clone();
            expected.sort_unstable();
            assert_eq!(
                tree_leaves, expected,
                "seed {seed:#x}: tree leaves diverged from model"
            );

            // Compute at a random size each step — including 0x0 and
            // u16::MAX extremes — and check the snapshot invariants.
            let (rows, cols) = rng.pick(SIZES);
            let layout = tree.compute(PtySize::new(rows, cols));
            assert_layout_sane(&layout, &leaves, PtySize::new(rows, cols));
        }

        // Tear the tree back down to a single leaf, computing at a
        // tiny size after each removal — the collapse path is where
        // single-child Splits historically lingered.
        while leaves.len() > 1 {
            let target = rng.pick(&leaves);
            if tree.remove_leaf(target) {
                leaves.retain(|id| *id != target);
                assert_structure(&tree);
                let layout = tree.compute(PtySize::new(3, 3));
                assert_layout_sane(&layout, &leaves, PtySize::new(3, 3));
            }
        }
    }
}

// Splits during zoom are a workspace-level concern (zoom is a render
// override, not a tree mutation), but the tree must support the
// underlying sequence: split while another pane is "fullscreen", then
// compute. This pins that compute() of a freshly split tree at the
// zoomed pane's full-body size stays sane.
#[test]
fn split_then_compute_at_full_body_size_is_sane() {
    let mut tree = LayoutNode::leaf(0);
    assert!(tree.split_at(0, 1, SplitOrientation::Columns));
    assert!(tree.split_at(1, 2, SplitOrientation::Rows));
    for (rows, cols) in [(1u16, 1u16), (2, 2), (24, 80), (65535, 65535)] {
        let layout = tree.compute(PtySize::new(rows, cols));
        assert_eq!(layout.pane_count(), 3);
        for (_, pane) in &layout.panes {
            // Content may clip to zero on tiny terminals, but the PTY
            // handed to the child must stay at least 1x1.
            let pty = pane.pty_size();
            assert!(pty.rows >= 1 && pty.cols >= 1);
        }
    }
}
