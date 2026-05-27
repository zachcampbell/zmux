// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::io;
use std::sync::{Arc, Mutex, mpsc};

use crate::events::{Event, EventBus};
use crate::layout::PaneId;
use crate::mouse::MouseTrackingMode;
use crate::pty::PtySize;
use crate::workspace::{PaneSummaryView, Workspace};

const WINDOW_PANE_ID_STRIDE: PaneId = 10_000;

pub struct WindowPaneSummaryView {
    pub window_index: usize,
    pub active_window: bool,
    pub pane: PaneSummaryView,
}

// A session hosts one-or-more windows. Each window is a full Workspace
// (its own pane tree, active pane, zoom/search/selection/rename state).
// Only the active window is rendered and targeted by input; others
// keep ingesting their shells' output in the background so switching
// to them is immediate.
pub struct WindowSet {
    windows: Vec<Workspace>,
    active: usize,
    session_event_bus: Arc<Mutex<EventBus>>,
    next_window_pane_base: PaneId,
    // Cached so every new window inherits the same config-derived
    // values without re-reading the file. Matches what was used for
    // window 0 at session spawn time.
    shell: String,
    session_name: String,
    scrollback_lines: usize,
    status_bar_hints: bool,
    status_label_override: Option<String>,
}

impl WindowSet {
    pub fn new(
        mut first: Workspace,
        shell: String,
        session_name: String,
        scrollback_lines: usize,
        status_bar_hints: bool,
        status_label_override: Option<String>,
    ) -> Self {
        let session_event_bus = Arc::new(Mutex::new(EventBus::default()));
        first.set_session_event_bus(session_event_bus.clone());
        let mut set = Self {
            windows: vec![first],
            active: 0,
            session_event_bus,
            next_window_pane_base: WINDOW_PANE_ID_STRIDE,
            shell,
            session_name,
            scrollback_lines,
            status_bar_hints,
            status_label_override,
        };
        set.sync_window_indicator();
        set
    }

    pub fn active(&self) -> &Workspace {
        &self.windows[self.active]
    }

    pub fn active_mut(&mut self) -> &mut Workspace {
        &mut self.windows[self.active]
    }

    /// Subscribe to the session-wide event bus used by MCP
    /// `watch_events`. Workspace-local subscribers still exist for UI
    /// overlays; this bus mirrors events from every window so external
    /// controllers can observe background-window panes too.
    pub fn subscribe_events(&mut self) -> mpsc::Receiver<Event> {
        self.session_event_bus
            .lock()
            .expect("session event bus poisoned")
            .subscribe()
    }

    // Look up a pane by id across every window in the set. WindowSet
    // starts each newly-created window at a distinct id base, so MCP
    // callers can target background-window panes without colliding with
    // the original window's low-numbered panes.
    pub fn find_pane_mut(&mut self, pane_id: usize) -> Option<&mut crate::pane::Pane> {
        for window in self.windows.iter_mut() {
            if let Some(pane) = window.pane_by_id_mut(pane_id) {
                return Some(pane);
            }
        }
        None
    }

    /// Cross-window pane-label setter. Returns true iff the label
    /// changed and publishes `Event::LabelChanged` on the owning
    /// workspace's bus.
    pub fn set_pane_label(&mut self, pane_id: u32, label: Option<String>) -> bool {
        for window in self.windows.iter_mut() {
            if window.pane_by_id_mut(pane_id as usize).is_some() {
                return window.set_pane_label(pane_id, label);
            }
        }
        false
    }

    /// Cross-window PTY-input write. Walks every window's workspace
    /// and forwards `bytes` to the first matching pane. Returns
    /// `ErrorKind::NotFound` when no window owns the pane id — keeps
    /// the MCP `send_keys` path single-call (no need to also resolve
    /// which window first). Mirrors `set_pane_label`'s cross-window
    /// walk; with window-specific pane-id ranges there should be only
    /// one match in normal use.
    pub fn send_pty_input(&mut self, pane_id: u32, bytes: &[u8]) -> io::Result<()> {
        for window in self.windows.iter_mut() {
            if window.pane_by_id_mut(pane_id as usize).is_some() {
                return window.send_pty_input(pane_id, bytes);
            }
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no pane with id {pane_id} in any window"),
        ))
    }

    /// Cross-window grid flush. Walks every window and drains the named
    /// pane's primary-screen grid into its scrollback. Used by paths
    /// that explicitly want the grid materialized (e.g. session teardown).
    /// Returns `true` if any window owned the pane and flushed it.
    pub fn flush_pane_grid(&mut self, pane_id: u32) -> bool {
        for window in self.windows.iter_mut() {
            if window.flush_pane_grid(pane_id) {
                return true;
            }
        }
        false
    }

    /// Cross-window non-mutating visible-lines snapshot. Composes the
    /// renderer's view (scrollback tail + live primary grid) for MCP
    /// `read_pane`. Doesn't alter ingest state — keeps a running TUI's
    /// in-place edits intact across reads.
    pub fn snapshot_visible_lines(&self, pane_id: u32) -> Option<Vec<String>> {
        for window in self.windows.iter() {
            if let Some(lines) = window.snapshot_visible_lines(pane_id) {
                return Some(lines);
            }
        }
        None
    }

    /// Cross-window non-mutating tail-of-scrollback snapshot. Same
    /// rationale as `snapshot_visible_lines`; covers MCP `read_pane`'s
    /// scrollback mode.
    pub fn snapshot_scrollback_lines(&self, pane_id: u32, lines: usize) -> Option<Vec<String>> {
        for window in self.windows.iter() {
            if let Some(snapshot) = window.snapshot_scrollback_lines(pane_id, lines) {
                return Some(snapshot);
            }
        }
        None
    }

    /// Cross-window cursor-based raw-output transcript slice.
    pub fn pane_output_since(
        &self,
        pane_id: u32,
        since_byte: u64,
        max_bytes: usize,
    ) -> Option<crate::pane::PaneOutputSlice> {
        for window in self.windows.iter() {
            if let Some(slice) = window.pane_output_since(pane_id, since_byte, max_bytes) {
                return Some(slice);
            }
        }
        None
    }

    /// Cross-window query for whether the named pane's shell has DECSET
    /// 2004 active. First-match wins; mirrors `snapshot_visible_lines`.
    /// Used by the MCP `send_keys` tool to skip the deferred-CR dance
    /// when bracketed paste is supported.
    pub fn pane_bracketed_paste(&self, pane_id: u32) -> Option<bool> {
        for window in self.windows.iter() {
            if let Some(enabled) = window.pane_bracketed_paste(pane_id) {
                return Some(enabled);
            }
        }
        None
    }

    /// Cross-window pane kill. Walks every window's workspace and
    /// closes the first pane that matches. If the target is the sole
    /// pane in a non-final window, close that whole window instead —
    /// this lets MCP controllers clean up worker windows by pane id
    /// while preserving the invariant that the session always has at
    /// least one renderable workspace. Returns `Ok(false)` only when
    /// the target is the final pane in the final window.
    pub fn kill_pane_by_id(&mut self, pane_id: u32) -> io::Result<bool> {
        for index in 0..self.windows.len() {
            let owns_pane = self.windows[index]
                .pane_by_id_mut(pane_id as usize)
                .is_some();
            if !owns_pane {
                continue;
            }
            if self.windows[index].pane_count() == 1 {
                if self.windows.len() <= 1 {
                    return Ok(false);
                }
                return self.close_window_at(index);
            }
            return self.windows[index].kill_pane_by_id(pane_id);
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no pane with id {pane_id} in any window"),
        ))
    }

    pub fn window_count(&self) -> usize {
        self.windows.len()
    }

    pub fn active_index(&self) -> usize {
        self.active
    }

    // Spawn a brand-new window running the user's shell in a fresh
    // single-pane workspace. Sized to the active window's current size
    // so the new window's frame matches what the client is already
    // rendering; the next broadcast will replace the old content.
    pub fn new_window(&mut self) -> io::Result<usize> {
        let size = self.active().size();
        let pane_id = self.allocate_window_pane_base();
        let mut workspace = Workspace::spawn_single_named_with_options_at_pane_id(
            &self.shell,
            size,
            &self.session_name,
            self.scrollback_lines,
            self.status_bar_hints,
            pane_id,
        )?;
        workspace.set_session_event_bus(self.session_event_bus.clone());
        // Carry the status_label_override onto the new window so the
        // left label stays consistent across windows.
        workspace.set_status_label_override(self.status_label_override.clone());
        self.windows.push(workspace);
        self.active = self.windows.len() - 1;
        self.sync_window_indicator();
        self.publish_session_event(Event::PaneSpawned {
            pane_id: pane_id as u32,
            label: None,
        });
        Ok(self.active)
    }

    /// Same as `new_window`, but the genesis pane runs `command`
    /// instead of the user's shell. Used by the MCP `spawn_pane`
    /// tool when split = "window". Returns the new window's pane id,
    /// allocated from a window-specific range so MCP callers can
    /// address it unambiguously across the session.
    pub fn new_window_with_command(&mut self, command: &str) -> io::Result<u32> {
        let size = self.active().size();
        let pane_id = self.allocate_window_pane_base();
        let mut workspace = Workspace::spawn_single_with_command_at_pane_id(
            command,
            size,
            &self.session_name,
            self.scrollback_lines,
            self.status_bar_hints,
            pane_id,
        )?;
        workspace.set_session_event_bus(self.session_event_bus.clone());
        workspace.set_status_label_override(self.status_label_override.clone());
        self.windows.push(workspace);
        self.active = self.windows.len() - 1;
        self.sync_window_indicator();
        self.publish_session_event(Event::PaneSpawned {
            pane_id: pane_id as u32,
            label: None,
        });
        Ok(pane_id as u32)
    }

    fn allocate_window_pane_base(&mut self) -> PaneId {
        let pane_id = self.next_window_pane_base;
        self.next_window_pane_base = self
            .next_window_pane_base
            .saturating_add(WINDOW_PANE_ID_STRIDE);
        pane_id
    }

    pub fn pane_summaries_all(&self) -> Vec<WindowPaneSummaryView> {
        self.windows
            .iter()
            .enumerate()
            .flat_map(|(window_index, window)| {
                let active_window = window_index == self.active;
                window
                    .pane_summaries()
                    .into_iter()
                    .map(move |pane| WindowPaneSummaryView {
                        window_index,
                        active_window,
                        pane,
                    })
            })
            .collect()
    }

    // Jump straight to a specific window by 0-based index. Used by the
    // Ctrl-a <digit> bindings: digit key `1` → index 0, digit `9` →
    // index 8. Out-of-range indices are a no-op (not an error), which
    // matches tmux's behavior when a user presses a digit for a window
    // that doesn't exist. Returns false when the target is either
    // out-of-range or already active — both cases should not trigger
    // a redraw.
    pub fn select_window(&mut self, index: usize) -> bool {
        if index >= self.windows.len() || index == self.active {
            return false;
        }
        self.active = index;
        self.sync_window_indicator();
        true
    }

    pub fn next_window(&mut self) -> bool {
        if self.windows.len() <= 1 {
            return false;
        }
        self.active = (self.active + 1) % self.windows.len();
        self.sync_window_indicator();
        true
    }

    pub fn previous_window(&mut self) -> bool {
        if self.windows.len() <= 1 {
            return false;
        }
        self.active = (self.active + self.windows.len() - 1) % self.windows.len();
        self.sync_window_indicator();
        true
    }

    // Close the active window. Returns Ok(true) if a window was
    // actually removed; Ok(false) when there was only one window left
    // (the caller should treat that case as "shutdown the server"
    // since closing the final window would leave nothing to render).
    pub fn close_active_window(&mut self) -> io::Result<bool> {
        self.close_window_at(self.active)
    }

    fn close_window_at(&mut self, index: usize) -> io::Result<bool> {
        if self.windows.len() <= 1 || index >= self.windows.len() {
            return Ok(false);
        }
        self.windows[index].close_all_panes_for_window_removal()?;
        self.windows.remove(index);
        if self.active > index {
            self.active -= 1;
        } else if self.active >= self.windows.len() {
            self.active = self.windows.len() - 1;
        }
        self.sync_window_indicator();
        Ok(true)
    }

    // Resize every window, not just the active one, so switching to a
    // background window after a client resize shows content at the new
    // size instead of the pre-resize slots.
    pub fn resize(&mut self, size: PtySize) -> io::Result<()> {
        for window in self.windows.iter_mut() {
            window.resize(size)?;
        }
        Ok(())
    }

    // Propagate try_wait across every window's panes. A shell dying in
    // a background window still marks its pane as `exit N` and must be
    // visible immediately when the user switches back to that window.
    pub fn update_exit_statuses(&mut self) -> io::Result<bool> {
        let mut active_dirty = false;
        for (index, window) in self.windows.iter_mut().enumerate() {
            let dirty = window.update_exit_statuses()?;
            if dirty && index == self.active {
                active_dirty = true;
            }
        }
        Ok(active_dirty)
    }

    // The server exits (and the socket gets removed) only when EVERY
    // window's shells have exited. As long as any background window
    // still has a live shell, the session stays up.
    pub fn exit_code_if_complete(&self) -> Option<i32> {
        let mut final_code: Option<i32> = None;
        for window in self.windows.iter() {
            let code = window.exit_code_if_complete()?;
            // First non-zero wins; otherwise keep propagating 0.
            match final_code {
                None => final_code = Some(code),
                Some(0) if code != 0 => final_code = Some(code),
                _ => {}
            }
        }
        final_code
    }

    // Drain stdout from every window's panes so shells in background
    // windows make forward progress. Returns true when the ACTIVE
    // window produced new output (which is all the caller needs to
    // decide whether to broadcast a frame).
    pub fn ingest_available_output(&mut self) -> io::Result<bool> {
        let mut active_dirty = false;
        for (index, window) in self.windows.iter_mut().enumerate() {
            let dirty = window.ingest_available_output()?;
            if dirty && index == self.active {
                active_dirty = true;
            }
        }
        Ok(active_dirty)
    }

    /// Per-frame agent-state tick across every window — background
    /// windows still get their idle detectors checked so the
    /// supervisor overlay sees consistent state regardless of which
    /// window is focused.
    pub fn tick_agents(&mut self, now: std::time::Instant) {
        for window in self.windows.iter_mut() {
            window.tick_agents(now);
        }
    }

    // Mouse tracking mode of the active window — what the client needs
    // to configure the terminal for. Non-active windows can disagree;
    // we only care about the frontmost one.
    pub fn mouse_tracking_mode(&self) -> MouseTrackingMode {
        self.active().mouse_tracking_mode()
    }

    pub fn size(&self) -> PtySize {
        self.active().size()
    }

    // Push a `[w:N/M]` tag into the active window's status label so
    // multi-window users can see at a glance where they are. Single-
    // window sessions skip the tag so the bar looks identical to the
    // pre-windows UI.
    fn sync_window_indicator(&mut self) {
        let indicator = if self.windows.len() == 1 {
            self.status_label_override.clone()
        } else {
            let base = self
                .status_label_override
                .clone()
                .unwrap_or_else(|| format!("{}@{}", self.session_name, read_hostname_fallback()));
            Some(format!(
                "{} [w:{}/{}]",
                base,
                self.active + 1,
                self.windows.len()
            ))
        };
        for window in self.windows.iter_mut() {
            window.set_status_label_override(indicator.clone());
        }
    }

    fn publish_session_event(&mut self, event: Event) {
        if let Ok(mut bus) = self.session_event_bus.lock() {
            bus.publish(event);
        }
    }
}

// WindowSet recomputes the label itself when switching windows, which
// means it needs the hostname. Workspace has its own private
// `read_hostname` helper; reproducing it here keeps the module
// boundary clean rather than re-exporting one function just for this.
fn read_hostname_fallback() -> String {
    if let Ok(name) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
        return name.trim().to_string();
    }
    std::env::var("HOSTNAME").unwrap_or_else(|_| "host".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DEFAULT_SCROLLBACK_LINES, DEFAULT_STATUS_BAR_HINTS};

    fn build() -> WindowSet {
        let workspace = Workspace::spawn_single_named_with_options(
            "/bin/sh",
            PtySize::new(24, 80),
            "test",
            DEFAULT_SCROLLBACK_LINES,
            DEFAULT_STATUS_BAR_HINTS,
        )
        .expect("spawn initial workspace");
        WindowSet::new(
            workspace,
            "/bin/sh".to_string(),
            "test".to_string(),
            DEFAULT_SCROLLBACK_LINES,
            DEFAULT_STATUS_BAR_HINTS,
            None,
        )
    }

    #[test]
    fn new_window_grows_the_set_and_focuses_the_newcomer() {
        let mut set = build();
        assert_eq!(set.window_count(), 1);
        assert_eq!(set.active_index(), 0);

        let created = set.new_window().expect("new window");
        assert_eq!(created, 1);
        assert_eq!(set.window_count(), 2);
        assert_eq!(set.active_index(), 1);
    }

    #[test]
    fn next_and_previous_wrap_around() {
        let mut set = build();
        set.new_window().unwrap();
        set.new_window().unwrap();
        assert_eq!(set.window_count(), 3);
        assert_eq!(set.active_index(), 2);

        assert!(set.next_window());
        assert_eq!(set.active_index(), 0);
        assert!(set.previous_window());
        assert_eq!(set.active_index(), 2);
    }

    #[test]
    fn cycling_with_one_window_is_a_noop() {
        let mut set = build();
        assert!(!set.next_window());
        assert!(!set.previous_window());
    }

    #[test]
    fn select_window_jumps_to_requested_index() {
        let mut set = build();
        set.new_window().unwrap();
        set.new_window().unwrap();
        assert_eq!(set.active_index(), 2);

        // Jump to index 0; returns true because the active index changed.
        assert!(set.select_window(0));
        assert_eq!(set.active_index(), 0);

        // Selecting the current active window is a no-op.
        assert!(!set.select_window(0));

        // Out-of-range is silently rejected — keeps the keyboard
        // binding honest when the user presses Ctrl-a 9 and only four
        // windows exist.
        assert!(!set.select_window(99));
        assert_eq!(set.active_index(), 0);
    }

    #[test]
    fn close_active_window_refuses_when_one_remains() {
        let mut set = build();
        assert!(!set.close_active_window().unwrap());
        assert_eq!(set.window_count(), 1);
    }

    #[test]
    fn close_active_window_reassigns_active_index() {
        let mut set = build();
        set.new_window().unwrap();
        assert_eq!(set.active_index(), 1);

        // Close the last window: active should fall back to window 0.
        assert!(set.close_active_window().unwrap());
        assert_eq!(set.window_count(), 1);
        assert_eq!(set.active_index(), 0);
    }

    #[test]
    fn find_pane_mut_locates_panes_in_non_active_windows() {
        // New windows start at a distinct pane-id base so MCP callers
        // can address panes by id without accidentally hitting window 0.
        let mut set = build();
        set.new_window().unwrap();
        assert_eq!(set.window_count(), 2);
        assert_eq!(set.active_index(), 1);

        assert!(set.find_pane_mut(1).is_some());
        assert!(set.find_pane_mut(WINDOW_PANE_ID_STRIDE).is_some());

        set.select_window(1);
        set.active_mut()
            .split_active(crate::layout::SplitOrientation::Columns)
            .unwrap();
        let background_split = WINDOW_PANE_ID_STRIDE + 1;
        set.select_window(0);
        assert_eq!(set.active_index(), 0);
        assert!(
            set.find_pane_mut(background_split).is_some(),
            "pane id {background_split} lives in the non-active window 1; find_pane_mut must reach it",
        );
        // And a truly missing id is still None.
        assert!(set.find_pane_mut(999).is_none());
    }

    #[test]
    fn pane_summaries_all_includes_background_windows() {
        let mut set = build();
        set.new_window().unwrap();

        let summaries = set.pane_summaries_all();
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].window_index, 0);
        assert!(!summaries[0].active_window);
        assert_eq!(summaries[0].pane.pane_id, 1);
        assert_eq!(summaries[1].window_index, 1);
        assert!(summaries[1].active_window);
        assert_eq!(summaries[1].pane.pane_id, WINDOW_PANE_ID_STRIDE as u32);
    }

    #[test]
    fn status_label_includes_window_indicator_when_multi_window() {
        let mut set = build();
        let single = set.active().render_frame();
        assert!(
            !single.iter().any(|line| line.contains("[w:")),
            "single-window session should not show [w:...] indicator",
        );

        set.new_window().unwrap();
        let multi = set.active().render_frame();
        assert!(
            multi.iter().any(|line| line.contains("[w:2/2]")),
            "status bar should show active/total window indicator: {multi:?}",
        );
    }
}
