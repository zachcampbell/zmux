// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashSet;
use std::mem;

use crate::mouse::{MouseTrackingMode, ScreenMode};
use crate::pane::Pane;
use crate::pty::PtySize;
use crate::scrollback::{ScrollbackLine, search_line_indices, trimmed_line_text};
use crate::style::{Cell, Color, Style};

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

#[derive(Debug, Default)]
pub struct ResizeOutcome {
    pub(crate) evicted_primary_rows: Vec<ScrollbackLine>,
    pub(crate) live_tail: usize,
}

// Cursor state captured by DECSC (ESC 7) / CSI s and restored by DECRC
// (ESC 8) / CSI u. Per xterm, the save covers the SGR pen as well as
// the position — TUIs bracket styled fragments with save/restore and
// rely on the pen coming back, so restoring position alone leaks the
// styled pen into everything printed afterwards. Real DECSC saves the
// character-set state too (which glyph table G0/G1 point at, and which
// of them GL is currently reading through) — without that, a save/
// restore bracketing a `\x1b(0 ... \x1b(B` charset dance can strand the
// terminal in graphics mode, so `charset` rides along with `pen`.
#[derive(Debug, Clone)]
struct SavedCursor {
    row: usize,
    col: usize,
    pen: Style,
    charset: CharsetState,
    // DECOM at save time — DEC STD 070 lists origin mode in the DECSC
    // state, so a DECRC after the app toggled it puts the mode back.
    origin: bool,
}

// A G0/G1 designator's target character set. `ESC ( 0` / `ESC ) 0`
// select DecSpecialGraphics (the vt100 "line drawing" set); every other
// final byte we recognize as a valid designator (`B` = US-ASCII, `A` =
// UK, and anything else we don't special-case) maps back to plain
// Ascii passthrough — those variants only ever differ from ASCII in a
// couple of currency/punctuation glyphs that don't matter to a
// headless multiplexer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Charset {
    #[default]
    Ascii,
    DecSpecialGraphics,
}

// Which designated slot (G0 or G1) GL currently reads through. Selected
// by SI (0x0F, -> G0) / SO (0x0E, -> G1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum CharsetSlot {
    #[default]
    G0,
    G1,
}

// Bundles the two designated slots plus which one is currently active,
// so the live state (`TerminalIngest::charset`), the alt-screen's own
// DECSC save slot (`alt_saved_charset`), and the primary DECSC save
// slot (`SavedCursor::charset`) can all share one type instead of three
// parallel fields apiece.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct CharsetState {
    g0: Charset,
    g1: Charset,
    active: CharsetSlot,
}

impl CharsetState {
    fn active_charset(&self) -> Charset {
        match self.active {
            CharsetSlot::G0 => self.g0,
            CharsetSlot::G1 => self.g1,
        }
    }
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
    // DECOM (`CSI ?6 h/l`). While set, CUP/HVP/VPA address rows
    // relative to the active scroll region's top margin and clamp
    // inside the margins, and CPR reports margin-relative coordinates.
    // One flag for both screens (xterm keeps it in the shared mode
    // flags; each screen still applies it against its own margins).
    origin_mode: bool,
    alternate: AlternateScreen,
    // Tab-stop table for HT (0x09), HTS (`ESC H`), TBC (`CSI g`), CHT
    // (`CSI I`) and CBT (`CSI Z`). One table, not one per screen: tab
    // stops are a property of the terminal's column geometry, which the
    // primary and alternate screens share (`alternate.cols`) — xterm
    // does not give the alt screen its own stops either.
    //
    // `None` means "no HTS/TBC has ever run" and stops follow the plain
    // default rule (every 8th column). This is the common case and
    // costs nothing — no allocation, no per-resize bookkeeping, since
    // `is_tab_stop` just computes `col % 8 == 0` against whatever width
    // is current.
    //
    // `Some(set)` means the table has been customized and IS the whole
    // truth from then on, not "defaults plus these extras": HTS
    // materializes it by seeding the default stops for the current
    // width and adding the new one, TBC-at-cursor removes a single
    // entry, and `CSI 3 g` empties the set outright — leaving genuinely
    // *no* stops, matching xterm rather than reverting to the default
    // rule. See `is_tab_stop` / `set_tab_stop` / `clear_all_tab_stops`.
    //
    // On resize, entries that fall outside the new width are dropped
    // (a stop past the right edge is meaningless) but everything else —
    // including the emptied-by-`3g` case — survives, matching "xterm
    // keeps custom stops" rather than recomputing from scratch.
    tab_stops: Option<HashSet<usize>>,
    // DECTCEM (`CSI ?25 h/l`): whether the application wants its text
    // cursor shown. One global flag, not per-screen — xterm treats it
    // as a mode, and DECSC/1049 do not save or restore it. The attached
    // client uses this (via `screen_cursor`) to decide whether to paint
    // a host cursor at the pane's cursor cell.
    cursor_visible: bool,
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
    // Live G0/G1 designations and which of them GL currently reads
    // through. Global like `current_style` (not per-screen): switching
    // to the alternate screen carries whatever charset was active on
    // the way in, exactly like the pen does. Set by `ESC ( <byte>` /
    // `ESC ) <byte>` (designation) and SI/SO (0x0F/0x0E, slot select).
    charset: CharsetState,
    // Charset half of `alt_saved_pen`: the alternate screen's own DECSC
    // save slot, separate from `primary_saved_cursor` so a DECSC issued
    // while the alt screen is up doesn't clobber the pre-1049 primary
    // state. Reset to CharsetState::default() on alt-screen entry, same
    // as `alt_saved_pen`.
    alt_saved_charset: CharsetState,
    // Origin-mode half of the alt screen's DECSC slot, mirroring
    // `SavedCursor::origin` on the primary side.
    alt_saved_origin: bool,
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
// hit, `scan_from` advances to `bytes.len() - (MAX_ESU_LEN - 1)`,
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
    // `ESC #` (DECDHL / DECSWL / DECDWL / DECALN): a single private
    // byte follows and zmux doesn't model any of the five variants,
    // so this state exists purely to eat that one byte instead of
    // letting it fall through to Ground and print literally.
    EscapeHash,
    // Mid `ESC ( <byte>` / `ESC ) <byte>` (Some(slot): which of G0/G1 the
    // pending final byte designates) or `ESC * / + / - / . / <byte>`
    // (None: a G2/G3 designator we don't track state for — the next byte
    // is swallowed and discarded either way).
    Charset(Option<CharsetSlot>),
    Csi,
    Osc,
    OscEscape,
    // Generic "swallow a string up to the String Terminator" state
    // shared by DCS (`ESC P`), APC (`ESC _`), PM (`ESC ^`), and SOS
    // (`ESC X`). zmux doesn't interpret any of their payloads (device
    // control data, kitty graphics protocol blobs, application/
    // privacy-message strings), so all four get the same treatment:
    // consume bytes without storing or printing them until ST
    // (`ESC \`) closes the string.
    StringSwallow,
    StringSwallowEscape,
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
            origin_mode: false,
            alternate: AlternateScreen::new(size.rows as usize, size.cols as usize),
            tab_stops: None,
            cursor_visible: true,
            alt_saved_pen: Style::DEFAULT,
            current_style: Style::DEFAULT,
            charset: CharsetState::default(),
            alt_saved_charset: CharsetState::default(),
            alt_saved_origin: false,
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

    pub fn resize(&mut self, size: PtySize) -> ResizeOutcome {
        let old_rows = self.alternate.rows.max(1);
        let old_region_was_full = self.primary_scroll_top == 0
            && self.primary_scroll_bottom >= old_rows.saturating_sub(1);
        let new_rows = size.rows as usize;
        let new_cols = size.cols as usize;
        self.alternate.resize(new_rows, new_cols);

        // Rows displaced by a height shrink are real terminal history,
        // not disposable pixels. Return them to Session so it can append
        // them to the pane's bounded scrollback. Columns are deliberately
        // retained off-screen: a later widen can reveal them again, while
        // rendering clips to the current geometry.
        let row_cap = new_rows.max(1);
        let drop = self.primary_grid.len().saturating_sub(row_cap);
        let evicted_primary_rows = self.primary_grid.drain(0..drop).collect();
        let col_cap = new_cols.max(1);
        // Drop any customized stop that no longer fits — a column past
        // the new right edge can't be tabbed to. If the table was never
        // customized (`None`, still on the default-every-8 rule) there's
        // nothing to do: that rule reads the live width on every lookup,
        // so it adapts to the resize for free.
        if let Some(stops) = self.tab_stops.as_mut() {
            let last_col = col_cap.saturating_sub(1);
            stops.retain(|&col| col <= last_col);
        }
        let max_row = self.primary_grid.len().saturating_sub(1);
        self.primary_cursor_row = self.primary_cursor_row.saturating_sub(drop).min(max_row);
        self.primary_cursor_col = self.primary_cursor_col.min(col_cap.saturating_sub(1));
        self.primary_wrap_pending = false;
        if let Some(saved) = self.primary_saved_cursor.as_mut() {
            saved.row = saved.row.saturating_sub(drop).min(max_row);
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

        ResizeOutcome {
            evicted_primary_rows,
            live_tail: self.primary_grid.len(),
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
                if let Some((rel_start, rel_end)) = find_esu(&sync.bytes[scan_window..]) {
                    let esu_at = scan_window + rel_start;
                    let collected = std::mem::take(&mut sync.bytes);
                    self.synchronized_buffer = None;
                    let body = &collected[..esu_at];
                    let after_esu_start = scan_window + rel_end;
                    self.dispatch_bytes(pane, body, &mut replies);
                    pending = collected[after_esu_start..].to_vec();
                    continue;
                }

                // No ESU yet. Advance the cursor past everything we've
                // scanned, keeping a `MAX_ESU_LEN - 1` overlap so an ESU
                // split across feeds — including a combined-parameter one
                // longer than the bare form — is still caught.
                let overlap = MAX_ESU_LEN.saturating_sub(1);
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

        // Keep the scrollback's combined-timeline math (see
        // `ScrollbackBuffer::set_live_tail`) up to date with however
        // large the live grid ended up after this feed. This is the
        // primary sync point — it covers every code path above,
        // including bytes that never touched `primary_index` (e.g. a
        // feed that only moved the cursor).
        self.sync_live_tail(pane);

        replies
    }

    fn dispatch_bytes(&mut self, pane: &mut Pane, bytes: &[u8], replies: &mut Vec<u8>) {
        for (index, &byte) in bytes.iter().enumerate() {
            match self.state {
                ParseState::Ground => self.handle_ground(pane, byte),
                ParseState::Escape => self.handle_escape(pane, byte),
                ParseState::EscapeHash => self.handle_escape_hash(pane, byte),
                ParseState::Charset(slot) => {
                    self.state = ParseState::Ground;
                    // G2/G3 (`slot == None`) designators are swallowed
                    // without being tracked — see the ParseState::Charset
                    // doc comment. Only a real G0/G1 designator updates
                    // charset state.
                    if let Some(slot) = slot {
                        self.designate_charset(slot, byte);
                    }
                }
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
                ParseState::StringSwallow => self.handle_string_swallow(byte),
                ParseState::StringSwallowEscape => self.handle_string_swallow_escape(byte),
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
        // The grid was just drained to nothing (see the `mem::take`
        // above) — the live tail is now 0, and every row that used to
        // count toward it is now a committed scrollback line instead.
        self.sync_live_tail(pane);
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
            .map(|row| {
                let mut text = String::new();
                for cell in clip_row_to_width(row, self.alternate.cols) {
                    cell.push_text(&mut text);
                }
                text
            })
            .collect()
    }

    // Cell-level counterpart to `primary_grid_text`: same rows, styled.
    // Used by `Session::snapshot_scrollback_cells` so MCP `read_pane`'s
    // `strip_ansi=false` scrollback mode can re-serialize real SGR
    // instead of the plain-char passthrough. Wide-char continuation
    // sentinels are left in place — `style::serialize_row` already
    // skips them, matching `primary_grid_text`'s filter.
    pub fn primary_grid_cells(&self) -> Vec<Vec<Cell>> {
        self.primary_grid
            .iter()
            .map(|row| clip_row_to_width(row, self.alternate.cols))
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

    // Cell-level counterpart to `render_lines`: same viewport rows and
    // trimming rules, but the cells keep their SGR style instead of
    // being collapsed to chars. Shared by `render_lines` (which
    // collapses the result to plain text) and MCP `read_pane`'s
    // `strip_ansi=false` path (which re-serializes it through
    // `style::serialize_row`) so both representations describe
    // exactly the same visible region.
    //
    // Trims trailing blank cells per row (a cell equal to `Cell::BLANK`
    // — a plain space with default style; a styled blank, e.g. a
    // painted background, is kept) and drops trailing rows that carry
    // no visible character, mirroring the pre-cell text behavior.
    pub fn render_visible_cells(&self, pane: &Pane) -> Vec<Vec<Cell>> {
        let mut rows = self.render_cells(pane);
        for row in &mut rows {
            let trimmed_end = row
                .iter()
                .rposition(|cell| *cell != Cell::BLANK)
                .map(|i| i + 1)
                .unwrap_or(0);
            row.truncate(trimmed_end);
        }
        while rows
            .last()
            .is_some_and(|row| row.iter().all(|cell| cell.ch == '\0'))
        {
            rows.pop();
        }
        rows
    }

    pub fn render_lines(&self, pane: &Pane) -> Vec<String> {
        // Produce a plain-text rendering (no ANSI escapes) — this is
        // what older tests inspect. Skip the '\0' continuation
        // sentinel that follows a wide char so the text output matches
        // what a human would see.
        self.render_visible_cells(pane)
            .iter()
            .map(|row| {
                let mut text = String::new();
                for cell in row {
                    cell.push_text(&mut text);
                }
                text
            })
            .collect()
    }

    // Cell-level rendering used by the workspace compositor so it can
    // overlay separators, pane headers, and the big-digit overlay on top
    // of pane content without destroying ANSI transitions. Both
    // primary- and alt-screen rows carry the SGR state that was active
    // when each cell was written.
    pub fn render_cells(&self, pane: &Pane) -> Vec<Vec<Cell>> {
        let rows = match pane.screen_mode() {
            ScreenMode::Primary => self.render_primary_cells(pane),
            // The alternate buffer itself has no history. When the pane's
            // viewport is detached, show the retained primary timeline;
            // returning to the bottom restores the live alternate screen.
            // This is the same model tmux users expect when scrolling a
            // full-screen program whose own mouse tracking is disabled.
            ScreenMode::Alternate if !pane.follow_output() => {
                self.render_scrolled_primary_view(pane)
            }
            ScreenMode::Alternate => self.alternate.cells().to_vec(),
        };
        rows.iter()
            .map(|row| clip_row_to_width(row, self.alternate.cols))
            .collect()
    }

    fn render_primary_cells(&self, pane: &Pane) -> Vec<Vec<Cell>> {
        if !pane.follow_output() {
            // Scrolled back: the addressable timeline is scrollback
            // plus whatever's still live in the grid (they're never
            // spliced together anywhere else — this is the one place
            // that needs the union). See `render_scrolled_primary_view`.
            return self.render_scrolled_primary_view(pane);
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

    // Non-destructive combined-timeline render for the scrolled-back
    // (non-follow) case. The live grid is never flushed to produce this
    // — flushing resets the cursor and, worse, makes the *next* bit of
    // follow-mode output render top-aligned with blank rows below it
    // instead of growing naturally from the bottom (that's why this
    // fix doesn't use `flush_incomplete_line` to solve the wheel-jump
    // bug). Instead we address into "scrollback lines, then live grid
    // rows" as one continuous space: `pane.viewport_top()` /
    // `pane.total_lines()` are already combined-timeline-aware via
    // `ScrollbackBuffer`'s `live_tail` (see `sync_live_tail`), so this
    // just has to pick the right side of the split for each row.
    fn render_scrolled_primary_view(&self, pane: &Pane) -> Vec<Vec<Cell>> {
        let top = pane.viewport_top();
        let height = pane.viewport_height();
        (top..top + height)
            .map(|combined_index| self.combined_line_cells(pane, combined_index))
            .collect()
    }

    // Non-destructive combined-timeline read of a single line's raw
    // cells: committed scrollback first, then whatever's still live in
    // the grid, addressed as one continuous index space. Shared by
    // `render_scrolled_primary_view` and `Session::combined_line_cells`
    // (and everything built on it below) so any caller can address an
    // arbitrary combined index without flushing the grid. See
    // `render_scrolled_primary_view`'s comment for why flushing is off
    // the table here.
    //
    // Wide-char continuation sentinels (`\0`) are kept, matching
    // `ScrollbackBuffer::line_cells` — callers that need visual-column
    // indexing (mouse selections) want the raw cells; callers that
    // want plain text should go through `combined_extract_lines`.
    pub fn combined_line_cells(&self, pane: &Pane, index: usize) -> ScrollbackLine {
        let scrollback_len = pane.total_lines();
        if index < scrollback_len {
            pane.scrollback_line_cells(index)
        } else {
            // Past the end of committed scrollback: read straight from
            // the live grid. `.get(...)` returns `None` (empty vec)
            // once `index` runs past the live grid's own length too —
            // e.g. a caller probing past the end of the timeline.
            self.primary_grid
                .get(index - scrollback_len)
                .cloned()
                .unwrap_or_default()
        }
    }

    // Combined-timeline index of the first row `render_cells` shows for
    // this pane — the number selection code must add to a screen row
    // offset to get a combined line index. This is NOT always
    // `pane.viewport_top()`: follow mode with a grid shorter than the
    // viewport (e.g. right after an ED 2 that kept scrollback) renders
    // the grid TOP-aligned, while the scrollback viewport math is
    // bottom-anchored — mapping through viewport_top there points
    // selections at scrollback lines that aren't on screen at all.
    // Every arm below mirrors the corresponding branch of
    // `render_primary_cells` exactly; keep them in sync.
    pub fn rendered_viewport_origin(&self, pane: &Pane) -> usize {
        if pane.screen_mode() == ScreenMode::Primary
            && pane.follow_output()
            && !(self.primary_grid.is_empty() && self.primary_flushed_to_scrollback)
        {
            let scrollback_len = pane.total_lines();
            if self.primary_grid.len() >= pane.viewport_height() {
                // Bottom window of the grid: same as viewport_top, but
                // computed from the grid so the two can't drift.
                scrollback_len + self.primary_grid.len() - pane.viewport_height()
            } else {
                // Top-aligned sparse grid: row 0 is the grid's first
                // row, i.e. the first combined index past scrollback.
                scrollback_len
            }
        } else {
            pane.viewport_top()
        }
    }

    // Total addressable lines across scrollback plus the live primary
    // grid — i.e. what `pane.total_lines()` would read immediately
    // after a `flush_incomplete_line`, without paying for the flush's
    // side effects (draining the grid, resetting the cursor).
    pub fn combined_total_lines(&self, pane: &Pane) -> usize {
        pane.total_lines() + self.primary_grid.len()
    }

    // Combined-timeline counterpart to `ScrollbackBuffer::extract_lines`:
    // same inclusive-range / order-normalized / trailing-blank-trimmed
    // text semantics (via the shared `trimmed_line_text` helper), but
    // addressed through `combined_line_cells` so a selection that spans
    // the scrollback/live-grid boundary — or that was made entirely
    // against rows still sitting in the live grid — reads correctly
    // without flushing. Out-of-range indices are silently skipped, same
    // as the scrollback-only version.
    pub fn combined_extract_lines(&self, pane: &Pane, start: usize, end: usize) -> String {
        let (low, high) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        let total = self.combined_total_lines(pane);
        let mut out = String::new();
        for index in low..=high {
            if index >= total {
                continue;
            }
            let cells = self.combined_line_cells(pane, index);
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&trimmed_line_text(&cells));
        }
        out
    }

    // Combined-timeline counterpart to `ScrollbackBuffer::search`.
    // Scans committed scrollback first, then the live grid, and
    // returns indices in the same combined address space
    // `combined_line_cells` reads — so a match found in an
    // as-yet-unflushed grid row still resolves correctly through
    // `Session::combined_line_cells`/`combined_extract_lines` when the
    // caller jumps to it, even after that row later evicts into
    // scrollback (combined indices are stable across eviction: a row's
    // index doesn't change when it moves from grid to scrollback,
    // because scrollback grows by exactly the amount the grid's
    // implicit offset shrinks).
    pub fn combined_search(&self, pane: &Pane, needle: &str) -> Vec<usize> {
        let mut matches = pane.search_scrollback(needle);
        let scrollback_len = pane.total_lines();
        let grid_matches = search_line_indices(self.primary_grid.iter(), needle);
        matches.extend(grid_matches.into_iter().map(|index| index + scrollback_len));
        matches
    }

    // Tell the pane's scrollback buffer how many rows the live primary
    // grid currently holds, so its viewport math can treat "scrollback
    // ++ live grid" as one continuous timeline (see
    // `ScrollbackBuffer::set_live_tail`). Cheap — `Vec::len()` — so it's
    // called liberally rather than only where it's strictly load-bearing.
    fn sync_live_tail(&self, pane: &mut Pane) {
        pane.set_scrollback_live_tail(self.primary_grid.len());
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
            // VT (0x0B) and FF (0x0C): real terminals treat both as a
            // linefeed rather than dropping them (there's no glyph for
            // either). Routing them into the exact same handler as `\n`
            // means they inherit whatever this parser's `\n` already
            // does for the active screen — CR+IND on primary,
            // column-preserving IND on the alt screen (see
            // `primary_linefeed` / `AlternateScreen::linefeed`) —
            // instead of needing their own copy of that logic.
            // `printf`'s `\v`/`\f` and stray form-feeds from piped
            // output hit this.
            b'\n' | b'\x0b' | b'\x0c' => self.handle_newline(pane),
            b'\r' => self.handle_carriage_return(pane),
            b'\x08' => self.handle_backspace(pane),
            b'\t' => self.handle_tab(pane),
            // SI/SO: point GL at G0/G1. ncurses toggles between an ACS
            // border drawn via SO and normal text via SI on terminals
            // that designate the line-drawing set into G1 instead of
            // (or in addition to) G0.
            0x0f => self.charset.active = CharsetSlot::G0, // SI
            0x0e => self.charset.active = CharsetSlot::G1, // SO
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
            // DCS (`ESC P`), APC (`ESC _`), PM (`ESC ^`), SOS (`ESC X`):
            // all four introduce a string that runs to the String
            // Terminator (`ESC \`). zmux doesn't interpret any of their
            // payloads, so all four route into the same swallow state.
            // Swallowing matters most for APC: kitty's graphics protocol
            // (and the notcurses/chafa probes for it) opens one with a
            // base64 image payload that can run tens of KB — none of
            // which may reach the screen as printable text.
            b'P' | b'_' | b'^' | b'X' => ParseState::StringSwallow,
            // `ESC #`: DECDHL (`3`/`4`), DECSWL (`5`), DECDWL (`6`), and
            // DECALN (`8`, screen-alignment test) all follow with a
            // single byte. zmux models none of them; EscapeHash just
            // eats that one byte so it doesn't fall through to Ground
            // and print literally (e.g. `ESC # 8` printing a stray "8").
            b'#' => ParseState::EscapeHash,
            // `ESC ( <byte>` / `ESC ) <byte>` designate G0 / G1. The
            // other four intermediates (`* + - .` for G2/G3, and `/`
            // for a 96-charset G3 variant) select slots zmux never
            // reads from, so their final byte is swallowed with no
            // state recorded.
            b'(' => ParseState::Charset(Some(CharsetSlot::G0)),
            b')' => ParseState::Charset(Some(CharsetSlot::G1)),
            b'*' | b'+' | b'-' | b'.' | b'/' => ParseState::Charset(None),
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
                match pane.screen_mode() {
                    ScreenMode::Primary => self.primary_index(pane),
                    ScreenMode::Alternate => self.alternate.index(),
                }
                ParseState::Ground
            }
            b'M' => {
                match pane.screen_mode() {
                    ScreenMode::Primary => self.primary_reverse_index(),
                    ScreenMode::Alternate => self.alternate.reverse_index(),
                }
                ParseState::Ground
            }
            b'E' => {
                // NEL: carriage return + IND. `primary_linefeed` already
                // *is* CR-plus-index (see its comment), so this is a
                // direct reuse; the alt screen keeps the two split apart
                // like a real terminal, so it calls both explicitly.
                match pane.screen_mode() {
                    ScreenMode::Primary => self.primary_linefeed(pane),
                    ScreenMode::Alternate => {
                        self.alternate.carriage_return();
                        self.alternate.index();
                    }
                }
                ParseState::Ground
            }
            b'H' => {
                // HTS: plant a tab stop at the cursor's current column
                // on whichever screen is active — see `set_tab_stop`.
                let col = match pane.screen_mode() {
                    ScreenMode::Primary => self.primary_cursor_col,
                    ScreenMode::Alternate => self.alternate.cursor_col,
                };
                self.set_tab_stop(col);
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

    // `ESC ( <designator>` / `ESC ) <designator>`: point G0 or G1 at a
    // character set. `0` is DEC Special Graphics (line drawing); every
    // other designator we might see (`B` US-ASCII, `A` UK, `1`/`2` the
    // alternate-ROM variants some old apps probe for) is treated as
    // plain ASCII since none of them remap a glyph zmux's headless
    // grid needs to care about.
    fn designate_charset(&mut self, slot: CharsetSlot, designator: u8) {
        let charset = match designator {
            b'0' => Charset::DecSpecialGraphics,
            _ => Charset::Ascii,
        };
        match slot {
            CharsetSlot::G0 => self.charset.g0 = charset,
            CharsetSlot::G1 => self.charset.g1 = charset,
        }
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
        // `s` / `u` / `m` below are the unmarked-final-byte sequences
        // SCOSC / DECRC / SGR — but those same final bytes also close
        // marked sequences this parser doesn't implement: the kitty
        // keyboard protocol's push/pop/query (`CSI > 1 u`, `CSI < u`,
        // `CSI ? u`) and xterm's modifyOtherKeys query/set (`CSI > 4 m`,
        // `CSI > 4 ; 2 m`). Every one of those leads with a private
        // marker byte (`<`, `=`, `>`, `?`) that plain SCOSC/DECRC/SGR
        // never carry. `prefix` above only tracks the three markers
        // this parser gives other meaning to elsewhere (`?`, `>`, `!`),
        // so check the raw lead byte here instead — that also catches
        // `<` and `=`, which `prefix` doesn't. Without this guard,
        // kitty's probes fall through to DECRC and teleport the cursor
        // mid-redraw (fatal on the alt screen, since Claude Code sends
        // these at startup), and modifyOtherKeys falls through to SGR
        // and turns on underline for everything printed afterwards.
        // zmux implements none of these marked protocols, so the
        // correct behavior for the marked forms is exactly what falls
        // out of leaving them to the catch-all `_ => {}` below: no
        // state change, no reply — apps fall back correctly when an
        // unsupported probe goes unanswered.
        let has_private_marker = matches!(buffer.first(), Some(b'<' | b'=' | b'>' | b'?'));
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
            b'm' if !has_private_marker => {
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
            b's' if !has_private_marker => self.save_cursor_state(pane),
            b'u' if !has_private_marker => self.restore_cursor_state(pane),
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
                        // print via ensure_primary_cell. Under DECOM the
                        // row is relative to the top margin and confined
                        // to the region (see `primary_absolute_row`).
                        self.primary_wrap_pending = false;
                        self.primary_cursor_row = self.primary_absolute_row(row);
                        self.primary_cursor_col = col
                            .saturating_sub(1)
                            .min(self.alternate.cols.saturating_sub(1));
                    }
                    ScreenMode::Alternate => {
                        let target = self.alternate.absolute_row(row, self.origin_mode);
                        self.alternate.set_cursor(target, col.saturating_sub(1));
                    }
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
                    // CUP above. The grid lazy-grows on print. Row
                    // addressing is DECOM-relative, same as CUP.
                    let target_row = params.first().and_then(|value| *value).unwrap_or(1).max(1);
                    self.primary_wrap_pending = false;
                    self.primary_cursor_row = self.primary_absolute_row(target_row);
                }
                ScreenMode::Alternate => {
                    let row = params.first().and_then(|value| *value).unwrap_or(1).max(1);
                    let target = self.alternate.absolute_row(row, self.origin_mode);
                    self.alternate.vertical_absolute(target);
                }
            },
            b'A' => {
                // CUU: cursor up N rows, stopping at the scroll region's
                // top margin (row 0 only when starting above it) — see
                // `primary_cursor_up`. An explicit 0 means 1, per xterm.
                let count = params.first().and_then(|value| *value).unwrap_or(1).max(1);
                match pane.screen_mode() {
                    ScreenMode::Primary => self.primary_cursor_up(count),
                    ScreenMode::Alternate => self.alternate.cursor_up(count),
                }
            }
            b'B' => {
                // CUD: cursor down N rows, stopping at the scroll
                // region's bottom margin (viewport bottom only when
                // starting below it). Clamps against the VIEWPORT, not
                // grid.len() — apps address fixed terminal rows that
                // may not have been written yet; the grid lazy-grows
                // on print.
                let count = params.first().and_then(|value| *value).unwrap_or(1).max(1);
                match pane.screen_mode() {
                    ScreenMode::Primary => self.primary_cursor_down(count),
                    ScreenMode::Alternate => self.alternate.cursor_down(count),
                }
            }
            b'C' => match pane.screen_mode() {
                ScreenMode::Primary => {
                    // CUF on primary: cursor right N columns, clamped at
                    // cols-1. Pure cursor move — cell content underneath
                    // is unchanged; the row pads lazily on the next
                    // print.
                    let count = params.first().and_then(|value| *value).unwrap_or(1).max(1);
                    self.primary_wrap_pending = false;
                    self.primary_cursor_col = (self.primary_cursor_col + count)
                        .min(self.alternate.cols.saturating_sub(1));
                }
                ScreenMode::Alternate => {
                    // Explicit 0 means 1, per xterm.
                    let count = params.first().and_then(|value| *value).unwrap_or(1).max(1);
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
                    // Explicit 0 means 1, per xterm.
                    let count = params.first().and_then(|value| *value).unwrap_or(1).max(1);
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
            b'S' if prefix.is_none()
                && !has_private_marker
                && intermediate.is_none()
                && params.len() <= 1 =>
            {
                // SU — Scroll Up Ps rows inside the active DECSTBM
                // region without moving the cursor. Codex uses a
                // top-anchored region for its transcript and composer;
                // rows leaving that kind of primary region must enter
                // scrollback just like rows displaced by IND.
                let count = params.first().and_then(|value| *value).unwrap_or(1).max(1);
                match pane.screen_mode() {
                    ScreenMode::Primary => self.primary_scroll_up_within_region(pane, count),
                    ScreenMode::Alternate => self.alternate.scroll_up_within_region(count),
                }
            }
            b'T' if prefix.is_none()
                && !has_private_marker
                && intermediate.is_none()
                && params.len() <= 1 =>
            {
                // SD — Scroll Down Ps rows inside the active DECSTBM
                // region, cursor unchanged. The parameter-count guard
                // avoids confusing the historical multi-parameter
                // xterm highlight-tracking form with ECMA-48 SD.
                let count = params.first().and_then(|value| *value).unwrap_or(1).max(1);
                match pane.screen_mode() {
                    ScreenMode::Primary => self.primary_scroll_down_within_region(count),
                    ScreenMode::Alternate => self.alternate.scroll_down_within_region(count),
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
                        // BCE: a styled pen repaints the whole viewport
                        // with the active background instead of leaving
                        // the implicit default-blank screen.
                        if self.current_style.bg != Color::Default {
                            let viewport = self.alternate.rows.max(1);
                            let painted = self.primary_blank_row();
                            self.primary_grid.resize(viewport, painted);
                        }
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
            b'I' if prefix.is_none() => {
                // CHT — Cursor Forward Tabulation. HT repeated N times
                // (default 1); see `advance_tab_stops`.
                let count = params.first().and_then(|value| *value).unwrap_or(1).max(1);
                self.advance_tab_stops(pane, count);
            }
            b'Z' if prefix.is_none() => {
                // CBT — Cursor Backward Tabulation. The mirror of CHT,
                // default 1; see `retreat_tab_stops`.
                let count = params.first().and_then(|value| *value).unwrap_or(1).max(1);
                self.retreat_tab_stops(pane, count);
            }
            b'g' if prefix.is_none() => {
                // TBC — Tab Clear. Ps 0 (default, no param) clears the
                // stop at the cursor's current column; Ps 3 empties the
                // whole table (see `clear_all_tab_stops`). Other Ps
                // values (1/2/4/5 — DEC's line-tabs variants) don't
                // apply here since zmux doesn't model per-line tab
                // stops, so they're silently ignored like every other
                // unrecognized parameter in this parser.
                let mode = params.first().and_then(|value| *value).unwrap_or(0);
                match mode {
                    0 => {
                        let col = match pane.screen_mode() {
                            ScreenMode::Primary => self.primary_cursor_col,
                            ScreenMode::Alternate => self.alternate.cursor_col,
                        };
                        self.clear_tab_stop(col);
                    }
                    3 => self.clear_all_tab_stops(),
                    _ => {}
                }
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
                    ScreenMode::Alternate => self.alternate.set_scroll_region(
                        top.saturating_sub(1),
                        bottom.saturating_sub(1),
                        self.origin_mode,
                    ),
                }
            }
            b'n' if prefix.is_none() => {
                let code = params.first().and_then(|value| *value).unwrap_or(0);
                match code {
                    // DSR 5: "report status" — always answer "OK". Some
                    // TUI libraries probe this at startup and wait on
                    // the reply.
                    5 => return Some(b"\x1b[0n".to_vec()),
                    6 => return Some(self.cursor_position_report(pane)),
                    _ => {}
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
                            // Clear-on-entry is a 1049-only behavior in
                            // xterm. Mode 47 is a bare buffer switch —
                            // content from the last alt session survives
                            // until the entering app repaints — and 1047
                            // clears on EXIT instead (see the `l` arm).
                            if param == 1049 {
                                self.alternate.reset();
                                // reset() just baselined fill to the
                                // default pen, but erases in the freshly-
                                // entered alt screen must blank with the
                                // CURRENTLY RUNNING pen (BCE), not a
                                // neutral default — resync it now that
                                // reset() is done clearing. (47/1047 skip
                                // this: fill is already tracking the pen.)
                                self.alternate.set_fill_style(self.current_style.clone());
                                self.alt_saved_pen = Style::DEFAULT;
                                self.alt_saved_charset = CharsetState::default();
                                self.alt_saved_origin = false;
                            }
                        }
                        6 => {
                            // DECOM set: row addressing becomes margin-
                            // relative, and the cursor homes to the
                            // region's origin (xterm homes on both set
                            // and reset).
                            self.origin_mode = true;
                            match pane.screen_mode() {
                                ScreenMode::Primary => {
                                    let top = self.primary_scroll_top;
                                    self.primary_set_cursor(top, 0);
                                }
                                ScreenMode::Alternate => {
                                    let top = self.alternate.scroll_top;
                                    self.alternate.set_cursor(top, 0);
                                }
                            }
                        }
                        7 => {
                            self.set_primary_auto_wrap(true);
                            self.alternate.set_auto_wrap(true);
                        }
                        25 => self.cursor_visible = true,
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
                            // 1047 is the "clear on exit" variant: wipe
                            // the alt buffer on a real alt→primary
                            // transition so stale frames can't leak into
                            // the next bare-47 session (which enters
                            // without clearing, per xterm).
                            if param == 1047 && was_alternate {
                                self.alternate.clear_all();
                            }
                            pane.set_screen_mode(ScreenMode::Primary);
                            if param == 1049 && was_alternate {
                                self.restore_cursor_state(pane);
                            }
                        }
                        6 => {
                            // DECOM reset: back to absolute addressing,
                            // cursor homes to the screen origin.
                            self.origin_mode = false;
                            match pane.screen_mode() {
                                ScreenMode::Primary => self.primary_set_cursor(0, 0),
                                ScreenMode::Alternate => self.alternate.set_cursor(0, 0),
                            }
                        }
                        7 => {
                            self.set_primary_auto_wrap(false);
                            self.alternate.set_auto_wrap(false);
                        }
                        25 => self.cursor_visible = false,
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

    // Where the attached client should paint the host cursor, in
    // viewport-relative 0-based (row, col) — or None when the
    // application asked for it to be hidden (DECTCEM), or the primary
    // cursor has scrolled above the visible viewport. The primary
    // mapping mirrors `render_primary_cells`: when the grid outgrew the
    // viewport the client paints the grid's last `viewport` rows, so
    // the grid-absolute cursor row is rebased against that window.
    pub fn screen_cursor(&self, pane: &Pane) -> Option<(usize, usize)> {
        if !self.cursor_visible {
            return None;
        }
        match pane.screen_mode() {
            ScreenMode::Primary => {
                let viewport = pane.viewport_height().max(1);
                let start = if self.primary_grid.len() >= viewport {
                    self.primary_grid.len() - viewport
                } else {
                    0
                };
                let row = self.primary_cursor_row.checked_sub(start)?;
                if row >= viewport {
                    return None;
                }
                Some((row, self.primary_cursor_col))
            }
            ScreenMode::Alternate => Some((self.alternate.cursor_row, self.alternate.cursor_col)),
        }
    }

    fn cursor_position_report(&self, pane: &Pane) -> Vec<u8> {
        // Under DECOM the report is margin-relative, mirroring how CUP
        // addresses rows — an app that positions with relative
        // coordinates must read back the same coordinate space.
        let (row, col) = match pane.screen_mode() {
            ScreenMode::Primary => {
                let row = if self.origin_mode {
                    self.primary_cursor_row
                        .saturating_sub(self.primary_scroll_top)
                } else {
                    self.primary_cursor_row
                };
                (
                    row.saturating_add(1),
                    self.primary_cursor_col.saturating_add(1),
                )
            }
            ScreenMode::Alternate => {
                let (row, col) = self.alternate.cursor_position();
                if self.origin_mode {
                    (row.saturating_sub(self.alternate.scroll_top), col)
                } else {
                    (row, col)
                }
            }
        };

        format!("\x1b[{row};{col}R").into_bytes()
    }

    fn hard_reset(&mut self, pane: &mut Pane) {
        pane.set_screen_mode(ScreenMode::Primary);
        pane.set_mouse_tracking_mode(MouseTrackingMode::Off);
        self.current_style = Style::DEFAULT;
        // No explicit set_fill_style() needed here: reset() itself now
        // baselines fill to Cell::BLANK (the default pen), which is
        // exactly what a hard reset wants since current_style is being
        // reset to default in this same call.
        self.alternate.reset();
        self.alt_saved_pen = Style::DEFAULT;
        self.alt_saved_charset = CharsetState::default();
        self.alt_saved_origin = false;
        self.alternate.set_auto_wrap(true);
        self.origin_mode = false;
        self.reset_primary_modes(true);
        // RIS restores the power-up default tab stops (every 8th
        // column). Unlike DECSTR below, hard reset is documented to put
        // *everything* back to its initial state, tabs included.
        self.tab_stops = None;
        self.cursor_shape = CursorShape::Default;
        self.cursor_visible = true;
        self.last_graphic = None;
        self.bracketed_paste = false;
        self.focus_events = false;
        self.synchronized_buffer = None;
        self.utf8_buffer.clear();
        self.utf8_remaining = 0;
        // RIS reinitializes character-set state: G0/G1 back to ASCII,
        // GL back to G0.
        self.charset = CharsetState::default();
    }

    fn soft_reset(&mut self, pane: &mut Pane) {
        pane.set_mouse_tracking_mode(MouseTrackingMode::Off);
        self.current_style = Style::DEFAULT;
        self.alternate.set_fill_style(Style::DEFAULT);
        // DECSTR resets DECOM, per DEC's documented reset list.
        self.origin_mode = false;
        self.reset_primary_modes(false);
        self.alternate.set_auto_wrap(true);
        self.alternate.scroll_top = 0;
        self.alternate.scroll_bottom = self.alternate.rows.saturating_sub(1);
        self.cursor_shape = CursorShape::Default;
        // DECSTR resets DECTCEM to visible, same as xterm.
        self.cursor_visible = true;
        self.bracketed_paste = false;
        self.focus_events = false;
        self.synchronized_buffer = None;
        // DECSTR also reinvokes the default character sets (G0/G1 ->
        // ASCII, GL -> G0), same as RIS.
        self.charset = CharsetState::default();
        // Deliberately NOT touched: `tab_stops`. DEC's documented DECSTR
        // reset list (cursor visibility, insert/replace mode, origin
        // mode, autowrap, margins, character sets, SGR, saved cursor)
        // does not include tab stops — only RIS does, in `hard_reset`.
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

    // DECSC / CSI s. Saves position + pen + charset state for whichever
    // screen is active.
    fn save_cursor_state(&mut self, pane: &Pane) {
        match pane.screen_mode() {
            ScreenMode::Primary => {
                self.primary_saved_cursor = Some(SavedCursor {
                    row: self.primary_cursor_row,
                    col: self.primary_cursor_col,
                    pen: self.current_style.clone(),
                    charset: self.charset,
                    origin: self.origin_mode,
                });
            }
            ScreenMode::Alternate => {
                self.alternate.save_cursor();
                self.alt_saved_pen = self.current_style.clone();
                self.alt_saved_charset = self.charset;
                self.alt_saved_origin = self.origin_mode;
            }
        }
    }

    // DECRC / CSI u. Restores position + pen + charset state. Primary
    // keeps its historical no-op when nothing was saved; the alternate
    // screen restores its (home, default-pen, default-charset) baseline,
    // matching xterm's reset-attributes behavior for DECRC without a
    // prior DECSC.
    fn restore_cursor_state(&mut self, pane: &Pane) {
        match pane.screen_mode() {
            ScreenMode::Primary => {
                if let Some(saved) = self.primary_saved_cursor.clone() {
                    self.primary_set_cursor(saved.row, saved.col);
                    self.current_style = saved.pen;
                    self.charset = saved.charset;
                    // Position was saved absolute; if the restored mode
                    // is DECOM, pull it back inside the margins so the
                    // cursor can't land where relative addressing could
                    // never have put it.
                    self.origin_mode = saved.origin;
                    if self.origin_mode {
                        let max_row = self.alternate.rows.saturating_sub(1);
                        self.primary_cursor_row = self
                            .primary_cursor_row
                            .clamp(self.primary_scroll_top, self.primary_scroll_bottom)
                            .min(max_row);
                    }
                }
            }
            ScreenMode::Alternate => {
                self.alternate.restore_cursor();
                self.current_style = self.alt_saved_pen.clone();
                self.charset = self.alt_saved_charset;
                self.origin_mode = self.alt_saved_origin;
                if self.origin_mode {
                    let row = self
                        .alternate
                        .cursor_row
                        .clamp(self.alternate.scroll_top, self.alternate.scroll_bottom);
                    self.alternate.cursor_row = row.min(self.alternate.rows.saturating_sub(1));
                }
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

    // Resolve a 1-based CUP/VPA row parameter to an absolute grid row.
    // With DECOM reset this is plain 1-based→0-based conversion clamped
    // to the viewport; with DECOM set the parameter is relative to the
    // scroll region's top margin and confined to the region.
    fn primary_absolute_row(&self, row_param: usize) -> usize {
        let max_row = self.alternate.rows.saturating_sub(1);
        let row = row_param.saturating_sub(1);
        if self.origin_mode {
            self.primary_scroll_top
                .saturating_add(row)
                .min(self.primary_scroll_bottom.min(max_row))
        } else {
            row.min(max_row)
        }
    }

    fn primary_set_scroll_region(&mut self, top: usize, bottom: usize) {
        // xterm clamps an oversized bottom margin to the last row rather
        // than rejecting the sequence — `CSI 3;999r` is a common way for
        // apps to say "row 3 through the bottom, whatever the height".
        // Only a region that is still degenerate after clamping (top at
        // or below bottom) is ignored.
        let max_row = self.alternate.rows.saturating_sub(1);
        let bottom = bottom.min(max_row);
        if top >= bottom {
            return;
        }

        self.primary_scroll_top = top;
        self.primary_scroll_bottom = bottom;
        // DECSTBM homes to the origin: the screen's top-left, or the
        // region's when DECOM is set.
        let home_row = if self.origin_mode { top } else { 0 };
        self.primary_set_cursor(home_row, 0);
    }

    /// Background cell using the current SGR style — what an erase
    /// op should leave behind so the cleared region picks up whatever
    /// background color is active (matches xterm semantics).
    // The cell erase/insert fills paint with — the pen's background and
    // nothing else (see Style::erase_fill).
    fn primary_blank_cell(&self) -> Cell {
        Cell::styled(' ', self.current_style.erase_fill())
    }

    fn primary_blank_row(&self) -> Vec<Cell> {
        vec![self.primary_blank_cell(); self.alternate.cols.max(1)]
    }

    // Blank rows entering the grid from IL/DL/SU/SD fills. With a
    // default background these stay zero-length — rows past a line's
    // stored tail are implicitly blank, and keeping them sparse means
    // flushed scrollback lines (and therefore yanked text) don't
    // accumulate full-width runs of trailing spaces. A pen with a real
    // background materializes cells so it paints the cleared band
    // (BCE). The gate is the BACKGROUND only, matching xterm: fg and
    // attributes don't propagate onto erased cells.
    fn primary_region_fill_row(&self) -> Vec<Cell> {
        if self.current_style.bg == Color::Default {
            Vec::new()
        } else {
            self.primary_blank_row()
        }
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
        self.primary_wrap_pending = false;
        let cols = self.alternate.cols;
        let row = self.primary_cursor_row;
        let col = self.primary_cursor_col;
        let blank = self.primary_blank_cell();
        // BCE: with the default pen, erased cells can stay implicit
        // (rows keep their sparse tails). A pen with a real background
        // must leave styled blanks behind so the cleared span shows
        // that background — including cells past a row's stored end,
        // which have to be materialized to carry a style at all.
        let styled = self.current_style.bg != Color::Default;
        if self.primary_grid.is_empty() && !styled {
            return;
        }
        if styled {
            while self.primary_grid.len() <= row {
                self.primary_grid.push(Vec::new());
            }
        }
        match mode {
            // 0 (default): cursor → eol. Right of `line.len()` is
            // implicitly blank with the default pen; a styled pen
            // extends the row to full width first. The gap left of the
            // cursor (never-written cells) pads with DEFAULT blanks —
            // only the erased span picks up the pen.
            0 => {
                if let Some(line) = self.primary_grid.get_mut(row) {
                    // A cursor sitting on a continuation cell splits its
                    // pair: the base to the left survives the erase.
                    crate::style::sever_wide_pair(line, col);
                    if styled {
                        if line.len() < col {
                            line.resize(col, Cell::BLANK);
                        }
                        if line.len() < cols {
                            line.resize(cols, blank.clone());
                        }
                    }
                    if line.len() > col {
                        for cell in &mut line[col..] {
                            *cell = blank.clone();
                        }
                    }
                }
            }
            // 1: sol → cursor (inclusive)
            1 => {
                self.primary_pad_row(row, (col + 1).min(cols));
                if let Some(line) = self.primary_grid.get_mut(row) {
                    let end = (col + 1).min(line.len());
                    // A cursor on the LEFT half erases the base but
                    // leaves the continuation just past the span.
                    crate::style::sever_wide_pair(line, end);
                    for cell in &mut line[..end] {
                        *cell = blank.clone();
                    }
                }
            }
            // 2: whole line
            2 => {
                let full = if styled {
                    Some(self.primary_blank_row())
                } else {
                    None
                };
                if let Some(line) = self.primary_grid.get_mut(row) {
                    match full {
                        Some(painted) => *line = painted,
                        None => line.clear(),
                    }
                }
            }
            _ => {}
        }
    }

    fn primary_erase_below(&mut self) {
        // ED 0: cursor → eol on current row, then the rows below. With
        // the default pen dropping them is enough (they're implicitly
        // blank); a styled pen paints every viewport row below the
        // cursor with the active background (BCE), materializing rows
        // that never existed.
        self.primary_erase_line(0);
        let row = self.primary_cursor_row;
        if self.current_style.bg != Color::Default {
            let viewport = self.alternate.rows.max(1);
            let painted = self.primary_blank_row();
            while self.primary_grid.len() < viewport {
                self.primary_grid.push(Vec::new());
            }
            for line in self.primary_grid.iter_mut().skip(row + 1) {
                *line = painted.clone();
            }
        } else if row + 1 < self.primary_grid.len() {
            self.primary_grid.truncate(row + 1);
        }
    }

    fn primary_erase_above(&mut self) {
        // ED 1: blank rows above cursor + sol → cursor on current row.
        // Styled pens replace each row with a full-width painted row
        // (BCE); the default pen keeps the old fill-in-place behavior.
        let row = self.primary_cursor_row;
        if self.current_style.bg != Color::Default {
            let painted = self.primary_blank_row();
            for line in self.primary_grid.iter_mut().take(row) {
                *line = painted.clone();
            }
        } else {
            let blank = self.primary_blank_cell();
            for line in self.primary_grid.iter_mut().take(row) {
                line.fill(blank.clone());
            }
        }
        self.primary_erase_line(1);
    }

    fn primary_insert_lines(&mut self, count: usize) {
        // IL: shift rows [cursor..=region bottom] down inside the
        // DECSTBM region, blanks entering at the cursor and rows leaving
        // past the region's bottom margin discarded. Rows BELOW the
        // region must not move — a raw Vec::insert shifts the whole
        // tail, which chews up composer/status rows apps park under a
        // top-anchored region (codex's transcript layout). Outside the
        // region IL is a no-op, per xterm.
        self.primary_wrap_pending = false;
        let max_row = self.alternate.rows.saturating_sub(1);
        let top = self.primary_scroll_top.min(max_row);
        let bottom = self.primary_scroll_bottom.min(max_row);
        let row = self.primary_cursor_row;
        if row < top || row > bottom {
            return;
        }
        // Materialize sparse rows so the rotate below has real storage,
        // same as primary_scroll_up_within_region.
        while self.primary_grid.len() <= bottom {
            self.primary_grid.push(Vec::new());
        }
        let count = count.min(bottom - row + 1);
        let blank = self.primary_region_fill_row();
        let region = &mut self.primary_grid[row..=bottom];
        region.rotate_right(count);
        for line in region.iter_mut().take(count) {
            *line = blank.clone();
        }
        // Cursor stays put; col clamps to width.
    }

    fn primary_delete_lines(&mut self, count: usize) {
        // DL: the mirror of IL — rows [cursor..=region bottom] shift up,
        // blanks enter at the region's bottom margin, and rows below the
        // region stay put instead of being dragged up through it.
        // No-op outside the region, per xterm.
        self.primary_wrap_pending = false;
        let max_row = self.alternate.rows.saturating_sub(1);
        let top = self.primary_scroll_top.min(max_row);
        let bottom = self.primary_scroll_bottom.min(max_row);
        let row = self.primary_cursor_row;
        if row < top || row > bottom {
            return;
        }
        while self.primary_grid.len() <= bottom {
            self.primary_grid.push(Vec::new());
        }
        let count = count.min(bottom - row + 1);
        let blank = self.primary_region_fill_row();
        let region = &mut self.primary_grid[row..=bottom];
        region.rotate_left(count);
        let len = region.len();
        for line in region.iter_mut().skip(len - count) {
            *line = blank.clone();
        }
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
            // Both deletion boundaries can split a wide-char pair: the
            // cursor can sit on a continuation cell, and the cell just
            // past the deleted span can be a continuation whose base is
            // being deleted (it shifts left as an orphan otherwise).
            crate::style::sever_wide_pair(line, col);
            crate::style::sever_wide_pair(line, end);
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
            // Inserting between the halves of a wide char splits the
            // pair; blank both halves rather than strand them around
            // the inserted blanks. (The truncation below can also strand
            // a base at the last column — render-time clipping blanks
            // that one, same as any other edge-clipped wide glyph.)
            crate::style::sever_wide_pair(line, col);
            for _ in 0..count {
                line.insert(col, blank.clone());
            }
            if line.len() > cols {
                line.truncate(cols);
            }
        }
    }

    // CUU/CUD (and CNL/CPL, which are these plus a carriage return) are
    // margin-confined, per DEC STD 070 / xterm: a cursor starting at or
    // below the top margin stops there on the way up, one starting at
    // or above the bottom margin stops there on the way down. Only a
    // cursor already OUTSIDE the region gets the full screen bound.
    // Never scrolls — relative moves clamp, they don't push content.
    fn primary_cursor_up(&mut self, count: usize) {
        self.primary_wrap_pending = false;
        let top = if self.primary_cursor_row < self.primary_scroll_top {
            0
        } else {
            self.primary_scroll_top
        };
        self.primary_cursor_row = self.primary_cursor_row.saturating_sub(count).max(top);
    }

    fn primary_cursor_down(&mut self, count: usize) {
        self.primary_wrap_pending = false;
        let max_row = self.alternate.rows.saturating_sub(1);
        let bottom = if self.primary_cursor_row > self.primary_scroll_bottom {
            max_row
        } else {
            self.primary_scroll_bottom.min(max_row)
        };
        self.primary_cursor_row = self.primary_cursor_row.saturating_add(count).min(bottom);
    }

    fn primary_next_line(&mut self, count: usize) {
        // CNL: CUD + CR. It does not print or scroll (margin-clamped
        // like any relative vertical move); subsequent output lazily
        // grows the grid if the target row hasn't existed yet.
        self.primary_cursor_down(count);
        self.primary_cursor_col = 0;
    }

    fn primary_previous_line(&mut self, count: usize) {
        // CPL: CUU + CR. Missing this leaves redraw cursors too low
        // when a TUI walks back up to repaint boxes.
        self.primary_cursor_up(count);
        self.primary_cursor_col = 0;
    }

    fn set_primary_auto_wrap(&mut self, enabled: bool) {
        self.primary_auto_wrap = enabled;
        if !enabled {
            self.primary_wrap_pending = false;
        }
    }

    fn primary_erase_chars(&mut self, count: usize) {
        // ECH: replace N cells at cursor with blanks; no shift. Styled
        // pens materialize the erased span past a row's stored end so
        // the background paints (BCE), same as primary_erase_line.
        self.primary_wrap_pending = false;
        let col = self.primary_cursor_col;
        let row = self.primary_cursor_row;
        let cols = self.alternate.cols;
        let blank = self.primary_blank_cell();
        let styled = self.current_style.bg != Color::Default;
        if self.primary_grid.is_empty() && !styled {
            return;
        }
        if styled {
            while self.primary_grid.len() <= row {
                self.primary_grid.push(Vec::new());
            }
        }
        if let Some(line) = self.primary_grid.get_mut(row) {
            let target_end = col.saturating_add(count).min(cols);
            if styled {
                if line.len() < col {
                    line.resize(col, Cell::BLANK);
                }
                if line.len() < target_end {
                    line.resize(target_end, blank.clone());
                }
            }
            if col >= line.len() {
                return;
            }
            let end = target_end.min(line.len());
            // Either edge of the erased span can split a wide pair.
            crate::style::sever_wide_pair(line, col);
            crate::style::sever_wide_pair(line, end);
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

    // Shared swallow loop for DCS/APC/PM/SOS (see the `ParseState`
    // variant doc). No buffer, no cap needed: unlike OSC (which has to
    // hold onto its payload to dispatch title/hyperlink/palette-query
    // actions) these four are consumed and thrown away byte-by-byte, so
    // there's nothing here that can grow unbounded the way an
    // unterminated OSC's stored buffer could.
    fn handle_string_swallow(&mut self, byte: u8) {
        self.state = match byte {
            b'\x1b' => ParseState::StringSwallowEscape,
            _ => ParseState::StringSwallow,
        };
    }

    fn handle_string_swallow_escape(&mut self, byte: u8) {
        self.state = match byte {
            b'\\' => ParseState::Ground,
            _ => ParseState::StringSwallow,
        };
    }

    // `ESC #` DEC private single-byte sequences (DECDHL/DECSWL/DECDWL/
    // DECALN). zmux doesn't model line-height/width attributes or the
    // DECALN screen-alignment fill, so the following byte is simply
    // discarded — the point of this state is only to keep it from
    // falling through to Ground and printing as a stray character.
    fn handle_escape_hash(&mut self, pane: &mut Pane, byte: u8) {
        // Of the `ESC #` family only DECALN (`8`, screen-alignment test)
        // is modeled; DECDHL/DECSWL/DECDWL (`3`/`4`/`5`/`6`) still just
        // swallow their final byte so it doesn't print literally.
        if byte == b'8' {
            self.decaln(pane);
        }
        self.state = ParseState::Ground;
    }

    // DECALN (`ESC # 8`): fill the active screen with uppercase 'E' in
    // the default rendition, reset the scroll margins and DECOM, and
    // home the cursor. Test tools (vttest, esctest) use it to verify
    // screen geometry; margins/origin reset per VT510 & xterm.
    fn decaln(&mut self, pane: &mut Pane) {
        self.origin_mode = false;
        match pane.screen_mode() {
            ScreenMode::Primary => {
                let rows = self.alternate.rows.max(1);
                let cols = self.alternate.cols.max(1);
                self.primary_scroll_top = 0;
                self.primary_scroll_bottom = rows - 1;
                self.primary_wrap_pending = false;
                let row = vec![Cell::styled('E', Style::DEFAULT); cols];
                self.primary_grid = vec![row; rows];
                self.primary_cursor_row = 0;
                self.primary_cursor_col = 0;
            }
            ScreenMode::Alternate => self.alternate.decaln(),
        }
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

    // HT (0x09): move the cursor to the next tab stop, or the last
    // column if there isn't one ahead. NON-destructive — real HT is a
    // cursor move, not a fill: it must not write cells (an in-place
    // redraw over tab-indented text would otherwise blow away whatever
    // was already there) and must not touch the SGR pen (a tab between
    // two differently-styled fragments must not repaint the gap).
    // CHT (`CSI I`) is the same move repeated N times; see
    // `advance_tab_stops`.
    fn handle_tab(&mut self, pane: &mut Pane) {
        self.advance_tab_stops(pane, 1);
    }

    // Shared by HT (count=1) and CHT (count=N, `CSI I`): hop the cursor
    // forward one tab stop at a time. Hopping one-at-a-time (rather than
    // computing the Nth stop directly) means a stop table with uneven
    // gaps still lands correctly, and running out of stops before using
    // up `count` just parks the cursor at the last column (each further
    // hop is then a no-op — `next_tab_stop` saturates there) instead of
    // panicking or overshooting.
    fn advance_tab_stops(&mut self, pane: &mut Pane, count: usize) {
        let last_col = self.alternate.cols.saturating_sub(1);
        for _ in 0..count.max(1) {
            match pane.screen_mode() {
                ScreenMode::Primary => {
                    let next = self.next_tab_stop(self.primary_cursor_col, last_col);
                    self.primary_wrap_pending = false;
                    self.primary_cursor_col = next;
                }
                ScreenMode::Alternate => {
                    let next = self.next_tab_stop(self.alternate.cursor_col, last_col);
                    self.alternate.horizontal_absolute(next);
                }
            }
        }
    }

    // CBT (`CSI Z`, count=N): the backward mirror of `advance_tab_stops`.
    // No control code drives this with count=1 (there's no "bare CBT"
    // control character), only the CSI form, but the loop shape matches
    // for the same reasons — hop one stop at a time, saturate at column 0.
    fn retreat_tab_stops(&mut self, pane: &mut Pane, count: usize) {
        for _ in 0..count.max(1) {
            match pane.screen_mode() {
                ScreenMode::Primary => {
                    let prev = self.previous_tab_stop(self.primary_cursor_col);
                    self.primary_wrap_pending = false;
                    self.primary_cursor_col = prev;
                }
                ScreenMode::Alternate => {
                    let prev = self.previous_tab_stop(self.alternate.cursor_col);
                    self.alternate.horizontal_absolute(prev);
                }
            }
        }
    }

    // True if `col` is a tab stop. Customized tables (`Some`) answer
    // directly from the explicit set; the uncustomized default table
    // (`None`) has a stop at every 8th column (8, 16, 24, ...) — column
    // 0 is deliberately excluded since both search directions
    // (`next_tab_stop` / `previous_tab_stop`) only ever look strictly
    // past the cursor, so a "stop" sitting exactly on it would never be
    // reachable anyway.
    fn is_tab_stop(&self, col: usize) -> bool {
        match &self.tab_stops {
            Some(stops) => stops.contains(&col),
            None => col > 0 && col % 8 == 0,
        }
    }

    // HT / CHT target: the smallest tab stop strictly right of `col`,
    // or `last_col` if the table has nothing further out. Matches
    // xterm, which drives the cursor to the right edge rather than
    // leaving it in place once it runs out of stops.
    fn next_tab_stop(&self, col: usize, last_col: usize) -> usize {
        ((col + 1)..=last_col)
            .find(|&candidate| self.is_tab_stop(candidate))
            .unwrap_or(last_col)
    }

    // CBT target: the largest tab stop strictly left of `col`, or
    // column 0 if there isn't one — the mirror image of `next_tab_stop`.
    fn previous_tab_stop(&self, col: usize) -> usize {
        (0..col)
            .rev()
            .find(|&candidate| self.is_tab_stop(candidate))
            .unwrap_or(0)
    }

    // The default rule (every 8th column) materialized into a concrete
    // set, for the moment a customization (HTS/TBC) needs one to build
    // on. `last_col` is `cols - 1`; stops run 8, 16, 24, ... up to it.
    fn default_tab_stops(last_col: usize) -> HashSet<usize> {
        (8..=last_col).step_by(8).collect()
    }

    // HTS (`ESC H`): add a stop at `col`. First call materializes the
    // explicit table (see the `tab_stops` field comment) seeded with the
    // current defaults, so the new stop joins the ones already implied
    // rather than replacing them.
    fn set_tab_stop(&mut self, col: usize) {
        let last_col = self.alternate.cols.saturating_sub(1);
        self.tab_stops
            .get_or_insert_with(|| Self::default_tab_stops(last_col))
            .insert(col);
    }

    // TBC `CSI 0 g` (default): clear the stop at `col`. Same
    // materialize-on-first-use behavior as `set_tab_stop`.
    fn clear_tab_stop(&mut self, col: usize) {
        let last_col = self.alternate.cols.saturating_sub(1);
        self.tab_stops
            .get_or_insert_with(|| Self::default_tab_stops(last_col))
            .remove(&col);
    }

    // TBC `CSI 3 g`: empty the table outright. xterm does not fall back
    // to the default rule after this — there are genuinely no stops
    // left, so HT/CHT run all the way to the last column until a new
    // HTS puts one back. See the `tab_stops` field comment.
    fn clear_all_tab_stops(&mut self) {
        self.tab_stops = Some(HashSet::new());
    }

    fn handle_printable(&mut self, pane: &mut Pane, ch: char) {
        // DEC Special Graphics substitutes 31 GL positions (0x60..=0x7e,
        // backtick through tilde) with line-drawing / math glyphs; every
        // other GL byte — including the digits and lower punctuation —
        // prints literally even while the slot is active. This only
        // ever fires for single-byte ASCII printables: `ch` arrives
        // here either straight from the 0x20..=0x7e ground-state arm or
        // as a fully reassembled UTF-8 multibyte char (always >= 0x80),
        // so a decoded multibyte char can never land in the translated
        // range and passes through untouched, as required.
        let ch = if self.charset.active_charset() == Charset::DecSpecialGraphics
            && ('\u{60}'..='\u{7e}').contains(&ch)
        {
            dec_special_graphics(ch)
        } else {
            ch
        };
        match pane.screen_mode() {
            ScreenMode::Primary => self.primary_put_char(pane, ch),
            ScreenMode::Alternate => self.alternate.put_char(ch, self.current_style.clone()),
        }
        // Remember the last graphic char so CSI `b` (REP) can repeat it.
        // Only printable chars qualify — control codes (newline, CR,
        // backspace, tab) are routed elsewhere and intentionally do
        // not poison this slot. Stored POST-translation so a REP that
        // follows a box-drawing char repeats the drawn glyph, not the
        // ASCII letter that was sent for it.
        self.last_graphic = Some(ch);
    }

    // Linefeed on the primary grid: cursor down one row, col reset to 0.
    // (Most terminals separate linefeed from carriage return, but every
    // existing caller of this code treats `\n` as both — shells emit
    // `\r\n` so `\r` is a no-op there, and bare `\n` from poorly-behaved
    // sources still wants to land on a fresh column-0 line.) That makes
    // this identical to NEL (CR + IND), so NEL reuses it directly rather
    // than re-deriving the same two steps.
    fn primary_linefeed(&mut self, pane: &mut Pane) {
        self.primary_cursor_col = 0;
        self.primary_index(pane);
    }

    // IND (`ESC D`): cursor down one row, column untouched — the part of
    // `primary_linefeed` that isn't the carriage return, split out so
    // IND can share it without inheriting LF's column reset.
    //
    // Push behavior:
    //   - if the cursor is at the bottom of the grid AND the grid hasn't
    //     hit the PTY-row ceiling: append a fresh blank row, advance
    //     the cursor into it.
    //   - if the grid is at the ceiling: evict grid[0] to scrollback,
    //     shift everything up one row, push a blank row at the bottom,
    //     leave the cursor at the (now-bottom) row.
    //   - otherwise: just advance the cursor.
    fn primary_index(&mut self, pane: &mut Pane) {
        self.primary_wrap_pending = false;
        let max_rows = self.alternate.rows.max(1);

        // Make sparse absolute-positioned rows real before applying IND.
        // Without this, CUP/CUD to the bottom followed by IND collapses
        // the cursor back to the current grid tail instead of scrolling
        // the viewport/scroll-region like a terminal would.
        while self.primary_grid.len() <= self.primary_cursor_row {
            self.primary_grid.push(Vec::new());
        }

        if self.primary_cursor_row == self.primary_scroll_bottom {
            if self.primary_scroll_top == 0 && self.primary_scroll_bottom == max_rows - 1 {
                let evicted = self.primary_grid.remove(0);
                // Sync before handing the row to scrollback: `append_
                // output_line` clamps the pane's viewport_top against
                // `max_viewport_top()` as part of appending, and that
                // clamp needs to see the grid's current size, not
                // whatever `live_tail` was left at by the last full
                // sync (which can be stale-low right after a flush or
                // a burst that grew the grid without evicting yet).
                // Without this, a scrolled-back viewport can get
                // wrongly pulled down mid-burst before `ingest_bytes`'
                // end-of-call sync ever runs — see `sync_live_tail`.
                self.sync_live_tail(pane);
                pane.append_output_line(evicted);
                self.primary_grid.push(Vec::new());
                self.primary_cursor_row = max_rows - 1;
            } else {
                self.primary_scroll_up_within_region(pane, 1);
            }
            return;
        }

        // Otherwise the cursor sits on the last screen row but NOT at
        // the scroll bottom — i.e. below an active region's bottom
        // margin (the margin-equal case returned above). xterm parks
        // the cursor: no move, and absolutely no scroll — evicting
        // grid[0] here would drag rows out through a region that exists
        // to protect them (codex parks its composer exactly like this,
        // under a top-anchored region).
        if self.primary_cursor_row + 1 < max_rows {
            self.primary_cursor_row += 1;
        }
    }

    // RI (`ESC M`): cursor up one row, column untouched. Mirrors
    // `AlternateScreen::reverse_index` — at the scroll region's top
    // margin the region scrolls down (blank line at the top, the
    // region's bottom line discarded) instead of moving the cursor past
    // it. Reverse scrolling never feeds scrollback; that only happens on
    // the forward (IND) direction.
    //
    // Unlike `AlternateScreen::reverse_index`'s bare `cursor_row -= 1`,
    // this uses `saturating_sub`: DECSTBM parks the cursor at (0, 0)
    // after setting a region, so a region whose top isn't row 0 can
    // leave the cursor above `primary_scroll_top`. An RI from there must
    // clamp at row 0 like every other primary-screen vertical move, not
    // underflow.
    fn primary_reverse_index(&mut self) {
        self.primary_wrap_pending = false;
        if self.primary_cursor_row == self.primary_scroll_top {
            self.primary_scroll_down_within_region(1);
        } else {
            self.primary_cursor_row = self.primary_cursor_row.saturating_sub(1);
        }
    }

    // Print one character at the cursor's current cell on the primary
    // grid, with wide-char handling identical to the alt-screen path.
    // Lazily grows the grid (rows up to cursor_row+1) and the row's cell
    // vector (cells up to cursor_col+width) with `Cell::BLANK`. Wraps
    // to the next line when the cursor would advance off the right edge.
    fn primary_put_char(&mut self, pane: &mut Pane, ch: char) {
        let cols = self.alternate.cols.max(1);
        let width = crate::style::char_width(ch);

        if width == 0 {
            if let Some(cell) = self.primary_previous_cell_mut() {
                cell.append_suffix(ch);
            }
            return;
        }
        if self
            .primary_previous_cell_mut()
            .is_some_and(|cell| cell.suffix_ends_with_joiner())
        {
            if let Some(cell) = self.primary_previous_cell_mut() {
                cell.append_suffix(ch);
            }
            return;
        }

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
        // The write covers [col, col+width); if either boundary lands
        // mid-wide-char, blank the half that survives outside the span
        // (see style::sever_wide_pair for why orphans corrupt the row).
        crate::style::sever_wide_pair(row, col);
        crate::style::sever_wide_pair(row, col + width);
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
            // screen — including the edge rule: a wide base in the last
            // column (only reachable with autowrap off) gets NO
            // continuation cell. Pushing one would grow the row past the
            // pane width; the orphaned base render-clips to a blank.
            let cont_col = col + 1;
            if cont_col < cols {
                if cont_col < row.len() {
                    row[cont_col] = Cell::styled('\0', self.current_style.clone());
                } else {
                    row.push(Cell::styled('\0', self.current_style.clone()));
                }
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

    fn primary_previous_cell_mut(&mut self) -> Option<&mut Cell> {
        let row = self.primary_grid.get_mut(self.primary_cursor_row)?;
        let end = if self.primary_wrap_pending {
            self.primary_cursor_col.saturating_add(1)
        } else {
            self.primary_cursor_col
        }
        .min(row.len());
        row[..end].iter_mut().rfind(|cell| cell.ch != '\0')
    }

    fn primary_scroll_up_within_region(&mut self, pane: &mut Pane, count: usize) {
        self.primary_wrap_pending = false;
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
        let scrolled_off = if top == 0 {
            self.primary_grid[..count].to_vec()
        } else {
            Vec::new()
        };
        let blank = self.primary_region_fill_row();
        let region = &mut self.primary_grid[top..=bottom];
        region.rotate_left(count);
        let len = region.len();
        for row in region.iter_mut().skip(len.saturating_sub(count)) {
            *row = blank.clone();
        }

        // xterm commits rows that leave a primary scroll region anchored at
        // the top edge, even when the bottom margin reserves UI rows. Codex
        // uses exactly that layout for transcript + composer; dropping these
        // rows made its completed output impossible to scroll back to.
        if !scrolled_off.is_empty() {
            self.sync_live_tail(pane);
            for row in scrolled_off {
                pane.append_output_line(row);
            }
        }
    }

    // RI's counterpart to `primary_scroll_up_within_region`: rotate the
    // region the other way and blank the rows that rotated in at the
    // top instead of the bottom.
    fn primary_scroll_down_within_region(&mut self, count: usize) {
        self.primary_wrap_pending = false;
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
        let blank = self.primary_region_fill_row();
        let region = &mut self.primary_grid[top..=bottom];
        region.rotate_right(count);
        for row in region.iter_mut().take(count) {
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

fn clip_row_to_width(row: &[Cell], width: usize) -> Vec<Cell> {
    let width = width.max(1);
    let end = row.len().min(width);
    let mut clipped = row[..end].to_vec();
    let Some(last) = clipped.last_mut() else {
        return clipped;
    };

    // A double-width base at the right edge is only renderable when its
    // continuation cell also fits. Hide the orphan for this viewport but
    // leave the source row untouched so widening can reveal it again.
    if last.ch != '\0' && crate::style::char_width(last.ch) == 2 {
        *last = Cell::BLANK;
    }
    clipped
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
        // BCE keeps only the background: erased/shifted-in blanks must
        // not inherit fg, underline/reverse, or a hyperlink from the
        // pen (see Style::erase_fill).
        self.fill = Cell::styled(' ', style.erase_fill());
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

    // The alt-screen counterpart of `primary_absolute_row`: resolve a
    // 1-based CUP/VPA row parameter, margin-relative and region-confined
    // when DECOM is set.
    fn absolute_row(&self, row_param: usize, origin: bool) -> usize {
        let max_row = self.rows.saturating_sub(1);
        let row = row_param.saturating_sub(1);
        if origin {
            self.scroll_top
                .saturating_add(row)
                .min(self.scroll_bottom.min(max_row))
        } else {
            row.min(max_row)
        }
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

    // Margin-confined relative vertical moves (CUU/CUD and the CNL/CPL
    // built on them), mirroring `primary_cursor_up`/`primary_cursor_down`:
    // stop at the scroll margin when the cursor starts inside the
    // region, at the screen edge only when it starts outside. Never
    // scrolls.
    fn cursor_up(&mut self, count: usize) {
        self.wrap_pending = false;
        let top = if self.cursor_row < self.scroll_top {
            0
        } else {
            self.scroll_top
        };
        self.cursor_row = self.cursor_row.saturating_sub(count).max(top);
    }

    fn cursor_down(&mut self, count: usize) {
        self.wrap_pending = false;
        let bottom = if self.cursor_row > self.scroll_bottom {
            self.rows.saturating_sub(1)
        } else {
            self.scroll_bottom.min(self.rows.saturating_sub(1))
        };
        self.cursor_row = self.cursor_row.saturating_add(count).min(bottom);
    }

    fn set_scroll_region(&mut self, top: usize, bottom: usize, origin: bool) {
        // Clamp-then-validate, same as the primary screen: xterm pulls
        // an oversized bottom margin back to the last row instead of
        // dropping the whole sequence.
        let bottom = bottom.min(self.rows.saturating_sub(1));
        if top >= bottom {
            return;
        }

        self.scroll_top = top;
        self.scroll_bottom = bottom;
        // DECSTBM homes to the origin: the screen's top-left, or the
        // region's when DECOM is set.
        let home_row = if origin { top } else { 0 };
        self.set_cursor(home_row, 0);
    }

    fn reset(&mut self) {
        // Baseline `fill` to the default pen before clearing so the
        // struct is self-consistently "default" after a reset, same as
        // every other field below. This struct has no access to
        // current_style, so it can't know the pen actually running when
        // reset() is called (e.g. on alt-screen entry, mid-session);
        // callers that need `fill` to reflect the live pen must resync
        // it with set_fill_style() themselves right after calling this.
        self.fill = Cell::BLANK;
        self.clear_all();
        self.scroll_top = 0;
        self.scroll_bottom = self.rows.saturating_sub(1);
        self.auto_wrap = true;
        self.wrap_pending = false;
        self.set_cursor(0, 0);
        self.save_cursor();
    }

    // DECALN's alt-screen half: E-fill in the default rendition, full
    // margins, cursor home (see TerminalIngest::decaln).
    fn decaln(&mut self) {
        self.scroll_top = 0;
        self.scroll_bottom = self.rows.saturating_sub(1);
        let fill = Cell::styled('E', Style::DEFAULT);
        for row in &mut self.cells {
            row.fill(fill.clone());
        }
        self.set_cursor(0, 0);
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
            // saturating: DECSTBM homes the cursor to (0, 0), so a
            // region with top > 0 leaves the cursor above scroll_top
            // and RI from row 0 would underflow here.
            self.cursor_row = self.cursor_row.saturating_sub(1);
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
        // CNL is CUD + CR — a pure cursor move. It clamps at the bottom
        // margin and NEVER scrolls (routing it through linefeed() would
        // scroll the region away once the cursor reaches the margin).
        self.cursor_down(count.max(1));
        self.carriage_return();
    }

    fn previous_line(&mut self, count: usize) {
        // CPL: CUU + CR, margin-clamped like next_line.
        self.cursor_up(count.max(1));
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

        let width = crate::style::char_width(ch);
        if width == 0 {
            if let Some(cell) = self.previous_cell_mut() {
                cell.append_suffix(ch);
            }
            return;
        }
        if self
            .previous_cell_mut()
            .is_some_and(|cell| cell.suffix_ends_with_joiner())
        {
            if let Some(cell) = self.previous_cell_mut() {
                cell.append_suffix(ch);
            }
            return;
        }

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

        // Same boundary rule as the primary path: blank the halves of
        // any wide-char pair this write splits instead of leaving
        // orphaned cells behind.
        let row = &mut self.cells[self.cursor_row];
        crate::style::sever_wide_pair(row, self.cursor_col);
        crate::style::sever_wide_pair(row, self.cursor_col + width);
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

    fn previous_cell_mut(&mut self) -> Option<&mut Cell> {
        let row = self.cells.get_mut(self.cursor_row)?;
        let end = if self.wrap_pending {
            self.cursor_col.saturating_add(1)
        } else {
            self.cursor_col
        }
        .min(row.len());
        row[..end].iter_mut().rfind(|cell| cell.ch != '\0')
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
        // Splitting a wide pair at the insertion point strands both
        // halves around the inserted blanks; blank them instead. A pair
        // pushed off the right edge always lands its base on the last
        // column, which render-time clipping already blanks.
        crate::style::sever_wide_pair(row, self.cursor_col);
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
        // Same boundary rule as primary DCH: the cursor can sit on a
        // continuation cell, and the cell just past the deleted span can
        // be a continuation whose base is deleted out from under it.
        crate::style::sever_wide_pair(row, self.cursor_col);
        crate::style::sever_wide_pair(row, self.cursor_col + count);
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
                // Same span as an EL 1 on the cursor row — reuse it so
                // the wide-pair boundary handling stays in one place.
                self.erase_line(1);
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
                let end = self.cursor_col.min(self.cols.saturating_sub(1)) + 1;
                // Erasing up to a wide base leaves its continuation just
                // past the span — blank it rather than orphan it.
                crate::style::sever_wide_pair(&mut self.cells[self.cursor_row], end);
                for col in 0..end {
                    self.cells[self.cursor_row][col] = self.fill.clone();
                }
            }
            2 => self.cells[self.cursor_row].fill(self.fill.clone()),
            _ => {
                // A cursor on a continuation cell splits its pair.
                crate::style::sever_wide_pair(&mut self.cells[self.cursor_row], self.cursor_col);
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
        // Either edge of the erased span can split a wide pair.
        crate::style::sever_wide_pair(&mut self.cells[self.cursor_row], self.cursor_col);
        crate::style::sever_wide_pair(&mut self.cells[self.cursor_row], end);
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

// Translates one GL byte through the DEC Special Graphics ("line
// drawing") character set — the table `ESC ( 0` / `ESC ) 0` designate.
// This is the table xterm (and the terminfo `acsc`-driven glyphs
// ncurses apps rely on for `ACS_*` constants) implements for the vt100
// "0" character set: box-drawing, math comparison signs, and the vt100
// control-picture symbols. Only called for chars in 0x60..=0x7e — the
// full standard table's range; everything below `` ` `` (including the
// digits and `_`) was never part of the vt100 mapping and always prints
// as itself, graphics mode or not.
//
// This is the table dialog/whiptail (and anything else built on old
// ncurses ACS borders) rely on: without it `smacs`/`rmacs` bytes swap
// the border in but the corner/edge glyphs render as the literal
// letters `l`, `q`, `k`, etc. instead of `┌`, `─`, `┐`.
fn dec_special_graphics(ch: char) -> char {
    match ch {
        '`' => '◆', // U+25C6 diamond
        'a' => '▒', // U+2592 checkerboard (medium shade)
        'b' => '␉', // U+2409 HT symbol
        'c' => '␌', // U+240C FF symbol
        'd' => '␍', // U+240D CR symbol
        'e' => '␊', // U+240A LF symbol
        'f' => '°', // U+00B0 degree
        'g' => '±', // U+00B1 plus/minus
        'h' => '␤', // U+2424 NL symbol
        'i' => '␋', // U+240B VT symbol
        'j' => '┘', // U+2518 lower-right corner
        'k' => '┐', // U+2510 upper-right corner
        'l' => '┌', // U+250C upper-left corner
        'm' => '└', // U+2514 lower-left corner
        'n' => '┼', // U+253C crossing lines
        'o' => '⎺', // U+23BA scan line 1 (top)
        'p' => '⎻', // U+23BB scan line 3
        'q' => '─', // U+2500 scan line 5 (horizontal line)
        'r' => '⎼', // U+23BC scan line 7
        's' => '⎽', // U+23BD scan line 9 (bottom)
        't' => '├', // U+251C left tee
        'u' => '┤', // U+2524 right tee
        'v' => '┴', // U+2534 bottom tee
        'w' => '┬', // U+252C top tee
        'x' => '│', // U+2502 vertical line
        'y' => '≤', // U+2264 less-than-or-equal
        'z' => '≥', // U+2265 greater-than-or-equal
        '{' => 'π', // U+03C0 pi
        '|' => '≠', // U+2260 not-equal
        '}' => '£', // U+00A3 UK pound
        '~' => '·', // U+00B7 centered dot
        other => other,
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

// Longest ESU we reassemble across a feed boundary. The bare form is
// `ESC [ ? 2 0 2 6 l` (9 bytes), but a host may close the region with a
// combined-parameter reset that names 2026 alongside other DEC modes
// (e.g. `ESC[?2026;25l`, resetting sync and cursor visibility at once).
// Keeping a `MAX_ESU_LEN - 1` overlap between feeds means such an ESU
// split across two `ingest_bytes` calls is still caught. Only a reset
// whose parameter list runs past this bound AND lands exactly on a feed
// boundary would be missed — no real terminal emits one.
const MAX_ESU_LEN: usize = 32;

// Hard cap on the synchronized-output buffer. We trust well-behaved
// hosts; a normal sync update is a few KB. Misbehaving ones (BSU
// without ESU, or runaway output between them) get observable frame
// tearing — we flush the buffer non-atomically and warn — instead of
// memory exhaustion.
const SYNCHRONIZED_BUFFER_MAX: usize = 1 << 20;

// Find the first End-of-Synchronized-Update reset in `haystack` and
// return its `(start, end)` byte range, or None if absent. An ESU is a
// DEC-private mode reset `ESC [ ? <params> l` whose parameter list
// includes `2026`. Matching by parameter — rather than against the
// literal `ESC[?2026l` — is what lets us recognize the combined-
// parameter forms a real terminal emits (`ESC[?2026;25l`,
// `ESC[?25;2026l`, …); the bare-literal scan this replaced froze the
// whole buffered frame until some later standalone ESU happened to
// arrive. A `?h` (set) or a reset without 2026 (e.g. a stray `ESC[?25l`
// hiding the cursor) is deliberately not treated as an ESU.
//
// Called on each feed; a partial ESU at the tail (no final byte yet)
// simply doesn't match and stays buffered for the next feed — see the
// `MAX_ESU_LEN` overlap the caller keeps.
fn find_esu(haystack: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i + 2 < haystack.len() {
        if &haystack[i..i + 3] != b"\x1b[?" {
            i += 1;
            continue;
        }
        // Consume the parameter bytes (digits and `;`) up to the final.
        let mut j = i + 3;
        while j < haystack.len() && (haystack[j].is_ascii_digit() || haystack[j] == b';') {
            j += 1;
        }
        if j < haystack.len()
            && haystack[j] == b'l'
            && haystack[i + 3..j]
                .split(|&b| b == b';')
                .any(|token| token == b"2026")
        {
            return Some((i, j + 1));
        }
        i += 1;
    }
    None
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
    let (params, url) = rest.split_once(';')?;
    // The URL is serialized back to the client terminal verbatim. Never
    // retain terminal controls (or accept them in the parameter field),
    // otherwise a child process could smuggle a fresh escape sequence into
    // zmux's rendered output through OSC 8.
    if params.chars().any(char::is_control) || url.chars().any(char::is_control) {
        return None;
    }
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

    use super::{CursorShape, TerminalIngest, parse_osc_hyperlink};

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
    fn scrolling_while_on_alternate_screen_renders_primary_history() {
        let mut pane = Pane::new("tui", 32, 3);
        let mut ingest = TerminalIngest::new(PtySize::new(3, 20));
        ingest.ingest_bytes(
            &mut pane,
            b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\n\x1b[?1049hALT",
        );
        assert!(ingest.render_lines(&pane).iter().any(|line| line == "ALT"));

        let outcome = pane.wheel_up(1);
        assert!(matches!(
            outcome,
            crate::pane::WheelOutcome::ViewportChanged {
                lines_scrolled: 1,
                follow_output: false
            }
        ));
        let history = ingest.render_lines(&pane);
        assert!(
            history.iter().any(|line| line.contains("four")),
            "alternate-screen scroll did not reveal primary history: {history:?}"
        );
        assert!(
            history.iter().all(|line| line != "ALT"),
            "alternate buffer remained visible after entering scrollback: {history:?}"
        );

        pane.scroll_to_bottom();
        assert!(ingest.render_lines(&pane).iter().any(|line| line == "ALT"));
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
    fn combining_marks_and_emoji_sequences_do_not_advance_the_cursor() {
        let mut pane = Pane::new("unicode", 16, 4);
        let mut ingest = TerminalIngest::new(PtySize::new(4, 40));
        let text = "e\u{0301} ✈\u{fe0f} 👩\u{1f3fd}\u{200d}💻X";

        ingest.ingest_bytes(&mut pane, text.as_bytes());

        assert_eq!(ingest.render_lines(&pane), vec![text]);
        assert_eq!(
            ingest.screen_cursor(&pane),
            Some((0, 8)),
            "cursor must count composed terminal cells, not Unicode scalars"
        );
    }

    #[test]
    fn alternate_screen_composes_combining_and_zwj_sequences_in_one_cell_run() {
        let mut pane = Pane::new("unicode-alt", 16, 4);
        let mut ingest = TerminalIngest::new(PtySize::new(4, 40));
        let text = "e\u{0301} 👩\u{1f3fd}\u{200d}💻X";

        ingest.ingest_bytes(&mut pane, b"\x1b[?1049h");
        ingest.ingest_bytes(&mut pane, text.as_bytes());

        assert_eq!(ingest.render_lines(&pane), vec![text]);
        assert_eq!(ingest.screen_cursor(&pane), Some((0, 5)));
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
    fn osc_8_hyperlinks_reject_control_characters() {
        for payload in [
            "8;;https://example.com/\u{1b}[31m",
            "8;id=unsafe\u{1b}[2J;https://example.com",
            "8;;https://example.com/\nnext",
        ] {
            assert!(
                parse_osc_hyperlink(payload).is_none(),
                "accepted unsafe hyperlink payload {payload:?}"
            );
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
    fn synchronized_output_closes_on_combined_parameter_esu() {
        // A host may end the region with a DEC-private reset that names
        // 2026 alongside other modes — e.g. `ESC[?2026;25l` (sync +
        // cursor visibility) — rather than the bare `ESC[?2026l`. The
        // old literal scan missed these and froze the buffered frame
        // until some later standalone ESU landed; the parameter-aware
        // scan must release it immediately.
        for esu in [
            &b"\x1b[?2026;25l"[..],
            &b"\x1b[?25;2026l"[..],
            &b"\x1b[?1;2026;12l"[..],
        ] {
            let mut pane = Pane::new("shell", 16, 4);
            let mut ingest = TerminalIngest::default();

            ingest.ingest_bytes(&mut pane, b"\x1b[?2026h");
            ingest.ingest_bytes(&mut pane, b"FRAME");
            assert!(
                pane.visible_text().iter().all(|l| l.is_empty()),
                "content must stay buffered until the ESU",
            );
            ingest.ingest_bytes(&mut pane, esu);
            ingest.flush_incomplete_line(&mut pane);
            assert_eq!(
                pane.visible_text()[0],
                "FRAME",
                "combined-parameter ESU {esu:?} must release the buffered frame",
            );
        }
    }

    #[test]
    fn synchronized_output_ignores_non_2026_private_reset() {
        // A `?l` reset that does NOT name 2026 (a bare cursor-hide, say)
        // is ordinary buffered content, not an ESU — it must not close
        // the region early.
        let mut pane = Pane::new("shell", 16, 4);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(&mut pane, b"\x1b[?2026h");
        ingest.ingest_bytes(&mut pane, b"AAA\x1b[?25lBBB");
        assert!(
            pane.visible_text().iter().all(|l| l.is_empty()),
            "an unrelated ?25l reset must not be mistaken for the ESU",
        );
        // The real ESU still closes it, and the buffered ?25l applied.
        ingest.ingest_bytes(&mut pane, b"\x1b[?2026l");
        ingest.flush_incomplete_line(&mut pane);
        assert_eq!(pane.visible_text()[0], "AAABBB");
    }

    #[test]
    fn synchronized_output_finds_combined_esu_split_across_feeds() {
        // The combined form is longer than the bare `ESC[?2026l`, so the
        // cross-feed overlap has to be wide enough (MAX_ESU_LEN) to
        // rejoin it when it straddles a feed boundary.
        let mut pane = Pane::new("shell", 16, 4);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(&mut pane, b"\x1b[?2026h");
        for &b in b"payload" {
            ingest.ingest_bytes(&mut pane, &[b]);
        }
        for &b in b"\x1b[?2026;25l" {
            ingest.ingest_bytes(&mut pane, &[b]);
        }
        ingest.flush_incomplete_line(&mut pane);
        assert_eq!(pane.visible_text()[0], "payload");
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
    fn dec_special_graphics_charset_draws_ncurses_acs_borders() {
        // `ESC ( 0` designates G0 as DEC Special Graphics (vt100 line
        // drawing); ncurses' ACS_* border glyphs are just ASCII letters
        // sent under that charset. xterm-256color's terminfo `smacs`/
        // `rmacs` are exactly `\E(0` / `\E(B`, so this is dialog/
        // whiptail's actual wire format for a box's top border. Before
        // charset tracking, `ESC ( 0` was swallowed with no effect and
        // this printed the literal letters `lqqqk`.
        let mut pane = Pane::new("shell", 16, 4);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(&mut pane, b"\x1b(0lqqqk\x1b(B");
        ingest.flush_incomplete_line(&mut pane);

        assert_eq!(ingest.render_lines(&pane), vec!["┌───┐"]);
    }

    #[test]
    fn charset_reverts_to_ascii_after_g0_is_redesignated_as_usascii() {
        // `ESC ( B` re-designates G0 as US-ASCII. GL bytes that fall in
        // the graphics-substitution range must print literally again
        // afterwards instead of staying translated.
        let mut pane = Pane::new("shell", 16, 4);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(&mut pane, b"\x1b(0q\x1b(Bq");
        ingest.flush_incomplete_line(&mut pane);

        assert_eq!(ingest.render_lines(&pane), vec!["─q"]);
    }

    #[test]
    fn so_si_switch_gl_between_g0_ascii_and_g1_graphics() {
        // Some ncurses builds designate line-drawing into G1 and flip
        // GL with SO (0x0E, shift out to G1) / SI (0x0F, shift back to
        // G0) instead of redesignating G0 directly. G0 stays ASCII the
        // whole time here; only the active slot moves.
        let mut pane = Pane::new("shell", 16, 4);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(&mut pane, b"\x1b)0a\x0eq\x0fq");
        ingest.flush_incomplete_line(&mut pane);

        // 'a' prints under the still-default G0/ASCII slot. SO moves GL
        // to G1 (graphics), so 'q' -> '─'. SI moves GL back to G0
        // (ASCII), so the final 'q' prints literally.
        assert_eq!(ingest.render_lines(&pane), vec!["a─q"]);
    }

    #[test]
    fn utf8_multibyte_chars_pass_through_untranslated_in_graphics_mode() {
        // DEC Special Graphics only remaps single-byte GL positions
        // 0x60..=0x7e. A UTF-8 multibyte char reassembled by the ground
        // state always decodes to >= U+0080, so it can never land in
        // that range — text interleaved with box-drawing must render
        // untouched.
        let mut pane = Pane::new("shell", 16, 4);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(&mut pane, "\x1b(0q你好q".as_bytes());
        ingest.flush_incomplete_line(&mut pane);

        assert_eq!(ingest.render_lines(&pane), vec!["─你好─"]);
    }

    #[test]
    fn decsc_decrc_save_and_restore_charset_state() {
        // Real DECSC saves charset state (G0/G1 + active slot) along
        // with the pen and cursor position. Enter graphics mode, save,
        // switch to ASCII and print (which also advances the cursor),
        // then restore: DECRC puts the cursor back at the save point
        // AND puts the charset back in graphics mode, so the restored
        // write overwrites the interim char with a translated glyph
        // instead of a literal letter.
        let mut pane = Pane::new("shell", 16, 4);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(&mut pane, b"\x1b(0\x1b7\x1b(Bq\x1b8q");
        ingest.flush_incomplete_line(&mut pane);

        assert_eq!(ingest.render_lines(&pane), vec!["─"]);
    }

    #[test]
    fn alt_screen_1049_roundtrip_preserves_primary_charset() {
        // 1049 does an implicit DECSC/DECRC bracketing the alt-screen
        // visit. A TUI that flips into a different charset inside the
        // alt screen must not leak that into the primary screen on
        // exit — the primary's own charset (set before entering alt)
        // must survive the round trip.
        let mut pane = Pane::new("shell", 16, 4);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(&mut pane, b"\x1b(0q\x1b[?1049h\x1b(Bq\x1b[?1049lq");
        ingest.flush_incomplete_line(&mut pane);

        // Primary: 'q' under graphics ('─') before entering alt screen.
        // Inside alt, `\x1b(B` and the following 'q' land on the alt
        // grid and never touch primary. After 1049l (implicit DECRC),
        // the primary charset from before 1049h — graphics — must be
        // active again, so the final 'q' also renders as '─'.
        assert_eq!(ingest.render_lines(&pane), vec!["──"]);
    }

    #[test]
    fn rep_repeats_the_translated_graphics_char() {
        // CSI `b` (REP) must repeat what was actually drawn. A
        // box-drawing run compressed with REP should repeat '─', not
        // the raw ASCII 'q' that was sent for it.
        let mut pane = Pane::new("shell", 16, 4);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(&mut pane, b"\x1b(0q\x1b[9b");
        ingest.flush_incomplete_line(&mut pane);

        assert_eq!(ingest.render_lines(&pane), vec!["─".repeat(10)]);
    }

    #[test]
    fn hard_reset_clears_charset_state() {
        // RIS (`ESC c`) must put G0/G1 back to ASCII and GL back to G0,
        // so graphics mode left dangling by a killed app doesn't poison
        // whatever runs next in the same pane.
        let mut pane = Pane::new("shell", 16, 4);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(&mut pane, b"\x1b(0\x1bcq");
        ingest.flush_incomplete_line(&mut pane);

        assert_eq!(ingest.render_lines(&pane), vec!["q"]);
    }

    #[test]
    fn soft_reset_clears_charset_state() {
        // DECSTR (`CSI ! p`) resets charset state the same way RIS does.
        let mut pane = Pane::new("shell", 16, 4);
        let mut ingest = TerminalIngest::default();

        ingest.ingest_bytes(&mut pane, b"\x1b(0\x1b[!pq");
        ingest.flush_incomplete_line(&mut pane);

        assert_eq!(ingest.render_lines(&pane), vec!["q"]);
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
        assert_eq!(
            pane.total_lines(),
            0,
            "a region below row 0 must remain an in-place TUI operation"
        );
    }

    #[test]
    fn top_anchored_primary_scroll_region_preserves_codex_history() {
        let mut pane = Pane::new("codex", 32, 6);
        let mut ingest = TerminalIngest::new(PtySize::new(6, 32));

        ingest.ingest_bytes(&mut pane, b"one\ntwo\nthree\nfour\nfive\nsix");
        // Codex keeps its composer in the bottom rows and scrolls the
        // transcript through a top-anchored partial DECSTBM region.
        ingest.ingest_bytes(&mut pane, b"\x1b[1;4r\x1b[4;1H\n");

        assert_eq!(
            pane.scrollback_text(8, true),
            vec!["one"],
            "the transcript row leaving a top-anchored region was discarded"
        );
        assert!(matches!(
            pane.wheel_up(1),
            crate::pane::WheelOutcome::ViewportChanged {
                lines_scrolled: 1,
                follow_output: false
            }
        ));
        assert_eq!(
            ingest.render_lines(&pane).first().map(String::as_str),
            Some("one"),
            "the preserved Codex transcript must become visible on scroll-up"
        );
    }

    #[test]
    fn codex_su_scrolls_partial_region_and_commits_transcript_rows() {
        let mut pane = Pane::new("codex", 32, 6);
        let mut ingest = TerminalIngest::new(PtySize::new(6, 32));

        ingest.ingest_bytes(&mut pane, b"one\ntwo\nthree\nfour\nfive\nsix");
        // Trace-derived Codex shape: reserve the bottom two composer rows,
        // then use SU rather than LF/IND to advance two transcript rows.
        ingest.ingest_bytes(&mut pane, b"\x1b[1;4r\x1b[2S");

        assert_eq!(
            pane.scrollback_text(8, true),
            vec!["one", "two"],
            "CSI S rows leaving a top-anchored region must enter history"
        );
        assert_eq!(
            ingest.render_lines(&pane),
            vec!["three", "four", "", "", "five", "six"],
            "CSI S must shift only the configured transcript region"
        );
    }

    #[test]
    fn primary_su_clears_delayed_wrap_before_the_next_graphic() {
        let mut pane = Pane::new("codex", 32, 4);
        let mut ingest = TerminalIngest::new(PtySize::new(4, 4));

        // Filling the rightmost column arms delayed autowrap. SU keeps the
        // cursor in place but, like every editing operation, must cancel
        // that pending wrap so X overwrites the current cell instead of
        // triggering an extra linefeed/scroll first.
        ingest.ingest_bytes(&mut pane, b"abcd\x1b[SX");
        let rendered = ingest.render_lines(&pane);
        assert_eq!(rendered.first().map(String::as_str), Some("   X"));
    }

    #[test]
    fn primary_sd_shifts_only_the_configured_region() {
        let mut pane = Pane::new("app", 32, 4);
        let mut ingest = TerminalIngest::new(PtySize::new(4, 16));
        ingest.ingest_bytes(&mut pane, b"top\none\ntwo\nbottom\x1b[2;3r\x1b[T");

        assert_eq!(ingest.render_lines(&pane), vec!["top", "", "one", "bottom"]);
        assert_eq!(pane.total_lines(), 0, "SD must not synthesize history");
    }

    #[test]
    fn private_and_intermediate_su_forms_are_ignored() {
        let mut pane = Pane::new("app", 32, 4);
        let mut ingest = TerminalIngest::new(PtySize::new(4, 16));
        ingest.ingest_bytes(&mut pane, b"one\ntwo");
        let before = ingest.render_lines(&pane);

        ingest.ingest_bytes(&mut pane, b"\x1b[<S\x1b[ S\x1b[1;2S");
        assert_eq!(ingest.render_lines(&pane), before);
    }

    #[test]
    fn su_and_sd_shift_alternate_scroll_region_without_moving_cursor() {
        let mut pane = Pane::new("app", 16, 5);
        let mut ingest = TerminalIngest::new(PtySize::new(5, 16));

        ingest.ingest_bytes(
            &mut pane,
            b"\x1b[?1049h\x1b[Htop\x1b[2;1Hone\x1b[3;1Htwo\x1b[4;1Hthree\x1b[5;1Hbottom\x1b[2;4r\x1b[3;7H\x1b[S",
        );
        assert_eq!(ingest.screen_cursor(&pane), Some((2, 6)));
        assert_eq!(
            ingest.render_lines(&pane),
            vec!["top", "two", "three", "", "bottom"]
        );

        ingest.ingest_bytes(&mut pane, b"\x1b[T");
        assert_eq!(ingest.screen_cursor(&pane), Some((2, 6)));
        assert_eq!(
            ingest.render_lines(&pane),
            vec!["top", "", "two", "three", "bottom"]
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
    fn apc_pm_sos_strings_do_not_leak_bytes_into_the_screen() {
        // Kitty's graphics protocol opens an APC (`ESC _`) with a
        // base64-encoded image payload and closes it with ST (`ESC \`);
        // notcurses and chafa probe capabilities the same way. PM
        // (`ESC ^`) and SOS (`ESC X`) are the same shape. Before routing
        // all three into the DCS-style swallow state, they fell through
        // to Ground and the payload printed as literal garbage. Mirrors
        // `dcs_and_charset_sequences_do_not_leak_bytes_into_the_screen`
        // above.
        let mut pane = Pane::new("shell", 32, 5);
        let mut ingest = TerminalIngest::new(PtySize::new(5, 32));

        ingest.ingest_bytes(&mut pane, b"\x1b_Gf=100,a=T;aGVsbG8gd29ybGQ=\x1b\\ok");
        assert_eq!(
            ingest.render_lines(&pane)[0],
            "ok",
            "APC payload must not print, and parsing must resume normally after ST",
        );

        let mut pane = Pane::new("shell", 32, 5);
        let mut ingest = TerminalIngest::new(PtySize::new(5, 32));
        ingest.ingest_bytes(&mut pane, b"\x1b^privacy message\x1b\\pm");
        assert_eq!(ingest.render_lines(&pane)[0], "pm");

        let mut pane = Pane::new("shell", 32, 5);
        let mut ingest = TerminalIngest::new(PtySize::new(5, 32));
        ingest.ingest_bytes(&mut pane, b"\x1bXsos body\x1b\\sos");
        assert_eq!(ingest.render_lines(&pane)[0], "sos");
    }

    #[test]
    fn escape_hash_sequences_swallow_their_argument_byte() {
        // `ESC #` (DECDHL/DECSWL/DECDWL/DECALN) takes exactly one more
        // byte. Before EscapeHash existed, `#` fell through to the
        // wildcard arm straight to Ground, so the following byte
        // printed as a stray character instead of being consumed as
        // part of the sequence. DECSWL (`ESC # 5`) stays unimplemented,
        // making it the pure-swallow probe; DECALN (`ESC # 8`) now has
        // real behavior covered in tests/vt_render_regressions.rs.
        let mut pane = Pane::new("shell", 16, 4);
        let mut ingest = TerminalIngest::new(PtySize::new(4, 16));

        ingest.ingest_bytes(&mut pane, b"\x1b#5ok");

        assert_eq!(
            ingest.render_lines(&pane)[0],
            "ok",
            "the '5' argument byte of ESC # 5 must not print",
        );
    }

    #[test]
    fn kitty_keyboard_protocol_csi_u_does_not_restore_cursor_on_alt_screen() {
        // Kitty keyboard protocol push/pop/query (`CSI > Ps u`,
        // `CSI < u`, `CSI ? u`) share the `u` final byte with DECRC
        // (`CSI u`). Claude Code probes these at startup. Before the
        // private-marker guard, they fell through to
        // restore_cursor_state and teleported the cursor to the alt
        // screen's saved position (which defaults to (0, 0) any time
        // nothing has explicitly saved it, e.g. right after 1049 entry)
        // — corrupting an in-progress redraw.
        let mut pane = Pane::new("shell", 40, 10);
        let mut ingest = TerminalIngest::new(PtySize::new(10, 40));

        // Enter the alt screen and move well away from (0, 0), which is
        // where the (unset) saved cursor still points.
        ingest.ingest_bytes(&mut pane, b"\x1b[?1049h\x1b[5;10H");
        assert_eq!(ingest.screen_cursor(&pane), Some((4, 9)));

        ingest.ingest_bytes(&mut pane, b"\x1b[>1u");
        assert_eq!(
            ingest.screen_cursor(&pane),
            Some((4, 9)),
            "CSI > 1 u (kitty push) must not restore the cursor",
        );

        ingest.ingest_bytes(&mut pane, b"\x1b[<u");
        assert_eq!(
            ingest.screen_cursor(&pane),
            Some((4, 9)),
            "CSI < u (kitty pop) must not restore the cursor",
        );

        ingest.ingest_bytes(&mut pane, b"\x1b[?u");
        assert_eq!(
            ingest.screen_cursor(&pane),
            Some((4, 9)),
            "CSI ? u (kitty query) must not restore the cursor",
        );

        // A following redraw write lands exactly where it was left,
        // confirming the parser's own state wasn't corrupted either.
        ingest.ingest_bytes(&mut pane, b"X");
        let cells = ingest.render_cells(&pane);
        assert_eq!(cells[4][9].ch, 'X');

        // Plain DECRC (no private marker) still works.
        ingest.ingest_bytes(&mut pane, b"\x1b[u");
        assert_eq!(
            ingest.screen_cursor(&pane),
            Some((0, 0)),
            "unmarked CSI u must still restore the (default) saved cursor",
        );
    }

    #[test]
    fn modify_other_keys_csi_m_does_not_touch_sgr_pen() {
        // xterm modifyOtherKeys query/set (`CSI > 4 m`, `CSI > 4 ; 2 m`)
        // share the `m` final byte with SGR. Before the private-marker
        // guard, `CSI > 4 ; 2 m` fell through to the SGR arm and applied
        // SGR 4 (underline) to every subsequent character — a live
        // source of "everything underlined" bug reports.
        let mut pane = Pane::new("shell", 40, 4);
        let mut ingest = TerminalIngest::new(PtySize::new(4, 40));

        ingest.ingest_bytes(&mut pane, b"\x1b[>4;2ma");
        let cells = ingest.render_cells(&pane);
        assert!(
            !cells[0][0].style.attrs.underline,
            "CSI > 4 ; 2 m must not touch the SGR pen",
        );

        ingest.ingest_bytes(&mut pane, b"\x1b[>4mb");
        let cells = ingest.render_cells(&pane);
        assert!(
            !cells[0][1].style.attrs.underline,
            "CSI > 4 m must not touch the SGR pen",
        );

        // Plain SGR (no private marker) still works.
        ingest.ingest_bytes(&mut pane, b"\x1b[4mc");
        let cells = ingest.render_cells(&pane);
        assert!(
            cells[0][2].style.attrs.underline,
            "plain CSI 4 m must still enable underline",
        );
    }

    #[test]
    fn vt_and_ff_act_as_newline() {
        // VT (0x0B) and FF (0x0C) have no glyph; real terminals treat
        // both as a linefeed rather than dropping them. Before this
        // fix, handle_ground had no arm for either byte and both were
        // silently swallowed with no cursor movement at all — a
        // `printf 'one\x0btwo'` stream would land as "onetwo" glued on
        // one line. Routed into the exact same handler as `\n`, so on
        // the primary screen they inherit `\n`'s existing CR+IND
        // behavior here (see `primary_linefeed`).
        let mut pane = Pane::new("shell", 16, 4);
        let mut ingest = TerminalIngest::new(PtySize::new(4, 16));

        ingest.ingest_bytes(&mut pane, b"one\x0btwo");
        let rendered = ingest.render_lines(&pane);
        assert_eq!(rendered[0], "one");
        assert_eq!(rendered[1], "two", "VT (0x0B) must drop to the next line");

        let mut pane = Pane::new("shell", 16, 4);
        let mut ingest = TerminalIngest::new(PtySize::new(4, 16));

        ingest.ingest_bytes(&mut pane, b"abc\x0cxyz");
        let rendered = ingest.render_lines(&pane);
        assert_eq!(rendered[0], "abc");
        assert_eq!(rendered[1], "xyz", "FF (0x0C) must drop to the next line");
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
    fn alt_screen_reentry_does_not_leak_previous_session_fill_color() {
        // AlternateScreen::fill mirrors the running SGR pen (see its own
        // doc comment: "Updated by the ingest whenever current_style
        // changes") so erase/scroll/insert/delete ops paint blanks with
        // the active background (BCE). But AlternateScreen::reset() —
        // run on every 47/1047/1049 mode-entry — used to leave `fill`
        // untouched, so a background color left over from whatever last
        // touched it (e.g. a red status bar a PRIOR alt-screen occupant
        // painted) bled into the blanks the NEXT occupant paints, even
        // before that occupant ever touches SGR itself.
        //
        // Reproducing that divergence with CSI bytes alone is not
        // possible in this codebase: every other place current_style
        // changes (the SGR handler, hard_reset, soft_reset, and
        // restore_cursor_state's unconditional trailing resync) already
        // keeps `fill` in lockstep with it, which is exactly why the
        // mode-entry call site was the one gap worth fixing. So this
        // test pokes `alternate`'s private fields directly to stand in
        // for "whatever state a previous occupant left the alt screen
        // in" — the same state reset() sees when it runs — and then
        // drives mode entry, erase, and the assertion through the
        // normal ingest_bytes/render_cells path.
        let mut pane = Pane::new("shell", 16, 4);
        let mut ingest = TerminalIngest::default();

        // Simulate a previous alt-screen occupant leaving `fill` red
        // while the pen actually running in this session (current_style)
        // is, and always has been, default — i.e. app #2 hasn't touched
        // SGR at all yet.
        ingest.alternate.set_fill_style(crate::style::Style {
            bg: crate::style::Color::Indexed(1),
            ..crate::style::Style::DEFAULT
        });

        // App #2 enters the alt screen and erases before emitting any
        // SGR of its own.
        ingest.ingest_bytes(&mut pane, b"\x1b[?1049h\x1b[2J");

        let cells = ingest.render_cells(&pane);
        for cell in &cells[0] {
            assert_eq!(
                cell.style.bg,
                crate::style::Color::Default,
                "blank cells must carry the current (default) pen's background, not a previous occupant's leftover red"
            );
        }

        // BCE-correct case: leave, then simulate a green pen already
        // running at the moment of re-entry (again poked directly, since
        // the SGR handler's own resync would otherwise mask the
        // mode-entry gap) and erase. Blanks must pick up the CURRENTLY
        // ACTIVE pen, not whatever neutral baseline reset() leaves fill
        // at.
        ingest.ingest_bytes(&mut pane, b"\x1b[?1049l");
        ingest.current_style = crate::style::Style {
            bg: crate::style::Color::Indexed(2),
            ..crate::style::Style::DEFAULT
        };

        ingest.ingest_bytes(&mut pane, b"\x1b[?1049h\x1b[2J");

        let cells = ingest.render_cells(&pane);
        for cell in &cells[0] {
            assert_eq!(
                cell.style.bg,
                crate::style::Color::Indexed(2),
                "blank cells must carry the currently active green background"
            );
        }
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

    // ESC D (IND) and ESC M (RI) used to be alt-screen-only — silent
    // no-ops on the primary grid — and ESC E (NEL) wasn't wired up at
    // all. The tests below pin the primary-screen behavior to match
    // xterm, reusing the same scroll machinery DECSTBM/linefeed already
    // exercise rather than a parallel implementation.

    #[test]
    fn ind_and_ri_move_the_cursor_without_scrolling_mid_screen() {
        // Neither escape should touch content or column when the cursor
        // isn't sitting on a scroll-region margin — this is the case a
        // naive "always scroll" implementation gets wrong.
        let mut pane = Pane::new("shell", 32, 5);
        let mut ingest = TerminalIngest::new(PtySize::new(5, 32));

        ingest.ingest_bytes(&mut pane, b"r0\nr1\nr2\nr3\nr4");
        // Park at row 3 (1-based), col 3 — nowhere near either margin of
        // the default full-screen region (rows 0..4).
        let reply = ingest.ingest_bytes(&mut pane, b"\x1b[3;3H\x1bD\x1b[6n");
        assert_eq!(
            reply, b"\x1b[4;3R",
            "IND should move down one row, column held"
        );

        let reply = ingest.ingest_bytes(&mut pane, b"\x1bM\x1b[6n");
        assert_eq!(reply, b"\x1b[3;3R", "RI should undo the IND exactly");

        assert_eq!(
            ingest.render_lines(&pane),
            vec!["r0", "r1", "r2", "r3", "r4"],
            "mid-screen IND/RI must not scroll or mutate content",
        );
    }

    #[test]
    fn ri_at_viewport_top_pushes_content_down_with_no_scroll_region_set() {
        // No DECSTBM in play, so the "region" is the whole viewport.
        // RI at row 0 must scroll the whole screen down: a blank line
        // appears at the top and the bottom line is discarded (RI never
        // feeds a scrollback line — that only happens on forward scroll).
        let mut pane = Pane::new("shell", 32, 4);
        let mut ingest = TerminalIngest::new(PtySize::new(4, 32));

        ingest.ingest_bytes(&mut pane, b"a\nb\nc\nd");
        ingest.ingest_bytes(&mut pane, b"\x1b[1;1H\x1bM");

        assert_eq!(
            ingest.render_lines(&pane),
            vec!["", "a", "b", "c"],
            "RI at top must push rows down and drop the bottom row",
        );
        assert_eq!(
            pane.total_lines(),
            0,
            "reverse scroll must not feed anything to scrollback",
        );
    }

    #[test]
    fn ri_at_top_of_a_decstbm_region_scrolls_only_the_region() {
        // Mirror of `primary_scroll_region_limits_linefeed_scrolling`
        // but for RI: rows outside the [top, bottom] band must be
        // untouched, and the discarded line is the region's bottom row,
        // not the viewport's.
        let mut pane = Pane::new("shell", 32, 4);
        let mut ingest = TerminalIngest::new(PtySize::new(4, 32));

        ingest.ingest_bytes(&mut pane, b"top\none\ntwo\nbottom");
        // Region rows 2..3 (1-based) covers "one"/"two". DECSTBM parks
        // the cursor at (0,0), so explicitly return to the region's top
        // row before sending RI.
        ingest.ingest_bytes(&mut pane, b"\x1b[2;3r\x1b[2;1H\x1bM");

        let rendered = ingest.render_lines(&pane);
        assert_eq!(rendered[0], "top", "row above region changed: {rendered:?}");
        assert_eq!(rendered[1], "", "region top must go blank: {rendered:?}");
        assert_eq!(
            rendered[2], "one",
            "region content shifted down: {rendered:?}"
        );
        assert_eq!(
            rendered[3], "bottom",
            "row below region changed: {rendered:?}"
        );
    }

    #[test]
    fn ind_at_bottom_of_a_decstbm_region_scrolls_only_the_region_and_holds_column() {
        let mut pane = Pane::new("shell", 32, 4);
        let mut ingest = TerminalIngest::new(PtySize::new(4, 32));

        ingest.ingest_bytes(&mut pane, b"top\none\ntwo\nbottom");
        // Region rows 2..3 (1-based) covers "one"/"two". Land on the
        // region's bottom row at column 2 (not 0) — IND must scroll
        // the region up in place without resetting the column, unlike
        // linefeed/NEL which fold in a carriage return.
        let reply = ingest.ingest_bytes(&mut pane, b"\x1b[2;3r\x1b[3;2H\x1bD\x1b[6n");
        assert_eq!(
            reply, b"\x1b[3;2R",
            "IND at the region margin must hold column"
        );

        let rendered = ingest.render_lines(&pane);
        assert_eq!(rendered[0], "top", "row above region changed: {rendered:?}");
        assert_eq!(
            rendered[1], "two",
            "region content shifted up: {rendered:?}"
        );
        assert_eq!(rendered[2], "", "region bottom must go blank: {rendered:?}");
        assert_eq!(
            rendered[3], "bottom",
            "row below region changed: {rendered:?}"
        );
    }

    #[test]
    fn nel_is_carriage_return_plus_index() {
        let mut pane = Pane::new("shell", 32, 5);
        let mut ingest = TerminalIngest::new(PtySize::new(5, 32));

        ingest.ingest_bytes(&mut pane, b"aa\nbb\ncc\ndd\nee");
        // Park mid-row (row 2, col 3) — NEL must both drop to column 0
        // AND advance a row, same as a real carriage-return + IND pair.
        let reply = ingest.ingest_bytes(&mut pane, b"\x1b[2;3H\x1bE\x1b[6n");
        assert_eq!(
            reply, b"\x1b[3;1R",
            "NEL must CR to col 1 and advance one row"
        );

        assert_eq!(
            ingest.render_lines(&pane),
            vec!["aa", "bb", "cc", "dd", "ee"],
            "NEL away from any margin must not mutate content",
        );
    }

    #[test]
    fn screen_cursor_tracks_dectcem_and_viewport_mapping() {
        // DECTCEM (`CSI ?25 h/l`) drives whether the attached client
        // paints a host cursor at the pane's cursor cell. Agent TUIs
        // toggle it constantly (hide during a redraw burst, show at
        // rest); ignoring it would leave the client's cursor glued on
        // while claude repaints its input box.
        let mut pane = Pane::new("shell", 64, 4);
        let mut ingest = TerminalIngest::new(PtySize::new(4, 32));

        ingest.ingest_bytes(&mut pane, b"ab");
        assert_eq!(
            ingest.screen_cursor(&pane),
            Some((0, 2)),
            "cursor defaults to visible, after the printed text",
        );

        ingest.ingest_bytes(&mut pane, b"\x1b[?25l");
        assert_eq!(ingest.screen_cursor(&pane), None, "?25l hides the cursor");

        ingest.ingest_bytes(&mut pane, b"\x1b[?25hc");
        assert_eq!(ingest.screen_cursor(&pane), Some((0, 3)));

        // When the primary grid outgrows the viewport, the reported
        // position must be viewport-relative (the client paints the
        // grid's last `viewport` rows), not grid-absolute.
        ingest.ingest_bytes(&mut pane, b"\r\n1\r\n2\r\n3\r\n4\r\n5\r\nx");
        let (row, col) = ingest.screen_cursor(&pane).expect("visible");
        assert_eq!(
            (row, col),
            (3, 1),
            "cursor lands on the last viewport row after scrolling",
        );

        // Alt screen reports its own grid position.
        ingest.ingest_bytes(&mut pane, b"\x1b[?1049h\x1b[2;5H");
        assert_eq!(ingest.screen_cursor(&pane), Some((1, 4)));

        // RIS restores visibility along with everything else.
        ingest.ingest_bytes(&mut pane, b"\x1b[?25l\x1bc");
        assert_eq!(ingest.screen_cursor(&pane), Some((0, 0)));
    }

    #[test]
    fn alt_screen_ri_above_the_region_top_does_not_underflow() {
        // DECSTBM parks the cursor at (0, 0), so a region whose top
        // margin isn't row 0 leaves the cursor sitting ABOVE
        // scroll_top. RI from there takes the "not at the margin"
        // branch, and a bare `cursor_row -= 1` underflows: panic in
        // debug builds, a usize wrap (and out-of-bounds row index on
        // the next write) in release. The cursor must clamp at row 0
        // like every other vertical move.
        let mut pane = Pane::new("shell", 32, 8);
        let mut ingest = TerminalIngest::new(PtySize::new(8, 32));

        ingest.ingest_bytes(&mut pane, b"\x1b[?1049h\x1b[3;6r\x1bM\x1bMx");

        let cells = ingest.render_cells(&pane);
        assert_eq!(
            cells[0][0].ch, 'x',
            "RI above the region top clamps at row 0",
        );
    }

    #[test]
    fn ht_advances_to_the_next_8_column_stop() {
        // The bug this fixes: `printf 'A\tB\tC\n'` used to render B at
        // column 5 (write 4 spaces after 'A' at col 0) instead of
        // xterm's real behavior — a stop every 8 columns.
        let mut pane = Pane::new("shell", 64, 4);
        let mut ingest = TerminalIngest::new(PtySize::new(4, 32));

        ingest.ingest_bytes(&mut pane, b"A\tB\tC");

        let cells = ingest.render_cells(&pane);
        assert_eq!(cells[0][0].ch, 'A');
        assert_eq!(cells[0][8].ch, 'B', "B must land on column 8, not 5");
        assert_eq!(cells[0][16].ch, 'C', "C must land on column 16, not 10");
    }

    #[test]
    fn ht_does_not_erase_or_restyle_the_cells_it_jumps_over() {
        // Real HT is a cursor move, not a fill. Write red "XY", jump
        // back to col 0, switch the pen to green, then tab over "XY" —
        // the cells must survive completely untouched: same chars, same
        // (red) style, and the row must not even grow (HT writes
        // nothing, not even blanks, into the cells it skips).
        let mut pane = Pane::new("shell", 64, 4);
        let mut ingest = TerminalIngest::new(PtySize::new(4, 32));

        ingest.ingest_bytes(&mut pane, b"\x1b[31mXY\x1b[0m\x1b[1G\x1b[32m\t");

        let cells = ingest.render_cells(&pane);
        assert_eq!(
            cells[0].len(),
            2,
            "HT must not materialize blank cells under the skipped span"
        );
        assert_eq!(cells[0][0].ch, 'X');
        assert_eq!(cells[0][1].ch, 'Y');
        assert_eq!(
            cells[0][0].style.fg,
            Color::Indexed(1),
            "the pen active when 'X' was written must survive the tab"
        );
        assert_eq!(cells[0][1].style.fg, Color::Indexed(1));
        assert_eq!(
            ingest.screen_cursor(&pane),
            Some((0, 8)),
            "cursor still lands on the tab stop past the untouched text"
        );
    }

    #[test]
    fn hts_plants_a_custom_stop() {
        // `ESC H` at column 5 adds a stop there; a tab from column 0
        // must stop at 5 instead of sailing past it to the default
        // column-8 stop.
        let mut pane = Pane::new("shell", 64, 4);
        let mut ingest = TerminalIngest::new(PtySize::new(4, 32));

        ingest.ingest_bytes(&mut pane, b"\x1b[6G\x1bH\x1b[1G\t");

        assert_eq!(ingest.screen_cursor(&pane), Some((0, 5)));

        // HTS adds to the defaults rather than replacing them — the
        // stop at column 8 is still there afterward.
        ingest.ingest_bytes(&mut pane, b"\t");
        assert_eq!(ingest.screen_cursor(&pane), Some((0, 8)));
    }

    #[test]
    fn tbc_clears_a_default_stop_and_tab_clear_all_leaves_none() {
        // `CSI g` (Ps 0) clears the stop under the cursor — even an
        // implicit default one, not just an HTS-added custom one; that
        // first clear also materializes the explicit table (see
        // `clear_tab_stop`). `CSI 3 g` then empties it completely:
        // xterm's documented behavior is that after a clear-all there
        // are NO stops left at all (not even the defaults), so HT runs
        // all the way to the last column.
        let mut pane = Pane::new("shell", 64, 4);
        let mut ingest = TerminalIngest::new(PtySize::new(4, 32));

        // Clear the default stop at column 8, then tab from column 0:
        // the next surviving default stop is column 16.
        ingest.ingest_bytes(&mut pane, b"\x1b[9G\x1b[g\x1b[1G\t");
        assert_eq!(
            ingest.screen_cursor(&pane),
            Some((0, 16)),
            "clearing the column-8 stop must not disturb column 16"
        );

        // Clear-all: from column 0, HT now has nothing to aim for but
        // the last column (31 in a 32-column terminal).
        ingest.ingest_bytes(&mut pane, b"\x1b[1G\x1b[3g\t");
        assert_eq!(
            ingest.screen_cursor(&pane),
            Some((0, 31)),
            "CSI 3 g leaves no stops at all, so HT goes to the last column"
        );
    }

    #[test]
    fn cbt_moves_backward_to_the_previous_stop() {
        // CBT (`CSI Z`) from column 20 must land on column 16, the
        // nearest default stop strictly to its left.
        let mut pane = Pane::new("shell", 64, 4);
        let mut ingest = TerminalIngest::new(PtySize::new(4, 40));

        ingest.ingest_bytes(&mut pane, b"\x1b[21G\x1b[Z");

        assert_eq!(ingest.screen_cursor(&pane), Some((0, 16)));
    }

    #[test]
    fn tab_stops_work_on_the_alternate_screen() {
        // HT/HTS use a cursor model entirely separate from the primary
        // screen's (`alternate.cursor_col` vs. `primary_cursor_col`);
        // this exercises the same HTS-then-HT round trip as
        // `hts_plants_a_custom_stop` but with the alt screen active to
        // make sure both paths are wired up.
        let mut pane = Pane::new("shell", 64, 4);
        let mut ingest = TerminalIngest::new(PtySize::new(4, 32));

        ingest.ingest_bytes(&mut pane, b"\x1b[?1049h\x1b[6G\x1bH\x1b[1G\t");
        assert_eq!(pane.screen_mode(), ScreenMode::Alternate);
        assert_eq!(ingest.screen_cursor(&pane), Some((0, 5)));

        // The HTS-added stop augments the defaults; the next HT lands
        // on the still-present column-8 default stop.
        ingest.ingest_bytes(&mut pane, b"\t");
        assert_eq!(ingest.screen_cursor(&pane), Some((0, 8)));
    }

    #[test]
    fn ht_at_the_last_column_clamps_without_wrapping() {
        // HT with nowhere left to go must park at the last column and
        // leave the pending-wrap flag clear — same contract as CUF/CHA
        // at the right edge. A print right after should land ON that
        // column, not wrap to a fresh row (which would mean HT left
        // stale wrap-pending state behind).
        let mut pane = Pane::new("shell", 64, 4);
        let mut ingest = TerminalIngest::new(PtySize::new(4, 10));

        ingest.ingest_bytes(&mut pane, b"\x1b[10G\tZ");

        // render_visible_cells trims the trailing blank viewport rows
        // that render_cells pads in — real wrapping would have produced
        // a second row with actual content, which trimming wouldn't
        // remove.
        let cells = ingest.render_visible_cells(&pane);
        assert_eq!(
            cells.len(),
            1,
            "HT then print at the last column must not wrap to a new row"
        );
        assert_eq!(cells[0][9].ch, 'Z');
    }

    #[test]
    fn resize_drops_custom_tab_stops_that_no_longer_fit() {
        // "xterm keeps custom stops" across a resize, but a stop past
        // the new right edge is meaningless and must not resurrect if
        // the terminal widens again later.
        let mut pane = Pane::new("shell", 64, 4);
        let mut ingest = TerminalIngest::new(PtySize::new(4, 40));

        // HTS at column 5 and column 30. The first HTS call materializes
        // the explicit table by seeding it with the (then-current)
        // 40-column defaults — 8, 16, 24, 32 — plus 5; the second adds
        // 30. So going into the resize the full set is
        // {5, 8, 16, 24, 30, 32}.
        ingest.ingest_bytes(&mut pane, b"\x1b[6G\x1bH\x1b[31G\x1bH\x1b[1G");

        ingest.resize(PtySize::new(4, 10));

        // Columns 5 and 8 both fit inside the shrunk 10-column width and
        // survive; 16/24/30/32 don't. Three tabs from column 0: land on
        // 5, then 8, then (nothing left) the last column, 9.
        ingest.ingest_bytes(&mut pane, b"\t");
        assert_eq!(ingest.screen_cursor(&pane), Some((0, 5)));
        ingest.ingest_bytes(&mut pane, b"\t");
        assert_eq!(
            ingest.screen_cursor(&pane),
            Some((0, 8)),
            "the default stop at column 8 also survives the shrink"
        );
        ingest.ingest_bytes(&mut pane, b"\t");
        assert_eq!(
            ingest.screen_cursor(&pane),
            Some((0, 9)),
            "columns 16/24/30/32 fell outside the shrunk width and must not survive"
        );
    }

    #[test]
    fn primary_columns_survive_a_temporary_shrink_without_wide_glyph_artifacts() {
        let mut pane = Pane::new("shell", 64, 2);
        let mut ingest = TerminalIngest::new(PtySize::new(2, 4));
        ingest.ingest_bytes(&mut pane, "a你b".as_bytes());

        ingest.resize(PtySize::new(2, 2));
        assert_eq!(
            ingest.render_lines(&pane),
            vec!["a"],
            "a clipped wide glyph must not render without its continuation cell"
        );

        ingest.resize(PtySize::new(2, 4));
        assert_eq!(
            ingest.render_lines(&pane),
            vec!["a你b"],
            "widening should reveal primary cells preserved beyond the temporary edge"
        );
    }

    // --- Mouse-wheel scroll-jump fix -----------------------------------
    //
    // Regression coverage for the combined-timeline scroll fix. Before
    // this fix, `render_primary_cells`'s non-follow branch rendered
    // `pane.visible_lines()` — scrollback only — while follow-mode
    // viewport bookkeeping only ever accounted for committed scrollback
    // lines, never the live grid sitting past the end of it. The first
    // wheel-up notch out of follow mode therefore always landed a whole
    // grid's worth of lines above where the user was actually looking,
    // even though every subsequent notch (already scrolled away from
    // follow, no live grid involved) scrolled smoothly. These tests
    // drive content through the real ingest path — never hand-appended
    // scrollback — because that's exactly the code path the bug lived
    // in.
    //
    // All four tests share one setup: a 4-row PTY / 4-row viewport pane
    // fed ten single-character-wider lines ("L1".."L10") with no
    // trailing newline. By the end, six lines have evicted to
    // scrollback (L1..L6) and the live grid holds the last four
    // (L7..L10) — a full screen still live, exactly the shape that
    // exposes the bug.

    fn wheel_jump_fixture() -> (Pane, TerminalIngest) {
        let mut pane = Pane::new("shell", 64, 4);
        let mut ingest = TerminalIngest::new(PtySize::new(4, 80));
        ingest.ingest_bytes(&mut pane, b"L1\nL2\nL3\nL4\nL5\nL6\nL7\nL8\nL9\nL10");
        (pane, ingest)
    }

    #[test]
    fn wheel_up_from_follow_scrolls_by_the_requested_amount_not_a_full_screen() {
        let (mut pane, ingest) = wheel_jump_fixture();

        // Sanity: following shows the live grid's bottom four rows.
        assert!(pane.follow_output());
        assert_eq!(ingest.render_lines(&pane), vec!["L7", "L8", "L9", "L10"]);

        // One explicit three-line request must move the
        // top visible row up by exactly 3 lines — from L7 to L4 — not
        // by a whole grid's worth (would land on L1) and not by zero
        // (the pre-fix scrollback-only ceiling could also produce this
        // if it happened to already sit at its own bottom).
        pane.wheel_up(3);
        assert!(!pane.follow_output());
        assert_eq!(
            ingest.render_lines(&pane),
            vec!["L4", "L5", "L6", "L7"],
            "a three-line request must scroll by 3 lines, not jump a full screen"
        );

        // Subsequent requests keep scrolling smoothly by the same
        // amount — the "later notches are fine" half of the bug
        // report, confirmed unchanged by the fix.
        pane.wheel_up(3);
        assert_eq!(ingest.render_lines(&pane), vec!["L1", "L2", "L3", "L4"]);
    }

    #[test]
    fn wheel_down_back_to_bottom_reenables_follow_and_shows_live_grid_seamlessly() {
        let (mut pane, ingest) = wheel_jump_fixture();

        pane.wheel_up(6);
        assert!(!pane.follow_output());

        // Scroll all the way back down. No further ingest happens in
        // between — the grid is untouched, so this must land exactly
        // back on the live grid's tail with no top-alignment glitch
        // (the kind `flush_incomplete_line` would cause by resetting
        // the cursor and leaving follow-mode render starting from a
        // blank top row instead of the live bottom).
        pane.wheel_down(100);
        assert!(pane.follow_output());
        assert_eq!(ingest.render_lines(&pane), vec!["L7", "L8", "L9", "L10"]);
    }

    #[test]
    fn new_output_while_scrolled_back_does_not_move_the_viewport() {
        // This is the mid-burst clamp hazard the extra `sync_live_tail`
        // calls inside `primary_index`'s eviction branches guard
        // against: without them, a single `ingest_bytes` call that
        // evicts several grid rows while the user is scrolled back can
        // clamp `viewport_top` down using a stale (too-small)
        // `live_tail`, visibly dragging the view toward the bottom mid-
        // burst.
        let (mut pane, mut ingest) = wheel_jump_fixture();

        pane.wheel_up(3);
        assert!(!pane.follow_output());
        let before = ingest.render_lines(&pane);
        assert_eq!(before, vec!["L4", "L5", "L6", "L7"]);

        // Two more lines arrive, evicting L7 and L8 out of the grid and
        // into scrollback behind the scenes.
        ingest.ingest_bytes(&mut pane, b"\nL11\nL12");

        assert!(
            !pane.follow_output(),
            "new output must not silently re-enable follow while scrolled back"
        );
        assert_eq!(
            ingest.render_lines(&pane),
            before,
            "the scrolled-back view must not shift when new output arrives"
        );
    }

    #[test]
    fn clear_then_fresh_output_still_renders_top_aligned_in_follow_mode() {
        // Regression guard: this fix only replaces the non-follow
        // branch of `render_primary_cells`. The follow-mode branch
        // (small/growing grid, blank rows padded below) must render
        // exactly as it did before — content starting at the top, not
        // spliced with (now-cleared) scrollback and not bottom-aligned.
        let mut pane = Pane::new("shell", 64, 4);
        let mut ingest = TerminalIngest::new(PtySize::new(4, 80));

        ingest.ingest_bytes(&mut pane, b"old1\nold2\nold3\nold4");
        ingest.ingest_bytes(&mut pane, b"\x1b[H\x1b[2J\x1b[3J");
        assert_eq!(pane.total_lines(), 0);

        ingest.ingest_bytes(&mut pane, b"fresh");

        assert!(pane.follow_output());
        assert_eq!(ingest.render_lines(&pane), vec!["fresh"]);
    }
}
