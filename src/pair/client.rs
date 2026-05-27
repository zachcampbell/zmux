// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Peer MCP client used by `zmux pair` to talk to the running session
//! daemon's `<session>.mcp.sock`. Hand-rolled line-framed JSON-RPC
//! over Unix sockets; pair connects through the public MCP surface
//! rather than the daemon's internal `mcp::server` types.

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::{Value, json};

/// `Send` so it can be moved onto the watcher thread; the inner
/// `Mutex`es are only held for a single I/O operation at a time.
#[allow(dead_code)]
pub struct Client {
    writer: Mutex<UnixStream>,
    reader: Mutex<BufReader<UnixStream>>,
    next_id: Mutex<u64>,
}

impl Client {
    /// Opens the raw socket; the MCP handshake is run separately by
    /// [`Self::initialize`].
    pub fn connect(session: &str) -> io::Result<Self> {
        let path = socket_path(session);
        let writer = UnixStream::connect(&path)?;
        let reader = writer.try_clone()?;
        Ok(Self {
            writer: Mutex::new(writer),
            reader: Mutex::new(BufReader::new(reader)),
            next_id: Mutex::new(1),
        })
    }

    #[allow(dead_code)]
    pub(super) fn next_id(&self) -> u64 {
        let mut g = self.next_id.lock().expect("next_id mutex");
        let id = *g;
        *g += 1;
        id
    }

    #[allow(dead_code)]
    pub(super) fn send_frame(&self, frame: &Value) -> io::Result<()> {
        let mut g = self.writer.lock().expect("writer mutex");
        let line = serde_json::to_string(frame)?;
        g.write_all(line.as_bytes())?;
        g.write_all(b"\n")?;
        g.flush()
    }

    /// Returns `None` on EOF.
    #[allow(dead_code)]
    pub(super) fn read_frame(&self) -> io::Result<Option<Value>> {
        let mut buf = String::new();
        let mut g = self.reader.lock().expect("reader mutex");
        let n = g.read_line(&mut buf)?;
        if n == 0 {
            return Ok(None);
        }
        let v: Value = serde_json::from_str(buf.trim_end_matches('\n'))?;
        Ok(Some(v))
    }

    /// MCP requires this handshake before any tool call; consumes
    /// the init response so it doesn't pollute later reads.
    pub fn initialize(&self) -> io::Result<()> {
        let id = self.next_id();
        self.send_frame(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "zmux pair", "version": env!("CARGO_PKG_VERSION") }
            }
        }))?;

        // Clear the response so subsequent call_tool reads see only
        // their own responses; pair doesn't negotiate features.
        let resp = self.read_frame()?.ok_or_else(|| {
            io::Error::new(io::ErrorKind::UnexpectedEof, "EOF before init response")
        })?;
        let resp_id = resp.get("id").cloned().unwrap_or(Value::Null);
        if resp_id != id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("init response id mismatch: got {resp_id}, expected {id}"),
            ));
        }
        if let Some(err) = resp.get("error") {
            return Err(io::Error::other(format!("initialize error: {err}")));
        }

        self.send_frame(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }))
    }

    /// Synchronous `tools/call` that returns the `structuredContent`
    /// value on success. ONLY safe to call BEFORE `watch_events`
    /// takes ownership of the read half; after that, callers must
    /// route tool calls through a separate channel.
    pub fn call_tool(&self, name: &str, args: Value) -> io::Result<Value> {
        let id = self.next_id();
        self.send_frame(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": name, "arguments": args }
        }))?;

        let resp = self.read_frame()?.ok_or_else(|| {
            io::Error::new(io::ErrorKind::UnexpectedEof, "EOF before tool response")
        })?;

        if let Some(err) = resp.get("error") {
            return Err(io::Error::other(format!("tool {name} error: {err}")));
        }
        let result = resp.get("result").ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("tool {name}: response missing `result`"),
            )
        })?;
        if result
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            let text = result
                .pointer("/content/0/text")
                .and_then(|v| v.as_str())
                .unwrap_or("(no message)");
            return Err(io::Error::other(text.to_string()));
        }
        Ok(result
            .get("structuredContent")
            .cloned()
            .unwrap_or(Value::Null))
    }

    /// After this call, `call_tool` MUST NOT be invoked directly on
    /// this `Client` — the background reader thread now owns the
    /// read half. The returned `JoinHandle` lets the caller observe
    /// watcher termination (e.g. on socket EOF).
    pub fn watch_events(
        self: &std::sync::Arc<Self>,
        event_tx: std::sync::mpsc::Sender<crate::events::Event>,
    ) -> io::Result<std::thread::JoinHandle<()>> {
        let id = self.next_id();
        self.send_frame(&json!({
            "jsonrpc":"2.0","id":id,
            "method":"tools/call",
            "params":{"name":"watch_events","arguments":{}}
        }))?;

        let me = std::sync::Arc::clone(self);
        let handle = std::thread::spawn(move || {
            loop {
                match me.read_frame() {
                    Ok(Some(frame)) => {
                        // Frames carrying an id are responses to
                        // requests we issued; we don't have a
                        // multiplexer here, so drop them. This also
                        // swallows daemon rejections of `watch_events`
                        // itself — the caller sees an event channel
                        // that never produces.
                        if frame.get("id").is_some() {
                            continue;
                        }
                        if frame.get("method").and_then(|v| v.as_str()) == Some("zmux/event")
                            && let Some(params) = frame.get("params")
                            && let Some(event) = parse_event(params)
                            && event_tx.send(event).is_err()
                        {
                            return;
                        }
                    }
                    Ok(None) => return,
                    Err(_) => return,
                }
            }
        });
        Ok(handle)
    }
}

/// Unrecognized event kinds return `None` so future additions
/// don't crash pair.
pub(super) fn parse_event(params: &Value) -> Option<crate::events::Event> {
    use crate::events::Event;
    let kind = params.get("kind")?.as_str()?;
    let data = params.get("data")?;
    match kind {
        "PaneSpawned" => Some(Event::PaneSpawned {
            pane_id: data.get("pane_id")?.as_u64()? as u32,
            label: data.get("label").and_then(|v| v.as_str().map(String::from)),
        }),
        "PaneClosed" => Some(Event::PaneClosed {
            pane_id: data.get("pane_id")?.as_u64()? as u32,
        }),
        "PaneStateChanged" => Some(Event::PaneStateChanged {
            pane_id: data.get("pane_id")?.as_u64()? as u32,
            from: data.get("from")?.as_str()?.to_string(),
            to: data.get("to")?.as_str()?.to_string(),
        }),
        "PaneOutput" => Some(Event::PaneOutput {
            pane_id: data.get("pane_id")?.as_u64()? as u32,
            bytes_delta: data.get("bytes_delta")?.as_u64()?,
            last_line_preview: data
                .get("last_line_preview")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        }),
        "PaneExited" => Some(Event::PaneExited {
            pane_id: data.get("pane_id")?.as_u64()? as u32,
            exit_code: data.get("exit_code")?.as_i64()? as i32,
        }),
        "LabelChanged" => Some(Event::LabelChanged {
            pane_id: data.get("pane_id")?.as_u64()? as u32,
            label: data.get("label").and_then(|v| v.as_str().map(String::from)),
        }),
        _ => None,
    }
}

fn socket_path(session: &str) -> PathBuf {
    let user = std::env::var("USER").unwrap_or_else(|_| "user".to_string());
    std::env::temp_dir()
        .join(format!("zmux-{user}"))
        .join(format!("{session}.mcp.sock"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::net::UnixListener;
    use std::thread;

    fn pair_with_listener() -> (Client, UnixStream) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("zmux-pair-test-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("test.mcp.sock");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind listener");
        let server_side = thread::spawn(move || listener.accept().expect("accept").0);
        let client = {
            let writer = UnixStream::connect(&path).expect("client connect");
            let reader = writer.try_clone().expect("clone");
            Client {
                writer: Mutex::new(writer),
                reader: Mutex::new(BufReader::new(reader)),
                next_id: Mutex::new(1),
            }
        };
        let server_stream = server_side.join().expect("listener join");
        (client, server_stream)
    }

    #[test]
    fn next_id_is_monotonic() {
        let (client, _server) = pair_with_listener();
        let a = client.next_id();
        let b = client.next_id();
        let c = client.next_id();
        assert!(a < b && b < c);
    }

    #[test]
    fn send_and_read_frame_roundtrip() {
        let (client, mut server) = pair_with_listener();
        client
            .send_frame(&json!({"jsonrpc":"2.0","id":1,"method":"ping"}))
            .unwrap();

        let mut buf = String::new();
        BufReader::new(&mut server).read_line(&mut buf).unwrap();
        let parsed: Value = serde_json::from_str(buf.trim_end()).unwrap();
        assert_eq!(parsed["method"], "ping");

        server
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":\"pong\"}\n")
            .unwrap();
        server.flush().unwrap();

        let frame = client.read_frame().unwrap().unwrap();
        assert_eq!(frame["result"], "pong");
    }

    #[test]
    fn read_frame_returns_none_on_eof() {
        let (client, server) = pair_with_listener();
        drop(server);
        let frame = client.read_frame().unwrap();
        assert!(frame.is_none(), "expected EOF -> None, got {frame:?}");
    }

    #[test]
    fn initialize_sends_handshake_and_consumes_response() {
        let (client, mut server) = pair_with_listener();

        let server_thread = thread::spawn(move || {
            let mut buf = String::new();
            BufReader::new(&mut server).read_line(&mut buf).unwrap();
            let init: Value = serde_json::from_str(buf.trim_end()).unwrap();
            assert_eq!(init["method"], "initialize");
            let id = init["id"].clone();

            let resp = json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":"2025-06-18","serverInfo":{"name":"zmux"}}});
            let line = serde_json::to_string(&resp).unwrap();
            server.write_all(line.as_bytes()).unwrap();
            server.write_all(b"\n").unwrap();
            server.flush().unwrap();

            buf.clear();
            BufReader::new(&mut server).read_line(&mut buf).unwrap();
            let notif: Value = serde_json::from_str(buf.trim_end()).unwrap();
            assert_eq!(notif["method"], "notifications/initialized");
            assert!(
                notif.get("id").is_none(),
                "initialized must be a notification"
            );
        });

        client.initialize().expect("initialize ok");
        server_thread.join().unwrap();
    }

    #[test]
    fn call_tool_sends_envelope_and_returns_structured_content() {
        let (client, mut server) = pair_with_listener();

        let server_thread = thread::spawn(move || {
            let mut buf = String::new();
            BufReader::new(&mut server).read_line(&mut buf).unwrap();
            let req: Value = serde_json::from_str(buf.trim_end()).unwrap();
            assert_eq!(req["method"], "tools/call");
            assert_eq!(req["params"]["name"], "list_panes");
            let id = req["id"].clone();

            let resp = json!({
                "jsonrpc":"2.0",
                "id": id,
                "result": {
                    "content": [{"type":"text","text":"..."}],
                    "structuredContent": {"panes": [{"pane_id": 1, "state": "Idle"}]},
                    "isError": false
                }
            });
            let line = serde_json::to_string(&resp).unwrap();
            server.write_all(line.as_bytes()).unwrap();
            server.write_all(b"\n").unwrap();
            server.flush().unwrap();
        });

        let result = client.call_tool("list_panes", json!({})).expect("call ok");
        assert_eq!(result["panes"][0]["pane_id"], 1);
        server_thread.join().unwrap();
    }

    #[test]
    fn call_tool_surfaces_iserror_results() {
        let (client, mut server) = pair_with_listener();

        let server_thread = thread::spawn(move || {
            let mut buf = String::new();
            BufReader::new(&mut server).read_line(&mut buf).unwrap();
            let req: Value = serde_json::from_str(buf.trim_end()).unwrap();
            let id = req["id"].clone();

            let resp = json!({
                "jsonrpc":"2.0",
                "id": id,
                "result": {
                    "content": [{"type":"text","text":"pane 99 not found"}],
                    "isError": true
                }
            });
            let line = serde_json::to_string(&resp).unwrap();
            server.write_all(line.as_bytes()).unwrap();
            server.write_all(b"\n").unwrap();
            server.flush().unwrap();
        });

        let err = client
            .call_tool("read_pane", json!({"pane_id":99}))
            .unwrap_err();
        assert!(err.to_string().contains("pane 99 not found"), "got: {err}");
        server_thread.join().unwrap();
    }

    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn watch_events_forwards_notifications_to_event_channel() {
        let (client, mut server) = pair_with_listener();

        let server_thread = thread::spawn(move || {
            let mut buf = String::new();
            BufReader::new(&mut server).read_line(&mut buf).unwrap();
            let req: Value = serde_json::from_str(buf.trim_end()).unwrap();
            let id = req["id"].clone();
            assert_eq!(req["method"], "tools/call");
            assert_eq!(req["params"]["name"], "watch_events");
            let ok = json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":"{}"}],"structuredContent":{},"isError":false}});
            server
                .write_all(serde_json::to_string(&ok).unwrap().as_bytes())
                .unwrap();
            server.write_all(b"\n").unwrap();
            let n1 = json!({"jsonrpc":"2.0","method":"zmux/event","params":{"kind":"PaneStateChanged","data":{"pane_id":2,"from":"Working","to":"Errored"}}});
            let n2 = json!({"jsonrpc":"2.0","method":"zmux/event","params":{"kind":"PaneExited","data":{"pane_id":2,"exit_code":1}}});
            server
                .write_all(serde_json::to_string(&n1).unwrap().as_bytes())
                .unwrap();
            server.write_all(b"\n").unwrap();
            server
                .write_all(serde_json::to_string(&n2).unwrap().as_bytes())
                .unwrap();
            server.write_all(b"\n").unwrap();
            server.flush().unwrap();
            thread::sleep(Duration::from_millis(50));
        });

        let client = std::sync::Arc::new(client);
        let (event_tx, event_rx) = mpsc::channel();
        let _watcher = client.watch_events(event_tx).expect("watch_events ok");

        let e1 = event_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("recv 1");
        let e2 = event_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("recv 2");
        match e1 {
            crate::events::Event::PaneStateChanged { pane_id, from, to } => {
                assert_eq!(pane_id, 2);
                assert_eq!(from, "Working");
                assert_eq!(to, "Errored");
            }
            other => panic!("expected PaneStateChanged, got {other:?}"),
        }
        match e2 {
            crate::events::Event::PaneExited { pane_id, exit_code } => {
                assert_eq!(pane_id, 2);
                assert_eq!(exit_code, 1);
            }
            other => panic!("expected PaneExited, got {other:?}"),
        }
        server_thread.join().unwrap();
    }
}
