// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::VecDeque;

use crate::style::Cell;

pub type ScrollbackLine = Vec<Cell>;

// Shared per-line helpers used by both `ScrollbackBuffer` (committed
// history) and `Session`'s combined-timeline accessors (`terminal.rs` /
// `session.rs`), which stitch scrollback and the live primary grid into
// one addressable index space. Keeping the trimming and matching rules
// in one place means a combined-timeline read behaves identically to a
// scrollback-only read for the same line, regardless of which side of
// the scrollback/grid boundary that line currently lives on.

// Trim a single line's cells the same way `ScrollbackBuffer::
// extract_lines` does: drop trailing blanks, then drop the wide-char
// continuation sentinel (`\0`, see `style::char_width`) from what's
// left so a copied CJK/emoji line reads as plain text instead of
// embedding NUL bytes in the clipboard. The trailing-blank scan walks
// the raw cells first (a `\0` is `!= ' '`, so it correctly keeps a
// wide glyph that sits at the end of the line) — only the final push
// loop skips it.
pub(crate) fn trimmed_line_text(cells: &[Cell]) -> String {
    let trailing = cells
        .iter()
        .rposition(|cell| cell.ch != ' ')
        .map(|i| i + 1)
        .unwrap_or(0);
    let mut out = String::new();
    for cell in &cells[..trailing] {
        if cell.ch != '\0' {
            out.push(cell.ch);
        }
    }
    out
}

// Case-insensitive substring search over an arbitrary sequence of
// lines, returning the 0-based indices (in iteration order) of every
// line whose plain char content contains `needle`. Empty needles
// return no matches so a caller who accidentally commits an empty
// prompt doesn't get "every line matches." Shared by `ScrollbackBuffer
// ::search` and `Session::combined_search` so a search over the
// combined timeline finds the same matches a scrollback-only search
// would once those lines are flushed.
pub(crate) fn search_line_indices<'a, I>(lines: I, needle: &str) -> Vec<usize>
where
    I: IntoIterator<Item = &'a ScrollbackLine>,
{
    if needle.is_empty() {
        return Vec::new();
    }
    let lower = needle.to_lowercase();
    lines
        .into_iter()
        .enumerate()
        .filter_map(|(index, cells)| {
            let text: String = cells
                .iter()
                .map(|cell| cell.ch)
                .collect::<String>()
                .to_lowercase();
            if text.contains(&lower) {
                Some(index)
            } else {
                None
            }
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct ScrollbackBuffer {
    capacity: usize,
    viewport_height: usize,
    lines: VecDeque<ScrollbackLine>,
    viewport_top: usize,
    follow_output: bool,
    // Rows currently held live in the ingester's on-screen grid, past
    // the end of `lines`. The grid isn't stored here — it lives in
    // `TerminalIngest` — but the scroll math needs its length to treat
    // "scrollback ++ live grid" as one continuous addressable timeline.
    // Without this, `max_viewport_top` only sees committed scrollback
    // lines, which sit `live_tail` rows behind the true live bottom;
    // the first wheel-up notch out of follow mode would then jump the
    // viewport up by a whole grid's worth instead of a few lines. See
    // `set_live_tail` for how the ingester keeps this current.
    live_tail: usize,
}

impl ScrollbackBuffer {
    // Zero capacity / height are clamped to 1 rather than rejected:
    // both can reach here from the outside world (a layout split on a
    // tiny terminal can hand a pane 0 rows), and a degenerate-but-alive
    // buffer beats a daemon panic.
    pub fn new(capacity: usize, viewport_height: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            capacity,
            viewport_height: viewport_height.max(1),
            lines: VecDeque::with_capacity(capacity),
            viewport_top: 0,
            follow_output: true,
            live_tail: 0,
        }
    }

    pub fn append_line(&mut self, line: ScrollbackLine) {
        self.lines.push_back(line);

        let mut evicted = 0usize;
        while self.lines.len() > self.capacity {
            self.lines.pop_front();
            evicted += 1;
        }

        if evicted > 0 && !self.follow_output {
            self.viewport_top = self.viewport_top.saturating_sub(evicted);
        }

        self.viewport_top = self.viewport_top.min(self.max_viewport_top());
        if self.follow_output {
            self.scroll_to_bottom();
        } else {
            self.follow_output = self.viewport_top == self.max_viewport_top();
        }
    }

    pub fn scroll_up(&mut self, lines: usize) -> usize {
        let previous = self.viewport_top;
        self.viewport_top = self.viewport_top.saturating_sub(lines);
        self.follow_output = self.viewport_top == self.max_viewport_top();
        previous.saturating_sub(self.viewport_top)
    }

    pub fn scroll_down(&mut self, lines: usize) -> usize {
        let previous = self.viewport_top;
        self.viewport_top = (self.viewport_top + lines).min(self.max_viewport_top());
        self.follow_output = self.viewport_top == self.max_viewport_top();
        self.viewport_top.saturating_sub(previous)
    }

    pub fn scroll_to_bottom(&mut self) {
        self.viewport_top = self.max_viewport_top();
        self.follow_output = true;
    }

    /// Return the most-recent `n` lines from the buffer (oldest first
    /// among the returned slice). Spans the full retained buffer —
    /// scrollback plus whatever currently sits in the viewport — and
    /// caps at the buffer's actual size when `n` exceeds it. Cheap:
    /// the underlying `VecDeque` is contiguous-by-index so a tail
    /// slice is one `iter().skip(...)`.
    pub fn tail_lines(&self, n: usize) -> Vec<ScrollbackLine> {
        let len = self.lines.len();
        let take = n.min(len);
        let start = len - take;
        self.lines.iter().skip(start).cloned().collect()
    }

    pub fn visible_lines(&self) -> Vec<ScrollbackLine> {
        let len = self.lines.len();
        if len == 0 {
            return Vec::new();
        }

        let start = self.viewport_top.min(self.max_viewport_top());
        let end = (start + self.viewport_height).min(len);

        self.lines
            .iter()
            .skip(start)
            .take(end.saturating_sub(start))
            .cloned()
            .collect()
    }

    pub fn set_viewport_height(&mut self, viewport_height: usize) {
        // Clamped for the same reason as `new`: resize paths can
        // legitimately compute a 0-row pane on a tiny terminal.
        self.viewport_height = viewport_height.max(1);
        self.viewport_top = self.viewport_top.min(self.max_viewport_top());
        if self.follow_output {
            self.scroll_to_bottom();
        } else {
            self.follow_output = self.viewport_top == self.max_viewport_top();
        }
    }

    // Told by the ingester how many rows of its live grid currently sit
    // past the end of committed scrollback (see the `live_tail` field
    // comment). Follows the same clamp/re-follow idiom as
    // `set_viewport_height` and `append_line`: shrinking the combined
    // timeline can push a scrolled-back viewport_top past the new
    // ceiling, so it's re-clamped, and follow mode (if active) snaps
    // back to the new bottom rather than drifting.
    pub fn set_live_tail(&mut self, live_tail: usize) {
        self.live_tail = live_tail;
        self.viewport_top = self.viewport_top.min(self.max_viewport_top());
        if self.follow_output {
            self.scroll_to_bottom();
        } else {
            self.follow_output = self.viewport_top == self.max_viewport_top();
        }
    }

    pub fn total_lines(&self) -> usize {
        self.lines.len()
    }

    // Drop every retained line and snap the viewport back to the top.
    // Called when the guest emits `\x1b[3J` (erase scrollback) — the
    // escape sequence that bash's `clear` and xterm's "clear scrollback"
    // menu item both produce. follow_output flips on so new shell
    // output lands in the first slot.
    pub fn clear(&mut self) {
        self.lines.clear();
        self.viewport_top = 0;
        self.follow_output = true;
    }

    // Case-insensitive substring search across the entire buffer.
    // Returns the line indices (0-based, from the oldest retained line)
    // that contain `needle`. Empty needles return no matches so a
    // caller who accidentally commits an empty prompt doesn't get
    // "every line matches." The match is performed on the plain char
    // content of each cell — styling is ignored.
    pub fn search(&self, needle: &str) -> Vec<usize> {
        search_line_indices(self.lines.iter(), needle)
    }

    // Join the text of buffer lines `start..=end` (inclusive) into a
    // newline-separated String with trailing blanks trimmed per line.
    // Out-of-range indices are silently skipped so the caller can pass
    // cursor/anchor pairs without worrying about buffer churn racing
    // with a live shell between the user pressing `v` and pressing `y`.
    //
    // Wide-char continuation sentinels (`\0`, see `style::char_width`)
    // are dropped from the output — every other read path (`Pane::
    // visible_text`, `scrollback_text`, `TerminalIngest::render_lines`)
    // already filters them so a copied CJK/emoji line reads as plain
    // text instead of embedding NUL bytes in the clipboard. The
    // trailing-blank scan still walks the raw cells (a `\0` is `!=
    // ' '`, so it correctly keeps a wide glyph that sits at the end of
    // the line) — only the final push loop skips it.
    pub fn extract_lines(&self, start: usize, end: usize) -> String {
        let (low, high) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        let mut out = String::new();
        for index in low..=high {
            if let Some(cells) = self.lines.get(index) {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&trimmed_line_text(cells));
            }
        }
        out
    }

    // Raw cells of a single buffer line, or an empty vec when `index`
    // is out of range. Unlike `extract_lines`, the wide-char
    // continuation sentinel (`\0`) is kept in place — callers that
    // need to slice a line by *visual* column (mouse-driven char/rect
    // selections) must index into the same cell array the renderer
    // and the mouse coordinate math both use, where one on-screen
    // column is one `Cell`, wide char or not. Filtering `\0` here
    // would shift every cell after a wide glyph one column to the
    // left of where the click actually landed.
    pub fn line_cells(&self, index: usize) -> ScrollbackLine {
        self.lines.get(index).cloned().unwrap_or_default()
    }

    // Position the viewport so `line_index` is centered vertically when
    // possible. Falls back to clamping against the buffer bounds so a
    // match near the top or bottom still shows as much context as the
    // buffer allows. Used by search to jump between matches.
    pub fn center_viewport_on(&mut self, line_index: usize) {
        let half = self.viewport_height / 2;
        let desired_top = line_index.saturating_sub(half);
        self.viewport_top = desired_top.min(self.max_viewport_top());
        self.follow_output = self.viewport_top == self.max_viewport_top();
    }

    // Scroll the minimum amount required to keep `line_index` inside
    // the current viewport. If it's already visible, no change. Used
    // by selection-mode cursor movement so the viewport "follows" the
    // cursor without making jarring big jumps.
    pub fn ensure_line_visible(&mut self, line_index: usize) {
        let bottom = self.viewport_top + self.viewport_height.saturating_sub(1);
        if line_index < self.viewport_top {
            self.viewport_top = line_index;
        } else if line_index > bottom {
            self.viewport_top = line_index
                .saturating_sub(self.viewport_height.saturating_sub(1))
                .min(self.max_viewport_top());
        }
        self.follow_output = self.viewport_top == self.max_viewport_top();
    }

    pub fn viewport_top(&self) -> usize {
        self.viewport_top
    }

    pub fn viewport_height(&self) -> usize {
        self.viewport_height
    }

    pub fn follow_output(&self) -> bool {
        self.follow_output
    }

    // The addressable timeline is committed scrollback lines followed
    // by whatever's still live in the ingester's grid (`live_tail`).
    // Every caller of this — `append_line`, `scroll_up`/`scroll_down`,
    // `set_viewport_height`, `set_live_tail`, `center_viewport_on`,
    // `ensure_line_visible` — already treats it as "the largest legal
    // viewport_top," so folding `live_tail` in here is enough to make
    // all of them combined-timeline-aware without touching their
    // bodies.
    fn max_viewport_top(&self) -> usize {
        (self.lines.len() + self.live_tail).saturating_sub(self.viewport_height)
    }
}

#[cfg(test)]
mod tests {
    use super::ScrollbackBuffer;
    use crate::style::Cell;

    fn line(text: &str) -> Vec<Cell> {
        text.chars().map(Cell::new).collect()
    }

    fn plain(lines: &[Vec<Cell>]) -> Vec<String> {
        lines
            .iter()
            .map(|cells| cells.iter().map(|c| c.ch).collect())
            .collect()
    }

    #[test]
    fn follows_output_by_default() {
        let mut buffer = ScrollbackBuffer::new(16, 3);
        for index in 1..=5 {
            buffer.append_line(line(&format!("line {index}")));
        }

        assert!(buffer.follow_output());
        assert_eq!(buffer.viewport_top(), 2);
        assert_eq!(
            plain(&buffer.visible_lines()),
            vec!["line 3", "line 4", "line 5"]
        );
    }

    #[test]
    fn stays_pinned_when_new_output_arrives_while_scrolled_back() {
        let mut buffer = ScrollbackBuffer::new(16, 3);
        for index in 1..=5 {
            buffer.append_line(line(&format!("line {index}")));
        }

        assert_eq!(buffer.scroll_up(2), 2);
        buffer.append_line(line("line 6"));

        assert!(!buffer.follow_output());
        assert_eq!(buffer.viewport_top(), 0);
        assert_eq!(
            plain(&buffer.visible_lines()),
            vec!["line 1", "line 2", "line 3"]
        );
    }

    #[test]
    fn scrolling_back_to_bottom_reenables_follow_mode() {
        let mut buffer = ScrollbackBuffer::new(16, 3);
        for index in 1..=5 {
            buffer.append_line(line(&format!("line {index}")));
        }

        buffer.scroll_up(2);
        assert_eq!(buffer.scroll_down(100), 2);

        assert!(buffer.follow_output());
        assert_eq!(
            plain(&buffer.visible_lines()),
            vec!["line 3", "line 4", "line 5"]
        );
    }

    #[test]
    fn search_finds_lines_case_insensitively() {
        let mut buffer = ScrollbackBuffer::new(16, 3);
        buffer.append_line(line("alpha"));
        buffer.append_line(line("Beta"));
        buffer.append_line(line("gamma beta"));
        buffer.append_line(line("DELTA"));

        let matches = buffer.search("beta");
        assert_eq!(matches, vec![1, 2]);

        // Empty needle must not match everything.
        assert!(buffer.search("").is_empty());
    }

    #[test]
    fn center_viewport_on_positions_the_match_in_the_middle() {
        let mut buffer = ScrollbackBuffer::new(32, 4);
        for index in 0..20 {
            buffer.append_line(line(&format!("line {index}")));
        }

        buffer.center_viewport_on(10);
        // viewport_height is 4, half is 2; matching line 10 → top = 8.
        assert_eq!(buffer.viewport_top(), 8);

        // Matches near the top cap at zero.
        buffer.center_viewport_on(0);
        assert_eq!(buffer.viewport_top(), 0);
    }

    #[test]
    fn tail_lines_spans_the_full_retained_buffer() {
        // 4-row viewport, 16-line scrollback capacity. Push 10 lines
        // — six of them are above the viewport, four are visible.
        // `tail_lines(8)` must return lines 3..=10 (mixing scrollback
        // and viewport), not just the visible four.
        let mut buffer = ScrollbackBuffer::new(16, 4);
        for index in 1..=10 {
            buffer.append_line(line(&format!("line {index}")));
        }
        let tail = buffer.tail_lines(8);
        assert_eq!(tail.len(), 8);
        let texts: Vec<String> = tail
            .iter()
            .map(|cells| cells.iter().map(|c| c.ch).collect())
            .collect();
        assert_eq!(texts.first().map(String::as_str), Some("line 3"));
        assert_eq!(texts.last().map(String::as_str), Some("line 10"));
        // Asking for more than the buffer holds caps at total size.
        assert_eq!(buffer.tail_lines(100).len(), 10);
        // Empty buffer returns empty.
        let empty: ScrollbackBuffer = ScrollbackBuffer::new(8, 4);
        assert!(empty.tail_lines(5).is_empty());
    }

    #[test]
    fn eviction_shifts_the_viewport_without_forcing_bottom_follow() {
        let mut buffer = ScrollbackBuffer::new(5, 3);
        for index in 1..=5 {
            buffer.append_line(line(&format!("line {index}")));
        }

        buffer.scroll_up(1);
        buffer.append_line(line("line 6"));

        assert!(!buffer.follow_output());
        assert_eq!(buffer.total_lines(), 5);
        assert_eq!(buffer.viewport_top(), 0);
        assert_eq!(
            plain(&buffer.visible_lines()),
            vec!["line 2", "line 3", "line 4"]
        );
    }

    #[test]
    fn set_live_tail_extends_the_combined_timeline_ceiling() {
        // Two committed scrollback lines plus a full 4-row live grid:
        // the combined timeline is 6 lines deep, so the true bottom
        // (max_viewport_top) is 2 — not 0, which is what the
        // scrollback-only formula would say before `live_tail` is
        // folded in.
        let mut buffer = ScrollbackBuffer::new(16, 4);
        buffer.append_line(line("line 1"));
        buffer.append_line(line("line 2"));

        buffer.set_live_tail(4);

        assert!(buffer.follow_output());
        assert_eq!(buffer.viewport_top(), 2);
    }

    #[test]
    fn scroll_up_from_full_live_grid_moves_by_the_requested_line_count() {
        // This is the arithmetic behind the mouse-wheel-jump bug: with
        // 8 committed scrollback lines sitting under a full 4-row live
        // grid, the combined bottom is at combined index 8. A single
        // wheel-up notch (3 lines) must move the top by exactly 3 —
        // not by a whole grid's worth (4) and not by nothing, which is
        // what happens when `max_viewport_top` ignores the live tail.
        let mut buffer = ScrollbackBuffer::new(32, 4);
        for index in 1..=8 {
            buffer.append_line(line(&format!("line {index}")));
        }
        buffer.set_live_tail(4);
        assert_eq!(buffer.viewport_top(), 8);

        assert_eq!(buffer.scroll_up(3), 3);
        assert_eq!(buffer.viewport_top(), 5);
        assert!(!buffer.follow_output());

        // The next notch keeps scrolling smoothly by the same amount —
        // this is the "subsequent notches scroll fine" half of the bug
        // report, confirmed still true under the fix.
        assert_eq!(buffer.scroll_up(3), 3);
        assert_eq!(buffer.viewport_top(), 2);
    }

    #[test]
    fn set_live_tail_reclamps_a_scrolled_back_viewport_when_the_grid_shrinks() {
        // A scrolled-back viewport sitting near the (old) bottom can be
        // stranded above the new ceiling if the live tail shrinks out
        // from under it (e.g. the grid emptied). `set_live_tail` must
        // re-clamp `viewport_top`, mirroring the same pattern
        // `set_viewport_height` and `append_line` already use for
        // their own ceiling-affecting changes.
        let mut buffer = ScrollbackBuffer::new(32, 4);
        for index in 1..=10 {
            buffer.append_line(line(&format!("line {index}")));
        }
        buffer.set_live_tail(4);
        assert_eq!(buffer.viewport_top(), 10);

        assert_eq!(buffer.scroll_up(1), 1);
        assert_eq!(buffer.viewport_top(), 9);
        assert!(!buffer.follow_output());

        // The grid empties out (combined ceiling drops from 10 to 6):
        // viewport_top must be pulled back down to the new max rather
        // than pointing past the end of the now-shorter timeline.
        buffer.set_live_tail(0);
        assert_eq!(buffer.viewport_top(), 6);
    }
}
