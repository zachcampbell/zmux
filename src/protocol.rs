// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::io;

use crate::mouse::MouseTrackingMode;
use crate::pty::PtySize;

const CLIENT_ATTACH: u8 = 1;
const CLIENT_RESIZE: u8 = 2;
const CLIENT_INPUT: u8 = 3;
const CLIENT_DETACH: u8 = 4;
const CLIENT_SHUTDOWN: u8 = 5;
const CLIENT_SPLIT_PANE_COLUMNS: u8 = 6;
const CLIENT_CLOSE_PANE: u8 = 7;
const CLIENT_CYCLE_PANE: u8 = 8;
const CLIENT_CYCLE_PANE_BACKWARD: u8 = 9;
const CLIENT_SPLIT_PANE_ROWS: u8 = 10;
const CLIENT_SHOW_PANE_NUMBERS: u8 = 11;
const CLIENT_RESIZE_PANE_LEFT: u8 = 12;
const CLIENT_RESIZE_PANE_RIGHT: u8 = 13;
const CLIENT_RESIZE_PANE_UP: u8 = 14;
const CLIENT_RESIZE_PANE_DOWN: u8 = 15;
const CLIENT_PRESET_TWO_COLUMNS: u8 = 16;
const CLIENT_PRESET_THREE_COLUMNS: u8 = 17;
const CLIENT_PRESET_QUADRANTS: u8 = 18;
const CLIENT_SCROLL_UP: u8 = 19;
const CLIENT_SCROLL_DOWN: u8 = 20;
const CLIENT_SCROLL_TO_BOTTOM: u8 = 21;
const CLIENT_YANK_VIEWPORT: u8 = 22;
const CLIENT_TOGGLE_ZOOM: u8 = 23;
const CLIENT_SEARCH_BEGIN: u8 = 24;
const CLIENT_SEARCH_INPUT: u8 = 25;
const CLIENT_SEARCH_COMMIT: u8 = 26;
const CLIENT_SEARCH_CANCEL: u8 = 27;
const CLIENT_SEARCH_CLEAR: u8 = 28;
const CLIENT_SEARCH_NEXT: u8 = 29;
const CLIENT_SEARCH_PREV: u8 = 30;
const CLIENT_BEGIN_SELECTION: u8 = 31;
const CLIENT_EXTEND_SELECTION: u8 = 32;
const CLIENT_YANK_SELECTION: u8 = 33;
const CLIENT_CLEAR_SELECTION: u8 = 34;
const CLIENT_SWAP_PANE_NEXT: u8 = 35;
const CLIENT_SWAP_PANE_PREVIOUS: u8 = 36;
const CLIENT_RENAME_BEGIN: u8 = 37;
const CLIENT_RENAME_INPUT: u8 = 38;
const CLIENT_RENAME_COMMIT: u8 = 39;
const CLIENT_RENAME_CANCEL: u8 = 40;
const CLIENT_COMMAND_PROMPT_BEGIN: u8 = 41;
const CLIENT_COMMAND_PROMPT_INPUT: u8 = 42;
const CLIENT_COMMAND_PROMPT_COMMIT: u8 = 43;
const CLIENT_COMMAND_PROMPT_CANCEL: u8 = 44;
const CLIENT_NEW_WINDOW: u8 = 45;
const CLIENT_CLOSE_WINDOW: u8 = 46;
const CLIENT_NEXT_WINDOW: u8 = 47;
const CLIENT_PREVIOUS_WINDOW: u8 = 48;
const CLIENT_TOGGLE_SYNC_PANES: u8 = 49;
const CLIENT_CYCLE_PRESET: u8 = 50;
const CLIENT_SELECT_WINDOW: u8 = 51;
const CLIENT_PASTE_BUFFER: u8 = 52;
const CLIENT_CAPTURE: u8 = 53;
const CLIENT_LIST_PANES: u8 = 54;
const CLIENT_OPEN_SUPERVISOR: u8 = 55;
const CLIENT_SET_LABEL: u8 = 56;
const CLIENT_LAST_WINDOW: u8 = 57;

const SERVER_FRAME: u8 = 1;
const SERVER_EXITED: u8 = 2;
const SERVER_ERROR: u8 = 3;
const SERVER_BUSY: u8 = 4;
const SERVER_CLIPBOARD: u8 = 5;
const SERVER_PANE_LIST: u8 = 6;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientMessage {
    Attach {
        size: PtySize,
    },
    Resize {
        size: PtySize,
    },
    Input(Vec<u8>),
    Detach,
    Shutdown,
    SplitPaneColumns,
    SplitPaneRows,
    ClosePane,
    CyclePane,
    CyclePaneBackward,
    ShowPaneNumbers,
    ResizePaneLeft,
    ResizePaneRight,
    ResizePaneUp,
    ResizePaneDown,
    PresetTwoColumns,
    PresetThreeColumns,
    PresetQuadrants,
    ScrollUp(u16),
    ScrollDown(u16),
    ScrollToBottom,
    YankViewport,
    ToggleZoom,
    SearchBegin,
    SearchInput(Vec<u8>),
    SearchCommit,
    SearchCancel,
    SearchClear,
    SearchNext,
    SearchPrev,
    BeginSelection(SelectionKind),
    ExtendSelection(SelectionMove),
    YankSelection,
    ClearSelection,
    SwapPaneNext,
    SwapPanePrevious,
    RenameBegin,
    RenameInput(Vec<u8>),
    RenameCommit,
    RenameCancel,
    CommandPromptBegin(CommandPromptKind),
    CommandPromptInput(Vec<u8>),
    CommandPromptCommit,
    CommandPromptCancel,
    NewWindow,
    CloseWindow,
    NextWindow,
    PreviousWindow,
    LastWindow,
    ToggleSyncPanes,
    CyclePreset,
    SelectWindow(u8),
    PasteBuffer,
    /// Admin: dump raw PTY bytes from a pane to the given file path
    /// (server-local). Used by `zmux capture` for VT bisection tooling.
    Capture {
        pane_id: u32,
        path: String,
    },
    /// Cross-window per-pane snapshot replied with
    /// `ServerMessage::PaneList`. Shared by `zmux ls --verbose`, the
    /// supervisor overlay, and the MCP server.
    ListPanes,
    /// Open the Ctrl-a A supervisor overlay on the workspace's
    /// current window. The server toggles `Workspace::supervisor_open`
    /// and routes subsequent `Input` bytes through
    /// `Workspace::supervisor_handle_key` instead of the focused
    /// pane's PTY.
    OpenSupervisor,
    /// Admin: rename a pane's label. Pane id is workspace-scoped; the
    /// daemon resolves it across all windows via the cross-window
    /// `find_pane_mut` helper. `None` clears the label so the
    /// renderer falls back to the synthesized "pane #N" string.
    SetLabel {
        pane_id: u32,
        label: Option<String>,
    },
}

/// Per-pane row in `ServerMessage::PaneList`. Field order is stable
/// (drives both the wire encode and the human-readable print column
/// order in `print_session_list_verbose`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneSummary {
    pub pane_id: u32,
    pub label: Option<String>,
    pub state: String,
    pub last_command: Option<String>,
    pub last_exit: Option<i32>,
    pub size_cols: u16,
    pub size_rows: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandPromptKind {
    SplitColumns,
    SplitRows,
    General,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMove {
    LineUp,
    LineDown,
    HalfPageUp,
    HalfPageDown,
    FullPageUp,
    FullPageDown,
    BufferTop,
    BufferBottom,
    CharLeft,
    CharRight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionKind {
    Line,
    Char,
    Rect,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerMessage {
    Frame {
        size: PtySize,
        mouse_tracking_mode: MouseTrackingMode,
        lines: Vec<String>,
        /// Absolute 1-based (row, col) where the client should show the
        /// host cursor; None keeps it hidden. Encoded as an optional
        /// trailing section so frames from older daemons (which end at
        /// the last line) still decode — as None.
        cursor: Option<(u16, u16)>,
    },
    Exited {
        code: i32,
    },
    Error(String),
    Busy,
    Clipboard(String),
    /// Reply to `ClientMessage::ListPanes`. One row per pane across all
    /// windows in the receiving session.
    PaneList(Vec<PaneSummary>),
}

#[derive(Debug, Default)]
pub struct ClientDecoder {
    pending: Vec<u8>,
}

#[derive(Debug, Default)]
pub struct ServerDecoder {
    pending: Vec<u8>,
}

impl ClientDecoder {
    pub fn push_bytes(&mut self, bytes: &[u8]) -> io::Result<Vec<ClientMessage>> {
        self.pending.extend_from_slice(bytes);
        let mut messages = Vec::new();
        while let Some(frame) = take_frame(&mut self.pending)? {
            messages.push(parse_client_message(&frame)?);
        }
        Ok(messages)
    }
}

impl ServerDecoder {
    pub fn push_bytes(&mut self, bytes: &[u8]) -> io::Result<Vec<ServerMessage>> {
        self.pending.extend_from_slice(bytes);
        let mut messages = Vec::new();
        while let Some(frame) = take_frame(&mut self.pending)? {
            messages.push(parse_server_message(&frame)?);
        }
        Ok(messages)
    }
}

pub fn encode_client_message(message: &ClientMessage) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    match message {
        ClientMessage::Attach { size } => {
            body.push(CLIENT_ATTACH);
            write_size(&mut body, *size);
        }
        ClientMessage::Resize { size } => {
            body.push(CLIENT_RESIZE);
            write_size(&mut body, *size);
        }
        ClientMessage::Input(bytes) => {
            body.push(CLIENT_INPUT);
            write_u32(&mut body, bytes.len() as u32);
            body.extend_from_slice(bytes);
        }
        ClientMessage::Detach => body.push(CLIENT_DETACH),
        ClientMessage::Shutdown => body.push(CLIENT_SHUTDOWN),
        ClientMessage::SplitPaneColumns => body.push(CLIENT_SPLIT_PANE_COLUMNS),
        ClientMessage::SplitPaneRows => body.push(CLIENT_SPLIT_PANE_ROWS),
        ClientMessage::ClosePane => body.push(CLIENT_CLOSE_PANE),
        ClientMessage::CyclePane => body.push(CLIENT_CYCLE_PANE),
        ClientMessage::CyclePaneBackward => body.push(CLIENT_CYCLE_PANE_BACKWARD),
        ClientMessage::ShowPaneNumbers => body.push(CLIENT_SHOW_PANE_NUMBERS),
        ClientMessage::ResizePaneLeft => body.push(CLIENT_RESIZE_PANE_LEFT),
        ClientMessage::ResizePaneRight => body.push(CLIENT_RESIZE_PANE_RIGHT),
        ClientMessage::ResizePaneUp => body.push(CLIENT_RESIZE_PANE_UP),
        ClientMessage::ResizePaneDown => body.push(CLIENT_RESIZE_PANE_DOWN),
        ClientMessage::PresetTwoColumns => body.push(CLIENT_PRESET_TWO_COLUMNS),
        ClientMessage::PresetThreeColumns => body.push(CLIENT_PRESET_THREE_COLUMNS),
        ClientMessage::PresetQuadrants => body.push(CLIENT_PRESET_QUADRANTS),
        ClientMessage::ScrollUp(amount) => {
            body.push(CLIENT_SCROLL_UP);
            write_u16(&mut body, *amount);
        }
        ClientMessage::ScrollDown(amount) => {
            body.push(CLIENT_SCROLL_DOWN);
            write_u16(&mut body, *amount);
        }
        ClientMessage::ScrollToBottom => body.push(CLIENT_SCROLL_TO_BOTTOM),
        ClientMessage::YankViewport => body.push(CLIENT_YANK_VIEWPORT),
        ClientMessage::ToggleZoom => body.push(CLIENT_TOGGLE_ZOOM),
        ClientMessage::SearchBegin => body.push(CLIENT_SEARCH_BEGIN),
        ClientMessage::SearchInput(bytes) => {
            body.push(CLIENT_SEARCH_INPUT);
            write_u32(&mut body, bytes.len() as u32);
            body.extend_from_slice(bytes);
        }
        ClientMessage::SearchCommit => body.push(CLIENT_SEARCH_COMMIT),
        ClientMessage::SearchCancel => body.push(CLIENT_SEARCH_CANCEL),
        ClientMessage::SearchClear => body.push(CLIENT_SEARCH_CLEAR),
        ClientMessage::SearchNext => body.push(CLIENT_SEARCH_NEXT),
        ClientMessage::SearchPrev => body.push(CLIENT_SEARCH_PREV),
        ClientMessage::BeginSelection(kind) => {
            body.push(CLIENT_BEGIN_SELECTION);
            body.push(match kind {
                SelectionKind::Line => 0,
                SelectionKind::Char => 1,
                SelectionKind::Rect => 2,
            });
        }
        ClientMessage::ExtendSelection(direction) => {
            body.push(CLIENT_EXTEND_SELECTION);
            body.push(encode_selection_move(*direction));
        }
        ClientMessage::YankSelection => body.push(CLIENT_YANK_SELECTION),
        ClientMessage::ClearSelection => body.push(CLIENT_CLEAR_SELECTION),
        ClientMessage::SwapPaneNext => body.push(CLIENT_SWAP_PANE_NEXT),
        ClientMessage::SwapPanePrevious => body.push(CLIENT_SWAP_PANE_PREVIOUS),
        ClientMessage::RenameBegin => body.push(CLIENT_RENAME_BEGIN),
        ClientMessage::RenameInput(bytes) => {
            body.push(CLIENT_RENAME_INPUT);
            write_u32(&mut body, bytes.len() as u32);
            body.extend_from_slice(bytes);
        }
        ClientMessage::RenameCommit => body.push(CLIENT_RENAME_COMMIT),
        ClientMessage::RenameCancel => body.push(CLIENT_RENAME_CANCEL),
        ClientMessage::CommandPromptBegin(kind) => {
            body.push(CLIENT_COMMAND_PROMPT_BEGIN);
            body.push(match kind {
                CommandPromptKind::SplitColumns => 0,
                CommandPromptKind::SplitRows => 1,
                CommandPromptKind::General => 2,
            });
        }
        ClientMessage::CommandPromptInput(bytes) => {
            body.push(CLIENT_COMMAND_PROMPT_INPUT);
            write_u32(&mut body, bytes.len() as u32);
            body.extend_from_slice(bytes);
        }
        ClientMessage::CommandPromptCommit => body.push(CLIENT_COMMAND_PROMPT_COMMIT),
        ClientMessage::CommandPromptCancel => body.push(CLIENT_COMMAND_PROMPT_CANCEL),
        ClientMessage::NewWindow => body.push(CLIENT_NEW_WINDOW),
        ClientMessage::CloseWindow => body.push(CLIENT_CLOSE_WINDOW),
        ClientMessage::NextWindow => body.push(CLIENT_NEXT_WINDOW),
        ClientMessage::PreviousWindow => body.push(CLIENT_PREVIOUS_WINDOW),
        ClientMessage::LastWindow => body.push(CLIENT_LAST_WINDOW),
        ClientMessage::ToggleSyncPanes => body.push(CLIENT_TOGGLE_SYNC_PANES),
        ClientMessage::CyclePreset => body.push(CLIENT_CYCLE_PRESET),
        ClientMessage::SelectWindow(index) => {
            body.push(CLIENT_SELECT_WINDOW);
            body.push(*index);
        }
        ClientMessage::PasteBuffer => body.push(CLIENT_PASTE_BUFFER),
        ClientMessage::Capture { pane_id, path } => {
            body.push(CLIENT_CAPTURE);
            write_u32(&mut body, *pane_id);
            let path_bytes = path.as_bytes();
            write_u32(&mut body, path_bytes.len() as u32);
            body.extend_from_slice(path_bytes);
        }
        ClientMessage::ListPanes => body.push(CLIENT_LIST_PANES),
        ClientMessage::OpenSupervisor => body.push(CLIENT_OPEN_SUPERVISOR),
        ClientMessage::SetLabel { pane_id, label } => {
            body.push(CLIENT_SET_LABEL);
            write_u32(&mut body, *pane_id);
            write_optional_string(&mut body, label.as_deref());
        }
    }

    wrap_frame(body)
}

pub fn encode_server_message(message: &ServerMessage) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    match message {
        ServerMessage::Frame {
            size,
            mouse_tracking_mode,
            lines,
            cursor,
        } => {
            body.push(SERVER_FRAME);
            write_size(&mut body, *size);
            body.push(mouse_tracking_mode.as_u8());
            write_u16(&mut body, lines.len() as u16);
            for line in lines {
                let bytes = line.as_bytes();
                write_u32(&mut body, bytes.len() as u32);
                body.extend_from_slice(bytes);
            }
            match cursor {
                None => body.push(0),
                Some((row, col)) => {
                    body.push(1);
                    write_u16(&mut body, *row);
                    write_u16(&mut body, *col);
                }
            }
        }
        ServerMessage::Exited { code } => {
            body.push(SERVER_EXITED);
            body.extend_from_slice(&code.to_le_bytes());
        }
        ServerMessage::Error(message) => {
            body.push(SERVER_ERROR);
            let bytes = message.as_bytes();
            write_u32(&mut body, bytes.len() as u32);
            body.extend_from_slice(bytes);
        }
        ServerMessage::Busy => body.push(SERVER_BUSY),
        ServerMessage::Clipboard(text) => {
            body.push(SERVER_CLIPBOARD);
            let bytes = text.as_bytes();
            write_u32(&mut body, bytes.len() as u32);
            body.extend_from_slice(bytes);
        }
        ServerMessage::PaneList(rows) => {
            body.push(SERVER_PANE_LIST);
            write_u32(&mut body, rows.len() as u32);
            for row in rows {
                write_u32(&mut body, row.pane_id);
                write_optional_string(&mut body, row.label.as_deref());
                let state = row.state.as_bytes();
                write_u32(&mut body, state.len() as u32);
                body.extend_from_slice(state);
                write_optional_string(&mut body, row.last_command.as_deref());
                // Exit code: 0 = absent, 1 = present + i32 LE.
                match row.last_exit {
                    Some(code) => {
                        body.push(1);
                        body.extend_from_slice(&code.to_le_bytes());
                    }
                    None => body.push(0),
                }
                write_u16(&mut body, row.size_cols);
                write_u16(&mut body, row.size_rows);
            }
        }
    }

    wrap_frame(body)
}

fn write_optional_string(buffer: &mut Vec<u8>, value: Option<&str>) {
    match value {
        Some(s) => {
            buffer.push(1);
            let bytes = s.as_bytes();
            write_u32(buffer, bytes.len() as u32);
            buffer.extend_from_slice(bytes);
        }
        None => buffer.push(0),
    }
}

fn wrap_frame(body: Vec<u8>) -> io::Result<Vec<u8>> {
    if body.len() > u32::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "protocol frame too large",
        ));
    }

    let mut framed = Vec::with_capacity(body.len() + 4);
    write_u32(&mut framed, body.len() as u32);
    framed.extend_from_slice(&body);
    Ok(framed)
}

fn take_frame(pending: &mut Vec<u8>) -> io::Result<Option<Vec<u8>>> {
    if pending.len() < 4 {
        return Ok(None);
    }

    let length = u32::from_le_bytes([pending[0], pending[1], pending[2], pending[3]]) as usize;
    if pending.len() < 4 + length {
        return Ok(None);
    }

    let frame = pending[4..4 + length].to_vec();
    pending.drain(..4 + length);
    Ok(Some(frame))
}

fn parse_client_message(frame: &[u8]) -> io::Result<ClientMessage> {
    let Some((&kind, rest)) = frame.split_first() else {
        return Err(invalid_data("empty client frame"));
    };

    match kind {
        CLIENT_ATTACH => Ok(ClientMessage::Attach {
            size: parse_size(rest)?,
        }),
        CLIENT_RESIZE => Ok(ClientMessage::Resize {
            size: parse_size(rest)?,
        }),
        CLIENT_INPUT => {
            let (length, rest) = take_u32(rest)?;
            if rest.len() != length as usize {
                return Err(invalid_data("invalid client input payload"));
            }
            Ok(ClientMessage::Input(rest.to_vec()))
        }
        CLIENT_DETACH if rest.is_empty() => Ok(ClientMessage::Detach),
        CLIENT_SHUTDOWN if rest.is_empty() => Ok(ClientMessage::Shutdown),
        CLIENT_SPLIT_PANE_COLUMNS if rest.is_empty() => Ok(ClientMessage::SplitPaneColumns),
        CLIENT_SPLIT_PANE_ROWS if rest.is_empty() => Ok(ClientMessage::SplitPaneRows),
        CLIENT_CLOSE_PANE if rest.is_empty() => Ok(ClientMessage::ClosePane),
        CLIENT_CYCLE_PANE if rest.is_empty() => Ok(ClientMessage::CyclePane),
        CLIENT_CYCLE_PANE_BACKWARD if rest.is_empty() => Ok(ClientMessage::CyclePaneBackward),
        CLIENT_SHOW_PANE_NUMBERS if rest.is_empty() => Ok(ClientMessage::ShowPaneNumbers),
        CLIENT_RESIZE_PANE_LEFT if rest.is_empty() => Ok(ClientMessage::ResizePaneLeft),
        CLIENT_RESIZE_PANE_RIGHT if rest.is_empty() => Ok(ClientMessage::ResizePaneRight),
        CLIENT_RESIZE_PANE_UP if rest.is_empty() => Ok(ClientMessage::ResizePaneUp),
        CLIENT_RESIZE_PANE_DOWN if rest.is_empty() => Ok(ClientMessage::ResizePaneDown),
        CLIENT_PRESET_TWO_COLUMNS if rest.is_empty() => Ok(ClientMessage::PresetTwoColumns),
        CLIENT_PRESET_THREE_COLUMNS if rest.is_empty() => Ok(ClientMessage::PresetThreeColumns),
        CLIENT_PRESET_QUADRANTS if rest.is_empty() => Ok(ClientMessage::PresetQuadrants),
        CLIENT_SCROLL_UP => {
            let (amount, rest) = take_u16(rest)?;
            if !rest.is_empty() {
                return Err(invalid_data("trailing bytes after scroll-up"));
            }
            Ok(ClientMessage::ScrollUp(amount))
        }
        CLIENT_SCROLL_DOWN => {
            let (amount, rest) = take_u16(rest)?;
            if !rest.is_empty() {
                return Err(invalid_data("trailing bytes after scroll-down"));
            }
            Ok(ClientMessage::ScrollDown(amount))
        }
        CLIENT_SCROLL_TO_BOTTOM if rest.is_empty() => Ok(ClientMessage::ScrollToBottom),
        CLIENT_YANK_VIEWPORT if rest.is_empty() => Ok(ClientMessage::YankViewport),
        CLIENT_TOGGLE_ZOOM if rest.is_empty() => Ok(ClientMessage::ToggleZoom),
        CLIENT_SEARCH_BEGIN if rest.is_empty() => Ok(ClientMessage::SearchBegin),
        CLIENT_SEARCH_INPUT => {
            let (length, rest) = take_u32(rest)?;
            if rest.len() != length as usize {
                return Err(invalid_data("invalid search-input payload"));
            }
            Ok(ClientMessage::SearchInput(rest.to_vec()))
        }
        CLIENT_SEARCH_COMMIT if rest.is_empty() => Ok(ClientMessage::SearchCommit),
        CLIENT_SEARCH_CANCEL if rest.is_empty() => Ok(ClientMessage::SearchCancel),
        CLIENT_SEARCH_CLEAR if rest.is_empty() => Ok(ClientMessage::SearchClear),
        CLIENT_SEARCH_NEXT if rest.is_empty() => Ok(ClientMessage::SearchNext),
        CLIENT_SEARCH_PREV if rest.is_empty() => Ok(ClientMessage::SearchPrev),
        CLIENT_BEGIN_SELECTION => {
            if rest.len() != 1 {
                return Err(invalid_data("begin-selection needs one byte"));
            }
            let kind = match rest[0] {
                0 => SelectionKind::Line,
                1 => SelectionKind::Char,
                2 => SelectionKind::Rect,
                _ => return Err(invalid_data("unknown selection kind")),
            };
            Ok(ClientMessage::BeginSelection(kind))
        }
        CLIENT_EXTEND_SELECTION => {
            if rest.len() != 1 {
                return Err(invalid_data("extend-selection needs one byte"));
            }
            let direction = decode_selection_move(rest[0])
                .ok_or_else(|| invalid_data("unknown selection move code"))?;
            Ok(ClientMessage::ExtendSelection(direction))
        }
        CLIENT_YANK_SELECTION if rest.is_empty() => Ok(ClientMessage::YankSelection),
        CLIENT_CLEAR_SELECTION if rest.is_empty() => Ok(ClientMessage::ClearSelection),
        CLIENT_SWAP_PANE_NEXT if rest.is_empty() => Ok(ClientMessage::SwapPaneNext),
        CLIENT_SWAP_PANE_PREVIOUS if rest.is_empty() => Ok(ClientMessage::SwapPanePrevious),
        CLIENT_RENAME_BEGIN if rest.is_empty() => Ok(ClientMessage::RenameBegin),
        CLIENT_RENAME_INPUT => {
            let (length, rest) = take_u32(rest)?;
            if rest.len() != length as usize {
                return Err(invalid_data("invalid rename-input payload"));
            }
            Ok(ClientMessage::RenameInput(rest.to_vec()))
        }
        CLIENT_RENAME_COMMIT if rest.is_empty() => Ok(ClientMessage::RenameCommit),
        CLIENT_RENAME_CANCEL if rest.is_empty() => Ok(ClientMessage::RenameCancel),
        CLIENT_COMMAND_PROMPT_BEGIN => {
            if rest.len() != 1 {
                return Err(invalid_data("command-prompt-begin needs one byte"));
            }
            let kind = match rest[0] {
                0 => CommandPromptKind::SplitColumns,
                1 => CommandPromptKind::SplitRows,
                2 => CommandPromptKind::General,
                _ => return Err(invalid_data("unknown command-prompt kind")),
            };
            Ok(ClientMessage::CommandPromptBegin(kind))
        }
        CLIENT_COMMAND_PROMPT_INPUT => {
            let (length, rest) = take_u32(rest)?;
            if rest.len() != length as usize {
                return Err(invalid_data("invalid command-prompt-input payload"));
            }
            Ok(ClientMessage::CommandPromptInput(rest.to_vec()))
        }
        CLIENT_COMMAND_PROMPT_COMMIT if rest.is_empty() => Ok(ClientMessage::CommandPromptCommit),
        CLIENT_COMMAND_PROMPT_CANCEL if rest.is_empty() => Ok(ClientMessage::CommandPromptCancel),
        CLIENT_NEW_WINDOW if rest.is_empty() => Ok(ClientMessage::NewWindow),
        CLIENT_CLOSE_WINDOW if rest.is_empty() => Ok(ClientMessage::CloseWindow),
        CLIENT_NEXT_WINDOW if rest.is_empty() => Ok(ClientMessage::NextWindow),
        CLIENT_PREVIOUS_WINDOW if rest.is_empty() => Ok(ClientMessage::PreviousWindow),
        CLIENT_LAST_WINDOW if rest.is_empty() => Ok(ClientMessage::LastWindow),
        CLIENT_TOGGLE_SYNC_PANES if rest.is_empty() => Ok(ClientMessage::ToggleSyncPanes),
        CLIENT_CYCLE_PRESET if rest.is_empty() => Ok(ClientMessage::CyclePreset),
        CLIENT_SELECT_WINDOW => {
            if rest.len() != 1 {
                return Err(invalid_data("select-window needs one byte"));
            }
            Ok(ClientMessage::SelectWindow(rest[0]))
        }
        CLIENT_PASTE_BUFFER if rest.is_empty() => Ok(ClientMessage::PasteBuffer),
        CLIENT_CAPTURE => {
            let (pane_id, rest) = take_u32(rest)?;
            let (length, rest) = take_u32(rest)?;
            if rest.len() != length as usize {
                return Err(invalid_data("invalid capture payload"));
            }
            let path = std::str::from_utf8(rest)
                .map_err(|_| invalid_data("invalid utf-8 capture path"))?
                .to_string();
            Ok(ClientMessage::Capture { pane_id, path })
        }
        CLIENT_LIST_PANES if rest.is_empty() => Ok(ClientMessage::ListPanes),
        CLIENT_OPEN_SUPERVISOR if rest.is_empty() => Ok(ClientMessage::OpenSupervisor),
        CLIENT_SET_LABEL => {
            let (pane_id, rest) = take_u32(rest)?;
            let (label, rest) = take_optional_string(rest)?;
            if !rest.is_empty() {
                return Err(invalid_data("trailing bytes after set-label"));
            }
            Ok(ClientMessage::SetLabel { pane_id, label })
        }
        _ => Err(invalid_data("unknown client message")),
    }
}

// Single-byte wire codes for SelectionMove; kept separate from the u8
// constants above so the numeric space stays obviously distinct from
// ClientMessage discriminants.
fn encode_selection_move(direction: SelectionMove) -> u8 {
    match direction {
        SelectionMove::LineUp => 0,
        SelectionMove::LineDown => 1,
        SelectionMove::HalfPageUp => 2,
        SelectionMove::HalfPageDown => 3,
        SelectionMove::FullPageUp => 4,
        SelectionMove::FullPageDown => 5,
        SelectionMove::BufferTop => 6,
        SelectionMove::BufferBottom => 7,
        SelectionMove::CharLeft => 8,
        SelectionMove::CharRight => 9,
    }
}

fn decode_selection_move(byte: u8) -> Option<SelectionMove> {
    Some(match byte {
        0 => SelectionMove::LineUp,
        1 => SelectionMove::LineDown,
        2 => SelectionMove::HalfPageUp,
        3 => SelectionMove::HalfPageDown,
        4 => SelectionMove::FullPageUp,
        5 => SelectionMove::FullPageDown,
        6 => SelectionMove::BufferTop,
        7 => SelectionMove::BufferBottom,
        8 => SelectionMove::CharLeft,
        9 => SelectionMove::CharRight,
        _ => return None,
    })
}

fn parse_server_message(frame: &[u8]) -> io::Result<ServerMessage> {
    let Some((&kind, rest)) = frame.split_first() else {
        return Err(invalid_data("empty server frame"));
    };

    match kind {
        SERVER_FRAME => {
            let (size, rest) = take_size(rest)?;
            let Some((&mouse_mode, mut rest)) = rest.split_first() else {
                return Err(invalid_data("missing mouse mode"));
            };
            let mouse_tracking_mode = MouseTrackingMode::from_u8(mouse_mode)
                .ok_or_else(|| invalid_data("invalid mouse mode"))?;
            let (line_count, next) = take_u16(rest)?;
            rest = next;
            // Cap the preallocation at the bytes actually remaining:
            // every line carries a 4-byte length prefix, so the real
            // count can't exceed `rest.len()`. Trusting the wire's
            // count verbatim would let a peer request a gigantic
            // allocation (Vec::with_capacity aborts the process on
            // failure — not a catchable panic). The loop is already
            // self-limiting because take_u32 errors when bytes run out.
            let mut lines = Vec::with_capacity((line_count as usize).min(rest.len()));
            for _ in 0..line_count {
                let (length, next) = take_u32(rest)?;
                rest = next;
                if rest.len() < length as usize {
                    return Err(invalid_data("truncated frame line"));
                }
                let line = std::str::from_utf8(&rest[..length as usize])
                    .map_err(|_| invalid_data("invalid utf-8 frame line"))?;
                lines.push(line.to_string());
                rest = &rest[length as usize..];
            }
            // Optional cursor tail. Frames from pre-cursor daemons end
            // right after the last line; treat that as "hidden" so a
            // newer client can attach to an older running daemon.
            let cursor = match rest.split_first() {
                None => None,
                Some((0, tail)) => {
                    rest = tail;
                    None
                }
                Some((1, tail)) => {
                    let (row, next) = take_u16(tail)?;
                    let (col, next) = take_u16(next)?;
                    rest = next;
                    Some((row, col))
                }
                Some(_) => return Err(invalid_data("invalid cursor tail in frame")),
            };
            if !rest.is_empty() {
                return Err(invalid_data("trailing bytes in frame"));
            }
            Ok(ServerMessage::Frame {
                size,
                mouse_tracking_mode,
                lines,
                cursor,
            })
        }
        SERVER_EXITED => {
            if rest.len() != 4 {
                return Err(invalid_data("invalid exit payload"));
            }
            Ok(ServerMessage::Exited {
                code: i32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]),
            })
        }
        SERVER_ERROR => {
            let (length, rest) = take_u32(rest)?;
            if rest.len() != length as usize {
                return Err(invalid_data("invalid error payload"));
            }
            let message = std::str::from_utf8(rest)
                .map_err(|_| invalid_data("invalid utf-8 error"))?
                .to_string();
            Ok(ServerMessage::Error(message))
        }
        SERVER_BUSY if rest.is_empty() => Ok(ServerMessage::Busy),
        SERVER_CLIPBOARD => {
            let (length, rest) = take_u32(rest)?;
            if rest.len() != length as usize {
                return Err(invalid_data("invalid clipboard payload"));
            }
            let text = std::str::from_utf8(rest)
                .map_err(|_| invalid_data("invalid utf-8 clipboard"))?
                .to_string();
            Ok(ServerMessage::Clipboard(text))
        }
        SERVER_PANE_LIST => {
            let (count, mut rest) = take_u32(rest)?;
            // Cap as in SERVER_FRAME above: a pane row is several bytes
            // minimum, so `rest.len()` is a safe upper bound and blocks
            // the `count = u32::MAX` allocation bomb. The loop's
            // take_u32 calls fail-fast when the bytes run out.
            let mut rows = Vec::with_capacity((count as usize).min(rest.len()));
            for _ in 0..count {
                let (pane_id, next) = take_u32(rest)?;
                rest = next;
                let (label, next) = take_optional_string(rest)?;
                rest = next;
                let (state_len, next) = take_u32(rest)?;
                rest = next;
                if rest.len() < state_len as usize {
                    return Err(invalid_data("truncated pane state"));
                }
                let state = std::str::from_utf8(&rest[..state_len as usize])
                    .map_err(|_| invalid_data("invalid utf-8 pane state"))?
                    .to_string();
                rest = &rest[state_len as usize..];
                let (last_command, next) = take_optional_string(rest)?;
                rest = next;
                let Some((&exit_present, after)) = rest.split_first() else {
                    return Err(invalid_data("missing pane exit-presence byte"));
                };
                rest = after;
                let last_exit = match exit_present {
                    0 => None,
                    1 => {
                        if rest.len() < 4 {
                            return Err(invalid_data("truncated pane exit code"));
                        }
                        let code = i32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]);
                        rest = &rest[4..];
                        Some(code)
                    }
                    _ => return Err(invalid_data("invalid pane exit-presence byte")),
                };
                let (size_cols, next) = take_u16(rest)?;
                rest = next;
                let (size_rows, next) = take_u16(rest)?;
                rest = next;
                rows.push(PaneSummary {
                    pane_id,
                    label,
                    state,
                    last_command,
                    last_exit,
                    size_cols,
                    size_rows,
                });
            }
            if !rest.is_empty() {
                return Err(invalid_data("trailing bytes in pane-list"));
            }
            Ok(ServerMessage::PaneList(rows))
        }
        _ => Err(invalid_data("unknown server message")),
    }
}

fn take_optional_string(bytes: &[u8]) -> io::Result<(Option<String>, &[u8])> {
    let Some((&present, rest)) = bytes.split_first() else {
        return Err(invalid_data("missing optional-string presence byte"));
    };
    match present {
        0 => Ok((None, rest)),
        1 => {
            let (length, rest) = take_u32(rest)?;
            if rest.len() < length as usize {
                return Err(invalid_data("truncated optional string"));
            }
            let s = std::str::from_utf8(&rest[..length as usize])
                .map_err(|_| invalid_data("invalid utf-8 optional string"))?
                .to_string();
            Ok((Some(s), &rest[length as usize..]))
        }
        _ => Err(invalid_data("invalid optional-string presence byte")),
    }
}

fn parse_size(bytes: &[u8]) -> io::Result<PtySize> {
    let (size, rest) = take_size(bytes)?;
    if !rest.is_empty() {
        return Err(invalid_data("trailing bytes after size"));
    }
    Ok(size)
}

fn take_size(bytes: &[u8]) -> io::Result<(PtySize, &[u8])> {
    let (rows, bytes) = take_u16(bytes)?;
    let (cols, bytes) = take_u16(bytes)?;
    Ok((PtySize::new(rows, cols), bytes))
}

fn take_u16(bytes: &[u8]) -> io::Result<(u16, &[u8])> {
    if bytes.len() < 2 {
        return Err(invalid_data("truncated u16"));
    }
    Ok((u16::from_le_bytes([bytes[0], bytes[1]]), &bytes[2..]))
}

fn take_u32(bytes: &[u8]) -> io::Result<(u32, &[u8])> {
    if bytes.len() < 4 {
        return Err(invalid_data("truncated u32"));
    }
    Ok((
        u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        &bytes[4..],
    ))
}

fn write_size(buffer: &mut Vec<u8>, size: PtySize) {
    write_u16(buffer, size.rows);
    write_u16(buffer, size.cols);
}

fn write_u16(buffer: &mut Vec<u8>, value: u16) {
    buffer.extend_from_slice(&value.to_le_bytes());
}

fn write_u32(buffer: &mut Vec<u8>, value: u32) {
    buffer.extend_from_slice(&value.to_le_bytes());
}

fn invalid_data(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use crate::mouse::MouseTrackingMode;

    use super::{
        ClientDecoder, ClientMessage, CommandPromptKind, PaneSummary, SelectionKind, SelectionMove,
        ServerDecoder, ServerMessage, encode_client_message, encode_server_message,
    };
    use crate::pty::PtySize;

    #[test]
    fn pane_list_with_huge_count_errors_instead_of_allocating() {
        // Body: SERVER_PANE_LIST tag + count=u32::MAX, then NO row
        // bytes. Pre-fix this requested a ~u32::MAX-element Vec (abort
        // on alloc failure); now the capacity is clamped to the
        // remaining byte count and the row loop errors cleanly when it
        // runs out of bytes.
        let mut body = vec![6u8]; // SERVER_PANE_LIST
        body.extend_from_slice(&u32::MAX.to_le_bytes());
        let mut frame = (body.len() as u32).to_le_bytes().to_vec();
        frame.extend_from_slice(&body);

        let mut decoder = ServerDecoder::default();
        let result = decoder.push_bytes(&frame);
        assert!(
            result.is_err(),
            "huge pane-list count must be rejected, not allocated"
        );
    }

    #[test]
    fn frame_with_huge_line_count_errors_instead_of_allocating() {
        // SERVER_FRAME tag + size(4) + mouse_mode(1) + line_count=u16::MAX,
        // then no line data.
        let mut body = vec![1u8]; // SERVER_FRAME
        body.extend_from_slice(&24u16.to_le_bytes()); // rows
        body.extend_from_slice(&80u16.to_le_bytes()); // cols
        body.push(0u8); // mouse mode (Off)
        body.extend_from_slice(&u16::MAX.to_le_bytes()); // line_count
        let mut frame = (body.len() as u32).to_le_bytes().to_vec();
        frame.extend_from_slice(&body);

        let mut decoder = ServerDecoder::default();
        assert!(
            decoder.push_bytes(&frame).is_err(),
            "huge line count must be rejected, not allocated"
        );
    }

    #[test]
    fn client_messages_round_trip() {
        let messages = vec![
            ClientMessage::Attach {
                size: PtySize::new(24, 80),
            },
            ClientMessage::Input(b"exit\r".to_vec()),
            ClientMessage::Detach,
            ClientMessage::Shutdown,
            ClientMessage::SplitPaneColumns,
            ClientMessage::SplitPaneRows,
            ClientMessage::ClosePane,
            ClientMessage::CyclePane,
            ClientMessage::CyclePaneBackward,
            ClientMessage::ShowPaneNumbers,
            ClientMessage::ResizePaneLeft,
            ClientMessage::ResizePaneRight,
            ClientMessage::ResizePaneUp,
            ClientMessage::ResizePaneDown,
            ClientMessage::PresetTwoColumns,
            ClientMessage::PresetThreeColumns,
            ClientMessage::PresetQuadrants,
            ClientMessage::ScrollUp(5),
            ClientMessage::ScrollDown(3),
            ClientMessage::ScrollToBottom,
            ClientMessage::YankViewport,
            ClientMessage::ToggleZoom,
            ClientMessage::SearchBegin,
            ClientMessage::SearchInput(b"hello".to_vec()),
            ClientMessage::SearchCommit,
            ClientMessage::SearchCancel,
            ClientMessage::SearchClear,
            ClientMessage::SearchNext,
            ClientMessage::SearchPrev,
            ClientMessage::BeginSelection(SelectionKind::Line),
            ClientMessage::BeginSelection(SelectionKind::Char),
            ClientMessage::BeginSelection(SelectionKind::Rect),
            ClientMessage::ExtendSelection(SelectionMove::LineUp),
            ClientMessage::ExtendSelection(SelectionMove::LineDown),
            ClientMessage::ExtendSelection(SelectionMove::HalfPageUp),
            ClientMessage::ExtendSelection(SelectionMove::HalfPageDown),
            ClientMessage::ExtendSelection(SelectionMove::FullPageUp),
            ClientMessage::ExtendSelection(SelectionMove::FullPageDown),
            ClientMessage::ExtendSelection(SelectionMove::BufferTop),
            ClientMessage::ExtendSelection(SelectionMove::BufferBottom),
            ClientMessage::ExtendSelection(SelectionMove::CharLeft),
            ClientMessage::ExtendSelection(SelectionMove::CharRight),
            ClientMessage::YankSelection,
            ClientMessage::ClearSelection,
            ClientMessage::SwapPaneNext,
            ClientMessage::SwapPanePrevious,
            ClientMessage::RenameBegin,
            ClientMessage::RenameInput(b"foo".to_vec()),
            ClientMessage::RenameCommit,
            ClientMessage::RenameCancel,
            ClientMessage::CommandPromptBegin(CommandPromptKind::SplitColumns),
            ClientMessage::CommandPromptBegin(CommandPromptKind::SplitRows),
            ClientMessage::CommandPromptBegin(CommandPromptKind::General),
            ClientMessage::CommandPromptInput(b"htop".to_vec()),
            ClientMessage::CommandPromptCommit,
            ClientMessage::CommandPromptCancel,
            ClientMessage::NewWindow,
            ClientMessage::CloseWindow,
            ClientMessage::NextWindow,
            ClientMessage::PreviousWindow,
            ClientMessage::LastWindow,
            ClientMessage::ToggleSyncPanes,
            ClientMessage::CyclePreset,
            ClientMessage::SelectWindow(0),
            ClientMessage::SelectWindow(7),
            ClientMessage::PasteBuffer,
            ClientMessage::ListPanes,
            ClientMessage::OpenSupervisor,
        ];

        let mut encoded = Vec::new();
        for message in &messages {
            encoded.extend_from_slice(&encode_client_message(message).expect("encode client"));
        }

        let mut decoder = ClientDecoder::default();
        let decoded = decoder.push_bytes(&encoded).expect("decode clients");
        assert_eq!(decoded, messages);
    }

    #[test]
    fn server_messages_round_trip() {
        let messages = vec![
            ServerMessage::Frame {
                size: PtySize::new(24, 80),
                mouse_tracking_mode: MouseTrackingMode::Drag,
                lines: vec!["left".into(), "right".into()],
                cursor: Some((3, 42)),
            },
            ServerMessage::Frame {
                size: PtySize::new(24, 80),
                mouse_tracking_mode: MouseTrackingMode::Off,
                lines: vec!["only".into()],
                cursor: None,
            },
            ServerMessage::Exited { code: 7 },
            ServerMessage::Busy,
        ];

        let mut encoded = Vec::new();
        for message in &messages {
            encoded.extend_from_slice(&encode_server_message(message).expect("encode server"));
        }

        let mut decoder = ServerDecoder::default();
        let decoded = decoder.push_bytes(&encoded).expect("decode servers");
        assert_eq!(decoded, messages);
    }

    #[test]
    fn frame_without_cursor_tail_decodes_as_hidden() {
        // Frames from a pre-0.3 daemon end right after the lines — no
        // cursor tail. A newer client must treat that as "no cursor"
        // (today's behavior) instead of erroring, so attaching to an
        // old running daemon keeps working.
        let mut body = Vec::new();
        body.push(super::SERVER_FRAME);
        super::write_size(&mut body, PtySize::new(24, 80));
        body.push(MouseTrackingMode::Off.as_u8());
        super::write_u16(&mut body, 1);
        super::write_u32(&mut body, 2);
        body.extend_from_slice(b"hi");
        let framed = super::wrap_frame(body).expect("wrap frame");

        let mut decoder = ServerDecoder::default();
        let decoded = decoder.push_bytes(&framed).expect("decode legacy frame");
        assert_eq!(
            decoded,
            vec![ServerMessage::Frame {
                size: PtySize::new(24, 80),
                mouse_tracking_mode: MouseTrackingMode::Off,
                lines: vec!["hi".into()],
                cursor: None,
            }],
        );
    }

    #[test]
    fn capture_round_trip() {
        let original = ClientMessage::Capture {
            pane_id: 7,
            path: "/tmp/zmux-cap.bin".to_string(),
        };
        let bytes = encode_client_message(&original).expect("encode capture");
        let mut decoder = ClientDecoder::default();
        let decoded = decoder.push_bytes(&bytes).expect("decode capture");
        assert_eq!(decoded, vec![original]);
    }

    #[test]
    fn pane_label_round_trip() {
        let cases = vec![
            ClientMessage::SetLabel {
                pane_id: 7,
                label: Some("agent-α".into()),
            },
            ClientMessage::SetLabel {
                pane_id: 0,
                label: None,
            },
        ];
        let mut encoded = Vec::new();
        for case in &cases {
            encoded.extend_from_slice(&encode_client_message(case).expect("encode set-label"));
        }
        let mut decoder = ClientDecoder::default();
        let decoded = decoder.push_bytes(&encoded).expect("decode set-label");
        assert_eq!(decoded, cases);
    }

    #[test]
    fn pane_list_round_trip() {
        let original = ServerMessage::PaneList(vec![
            PaneSummary {
                pane_id: 1,
                label: None,
                state: "Idle".into(),
                last_command: None,
                last_exit: None,
                size_cols: 80,
                size_rows: 24,
            },
            PaneSummary {
                pane_id: 2,
                label: Some("agent".into()),
                state: "Working".into(),
                last_command: Some("htop".into()),
                last_exit: Some(0),
                size_cols: 120,
                size_rows: 36,
            },
            PaneSummary {
                pane_id: 3,
                label: None,
                state: "Exited(137)".into(),
                last_command: None,
                last_exit: Some(137),
                size_cols: 1,
                size_rows: 1,
            },
        ]);
        let bytes = encode_server_message(&original).expect("encode pane-list");
        let mut decoder = ServerDecoder::default();
        let decoded = decoder.push_bytes(&bytes).expect("decode pane-list");
        assert_eq!(decoded, vec![original]);
    }

    #[test]
    fn pane_list_empty_round_trip() {
        let original = ServerMessage::PaneList(Vec::new());
        let bytes = encode_server_message(&original).expect("encode empty pane-list");
        let mut decoder = ServerDecoder::default();
        let decoded = decoder.push_bytes(&bytes).expect("decode empty pane-list");
        assert_eq!(decoded, vec![original]);
    }

    #[test]
    fn partial_frames_stay_buffered_until_complete() {
        let encoded = encode_server_message(&ServerMessage::Exited { code: 9 }).expect("encode");
        let split = encoded.len() / 2;

        let mut decoder = ServerDecoder::default();
        let first = decoder
            .push_bytes(&encoded[..split])
            .expect("decode partial");
        let second = decoder
            .push_bytes(&encoded[split..])
            .expect("decode remainder");

        assert!(first.is_empty());
        assert_eq!(second, vec![ServerMessage::Exited { code: 9 }]);
    }
}
