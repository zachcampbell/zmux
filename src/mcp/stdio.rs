// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Stdio bridge for MCP clients.
//!
//! External MCP clients launch their MCP servers as subprocesses and
//! speak JSON-RPC over the child's stdin/stdout. This adapter routes
//! stdin → daemon socket and socket → stdout, with a `BridgeState`
//! that survives daemon restarts by synthesizing error responses for
//! in-flight requests, reconnecting, and replaying the `initialize`
//! handshake. Batched arrays are rejected with a JSON-RPC -32600.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::bridge_state::{
    BridgeState, IncomingDisposition, batch_rejection_frame, is_batch_frame,
};
use super::server::session_mcp_socket_path;
use crate::daemon::create_session;

/// `create_session` returns once the client socket is bound; the MCP
/// socket is bound a moment later, so even on Ok we can race the
/// second bind.
const MCP_SOCKET_WAIT: Duration = Duration::from_secs(3);

const RECONNECT_DEADLINE: Duration = Duration::from_secs(30);

const RECONNECT_BACKOFF_INITIAL: Duration = Duration::from_millis(100);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(2);

enum Event {
    StdinFrame(Vec<u8>),
    StdinEof,
    SocketFrame { generation: u32, frame: Vec<u8> },
    SocketEof { generation: u32 },
}

/// Run the stdin↔socket bridge against a session's MCP socket.
///
/// Auto-starts a daemon if the MCP socket isn't there yet (same path
/// runs on every reconnect, so a `zmux kill` followed by client
/// traffic recovers transparently). Returns once stdin EOFs or the
/// reconnect budget is exhausted.
pub fn run_stdio_bridge(session: &str) -> io::Result<()> {
    let socket_path: PathBuf = session_mcp_socket_path(session);
    let stream = connect_with_autostart(&socket_path, session)?;
    let (event_tx, event_rx) = mpsc::channel::<Event>();
    let _stdin_handle = spawn_stdin_thread(event_tx.clone());
    let mut current_gen: u32 = 0;
    let mut socket_reader: Option<JoinHandle<()>> = Some(spawn_socket_reader(
        stream.try_clone()?,
        current_gen,
        event_tx.clone(),
    )?);
    let mut socket: Option<UnixStream> = Some(stream);
    let mut state = BridgeState::new();
    // After stdin EOF we keep draining socket responses; otherwise a
    // fast stdin EOF would race the daemon's last reply and the
    // client would miss it.
    let mut draining_after_stdin_eof = false;
    let mut writable = true;

    run_event_loop(
        &event_rx,
        &event_tx,
        &mut socket,
        &mut socket_reader,
        &mut current_gen,
        &mut state,
        &mut draining_after_stdin_eof,
        &mut writable,
        &socket_path,
        session,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_event_loop(
    event_rx: &Receiver<Event>,
    event_tx: &Sender<Event>,
    socket: &mut Option<UnixStream>,
    socket_reader: &mut Option<JoinHandle<()>>,
    current_gen: &mut u32,
    state: &mut BridgeState,
    draining_after_stdin_eof: &mut bool,
    writable: &mut bool,
    socket_path: &Path,
    session: &str,
) -> io::Result<()> {
    let stdout = io::stdout();
    while let Ok(event) = event_rx.recv() {
        match event {
            Event::StdinFrame(frame) => {
                handle_stdin_frame(frame, socket, state, writable, &stdout)?;
            }
            Event::StdinEof => {
                if let Some(s) = socket.as_ref() {
                    let _ = s.shutdown(Shutdown::Write);
                }
                *draining_after_stdin_eof = true;
            }
            Event::SocketFrame { generation, frame } => {
                if generation != *current_gen {
                    continue;
                }
                handle_socket_frame(&frame, state, &stdout)?;
            }
            Event::SocketEof { generation } => {
                if generation != *current_gen {
                    continue;
                }
                if *draining_after_stdin_eof {
                    return Ok(());
                }
                handle_disconnect(
                    socket,
                    socket_reader,
                    current_gen,
                    state,
                    writable,
                    socket_path,
                    session,
                    event_tx,
                    &stdout,
                )?;
            }
        }
    }
    Ok(())
}

fn handle_stdin_frame(
    frame: Vec<u8>,
    socket: &mut Option<UnixStream>,
    state: &mut BridgeState,
    writable: &mut bool,
    stdout: &io::Stdout,
) -> io::Result<()> {
    if is_batch_frame(&frame) {
        let mut sink = stdout.lock();
        sink.write_all(&batch_rejection_frame())?;
        sink.flush()?;
        return Ok(());
    }
    state.observe_outgoing(&frame);
    if !*writable {
        return Ok(());
    }
    let Some(s) = socket.as_mut() else {
        return Ok(());
    };
    if write_frame(s, &frame).is_err() {
        // Shut down so the reader emits SocketEof and the disconnect
        // handler can synthesize errors and reconnect.
        let _ = s.shutdown(Shutdown::Both);
        *writable = false;
    }
    Ok(())
}

fn handle_socket_frame(
    frame: &[u8],
    state: &mut BridgeState,
    stdout: &io::Stdout,
) -> io::Result<()> {
    match state.observe_incoming(frame) {
        IncomingDisposition::Forward => {
            let mut sink = stdout.lock();
            sink.write_all(frame)?;
            sink.flush()?;
        }
        IncomingDisposition::ConsumeSynthetic => {
            // Bridge-issued (init replay); the client never saw the
            // request, so swallow the response.
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn handle_disconnect(
    socket: &mut Option<UnixStream>,
    socket_reader: &mut Option<JoinHandle<()>>,
    current_gen: &mut u32,
    state: &mut BridgeState,
    writable: &mut bool,
    socket_path: &Path,
    session: &str,
    event_tx: &Sender<Event>,
    stdout: &io::Stdout,
) -> io::Result<()> {
    *socket = None;
    if let Some(handle) = socket_reader.take() {
        let _ = handle.join();
    }
    {
        let mut sink = stdout.lock();
        for frame in state.synthesize_pending_errors() {
            sink.write_all(&frame)?;
        }
        sink.flush()?;
    }

    let stream = match reconnect_with_backoff(socket_path, session) {
        Some(s) => s,
        None => {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "zmux mcp: reconnect deadline exceeded; bridge exiting",
            ));
        }
    };

    // Bump generation BEFORE spawning the new reader so any stale
    // events from the old reader get filtered.
    *current_gen = current_gen.wrapping_add(1);
    let new_reader = spawn_socket_reader(stream.try_clone()?, *current_gen, event_tx.clone())?;
    *socket_reader = Some(new_reader);

    {
        let replay = state.replay_init_frames();
        for frame in &replay {
            if write_frame_owned(&stream, frame).is_err() {
                let _ = stream.shutdown(Shutdown::Both);
                return Err(io::Error::other(
                    "zmux mcp: replay write failed; daemon died during reconnect",
                ));
            }
        }
    }
    *socket = Some(stream);
    *writable = true;
    Ok(())
}

fn reconnect_with_backoff(socket_path: &Path, session: &str) -> Option<UnixStream> {
    let deadline = Instant::now() + RECONNECT_DEADLINE;
    let mut backoff = RECONNECT_BACKOFF_INITIAL;
    loop {
        match connect_with_autostart(socket_path, session) {
            Ok(stream) => return Some(stream),
            Err(_) if Instant::now() < deadline => {
                thread::sleep(backoff);
                backoff = std::cmp::min(backoff * 2, RECONNECT_BACKOFF_MAX);
            }
            Err(_) => return None,
        }
    }
}

/// Connect to the MCP socket, auto-starting the daemon if no socket
/// is present (NotFound) or the existing one is stale (ConnRefused).
fn connect_with_autostart(socket_path: &Path, session: &str) -> io::Result<UnixStream> {
    match UnixStream::connect(socket_path) {
        Ok(s) => Ok(s),
        Err(err) if is_no_daemon(err.kind()) => {
            ensure_session_running(session)?;
            wait_for_mcp_socket(socket_path, MCP_SOCKET_WAIT)?;
            UnixStream::connect(socket_path).map_err(|err| {
                io::Error::new(
                    err.kind(),
                    format!(
                        "connect to MCP socket {} after auto-starting daemon: {err}",
                        socket_path.display()
                    ),
                )
            })
        }
        Err(err) => Err(io::Error::new(
            err.kind(),
            format!("connect to MCP socket {}: {err}", socket_path.display()),
        )),
    }
}

/// NotFound = no socket file at all; ConnectionRefused = stale socket
/// left by a crashed daemon (Linux returns ECONNREFUSED when the
/// listener inode is gone).
fn is_no_daemon(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
}

/// Treats "already running" as success — that's the path where a
/// daemon exists but its MCP socket is briefly behind the client
/// socket.
fn ensure_session_running(session: &str) -> io::Result<()> {
    match create_session(session) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(err) => Err(io::Error::new(
            err.kind(),
            format!("auto-start session `{session}`: {err}"),
        )),
    }
}

fn wait_for_mcp_socket(path: &Path, timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(20));
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!(
            "MCP socket {} did not appear within {:?} after starting the daemon",
            path.display(),
            timeout
        ),
    ))
}

fn write_frame(stream: &mut UnixStream, frame: &[u8]) -> io::Result<()> {
    write_frame_owned(stream, frame)
}

/// Ensures the frame ends with exactly one newline; double-appending
/// would confuse the daemon's line-based reader.
fn write_frame_owned(mut stream: &UnixStream, frame: &[u8]) -> io::Result<()> {
    stream.write_all(frame)?;
    if !frame.ends_with(b"\n") {
        stream.write_all(b"\n")?;
    }
    stream.flush()?;
    Ok(())
}

fn spawn_stdin_thread(tx: Sender<Event>) -> io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("zmux-mcp-stdin".into())
        .spawn(move || {
            let stdin = io::stdin();
            let mut reader = BufReader::new(stdin.lock());
            loop {
                let mut buf: Vec<u8> = Vec::new();
                match reader.read_until(b'\n', &mut buf) {
                    Ok(0) => {
                        let _ = tx.send(Event::StdinEof);
                        return;
                    }
                    Ok(_) => {
                        if tx.send(Event::StdinFrame(buf)).is_err() {
                            return;
                        }
                    }
                    Err(_) => {
                        let _ = tx.send(Event::StdinEof);
                        return;
                    }
                }
            }
        })
}

fn spawn_socket_reader(
    stream: UnixStream,
    generation: u32,
    tx: Sender<Event>,
) -> io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name(format!("zmux-mcp-socket-{generation}"))
        .spawn(move || {
            let mut reader = BufReader::new(SocketReadHalf::new(stream));
            loop {
                let mut buf: Vec<u8> = Vec::new();
                match reader.read_until(b'\n', &mut buf) {
                    Ok(0) => {
                        let _ = tx.send(Event::SocketEof { generation });
                        return;
                    }
                    Ok(_) => {
                        if tx
                            .send(Event::SocketFrame {
                                generation,
                                frame: buf,
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(_) => {
                        let _ = tx.send(Event::SocketEof { generation });
                        return;
                    }
                }
            }
        })
}

/// Lets the reader `BufRead` without giving up the `UnixStream`
/// ownership the main-thread write path needs.
struct SocketReadHalf {
    inner: UnixStream,
}

impl SocketReadHalf {
    fn new(inner: UnixStream) -> Self {
        Self { inner }
    }
}

impl Read for SocketReadHalf {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}
