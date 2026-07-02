// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::mem;

use crate::mouse::{MouseTrackingMode, ScreenMode};
use crate::pane::Pane;
use crate::pty::PtySize;
use crate::style::{Cell, Style};

// Cursor shape requested via DECSCUSR (`CSI Ps SP q`). Cosmetic but
// agent CLIs flip it to `BlinkingBar` to draw attention to a streaming
// response and back to `SteadyBlock` when the prompt regains focus.
// Tracked here so a future renderer can mirror it; the field is
// observable from outside via `cursor_shape()` even though zmux's
// current TTY backend doesn't yet repaint the on-screen cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorShape {
    Default,
    BlinkingBlock,
    SteadyBlock,
    BlinkingUnderline,
    SteadyUnderline,
    BlinkingBar,
    SteadyBar,
}

// Cursor state captured by DECSC (ESC 7) / CSI s and restored by DECRC
// (ESC 8) / CSI u. Per xterm, the save covers the SGR pen as well as
// the position — TUIs bracket styled fragments with save/restore and
// rely on the pen coming back, so restoring position alone leaks the
// styled pen into everything printed afterwards.
#[derive(Debug, Clone)]
struct SavedCursor {
    row: usize,
    col: usize,
    pen: Style,
}

#[derive(Debug)]
pub struct TerminalIngest {
    state: ParseState,
    csi_buffer: Vec<u8>,
    osc_buffer: Vec<u8>,
    // Set when csi_buffer / osc_buffer hit their caps. The parser
    // keeps consuming the sequence's bytes (so they don't spray into
    // the grid as text) but stops storing them, and the finished
    // sequence is discarded as malformed. Without the caps, a stream
    // that opens a CSI/OSC and never sends the terminator grows the
    // buffer without bound — a slow OOM of the daemon.
    csi_overflow: bool,
    osc_overflow: bool,
    // Primary-screen 2D editable grid. Each entry is a row of cells; the
    // last row is the row the cursor currently sits in. Rows above are
    // older live rows that haven't yet been scrolled into pane scrollback.
    // The grid is bounded by `self.alternate.rows` (the PTY's reported
    // rows): a newline that would push past that ceiling evicts the
    // oldest grid row to scrollback before continuing.
    //
    // Replaces the old `current_line: Vec<Cell>` + `primary_carriage_return`
    // pair. The append-stream model couldn't represent in-place edits
    // (CUU + write), which is exactly what modern TUIs (claude, fzf,
    // anything that animates an input box) do every keystroke. The 2D
    // grid lets CUU revisit a prior row, CUF/CUB/CHA jump anywhere within
    // a row, and DECSC/DECRC round-trip the cursor.
    primary_grid: Vec<Vec<Cell>>,
    primary_cursor_row: usize,
    primary_cursor_col: usize,
    primary_saved_cursor: Option<SavedCursor>,
    primary_scroll_top: usize,
    primary_scroll_bottom: usize,
    primary_flushed_to_scrollback: bool,
    primary_wrap_pending: bool,
    primary_auto_wrap: bool,
    alternate: AlternateScreen,
    // Pen saved by DECSC while the alternate screen is active. The
    // position half lives inside AlternateScreen; the pen lives here
    // because `current_style` does. Defaults to Style::DEFAULT, which
    // gives alt-screen DECRC-without-DECSC xterm's reset-to-normal
    // behavior for free.
    alt_saved_pen: Style,
    // Active SGR state. Updated as `CSI m` sequences come in and copied
    // into each Cell written to either screen. The primary-screen
    // scrollback and the alternate-screen grid both carry these styles
    // for their lifetime.
    current_style: Style,
    // Accumulator for multi-byte UTF-8 sequences so we can reassemble
    // them into a single `char`. A byte 0xC0..=0xFD in the ground state
    // starts a sequence whose total length is encoded in its leading
    // bits; subsequent 0x80..=0xBF bytes are continuations.
    utf8_buffer: Vec<u8>,
    utf8_remaining: usize,
    cursor_shape: CursorShape,
    // Most-recently emitted printable char. CSI `b` (REP) repeats it N
    // times. Box-drawing optimizers in TUI agents use REP to compress
    // long border runs; without it the borders render as a single dash.
    last_graphic: Option<char>,
    // DECSET/DECRST 2004. When set, the host wants pasted text wrapped
    // in `ESC[200~ ... ESC[201~` so it can distinguish typed input from
    // a paste blob. zmux just tracks the toggle here; the workspace
    // layer reads it and wraps before writing to the PTY.
    bracketed_paste: bool,
    // DECSET/DECRST 1004. When set, the host wants `ESC[I` on focus gain
    // and `ESC[O` on focus loss so it can pause animations / dim the
    // prompt while the user is in another pane. Same shape as 2004:
    // toggle here, workspace decides when to emit.
    focus_events: bool,
    // Synchronized Output (DECSET 2026 a.k.a. BSU/ESU). When `Some`, the
    // host has opened a synchronized region and wants every subsequent
    // byte buffered until the matching ESU (`ESC[?2026l`) lands. zmux
    // collects bytes here, scans for the literal ESU sub-sequence on
    // every feed, and then dispatches the buffered prefix atomically so
    // the alternate-screen renderer never sees a partial frame.
    // None means no region is open and bytes flow through the normal
    // per-byte parser loop. The inner `SyncBuffer` carries a scan
    // cursor so each feed only re-scans the new tail (plus an overlap
    // window) instead of re-scanning the whole buffer — preventing the
    // O(N·M) blowup when ESU is far behind a long region.
    synchronized_buffer: Option<SyncBuffer>,
}

// Holding state for a Synchronized Output region. `bytes` accumulates
// every byte received between BSU and ESU; `scan_from` is the index at
// which the next ESU search should begin. After each feed without a
// hit, `scan_from` advances to `bytes.len() - (ESU_BYTES.len() - 1)`,
// keeping a small overlap so an ESU split across a feed boundary still
// gets caught.
#[derive(Debug)]
struct SyncBuffer {
    bytes: Vec<u8>,
    scan_from: usize,
}

impl SyncBuffer {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            scan_from: 0,
        }
    }
}

impl Default for TerminalIngest {
    fn default() -> Self {
        Self::new(PtySize::new(24, 80))
    }
}

#[derive(Debug, Default)]
enum ParseState {
    #[default]
    Ground,
    Escape,
    Charset,
    Csi,
    Osc,
    OscEscape,
    Dcs,
    DcsEscape,
}

impl TerminalIngest {
    pub fn new(size: PtySize) -> Self {
        Self {
            state: ParseState::Ground,
            csi_buffer: Vec::new(),
            osc_buffer: Vec::new(),
            csi_overflow: false,
            osc_overflow: false,
            primary_grid: Vec::new(),
            primary_cursor_row: 0,
            primary_cursor_col: 0,
            primary_saved_cursor: None,
            primary_scroll_top: 0,
            primary_scroll_bottom: size.rows.saturating_sub(1) as usize,
            primary_flushed_to_scrollback: false,
            primary_wrap_pending: false,
            primary_auto_wrap: true,
            alternate: AlternateScreen::new(size.rows as usize, size.cols as usize),
            alt_saved_pen: Style::DEFAULT,
            current_style: Style::DEFAULT,
            utf8_buffer: Vec::new(),
            utf8_remaining: 0,
            cursor_shape: CursorShape::Default,
            last_graphic: None,
            bracketed_paste: false,
            focus_events: false,
            synchronized_buffer: None,
        }
    }

    pub fn cursor_shape(&self) -> CursorShape {
        self.cursor_shape
    }

    pub fn bracketed_paste_enabled(&self) -> bool {
        self.bracketed_paste
    }

    pub fn focus_events_enabled(&self) -> bool {
        self.focus_events
    }

    pub fn resize(&mut self, size: PtySize) {
        let old_rows = self.alternate.rows.max(1);
        let old_region_was_full = self.primary_scroll_top == 0
            && self.primary_scroll_bottom >= old_rows.saturating_sub(1);
        let new_rows = size.rows as usize;
        let new_cols = size.cols as usize;
        self.alternate.resize(new_rows, new_cols);

        // Trim the primary grid to the new geometry. Without this, a
        // shrink leaves rows whose cell count exceeds new_cols (later
        // CUF clamps work but cells beyond the right edge are visible
        // until they evict) and rows beyond new_rows that won't scroll
        // because the row-cap check uses the new ceiling. We truncate
        // rows over the cap (oldest first, matching the scroll model)
        // and trim each row's cells. Cursor is clamped back into bounds.
        let row_cap = new_rows.max(1);
        if self.primary_grid.len() > row_cap {
            let drop = self.primary_grid.len() - row_cap;
            self.primary_grid.drain(0..drop);
        }
        let col_cap = new_cols.max(1);
        for row in self.primary_grid.iter_mut() {
            if row.len() > col_cap {
                row.truncate(col_cap);
            }
        }
        let max_row = self.primary_grid.len().saturating_sub(1);
        self.primary_cursor_row = self.primary_cursor_row.min(max_row);
        self.primary_cursor_col = self.primary_cursor_col.min(col_cap.saturating_sub(1));
        self.primary_wrap_pending = false;
        if let Some(saved) = self.primary_saved_cursor.as_mut() {
            saved.row = saved.row.min(max_row);
            saved.col = saved.col.min(col_cap.saturating_sub(1));
        }
        if old_region_was_full {
            self.primary_scroll_top = 0;
            self.primary_scroll_bottom = row_cap.saturating_sub(1);
        } else {
            self.primary_scroll_top = self.primary_scroll_top.min(row_cap.saturating_sub(1));
            self.primary_scroll_bottom = self.primary_scroll_bottom.min(row_cap.saturating_sub(1));
            if self.primary_scroll_top >= self.primary_scroll_bottom {
                self.primary_scroll_top = 0;
                self.primary_scroll_bottom = row_cap.saturating_sub(1);
            }
        }
    }

    pub fn ingest_bytes(&mut self, pane: &mut Pane, bytes: &[u8]) -> Vec<u8> {
        if !bytes.is_empty() {
            self.primary_flushed_to_scrollback = false;
        }
        let mut replies = Vec::new();
        // Synchronized Output (mode 2026) intercept. While a BSU/ESU
        // region is open, every incoming byte goes into a holding buffer
        // instead of the per-byte dispatch loop. We scan for the literal
        // ESU sub-sequence on each feed and, when found, atomically
        // dispatch the buffered prefix and resume normal dispatch on
        // anything after ESU. The cross-feed split case (BSU /content/
        // ESU spread across multiple ingest_bytes calls) is why this
        // lives outside the byte loop — we have to retain partial bytes
        // between feeds without losing them.
        //
        // We deliberately re-loop after each transition (sync→normal,
        // normal→sync) so a single feed that opens, fills, closes, and
        // re-opens a sync region all completes before we return.
        let mut pending: Vec<u8> = bytes.to_vec();
        loop {
            if self.synchronized_buffer.is_some() {
                // Append `pending` into the active sync buffer and look
                // for ESU starting from `scan_from`. If found, dispatch
                // the prefix; whatever's after ESU becomes the new
                // `pending` and we re-loop.
                let sync = self.synchronized_buffer.as_mut().expect("just checked");
                let scan_start = sync.scan_from;
                sync.bytes.extend_from_slice(&pending);

                // ESU might straddle the previous tail; scan from
                // `scan_start` (saturating into the overlap window) so
                // we never miss a sequence that began before this feed.
                let scan_window = scan_start.min(sync.bytes.len());
                if let Some(rel) = find_subslice(&sync.bytes[scan_window..], ESU_BYTES) {
                    let esu_at = scan_window + rel;
                    let collected = std::mem::take(&mut sync.bytes);
                    self.synchronized_buffer = None;
                    let body = &collected[..esu_at];
                    let after_esu_start = esu_at + ESU_BYTES.len();
                    self.dispatch_bytes(pane, body, &mut replies);
                    pending = collected[after_esu_start..].to_vec();
                    continue;
                }

                // No ESU yet. Advance the cursor past everything we've
                // scanned, keeping an `ESU_BYTES.len() - 1` overlap so
                // an ESU split across feeds is still caught.
                let overlap = ESU_BYTES.len().saturating_sub(1);
                sync.scan_from = sync.bytes.len().saturating_sub(overlap);

                // Hard cap: degrade to non-atomic flush rather than
                // OOM if the host opened BSU without closing it.
                if sync.bytes.len() > SYNCHRONIZED_BUFFER_MAX {
                    eprintln!(
                        "zmux terminal: synchronized buffer exceeded {} bytes; flushing without atomicity",
                        SYNCHRONIZED_BUFFER_MAX
                    );
                    let collected = std::mem::take(&mut sync.bytes);
                    self.synchronized_buffer = None;
                    self.dispatch_bytes(pane, &collected, &mut replies);
                    // Anything that lands after the cap arrives via a
                    // subsequent feed and goes through normal dispatch.
                    break;
                }

                break;
            }

            if pending.is_empty() {
                break;
            }

            // Normal dispatch. If a fresh BSU lands mid-pending, the
            // dispatch_bytes helper pushes the tail into the new sync
            // buffer and returns; we re-loop and pick the buffer back up.
            let chunk = std::mem::take(&mut pending);
            self.dispatch_bytes(pane, &chunk, &mut replies);
            // Loop again only if dispatch_bytes ended by arming the
            // sync buffer (which means there might be more bytes inside
            // it that include an ESU). If sync wasn't armed, pending
            // is empty and we exit.
            if self.synchronized_buffer.is_none() {
                break;
            }
        }

        replies
    }

    fn dispatch_bytes(&mut self, pane: &mut Pane, bytes: &[u8], replies: &mut Vec<u8>) {
        for (index, &byte) in bytes.iter().enumerate() {
            match self.state {
                ParseState::Ground => self.handle_ground(pane, byte),
                ParseState::Escape => self.handle_escape(pane, byte),
                ParseState::Charset => self.state = ParseState::Ground,
                ParseState::Csi => {
                    if let Some(reply) = self.handle_csi(pane, byte) {
                        replies.extend(reply);
                    }
                }
                ParseState::Osc => {
                    if let Some(reply) = self.handle_osc(pane, byte) {
                        replies.extend(reply);
                    }
                }
                ParseState::OscEscape => {
                    if let Some(reply) = self.handle_osc_escape(pane, byte) {
                        replies.extend(reply);
                    }
                }
                ParseState::Dcs => self.handle_dcs(byte),
                ParseState::DcsEscape => self.handle_dcs_escape(byte),
            }

            // The CSI handler may have just armed Synchronized Output
            // (BSU). If so, every remaining byte in this chunk belongs
            // in the sync buffer, not the per-byte loop. Push the tail
            // and bail out of the dispatch loop; the outer ingest_bytes
            // / the next ingest_bytes call will resume buffer-scanning.
            if let Some(sync) = self.synchronized_buffer.as_mut() {
                sync.bytes.extend_from_slice(&bytes[index + 1..]);
                return;
            }
        }
    }

    // Drain the live primary grid into pane scrollback. Each grid row
    // becomes one scrollback line, in top-to-bottom order; the cursor
    // is reset to (0, 0) and the saved-cursor slot is cleared. Callers
    // use this when they need scrollback to reflect everything the PTY
    // has emitted so far — e.g. before reading `pane.visible_text()` in
    // a test, or before tearing the pane down. No-op when the active
    // screen is the alternate buffer.
    //
    // Trailing zero-cell rows are skipped — they're the parked-cursor
    // rows left behind by a final `\n` or by a fresh primary buffer
    // that received only newlines. Older zmux's append-stream model
    // never wrote them to scrollback in the first place, so we preserve
    // that contract here. (A row with explicit blank cells from CUF /
    // CHA padding is NOT trailing-empty in this sense — it has cells.)
    pub fn flush_incomplete_line(&mut self, pane: &mut Pane) {
        if pane.screen_mode() != ScreenMode::Primary {
            return;
        }
        let mut grid = mem::take(&mut self.primary_grid);
        while grid.last().is_some_and(|row| row.is_empty()) {
            grid.pop();
        }
        let mut appended = false;
        for row in grid {
            appended = true;
            pane.append_output_line(row);
        }
        self.primary_cursor_row = 0;
        self.primary_cursor_col = 0;
        // Unlike ED 2/3, this DOES need to drop the saved cursor. This
        // isn't an xterm-modeled escape sequence — it's zmux's internal
        // grid->scrollback drain, and it just changed the coordinate
        // frame out from under any pending save: the live cursor above
        // was reset to (0, 0) on a now-EMPTY grid, so a saved (row, col)
        // from before the drain no longer identifies "the same cell it
        // did a moment ago" the way it still does across an in-place ED
        // erase (same grid, same addressable region, content wiped but
        // coordinates unchanged). primary_set_cursor's clamp keeps a
        // stale saved position in bounds, but in-bounds isn't the same
        // as correct here: replaying it after a drain would silently
        // relocate a future DECRC onto unrelated freshly-printed content
        // instead of leaving DECRC as the pre-existing no-op. Keeping
        // the clear can't break a real DECSC/CSI-2J/DECRC bracket in
        // practice because a flush never lands between those three
        // bytes of one escape-sequence-driven redraw — it's only
        // triggered from outside the ingest loop (MCP snapshot reads,
        // session teardown), never mid-parse.
        self.primary_saved_cursor = None;
        self.primary_wrap_pending = false;
        self.primary_flushed_to_scrollback = appended;
    }

    // Plain-text snapshot of the live primary grid, oldest row first.
    // Wide-char continuation sentinels (`\0`) are dropped so the result
    // matches what a human would see on screen. Trailing empty rows are
    // included — callers that want to mirror `flush_incomplete_line`'s
    // trailing-empty-drop semantic should filter on their end.
    //
    // Used by `Session::snapshot_scrollback_lines` to surface the live
    // editable area to MCP readers without mutating ingest state.
    pub fn primary_grid_text(&self) -> Vec<String> {
        self.primary_grid
            .iter()
            .map(|row| row.iter().filter(|c| c.ch != '\0').map(|c| c.ch).collect())
            .collect()
    }

    // Cells in the cursor's current grid row — NOT the bottom of
    // the live area. After a CUU this lands on a previously-painted
    // row; callers wanting the bottom should use `primary_grid.last()`.
    pub fn current_line(&self) -> &[Cell] {
        self.primary_grid
            .get(self.primary_cursor_row)
            .map(|row| row.as_slice())
            .unwrap_or(&[])
    }

    pub fn rendered_line_count(&self, pane: &Pane) -> usize {
        match pane.screen_mode() {
            ScreenMode::Primary => {
                if pane.follow_output() {
                    pane.total_lines() + self.primary_grid.len()
                } else {
                    pane.total_lines()
                }
            }
            ScreenMode::Alternate => self.alternate.rows(),
        }
    }

    pub fn render_lines(&self, pane: &Pane) -> Vec<String> {
        let mut lines: Vec<String> = self
            .render_cells(pane)
            .iter()
            .map(|row| {
                // Produce a plain-text rendering (no ANSI escapes) — this
                // is what older tests inspect. Trim trailing blank cells
                // so the strings match the pre-cell behavior. Skip the
                // '\0' continuation sentinel that follows a wide char so
                // the text output matches what a human would see.
                let trimmed_end = row
                    .iter()
                    .rposition(|cell| *cell != Cell::BLANK)
                    .map(|i| i + 1)
                    .unwrap_or(0);
                row[..trimmed_end]
                    .iter()
                    .filter(|c| c.ch != '\0')
                    .map(|c| c.ch)
                    .collect()
            })
            .collect();
        while lines.last().is_some_and(|line| line.is_empty()) {
            lines.pop();
        }
        lines
    }

    // Cell-level rendering used by the workspace compositor so it can
    // overlay separators, pane headers, and the big-digit overlay on top
    // of pane content without destroying ANSI transitions. Both
    // primary- and alt-screen rows carry the SGR state that was active
    // when each cell was written.
    pub fn render_cells(&self, pane: &Pane) -> Vec<Vec<Cell>> {
        match pane.screen_mode() {
            ScreenMode::Primary => self.render_primary_cells(pane),
            ScreenMode::Alternate => self.alternate.cells().to_vec(),
        }
    }

    fn render_primary_cells(&self, pane: &Pane) -> Vec<Vec<Cell>> {
        if !pane.follow_output() {
            return pane.visible_lines();
        }

        if self.primary_grid.is_empty() && self.primary_flushed_to_scrollback {
            return pane.visible_lines();
        }

        let viewport = pane.viewport_height();
        if self.primary_grid.len() >= viewport {
            let start = self.primary_grid.len() - viewport;
            return self.primary_grid[start..].to_vec();
        }

        // Follow mode renders the live terminal screen, not a splice of
        // scrollback plus live rows. If a TUI clears the screen or deletes
        // rows, old scrollback must not reappear above the sparse grid as
        // ghost content. Missing rows are simply blank viewport rows.
        let mut rows = self.primary_grid.clone();
        rows.resize_with(viewport, Vec::new);
        rows
    }

    fn handle_ground(&mut self, pane: &mut Pane, byte: u8) {
        // If we're mid-UTF-8-sequence, the only bytes we accept are
        // continuation bytes (0b10xxxxxx). Anything else aborts the
        // in-flight sequence so we don't hang on garbage.
        if self.utf8_remaining > 0 {
            if (0x80..=0xBF).contains(&byte) {
                self.utf8_buffer.push(byte);
                self.utf8_remaining -= 1;
                if self.utf8_remaining == 0 {
                    if let Ok(text) = std::str::from_utf8(&self.utf8_buffer)
                        && let Some(ch) = text.chars().next()
                    {
                        self.handle_printable(pane, ch);
                    }
                    self.utf8_buffer.clear();
                }
                return;
            }
            // Malformed sequence — drop what we have and fall through to
            // process this byte as a fresh start.
            self.utf8_buffer.clear();
            self.utf8_remaining = 0;
        }

        match byte {
            b'\x1b' => self.state = ParseState::Escape,
            b'\n' => self.handle_newline(pane),
            b'\r' => self.handle_carriage_return(pane),
            b'\x08' => self.handle_backspace(pane),
            b'\t' => self.handle_tab(pane),
            0x20..=0x7e => self.handle_printable(pane, byte as char),
            0xC0..=0xDF => self.start_utf8_sequence(byte, 1),
            0xE0..=0xEF => self.start_utf8_sequence(byte, 2),
            0xF0..=0xF7 => self.start_utf8_sequence(byte, 3),
            _ => {} // C0 controls we don't handle + stray continuations
        }
    }

    fn start_utf8_sequence(&mut self, leading: u8, expected_continuations: usize) {
        self.utf8_buffer.clear();
        self.utf8_buffer.push(leading);
        self.utf8_remaining = expected_continuations;
    }

    fn handle_escape(&mut self, pane: &mut Pane, byte: u8) {
        self.state = match byte {
            b'[' => {
                self.csi_buffer.clear();
                self.csi_overflow = false;
                ParseState::Csi
            }
            b']' => {
                self.osc_buffer.clear();
                self.osc_overflow = false;
                ParseState::Osc
            }
            b'P' => ParseState::Dcs,
            b'(' | b')' | b'*' | b'+' | b'-' | b'.' | b'/' => ParseState::Charset,
            b'7' => {
                // DECSC: stash (row, col) + pen so the next `ESC 8` can
                // put both right back. claude and friends use this to
                // mark the bottom of the input box, draw the spinner,
                // then restore.
                self.save_cursor_state(pane);
                ParseState::Ground
            }
            b'8' => {
                self.restore_cursor_state(pane);
                ParseState::Ground
            }
            b'D' => {
                if pane.screen_mode() == ScreenMode::Alternate {
                    self.alternate.index();
                }
                ParseState::Ground
            }
            b'M' => {
                if pane.screen_mode() == ScreenMode::Alternate {
                    self.alternate.reverse_index();
                }
                ParseState::Ground
            }
            b'c' => {
                self.hard_reset(pane);
                ParseState::Ground
            }
            b'=' | b'>' => ParseState::Ground,
            _ => ParseState::Ground,
        };
    }

    fn handle_csi(&mut self, pane: &mut Pane, byte: u8) -> Option<Vec<u8>> {
        if (0x40..=0x7e).contains(&byte) {
            let buffer = mem::take(&mut self.csi_buffer);
            let overflowed = mem::take(&mut self.csi_overflow);
            self.state = ParseState::Ground;
            if overflowed {
                // The body blew past CSI_BUFFER_MAX — no real control
                // sequence is that long. Discard it as malformed.
                return None;
            }
            self.process_csi(pane, buffer, byte)
        } else {
            if self.csi_buffer.len() >= CSI_BUFFER_MAX {
                self.csi_overflow = true;
            } else {
                self.csi_buffer.push(byte);
            }
            None
        }
    }

    fn process_csi(&mut self, pane: &mut Pane, buffer: Vec<u8>, final_byte: u8) -> Option<Vec<u8>> {
        let prefix = match buffer.first().copied() {
            Some(b'?') => Some(b'?'),
            Some(b'>') => Some(b'>'),
            Some(b'!') => Some(b'!'),
            _ => None,
        };
        let body_with_intermediate = if prefix.is_some() {
            &buffer[1..]
        } else {
            &buffer[..]
        };
        // DECSCUSR (`CSI Ps SP q`) is the only intermediate-bearing
        // CSI we handle, so peel at most one trailing space (0x20..=0x2F).
        // Multi-byte intermediates are ignored, matching the rest of
        // the parser's best-effort-subset stance.
        let intermediate = body_with_intermediate
            .last()
            .copied()
            .filter(|b| (0x20..=0x2F).contains(b));
        let body_bytes = if intermediate.is_some() {
            &body_with_intermediate[..body_with_intermediate.len() - 1]
        } else {
            body_with_intermediate
        };
        let Ok(body) = std::str::from_utf8(body_bytes) else {
            return None;
        };
        let params = parse_params(body);

        match final_byte {
            b'm' => {
                // SGR: update the running style so subsequent chars are
                // written with the correct color / attributes. Each
                // ';'-separated part may carry ':' subparameters (kitty
                // underline styles, colon-form extended colors); those
                // dispatch as a self-contained group, while plain numeric
                // parts run through the legacy path with lookahead for
                // `38;5;N` / `38;2;R;G;B` payloads. Empty parts default
                // to 0 (reset) per the CSI spec.
                let parts: Vec<&str> = if body.is_empty() {
                    vec![""]
                } else {
                    body.split(';').collect()
                };
                let leads: Vec<u16> = parts
                    .iter()
                    .map(|part| sgr_value(part.split(':').next().unwrap_or("")))
                    .collect();
                let mut index = 0;
                while index < parts.len() {
                    if parts[index].contains(':') {
                        let subs: Vec<u16> =
                            parts[index].split(':').skip(1).map(sgr_value).collect();
                        self.current_style.apply_sgr_colon(leads[index], &subs);
                        index += 1;
                    } else {
                        let consumed = self
                            .current_style
                            .apply_sgr(leads[index], &leads[index + 1..]);
                        index += 1 + consumed;
                    }
                }
                // Keep the alt-screen's fill cell in sync so subsequent
                // erase / scroll / insert / delete operations paint blank
                // cells with the current background.
                self.alternate.set_fill_style(self.current_style.clone());
            }
            b's' => self.save_cursor_state(pane),
            b'u' => self.restore_cursor_state(pane),
            b'H' | b'f' => {
                let row = params.first().and_then(|value| *value).unwrap_or(1);
                let col = params.get(1).and_then(|value| *value).unwrap_or(1);
                match pane.screen_mode() {
                    ScreenMode::Primary => {
                        // CUP on primary: jump cursor to (row, col), both
                        // 1-based. Row is clamped against the VIEWPORT
                        // (PTY rows), NOT the current grid length —
                        // claude/codex use CUP to position UI elements
                        // at fixed terminal coordinates and expect those
                        // rows to logically exist even before they've
                        // been written to. The grid grows lazily on
                        // print via ensure_primary_cell.
                        let max_row = self.alternate.rows.saturating_sub(1);
                        self.primary_cursor_row = row.saturating_sub(1).min(max_row);
                        self.primary_cursor_col = col
                            .saturating_sub(1)
                            .min(self.alternate.cols.saturating_sub(1));
                    }
                    ScreenMode::Alternate => self
                        .alternate
                        .set_cursor(row.saturating_sub(1), col.saturating_sub(1)),
                }
            }
            b'G' => match pane.screen_mode() {
                ScreenMode::Primary => {
                    // CHA on primary: jump cursor to absolute column N
                    // (1-based). With the 2D grid the cursor moves
                    // freely; the row's cells are lazily padded if the
                    // next print extends past current row length.
                    let target_col = params.first().and_then(|value| *value).unwrap_or(1).max(1);
                    self.primary_wrap_pending = false;
                    self.primary_cursor_col =
                        (target_col.saturating_sub(1)).min(self.alternate.cols.saturating_sub(1));
                }
                ScreenMode::Alternate => {
                    let col = params.first().and_then(|value| *value).unwrap_or(1);
                    self.alternate.horizontal_absolute(col.saturating_sub(1));
                }
            },
            b'd' => match pane.screen_mode() {
                ScreenMode::Primary => {
                    // VPA on primary: jump cursor to absolute row N
                    // (1-based). Bounded by the VIEWPORT (PTY rows),
                    // not the current grid length — same reasoning as
                    // CUP above. The grid lazy-grows on print.
                    let target_row = params.first().and_then(|value| *value).unwrap_or(1).max(1);
                    let max_row = self.alternate.rows.saturating_sub(1);
                    self.primary_wrap_pending = false;
                    self.primary_cursor_row = (target_row.saturating_sub(1)).min(max_row);
                }
                ScreenMode::Alternate => {
                    let row = params.first().and_then(|value| *value).unwrap_or(1);
                    self.alternate.vertical_absolute(row.saturating_sub(1));
                }
            },
            b'A' => match pane.screen_mode() {
                ScreenMode::Primary => {
                    // CUU on primary: cursor up N rows, clamped at row 0.
                    // This is the bug-fix sequence — claude redraws its
                    // input box by sending CUU + write, and prior to
                    // this the move was silently dropped so writes
                    // cascaded down.
                    let count = params.first().and_then(|value| *value).unwrap_or(1).max(1);
                    self.primary_wrap_pending = false;
                    self.primary_cursor_row = self.primary_cursor_row.saturating_sub(count);
                }
                ScreenMode::Alternate => {
                    let count = params.first().and_then(|value| *value).unwrap_or(1);
                    self.alternate.move_cursor(-(count as isize), 0);
                }
            },
            b'B' => match pane.screen_mode() {
                ScreenMode::Primary => {
                    // CUD on primary: cursor down N rows, clamped at the
                    // VIEWPORT bottom (PTY rows). Same reasoning as CUP/
                    // VPA — claude moves the cursor to a fixed terminal
                    // row to redraw UI; clamping to grid.len() left it
                    // pinned to the existing tail and produced ghost-
                    // duplicate UI elements.
                    let count = params.first().and_then(|value| *value).unwrap_or(1).max(1);
                    let max_row = self.alternate.rows.saturating_sub(1);
                    self.primary_wrap_pending = false;
                    self.primary_cursor_row = (self.primary_cursor_row + count).min(max_row);
                }
                ScreenMode::Alternate => {
                    let count = params.first().and_then(|value| *value).unwrap_or(1);
                    self.alternate.move_cursor(count as isize, 0);
                }
            },
            b'C' => match pane.screen_mode() {
                ScreenMode::Primary => {
                    // CUF on primary: cursor right N columns, clamped at
                    // cols-1. Cell content underneath is unchanged — the
                    // prior implementation extended the row with blanks,
                    // but with a 2D grid the row is padded lazily on the
                    // next print, so CUF just moves the cursor.
                    let count = params.first().and_then(|value| *value).unwrap_or(1).max(1);
                    self.primary_wrap_pending = false;
                    self.primary_cursor_col = (self.primary_cursor_col + count)
                        .min(self.alternate.cols.saturating_sub(1));
                }
                ScreenMode::Alternate => {
                    let count = params.first().and_then(|value| *value).unwrap_or(1);
                    self.alternate.move_cursor(0, count as isize);
                }
            },
            b'D' => match pane.screen_mode() {
                ScreenMode::Primary => {
                    // CUB on primary: cursor left N columns, clamped at 0.
                    let count = params.first().and_then(|value| *value).unwrap_or(1).max(1);
                    self.primary_wrap_pending = false;
                    self.primary_cursor_col = self.primary_cursor_col.saturating_sub(count);
                }
                ScreenMode::Alternate => {
                    let count = params.first().and_then(|value| *value).unwrap_or(1);
                    self.alternate.move_cursor(0, -(count as isize));
                }
            },
            b'@' => {
                let count = params.first().and_then(|value| *value).unwrap_or(1).max(1);
                match pane.screen_mode() {
                    ScreenMode::Primary => self.primary_insert_chars(count),
                    ScreenMode::Alternate => self.alternate.insert_chars(count),
                }
            }
            b'E' => {
                let count = params.first().and_then(|value| *value).unwrap_or(1).max(1);
                match pane.screen_mode() {
                    ScreenMode::Primary => self.primary_next_line(count),
                    ScreenMode::Alternate => self.alternate.next_line(count),
                }
            }
            b'F' => {
                let count = params.first().and_then(|value| *value).unwrap_or(1).max(1);
                match pane.screen_mode() {
                    ScreenMode::Primary => self.primary_previous_line(count),
                    ScreenMode::Alternate => self.alternate.previous_line(count),
                }
            }
            b'J' if pane.screen_mode() == ScreenMode::Alternate => {
                let mode = params.first().and_then(|value| *value).unwrap_or(0);
                self.alternate.erase_display(mode);
            }
            b'J' if pane.screen_mode() == ScreenMode::Primary => {
                // ED — Erase in Display. Modes 0/1/2/3.
                //   2/3: wipe the live grid + (3 only) scrollback.
                //   0:   from cursor to end of grid.
                //   1:   from start of grid up to (and including) cursor.
                let mode = params.first().and_then(|value| *value).unwrap_or(0);
                match mode {
                    2 | 3 => {
                        self.primary_grid.clear();
                        self.primary_cursor_row = 0;
                        self.primary_cursor_col = 0;
                        // Do NOT clear primary_saved_cursor here. xterm
                        // does not invalidate the DECSC-saved cursor when
                        // the display is erased — a DECSC … CSI 2J …
                        // DECRC bracket gets its position and pen back.
                        // A stale saved position can't land out of
                        // bounds either: primary_set_cursor (the only
                        // path DECRC uses to apply it) clamps to the
                        // viewport (alternate.rows/cols), not grid.len(),
                        // so restoring after the grid was just emptied is
                        // safe by construction.
                        self.primary_wrap_pending = false;
                        if mode == 3 {
                            pane.clear_scrollback();
                        }
                    }
                    0 => self.primary_erase_below(),
                    1 => self.primary_erase_above(),
                    _ => {}
                }
            }
            b'K' if pane.screen_mode() == ScreenMode::Alternate => {
                let mode = params.first().and_then(|value| *value).unwrap_or(0);
                self.alternate.erase_line(mode);
            }
            b'K' if pane.screen_mode() == ScreenMode::Primary => {
                // EL — Erase in Line. The most common redraw-cleanup
                // sequence for TUIs that update in place; without it
                // every frame leaves residual chars from the previous
                // frame. Mode 0 = cursor → eol, 1 = sol → cursor,
                // 2 = whole line.
                let mode = params.first().and_then(|value| *value).unwrap_or(0);
                self.primary_erase_line(mode);
            }
            b'L' if pane.screen_mode() == ScreenMode::Alternate => {
                let count = params.first().and_then(|value| *value).unwrap_or(1);
                self.alternate.insert_lines(count.max(1));
            }
            b'L' if pane.screen_mode() == ScreenMode::Primary => {
                // IL — Insert Lines at cursor. Rows below cursor shift
                // down; rows past the viewport bottom drop. Used by
                // TUIs that animate scrolling regions.
                let count = params.first().and_then(|value| *value).unwrap_or(1).max(1);
                self.primary_insert_lines(count);
            }
            b'M' if pane.screen_mode() == ScreenMode::Alternate => {
                let count = params.first().and_then(|value| *value).unwrap_or(1);
                self.alternate.delete_lines(count.max(1));
            }
            b'M' if pane.screen_mode() == ScreenMode::Primary => {
                // DL — Delete Lines at cursor. Rows below shift up;
                // empty rows append at the viewport bottom.
                let count = params.first().and_then(|value| *value).unwrap_or(1).max(1);
                self.primary_delete_lines(count);
            }
            b'P' if pane.screen_mode() == ScreenMode::Alternate => {
                let count = params.first().and_then(|value| *value).unwrap_or(1);
                self.alternate.delete_chars(count.max(1));
            }
            b'P' if pane.screen_mode() == ScreenMode::Primary => {
                // DCH — Delete Characters. Cells from cursor shift
                // left; trailing cells fill with blanks.
                let count = params.first().and_then(|value| *value).unwrap_or(1).max(1);
                self.primary_delete_chars(count);
            }
            b'X' if pane.screen_mode() == ScreenMode::Alternate => {
                let count = params.first().and_then(|value| *value).unwrap_or(1);
                self.alternate.erase_chars(count.max(1));
            }
            b'X' if pane.screen_mode() == ScreenMode::Primary => {
                // ECH — Erase Characters. Replace N cells starting at
                // cursor with blanks; cursor doesn't move and other
                // cells don't shift.
                let count = params.first().and_then(|value| *value).unwrap_or(1).max(1);
                self.primary_erase_chars(count);
            }
            b'r' => {
                let top = params.first().and_then(|value| *value).unwrap_or(1);
                let bottom = params
                    .get(1)
                    .and_then(|value| *value)
                    .unwrap_or(self.alternate.rows());
                match pane.screen_mode() {
                    ScreenMode::Primary => {
                        self.primary_set_scroll_region(
                            top.saturating_sub(1),
                            bottom.saturating_sub(1),
                        );
                    }
                    ScreenMode::Alternate => self
                        .alternate
                        .set_scroll_region(top.saturating_sub(1), bottom.saturating_sub(1)),
                }
            }
            b'n' if prefix.is_none() => {
                let code = params.first().and_then(|value| *value).unwrap_or(0);
                if code == 6 {
                    return Some(self.cursor_position_report(pane));
                }
            }
            b'c' if prefix == Some(b'>') => return Some(b"\x1b[>0;1;0c".to_vec()),
            b'c' if prefix.is_none() => return Some(b"\x1b[?62;1;6c".to_vec()),
            b'h' if prefix == Some(b'?') => {
                let mut mouse_tracking_mode = pane.mouse_tracking_mode();
                for param in params.into_iter().flatten() {
                    match param {
                        47 | 1047 | 1049 => {
                            // 1049 does an implicit DECSC before switching
                            // (xterm semantics) so the matching 1049l can
                            // hand back the pre-alt cursor and pen. Only
                            // save on a real primary→alt transition; a
                            // redundant re-enter must not clobber the
                            // saved primary state with alt-screen state.
                            if param == 1049 && pane.screen_mode() == ScreenMode::Primary {
                                self.save_cursor_state(pane);
                            }
                            pane.set_screen_mode(ScreenMode::Alternate);
                            self.alternate.reset();
                            self.alt_saved_pen = Style::DEFAULT;
                        }
                        7 => {
                            self.set_primary_auto_wrap(true);
                            self.alternate.set_auto_wrap(true);
                        }
                        1000 => {
                            mouse_tracking_mode = mouse_tracking_mode.max(MouseTrackingMode::Click)
                        }
                        1002 => {
                            mouse_tracking_mode = mouse_tracking_mode.max(MouseTrackingMode::Drag)
                        }
                        1003 => {
                            mouse_tracking_mode = mouse_tracking_mode.max(MouseTrackingMode::Motion)
                        }
                        1004 => self.focus_events = true,
                        2004 => self.bracketed_paste = true,
                        2026 => {
                            // BSU: open a Synchronized Output region.
                            // Subsequent bytes (in this feed and later
                            // feeds) get held in a buffer and dispatched
                            // atomically when the matching ESU arrives.
                            // Re-arming an already-open region is a
                            // no-op per the spec.
                            self.synchronized_buffer.get_or_insert_with(SyncBuffer::new);
                        }
                        _ => {}
                    }
                }
                pane.set_mouse_tracking_mode(mouse_tracking_mode);
            }
            b'l' if prefix == Some(b'?') => {
                let mut disable_mouse = false;
                for param in params.into_iter().flatten() {
                    match param {
                        47 | 1047 | 1049 => {
                            // 1049 restores as in DECRC on the way out —
                            // pen included — but only on a real alt→primary
                            // transition so a stray 1049l can't teleport
                            // the primary cursor.
                            let was_alternate = pane.screen_mode() == ScreenMode::Alternate;
                            pane.set_screen_mode(ScreenMode::Primary);
                            if param == 1049 && was_alternate {
                                self.restore_cursor_state(pane);
                            }
                        }
                        7 => {
                            self.set_primary_auto_wrap(false);
                            self.alternate.set_auto_wrap(false);
                        }
                        1000 | 1002 | 1003 => disable_mouse = true,
                        1004 => self.focus_events = false,
                        2004 => self.bracketed_paste = false,
                        _ => {}
                    }
                }
                if disable_mouse {
                    pane.set_mouse_tracking_mode(MouseTrackingMode::Off);
                }
            }
            b'b' if prefix.is_none() => {
                // REP: repeat the last graphic char N times (default 1).
                // Box-drawing optimizers in TUI agents emit `─\x1b[78b`
                // instead of 79 dashes; without this the border collapses
                // to a single dash.
                if let Some(ch) = self.last_graphic {
                    let count = params.first().and_then(|value| *value).unwrap_or(1).max(1);
                    for _ in 0..count {
                        self.handle_printable(pane, ch);
                    }
                }
            }
            b'q' if intermediate == Some(b' ') => {
                // DECSCUSR: 0/1 = blinking block (1 is the explicit form,
                // 0 is "reset to terminal default"), 2 = steady block,
                // 3 = blinking underline, 4 = steady underline,
                // 5 = blinking bar, 6 = steady bar. Anything else is
                // ignored — terminals disagree on extension values and
                // none of them are common.
                let value = params.first().and_then(|value| *value).unwrap_or(0);
                self.cursor_shape = match value {
                    0 => CursorShape::Default,
                    1 => CursorShape::BlinkingBlock,
                    2 => CursorShape::SteadyBlock,
                    3 => CursorShape::BlinkingUnderline,
                    4 => CursorShape::SteadyUnderline,
                    5 => CursorShape::BlinkingBar,
                    6 => CursorShape::SteadyBar,
                    _ => self.cursor_shape,
                };
            }
            b'p' if prefix == Some(b'!') => {
                // DECSTR: soft terminal reset. Agent CLIs use this on
                // teardown to put cursor/modes/margins back before the
                // shell prompt resumes. It intentionally does not erase
                // the visible screen; RIS (ESC c) handles the hard clear.
                self.soft_reset(pane);
            }
            _ => {}
        }

        None
    }

    fn cursor_position_report(&self, pane: &Pane) -> Vec<u8> {
        let (row, col) = match pane.screen_mode() {
            ScreenMode::Primary => (
                self.primary_cursor_row.saturating_add(1),
                self.primary_cursor_col.saturating_add(1),
            ),
            ScreenMode::Alternate => self.alternate.cursor_position(),
        };

        format!("\x1b[{row};{col}R").into_bytes()
    }

    fn hard_reset(&mut self, pane: &mut Pane) {
        pane.set_screen_mode(ScreenMode::Primary);
        pane.set_mouse_tracking_mode(MouseTrackingMode::Off);
        self.current_style = Style::DEFAULT;
        self.alternate.set_fill_style(Style::DEFAULT);
        self.alternate.reset();
        self.alt_saved_pen = Style::DEFAULT;
        self.alternate.set_auto_wrap(true);
        self.reset_primary_modes(true);
        self.cursor_shape = CursorShape::Default;
        self.last_graphic = None;
        self.bracketed_paste = false;
        self.focus_events = false;
        self.synchronized_buffer = None;
        self.utf8_buffer.clear();
        self.utf8_remaining = 0;
    }

    fn soft_reset(&mut self, pane: &mut Pane) {
        pane.set_mouse_tracking_mode(MouseTrackingMode::Off);
        self.current_style = Style::DEFAULT;
        self.alternate.set_fill_style(Style::DEFAULT);
        self.reset_primary_modes(false);
        self.alternate.set_auto_wrap(true);
        self.alternate.scroll_top = 0;
        self.alternate.scroll_bottom = self.alternate.rows.saturating_sub(1);
        self.cursor_shape = CursorShape::Default;
        self.bracketed_paste = false;
        self.focus_events = false;
        self.synchronized_buffer = None;
    }

    fn reset_primary_modes(&mut self, clear_screen: bool) {
        if clear_screen {
            self.primary_grid.clear();
            self.primary_flushed_to_scrollback = false;
        }
        self.primary_cursor_row = 0;
        self.primary_cursor_col = 0;
        // Unlike ED 2/3, dropping the save here is correct per xterm:
        // both RIS (hard_reset) and DECSTR (soft_reset) route through
        // this and are documented to reset DECSC-saved cursor state to
        // its power-up default, not just leave the display/cursor alone
        // like a plain erase does.
        self.primary_saved_cursor = None;
        self.primary_scroll_top = 0;
        self.primary_scroll_bottom = self.alternate.rows.saturating_sub(1);
        self.primary_wrap_pending = false;
        self.primary_auto_wrap = true;
    }

    // DECSC / CSI s. Saves position + pen for whichever screen is active.
    fn save_cursor_state(&mut self, pane: &Pane) {
        match pane.screen_mode() {
            ScreenMode::Primary => {
                self.primary_saved_cursor = Some(SavedCursor {
                    row: self.primary_cursor_row,
                    col: self.primary_cursor_col,
                    pen: self.current_style.clone(),
                });
            }
            ScreenMode::Alternate => {
                self.alternate.save_cursor();
                self.alt_saved_pen = self.current_style.clone();
            }
        }
    }

    // DECRC / CSI u. Restores position + pen. Primary keeps its
    // historical no-op when nothing was saved; the alternate screen
    // restores its (home, default-pen) baseline, matching xterm's
    // reset-attributes behavior for DECRC without a prior DECSC.
    fn restore_cursor_state(&mut self, pane: &Pane) {
        match pane.screen_mode() {
            ScreenMode::Primary => {
                if let Some(saved) = self.primary_saved_cursor.clone() {
                    self.primary_set_cursor(saved.row, saved.col);
                    self.current_style = saved.pen;
                }
            }
            ScreenMode::Alternate => {
                self.alternate.restore_cursor();
                self.current_style = self.alt_saved_pen.clone();
            }
        }
        // Erase/scroll fills must blank with the restored pen, same as
        // after an explicit SGR.
        self.alternate.set_fill_style(self.current_style.clone());
    }

    // Set the primary cursor, clamped to the addressable region. Used by
    // DECSC/DECRC + CSI s/u restore paths so a saved cursor position that
    // lands outside the current grid (e.g. after the grid scrolled) gets
    // pulled back into bounds rather than silently breaking the next print.
    fn primary_set_cursor(&mut self, row: usize, col: usize) {
        // Clamp to viewport, not grid.len() — DECRC and other
        // absolute-positioning operations need to land on rows that
        // logically exist in the viewport, even if the grid hasn't
        // grown there yet. ensure_primary_cell will lazy-grow on the
        // next print.
        self.primary_wrap_pending = false;
        let max_row = self.alternate.rows.saturating_sub(1);
        self.primary_cursor_row = row.min(max_row);
        self.primary_cursor_col = col.min(self.alternate.cols.saturating_sub(1));
    }

    fn primary_set_scroll_region(&mut self, top: usize, bottom: usize) {
        let max_row = self.alternate.rows.saturating_sub(1);
        if top >= self.alternate.rows || bottom >= self.alternate.rows || top >= bottom {
            return;
        }

        self.primary_scroll_top = top;
        self.primary_scroll_bottom = bottom.min(max_row);
        self.primary_set_cursor(0, 0);
    }

    /// Background cell using the current SGR style — what an erase
    /// op should leave behind so the cleared region picks up whatever
    /// background color is active (matches xterm semantics).
    fn primary_blank_cell(&self) -> Cell {
        Cell::styled(' ', self.current_style.clone())
    }

    fn primary_blank_row(&self) -> Vec<Cell> {
        vec![self.primary_blank_cell(); self.alternate.cols.max(1)]
    }

    /// Pad `primary_grid[row]` with blank cells up to `len`. No-op if
    /// the row doesn't exist (caller's responsibility to grow grid)
    /// or already has enough cells.
    fn primary_pad_row(&mut self, row: usize, len: usize) {
        let blank = self.primary_blank_cell();
        if let Some(line) = self.primary_grid.get_mut(row)
            && line.len() < len
        {
            line.resize(len, blank);
        }
    }

    fn primary_erase_line(&mut self, mode: usize) {
        if self.primary_grid.is_empty() {
            return;
        }
        self.primary_wrap_pending = false;
        let cols = self.alternate.cols;
        let row = self.primary_cursor_row;
        let col = self.primary_cursor_col;
        // Make sure the row is sized so the indices below land in
        // existing storage (mode 0 erases past current end → just
        // truncate at cursor, mode 1/2 need the row to exist at full
        // width since they touch cells before/across the cursor).
        let blank = self.primary_blank_cell();
        match mode {
            // 0 (default): cursor → eol. Right of `line.len()` is
            // implicitly blank (we never store trailing blanks), so
            // we only need to overwrite the populated tail.
            0 => {
                if let Some(line) = self.primary_grid.get_mut(row)
                    && line.len() > col
                {
                    for cell in &mut line[col..] {
                        *cell = blank.clone();
                    }
                }
            }
            // 1: sol → cursor (inclusive)
            1 => {
                self.primary_pad_row(row, (col + 1).min(cols));
                if let Some(line) = self.primary_grid.get_mut(row) {
                    let end = (col + 1).min(line.len());
                    for cell in &mut line[..end] {
                        *cell = blank.clone();
                    }
                }
            }
            // 2: whole line
            2 => {
                if let Some(line) = self.primary_grid.get_mut(row) {
                    line.clear();
                }
            }
            _ => {}
        }
    }

    fn primary_erase_below(&mut self) {
        // ED 0: cursor → eol on current row, then drop all rows below.
        self.primary_erase_line(0);
        let row = self.primary_cursor_row;
        if row + 1 < self.primary_grid.len() {
            self.primary_grid.truncate(row + 1);
        }
    }

    fn primary_erase_above(&mut self) {
        // ED 1: blank rows above cursor + sol → cursor on current row.
        let row = self.primary_cursor_row;
        let blank = self.primary_blank_cell();
        for line in self.primary_grid.iter_mut().take(row) {
            line.fill(blank.clone());
        }
        self.primary_erase_line(1);
    }

    fn primary_insert_lines(&mut self, count: usize) {
        // IL: only valid when cursor is within the grid. Insert blank
        // rows at cursor_row; rows past the viewport bottom drop off
        // (we don't push them to scrollback — IL is an in-area op).
        if self.primary_grid.is_empty() {
            return;
        }
        self.primary_wrap_pending = false;
        let row = self.primary_cursor_row;
        // The grid grows lazily, so the cursor can sit beyond its
        // current tail (CUP/CNL clamp against the viewport, not grid
        // length). Rows past the tail are implicitly blank — inserting
        // blanks among them shifts nothing, so IL is a no-op there
        // (and Vec::insert past len would panic).
        if row >= self.primary_grid.len() {
            return;
        }
        let viewport = self.alternate.rows.max(1);
        let count = count.min(viewport.saturating_sub(row));
        for _ in 0..count {
            self.primary_grid.insert(row, Vec::new());
        }
        if self.primary_grid.len() > viewport {
            self.primary_grid.truncate(viewport);
        }
        // Cursor stays put; col clamps to width.
    }

    fn primary_delete_lines(&mut self, count: usize) {
        // DL: remove rows at cursor_row, append blanks at the bottom.
        if self.primary_grid.is_empty() {
            return;
        }
        self.primary_wrap_pending = false;
        let row = self.primary_cursor_row;
        let available = self.primary_grid.len().saturating_sub(row);
        let count = count.min(available);
        for _ in 0..count {
            self.primary_grid.remove(row);
        }
        // Don't grow the grid past where it was — DL just shrinks
        // visible content. The grid will re-grow naturally on the
        // next print/newline.
    }

    fn primary_delete_chars(&mut self, count: usize) {
        // DCH: delete N cells at cursor. Cells right of cursor shift
        // left; trailing cells fill with blanks (only matters if
        // anything was there beyond the deletion).
        if self.primary_grid.is_empty() {
            return;
        }
        self.primary_wrap_pending = false;
        let col = self.primary_cursor_col;
        let cols = self.alternate.cols;
        if let Some(line) = self.primary_grid.get_mut(self.primary_cursor_row) {
            if col >= line.len() {
                return; // nothing to delete
            }
            let end = (col + count).min(line.len());
            line.drain(col..end);
            // We don't pad with trailing blanks — empty cells are
            // implicit. But if the row was full-width and we deleted
            // from the middle, the right edge is now shorter than
            // before, which matches xterm's behavior of "the now-
            // exposed cells beyond what was there are blank by default."
            let _ = cols;
        }
    }

    fn primary_insert_chars(&mut self, count: usize) {
        // ICH: insert blank cells at the cursor and shift existing cells
        // right. Unlike CUF, this mutates the row; TUIs use it for inline
        // edits where stale cells would otherwise remain in place.
        if self.primary_grid.is_empty() {
            return;
        }
        self.primary_wrap_pending = false;
        let row_index = self.primary_cursor_row;
        let col = self.primary_cursor_col;
        let cols = self.alternate.cols;
        if row_index >= self.primary_grid.len() || col >= cols {
            return;
        }
        self.primary_pad_row(row_index, col.min(cols));
        let blank = self.primary_blank_cell();
        if let Some(line) = self.primary_grid.get_mut(row_index) {
            let count = count.min(cols.saturating_sub(col));
            for _ in 0..count {
                line.insert(col, blank.clone());
            }
            if line.len() > cols {
                line.truncate(cols);
            }
        }
    }

    fn primary_next_line(&mut self, count: usize) {
        // CNL: move down N rows and return to column 0. It does not
        // print or scroll; subsequent output lazily grows the grid if
        // the target row hasn't existed yet.
        let max_row = self.alternate.rows.saturating_sub(1);
        self.primary_wrap_pending = false;
        self.primary_cursor_row = (self.primary_cursor_row + count).min(max_row);
        self.primary_cursor_col = 0;
    }

    fn primary_previous_line(&mut self, count: usize) {
        // CPL: move up N rows and return to column 0. Missing this leaves
        // redraw cursors too low when a TUI walks back up to repaint boxes.
        self.primary_wrap_pending = false;
        self.primary_cursor_row = self.primary_cursor_row.saturating_sub(count);
        self.primary_cursor_col = 0;
    }

    fn set_primary_auto_wrap(&mut self, enabled: bool) {
        self.primary_auto_wrap = enabled;
        if !enabled {
            self.primary_wrap_pending = false;
        }
    }

    fn primary_erase_chars(&mut self, count: usize) {
        // ECH: replace N cells at cursor with blanks; no shift.
        if self.primary_grid.is_empty() {
            return;
        }
        self.primary_wrap_pending = false;
        let col = self.primary_cursor_col;
        let blank = self.primary_blank_cell();
        if let Some(line) = self.primary_grid.get_mut(self.primary_cursor_row) {
            if col >= line.len() {
                return;
            }
            let end = (col + count).min(line.len());
            for cell in &mut line[col..end] {
                *cell = blank.clone();
            }
        }
    }

    fn handle_osc(&mut self, pane: &mut Pane, byte: u8) -> Option<Vec<u8>> {
        match byte {
            0x07 => {
                self.state = ParseState::Ground;
                self.finish_osc(pane, OscTerminator::Bel)
            }
            b'\x1b' => {
                self.state = ParseState::OscEscape;
                None
            }
            _ => {
                self.push_osc_byte(byte);
                self.state = ParseState::Osc;
                None
            }
        }
    }

    fn handle_osc_escape(&mut self, pane: &mut Pane, byte: u8) -> Option<Vec<u8>> {
        match byte {
            b'\\' => {
                self.state = ParseState::Ground;
                self.finish_osc(pane, OscTerminator::St)
            }
            _ => {
                self.push_osc_byte(b'\x1b');
                self.push_osc_byte(byte);
                self.state = ParseState::Osc;
                None
            }
        }
    }

    fn push_osc_byte(&mut self, byte: u8) {
        if self.osc_buffer.len() >= OSC_BUFFER_MAX {
            self.osc_overflow = true;
        } else {
            self.osc_buffer.push(byte);
        }
    }

    fn finish_osc(&mut self, pane: &mut Pane, terminator: OscTerminator) -> Option<Vec<u8>> {
        if mem::take(&mut self.osc_overflow) {
            // Payload blew past OSC_BUFFER_MAX — discard as malformed
            // rather than set a megabyte pane title.
            self.osc_buffer.clear();
            return None;
        }
        let payload = String::from_utf8_lossy(&self.osc_buffer).into_owned();
        let reply = match payload.as_str() {
            // OSC 10/11 are foreground/background color queries. Agent
            // CLIs send these at startup to decide whether to use
            // truecolor; without a reply they degrade to 8-color mode.
            // Mirror the request's terminator on the reply — some
            // programs only recognize one form and silently drop the
            // other.
            "10;?" => Some(build_color_reply(b"10", b"ffff/ffff/ffff", terminator)),
            "11;?" => Some(build_color_reply(b"11", b"0000/0000/0000", terminator)),
            _ => {
                if let Some(title) = parse_osc_title(&payload) {
                    pane.set_title(title);
                    None
                } else if let Some(hyperlink) = parse_osc_hyperlink(&payload) {
                    self.current_style.hyperlink = hyperlink;
                    None
                } else {
                    parse_osc_palette_query(&payload, terminator)
                }
            }
        };
        self.osc_buffer.clear();
        reply
    }

    fn handle_dcs(&mut self, byte: u8) {
        self.state = match byte {
            b'\x1b' => ParseState::DcsEscape,
            _ => ParseState::Dcs,
        };
    }

    fn handle_dcs_escape(&mut self, byte: u8) {
        self.state = match byte {
            b'\\' => ParseState::Ground,
            _ => ParseState::Dcs,
        };
    }

    fn handle_newline(&mut self, pane: &mut Pane) {
        match pane.screen_mode() {
            ScreenMode::Primary => self.primary_linefeed(pane),
            ScreenMode::Alternate => self.alternate.linefeed(),
        }
    }

    fn handle_carriage_return(&mut self, pane: &mut Pane) {
        match pane.screen_mode() {
            ScreenMode::Primary => {
                self.primary_wrap_pending = false;
                self.primary_cursor_col = 0;
            }
            ScreenMode::Alternate => self.alternate.carriage_return(),
        }
    }

    fn handle_backspace(&mut self, pane: &mut Pane) {
        match pane.screen_mode() {
            ScreenMode::Primary => {
                // Move the cursor one column left (clamped at 0). Don't
                // touch the cell underneath — backspace traditionally
                // doesn't erase, callers that want erasure follow up
                // with a space + another backspace.
                self.primary_wrap_pending = false;
                self.primary_cursor_col = self.primary_cursor_col.saturating_sub(1);
            }
            ScreenMode::Alternate => self.alternate.backspace(),
        }
    }

    fn handle_tab(&mut self, pane: &mut Pane) {
        let spaces = 4;
        match pane.screen_mode() {
            ScreenMode::Primary => {
                for _ in 0..spaces {
                    self.primary_put_char(pane, ' ');
                }
            }
            ScreenMode::Alternate => {
                for _ in 0..spaces {
                    self.alternate.put_char(' ', self.current_style.clone());
                }
            }
        }
    }

    fn handle_printable(&mut self, pane: &mut Pane, ch: char) {
        match pane.screen_mode() {
            ScreenMode::Primary => self.primary_put_char(pane, ch),
            ScreenMode::Alternate => self.alternate.put_char(ch, self.current_style.clone()),
        }
        // Remember the last graphic char so CSI `b` (REP) can repeat it.
        // Only printable chars qualify — control codes (newline, CR,
        // backspace, tab) are routed elsewhere and intentionally do
        // not poison this slot.
        self.last_graphic = Some(ch);
    }

    // Linefeed on the primary grid: cursor down one row, col reset to 0.
    // (Most terminals separate linefeed from carriage return, but every
    // existing caller of this code treats `\n` as both — shells emit
    // `\r\n` so `\r` is a no-op there, and bare `\n` from poorly-behaved
    // sources still wants to land on a fresh column-0 line.)
    //
    // Push behavior:
    //   - if the cursor is at the bottom of the grid AND the grid hasn't
    //     hit the PTY-row ceiling: append a fresh blank row, advance
    //     the cursor into it.
    //   - if the grid is at the ceiling: evict grid[0] to scrollback,
    //     shift everything up one row, push a blank row at the bottom,
    //     leave the cursor at the (now-bottom) row.
    //   - otherwise: just advance the cursor.
    fn primary_linefeed(&mut self, pane: &mut Pane) {
        self.primary_wrap_pending = false;
        self.primary_cursor_col = 0;
        let max_rows = self.alternate.rows.max(1);

        // Make sparse absolute-positioned rows real before applying LF.
        // Without this, CUP/CUD to the bottom followed by LF collapses the
        // cursor back to the current grid tail instead of scrolling the
        // viewport/scroll-region like a terminal would.
        while self.primary_grid.len() <= self.primary_cursor_row {
            self.primary_grid.push(Vec::new());
        }

        if self.primary_cursor_row == self.primary_scroll_bottom {
            if self.primary_scroll_top == 0 && self.primary_scroll_bottom == max_rows - 1 {
                let evicted = self.primary_grid.remove(0);
                pane.append_output_line(evicted);
                self.primary_grid.push(Vec::new());
                self.primary_cursor_row = max_rows - 1;
            } else {
                self.primary_scroll_up_within_region(1);
            }
            return;
        }

        if self.primary_cursor_row + 1 < max_rows {
            self.primary_cursor_row += 1;
            return;
        }

        let evicted = self.primary_grid.remove(0);
        pane.append_output_line(evicted);
        self.primary_grid.push(Vec::new());
        self.primary_cursor_row = max_rows - 1;
    }

    // Print one character at the cursor's current cell on the primary
    // grid, with wide-char handling identical to the alt-screen path.
    // Lazily grows the grid (rows up to cursor_row+1) and the row's cell
    // vector (cells up to cursor_col+width) with `Cell::BLANK`. Wraps
    // to the next line when the cursor would advance off the right edge.
    fn primary_put_char(&mut self, pane: &mut Pane, ch: char) {
        let cols = self.alternate.cols.max(1);
        let width = crate::style::char_width(ch).max(1);

        if self.primary_wrap_pending {
            if self.primary_auto_wrap {
                self.primary_linefeed(pane);
            } else {
                self.primary_wrap_pending = false;
            }
        }

        // Wide-char wrap: if writing the glyph would straddle the right
        // edge, start a fresh row first. Without this, East-Asian text
        // shears at the wrap point.
        if self.primary_auto_wrap && width == 2 && self.primary_cursor_col + 1 >= cols {
            self.primary_linefeed(pane);
        }

        self.ensure_primary_cell();

        let row = &mut self.primary_grid[self.primary_cursor_row];
        let col = self.primary_cursor_col;
        if col < row.len() {
            row[col] = Cell::styled(ch, self.current_style.clone());
        } else {
            // ensure_primary_cell already padded up to `col`, so this
            // fills the slot the cursor was about to land on.
            row.push(Cell::styled(ch, self.current_style.clone()));
        }

        if width == 2 {
            // Continuation sentinel keeps cell-count-equals-display-width
            // so layout math stays honest. Same convention as the alt
            // screen.
            let cont_col = col + 1;
            if cont_col < row.len() {
                row[cont_col] = Cell::styled('\0', self.current_style.clone());
            } else {
                row.push(Cell::styled('\0', self.current_style.clone()));
            }
        }

        let advance = width;
        if self.primary_cursor_col + advance >= cols {
            // xterm-style delayed autowrap: landing in the last column
            // arms wrap but does not move yet. A following printable
            // character performs the wrap; cursor controls such as CR or
            // CUU clear the pending state and stay on the current row.
            if self.primary_auto_wrap {
                self.primary_wrap_pending = true;
            } else {
                self.primary_cursor_col = cols.saturating_sub(1);
                self.primary_wrap_pending = false;
            }
        } else {
            self.primary_cursor_col += advance;
            self.primary_wrap_pending = false;
        }
    }

    fn primary_scroll_up_within_region(&mut self, count: usize) {
        let max_row = self.alternate.rows.saturating_sub(1);
        let top = self.primary_scroll_top.min(max_row);
        let bottom = self.primary_scroll_bottom.min(max_row);
        if top >= bottom {
            return;
        }

        while self.primary_grid.len() <= bottom {
            self.primary_grid.push(Vec::new());
        }

        let count = count.min(bottom.saturating_sub(top) + 1);
        let blank = self.primary_blank_row();
        let region = &mut self.primary_grid[top..=bottom];
        region.rotate_left(count);
        let len = region.len();
        for row in region.iter_mut().skip(len.saturating_sub(count)) {
            *row = blank.clone();
        }
    }

    // Lazily build out the grid so `(cursor_row, cursor_col)` is
    // addressable. New rows are appended as empty `Vec<Cell>`s; the
    // cursor's row is padded with `Cell::BLANK` until it reaches at
    // least `cursor_col` cells (the cell at index cursor_col itself is
    // either appended or overwritten by the caller).
    fn ensure_primary_cell(&mut self) {
        while self.primary_grid.len() <= self.primary_cursor_row {
            self.primary_grid.push(Vec::new());
        }
        let row = &mut self.primary_grid[self.primary_cursor_row];
        while row.len() < self.primary_cursor_col {
            row.push(Cell::BLANK);
        }
    }
}

#[derive(Debug)]
struct AlternateScreen {
    rows: usize,
    cols: usize,
    cells: Vec<Vec<Cell>>,
    cursor_row: usize,
    cursor_col: usize,
    saved_cursor_row: usize,
    saved_cursor_col: usize,
    scroll_top: usize,
    scroll_bottom: usize,
    auto_wrap: bool,
    wrap_pending: bool,
    // The cell used when clear/erase/insert ops blank a region. It mirrors
    // the SGR state at the time of the op so programs that set a
    // background color and then erase (common in TUIs) actually see that
    // background on the cleared cells. Updated by the ingest whenever
    // current_style changes.
    fill: Cell,
}

impl AlternateScreen {
    fn new(rows: usize, cols: usize) -> Self {
        let rows = rows.max(1);
        let cols = cols.max(1);
        Self {
            rows,
            cols,
            cells: vec![vec![Cell::BLANK; cols]; rows],
            cursor_row: 0,
            cursor_col: 0,
            saved_cursor_row: 0,
            saved_cursor_col: 0,
            scroll_top: 0,
            scroll_bottom: rows.saturating_sub(1),
            auto_wrap: true,
            wrap_pending: false,
            fill: Cell::BLANK,
        }
    }

    fn set_fill_style(&mut self, style: Style) {
        self.fill = Cell::styled(' ', style);
    }

    fn set_auto_wrap(&mut self, enabled: bool) {
        self.auto_wrap = enabled;
        if !enabled {
            self.wrap_pending = false;
        }
    }

    fn rows(&self) -> usize {
        self.rows
    }

    fn resize(&mut self, rows: usize, cols: usize) {
        let rows = rows.max(1);
        let cols = cols.max(1);
        let mut cells = vec![vec![Cell::BLANK; cols]; rows];

        for (row_index, row) in self.cells.iter().enumerate().take(rows) {
            for (col_index, cell) in row.iter().enumerate().take(cols) {
                cells[row_index][col_index] = cell.clone();
            }
        }

        self.rows = rows;
        self.cols = cols;
        self.cells = cells;
        self.cursor_row = self.cursor_row.min(self.rows.saturating_sub(1));
        self.cursor_col = self.cursor_col.min(self.cols.saturating_sub(1));
        self.saved_cursor_row = self.saved_cursor_row.min(self.rows.saturating_sub(1));
        self.saved_cursor_col = self.saved_cursor_col.min(self.cols.saturating_sub(1));
        self.wrap_pending = false;
        self.scroll_top = self.scroll_top.min(self.rows.saturating_sub(1));
        self.scroll_bottom = self.scroll_bottom.min(self.rows.saturating_sub(1));
        if self.scroll_top >= self.scroll_bottom {
            self.scroll_top = 0;
            self.scroll_bottom = self.rows.saturating_sub(1);
        }
    }

    fn cells(&self) -> &[Vec<Cell>] {
        &self.cells
    }

    fn set_cursor(&mut self, row: usize, col: usize) {
        self.wrap_pending = false;
        self.cursor_row = row.min(self.rows.saturating_sub(1));
        self.cursor_col = col.min(self.cols.saturating_sub(1));
    }

    fn save_cursor(&mut self) {
        self.saved_cursor_row = self.cursor_row;
        self.saved_cursor_col = self.cursor_col;
    }

    fn restore_cursor(&mut self) {
        self.set_cursor(self.saved_cursor_row, self.saved_cursor_col);
    }

    fn cursor_position(&self) -> (usize, usize) {
        (
            self.cursor_row.saturating_add(1),
            self.cursor_col.saturating_add(1),
        )
    }

    fn horizontal_absolute(&mut self, col: usize) {
        self.wrap_pending = false;
        self.cursor_col = col.min(self.cols.saturating_sub(1));
    }

    fn vertical_absolute(&mut self, row: usize) {
        self.wrap_pending = false;
        self.cursor_row = row.min(self.rows.saturating_sub(1));
    }

    fn move_cursor(&mut self, row_delta: isize, col_delta: isize) {
        let row = self.cursor_row.saturating_add_signed(row_delta);
        let col = self.cursor_col.saturating_add_signed(col_delta);
        self.set_cursor(row, col);
    }

    fn set_scroll_region(&mut self, top: usize, bottom: usize) {
        if top >= self.rows || bottom >= self.rows || top >= bottom {
            return;
        }

        self.scroll_top = top;
        self.scroll_bottom = bottom;
        self.set_cursor(0, 0);
    }

    fn reset(&mut self) {
        self.clear_all();
        self.scroll_top = 0;
        self.scroll_bottom = self.rows.saturating_sub(1);
        self.auto_wrap = true;
        self.wrap_pending = false;
        self.set_cursor(0, 0);
        self.save_cursor();
    }

    fn index(&mut self) {
        self.wrap_pending = false;
        if self.cursor_row == self.scroll_bottom {
            self.scroll_up_within_region(1);
        } else {
            self.cursor_row = (self.cursor_row + 1).min(self.rows.saturating_sub(1));
        }
    }

    fn reverse_index(&mut self) {
        self.wrap_pending = false;
        if self.cursor_row == self.scroll_top {
            self.scroll_down_within_region(1);
        } else {
            self.cursor_row -= 1;
        }
    }

    fn linefeed(&mut self) {
        self.index();
    }

    fn carriage_return(&mut self) {
        self.wrap_pending = false;
        self.cursor_col = 0;
    }

    fn next_line(&mut self, count: usize) {
        // Cap at 2×rows: once the cursor has reached the scroll bottom
        // and the region has scrolled through its full height, every
        // further linefeed leaves the screen in the same state, so a
        // larger count is wasted work (parse_params already bounds the
        // value; this keeps the loop O(rows) regardless).
        let count = count.max(1).min(self.rows.saturating_mul(2).max(2));
        for _ in 0..count {
            self.linefeed();
        }
        self.carriage_return();
    }

    fn previous_line(&mut self, count: usize) {
        self.wrap_pending = false;
        self.cursor_row = self.cursor_row.saturating_sub(count.max(1));
        self.carriage_return();
    }

    fn backspace(&mut self) {
        self.wrap_pending = false;
        self.cursor_col = self.cursor_col.saturating_sub(1);
    }

    fn put_char(&mut self, ch: char, style: Style) {
        if self.cursor_row >= self.rows || self.cursor_col >= self.cols {
            return;
        }

        let width = crate::style::char_width(ch).max(1);

        if self.wrap_pending {
            if self.auto_wrap {
                self.linefeed();
            }
            self.wrap_pending = false;
        }

        // Wide-char wrap: if a width-2 glyph would straddle the right
        // edge, bump to the next line and re-anchor at column 0 before
        // writing. Without this, East-Asian text shears at wrap points.
        if self.auto_wrap && width == 2 && self.cursor_col + 1 >= self.cols {
            self.cursor_col = 0;
            self.linefeed();
            if self.cursor_row >= self.rows {
                return;
            }
        }

        self.cells[self.cursor_row][self.cursor_col] = Cell::styled(ch, style.clone());

        if width == 2 && self.cursor_col + 1 < self.cols {
            // Continuation sentinel keeps the cell count aligned with
            // display width so layout math doesn't drift. serialize_row
            // skips '\0' cells when emitting.
            self.cells[self.cursor_row][self.cursor_col + 1] = Cell::styled('\0', style);
        }

        let advance = width;
        if self.cursor_col + advance >= self.cols {
            if self.auto_wrap {
                self.wrap_pending = true;
            } else {
                self.cursor_col = self.cols.saturating_sub(1);
                self.wrap_pending = false;
            }
        } else {
            self.cursor_col += advance;
            self.wrap_pending = false;
        }
    }

    fn insert_lines(&mut self, count: usize) {
        self.wrap_pending = false;
        if self.cursor_row < self.scroll_top || self.cursor_row > self.scroll_bottom {
            return;
        }

        let count = count.min(self.scroll_bottom.saturating_sub(self.cursor_row) + 1);
        let region = &mut self.cells[self.cursor_row..=self.scroll_bottom];
        region.rotate_right(count);
        for row in region.iter_mut().take(count) {
            row.fill(self.fill.clone());
        }
    }

    fn delete_lines(&mut self, count: usize) {
        self.wrap_pending = false;
        if self.cursor_row < self.scroll_top || self.cursor_row > self.scroll_bottom {
            return;
        }

        let count = count.min(self.scroll_bottom.saturating_sub(self.cursor_row) + 1);
        let region = &mut self.cells[self.cursor_row..=self.scroll_bottom];
        region.rotate_left(count);
        let len = region.len();
        for row in region.iter_mut().skip(len.saturating_sub(count)) {
            row.fill(self.fill.clone());
        }
    }

    fn insert_chars(&mut self, count: usize) {
        self.wrap_pending = false;
        if self.cursor_row >= self.rows || self.cursor_col >= self.cols {
            return;
        }

        let count = count.min(self.cols.saturating_sub(self.cursor_col));
        let row = &mut self.cells[self.cursor_row];
        row[self.cursor_col..].rotate_right(count);
        for cell in row.iter_mut().skip(self.cursor_col).take(count) {
            *cell = self.fill.clone();
        }
    }

    fn delete_chars(&mut self, count: usize) {
        self.wrap_pending = false;
        if self.cursor_row >= self.rows || self.cursor_col >= self.cols {
            return;
        }

        let count = count.min(self.cols.saturating_sub(self.cursor_col));
        let row = &mut self.cells[self.cursor_row];
        row[self.cursor_col..].rotate_left(count);
        let len = row.len();
        for cell in row.iter_mut().skip(len.saturating_sub(count)) {
            *cell = self.fill.clone();
        }
    }

    fn erase_display(&mut self, mode: usize) {
        self.wrap_pending = false;
        match mode {
            0 => {
                self.erase_line(0);
                for row in self.cursor_row + 1..self.rows {
                    self.cells[row].fill(self.fill.clone());
                }
            }
            1 => {
                for row in 0..self.cursor_row {
                    self.cells[row].fill(self.fill.clone());
                }
                for col in 0..=self.cursor_col.min(self.cols.saturating_sub(1)) {
                    self.cells[self.cursor_row][col] = self.fill.clone();
                }
            }
            _ => self.clear_all(),
        }
    }

    fn erase_line(&mut self, mode: usize) {
        self.wrap_pending = false;
        if self.cursor_row >= self.rows {
            return;
        }

        match mode {
            1 => {
                for col in 0..=self.cursor_col.min(self.cols.saturating_sub(1)) {
                    self.cells[self.cursor_row][col] = self.fill.clone();
                }
            }
            2 => self.cells[self.cursor_row].fill(self.fill.clone()),
            _ => {
                for col in self.cursor_col..self.cols {
                    self.cells[self.cursor_row][col] = self.fill.clone();
                }
            }
        }
    }

    fn erase_chars(&mut self, count: usize) {
        self.wrap_pending = false;
        if self.cursor_row >= self.rows {
            return;
        }

        let end = self.cursor_col.saturating_add(count).min(self.cols);
        for col in self.cursor_col..end {
            self.cells[self.cursor_row][col] = self.fill.clone();
        }
    }

    fn clear_all(&mut self) {
        self.wrap_pending = false;
        for row in &mut self.cells {
            row.fill(self.fill.clone());
        }
    }

    fn scroll_up_within_region(&mut self, count: usize) {
        self.wrap_pending = false;
        let count = count.min(self.scroll_bottom.saturating_sub(self.scroll_top) + 1);
        let region = &mut self.cells[self.scroll_top..=self.scroll_bottom];
        region.rotate_left(count);
        let len = region.len();
        for row in region.iter_mut().skip(len.saturating_sub(count)) {
            row.fill(self.fill.clone());
        }
    }

    fn scroll_down_within_region(&mut self, count: usize) {
        self.wrap_pending = false;
        let count = count.min(self.scroll_bottom.saturating_sub(self.scroll_top) + 1);
        let region = &mut self.cells[self.scroll_top..=self.scroll_bottom];
        region.rotate_right(count);
        for row in region.iter_mut().take(count) {
            row.fill(self.fill.clone());
        }
    }
}

// Caps on the CSI / OSC accumulation buffers. No real CSI body comes
// close to 1 KiB; OSC gets 1 MiB (the same allowance as the
// synchronized-output buffer) since titles and hyperlinks are small
// but future pass-through payloads (OSC 52 clipboard) can be large.
// Overflowing sequences are consumed and discarded as malformed —
// the alternative is a slow OOM on a stream that never terminates.
const CSI_BUFFER_MAX: usize = 1 << 10;
const OSC_BUFFER_MAX: usize = 1 << 20;

// The literal byte sequence that closes a Synchronized Output region
// (DECRST 2026). Detected by `ingest_bytes` while the sync buffer is
// active; we don't route it through the CSI parser because the buffer
// has already diverted those bytes.
const ESU_BYTES: &[u8] = b"\x1b[?2026l";

// Hard cap on the synchronized-output buffer. We trust well-behaved
// hosts; a normal sync update is a few KB. Misbehaving ones (BSU
// without ESU, or runaway output between them) get observable frame
// tearing — we flush the buffer non-atomically and warn — instead of
// memory exhaustion.
const SYNCHRONIZED_BUFFER_MAX: usize = 1 << 20;

// Locate `needle` inside `haystack` and return the start index, or None
// if absent. We don't need substring partial-match recovery for sync
// output — `find_subslice` is called on each feed, and any leading
// partial-ESU bytes simply remain in the buffer until the rest arrives
// in a subsequent feed.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

// Which terminator the host used to close the OSC sequence. We mirror
// it on any synthesized reply because some consumers only accept one
// form: pre-2018 xterm replies always end in ST, modern shells often
// send BEL and only handle BEL on the way back.
#[derive(Debug, Clone, Copy)]
enum OscTerminator {
    Bel,
    St,
}

impl OscTerminator {
    fn bytes(self) -> &'static [u8] {
        match self {
            OscTerminator::Bel => b"\x07",
            OscTerminator::St => b"\x1b\\",
        }
    }
}

fn build_color_reply(code: &[u8], rgb: &[u8], terminator: OscTerminator) -> Vec<u8> {
    let mut out = Vec::with_capacity(code.len() + rgb.len() + 8);
    out.extend_from_slice(b"\x1b]");
    out.extend_from_slice(code);
    out.extend_from_slice(b";rgb:");
    out.extend_from_slice(rgb);
    out.extend_from_slice(terminator.bytes());
    out
}

// Parses an OSC 8 hyperlink sequence. The payload format is
// `8;<params>;<URL>` where params is an optional colon-separated list of
// key=value pairs (commonly `id=abc`). An empty URL closes the current
// link. Returns:
//   Outer None → not an OSC 8 sequence at all, keep trying other shapes
//   Some(None) → OSC 8 close link
//   Some(Some(url)) → OSC 8 open link to url
fn parse_osc_hyperlink(payload: &str) -> Option<Option<std::sync::Arc<str>>> {
    let rest = payload.strip_prefix("8;")?;
    let (_params, url) = rest.split_once(';')?;
    if url.is_empty() {
        Some(None)
    } else {
        Some(Some(std::sync::Arc::from(url)))
    }
}

// Captures the text from an xterm window-title OSC: `0;title` (icon +
// window), `1;title` (icon only), or `2;title` (window only). We treat
// all three as "pane title" — we don't distinguish icon from window
// since zmux has only one label slot per pane. Returns None if the
// payload isn't a recognized title OSC so the caller can keep trying
// other interpretations.
fn parse_osc_title(payload: &str) -> Option<String> {
    let rest = payload
        .strip_prefix("0;")
        .or_else(|| payload.strip_prefix("1;"))
        .or_else(|| payload.strip_prefix("2;"))?;
    Some(rest.to_string())
}

// Build an xterm OSC 4 palette-query reply. xterm replies with the entry's
// RGB triple in its own 16-bit-padded hex form: `rgb:RRRR/GGGG/BBBB`. We
// route the index through color_from_256 so programs like neovim's
// `&termguicolors` probe get a sensible answer instead of "unhandled".
// The terminator byte(s) mirror the one the host used on the request —
// some legacy clients only recognize the form they sent and silently
// drop the other.
fn parse_osc_palette_query(payload: &str, terminator: OscTerminator) -> Option<Vec<u8>> {
    let rest = payload.strip_prefix("4;")?;
    let (index_str, tail) = rest.split_once(';')?;
    if tail != "?" {
        return None;
    }
    let index: u8 = index_str.parse().ok()?;
    let (r, g, b) = crate::style::rgb_from_256(index)?;
    let expand = |c: u8| ((c as u16) << 8) | c as u16;
    let reply = format!(
        "\x1b]4;{index};rgb:{:04x}/{:04x}/{:04x}",
        expand(r),
        expand(g),
        expand(b),
    );
    let mut out = reply.into_bytes();
    out.extend_from_slice(terminator.bytes());
    Some(out)
}

// CSI parameters are clamped to 65 535, matching xterm. Screen
// coordinates fit in u16, so no legitimate parameter exceeds this —
// but an unclamped count is a CPU-exhaustion vector (`CSI
// 18446744073709551615 b` would spin the REP loop for centuries) and
// an overflow vector (`cursor + count` arithmetic in the movement
// handlers). Values that don't parse at all (non-numeric, colon
// sub-params) stay `None` and pick up each control's default.
const CSI_PARAM_MAX: usize = 65_535;

// A single SGR value: empty → 0 (the CSI default), non-numeric → 0
// (matching the old flatten behavior for garbage), clamped like every
// other CSI parameter.
fn sgr_value(text: &str) -> u16 {
    if text.is_empty() {
        return 0;
    }
    text.parse::<usize>()
        .map(|value| value.min(CSI_PARAM_MAX) as u16)
        .unwrap_or(0)
}

fn parse_params(body: &str) -> Vec<Option<usize>> {
    if body.is_empty() {
        return Vec::new();
    }

    body.split(';')
        .map(|part| {
            if part.is_empty() {
                None
            } else {
                part.parse::<usize>()
                    .ok()
                    .map(|value| value.min(CSI_PARAM_MAX))
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::mouse::{MouseTrackingMode, ScreenMode};
    use crate::style::Color;
    use crate::{Pane, PtySize};

    use super::{CursorShape, TerminalIngest};

    #[test]
    fn clear_command_empties_primary_scrollback() {
        // The standard `clear` command on an xterm-256color TERM emits
        // `\x1b[H\x1b[2J\x1b[3J` — cursor home, erase display, erase
        // saved lines. Before this handler, primary-mode zmux silently
        // ignored all three and the old content stayed visible. Now
        // modes 2 and 3 both wipe scrollback AND the live grid.
        let mut pane = Pane::new("shell", 64, 4);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(&mut pane, b"line1\nline2\nline3\n");
        // The 2D grid keeps streamed lines live (so CUU can revisit them)
        // until they evict to scrollback or until flush. Surface the
        // committed line count via a flush before checking.
        ingest.flush_incomplete_line(&mut pane);
        assert_eq!(pane.total_lines(), 3);

        ingest.ingest_bytes(&mut pane, b"\x1b[H\x1b[2J\x1b[3J");
        assert_eq!(
            pane.total_lines(),
            0,
            "clear must drop every retained scrollback line",
        );

        // Post-clear writes land on a fresh buffer at line 0.
        ingest.ingest_bytes(&mut pane, b"after\n");
        ingest.flush_incomplete_line(&mut pane);
        assert_eq!(pane.visible_text(), vec!["after"]);
    }

    #[test]
    fn strips_basic_ansi_sequences_and_keeps_lines() {
        let mut pane = Pane::new("shell", 16, 4);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(
            &mut pane,
            b"\x1b[31mhello\x1b[0m\nplain\n\x1b]0;title\x07done\n",
        );
        // The grid holds streamed rows live; flush to surface them in
        // pane.visible_text(). Trailing empty row from the final `\n`
        // gets included.
        ingest.flush_incomplete_line(&mut pane);

        assert_eq!(pane.visible_text(), vec!["hello", "plain", "done"]);
    }

    #[test]
    fn primary_screen_preserves_sgr_colors_into_scrollback() {
        // Previously the primary path stripped SGR; now each Cell should
        // carry the style that was active when its char was written.
        let mut pane = Pane::new("shell", 16, 4);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(&mut pane, b"\x1b[31mred\x1b[0m plain\n");
        ingest.flush_incomplete_line(&mut pane);

        let lines = pane.visible_lines();
        assert_eq!(lines.len(), 1);
        let row = &lines[0];
        assert_eq!(row.len(), "red plain".len());
        // 'r' 'e' 'd' should carry fg Color::Indexed(1)
        for cell in &row[..3] {
            match cell.style.fg {
                crate::style::Color::Indexed(1) => {}
                other => panic!("expected red fg on 'red' chars, got {other:?}"),
            }
        }
        // ' ' 'p' 'l' 'a' 'i' 'n' should be default-styled
        for cell in &row[3..] {
            assert_eq!(
                cell.style,
                crate::style::Style::DEFAULT,
                "plain chars must reset to default"
            );
        }
    }

    #[test]
    fn can_flush_an_incomplete_line() {
        let mut pane = Pane::new("shell", 16, 4);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(&mut pane, b"prompt> ");
        ingest.flush_incomplete_line(&mut pane);

        assert_eq!(pane.visible_text(), vec!["prompt> "]);
    }

    #[test]
    fn can_render_basic_alternate_screen_output() {
        let mut pane = Pane::new("shell", 16, 4);
        let mut ingest = TerminalIngest::new(PtySize::new(4, 8));

        ingest.ingest_bytes(&mut pane, b"\x1b[?1049h\x1b[2J\x1b[Htop\x1b[2;3Hcpu");

        assert_eq!(pane.screen_mode(), ScreenMode::Alternate);
        let lines = ingest.render_lines(&pane);
        assert_eq!(lines[0], "top");
        assert_eq!(lines[1], "  cpu");
    }

    #[test]
    fn utf8_box_drawing_chars_survive_the_ingest() {
        // btop and htop render bar graphs with Unicode block characters.
        // Before UTF-8 decoding was wired up, high bytes were dropped
        // silently and these glyphs never appeared on the alternate
        // screen.
        let mut pane = Pane::new("tui", 16, 4);
        let mut ingest = TerminalIngest::new(PtySize::new(4, 8));

        // \u{2502} = │ (box drawing vertical) → 0xE2 0x94 0x82
        // \u{2588} = █ (full block)          → 0xE2 0x96 0x88
        ingest.ingest_bytes(&mut pane, "\x1b[?1049h\x1b[H│█".as_bytes());

        let lines = ingest.render_lines(&pane);
        assert!(lines[0].contains('│'), "expected │ in {:?}", lines[0]);
        assert!(lines[0].contains('█'), "expected █ in {:?}", lines[0]);
    }

    #[test]
    fn gemini_startup_capture_lands_text_in_render_lines() {
        // Regression: gemini-cli's startup output drives the cursor up
        // a few rows and re-draws in place (\e[1A\e[2K\e[G ... reprint).
        // After ingest the snapshot path was returning empty even though
        // the same render_lines is what the workspace compositor renders
        // to attached terminals. The fixture is a real PTY capture
        // taken from `zmux capture`.
        let bytes = include_bytes!("../tests/fixtures/gemini-startup.bin");
        let mut pane = Pane::new("gemini-test", 51, 51);
        let mut ingest = TerminalIngest::new(PtySize::new(51, 51));
        ingest.ingest_bytes(&mut pane, bytes);

        let lines = ingest.render_lines(&pane);
        let joined = lines.join("\n");
        eprintln!(
            "render produced {} lines, primary_grid.len()={}, flushed={}",
            lines.len(),
            ingest.primary_grid.len(),
            ingest.primary_flushed_to_scrollback,
        );
        eprintln!("rendered text:\n{joined}");

        // Whatever we render must include gemini's prompt text — that's
        // what the user sees on the screen at this point. The bug being
        // diagnosed: this assertion currently fails because the snapshot
        // returns empty rows.
        assert!(
            joined.contains("Type your message"),
            "render_lines should contain gemini's prompt; got {} lines:\n{joined}",
            lines.len()
        );
    }

    #[test]
    fn utf8_box_drawing_chars_survive_on_primary_screen() {
        // Same as the alt-screen variant above but without the alt-screen
        // switch. Regression check for the dogfooded mojibake we saw
        // when claude/gemini draw their splash on the primary screen:
        // box chars came through as one Latin-1-ish codepoint per UTF-8
        // byte, e.g. ╭ → â + (control) + ­.
        let mut pane = Pane::new("tui", 16, 4);
        let mut ingest = TerminalIngest::new(PtySize::new(4, 16));

        // ╭ = U+256D (E2 95 AD), ─ = U+2500 (E2 94 80), ╮ = U+256E (E2 95 AE)
        ingest.ingest_bytes(&mut pane, "╭─╮".as_bytes());

        let lines = ingest.render_lines(&pane);
        let joined = lines.join("\n");
        assert!(joined.contains('╭'), "expected ╭ in {joined:?}");
        assert!(joined.contains('─'), "expected ─ in {joined:?}");
        assert!(joined.contains('╮'), "expected ╮ in {joined:?}");
    }

    #[test]
    fn malformed_utf8_does_not_crash_or_hang_the_ingest() {
        let mut pane = Pane::new("tui", 16, 4);
        let mut ingest = TerminalIngest::new(PtySize::new(4, 8));

        // Start of a 2-byte sequence, then ASCII 'x' (malformed — no
        // continuation). We should discard the partial sequence and
        // still place 'x' on the screen.
        ingest.ingest_bytes(&mut pane, b"\x1b[?1049h\x1b[H\xC3x");
        let lines = ingest.render_lines(&pane);
        assert!(lines[0].contains('x'), "x must survive in {:?}", lines[0]);
    }

    #[test]
    fn leaving_alternate_screen_restores_primary_rendering() {
        // Wider PTY than before (16 cols vs 8) so "shell prompt" (12
        // chars) doesn't wrap on the new 2D primary grid. The intent of
        // this test is unchanged: verify that flipping into and back out
        // of the alt screen leaves the primary rendering intact.
        let mut pane = Pane::new("shell", 16, 4);
        let mut ingest = TerminalIngest::new(PtySize::new(4, 16));

        ingest.ingest_bytes(&mut pane, b"shell prompt");
        ingest.ingest_bytes(&mut pane, b"\x1b[?1049h\x1b[2J\x1b[Hfull");
        ingest.ingest_bytes(&mut pane, b"\x1b[?1049l");

        assert_eq!(pane.screen_mode(), ScreenMode::Primary);
        assert_eq!(ingest.render_lines(&pane), vec!["shell prompt"]);
    }

    #[test]
    fn mouse_private_modes_toggle_app_mouse_capture() {
        let mut pane = Pane::new("shell", 16, 4);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(&mut pane, b"\x1b[?1000h");
        assert_eq!(pane.mouse_tracking_mode(), MouseTrackingMode::Click);

        ingest.ingest_bytes(&mut pane, b"\x1b[?1000l");
        assert!(!pane.app_captures_mouse());
    }

    #[test]
    fn higher_mouse_tracking_modes_are_retained() {
        let mut pane = Pane::new("shell", 16, 4);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(&mut pane, b"\x1b[?1002h");
        assert_eq!(pane.mouse_tracking_mode(), MouseTrackingMode::Drag);

        ingest.ingest_bytes(&mut pane, b"\x1b[?1003h");
        assert_eq!(pane.mouse_tracking_mode(), MouseTrackingMode::Motion);
    }

    #[test]
    fn primary_screen_handles_crlf_without_losing_the_line() {
        let mut pane = Pane::new("shell", 16, 4);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(&mut pane, b"booting\r\nready\r\n");
        ingest.flush_incomplete_line(&mut pane);

        assert_eq!(pane.visible_text(), vec!["booting", "ready"]);
    }

    #[test]
    fn carriage_return_allows_primary_line_overwrite() {
        let mut pane = Pane::new("shell", 16, 4);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(&mut pane, b"foo\rbar");

        assert_eq!(ingest.render_lines(&pane), vec!["bar"]);
    }

    #[test]
    fn carriage_return_after_full_width_row_redraws_same_row() {
        let mut pane = Pane::new("shell", 16, 3);
        let mut ingest = TerminalIngest::new(PtySize::new(3, 8));

        ingest.ingest_bytes(&mut pane, b"--------\rspin\x1b[K");

        let rendered = ingest.render_lines(&pane);
        assert_eq!(
            rendered[0], "spin",
            "CR after last-column write must not move down"
        );
        assert_eq!(
            rendered.get(1).map(String::as_str),
            None,
            "spinner redraw leaked onto a new line: {rendered:?}",
        );
    }

    #[test]
    fn next_print_after_full_width_row_performs_delayed_wrap() {
        let mut pane = Pane::new("shell", 16, 3);
        let mut ingest = TerminalIngest::new(PtySize::new(3, 8));

        ingest.ingest_bytes(&mut pane, b"--------x");

        let rendered = ingest.render_lines(&pane);
        assert_eq!(rendered[0], "--------");
        assert_eq!(rendered[1], "x");
    }

    #[test]
    fn carriage_return_after_alternate_full_width_row_redraws_same_row() {
        let mut pane = Pane::new("tui", 16, 3);
        let mut ingest = TerminalIngest::new(PtySize::new(3, 8));

        ingest.ingest_bytes(&mut pane, b"\x1b[?1049h--------\rspin\x1b[K");

        let rendered = ingest.render_lines(&pane);
        assert_eq!(
            rendered[0], "spin",
            "alternate CR after last-column write must stay on the same row"
        );
        assert_eq!(
            rendered.get(1).map(String::as_str),
            None,
            "alternate redraw leaked onto a new line: {rendered:?}",
        );
    }

    #[test]
    fn alternate_full_width_bottom_row_does_not_scroll_until_next_print() {
        let mut pane = Pane::new("tui", 16, 3);
        let mut ingest = TerminalIngest::new(PtySize::new(3, 8));

        ingest.ingest_bytes(&mut pane, b"\x1b[?1049htop\x1b[3;1H--------");

        let rendered = ingest.render_lines(&pane);
        assert_eq!(
            rendered[0], "top",
            "writing exactly to the last cell must not scroll the alt screen: {rendered:?}"
        );
        assert_eq!(rendered[2], "--------");
    }

    #[test]
    fn decawm_disabled_prevents_primary_right_edge_wrap() {
        let mut pane = Pane::new("shell", 16, 3);
        let mut ingest = TerminalIngest::new(PtySize::new(3, 8));

        ingest.ingest_bytes(&mut pane, b"\x1b[?7l--------x");

        let rendered = ingest.render_lines(&pane);
        assert_eq!(rendered[0], "-------x");
        assert_eq!(
            rendered.get(1).map(String::as_str),
            None,
            "DECAWM disabled should not spill into the next row: {rendered:?}"
        );
    }

    #[test]
    fn decawm_disabled_prevents_alternate_right_edge_wrap() {
        let mut pane = Pane::new("tui", 16, 3);
        let mut ingest = TerminalIngest::new(PtySize::new(3, 8));

        ingest.ingest_bytes(&mut pane, b"\x1b[?1049h\x1b[?7l--------x");

        let rendered = ingest.render_lines(&pane);
        assert_eq!(rendered[0], "-------x");
        assert_eq!(
            rendered.get(1).map(String::as_str),
            None,
            "alternate-screen DECAWM disabled should not wrap: {rendered:?}"
        );
    }

    #[test]
    fn ris_clears_primary_screen_for_client_teardown() {
        let mut pane = Pane::new("shell", 16, 4);
        let mut ingest = TerminalIngest::new(PtySize::new(4, 16));

        ingest.ingest_bytes(&mut pane, b"client ui\nstatus line\x1bc$>");

        assert_eq!(pane.screen_mode(), ScreenMode::Primary);
        assert_eq!(ingest.render_lines(&pane), vec!["$>"]);
    }

    #[test]
    fn decstr_resets_primary_margins_and_modes_without_erasing() {
        let mut pane = Pane::new("shell", 16, 4);
        let mut ingest = TerminalIngest::new(PtySize::new(4, 16));

        ingest.ingest_bytes(&mut pane, b"still visible\x1b[2;3r\x1b[?2004h\x1b[!p");

        assert_eq!(ingest.render_lines(&pane), vec!["still visible"]);
        assert_eq!(ingest.primary_scroll_top, 0);
        assert_eq!(ingest.primary_scroll_bottom, 3);
        assert!(!ingest.bracketed_paste_enabled());
    }

    #[test]
    fn csi_save_restore_cursor_supports_tui_repaint_patterns() {
        let mut pane = Pane::new("shell", 16, 5);
        let mut ingest = TerminalIngest::new(PtySize::new(5, 12));

        ingest.ingest_bytes(
            &mut pane,
            b"\x1b[?1049h\x1b[2J\x1b[Hhead\x1b[s\x1b[3;5Hbody\x1b[uok",
        );

        let lines = ingest.render_lines(&pane);
        assert_eq!(lines[0], "headok");
        assert_eq!(lines[2], "    body");
    }

    #[test]
    fn dec_save_restore_cursor_sequences_are_supported() {
        let mut pane = Pane::new("shell", 16, 5);
        let mut ingest = TerminalIngest::new(PtySize::new(5, 12));

        ingest.ingest_bytes(&mut pane, b"\x1b[?1049h\x1b[Hxy\x1b7\x1b[4;4Hzz\x1b8!");

        let lines = ingest.render_lines(&pane);
        assert_eq!(lines[0], "xy!");
        assert_eq!(lines[3], "   zz");
    }

    #[test]
    fn horizontal_absolute_and_erase_chars_update_the_alternate_screen() {
        let mut pane = Pane::new("shell", 16, 5);
        let mut ingest = TerminalIngest::new(PtySize::new(5, 12));

        ingest.ingest_bytes(
            &mut pane,
            b"\x1b[?1049h\x1b[Habcdef\x1b[1G\x1b[3X\x1b[2;1Hrow2",
        );

        let lines = ingest.render_lines(&pane);
        assert_eq!(lines[0], "   def");
        assert_eq!(lines[1], "row2");
    }

    #[test]
    fn scroll_regions_limit_reverse_index_to_the_configured_band() {
        let mut pane = Pane::new("shell", 16, 5);
        let mut ingest = TerminalIngest::new(PtySize::new(5, 8));

        ingest.ingest_bytes(
            &mut pane,
            b"\x1b[?1049h\x1b[Haaa\x1b[2;1Hbbb\x1b[3;1Hccc\x1b[4;1Hddd\x1b[2;4r\x1b[2;1H\x1bM",
        );

        let lines = ingest.render_lines(&pane);
        assert_eq!(lines[0], "aaa");
        assert_eq!(lines[1], "");
        assert_eq!(lines[2], "bbb");
        assert_eq!(lines[3], "ccc");
    }

    #[test]
    fn insert_and_delete_lines_shift_only_within_the_scroll_region() {
        let mut pane = Pane::new("shell", 16, 6);
        let mut ingest = TerminalIngest::new(PtySize::new(6, 8));

        ingest.ingest_bytes(
            &mut pane,
            b"\x1b[?1049h\x1b[H1\x1b[2;1H2\x1b[3;1H3\x1b[4;1H4\x1b[2;4r\x1b[3;1H\x1b[L",
        );

        let lines = ingest.render_lines(&pane);
        assert_eq!(lines[0], "1");
        assert_eq!(lines[1], "2");
        assert_eq!(lines[2], "");
        assert_eq!(lines[3], "3");

        ingest.ingest_bytes(&mut pane, b"\x1b[2;1H\x1b[M");
        let lines = ingest.render_lines(&pane);
        assert_eq!(lines[0], "1");
        assert_eq!(lines[1], "");
        assert_eq!(lines[2], "3");
    }

    #[test]
    fn insert_and_delete_chars_support_editor_style_redraws() {
        let mut pane = Pane::new("shell", 16, 5);
        let mut ingest = TerminalIngest::new(PtySize::new(5, 12));

        ingest.ingest_bytes(&mut pane, b"\x1b[?1049h\x1b[Habcdef\x1b[1;2H\x1b[2@ZZ");
        let lines = ingest.render_lines(&pane);
        assert_eq!(lines[0], "aZZbcdef");

        ingest.ingest_bytes(&mut pane, b"\x1b[1;2H\x1b[2P");
        let lines = ingest.render_lines(&pane);
        assert_eq!(lines[0], "abcdef");
    }

    #[test]
    fn cursor_position_queries_receive_a_reply() {
        let mut pane = Pane::new("shell", 16, 5);
        let mut ingest = TerminalIngest::new(PtySize::new(5, 12));

        let reply = ingest.ingest_bytes(&mut pane, b"\x1b[?1049h\x1b[3;4H\x1b[6n");

        assert_eq!(reply, b"\x1b[3;4R");
    }

    #[test]
    fn osc_color_queries_receive_terminal_defaults() {
        let mut pane = Pane::new("shell", 16, 5);
        let mut ingest = TerminalIngest::default();

        let reply = ingest.ingest_bytes(&mut pane, b"\x1b]11;?\x07");

        assert_eq!(reply, b"\x1b]11;rgb:0000/0000/0000\x07");
    }

    #[test]
    fn osc_10_query_replies_with_default_fg() {
        // Agent CLIs send OSC 10 ? at startup to discover the foreground
        // color; without a reply they drop into degraded color mode. We
        // claim white (`ffff/ffff/ffff` — xterm's 16-bit-padded form) as
        // a sane default and mirror the BEL terminator the host used.
        let mut pane = Pane::new("shell", 16, 5);
        let mut ingest = TerminalIngest::default();

        let reply = ingest.ingest_bytes(&mut pane, b"\x1b]10;?\x07");

        assert_eq!(reply, b"\x1b]10;rgb:ffff/ffff/ffff\x07");
    }

    #[test]
    fn osc_11_query_with_st_terminator_is_replied_with_st() {
        // Some programs only recognize the ST (`ESC \`) terminator on the
        // reply, even when they sent BEL. zmux mirrors the request's
        // terminator so both modern and legacy clients get a parseable
        // answer.
        let mut pane = Pane::new("shell", 16, 5);
        let mut ingest = TerminalIngest::default();

        let reply = ingest.ingest_bytes(&mut pane, b"\x1b]11;?\x1b\\");

        assert_eq!(reply, b"\x1b]11;rgb:0000/0000/0000\x1b\\");
    }

    #[test]
    fn osc_10_query_with_st_terminator_is_replied_with_st() {
        let mut pane = Pane::new("shell", 16, 5);
        let mut ingest = TerminalIngest::default();

        let reply = ingest.ingest_bytes(&mut pane, b"\x1b]10;?\x1b\\");

        assert_eq!(reply, b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\");
    }

    #[test]
    fn osc_window_title_updates_the_pane_title() {
        // vim emits `ESC ] 2 ; file.rs BEL` on file open; a shell with a
        // configured PS1 emits `ESC ] 0 ; user@host BEL` on every prompt.
        // Both should land on pane.title and never render as visible text.
        let mut pane = Pane::new("shell", 16, 5);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(&mut pane, b"\x1b]2;file.rs\x07");
        assert_eq!(pane.title(), "file.rs");

        ingest.ingest_bytes(&mut pane, b"\x1b]0;user@host: ~/proj\x07");
        assert_eq!(pane.title(), "user@host: ~/proj");

        // OSC 1 is icon-only in xterm, but we treat it the same.
        ingest.ingest_bytes(&mut pane, b"\x1b]1;iconized\x07");
        assert_eq!(pane.title(), "iconized");

        // And the title bytes must not leak into scrollback.
        assert!(pane.visible_text().iter().all(|line| line.is_empty()));
    }

    #[test]
    fn osc_title_terminated_by_string_terminator_also_captures() {
        let mut pane = Pane::new("shell", 16, 5);
        let mut ingest = TerminalIngest::default();

        // ESC \ is the 7-bit string terminator; some programs use it
        // instead of BEL.
        ingest.ingest_bytes(&mut pane, b"\x1b]2;via-st\x1b\\");
        assert_eq!(pane.title(), "via-st");
    }

    #[test]
    fn wide_chars_occupy_two_cells_on_the_alternate_screen() {
        // CJK glyphs and emoji render at double width. If we write one
        // Cell per char, layouts like the cursor column and right-edge
        // wrap desync from what the user sees on screen.
        let mut pane = Pane::new("tui", 16, 4);
        let mut ingest = TerminalIngest::new(PtySize::new(4, 8));

        // `你好` (two wide chars) + `ok` (two narrow chars) = 6 display
        // cells. Write them to column 0 of the alternate screen.
        ingest.ingest_bytes(&mut pane, "\x1b[?1049h\x1b[H你好ok".as_bytes());

        let cells = ingest.render_cells(&pane);
        let row = &cells[0];
        assert_eq!(row[0].ch, '你');
        assert_eq!(
            row[1].ch, '\0',
            "wide char must be followed by a continuation sentinel"
        );
        assert_eq!(row[2].ch, '好');
        assert_eq!(row[3].ch, '\0');
        assert_eq!(row[4].ch, 'o');
        assert_eq!(row[5].ch, 'k');

        // And the plain-text render suppresses sentinels so downstream
        // consumers (copy mode, search) see the actual text.
        assert_eq!(ingest.render_lines(&pane)[0], "你好ok");
    }

    #[test]
    fn wide_chars_wrap_cleanly_at_the_right_edge() {
        // 8-col screen; `abcdefg` fills cols 0..7, then a wide char
        // needs both col 7 and col 8 — but col 8 doesn't exist. We
        // should advance to the next row before writing the glyph so
        // it doesn't straddle the edge.
        let mut pane = Pane::new("tui", 16, 4);
        let mut ingest = TerminalIngest::new(PtySize::new(4, 8));

        ingest.ingest_bytes(&mut pane, "\x1b[?1049h\x1b[Habcdefg你".as_bytes());
        let lines = ingest.render_lines(&pane);
        assert_eq!(lines[0], "abcdefg");
        assert_eq!(lines[1], "你");
    }

    #[test]
    fn osc_8_hyperlinks_attach_to_subsequent_cells_and_clear_on_empty_url() {
        // The standard form is `ESC ] 8 ; <params> ; <URL> ST` to open a
        // link, and `ESC ] 8 ; ; ST` to close. Text between the two should
        // carry the URL on each cell's style.
        let mut pane = Pane::new("shell", 16, 4);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(
            &mut pane,
            b"before\x1b]8;;https://example.com\x1b\\inside\x1b]8;;\x1b\\after",
        );
        ingest.flush_incomplete_line(&mut pane);

        let lines = pane.visible_lines();
        assert_eq!(lines.len(), 1);
        let row = &lines[0];
        // `before` has no link.
        for cell in &row[..6] {
            assert!(
                cell.style.hyperlink.is_none(),
                "pre-link chars leaked a URL"
            );
        }
        // `inside` carries the URL on every cell.
        for cell in &row[6..12] {
            let url = cell.style.hyperlink.as_deref();
            assert_eq!(url, Some("https://example.com"), "missing URL in {cell:?}");
        }
        // `after` has the link cleared.
        for cell in &row[12..] {
            assert!(cell.style.hyperlink.is_none(), "link leaked past close");
        }
    }

    #[test]
    fn osc_palette_query_returns_the_indexed_rgb() {
        // neovim's termguicolors probe sends `\x1b]4;N;?\x07`. Without a
        // reply it disables truecolor autodetection.
        let mut pane = Pane::new("shell", 16, 5);
        let mut ingest = TerminalIngest::default();

        // Index 16 is the first 6x6x6 cube entry → pure black.
        let reply = ingest.ingest_bytes(&mut pane, b"\x1b]4;16;?\x07");
        assert_eq!(reply, b"\x1b]4;16;rgb:0000/0000/0000\x07");

        // Index 231 is the last cube entry → pure white. Xterm pads each
        // 8-bit channel to 16 bits by duplicating: 0xFF → 0xFFFF.
        let reply = ingest.ingest_bytes(&mut pane, b"\x1b]4;231;?\x07");
        assert_eq!(reply, b"\x1b]4;231;rgb:ffff/ffff/ffff\x07");
    }

    #[test]
    fn osc_4_query_with_st_terminator_is_replied_with_st() {
        // Same regression class as the OSC 10/11 ST-mirror fix: when the
        // host sends `OSC 4;N;? ESC \`, the reply must end in ST too.
        // Legacy clients that only accept the form they sent silently
        // drop a BEL-terminated reply to an ST-terminated request.
        let mut pane = Pane::new("shell", 16, 5);
        let mut ingest = TerminalIngest::default();

        let reply = ingest.ingest_bytes(&mut pane, b"\x1b]4;16;?\x1b\\");
        assert_eq!(reply, b"\x1b]4;16;rgb:0000/0000/0000\x1b\\");
    }

    #[test]
    fn synchronized_output_buffers_mutations_until_esu() {
        // Mode 2026 (BSU/ESU) lets the host wrap a screen update so the
        // renderer never sees a partial frame. zmux must hold writes
        // between BSU and ESU and flush them atomically when ESU lands.
        // Tested on the primary screen because that's the simpler render
        // target — same buffering path covers the alt-screen case.
        let mut pane = Pane::new("shell", 16, 4);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(&mut pane, b"\x1b[?2026h");
        ingest.ingest_bytes(&mut pane, b"AAA\nBBB");
        // Mid-region: the writes must not have landed yet.
        let mid = pane.visible_text();
        for line in &mid {
            assert!(
                line.is_empty(),
                "no content should be visible mid-BSU; saw {line:?}"
            );
        }

        ingest.ingest_bytes(&mut pane, b"\x1b[?2026l");
        ingest.flush_incomplete_line(&mut pane);
        let after = pane.visible_text();
        assert_eq!(after[0], "AAA");
        assert_eq!(after[1], "BBB");
    }

    #[test]
    fn synchronized_output_handles_split_feeds() {
        // The BSU sequence itself can be split across feeds (e.g. a TLS
        // chunk boundary lands in the middle of `\x1b[?2026h`). The
        // CSI parser already rejoins partial sequences, so BSU detection
        // works as long as the parser eventually sees the complete CSI.
        let mut pane = Pane::new("shell", 16, 4);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(&mut pane, b"\x1b[?202");
        ingest.ingest_bytes(&mut pane, b"6h");
        ingest.ingest_bytes(&mut pane, b"AAA");
        // ESU split across feeds too — partial ESU stays in the buffer.
        ingest.ingest_bytes(&mut pane, b"\x1b[?202");
        ingest.ingest_bytes(&mut pane, b"6l");
        ingest.flush_incomplete_line(&mut pane);
        assert_eq!(pane.visible_text()[0], "AAA");
    }

    #[test]
    fn synchronized_output_dispatches_trailing_bytes_in_same_feed() {
        // Critical invariant from the plan: a single feed containing
        // BSU + body + ESU + trailing bytes must land the trailing
        // bytes through normal dispatch. Without the post-ESU re-loop
        // they'd be silently dropped.
        let mut pane = Pane::new("shell", 16, 4);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(&mut pane, b"\x1b[?2026hAAA\x1b[?2026lTAIL");
        ingest.flush_incomplete_line(&mut pane);
        assert_eq!(pane.visible_text()[0], "AAATAIL");
    }

    #[test]
    fn synchronized_output_finds_esu_after_many_small_feeds() {
        // The scan cursor only re-scans new tail (plus overlap), so we
        // need the boundary case: many tiny feeds must still detect an
        // ESU that begins on one feed and ends on the next.
        let mut pane = Pane::new("shell", 16, 4);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(&mut pane, b"\x1b[?2026h");
        // Drip the body in one byte at a time.
        for &b in b"hello world" {
            ingest.ingest_bytes(&mut pane, &[b]);
        }
        // Drip ESU itself one byte at a time so it straddles feeds —
        // the overlap window is what makes this work.
        for &b in b"\x1b[?2026l" {
            ingest.ingest_bytes(&mut pane, &[b]);
        }
        ingest.flush_incomplete_line(&mut pane);
        assert_eq!(pane.visible_text()[0], "hello world");
    }

    #[test]
    fn synchronized_output_caps_unbounded_buffer_and_recovers() {
        // Misbehaving host: opens BSU but never sends ESU. zmux caps
        // the buffer, dumps the held bytes (non-atomic flush), and
        // returns to the normal dispatch path on subsequent feeds.
        let mut pane = Pane::new("shell", 16, 4);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(&mut pane, b"\x1b[?2026h");
        // Push past the cap. We use a single big feed so we know it
        // crosses the threshold in one step.
        let huge = vec![b'x'; super::SYNCHRONIZED_BUFFER_MAX + 1];
        ingest.ingest_bytes(&mut pane, &huge);

        // The graceful-degradation flush must have closed the sync
        // region — a follow-up feed should land through normal
        // dispatch instead of being held forever.
        ingest.ingest_bytes(&mut pane, b"\nNORMAL");
        ingest.flush_incomplete_line(&mut pane);

        let lines = pane.visible_text();
        // The last visible line should be NORMAL (the post-cap text).
        // The earlier `xxx…` block landed verbatim during the flush;
        // we don't pin its exact line shape, only that the recovery
        // path delivered the trailing text.
        assert!(
            lines.iter().any(|l| l == "NORMAL"),
            "post-cap text should reach the screen; got {lines:?}",
        );
    }

    #[test]
    fn focus_events_mode_tracks_state() {
        // DECSET/DECRST 1004 toggles whether the host wants `ESC[I` on
        // focus gain and `ESC[O` on focus loss. zmux's terminal layer
        // just stores the toggle; the workspace decides when a focus
        // transition has happened and emits the bytes.
        let mut pane = Pane::new("shell", 16, 4);
        let mut ingest = TerminalIngest::default();

        assert!(!ingest.focus_events_enabled());
        ingest.ingest_bytes(&mut pane, b"\x1b[?1004h");
        assert!(ingest.focus_events_enabled());
        ingest.ingest_bytes(&mut pane, b"\x1b[?1004l");
        assert!(!ingest.focus_events_enabled());
    }

    #[test]
    fn bracketed_paste_mode_tracks_state() {
        // DECSET/DECRST 2004 toggles whether the host wants pasted text
        // bracketed with `ESC[200~ ... ESC[201~`. zmux just stores the
        // toggle here; the workspace layer reads it before writing the
        // paste buffer to the active pane's PTY.
        let mut pane = Pane::new("shell", 16, 4);
        let mut ingest = TerminalIngest::default();

        assert!(!ingest.bracketed_paste_enabled());
        ingest.ingest_bytes(&mut pane, b"\x1b[?2004h");
        assert!(ingest.bracketed_paste_enabled());
        ingest.ingest_bytes(&mut pane, b"\x1b[?2004l");
        assert!(!ingest.bracketed_paste_enabled());
    }

    #[test]
    fn rep_repeats_last_graphic_char() {
        // CSI `b` repeats the last printed graphic char. Box-drawing
        // optimizers in TUI agents lean on this to compress long border
        // runs (`─\x1b[78b` instead of 79 dashes). Test prints one dash
        // then asks for 10 more; the row should hold 11 total.
        let mut pane = Pane::new("shell", 16, 4);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(&mut pane, b"-");
        ingest.ingest_bytes(&mut pane, b"\x1b[10b");
        ingest.flush_incomplete_line(&mut pane);

        assert_eq!(ingest.render_lines(&pane), vec!["-----------"]);
    }

    #[test]
    fn cuf_on_primary_pads_with_blanks_so_words_keep_their_spacing() {
        // Claude Code's splash writes its title bar like:
        //   "Claude\x1b[1CCode\x1b[1Cv2.1.121"
        // i.e. CUF (cursor-forward) instead of literal spaces, so the
        // cells "underneath" the gap retain the prior background.
        // Prior to this fix, CUF was alt-screen-only on primary the
        // sequence vanished, collapsing to "ClaudeCodev2.1.121" which
        // is exactly what the user reported on real claude.
        let mut pane = Pane::new("shell", 80, 4);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(&mut pane, b"Claude\x1b[1CCode\x1b[1Cv2.1.121");
        ingest.flush_incomplete_line(&mut pane);

        assert_eq!(ingest.render_lines(&pane)[0], "Claude Code v2.1.121");
    }

    #[test]
    fn cuf_on_primary_caps_at_viewport_width() {
        // A pathological `\x1b[100000C` must not blow up the grid:
        // the 2D model clamps cursor_col at cols-1 so the runaway
        // can't allocate cells, and the eventual print only pads up
        // to the cursor's column.
        let mut pane = Pane::new("shell", 80, 4);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(&mut pane, b"a\x1b[100000Cb");
        ingest.flush_incomplete_line(&mut pane);

        let line = &ingest.render_lines(&pane)[0];
        assert!(
            line.starts_with('a') && line.ends_with('b'),
            "CUF cap regression: line was {line:?}",
        );
        // With cursor clamped at cols-1=79, the post-cap layout is
        // 'a' at col 0, blanks from cols 1..79, 'b' at col 79. After
        // the print, cursor_col becomes 80 which triggers a linefeed.
        // So the saved line has exactly 80 cells.
        let line_chars = line.chars().count();
        assert_eq!(
            line_chars, 80,
            "CUF cap should land 'b' at the right edge: line had {line_chars} chars",
        );
    }

    #[test]
    fn cha_on_primary_pads_to_target_column() {
        // CHA `\x1b[NG` sets the cursor to column N (1-based). With the
        // 2D grid the cursor moves freely; the row is lazily padded with
        // blanks when the next print extends past the current row length.
        let mut pane = Pane::new("shell", 80, 4);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(&mut pane, b"hi\x1b[10Gthere");
        ingest.flush_incomplete_line(&mut pane);

        // "hi" at cols 0-1, blanks at cols 2-8, "there" starting at col 9.
        assert_eq!(ingest.render_lines(&pane)[0], "hi       there");
    }

    #[test]
    fn rep_with_no_prior_graphic_is_a_noop() {
        // If REP runs before any printable char has landed, last_graphic
        // is None and we should not crash, write garbage, or move the
        // cursor. This protects against pathological agent output that
        // emits REP immediately after a screen clear.
        let mut pane = Pane::new("shell", 16, 4);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(&mut pane, b"\x1b[5b");
        ingest.flush_incomplete_line(&mut pane);

        assert!(pane.visible_text().iter().all(|line| line.is_empty()));
    }

    #[test]
    fn decscusr_sets_cursor_shape() {
        // `CSI Ps SP q` (DECSCUSR) tells the terminal which cursor glyph
        // to draw. Agent CLIs flip between SteadyBlock during prompt
        // entry and BlinkingBar during streaming responses; the value
        // needs to round-trip through our parser even though the on-screen
        // cursor isn't yet rendered with the requested shape.
        let mut pane = Pane::new("shell", 16, 4);
        let mut ingest = TerminalIngest::default();

        assert_eq!(ingest.cursor_shape(), CursorShape::Default);

        ingest.ingest_bytes(&mut pane, b"\x1b[5 q");
        assert_eq!(ingest.cursor_shape(), CursorShape::BlinkingBar);

        ingest.ingest_bytes(&mut pane, b"\x1b[2 q");
        assert_eq!(ingest.cursor_shape(), CursorShape::SteadyBlock);

        // 0 is "reset to terminal default" per DEC's spec.
        ingest.ingest_bytes(&mut pane, b"\x1b[0 q");
        assert_eq!(ingest.cursor_shape(), CursorShape::Default);
    }

    #[test]
    fn cuu_on_primary_revisits_a_prior_row_for_in_place_edits() {
        // Regression: CSI N A (CUU) on primary must move the cursor
        // to a prior row so in-place edits (e.g. an agent CLI
        // redrawing its input box) land on the right cells instead
        // of all piling up at the bottom of the buffer.
        let mut pane = Pane::new("shell", 64, 4);
        let mut ingest = TerminalIngest::default();

        // Seed three rows; cursor parks on a fresh row 3.
        ingest.ingest_bytes(&mut pane, b"row0\nrow1\nrow2\n");
        // Up two — to row 1.
        ingest.ingest_bytes(&mut pane, b"\x1b[2A");
        // Overwrite col 0 of row 1.
        ingest.ingest_bytes(&mut pane, b"X");
        ingest.flush_incomplete_line(&mut pane);

        // The middle row's first char should be the X.
        assert_eq!(
            pane.visible_text(),
            vec!["row0", "Xow1", "row2"],
            "CUU+write must land in row 1, not cascade to a fresh row",
        );
    }

    #[test]
    fn primary_cursor_col_survives_vertical_moves() {
        // Real-terminal invariant: CUU/CUD/VPA preserve cursor_col. claude
        // depends on this for its input-box redraw — it CHA's to a column,
        // CUU's, writes, CUD's; the col after CUD is whatever the print
        // advanced to. Test forces explicit CUU + CHA + write + CUD + CHA
        // + write so each row's edited cell is at a known column.
        let mut pane = Pane::new("shell", 64, 4);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(&mut pane, b"row0\nrow1\n");
        // Up 2 → row 0; CHA col 2 (idx 1); write 'A' at (0, 1).
        ingest.ingest_bytes(&mut pane, b"\x1b[2A\x1b[2GA");
        // Down 1 → row 1; CHA col 3 (idx 2); write 'B' at (1, 2).
        ingest.ingest_bytes(&mut pane, b"\x1b[1B\x1b[3GB");
        ingest.flush_incomplete_line(&mut pane);

        assert_eq!(
            pane.visible_text(),
            vec!["rAw0", "roB1"],
            "CUU/CUD must preserve the col that CHA + the print left behind",
        );
    }

    #[test]
    fn cuu_at_the_top_row_is_a_clamped_no_op() {
        // A runaway `\x1b[100A` from row 0 must clamp the cursor at row 0
        // rather than wrap around or panic. After the clamp the print
        // still lands on row 0 col 0.
        let mut pane = Pane::new("shell", 64, 4);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(&mut pane, b"\x1b[100AX");
        ingest.flush_incomplete_line(&mut pane);

        assert_eq!(pane.visible_text(), vec!["X"]);
    }

    #[test]
    fn dec_save_restore_cursor_round_trips_on_primary() {
        // Regression: ESC 7 / ESC 8 must work on primary too. Agent
        // CLIs save the cursor before drawing a spinner and restore
        // after; without the round-trip the next byte lands wherever
        // the spinner left the cursor.
        let mut pane = Pane::new("shell", 64, 4);
        let mut ingest = TerminalIngest::default();

        // Park cursor at (0, 3) and save it.
        ingest.ingest_bytes(&mut pane, b"abc\x1b7");
        // Stream content elsewhere.
        ingest.ingest_bytes(&mut pane, b"\nmore");
        // Restore: cursor jumps back to (0, 3).
        ingest.ingest_bytes(&mut pane, b"\x1b8");
        // Print at the saved spot.
        ingest.ingest_bytes(&mut pane, b"X");
        ingest.flush_incomplete_line(&mut pane);

        assert_eq!(
            pane.visible_text(),
            vec!["abcX", "more"],
            "DECRC must put 'X' back at the saved position, not at end-of-stream",
        );
    }

    #[test]
    fn newline_past_pty_rows_evicts_oldest_grid_row_to_scrollback() {
        // The grid is bounded by PTY rows (2 here). A third row that
        // arrives via newline must scroll: oldest grid row evicts to
        // scrollback, all rows shift up, cursor stays at the bottom.
        let mut pane = Pane::new("shell", 64, 8);
        // 2 PTY rows so the third newline forces a scroll.
        let mut ingest = TerminalIngest::new(PtySize::new(2, 80));

        ingest.ingest_bytes(&mut pane, b"L1\nL2\nL3");
        // Pre-flush: scrollback should already have the evicted row(s).
        // L1 evicted on the L2->L3 transition (when '\n' after L2 had
        // no room to add a new bottom row).
        let pre_flush = pane.visible_text();
        assert!(
            pre_flush.iter().any(|line| line == "L1"),
            "L1 must evict to scrollback once the grid hits the row cap; got {pre_flush:?}",
        );

        ingest.flush_incomplete_line(&mut pane);
        // Grid was [[L2], [L3]]; flush appends both. Combined with the
        // already-evicted L1, scrollback ends up [L1, L2, L3].
        assert_eq!(pane.visible_text(), vec!["L1", "L2", "L3"]);
    }

    #[test]
    fn primary_screen_grid_supports_wide_chars() {
        // Wide-char invariant: a width-2 glyph occupies two cells, with
        // the second cell holding a `\0` continuation sentinel so layout
        // math doesn't drift. Same convention as the alt screen, applied
        // to the primary grid for parity.
        let mut pane = Pane::new("shell", 64, 4);
        let mut ingest = TerminalIngest::new(PtySize::new(4, 8));

        ingest.ingest_bytes(&mut pane, "你ok".as_bytes());
        ingest.flush_incomplete_line(&mut pane);

        let lines = pane.visible_lines();
        assert_eq!(lines.len(), 1);
        let row = &lines[0];
        assert_eq!(row[0].ch, '你');
        assert_eq!(
            row[1].ch, '\0',
            "wide char must be followed by a continuation sentinel"
        );
        assert_eq!(row[2].ch, 'o');
        assert_eq!(row[3].ch, 'k');
    }

    #[test]
    fn cub_on_primary_moves_cursor_left_without_erasing() {
        // CSI N D (CUB) is "move cursor left by N", not "erase N". The
        // cells underneath stay put; a follow-up print overwrites
        // whatever's at the new cursor position.
        let mut pane = Pane::new("shell", 64, 4);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(&mut pane, b"abcdef");
        // Cursor at col 6; CUB 3 → col 3; write 'Z' → overwrite 'd'.
        ingest.ingest_bytes(&mut pane, b"\x1b[3DZ");
        ingest.flush_incomplete_line(&mut pane);

        assert_eq!(pane.visible_text(), vec!["abcZef"]);
    }

    #[test]
    fn vpa_on_primary_jumps_to_an_existing_row() {
        // VPA `\x1b[Nd` parks cursor_row at row N-1 (1-based input).
        // Bounded by the rows currently in the grid; VPA never grows
        // the grid (only newlines do that).
        let mut pane = Pane::new("shell", 64, 4);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(&mut pane, b"row0\nrow1\nrow2");
        // VPA 1 → row 0; print at col 0.
        ingest.ingest_bytes(&mut pane, b"\x1b[1d\x1b[1GZ");
        ingest.flush_incomplete_line(&mut pane);

        assert_eq!(pane.visible_text(), vec!["Zow0", "row1", "row2"]);
    }

    #[test]
    fn el_clears_residual_chars_so_in_place_redraws_dont_ghost() {
        // The bug pattern Claude Code hit: TUI redraws "frame 2" on
        // top of "frame 1" but the new frame is shorter, so without
        // EL the tail of the old frame stays visible. Reproduces the
        // exact symptom the user reported (e.g. "tiOpuse4.7g" =
        // residual "ti…g" + new "Opus 4.7" mashed together).
        let mut pane = Pane::new("shell", 64, 4);
        let mut ingest = TerminalIngest::default();

        // Frame 1: longer text on row 0.
        ingest.ingest_bytes(&mut pane, b"old longer text here\n");
        // "Redraw" row 0: go back to col 0, EL 0, write shorter text.
        ingest.ingest_bytes(&mut pane, b"\x1b[1A\x1b[1G\x1b[Knew");

        // render_lines trims trailing blanks the way the renderer
        // actually paints to the user's terminal — that's the surface
        // the user sees. pane.visible_text() doesn't trim, which is
        // why the bug looked like "ghosts" (blank cells with the old
        // chars still rendered with their old style).
        let rendered = ingest.render_lines(&pane);
        assert_eq!(
            rendered[0], "new",
            "EL 0 must clear the tail of the old frame; got {rendered:?}",
        );
    }

    #[test]
    fn el_modes_0_1_2_clear_the_correct_segment() {
        // Mode 0 (default) clears cursor → eol, mode 1 clears
        // sol → cursor (inclusive), mode 2 clears the whole line.
        // Three identical setups, three different EL modes — each
        // expects a different surviving substring.
        let cases: &[(&[u8], &str)] = &[
            // Mode 0: cursor at col 5, clear to eol → "01234"
            (b"0123456789\x1b[6G\x1b[0K", "01234"),
            // Mode 1: cursor at col 5, clear sol→cursor (cols 0-5) → "      6789"
            (b"0123456789\x1b[6G\x1b[1K", "      6789"),
            // Mode 2: whole line cleared → ""
            (b"0123456789\x1b[6G\x1b[2K", ""),
        ];
        for (input, expected) in cases {
            let mut pane = Pane::new("shell", 16, 4);
            let mut ingest = TerminalIngest::default();
            ingest.ingest_bytes(&mut pane, input);
            // Use render_lines (the actual rendered surface) so trailing
            // blank cells are trimmed and EL mode 2 (whole-line clear)
            // doesn't get swallowed by flush's empty-row drop.
            let rendered = ingest.render_lines(&pane);
            let line = rendered.first().map(String::as_str).unwrap_or("");
            assert_eq!(
                line.trim_end(),
                expected.trim_end(),
                "EL mode mismatch for input {input:?}: got {rendered:?}",
            );
        }
    }

    #[test]
    fn ed_mode_0_drops_rows_below_cursor() {
        // ED 0 = cursor → end of display. Row at cursor gets
        // cursor→eol cleared; rows below the cursor are dropped.
        let mut pane = Pane::new("shell", 16, 4);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(&mut pane, b"top line\nmid line\nbottom\n");
        // Cursor is now on row 3 col 0. Move up 2 rows (row 1) col 4.
        ingest.ingest_bytes(&mut pane, b"\x1b[2A\x1b[5G\x1b[0J");
        ingest.flush_incomplete_line(&mut pane);

        let lines = pane.visible_text();
        // row 0 untouched, row 1 truncated at col 4 → "mid ", rows 2+ dropped.
        assert_eq!(
            lines[0], "top line",
            "row above cursor must survive ED 0; got {lines:?}",
        );
        assert!(
            lines[1].starts_with("mid"),
            "row at cursor must keep prefix before cursor; got {lines:?}",
        );
        assert!(
            !lines[1].contains("line"),
            "row at cursor must drop tail past cursor; got {lines:?}",
        );
    }

    #[test]
    fn clear_screen_does_not_backfill_scrollback_into_live_view() {
        let mut pane = Pane::new("shell", 16, 4);
        let mut ingest = TerminalIngest::new(PtySize::new(4, 16));

        ingest.ingest_bytes(&mut pane, b"old0\nold1\nold2\nold3\nold4\n");
        assert!(pane.total_lines() > 0, "setup should create scrollback");

        ingest.ingest_bytes(&mut pane, b"\x1b[2Jfresh");

        let rendered = ingest.render_lines(&pane);
        assert_eq!(
            rendered[0], "fresh",
            "fresh screen starts at row 0: {rendered:?}"
        );
        assert!(
            rendered.iter().all(|line| !line.contains("old")),
            "cleared live rows must not resurrect scrollback: {rendered:?}",
        );
    }

    #[test]
    fn cnl_and_cpl_on_primary_move_to_line_start() {
        let mut pane = Pane::new("shell", 32, 4);
        let mut ingest = TerminalIngest::new(PtySize::new(4, 32));

        ingest.ingest_bytes(&mut pane, b"top\nlower");
        // Cursor sits after "lower" on row 1. CPL 1 should move to
        // row 0, column 0 before writing.
        ingest.ingest_bytes(&mut pane, b"\x1b[1FZ");
        let rendered = ingest.render_lines(&pane);
        assert_eq!(rendered[0], "Zop", "CPL should repaint row 0: {rendered:?}");
        assert_eq!(
            rendered[1], "lower",
            "CPL must not keep writing lower: {rendered:?}"
        );

        ingest.ingest_bytes(&mut pane, b"\x1b[1EZ");
        let rendered = ingest.render_lines(&pane);
        assert_eq!(
            rendered[1], "Zower",
            "CNL should move to row 1 col 0: {rendered:?}"
        );
    }

    #[test]
    fn ich_on_primary_inserts_cells_without_losing_tail_content() {
        let mut pane = Pane::new("shell", 32, 4);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(&mut pane, b"abef\x1b[3G\x1b[2@cd");
        ingest.flush_incomplete_line(&mut pane);

        assert_eq!(pane.visible_text()[0], "abcdef");
    }

    #[test]
    fn dch_shifts_cells_left_and_ech_blanks_in_place() {
        // DCH (`\x1b[NP`) deletes N cells at cursor, shifting right.
        // ECH (`\x1b[NX`) replaces N cells with blanks, no shift.
        let mut pane = Pane::new("shell", 32, 4);
        let mut ingest = TerminalIngest::default();
        // Start with "abcdefgh" on row 0, cursor back to col 2 ('c').
        ingest.ingest_bytes(&mut pane, b"abcdefgh\x1b[3G\x1b[2P");
        ingest.flush_incomplete_line(&mut pane);
        // DCH 2 deleted 'c' and 'd'; row is now "abefgh".
        assert_eq!(pane.visible_text()[0], "abefgh");

        // ECH variant: same setup, ECH instead of DCH.
        let mut pane = Pane::new("shell", 32, 4);
        let mut ingest = TerminalIngest::default();
        ingest.ingest_bytes(&mut pane, b"abcdefgh\x1b[3G\x1b[2X");
        ingest.flush_incomplete_line(&mut pane);
        // ECH 2 blanked cols 2-3; row is now "ab  efgh".
        assert_eq!(pane.visible_text()[0], "ab  efgh");
    }

    #[test]
    fn il_inserts_blank_rows_dl_removes_them() {
        // IL inserts blank rows at cursor; DL deletes them. Both are
        // bounded by the viewport and don't bleed into scrollback —
        // they're in-area scroll-region operations.
        let mut pane = Pane::new("shell", 32, 4);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(&mut pane, b"row0\nrow1\nrow2\n");
        // Cursor is at row 3. Move to row 1, IL 1.
        ingest.ingest_bytes(&mut pane, b"\x1b[2;1H\x1b[1L");
        ingest.flush_incomplete_line(&mut pane);

        let after_il = pane.visible_text();
        assert_eq!(after_il[0], "row0", "row above insert untouched");
        assert_eq!(after_il[1], "", "blank row inserted at cursor");
        assert_eq!(after_il[2], "row1", "row1 shifted down");
    }

    #[test]
    fn primary_scroll_region_limits_linefeed_scrolling() {
        let mut pane = Pane::new("shell", 32, 4);
        let mut ingest = TerminalIngest::new(PtySize::new(4, 32));

        ingest.ingest_bytes(&mut pane, b"top\none\ntwo\nbottom");
        // Region rows 2..3 (1-based), then LF at row 3. Only "one/two"
        // should scroll; rows outside the region must stay put.
        ingest.ingest_bytes(&mut pane, b"\x1b[2;3r\x1b[3;1H\n");

        let rendered = ingest.render_lines(&pane);
        assert_eq!(rendered[0], "top", "row above region changed: {rendered:?}");
        assert_eq!(rendered[1], "two", "region did not scroll up: {rendered:?}");
        assert_eq!(
            rendered[2], "",
            "region bottom must be cleared: {rendered:?}"
        );
        assert_eq!(
            rendered[3], "bottom",
            "row below region changed: {rendered:?}"
        );
    }

    #[test]
    fn cup_targets_viewport_rows_not_just_existing_grid_rows() {
        // The duplicated-UI bug: claude positions its UI elements at
        // fixed terminal rows via CUP. When CUP targeted row N but our
        // grid only had K<N rows, the cursor used to clamp to row K-1,
        // landing the new UI on the existing tail and leaving the
        // OLD UI visible above it.
        //
        // With the fix, CUP can target any row in [0, viewport_rows);
        // ensure_primary_cell lazy-grows the grid on the next print.
        let mut pane = Pane::new("shell", 32, 8);
        let mut ingest = TerminalIngest::new(PtySize::new(8, 32));

        // Stream 3 lines so the grid has 3 rows.
        ingest.ingest_bytes(&mut pane, b"row0\nrow1\nrow2");
        // Jump to row 7 col 1 (1-based: \x1b[8;1H = row 7, col 0)
        // and write a marker. Row 7 is the last viewport row; the
        // grid currently has 3 rows so this CUP must extend the
        // grid lazily.
        ingest.ingest_bytes(&mut pane, b"\x1b[8;1HMARKER");
        let rendered = ingest.render_lines(&pane);
        assert_eq!(
            rendered.last().map(String::as_str),
            Some("MARKER"),
            "CUP-then-write should land on viewport row 7; got {rendered:?}",
        );
        // The grid should have 8 rows now (cursor extended to row 7),
        // so the original "row0/row1/row2" content sits at the top
        // and the in-between rows are blank.
        assert_eq!(rendered.len(), 8, "grid should fill viewport: {rendered:?}");
        assert_eq!(rendered[0], "row0");
        assert_eq!(rendered[1], "row1");
        assert_eq!(rendered[2], "row2");
    }

    #[test]
    fn cud_clamps_at_viewport_bottom_not_at_existing_grid() {
        // CUD past the existing grid should clamp to viewport_rows-1
        // so that subsequent EL/print operations target the correct
        // row, not whatever happened to be the last existing row.
        let mut pane = Pane::new("shell", 16, 6);
        let mut ingest = TerminalIngest::new(PtySize::new(6, 16));

        ingest.ingest_bytes(&mut pane, b"a\nb");
        // Grid has 2 rows; cursor at row 1 col 1. CUD 10 → clamp to
        // viewport row 5 (NOT to the existing tail at row 1). CR
        // first so X lands at col 0 of the bottom row.
        ingest.ingest_bytes(&mut pane, b"\x1b[10B\rX");
        let rendered = ingest.render_lines(&pane);
        assert_eq!(
            rendered.last().map(String::as_str),
            Some("X"),
            "CUD 10 should clamp at viewport bottom; got {rendered:?}",
        );
        assert_eq!(rendered[0], "a", "row 0 untouched: {rendered:?}");
        assert_eq!(rendered[1], "b", "row 1 untouched: {rendered:?}");
        assert_eq!(rendered.len(), 6, "grid extends to viewport: {rendered:?}");
    }

    #[test]
    fn dcs_and_charset_sequences_do_not_leak_bytes_into_the_screen() {
        let mut pane = Pane::new("shell", 16, 5);
        let mut ingest = TerminalIngest::new(PtySize::new(5, 12));

        ingest.ingest_bytes(&mut pane, b"\x1b[?1049h\x1b(B\x1bPzz\x1b\\ok");

        assert_eq!(ingest.render_lines(&pane)[0], "ok");
    }

    #[test]
    fn decrc_restores_sgr_pen_on_primary_screen() {
        // DECSC saves the SGR pen along with the cursor; DECRC brings
        // both back. claude's redraw loop leans on this: save, style a
        // spinner/status fragment, restore, keep printing plain text.
        // Restoring only the position leaves the styled pen active and
        // every subsequent character in the pane inherits it.
        let mut pane = Pane::new("shell", 64, 4);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(&mut pane, b"\x1b7\x1b[4mspin\x1b8after");

        let cells = ingest.render_cells(&pane);
        let text: String = cells[0].iter().map(|cell| cell.ch).collect();
        assert_eq!(text, "after");
        for cell in cells[0].iter().take(5) {
            assert!(
                !cell.style.attrs.underline,
                "DECRC must restore the saved pen; {:?} kept underline",
                cell.ch,
            );
        }
    }

    #[test]
    fn decrc_restores_sgr_pen_on_alt_screen() {
        let mut pane = Pane::new("shell", 64, 4);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(&mut pane, b"\x1b[?1049h\x1b7\x1b[4mspin\x1b8after");

        let cells = ingest.render_cells(&pane);
        for cell in cells[0].iter().take(5) {
            assert!(
                !cell.style.attrs.underline,
                "alt-screen DECRC must restore the saved pen; {:?} kept underline",
                cell.ch,
            );
        }
    }

    #[test]
    fn alt_screen_roundtrip_restores_primary_pen() {
        // DECSET/DECRST 1049 does an implicit DECSC on enter and DECRC
        // on exit (xterm semantics), pen included. Without the restore,
        // styles set while an alt-screen view is up (agent TUI pages)
        // leak into everything printed on the primary screen after exit.
        let mut pane = Pane::new("shell", 64, 4);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(
            &mut pane,
            b"\x1b[31mred\x1b[?1049h\x1b[4;32munderlined\x1b[?1049lback",
        );

        let cells = ingest.render_cells(&pane);
        let text: String = cells[0].iter().map(|cell| cell.ch).collect();
        assert_eq!(text, "redback");
        for cell in cells[0].iter().skip(3).take(4) {
            assert!(
                !cell.style.attrs.underline,
                "1049l must restore the pre-alt pen; {:?} kept underline",
                cell.ch,
            );
            assert_eq!(
                cell.style.fg,
                Color::Indexed(1),
                "1049l must restore the pre-alt foreground on {:?}",
                cell.ch,
            );
        }
    }

    #[test]
    fn decrc_survives_ed2_restoring_position_and_pen_on_primary() {
        // xterm does NOT invalidate the DECSC-saved cursor when the
        // screen is erased: a program that does DECSC, CSI 2J, DECRC
        // gets its cursor position AND SGR pen back. Full-screen
        // redraws that bracket a clear with save/restore (park cursor,
        // wipe screen, jump back, resume styled output) rely on this
        // round-trip landing where they left off instead of wherever
        // the clear left the cursor with whatever pen was active then.
        let mut pane = Pane::new("shell", 64, 4);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(
            &mut pane,
            // Park the cursor at (row 1, col 5) with a red pen, DECSC.
            // Switch to an underlined green pen and move to the top
            // left, then erase the whole display, then DECRC, then
            // print — the 'X' must land back at (1, 5) in red, not at
            // (0, 0) in underlined green.
            b"line1\nline2\x1b[31m\x1b7\x1b[4;32m\x1b[1;1H\x1b[2J\x1b8X",
        );

        let cells = ingest.render_cells(&pane);
        let cell = &cells[1][5];
        assert_eq!(
            cell.ch, 'X',
            "DECRC must restore the saved position across CSI 2J, not leave the cursor at (0, 0)",
        );
        assert_eq!(
            cell.style.fg,
            Color::Indexed(1),
            "DECRC must restore the saved (red) pen across CSI 2J, not the post-erase pen",
        );
        assert!(
            !cell.style.attrs.underline,
            "DECRC must restore the saved pen across CSI 2J; CSI 2J must not drop the saved cursor/pen",
        );
    }

    #[test]
    fn colon_underline_subparams_do_not_reset_style() {
        // kitty-style underline (ISO 8613-6 colon subparameters):
        // `CSI 4:3m` = curly underline on, `CSI 4:0m` = underline off.
        // A parser that can't read colon parts must drop them as a
        // unit — flattening them to SGR 0 nukes the whole style.
        let mut pane = Pane::new("shell", 64, 4);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(&mut pane, b"\x1b[31mred \x1b[4:3mcurly\x1b[4:0moff");

        let cells = ingest.render_cells(&pane);
        let text: String = cells[0].iter().map(|cell| cell.ch).collect();
        assert_eq!(text, "red curlyoff");
        for cell in cells[0].iter().skip(4).take(5) {
            assert_eq!(
                cell.style.fg,
                Color::Indexed(1),
                "4:3 must not reset the foreground on {:?}",
                cell.ch,
            );
            assert!(
                cell.style.attrs.underline,
                "4:3 is an underline style; {:?} should be underlined",
                cell.ch,
            );
        }
        for cell in cells[0].iter().skip(9).take(3) {
            assert!(
                !cell.style.attrs.underline,
                "4:0 clears the underline; {:?} kept it",
                cell.ch,
            );
            assert_eq!(cell.style.fg, Color::Indexed(1));
        }
    }

    #[test]
    fn underline_color_sgr_is_consumed_without_side_effects() {
        // SGR 58 (underline color) isn't tracked, but its payload must
        // be consumed: `58;2;10;20;30` misread param-by-param turns 2
        // into "dim" and 30 into "black foreground".
        let mut pane = Pane::new("shell", 64, 4);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(&mut pane, b"\x1b[31m\x1b[58;2;10;20;30mx\x1b[58:5:7my");

        let cells = ingest.render_cells(&pane);
        let x = &cells[0][0];
        assert_eq!(x.ch, 'x');
        assert_eq!(x.style.fg, Color::Indexed(1), "58;2;R;G;B payload leaked");
        assert!(!x.style.attrs.dim, "58's `2` payload was misread as dim");
        let y = &cells[0][1];
        assert_eq!(y.ch, 'y');
        assert_eq!(y.style.fg, Color::Indexed(1), "58:5:N reset the style");
    }

    #[test]
    fn colon_form_extended_colors_match_semicolon_forms() {
        let mut semi_pane = Pane::new("shell", 64, 4);
        let mut semi = TerminalIngest::default();
        semi.ingest_bytes(
            &mut semi_pane,
            b"\x1b[38;5;196ma\x1b[0m\x1b[38;2;10;20;30mb",
        );

        let mut colon_pane = Pane::new("shell", 64, 4);
        let mut colon = TerminalIngest::default();
        colon.ingest_bytes(
            &mut colon_pane,
            b"\x1b[38:5:196ma\x1b[0m\x1b[38:2:10:20:30mb",
        );

        let semi_cells = semi.render_cells(&semi_pane);
        let colon_cells = colon.render_cells(&colon_pane);
        assert_eq!(
            semi_cells[0][0].style, colon_cells[0][0].style,
            "38:5:N should match 38;5;N",
        );
        assert_eq!(
            semi_cells[0][1].style, colon_cells[0][1].style,
            "38:2:R:G:B should match 38;2;R;G;B",
        );
    }
}
