// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Append-only audit log for mutating MCP tool calls.
//!
//! The MCP socket grants shell-equivalent authority with no
//! authentication beyond filesystem permissions, so the daemon keeps
//! a per-session record of every call that injects input, spawns or
//! kills panes, or relabels them — enough to reconstruct "which
//! connection did what, when" after an agent does something
//! surprising. Read-only tools (`list_panes`, `read_pane`,
//! `wait_pane`, …) are not recorded.
//!
//! One JSON object per line, written to
//! `$ZMUX_STATE_DIR/audit/<session>.jsonl` (mode 0600):
//!
//! ```text
//! {"ts_ms":1765432100123,"conn":3,"tool":"send_keys","pane_id":10002,"keys":"cargo test\r","enter":true}
//! ```
//!
//! `conn` is a daemon-lifetime counter assigned per accepted MCP
//! connection — it distinguishes concurrent controllers, not users
//! (the socket has no client identity to record). Logging is
//! best-effort: an unwritable state dir disables it with one warning
//! rather than degrading tool calls.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;

use crate::state_paths::{safe_component, state_dir};

use super::dispatch::McpCall;

// Keystroke / command payloads land in the log verbatim up to this
// many bytes — enough to reconstruct what was typed without letting a
// hostile client balloon the log with megabyte sends.
const MAX_LOGGED_TEXT: usize = 2048;

pub struct AuditLog {
    file: Option<File>,
}

impl AuditLog {
    /// Open (creating if needed) the audit log for `session`. Never
    /// fails: on error the log is disabled and a single warning goes
    /// to stderr.
    pub fn open(session: &str) -> Self {
        let path = audit_path(session);
        let file = open_append_0600(&path);
        if file.is_none() {
            eprintln!(
                "zmux mcp: audit log disabled (cannot open {})",
                path.display()
            );
        }
        Self { file }
    }

    /// A disabled log, for tests and callers without a session.
    pub fn disabled() -> Self {
        Self { file: None }
    }

    /// Record a mutating tool call. Read-only calls are ignored.
    pub fn record(&mut self, conn_id: u64, call: &McpCall) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        let mut entry = match call {
            McpCall::SpawnPane {
                command,
                label,
                split,
                target_pane,
                ..
            } => json!({
                "tool": "spawn_pane",
                "command": truncate(command),
                "label": label,
                "split": format!("{split:?}").to_lowercase(),
                "target_pane": target_pane,
            }),
            McpCall::SendKeys {
                pane_id,
                keys,
                enter,
                clear_input,
                ..
            } => json!({
                "tool": "send_keys",
                "pane_id": pane_id,
                "keys": truncate(keys),
                "enter": enter,
                "clear_input": clear_input,
            }),
            McpCall::KillPane { pane_id } => json!({
                "tool": "kill_pane",
                "pane_id": pane_id,
            }),
            McpCall::SetLabel { pane_id, label } => json!({
                "tool": "set_label",
                "pane_id": pane_id,
                "label": truncate(label),
            }),
            // Read-only tools are intentionally not audited.
            McpCall::ListPanes
            | McpCall::WaitPane { .. }
            | McpCall::ReadPane { .. }
            | McpCall::ReadPaneOutput { .. } => return,
        };
        let object = entry.as_object_mut().expect("audit entries are objects");
        object.insert("ts_ms".into(), json!(epoch_ms()));
        object.insert("conn".into(), json!(conn_id));

        if writeln!(file, "{entry}").is_err() {
            // Disk gone / log rotated out from under us: disable
            // rather than spam an error per call.
            eprintln!("zmux mcp: audit log write failed; disabling");
            self.file = None;
        }
    }
}

fn epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn audit_path(session: &str) -> PathBuf {
    state_dir()
        .join("audit")
        .join(format!("{}.jsonl", safe_component(session)))
}

fn open_append_0600(path: &PathBuf) -> Option<File> {
    let dir = path.parent()?;
    fs::create_dir_all(dir).ok()?;
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .ok()
}

fn truncate(text: &str) -> String {
    if text.len() <= MAX_LOGGED_TEXT {
        return text.to_string();
    }
    let mut cut = MAX_LOGGED_TEXT;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…(+{} bytes)", &text[..cut], text.len() - cut)
}
