// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::io;

use crate::input::MouseEvent;
use crate::mouse::MouseTrackingMode;
use crate::pane::WheelOutcome;
use crate::pane::{Pane, PaneOutputSlice};
use crate::pty::{PtyProcess, PtySize};
use crate::terminal::TerminalIngest;
use crate::trace::{TraceContext, TraceHub, TraceKind};

#[derive(Debug)]
pub struct Session {
    pane: Pane,
    pty: PtyProcess,
    ingest: TerminalIngest,
    trace: Option<(TraceHub, TraceContext)>,
}

impl Session {
    pub fn spawn_command(
        title: impl Into<String>,
        program: &str,
        args: &[&str],
        size: PtySize,
        scrollback_capacity: usize,
        viewport_height: usize,
    ) -> io::Result<Self> {
        let pane = Pane::new(title, scrollback_capacity, viewport_height);
        let pty = PtyProcess::spawn(program, args, size)?;

        Ok(Self {
            pane,
            pty,
            ingest: TerminalIngest::new(size),
            trace: None,
        })
    }

    pub fn write_input(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.pty.write_all(bytes)?;
        self.trace_bytes(TraceKind::PaneInput, bytes);
        Ok(())
    }

    /// Attach this PTY to the daemon's session-level trace controller.
    /// The hub is cheap to clone and a no-op while tracing is disabled.
    pub fn set_trace(&mut self, hub: TraceHub, context: TraceContext) {
        self.trace = Some((hub, context));
    }

    fn trace_bytes(&self, kind: TraceKind, bytes: &[u8]) {
        if let Some((hub, context)) = &self.trace {
            let _ = hub.record_bytes(kind, *context, bytes);
        }
    }

    fn trace_resize(&self, size: PtySize) {
        if let Some((hub, context)) = &self.trace {
            if !hub.is_active() {
                return;
            }
            let _ = hub.record_json(
                TraceKind::Resize,
                *context,
                &serde_json::json!({ "rows": size.rows, "cols": size.cols }),
            );
        }
    }

    fn trace_terminal_state(&self, output_bytes: usize) {
        if let Some((hub, context)) = &self.trace {
            if !hub.is_active() {
                return;
            }
            let _ = hub.record_json(
                TraceKind::State,
                *context,
                &serde_json::json!({
                    "event": "terminal_state",
                    "output_bytes": output_bytes,
                    "screen_mode": format!("{:?}", self.screen_mode()),
                    "mouse_tracking": format!("{:?}", self.mouse_tracking_mode()),
                    "follow_output": self.follow_output(),
                    "viewport_top": self.pane.viewport_top(),
                    "viewport_height": self.pane.viewport_height(),
                    "bracketed_paste": self.bracketed_paste_enabled(),
                }),
            );
        }
    }

    // Mutable access to the underlying Pane. Used by the daemon's
    // Capture admin handler to attach a file-backed capture sink.
    pub fn pane_mut(&mut self) -> &mut Pane {
        &mut self.pane
    }

    pub fn pane(&self) -> &Pane {
        &self.pane
    }

    /// Drain the live primary-screen grid into pane scrollback. Snapshot
    /// readers (MCP `read_pane`, watchers, etc.) need this because the
    /// 2D grid model keeps the latest streamed rows editable in memory
    /// rather than appending them to scrollback line-by-line. Without an
    /// explicit flush, those readers see stale scrollback and miss the
    /// content the user is actually looking at on screen. No-op while
    /// the alt screen is active.
    ///
    /// DESTRUCTIVE — this empties the live primary grid and resets the
    /// cursor. It must never be called from an interactive input path
    /// (mouse selection, search, keyboard scrolling, etc.). Doing so
    /// caused a real regression: a bare click in a shell pane (mouse
    /// press+release, no drag) flushed the grid, and the *next* bit of
    /// follow-mode output then rendered top-aligned on an otherwise
    /// blank viewport instead of appending under existing content,
    /// because `render_primary_cells`'s follow-mode branch pads a
    /// near-empty grid with blank rows *below* it — correct after a
    /// real `clear`, wrong here. Interactive code that needs to read
    /// scrollback-plus-live-grid state should use the non-destructive
    /// `combined_*` accessors below instead, which address scrollback
    /// and the live grid as one continuous index space without
    /// touching either.
    pub fn flush_grid_to_scrollback(&mut self) {
        self.ingest.flush_incomplete_line(&mut self.pane);
    }

    /// Non-mutating snapshot of what the renderer would draw right now,
    /// composed from scrollback + the live primary grid. Used by MCP
    /// `read_pane` so callers see in-progress TUI state without forcing
    /// the live grid into scrollback (which would let the running TUI's
    /// next CUU find an empty grid and cascade — see the cascade-fix
    /// commit). Returns a vector of plain-text lines, oldest first.
    ///
    /// When the live grid is empty (e.g., a primary-screen TUI like
    /// gemini-cli has just done `\e[2J` mid-redraw, or has scrolled all
    /// content out into scrollback), fall back to the scrollback tail.
    /// The live attached terminal still shows the empty viewport (that's
    /// what a real terminal would show), but a snapshot consumer (an
    /// agent reading via MCP) gets the recent visible content. This
    /// trades exact "what's on screen right now" for "what was visible
    /// recently", which is the more useful answer for read_pane callers.
    pub fn snapshot_visible_lines(&self) -> Vec<String> {
        let rendered = self.ingest.render_lines(&self.pane);
        if !rendered.is_empty() {
            return rendered;
        }
        let viewport = self.pane.viewport_height().max(1);
        // strip_ansi=true returns plain text; today both branches do the
        // same thing (cells are already escape-free), but match the
        // long-standing read_pane contract.
        self.pane.scrollback_text(viewport, true)
    }

    /// Cell-level counterpart to `snapshot_visible_lines`: identical
    /// composition and empty-grid fallback, but the cells keep their
    /// SGR style so MCP `read_pane`'s `strip_ansi=false` path can
    /// re-serialize real ANSI instead of returning already-plain text.
    pub fn snapshot_visible_cells(&self) -> Vec<Vec<crate::style::Cell>> {
        let rendered = self.ingest.render_visible_cells(&self.pane);
        if !rendered.is_empty() {
            return rendered;
        }
        let viewport = self.pane.viewport_height().max(1);
        self.pane.scrollback_cells(viewport)
    }

    /// Non-mutating snapshot of `lines` most-recent rows, spanning
    /// scrollback and the live primary grid. The grid contributes its
    /// cell-rows directly (oldest first); any remaining capacity is
    /// filled from the tail of scrollback. Useful for MCP `read_pane`'s
    /// scrollback mode where the caller wants more than just the
    /// viewport.
    pub fn snapshot_scrollback_lines(&self, lines: usize) -> Vec<String> {
        // Pull the grid as plain text (filtering out wide-char `\0`
        // continuation sentinels for parity with `Pane::scrollback_text`).
        let grid_text: Vec<String> = self.ingest.primary_grid_text();
        let total_grid = grid_text.len();
        if lines == 0 {
            return Vec::new();
        }
        if total_grid >= lines {
            // Caller asked for fewer lines than the grid holds; serve
            // the grid's tail. (Rare in practice — a few-row input box
            // vs. a 200-line ask — but the math has to be honest.)
            return grid_text[total_grid - lines..].to_vec();
        }
        // Need history to fill the gap. Pull scrollback's tail and
        // append the full grid.
        let want_history = lines - total_grid;
        let mut history = self.pane.scrollback_text(want_history, true);
        history.extend(grid_text);
        history
    }

    /// Cell-level counterpart to `snapshot_scrollback_lines`: same
    /// history-plus-grid composition, but returning styled cells.
    /// Scrollback and the live grid both already store `Cell`s, so
    /// this needs no raw-byte plumbing — see MCP `read_pane`'s
    /// `strip_ansi=false` path.
    pub fn snapshot_scrollback_cells(&self, lines: usize) -> Vec<Vec<crate::style::Cell>> {
        let grid_cells = self.ingest.primary_grid_cells();
        let total_grid = grid_cells.len();
        if lines == 0 {
            return Vec::new();
        }
        if total_grid >= lines {
            return grid_cells[total_grid - lines..].to_vec();
        }
        let want_history = lines - total_grid;
        let mut history = self.pane.scrollback_cells(want_history);
        history.extend(grid_cells);
        history
    }

    pub fn ingest_available_output(&mut self) -> io::Result<usize> {
        let bytes = self.pty.read_available()?;
        let count = bytes.len();
        if count > 0 {
            self.trace_bytes(TraceKind::PaneOutput, &bytes);
            // Optional raw-byte capture for debugging ingest issues. Set
            // ZMUX_PTY_DUMP to a file path to log everything we read from
            // this pane's PTY. The file is append-only and shared across
            // panes, which is fine for a single-shell debug session.
            if let Ok(path) = std::env::var("ZMUX_PTY_DUMP") {
                use std::io::Write as _;
                if let Ok(mut file) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                {
                    let _ = file.write_all(&bytes);
                }
            }
            // Mirror raw PTY bytes to any attached capture sink before
            // the ingester consumes them. This is the wire for `zmux
            // capture` — same chunking the terminal sees, no parsing.
            self.pane.record_output(&bytes);
            self.pane.mirror_capture(&bytes);
            let replies = self.ingest.ingest_bytes(&mut self.pane, &bytes);
            if !replies.is_empty() {
                self.write_input(&replies)?;
            }
            self.trace_terminal_state(count);
        }

        Ok(count)
    }

    pub fn resize(&mut self, size: PtySize, viewport_height: usize) -> io::Result<()> {
        self.pty.resize(size)?;
        let resize = self.ingest.resize(size);
        for row in resize.evicted_primary_rows {
            self.pane.append_output_line(row);
        }
        self.pane.set_scrollback_live_tail(resize.live_tail);
        self.pane.set_viewport_height(viewport_height);
        self.trace_resize(size);
        Ok(())
    }

    pub fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        self.pty.try_wait()
    }

    pub fn close(&mut self) -> io::Result<std::process::ExitStatus> {
        self.pty.kill()
    }

    pub fn wheel_up(&mut self, lines: usize) {
        let _ = self.pane.wheel_up(lines);
    }

    pub fn wheel_down(&mut self, lines: usize) {
        let _ = self.pane.wheel_down(lines);
    }

    pub fn scroll_to_bottom(&mut self) {
        self.pane.scroll_to_bottom();
    }

    pub fn search_scrollback(&self, needle: &str) -> Vec<usize> {
        self.pane.search_scrollback(needle)
    }

    pub fn center_viewport_on(&mut self, line_index: usize) {
        self.pane.center_viewport_on(line_index);
    }

    pub fn ensure_line_visible(&mut self, line_index: usize) {
        self.pane.ensure_line_visible(line_index);
    }

    pub fn extract_scrollback_lines(&self, start: usize, end: usize) -> String {
        self.pane.extract_scrollback_lines(start, end)
    }

    // See `Pane::scrollback_line_cells` — cell-level line access that
    // preserves wide-char continuation sentinels for visual-column
    // slicing.
    pub fn scrollback_line_cells(&self, index: usize) -> crate::scrollback::ScrollbackLine {
        self.pane.scrollback_line_cells(index)
    }

    // Combined-timeline read accessors: committed scrollback plus the
    // live primary grid, addressed as one continuous index space, with
    // no flush and no mutation. These exist so interactive code
    // (mouse selection, `/` search) can look up arbitrary lines —
    // including lines still sitting unflushed in the live grid, or a
    // range that spans the scrollback/live-grid boundary — without
    // calling `flush_grid_to_scrollback`. See that method's doc
    // comment for the regression this replaces it for. Combined
    // indices are stable across grid→scrollback eviction: when a row
    // evicts, scrollback's length grows by exactly the amount the
    // grid's contribution to the index space shrinks, so a captured
    // index still names the same line before and after.
    pub fn combined_line_cells(&self, index: usize) -> crate::scrollback::ScrollbackLine {
        self.ingest.combined_line_cells(&self.pane, index)
    }

    pub fn combined_total_lines(&self) -> usize {
        self.ingest.combined_total_lines(&self.pane)
    }

    pub fn combined_extract_lines(&self, start: usize, end: usize) -> String {
        self.ingest.combined_extract_lines(&self.pane, start, end)
    }

    pub fn combined_search(&self, needle: &str) -> Vec<usize> {
        self.ingest.combined_search(&self.pane, needle)
    }

    pub fn output_byte_cursor(&self) -> u64 {
        self.pane.output_byte_cursor()
    }

    pub fn output_since(&self, since_byte: u64, max_bytes: usize) -> PaneOutputSlice {
        self.pane.output_since(since_byte, max_bytes)
    }

    pub fn scrollback_viewport_top(&self) -> usize {
        self.pane.scrollback_viewport_top()
    }

    pub fn scrollback_viewport_height(&self) -> usize {
        self.pane.scrollback_viewport_height()
    }

    pub fn follow_output(&self) -> bool {
        self.pane.follow_output()
    }

    // Viewport-relative cursor cell for the attached client's host
    // cursor, or None when the app hid it via DECTCEM. See
    // `TerminalIngest::screen_cursor`.
    pub fn screen_cursor(&self) -> Option<(usize, usize)> {
        self.ingest.screen_cursor(&self.pane)
    }

    pub fn total_lines(&self) -> usize {
        self.pane.total_lines()
    }

    pub fn screen_mode(&self) -> crate::mouse::ScreenMode {
        self.pane.screen_mode()
    }

    pub fn app_captures_mouse(&self) -> bool {
        self.pane.app_captures_mouse()
    }

    pub fn mouse_tracking_mode(&self) -> MouseTrackingMode {
        self.pane.mouse_tracking_mode()
    }

    // True when the shell turned on DECSET 2004 — i.e., it expects any
    // pasted text to be bracketed with `ESC[200~ ... ESC[201~`. Workspace
    // checks this before writing the paste buffer to the PTY.
    pub fn bracketed_paste_enabled(&self) -> bool {
        self.ingest.bracketed_paste_enabled()
    }

    // True when the shell turned on DECSET 1004 — i.e., it wants
    // `ESC[I` on focus gain and `ESC[O` on focus loss. Workspace checks
    // this on every focus transition before writing the markers.
    pub fn focus_events_enabled(&self) -> bool {
        self.ingest.focus_events_enabled()
    }

    pub fn rendered_line_count(&self) -> usize {
        self.ingest.rendered_line_count(&self.pane)
    }

    pub fn render_lines(&self) -> Vec<String> {
        self.ingest.render_lines(&self.pane)
    }

    pub fn render_cells(&self) -> Vec<Vec<crate::style::Cell>> {
        self.ingest.render_cells(&self.pane)
    }

    /// Plain-text last non-empty line of what the renderer would draw
    /// right now — composed from scrollback + the live primary grid,
    /// same as `snapshot_visible_lines`. Used by the agent-state prompt
    /// detector.
    ///
    /// This intentionally does NOT delegate to `Pane::visible_last_line`:
    /// that method only ever sees committed scrollback (`Pane::
    /// visible_text` -> `ScrollbackBuffer::visible_lines`), so while the
    /// pane is following live output it always reports the line that
    /// was on screen `grid_rows` writes ago instead of the line that's
    /// actually on screen now — the same staleness this fix's combined
    /// timeline addresses for scrolling. Prompt detection needs "what's
    /// on screen right now," so it goes through the same composed
    /// render as `snapshot_visible_lines` instead.
    pub fn visible_last_line(&self) -> Option<String> {
        self.snapshot_visible_lines()
            .into_iter()
            .rev()
            .find(|line| !line.trim().is_empty())
    }

    pub fn handle_mouse_event(
        &mut self,
        mouse: MouseEvent,
        wheel_scroll_lines: usize,
    ) -> io::Result<bool> {
        if let Some(lines) = mouse.wheel_lines(wheel_scroll_lines) {
            let outcome = if mouse.is_scroll_up() {
                self.pane.wheel_up(lines)
            } else {
                self.pane.wheel_down(lines)
            };

            return match outcome {
                WheelOutcome::ViewportChanged { .. } => Ok(true),
                WheelOutcome::PassedToApplication => {
                    self.write_input(&mouse.encode_sgr())?;
                    Ok(true)
                }
            };
        }

        if self.pane.app_captures_mouse() {
            self.write_input(&mouse.encode_sgr())?;
            return Ok(true);
        }

        Ok(false)
    }

    pub fn drain_to_completion(mut self) -> io::Result<CompletedSession> {
        let bytes = self.pty.read_to_end()?;
        self.trace_bytes(TraceKind::PaneOutput, &bytes);
        self.pane.mirror_capture(&bytes);
        let _ = self.ingest.ingest_bytes(&mut self.pane, &bytes);
        self.ingest.flush_incomplete_line(&mut self.pane);
        let exit_status = self.pty.wait()?;

        Ok(CompletedSession {
            pane: self.pane,
            exit_status,
        })
    }
}

#[derive(Debug)]
pub struct CompletedSession {
    pane: Pane,
    exit_status: std::process::ExitStatus,
}

impl CompletedSession {
    pub fn pane(&self) -> &Pane {
        &self.pane
    }

    pub fn exit_status(&self) -> std::process::ExitStatus {
        self.exit_status
    }
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::Duration;

    use crate::input::MouseEvent;
    use crate::mouse::ScreenMode;
    use crate::pty::PtySize;

    use super::Session;

    // Test sink that records writes into a shared Vec<u8> so the test
    // can assert exactly what bytes the capture tap saw.
    struct SharedSink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for SharedSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn capture_sink_receives_bytes_from_real_ingest_path() {
        // Regression for the "tests don't cover the wired path" gap:
        // pane.rs already proves mirror_capture works in isolation, but
        // that test would still pass if Session::ingest_available_output
        // (or drain_to_completion) stopped calling mirror_capture. This
        // test runs a real PTY-backed Session, attaches a sink to the
        // pane *before* draining, and asserts the bytes the shell wrote
        // landed in the sink — proving the mirror is wired into the
        // real ingest path used by the daemon.
        let mut session = Session::spawn_command(
            "shell",
            "/bin/sh",
            &["-lc", "printf hello"],
            PtySize::new(24, 80),
            32,
            8,
        )
        .expect("spawn session");

        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        session
            .pane_mut()
            .attach_capture(Box::new(SharedSink(std::sync::Arc::clone(&buf))));

        let completed = session.drain_to_completion().expect("drain session");
        assert!(completed.exit_status().success());

        let captured = buf.lock().unwrap().clone();
        assert!(
            captured.windows(b"hello".len()).any(|w| w == b"hello"),
            "expected captured stream to contain `hello`, got {:?}",
            String::from_utf8_lossy(&captured),
        );
    }

    #[test]
    fn command_output_flows_into_the_pane_scrollback() {
        let session = Session::spawn_command(
            "shell",
            "/bin/sh",
            &["-lc", "printf '\\033[32mhello\\033[0m\\nworld\\n'"],
            PtySize::new(24, 80),
            32,
            8,
        )
        .expect("spawn session");

        let completed = session.drain_to_completion().expect("drain session");

        assert!(completed.exit_status().success());
        assert_eq!(completed.pane().visible_text(), vec!["hello", "world"]);
    }

    #[test]
    fn alternate_screen_mouse_events_can_be_forwarded_to_the_application() {
        let mut session = Session::spawn_command(
            "cat-mouse",
            "/bin/sh",
            &[
                "-lc",
                "stty raw -echo; printf '\\033[?1049h\\033[?1000h\\033[?1006h'; dd bs=1 count=10 2>/dev/null | cat -v",
            ],
            PtySize::new(6, 24),
            32,
            6,
        )
        .expect("spawn session");

        for _ in 0..10 {
            let _ = session.ingest_available_output().expect("ingest output");
            if session.screen_mode() == ScreenMode::Alternate && session.app_captures_mouse() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }

        assert_eq!(session.screen_mode(), ScreenMode::Alternate);
        assert!(session.app_captures_mouse());

        session
            .handle_mouse_event(
                MouseEvent {
                    button: 64,
                    col: 4,
                    row: 2,
                    final_byte: b'M',
                },
                1,
            )
            .expect("forward wheel event");

        for _ in 0..10 {
            if session
                .ingest_available_output()
                .expect("ingest echoed mouse output")
                > 0
            {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }

        let rendered = session.render_lines().join("\n");
        assert!(rendered.contains("^[[<64;5;3M"));

        let completed = session.drain_to_completion().expect("drain session");
        assert!(completed.exit_status().success());
    }

    #[test]
    fn shrinking_rows_moves_primary_rows_into_scrollback() {
        let mut session = Session::spawn_command(
            "resize-history",
            "/bin/sh",
            &["-lc", "sleep 5"],
            PtySize::new(4, 20),
            32,
            4,
        )
        .expect("spawn session");
        session
            .ingest
            .ingest_bytes(&mut session.pane, b"one\r\ntwo\r\nthree\r\nfour");

        session
            .resize(PtySize::new(2, 20), 2)
            .expect("resize session");

        assert_eq!(
            session.snapshot_scrollback_lines(4),
            vec!["one", "two", "three", "four"],
            "rows removed by a terminal shrink must remain in history"
        );
        let _ = session.close();
    }
}
