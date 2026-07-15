// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::fmt::Write as _;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::AsRawFd;

use crate::mouse::MouseTrackingMode;
use crate::pty::PtySize;

const BRKINT: u32 = 0o000002;
const ICRNL: u32 = 0o000400;
const INPCK: u32 = 0o000020;
const ISTRIP: u32 = 0o000040;
const IXON: u32 = 0o002000;
const OPOST: u32 = 0o000001;
const CS8: u32 = 0o000060;
const ECHO: u32 = 0o000010;
const ICANON: u32 = 0o000002;
const IEXTEN: u32 = 0o100000;
const ISIG: u32 = 0o000001;

const VTIME: usize = 5;
const VMIN: usize = 6;

const TCSANOW: i32 = 0;
const TIOCGWINSZ: u64 = 0x5413;

const POLLIN: i16 = 0x0001;

#[repr(C)]
#[derive(Clone, Copy)]
struct Termios {
    c_iflag: u32,
    c_oflag: u32,
    c_cflag: u32,
    c_lflag: u32,
    c_line: u8,
    c_cc: [u8; 32],
    c_ispeed: u32,
    c_ospeed: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct Winsize {
    ws_row: u16,
    ws_col: u16,
    ws_xpixel: u16,
    ws_ypixel: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

unsafe extern "C" {
    fn tcgetattr(fd: i32, termios_p: *mut Termios) -> i32;
    fn tcsetattr(fd: i32, optional_actions: i32, termios_p: *const Termios) -> i32;
    fn ioctl(fd: i32, request: u64, ...) -> i32;
    fn poll(fds: *mut PollFd, nfds: usize, timeout: i32) -> i32;
}

pub struct TerminalGuard {
    stdin_fd: i32,
    stdout: io::Stdout,
    original_mode: Termios,
    mouse_tracking_mode: MouseTrackingMode,
    // Cached rows from the last render so we can skip redrawing rows that
    // didn't change. The Vec length also tells us whether a full repaint
    // is needed (e.g. after a resize, the row count changes).
    last_frame: Vec<String>,
    last_size: Option<PtySize>,
    // Cursor state from the last frame, so a cursor-only change (the
    // app moved its cursor without altering any row's content — arrow
    // keys on a readline, say) still triggers a write even though every
    // row diffed clean.
    last_cursor: Option<(u16, u16)>,
    // Optional exact-byte tap used by the structured trace client. This
    // lives at the final stdout boundary, after frame diffing and cursor/
    // mouse-mode serialization, so it captures what the host terminal
    // actually received rather than merely what the daemon intended to
    // render. Ordinary foreground/demo callers leave the tap disabled.
    capture_writes: bool,
    captured_writes: Vec<u8>,
}

impl TerminalGuard {
    pub fn enter() -> io::Result<Self> {
        Self::enter_inner(false)
    }

    /// Enter raw terminal mode while retaining exact stdout writes for a
    /// structured session trace. Callers must drain `take_captured_writes`
    /// regularly; the attach loop does so once per poll iteration.
    pub fn enter_with_write_capture() -> io::Result<Self> {
        Self::enter_inner(true)
    }

    fn enter_inner(capture_writes: bool) -> io::Result<Self> {
        let stdin = io::stdin();
        let stdin_fd = stdin.as_raw_fd();
        let original_mode = read_termios(stdin_fd)?;
        let raw_mode = make_raw(original_mode);
        write_termios(stdin_fd, &raw_mode)?;

        let mut guard = Self {
            stdin_fd,
            stdout: io::stdout(),
            original_mode,
            mouse_tracking_mode: MouseTrackingMode::Off,
            last_frame: Vec::new(),
            last_size: None,
            last_cursor: None,
            capture_writes,
            captured_writes: Vec::new(),
        };
        // `?1049h` alt-screen, `?25l` hide cursor, clear+home. `?2004h`
        // (bracketed paste) tells the host terminal to wrap multiline
        // pastes in `ESC[200~ ... ESC[201~` markers instead of sending
        // bare newlines — without it, pasting multiline text into a
        // pane submits line-by-line (vim/Claude Code mis-fire on every
        // embedded newline) instead of landing as a single paste.
        guard.write_escape("\x1b[?1049h\x1b[?25l\x1b[2J\x1b[H\x1b[?2004h")?;
        guard.set_mouse_tracking_mode(MouseTrackingMode::Click)?;
        Ok(guard)
    }

    /// Drain exact ANSI/text bytes written since the previous call.
    pub fn take_captured_writes(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.captured_writes)
    }

    pub fn set_write_capture(&mut self, enabled: bool) {
        self.capture_writes = enabled;
        if !enabled {
            self.captured_writes.clear();
        }
    }

    pub fn size(&self) -> io::Result<PtySize> {
        let stdout_fd = self.stdout.as_raw_fd();
        let mut winsize = MaybeUninit::<Winsize>::uninit();
        let result = unsafe { ioctl(stdout_fd, TIOCGWINSZ, winsize.as_mut_ptr()) };
        if result == -1 {
            return Err(io::Error::last_os_error());
        }

        let winsize = unsafe { winsize.assume_init() };
        Ok(PtySize::new(winsize.ws_row.max(2), winsize.ws_col.max(1)))
    }

    pub fn read_input(&mut self, timeout_ms: i32) -> io::Result<Vec<u8>> {
        if !poll_readable(self.stdin_fd, timeout_ms)? {
            return Ok(Vec::new());
        }

        let mut buffer = vec![0u8; 1024];
        // A stray signal (zmux installs no handlers today, but a future
        // one — or a signal a library installs on our behalf — must not
        // turn a routine interrupt into a lost read) can make this
        // return EINTR with nothing actually wrong; retry rather than
        // surfacing it as a hard error.
        let count = loop {
            match io::stdin().read(&mut buffer) {
                Ok(count) => break count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        };
        buffer.truncate(count);
        Ok(buffer)
    }

    pub fn render(
        &mut self,
        title: &str,
        lines: &[String],
        status: &str,
        size: PtySize,
    ) -> io::Result<()> {
        let content_rows = size.rows.saturating_sub(1) as usize;
        let mut rows = Vec::with_capacity(size.rows as usize);
        for row in 0..content_rows {
            rows.push(lines.get(row).cloned().unwrap_or_default());
        }

        let mut status_line = String::new();
        let _ = write!(&mut status_line, "{title} | {status}");
        rows.push(status_line);

        self.render_frame(&rows, size, None)
    }

    pub fn render_frame(
        &mut self,
        rows: &[String],
        size: PtySize,
        cursor: Option<(u16, u16)>,
    ) -> io::Result<()> {
        let expected_rows = size.rows as usize;
        // Repaint every row when the terminal size changes or the cache was
        // invalidated; otherwise redraw only rows whose serialized content
        // changed. This keeps clock ticks and small pane updates from
        // rewriting the whole screen while still recovering cleanly from
        // resizes, overlays, and first attach.
        let full_repaint = self.last_size != Some(size) || self.last_frame.len() != expected_rows;
        // The server already emits rows that are exactly `cols` visible
        // columns wide, with SGR transitions interleaved. We must NOT
        // .chars().take(cols) here — a row typically contains far more
        // chars than visible columns (every per-cell truecolor transition
        // is ~30 chars of escape), so naive truncation slices mid-escape
        // and destroys most of the content.
        let target_rows: Vec<String> = (0..expected_rows)
            .map(|index| rows.get(index).cloned().unwrap_or_default())
            .collect();

        let mut buffer = String::new();
        // DEC mode 2026 (synchronized update): terminals that support it
        // buffer everything between ?2026h and ?2026l and flip once, so
        // the user never sees a half-drawn frame. Terminals that don't
        // recognize the mode silently ignore the sequence.
        buffer.push_str("\x1b[?2026h");

        // A viewport wheel step changes nearly every content row by moving
        // it one slot. Repainting those rows individually is correct but
        // expensive (the real Termius trace showed ~10 KiB per notch at
        // 215x41). When the new frame is predominantly a vertical shift,
        // let the host terminal move the matching band with SU/SD and only
        // redraw the newly exposed edge rows plus changed chrome.
        let same_index_changed: Vec<bool> = if full_repaint {
            vec![true; expected_rows]
        } else {
            self.last_frame
                .iter()
                .zip(&target_rows)
                .map(|(previous, next)| previous != next)
                .collect()
        };
        let changed_row_count = same_index_changed
            .iter()
            .filter(|changed| **changed)
            .count();
        let vertical_shift = if full_repaint || changed_row_count < 6 {
            None
        } else {
            detect_vertical_shift(&self.last_frame, &target_rows)
        };
        let mut shifted_previous = None;
        if let Some(shift) = vertical_shift {
            append_vertical_shift(&mut buffer, shift);
            let mut rows = self.last_frame.clone();
            apply_vertical_shift_to_cache(&mut rows, shift);
            shifted_previous = Some(rows);
        }
        let previous_rows = shifted_previous.as_deref().unwrap_or(&self.last_frame);

        let mut changed_rows: usize = 0;
        for (index, new_row) in target_rows.iter().enumerate() {
            let previous = previous_rows.get(index);
            let unchanged = if vertical_shift.is_some() {
                previous.map(|row| row == new_row).unwrap_or(false)
            } else {
                !same_index_changed[index]
            };
            if !full_repaint && unchanged {
                continue;
            }

            changed_rows += 1;
            // Explicit absolute positioning per row. We can't rely on '\n'
            // because raw mode disables OPOST, so LF won't carriage-return.
            //
            // Even when the workspace intends to emit a full-width row,
            // host terminals can keep stale glyphs/backgrounds around if
            // a row contains wide glyphs, ignored OSC/SGR transitions, or
            // a short non-workspace status line. Reset before the clear so
            // EL does not smear a previous reverse-video/background style.
            append_row_update(&mut buffer, index, new_row);
        }

        // Land the host cursor: at the active pane's cursor cell when
        // the daemon says it's visible, otherwise hidden and parked on
        // the status row's leftmost column so terminals that render a
        // shadow block for hidden cursors keep it somewhere predictable.
        append_cursor_tail(&mut buffer, cursor, expected_rows);
        buffer.push_str("\x1b[?2026l");

        // A cursor-only change (position moved or visibility flipped)
        // must still write even when every row diffed clean.
        if full_repaint
            || vertical_shift.is_some()
            || changed_rows > 0
            || cursor != self.last_cursor
        {
            self.write_escape(&buffer)?;
        }
        self.last_frame = target_rows;
        self.last_size = Some(size);
        self.last_cursor = cursor;
        Ok(())
    }

    // Emit OSC 52 so the hosting terminal copies `text` to the user's
    // system clipboard. We use the `c` (primary clipboard) selection
    // and base64-encode the payload per the spec. Terminals that don't
    // support OSC 52 just ignore the escape.
    pub fn emit_clipboard(&mut self, text: &str) -> io::Result<()> {
        let encoded = crate::style::base64_encode(text.as_bytes());
        let sequence = format!("\x1b]52;c;{encoded}\x1b\\");
        self.write_escape(&sequence)
    }

    pub fn set_mouse_tracking_mode(
        &mut self,
        mouse_tracking_mode: MouseTrackingMode,
    ) -> io::Result<()> {
        if mouse_tracking_mode == self.mouse_tracking_mode {
            return Ok(());
        }

        self.write_escape(&mouse_tracking_sequence(mouse_tracking_mode))?;
        self.mouse_tracking_mode = mouse_tracking_mode;
        Ok(())
    }

    fn write_escape(&mut self, sequence: &str) -> io::Result<()> {
        self.stdout.write_all(sequence.as_bytes())?;
        if self.capture_writes {
            self.captured_writes.extend_from_slice(sequence.as_bytes());
        }
        self.stdout.flush()
    }

    // Write arbitrary ANSI / text directly to the host terminal. Used by
    // the session-picker overlay, which needs to paint its own chrome
    // without going through render_frame. The caller is responsible for
    // positioning (CUP), colors (SGR), and clean-up. Pair with
    // `invalidate_frame_cache` before the next server frame renders.
    pub fn write_ansi(&mut self, sequence: &str) -> io::Result<()> {
        self.write_escape(sequence)
    }

    // Drop the diff cache so the next render_frame call does a full
    // repaint. The picker overlay paints over cells that render_frame
    // thinks are still "clean" — without this, the overlay would leak
    // under fresh frames until something about each row changed.
    pub fn invalidate_frame_cache(&mut self) {
        self.last_frame.clear();
        self.last_size = None;
    }
}

fn append_row_update(buffer: &mut String, index: usize, row: &str) {
    let _ = write!(buffer, "\x1b[{};1H\x1b[0m\x1b[2K", index + 1);
    buffer.push_str(row);
    buffer.push_str("\x1b[0m");
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VerticalShiftDirection {
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VerticalShift {
    top: usize,
    bottom: usize,
    amount: usize,
    direction: VerticalShiftDirection,
    matched_rows: usize,
}

fn detect_vertical_shift(previous: &[String], next: &[String]) -> Option<VerticalShift> {
    if previous.len() != next.len() || previous.len() < 3 {
        return None;
    }
    // Hash each row once so scanning up to 32 possible deltas remains
    // linear in frame bytes rather than repeatedly comparing long ANSI-
    // styled strings (a pathological 512x1024 frame can be tens of MiB).
    let previous_hashes: Vec<u64> = previous.iter().map(|row| row_fingerprint(row)).collect();
    let next_hashes: Vec<u64> = next.iter().map(|row| row_fingerprint(row)).collect();
    let baseline_matches = previous_hashes
        .iter()
        .zip(&next_hashes)
        .filter(|(before, after)| before == after)
        .count();
    let mut best: Option<VerticalShift> = None;
    let max_amount = previous.len().saturating_sub(1).min(32);

    for amount in 1..=max_amount {
        for direction in [VerticalShiftDirection::Up, VerticalShiftDirection::Down] {
            let mut run_start = 0;
            let mut run_len = 0;
            let mut consider_run = |start: usize, len: usize| {
                if len < 4 || len <= baseline_matches.saturating_add(2) {
                    return;
                }
                let candidate = VerticalShift {
                    top: start,
                    bottom: start + len + amount - 1,
                    amount,
                    direction,
                    matched_rows: len,
                };
                if candidate.bottom >= previous.len() {
                    return;
                }
                let replace = best.is_none_or(|current| {
                    candidate.matched_rows > current.matched_rows
                        || (candidate.matched_rows == current.matched_rows
                            && candidate.amount < current.amount)
                });
                if replace {
                    best = Some(candidate);
                }
            };

            for index in 0..previous.len() - amount {
                let matches = match direction {
                    VerticalShiftDirection::Up => {
                        next_hashes[index] == previous_hashes[index + amount]
                            && next[index] == previous[index + amount]
                    }
                    VerticalShiftDirection::Down => {
                        next_hashes[index + amount] == previous_hashes[index]
                            && next[index + amount] == previous[index]
                    }
                };
                if matches {
                    if run_len == 0 {
                        run_start = index;
                    }
                    run_len += 1;
                } else {
                    consider_run(run_start, run_len);
                    run_len = 0;
                }
            }
            consider_run(run_start, run_len);
        }
    }
    best
}

fn row_fingerprint(row: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    row.hash(&mut hasher);
    hasher.finish()
}

fn append_vertical_shift(buffer: &mut String, shift: VerticalShift) {
    let command = match shift.direction {
        VerticalShiftDirection::Up => 'S',
        VerticalShiftDirection::Down => 'T',
    };
    let _ = write!(
        buffer,
        "\x1b[{};{}r\x1b[{}{}\x1b[r",
        shift.top + 1,
        shift.bottom + 1,
        shift.amount,
        command,
    );
}

fn apply_vertical_shift_to_cache(rows: &mut [String], shift: VerticalShift) {
    let region = &mut rows[shift.top..=shift.bottom];
    match shift.direction {
        VerticalShiftDirection::Up => {
            region.rotate_left(shift.amount);
            let exposed = region.len().saturating_sub(shift.amount);
            for row in &mut region[exposed..] {
                *row = "\0".to_string();
            }
        }
        VerticalShiftDirection::Down => {
            region.rotate_right(shift.amount);
            for row in &mut region[..shift.amount] {
                *row = "\0".to_string();
            }
        }
    }
}

// The frame's final cursor placement. `cursor` is absolute 1-based
// (row, col) from the daemon — Some means "show the host cursor there"
// (DECTCEM show after the move so the cursor never flashes at a stale
// position), None means keep it hidden and parked below the content.
fn append_cursor_tail(buffer: &mut String, cursor: Option<(u16, u16)>, park_row: usize) {
    match cursor {
        Some((row, col)) => {
            let _ = write!(buffer, "\x1b[{row};{col}H\x1b[?25h");
        }
        None => {
            let _ = write!(buffer, "\x1b[?25l\x1b[{park_row};1H");
        }
    }
}

fn mouse_tracking_reset_sequence() -> &'static str {
    "\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l"
}

fn mouse_tracking_sequence(mouse_tracking_mode: MouseTrackingMode) -> String {
    let mut sequence = String::from(mouse_tracking_reset_sequence());
    match mouse_tracking_mode {
        MouseTrackingMode::Off => {}
        MouseTrackingMode::Click => sequence.push_str("\x1b[?1000h\x1b[?1006h"),
        MouseTrackingMode::Drag => sequence.push_str("\x1b[?1000h\x1b[?1002h\x1b[?1006h"),
        MouseTrackingMode::Motion => sequence.push_str("\x1b[?1000h\x1b[?1003h\x1b[?1006h"),
    }
    sequence
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Undo every mode `enter()` turned on before leaving the alt
        // screen: mouse tracking, then bracketed paste (`?2004l`), then
        // show the cursor. Leaving `?2004h` set would have the host
        // terminal keep wrapping pastes in `ESC[200~`/`ESC[201~` markers
        // for whatever the user runs next in this terminal.
        let _ = self.write_escape(&format!(
            "\x1b[0m{}\x1b[?2004l\x1b[?25h\x1b[?1049l",
            mouse_tracking_reset_sequence()
        ));
        let _ = write_termios(self.stdin_fd, &self.original_mode);
    }
}

pub fn poll_readable(fd: i32, timeout_ms: i32) -> io::Result<bool> {
    let mut poll_fd = [PollFd {
        fd,
        events: POLLIN,
        revents: 0,
    }];

    // Retry on EINTR rather than propagating it: a signal landing mid-poll
    // isn't a poll failure, and treating it as one would make an
    // otherwise-routine signal look like a dead fd to every caller
    // (client reads, PTY reads, the accept-loop).
    loop {
        let ready = unsafe { poll(poll_fd.as_mut_ptr(), poll_fd.len(), timeout_ms) };
        if ready == -1 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }

        return Ok(ready > 0 && poll_fd[0].revents & POLLIN != 0);
    }
}

fn read_termios(fd: i32) -> io::Result<Termios> {
    let mut termios = MaybeUninit::<Termios>::uninit();
    let result = unsafe { tcgetattr(fd, termios.as_mut_ptr()) };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }

    Ok(unsafe { termios.assume_init() })
}

fn write_termios(fd: i32, termios: &Termios) -> io::Result<()> {
    let result = unsafe { tcsetattr(fd, TCSANOW, termios) };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}

fn make_raw(mut termios: Termios) -> Termios {
    termios.c_iflag &= !(BRKINT | ICRNL | INPCK | ISTRIP | IXON);
    termios.c_oflag &= !OPOST;
    termios.c_cflag |= CS8;
    termios.c_lflag &= !(ECHO | ICANON | IEXTEN | ISIG);
    termios.c_cc[VMIN] = 0;
    termios.c_cc[VTIME] = 0;
    termios
}

#[cfg(test)]
mod tests {
    use super::{
        VerticalShift, VerticalShiftDirection, append_cursor_tail, append_row_update,
        append_vertical_shift, apply_vertical_shift_to_cache, detect_vertical_shift,
        mouse_tracking_reset_sequence, mouse_tracking_sequence,
    };
    use crate::mouse::MouseTrackingMode;

    #[test]
    fn cursor_tail_shows_at_position_or_hides_and_parks() {
        // Move BEFORE show so the cursor can't flash at its stale spot;
        // hide BEFORE park for the same reason in reverse.
        let mut shown = String::new();
        append_cursor_tail(&mut shown, Some((3, 12)), 38);
        assert_eq!(shown, "\x1b[3;12H\x1b[?25h");

        let mut hidden = String::new();
        append_cursor_tail(&mut hidden, None, 38);
        assert_eq!(hidden, "\x1b[?25l\x1b[38;1H");
    }

    #[test]
    fn row_updates_reset_and_clear_before_redraw() {
        let mut buffer = String::new();
        append_row_update(&mut buffer, 2, "new row");

        assert_eq!(buffer, "\x1b[3;1H\x1b[0m\x1b[2Knew row\x1b[0m");
    }

    #[test]
    fn detects_and_models_native_viewport_scrolls() {
        let before: Vec<String> = ["header", "one", "two", "three", "four", "five", "status"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let after: Vec<String> = [
            "header changed",
            "two",
            "three",
            "four",
            "five",
            "six",
            "status changed",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();

        let shift = detect_vertical_shift(&before, &after).expect("vertical shift");
        assert_eq!(
            shift,
            VerticalShift {
                top: 1,
                bottom: 5,
                amount: 1,
                direction: VerticalShiftDirection::Up,
                matched_rows: 4,
            }
        );
        let mut sequence = String::new();
        append_vertical_shift(&mut sequence, shift);
        assert_eq!(sequence, "\x1b[2;6r\x1b[1S\x1b[r");

        let mut modeled = before;
        apply_vertical_shift_to_cache(&mut modeled, shift);
        assert_eq!(&modeled[1..5], &after[1..5]);
        assert_eq!(modeled[5], "\0", "exposed row must be redrawn");

        let down_after: Vec<String> = [
            "header changed again",
            "one",
            "two",
            "three",
            "four",
            "five",
            "status changed again",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        let down = detect_vertical_shift(&after, &down_after).expect("downward shift");
        assert_eq!(down.direction, VerticalShiftDirection::Down);
        assert_eq!((down.top, down.bottom, down.amount), (1, 5, 1));
        let mut modeled_down = after;
        apply_vertical_shift_to_cache(&mut modeled_down, down);
        assert_eq!(&modeled_down[2..6], &down_after[2..6]);
        assert_eq!(modeled_down[1], "\0", "top exposed row must redraw");
    }

    #[test]
    fn ordinary_sparse_row_change_is_not_misclassified_as_scroll() {
        let before: Vec<String> = ["a", "b", "c", "d", "e", "f", "g"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let mut after = before.clone();
        after[3] = "changed".to_string();

        assert_eq!(detect_vertical_shift(&before, &after), None);
    }

    #[test]
    fn drag_mouse_tracking_keeps_basic_reporting_for_wheel_events() {
        assert_eq!(
            mouse_tracking_sequence(MouseTrackingMode::Drag),
            "\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1000h\x1b[?1002h\x1b[?1006h"
        );
    }

    #[test]
    fn mouse_tracking_reset_disables_every_enabled_mode() {
        assert_eq!(
            mouse_tracking_reset_sequence(),
            "\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l"
        );
    }
}
