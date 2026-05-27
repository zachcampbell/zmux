// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

//! End-to-end smoke tests for the pair MCP client against a real
//! `zmux serve` daemon. Mirrors the pattern used by
//! `tests/daemon_integration.rs`.

use std::io;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;
use zmux::events::Event;
use zmux::pair::client::Client;

unsafe extern "C" {
    fn setsid() -> i32;
}

fn socket_path(name: &str) -> PathBuf {
    let user = std::env::var("USER").unwrap_or_else(|_| "user".to_string());
    std::env::temp_dir()
        .join(format!("zmux-{user}"))
        .join(format!("{name}.mcp.sock"))
}

fn spawn_daemon(name: &str) -> Child {
    let exe = env!("CARGO_BIN_EXE_zmux");
    let mut command = Command::new(exe);
    command
        .arg("serve")
        .arg(name)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            if setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn().expect("spawn daemon")
}

fn wait_for_socket(name: &str) {
    let path = socket_path(name);
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if path.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("daemon socket {} did not appear", path.display());
}

fn unique_name(tag: &str) -> String {
    format!("pair-it-{}-{}", tag, std::process::id())
}

#[test]
fn client_connects_initializes_and_lists_panes() {
    let session = unique_name("list");
    let mut child = spawn_daemon(&session);
    wait_for_socket(&session);

    let client = Client::connect(&session).expect("connect");
    client.initialize().expect("init");
    let panes = client
        .call_tool("list_panes", json!({}))
        .expect("list_panes");
    assert!(panes["panes"].is_array());
    assert!(
        !panes["panes"].as_array().unwrap().is_empty(),
        "expected at least one pane"
    );

    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn client_receives_pane_output_event_after_send_keys() {
    let session = unique_name("events");
    let mut child = spawn_daemon(&session);
    wait_for_socket(&session);

    // Watcher client: subscribes to events. After watch_events runs the
    // reader half is owned by the spawned thread and call_tool MUST NOT
    // be used on this client (see client.rs `watch_events` docstring).
    let watch_client = Arc::new(Client::connect(&session).expect("connect watcher"));
    watch_client.initialize().expect("init watcher");
    let panes = watch_client
        .call_tool("list_panes", json!({}))
        .expect("list");
    let pane_id = panes["panes"][0]["pane_id"].as_u64().expect("pane id") as u32;

    let (tx, rx) = mpsc::channel::<Event>();
    let _w = watch_client.watch_events(tx).expect("watch");

    // Driver client: separate connection used to invoke send_keys
    // without contending for the watcher's reader.
    let driver = Client::connect(&session).expect("connect driver");
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

    let _ = child.kill();
    let _ = child.wait();
}
