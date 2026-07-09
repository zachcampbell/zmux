// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Wire-validation layer. Argument-shape errors surface as JSON-RPC
//! `invalid_params` here; valid calls are shipped into the daemon
//! main loop as typed [`McpCall`] values.

use std::sync::mpsc::{self, Sender};
use std::thread;

use serde_json::{Value, json};

use crate::pane::OUTPUT_RING_CAPACITY;

use super::execute::{McpRequest, McpResponse};
use super::protocol::{
    MethodError, event_notification, format_resource_read, format_tool_result, initialize_result,
    resources_list_result, tools_list_result,
};
use super::server::OutboundQueue;

#[derive(Debug)]
pub enum McpCall {
    ListPanes,
    SpawnPane {
        command: String,
        label: Option<String>,
        split: SpawnSplit,
        target_pane: Option<u32>,
        wait_for_idle: bool,
        max_wait_ms: u32,
    },
    SendKeys {
        pane_id: u32,
        keys: String,
        enter: bool,
        clear_input: bool,
        wait_for_idle: bool,
        max_wait_ms: u32,
        expect_text: Option<String>,
        wait_lines: u32,
    },
    WaitPane {
        pane_id: u32,
        max_wait_ms: u32,
        expect_text: Option<String>,
        wait_lines: u32,
    },
    ReadPane {
        pane_id: u32,
        lines: u32,
        strip_ansi: bool,
        mode: ReadMode,
    },
    ReadPaneOutput {
        pane_id: u32,
        since_byte: u64,
        max_bytes: usize,
        strip_ansi: bool,
    },
    KillPane {
        pane_id: u32,
    },
    SetLabel {
        pane_id: u32,
        label: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadMode {
    Visible,
    Scrollback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnSplit {
    Horizontal,
    Vertical,
    NewWindow,
}

pub(super) fn dispatch_method(
    method: &str,
    params: &Value,
    tx: &Sender<McpRequest>,
    notify_tx: Option<&OutboundQueue>,
) -> Result<Value, MethodError> {
    match method {
        "initialize" => Ok(initialize_result()),
        "tools/list" => Ok(tools_list_result()),
        "tools/call" => dispatch_tool_call(params, tx, notify_tx),
        "resources/list" => Ok(resources_list_result()),
        "resources/read" => dispatch_resources_read(params, tx),
        // Per MCP spec this is a notification; accepted as a request
        // too so synchronous-style clients get a clean ack rather
        // than method-not-found.
        "notifications/initialized" => Ok(Value::Null),
        "ping" => Ok(json!({})),
        other => Err(MethodError::method_not_found(format!(
            "unknown method `{other}`"
        ))),
    }
}

fn dispatch_resources_read(params: &Value, tx: &Sender<McpRequest>) -> Result<Value, MethodError> {
    let uri = params
        .get("uri")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MethodError::invalid_params("resources/read requires a string `uri`"))?;
    match uri {
        "zmux://panes" => {
            let response = ship_to_main(McpCall::ListPanes, tx)?;
            match response {
                McpResponse::Ok(payload) => Ok(format_resource_read("zmux://panes", &payload)),
                McpResponse::Err(message) => {
                    Err(MethodError::internal(format!("zmux://panes: {message}")))
                }
            }
        }
        other => Err(MethodError::invalid_params(format!(
            "unknown resource uri `{other}`"
        ))),
    }
}

fn dispatch_tool_call(
    params: &Value,
    tx: &Sender<McpRequest>,
    notify_tx: Option<&OutboundQueue>,
) -> Result<Value, MethodError> {
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MethodError::invalid_params("tools/call requires a string `name`"))?;
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or(Value::Object(Default::default()));
    // `watch_events` is async: it opens a subscription, spawns a
    // pump thread that streams notifications, and acks immediately.
    if name == "watch_events" {
        return dispatch_watch_events(&arguments, tx, notify_tx);
    }
    let call = build_call(name, &arguments)?;
    let response = ship_to_main(call, tx)?;
    Ok(format_tool_result(response))
}

fn dispatch_watch_events(
    arguments: &Value,
    tx: &Sender<McpRequest>,
    notify_tx: Option<&OutboundQueue>,
) -> Result<Value, MethodError> {
    if let Some(obj) = arguments.as_object()
        && !obj.is_empty()
    {
        return Err(MethodError::invalid_params(
            "watch_events takes no arguments",
        ));
    }
    let notify_tx = notify_tx.ok_or_else(|| {
        MethodError::internal(
            "watch_events requires an outbound notification channel (per-conn writer)",
        )
    })?;
    if !notify_tx.try_mark_subscribed() {
        return Ok(format_tool_result(McpResponse::Err(
            "already subscribed; close this connection and reconnect to start a new subscription"
                .to_string(),
        )));
    }
    let (sub_tx, sub_rx) = mpsc::channel();
    tx.send(McpRequest::Subscribe { reply: sub_tx })
        .map_err(|_| MethodError::internal("daemon main loop is gone"))?;
    let receiver = sub_rx
        .recv()
        .map_err(|_| MethodError::internal("daemon dropped the subscription channel"))?;
    let pump_tx = notify_tx.clone();
    let _ = thread::Builder::new()
        .name("zmux-mcp-watch".into())
        .spawn(move || {
            while let Ok(event) = receiver.recv() {
                let notif = event_notification(&event);
                let mut bytes = notif.to_string().into_bytes();
                bytes.push(b'\n');
                if !pump_tx.send_notification(bytes) {
                    return;
                }
            }
        });
    Ok(format_tool_result(McpResponse::Ok(json!({
        "ok": true,
        "subscription_active": true,
    }))))
}

fn build_call(name: &str, arguments: &Value) -> Result<McpCall, MethodError> {
    match name {
        "list_panes" => {
            if let Some(obj) = arguments.as_object()
                && !obj.is_empty()
            {
                return Err(MethodError::invalid_params("list_panes takes no arguments"));
            }
            Ok(McpCall::ListPanes)
        }
        "spawn_pane" => {
            let command = arguments
                .get("command")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    MethodError::invalid_params("spawn_pane requires a string `command`")
                })?
                .to_string();
            if command.is_empty() {
                return Err(MethodError::invalid_params(
                    "spawn_pane `command` must not be empty",
                ));
            }
            let label = match arguments.get("label") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) => Some(s.clone()),
                Some(_) => {
                    return Err(MethodError::invalid_params(
                        "spawn_pane `label` must be a string or null",
                    ));
                }
            };
            let split = match arguments.get("split") {
                None | Some(Value::Null) => SpawnSplit::Horizontal,
                Some(Value::String(s)) => match s.as_str() {
                    "h" => SpawnSplit::Horizontal,
                    "v" => SpawnSplit::Vertical,
                    "window" => SpawnSplit::NewWindow,
                    other => {
                        return Err(MethodError::invalid_params(format!(
                            "spawn_pane `split` must be one of \"h\", \"v\", \"window\"; got {other:?}"
                        )));
                    }
                },
                Some(_) => {
                    return Err(MethodError::invalid_params(
                        "spawn_pane `split` must be a string",
                    ));
                }
            };
            let target_pane = match arguments.get("target_pane") {
                None | Some(Value::Null) => None,
                Some(v) => Some(v.as_u64().and_then(|n| u32::try_from(n).ok()).ok_or_else(
                    || {
                        MethodError::invalid_params(
                            "spawn_pane `target_pane` must be a non-negative integer",
                        )
                    },
                )?),
            };
            let wait_for_idle = match arguments.get("wait_for_idle") {
                None | Some(Value::Null) => false,
                Some(Value::Bool(b)) => *b,
                Some(_) => {
                    return Err(MethodError::invalid_params(
                        "spawn_pane `wait_for_idle` must be a boolean",
                    ));
                }
            };
            let max_wait_ms = match arguments.get("max_wait_ms") {
                None | Some(Value::Null) => 5_000_u32,
                Some(v) => {
                    let n = v.as_u64().ok_or_else(|| {
                        MethodError::invalid_params(
                            "spawn_pane `max_wait_ms` must be a non-negative integer",
                        )
                    })?;
                    let clamped = n.min(60_000);
                    u32::try_from(clamped).unwrap_or(60_000)
                }
            };
            Ok(McpCall::SpawnPane {
                command,
                label,
                split,
                target_pane,
                wait_for_idle,
                max_wait_ms,
            })
        }
        "kill_pane" => {
            let pane_id = arguments
                .get("pane_id")
                .and_then(|v| v.as_u64())
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| {
                    MethodError::invalid_params(
                        "kill_pane requires a non-negative integer `pane_id`",
                    )
                })?;
            Ok(McpCall::KillPane { pane_id })
        }
        "set_label" => {
            let pane_id = arguments
                .get("pane_id")
                .and_then(|v| v.as_u64())
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| {
                    MethodError::invalid_params(
                        "set_label requires a non-negative integer `pane_id`",
                    )
                })?;
            let label = arguments
                .get("label")
                .and_then(|v| v.as_str())
                .ok_or_else(|| MethodError::invalid_params("set_label requires a string `label`"))?
                .to_string();
            Ok(McpCall::SetLabel { pane_id, label })
        }
        "read_pane" => {
            let pane_id = arguments
                .get("pane_id")
                .and_then(|v| v.as_u64())
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| {
                    MethodError::invalid_params(
                        "read_pane requires a non-negative integer `pane_id`",
                    )
                })?;
            let lines = match arguments.get("lines") {
                None | Some(Value::Null) => 200u32,
                Some(v) => {
                    let n = v.as_u64().ok_or_else(|| {
                        MethodError::invalid_params("read_pane `lines` must be a positive integer")
                    })?;
                    if n == 0 {
                        return Err(MethodError::invalid_params(
                            "read_pane `lines` must be >= 1",
                        ));
                    }
                    u32::try_from(n).map_err(|_| {
                        MethodError::invalid_params("read_pane `lines` exceeds u32::MAX")
                    })?
                }
            };
            let strip_ansi = match arguments.get("strip_ansi") {
                None | Some(Value::Null) => false,
                Some(Value::Bool(b)) => *b,
                Some(_) => {
                    return Err(MethodError::invalid_params(
                        "read_pane `strip_ansi` must be a boolean",
                    ));
                }
            };
            let mode = match arguments.get("mode") {
                None | Some(Value::Null) => ReadMode::Visible,
                Some(Value::String(s)) => match s.as_str() {
                    "visible" => ReadMode::Visible,
                    "scrollback" => ReadMode::Scrollback,
                    other => {
                        return Err(MethodError::invalid_params(format!(
                            "read_pane `mode` must be \"visible\" or \"scrollback\"; got {other:?}"
                        )));
                    }
                },
                Some(_) => {
                    return Err(MethodError::invalid_params(
                        "read_pane `mode` must be a string",
                    ));
                }
            };
            Ok(McpCall::ReadPane {
                pane_id,
                lines,
                strip_ansi,
                mode,
            })
        }

        "read_pane_output" => {
            let pane_id = arguments
                .get("pane_id")
                .and_then(|v| v.as_u64())
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| {
                    MethodError::invalid_params(
                        "read_pane_output requires a non-negative integer `pane_id`",
                    )
                })?;
            let since_byte = match arguments.get("since_byte") {
                None | Some(Value::Null) => 0,
                Some(v) => v.as_u64().ok_or_else(|| {
                    MethodError::invalid_params(
                        "read_pane_output `since_byte` must be a non-negative integer",
                    )
                })?,
            };
            let max_bytes = match arguments.get("max_bytes") {
                None | Some(Value::Null) => 64 * 1024,
                Some(v) => {
                    let n = v.as_u64().ok_or_else(|| {
                        MethodError::invalid_params(
                            "read_pane_output `max_bytes` must be a non-negative integer",
                        )
                    })?;
                    let clamped = n.min(OUTPUT_RING_CAPACITY as u64);
                    usize::try_from(clamped).unwrap_or(OUTPUT_RING_CAPACITY)
                }
            };
            let strip_ansi = match arguments.get("strip_ansi") {
                None | Some(Value::Null) => false,
                Some(Value::Bool(b)) => *b,
                Some(_) => {
                    return Err(MethodError::invalid_params(
                        "read_pane_output `strip_ansi` must be a boolean",
                    ));
                }
            };
            Ok(McpCall::ReadPaneOutput {
                pane_id,
                since_byte,
                max_bytes,
                strip_ansi,
            })
        }
        "send_keys" => {
            let pane_id = arguments
                .get("pane_id")
                .and_then(|v| v.as_u64())
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| {
                    MethodError::invalid_params(
                        "send_keys requires a non-negative integer `pane_id`",
                    )
                })?;
            let keys = arguments
                .get("keys")
                .and_then(|v| v.as_str())
                .ok_or_else(|| MethodError::invalid_params("send_keys requires a string `keys`"))?
                .to_string();
            let enter = match arguments.get("enter") {
                None | Some(Value::Null) => false,
                Some(Value::Bool(b)) => *b,
                Some(_) => {
                    return Err(MethodError::invalid_params(
                        "send_keys `enter` must be a boolean",
                    ));
                }
            };
            let clear_input = match arguments.get("clear_input") {
                None | Some(Value::Null) => false,
                Some(Value::Bool(b)) => *b,
                Some(_) => {
                    return Err(MethodError::invalid_params(
                        "send_keys `clear_input` must be a boolean",
                    ));
                }
            };
            let mut wait_for_idle = match arguments.get("wait_for_idle") {
                None | Some(Value::Null) => false,
                Some(Value::Bool(b)) => *b,
                Some(_) => {
                    return Err(MethodError::invalid_params(
                        "send_keys `wait_for_idle` must be a boolean",
                    ));
                }
            };
            let expect_text = match arguments.get("expect_text") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
                Some(Value::String(_)) => {
                    return Err(MethodError::invalid_params(
                        "send_keys `expect_text` must not be empty",
                    ));
                }
                Some(_) => {
                    return Err(MethodError::invalid_params(
                        "send_keys `expect_text` must be a string or null",
                    ));
                }
            };
            if expect_text.is_some() {
                wait_for_idle = true;
            }
            let max_wait_ms = match arguments.get("max_wait_ms") {
                None | Some(Value::Null) => 5_000_u32,
                Some(v) => {
                    let n = v.as_u64().ok_or_else(|| {
                        MethodError::invalid_params(
                            "send_keys `max_wait_ms` must be a non-negative integer",
                        )
                    })?;
                    let clamped = n.min(60_000);
                    u32::try_from(clamped).unwrap_or(60_000)
                }
            };
            let wait_lines = match arguments.get("wait_lines") {
                None | Some(Value::Null) => 200_u32,
                Some(v) => {
                    let n = v.as_u64().ok_or_else(|| {
                        MethodError::invalid_params(
                            "send_keys `wait_lines` must be a positive integer",
                        )
                    })?;
                    if n == 0 {
                        return Err(MethodError::invalid_params(
                            "send_keys `wait_lines` must be >= 1",
                        ));
                    }
                    let clamped = n.min(10_000);
                    u32::try_from(clamped).unwrap_or(10_000)
                }
            };
            Ok(McpCall::SendKeys {
                pane_id,
                keys,
                enter,
                clear_input,
                wait_for_idle,
                max_wait_ms,
                expect_text,
                wait_lines,
            })
        }
        "wait_pane" => {
            let pane_id = arguments
                .get("pane_id")
                .and_then(|v| v.as_u64())
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| {
                    MethodError::invalid_params(
                        "wait_pane requires a non-negative integer `pane_id`",
                    )
                })?;
            let expect_text = match arguments.get("expect_text") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
                Some(Value::String(_)) => {
                    return Err(MethodError::invalid_params(
                        "wait_pane `expect_text` must not be empty",
                    ));
                }
                Some(_) => {
                    return Err(MethodError::invalid_params(
                        "wait_pane `expect_text` must be a string or null",
                    ));
                }
            };
            let max_wait_ms = match arguments.get("max_wait_ms") {
                None | Some(Value::Null) => 5_000_u32,
                Some(v) => {
                    let n = v.as_u64().ok_or_else(|| {
                        MethodError::invalid_params(
                            "wait_pane `max_wait_ms` must be a non-negative integer",
                        )
                    })?;
                    let clamped = n.min(60_000);
                    u32::try_from(clamped).unwrap_or(60_000)
                }
            };
            let wait_lines = match arguments.get("wait_lines") {
                None | Some(Value::Null) => 200_u32,
                Some(v) => {
                    let n = v.as_u64().ok_or_else(|| {
                        MethodError::invalid_params(
                            "wait_pane `wait_lines` must be a positive integer",
                        )
                    })?;
                    if n == 0 {
                        return Err(MethodError::invalid_params(
                            "wait_pane `wait_lines` must be >= 1",
                        ));
                    }
                    let clamped = n.min(10_000);
                    u32::try_from(clamped).unwrap_or(10_000)
                }
            };
            Ok(McpCall::WaitPane {
                pane_id,
                max_wait_ms,
                expect_text,
                wait_lines,
            })
        }
        other => Err(MethodError::invalid_params(format!(
            "unknown tool `{other}`"
        ))),
    }
}

fn ship_to_main(call: McpCall, tx: &Sender<McpRequest>) -> Result<McpResponse, MethodError> {
    let (reply_tx, reply_rx) = mpsc::channel::<McpResponse>();
    tx.send(McpRequest::ToolCall {
        call,
        reply: reply_tx,
        conn_id: super::server::current_conn_id(),
    })
    .map_err(|_| MethodError::internal("daemon main loop is gone"))?;
    reply_rx
        .recv()
        .map_err(|_| MethodError::internal("daemon dropped the reply channel"))
}

#[cfg(test)]
mod tests {
    use super::super::protocol::process_request_line;
    use super::*;
    use std::thread;

    fn process(line: &str, tx: &Sender<McpRequest>) -> Option<Value> {
        process_request_line(line, tx, None)
    }

    fn expect_tool_call(rx: &mpsc::Receiver<McpRequest>) -> (McpCall, Sender<McpResponse>) {
        match rx.recv().expect("expected a queued request") {
            McpRequest::ToolCall { call, reply, .. } => (call, reply),
            McpRequest::Subscribe { .. } => {
                panic!("dispatch tests should not produce Subscribe requests");
            }
        }
    }

    #[test]
    fn tools_call_list_panes_envelope() {
        let (tx, rx) = mpsc::channel::<McpRequest>();
        let req = json!({
            "jsonrpc":"2.0","id":42,"method":"tools/call",
            "params":{"name":"list_panes","arguments":{}}
        })
        .to_string();
        let handle = thread::spawn(move || process(&req, &tx));
        let (call, reply) = expect_tool_call(&rx);
        assert!(matches!(call, McpCall::ListPanes));
        reply
            .send(McpResponse::Ok(json!({"panes": [{"pane_id": 1}]})))
            .unwrap();
        let response = handle.join().unwrap().expect("expected reply");
        assert_eq!(response["id"], 42);
        // MCP 2025-06-18 requires structuredContent to be an object;
        // list_panes wraps the rows under `panes`.
        let structured = &response["result"]["structuredContent"];
        assert!(
            structured.is_object(),
            "structuredContent must be an object per MCP 2025-06-18, got {structured}",
        );
        assert_eq!(structured["panes"][0]["pane_id"], 1);
        let content = &response["result"]["content"];
        let text = content[0]["text"].as_str().unwrap();
        assert!(text.contains("\"panes\""));
        assert!(text.contains("\"pane_id\""));
        assert!(text.contains('1'));
        assert_eq!(response["result"]["isError"], false);
    }

    #[test]
    fn unknown_tool_name_returns_invalid_params() {
        let (tx, rx) = mpsc::channel::<McpRequest>();
        let req = json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"nope","arguments":{}}
        })
        .to_string();
        let response = process(&req, &tx).expect("expected reply");
        assert_eq!(response["error"]["code"], -32602);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn list_panes_rejects_extra_arguments() {
        let (tx, _rx) = mpsc::channel::<McpRequest>();
        let req = json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"list_panes","arguments":{"unexpected":1}}
        })
        .to_string();
        let response = process(&req, &tx).expect("expected reply");
        assert_eq!(response["error"]["code"], -32602);
    }

    #[test]
    fn spawn_pane_default_split_is_horizontal() {
        let (tx, rx) = mpsc::channel::<McpRequest>();
        let req = json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"spawn_pane","arguments":{"command":"ls","label":"build"}}
        })
        .to_string();
        let _handle = thread::spawn(move || process(&req, &tx));
        let (call, reply) = expect_tool_call(&rx);
        match call {
            McpCall::SpawnPane {
                command,
                label,
                split,
                target_pane,
                wait_for_idle,
                max_wait_ms,
            } => {
                assert_eq!(command, "ls");
                assert_eq!(label.as_deref(), Some("build"));
                assert_eq!(split, SpawnSplit::Horizontal);
                assert_eq!(target_pane, None);
                assert!(!wait_for_idle);
                assert_eq!(max_wait_ms, 5_000);
            }
            other => panic!("expected SpawnPane, got {other:?}"),
        }
        reply.send(McpResponse::Ok(json!({"pane_id": 2}))).unwrap();
    }

    #[test]
    fn spawn_pane_split_validation() {
        let (tx, rx) = mpsc::channel::<McpRequest>();
        for (split_str, expected) in [
            ("v", SpawnSplit::Vertical),
            ("window", SpawnSplit::NewWindow),
        ] {
            let tx = tx.clone();
            let req = json!({
                "jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":"spawn_pane","arguments":{"command":"x","split":split_str}}
            })
            .to_string();
            let _h = thread::spawn(move || process(&req, &tx));
            let (call, reply) = expect_tool_call(&rx);
            match call {
                McpCall::SpawnPane { split, .. } => assert_eq!(split, expected),
                other => panic!("expected SpawnPane got {other:?}"),
            }
            reply.send(McpResponse::Ok(json!({"pane_id":1}))).unwrap();
        }
        let (tx2, rx2) = mpsc::channel::<McpRequest>();
        let req = json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"spawn_pane","arguments":{"command":"x","split":"diagonal"}}
        })
        .to_string();
        let response = process(&req, &tx2).expect("reply");
        assert_eq!(response["error"]["code"], -32602);
        assert!(
            rx2.try_recv().is_err(),
            "bad split must not reach the main loop"
        );
    }

    #[test]
    fn kill_pane_validation() {
        let (tx, rx) = mpsc::channel::<McpRequest>();
        let req = json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"kill_pane","arguments":{"pane_id":2}}
        })
        .to_string();
        let _h = thread::spawn(move || process(&req, &tx));
        let (call, reply) = expect_tool_call(&rx);
        match call {
            McpCall::KillPane { pane_id } => assert_eq!(pane_id, 2),
            other => panic!("expected KillPane, got {other:?}"),
        }
        reply.send(McpResponse::Ok(json!({"ok": true}))).unwrap();

        let (tx2, _rx2) = mpsc::channel::<McpRequest>();
        let req = json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"kill_pane","arguments":{}}
        })
        .to_string();
        let response = process(&req, &tx2).expect("reply");
        assert_eq!(response["error"]["code"], -32602);
    }

    #[test]
    fn set_label_validation() {
        let (tx, rx) = mpsc::channel::<McpRequest>();
        let req = json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"set_label","arguments":{"pane_id":4,"label":"build"}}
        })
        .to_string();
        let _h = thread::spawn(move || process(&req, &tx));
        let (call, reply) = expect_tool_call(&rx);
        match call {
            McpCall::SetLabel { pane_id, label } => {
                assert_eq!(pane_id, 4);
                assert_eq!(label, "build");
            }
            other => panic!("expected SetLabel, got {other:?}"),
        }
        reply.send(McpResponse::Ok(json!({"ok": true}))).unwrap();

        let (tx2, _rx2) = mpsc::channel::<McpRequest>();
        let req = json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"set_label","arguments":{"pane_id":4}}
        })
        .to_string();
        let response = process(&req, &tx2).expect("reply");
        assert_eq!(response["error"]["code"], -32602);
    }

    #[test]
    fn read_pane_defaults() {
        let (tx, rx) = mpsc::channel::<McpRequest>();
        let req = json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"read_pane","arguments":{"pane_id":1}}
        })
        .to_string();
        let _h = thread::spawn(move || process(&req, &tx));
        let (call, reply) = expect_tool_call(&rx);
        match call {
            McpCall::ReadPane {
                pane_id,
                lines,
                strip_ansi,
                mode,
            } => {
                assert_eq!(pane_id, 1);
                assert_eq!(lines, 200);
                assert!(!strip_ansi);
                assert_eq!(mode, ReadMode::Visible);
            }
            other => panic!("expected ReadPane, got {other:?}"),
        }
        reply
            .send(McpResponse::Ok(json!({"text":"","cursor_at_bottom":true})))
            .unwrap();
    }

    #[test]
    fn read_pane_rejects_unknown_mode() {
        let (tx, rx) = mpsc::channel::<McpRequest>();
        let req = json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"read_pane","arguments":{"pane_id":1,"mode":"raw"}}
        })
        .to_string();
        let response = process(&req, &tx).expect("reply");
        assert_eq!(response["error"]["code"], -32602);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn read_pane_rejects_invalid_lines() {
        let (tx, _rx) = mpsc::channel::<McpRequest>();
        let req = json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"read_pane","arguments":{"pane_id":1,"lines":0}}
        })
        .to_string();
        let response = process(&req, &tx).expect("reply");
        assert_eq!(response["error"]["code"], -32602);
    }

    #[test]
    fn send_keys_validation() {
        let (tx, rx) = mpsc::channel::<McpRequest>();
        let req = json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"send_keys","arguments":{"pane_id":3,"keys":"ls","enter":true}}
        })
        .to_string();
        let _h = thread::spawn(move || process(&req, &tx));
        let (call, reply) = expect_tool_call(&rx);
        match call {
            McpCall::SendKeys {
                pane_id,
                keys,
                enter,
                clear_input,
                wait_for_idle,
                max_wait_ms,
                expect_text,
                wait_lines,
            } => {
                assert_eq!(pane_id, 3);
                assert_eq!(keys, "ls");
                assert!(enter);
                assert!(!clear_input);
                assert!(!wait_for_idle);
                assert_eq!(max_wait_ms, 5_000);
                assert_eq!(expect_text, None);
                assert_eq!(wait_lines, 200);
            }
            other => panic!("expected SendKeys, got {other:?}"),
        }
        reply.send(McpResponse::Ok(json!({"ok": true}))).unwrap();

        let (tx2, rx2) = mpsc::channel::<McpRequest>();
        let req = json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"send_keys","arguments":{"keys":"x"}}
        })
        .to_string();
        let response = process(&req, &tx2).expect("reply");
        assert_eq!(response["error"]["code"], -32602);
        assert!(rx2.try_recv().is_err());

        let (tx3, _rx3) = mpsc::channel::<McpRequest>();
        let req = json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"send_keys","arguments":{"pane_id":1,"keys":"x","enter":"yes"}}
        })
        .to_string();
        let response = process(&req, &tx3).expect("reply");
        assert_eq!(response["error"]["code"], -32602);
    }

    #[test]
    fn wait_pane_accepts_expect_options() {
        let (tx, rx) = mpsc::channel::<McpRequest>();
        let req = json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"wait_pane","arguments":{
                "pane_id":7,
                "max_wait_ms":120000,
                "expect_text":"DONE",
                "wait_lines":12000
            }}
        })
        .to_string();
        let _h = thread::spawn(move || process(&req, &tx));
        let (call, reply) = expect_tool_call(&rx);
        match call {
            McpCall::WaitPane {
                pane_id,
                max_wait_ms,
                expect_text,
                wait_lines,
            } => {
                assert_eq!(pane_id, 7);
                assert_eq!(max_wait_ms, 60_000);
                assert_eq!(expect_text.as_deref(), Some("DONE"));
                assert_eq!(wait_lines, 10_000);
            }
            other => panic!("expected WaitPane, got {other:?}"),
        }
        reply
            .send(McpResponse::Ok(json!({"ok": true, "matched_expect": true})))
            .unwrap();

        let (tx2, rx2) = mpsc::channel::<McpRequest>();
        let req = json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"wait_pane","arguments":{"expect_text":"DONE"}}
        })
        .to_string();
        let response = process(&req, &tx2).expect("reply");
        assert_eq!(response["error"]["code"], -32602);
        assert!(rx2.try_recv().is_err());
    }

    #[test]
    fn send_keys_accepts_wait_for_idle_options() {
        let (tx, rx) = mpsc::channel::<McpRequest>();
        let req = json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"send_keys","arguments":{
                "pane_id":3,
                "keys":"ls",
                "enter":true,
                "clear_input":true,
                "wait_for_idle":false,
                "max_wait_ms":120000,
                "expect_text":"DONE",
                "wait_lines":12000
            }}
        })
        .to_string();
        let _h = thread::spawn(move || process(&req, &tx));
        let (call, reply) = expect_tool_call(&rx);
        match call {
            McpCall::SendKeys {
                pane_id,
                keys,
                enter,
                clear_input,
                wait_for_idle,
                max_wait_ms,
                expect_text,
                wait_lines,
            } => {
                assert_eq!(pane_id, 3);
                assert_eq!(keys, "ls");
                assert!(enter);
                assert!(clear_input);
                assert!(wait_for_idle);
                assert_eq!(max_wait_ms, 60_000);
                assert_eq!(expect_text.as_deref(), Some("DONE"));
                assert_eq!(wait_lines, 10_000);
            }
            other => panic!("expected SendKeys, got {other:?}"),
        }
        reply.send(McpResponse::Ok(json!({"ok": true}))).unwrap();
    }

    #[test]
    fn spawn_pane_requires_non_empty_command() {
        let (tx, _rx) = mpsc::channel::<McpRequest>();
        let req = json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"spawn_pane","arguments":{"command":""}}
        })
        .to_string();
        let response = process(&req, &tx).expect("reply");
        assert_eq!(response["error"]["code"], -32602);

        let (tx, _rx) = mpsc::channel::<McpRequest>();
        let req = json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"spawn_pane","arguments":{}}
        })
        .to_string();
        let response = process(&req, &tx).expect("reply");
        assert_eq!(response["error"]["code"], -32602);
    }

    #[test]
    fn watch_events_rejects_extra_arguments() {
        let (tx, _rx) = mpsc::channel::<McpRequest>();
        let req = json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"watch_events","arguments":{"foo":1}}
        })
        .to_string();
        let response = process(&req, &tx).expect("reply");
        assert_eq!(response["error"]["code"], -32602);
    }

    #[test]
    fn watch_events_without_notify_tx_is_internal_error() {
        let (tx, _rx) = mpsc::channel::<McpRequest>();
        let req = json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"watch_events","arguments":{}}
        })
        .to_string();
        let response = process(&req, &tx).expect("reply");
        assert_eq!(response["error"]["code"], -32603);
    }

    #[test]
    fn watch_events_returns_subscription_active_envelope() {
        let (tx, rx) = mpsc::channel::<McpRequest>();
        let (notify_tx, _notify_rx) = super::super::server::outbound_for_test();
        let (sub_event_tx, sub_event_rx) = mpsc::channel::<crate::events::Event>();
        let req = json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"watch_events","arguments":{}}
        })
        .to_string();
        let handle = thread::spawn(move || process_request_line(&req, &tx, Some(&notify_tx)));
        match rx.recv().expect("queued Subscribe") {
            McpRequest::Subscribe { reply } => {
                reply
                    .send(sub_event_rx)
                    .expect("send subscription receiver");
            }
            McpRequest::ToolCall { .. } => panic!("expected Subscribe, got ToolCall"),
        }
        let response = handle.join().unwrap().expect("expected reply");
        assert_eq!(response["result"]["isError"], false);
        let payload = &response["result"]["structuredContent"];
        assert_eq!(payload["subscription_active"], true);
        sub_event_tx
            .send(crate::events::Event::PaneClosed { pane_id: 1 })
            .expect("event delivered to pump");
    }

    #[test]
    fn watch_events_rejects_duplicate_subscription_on_same_connection() {
        let (tx, rx) = mpsc::channel::<McpRequest>();
        let (notify_tx, _notify_rx) = super::super::server::outbound_for_test();
        let (sub_event_tx, sub_event_rx) = mpsc::channel::<crate::events::Event>();
        let req = json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"watch_events","arguments":{}}
        })
        .to_string();
        let notify_tx_clone = notify_tx.clone();
        let req_first = req.clone();
        let handle =
            thread::spawn(move || process_request_line(&req_first, &tx, Some(&notify_tx_clone)));
        match rx.recv().expect("queued Subscribe") {
            McpRequest::Subscribe { reply } => {
                reply
                    .send(sub_event_rx)
                    .expect("send subscription receiver");
            }
            McpRequest::ToolCall { .. } => {
                panic!("expected Subscribe, got ToolCall")
            }
        }
        let first = handle.join().unwrap().expect("expected reply");
        assert_eq!(first["result"]["isError"], false);
        assert_eq!(
            first["result"]["structuredContent"]["subscription_active"],
            true
        );

        let (tx2, rx2) = mpsc::channel::<McpRequest>();
        let response_second = process_request_line(&req, &tx2, Some(&notify_tx)).expect("reply");
        assert_eq!(response_second["result"]["isError"], true);
        let text = response_second["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("");
        assert!(
            text.contains("already subscribed"),
            "expected 'already subscribed' in error text, got: {text}",
        );
        assert!(
            rx2.try_recv().is_err(),
            "duplicate watch_events should not send a Subscribe to the daemon",
        );

        // Keep the first subscription's sender alive so the pump
        // thread doesn't hang up while the test exits.
        sub_event_tx
            .send(crate::events::Event::PaneClosed { pane_id: 1 })
            .expect("event delivered to pump");
    }

    #[test]
    fn outbound_queue_drops_on_full_and_continues() {
        let (queue, rx) = super::super::server::outbound_for_test_with_bound(2);
        assert!(queue.send_notification(b"one\n".to_vec()));
        assert!(queue.send_notification(b"two\n".to_vec()));
        assert!(
            queue.send_notification(b"three\n".to_vec()),
            "drop-on-full returns true"
        );
        let first = rx.recv().expect("first payload");
        assert_eq!(first, b"one\n");
        assert!(queue.send_notification(b"four\n".to_vec()));
        drop(queue);
        let remaining: Vec<Vec<u8>> = rx.iter().collect();
        assert!(!remaining.is_empty());
    }

    #[test]
    fn outbound_tool_response_waits_for_queue_space_instead_of_disappearing() {
        let (queue, rx) = super::super::server::outbound_for_test_with_bound(1);
        assert!(queue.send_notification(b"notification\n".to_vec()));
        let response_queue = queue.clone();
        let response =
            std::thread::spawn(move || response_queue.send_response(b"response\n".to_vec()));

        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(
            !response.is_finished(),
            "a full notification queue silently discarded a tool response"
        );
        assert_eq!(rx.recv().unwrap(), b"notification\n");
        assert!(response.join().unwrap());
        assert_eq!(rx.recv().unwrap(), b"response\n");
    }
}
