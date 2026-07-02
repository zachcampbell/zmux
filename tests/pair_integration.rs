// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

//! End-to-end smoke tests for the pair MCP client against a real
//! `zmux serve` daemon. Mirrors the pattern used by
//! `tests/daemon_integration.rs`.

use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::json;
use zmux::events::Event;
use zmux::pair::client::Client;

mod support;

fn unique_name(tag: &str) -> String {
    format!("pair-it-{}-{}", tag, std::process::id())
}

#[test]
fn client_connects_initializes_and_lists_panes() {
    let name = unique_name("list");
    let _session = support::spawn_serve(&name);
    support::wait_for_socket(&support::mcp_socket_path(&name));

    let client = Client::connect(&name).expect("connect");
    client.initialize().expect("init");
    let panes = client
        .call_tool("list_panes", json!({}))
        .expect("list_panes");
    assert!(panes["panes"].is_array());
    assert!(
        !panes["panes"].as_array().unwrap().is_empty(),
        "expected at least one pane"
    );
}

#[test]
fn client_receives_pane_output_event_after_send_keys() {
    let name = unique_name("events");
    let _session = support::spawn_serve(&name);
    support::wait_for_socket(&support::mcp_socket_path(&name));

    // Watcher client: subscribes to events. After watch_events runs the
    // reader half is owned by the spawned thread and call_tool MUST NOT
    // be used on this client (see client.rs `watch_events` docstring).
    let watch_client = Arc::new(Client::connect(&name).expect("connect watcher"));
    watch_client.initialize().expect("init watcher");
    let panes = watch_client
        .call_tool("list_panes", json!({}))
        .expect("list");
    let pane_id = panes["panes"][0]["pane_id"].as_u64().expect("pane id") as u32;

    let (tx, rx) = mpsc::channel::<Event>();
    let _w = watch_client.watch_events(tx).expect("watch");

    // Driver client: separate connection used to invoke send_keys
    // without contending for the watcher's reader.
    let driver = Client::connect(&name).expect("connect driver");
    driver.initialize().expect("init driver");
    driver
        .call_tool(
            "send_keys",
            json!({"pane_id": pane_id, "keys": "echo zmux-pair-ok", "enter": true}),
        )
        .expect("send_keys");

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut saw_output = false;
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(Event::PaneOutput { pane_id: id, .. }) if id == pane_id => {
                saw_output = true;
                break;
            }
            Ok(_) => continue,
            Err(_) => continue,
        }
    }
    assert!(saw_output, "expected a PaneOutput event for pane {pane_id}");
}
