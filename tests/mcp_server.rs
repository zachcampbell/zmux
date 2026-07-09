// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

//! End-to-end tests for the in-daemon MCP server.
//!
//! These spin up a real `zmux serve <name>` subprocess so the test
//! exercises the actual listener-thread bind, per-connection
//! parsing, request channel, and main-loop drain. Tests that only
//! validate the JSON-RPC dispatch shape live in `src/mcp.rs` (unit
//! tests) — those don't need a subprocess and run instantly.
//!
//! Each test gets a unique session name so parallel `cargo test`
//! runs don't clobber each other on the shared `$TMPDIR/zmux-$USER`
//! directory.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

mod support;
use support::{
    TestSession, client_socket_path, mcp_socket_path, spawn_serve, unique_name, wait_for_socket,
};

const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Connect to the MCP socket and set sensible read/write timeouts so
/// a hung server can't block the test runner forever.
fn connect_mcp(name: &str) -> UnixStream {
    let path = mcp_socket_path(name);
    support::wait_for_socket(&path);
    let stream = UnixStream::connect(&path).expect("connect to MCP socket");
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .expect("set read timeout");
    stream
        .set_write_timeout(Some(READ_TIMEOUT))
        .expect("set write timeout");
    stream
}

/// Send one JSON-RPC request and read one JSON-RPC response. Used
/// when the test only cares about a single round-trip.
fn round_trip(name: &str, request: Value) -> Value {
    let stream = connect_mcp(name);
    let mut writer = stream.try_clone().expect("clone stream");
    let mut reader = BufReader::new(stream);
    let mut bytes = serde_json::to_vec(&request).expect("serialize request");
    bytes.push(b'\n');
    writer.write_all(&bytes).expect("write request");
    writer.flush().expect("flush request");
    let mut line = String::new();
    reader.read_line(&mut line).expect("read response line");
    serde_json::from_str(&line).expect("parse JSON-RPC response")
}

#[test]
fn initialize_returns_protocol_handshake() {
    let name = unique_name("mcp-init");
    let _session = spawn_serve(&name);
    let response = round_trip(&name, json!({"jsonrpc":"2.0","id":1,"method":"initialize"}));
    let result = &response["result"];
    assert_eq!(
        result["protocolVersion"], "2025-06-18",
        "protocol version must match the spec we target"
    );
    assert_eq!(result["serverInfo"]["name"], "zmux");
    assert!(
        result["serverInfo"]["version"].is_string(),
        "version must be present"
    );
    assert!(
        result["capabilities"]["tools"].is_object(),
        "must advertise tools capability"
    );
    assert!(
        result["capabilities"]["resources"].is_object(),
        "must advertise resources capability"
    );
}

/// `resources/list` enumerates `zmux://panes`. The resource mirrors
/// `list_panes` so MCP clients can pull pane state without invoking
/// a tool.
#[test]
fn resources_list_advertises_zmux_panes() {
    let name = unique_name("mcp-rl");
    let _session = spawn_serve(&name);
    let response = round_trip(
        &name,
        json!({"jsonrpc":"2.0","id":1,"method":"resources/list"}),
    );
    let resources = response["result"]["resources"]
        .as_array()
        .expect("resources array");
    let uris: Vec<&str> = resources.iter().filter_map(|r| r["uri"].as_str()).collect();
    assert!(
        uris.contains(&"zmux://panes"),
        "zmux://panes must be advertised; got {uris:?}"
    );
}

/// `resources/read` with `zmux://panes` returns the same JSON the
/// `list_panes` tool would. The text payload is pretty-printed JSON
/// per our envelope conventions.
#[test]
fn resources_read_panes_returns_list_panes_json() {
    let name = unique_name("mcp-rr");
    let _session = spawn_serve(&name);
    let response = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":1,"method":"resources/read",
            "params":{"uri":"zmux://panes"}
        }),
    );
    let contents = response["result"]["contents"]
        .as_array()
        .expect("contents array");
    assert_eq!(contents.len(), 1, "expected one content entry");
    let entry = &contents[0];
    assert_eq!(entry["uri"], "zmux://panes");
    assert_eq!(entry["mimeType"], "application/json");
    let text = entry["text"].as_str().expect("text string");
    let parsed: Value = serde_json::from_str(text).expect("parse panes JSON");
    let arr = parsed["panes"].as_array().expect("`panes` array");
    assert_eq!(arr.len(), 1, "fresh session has the one genesis pane");
    assert_eq!(arr[0]["pane_id"], 1);
}

/// Unknown URIs are rejected with a JSON-RPC `invalid_params` error so
/// future additions are caught at the wire boundary instead of being
/// silently ignored.
#[test]
fn resources_read_unknown_uri_returns_invalid_params() {
    let name = unique_name("mcp-rr-bad");
    let _session = spawn_serve(&name);
    let response = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":1,"method":"resources/read",
            "params":{"uri":"zmux://nope"}
        }),
    );
    assert_eq!(response["error"]["code"], -32602);
}

#[test]
fn tools_list_advertises_list_panes() {
    let name = unique_name("mcp-tools");
    let _session = spawn_serve(&name);
    let response = round_trip(&name, json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}));
    let tools = response["result"]["tools"].as_array().expect("tools array");
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(
        names.contains(&"list_panes"),
        "list_panes must be advertised; got {names:?}"
    );
}

#[test]
fn list_panes_returns_genesis_pane_summary() {
    let name = unique_name("mcp-list");
    let _session = spawn_serve(&name);
    let response = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"list_panes","arguments":{}}
        }),
    );
    assert_eq!(response["result"]["isError"], false);
    // structuredContent carries the typed payload per MCP 2025-06-18;
    // content[0].text is just the human-readable pretty-printed mirror.
    // MCP 2025-06-18 requires structuredContent to be an object, so
    // list_panes wraps the row array under `panes`.
    let structured = &response["result"]["structuredContent"];
    assert!(
        structured.is_object(),
        "structuredContent must be an object: {structured}",
    );
    let arr = structured["panes"].as_array().expect("`panes` is an array");
    assert_eq!(arr.len(), 1, "fresh session has exactly one genesis pane");
    let pane = &arr[0];
    assert_eq!(pane["pane_id"], 1, "genesis pane id is 1");
    // size_cols / size_rows defaults to the daemon's startup PtySize
    // (24x80). Don't assert exact dims because the daemon resizes to
    // attached clients; we only check that the fields are present
    // and non-zero.
    assert!(
        pane["size_cols"].as_u64().unwrap_or(0) > 0,
        "size_cols must be reported"
    );
    assert!(
        pane["size_rows"].as_u64().unwrap_or(0) > 0,
        "size_rows must be reported"
    );
}

#[test]
fn spawn_pane_creates_a_new_pane_visible_in_list_panes() {
    let name = unique_name("mcp-spawn");
    let _session = spawn_serve(&name);
    // Spawn a long-running shell so list_panes definitely sees it
    // before the daemon reaps the exit. We rely on the daemon's
    // 20ms poll to drain MCP requests; give the spawn a beat to
    // settle into the workspace before we list.
    let spawn = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{
                "name":"spawn_pane",
                "arguments":{"command":"sleep 30","label":"sleeper"}
            }
        }),
    );
    assert_eq!(
        spawn["result"]["isError"], false,
        "spawn_pane should succeed in a 24x80 workspace; got {spawn}"
    );
    let new_pane_id = spawn["result"]["structuredContent"]["pane_id"]
        .as_u64()
        .expect("pane_id u64") as u32;
    assert!(
        new_pane_id >= 2,
        "new pane id should be >= 2 (genesis is 1)"
    );

    // Brief settle so the workspace publishes its layout updates.
    thread::sleep(Duration::from_millis(150));

    let list = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"list_panes","arguments":{}}
        }),
    );
    let arr = list["result"]["structuredContent"]["panes"]
        .as_array()
        .expect("`panes` array");
    let new_pane = arr
        .iter()
        .find(|p| p["pane_id"].as_u64() == Some(u64::from(new_pane_id)))
        .unwrap_or_else(|| {
            panic!(
                "new pane {new_pane_id} not in list: {}",
                list["result"]["structuredContent"]
            )
        });
    assert_eq!(new_pane["label"].as_str(), Some("sleeper"));
}

#[test]
fn spawn_pane_with_wait_for_idle_returns_text_in_response() {
    // Regression: spawn_pane wait_for_idle defers the response until
    // the pane has gone Working then settled (Idle/AwaitingInput) or
    // errored/exited. The reply must include the rendered text so the
    // caller skips the follow-up read_pane round trip.
    let name = unique_name("mcp-spawn-wait");
    let _session = spawn_serve(&name);

    // printf produces a known marker, then sleep keeps the pane alive
    // past the idle threshold (DEFAULT_IDLE_THRESHOLD = 750ms). The
    // pane goes Working when printf writes, then Idle after the quiet
    // window. wait_for_idle should fire at that transition, well
    // before max_wait_ms.
    let response = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"spawn_pane","arguments":{
                "command": "printf 'spawn-and-wait OK\\n'; sleep 5",
                "wait_for_idle": true,
                "max_wait_ms": 4000,
            }}
        }),
    );
    assert_eq!(
        response["result"]["isError"], false,
        "spawn-and-wait should succeed: {response}"
    );
    let result = &response["result"]["structuredContent"];
    assert!(
        result["pane_id"].is_u64(),
        "pane_id missing or wrong type: {result}"
    );
    let text = result["text"]
        .as_str()
        .expect("text field present and string");
    assert!(
        text.contains("spawn-and-wait OK"),
        "text should contain printf output, got {text:?}"
    );
    let state = result["state"].as_str().expect("state field");
    assert!(
        matches!(state, "Idle" | "AwaitingInput"),
        "state should be settled; got {state}"
    );
    assert_eq!(
        result["timed_out"], false,
        "should have settled before deadline"
    );
}

#[test]
fn kill_pane_removes_pane_from_list_panes() {
    let name = unique_name("mcp-kill");
    let _session = spawn_serve(&name);
    // Spawn a long-lived pane we can kill cleanly.
    let spawn = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"spawn_pane","arguments":{"command":"sleep 30"}}
        }),
    );
    assert_eq!(spawn["result"]["isError"], false, "spawn: {spawn}");
    let pane_id = spawn["result"]["structuredContent"]["pane_id"]
        .as_u64()
        .unwrap() as u32;

    thread::sleep(Duration::from_millis(150));

    let kill = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"kill_pane","arguments":{"pane_id": pane_id}}
        }),
    );
    assert_eq!(kill["result"]["isError"], false, "kill: {kill}");

    // Brief settle, then list — the killed pane must be gone.
    thread::sleep(Duration::from_millis(150));
    let list = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"list_panes","arguments":{}}
        }),
    );
    let panes = list["result"]["structuredContent"]["panes"]
        .as_array()
        .expect("`panes` array");
    assert!(
        !panes
            .iter()
            .any(|p| { p["pane_id"].as_u64() == Some(u64::from(pane_id)) }),
        "pane {pane_id} should be gone after kill_pane; got {}",
        list["result"]["structuredContent"],
    );
}

#[test]
fn kill_pane_closes_single_pane_worker_window() {
    let name = unique_name("mcp-kill-worker-window");
    let _session = spawn_serve(&name);
    let spawn = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"spawn_pane","arguments":{
                "command":"sleep 30", "split":"window", "label":"worker-cleanup"
            }}
        }),
    );
    assert_eq!(spawn["result"]["isError"], false, "spawn: {spawn}");
    let pane_id = spawn["result"]["structuredContent"]["pane_id"]
        .as_u64()
        .unwrap() as u32;
    assert_ne!(pane_id, 1, "worker window pane id should be session-unique");

    let kill = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"kill_pane","arguments":{"pane_id": pane_id}}
        }),
    );
    assert_eq!(kill["result"]["isError"], false, "kill: {kill}");

    thread::sleep(Duration::from_millis(150));
    let list = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"list_panes","arguments":{}}
        }),
    );
    let panes = list["result"]["structuredContent"]["panes"]
        .as_array()
        .expect("`panes` array");
    assert_eq!(panes.len(), 1, "worker window should be gone: {list}");
    assert_eq!(panes[0]["pane_id"], 1, "genesis pane should remain: {list}");
    assert_eq!(
        panes[0]["window_index"], 0,
        "remaining window should be reindexed: {list}"
    );
    assert_eq!(
        panes[0]["active_window"], true,
        "remaining window should be active: {list}"
    );
}

#[test]
fn kill_pane_refuses_last_pane_in_window() {
    let name = unique_name("mcp-kill-last");
    let _session = spawn_serve(&name);
    // Genesis pane id 1 is the only pane in the only window.
    let response = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"kill_pane","arguments":{"pane_id":1}}
        }),
    );
    assert_eq!(response["result"]["isError"], true);
    let text = response["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("last remaining pane"), "text: {text}");
}

#[test]
fn set_label_updates_list_panes_label() {
    let name = unique_name("mcp-label");
    let _session = spawn_serve(&name);
    let spawn = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"spawn_pane","arguments":{"command":"sleep 30"}}
        }),
    );
    assert_eq!(spawn["result"]["isError"], false, "spawn: {spawn}");
    let pane_id = spawn["result"]["structuredContent"]["pane_id"]
        .as_u64()
        .unwrap() as u32;

    let label = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"set_label","arguments":{
                "pane_id": pane_id, "label": "frontend"
            }}
        }),
    );
    assert_eq!(label["result"]["isError"], false, "set_label: {label}");

    let list = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"list_panes","arguments":{}}
        }),
    );
    let panes = list["result"]["structuredContent"]["panes"]
        .as_array()
        .expect("`panes` array");
    let pane = panes
        .iter()
        .find(|p| p["pane_id"].as_u64() == Some(u64::from(pane_id)))
        .unwrap();
    assert_eq!(pane["label"].as_str(), Some("frontend"));
}

#[test]
fn set_label_unknown_pane_returns_tool_error() {
    let name = unique_name("mcp-label-bad");
    let _session = spawn_serve(&name);
    let response = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"set_label","arguments":{
                "pane_id": 9999, "label": "x"
            }}
        }),
    );
    assert_eq!(response["result"]["isError"], true);
}

#[test]
fn read_pane_visible_returns_recent_output_with_strip_ansi() {
    let name = unique_name("mcp-read");
    let _session = spawn_serve(&name);
    // Print a colored line then sleep so the output settles in the
    // pane's viewport before we read it.
    let spawn = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"spawn_pane","arguments":{
                "command": "printf '\\033[31mred\\033[0m line\\n'; sleep 5"
            }}
        }),
    );
    assert_eq!(spawn["result"]["isError"], false, "spawn: {spawn}");
    let pane_id = spawn["result"]["structuredContent"]["pane_id"]
        .as_u64()
        .unwrap() as u32;

    // Wait for the print to actually land in the pane's viewport.
    // The daemon polls at ~20ms; printf is fast; 250ms gives plenty
    // of margin without making the test feel slow.
    thread::sleep(Duration::from_millis(250));

    let read = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"read_pane","arguments":{
                "pane_id": pane_id, "strip_ansi": true, "lines": 5
            }}
        }),
    );
    assert_eq!(read["result"]["isError"], false, "read: {read}");
    let payload = &read["result"]["structuredContent"];
    let pane_text = payload["text"].as_str().expect("text field");
    // The pane stores rendered cells, so the colored sequence is
    // already gone by the time strip_ansi runs — but the visible
    // chars must still be present.
    assert!(
        pane_text.contains("red") && pane_text.contains("line"),
        "expected `red` and `line` in pane text, got: {pane_text:?}"
    );
    // Fresh pane that hasn't been scrolled is at the bottom.
    assert_eq!(
        payload["cursor_at_bottom"], true,
        "fresh pane viewport must follow output"
    );
}

#[test]
fn read_pane_strip_ansi_false_returns_styled_sgr_text() {
    let name = unique_name("mcp-read-styled");
    let _session = spawn_serve(&name);
    let spawn = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"spawn_pane","arguments":{
                "command": "printf '\\033[31mred\\033[0m line\\n'; sleep 5"
            }}
        }),
    );
    assert_eq!(spawn["result"]["isError"], false, "spawn: {spawn}");
    let pane_id = spawn["result"]["structuredContent"]["pane_id"]
        .as_u64()
        .unwrap() as u32;

    // Wait for the print to actually land in the pane's viewport.
    thread::sleep(Duration::from_millis(250));

    // strip_ansi=false (the default) must reflect the real cell
    // styling — a red foreground on "red" — as SGR escapes, not a
    // plain-text passthrough.
    let styled = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"read_pane","arguments":{
                "pane_id": pane_id, "strip_ansi": false, "lines": 5
            }}
        }),
    );
    assert_eq!(styled["result"]["isError"], false, "read: {styled}");
    let styled_text = styled["result"]["structuredContent"]["text"]
        .as_str()
        .expect("text field");
    assert!(
        styled_text.contains("\x1b[31m"),
        "expected red SGR escape (\\x1b[31m) in styled text, got: {styled_text:?}"
    );
    assert!(
        styled_text.contains("red") && styled_text.contains("line"),
        "expected visible chars alongside styling, got: {styled_text:?}"
    );

    // strip_ansi=true must give back plain text with no escape bytes
    // at all, even though the same cells are styled underneath.
    let plain = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"read_pane","arguments":{
                "pane_id": pane_id, "strip_ansi": true, "lines": 5
            }}
        }),
    );
    assert_eq!(plain["result"]["isError"], false, "read: {plain}");
    let plain_text = plain["result"]["structuredContent"]["text"]
        .as_str()
        .expect("text field");
    assert!(
        !plain_text.contains('\u{1b}'),
        "expected no escape bytes in stripped text, got: {plain_text:?}"
    );
    assert!(
        plain_text.contains("red") && plain_text.contains("line"),
        "expected visible chars to survive stripping, got: {plain_text:?}"
    );
}

#[test]
fn read_pane_scrollback_strip_ansi_false_returns_styled_sgr_text() {
    // Scrollback storage already keeps styled `Cell`s (see
    // scrollback.rs / pane.rs), so mode="scrollback" gets the same
    // SGR-serialization treatment as mode="visible" — no raw-byte
    // plumbing needed. Ask for far more lines than the viewport holds
    // so the composition path pulls the full grid (plus whatever
    // scrollback history exists) rather than just a tail slice.
    let name = unique_name("mcp-read-scroll-styled");
    let _session = spawn_serve(&name);
    let spawn = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"spawn_pane","arguments":{
                "command": "printf '\\033[31mred\\033[0m line\\n'; sleep 5"
            }}
        }),
    );
    assert_eq!(spawn["result"]["isError"], false, "spawn: {spawn}");
    let pane_id = spawn["result"]["structuredContent"]["pane_id"]
        .as_u64()
        .unwrap() as u32;

    thread::sleep(Duration::from_millis(250));

    let styled = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"read_pane","arguments":{
                "pane_id": pane_id, "mode": "scrollback", "strip_ansi": false, "lines": 500
            }}
        }),
    );
    assert_eq!(styled["result"]["isError"], false, "read: {styled}");
    let styled_text = styled["result"]["structuredContent"]["text"]
        .as_str()
        .expect("text field");
    assert!(
        styled_text.contains("\x1b[31m"),
        "expected red SGR escape (\\x1b[31m) in styled scrollback text, got: {styled_text:?}"
    );
    assert!(
        styled_text.contains("red") && styled_text.contains("line"),
        "expected visible chars alongside styling, got: {styled_text:?}"
    );

    let plain = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"read_pane","arguments":{
                "pane_id": pane_id, "mode": "scrollback", "strip_ansi": true, "lines": 500
            }}
        }),
    );
    assert_eq!(plain["result"]["isError"], false, "read: {plain}");
    let plain_text = plain["result"]["structuredContent"]["text"]
        .as_str()
        .expect("text field");
    assert!(
        !plain_text.contains('\u{1b}'),
        "expected no escape bytes in stripped scrollback text, got: {plain_text:?}"
    );
}

#[test]
fn read_pane_output_returns_cursor_based_raw_transcript() {
    let name = unique_name("mcp-read-output");
    let _session = spawn_serve(&name);
    let spawn = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"spawn_pane","arguments":{
                "command":"bash -i", "split":"window"
            }}
        }),
    );
    assert_eq!(spawn["result"]["isError"], false, "spawn: {spawn}");
    let pane_id = spawn["result"]["structuredContent"]["pane_id"]
        .as_u64()
        .unwrap() as u32;

    thread::sleep(Duration::from_millis(300));
    let cursor = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"read_pane_output","arguments":{
                "pane_id": pane_id, "max_bytes": 0
            }}
        }),
    );
    assert_eq!(cursor["result"]["isError"], false, "cursor: {cursor}");
    let start = cursor["result"]["structuredContent"]["byte_cursor"]
        .as_u64()
        .expect("byte_cursor");

    let send = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"send_keys","arguments":{
                "pane_id": pane_id,
                "keys": "printf 'ZMUX_RAW_OUTPUT_OK\n'",
                "enter": true,
                "clear_input": true,
                "expect_text": "ZMUX_RAW_OUTPUT_OK",
                "max_wait_ms": 4000,
                "wait_lines": 80
            }}
        }),
    );
    assert_eq!(send["result"]["isError"], false, "send_keys: {send}");

    let output = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":4,"method":"tools/call",
            "params":{"name":"read_pane_output","arguments":{
                "pane_id": pane_id,
                "since_byte": start,
                "max_bytes": 4096,
                "strip_ansi": true
            }}
        }),
    );
    assert_eq!(output["result"]["isError"], false, "output: {output}");
    let payload = &output["result"]["structuredContent"];
    assert_eq!(payload["start_byte"].as_u64(), Some(start));
    assert!(
        payload["byte_cursor"].as_u64().unwrap() > start,
        "cursor should advance: {output}"
    );
    assert_eq!(payload["truncated"], false);
    let text = payload["text"].as_str().expect("text field");
    assert!(
        text.contains("ZMUX_RAW_OUTPUT_OK"),
        "raw output transcript should include command output, got {text:?}",
    );
}

#[test]
fn read_pane_output_unknown_pane_returns_tool_error() {
    let name = unique_name("mcp-read-output-bad");
    let _session = spawn_serve(&name);
    let response = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"read_pane_output","arguments":{"pane_id":9999}}
        }),
    );
    assert_eq!(response["result"]["isError"], true);
}

#[test]
fn read_pane_unknown_pane_returns_tool_error() {
    let name = unique_name("mcp-read-bad");
    let _session = spawn_serve(&name);
    let response = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"read_pane","arguments":{"pane_id":9999}}
        }),
    );
    assert_eq!(response["result"]["isError"], true);
}

#[test]
fn send_keys_writes_bytes_to_target_pane_pty() {
    let name = unique_name("mcp-keys");
    let _session = spawn_serve(&name);
    // Use a unique tempfile per run so parallel `cargo test`
    // invocations don't collide.
    let outfile = std::env::temp_dir().join(format!("zmux-mcp-out-{}", unique_name("k")));
    // Belt-and-suspenders: make sure the file isn't lying around
    // from a previous failed run.
    let _ = std::fs::remove_file(&outfile);
    let outfile_str = outfile.display().to_string();
    // `cat > FILE` reads from stdin until EOF (Ctrl-D); we send a
    // line of text + newline + EOF, then check the file holds it.
    let spawn_cmd = format!("cat > {outfile_str}; echo done");

    let spawn = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"spawn_pane","arguments":{"command":spawn_cmd}}
        }),
    );
    assert_eq!(
        spawn["result"]["isError"], false,
        "spawn must succeed: {spawn}"
    );
    let pane_id = spawn["result"]["structuredContent"]["pane_id"]
        .as_u64()
        .unwrap() as u32;

    // Give cat a beat to actually start reading from stdin.
    thread::sleep(Duration::from_millis(200));

    let send1 = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"send_keys","arguments":{
                "pane_id": pane_id, "keys": "hello", "enter": true
            }}
        }),
    );
    assert_eq!(
        send1["result"]["isError"], false,
        "send_keys hello: {send1}"
    );
    let send2 = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"send_keys","arguments":{
                "pane_id": pane_id, "keys": "\u{0004}", "enter": false
            }}
        }),
    );
    assert_eq!(
        send2["result"]["isError"], false,
        "send_keys ctrl-d: {send2}"
    );

    // Wait up to ~1.5s for cat to flush + close. PTY writes are
    // asynchronous — we can't observe the shell's flush directly
    // from here, only via the file.
    let deadline = Instant::now() + Duration::from_millis(1500);
    let mut contents = String::new();
    while Instant::now() < deadline {
        if let Ok(read) = std::fs::read_to_string(&outfile)
            && read.contains("hello")
        {
            contents = read;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    let _ = std::fs::remove_file(&outfile);

    assert!(
        contents.contains("hello"),
        "expected `hello` in {}, got {contents:?}",
        outfile.display()
    );
}

#[test]
fn send_keys_short_wait_budget_still_delivers_deferred_enter() {
    let name = unique_name("mcp-keys-short-wait");
    let _session = spawn_serve(&name);
    let outfile =
        std::env::temp_dir().join(format!("zmux-mcp-short-wait-{}", unique_name("enter")));
    let _ = std::fs::remove_file(&outfile);
    let command = format!(
        "IFS= read -r line && printf '%s' \"$line\" > {}",
        outfile.display()
    );
    let spawn = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"spawn_pane","arguments":{"command":command}}
        }),
    );
    let pane_id = spawn["result"]["structuredContent"]["pane_id"]
        .as_u64()
        .expect("spawned pane id") as u32;
    thread::sleep(Duration::from_millis(100));

    let send = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"send_keys","arguments":{
                "pane_id":pane_id,
                "keys":"hello",
                "enter":true,
                "wait_for_idle":true,
                "max_wait_ms":1
            }}
        }),
    );
    assert_eq!(send["result"]["isError"], false, "send_keys: {send}");

    let deadline = Instant::now() + Duration::from_secs(1);
    let contents = loop {
        if let Ok(contents) = std::fs::read_to_string(&outfile) {
            break contents;
        }
        assert!(
            Instant::now() < deadline,
            "deferred Enter was not delivered before the short wait completed"
        );
        thread::sleep(Duration::from_millis(20));
    };
    let _ = std::fs::remove_file(&outfile);
    assert_eq!(contents, "hello");
}

#[test]
fn send_keys_wait_for_idle_returns_settled_output_for_window_pane() {
    let name = unique_name("mcp-keys-wait");
    let _session = spawn_serve(&name);
    let spawn = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"spawn_pane","arguments":{
                "command":"bash -i", "split":"window"
            }}
        }),
    );
    assert_eq!(spawn["result"]["isError"], false, "spawn: {spawn}");
    let pane_id = spawn["result"]["structuredContent"]["pane_id"]
        .as_u64()
        .unwrap() as u32;
    assert_ne!(pane_id, 1, "window pane id must not collide with pane 1");

    thread::sleep(Duration::from_millis(300));
    let stale = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"send_keys","arguments":{
                "pane_id": pane_id,
                "keys": "garbage ",
                "enter": false
            }}
        }),
    );
    assert_eq!(stale["result"]["isError"], false, "stale input: {stale}");

    let send = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"send_keys","arguments":{
                "pane_id": pane_id,
                "keys": "printf 'ZMUX_WAIT_OK\n'",
                "enter": true,
                "clear_input": true,
                "expect_text": "ZMUX_WAIT_OK",
                "max_wait_ms": 4000,
                "wait_lines": 50
            }}
        }),
    );
    assert_eq!(send["result"]["isError"], false, "send_keys: {send}");
    let payload = &send["result"]["structuredContent"];
    assert_eq!(payload["ok"], true);
    assert_eq!(payload["timed_out"], false, "send_keys timed out: {send}");
    assert_eq!(
        payload["matched_expect"], true,
        "sentinel not matched: {send}"
    );
    let text = payload["text"].as_str().expect("text field");
    assert!(
        text.contains("ZMUX_WAIT_OK"),
        "wait response should include command output, got {text:?}",
    );
}

#[test]
fn wait_pane_observes_output_without_sending_input() {
    let name = unique_name("mcp-wait-pane");
    let _session = spawn_serve(&name);
    let spawn = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"spawn_pane","arguments":{
                "command":"bash -i", "split":"window"
            }}
        }),
    );
    assert_eq!(spawn["result"]["isError"], false, "spawn: {spawn}");
    let pane_id = spawn["result"]["structuredContent"]["pane_id"]
        .as_u64()
        .unwrap() as u32;

    thread::sleep(Duration::from_millis(300));
    let send = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"send_keys","arguments":{
                "pane_id": pane_id,
                "keys": "sleep 0.2; printf 'ZMUX_WAIT_PANE_OK\n'",
                "enter": true,
                "clear_input": true
            }}
        }),
    );
    assert_eq!(send["result"]["isError"], false, "send_keys: {send}");

    let wait = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"wait_pane","arguments":{
                "pane_id": pane_id,
                "expect_text": "ZMUX_WAIT_PANE_OK",
                "max_wait_ms": 4000,
                "wait_lines": 80
            }}
        }),
    );
    assert_eq!(wait["result"]["isError"], false, "wait_pane: {wait}");
    let payload = &wait["result"]["structuredContent"];
    assert_eq!(payload["ok"], true);
    assert_eq!(payload["timed_out"], false, "wait_pane timed out: {wait}");
    assert_eq!(
        payload["matched_expect"], true,
        "sentinel not matched: {wait}"
    );
    let text = payload["text"].as_str().expect("text field");
    assert!(
        text.contains("ZMUX_WAIT_PANE_OK"),
        "wait response should include command output, got {text:?}",
    );
}

#[test]
fn send_keys_wraps_in_bracketed_paste_when_shell_enables_2004() {
    // When the shell has DECSET 2004 active, send_keys wraps typed text
    // in `\x1b[200~ ... \x1b[201~` and writes the CR in the SAME PTY
    // write. The close marker unambiguously ends paste mode, so the
    // trailing CR is interpreted as a fresh keystroke without the 75ms
    // gap the unbracketed path needs. This test verifies the wire bytes
    // by having the shell echo them into a file via cat.
    let name = unique_name("mcp-keys-bp");
    let _session = spawn_serve(&name);
    let outfile = std::env::temp_dir().join(format!("zmux-mcp-out-{}", unique_name("kbp")));
    let _ = std::fs::remove_file(&outfile);
    let outfile_str = outfile.display().to_string();
    // Emit DECSET 2004 to the daemon's terminal ingest (it parses
    // master-side output and tracks state on the pane), then run cat
    // to capture every byte we write to its stdin. /bin/sh is dash,
    // whose printf doesn't grok \xHH — use the POSIX octal form.
    let spawn_cmd = format!("printf '\\033[?2004h' && cat > {outfile_str}; echo done");

    let spawn = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"spawn_pane","arguments":{"command":spawn_cmd}}
        }),
    );
    assert_eq!(
        spawn["result"]["isError"], false,
        "spawn must succeed: {spawn}"
    );
    let pane_id = spawn["result"]["structuredContent"]["pane_id"]
        .as_u64()
        .unwrap() as u32;

    // Give the shell time to print DECSET 2004 (which the daemon must
    // ingest before send_keys queries pane_bracketed_paste) and exec
    // cat. The plain send_keys test gets away with 200ms because it
    // doesn't depend on terminal state being up-to-date; we do.
    thread::sleep(Duration::from_millis(800));

    let send = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"send_keys","arguments":{
                "pane_id": pane_id, "keys": "hello", "enter": true
            }}
        }),
    );
    assert_eq!(send["result"]["isError"], false, "send_keys: {send}");

    // EOF cat so the file flushes.
    let _ = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"send_keys","arguments":{
                "pane_id": pane_id, "keys": "\u{0004}", "enter": false
            }}
        }),
    );

    let deadline = Instant::now() + Duration::from_millis(1500);
    let mut contents: Vec<u8> = Vec::new();
    let expected: &[u8] = b"\x1b[200~hello\x1b[201~";
    while Instant::now() < deadline {
        if let Ok(bytes) = std::fs::read(&outfile)
            && bytes.windows(expected.len()).any(|w| w == expected)
        {
            contents = bytes;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    let _ = std::fs::remove_file(&outfile);

    assert!(
        contents.windows(expected.len()).any(|w| w == expected),
        "expected bracketed-paste envelope `\\x1b[200~hello\\x1b[201~` in file bytes; got {contents:?}",
    );
}

#[test]
fn send_keys_unknown_pane_returns_tool_error() {
    let name = unique_name("mcp-keys-bad");
    let _session = spawn_serve(&name);
    let response = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"send_keys","arguments":{"pane_id":9999,"keys":"x"}}
        }),
    );
    // The dispatch succeeds (valid args), so we get a tools/call
    // result with isError=true rather than a JSON-RPC -32602.
    assert_eq!(response["result"]["isError"], true);
    let text = response["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("send_keys failed"), "text was: {text}");
}

#[test]
fn spawn_pane_window_split_creates_new_window_and_returns_pane_id() {
    let name = unique_name("mcp-spawn-win");
    let _session = spawn_serve(&name);
    // split="window" creates a brand-new window whose genesis pane
    // runs the command. WindowSet hands back a session-unique pane id
    // so MCP clients can address it without colliding with window 0.
    let spawn = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"spawn_pane","arguments":{
                "command":"sleep 30", "split":"window"
            }}
        }),
    );
    assert_eq!(spawn["result"]["isError"], false, "spawn: {spawn}");
    let spawn_payload = &spawn["result"]["structuredContent"];
    assert_ne!(
        spawn_payload["pane_id"], 1,
        "fresh window's genesis pane id should not collide with the original pane"
    );

    // Programmatic window spawns must NOT steal focus (tmux
    // `new-window -d` semantics): the worker lands at window_index 1
    // in the background and the original window stays active.
    thread::sleep(Duration::from_millis(150));
    let list = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"list_panes","arguments":{}}
        }),
    );
    let arr = list["result"]["structuredContent"]["panes"]
        .as_array()
        .expect("`panes` array");
    assert_eq!(arr.len(), 2, "session should expose both windows");
    assert!(
        arr.iter()
            .any(|pane| pane["pane_id"] == spawn_payload["pane_id"]
                && pane["active_window"] == false
                && pane["window_index"] == 1),
        "worker window should exist in the background: {}",
        list["result"]["structuredContent"],
    );
    assert!(
        arr.iter()
            .any(|pane| pane["pane_id"] == 1 && pane["active_window"] == true),
        "original window must keep focus after an MCP window spawn: {}",
        list["result"]["structuredContent"],
    );
}

/// `target_pane` is meaningless when split=window (the new pane is
/// allocated by the new window) — surface that as a tool-level error
/// rather than silently ignoring the argument.
#[test]
fn spawn_pane_window_with_target_pane_returns_tool_error() {
    let name = unique_name("mcp-spawn-win-bad");
    let _session = spawn_serve(&name);
    let response = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"spawn_pane","arguments":{
                "command":"true","split":"window","target_pane":1
            }}
        }),
    );
    assert_eq!(response["result"]["isError"], true);
}

#[test]
fn spawn_pane_rejects_unknown_split_value() {
    let name = unique_name("mcp-bad-split");
    let _session = spawn_serve(&name);
    let response = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{
                "name":"spawn_pane",
                "arguments":{"command":"true","split":"diagonal"}
            }
        }),
    );
    assert_eq!(response["error"]["code"], -32602);
}

#[test]
fn parse_error_returns_minus_32700_with_null_id() {
    let name = unique_name("mcp-parse");
    let _session = spawn_serve(&name);
    let stream = connect_mcp(&name);
    let mut writer = stream.try_clone().expect("clone");
    let mut reader = BufReader::new(stream);
    writer.write_all(b"not json\n").expect("write");
    writer.flush().expect("flush");
    let mut line = String::new();
    reader.read_line(&mut line).expect("read");
    let response: Value = serde_json::from_str(&line).expect("parse");
    assert_eq!(response["error"]["code"], -32700);
    assert!(response["id"].is_null());
}

/// `watch_events` opens a notification subscription on the per-conn
/// socket. The server must reply to the original tools/call with a
/// subscription_active envelope, then stream JSON-RPC notifications
/// (`method = "zmux/event"`) for every event the workspace publishes.
/// Spawning a pane after the subscription should produce at least
/// PaneSpawned and PaneStateChanged notifications.
#[test]
fn watch_events_streams_pane_lifecycle_notifications() {
    let name = unique_name("mcp-watch");
    let _session = spawn_serve(&name);
    let stream = connect_mcp(&name);
    let mut writer = stream.try_clone().expect("clone");
    let mut reader = BufReader::new(stream);

    // Subscribe.
    let req = json!({
        "jsonrpc":"2.0","id":1,"method":"tools/call",
        "params":{"name":"watch_events","arguments":{}}
    });
    let mut bytes = serde_json::to_vec(&req).expect("serialize");
    bytes.push(b'\n');
    writer.write_all(&bytes).expect("write subscribe");
    writer.flush().expect("flush");
    let mut line = String::new();
    reader.read_line(&mut line).expect("read subscribe ack");
    let ack: Value = serde_json::from_str(&line).expect("parse ack");
    assert_eq!(ack["id"], 1, "subscription ack must echo id 1");
    assert_eq!(ack["result"]["isError"], false, "ack: {ack}");
    assert_eq!(
        ack["result"]["structuredContent"]["subscription_active"], true,
        "watch_events must reply subscription_active=true"
    );

    // Drive a workspace event by spawning a pane on a different
    // connection — using the same socket would block the read loop.
    // Use a new window to verify watch_events observes the whole
    // session, not only the workspace that was active at subscription
    // time.
    let spawn = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":99,"method":"tools/call",
            "params":{"name":"spawn_pane","arguments":{
                "command":"sleep 30","label":"watcher","split":"window"
            }}
        }),
    );
    assert_eq!(spawn["result"]["isError"], false, "spawn: {spawn}");

    // Drain notifications for up to ~2s and assert a PaneSpawned
    // arrived. Other variants (StateChanged, Output) may also come
    // through; we only require PaneSpawned because it's the deterministic
    // one.
    let pane_id = spawn["result"]["structuredContent"]["pane_id"]
        .as_u64()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut saw_spawn = false;
    while Instant::now() < deadline {
        let mut buf = String::new();
        match reader.read_line(&mut buf) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        let v: Value = match serde_json::from_str(&buf) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v["method"] == "zmux/event"
            && v["params"]["kind"] == "PaneSpawned"
            && v["params"]["data"]["pane_id"] == pane_id
        {
            assert!(pane_id >= 2, "PaneSpawned pane_id should be >=2; got {v}");
            saw_spawn = true;
            break;
        }
    }
    assert!(saw_spawn, "expected a PaneSpawned notification within 2s");

    let kill = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":100,"method":"tools/call",
            "params":{"name":"kill_pane","arguments":{"pane_id": pane_id}}
        }),
    );
    assert_eq!(kill["result"]["isError"], false, "kill: {kill}");

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut saw_closed = false;
    while Instant::now() < deadline {
        let mut buf = String::new();
        match reader.read_line(&mut buf) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        let v: Value = match serde_json::from_str(&buf) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v["method"] == "zmux/event"
            && v["params"]["kind"] == "PaneClosed"
            && v["params"]["data"]["pane_id"] == pane_id
        {
            saw_closed = true;
            break;
        }
    }
    assert!(saw_closed, "expected a PaneClosed notification within 2s");
}

/// Sending more than the per-line cap (1 MiB) without a newline must
/// trip the server's oversize-line guard and close the connection
/// rather than letting a misbehaving local client exhaust memory.
/// We send 2 MiB of `x` bytes (no newline), then issue a small
/// follow-up read — the read returns 0 / EOF, demonstrating the
/// connection was closed by the server.
#[test]
fn oversize_line_closes_connection() {
    let name = unique_name("mcp-oversize");
    let _session = spawn_serve(&name);
    let stream = connect_mcp(&name);
    let mut writer = stream.try_clone().expect("clone");
    let mut reader = BufReader::new(stream);
    // Two MiB > 1 MiB cap. We don't include a newline because the
    // server must not be allowed to wait forever for one. Some bytes
    // may fail to flush mid-write once the server hangs up — that's
    // fine, the assertion is on the read side.
    let payload = vec![b'x'; 2 * 1024 * 1024];
    let _ = writer.write_all(&payload);
    let _ = writer.flush();
    // The server should have closed the connection. read_line returns
    // Ok(0) on EOF; the server may also reset the socket which would
    // surface as an Err. Either outcome proves the cap tripped.
    let mut sink = String::new();
    let result = reader.read_line(&mut sink);
    match result {
        Ok(0) => { /* clean EOF — the server closed cleanly */ }
        Ok(n) => panic!(
            "expected EOF after oversize line; got {n} bytes: {:?}",
            &sink[..sink.len().min(120)]
        ),
        Err(_) => { /* connection reset is also acceptable */ }
    }
}

/// `zmux mcp --session <name>` is the stdio bridge for external MCP
/// clients. We launch it as a subprocess, feed JSON-RPC on stdin, and
/// expect responses back on stdout. The bridge connects to the
/// session's `*.mcp.sock` and pipes both directions until either end
/// closes.
#[test]
fn stdio_bridge_round_trips_initialize_and_tools_list() {
    let name = unique_name("mcp-stdio");
    let mut session = spawn_serve(&name);
    // Wait for the daemon's MCP socket to come up before launching
    // the bridge — otherwise the bridge dies fast with ENOENT.
    wait_for_socket(&mcp_socket_path(&name));

    let exe = env!("CARGO_BIN_EXE_zmux");
    let mut bridge = Command::new(exe)
        .arg("mcp")
        .arg("--session")
        .arg(&name)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn zmux mcp bridge");
    let mut stdin = bridge.stdin.take().expect("bridge stdin");
    let stdout = bridge.stdout.take().expect("bridge stdout");
    let mut reader = BufReader::new(stdout);
    // Adopt the bridge into the session guard now, before any
    // assertions run, so a panic below still reaps both the bridge
    // and the daemon instead of leaking them.
    session.adopt(bridge);

    // Send both requests up front, then close stdin so the bridge's
    // stdin→socket pump sees EOF and the daemon closes the socket
    // after replying. Reading stdout then drains exactly the two
    // responses without us having to coordinate stdin/stdout
    // interleaving on the test side.
    let init = json!({"jsonrpc":"2.0","id":1,"method":"initialize"});
    let mut bytes = serde_json::to_vec(&init).expect("serialize init");
    bytes.push(b'\n');
    stdin.write_all(&bytes).expect("write init");
    let list = json!({"jsonrpc":"2.0","id":2,"method":"tools/list"});
    let mut bytes = serde_json::to_vec(&list).expect("serialize list");
    bytes.push(b'\n');
    stdin.write_all(&bytes).expect("write tools/list");
    stdin.flush().expect("flush requests");
    drop(stdin);

    // First response: initialize
    let mut line = String::new();
    reader.read_line(&mut line).expect("read init reply");
    let v: Value = serde_json::from_str(&line).expect("parse init reply");
    assert_eq!(v["id"], 1, "initialize id must round-trip");
    assert_eq!(v["result"]["protocolVersion"], "2025-06-18");

    // Second response: tools/list
    line.clear();
    reader.read_line(&mut line).expect("read tools/list reply");
    let v: Value = serde_json::from_str(&line).expect("parse tools/list reply");
    assert_eq!(v["id"], 2);
    let tools = v["result"]["tools"].as_array().expect("tools array");
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(
        names.contains(&"list_panes") && names.contains(&"watch_events"),
        "tools/list via stdio bridge must include list_panes + watch_events; got {names:?}"
    );

    // Teardown (both the bridge and the daemon) happens via
    // `session`'s Drop — success or panic.
}

/// MCP clients (Claude Code, Cursor) configure the stdio bridge with
/// just `zmux mcp --session <name>` and expect it to work even on a
/// fresh boot. Verifies the bridge auto-starts a daemon when no MCP
/// socket is present yet, then completes the same initialize+
/// tools/list round-trip the explicit-spawn test exercises.
#[test]
fn stdio_bridge_auto_starts_daemon_when_socket_is_missing() {
    let name = unique_name("mcp-autostart");
    // Belt-and-braces: ensure no leftover socket from a previous run
    // could mask the auto-start path.
    let _ = std::fs::remove_file(client_socket_path(&name));
    let _ = std::fs::remove_file(mcp_socket_path(&name));
    assert!(
        !mcp_socket_path(&name).exists(),
        "precondition: MCP socket must not exist before the bridge runs",
    );
    // We never hold a `Child` for the daemon (the bridge auto-starts
    // it), so the guard tracks it by name only; Drop still tears it
    // down via `zmux::kill_session`.
    let mut session = TestSession::for_name(&name);

    let exe = env!("CARGO_BIN_EXE_zmux");
    let mut bridge = Command::new(exe)
        .arg("mcp")
        .arg("--session")
        .arg(&name)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn zmux mcp bridge");
    let mut stdin = bridge.stdin.take().expect("bridge stdin");
    let stdout = bridge.stdout.take().expect("bridge stdout");
    let mut reader = BufReader::new(stdout);
    // Adopt the bridge before any assertions so a panic below still
    // reaps it (and, via the guard's name-based Drop, the daemon it
    // auto-started).
    session.adopt(bridge);

    let init = json!({"jsonrpc":"2.0","id":1,"method":"initialize"});
    let mut bytes = serde_json::to_vec(&init).expect("serialize init");
    bytes.push(b'\n');
    stdin.write_all(&bytes).expect("write init");
    let list = json!({"jsonrpc":"2.0","id":2,"method":"tools/list"});
    let mut bytes = serde_json::to_vec(&list).expect("serialize list");
    bytes.push(b'\n');
    stdin.write_all(&bytes).expect("write tools/list");
    stdin.flush().expect("flush requests");
    drop(stdin);

    // First response: initialize. If this lands the auto-start worked.
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .expect("read init reply (auto-start path)");
    let v: Value = serde_json::from_str(&line).expect("parse init reply (auto-start path)");
    assert_eq!(v["id"], 1, "initialize id must round-trip");
    assert_eq!(v["result"]["protocolVersion"], "2025-06-18");

    // Second response: tools/list — proves the daemon is fully up.
    line.clear();
    reader
        .read_line(&mut line)
        .expect("read tools/list reply (auto-start path)");
    let v: Value = serde_json::from_str(&line).expect("parse tools/list reply (auto-start path)");
    assert_eq!(v["id"], 2);
    let tools = v["result"]["tools"].as_array().expect("tools array");
    assert!(
        tools.iter().any(|t| t["name"] == "list_panes"),
        "auto-started daemon must advertise list_panes"
    );

    // The auto-started daemon outlives the bridge; both are torn down
    // by `session`'s Drop (which kills the daemon by name since we
    // never held a `Child` for it, and reaps the adopted bridge).
}

/// The dogfood scenario: a long-lived MCP client (Claude Code) is
/// connected through the bridge when the daemon is killed and
/// restarted. Pre-bridge-reconnect this would tear down the client's
/// MCP transport and require a manual `/mcp`. With bridge reconnect,
/// the bridge synthesizes errors for in-flight requests, replays the
/// initialize handshake to the new daemon (with a synthetic id), and
/// the client's subsequent traffic flows through transparently.
#[test]
fn stdio_bridge_recovers_across_daemon_restart() {
    let name = unique_name("mcp-reconnect");
    let mut session = spawn_serve(&name);
    wait_for_socket(&mcp_socket_path(&name));

    let exe = env!("CARGO_BIN_EXE_zmux");
    let mut bridge = Command::new(exe)
        .arg("mcp")
        .arg("--session")
        .arg(&name)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn zmux mcp bridge");
    let mut stdin = bridge.stdin.take().expect("bridge stdin");
    let stdout = bridge.stdout.take().expect("bridge stdout");
    let mut reader = BufReader::new(stdout);
    // Adopt the bridge now so a panic anywhere below still reaps it
    // alongside whichever daemon (A or B) the guard is currently
    // tracking.
    session.adopt(bridge);

    // Phase 1: complete the MCP handshake against daemon A so the
    // bridge has an `initialize` cached for replay.
    let init = json!({"jsonrpc":"2.0","id":1,"method":"initialize"});
    let mut bytes = serde_json::to_vec(&init).expect("serialize init");
    bytes.push(b'\n');
    stdin.write_all(&bytes).expect("write init");
    let initialized = json!({"jsonrpc":"2.0","method":"notifications/initialized"});
    let mut bytes = serde_json::to_vec(&initialized).expect("serialize initialized");
    bytes.push(b'\n');
    stdin.write_all(&bytes).expect("write initialized");
    stdin.flush().expect("flush handshake");

    let mut line = String::new();
    reader
        .read_line(&mut line)
        .expect("read init reply (daemon A)");
    let v: Value = serde_json::from_str(&line).expect("parse init reply");
    assert_eq!(v["id"], 1, "client's init id round-trips on first connect");

    // Phase 2: a tools/list against daemon A succeeds.
    let list1 = json!({"jsonrpc":"2.0","id":2,"method":"tools/list"});
    let mut bytes = serde_json::to_vec(&list1).expect("serialize list1");
    bytes.push(b'\n');
    stdin.write_all(&bytes).expect("write list1");
    stdin.flush().expect("flush list1");

    line.clear();
    reader.read_line(&mut line).expect("read list1 reply");
    let v: Value = serde_json::from_str(&line).expect("parse list1 reply");
    assert_eq!(v["id"], 2);
    assert!(
        v["result"]["tools"].is_array(),
        "tools/list against daemon A must succeed"
    );

    // Phase 3: kill daemon A. The bridge's socket reader will see EOF
    // and trigger reconnect handling; absent any pending request, no
    // error frames will be synthesized — but the bridge will still
    // attempt to reconnect immediately.
    session.kill_now();
    // Give the bridge's reconnect loop a beat to notice the close
    // before we put a fresh daemon in front of it. Without this the
    // bridge might race ahead and connect to A's stale socket file
    // (which kill_now just removed).
    thread::sleep(Duration::from_millis(150));

    // Phase 4: bring up daemon B at the same session name. The
    // bridge's reconnect-with-backoff loop will catch this on its
    // next attempt.
    session.respawn();
    wait_for_socket(&mcp_socket_path(&name));

    // Phase 5: send a tools/list THROUGH the bridge. With reconnect
    // working, the bridge must:
    //   * have re-init'd daemon B (synthetic-id initialize replay)
    //   * forward this tools/list to daemon B
    //   * forward daemon B's response back unchanged (id == 3)
    // If reconnect fails, the bridge subprocess will exit and our
    // write or read will fail, which the test catches.
    let list2 = json!({"jsonrpc":"2.0","id":3,"method":"tools/list"});
    let mut bytes = serde_json::to_vec(&list2).expect("serialize list2");
    bytes.push(b'\n');
    stdin
        .write_all(&bytes)
        .expect("write list2 across reconnect");
    stdin.flush().expect("flush list2");

    line.clear();
    reader
        .read_line(&mut line)
        .expect("read list2 reply (post-reconnect)");
    let v: Value = serde_json::from_str(&line).expect("parse list2 reply");
    assert_eq!(
        v["id"], 3,
        "client's request id must round-trip through the bridge after reconnect"
    );
    let tools = v["result"]["tools"].as_array().expect("tools array");
    assert!(
        tools.iter().any(|t| t["name"] == "list_panes"),
        "tools/list across reconnect must reach the new daemon and return its tools"
    );

    // Close stdin; `session`'s Drop reaps both the adopted bridge and
    // daemon B (whichever is currently in `child`) unconditionally,
    // so no manual wait/kill dance is needed here.
    drop(stdin);
}

/// In-flight requests at the moment of disconnect should be answered
/// with synthesized JSON-RPC errors so the client unblocks. This test
/// would have been impossible pre-reconnect (the bridge just exited),
/// but the new state machine guarantees every id forwarded to the
/// daemon either gets a real response or a synthesized error.
#[test]
fn stdio_bridge_synthesizes_errors_for_inflight_requests_on_daemon_death() {
    let name = unique_name("mcp-inflight");
    let mut session = spawn_serve(&name);
    wait_for_socket(&mcp_socket_path(&name));

    let exe = env!("CARGO_BIN_EXE_zmux");
    let mut bridge = Command::new(exe)
        .arg("mcp")
        .arg("--session")
        .arg(&name)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn zmux mcp bridge");
    let mut stdin = bridge.stdin.take().expect("bridge stdin");
    let stdout = bridge.stdout.take().expect("bridge stdout");
    let mut reader = BufReader::new(stdout);
    // Adopt the bridge so a panic anywhere below still reaps it; the
    // daemon (and any reconnect-spawned replacement under the same
    // name) is torn down by name in Drop regardless of whether we
    // hold a live `Child` handle for it.
    session.adopt(bridge);

    // Handshake first so cached_init is set; the disconnect path then
    // also exercises the replay branch (even though we don't send a
    // post-reconnect request in this test).
    let init = json!({"jsonrpc":"2.0","id":1,"method":"initialize"});
    let mut bytes = serde_json::to_vec(&init).expect("serialize init");
    bytes.push(b'\n');
    stdin.write_all(&bytes).expect("write init");
    stdin.flush().expect("flush init");
    let mut line = String::new();
    reader.read_line(&mut line).expect("read init reply");

    // Send a tools/list, then immediately kill the daemon BEFORE
    // reading the response. The race we're forcing: request is in
    // flight when daemon dies. The bridge must synthesize an error
    // for id=42 so the client doesn't block forever.
    let list = json!({"jsonrpc":"2.0","id":42,"method":"tools/list"});
    let mut bytes = serde_json::to_vec(&list).expect("serialize list");
    bytes.push(b'\n');
    stdin.write_all(&bytes).expect("write list");
    stdin.flush().expect("flush list");

    session.kill_now();

    // Read the next frame the bridge emits. It will be EITHER:
    //   (a) the real tools/list response (daemon answered before we
    //       killed it — likely on a fast machine), or
    //   (b) a synthesized JSON-RPC error with id=42 and code=-32603.
    // Either way the client's awaiting promise resolves; if neither
    // arrives within 5s the bridge has hung and the test fails.
    line.clear();
    reader
        .read_line(&mut line)
        .expect("bridge must emit either a real response or a synthesized error within 5s");
    let v: Value = serde_json::from_str(&line).expect("parse frame");
    assert_eq!(
        v["id"], 42,
        "the frame must answer the in-flight request id"
    );
    let real_response = v["result"].is_object();
    let synthesized_error = v["error"]["code"]
        .as_i64()
        .map(|c| c == -32603)
        .unwrap_or(false);
    assert!(
        real_response || synthesized_error,
        "frame must be either a real response or a -32603 disconnect error: {v}",
    );

    // The bridge's reconnect path may have auto-spawned a fresh daemon
    // for `name` between disconnect and our shutdown. We don't hold a
    // `Child` handle for it, but `session`'s Drop tears sessions down
    // by name (`zmux::kill_session`), which covers it, plus reaps the
    // adopted bridge and sweeps both socket files.
    drop(stdin);
}

// ---------------------------------------------------------------- audit

/// Mutating tool calls (send_keys here) must land in the per-session
/// audit log with a connection id; read-only calls (list_panes) must
/// not. The daemon writes to $ZMUX_STATE_DIR/audit/<session>.jsonl,
/// so point the state dir at a test-scoped temp directory.
#[test]
fn mutating_tool_calls_land_in_the_audit_log() {
    let name = unique_name("mcp-audit");
    let state_dir = std::env::temp_dir().join(format!("zmux-test-state-{name}"));
    let _session =
        support::spawn_serve_with_envs(&name, [("ZMUX_STATE_DIR", state_dir.as_os_str())]);

    // A read-only call first — must NOT be audited.
    let list = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"list_panes","arguments":{}}
        }),
    );
    assert_eq!(list["result"]["isError"], false);

    // A mutating call — must be audited. Genesis pane id is 1.
    let send = round_trip(
        &name,
        json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{
                "name":"send_keys",
                "arguments":{"pane_id":1,"keys":"echo audit-probe","enter":false}
            }
        }),
    );
    assert_eq!(send["result"]["isError"], false, "send_keys failed: {send}");

    // The audit line is written before the tool executes, so it is on
    // disk by the time the reply arrives; poll briefly anyway in case
    // the filesystem is slow.
    let audit_path = state_dir.join("audit").join(format!("{name}.jsonl"));
    let deadline = Instant::now() + READ_TIMEOUT;
    let contents = loop {
        match std::fs::read_to_string(&audit_path) {
            Ok(s) if !s.is_empty() => break s,
            _ if Instant::now() > deadline => {
                panic!("audit log never appeared at {}", audit_path.display())
            }
            _ => thread::sleep(Duration::from_millis(20)),
        }
    };

    let audit_dir_mode = std::fs::metadata(audit_path.parent().unwrap())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    let audit_file_mode = std::fs::metadata(&audit_path).unwrap().permissions().mode() & 0o777;
    assert_eq!(audit_dir_mode, 0o700, "audit directory must be private");
    assert_eq!(audit_file_mode, 0o600, "audit log must be private");

    let lines: Vec<Value> = contents
        .lines()
        .map(|l| serde_json::from_str(l).expect("audit lines are JSON"))
        .collect();
    assert_eq!(
        lines.len(),
        1,
        "exactly the send_keys call is audited (list_panes is read-only): {contents}"
    );
    let entry = &lines[0];
    assert_eq!(entry["tool"], "send_keys");
    assert_eq!(entry["pane_id"], 1);
    assert_eq!(entry["keys"], "echo audit-probe");
    assert_eq!(entry["enter"], false);
    assert!(
        entry["conn"].as_u64().unwrap_or(0) >= 1,
        "audit entries carry the MCP connection id: {entry}"
    );
    assert!(
        entry["ts_ms"].as_u64().unwrap_or(0) > 0,
        "audit entries carry a timestamp: {entry}"
    );

    let _ = std::fs::remove_dir_all(&state_dir);
}

// ------------------------------------------------------------ guard proof

/// Proves the whole point of `TestSession`: a test body that panics
/// before reaching any explicit teardown must still leave no daemon
/// process and no socket files behind. We can't panic *this* test
/// without failing it, so we run the panicking body inside
/// `catch_unwind` and assert on the aftermath.
#[test]
fn drop_tears_down_session_on_panic() {
    let name = unique_name("guard-panic");
    let mut pid: Option<u32> = None;

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let session = spawn_serve(&name);
        wait_for_socket(&mcp_socket_path(&name));
        pid = session.pid();
        panic!("intentional panic to exercise TestSession::drop on unwind");
    }));

    assert!(
        result.is_err(),
        "inner closure was expected to panic (proves this isn't a no-op)"
    );
    let pid = pid.expect("daemon pid must have been captured before the panic fired");

    assert!(
        !client_socket_path(&name).exists(),
        "client socket must be removed by Drop after a panic"
    );
    assert!(
        !mcp_socket_path(&name).exists(),
        "mcp socket must be removed by Drop after a panic"
    );
    assert!(
        !process_alive(pid),
        "daemon process {pid} must be gone after Drop ran during unwind"
    );
}

fn process_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}
