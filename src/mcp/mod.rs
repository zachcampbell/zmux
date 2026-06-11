// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

//! In-daemon MCP (Model Context Protocol) server.
//!
//! Exposes a hand-rolled newline-delimited JSON-RPC 2.0 server on a
//! per-session Unix socket so external AI clients can spawn and drive
//! panes. Tool calls are queued via `mpsc` and executed on the daemon's
//! render thread so the `Workspace` stays uniquely owned and
//! synchronous.
//!
//! - `protocol` — JSON-RPC framing + MCP envelope helpers.
//! - `dispatch` — turns `tools/call` arguments into typed `McpCall`s.
//! - `execute` — runs `McpCall`s on the main thread via `drain_requests`.
//! - `server` — Unix-socket transport (listener + per-conn thread).
//! - `stdio` — stdio↔socket bridge for MCP clients that launch zmux
//!   as a subprocess.

mod audit;
mod bridge_state;
mod dispatch;
mod execute;
mod protocol;
mod server;
mod stdio;

pub use audit::AuditLog;
pub use execute::{McpRequest, Pending, drain_requests, tick_pending};
pub use protocol::{JsonRpcRequest, event_notification, process_request_line_for_test};
pub use server::{session_mcp_socket_path, spawn_listener};
pub use stdio::run_stdio_bridge;
