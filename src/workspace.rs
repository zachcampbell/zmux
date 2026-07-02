// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::io;
use std::mem::MaybeUninit;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::agent::{AgentState, IdleDetector, PromptDetector};
use crate::config::{DEFAULT_SCROLLBACK_LINES, DEFAULT_STATUS_BAR_HINTS};
use crate::events::{Event, EventBus};
use crate::input::{InputAction, MouseEvent};
use crate::layout::{
    LayoutNode, MIN_PANE_WIDTH, PaneId, PaneLayout, Rect, ResizeDirection, SplitOrientation,
    WorkspaceLayout,
};
use crate::mouse::{MouseTrackingMode, ScreenMode};
use crate::pty::PtySize;
use crate::session::Session;
use crate::style::{Attrs, Cell, Color, Style, serialize_row};
use crate::supervisor::{
    BroadcastFilter, BroadcastState, SupervisorRow, SupervisorState, render_supervisor,
};
use crate::workspace_render::{draw_big_digits, stamp_cells, stamp_row_text, stamp_text};

// Default cadence for `IdleDetector::tick`; the workspace's per-frame
// poll honors this even when the user changes it via `[agent]` config.
const DEFAULT_IDLE_THRESHOLD: Duration = Duration::from_millis(750);
// Threshold for emitting a `PaneOutput` event from a single ingest:
// at least this many bytes OR at least this much wall time since the
// last emit. Keeps high-rate streams (build logs) from flooding
// subscribers without losing low-rate notifications (single-line
// status updates).
const PANE_OUTPUT_BYTE_THRESHOLD: u64 = 64;
const PANE_OUTPUT_TIME_THRESHOLD: Duration = Duration::from_millis(100);

// Minimal subset of the POSIX `struct tm` fields, used to format local
// wall-clock time for the status bar. Order matches glibc/musl layout on
// Linux (sec, min, hour, ...); the trailing reserved block keeps us safe
// against the differing tail fields (tm_gmtoff, tm_zone) across libcs
// since we never read past tm_sec/min/hour.
#[repr(C)]
struct PosixTm {
    tm_sec: i32,
    tm_min: i32,
    tm_hour: i32,
    tm_mday: i32,
    tm_mon: i32,
    tm_year: i32,
    tm_wday: i32,
    tm_yday: i32,
    tm_isdst: i32,
    _reserved: [u8; 32],
}

unsafe extern "C" {
    fn localtime_r(timep: *const i64, result: *mut PosixTm) -> *mut PosixTm;
}

// State for the active command-prompt overlay.  Two variants because the
// commit path differs: `SplitWith` spawns a pane; `General` returns the
// buffer to the caller for dispatch.
#[derive(Debug)]
enum PromptState {
    SplitWith {
        orientation: SplitOrientation,
        buffer: String,
    },
    General {
        buffer: String,
    },
}

// What the supervisor UI state machine asked the caller to do after a
// key press. Returned by `Workspace::supervisor_key`; executed either
// window-locally (`Workspace::supervisor_handle_key`) or session-wide
// (`WindowSet::supervisor_handle_key`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SupervisorAction {
    Attach(u32),
    Kill(u32),
    SetLabel(u32, Option<String>),
    Broadcast(String, Vec<u32>),
}

/// Public mirror of `PromptState` variants — returned by
/// [`Workspace::active_prompt_kind`] so callers can branch on the kind without
/// access to the private `PromptState` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    SplitWith,
    General,
}

#[derive(Debug)]
pub struct Workspace {
    sessions: Vec<ManagedSession>,
    active: PaneId,
    tree: LayoutNode,
    layout: WorkspaceLayout,
    size: PtySize,
    shell: String,
    // Pane ids are workspace-local for keyboard/UI splits, but
    // WindowSet starts each new window at a distinct base so MCP
    // cross-window addressing can target panes unambiguously.
    next_pane_id: PaneId,
    // Session name + hostname are fixed for the life of this workspace and
    // shown in the top-left of the status bar. They're passed in by the
    // daemon so the server can label the session however it was created.
    session_name: String,
    hostname: String,
    // Optional config override for the status-bar left label. When Some,
    // replaces the default `{session_name}@{hostname}` string.
    status_label_override: Option<String>,
    // When Some, the renderer overlays a centered large digit on each
    // pane until `now >= instant`. None means the overlay is not active.
    pane_number_overlay_until: Option<Instant>,
    // When Some, one pane occupies the full body rect and siblings are
    // hidden. Toggled by Ctrl-a z. Any structural mutation (split, close,
    // cycle, resize) implicitly clears this back to None so the user's
    // next layout action always starts from the tree's real state.
    zoomed: Option<PaneId>,
    // Scrollback-search state for the active pane. `search_input`
    // tracks an in-progress prompt (`/` mode); `search_results` tracks
    // committed matches the user is navigating with n/N. Both are
    // always paired with `self.active`: switching panes via cycle or
    // mouse clears them so results from pane A don't leak into pane B.
    search_input: Option<String>,
    search_results: Option<SearchResults>,
    // Line-based visual selection on the active pane's scrollback.
    // `v` in scroll mode anchors at the viewport top; motion keys move
    // the cursor (with viewport auto-follow); `y` yanks the covered
    // lines to the client's clipboard and clears the selection. Lives
    // at workspace level (not per-pane) because only the active pane
    // can be selecting; cycling/closing panes clears it.
    selection: Option<Selection>,
    // Active rename prompt on the focused pane. Follows the same
    // begin/input/commit/cancel lifecycle as `search_input`; committing
    // overwrites the active pane's title string (shown in its header).
    rename_input: Option<String>,
    // When true, keyboard input destined for the active pane is also
    // mirrored to every other live pane in this window. Toggled by
    // Ctrl-a =. Mouse events are NOT mirrored — clicking a pane
    // should still change focus locally the way it always does.
    sync_panes: bool,
    // True while the user is actively dragging the mouse with the
    // left button held. Used so that motion events only extend the
    // selection when they belong to an in-progress drag, not when the
    // terminal happens to emit stray motion for another reason.
    mouse_drag_active: bool,
    // Set when the user grabbed a pane border with the mouse. Holds
    // the two neighbor panes on either side of the separator plus the
    // last pointer position we applied weight adjustments at, so each
    // subsequent motion event translates delta-cells into resize_pane
    // calls.
    mouse_resize: Option<MouseResize>,
    // Last preset applied via Ctrl-a Space. Persists across calls so
    // Space actually cycles (Two → Three → Quad → Two…) instead of
    // reapplying the same preset every time.
    last_preset: LayoutPreset,
    // Set by mouse-drag release when the selection spans more than one
    // cell. The server-side input loop drains this via
    // `take_pending_clipboard` and routes it back to the requesting
    // client as an OSC 52 write — matching the native-feel "drag to
    // highlight, release to copy" workflow most desktop terminals have.
    pending_clipboard: Option<String>,
    // Most-recent yanked text. Ctrl-a ] pastes this into the active
    // pane by writing it to the PTY as if typed. Populated by every
    // yank path — keyboard `y` in copy mode and mouse-drag release —
    // so the OSC 52 clipboard isn't the only place the selection
    // lands. A single slot for now; a buffer stack can come later.
    paste_buffer: Option<String>,
    // Active command prompt. Either a split-with-command prompt (which
    // captures the orientation for the new pane) or a general runtime
    // command prompt (`:` mode). Separate from `rename_input` because
    // the lifecycles differ — committing a rename edits state; committing
    // a split prompt spawns a pane; committing a general prompt returns
    // the command string to the caller.
    command_input: Option<PromptState>,
    // How many rows of scrollback every new pane's Session buffers.
    // Flows through to Session::spawn_command's retain/scrollback arg.
    scrollback_lines: usize,
    // When false, the status bar omits the Ctrl-a hint strip so the
    // middle section is empty. Useful for users who already know the
    // bindings.
    status_bar_hints: bool,
    // Last error from a general command-prompt dispatch (parse failure or
    // unknown command). Shown in the status bar middle section until the
    // next keypress clears it. Populated by `set_prompt_error`; drained
    // one-shot by `take_prompt_error`.
    prompt_error: Option<String>,
    // Broadcast bus for pane lifecycle / state events. Publish
    // happens only from the dispatch thread; subscribers (supervisor
    // overlay, MCP server) pull from their own thread, so no
    // synchronization beyond `mpsc::Sender` is needed.
    event_bus: EventBus,
    // Optional session-level mirror owned by `WindowSet`. When present,
    // every workspace event is also published there so MCP watch_events
    // subscribers can observe panes across every window, including
    // background windows.
    session_event_bus: Option<Arc<Mutex<EventBus>>>,
    // Wall-clock threshold the per-pane `IdleDetector` uses to decide
    // when a pane has gone quiet. Loaded from `[agent].idle_threshold_ms`
    // when present; defaults to `DEFAULT_IDLE_THRESHOLD` otherwise. Held
    // here so newly-spawned panes pick up the same threshold as window
    // 0 without re-reading config.
    idle_threshold: Duration,
    // Pattern registries for the `PromptDetector`. Stored at workspace
    // scope for the same reason as `idle_threshold`: every new pane
    // gets a fresh detector seeded from the workspace defaults.
    shell_prompts: Vec<String>,
    agent_prompts: Vec<String>,
    // When Some, the Ctrl-a A supervisor overlay is open and the
    // server-side input router (`supervisor_handle_key`) consumes
    // keystrokes here instead of forwarding them to the focused
    // pane's PTY.
    supervisor: Option<SupervisorState>,
}

pub const PANE_NUMBER_OVERLAY_DURATION: Duration = Duration::from_millis(1_500);

#[derive(Debug, Clone)]
struct SearchResults {
    query: String,
    matches: Vec<usize>,
    current: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMode {
    // Whole lines: j/k extend by line, yank returns newline-joined
    // trimmed rows (ideal for log scraping).
    Line,
    // Cells flow as a text stream from anchor to cursor; h/l move the
    // cursor a column, j/k a row. Yank preserves line breaks but
    // trims trailing blanks on the final partial row.
    Char,
    // Rectangle bounded by (min_line, min_col) and (max_line, max_col).
    // Each selected row contributes exactly the cells in [min_col,
    // max_col]. Yank joins those column slices with newlines — ideal
    // for pulling aligned columnar output (ps, ls -l, table cells).
    Rect,
}

// State recorded when the user mouse-presses on a pane separator,
// preserved across motion events until release. `orientation` tells us
// which axis to translate motion deltas along; the two neighbor ids
// are the panes that flank the separator (grow_toward / shrink_from
// switches depending on drag direction).
#[derive(Debug, Clone, Copy)]
struct MouseResize {
    orientation: SplitOrientation,
    left_or_top_pane: PaneId,
    right_or_bottom_pane: PaneId,
    last_col: u16,
    last_row: u16,
}

#[derive(Debug, Clone, Copy)]
struct Selection {
    mode: SelectionMode,
    // The (line, col) where the user hit `v` / `V`. Immutable for the
    // life of the selection.
    anchor_line: usize,
    anchor_col: usize,
    // The (line, col) the user is currently extending to. Motion keys
    // move this end; the anchor stays put.
    cursor_line: usize,
    cursor_col: usize,
}

impl Selection {
    fn line_range(self) -> (usize, usize) {
        if self.anchor_line <= self.cursor_line {
            (self.anchor_line, self.cursor_line)
        } else {
            (self.cursor_line, self.anchor_line)
        }
    }

    fn clamped_line_count(self, total: usize) -> usize {
        if total == 0 {
            return 0;
        }
        let (low, high) = self.line_range();
        let clamped_high = high.min(total - 1);
        if clamped_high < low {
            0
        } else {
            clamped_high - low + 1
        }
    }

    // Return (start, end) where start <= end in scrollback-linear order
    // (line-major, column-minor). Used by char mode to determine what
    // "comes before" what when yanking a stream that spans lines.
    fn stream_endpoints(self) -> ((usize, usize), (usize, usize)) {
        let a = (self.anchor_line, self.anchor_col);
        let b = (self.cursor_line, self.cursor_col);
        if a <= b { (a, b) } else { (b, a) }
    }

    // Return the (lo, hi) column endpoints in ascending order. For
    // rect mode these are the horizontal bounds of the selection
    // rectangle; meaningless in Line mode and unused there.
    fn col_range(self) -> (usize, usize) {
        if self.anchor_col <= self.cursor_col {
            (self.anchor_col, self.cursor_col)
        } else {
            (self.cursor_col, self.anchor_col)
        }
    }

    // Is cell (line, col) inside the selection given this mode?
    fn contains_cell(self, line: usize, col: usize) -> bool {
        match self.mode {
            SelectionMode::Line => {
                let (lo, hi) = self.line_range();
                line >= lo && line <= hi
            }
            SelectionMode::Char => {
                let (start, end) = self.stream_endpoints();
                if line < start.0 || line > end.0 {
                    return false;
                }
                if start.0 == end.0 {
                    // Same row: inclusive range between start.1 and end.1.
                    col >= start.1 && col <= end.1
                } else if line == start.0 {
                    col >= start.1
                } else if line == end.0 {
                    col <= end.1
                } else {
                    true
                }
            }
            SelectionMode::Rect => {
                let (row_lo, row_hi) = self.line_range();
                let (col_lo, col_hi) = self.col_range();
                line >= row_lo && line <= row_hi && col >= col_lo && col <= col_hi
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutPreset {
    TwoColumns,
    ThreeColumns,
    Quadrants,
}

/// Snapshot of a pane's agent-observability state plus its current
/// PTY size. Returned by `Workspace::pane_summaries`. Owned strings
/// so the caller can drop the workspace borrow before serializing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneSummaryView {
    pub pane_id: u32,
    pub label: Option<String>,
    pub state: AgentState,
    pub last_command: Option<String>,
    pub last_exit: Option<i32>,
    pub size_cols: u16,
    pub size_rows: u16,
}

#[derive(Debug)]
struct ManagedSession {
    id: PaneId,
    title: String,
    session: Session,
    exit_status: Option<ExitStatus>,
    // Per-pane agent observability state. The detectors live here
    // (rather than on `Pane`) so the workspace can publish events
    // when transitions are observed without making `Pane` aware of
    // the event bus. `bytes_since_emit` and `last_output_emit_at`
    // power the debounce in `Workspace::ingest_available_output`.
    idle: IdleDetector,
    prompt: PromptDetector,
    bytes_since_emit: u64,
    last_output_emit_at: Instant,
}

impl Workspace {
    // Preserved for the `--mux` foreground demo; produces a two-pane
    // workspace arranged side-by-side.
    pub fn spawn_two_pane(shell: &str, size: PtySize) -> io::Result<Self> {
        let mut workspace = Self::spawn_single_named(shell, size, "mux")?;
        let existing_active = workspace.active;
        workspace.split_pane_with_id(existing_active, SplitOrientation::Columns)?;
        workspace.active = workspace.leaves_in_order()[0];
        Ok(workspace)
    }

    pub fn spawn_single(shell: &str, size: PtySize) -> io::Result<Self> {
        Self::spawn_single_named(shell, size, "session")
    }

    pub fn spawn_single_named(shell: &str, size: PtySize, name: &str) -> io::Result<Self> {
        Self::spawn_single_named_with_options(
            shell,
            size,
            name,
            DEFAULT_SCROLLBACK_LINES,
            DEFAULT_STATUS_BAR_HINTS,
        )
    }

    // Config-aware spawn path. `scrollback_lines` sets the Session
    // retain size for this workspace's panes; `status_bar_hints` flips
    // off the `Ctrl-a …` strip in render_frame. Keeping this separate
    // from `spawn_single_named` means default callers (tests, the --mux
    // demo) don't need to know about the knobs.
    pub fn spawn_single_named_with_options(
        shell: &str,
        size: PtySize,
        name: &str,
        scrollback_lines: usize,
        status_bar_hints: bool,
    ) -> io::Result<Self> {
        Self::spawn_single_named_with_options_at_pane_id(
            shell,
            size,
            name,
            scrollback_lines,
            status_bar_hints,
            1,
        )
    }

    pub(crate) fn spawn_single_named_with_options_at_pane_id(
        shell: &str,
        size: PtySize,
        name: &str,
        scrollback_lines: usize,
        status_bar_hints: bool,
        pane_id: PaneId,
    ) -> io::Result<Self> {
        let tree = LayoutNode::leaf(pane_id);
        let layout = tree.compute(size);
        let pane_layout = layout.panes[0].1;
        let title = format!("pane-{pane_id}");
        let session = ManagedSession {
            id: pane_id,
            title: title.clone(),
            session: Session::spawn_command(
                &title,
                shell,
                &["-i"],
                pane_layout.pty_size(),
                scrollback_lines,
                pane_layout.pty_size().rows as usize,
            )?,
            exit_status: None,
            idle: IdleDetector::new(DEFAULT_IDLE_THRESHOLD),
            prompt: PromptDetector::defaults(),
            bytes_since_emit: 0,
            last_output_emit_at: Instant::now(),
        };

        Ok(Self {
            sessions: vec![session],
            active: pane_id,
            tree,
            layout,
            size,
            shell: shell.to_string(),
            next_pane_id: pane_id.saturating_add(1),
            session_name: name.to_string(),
            hostname: read_hostname(),
            status_label_override: None,
            pane_number_overlay_until: None,
            zoomed: None,
            search_input: None,
            search_results: None,
            selection: None,
            rename_input: None,
            sync_panes: false,
            mouse_drag_active: false,
            mouse_resize: None,
            // Start at Quadrants so the first Ctrl-a Space cycles to
            // TwoColumns — the least-destructive default of the three.
            last_preset: LayoutPreset::Quadrants,
            pending_clipboard: None,
            paste_buffer: None,
            command_input: None,
            scrollback_lines,
            status_bar_hints,
            prompt_error: None,
            // Genesis pane (id 1) predates any possible subscriber
            // so we deliberately don't publish `PaneSpawned` for it —
            // late subscribers should enumerate via `ListPanes`.
            event_bus: EventBus::default(),
            session_event_bus: None,
            idle_threshold: DEFAULT_IDLE_THRESHOLD,
            shell_prompts: vec!["$ ".into(), "# ".into(), "> ".into(), "% ".into()],
            agent_prompts: vec!["│ > ".into(), "architect> ".into(), ">>> ".into()],
            supervisor: None,
        })
    }

    /// Single-pane workspace whose genesis pane runs `command`
    /// (passed to `/bin/sh -c`) instead of the user's shell. Used by
    /// `WindowSet::new_window_with_command` so the MCP `spawn_pane`
    /// tool can create a new window with a specific process. Mirrors
    /// `spawn_single_named_with_options` field-for-field — keep them
    /// in sync if the workspace shape grows.
    pub fn spawn_single_with_command(
        command: &str,
        size: PtySize,
        name: &str,
        scrollback_lines: usize,
        status_bar_hints: bool,
    ) -> io::Result<Self> {
        Self::spawn_single_with_command_at_pane_id(
            command,
            size,
            name,
            scrollback_lines,
            status_bar_hints,
            1,
        )
    }

    pub(crate) fn spawn_single_with_command_at_pane_id(
        command: &str,
        size: PtySize,
        name: &str,
        scrollback_lines: usize,
        status_bar_hints: bool,
        pane_id: PaneId,
    ) -> io::Result<Self> {
        let tree = LayoutNode::leaf(pane_id);
        let layout = tree.compute(size);
        let pane_layout = layout.panes[0].1;
        let title = truncate_command_label(command);
        let mut session = ManagedSession {
            id: pane_id,
            title: title.clone(),
            session: Session::spawn_command(
                &title,
                "/bin/sh",
                &["-c", command],
                pane_layout.pty_size(),
                scrollback_lines,
                pane_layout.pty_size().rows as usize,
            )?,
            exit_status: None,
            idle: IdleDetector::new(DEFAULT_IDLE_THRESHOLD),
            prompt: PromptDetector::defaults(),
            bytes_since_emit: 0,
            last_output_emit_at: Instant::now(),
        };
        // Stash the spawn-time command so list_panes / supervisor can
        // render what's actually running here. Mirror what
        // `split_pane_with_command` does on the same field.
        session.session.pane_mut().last_command = Some(command.to_string());

        Ok(Self {
            sessions: vec![session],
            active: pane_id,
            tree,
            layout,
            size,
            shell: "/bin/sh".to_string(),
            next_pane_id: pane_id.saturating_add(1),
            session_name: name.to_string(),
            hostname: read_hostname(),
            status_label_override: None,
            pane_number_overlay_until: None,
            zoomed: None,
            search_input: None,
            search_results: None,
            selection: None,
            rename_input: None,
            sync_panes: false,
            mouse_drag_active: false,
            mouse_resize: None,
            last_preset: LayoutPreset::Quadrants,
            pending_clipboard: None,
            paste_buffer: None,
            command_input: None,
            scrollback_lines,
            status_bar_hints,
            prompt_error: None,
            event_bus: EventBus::default(),
            session_event_bus: None,
            idle_threshold: DEFAULT_IDLE_THRESHOLD,
            shell_prompts: vec!["$ ".into(), "# ".into(), "> ".into(), "% ".into()],
            agent_prompts: vec!["│ > ".into(), "architect> ".into(), ">>> ".into()],
            supervisor: None,
        })
    }

    pub fn show_pane_numbers(&mut self, duration: Duration) {
        self.pane_number_overlay_until = Some(Instant::now() + duration);
    }

    fn pane_numbers_active(&self) -> bool {
        self.pane_number_overlay_until
            .is_some_and(|deadline| Instant::now() < deadline)
    }

    pub fn resize(&mut self, size: PtySize) -> io::Result<()> {
        self.size = size;
        self.layout = self.compute_layout();
        self.apply_layout_to_panes()
    }

    // Toggle zoom on the currently-active pane. When zooming in, the
    // active pane is promoted to fill the whole body rect and other
    // panes are hidden from the rendered layout (their PTYs keep running
    // at their last size — they just aren't shown). Zooming back out
    // recomputes from the tree and resizes every pane to its real slot.
    pub fn toggle_zoom(&mut self) -> io::Result<bool> {
        if self.zoomed.is_some() {
            self.zoomed = None;
        } else {
            self.zoomed = Some(self.active);
        }
        self.layout = self.compute_layout();
        self.apply_layout_to_panes()?;
        Ok(true)
    }

    // Central helper that either returns a full-body single-pane layout
    // (when zoomed) or the tree's natural layout. Everything that writes
    // to `self.layout` goes through here so zoom state is always
    // respected consistently.
    fn compute_layout(&self) -> WorkspaceLayout {
        if let Some(zoomed_id) = self.zoomed
            && self.sessions.iter().any(|session| session.id == zoomed_id)
        {
            let body_rows = self.size.rows.saturating_sub(1).max(2);
            let frame = Rect {
                x: 0,
                y: 0,
                width: self.size.cols.max(MIN_PANE_WIDTH),
                height: body_rows,
            };
            return WorkspaceLayout {
                size: self.size,
                status_row: body_rows,
                panes: vec![(zoomed_id, PaneLayout::from_frame(frame))],
                separators: Vec::new(),
            };
        }
        self.tree.compute(self.size)
    }

    // Implicit exit from zoom before any structural mutation (split,
    // close, cycle, resize-pane, preset). Leaves the tree itself alone;
    // the next compute_layout call will rebuild from the tree.
    fn unzoom(&mut self) {
        self.zoomed = None;
    }

    pub fn is_zoomed(&self) -> bool {
        self.zoomed.is_some()
    }

    pub fn active_is_scrolled_back(&self) -> bool {
        self.session_for(self.active)
            .map(|managed| !managed.session.follow_output())
            .unwrap_or(false)
    }

    // Returns Ok(true) if a new pane was actually spawned. Refuses with
    // Ok(false) when the resulting layout would push any pane below the
    // minimum usable dimensions.
    pub fn split_active(&mut self, orientation: SplitOrientation) -> io::Result<bool> {
        if !self
            .tree
            .fits_after_split(self.size, self.active, orientation)
        {
            return Ok(false);
        }

        self.unzoom();
        let active = self.active;
        self.split_pane_with_id(active, orientation)?;
        Ok(true)
    }

    fn split_pane_with_id(
        &mut self,
        target: PaneId,
        orientation: SplitOrientation,
    ) -> io::Result<()> {
        let new_id = self.next_pane_id;
        self.next_pane_id += 1;

        if !self.tree.split_at(target, new_id, orientation) {
            return Err(io::Error::other(format!(
                "pane id {target} not found in layout tree",
            )));
        }

        self.layout = self.compute_layout();

        let pane_frame = self
            .layout
            .pane_frame(new_id)
            .expect("split_at must leave the new pane in the tree");
        let title = format!("pane-{new_id}");
        let session = ManagedSession {
            id: new_id,
            title: title.clone(),
            session: Session::spawn_command(
                &title,
                &self.shell,
                &["-i"],
                pane_frame.pty_size(),
                self.scrollback_lines,
                pane_frame.pty_size().rows as usize,
            )?,
            exit_status: None,
            idle: IdleDetector::new(self.idle_threshold),
            prompt: PromptDetector::new(self.shell_prompts.clone(), self.agent_prompts.clone()),
            bytes_since_emit: 0,
            last_output_emit_at: Instant::now(),
        };
        self.sessions.push(session);

        self.apply_layout_to_panes()?;
        self.change_active(new_id);
        self.publish_event(Event::PaneSpawned {
            pane_id: new_id as u32,
            label: None,
        });
        Ok(())
    }

    // Returns Ok(true) if the active pane was actually closed. Refuses
    // silently when there's only one pane left so the workspace always
    // has something to render; the daemon treats "close the last pane"
    // as an explicit Shutdown instead.
    pub fn close_active(&mut self) -> io::Result<bool> {
        if self.sessions.len() <= 1 {
            return Ok(false);
        }

        self.unzoom();
        self.clear_active_pane_transients();
        let closing = self.active;
        if !self.tree.remove_leaf(closing) {
            return Ok(false);
        }

        // Pick the next focus before mutating the session list so we can
        // fall back on the tree's new leaf order.
        let next_active = self
            .tree
            .leaves()
            .first()
            .copied()
            .expect("tree must retain at least one leaf after remove");

        if let Some(index) = self.sessions.iter().position(|pane| pane.id == closing) {
            let mut removed = self.sessions.remove(index);
            // Idempotent force-kill so the shell isn't left as a zombie.
            // The Result is intentionally dropped: kill on an already-reaped
            // child returns InvalidInput, and wait returns the cached status
            // — neither is actionable without a log channel.
            let _ = removed.session.close();
            self.publish_event(Event::PaneClosed {
                pane_id: closing as u32,
            });
        }

        // Active pane just got dropped — change_active would try to
        // emit a focus-loss marker on it, but it's already gone from
        // sessions, so the no-op happens naturally. Do the swap before
        // re-laying-out so the focus event fires against the live PTY
        // size (it's a single byte sequence either way).
        self.change_active(next_active);
        self.layout = self.compute_layout();
        self.apply_layout_to_panes()?;
        Ok(true)
    }

    pub fn cycle_active(&mut self) -> io::Result<bool> {
        let leaves = self.leaves_in_order();
        if leaves.len() <= 1 {
            return Ok(false);
        }
        let position = leaves.iter().position(|id| *id == self.active).unwrap_or(0);
        self.change_active(leaves[(position + 1) % leaves.len()]);
        self.clear_active_pane_transients();
        self.clear_zoom_if_any()?;
        Ok(true)
    }

    // Swap the active pane with its neighbor in `leaves_in_order`. Keeps
    // active pointing at the same Session (same shell, same scrollback)
    // but its visual slot moves to where the neighbor was. Wraps at the
    // ends so `}` on the last pane swaps with the first. Returns false
    // when there's only one pane — nothing to swap with.
    pub fn swap_active_with_next(&mut self) -> io::Result<bool> {
        self.swap_active_by(1)
    }

    pub fn swap_active_with_previous(&mut self) -> io::Result<bool> {
        self.swap_active_by(-1)
    }

    fn swap_active_by(&mut self, delta: isize) -> io::Result<bool> {
        let leaves = self.leaves_in_order();
        let len = leaves.len();
        if len <= 1 {
            return Ok(false);
        }
        let position = leaves.iter().position(|id| *id == self.active).unwrap_or(0) as isize;
        let target_index = position.rem_euclid(len as isize) + delta;
        let neighbor_index = target_index.rem_euclid(len as isize) as usize;
        let neighbor = leaves[neighbor_index];
        if neighbor == self.active {
            return Ok(false);
        }
        if !self.tree.swap_leaves(self.active, neighbor) {
            return Ok(false);
        }
        // Any transient UI state was tied to the active pane's visual
        // slot; swapping slots invalidates it.
        self.unzoom();
        self.clear_active_pane_transients();
        self.layout = self.compute_layout();
        self.apply_layout_to_panes()?;
        Ok(true)
    }

    // Drops any zoom state and rebuilds the layout + pane sizes from the
    // tree. No-op when not zoomed. Called when the user performs any
    // action that would be confusing under a zoomed view.
    fn clear_zoom_if_any(&mut self) -> io::Result<()> {
        if self.zoomed.is_none() {
            return Ok(());
        }
        self.zoomed = None;
        self.layout = self.compute_layout();
        self.apply_layout_to_panes()
    }

    // A named multi-pane shape the user can summon with a single prefix
    // binding.
    pub fn apply_preset(&mut self, preset: LayoutPreset) -> io::Result<bool> {
        // Presets assume a clean slate: applying a preset on top of an
        // existing multi-pane layout would either need to destroy live
        // shells or invent arbitrary nesting. Refuse instead so the user
        // never loses running work by accident.
        if self.sessions.len() != 1 {
            return Ok(false);
        }

        match preset {
            LayoutPreset::TwoColumns => {
                self.split_active(SplitOrientation::Columns)?;
            }
            LayoutPreset::ThreeColumns => {
                self.split_active(SplitOrientation::Columns)?;
                self.split_active(SplitOrientation::Columns)?;
            }
            LayoutPreset::Quadrants => {
                // Same recipe as the manual "four keystrokes" flow:
                // | then - on the right column, cycle back to the left
                // column, then - on the left.
                self.split_active(SplitOrientation::Columns)?;
                self.split_active(SplitOrientation::Rows)?;
                self.cycle_active()?;
                self.split_active(SplitOrientation::Rows)?;
            }
        }
        self.last_preset = preset;
        Ok(true)
    }

    // Cycle through the three presets in order. Backed by a Workspace
    // field so `Ctrl-a Space` can be stateless at the client: the
    // server rotates TwoColumns → ThreeColumns → Quadrants → Two…
    // and calls apply_preset. Fails (Ok(false)) for the same reason
    // apply_preset does — only valid on a single-pane workspace.
    pub fn cycle_preset(&mut self) -> io::Result<bool> {
        let next = match self.last_preset {
            LayoutPreset::TwoColumns => LayoutPreset::ThreeColumns,
            LayoutPreset::ThreeColumns => LayoutPreset::Quadrants,
            LayoutPreset::Quadrants => LayoutPreset::TwoColumns,
        };
        self.apply_preset(next)
    }

    // Scroll the active pane's viewport back (Up) or forward (Down) by
    // `lines`. Returns true if the viewport actually moved so the daemon
    // can decide whether to repaint.
    pub fn scroll_active_up(&mut self, lines: usize) -> bool {
        self.session_for_mut(self.active)
            .map(|session| {
                let before = session.session.follow_output();
                session.session.wheel_up(lines);
                // wheel_up returns no explicit bool; treat it as dirty if
                // we transitioned out of follow-output mode or viewport
                // moved. The session API doesn't expose the movement
                // delta cleanly, so we conservatively assume dirty.
                let _ = before;
                true
            })
            .unwrap_or(false)
    }

    pub fn scroll_active_down(&mut self, lines: usize) -> bool {
        self.session_for_mut(self.active)
            .map(|session| {
                session.session.wheel_down(lines);
                true
            })
            .unwrap_or(false)
    }

    // Build a plain-text snapshot of the active pane's visible viewport.
    // Trailing blank cells on each row are trimmed so the clipboard
    // payload looks like what the user actually sees, not a padded
    // rectangle. Used to satisfy the Ctrl-a y yank binding.
    pub fn yank_active_viewport(&mut self) -> Option<String> {
        let session = self.session_for(self.active)?;
        let rows = session.session.render_cells();
        let mut out = String::new();
        for (index, row) in rows.iter().enumerate() {
            if index > 0 {
                out.push('\n');
            }
            let end = row
                .iter()
                .rposition(|cell| *cell != Cell::BLANK)
                .map(|i| i + 1)
                .unwrap_or(0);
            for cell in &row[..end] {
                out.push(cell.ch);
            }
        }
        self.paste_buffer = Some(out.clone());
        Some(out)
    }

    // Begin a search prompt. Any previous committed results are
    // cleared so the user is clearly starting over. Returns true so the
    // caller can treat this as a dirty-frame trigger.
    pub fn begin_search(&mut self) -> bool {
        self.search_input = Some(String::new());
        self.search_results = None;
        true
    }

    // Append raw bytes to the in-progress search prompt. Printable
    // ASCII and UTF-8 continuation bytes accumulate into the query;
    // backspace/DEL deletes the last character; anything else is
    // silently ignored so control codes typed by mistake don't corrupt
    // the prompt. No-op when no prompt is active.
    pub fn search_input_bytes(&mut self, bytes: &[u8]) -> bool {
        let Some(query) = self.search_input.as_mut() else {
            return false;
        };
        let mut changed = false;
        for &byte in bytes {
            match byte {
                0x7f | 0x08 if query.pop().is_some() => {
                    changed = true;
                }
                byte if (0x20..0x7f).contains(&byte) => {
                    query.push(byte as char);
                    changed = true;
                }
                _ => {}
            }
        }
        changed
    }

    // Commit the current search prompt: run the search against the
    // active pane's scrollback, jump to the first match, and transition
    // into the "navigating matches" state. Returns true even when there
    // are no hits — the status bar still flips from "prompt" to "0
    // matches" and the caller should redraw.
    pub fn commit_search(&mut self) -> bool {
        let Some(query) = self.search_input.take() else {
            return false;
        };
        if query.is_empty() {
            return true;
        }
        let Some(active) = self.session_for_mut(self.active) else {
            return true;
        };
        let matches = active.session.search_scrollback(&query);
        if matches.is_empty() {
            self.search_results = Some(SearchResults {
                query,
                matches,
                current: 0,
            });
        } else {
            active.session.center_viewport_on(matches[0]);
            self.search_results = Some(SearchResults {
                query,
                matches,
                current: 0,
            });
        }
        true
    }

    pub fn cancel_search(&mut self) -> bool {
        let had_input = self.search_input.is_some();
        self.search_input = None;
        had_input
    }

    pub fn clear_search(&mut self) -> bool {
        let had_any = self.search_input.is_some() || self.search_results.is_some();
        self.search_input = None;
        self.search_results = None;
        had_any
    }

    // Drop all transient UI state that belongs to the currently-active
    // pane — search prompt + results + visual selection. Called on any
    // event that changes the active pane (cycle, close, mouse click)
    // so state from pane A doesn't bleed into pane B.
    fn clear_active_pane_transients(&mut self) -> bool {
        let a = self.clear_search();
        let b = self.clear_selection();
        let c = self.cancel_rename();
        let d = self.cancel_command_prompt();
        a || b || c || d
    }

    pub fn search_next(&mut self) -> bool {
        let Some(results) = self.search_results.as_mut() else {
            return false;
        };
        if results.matches.is_empty() {
            return false;
        }
        results.current = (results.current + 1) % results.matches.len();
        let target_line = results.matches[results.current];
        if let Some(active) = self.session_for_mut(self.active) {
            active.session.center_viewport_on(target_line);
        }
        true
    }

    pub fn search_prev(&mut self) -> bool {
        let Some(results) = self.search_results.as_mut() else {
            return false;
        };
        let len = results.matches.len();
        if len == 0 {
            return false;
        }
        results.current = (results.current + len - 1) % len;
        let target_line = results.matches[results.current];
        if let Some(active) = self.session_for_mut(self.active) {
            active.session.center_viewport_on(target_line);
        }
        true
    }

    // Begin a selection on the active pane in the given mode. Anchor
    // and cursor both start at the top-left of the visible viewport
    // so the user can extend down/right to grow the range.
    pub fn begin_selection(&mut self, mode: SelectionMode) -> bool {
        let Some(session) = self.session_for(self.active) else {
            return false;
        };
        let total = session.session.total_lines();
        let anchor_line = if total == 0 {
            0
        } else {
            session.session.scrollback_viewport_top().min(total - 1)
        };
        self.selection = Some(Selection {
            mode,
            anchor_line,
            anchor_col: 0,
            cursor_line: anchor_line,
            cursor_col: 0,
        });
        true
    }

    pub fn clear_selection(&mut self) -> bool {
        let had = self.selection.is_some();
        self.selection = None;
        had
    }

    pub fn has_selection(&self) -> bool {
        self.selection.is_some()
    }

    pub fn extend_selection_down(&mut self, lines: usize) -> bool {
        self.step_selection_line(|cursor, total| (cursor + lines).min(total.saturating_sub(1)))
    }

    pub fn extend_selection_up(&mut self, lines: usize) -> bool {
        self.step_selection_line(|cursor, _| cursor.saturating_sub(lines))
    }

    pub fn extend_selection_to_top(&mut self) -> bool {
        self.step_selection_line(|_, _| 0)
    }

    pub fn extend_selection_to_bottom(&mut self) -> bool {
        self.step_selection_line(|_, total| total.saturating_sub(1))
    }

    // Char-mode column motion. A no-op for Line-mode selections — the
    // status bar doesn't show column info there, so moving the cursor
    // column would be a surprise.
    pub fn extend_selection_right(&mut self, cols: usize) -> bool {
        self.step_selection_col(|col| col.saturating_add(cols))
    }

    pub fn extend_selection_left(&mut self, cols: usize) -> bool {
        self.step_selection_col(|col| col.saturating_sub(cols))
    }

    fn step_selection_line(&mut self, compute: impl FnOnce(usize, usize) -> usize) -> bool {
        let Some(mut selection) = self.selection else {
            return false;
        };
        let total = match self.session_for(self.active) {
            Some(session) => session.session.total_lines(),
            None => return false,
        };
        let new_cursor = compute(selection.cursor_line, total);
        if new_cursor == selection.cursor_line {
            return false;
        }
        selection.cursor_line = new_cursor;
        self.selection = Some(selection);
        if let Some(active) = self.session_for_mut(self.active) {
            active.session.ensure_line_visible(new_cursor);
        }
        true
    }

    fn step_selection_col(&mut self, compute: impl FnOnce(usize) -> usize) -> bool {
        let Some(mut selection) = self.selection else {
            return false;
        };
        if selection.mode == SelectionMode::Line {
            return false;
        }
        let new_col = compute(selection.cursor_col);
        if new_col == selection.cursor_col {
            return false;
        }
        selection.cursor_col = new_col;
        self.selection = Some(selection);
        true
    }

    // Join and return the text currently covered by the selection.
    // Line mode yanks whole trimmed lines joined by newlines; Char
    // mode yanks a text stream that preserves line breaks between
    // multi-line selections. Clears the selection on success.
    pub fn yank_selection(&mut self) -> Option<String> {
        let selection = self.selection.take()?;
        let active = self.session_for(self.active)?;
        let text = match selection.mode {
            SelectionMode::Line => {
                let (low, high) = selection.line_range();
                active.session.extract_scrollback_lines(low, high)
            }
            SelectionMode::Char => extract_char_stream(active, selection),
            SelectionMode::Rect => extract_rect_stream(active, selection),
        };
        self.paste_buffer = Some(text.clone());
        Some(text)
    }

    // Paste the most-recent yanked text into the active pane by writing
    // it to the PTY as if the user typed it. When the active pane's
    // shell has enabled bracketed paste mode (DECSET 2004), the text is
    // wrapped with `ESC[200~ ... ESC[201~` so the shell can tell a
    // paste from a typed line and skip auto-suggestion / multi-line
    // expansion. No-op if no yank has happened yet or the active pane's
    // shell has already exited.
    pub fn paste_buffer_into_active(&mut self) -> io::Result<bool> {
        let Some(text) = self.paste_buffer.clone() else {
            return Ok(false);
        };
        if let Some(active) = self.session_for_mut(self.active)
            && active.exit_status.is_none()
        {
            let payload = bracket_paste_if_enabled(active.session.bracketed_paste_enabled(), &text);
            active.session.write_input(&payload)?;
            return Ok(true);
        }
        Ok(false)
    }

    // Rename-prompt lifecycle. begin opens an empty prompt; input
    // appends printable bytes and backspace/DEL deletes the last char;
    // commit overwrites the active pane's title and closes the prompt;
    // cancel discards without touching the title.
    pub fn begin_rename(&mut self) -> bool {
        self.rename_input = Some(String::new());
        true
    }

    pub fn rename_input_bytes(&mut self, bytes: &[u8]) -> bool {
        let Some(buffer) = self.rename_input.as_mut() else {
            return false;
        };
        append_prompt_bytes(buffer, bytes)
    }

    pub fn commit_rename(&mut self) -> bool {
        let Some(new_title) = self.rename_input.take() else {
            return false;
        };
        let trimmed = new_title.trim();
        if trimmed.is_empty() {
            // Empty commit is treated as a cancel — prevents accidentally
            // blanking a pane header with Enter on an empty prompt.
            return true;
        }
        if let Some(active) = self.session_for_mut(self.active) {
            active.title = trimmed.to_string();
        }
        true
    }

    pub fn cancel_rename(&mut self) -> bool {
        let had = self.rename_input.is_some();
        self.rename_input = None;
        had
    }

    // Split-with-command prompt lifecycle. Parallels the rename prompt
    // but commits by spawning a new pane running the typed command.
    pub fn begin_command_prompt(&mut self, orientation: SplitOrientation) -> bool {
        self.command_input = Some(PromptState::SplitWith {
            orientation,
            buffer: String::new(),
        });
        true
    }

    pub fn command_input_bytes(&mut self, bytes: &[u8]) -> bool {
        match self.command_input.as_mut() {
            Some(PromptState::SplitWith { buffer, .. }) | Some(PromptState::General { buffer }) => {
                append_prompt_bytes(buffer, bytes)
            }
            None => false,
        }
    }

    // Commit the split-with-command prompt. Returns Ok(true) if a pane
    // was spawned, Ok(false) if the buffer was empty (treated as a cancel)
    // or if a non-SplitWith prompt is active.
    pub fn commit_command_prompt(&mut self) -> io::Result<bool> {
        let state = match self.command_input.take() {
            Some(PromptState::SplitWith {
                orientation,
                buffer,
            }) => (orientation, buffer),
            Some(other) => {
                // Wrong prompt kind — put it back and refuse.
                self.command_input = Some(other);
                return Ok(false);
            }
            None => return Ok(false),
        };
        let trimmed = state.1.trim().to_string();
        if trimmed.is_empty() {
            return Ok(false);
        }
        self.split_active_with_command(state.0, &trimmed)
    }

    pub fn cancel_command_prompt(&mut self) -> bool {
        let had = self.command_input.is_some();
        self.command_input = None;
        had
    }

    // General runtime command prompt (`:` mode). The caller is responsible
    // for dispatching the returned command string.
    pub fn begin_general_command_prompt(&mut self) -> bool {
        self.command_input = Some(PromptState::General {
            buffer: String::new(),
        });
        true
    }

    // Returns Ok(Some(line)) on successful non-empty commit, Ok(None) on
    // empty-commit-as-cancel or when no prompt is active. Returns Err if
    // called while a non-General prompt is active.
    pub fn commit_general_command_prompt(&mut self) -> io::Result<Option<String>> {
        match self.command_input.take() {
            Some(PromptState::General { buffer }) => {
                let trimmed = buffer.trim().to_string();
                if trimmed.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(trimmed))
                }
            }
            Some(other) => {
                self.command_input = Some(other);
                Err(io::Error::other(
                    "commit_general_command_prompt called with non-general prompt",
                ))
            }
            None => Ok(None),
        }
    }

    /// Returns the kind of the currently active command prompt, or `None` if
    /// no prompt is open.  Used by the daemon to branch between the split-pane
    /// commit path and the general-command dispatch path.
    pub fn active_prompt_kind(&self) -> Option<PromptKind> {
        match &self.command_input {
            Some(PromptState::SplitWith { .. }) => Some(PromptKind::SplitWith),
            Some(PromptState::General { .. }) => Some(PromptKind::General),
            None => None,
        }
    }

    /// Store an error string to display in the status bar until cleared.
    pub fn set_prompt_error(&mut self, msg: impl Into<String>) {
        self.prompt_error = Some(msg.into());
    }

    /// Consume and return the pending prompt error (clears it on read).
    pub fn take_prompt_error(&mut self) -> Option<String> {
        self.prompt_error.take()
    }

    /// Read the pending prompt error without consuming it.
    pub fn prompt_error(&self) -> Option<&str> {
        self.prompt_error.as_deref()
    }

    /// The `PaneId` of the currently focused pane.
    pub fn active_pane_id(&self) -> crate::layout::PaneId {
        self.active
    }

    /// Whether the Ctrl-a A supervisor overlay is currently open.
    /// The daemon's input router branches on this to redirect raw key
    /// bytes to `supervisor_handle_key` instead of the focused pane.
    pub fn supervisor_open(&self) -> bool {
        self.supervisor.is_some()
    }

    /// Open the supervisor overlay. Initialises a `SupervisorState`
    /// from the current `pane_summaries()` snapshot so the dashboard
    /// is populated immediately; subsequent live updates flow in via
    /// `publish_event`.
    pub fn open_supervisor(&mut self) {
        let summaries = self.pane_summaries();
        let mut state = SupervisorState::new();
        for s in summaries {
            state.rows.push(SupervisorRow {
                pane_id: s.pane_id,
                label: s.label,
                state: s.state.as_wire(),
                last_command: s.last_command,
                age_secs: 0,
                window: None,
            });
        }
        self.supervisor = Some(state);
    }

    /// Open the supervisor with a caller-built row set. `WindowSet`
    /// uses this to seed the overlay with every pane in the SESSION
    /// (foreign rows carrying their `window` tag), not just this
    /// window's panes.
    pub(crate) fn open_supervisor_with_rows(&mut self, rows: Vec<SupervisorRow>) {
        let mut state = SupervisorState::new();
        state.rows = rows;
        self.supervisor = Some(state);
    }

    /// Apply a session-bus event to the open overlay (no-op when
    /// closed). `WindowSet::pump_supervisor_events` feeds events from
    /// OTHER windows through here; this workspace's own events arrive
    /// via the `publish_event` mirror. Returns true when the overlay
    /// was open (so the caller can mark the frame dirty), and gives
    /// back the row set for post-apply annotation.
    pub(crate) fn supervisor_apply_foreign_event(
        &mut self,
        event: &crate::events::Event,
        window_tag: Option<usize>,
    ) -> bool {
        let Some(state) = self.supervisor.as_mut() else {
            return false;
        };
        state.apply_event(event);
        // PaneSpawned rows are created without a window; tag the new
        // row so the user can see Enter will switch windows.
        if let (Some(tag), crate::events::Event::PaneSpawned { pane_id, .. }) = (window_tag, event)
            && let Some(row) = state.rows.iter_mut().find(|r| r.pane_id == *pane_id)
        {
            row.window = Some(tag);
        }
        true
    }

    /// Whether `pane_id` belongs to this workspace. The supervisor
    /// pump uses this to tell local events (already mirrored by
    /// `publish_event`) from foreign-window events.
    pub(crate) fn has_pane(&self, pane_id: u32) -> bool {
        let target: PaneId = pane_id as PaneId;
        self.sessions.iter().any(|s| s.id == target)
    }

    /// Close the supervisor overlay (whether opened by Ctrl-a A or
    /// programmatically). Returns true if it was open.
    pub fn close_supervisor(&mut self) -> bool {
        self.supervisor.take().is_some()
    }

    /// Read-only access to the supervisor state (renderer + tests).
    pub fn supervisor_state(&self) -> Option<&SupervisorState> {
        self.supervisor.as_ref()
    }

    /// Route a single key byte to the open supervisor overlay. Returns
    /// `Ok(true)` if the workspace state changed (so the caller can
    /// push a fresh frame to clients). When the overlay is closed
    /// (e.g. on `q`/`Esc` or after Enter→attach), the next call will
    /// no-op until `open_supervisor` is called again.
    ///
    /// Actions that touch panes execute against THIS workspace only.
    /// The daemon routes through `WindowSet::supervisor_handle_key`
    /// instead, which uses `Workspace::supervisor_key` to execute
    /// the same actions session-wide. This wrapper remains for
    /// single-window callers and tests.
    pub fn supervisor_handle_key(&mut self, byte: u8) -> io::Result<bool> {
        let (dirty, action) = self.supervisor_key(byte);
        if let Some(action) = action {
            match action {
                SupervisorAction::Attach(pane_id) => self.change_active(pane_id as PaneId),
                SupervisorAction::Kill(pane_id) => {
                    self.kill_pane_by_id(pane_id)?;
                }
                SupervisorAction::SetLabel(pane_id, label) => {
                    self.set_pane_label(pane_id, label);
                }
                SupervisorAction::Broadcast(payload, recipients) => {
                    let _ = self.broadcast_to_panes(payload.as_bytes(), &recipients);
                }
            }
            return Ok(true);
        }
        Ok(dirty)
    }

    /// Advance the supervisor UI state machine by one key byte and
    /// return what (if anything) the caller must now execute. The
    /// overlay's pane rows can span every window in the session, so
    /// the state machine itself never touches panes — it reports a
    /// [`SupervisorAction`] and the caller picks the execution scope
    /// (this workspace, or the whole `WindowSet`).
    pub(crate) fn supervisor_key(&mut self, byte: u8) -> (bool, Option<SupervisorAction>) {
        if self.supervisor.is_none() {
            return (false, None);
        }

        // Prioritise modal sub-states (kill confirm / label input /
        // broadcast) over the main keymap so a stray `q` while typing
        // doesn't close the overlay.
        let in_kill = self
            .supervisor
            .as_ref()
            .is_some_and(|s| s.has_pending_kill());
        if in_kill {
            let to_kill = self
                .supervisor
                .as_mut()
                .and_then(|s| s.resolve_kill_confirm(byte));
            return (true, to_kill.map(SupervisorAction::Kill));
        }

        let in_label = self
            .supervisor
            .as_ref()
            .is_some_and(|s| s.has_label_input());
        if in_label {
            // Esc cancels the label edit without closing the overlay.
            if byte == 0x1b {
                if let Some(state) = self.supervisor.as_mut() {
                    state.cancel_label_input();
                }
                return (true, None);
            }
            let committed = self
                .supervisor
                .as_mut()
                .and_then(|s| s.label_input_byte(byte));
            if let Some(label) = committed
                && let Some(pane_id) = self.supervisor.as_ref().and_then(|s| s.selected_pane())
            {
                let trimmed = label.trim().to_string();
                let new_label = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                };
                return (true, Some(SupervisorAction::SetLabel(pane_id, new_label)));
            }
            return (true, None);
        }

        // Broadcast modal: `b`-confirm or typing buffer takes
        // precedence over the browse keymap so 'q' or 'l' inside a
        // typed payload don't escape the modal.
        let broadcast_state = self.supervisor.as_ref().and_then(|s| s.broadcast.clone());
        if let Some(broadcast) = broadcast_state {
            return self.broadcast_key(broadcast, byte);
        }

        // Main supervisor keymap.
        match byte {
            b'j' => {
                if let Some(state) = self.supervisor.as_mut() {
                    state.move_down();
                }
                (true, None)
            }
            b'k' => {
                if let Some(state) = self.supervisor.as_mut() {
                    state.move_up();
                }
                (true, None)
            }
            b'\r' | b'\n' => {
                let target = self.supervisor.as_ref().and_then(|s| s.selected_pane());
                self.close_supervisor();
                (true, target.map(SupervisorAction::Attach))
            }
            b'l' => {
                if let Some(state) = self.supervisor.as_mut() {
                    state.begin_label_input();
                }
                (true, None)
            }
            b'K' => {
                if let Some(state) = self.supervisor.as_mut() {
                    state.begin_kill_confirm();
                }
                (true, None)
            }
            b'f' => {
                if let Some(state) = self.supervisor.as_mut() {
                    state.cycle_filter();
                }
                (true, None)
            }
            b'b' => {
                if let Some(state) = self.supervisor.as_mut() {
                    state.begin_broadcast_confirm();
                }
                (true, None)
            }
            b'q' | 0x1b => {
                self.close_supervisor();
                (true, None)
            }
            _ => (false, None),
        }
    }

    fn broadcast_key(
        &mut self,
        broadcast: BroadcastState,
        byte: u8,
    ) -> (bool, Option<SupervisorAction>) {
        match broadcast {
            BroadcastState::Confirm => {
                if let Some(state) = self.supervisor.as_mut() {
                    state.resolve_broadcast_confirm(byte);
                }
                (true, None)
            }
            BroadcastState::Typing(_) => {
                let payload = self
                    .supervisor
                    .as_mut()
                    .and_then(|s| s.broadcast_input_byte(byte));
                if let Some(committed) = payload {
                    let recipients = self
                        .supervisor
                        .as_ref()
                        .map(|s| s.broadcast_recipient_ids())
                        .unwrap_or_default();
                    return (
                        true,
                        Some(SupervisorAction::Broadcast(committed, recipients)),
                    );
                }
                (true, None)
            }
        }
    }

    // Close a specific pane by id (used by the supervisor `K` confirm
    // and the MCP `kill_pane` tool). Mirrors `close_active` but
    // targets an arbitrary leaf and refuses when only one pane is
    // left so the workspace always has something to render — same
    // invariant as `close_active`. Exposed `pub` for the MCP path;
    // the supervisor still goes through the same call site.
    pub fn kill_pane_by_id(&mut self, pane_id: u32) -> io::Result<bool> {
        if self.sessions.len() <= 1 {
            return Ok(false);
        }
        let target: PaneId = pane_id as PaneId;
        if !self.sessions.iter().any(|s| s.id == target) {
            return Ok(false);
        }
        // If the targeted pane is currently active, route through
        // close_active so focus migration matches the prior behaviour.
        if self.active == target {
            return self.close_active();
        }
        if !self.tree.remove_leaf(target) {
            return Ok(false);
        }
        if let Some(idx) = self.sessions.iter().position(|s| s.id == target) {
            let mut removed = self.sessions.remove(idx);
            let _ = removed.session.close();
            self.publish_event(Event::PaneClosed {
                pane_id: target as u32,
            });
        }
        self.layout = self.compute_layout();
        self.apply_layout_to_panes()?;
        Ok(true)
    }

    /// Close every pane in this workspace before the entire window is
    /// removed. This intentionally bypasses the one-pane workspace
    /// invariant because the caller is tearing down the workspace as a
    /// whole. Returns the pane ids that were closed and publishes
    /// `PaneClosed` for each one so session-wide MCP watchers see the
    /// cleanup.
    pub(crate) fn close_all_panes_for_window_removal(&mut self) -> io::Result<Vec<u32>> {
        let pane_ids: Vec<u32> = self.sessions.iter().map(|s| s.id as u32).collect();
        for session in &mut self.sessions {
            let _ = session.session.close()?;
        }
        for pane_id in &pane_ids {
            self.publish_event(Event::PaneClosed { pane_id: *pane_id });
        }
        Ok(pane_ids)
    }

    /// Update a pane's label and broadcast `LabelChanged`. Single
    /// publish entry-point for label edits — used by both the
    /// supervisor `l` overlay and the `ClientMessage::SetLabel` path.
    pub fn set_pane_label(&mut self, pane_id: u32, label: Option<String>) -> bool {
        let target: PaneId = pane_id as PaneId;
        let mut changed = false;
        if let Some(session) = self.sessions.iter_mut().find(|s| s.id == target) {
            let pane = session.session.pane_mut();
            if pane.label != label {
                pane.label = label.clone();
                changed = true;
            }
        }
        if changed {
            self.publish_event(Event::LabelChanged { pane_id, label });
        }
        changed
    }

    /// The session name for this workspace (used to populate `#{session_name}`
    /// format placeholders).
    pub fn session_name(&self) -> &str {
        &self.session_name
    }

    // Open a new pane running `command` instead of the user's shell.
    // `command` is passed to `/bin/sh -c` so shell operators (pipes,
    // redirects, compound commands) work as the user expects.
    pub fn split_active_with_command(
        &mut self,
        orientation: SplitOrientation,
        command: &str,
    ) -> io::Result<bool> {
        if !self
            .tree
            .fits_after_split(self.size, self.active, orientation)
        {
            return Ok(false);
        }
        self.unzoom();
        self.split_pane_with_command(self.active, orientation, command)?;
        Ok(true)
    }

    fn split_pane_with_command(
        &mut self,
        target: PaneId,
        orientation: SplitOrientation,
        command: &str,
    ) -> io::Result<()> {
        let new_id = self.next_pane_id;
        self.next_pane_id += 1;

        if !self.tree.split_at(target, new_id, orientation) {
            return Err(io::Error::other(format!(
                "pane id {target} not found in layout tree",
            )));
        }

        self.layout = self.compute_layout();

        let pane_frame = self
            .layout
            .pane_frame(new_id)
            .expect("split_at must leave the new pane in the tree");
        // Pane header defaults to a shortened form of the command so the
        // user can tell at a glance what's running there. The user can
        // still rename it with Ctrl-a , afterward.
        let title = truncate_command_label(command);
        let session = ManagedSession {
            id: new_id,
            title: title.clone(),
            session: Session::spawn_command(
                &title,
                "/bin/sh",
                &["-c", command],
                pane_frame.pty_size(),
                self.scrollback_lines,
                pane_frame.pty_size().rows as usize,
            )?,
            exit_status: None,
            idle: IdleDetector::new(self.idle_threshold),
            prompt: PromptDetector::new(self.shell_prompts.clone(), self.agent_prompts.clone()),
            bytes_since_emit: 0,
            last_output_emit_at: Instant::now(),
        };
        self.sessions.push(session);
        // Stash the spawn-time command on the pane so callers (MCP,
        // supervisor overlay) can show what's running in a pane that
        // wasn't spawned with the user's default shell.
        if let Some(last) = self.sessions.last_mut() {
            last.session.pane_mut().last_command = Some(command.to_string());
        }

        self.apply_layout_to_panes()?;
        self.change_active(new_id);
        self.publish_event(Event::PaneSpawned {
            pane_id: new_id as u32,
            label: None,
        });
        Ok(())
    }

    pub fn scroll_active_to_bottom(&mut self) -> bool {
        self.session_for_mut(self.active)
            .map(|session| {
                session.session.scroll_to_bottom();
                true
            })
            .unwrap_or(false)
    }

    /// Programmatic PTY-input write. Looks the pane up in this
    /// workspace's session table and forwards `bytes` straight to the
    /// underlying PTY. Used by the MCP `send_keys` tool — the
    /// daemon's existing client paths route through `handle_input`
    /// (which expects parsed key actions). Returns
    /// `ErrorKind::NotFound` if the id doesn't live in this
    /// workspace; the MCP layer surfaces that as a tool-level error.
    pub fn send_pty_input(&mut self, pane_id: u32, bytes: &[u8]) -> io::Result<()> {
        let target: PaneId = pane_id as PaneId;
        match self.session_for_mut(target) {
            Some(managed) => managed.session.write_input(bytes),
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no pane with id {pane_id} in active window"),
            )),
        }
    }

    /// Force the named pane's live primary grid into scrollback so that
    /// snapshot readers (MCP `read_pane`, future watchers) see what the
    /// renderer is currently drawing. Returns `true` on success, `false`
    /// when the pane id isn't in this workspace. The 2D grid model
    /// requires this because rows aren't line-committed to scrollback
    /// until they evict; readers that only see scrollback would otherwise
    /// miss everything that's still live on screen.
    pub fn flush_pane_grid(&mut self, pane_id: u32) -> bool {
        let target: PaneId = pane_id as PaneId;
        match self.session_for_mut(target) {
            Some(managed) => {
                managed.session.flush_grid_to_scrollback();
                true
            }
            None => false,
        }
    }

    /// Non-mutating snapshot of the named pane's visible lines, composed
    /// the same way the renderer composes them (scrollback tail +
    /// primary grid). MCP `read_pane` uses this so its read doesn't
    /// disturb the live editable area — flushing the grid would force
    /// the running TUI to find an empty grid on its next CUU and
    /// cascade redraws below the input box.
    pub fn snapshot_visible_lines(&self, pane_id: u32) -> Option<Vec<String>> {
        let target: PaneId = pane_id as PaneId;
        self.session_for(target)
            .map(|managed| managed.session.snapshot_visible_lines())
    }

    /// Cell-level counterpart to `snapshot_visible_lines`, styled. See
    /// MCP `read_pane`'s `strip_ansi=false` path.
    pub fn snapshot_visible_cells(&self, pane_id: u32) -> Option<Vec<Vec<Cell>>> {
        let target: PaneId = pane_id as PaneId;
        self.session_for(target)
            .map(|managed| managed.session.snapshot_visible_cells())
    }

    /// Non-mutating tail-of-scrollback snapshot, spanning the pane's
    /// scrollback plus the live primary grid. See `snapshot_visible_lines`
    /// for the rationale.
    pub fn snapshot_scrollback_lines(&self, pane_id: u32, lines: usize) -> Option<Vec<String>> {
        let target: PaneId = pane_id as PaneId;
        self.session_for(target)
            .map(|managed| managed.session.snapshot_scrollback_lines(lines))
    }

    /// Cell-level counterpart to `snapshot_scrollback_lines`, styled.
    /// See MCP `read_pane`'s `strip_ansi=false` path.
    pub fn snapshot_scrollback_cells(&self, pane_id: u32, lines: usize) -> Option<Vec<Vec<Cell>>> {
        let target: PaneId = pane_id as PaneId;
        self.session_for(target)
            .map(|managed| managed.session.snapshot_scrollback_cells(lines))
    }

    /// Cursor-based raw-output transcript slice for the named pane.
    /// Unlike rendered snapshots, this preserves output that scrolled
    /// out of the visible TUI surface during the current turn.
    pub fn pane_output_since(
        &self,
        pane_id: u32,
        since_byte: u64,
        max_bytes: usize,
    ) -> Option<crate::pane::PaneOutputSlice> {
        let target: PaneId = pane_id as PaneId;
        self.session_for(target)
            .map(|managed| managed.session.output_since(since_byte, max_bytes))
    }

    /// Whether the named pane's shell has DECSET 2004 active.
    /// `None` if the pane id doesn't live in this workspace. Used by the
    /// MCP `send_keys` tool to decide whether to wrap typed text in
    /// bracketed-paste markers (which lets us deliver text+CR in a
    /// single PTY write without the 75ms gap the unbracketed path needs).
    pub fn pane_bracketed_paste(&self, pane_id: u32) -> Option<bool> {
        let target: PaneId = pane_id as PaneId;
        self.session_for(target)
            .map(|managed| managed.session.bracketed_paste_enabled())
    }

    /// Fan `payload` out to every pane in this workspace whose state
    /// matches `target_filter`. Returns the list of pane ids that
    /// successfully received the payload — panes that don't match the
    /// filter (or whose underlying PTY write fails) are skipped.
    ///
    /// The supervisor `b → y → type → Enter` flow is the primary
    /// caller; the MCP layer does NOT (yet) expose this as a tool —
    /// keeping broadcast as a supervisor-driven affordance until we've
    /// learned how it gets used in practice.
    pub fn broadcast(&mut self, payload: &[u8], target_filter: BroadcastFilter) -> Vec<u32> {
        let mut targets: Vec<u32> = Vec::new();
        for session in &self.sessions {
            if session.exit_status.is_some() {
                continue;
            }
            let state = session.session.pane().agent_state.as_wire();
            let matches = match target_filter {
                BroadcastFilter::AllVisible => true,
                BroadcastFilter::OnlyWorkingOrIdle => state == "Working" || state == "Idle",
            };
            if matches {
                targets.push(session.id as u32);
            }
        }
        self.broadcast_to_panes(payload, &targets)
    }

    /// Lower-level broadcast that takes an explicit recipient list (so
    /// the supervisor can pre-filter by both the visible filter AND the
    /// Working/Idle eligibility check, then hand a final list down).
    /// Returns the subset that successfully accepted the write — a
    /// pane whose write fails or that has since closed is silently
    /// skipped, matching the "best-effort fan-out" semantics the
    /// supervisor expects.
    fn broadcast_to_panes(&mut self, payload: &[u8], recipients: &[u32]) -> Vec<u32> {
        let mut delivered: Vec<u32> = Vec::with_capacity(recipients.len());
        for pane_id in recipients {
            let target: PaneId = *pane_id as PaneId;
            if let Some(managed) = self.session_for_mut(target)
                && managed.session.write_input(payload).is_ok()
            {
                delivered.push(*pane_id);
            }
        }
        delivered
    }

    /// Programmatically spawn a new pane running `command`, splitting
    /// off either the active pane or `target` (if Some). Returns the
    /// new pane id on success. The MCP `spawn_pane` tool is the
    /// primary caller; the supervisor overlay and prefix bindings
    /// keep using the active-pane wrapper above. Refusing the split
    /// (layout too small) surfaces as a structured `io::Error` so the
    /// MCP layer can ship a tool-level error rather than silently
    /// returning a stale id.
    pub fn spawn_pane_with_command(
        &mut self,
        command: &str,
        orientation: SplitOrientation,
        target: Option<u32>,
    ) -> io::Result<u32> {
        let target_pane: PaneId = match target {
            Some(id) => {
                let id = id as PaneId;
                if !self.sessions.iter().any(|s| s.id == id) {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("no pane with id {id} in active window"),
                    ));
                }
                id
            }
            None => self.active,
        };
        if !self
            .tree
            .fits_after_split(self.size, target_pane, orientation)
        {
            return Err(io::Error::other(
                "layout cannot fit another pane in the requested orientation",
            ));
        }
        self.unzoom();
        // `next_pane_id` is read inside `split_pane_with_command` —
        // capture it before the call so we can return the freshly
        // minted id without re-reading state that may have shifted.
        let new_id = self.next_pane_id;
        self.split_pane_with_command(target_pane, orientation, command)?;
        Ok(new_id as u32)
    }

    // Nudges the active pane's border in the requested direction. Returns
    // Ok(true) when the layout actually changed and shells have been
    // resized; Ok(false) when nothing moved (e.g. the neighbor is already
    // at minimum weight, or there's no applicable sibling to steal from).
    pub fn resize_active(&mut self, direction: ResizeDirection) -> io::Result<bool> {
        if !self.tree.resize_pane(self.active, direction) {
            return Ok(false);
        }
        self.unzoom();
        self.layout = self.compute_layout();
        self.apply_layout_to_panes()?;
        Ok(true)
    }

    pub fn cycle_active_backward(&mut self) -> io::Result<bool> {
        let leaves = self.leaves_in_order();
        let len = leaves.len();
        if len <= 1 {
            return Ok(false);
        }
        let position = leaves.iter().position(|id| *id == self.active).unwrap_or(0);
        self.change_active(leaves[(position + len - 1) % len]);
        self.clear_active_pane_transients();
        self.clear_zoom_if_any()?;
        Ok(true)
    }

    pub fn ingest_available_output(&mut self) -> io::Result<bool> {
        let mut dirty = false;
        // Defer event publishing until after the iter so we don't have
        // to share `&mut self.event_bus` with the per-session loop.
        let mut events: Vec<Event> = Vec::new();
        let now = Instant::now();
        for session in &mut self.sessions {
            if session.exit_status.is_some() {
                continue;
            }
            let prev_state = session.session.pane_mut().agent_state.clone();
            let count = session.session.ingest_available_output()?;
            if count > 0 {
                dirty = true;
                session.bytes_since_emit = session.bytes_since_emit.saturating_add(count as u64);
                session.idle.note_output(now);

                // Re-derive agent state. Prompt detector wins on a
                // recognized last-line; otherwise force Working —
                // without that, an earlier AwaitingInput is sticky
                // (`note_output` only flips Idle→Working) and a build
                // streaming output would still look like it needed input.
                let last_line = session.session.pane_mut().visible_last_line();
                let new_state = if last_line
                    .as_deref()
                    .is_some_and(|line| session.prompt.is_prompt(line))
                {
                    AgentState::AwaitingInput
                } else {
                    AgentState::Working
                };
                session.idle.force_state(new_state.clone());
                session.session.pane_mut().last_output_at = now;
                if new_state != prev_state {
                    session.session.pane_mut().agent_state = new_state.clone();
                    events.push(Event::PaneStateChanged {
                        pane_id: session.id as u32,
                        from: prev_state.as_wire(),
                        to: new_state.as_wire(),
                    });
                }

                // Debounced PaneOutput: flush when ≥64 bytes seen or
                // ≥100ms since the last emit. Subscribers see at most
                // one event per debounce window per pane.
                let elapsed = now.saturating_duration_since(session.last_output_emit_at);
                if session.bytes_since_emit >= PANE_OUTPUT_BYTE_THRESHOLD
                    || elapsed >= PANE_OUTPUT_TIME_THRESHOLD
                {
                    let preview = last_line.unwrap_or_default();
                    events.push(Event::PaneOutput {
                        pane_id: session.id as u32,
                        bytes_delta: session.bytes_since_emit,
                        last_line_preview: preview,
                    });
                    session.bytes_since_emit = 0;
                    session.last_output_emit_at = now;
                }
            }
        }
        for event in events {
            self.publish_event(event);
        }
        Ok(dirty)
    }

    /// Subscribe to the broadcast event bus. Returns a freshly attached
    /// `Receiver<Event>` that observes every event the workspace
    /// publishes from this point forward. The bus itself is held
    /// privately so callers cannot bypass the workspace's "every
    /// transition originates here" invariant by publishing directly;
    /// future per-window or session-scoped buses can be added without
    /// touching this signature.
    pub fn subscribe_events(&mut self) -> std::sync::mpsc::Receiver<Event> {
        self.event_bus.subscribe()
    }

    pub(crate) fn set_session_event_bus(&mut self, bus: Arc<Mutex<EventBus>>) {
        self.session_event_bus = Some(bus);
    }

    // Mirrors the event into the supervisor overlay state so the
    // dashboard stays live without a separate subscriber. New
    // publish sites should prefer this helper.
    pub(crate) fn publish_event(&mut self, event: Event) {
        if let Some(state) = self.supervisor.as_mut() {
            state.apply_event(&event);
        }
        self.event_bus.publish(event.clone());
        if let Some(bus) = &self.session_event_bus
            && let Ok(mut bus) = bus.lock()
        {
            bus.publish(event);
        }
    }

    /// Per-frame poll. Walks every pane's `IdleDetector` and emits
    /// `PaneStateChanged` for any Working → Idle transitions, plus a
    /// debounced `PaneOutput` flush for any pane that ingested fewer
    /// than `PANE_OUTPUT_BYTE_THRESHOLD` bytes more than
    /// `PANE_OUTPUT_TIME_THRESHOLD` ago. Should run on the same
    /// cadence as the render loop (see `daemon.rs`).
    pub fn tick_agents(&mut self, now: Instant) {
        let mut events: Vec<Event> = Vec::new();
        for session in &mut self.sessions {
            if session.exit_status.is_some() {
                continue;
            }

            // Flush stranded debounced output FIRST, before the
            // state-tick early-continue. The ingest path's flush gate
            // only fires inside `if count > 0`, so a pane that emits
            // a handful of bytes (under the byte threshold) and then
            // goes quiet leaves those bytes unflushed indefinitely
            // — subscribers never see the `PaneOutput` for that
            // activity until the next non-empty ingest, which may
            // never come (e.g. `git status`). This catches every
            // non-exited pane regardless of agent_state, so an
            // AwaitingInput pane with stranded bytes still flushes.
            if session.bytes_since_emit > 0
                && now.saturating_duration_since(session.last_output_emit_at)
                    >= PANE_OUTPUT_TIME_THRESHOLD
            {
                let preview = session
                    .session
                    .pane_mut()
                    .visible_last_line()
                    .unwrap_or_default();
                events.push(Event::PaneOutput {
                    pane_id: session.id as u32,
                    bytes_delta: session.bytes_since_emit,
                    last_line_preview: preview,
                });
                session.bytes_since_emit = 0;
                session.last_output_emit_at = now;
            }

            let prev = session.session.pane_mut().agent_state.clone();
            // Don't override AwaitingInput / Errored on idle timeout —
            // those are sticky until the next ingest churns the line.
            if !matches!(prev, AgentState::Working) {
                continue;
            }
            if let Some(new_state) = session.idle.tick(now)
                && new_state != prev
            {
                session.session.pane_mut().agent_state = new_state.clone();
                events.push(Event::PaneStateChanged {
                    pane_id: session.id as u32,
                    from: prev.as_wire(),
                    to: new_state.as_wire(),
                });
            }
        }
        for event in events {
            self.publish_event(event);
        }
    }

    /// Apply a runtime agent-config update (called from the daemon's
    /// `run_server_after_bind` once `Config::load()` has resolved).
    /// Replaces every existing pane's detectors so the new threshold
    /// and patterns take effect immediately — without this, the
    /// daemon-spawned genesis pane would ignore user config until the
    /// first manual split.
    pub fn set_agent_config(
        &mut self,
        idle_threshold: Duration,
        shell_prompts: Vec<String>,
        agent_prompts: Vec<String>,
    ) {
        self.idle_threshold = idle_threshold;
        self.shell_prompts = shell_prompts.clone();
        self.agent_prompts = agent_prompts.clone();
        for session in &mut self.sessions {
            session.idle = IdleDetector::new(idle_threshold);
            session.prompt = PromptDetector::new(shell_prompts.clone(), agent_prompts.clone());
        }
    }

    /// Replace the prompt-detector pattern lists for every existing
    /// pane *without* touching their `IdleDetector` state. The
    /// regression test for the AwaitingInput-stickiness fix needs
    /// this: it forces a pane into AwaitingInput by matching one set
    /// of patterns, then must swap to never-match patterns while
    /// preserving that forced state — so the next `note_output` /
    /// re-derive runs against `idle.state() == AwaitingInput`. The
    /// full `set_agent_config` reconstructs `IdleDetector::new()`,
    /// which silently resets the detector to `Idle` and would mask
    /// the bug this method exists to pin.
    pub fn set_prompt_patterns_only(
        &mut self,
        shell_prompts: Vec<String>,
        agent_prompts: Vec<String>,
    ) {
        self.shell_prompts = shell_prompts.clone();
        self.agent_prompts = agent_prompts.clone();
        for session in &mut self.sessions {
            session.prompt = PromptDetector::new(shell_prompts.clone(), agent_prompts.clone());
        }
    }

    /// Read-only snapshot of per-pane agent metadata. Pane size
    /// comes from the active layout, not a stored field, so
    /// `Workspace` stays the single source of truth for sizing.
    /// Exited sessions are included with their last known state.
    pub fn pane_summaries(&self) -> Vec<PaneSummaryView> {
        self.sessions
            .iter()
            .map(|s| {
                let pane = s.session.pane();
                let pty = self
                    .layout
                    .pane_frame(s.id)
                    .map(|f| f.pty_size())
                    .unwrap_or_else(|| PtySize::new(0, 0));
                PaneSummaryView {
                    pane_id: s.id as u32,
                    label: pane.label.clone(),
                    state: pane.agent_state.clone(),
                    last_command: pane.last_command.clone(),
                    last_exit: pane.last_exit,
                    size_cols: pty.cols,
                    size_rows: pty.rows,
                }
            })
            .collect()
    }

    pub fn update_exit_statuses(&mut self) -> io::Result<bool> {
        let mut dirty = false;
        let mut events: Vec<Event> = Vec::new();
        for session in &mut self.sessions {
            if session.exit_status.is_some() {
                continue;
            }
            if let Some(status) = session.session.try_wait()? {
                session.exit_status = Some(status);
                let _ = session.session.ingest_available_output()?;
                dirty = true;

                let exit_code = status
                    .code()
                    .unwrap_or_else(|| 128 + status.signal().unwrap_or_default());
                let prev = session.session.pane_mut().agent_state.clone();
                let new_state = AgentState::Exited(exit_code);
                session.idle.force_state(new_state.clone());
                session.session.pane_mut().agent_state = new_state.clone();
                session.session.pane_mut().last_exit = Some(exit_code);
                if prev != new_state {
                    events.push(Event::PaneStateChanged {
                        pane_id: session.id as u32,
                        from: prev.as_wire(),
                        to: new_state.as_wire(),
                    });
                }
                events.push(Event::PaneExited {
                    pane_id: session.id as u32,
                    exit_code,
                });
            }
        }
        for event in events {
            self.publish_event(event);
        }

        // If the active pane has exited, move focus to the first live one.
        if self
            .session_for(self.active)
            .is_some_and(|pane| pane.exit_status.is_some())
            && let Some(next) = self
                .sessions
                .iter()
                .find(|session| session.exit_status.is_none())
                .map(|session| session.id)
        {
            self.change_active(next);
            dirty = true;
        }

        Ok(dirty)
    }

    pub fn exit_code_if_complete(&self) -> Option<i32> {
        if self
            .sessions
            .iter()
            .any(|session| session.exit_status.is_none())
        {
            return None;
        }

        let mut exit_code = 0;
        for session in &self.sessions {
            if let Some(status) = session.exit_status {
                let code = status
                    .code()
                    .unwrap_or_else(|| 128 + status.signal().unwrap_or_default());
                if code != 0 {
                    exit_code = code;
                    break;
                }
            }
        }
        Some(exit_code)
    }

    pub fn handle_input(&mut self, action: InputAction) -> io::Result<bool> {
        match action {
            InputAction::Forward(bytes) => {
                if self.sync_panes {
                    // Mirror to every live pane in the window. We write
                    // to the active pane first so its scroll/view state
                    // behaves the same as unsynced input; the rest
                    // receive the same bytes in iteration order.
                    let mut wrote_any = false;
                    for session in self.sessions.iter_mut() {
                        if session.exit_status.is_some() {
                            continue;
                        }
                        let payload = forward_payload_for_pane(
                            session.session.bracketed_paste_enabled(),
                            &bytes,
                        );
                        session.session.write_input(&payload)?;
                        wrote_any = true;
                    }
                    return Ok(wrote_any);
                }
                if let Some(active) = self.session_for_mut(self.active)
                    && active.exit_status.is_none()
                {
                    let payload =
                        forward_payload_for_pane(active.session.bracketed_paste_enabled(), &bytes);
                    active.session.write_input(&payload)?;
                    return Ok(true);
                }
                Ok(false)
            }
            InputAction::Mouse(mouse) => self.handle_mouse(mouse),
        }
    }

    pub fn toggle_sync_panes(&mut self) -> bool {
        self.sync_panes = !self.sync_panes;
        true
    }

    // Drain any clipboard text produced by a mouse-drag release. The
    // server pulls this right after the Input message handler runs and
    // pushes it back to the requesting client as an OSC 52 write.
    pub fn take_pending_clipboard(&mut self) -> Option<String> {
        self.pending_clipboard.take()
    }

    pub fn sync_panes_active(&self) -> bool {
        self.sync_panes
    }

    pub fn mouse_tracking_mode(&self) -> MouseTrackingMode {
        // Floor at Drag (SGR 1002) so the host terminal forwards motion
        // events while a button is held. That's what drag-to-select
        // needs; Click-only mode would only give us press+release and
        // we couldn't follow the cursor across the drag.
        self.sessions
            .iter()
            .filter(|session| session.exit_status.is_none())
            .map(|session| session.session.mouse_tracking_mode())
            .max()
            .unwrap_or(MouseTrackingMode::Off)
            .max(MouseTrackingMode::Drag)
    }

    pub fn size(&self) -> PtySize {
        self.size
    }

    pub fn set_status_label_override(&mut self, label: Option<String>) {
        self.status_label_override = label;
    }

    pub fn pane_count(&self) -> usize {
        self.sessions.len()
    }

    // Absolute 1-based screen cell where the attached client should
    // paint the host cursor, or None to keep it hidden. Hidden when:
    // full-screen/status chrome owns the keyboard (supervisor overlay,
    // command prompt, rename, scrollback search), the active pane's
    // viewport detached from live output (the cursor's cell isn't on
    // screen), the app hid its cursor via DECTCEM, or the cursor sits
    // outside the pane's content rect. The column is clamped instead of
    // hidden at the right edge so a pending-wrap cursor (one past the
    // last column) still shows on the edge cell, like real terminals.
    pub fn cursor_screen_position(&self) -> Option<(u16, u16)> {
        if self.supervisor.is_some()
            || self.command_input.is_some()
            || self.rename_input.is_some()
            || self.search_input.is_some()
            || self.selection.is_some()
        {
            return None;
        }
        let content = self
            .layout
            .panes
            .iter()
            .find(|(id, _)| *id == self.active)
            .map(|(_, layout)| layout.content)?;
        if content.width == 0 || content.height == 0 {
            return None;
        }
        let session = self.session_for(self.active)?;
        if !session.session.follow_output() {
            return None;
        }
        let (row, col) = session.session.screen_cursor()?;
        if row >= content.height as usize {
            return None;
        }
        let col = col.min(content.width as usize - 1);
        Some((
            content.y.saturating_add(row as u16).saturating_add(1),
            content.x.saturating_add(col as u16).saturating_add(1),
        ))
    }

    pub fn render_frame(&self) -> Vec<String> {
        let rows = self.size.rows as usize;
        let cols = self.size.cols as usize;
        let mut frame = vec![vec![Cell::BLANK; cols]; rows];

        // Draw row separators first, then column separators. Column
        // separators win at intersections via simple overdraw. Separators
        // are drawn in a dim grey so they recede visually from pane
        // content.
        let separator_style = Style {
            fg: Color::Indexed(8), // bright black / grey
            ..Style::DEFAULT
        };
        for separator in &self.layout.separators {
            match separator.orientation {
                SplitOrientation::Rows => {
                    if let Some(row) = frame.get_mut(separator.y as usize) {
                        let start = separator.x as usize;
                        let end = (separator.x + separator.length) as usize;
                        for col in start..end.min(row.len()) {
                            row[col] = Cell::styled('-', separator_style.clone());
                        }
                    }
                }
                SplitOrientation::Columns => {
                    let col = separator.x as usize;
                    let start = separator.y as usize;
                    let end = (separator.y + separator.length) as usize;
                    for row_index in start..end {
                        if let Some(row) = frame.get_mut(row_index)
                            && col < row.len()
                        {
                            row[col] = Cell::styled('|', separator_style.clone());
                        }
                    }
                }
            }
        }

        for (pane_id, pane_layout) in &self.layout.panes {
            if let Some(session) = self.session_for(*pane_id) {
                let focused = session.id == self.active;
                let header_style = if focused {
                    Style {
                        attrs: Attrs {
                            reverse: true,
                            bold: true,
                            ..Attrs::default()
                        },
                        ..Style::DEFAULT
                    }
                } else {
                    Style {
                        fg: Color::Indexed(8),
                        ..Style::DEFAULT
                    }
                };
                let header = self.pane_header(session, pane_layout);
                stamp_text(
                    &mut frame,
                    pane_layout.frame.x,
                    pane_layout.frame.y,
                    pane_layout.frame.width,
                    &header,
                    header_style,
                );

                let cells = session.session.render_cells();
                // Only the active pane owns a selection; non-active
                // panes just draw their cells as-is. Selection state
                // lives in scrollback-buffer coordinates (line, col),
                // so we translate the pane's current viewport_top
                // to find which buffer rows each visible row maps to.
                let active_selection = if focused { self.selection } else { None };
                let viewport_top = session.session.scrollback_viewport_top();
                for (row_offset, line) in cells
                    .iter()
                    .enumerate()
                    .take(pane_layout.content.height as usize)
                {
                    let buffer_line = viewport_top + row_offset;
                    let stamped: Vec<Cell> = if let Some(sel) = active_selection {
                        line.iter()
                            .enumerate()
                            .map(|(col, cell)| {
                                if sel.contains_cell(buffer_line, col) {
                                    Cell {
                                        ch: cell.ch,
                                        style: Style {
                                            attrs: Attrs {
                                                reverse: !cell.style.attrs.reverse,
                                                ..cell.style.attrs
                                            },
                                            ..cell.style.clone()
                                        },
                                    }
                                } else {
                                    cell.clone()
                                }
                            })
                            .collect()
                    } else {
                        line.clone()
                    };
                    stamp_cells(
                        &mut frame,
                        pane_layout.content.x,
                        pane_layout.content.y.saturating_add(row_offset as u16),
                        pane_layout.content.width,
                        &stamped,
                    );
                }
            }
        }

        // Optional big-digit overlay for Ctrl-a q. Drawn after pane content
        // so the numbers sit on top of the shell output, before the status
        // bar is written. Use a bright foreground so it's readable over
        // any pane content underneath.
        if self.pane_numbers_active() {
            let overlay_style = Style {
                fg: Color::Indexed(11), // bright yellow
                attrs: Attrs {
                    bold: true,
                    ..Attrs::default()
                },
                ..Style::DEFAULT
            };
            for (position, (_, pane)) in self.layout.panes.iter().enumerate() {
                let label = format!("{}", position + 1);
                draw_big_digits(&mut frame, pane, &label, overlay_style.clone());
            }
        }

        // Ctrl-a A supervisor overlay. Drawn on top of pane content
        // (and the optional pane-numbers overlay) so it always
        // dominates the viewport while open. Each line is stamped with
        // a default style; the selected row gets reverse video.
        if let Some(state) = self.supervisor.as_ref() {
            let supervisor_frame = render_supervisor(state, cols as u16, rows as u16);
            let normal_style = Style::DEFAULT;
            let highlight_style = Style {
                attrs: Attrs {
                    reverse: true,
                    bold: true,
                    ..Attrs::default()
                },
                ..Style::DEFAULT
            };
            for (offset, line) in supervisor_frame.lines.iter().enumerate() {
                let y = supervisor_frame.origin_row.saturating_add(offset as u16);
                let style = if Some(offset) == supervisor_frame.highlight_line {
                    highlight_style.clone()
                } else {
                    normal_style.clone()
                };
                let line_chars = line.chars().count() as u16;
                stamp_text(
                    &mut frame,
                    supervisor_frame.origin_col,
                    y,
                    line_chars,
                    line,
                    style,
                );
            }
        }

        let active_position = self
            .leaves_in_order()
            .iter()
            .position(|id| *id == self.active)
            .map(|position| position + 1)
            .unwrap_or(1);

        // Status line uses reverse video so it reads as a chrome element
        // distinct from pane content.
        let status_style = Style {
            attrs: Attrs {
                reverse: true,
                ..Attrs::default()
            },
            ..Style::DEFAULT
        };
        let status_row_index = self.layout.status_row as usize;
        if let Some(row) = frame.get_mut(status_row_index) {
            for cell in row.iter_mut() {
                *cell = Cell::styled(' ', status_style.clone());
            }

            let label = self
                .status_label_override
                .clone()
                .unwrap_or_else(|| format!("{}@{}", self.session_name, self.hostname));
            stamp_row_text(row, 0, cols, &label, status_style.clone());

            let clock = local_hms();
            let clock_start = cols.saturating_sub(clock.chars().count());
            stamp_row_text(row, clock_start, cols, &clock, status_style.clone());

            let zoom_tag = if self.zoomed.is_some() { " [Z]" } else { "" };
            // Loud tag so a user who toggled sync by accident sees it
            // immediately — mirrored input can be surprising.
            let sync_tag = if self.sync_panes { " [SYNC]" } else { "" };
            // [SCROLL] surfaces the active pane's follow_output state to
            // the status bar. Without it, scroll mode is invisible chrome
            // — the pane header flips PRI/FOLLOW → PRI/SCROLL but users
            // look at the status bar, not the pane header. The tag goes
            // away the moment the user hits G / Enter / q and the pane
            // snaps back to the live tail.
            let scroll_tag = if self.active_is_scrolled_back() {
                " [SCROLL]"
            } else {
                ""
            };
            // [SEL N lines] or [SEL char] while visual selection is
            // active. Sits next to [Z]/[SCROLL] so a glance at the
            // status bar tells the user exactly which modal state
            // they're in.
            let selection_tag = self
                .selection
                .map(|sel| {
                    let total = self
                        .session_for(self.active)
                        .map(|s| s.session.total_lines())
                        .unwrap_or(0);
                    match sel.mode {
                        SelectionMode::Line => {
                            format!(" [SEL {} lines]", sel.clamped_line_count(total))
                        }
                        SelectionMode::Char => " [SEL char]".to_string(),
                        SelectionMode::Rect => " [SEL rect]".to_string(),
                    }
                })
                .unwrap_or_default();
            // Search / rename prompts and committed-result indicators
            // preempt the normal hint strip so the user can see what
            // they're typing / navigating. A '_' cursor marks the end
            // of the input buffer.
            let middle = if let Some(name) = &self.rename_input {
                format!(" | rename: {name}_")
            } else if let Some(prompt) = &self.command_input {
                match prompt {
                    PromptState::SplitWith {
                        orientation,
                        buffer,
                    } => {
                        let label = match orientation {
                            SplitOrientation::Columns => "split-right",
                            SplitOrientation::Rows => "split-down",
                        };
                        format!(" | {label}: {buffer}_")
                    }
                    PromptState::General { buffer } => format!(" | :{buffer}_"),
                }
            } else if let Some(query) = &self.search_input {
                format!(" | search: {query}_")
            } else if let Some(results) = &self.search_results {
                if results.matches.is_empty() {
                    format!(" | no matches for '{}'  (Esc to clear)", results.query)
                } else {
                    format!(
                        " | [{}/{}] search: '{}'  (n=next N=prev)",
                        results.current + 1,
                        results.matches.len(),
                        results.query,
                    )
                }
            } else if let Some(err) = &self.prompt_error {
                format!(" | error: {err}")
            } else if self.status_bar_hints {
                format!(
                    " | {} of {}{}{}{}{} | Ctrl-a: d=detach |-=split x=close z=zoom o/p=cycle [=scroll y=yank q=nums HJKL=resize",
                    active_position,
                    self.sessions.len(),
                    zoom_tag,
                    sync_tag,
                    scroll_tag,
                    selection_tag,
                )
            } else {
                format!(
                    " | {} of {}{}{}{}{}",
                    active_position,
                    self.sessions.len(),
                    zoom_tag,
                    sync_tag,
                    scroll_tag,
                    selection_tag,
                )
            };
            let middle_start = label.chars().count();
            let middle_end = clock_start.saturating_sub(1);
            if middle_end > middle_start {
                stamp_row_text(row, middle_start, middle_end, &middle, status_style);
            }
        }

        frame.into_iter().map(|row| serialize_row(&row)).collect()
    }

    // Try to handle the mouse event as a drag-resize gesture on a pane
    // separator. Returns `Ok(Some(dirty))` when we consumed it (press
    // on a border, motion during an in-progress resize, or release of
    // an in-progress resize). `Ok(None)` means the event is some other
    // mouse event the caller should process normally.
    fn handle_mouse_resize(&mut self, mouse: MouseEvent) -> io::Result<Option<bool>> {
        if mouse.is_left_press() {
            if let Some(separator) = self.layout.separator_at(mouse.col, mouse.row) {
                let (left_or_top, right_or_bottom) = match separator.orientation {
                    SplitOrientation::Columns => (
                        self.layout
                            .pane_at(separator.x.saturating_sub(1), mouse.row),
                        self.layout
                            .pane_at(separator.x.saturating_add(1), mouse.row),
                    ),
                    SplitOrientation::Rows => (
                        self.layout
                            .pane_at(mouse.col, separator.y.saturating_sub(1)),
                        self.layout
                            .pane_at(mouse.col, separator.y.saturating_add(1)),
                    ),
                };
                if let (Some(a), Some(b)) = (left_or_top, right_or_bottom) {
                    self.mouse_resize = Some(MouseResize {
                        orientation: separator.orientation,
                        left_or_top_pane: a,
                        right_or_bottom_pane: b,
                        last_col: mouse.col,
                        last_row: mouse.row,
                    });
                    return Ok(Some(true));
                }
                // Separator hit but neighbors unresolvable: treat as a
                // non-event so it falls through to the normal path.
                return Ok(None);
            }
            return Ok(None);
        }

        let Some(mut resize) = self.mouse_resize else {
            return Ok(None);
        };

        if mouse.is_left_drag_motion() {
            let mut dirty = false;
            match resize.orientation {
                SplitOrientation::Columns => {
                    let delta = mouse.col as i32 - resize.last_col as i32;
                    for _ in 0..delta.unsigned_abs() {
                        let applied = if delta > 0 {
                            // Moving right: grow the left neighbor into
                            // its next-column sibling.
                            self.tree
                                .resize_pane(resize.left_or_top_pane, ResizeDirection::Right)
                        } else {
                            // Moving left: grow the right neighbor into
                            // its previous-column sibling.
                            self.tree
                                .resize_pane(resize.right_or_bottom_pane, ResizeDirection::Left)
                        };
                        if !applied {
                            break;
                        }
                        dirty = true;
                    }
                }
                SplitOrientation::Rows => {
                    let delta = mouse.row as i32 - resize.last_row as i32;
                    for _ in 0..delta.unsigned_abs() {
                        let applied = if delta > 0 {
                            self.tree
                                .resize_pane(resize.left_or_top_pane, ResizeDirection::Down)
                        } else {
                            self.tree
                                .resize_pane(resize.right_or_bottom_pane, ResizeDirection::Up)
                        };
                        if !applied {
                            break;
                        }
                        dirty = true;
                    }
                }
            }
            resize.last_col = mouse.col;
            resize.last_row = mouse.row;
            self.mouse_resize = Some(resize);
            if dirty {
                self.layout = self.compute_layout();
                self.apply_layout_to_panes()?;
            }
            return Ok(Some(dirty));
        }

        if mouse.is_left_release() {
            self.mouse_resize = None;
            return Ok(Some(false));
        }

        Ok(None)
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> io::Result<bool> {
        // Separator hit test — drag-resize hijacks press/motion/release
        // over a pane border so a grab on the border line becomes a
        // resize gesture instead of a selection inside the pane below.
        if let Some(dirty) = self.handle_mouse_resize(mouse)? {
            return Ok(dirty);
        }

        if let Some((pane_id, local_col, local_row)) =
            self.layout.content_position(mouse.col, mouse.row)
        {
            let active_changed = self.active != pane_id;
            if active_changed {
                self.clear_active_pane_transients();
            }
            self.change_active(pane_id);

            // Read-only probes up front, so the mutable borrow needed
            // for `handle_mouse_event` later doesn't conflict with
            // `self.selection` / `self.mouse_drag_active` access.
            let (is_primary, viewport_top, exit_status) = match self.session_for(pane_id) {
                Some(managed) => (
                    managed.session.screen_mode() == ScreenMode::Primary,
                    managed.session.scrollback_viewport_top(),
                    managed.exit_status,
                ),
                None => return Ok(active_changed),
            };
            if exit_status.is_some() {
                return Ok(active_changed);
            }

            // On the primary screen we handle click+drag ourselves to
            // produce a char-mode selection — the behavior tmux/xterm
            // users expect from dragging inside a shell. The alternate
            // screen (vim, less, etc.) is left alone; those apps enable
            // their own mouse tracking and we just forward to them.
            if is_primary {
                if mouse.is_left_press() {
                    let buffer_line = viewport_top + local_row as usize;
                    let buffer_col = local_col as usize;
                    self.selection = Some(Selection {
                        mode: SelectionMode::Char,
                        anchor_line: buffer_line,
                        anchor_col: buffer_col,
                        cursor_line: buffer_line,
                        cursor_col: buffer_col,
                    });
                    self.mouse_drag_active = true;
                    return Ok(true);
                }
                if mouse.is_left_drag_motion() && self.mouse_drag_active {
                    let buffer_line = viewport_top + local_row as usize;
                    let buffer_col = local_col as usize;
                    if let Some(selection) = self.selection.as_mut() {
                        selection.cursor_line = buffer_line;
                        selection.cursor_col = buffer_col;
                    }
                    return Ok(true);
                }
                if mouse.is_left_release() && self.mouse_drag_active {
                    self.mouse_drag_active = false;
                    if let Some(sel) = self.selection {
                        if sel.anchor_line == sel.cursor_line && sel.anchor_col == sel.cursor_col {
                            // Collapsed selection (press and release on
                            // the same cell). Nothing to copy; drop the
                            // stray [SEL char] tag.
                            self.selection = None;
                        } else if let Some(active) = self.session_for(self.active) {
                            // Auto-yank the drag selection so the user
                            // gets the desktop-terminal UX: release the
                            // mouse and the highlight is on the
                            // clipboard. Selection stays visible until
                            // the next click so the user sees what was
                            // copied.
                            let text = extract_char_stream(active, sel);
                            self.paste_buffer = Some(text.clone());
                            self.pending_clipboard = Some(text);
                        }
                    }
                    return Ok(true);
                }
            }

            let Some(managed) = self.session_for_mut(pane_id) else {
                return Ok(active_changed);
            };
            let local_mouse = mouse
                .translate(mouse.col - local_col, mouse.row - local_row)
                .expect("content hit-test must yield in-bounds coordinates");
            let handled = managed.session.handle_mouse_event(local_mouse)?;
            return Ok(active_changed || handled || mouse.is_left_press());
        }

        if mouse.wheel_lines().is_some()
            && let Some(active) = self.session_for_mut(self.active)
            && active.exit_status.is_none()
            && active.session.screen_mode() == ScreenMode::Primary
        {
            return active.session.handle_mouse_event(mouse);
        }

        if mouse.is_left_press()
            && let Some(pane_id) = self.layout.pane_at(mouse.col, mouse.row)
        {
            let changed = self.active != pane_id;
            if changed {
                self.clear_active_pane_transients();
            }
            self.change_active(pane_id);
            return Ok(changed);
        }

        Ok(false)
    }

    fn leaves_in_order(&self) -> Vec<PaneId> {
        self.tree.leaves()
    }

    fn session_for(&self, pane_id: PaneId) -> Option<&ManagedSession> {
        self.sessions.iter().find(|session| session.id == pane_id)
    }

    fn session_for_mut(&mut self, pane_id: PaneId) -> Option<&mut ManagedSession> {
        self.sessions
            .iter_mut()
            .find(|session| session.id == pane_id)
    }

    // Single point of truth for focus changes. Sets self.active and, for
    // each side of the transition whose shell has DECSET 1004 active,
    // writes the matching focus marker (`ESC[O` on the previously-active
    // pane, `ESC[I` on the newly-active one). Per-PTY write failures
    // are swallowed so a dead/closing shell on one side can't block a
    // focus change from completing on the other.
    pub(crate) fn change_active(&mut self, new_id: PaneId) {
        let old_id = self.active;
        self.active = new_id;
        if old_id == new_id {
            return;
        }
        if let Some(prev) = self.session_for_mut(old_id)
            && prev.session.focus_events_enabled()
        {
            let _ = prev.session.write_input(focus_change_bytes(false));
        }
        if let Some(next) = self.session_for_mut(new_id)
            && next.session.focus_events_enabled()
        {
            let _ = next.session.write_input(focus_change_bytes(true));
        }
    }

    // Look up the underlying terminal Pane by its workspace-scoped id.
    // Used by the daemon's Capture admin handler to attach a sink to a
    // specific pane. The id space is per-workspace (`next_pane_id`) so
    // callers should already have resolved which workspace they mean.
    pub fn pane_by_id_mut(&mut self, pane_id: PaneId) -> Option<&mut crate::pane::Pane> {
        self.session_for_mut(pane_id).map(|s| s.session.pane_mut())
    }

    fn apply_layout_to_panes(&mut self) -> io::Result<()> {
        let assignments: Vec<(PaneId, PtySize)> = self
            .layout
            .panes
            .iter()
            .map(|(id, pane)| (*id, pane.pty_size()))
            .collect();
        for (id, size) in assignments {
            if let Some(session) = self.session_for_mut(id) {
                session.resize(size)?;
            }
        }
        Ok(())
    }

    fn pane_header(&self, session: &ManagedSession, pane: &PaneLayout) -> String {
        let focus = if session.id == self.active { '*' } else { ' ' };
        let mode = match session.session.screen_mode() {
            ScreenMode::Primary if session.session.follow_output() => "PRI/FOLLOW",
            ScreenMode::Primary => "PRI/SCROLL",
            ScreenMode::Alternate if session.session.app_captures_mouse() => "ALT/APP",
            ScreenMode::Alternate => "ALT/PANE",
        };
        let process = match session.exit_status {
            Some(status) => format!("exit {}", status.code().unwrap_or_default()),
            None => "live".to_string(),
        };

        format!(
            "{focus}[{}:{}x{} {} {}]",
            session.title, pane.content.width, pane.content.height, mode, process
        )
    }
}

impl ManagedSession {
    fn resize(&mut self, size: PtySize) -> io::Result<()> {
        self.session.resize(size, size.rows as usize)
    }
}

// Local wall-clock HH:MM:SS via libc::localtime_r. Falls back to UTC math
// if the libc call misbehaves — which shouldn't happen on any Linux we
// care about, but avoids panicking if the TZ database is unreachable.
fn local_hms() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    let mut tm = MaybeUninit::<PosixTm>::uninit();
    // Zero the buffer so the trailing bytes we don't populate don't leak
    // random stack garbage if the libc impl reads them (none we know of
    // do, but it's free insurance).
    unsafe {
        std::ptr::write_bytes(tm.as_mut_ptr(), 0, 1);
    }
    let result = unsafe { localtime_r(&seconds as *const i64, tm.as_mut_ptr()) };
    if result.is_null() {
        let hour = (seconds / 3600).rem_euclid(24);
        let minute = (seconds / 60).rem_euclid(60);
        let second = seconds.rem_euclid(60);
        return format!("{:02}:{:02}:{:02}Z", hour, minute, second);
    }

    let tm = unsafe { tm.assume_init() };
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

// Best-effort hostname. Linux exposes it via /proc; fall back to the
// Pull the text of a char-mode selection out of the active pane's
// scrollback. The selection covers a stream from start=(line,col) to
// end=(line,col) inclusive at both ends. For multi-line selections
// the first partial line runs from start.col to end-of-line, middle
// lines are taken whole, and the final partial line runs from 0 to
// end.col. Trailing blanks are trimmed only from the final line —
// middle lines keep their full content so aligned output (like tree
// views or tables) survives the yank.
fn extract_char_stream(active: &ManagedSession, sel: Selection) -> String {
    // Clamp an inclusive column index to the `[0, len]` slice range
    // used for `chars[lo..hi]`: `end_col + 1` capped at the line length.
    fn slice_end(end_col: usize, len: usize) -> usize {
        end_col.saturating_add(1).min(len)
    }
    let (start, end) = sel.stream_endpoints();
    let mut out = String::new();
    // One-line case: just the substring.
    if start.0 == end.0 {
        let line_text = active.session.extract_scrollback_lines(start.0, start.0);
        let chars: Vec<char> = line_text.chars().collect();
        let lo = start.1.min(chars.len());
        let hi = slice_end(end.1, chars.len());
        if hi > lo {
            out.extend(chars[lo..hi].iter());
        }
        return out;
    }
    // Multi-line: partial first line.
    let first = active.session.extract_scrollback_lines(start.0, start.0);
    let first_chars: Vec<char> = first.chars().collect();
    let lo = start.1.min(first_chars.len());
    if first_chars.len() > lo {
        out.extend(first_chars[lo..].iter());
    }
    out.push('\n');
    // Middle lines, if any.
    if end.0 > start.0 + 1 {
        let middle = active
            .session
            .extract_scrollback_lines(start.0 + 1, end.0 - 1);
        out.push_str(&middle);
        out.push('\n');
    }
    // Partial last line.
    let last = active.session.extract_scrollback_lines(end.0, end.0);
    let last_chars: Vec<char> = last.chars().collect();
    let hi = slice_end(end.1, last_chars.len());
    if hi > 0 {
        out.extend(last_chars[..hi].iter());
    }
    out
}

#[cfg(test)]
fn handle_mouse_test_input(col: u16, row: u16, button: u16, final_byte: u8) -> MouseEvent {
    MouseEvent {
        button,
        col,
        row,
        final_byte,
    }
}

// Pull a rectangular region out of the active pane's scrollback. For
// every line in [row_lo, row_hi], take chars[col_lo..=col_hi] with
// padding when the row is shorter than col_hi + 1 (so the result
// keeps its column alignment). Rows are joined with newlines.
// Trailing whitespace on each row slice is preserved — rectangular
// selections are for aligned table-style output where trimming would
// destroy the alignment.
fn extract_rect_stream(active: &ManagedSession, sel: Selection) -> String {
    let (row_lo, row_hi) = sel.line_range();
    let (col_lo, col_hi) = sel.col_range();
    let width = col_hi.saturating_sub(col_lo) + 1;
    let mut out = String::new();
    for row in row_lo..=row_hi {
        if row > row_lo {
            out.push('\n');
        }
        let line_text = active.session.extract_scrollback_lines(row, row);
        let chars: Vec<char> = line_text.chars().collect();
        for offset in 0..width {
            let col = col_lo + offset;
            if let Some(ch) = chars.get(col) {
                out.push(*ch);
            } else {
                out.push(' ');
            }
        }
    }
    out
}

// Shared prompt-buffer byte handler: appends printable ASCII up to a
// 128-char cap, treats 0x7f/0x08 as backspace, and silently drops
// everything else. Returns true when the buffer changed so the caller
// can use it as a dirty-frame signal.
fn append_prompt_bytes(buffer: &mut String, bytes: &[u8]) -> bool {
    let mut changed = false;
    for &byte in bytes {
        match byte {
            0x7f | 0x08 if buffer.pop().is_some() => {
                changed = true;
            }
            byte if (0x20..0x7f).contains(&byte) && buffer.chars().count() < 128 => {
                buffer.push(byte as char);
                changed = true;
            }
            _ => {}
        }
    }
    changed
}

// Decide which DECSET-1004 marker to emit on a focus transition. Pure
// helper so it can be unit-tested without spinning up a Workspace.
// `gained` true => `ESC[I`; false => `ESC[O`.
pub(crate) fn focus_change_bytes(gained: bool) -> &'static [u8] {
    if gained { b"\x1b[I" } else { b"\x1b[O" }
}

// Wrap pasted text with the bracketed-paste markers when the destination
// shell has DECSET 2004 active. Returns owned bytes either way so the
// caller can write them to the PTY without juggling lifetimes. When
// 2004 is off we fall through with a plain copy.
pub(crate) fn bracket_paste_if_enabled(enabled: bool, text: &str) -> Vec<u8> {
    if !enabled {
        return text.as_bytes().to_vec();
    }
    let mut out = Vec::with_capacity(text.len() + 12);
    out.extend_from_slice(b"\x1b[200~");
    out.extend_from_slice(text.as_bytes());
    out.extend_from_slice(b"\x1b[201~");
    out
}

// Bytes forwarded from the host terminal (`InputAction::Forward`) carry
// the `ESC[200~`/`ESC[201~` bracketed-paste markers once the host has
// DECSET 2004 enabled (see `TerminalGuard::enter`). Whether the
// destination pane's own shell wants to see those markers depends on
// whether IT asked for bracketed paste, independent of the host: pass
// them through untouched when the pane's DECSET 2004 is on (the shell
// knows what to do with them), otherwise strip the marker sequences and
// deliver just the pasted text — a shell that never enabled bracketed
// paste has no idea what `ESC[200~` means and would otherwise echo it
// as literal garbage.
fn forward_payload_for_pane(pane_bracketed_paste_enabled: bool, bytes: &[u8]) -> Vec<u8> {
    if pane_bracketed_paste_enabled {
        return bytes.to_vec();
    }
    strip_bracketed_paste_markers(bytes)
}

fn strip_bracketed_paste_markers(bytes: &[u8]) -> Vec<u8> {
    const START: &[u8] = b"\x1b[200~";
    const END: &[u8] = b"\x1b[201~";
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index..].starts_with(START) {
            index += START.len();
        } else if bytes[index..].starts_with(END) {
            index += END.len();
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    out
}

// Produce a short, header-friendly label for a shell command. Takes the
// first whitespace token so "tail -f /var/log/syslog" becomes "tail".
// If there's no token, fall back to something non-empty so the pane
// header isn't misleadingly blank.
fn truncate_command_label(command: &str) -> String {
    command
        .split_whitespace()
        .next()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "cmd".to_string())
}

// $HOSTNAME env var; fall back to a literal if neither is available.
fn read_hostname() -> String {
    if let Ok(name) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
        let trimmed = name.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    if let Ok(name) = std::env::var("HOSTNAME")
        && !name.is_empty()
    {
        return name;
    }
    "host".to_string()
}

#[cfg(test)]
mod tests {
    use super::{
        BroadcastFilter, InputAction, PromptKind, Selection, SelectionMode, Workspace, stamp_text,
    };
    use crate::PtySize;
    use crate::agent::AgentState;
    use crate::layout::SplitOrientation;
    use crate::style::{Cell, Style};

    /// Strip ANSI escape sequences from a serialised row string so tests can
    /// assert on plain text content without caring about colour attributes.
    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\x1b' {
                // Consume the rest of the escape sequence.
                if chars.peek() == Some(&'[') {
                    chars.next();
                    // CSI: consume until a letter [A-Za-z]
                    for c in chars.by_ref() {
                        if c.is_ascii_alphabetic() {
                            break;
                        }
                    }
                } else {
                    // Other escape — consume one char.
                    chars.next();
                }
            } else {
                out.push(ch);
            }
        }
        out
    }

    #[test]
    fn focus_change_helper_emits_the_right_marker() {
        // Pure helper that maps "this pane gained focus" / "this pane
        // lost focus" to the bytes a DECSET-1004-aware shell expects.
        // Workspace::change_active uses this on each side of every focus
        // transition; here we just lock in the byte sequences so the
        // host-facing wire format can't drift silently.
        use super::focus_change_bytes;

        assert_eq!(focus_change_bytes(true), b"\x1b[I");
        assert_eq!(focus_change_bytes(false), b"\x1b[O");
    }

    #[test]
    fn bracket_paste_helper_wraps_only_when_2004_enabled() {
        // The active pane's shell decides whether pasted text gets the
        // `ESC[200~ ... ESC[201~` envelope. With 2004 off (the default
        // for a fresh pane) the bytes go through unwrapped so a plain
        // shell sees the same input as if it had been typed.
        use super::bracket_paste_if_enabled;

        let plain = bracket_paste_if_enabled(false, "hello\n");
        assert_eq!(plain, b"hello\n");

        let wrapped = bracket_paste_if_enabled(true, "hello\n");
        assert_eq!(
            wrapped, b"\x1b[200~hello\n\x1b[201~",
            "wrapped form must include both the open and close markers",
        );
    }

    #[test]
    fn forward_payload_passes_bracketed_paste_markers_through_when_pane_wants_them() {
        // Mirror image of the yank/paste helper above, but for bytes
        // arriving from the *host* terminal via InputAction::Forward:
        // when the destination pane's own shell has DECSET 2004 on, the
        // `ESC[200~ ... ESC[201~` markers the host sent must reach it
        // byte-for-byte untouched.
        use super::forward_payload_for_pane;

        let bytes = b"\x1b[200~hello\nworld\x1b[201~";
        assert_eq!(forward_payload_for_pane(true, bytes), bytes.to_vec());
    }

    #[test]
    fn forward_payload_strips_bracketed_paste_markers_when_pane_never_enabled_them() {
        // A shell that never sent `ESC[?2004h` has no idea what the
        // marker bytes mean and would otherwise echo them as literal
        // garbage — strip the markers and deliver just the payload.
        use super::forward_payload_for_pane;

        let bytes = b"\x1b[200~hello\nworld\x1b[201~";
        assert_eq!(
            forward_payload_for_pane(false, bytes),
            b"hello\nworld".to_vec()
        );
    }

    #[test]
    fn forward_payload_leaves_plain_bytes_alone_either_way() {
        use super::forward_payload_for_pane;

        assert_eq!(forward_payload_for_pane(true, b"ls -la\r"), b"ls -la\r");
        assert_eq!(forward_payload_for_pane(false, b"ls -la\r"), b"ls -la\r");
    }

    #[test]
    fn handle_input_strips_paste_markers_for_a_pane_without_bracketed_paste() {
        // End-to-end through `handle_input`: a fresh pane's shell
        // hasn't enabled DECSET 2004, so a host-forwarded paste
        // envelope must land in the pane with the markers gone and
        // only the payload written to the PTY (which the shell then
        // echoes back, proving the stripped text actually arrived).
        let size = PtySize { rows: 24, cols: 80 };
        let mut ws = Workspace::spawn_single("/bin/sh", size).expect("spawn workspace");
        assert!(
            !ws.sessions[0].session.bracketed_paste_enabled(),
            "a fresh pane's shell has not enabled bracketed paste"
        );

        let payload = b"\x1b[200~echo paste-marker-probe\r\x1b[201~".to_vec();
        ws.handle_input(InputAction::Forward(payload))
            .expect("forward input");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut plain = String::new();
        while std::time::Instant::now() < deadline {
            let _ = ws.ingest_available_output();
            plain = ws
                .render_frame()
                .iter()
                .map(|row| strip_ansi(row))
                .collect::<Vec<_>>()
                .join("\n");
            if plain.contains("paste-marker-probe") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            plain.contains("paste-marker-probe"),
            "expected the stripped payload text on screen, got: {plain:?}"
        );
        assert!(
            !plain.contains("200~") && !plain.contains("201~"),
            "bracketed-paste markers must not reach a pane that never enabled 2004: {plain:?}"
        );
    }

    #[test]
    fn stamp_text_respects_the_requested_width() {
        let mut frame = vec![vec![Cell::BLANK; 8]];
        stamp_text(&mut frame, 1, 0, 4, "abcdef", Style::DEFAULT);

        let rendered: String = frame.remove(0).iter().map(|cell| cell.ch).collect();
        assert_eq!(rendered, " abcd   ");
    }

    #[test]
    fn cursor_screen_position_maps_active_pane_and_hides_for_overlays() {
        // The attached client paints the host cursor wherever this
        // says; None means "keep it hidden". A fresh pane's cursor sits
        // at its content origin (1-based absolute screen coords, below
        // the pane header). Any full-screen chrome that owns the
        // keyboard (supervisor, command prompt, rename) must suppress
        // the pane cursor while it's up.
        let mut workspace =
            Workspace::spawn_two_pane("/bin/sh", PtySize::new(24, 80)).expect("spawn workspace");

        let content = workspace
            .layout
            .panes
            .iter()
            .find(|(id, _)| *id == workspace.active)
            .map(|(_, layout)| layout.content)
            .expect("active pane layout");
        assert_eq!(
            workspace.cursor_screen_position(),
            Some((content.y + 1, content.x + 1)),
            "fresh active pane shows its cursor at the content origin",
        );

        workspace.open_supervisor();
        assert_eq!(
            workspace.cursor_screen_position(),
            None,
            "supervisor overlay suppresses the pane cursor",
        );
        workspace.close_supervisor();
        assert!(workspace.cursor_screen_position().is_some());

        workspace.rename_input = Some(String::new());
        assert_eq!(
            workspace.cursor_screen_position(),
            None,
            "rename prompt suppresses the pane cursor",
        );
        workspace.rename_input = None;

        // A pane whose viewport detached from the live bottom (user
        // scrolled back) hides the cursor — it refers to a cell that
        // isn't on screen.
        let active = workspace.active;
        let session = workspace
            .sessions
            .iter_mut()
            .find(|s| s.id == active)
            .expect("active session");
        for _ in 0..64 {
            session.session.pane_mut().append_output_line(vec![]);
        }
        session.session.wheel_up(8);
        assert!(!session.session.follow_output());
        assert_eq!(
            workspace.cursor_screen_position(),
            None,
            "scrolled-back pane suppresses the cursor",
        );
    }

    #[test]
    fn split_active_vertically_adds_a_pane_to_the_right() {
        let mut workspace =
            Workspace::spawn_two_pane("/bin/sh", PtySize::new(24, 80)).expect("spawn workspace");
        assert_eq!(workspace.pane_count(), 2);

        assert!(
            workspace
                .split_active(SplitOrientation::Columns)
                .expect("split")
        );
        assert_eq!(workspace.pane_count(), 3);

        // The newly-spawned pane must be focused.
        assert_eq!(workspace.sessions.last().unwrap().id, workspace.active);
    }

    #[test]
    fn split_active_horizontally_stacks_a_pane_below() {
        let mut workspace =
            Workspace::spawn_single("/bin/sh", PtySize::new(40, 120)).expect("spawn workspace");

        assert!(
            workspace
                .split_active(SplitOrientation::Rows)
                .expect("split")
        );
        assert_eq!(workspace.pane_count(), 2);

        // Two stacked panes share the full width; the new bottom pane
        // starts below the first.
        let leaves = workspace.leaves_in_order();
        assert_eq!(leaves.len(), 2);
        let top = workspace.layout.pane_frame(leaves[0]).expect("top pane");
        let bottom = workspace.layout.pane_frame(leaves[1]).expect("bottom pane");
        assert_eq!(top.frame.x, bottom.frame.x);
        assert!(bottom.frame.y > top.frame.y);
    }

    #[test]
    fn close_active_refuses_the_last_pane() {
        let mut workspace =
            Workspace::spawn_single("/bin/sh", PtySize::new(24, 80)).expect("spawn workspace");
        let removed = workspace.close_active().expect("close call");
        assert!(!removed);
        assert_eq!(workspace.pane_count(), 1);
    }

    #[test]
    fn close_active_shrinks_mixed_layout_back_to_a_single_leaf() {
        let mut workspace =
            Workspace::spawn_two_pane("/bin/sh", PtySize::new(24, 80)).expect("spawn workspace");
        workspace
            .split_active(SplitOrientation::Rows)
            .expect("split");
        assert_eq!(workspace.pane_count(), 3);

        // Close the newly-created pane; the tree collapses the degenerate
        // single-child inner split back into a flat two-column layout.
        assert!(workspace.close_active().expect("close"));
        assert_eq!(workspace.pane_count(), 2);
    }

    #[test]
    fn apply_preset_quadrants_builds_a_2x2_grid() {
        let mut workspace =
            Workspace::spawn_single("/bin/sh", PtySize::new(40, 120)).expect("spawn workspace");

        assert!(
            workspace
                .apply_preset(super::LayoutPreset::Quadrants)
                .expect("apply preset")
        );
        assert_eq!(workspace.pane_count(), 4);

        // 2x2 means two vertical separators-per-row and two horizontal:
        // renderer emits one of each per split, so top-level Cols + two
        // Rows children → 1 column divider + 2 row dividers = 3.
        assert_eq!(workspace.layout.separators.len(), 3);
    }

    #[test]
    fn apply_preset_refuses_when_workspace_already_has_multiple_panes() {
        let mut workspace =
            Workspace::spawn_single("/bin/sh", PtySize::new(24, 80)).expect("spawn workspace");
        workspace
            .split_active(super::SplitOrientation::Columns)
            .expect("initial split");

        assert!(
            !workspace
                .apply_preset(super::LayoutPreset::Quadrants)
                .expect("preset call")
        );
        assert_eq!(
            workspace.pane_count(),
            2,
            "preset must leave existing layout alone"
        );
    }

    #[test]
    fn cycle_active_walks_tree_leaves_in_pre_order() {
        let mut workspace =
            Workspace::spawn_two_pane("/bin/sh", PtySize::new(24, 80)).expect("spawn workspace");

        let initial = workspace.active;
        assert!(workspace.cycle_active().expect("cycle"));
        assert_ne!(workspace.active, initial);
        assert!(workspace.cycle_active().expect("cycle"));
        assert_eq!(workspace.active, initial);
    }

    #[test]
    fn toggle_zoom_reduces_layout_to_single_full_frame_pane() {
        let mut workspace =
            Workspace::spawn_two_pane("/bin/sh", PtySize::new(24, 80)).expect("spawn workspace");
        assert_eq!(workspace.layout.panes.len(), 2);
        let active = workspace.active;

        assert!(workspace.toggle_zoom().expect("zoom in"));
        assert!(workspace.is_zoomed());
        // Only the active pane remains renderable; its frame covers the
        // whole body (status row excluded).
        assert_eq!(workspace.layout.panes.len(), 1);
        assert_eq!(workspace.layout.panes[0].0, active);
        assert_eq!(workspace.layout.panes[0].1.frame.x, 0);
        assert_eq!(workspace.layout.panes[0].1.frame.width, 80);
        assert_eq!(workspace.layout.separators.len(), 0);

        // Toggling a second time restores the original two-pane layout.
        assert!(workspace.toggle_zoom().expect("zoom out"));
        assert!(!workspace.is_zoomed());
        assert_eq!(workspace.layout.panes.len(), 2);
    }

    #[test]
    fn splitting_while_zoomed_implicitly_unzooms() {
        let mut workspace =
            Workspace::spawn_single("/bin/sh", PtySize::new(40, 120)).expect("spawn workspace");
        workspace
            .split_active(SplitOrientation::Columns)
            .expect("initial split");

        workspace.toggle_zoom().expect("zoom in");
        assert!(workspace.is_zoomed());

        // Splitting while zoomed must clear the zoom so the user sees
        // both the pre-existing sibling and the new pane.
        assert!(
            workspace
                .split_active(SplitOrientation::Rows)
                .expect("split")
        );
        assert!(!workspace.is_zoomed());
        assert_eq!(workspace.layout.panes.len(), 3);
    }

    #[test]
    fn search_prompt_state_machine_advances_correctly() {
        // Exercises begin → input → backspace → commit → clear without
        // depending on PTY-driven scrollback content. The "no matches"
        // path is what's covered here; the per-line scan itself is
        // tested directly against ScrollbackBuffer::search.
        let mut workspace =
            Workspace::spawn_single("/bin/sh", PtySize::new(24, 80)).expect("spawn workspace");

        assert!(workspace.begin_search());
        assert!(workspace.search_input.is_some());

        workspace.search_input_bytes(b"hello");
        assert_eq!(workspace.search_input.as_deref(), Some("hello"));

        // 0x7f is the DEL byte; treated as backspace by the prompt.
        workspace.search_input_bytes(&[0x7f]);
        assert_eq!(workspace.search_input.as_deref(), Some("hell"));

        assert!(workspace.commit_search());
        assert!(workspace.search_input.is_none());
        let results = workspace
            .search_results
            .as_ref()
            .expect("commit must populate results");
        assert_eq!(results.query, "hell");
        assert!(
            results.matches.is_empty(),
            "no PTY output was driven into the buffer, so there's nothing to match",
        );

        assert!(workspace.clear_search());
        assert!(workspace.search_input.is_none());
        assert!(workspace.search_results.is_none());
    }

    #[test]
    fn cycling_panes_clears_lingering_search_state() {
        let mut workspace =
            Workspace::spawn_two_pane("/bin/sh", PtySize::new(24, 80)).expect("spawn workspace");
        workspace.begin_search();
        workspace.search_input_bytes(b"foo");
        workspace.commit_search();
        assert!(workspace.search_results.is_some());

        workspace.cycle_active().expect("cycle");
        assert!(
            workspace.search_results.is_none(),
            "search state belongs to the previous active pane",
        );
    }

    #[test]
    fn cycling_while_zoomed_reveals_all_panes() {
        let mut workspace =
            Workspace::spawn_two_pane("/bin/sh", PtySize::new(24, 80)).expect("spawn workspace");

        workspace.toggle_zoom().expect("zoom in");
        assert!(workspace.is_zoomed());

        assert!(workspace.cycle_active().expect("cycle"));
        assert!(!workspace.is_zoomed(), "cycle must unzoom");
    }

    #[test]
    fn line_selection_begin_extend_yank_clear() {
        let mut workspace =
            Workspace::spawn_single("/bin/sh", PtySize::new(24, 80)).expect("spawn workspace");
        assert!(!workspace.has_selection());

        assert!(workspace.begin_selection(SelectionMode::Line));
        assert!(workspace.has_selection());

        // One-line buffer: extend clamps without moving.
        assert!(!workspace.extend_selection_down(5));
        assert!(!workspace.extend_selection_to_bottom());

        let yanked = workspace.yank_selection().expect("yank produces text");
        assert!(!workspace.has_selection(), "yank must clear state");
        let _ = yanked;

        assert!(!workspace.clear_selection());
    }

    #[test]
    fn cycling_panes_clears_any_pending_selection() {
        let mut workspace =
            Workspace::spawn_two_pane("/bin/sh", PtySize::new(24, 80)).expect("spawn workspace");

        assert!(workspace.begin_selection(SelectionMode::Line));
        assert!(workspace.has_selection());

        workspace.cycle_active().expect("cycle");
        assert!(
            !workspace.has_selection(),
            "selection belongs to the previous active pane",
        );
    }

    #[test]
    fn char_mode_extends_column_and_yanks_stream() {
        let mut workspace =
            Workspace::spawn_single("/bin/sh", PtySize::new(24, 120)).expect("spawn workspace");

        assert!(workspace.begin_selection(SelectionMode::Char));
        // Char motion should move the cursor column and return true.
        assert!(workspace.extend_selection_right(5));
        assert!(workspace.extend_selection_left(2));

        // Yanking a char-mode selection on an empty buffer is still
        // valid — we only require that it doesn't panic and that it
        // clears the state.
        let _ = workspace.yank_selection();
        assert!(!workspace.has_selection());
    }

    #[test]
    fn mouse_drag_begins_and_extends_a_char_selection_with_auto_yank() {
        use super::handle_mouse_test_input as press;
        let mut workspace =
            Workspace::spawn_single("/bin/sh", PtySize::new(24, 120)).expect("spawn workspace");

        // Row 0 is the pane header — use row 1 for content. Press at
        // (col=2, row=1) anchors a char selection.
        workspace
            .handle_input(InputAction::Mouse(press(2, 1, 0, b'M')))
            .expect("press");
        assert!(workspace.has_selection(), "press must start a selection");
        assert!(workspace.mouse_drag_active, "drag flag should be true");

        // Drag motion: left button held (button | 32), col advances.
        workspace
            .handle_input(InputAction::Mouse(press(6, 1, 32, b'M')))
            .expect("drag");
        let sel = workspace.selection.expect("still selecting");
        assert_eq!(sel.cursor_col, 6);

        // Release: drag flag clears; pending_clipboard populates because
        // the selection spans more than one cell.
        workspace
            .handle_input(InputAction::Mouse(press(6, 1, 0, b'm')))
            .expect("release");
        assert!(
            !workspace.mouse_drag_active,
            "drag flag must clear on release"
        );
        let _yanked = workspace
            .take_pending_clipboard()
            .expect("auto-yank must produce text");

        // A second click + release on the same cell is treated as a
        // bare click: selection clears, no pending clipboard.
        workspace
            .handle_input(InputAction::Mouse(press(1, 1, 0, b'M')))
            .expect("press");
        workspace
            .handle_input(InputAction::Mouse(press(1, 1, 0, b'm')))
            .expect("release");
        assert!(
            !workspace.has_selection(),
            "collapsed click must not leave a selection"
        );
        assert!(workspace.take_pending_clipboard().is_none());
    }

    #[test]
    fn wheel_event_outside_layout_scrolls_active_primary_pane() {
        use super::handle_mouse_test_input as ev;
        let mut workspace =
            Workspace::spawn_single("/bin/sh", PtySize::new(6, 20)).expect("spawn workspace");
        let active = workspace.active;

        {
            let managed = workspace.session_for_mut(active).expect("active pane");
            for index in 1..=8 {
                managed
                    .session
                    .pane_mut()
                    .append_plain(&format!("line {index}"));
            }
        }

        let before = workspace
            .session_for(active)
            .expect("active pane")
            .session
            .scrollback_viewport_top();
        assert!(before > 0, "fixture must start at the bottom of scrollback");

        let handled = workspace
            .handle_input(InputAction::Mouse(ev(90, 27, 64, b'M')))
            .expect("wheel outside layout");

        let after = workspace
            .session_for(active)
            .expect("active pane")
            .session
            .scrollback_viewport_top();
        assert!(handled, "out-of-layout wheel should be handled");
        assert!(
            after < before,
            "wheel-up outside the layout should still scroll the active primary pane"
        );
    }

    #[test]
    fn yank_populates_paste_buffer() {
        // After a yank, the paste buffer holds the same text the
        // clipboard got — so Ctrl-a ] can write it back into a pane
        // without round-tripping through the host system clipboard.
        let mut workspace =
            Workspace::spawn_single("/bin/sh", PtySize::new(24, 80)).expect("spawn workspace");
        assert!(workspace.paste_buffer.is_none());

        workspace.begin_selection(SelectionMode::Line);
        let _ = workspace.yank_selection();
        // Even an empty yank populates the buffer; the point is that
        // the field now shadows whatever the last yank produced.
        assert!(
            workspace.paste_buffer.is_some(),
            "yank_selection should populate paste_buffer",
        );
    }

    #[test]
    fn mouse_drag_on_separator_resizes_neighboring_panes() {
        use super::handle_mouse_test_input as ev;
        let mut workspace =
            Workspace::spawn_two_pane("/bin/sh", PtySize::new(40, 120)).expect("two-pane");
        let before_left = workspace
            .layout
            .pane_frame(workspace.leaves_in_order()[0])
            .unwrap()
            .frame
            .width;

        // Find the vertical separator column.
        let sep = workspace
            .layout
            .separators
            .iter()
            .copied()
            .find(|s| matches!(s.orientation, SplitOrientation::Columns))
            .expect("two-pane layout must have a column separator");

        // Press ON the separator: starts a resize drag.
        workspace
            .handle_input(InputAction::Mouse(ev(sep.x, sep.y + 1, 0, b'M')))
            .expect("press");
        assert!(
            workspace.mouse_resize.is_some(),
            "press on separator must grab it"
        );

        // Motion 6 cells right: resizes left pane outward 6 times.
        workspace
            .handle_input(InputAction::Mouse(ev(sep.x + 6, sep.y + 1, 32, b'M')))
            .expect("drag");

        let after_left = workspace
            .layout
            .pane_frame(workspace.leaves_in_order()[0])
            .unwrap()
            .frame
            .width;
        assert!(
            after_left > before_left,
            "left pane width should grow when dragging the separator right: {before_left} → {after_left}",
        );

        // Release clears the grab.
        workspace
            .handle_input(InputAction::Mouse(ev(sep.x + 6, sep.y + 1, 0, b'm')))
            .expect("release");
        assert!(workspace.mouse_resize.is_none());
    }

    #[test]
    fn cycle_preset_advances_through_all_three_presets() {
        // last_preset is initialized to Quadrants, so the first cycle
        // lands on TwoColumns (2 panes), the next on ThreeColumns (3),
        // the next on Quadrants (4), and so on.
        let mut workspace =
            Workspace::spawn_single("/bin/sh", PtySize::new(40, 120)).expect("spawn workspace");
        assert_eq!(workspace.sessions.len(), 1);

        assert!(workspace.cycle_preset().expect("cycle"));
        assert_eq!(workspace.sessions.len(), 2);

        // Subsequent cycles require closing back down to one pane — the
        // production binding-flow does, but at the workspace level we
        // just confirm cycle_preset refuses when sessions.len() > 1
        // (apply_preset's guard).
        assert!(!workspace.cycle_preset().expect("cycle on multi-pane"));
    }

    #[test]
    fn rect_mode_contains_only_cells_inside_the_rectangle() {
        // Pure logic test on the Selection shape: pick anchor/cursor
        // across two rows and two columns and make sure contains_cell
        // matches the rectangle rather than the stream shape char
        // mode would produce.
        let sel = Selection {
            mode: SelectionMode::Rect,
            anchor_line: 2,
            anchor_col: 3,
            cursor_line: 5,
            cursor_col: 7,
        };
        // Corners all inside.
        assert!(sel.contains_cell(2, 3));
        assert!(sel.contains_cell(5, 7));
        assert!(sel.contains_cell(3, 5));
        // Outside the column range but inside the row range — would be
        // inside a Char selection, not a Rect.
        assert!(!sel.contains_cell(3, 2));
        assert!(!sel.contains_cell(3, 8));
        // Outside the row range.
        assert!(!sel.contains_cell(1, 5));
        assert!(!sel.contains_cell(6, 5));
    }

    #[test]
    fn char_mode_column_motion_is_noop_in_line_mode() {
        let mut workspace =
            Workspace::spawn_single("/bin/sh", PtySize::new(24, 80)).expect("spawn workspace");
        workspace.begin_selection(SelectionMode::Line);
        assert!(
            !workspace.extend_selection_right(5),
            "line-mode selections should ignore column motion",
        );
    }

    #[test]
    fn swap_active_with_next_moves_active_to_siblings_slot() {
        let mut workspace =
            Workspace::spawn_two_pane("/bin/sh", PtySize::new(24, 80)).expect("spawn workspace");
        let before = workspace.leaves_in_order();
        assert_eq!(before.len(), 2);
        let active_id = workspace.active;
        assert!(workspace.swap_active_with_next().expect("swap"));

        // Active pane still points at the same Session, but its slot
        // moved. leaves_in_order should now have the two ids flipped.
        assert_eq!(workspace.active, active_id);
        let after = workspace.leaves_in_order();
        assert_ne!(before, after, "visual order must change after swap");
        assert_eq!(
            after
                .iter()
                .copied()
                .collect::<std::collections::HashSet<_>>(),
            before
                .iter()
                .copied()
                .collect::<std::collections::HashSet<_>>(),
            "no ids gained or lost"
        );

        // A second swap wraps back to the original order on a 2-pane
        // layout.
        assert!(workspace.swap_active_with_next().expect("swap back"));
        assert_eq!(workspace.leaves_in_order(), before);
        assert_eq!(workspace.active, active_id);
    }

    #[test]
    fn swap_previous_is_the_inverse_of_swap_next() {
        let mut workspace =
            Workspace::spawn_two_pane("/bin/sh", PtySize::new(24, 80)).expect("spawn workspace");
        let before = workspace.leaves_in_order();

        workspace.swap_active_with_next().expect("swap next");
        workspace.swap_active_with_previous().expect("swap prev");
        assert_eq!(
            workspace.leaves_in_order(),
            before,
            "swap-next then swap-prev should be identity",
        );
    }

    #[test]
    fn swap_on_single_pane_is_noop() {
        let mut workspace =
            Workspace::spawn_single("/bin/sh", PtySize::new(24, 80)).expect("spawn workspace");
        assert!(!workspace.swap_active_with_next().expect("swap"));
        assert!(!workspace.swap_active_with_previous().expect("swap"));
    }

    #[test]
    fn rename_prompt_updates_active_pane_title_on_commit() {
        let mut workspace =
            Workspace::spawn_single("/bin/sh", PtySize::new(24, 80)).expect("spawn workspace");
        // Default title is `pane-{id}`; render_frame should have it in
        // the pane header before we start.
        let lines_before = workspace.render_frame();
        assert!(lines_before.iter().any(|line| line.contains("[pane-")));

        assert!(workspace.begin_rename());
        assert!(workspace.rename_input_bytes(b"worker"));
        assert!(workspace.commit_rename());

        let lines_after = workspace.render_frame();
        assert!(
            lines_after.iter().any(|line| line.contains("[worker")),
            "renamed title should appear in the pane header: {lines_after:?}",
        );
    }

    #[test]
    fn rename_prompt_empty_commit_leaves_title_unchanged() {
        let mut workspace =
            Workspace::spawn_single("/bin/sh", PtySize::new(24, 80)).expect("spawn workspace");
        let before_title = workspace
            .session_for(workspace.active)
            .map(|s| s.title.clone())
            .expect("active session");

        workspace.begin_rename();
        // No characters typed before Enter — commit must not blank the
        // header.
        assert!(workspace.commit_rename());
        let after_title = workspace
            .session_for(workspace.active)
            .map(|s| s.title.clone())
            .expect("active session");
        assert_eq!(before_title, after_title);
    }

    #[test]
    fn rename_prompt_respects_backspace_and_ignores_control_bytes() {
        let mut workspace =
            Workspace::spawn_single("/bin/sh", PtySize::new(24, 80)).expect("spawn workspace");
        workspace.begin_rename();
        workspace.rename_input_bytes(b"zmux\x07"); // BEL byte ignored
        workspace.rename_input_bytes(&[0x7f]); // DEL → backspace
        workspace.rename_input_bytes(b"Y");
        workspace.commit_rename();

        let title = workspace
            .session_for(workspace.active)
            .map(|s| s.title.clone())
            .expect("active session");
        assert_eq!(title, "zmuY");
    }

    #[test]
    fn command_prompt_commits_and_spawns_a_new_pane() {
        let mut workspace =
            Workspace::spawn_single("/bin/sh", PtySize::new(24, 120)).expect("spawn workspace");
        let panes_before = workspace.leaves_in_order().len();

        assert!(workspace.begin_command_prompt(SplitOrientation::Columns));
        assert!(workspace.command_input_bytes(b"true"));
        assert!(
            workspace.commit_command_prompt().expect("commit"),
            "commit should spawn a pane when the buffer is non-empty",
        );
        assert_eq!(workspace.leaves_in_order().len(), panes_before + 1);

        // Header of the new pane should be labeled with the command's
        // first token, truncated.
        let lines = workspace.render_frame();
        assert!(
            lines.iter().any(|line| line.contains("[true:")),
            "new pane header should be labeled with the command name: {lines:?}",
        );
    }

    #[test]
    fn command_prompt_empty_commit_does_not_spawn_a_pane() {
        let mut workspace =
            Workspace::spawn_single("/bin/sh", PtySize::new(24, 120)).expect("spawn workspace");
        let panes_before = workspace.leaves_in_order().len();

        workspace.begin_command_prompt(SplitOrientation::Rows);
        // No command typed — commit should no-op without panicking.
        assert!(
            !workspace.commit_command_prompt().expect("commit"),
            "empty commit should not spawn a pane",
        );
        assert_eq!(workspace.leaves_in_order().len(), panes_before);
    }

    #[test]
    fn toggle_sync_panes_flips_flag_and_surfaces_in_status_bar() {
        let mut workspace =
            Workspace::spawn_single("/bin/sh", PtySize::new(24, 120)).expect("spawn workspace");
        assert!(!workspace.sync_panes_active());

        workspace.toggle_sync_panes();
        assert!(workspace.sync_panes_active());

        let lines = workspace.render_frame();
        assert!(
            lines.iter().any(|line| line.contains("[SYNC]")),
            "status bar must advertise [SYNC] when sync-panes is active",
        );

        workspace.toggle_sync_panes();
        assert!(!workspace.sync_panes_active());
        let lines = workspace.render_frame();
        assert!(
            !lines.iter().any(|line| line.contains("[SYNC]")),
            "tag must disappear when toggled off",
        );
    }

    #[test]
    fn status_label_override_replaces_default_label() {
        let mut workspace =
            Workspace::spawn_single("/bin/sh", PtySize::new(24, 80)).expect("spawn workspace");

        let baseline = workspace.render_frame();
        let status_default = baseline.last().expect("status row").clone();
        assert!(status_default.contains('@'), "default label uses name@host");

        workspace.set_status_label_override(Some("my-label".to_string()));
        let lines = workspace.render_frame();
        let status = lines.last().expect("status row");
        assert!(
            status.contains("my-label"),
            "override must appear in the status bar: {status:?}",
        );
    }

    #[test]
    fn fresh_workspace_has_no_scroll_tag() {
        // A brand-new workspace is following its shell's output, so the
        // `[SCROLL]` status-bar tag must not be present. Once
        // follow_output flips to false (which happens the moment the
        // buffer has history and the user scrolls up), the tag appears;
        // that transition is already tested in scrollback.rs.
        let workspace =
            Workspace::spawn_single("/bin/sh", PtySize::new(24, 80)).expect("spawn workspace");
        assert!(!workspace.active_is_scrolled_back());
        let lines = workspace.render_frame();
        assert!(
            !lines.iter().any(|line| line.contains("[SCROLL]")),
            "fresh workspace should not render [SCROLL]",
        );
    }

    #[test]
    fn general_command_prompt_stores_input_and_returns_it_on_commit() {
        let size = PtySize { rows: 24, cols: 80 };
        let mut ws = Workspace::spawn_single("/bin/sh", size).unwrap();
        assert!(ws.begin_general_command_prompt());
        ws.command_input_bytes(b"kill-pane -t 3");
        let committed = ws.commit_general_command_prompt().unwrap();
        assert_eq!(committed.as_deref(), Some("kill-pane -t 3"));
    }

    #[test]
    fn general_prompt_empty_commit_returns_none() {
        let size = PtySize { rows: 24, cols: 80 };
        let mut ws = Workspace::spawn_single("/bin/sh", size).unwrap();
        assert!(ws.begin_general_command_prompt());
        let committed = ws.commit_general_command_prompt().unwrap();
        assert!(committed.is_none());
    }

    #[test]
    fn general_prompt_cancel_clears_state() {
        let size = PtySize { rows: 24, cols: 80 };
        let mut ws = Workspace::spawn_single("/bin/sh", size).unwrap();
        ws.begin_general_command_prompt();
        ws.command_input_bytes(b"whatever");
        assert!(ws.cancel_command_prompt());
        // After cancel, committing should return None.
        assert!(ws.commit_general_command_prompt().unwrap().is_none());
    }

    #[test]
    fn split_prompt_and_general_prompt_are_distinct_modes() {
        use crate::layout::SplitOrientation;
        let size = PtySize { rows: 24, cols: 80 };
        let mut ws = Workspace::spawn_single("/bin/sh", size).unwrap();
        ws.begin_command_prompt(SplitOrientation::Columns);
        ws.command_input_bytes(b"echo hi");
        // Wrong-kind commit must be surfaced as Err, not silently swallowed.
        assert!(ws.commit_general_command_prompt().is_err());
        // And the SplitWith prompt must still be intact — the second
        // cancel must report "there was a prompt" (returns true), proving
        // the rejected general commit didn't consume the split state.
        assert!(ws.cancel_command_prompt());
    }

    #[test]
    fn active_prompt_kind_returns_none_when_no_prompt() {
        let size = PtySize { rows: 24, cols: 80 };
        let ws = Workspace::spawn_single("/bin/sh", size).unwrap();
        assert_eq!(ws.active_prompt_kind(), None);
    }

    #[test]
    fn active_prompt_kind_returns_general_during_general_prompt() {
        let size = PtySize { rows: 24, cols: 80 };
        let mut ws = Workspace::spawn_single("/bin/sh", size).unwrap();
        ws.begin_general_command_prompt();
        assert_eq!(ws.active_prompt_kind(), Some(PromptKind::General));
    }

    #[test]
    fn active_prompt_kind_returns_split_with_during_split_prompt() {
        use crate::layout::SplitOrientation;
        let size = PtySize { rows: 24, cols: 80 };
        let mut ws = Workspace::spawn_single("/bin/sh", size).unwrap();
        ws.begin_command_prompt(SplitOrientation::Columns);
        assert_eq!(ws.active_prompt_kind(), Some(PromptKind::SplitWith));
    }

    #[test]
    fn prompt_error_round_trip() {
        let size = PtySize { rows: 24, cols: 80 };
        let mut ws = Workspace::spawn_single("/bin/sh", size).unwrap();
        assert!(ws.prompt_error().is_none());
        ws.set_prompt_error("oops");
        assert_eq!(ws.prompt_error(), Some("oops"));
        let taken = ws.take_prompt_error();
        assert_eq!(taken.as_deref(), Some("oops"));
        // After take, it should be gone.
        assert!(ws.prompt_error().is_none());
    }

    #[test]
    fn prompt_error_appears_in_status_bar() {
        let size = PtySize { rows: 24, cols: 80 };
        let mut ws = Workspace::spawn_single("/bin/sh", size).unwrap();
        ws.set_prompt_error("unknown command: foobar");
        let frame = ws.render_frame();
        // The last row is the status bar; join its content.
        let last_row_raw = frame.last().unwrap();
        // Strip ANSI escapes to get plain text.
        let plain: String = strip_ansi(last_row_raw);
        assert!(
            plain.contains("error: unknown command: foobar"),
            "status bar did not show prompt_error; got: {plain:?}"
        );
    }

    #[test]
    fn session_name_and_active_pane_id_accessors() {
        let size = PtySize { rows: 24, cols: 80 };
        let ws = Workspace::spawn_single("/bin/sh", size).unwrap();
        // Default spawn uses empty session name.
        let _ = ws.session_name(); // just ensure it doesn't panic
        let pane = ws.active_pane_id();
        // There must be at least one pane after spawn.
        assert!(pane < usize::MAX);
    }

    /// Regression test for the low-volume PaneOutput stranding bug.
    ///
    /// The bug: `ingest_available_output` only checks the debounce
    /// flush gate (≥64 bytes OR ≥100ms elapsed) inside the
    /// `if count > 0` branch. A pane that emitted, say, 10 bytes once
    /// and then went quiet leaves those bytes in `bytes_since_emit`
    /// indefinitely — subscribers never see the `PaneOutput` event
    /// for that activity until the next non-empty ingest (which may
    /// never come, e.g. a `git status` run).
    ///
    /// The fix: `tick_agents` now flushes any non-exited pane whose
    /// `bytes_since_emit > 0` and whose last emit is older than
    /// `PANE_OUTPUT_TIME_THRESHOLD`. This test seeds the state
    /// directly (no shell — the bug isn't about reading from the PTY,
    /// it's about what tick does with already-counted bytes) and
    /// asserts the event lands.
    #[test]
    fn tick_flushes_stranded_low_volume_pane_output() {
        use crate::events::Event;
        use std::time::{Duration, Instant};
        let size = PtySize::new(24, 80);
        let mut ws = Workspace::spawn_single("/bin/sh", size).expect("spawn workspace");
        let rx = ws.subscribe_events();

        // Drain anything the shell already produced + any events the
        // ingest path published. We want to start from a known state
        // where the next PaneOutput we see came strictly from the
        // tick path, not from real shell traffic.
        let _ = ws.ingest_available_output().expect("drain pre-existing");
        while rx.try_recv().is_ok() {}

        // Seed the bug condition: a small pending byte count + a
        // stale `last_output_emit_at` so the time threshold is
        // already exceeded by the time we tick. 10 bytes is well
        // under the 64-byte threshold; the 250ms backdate is safely
        // past the 100ms time threshold.
        let now = Instant::now();
        {
            let session = ws.sessions.first_mut().expect("genesis session");
            session.bytes_since_emit = 10;
            session.last_output_emit_at = now - Duration::from_millis(250);
            // Pane state is Idle by default; the flush must not be
            // gated on Working — that's the whole point of the fix.
            session.session.pane_mut().agent_state = AgentState::Idle;
        }

        ws.tick_agents(now);

        // Walk the receiver looking for the flushed PaneOutput. Allow
        // for any other unrelated events to be present, but the
        // PaneOutput we want must be there with the seeded byte count.
        let mut saw_flush = false;
        while let Ok(event) = rx.try_recv() {
            if let Event::PaneOutput { bytes_delta, .. } = event
                && bytes_delta == 10
            {
                saw_flush = true;
                break;
            }
        }
        assert!(
            saw_flush,
            "tick_agents did not flush the stranded 10-byte PaneOutput; \
             low-volume panes will silently strand events",
        );

        // Counters must reset so a follow-up tick (with no new bytes)
        // doesn't re-publish the same flush.
        let session = ws.sessions.first().expect("genesis session");
        assert_eq!(session.bytes_since_emit, 0);
    }

    /// Regression test for the AwaitingInput-stickiness bug.
    ///
    /// The bug: in `ingest_available_output`, the non-prompt branch
    /// previously copied `idle.state().clone()` instead of unconditionally
    /// using `AgentState::Working`. Since `note_output` only flips
    /// Idle→Working (never AwaitingInput→Working), a pane that had been
    /// pushed to AwaitingInput by a prompt match would stay stuck there
    /// even as fresh non-prompt bytes streamed in — agent CLIs (claude
    /// code, aider) building/running tests would falsely look like they
    /// were waiting on input.
    ///
    /// Pinning this requires reproducing both:
    ///   - `pane.agent_state == AwaitingInput` (the workspace-side cache)
    ///   - `idle.state() == AwaitingInput` (the detector internal state)
    ///
    /// at the moment fresh output arrives. Direct mutation here (with
    /// no detector reset between forcing AwaitingInput and the next
    /// ingest) is the only deterministic way: a real shell prompt is
    /// flaky in non-interactive PTY environments, and going through
    /// `set_agent_config` (as the previous integration test did)
    /// silently constructs a fresh `IdleDetector::new()` that resets
    /// state to Idle — which masks the bug.
    #[test]
    fn awaiting_input_unsticks_when_output_arrives_without_prompt() {
        use std::time::{Duration, Instant};
        let size = PtySize::new(24, 80);
        let mut ws = Workspace::spawn_single("/bin/sh", size).expect("spawn workspace");

        // Swap to never-match patterns *without* resetting the detector.
        // The bug branch fires when the prompt detector returns false
        // for the rendered last line; `##NEVERMATCH##` guarantees that
        // no realistic shell echo can accidentally match.
        ws.set_prompt_patterns_only(vec!["##NEVERMATCH##".into()], vec![]);

        // Force the precondition: both pane state and detector state at
        // AwaitingInput. The bug only manifests when both halves agree
        // on AwaitingInput at the moment the next ingest runs.
        {
            let session = ws.sessions.first_mut().expect("genesis session");
            session.session.pane_mut().agent_state = AgentState::AwaitingInput;
            session.idle.force_state(AgentState::AwaitingInput);
        }

        // Drain anything the shell already printed before we forced the
        // state — otherwise `ingest_available_output` could silently
        // unstick us via that earlier output. We don't care what state
        // it leaves us in; we re-force AwaitingInput right after.
        let _ = ws
            .ingest_available_output()
            .expect("drain pre-existing output");
        {
            let session = ws.sessions.first_mut().expect("genesis session");
            session.session.pane_mut().agent_state = AgentState::AwaitingInput;
            session.idle.force_state(AgentState::AwaitingInput);
        }

        // Send a deterministic non-prompt line through the shell. `printf`
        // is in every POSIX shell's exec path; no newline-completion
        // overhead like `echo` and no risk of matching a prompt pattern.
        let payload = b"printf 'streaming-bytes-no-prompt-here\\n'\r".to_vec();
        ws.handle_input(InputAction::Forward(payload))
            .expect("forward input");

        // Drain output. The pane MUST move to Working when the next
        // non-empty ingest runs through the non-prompt branch. Loop a
        // few hundred ms because the shell takes a tick to read its
        // input and emit the printf'd bytes.
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut final_state = AgentState::AwaitingInput;
        while Instant::now() < deadline {
            let _ = ws.ingest_available_output().expect("ingest");
            final_state = ws
                .sessions
                .first()
                .map(|s| s.session.pane().agent_state.clone())
                .unwrap_or(AgentState::Idle);
            if matches!(final_state, AgentState::Working) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            matches!(final_state, AgentState::Working),
            "pane stayed at {final_state:?} after non-prompt output; expected Working \
             (the AwaitingInput-stickiness fix has been reverted)",
        );
    }

    fn ws_with_two_panes() -> Workspace {
        let mut ws =
            Workspace::spawn_single_named("/bin/sh", PtySize { rows: 24, cols: 80 }, "svtest")
                .expect("spawn workspace");
        ws.split_active(SplitOrientation::Columns)
            .expect("split workspace");
        ws
    }

    #[test]
    fn open_supervisor_populates_rows_and_sets_flag() {
        let mut ws = ws_with_two_panes();
        ws.open_supervisor();
        assert!(ws.supervisor_open());
        let state = ws.supervisor_state().expect("supervisor open");
        assert_eq!(state.rows.len(), 2);
    }

    #[test]
    fn supervisor_j_then_enter_attaches_to_selected_pane() {
        let mut ws = ws_with_two_panes();
        ws.open_supervisor();
        // Move down one row, capture which pane that row points at,
        // then commit. Active pane id MUST equal what the overlay
        // had selected at commit time.
        ws.supervisor_handle_key(b'j').expect("j");
        let expected_pane = ws
            .supervisor_state()
            .expect("supervisor open")
            .selected_pane()
            .expect("row 1 has a pane id");
        ws.supervisor_handle_key(b'\r').expect("enter");
        assert!(!ws.supervisor_open(), "Enter must close the overlay");
        assert_eq!(
            ws.active_pane_id() as u32,
            expected_pane,
            "Enter must focus the supervisor's selected pane",
        );
    }

    #[test]
    fn supervisor_q_closes_overlay_without_focus_change() {
        let mut ws = ws_with_two_panes();
        let before = ws.active_pane_id();
        ws.open_supervisor();
        ws.supervisor_handle_key(b'q').expect("q closes");
        assert!(!ws.supervisor_open());
        assert_eq!(ws.active_pane_id(), before);
    }

    #[test]
    fn supervisor_capital_k_arms_kill_confirm_then_n_cancels() {
        let mut ws = ws_with_two_panes();
        let pane_count_before = ws.pane_count();
        ws.open_supervisor();
        ws.supervisor_handle_key(b'K').expect("K arms confirm");
        assert!(
            ws.supervisor_state().unwrap().has_pending_kill(),
            "K must arm the kill confirm",
        );
        ws.supervisor_handle_key(b'n').expect("n cancels");
        assert!(!ws.supervisor_state().unwrap().has_pending_kill());
        assert_eq!(ws.pane_count(), pane_count_before, "n must NOT kill");
    }

    #[test]
    fn supervisor_label_input_renames_pane_and_emits_event() {
        let mut ws = ws_with_two_panes();
        let rx = ws.subscribe_events();
        ws.open_supervisor();
        ws.supervisor_handle_key(b'l').expect("l opens label input");
        for b in b"hi" {
            ws.supervisor_handle_key(*b).expect("byte");
        }
        ws.supervisor_handle_key(b'\r')
            .expect("enter commits label");
        // The selected row's pane id should now have label "hi".
        let summaries = ws.pane_summaries();
        assert!(
            summaries.iter().any(|s| s.label.as_deref() == Some("hi")),
            "expected a pane to be labeled \"hi\"; got {summaries:?}",
        );
        // A LabelChanged event should have been broadcast.
        let mut saw_label_event = false;
        while let Ok(event) = rx.try_recv() {
            if matches!(event, crate::events::Event::LabelChanged { .. }) {
                saw_label_event = true;
            }
        }
        assert!(saw_label_event, "expected a LabelChanged broadcast");
    }

    #[test]
    fn broadcast_sends_to_all_working_or_idle_panes() {
        let mut ws = ws_with_two_panes();
        let payload = b"echo broadcast\n";
        let recipients = ws.broadcast(payload, BroadcastFilter::OnlyWorkingOrIdle);
        // The exact count depends on what state the freshly-spawned
        // panes report (Idle by default), so only assert non-empty.
        assert!(
            !recipients.is_empty(),
            "expected at least one Working/Idle pane recipient, got none",
        );
    }

    #[test]
    fn supervisor_b_then_y_then_text_then_enter_runs_full_broadcast() {
        // End-to-end through the workspace key handler: 'b' arms the
        // confirm bar, 'y' advances to Typing, "hi" populates the buffer,
        // Enter commits and clears the modal. Doesn't assert on PTY
        // contents (that's covered by the workspace-level test above)
        // — focuses on the modal state machine wiring.
        let mut ws = ws_with_two_panes();
        ws.open_supervisor();
        ws.supervisor_handle_key(b'b').expect("b arms confirm");
        assert!(
            matches!(
                ws.supervisor_state().unwrap().broadcast,
                Some(crate::supervisor::BroadcastState::Confirm)
            ),
            "expected BroadcastState::Confirm",
        );
        ws.supervisor_handle_key(b'y')
            .expect("y advances to typing");
        assert!(
            matches!(
                ws.supervisor_state().unwrap().broadcast,
                Some(crate::supervisor::BroadcastState::Typing(_))
            ),
            "expected BroadcastState::Typing",
        );
        ws.supervisor_handle_key(b'h').expect("h appended");
        ws.supervisor_handle_key(b'i').expect("i appended");
        if let Some(crate::supervisor::BroadcastState::Typing(buf)) =
            &ws.supervisor_state().unwrap().broadcast
        {
            assert_eq!(buf, "hi");
        } else {
            panic!("expected Typing buffer to hold 'hi'");
        }
        ws.supervisor_handle_key(b'\r').expect("enter commits");
        assert!(
            ws.supervisor_state().unwrap().broadcast.is_none(),
            "Enter must clear the broadcast modal back to Browse",
        );
        assert!(ws.supervisor_open(), "overlay stays open after broadcast");
    }

    #[test]
    fn supervisor_b_then_n_cancels_broadcast() {
        let mut ws = ws_with_two_panes();
        ws.open_supervisor();
        ws.supervisor_handle_key(b'b').expect("b arms confirm");
        ws.supervisor_handle_key(b'n').expect("n cancels");
        assert!(ws.supervisor_state().unwrap().broadcast.is_none());
    }

    #[test]
    fn supervisor_reflects_external_label_change_while_open() {
        // Regression test for the publish_event mirror path: when an open
        // supervisor sees a label change that originated outside the
        // overlay (e.g. via the wire-level SetLabel admin message), the
        // dashboard row must update without re-opening the overlay.
        let mut ws = ws_with_two_panes();
        ws.open_supervisor();
        let target = ws
            .supervisor_state()
            .expect("supervisor open")
            .rows
            .first()
            .map(|row| row.pane_id)
            .expect("at least one row");
        assert!(
            ws.set_pane_label(target, Some("watcher".into())),
            "set_pane_label should find the target pane",
        );
        let row = ws
            .supervisor_state()
            .expect("supervisor still open")
            .rows
            .iter()
            .find(|row| row.pane_id == target)
            .expect("row for target pane");
        assert_eq!(
            row.label.as_deref(),
            Some("watcher"),
            "supervisor row did not pick up external label change",
        );
    }
}
