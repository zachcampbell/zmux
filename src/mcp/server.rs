// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Transport layer for the in-daemon MCP server: listener thread,
//! per-connection reader/writer split, and the socket-path naming
//! convention. JSON-RPC parsing and routing lives in
//! [`crate::mcp::protocol`].
//!
//! # Security
//!
//! Every tool this server exposes is a privileged action: `spawn_pane`,
//! `send_keys`, and `kill_pane` give a connected client the same
//! authority as a logged-in shell. The only access control is
//! filesystem-level — the socket directory is created `0o700` and the
//! socket file is `chmod 0o600` after bind (see `ensure_session_root`
//! in `daemon.rs` and the bind site below). Do not relax those modes,
//! do not bind the socket somewhere world-traversable, and do not
//! proxy it onto a network: a peer that can `connect()` here owns the
//! session.
//!
//! Per-connection topology:
//!
//! ```text
//! reader thread ──╮
//!                 ├──> outbound_tx ──> writer thread ──> socket.write_half
//! pump thread* ───╯       (Vec<u8>)
//! ```
//!
//! A single writer thread owns the socket write half, so request
//! replies and `watch_events` notifications can never interleave
//! mid-line without a mutex.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TrySendError};
use std::thread;

use super::execute::McpRequest;
use super::protocol::process_request_line;

/// A client that subscribes via `watch_events` and stops reading the
/// socket can grow the outbound queue without bound; once full, new
/// payloads are dropped with a one-time warning rather than letting
/// the daemon's memory walk.
const OUTBOUND_QUEUE_BOUND: usize = 1024;

/// Caps a single JSON-RPC request line so a misbehaving client can't
/// stream unbounded bytes without a newline and exhaust daemon memory.
const MAX_LINE_BYTES: usize = 1 << 20;

pub fn session_mcp_socket_path(name: &str) -> PathBuf {
    let user = std::env::var("USER").unwrap_or_else(|_| "user".to_string());
    std::env::temp_dir()
        .join(format!("zmux-{user}"))
        .join(format!("{name}.mcp.sock"))
}

/// Spawn the MCP listener thread. Removes any stale socket file
/// first so a leftover from a previous crash doesn't permanently
/// break a fresh `zmux serve`.
pub fn spawn_listener(socket_path: PathBuf) -> io::Result<Receiver<McpRequest>> {
    if socket_path.exists() {
        let _ = std::fs::remove_file(&socket_path);
    }
    let listener = UnixListener::bind(&socket_path)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;
    }
    let (tx, rx) = mpsc::channel::<McpRequest>();
    thread::Builder::new()
        .name("zmux-mcp-listener".into())
        .spawn(move || run_listener(listener, tx))?;
    Ok(rx)
}

fn run_listener(listener: UnixListener, tx: Sender<McpRequest>) {
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(_) => continue,
        };
        let tx = tx.clone();
        let _ = thread::Builder::new()
            .name("zmux-mcp-conn".into())
            .spawn(move || handle_connection(stream, tx));
    }
}

/// Per-connection reader. Spawns a writer thread that owns the
/// socket's write half so `watch_events` notifications can be pushed
/// without racing response writes. Lines longer than
/// [`MAX_LINE_BYTES`] log to stderr and close just this connection.
fn handle_connection(stream: UnixStream, tx: Sender<McpRequest>) {
    let read_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut reader = BufReader::new(read_stream);
    let write_stream = stream;
    let (outbound_tx, outbound_rx) = mpsc::sync_channel::<Vec<u8>>(OUTBOUND_QUEUE_BOUND);
    let writer_handle = thread::Builder::new()
        .name("zmux-mcp-writer".into())
        .spawn(move || run_writer(write_stream, outbound_rx))
        .expect("spawn mcp writer thread");
    let queue_full_logged = Arc::new(AtomicBool::new(false));
    // Latches on the first successful `watch_events`; subsequent
    // calls on the same connection are refused so the client doesn't
    // double-subscribe and see every event twice.
    let subscribed = Arc::new(AtomicBool::new(false));
    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        // Cap at MAX+1 so a full buffer with no terminating newline
        // signals overrun rather than a legitimate max-length line.
        let cap = MAX_LINE_BYTES as u64 + 1;
        let read_result = (&mut reader).take(cap).read_until(b'\n', &mut buf);
        match read_result {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        if buf.len() > MAX_LINE_BYTES && buf.last().is_none_or(|b| *b != b'\n') {
            eprintln!(
                "zmux mcp: client sent oversize line ({} bytes); closing connection",
                buf.len()
            );
            break;
        }
        let line = match std::str::from_utf8(&buf) {
            Ok(s) => s,
            Err(_) => {
                break;
            }
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let outbound = OutboundQueue {
            sender: outbound_tx.clone(),
            full_logged: queue_full_logged.clone(),
            subscribed: subscribed.clone(),
        };
        let response = process_request_line(trimmed, &tx, Some(&outbound));
        if let Some(response) = response {
            let mut bytes = response.to_string().into_bytes();
            bytes.push(b'\n');
            if !outbound.send(bytes) {
                break;
            }
        }
    }
    // Dropping our sender lets the writer thread exit once any
    // in-flight pump has also hung up. The writer's exit closes the
    // socket which ends the pump via its own send error.
    drop(outbound_tx);
    let _ = writer_handle.join();
}

/// Per-connection outbound payload sender. `Full` drops the payload
/// (with a one-time warning); `Disconnected` is reported back so the
/// caller can hang up cleanly.
pub(super) struct OutboundQueue {
    sender: SyncSender<Vec<u8>>,
    full_logged: Arc<AtomicBool>,
    subscribed: Arc<AtomicBool>,
}

impl OutboundQueue {
    /// Returns `false` only when the writer is gone; `Full` is
    /// silently dropped and reported as success.
    pub(super) fn send(&self, bytes: Vec<u8>) -> bool {
        try_send_outbound(&self.sender, bytes, &self.full_logged)
    }

    /// Returns `true` on the first claim; subsequent callers get
    /// `false` so the dispatch layer can refuse duplicate
    /// `watch_events` subscriptions on the same connection.
    pub(super) fn try_mark_subscribed(&self) -> bool {
        !self.subscribed.swap(true, Ordering::Relaxed)
    }
}

impl Clone for OutboundQueue {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            full_logged: self.full_logged.clone(),
            subscribed: self.subscribed.clone(),
        }
    }
}

#[cfg(test)]
pub(super) fn outbound_for_test() -> (OutboundQueue, Receiver<Vec<u8>>) {
    let (sender, rx) = mpsc::sync_channel::<Vec<u8>>(OUTBOUND_QUEUE_BOUND);
    let queue = OutboundQueue {
        sender,
        full_logged: Arc::new(AtomicBool::new(false)),
        subscribed: Arc::new(AtomicBool::new(false)),
    };
    (queue, rx)
}

#[cfg(test)]
pub(super) fn outbound_for_test_with_bound(bound: usize) -> (OutboundQueue, Receiver<Vec<u8>>) {
    let (sender, rx) = mpsc::sync_channel::<Vec<u8>>(bound);
    let queue = OutboundQueue {
        sender,
        full_logged: Arc::new(AtomicBool::new(false)),
        subscribed: Arc::new(AtomicBool::new(false)),
    };
    (queue, rx)
}

/// Drop policy is drop-newest under backpressure: if a stalled
/// client never recovers it eventually disconnects and the writer
/// notices on its own.
fn try_send_outbound(
    sender: &SyncSender<Vec<u8>>,
    bytes: Vec<u8>,
    full_logged: &Arc<AtomicBool>,
) -> bool {
    match sender.try_send(bytes) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) => {
            if !full_logged.swap(true, Ordering::Relaxed) {
                eprintln!("zmux mcp: notification queue full for connection; dropping events");
            }
            true
        }
        Err(TrySendError::Disconnected(_)) => false,
    }
}

fn run_writer(mut stream: UnixStream, outbound_rx: Receiver<Vec<u8>>) {
    while let Ok(payload) = outbound_rx.recv() {
        if stream.write_all(&payload).is_err() {
            return;
        }
    }
}
