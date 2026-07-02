// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::VecDeque;
use std::fmt;
use std::io::Write;
use std::time::Instant;

use crate::agent::AgentState;
use crate::mouse::{
    AltScreenScrollPolicy, MouseContext, MouseTrackingMode, ScreenMode, WheelDirection,
    WheelRouting, route_wheel,
};
use crate::scrollback::{ScrollbackBuffer, ScrollbackLine};
#[cfg(test)]
use crate::style::Cell;

pub const OUTPUT_RING_CAPACITY: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneOutputSlice {
    pub start_byte: u64,
    pub byte_cursor: u64,
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

pub struct Pane {
    title: String,
    screen_mode: ScreenMode,
    mouse_tracking_mode: MouseTrackingMode,
    alt_screen_policy: AltScreenScrollPolicy,
    scrollback: ScrollbackBuffer,
    // Optional copy of every raw PTY-byte chunk for `zmux capture`.
    // Boxed so future sinks (in-memory rings, broadcast tees, network
    // sinks) can land without changing the type signature.
    capture_sink: Option<Box<dyn Write + Send>>,
    // Bounded raw PTY transcript for cursor-based MCP readers,
    // separate from rendered scrollback: agent shims need the byte
    // stream for the current turn, `read_pane` needs a snapshot.
    output_ring: VecDeque<u8>,
    output_ring_start: u64,
    output_byte_cursor: u64,
    // Workspace owns the `IdleDetector` / `PromptDetector`; these
    // fields cache the latest derived values for cheap reads from the
    // dispatch and protocol layers. `last_output_at` is the wall-clock
    // time of the most recent ingestion, used by the per-frame tick
    // to decide when to flip a pane from `Working` back to `Idle`.
    pub label: Option<String>,
    pub agent_state: AgentState,
    pub last_output_at: Instant,
    pub last_command: Option<String>,
    pub last_exit: Option<i32>,
}

impl fmt::Debug for Pane {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pane")
            .field("title", &self.title)
            .field("screen_mode", &self.screen_mode)
            .field("mouse_tracking_mode", &self.mouse_tracking_mode)
            .field("alt_screen_policy", &self.alt_screen_policy)
            .field("scrollback", &self.scrollback)
            .field("capture_sink", &self.capture_sink.is_some())
            .field("output_ring_len", &self.output_ring.len())
            .field("output_ring_start", &self.output_ring_start)
            .field("output_byte_cursor", &self.output_byte_cursor)
            .field("label", &self.label)
            .field("agent_state", &self.agent_state)
            .field("last_output_at", &self.last_output_at)
            .field("last_command", &self.last_command)
            .field("last_exit", &self.last_exit)
            .finish()
    }
}

/// Strip CSI/OSC-style escape sequences from a string. Conservative
/// implementation aimed at the two forms most CLIs emit:
///
/// - `ESC [ ... <final>` (CSI / SGR): consume the introducer `[`, then
///   any parameter / intermediate bytes (`0x30..=0x3f` and `0x20..=0x2f`),
///   then a single final byte in `0x40..=0x7e`.
/// - `ESC ] ... BEL` or `ESC ] ... ESC \\` (OSC): consume bytes until
///   BEL (`0x07`) or `ESC \\`.
///
/// Anything else after `ESC` is consumed as a single-byte sequence
/// (covers `ESC =` / `ESC >` keypad-mode toggles). Non-ESC bytes pass
/// through unchanged.
pub fn strip_ansi_inplace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b != 0x1b {
            // Push the non-ESC run as one slice: ESC (0x1b) is ASCII
            // so run boundaries always land on UTF-8 char boundaries,
            // preserving multi-byte codepoints. A byte-by-byte
            // `out.push(b as char)` would corrupt every non-ASCII
            // char (E2/94/82 → U+00E2/0094/0082).
            let start = i;
            while i < bytes.len() && bytes[i] != 0x1b {
                i += 1;
            }
            out.push_str(&s[start..i]);
            continue;
        }
        // ESC seen — try to classify the next byte.
        let Some(&next) = bytes.get(i + 1) else {
            i += 1;
            continue;
        };
        match next {
            b'[' => {
                // CSI: skip params/intermediates, then one final byte.
                i += 2;
                while i < bytes.len() {
                    let c = bytes[i];
                    if (0x40..=0x7e).contains(&c) {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            b']' => {
                // OSC: skip until BEL or ESC \\.
                i += 2;
                while i < bytes.len() {
                    let c = bytes[i];
                    if c == 0x07 {
                        i += 1;
                        break;
                    }
                    if c == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            _ => {
                // ESC <other>: assume single-byte sequence.
                i += 2;
            }
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WheelOutcome {
    ViewportChanged {
        lines_scrolled: usize,
        follow_output: bool,
    },
    PassedToApplication,
}

impl Pane {
    pub fn new(
        title: impl Into<String>,
        scrollback_capacity: usize,
        viewport_height: usize,
    ) -> Self {
        Self {
            title: title.into(),
            screen_mode: ScreenMode::Primary,
            mouse_tracking_mode: MouseTrackingMode::Off,
            alt_screen_policy: AltScreenScrollPolicy::PaneScrollback,
            scrollback: ScrollbackBuffer::new(scrollback_capacity, viewport_height),
            capture_sink: None,
            output_ring: VecDeque::new(),
            output_ring_start: 0,
            output_byte_cursor: 0,
            label: None,
            agent_state: AgentState::Idle,
            last_output_at: Instant::now(),
            last_command: None,
            last_exit: None,
        }
    }

    // ANSI-stripped snapshot of the pane's recent output. The VT
    // parser already consumed escapes before cells landed in
    // scrollback, so `strip_ansi == true` is equivalent to collecting
    // the rendered chars today; the boolean exists so a future
    // raw-byte scrollback can change the `false` branch without a
    // signature change.
    pub fn scrollback_text(&self, lines: usize, strip_ansi: bool) -> Vec<String> {
        let raw: Vec<String> = self
            .scrollback
            .tail_lines(lines)
            .into_iter()
            .map(|cells| {
                cells
                    .iter()
                    .filter(|c| c.ch != '\0')
                    .map(|c| c.ch)
                    .collect()
            })
            .collect();
        if !strip_ansi {
            return raw;
        }
        raw.into_iter()
            .map(|line| strip_ansi_inplace(&line))
            .collect()
    }

    // Cell-level counterpart to `scrollback_text`: the same tail-of-buffer
    // slice, but returning the styled cells instead of collapsing them to
    // plain chars. Scrollback lines are stored as `Cell`s already (see
    // `ScrollbackBuffer::tail_lines`), so this is a direct passthrough —
    // no separate raw-byte storage needed. Used by MCP `read_pane`'s
    // `strip_ansi=false` path so callers asking for real output get real
    // SGR, not the historical chars-only passthrough.
    pub fn scrollback_cells(&self, lines: usize) -> Vec<ScrollbackLine> {
        self.scrollback.tail_lines(lines)
    }

    // VT-capture tap. Attaches a sink that will receive a copy of every
    // raw PTY-byte chunk fed into this pane (see `mirror_capture`).
    // `Session::ingest_available_output` is the actual call site once
    // the daemon hands the sink down.
    pub fn attach_capture(&mut self, sink: Box<dyn Write + Send>) {
        self.capture_sink = Some(sink);
    }

    pub fn detach_capture(&mut self) -> Option<Box<dyn Write + Send>> {
        self.capture_sink.take()
    }

    // Mirror a PTY-byte chunk into the capture sink, if one is attached.
    // Capture is a diagnostic aid, not part of the rendering contract:
    // we never propagate sink errors up and never tear the pane down.
    // But silently swallowing every write error means a broken sink
    // (full disk, dropped pipe, etc.) leaves the user staring at a file
    // that mysteriously stops growing — so on the *first* error we log
    // once to stderr and detach the sink, which both makes the failure
    // observable and stops us from re-logging on every subsequent chunk.
    pub fn mirror_capture(&mut self, bytes: &[u8]) {
        let detach = if let Some(sink) = self.capture_sink.as_mut() {
            match sink.write_all(bytes) {
                Ok(()) => false,
                Err(err) => {
                    eprintln!("zmux capture: sink write failed: {err}; detaching capture");
                    true
                }
            }
        } else {
            false
        };
        if detach {
            self.capture_sink = None;
        }
    }

    // Returns true while a capture sink is attached.
    pub fn is_capturing(&self) -> bool {
        self.capture_sink.is_some()
    }

    /// Record raw PTY bytes in the pane's bounded transcript ring.
    ///
    /// This mirrors the bytes handed to the terminal ingester, but does
    /// not attempt to parse VT state. Consumers use the monotonic byte
    /// cursor to capture output emitted after a known point in time.
    pub fn record_output(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.output_byte_cursor = self.output_byte_cursor.saturating_add(bytes.len() as u64);
        self.output_ring.extend(bytes.iter().copied());
        while self.output_ring.len() > OUTPUT_RING_CAPACITY {
            let _ = self.output_ring.pop_front();
            self.output_ring_start = self.output_ring_start.saturating_add(1);
        }
    }

    pub fn output_byte_cursor(&self) -> u64 {
        self.output_byte_cursor
    }

    pub fn output_since(&self, since_byte: u64, max_bytes: usize) -> PaneOutputSlice {
        if max_bytes == 0 {
            return PaneOutputSlice {
                start_byte: self.output_byte_cursor,
                byte_cursor: self.output_byte_cursor,
                bytes: Vec::new(),
                truncated: false,
            };
        }

        let mut truncated = false;
        let start_byte = if since_byte < self.output_ring_start {
            truncated = true;
            self.output_ring_start
        } else {
            since_byte.min(self.output_byte_cursor)
        };
        let available = self.output_byte_cursor.saturating_sub(start_byte) as usize;
        let take = available.min(max_bytes);
        if available > max_bytes {
            truncated = true;
        }
        let offset = start_byte.saturating_sub(self.output_ring_start) as usize;
        let bytes = self
            .output_ring
            .iter()
            .skip(offset)
            .take(take)
            .copied()
            .collect();

        PaneOutputSlice {
            start_byte,
            byte_cursor: self.output_byte_cursor,
            bytes,
            truncated,
        }
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    // Updates the pane title from an OSC 0/1/2 sequence emitted by the
    // running program. Programs like vim send `ESC ] 2 ; file.rs BEL`
    // whenever the active file changes; surfacing that in the pane header
    // makes it easy to tell panes apart at a glance.
    pub fn set_title(&mut self, title: impl Into<String>) {
        self.title = title.into();
    }

    pub fn set_screen_mode(&mut self, screen_mode: ScreenMode) {
        self.screen_mode = screen_mode;
    }

    pub fn screen_mode(&self) -> ScreenMode {
        self.screen_mode
    }

    pub fn set_mouse_tracking_mode(&mut self, mouse_tracking_mode: MouseTrackingMode) {
        self.mouse_tracking_mode = mouse_tracking_mode;
    }

    pub fn mouse_tracking_mode(&self) -> MouseTrackingMode {
        self.mouse_tracking_mode
    }

    pub fn app_captures_mouse(&self) -> bool {
        self.mouse_tracking_mode.captures_mouse()
    }

    pub fn set_alt_screen_policy(&mut self, policy: AltScreenScrollPolicy) {
        self.alt_screen_policy = policy;
    }

    // Drop all scrollback history. Called from the VT ingest path when
    // the shell emits `\x1b[2J` or `\x1b[3J` — which is what `clear`
    // produces on a standard xterm-256color TERM. Without this, `clear`
    // moves the cursor but leaves old output visible in scrollback,
    // which surprises users coming from tmux/xterm/native consoles.
    pub fn clear_scrollback(&mut self) {
        self.scrollback.clear();
    }

    pub fn append_output_line(&mut self, line: ScrollbackLine) {
        self.scrollback.append_line(line);
    }

    pub fn visible_lines(&self) -> Vec<ScrollbackLine> {
        self.scrollback.visible_lines()
    }

    // Convenience used only in tests that compare against plain text.
    pub fn visible_text(&self) -> Vec<String> {
        self.scrollback
            .visible_lines()
            .into_iter()
            .map(|cells| {
                cells
                    .iter()
                    .filter(|c| c.ch != '\0')
                    .map(|c| c.ch)
                    .collect()
            })
            .collect()
    }

    /// Visible viewport snapshot for export. Mirrors
    /// `scrollback_text`'s `strip_ansi` knob; rendered cells are
    /// already escape-free, so the boolean is forward-compat for a
    /// future raw-byte viewport.
    pub fn visible_text_for_export(&self, strip_ansi: bool) -> Vec<String> {
        let raw = self.visible_text();
        if !strip_ansi {
            return raw;
        }
        raw.into_iter()
            .map(|line| strip_ansi_inplace(&line))
            .collect()
    }

    /// True while the scrollback viewport is pinned to the latest
    /// output (the user hasn't scrolled up). Flips back to true once
    /// the viewport reaches bottom again.
    pub fn viewport_following_live(&self) -> bool {
        self.scrollback.follow_output()
    }

    // Writes a line of plain default-styled text. Used by tests.
    #[cfg(test)]
    pub fn append_plain(&mut self, text: &str) {
        let line: ScrollbackLine = text.chars().map(Cell::new).collect();
        self.scrollback.append_line(line);
    }

    pub fn set_viewport_height(&mut self, viewport_height: usize) {
        self.scrollback.set_viewport_height(viewport_height);
    }

    pub fn follow_output(&self) -> bool {
        self.scrollback.follow_output()
    }

    pub fn total_lines(&self) -> usize {
        self.scrollback.total_lines()
    }

    pub fn viewport_height(&self) -> usize {
        self.scrollback.viewport_height()
    }

    pub fn viewport_top(&self) -> usize {
        self.scrollback.viewport_top()
    }

    pub fn wheel_up(&mut self, lines: usize) -> WheelOutcome {
        self.handle_wheel(WheelDirection::Up(lines))
    }

    pub fn wheel_down(&mut self, lines: usize) -> WheelOutcome {
        self.handle_wheel(WheelDirection::Down(lines))
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scrollback.scroll_to_bottom();
    }

    pub fn search_scrollback(&self, needle: &str) -> Vec<usize> {
        self.scrollback.search(needle)
    }

    pub fn center_viewport_on(&mut self, line_index: usize) {
        self.scrollback.center_viewport_on(line_index);
    }

    pub fn ensure_line_visible(&mut self, line_index: usize) {
        self.scrollback.ensure_line_visible(line_index);
    }

    pub fn extract_scrollback_lines(&self, start: usize, end: usize) -> String {
        self.scrollback.extract_lines(start, end)
    }

    pub fn scrollback_viewport_top(&self) -> usize {
        self.scrollback.viewport_top()
    }

    pub fn scrollback_viewport_height(&self) -> usize {
        self.scrollback.viewport_height()
    }

    // Plain-text view of the active visible viewport — the same content
    // the renderer would draw, but as a single string with newline
    // separators. Used by the prompt detector (which only cares about
    // the last non-empty line) and by future MCP tools that need a
    // grep-friendly view of the pane.
    pub fn visible_last_line(&self) -> Option<String> {
        self.visible_text()
            .into_iter()
            .rev()
            .find(|line| !line.trim().is_empty())
    }

    fn handle_wheel(&mut self, direction: WheelDirection) -> WheelOutcome {
        let routing = route_wheel(
            MouseContext {
                screen_mode: self.screen_mode,
                mouse_tracking_mode: self.mouse_tracking_mode,
                alt_screen_policy: self.alt_screen_policy,
            },
            direction,
        );

        match routing {
            WheelRouting::ScrollViewportUp(lines) => WheelOutcome::ViewportChanged {
                lines_scrolled: self.scrollback.scroll_up(lines),
                follow_output: self.scrollback.follow_output(),
            },
            WheelRouting::ScrollViewportDown(lines) => WheelOutcome::ViewportChanged {
                lines_scrolled: self.scrollback.scroll_down(lines),
                follow_output: self.scrollback.follow_output(),
            },
            WheelRouting::SendToApplication => WheelOutcome::PassedToApplication,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use crate::mouse::{AltScreenScrollPolicy, MouseTrackingMode, ScreenMode};

    use super::{Pane, WheelOutcome};

    // Test sink that records writes into a shared Vec<u8> so the test can
    // assert exactly what bytes were captured.
    struct SharedSink(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn pane_capture_writes_raw_bytes_to_sink() {
        // The plan calls this `Pane::for_test(80, 24, 64)`; Pane has no
        // size of its own (terminal/PTY live elsewhere) so the only
        // dimensions that matter are scrollback capacity and viewport
        // height. `Pane::new("test", 64, 24)` is the equivalent.
        let mut pane = Pane::new("test", 64, 24);
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        pane.attach_capture(Box::new(SharedSink(Arc::clone(&buf))));
        // `feed_pty_bytes` in the plan: Pane never ingests PTY bytes
        // itself (Session does), so this drives the capture tap directly.
        pane.mirror_capture(b"\x1b[31mred\x1b[0m");
        let _ = pane.detach_capture();
        assert_eq!(*buf.lock().unwrap(), b"\x1b[31mred\x1b[0m");
    }

    #[test]
    fn pane_output_transcript_slices_by_byte_cursor() {
        let mut pane = Pane::new("test", 64, 24);
        pane.record_output(b"hello");
        assert_eq!(pane.output_byte_cursor(), 5);

        let full = pane.output_since(0, 100);
        assert_eq!(full.start_byte, 0);
        assert_eq!(full.byte_cursor, 5);
        assert_eq!(full.bytes, b"hello");
        assert!(!full.truncated);

        let partial = pane.output_since(2, 2);
        assert_eq!(partial.start_byte, 2);
        assert_eq!(partial.byte_cursor, 5);
        assert_eq!(partial.bytes, b"ll");
        assert!(partial.truncated);

        let cursor_only = pane.output_since(0, 0);
        assert_eq!(cursor_only.start_byte, 5);
        assert_eq!(cursor_only.byte_cursor, 5);
        assert!(cursor_only.bytes.is_empty());
        assert!(!cursor_only.truncated);
    }

    #[test]
    fn wheel_scrolling_moves_the_primary_viewport() {
        let mut pane = Pane::new("shell", 16, 3);
        for index in 1..=5 {
            pane.append_plain(&format!("line {index}"));
        }

        let outcome = pane.wheel_up(2);

        assert_eq!(
            outcome,
            WheelOutcome::ViewportChanged {
                lines_scrolled: 2,
                follow_output: false,
            }
        );
        assert_eq!(pane.visible_text(), vec!["line 1", "line 2", "line 3"]);
    }

    #[test]
    fn strip_ansi_inplace_removes_csi_sequences() {
        use super::strip_ansi_inplace;
        assert_eq!(strip_ansi_inplace("\x1b[31mred\x1b[0m text"), "red text");
        assert_eq!(strip_ansi_inplace("plain"), "plain");
        assert_eq!(strip_ansi_inplace(""), "");
        // OSC-style sequence terminated by BEL.
        assert_eq!(strip_ansi_inplace("\x1b]2;title\x07rest"), "rest");
    }

    #[test]
    fn strip_ansi_inplace_preserves_multibyte_utf8() {
        // Regression: previous impl pushed bytes as `b as char`, which
        // turned every UTF-8 leading/continuation byte into its own
        // Latin-1 codepoint. Box-drawing chars came out as â/•/­ etc.
        // in MCP read_pane output.
        use super::strip_ansi_inplace;
        assert_eq!(strip_ansi_inplace("╭─╮"), "╭─╮");
        assert_eq!(strip_ansi_inplace("│ > prompt"), "│ > prompt");
        assert_eq!(
            strip_ansi_inplace("\x1b[1mbold ▓▓▓\x1b[0m end"),
            "bold ▓▓▓ end"
        );
        // Mixed: ESC sequence between multi-byte glyphs.
        assert_eq!(strip_ansi_inplace("█\x1b[31m█\x1b[0m█"), "███");
    }

    #[test]
    fn pane_carries_default_agent_state_fields() {
        let pane = Pane::new("test", 8, 4);
        assert_eq!(pane.agent_state, crate::agent::AgentState::Idle);
        assert!(pane.label.is_none());
        assert!(pane.last_command.is_none());
        assert!(pane.last_exit.is_none());
    }

    #[test]
    fn scrollback_text_spans_scrollback_and_viewport() {
        // 4-row viewport, 32-line scrollback capacity. Push 10 lines
        // — six are above the viewport in scrollback, four are
        // currently visible. `scrollback_text(8, true)` must return
        // lines 3..=10 (mixing scrollback and viewport), not just the
        // last 4 visible rows. That's the load-bearing assertion: the
        // earlier implementation called `visible_text()` which capped
        // at the viewport height, so the test wrote 5 lines into a
        // 4-row viewport and couldn't catch the discrepancy.
        let mut pane = Pane::new("shell", 32, 4);
        for i in 1..=10 {
            pane.append_plain(&format!("line {i}"));
        }
        let text = pane.scrollback_text(8, true);
        assert_eq!(text.len(), 8);
        assert_eq!(text.first().map(String::as_str), Some("line 3"));
        assert_eq!(text.last().map(String::as_str), Some("line 10"));
        // Asking for more than the buffer holds caps at the buffer
        // size, not the viewport — without the fix this would still
        // be 4.
        let all = pane.scrollback_text(100, true);
        assert_eq!(all.len(), 10);
    }

    #[test]
    fn alternate_screen_can_pass_wheel_events_to_the_application() {
        let mut pane = Pane::new("vim", 16, 3);
        pane.set_screen_mode(ScreenMode::Alternate);
        pane.set_mouse_tracking_mode(MouseTrackingMode::Click);
        pane.set_alt_screen_policy(AltScreenScrollPolicy::PaneScrollback);

        assert_eq!(pane.wheel_up(1), WheelOutcome::PassedToApplication);
    }
}
