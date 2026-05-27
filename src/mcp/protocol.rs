// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

//! JSON-RPC and MCP envelope primitives. Owns wire-format shapes and
//! the per-line parser [`process_request_line`]. `dispatch`,
//! `execute`, and `server` stay free of JSON-RPC framing concerns.

use std::sync::mpsc::Sender;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::dispatch::dispatch_method;
use super::execute::{McpRequest, McpResponse};
use super::server::OutboundQueue;
use crate::events::Event;

const SERVER_NAME: &str = "zmux";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

pub(super) struct MethodError {
    code: i64,
    message: String,
    data: Option<Value>,
}

impl MethodError {
    pub(super) fn invalid_params(msg: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: msg.into(),
            data: None,
        }
    }

    pub(super) fn method_not_found(msg: impl Into<String>) -> Self {
        Self {
            code: -32601,
            message: msg.into(),
            data: None,
        }
    }

    pub(super) fn internal(msg: impl Into<String>) -> Self {
        Self {
            code: -32603,
            message: msg.into(),
            data: None,
        }
    }
}

/// `notify_tx`, when present, is the per-conn outbound queue
/// `watch_events` uses to stream notifications; passing `None` makes
/// that tool return -32603 but leaves the rest of the surface intact.
pub(super) fn process_request_line(
    line: &str,
    tx: &Sender<McpRequest>,
    notify_tx: Option<&OutboundQueue>,
) -> Option<Value> {
    let parsed: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(err) => {
            // JSON-RPC spec: parse errors carry id=null because the
            // request was never decoded far enough to read the id.
            return Some(error_response(
                Value::Null,
                -32700,
                &format!("parse error: {err}"),
            ));
        }
    };
    let id = parsed.get("id").cloned().unwrap_or(Value::Null);
    let is_notification = parsed.get("id").is_none();
    let method = match parsed.get("method").and_then(|v| v.as_str()) {
        Some(m) => m,
        None => {
            if is_notification {
                return None;
            }
            return Some(error_response(id, -32600, "missing `method` field"));
        }
    };
    let params = parsed.get("params").cloned().unwrap_or(Value::Null);
    let result = dispatch_method(method, &params, tx, notify_tx);
    if is_notification {
        return None;
    }
    Some(match result {
        Ok(value) => json!({"jsonrpc": "2.0", "id": id, "result": value}),
        Err(MethodError {
            code,
            message,
            data,
        }) => {
            let mut err = json!({"code": code, "message": message});
            if let Some(data) = data {
                // `err` was just constructed as a JSON object literal, so
                // `as_object_mut` always returns Some here.
                err.as_object_mut()
                    .expect("error frame just built as object")
                    .insert("data".into(), data);
            }
            json!({"jsonrpc": "2.0", "id": id, "error": err})
        }
    })
}

pub(super) fn initialize_result() -> Value {
    json!({
        "protocolVersion": MCP_PROTOCOL_VERSION,
        "capabilities": {
            "tools": {},
            "resources": {}
        },
        "serverInfo": {
            "name": SERVER_NAME,
            "version": SERVER_VERSION
        }
    })
}

pub(super) fn tools_list_result() -> Value {
    json!({"tools": tool_descriptors()})
}

pub(super) fn resources_list_result() -> Value {
    json!({
        "resources": [
            {
                "uri": "zmux://panes",
                "name": "panes",
                "description": "Live snapshot of all panes in the session. Mirrors the `list_panes` tool — clients that just want to observe pane state can read this resource without invoking a tool.",
                "mimeType": "application/json"
            }
        ]
    })
}

pub(super) fn format_resource_read(uri: &str, payload: &Value) -> Value {
    let text = serde_json::to_string_pretty(payload).unwrap_or_else(|_| payload.to_string());
    json!({
        "contents": [{
            "uri": uri,
            "mimeType": "application/json",
            "text": text
        }]
    })
}

/// Adding a tool requires a new entry here, a dispatch arm in
/// `mcp::dispatch::build_call`, and a variant in
/// `mcp::dispatch::McpCall`. Schemas are JSON-Schema 2020-12.
fn tool_descriptors() -> Vec<Value> {
    vec![
        json!({
            "name": "list_panes",
            "description": "List all panes in the session, with id, window index, active-window flag, label, agent state, last command, last exit, and PTY size. Returns `{ panes: [...] }`.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            },
            "outputSchema": {
                "type": "object",
                "properties": {
                    "panes": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "pane_id":      {"type": "integer", "minimum": 0},
                                "window_index": {"type": "integer", "minimum": 0},
                                "active_window":{"type": "boolean"},
                                "label":        {"type": ["string", "null"]},
                                "state":        {"type": "string"},
                                "last_command": {"type": ["string", "null"]},
                                "last_exit":    {"type": ["integer", "null"]},
                                "size_cols":    {"type": "integer", "minimum": 0},
                                "size_rows":    {"type": "integer", "minimum": 0}
                            },
                            "required": ["pane_id", "window_index", "active_window", "state", "size_cols", "size_rows"]
                        }
                    }
                },
                "required": ["panes"]
            }
        }),
        json!({
            "name": "kill_pane",
            "description": "Close a pane by id. If the pane is the sole pane in a non-final window, closes that window too; refuses only the final pane in the final window. Returns `{ ok: true }` on success.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pane_id": {"type": "integer", "minimum": 0}
                },
                "required": ["pane_id"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "set_label",
            "description": "Set the human-readable label for a pane (rendered in the pane header and the supervisor overlay). Use an empty string to clear the label.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pane_id": {"type": "integer", "minimum": 0},
                    "label": {"type": "string"}
                },
                "required": ["pane_id", "label"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "read_pane",
            "description": "Read a snapshot of a pane's rendered text. `mode` chooses \"visible\" (the current viewport, default) or \"scrollback\" (up to `lines` of recent history). `lines` defaults to 200; `strip_ansi` (default false) post-processes the lines through the ANSI-stripping helper. Returns `{ text, cursor_at_bottom }` where `text` is newline-joined and `cursor_at_bottom` reports whether the viewport is pinned to the latest output.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pane_id": {"type": "integer", "minimum": 0},
                    "lines": {"type": "integer", "minimum": 1, "default": 200},
                    "strip_ansi": {"type": "boolean", "default": false},
                    "mode": {
                        "type": "string",
                        "enum": ["visible", "scrollback"],
                        "default": "visible"
                    }
                },
                "required": ["pane_id"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "read_pane_output",
            "description": "Read raw PTY output recorded for a pane. `since_byte` is an absolute cursor previously returned by this tool; `max_bytes` defaults to 65536, is capped to the pane's retained transcript window, and may be 0 to query only the current cursor. `strip_ansi` removes common ANSI escape sequences after bytes are decoded as UTF-8-lossy. Returns `{ pane_id, start_byte, byte_cursor, text, truncated }`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pane_id": {"type": "integer", "minimum": 0},
                    "since_byte": {"type": "integer", "minimum": 0, "default": 0},
                    "max_bytes": {"type": "integer", "minimum": 0, "maximum": 4194304, "default": 65536},
                    "strip_ansi": {"type": "boolean", "default": false}
                },
                "required": ["pane_id"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "send_keys",
            "description": "Send raw bytes to a pane's PTY. `keys` is a UTF-8 string written verbatim (no shell expansion); set `enter: true` to append a carriage return (\"\\r\") afterwards. Set `clear_input: true` to send Ctrl-U before `keys`, useful when a previous agent prompt left stale text in the input box. Use embedded escapes for control codes: \"\\u0004\" is Ctrl-D, \"\\u001b[A\" is Up arrow. Set `wait_for_idle: true` to block until the pane emits output for this input and settles, returning `text`, `state`, `timed_out`, and `matched_expect` alongside `ok`. `expect_text` waits for sentinel text in the recent rendered lines and implies `wait_for_idle`; use `wait_lines` to tune how much recent output is scanned/returned. NOTE: panes spawned with `bash -c '<cmd>'` (or any non-interactive shell) only execute `<cmd>` and never read stdin, so `send_keys` will echo into the scrollback but nothing will run; spawn with `bash -i` or just `bash` for an interactive REPL that reads keystrokes.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pane_id": {"type": "integer", "minimum": 0},
                    "keys": {"type": "string"},
                    "enter": {"type": "boolean", "default": false},
                    "clear_input": {"type": "boolean", "default": false},
                    "wait_for_idle": {"type": "boolean", "default": false},
                    "max_wait_ms": {"type": "integer", "minimum": 0, "maximum": 60000, "default": 5000},
                    "expect_text": {"type": ["string", "null"]},
                    "wait_lines": {"type": "integer", "minimum": 1, "maximum": 10000, "default": 200}
                },
                "required": ["pane_id", "keys"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "wait_pane",
            "description": "Wait for a pane to settle without sending input. Completes when the pane is Idle/AwaitingInput (or errored/exited) and optional `expect_text` is present in recent rendered lines. Returns `text`, `state`, `timed_out`, and `matched_expect` alongside `ok`. Use after external input, hooks, or a human-driven step when the caller needs a bounded wait without injecting bytes. Use unique sentinels for `expect_text`; `max_wait_ms` defaults to 5000 and is capped at 60000, and `wait_lines` controls how much recent output is scanned/returned.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pane_id": {"type": "integer", "minimum": 0},
                    "expect_text": {"type": ["string", "null"]},
                    "max_wait_ms": {"type": "integer", "minimum": 0, "maximum": 60000, "default": 5000},
                    "wait_lines": {"type": "integer", "minimum": 1, "maximum": 10000, "default": 200}
                },
                "required": ["pane_id"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "spawn_pane",
            "description": "Spawn a new pane running `command` (passed to /bin/sh -c). `split` chooses how to place it: \"h\" (default) puts it to the right of `target_pane` (or the active pane), \"v\" puts it below, \"window\" creates a brand-new window. `label` optionally sets a human-readable label on the new pane.\n\nSet `wait_for_idle: true` to make the response block until the spawned pane has settled (gone Working → Idle / AwaitingInput, or errored / exited). The reply then includes a `text` field with the pane's rendered output and a `state` field with its agent state — saving the round-trip of a follow-up `read_pane` for slow-starting TUIs (e.g., agent CLIs that take seconds to paint a splash). `max_wait_ms` (default 5000, max 60000) caps how long the daemon will hold the response; if the deadline fires first you get `timed_out: true` so you can distinguish a still-loading pane.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "label": {"type": ["string", "null"]},
                    "split": {
                        "type": "string",
                        "enum": ["h", "v", "window"],
                        "default": "h"
                    },
                    "target_pane": {"type": ["integer", "null"], "minimum": 0},
                    "wait_for_idle": {"type": "boolean", "default": false},
                    "max_wait_ms": {"type": "integer", "minimum": 0, "maximum": 60000, "default": 5000}
                },
                "required": ["command"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "watch_events",
            "description": "Subscribe to live pane lifecycle events. Once called, the client receives JSON-RPC notifications (`method: \"zmux/event\"`) for PaneSpawned, PaneClosed, PaneStateChanged, PaneOutput, PaneExited, and LabelChanged. The subscription remains active for the connection lifetime; closing the socket detaches it. Only one subscription per connection is allowed — a second `watch_events` call returns a tool-level error; reconnect to start a fresh subscription.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        }),
    ]
}

/// `method` is always `zmux/event`; clients discriminate on
/// `params.kind` and read variant-specific fields from `params.data`.
pub fn event_notification(event: &Event) -> Value {
    let (kind, data) = match event {
        Event::PaneSpawned { pane_id, label } => {
            ("PaneSpawned", json!({ "pane_id": pane_id, "label": label }))
        }
        Event::PaneClosed { pane_id } => ("PaneClosed", json!({ "pane_id": pane_id })),
        Event::PaneStateChanged { pane_id, from, to } => (
            "PaneStateChanged",
            json!({ "pane_id": pane_id, "from": from, "to": to }),
        ),
        Event::PaneOutput {
            pane_id,
            bytes_delta,
            last_line_preview,
        } => (
            "PaneOutput",
            json!({
                "pane_id": pane_id,
                "bytes_delta": bytes_delta,
                "last_line_preview": last_line_preview,
            }),
        ),
        Event::PaneExited { pane_id, exit_code } => (
            "PaneExited",
            json!({ "pane_id": pane_id, "exit_code": exit_code }),
        ),
        Event::LabelChanged { pane_id, label } => (
            "LabelChanged",
            json!({ "pane_id": pane_id, "label": label }),
        ),
    };
    json!({
        "jsonrpc": "2.0",
        "method": "zmux/event",
        "params": { "kind": kind, "data": data }
    })
}

/// Per MCP spec 2025-06-18, structured tool output belongs in
/// `structuredContent`; `content[0].text` is the human-readable view.
pub(super) fn format_tool_result(response: McpResponse) -> Value {
    match response {
        McpResponse::Ok(value) => {
            let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
            json!({
                "content": [{
                    "type": "text",
                    "text": text
                }],
                "structuredContent": value,
                "isError": false
            })
        }
        McpResponse::Err(message) => json!({
            "content": [{
                "type": "text",
                "text": message
            }],
            "isError": true
        }),
    }
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message}
    })
}

/// Test-only surface: drives dispatch without a Unix socket; passes
/// `None` for the outbound queue so `watch_events` returns -32603.
#[doc(hidden)]
pub fn process_request_line_for_test(line: &str, tx: &Sender<McpRequest>) -> Option<Value> {
    process_request_line(line, tx, None)
}

#[derive(Serialize, Deserialize)]
#[doc(hidden)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Value,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn initialize_returns_protocol_version_and_server_info() {
        let (tx, _rx) = mpsc::channel::<McpRequest>();
        let req = json!({"jsonrpc":"2.0","id":1,"method":"initialize"}).to_string();
        let response = process_request_line(&req, &tx, None).expect("expected reply");
        let result = &response["result"];
        assert_eq!(result["protocolVersion"], MCP_PROTOCOL_VERSION);
        assert_eq!(result["serverInfo"]["name"], SERVER_NAME);
        assert_eq!(result["serverInfo"]["version"], SERVER_VERSION);
    }

    #[test]
    fn tools_list_includes_list_panes() {
        let (tx, _rx) = mpsc::channel::<McpRequest>();
        let req = json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}).to_string();
        let response = process_request_line(&req, &tx, None).expect("expected reply");
        let tools = response["result"]["tools"].as_array().expect("tools array");
        assert!(tools.iter().any(|t| t["name"] == "list_panes"));
        assert!(tools.iter().any(|t| t["name"] == "watch_events"));
    }

    #[test]
    fn parse_error_returns_minus_32700() {
        let (tx, _rx) = mpsc::channel::<McpRequest>();
        let response = process_request_line("not json", &tx, None).expect("expected reply");
        assert_eq!(response["error"]["code"], -32700);
        assert!(response["id"].is_null());
    }

    #[test]
    fn unknown_method_returns_minus_32601() {
        let (tx, _rx) = mpsc::channel::<McpRequest>();
        let req = json!({"jsonrpc":"2.0","id":7,"method":"frobnicate"}).to_string();
        let response = process_request_line(&req, &tx, None).expect("expected reply");
        assert_eq!(response["error"]["code"], -32601);
        assert_eq!(response["id"], 7);
    }

    #[test]
    fn notifications_drop_silently() {
        let (tx, _rx) = mpsc::channel::<McpRequest>();
        let req = json!({"jsonrpc":"2.0","method":"notifications/initialized"}).to_string();
        assert!(process_request_line(&req, &tx, None).is_none());
    }

    #[test]
    fn event_notification_pane_spawned_shape() {
        let v = event_notification(&Event::PaneSpawned {
            pane_id: 7,
            label: Some("agent".into()),
        });
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["method"], "zmux/event");
        assert!(v.get("id").is_none(), "notifications must omit id");
        assert_eq!(v["params"]["kind"], "PaneSpawned");
        assert_eq!(v["params"]["data"]["pane_id"], 7);
        assert_eq!(v["params"]["data"]["label"], "agent");
    }

    #[test]
    fn initialize_advertises_resources_capability() {
        let (tx, _rx) = mpsc::channel::<McpRequest>();
        let req = json!({"jsonrpc":"2.0","id":1,"method":"initialize"}).to_string();
        let response = process_request_line(&req, &tx, None).expect("expected reply");
        assert!(
            response["result"]["capabilities"]["resources"].is_object(),
            "resources capability must be advertised"
        );
    }

    #[test]
    fn resources_list_includes_zmux_panes() {
        let (tx, _rx) = mpsc::channel::<McpRequest>();
        let req = json!({"jsonrpc":"2.0","id":1,"method":"resources/list"}).to_string();
        let response = process_request_line(&req, &tx, None).expect("expected reply");
        let resources = response["result"]["resources"]
            .as_array()
            .expect("resources array");
        assert!(resources.iter().any(|r| r["uri"] == "zmux://panes"));
    }

    #[test]
    fn format_resource_read_shape() {
        let payload = json!([{"pane_id": 1}]);
        let v = format_resource_read("zmux://panes", &payload);
        let contents = v["contents"].as_array().expect("contents array");
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["uri"], "zmux://panes");
        assert_eq!(contents[0]["mimeType"], "application/json");
        let text = contents[0]["text"].as_str().expect("text string");
        let reparsed: Value = serde_json::from_str(text).expect("parse text");
        assert_eq!(reparsed, payload);
    }

    #[test]
    fn event_notification_covers_every_variant() {
        for event in [
            Event::PaneSpawned {
                pane_id: 1,
                label: None,
            },
            Event::PaneClosed { pane_id: 2 },
            Event::PaneStateChanged {
                pane_id: 3,
                from: "Idle".into(),
                to: "Working".into(),
            },
            Event::PaneOutput {
                pane_id: 4,
                bytes_delta: 128,
                last_line_preview: "hello".into(),
            },
            Event::PaneExited {
                pane_id: 5,
                exit_code: 0,
            },
            Event::LabelChanged {
                pane_id: 6,
                label: Some("x".into()),
            },
        ] {
            let v = event_notification(&event);
            assert_eq!(v["jsonrpc"], "2.0");
            assert_eq!(v["method"], "zmux/event");
            assert!(v["params"]["kind"].is_string());
            assert!(v["params"]["data"]["pane_id"].is_u64());
        }
    }
}
