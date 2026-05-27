// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::fmt::Write as _;
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
}

impl TerminalGuard {
    pub fn enter() -> io::Result<Self> {
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
        };
        guard.write_escape("\x1b[?1049h\x1b[?25l\x1b[2J\x1b[H")?;
        guard.set_mouse_tracking_mode(MouseTrackingMode::Click)?;
        Ok(guard)
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
        let mut poll_fd = [PollFd {
            fd: self.stdin_fd,
            events: POLLIN,
            revents: 0,
        }];

        let ready = unsafe { poll(poll_fd.as_mut_ptr(), poll_fd.len(), timeout_ms) };
        if ready == -1 {
            return Err(io::Error::last_os_error());
        }

        if ready == 0 || poll_fd[0].revents & POLLIN == 0 {
            return Ok(Vec::new());
        }

        let mut buffer = vec![0u8; 1024];
        let count = io::stdin().read(&mut buffer)?;
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

        self.render_frame(&rows, size)
    }

    pub fn render_frame(&mut self, rows: &[String], size: PtySize) -> io::Result<()> {
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

        let mut changed_rows: usize = 0;
        for (index, new_row) in target_rows.iter().enumerate() {
            let previous = self.last_frame.get(index);
            if !full_repaint && previous.map(|row| row == new_row).unwrap_or(false) {
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

        // Park the cursor on the status row's leftmost column so the
        // hidden cursor is at a predictable spot; some terminals render a
        // visible shadow block even while hidden.
        let _ = write!(&mut buffer, "\x1b[{};1H", expected_rows);
        buffer.push_str("\x1b[?2026l");

        if full_repaint || changed_rows > 0 {
            self.write_escape(&buffer)?;
        }
        self.last_frame = target_rows;
        self.last_size = Some(size);
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
        let _ = self.write_escape(&format!(
            "\x1b[0m{}\x1b[?25h\x1b[?1049l",
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

    let ready = unsafe { poll(poll_fd.as_mut_ptr(), poll_fd.len(), timeout_ms) };
    if ready == -1 {
        return Err(io::Error::last_os_error());
    }

    Ok(ready > 0 && poll_fd[0].revents & POLLIN != 0)
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
    use super::{append_row_update, mouse_tracking_reset_sequence, mouse_tracking_sequence};
    use crate::mouse::MouseTrackingMode;

    #[test]
    fn row_updates_reset_and_clear_before_redraw() {
        let mut buffer = String::new();
        append_row_update(&mut buffer, 2, "new row");

        assert_eq!(buffer, "\x1b[3;1H\x1b[0m\x1b[2Knew row\x1b[0m");
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
