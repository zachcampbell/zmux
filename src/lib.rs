// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

pub mod agent;
pub mod agent_shim;
pub mod claude;
pub mod claude_hooks;
pub mod codex;
pub mod command;
pub mod config;
pub mod daemon;
pub mod dispatch;
pub mod events;
pub mod input;
pub mod interactive;
pub mod layout;
pub mod mcp;
pub mod mouse;
pub mod mux;
pub mod pair;
pub mod pane;
pub mod protocol;
pub mod pty;
pub mod scrollback;
pub mod session;
pub mod state_paths;
pub mod style;
pub mod supervisor;
pub mod terminal;
pub mod tty;
pub mod windows;
pub mod workspace;
pub mod workspace_render;

pub use config::Config;
pub use daemon::{
    AttachOutcome, PruneReport, SessionEntry, attach_session, create_session, default_session_name,
    kill_session, list_sessions, list_sessions_verbose, print_session_list,
    print_session_list_verbose, prune_stale_sessions, run_server, send_admin_message,
};
pub use input::{InputAction, InputParser, MouseEvent};
pub use interactive::run_shell;
pub use layout::{PaneLayout, Rect, WorkspaceLayout};
pub use mouse::{
    AltScreenScrollPolicy, MouseTrackingMode, ScreenMode, WheelDirection, WheelRouting,
};
pub use mux::run_mux;
pub use pane::{Pane, WheelOutcome};
pub use protocol::ClientMessage;
pub use pty::{PtyProcess, PtySize};
pub use scrollback::ScrollbackBuffer;
pub use session::{CompletedSession, Session};
pub use windows::WindowSet;
pub use workspace::Workspace;
