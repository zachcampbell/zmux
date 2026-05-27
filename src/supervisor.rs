// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Ctrl-a A overlay: live dashboard of every pane in the session.

use crate::events::Event;

/// Output of `render_supervisor`. Plain ASCII/UTF-8 lines plus the
/// (col, row) anchor where the workspace renderer should stamp the
/// top-left of the box, plus the row index (within `lines`) that should
/// be highlighted with reverse video to indicate selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorFrame {
    pub origin_col: u16,
    pub origin_row: u16,
    pub lines: Vec<String>,
    /// Row offset (within `lines`) of the line that should be drawn
    /// with reverse video. None when the row list is empty.
    pub highlight_line: Option<usize>,
}

impl SupervisorFrame {
    pub fn lines(&self) -> &[String] {
        &self.lines
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum FilterMode {
    #[default]
    All,
    Working,
    Idle,
    Awaiting,
    Errored,
}

#[derive(Debug, Default)]
pub struct SupervisorState {
    pub rows: Vec<SupervisorRow>,
    pub selected: usize,
    pub filter: FilterMode,
    /// When Some, an inline single-line text editor for renaming the
    /// selected pane is open at the bottom of the overlay box.
    pub label_input: Option<String>,
    /// When Some, a "kill pane N? y/N" confirm bar is open at the
    /// bottom of the overlay box. The value is the pane id queued for
    /// destruction; `y` confirms, anything else cancels.
    pub pending_kill: Option<u32>,
    /// When Some, the broadcast modal is open. See [`BroadcastState`]
    /// for the two sub-states (`Confirm` then `Typing`). After the
    /// supervisor handler reads the committed payload it returns to
    /// [`Browse`](BroadcastState) by setting this back to `None`.
    pub broadcast: Option<BroadcastState>,
}

/// Sub-states for the supervisor broadcast modal.
///
/// Flow: `Browse` → `b` → `Confirm` → `y/Y` → `Typing(String)` → `Enter`
/// commits the buffer to every working/idle pane in the current filter.
/// `Esc` (in `Typing`) or any non-`y` key (in `Confirm`) cancels back
/// to `Browse`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BroadcastState {
    Confirm,
    Typing(String),
}

/// Recipient policy for `Workspace::broadcast`.
///
/// `AllVisible` writes to every live pane; `OnlyWorkingOrIdle` skips
/// panes whose agent state is `AwaitingInput` or `Errored` (the
/// supervisor's recommended default — broadcasting a command into a
/// pane that's mid-prompt usually isn't what you want).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BroadcastFilter {
    AllVisible,
    OnlyWorkingOrIdle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorRow {
    pub pane_id: u32,
    pub label: Option<String>,
    pub state: String,
    pub last_command: Option<String>,
    pub age_secs: u64,
}

impl SupervisorState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply_event(&mut self, event: &Event) {
        match event {
            Event::PaneSpawned { pane_id, label } => {
                self.rows.push(SupervisorRow {
                    pane_id: *pane_id,
                    label: label.clone(),
                    state: "Idle".into(),
                    last_command: None,
                    age_secs: 0,
                });
            }
            Event::PaneClosed { pane_id } => {
                self.rows.retain(|r| r.pane_id != *pane_id);
                let visible_len = self.visible_rows().len();
                if self.selected >= visible_len && visible_len > 0 {
                    self.selected = visible_len - 1;
                } else if visible_len == 0 {
                    self.selected = 0;
                }
            }
            Event::PaneStateChanged { pane_id, to, .. } => {
                if let Some(row) = self.rows.iter_mut().find(|r| r.pane_id == *pane_id) {
                    row.state = to.clone();
                }
                // A state change can shift visibility under non-All
                // filters. Clamp `selected` so it never points past the
                // visible list.
                let visible_len = self.visible_rows().len();
                if self.selected >= visible_len && visible_len > 0 {
                    self.selected = visible_len - 1;
                } else if visible_len == 0 {
                    self.selected = 0;
                }
            }
            Event::LabelChanged { pane_id, label } => {
                if let Some(row) = self.rows.iter_mut().find(|r| r.pane_id == *pane_id) {
                    row.label = label.clone();
                }
            }
            _ => {}
        }
    }

    pub fn move_down(&mut self) {
        let len = self.visible_rows().len();
        if len > 0 {
            self.selected = (self.selected + 1) % len;
        }
    }

    pub fn move_up(&mut self) {
        let len = self.visible_rows().len();
        if len > 0 {
            self.selected = self.selected.checked_sub(1).unwrap_or(len - 1);
        }
    }

    pub fn selected_pane(&self) -> Option<u32> {
        self.visible_rows().get(self.selected).map(|r| r.pane_id)
    }

    /// Advance the filter through `All → Working → Idle → Awaiting →
    /// Errored → All`. Resets `selected` so the highlight always starts
    /// at the top of the new filtered list (avoids pointing at a row
    /// that the new filter just hid).
    pub fn cycle_filter(&mut self) {
        self.filter = match self.filter {
            FilterMode::All => FilterMode::Working,
            FilterMode::Working => FilterMode::Idle,
            FilterMode::Idle => FilterMode::Awaiting,
            FilterMode::Awaiting => FilterMode::Errored,
            FilterMode::Errored => FilterMode::All,
        };
        self.selected = 0;
    }

    /// Rows the renderer should display, after applying the current
    /// `filter`. Selection indices are also indices into this slice —
    /// not into `rows`.
    pub fn visible_rows(&self) -> Vec<&SupervisorRow> {
        self.rows
            .iter()
            .filter(|r| match self.filter {
                FilterMode::All => true,
                FilterMode::Working => r.state == "Working",
                FilterMode::Idle => r.state == "Idle",
                FilterMode::Awaiting => r.state == "AwaitingInput",
                FilterMode::Errored => r.state == "Errored",
            })
            .collect()
    }

    pub fn begin_label_input(&mut self) {
        if self.selected < self.rows.len() {
            self.label_input = Some(String::new());
            // Begin shouldn't coexist with a kill confirm; the latter
            // takes priority but if a label edit was requested, drop
            // the kill confirm to avoid an ambiguous bottom bar.
            self.pending_kill = None;
        }
    }

    pub fn cancel_label_input(&mut self) -> bool {
        self.label_input.take().is_some()
    }

    /// Append a single byte to the in-progress label input. Returns
    /// `Some(label)` if the byte committed (Enter), `None` otherwise.
    /// Backspace pops the last char. Other control bytes are ignored;
    /// printable ASCII is appended.
    pub fn label_input_byte(&mut self, byte: u8) -> Option<String> {
        let buffer = self.label_input.as_mut()?;
        match byte {
            b'\r' | b'\n' => {
                let committed = std::mem::take(buffer);
                self.label_input = None;
                Some(committed)
            }
            0x7f | 0x08 => {
                buffer.pop();
                None
            }
            b if (0x20..0x7f).contains(&b) => {
                buffer.push(b as char);
                None
            }
            _ => None,
        }
    }

    pub fn begin_kill_confirm(&mut self) {
        if let Some(id) = self.selected_pane() {
            self.pending_kill = Some(id);
            self.label_input = None;
        }
    }

    /// Confirm or cancel a pending kill.
    /// Returns `Some(pane_id)` to kill, `None` if cancelled or no kill pending.
    pub fn resolve_kill_confirm(&mut self, byte: u8) -> Option<u32> {
        let pending = self.pending_kill.take()?;
        if matches!(byte, b'y' | b'Y') {
            Some(pending)
        } else {
            None
        }
    }

    pub fn has_label_input(&self) -> bool {
        self.label_input.is_some()
    }

    pub fn has_pending_kill(&self) -> bool {
        self.pending_kill.is_some()
    }

    /// True when the broadcast modal (confirm or typing) is open.
    pub fn has_broadcast(&self) -> bool {
        self.broadcast.is_some()
    }

    /// Begin the broadcast flow: park us in [`BroadcastState::Confirm`].
    /// Caller is responsible for ensuring no other modal is open.
    pub fn begin_broadcast_confirm(&mut self) {
        self.label_input = None;
        self.pending_kill = None;
        self.broadcast = Some(BroadcastState::Confirm);
    }

    /// Resolve the confirm step. `y/Y` advances to `Typing(empty)`,
    /// anything else cancels back to `Browse`. Returns true if the
    /// state changed.
    pub fn resolve_broadcast_confirm(&mut self, byte: u8) -> bool {
        if !matches!(self.broadcast, Some(BroadcastState::Confirm)) {
            return false;
        }
        if matches!(byte, b'y' | b'Y') {
            self.broadcast = Some(BroadcastState::Typing(String::new()));
        } else {
            self.broadcast = None;
        }
        true
    }

    /// Append a single byte to the broadcast type-buffer. Returns
    /// `Some(payload)` if the byte committed (Enter — payload includes
    /// the trailing newline), `None` otherwise. Esc cancels and clears
    /// the modal. Other control bytes are ignored; printable ASCII is
    /// appended; backspace pops the last char.
    pub fn broadcast_input_byte(&mut self, byte: u8) -> Option<String> {
        let buffer = match self.broadcast.as_mut() {
            Some(BroadcastState::Typing(buf)) => buf,
            _ => return None,
        };
        match byte {
            b'\r' | b'\n' => {
                let mut committed = std::mem::take(buffer);
                self.broadcast = None;
                committed.push('\n');
                Some(committed)
            }
            0x7f | 0x08 => {
                buffer.pop();
                None
            }
            0x1b => {
                // Esc cancels.
                self.broadcast = None;
                None
            }
            b if (0x20..0x7f).contains(&b) => {
                buffer.push(b as char);
                None
            }
            _ => None,
        }
    }

    /// Cancel any in-progress broadcast modal. Returns true if there
    /// was something to cancel.
    pub fn cancel_broadcast(&mut self) -> bool {
        self.broadcast.take().is_some()
    }

    /// Number of panes that would receive a broadcast right now: the
    /// intersection of the current filter and "Working or Idle".
    /// Surfaces in the confirm prompt — `broadcast to N panes? y/N`.
    pub fn broadcast_recipient_count(&self) -> usize {
        self.visible_rows()
            .iter()
            .filter(|r| r.state == "Working" || r.state == "Idle")
            .count()
    }

    /// Pane ids that are eligible recipients for a broadcast (filter ∩
    /// Working/Idle). Used by the workspace `broadcast` helper to
    /// decide who to fan out to without re-deriving the filter logic
    /// across module boundaries.
    pub fn broadcast_recipient_ids(&self) -> Vec<u32> {
        self.visible_rows()
            .iter()
            .filter(|r| r.state == "Working" || r.state == "Idle")
            .map(|r| r.pane_id)
            .collect()
    }
}

/// Format `age_secs` as a humanised duration (`"5s"`, `"3m"`, `"2h"`,
/// `"1d"`). The supervisor doesn't re-render on its own when only
/// this field would change.
fn humanize_age(secs: u64) -> String {
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// Map an AgentState debug-string (the form produced by
/// `format!("{:?}", state)`) to a single-cell glyph. Anything we don't
/// recognise renders as a space so the layout stays aligned.
fn state_glyph(state: &str) -> char {
    if state == "Working" {
        '\u{25CF}' // ●
    } else if state == "Idle" {
        '\u{25CB}' // ○
    } else if state == "AwaitingInput" {
        '\u{26A0}' // ⚠
    } else if state == "Errored" {
        '\u{2717}' // ✗
    } else if state.starts_with("Exited") {
        '\u{25D0}' // ◐
    } else {
        ' '
    }
}

/// Display string for the pane state column. Uses lowercase so it sits
/// quietly next to the bright glyph and the label column.
fn state_display(state: &str) -> String {
    if state == "AwaitingInput" {
        "awaiting".to_string()
    } else if let Some(rest) = state
        .strip_prefix("Exited(")
        .and_then(|s| s.strip_suffix(")"))
    {
        format!("exit {rest}")
    } else {
        state.to_lowercase()
    }
}

const SUPERVISOR_MIN_WIDTH: u16 = 60;

/// Render the supervisor overlay as plain UTF-8 lines plus an anchor
/// position. The workspace renderer composes these into the styled-cell
/// frame after pane content but before the status bar.
///
/// Width is the smaller of `SUPERVISOR_MIN_WIDTH` and the workspace
/// width minus a small margin; height grows with row count but is
/// capped at the workspace minus 2 rows of breathing room.
pub fn render_supervisor(state: &SupervisorState, cols: u16, rows: u16) -> SupervisorFrame {
    // Pick the smallest comfortable width: the spec calls for >= 60
    // cols, but if the workspace is narrower than that we draw at the
    // workspace width to avoid clipping. We never grow past
    // `SUPERVISOR_MIN_WIDTH` so the box stays compact even on a wide
    // terminal — the spec asks for "at least 60 cols", not "as wide
    // as the workspace".
    let width = cols.clamp(1, SUPERVISOR_MIN_WIDTH);
    let inner_width = width.saturating_sub(2) as usize;

    let title = match state.filter {
        FilterMode::All => " zmux supervisor ".to_string(),
        FilterMode::Working => " zmux supervisor [working] ".to_string(),
        FilterMode::Idle => " zmux supervisor [idle] ".to_string(),
        FilterMode::Awaiting => " zmux supervisor [awaiting] ".to_string(),
        FilterMode::Errored => " zmux supervisor [errored] ".to_string(),
    };
    let total = state.rows.len();
    let visible = state.visible_rows();
    let visible_count = visible.len();
    let count_label = match state.filter {
        FilterMode::All => {
            if total == 1 {
                " 1 pane ".to_string()
            } else {
                format!(" {total} panes ")
            }
        }
        _ => format!(" {visible_count} of {total} panes "),
    };

    // Footer differs by mode so the user always knows what keys do.
    let footer = if state.has_label_input() {
        " label: type · enter commit · esc cancel ".to_string()
    } else if state.has_pending_kill() {
        " kill pane? y confirm · any other key cancel ".to_string()
    } else if matches!(state.broadcast, Some(BroadcastState::Confirm)) {
        " broadcast: y confirm · any other key cancel ".to_string()
    } else if matches!(state.broadcast, Some(BroadcastState::Typing(_))) {
        " broadcast: type · enter send · esc cancel ".to_string()
    } else {
        " j/k nav · enter attach · l label · b broadcast · f filter · K kill · q close ".to_string()
    };

    // Compose lines.
    let mut lines: Vec<String> = Vec::new();
    lines.push(top_border(inner_width, &title, &count_label));

    let mut highlight_line: Option<usize> = None;
    if visible.is_empty() {
        let body = if total == 0 {
            format_body_line(inner_width, " (no panes) ")
        } else {
            format_body_line(inner_width, " (no panes match filter) ")
        };
        lines.push(body);
    } else {
        for (idx, row) in visible.iter().enumerate() {
            let line = format_row(inner_width, row);
            if idx == state.selected {
                highlight_line = Some(lines.len());
            }
            lines.push(line);
        }
    }

    // Optional inline input/confirm bar.
    if let Some(buffer) = &state.label_input {
        lines.push(format_body_line(inner_width, &format!(" label: {buffer}_")));
    } else if let Some(pane_id) = state.pending_kill {
        lines.push(format_body_line(
            inner_width,
            &format!(" kill pane {pane_id}? y/N "),
        ));
    } else if let Some(BroadcastState::Confirm) = &state.broadcast {
        let recipient_count = state.broadcast_recipient_count();
        lines.push(format_body_line(
            inner_width,
            &format!(" broadcast to {recipient_count} panes? y/N "),
        ));
    } else if let Some(BroadcastState::Typing(buf)) = &state.broadcast {
        lines.push(format_body_line(inner_width, &format!(" > {buf}_")));
    }

    lines.push(bottom_border(inner_width, &footer));

    // Centre the box. If the workspace is shorter than what we need,
    // anchor at row 0; the renderer will clip lines that overflow.
    let h = lines.len() as u16;
    let origin_row = if rows > h + 2 { (rows - h) / 2 } else { 0 };
    let origin_col = if cols > width { (cols - width) / 2 } else { 0 };

    SupervisorFrame {
        origin_col,
        origin_row,
        lines,
        highlight_line,
    }
}

fn top_border(inner_width: usize, title: &str, count_label: &str) -> String {
    // ┌─ title …count ─┐. Title sits left, count sits right; dashes
    // fill the middle so the box always reaches inner_width.
    let title_chars = title.chars().count();
    let count_chars = count_label.chars().count();
    // Leading dash + title + middle dashes + count + trailing dash
    // must equal `inner_width`.
    let consumed = 1 + title_chars + count_chars + 1;
    let middle_dashes = inner_width.saturating_sub(consumed);
    let mut s = String::new();
    s.push('\u{250C}');
    s.push('\u{2500}');
    s.push_str(title);
    for _ in 0..middle_dashes {
        s.push('\u{2500}');
    }
    s.push_str(count_label);
    s.push('\u{2500}');
    s.push('\u{2510}');
    s
}

fn bottom_border(inner_width: usize, footer: &str) -> String {
    let mut s = String::new();
    s.push('\u{2514}'); // └
    s.push('\u{2500}');
    let footer_chars = footer.chars().count();
    let dashes = inner_width.saturating_sub(footer_chars + 1);
    for _ in 0..dashes {
        s.push('\u{2500}');
    }
    s.push_str(footer);
    s.push('\u{2518}'); // ┘
    s
}

fn format_body_line(inner_width: usize, content: &str) -> String {
    let mut s = String::new();
    s.push('\u{2502}'); // │
    let mut count = 0;
    for ch in content.chars() {
        if count >= inner_width {
            break;
        }
        s.push(ch);
        count += 1;
    }
    while count < inner_width {
        s.push(' ');
        count += 1;
    }
    s.push('\u{2502}');
    s
}

fn format_row(inner_width: usize, row: &SupervisorRow) -> String {
    let glyph = state_glyph(&row.state);
    let label = row
        .label
        .clone()
        .unwrap_or_else(|| format!("pane #{}", row.pane_id));
    let state = state_display(&row.state);
    let cmd = row.last_command.clone().unwrap_or_else(|| "—".to_string());
    let age = humanize_age(row.age_secs);

    // Build a content row by joining the columns; we trim the command
    // text to fit the remaining space.
    //   " G LABEL(<=15)   STATE(<=10)  COMMAND(...)   AGE(<=4) "
    let label_col = pad_or_trim(&label, 15);
    let state_col = pad_or_trim(&state, 10);
    let age_col = pad_or_trim(&age, 5);
    // Reserved for: leading space, glyph, gap, label_col, gap, state_col,
    // gap, cmd, gap, age_col, trailing space.
    let fixed_overhead = (1 + 1 + 1 + 15 + 2 + 10 + 2) + 2 + 5 + 1;
    let cmd_room = inner_width.saturating_sub(fixed_overhead).max(1);
    let cmd_col = pad_or_trim(&cmd, cmd_room);

    let content = format!(
        " {glyph} {label_col}  {state_col}  {cmd_col}  {age_col} ",
        glyph = glyph,
        label_col = label_col,
        state_col = state_col,
        cmd_col = cmd_col,
        age_col = age_col,
    );
    format_body_line(inner_width, &content)
}

fn pad_or_trim(s: &str, width: usize) -> String {
    let count = s.chars().count();
    if count == width {
        s.to_string()
    } else if count > width {
        // Trim, keeping leading chars.
        s.chars().take(width).collect()
    } else {
        let mut out = s.to_string();
        for _ in count..width {
            out.push(' ');
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_then_state_change_updates_row() {
        let mut s = SupervisorState::new();
        s.apply_event(&Event::PaneSpawned {
            pane_id: 3,
            label: Some("claude #1".into()),
        });
        s.apply_event(&Event::PaneStateChanged {
            pane_id: 3,
            from: "Idle".into(),
            to: "Working".into(),
        });
        assert_eq!(s.rows[0].state, "Working");
    }

    #[test]
    fn close_removes_row_and_clamps_selection() {
        let mut s = SupervisorState::new();
        s.apply_event(&Event::PaneSpawned {
            pane_id: 1,
            label: None,
        });
        s.apply_event(&Event::PaneSpawned {
            pane_id: 2,
            label: None,
        });
        s.selected = 1;
        s.apply_event(&Event::PaneClosed { pane_id: 2 });
        assert_eq!(s.rows.len(), 1);
        assert_eq!(s.selected, 0);
    }

    #[test]
    fn renders_centered_box_with_rows_and_glyphs() {
        let mut state = SupervisorState::new();
        state.apply_event(&Event::PaneSpawned {
            pane_id: 1,
            label: Some("claude #1".into()),
        });
        state.apply_event(&Event::PaneStateChanged {
            pane_id: 1,
            from: "Idle".into(),
            to: "Working".into(),
        });
        let frame = render_supervisor(&state, 80, 24);
        let text = frame.lines();
        assert!(text.iter().any(|l| l.contains("zmux supervisor")));
        assert!(text.iter().any(|l| l.contains("1 pane")));
        assert!(
            text.iter()
                .any(|l| l.contains("claude #1") && l.contains("\u{25CF}"))
        );
        // Footer with the keybinding hints.
        assert!(
            text.iter()
                .any(|l| l.contains("attach") && l.contains("kill"))
        );
        // Centred horizontally — we asked for 80 cols, default min width 60,
        // so origin_col should be > 0.
        assert!(frame.origin_col > 0);
    }

    #[test]
    fn renders_multiple_rows_with_distinct_glyphs() {
        let mut state = SupervisorState::new();
        state.apply_event(&Event::PaneSpawned {
            pane_id: 1,
            label: Some("a".into()),
        });
        state.apply_event(&Event::PaneSpawned {
            pane_id: 2,
            label: Some("b".into()),
        });
        state.apply_event(&Event::PaneSpawned {
            pane_id: 3,
            label: Some("c".into()),
        });
        state.apply_event(&Event::PaneStateChanged {
            pane_id: 2,
            from: "Idle".into(),
            to: "AwaitingInput".into(),
        });
        state.apply_event(&Event::PaneStateChanged {
            pane_id: 3,
            from: "Idle".into(),
            to: "Errored".into(),
        });
        let frame = render_supervisor(&state, 80, 24);
        let body: String = frame.lines().join("\n");
        // Three glyphs: ○ (Idle), ⚠ (Awaiting), ✗ (Errored).
        assert!(body.contains("\u{25CB}"));
        assert!(body.contains("\u{26A0}"));
        assert!(body.contains("\u{2717}"));
        assert!(body.contains("3 panes"));
    }

    #[test]
    fn label_input_collects_bytes_and_commits_on_enter() {
        let mut state = SupervisorState::new();
        state.apply_event(&Event::PaneSpawned {
            pane_id: 1,
            label: None,
        });
        state.begin_label_input();
        assert!(state.has_label_input());
        assert!(state.label_input_byte(b'h').is_none());
        assert!(state.label_input_byte(b'i').is_none());
        // Backspace pops a char.
        assert!(state.label_input_byte(0x7f).is_none());
        assert_eq!(state.label_input.as_deref(), Some("h"));
        let committed = state.label_input_byte(b'\r');
        assert_eq!(committed.as_deref(), Some("h"));
        assert!(!state.has_label_input());
    }

    #[test]
    fn kill_confirm_returns_id_only_on_y() {
        let mut state = SupervisorState::new();
        state.apply_event(&Event::PaneSpawned {
            pane_id: 7,
            label: None,
        });
        state.begin_kill_confirm();
        assert!(state.has_pending_kill());
        // 'n' cancels.
        assert_eq!(state.resolve_kill_confirm(b'n'), None);
        assert!(!state.has_pending_kill());

        state.begin_kill_confirm();
        assert_eq!(state.resolve_kill_confirm(b'y'), Some(7));
    }

    #[test]
    fn cycle_filter_advances_through_modes() {
        let mut s = SupervisorState::new();
        assert_eq!(s.filter, FilterMode::All);
        s.cycle_filter();
        assert_eq!(s.filter, FilterMode::Working);
        for _ in 0..4 {
            s.cycle_filter();
        }
        assert_eq!(s.filter, FilterMode::All);
    }

    #[test]
    fn visible_rows_filters_by_state() {
        let mut s = SupervisorState::new();
        s.apply_event(&Event::PaneSpawned {
            pane_id: 1,
            label: None,
        });
        s.apply_event(&Event::PaneSpawned {
            pane_id: 2,
            label: None,
        });
        s.apply_event(&Event::PaneStateChanged {
            pane_id: 1,
            from: "Idle".into(),
            to: "Working".into(),
        });
        s.cycle_filter(); // → Working
        let visible: Vec<u32> = s.visible_rows().iter().map(|r| r.pane_id).collect();
        assert_eq!(visible, vec![1]);
    }

    #[test]
    fn renderer_shows_filter_in_title_with_visible_count() {
        // 3 panes total: pane 1 Working, panes 2 & 3 Idle. Filter
        // Working should produce a title like "[working]" and a count
        // like "1 of 3 panes".
        let mut state = SupervisorState::new();
        for id in 1..=3 {
            state.apply_event(&Event::PaneSpawned {
                pane_id: id,
                label: None,
            });
        }
        state.apply_event(&Event::PaneStateChanged {
            pane_id: 1,
            from: "Idle".into(),
            to: "Working".into(),
        });
        state.cycle_filter(); // → Working
        let frame = render_supervisor(&state, 80, 24);
        let text = frame.lines().join("\n");
        assert!(
            text.contains("[working]"),
            "expected '[working]' in title, got:\n{text}",
        );
        assert!(
            text.contains("1 of 3 panes"),
            "expected '1 of 3 panes' in title, got:\n{text}",
        );
        // Only the working pane should appear in body rows — pane 2/3
        // labels (auto-generated) shouldn't show up.
        // We verify by glyph count: only one ● should appear under the
        // [working] filter.
        let working_glyph_count = text.matches('\u{25CF}').count();
        assert_eq!(
            working_glyph_count, 1,
            "expected exactly one Working-glyph row in [working] filter",
        );
    }

    #[test]
    fn cycle_filter_resets_selection() {
        let mut s = SupervisorState::new();
        for id in 1..=3 {
            s.apply_event(&Event::PaneSpawned {
                pane_id: id,
                label: None,
            });
        }
        s.selected = 2;
        s.cycle_filter();
        assert_eq!(s.selected, 0);
    }

    #[test]
    fn broadcast_state_machine_transitions_through_confirm_and_typing() {
        let mut s = SupervisorState::new();
        s.apply_event(&Event::PaneSpawned {
            pane_id: 1,
            label: None,
        });
        s.apply_event(&Event::PaneStateChanged {
            pane_id: 1,
            from: "Idle".into(),
            to: "Working".into(),
        });
        // Browse → Confirm.
        assert!(s.broadcast.is_none());
        s.begin_broadcast_confirm();
        assert_eq!(s.broadcast, Some(BroadcastState::Confirm));
        // Confirm → Typing on 'y'.
        let changed = s.resolve_broadcast_confirm(b'y');
        assert!(changed);
        assert_eq!(s.broadcast, Some(BroadcastState::Typing(String::new())));
        // Typing collects bytes.
        assert!(s.broadcast_input_byte(b'h').is_none());
        assert!(s.broadcast_input_byte(b'i').is_none());
        if let Some(BroadcastState::Typing(buf)) = &s.broadcast {
            assert_eq!(buf, "hi");
        } else {
            panic!("expected Typing state");
        }
        // Backspace pops.
        assert!(s.broadcast_input_byte(0x7f).is_none());
        if let Some(BroadcastState::Typing(buf)) = &s.broadcast {
            assert_eq!(buf, "h");
        } else {
            panic!("expected Typing state after backspace");
        }
        // Enter commits — payload includes trailing newline.
        let committed = s.broadcast_input_byte(b'\r');
        assert_eq!(committed.as_deref(), Some("h\n"));
        assert!(s.broadcast.is_none(), "Enter must close the modal");
    }

    #[test]
    fn broadcast_confirm_cancels_on_non_y() {
        let mut s = SupervisorState::new();
        s.begin_broadcast_confirm();
        s.resolve_broadcast_confirm(b'n');
        assert!(s.broadcast.is_none());
    }

    #[test]
    fn broadcast_typing_esc_cancels() {
        let mut s = SupervisorState::new();
        s.begin_broadcast_confirm();
        s.resolve_broadcast_confirm(b'y');
        s.broadcast_input_byte(b'a');
        s.broadcast_input_byte(0x1b);
        assert!(s.broadcast.is_none());
    }

    #[test]
    fn broadcast_recipient_count_filters_to_working_or_idle() {
        let mut s = SupervisorState::new();
        for id in 1..=4 {
            s.apply_event(&Event::PaneSpawned {
                pane_id: id,
                label: None,
            });
        }
        // Pane 1: Working, pane 2: Idle (default), pane 3: AwaitingInput, pane 4: Errored.
        s.apply_event(&Event::PaneStateChanged {
            pane_id: 1,
            from: "Idle".into(),
            to: "Working".into(),
        });
        s.apply_event(&Event::PaneStateChanged {
            pane_id: 3,
            from: "Idle".into(),
            to: "AwaitingInput".into(),
        });
        s.apply_event(&Event::PaneStateChanged {
            pane_id: 4,
            from: "Idle".into(),
            to: "Errored".into(),
        });
        // Filter All → Working+Idle = panes 1 & 2.
        assert_eq!(s.broadcast_recipient_count(), 2);
        let ids = s.broadcast_recipient_ids();
        assert_eq!(ids, vec![1, 2]);

        // Filter Working → only pane 1.
        s.cycle_filter();
        assert_eq!(s.broadcast_recipient_count(), 1);
        assert_eq!(s.broadcast_recipient_ids(), vec![1]);
    }

    #[test]
    fn broadcast_confirm_renders_recipient_count() {
        let mut s = SupervisorState::new();
        for id in 1..=3 {
            s.apply_event(&Event::PaneSpawned {
                pane_id: id,
                label: None,
            });
        }
        s.apply_event(&Event::PaneStateChanged {
            pane_id: 1,
            from: "Idle".into(),
            to: "Working".into(),
        });
        s.begin_broadcast_confirm();
        let frame = render_supervisor(&s, 80, 24);
        let text = frame.lines().join("\n");
        assert!(
            text.contains("broadcast to 3 panes?"),
            "expected confirm bar showing recipient count, got:\n{text}",
        );
    }

    #[test]
    fn broadcast_typing_shows_input_buffer_in_render() {
        let mut s = SupervisorState::new();
        s.apply_event(&Event::PaneSpawned {
            pane_id: 1,
            label: None,
        });
        s.begin_broadcast_confirm();
        s.resolve_broadcast_confirm(b'y');
        s.broadcast_input_byte(b'h');
        s.broadcast_input_byte(b'i');
        let frame = render_supervisor(&s, 80, 24);
        let text = frame.lines().join("\n");
        assert!(
            text.contains("> hi_"),
            "expected input echo '> hi_' in render, got:\n{text}",
        );
    }
}
