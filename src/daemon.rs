// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::command::FormatContext;
use crate::command::parse as parse_command;
use crate::dispatch::{ClientId, CommandContext, CommandRegistry, SideEffect, register_tier1};
use crate::input::InputParser;
use crate::layout::{ResizeDirection, SplitOrientation};
use crate::mcp;
use crate::mouse::MouseTrackingMode;
use crate::protocol::{
    ClientDecoder, ClientMessage, ServerDecoder, ServerMessage, encode_client_message,
    encode_server_message,
};
use crate::pty::PtySize;
use crate::tty::{TerminalGuard, poll_readable};
use crate::workspace::{LayoutPreset, PANE_NUMBER_OVERLAY_DURATION, PromptKind, Workspace};

unsafe extern "C" {
    fn setsid() -> i32;
}

const DEFAULT_SESSION_NAME: &str = "default";
const SERVER_POLL_MS: u64 = 20;
const STARTUP_WAIT_MS: u64 = 1_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEntry {
    pub name: String,
    pub socket_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PruneReport {
    pub removed: Vec<PathBuf>,
    pub kept: Vec<SessionEntry>,
}

pub fn default_session_name() -> &'static str {
    DEFAULT_SESSION_NAME
}

pub fn create_session(name: &str) -> io::Result<()> {
    validate_session_name(name)?;
    ensure_session_root()?;

    let socket_path = session_socket_path(name);
    if socket_path.exists() {
        if UnixStream::connect(&socket_path).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("session `{name}` already exists"),
            ));
        }
        fs::remove_file(&socket_path)?;
    }

    let exe = env::current_exe()?;
    let dev_null = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")?;
    let stdin = dev_null.try_clone()?;
    let stdout = dev_null.try_clone()?;
    let stderr = dev_null;

    let mut command = Command::new(exe);
    command.arg("serve").arg(name);
    command.stdin(Stdio::from(stdin));
    command.stdout(Stdio::from(stdout));
    command.stderr(Stdio::from(stderr));
    unsafe {
        command.pre_exec(|| {
            if setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = command.spawn()?;
    let mut waited = 0;
    while waited < STARTUP_WAIT_MS {
        if socket_path.exists() {
            return Ok(());
        }

        if let Some(status) = child.try_wait()? {
            return Err(io::Error::other(format!(
                "session server exited early with {status}"
            )));
        }

        thread::sleep(Duration::from_millis(20));
        waited += 20;
    }

    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!("timed out waiting for session `{name}` to start"),
    ))
}

pub fn attach_session(name: &str) -> io::Result<AttachOutcome> {
    validate_session_name(name)?;
    let config = crate::config::Config::load();
    let mut stream = UnixStream::connect(session_socket_path(name))?;
    let mut terminal = TerminalGuard::enter()?;
    let mut decoder = ServerDecoder::default();
    let mut prefix_keys = PrefixKeyParser::with_prefix(config.prefix_byte);
    let mut current_size = terminal.size()?;
    // When true, keystrokes are consumed locally as scroll commands
    // (j/k/g/G/Ctrl-U/Ctrl-D/Space etc.) and translated into
    // ClientMessage::ScrollUp/Down/ToBottom instead of being forwarded
    // to the pane's shell. Any unrecognized key exits the mode.
    let mut scroll_mode = false;
    // Sub-mode within scroll mode: when true, the user opened a
    // search prompt with `/` and bytes are forwarded as SearchInput
    // until Enter (commit) or Esc (cancel) lands.
    let mut search_input_mode = false;
    // Sub-mode within scroll mode: after `v`, motion keys extend a
    // line selection on the active pane instead of scrolling, `y`
    // yanks the selected lines, and `Esc` cancels back to plain
    // scroll mode.
    let mut selection_mode = false;
    // Top-level prompt mode (NOT inside scroll mode). Set when the
    // user hits Ctrl-a , (rename) or Ctrl-a !/^ (command prompt);
    // while active, byte input is forwarded as the corresponding
    // Input protocol message until Enter commits or Esc cancels.
    let mut prompt_mode: Option<PromptMode> = None;

    send_client_message(&mut stream, &ClientMessage::Attach { size: current_size })?;

    loop {
        let fresh_size = terminal.size()?;
        if fresh_size != current_size {
            current_size = fresh_size;
            send_client_message(&mut stream, &ClientMessage::Resize { size: current_size })?;
        }

        if let Some(bytes) = read_socket_available(&mut stream)? {
            let messages = decoder.push_bytes(&bytes)?;
            for message in messages {
                match message {
                    ServerMessage::Frame {
                        size,
                        mouse_tracking_mode,
                        lines,
                        cursor,
                    } => {
                        terminal.set_mouse_tracking_mode(
                            mouse_tracking_mode.max(MouseTrackingMode::Click),
                        )?;
                        terminal.render_frame(&lines, size, cursor)?;
                    }
                    ServerMessage::Exited { code } => return Ok(AttachOutcome::Exited(code)),
                    ServerMessage::Error(message) => return Err(io::Error::other(message)),
                    ServerMessage::Busy => {
                        return Err(io::Error::new(
                            io::ErrorKind::AddrInUse,
                            format!("session `{name}` is already attached elsewhere"),
                        ));
                    }
                    ServerMessage::Clipboard(text) => {
                        terminal.emit_clipboard(&text)?;
                    }
                    ServerMessage::PaneList(_) => {
                        // Attached clients never request `ListPanes`;
                        // the admin path opens its own short-lived
                        // socket. If a server somehow sends one to an
                        // attached client, ignore it rather than
                        // tearing down the session.
                    }
                }
            }
        }

        let input = terminal.read_input(50)?;
        if input.is_empty() {
            continue;
        }

        for action in prefix_keys.push_bytes(&input) {
            match action {
                AttachInput::Forward(bytes) => {
                    if let Some(mode) = prompt_mode {
                        // Top-level prompt — route bytes per mode. Enter
                        // commits, Esc cancels, everything else streams
                        // as Input for the server to buffer and render.
                        let (input_msg, commit_msg, cancel_msg): (
                            fn(Vec<u8>) -> ClientMessage,
                            ClientMessage,
                            ClientMessage,
                        ) = match mode {
                            PromptMode::Rename => (
                                ClientMessage::RenameInput,
                                ClientMessage::RenameCommit,
                                ClientMessage::RenameCancel,
                            ),
                            PromptMode::CommandColumns
                            | PromptMode::CommandRows
                            | PromptMode::CommandGeneral => (
                                ClientMessage::CommandPromptInput,
                                ClientMessage::CommandPromptCommit,
                                ClientMessage::CommandPromptCancel,
                            ),
                        };
                        let mut pending: Vec<u8> = Vec::new();
                        for &byte in &bytes {
                            match byte {
                                0x0d | 0x0a => {
                                    if !pending.is_empty() {
                                        send_client_message(
                                            &mut stream,
                                            &input_msg(std::mem::take(&mut pending)),
                                        )?;
                                    }
                                    send_client_message(&mut stream, &commit_msg)?;
                                    prompt_mode = None;
                                }
                                0x1b => {
                                    pending.clear();
                                    send_client_message(&mut stream, &cancel_msg)?;
                                    prompt_mode = None;
                                }
                                _ => pending.push(byte),
                            }
                        }
                        if !pending.is_empty() {
                            send_client_message(&mut stream, &input_msg(pending))?;
                        }
                        continue;
                    }

                    if search_input_mode {
                        // Drain typed bytes against the prompt: Enter
                        // commits, Esc cancels, everything else is
                        // streamed to the server which interprets
                        // backspace + printable chars itself.
                        let mut pending: Vec<u8> = Vec::new();
                        for &byte in &bytes {
                            match byte {
                                0x0d | 0x0a => {
                                    if !pending.is_empty() {
                                        send_client_message(
                                            &mut stream,
                                            &ClientMessage::SearchInput(std::mem::take(
                                                &mut pending,
                                            )),
                                        )?;
                                    }
                                    send_client_message(&mut stream, &ClientMessage::SearchCommit)?;
                                    search_input_mode = false;
                                }
                                0x1b => {
                                    pending.clear();
                                    send_client_message(&mut stream, &ClientMessage::SearchCancel)?;
                                    search_input_mode = false;
                                }
                                _ => pending.push(byte),
                            }
                        }
                        if !pending.is_empty() {
                            send_client_message(&mut stream, &ClientMessage::SearchInput(pending))?;
                        }
                        continue;
                    }

                    if scroll_mode {
                        let effects = if selection_mode {
                            translate_selection_keys(&bytes)
                        } else {
                            translate_scroll_keys(&bytes, current_size.rows as usize)
                        };
                        let mut exiting = false;
                        let mut exit_selection_only = false;
                        for effect in effects {
                            match effect {
                                ScrollKeyEffect::Send(msg) => {
                                    send_client_message(&mut stream, &msg)?;
                                }
                                ScrollKeyEffect::EnterSearchInput => {
                                    search_input_mode = true;
                                    send_client_message(&mut stream, &ClientMessage::SearchBegin)?;
                                }
                                ScrollKeyEffect::EnterSelection(kind) => {
                                    selection_mode = true;
                                    send_client_message(
                                        &mut stream,
                                        &ClientMessage::BeginSelection(kind),
                                    )?;
                                }
                                ScrollKeyEffect::YankSelectionAndExit => {
                                    send_client_message(
                                        &mut stream,
                                        &ClientMessage::YankSelection,
                                    )?;
                                    selection_mode = false;
                                    exiting = true;
                                    break;
                                }
                                ScrollKeyEffect::CancelSelection => {
                                    send_client_message(
                                        &mut stream,
                                        &ClientMessage::ClearSelection,
                                    )?;
                                    selection_mode = false;
                                    exit_selection_only = true;
                                    break;
                                }
                                ScrollKeyEffect::ExitScrollMode => {
                                    exiting = true;
                                    break;
                                }
                            }
                        }
                        if exiting {
                            scroll_mode = false;
                            selection_mode = false;
                            // Leaving scroll mode jumps us back to live
                            // output so new shell output shows up.
                            let _ =
                                send_client_message(&mut stream, &ClientMessage::ClearSelection);
                            let _ = send_client_message(&mut stream, &ClientMessage::SearchClear);
                            let _ =
                                send_client_message(&mut stream, &ClientMessage::ScrollToBottom);
                        } else if exit_selection_only {
                            // Cancelled the selection; stay in scroll mode
                            // so the user can scroll around and re-anchor.
                        }
                        continue;
                    }
                    send_client_message(&mut stream, &ClientMessage::Input(bytes))?;
                }
                AttachInput::Detach => {
                    let _ = send_client_message(&mut stream, &ClientMessage::Detach);
                    return Ok(AttachOutcome::Detached);
                }
                AttachInput::SplitPaneColumns => {
                    send_client_message(&mut stream, &ClientMessage::SplitPaneColumns)?;
                }
                AttachInput::SplitPaneRows => {
                    send_client_message(&mut stream, &ClientMessage::SplitPaneRows)?;
                }
                AttachInput::ClosePane => {
                    send_client_message(&mut stream, &ClientMessage::ClosePane)?;
                }
                AttachInput::CyclePane => {
                    send_client_message(&mut stream, &ClientMessage::CyclePane)?;
                }
                AttachInput::CyclePaneBackward => {
                    send_client_message(&mut stream, &ClientMessage::CyclePaneBackward)?;
                }
                AttachInput::ShowPaneNumbers => {
                    send_client_message(&mut stream, &ClientMessage::ShowPaneNumbers)?;
                }
                AttachInput::OpenSupervisor => {
                    send_client_message(&mut stream, &ClientMessage::OpenSupervisor)?;
                }
                AttachInput::ResizePaneLeft => {
                    send_client_message(&mut stream, &ClientMessage::ResizePaneLeft)?;
                }
                AttachInput::ResizePaneRight => {
                    send_client_message(&mut stream, &ClientMessage::ResizePaneRight)?;
                }
                AttachInput::ResizePaneUp => {
                    send_client_message(&mut stream, &ClientMessage::ResizePaneUp)?;
                }
                AttachInput::ResizePaneDown => {
                    send_client_message(&mut stream, &ClientMessage::ResizePaneDown)?;
                }
                AttachInput::CyclePreset => {
                    send_client_message(&mut stream, &ClientMessage::CyclePreset)?;
                }
                AttachInput::SelectWindow(index) => {
                    send_client_message(&mut stream, &ClientMessage::SelectWindow(index))?;
                }
                AttachInput::PasteBuffer => {
                    send_client_message(&mut stream, &ClientMessage::PasteBuffer)?;
                }
                AttachInput::EnterScrollback => {
                    // Entering scrollback doesn't itself scroll; the
                    // first subsequent j/k/etc. will. We send an initial
                    // ScrollUp(0) so the server gets a chance to render
                    // the status indicator (pane header shows SCROLL)
                    // immediately.
                    scroll_mode = true;
                    let _ = send_client_message(&mut stream, &ClientMessage::ScrollUp(1));
                }
                AttachInput::YankViewport => {
                    send_client_message(&mut stream, &ClientMessage::YankViewport)?;
                }
                AttachInput::ToggleZoom => {
                    send_client_message(&mut stream, &ClientMessage::ToggleZoom)?;
                }
                AttachInput::SwapPaneNext => {
                    send_client_message(&mut stream, &ClientMessage::SwapPaneNext)?;
                }
                AttachInput::SwapPanePrevious => {
                    send_client_message(&mut stream, &ClientMessage::SwapPanePrevious)?;
                }
                AttachInput::BeginRename => {
                    prompt_mode = Some(PromptMode::Rename);
                    send_client_message(&mut stream, &ClientMessage::RenameBegin)?;
                }
                AttachInput::BeginCommandPromptColumns => {
                    prompt_mode = Some(PromptMode::CommandColumns);
                    send_client_message(
                        &mut stream,
                        &ClientMessage::CommandPromptBegin(
                            crate::protocol::CommandPromptKind::SplitColumns,
                        ),
                    )?;
                }
                AttachInput::BeginCommandPromptRows => {
                    prompt_mode = Some(PromptMode::CommandRows);
                    send_client_message(
                        &mut stream,
                        &ClientMessage::CommandPromptBegin(
                            crate::protocol::CommandPromptKind::SplitRows,
                        ),
                    )?;
                }
                AttachInput::BeginCommandPromptGeneral => {
                    prompt_mode = Some(PromptMode::CommandGeneral);
                    send_client_message(
                        &mut stream,
                        &ClientMessage::CommandPromptBegin(
                            crate::protocol::CommandPromptKind::General,
                        ),
                    )?;
                }
                AttachInput::NewWindow => {
                    send_client_message(&mut stream, &ClientMessage::NewWindow)?;
                }
                AttachInput::CloseWindow => {
                    send_client_message(&mut stream, &ClientMessage::CloseWindow)?;
                }
                AttachInput::NextWindow => {
                    send_client_message(&mut stream, &ClientMessage::NextWindow)?;
                }
                AttachInput::LastWindow => {
                    send_client_message(&mut stream, &ClientMessage::LastWindow)?;
                }
                AttachInput::PreviousWindow => {
                    send_client_message(&mut stream, &ClientMessage::PreviousWindow)?;
                }
                AttachInput::ToggleSyncPanes => {
                    send_client_message(&mut stream, &ClientMessage::ToggleSyncPanes)?;
                }
                AttachInput::ShowSessionPicker => {
                    // Ctrl-a s opens an in-client picker overlay listing
                    // every OTHER attachable session on this machine. If
                    // the user picks one, we detach from the current
                    // session and return to main.rs with `Switch(name)`;
                    // main.rs loops back into `attach_session` so the
                    // user experiences one continuous zmux UX.
                    let mut entries = list_sessions()?;
                    entries.retain(|entry| entry.name != name);
                    match run_session_picker(&mut terminal, &entries)? {
                        Some(target) => {
                            let _ = send_client_message(&mut stream, &ClientMessage::Detach);
                            return Ok(AttachOutcome::Switch(target));
                        }
                        None => {
                            // Cancelled — the overlay painted over cells
                            // the renderer thinks are still valid, so
                            // force a full repaint the next time the
                            // server sends a frame.
                            terminal.invalidate_frame_cache();
                        }
                    }
                }
            }
        }
    }
}

pub fn list_sessions() -> io::Result<Vec<SessionEntry>> {
    let mut sessions = all_session_socket_entries()?;
    sessions.retain(|entry| session_socket_is_live(&entry.socket_path));
    sessions.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(sessions)
}

fn all_session_socket_entries() -> io::Result<Vec<SessionEntry>> {
    let root = session_root();
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut sessions = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = session_name_from_socket_path(&path) else {
            continue;
        };
        sessions.push(SessionEntry {
            name,
            socket_path: path,
        });
    }
    sessions.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(sessions)
}

fn session_name_from_socket_path(path: &Path) -> Option<String> {
    if path.extension().and_then(|ext| ext.to_str()) != Some("sock") {
        return None;
    }
    let stem = path.file_stem()?.to_str()?;
    if stem.ends_with(".mcp") {
        return None;
    }
    Some(stem.to_string())
}

fn session_socket_is_live(path: &Path) -> bool {
    UnixStream::connect(path).is_ok()
}

fn matching_mcp_socket_path(entry: &SessionEntry) -> PathBuf {
    entry
        .socket_path
        .with_file_name(format!("{}.mcp.sock", entry.name))
}

pub fn prune_stale_sessions(dry_run: bool) -> io::Result<PruneReport> {
    let root = session_root();
    if !root.exists() {
        return Ok(PruneReport {
            removed: Vec::new(),
            kept: Vec::new(),
        });
    }

    let entries = all_session_socket_entries()?;
    let mut kept = Vec::new();
    let mut removed = Vec::new();

    for entry in entries {
        if session_socket_is_live(&entry.socket_path) {
            kept.push(entry);
            continue;
        }
        remove_or_record(&entry.socket_path, dry_run, &mut removed)?;
        let mcp = matching_mcp_socket_path(&entry);
        if mcp.exists() {
            remove_or_record(&mcp, dry_run, &mut removed)?;
        }
    }

    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if !is_mcp_socket_path(&path) {
            continue;
        }
        let normal = normal_socket_for_mcp_path(&path);
        if normal
            .as_ref()
            .is_some_and(|path| session_socket_is_live(path))
        {
            continue;
        }
        remove_or_record(&path, dry_run, &mut removed)?;
    }

    removed.sort();
    kept.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(PruneReport { removed, kept })
}

fn is_mcp_socket_path(path: &Path) -> bool {
    path.extension().and_then(|ext| ext.to_str()) == Some("sock")
        && path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(|stem| stem.ends_with(".mcp"))
}

fn normal_socket_for_mcp_path(path: &Path) -> Option<PathBuf> {
    let stem = path.file_stem()?.to_str()?;
    let normal_stem = stem.strip_suffix(".mcp")?;
    Some(path.with_file_name(format!("{normal_stem}.sock")))
}

fn remove_or_record(path: &Path, dry_run: bool, removed: &mut Vec<PathBuf>) -> io::Result<()> {
    if removed.iter().any(|existing| existing == path) {
        return Ok(());
    }

    removed.push(path.to_path_buf());
    if !dry_run {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

pub fn kill_session(name: &str) -> io::Result<()> {
    validate_session_name(name)?;
    let socket_path = session_socket_path(name);
    match UnixStream::connect(&socket_path) {
        Ok(mut stream) => send_client_message(&mut stream, &ClientMessage::Shutdown),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(error),
        Err(_) => {
            if socket_path.exists() {
                fs::remove_file(socket_path)?;
            }
            Ok(())
        }
    }
}

// One-shot admin message sender used by CLI subcommands like `zmux
// capture`. Opens the session's Unix socket, writes a single encoded
// `ClientMessage`, and closes.
//
// FIRE-AND-FORGET: returns Ok the moment bytes hit the socket;
// downstream errors (file-create failures, missing pane, etc.) are
// reported only via server-side eprintln, so CLI callers should
// hedge their success messages accordingly.
pub fn send_admin_message(name: &str, message: ClientMessage) -> io::Result<()> {
    validate_session_name(name)?;
    let mut stream = UnixStream::connect(session_socket_path(name))?;
    send_client_message(&mut stream, &message)
}

pub fn run_server(name: &str) -> io::Result<i32> {
    validate_session_name(name)?;
    ensure_session_root()?;

    let socket_path = session_socket_path(name);
    if socket_path.exists() {
        let _ = fs::remove_file(&socket_path);
    }

    let listener = UnixListener::bind(&socket_path)?;
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;
    }
    // Any failure after bind (shell spawn, loop body, ...) must still remove
    // the socket file so that stale, unowned sockets do not linger.
    let result = run_server_after_bind(&listener, &socket_path, name);
    let _ = fs::remove_file(&socket_path);
    result
}

fn run_server_after_bind(
    listener: &UnixListener,
    socket_path: &Path,
    name: &str,
) -> io::Result<i32> {
    listener.set_nonblocking(true)?;

    let config = crate::config::Config::load();
    let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    // Start with a single pane in a single window. The attaching client
    // immediately resizes, and the user can split (Ctrl-a |/-) or open
    // additional windows (Ctrl-a c) from there.
    let mut initial = Workspace::spawn_single_named_with_options(
        &shell,
        PtySize::new(24, 80),
        name,
        config.scrollback_lines,
        config.status_bar_hints,
    )?;
    // Push agent config into the workspace so the genesis pane's
    // detectors honor user thresholds and prompt patterns from byte 1.
    initial.set_agent_config(
        Duration::from_millis(config.agent.idle_threshold_ms),
        config.agent.shell_prompts.clone(),
        config.agent.agent_prompts.clone(),
    );
    let mut windows = crate::windows::WindowSet::new(
        initial,
        shell,
        name.to_string(),
        config.scrollback_lines,
        config.status_bar_hints,
        config.status_label.clone(),
    );
    let mut clients: Vec<AttachedClient> = Vec::new();
    let mut registry = CommandRegistry::new();
    register_tier1(&mut registry);

    // Spawn the MCP server alongside the client listener. A bind
    // failure logs and continues without MCP — losing programmatic
    // control beats refusing to start the session at all.
    let mcp_socket_path = mcp::session_mcp_socket_path(name);
    let mcp_rx = match mcp::spawn_listener(mcp_socket_path.clone()) {
        Ok(rx) => Some(rx),
        Err(err) => {
            eprintln!(
                "zmux: failed to bind MCP socket at {}: {err}",
                mcp_socket_path.display()
            );
            None
        }
    };

    // Mutating MCP tool calls (send_keys, spawn_pane, kill_pane,
    // set_label) are recorded per session; see mcp::audit. Only built
    // when the MCP listener actually came up.
    let mut mcp_audit = if mcp_rx.is_some() {
        mcp::AuditLog::open(name)
    } else {
        mcp::AuditLog::disabled()
    };

    let result = run_server_loop(
        listener,
        &mut windows,
        &mut clients,
        socket_path,
        &registry,
        mcp_rx.as_ref(),
        &mut mcp_audit,
    );
    // Mirror the client-socket cleanup from `run_server`: the MCP
    // socket is daemon-scoped, so removing it on shutdown keeps the
    // session directory clean for the next run.
    let _ = std::fs::remove_file(&mcp_socket_path);
    result
}

fn run_server_loop(
    listener: &UnixListener,
    windows: &mut crate::windows::WindowSet,
    clients: &mut Vec<AttachedClient>,
    socket_path: &Path,
    registry: &CommandRegistry,
    mcp_rx: Option<&std::sync::mpsc::Receiver<mcp::McpRequest>>,
    mcp_audit: &mut mcp::AuditLog,
) -> io::Result<i32> {
    // Tracks the last wall-clock second we rendered for a client so the
    // status bar clock ticks once per second even when nothing else
    // changes. Zero means "no frame sent yet in this session."
    let mut last_clock_second: u64 = 0;
    // Deferred MCP requests parked by handlers that returned
    // `Outcome::Defer`. Walked on every iteration via `tick_pending`;
    // see `mcp::execute` module docs for the full lifecycle.
    let mut pending_mcp: Vec<mcp::Pending> = Vec::new();
    loop {
        // If our socket was removed out from under us (e.g. a user manually
        // cleaning /tmp, or a stale-cleanup race), shut down rather than
        // looping forever as an unreachable zombie.
        if !socket_path.exists() {
            for client in clients.iter_mut() {
                let _ = send_server_message(
                    client.stream_mut(),
                    &ServerMessage::Error("session socket was removed".into()),
                );
            }
            return Ok(0);
        }

        let client_count_before = clients.len();
        accept_pending_connections(listener, clients);

        let mut shutdown = false;
        // Accumulate dirty state across every handler in this iteration so
        // we send at most one frame per poll cycle. A batched client message
        // that contains ten Input actions used to produce ten frame writes;
        // now it produces one.
        let mut dirty = false;
        // Indices of clients that detached or errored this iteration.
        let mut to_remove: Vec<usize> = Vec::new();
        // Clipboard replies queued for specific clients (by index).
        let mut clipboard_replies: Vec<(usize, String)> = Vec::new();
        // ListPanes replies queued for specific clients (by index).
        let mut pane_list_replies: Vec<(usize, Vec<crate::protocol::PaneSummary>)> = Vec::new();

        for index in 0..clients.len() {
            let read = read_socket_available(clients[index].stream_mut());
            let bytes = match read {
                Ok(Some(bytes)) => bytes,
                Ok(None) => continue,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::BrokenPipe
                            | io::ErrorKind::ConnectionReset
                            | io::ErrorKind::UnexpectedEof
                    ) =>
                {
                    to_remove.push(index);
                    continue;
                }
                Err(error) => return Err(error),
            };

            let messages = clients[index].decoder.push_bytes(&bytes)?;
            let mut detached_now = false;
            for message in messages {
                match message {
                    ClientMessage::Attach { size } | ClientMessage::Resize { size } => {
                        clients[index].size = size;
                        let new_size = min_client_size(clients);
                        windows.resize(new_size)?;
                        dirty = true;
                    }
                    ClientMessage::Input(bytes) => {
                        // Supervisor overlay (Ctrl-a A) intercepts all
                        // raw key bytes server-side: j/k navigate,
                        // Enter attaches, etc. NEVER forward these
                        // bytes to the focused pane's PTY while the
                        // overlay is open — they are dashboard
                        // controls, not shell input.
                        if windows.supervisor_open() {
                            for &b in bytes.iter() {
                                dirty |= windows.supervisor_handle_key(b)?;
                            }
                            continue;
                        }
                        let client = &mut clients[index];
                        for action in client.input_parser.push_bytes(&bytes) {
                            dirty |= windows.active_mut().handle_input(action)?;
                        }
                        // Mouse-drag release can leave auto-yanked text
                        // queued in the workspace; drain it here and
                        // send it back to THIS client as OSC 52 so the
                        // host terminal writes it to the system clipboard.
                        if let Some(text) = windows.active_mut().take_pending_clipboard() {
                            clipboard_replies.push((index, text));
                            dirty = true;
                        }
                    }
                    ClientMessage::SplitPaneColumns => {
                        dirty |= windows
                            .active_mut()
                            .split_active(SplitOrientation::Columns)?;
                    }
                    ClientMessage::SplitPaneRows => {
                        dirty |= windows.active_mut().split_active(SplitOrientation::Rows)?;
                    }
                    ClientMessage::ClosePane => {
                        dirty |= windows.active_mut().close_active()?;
                    }
                    ClientMessage::CyclePane => {
                        dirty |= windows.active_mut().cycle_active()?;
                    }
                    ClientMessage::CyclePaneBackward => {
                        dirty |= windows.active_mut().cycle_active_backward()?;
                    }
                    ClientMessage::ShowPaneNumbers => {
                        windows
                            .active_mut()
                            .show_pane_numbers(PANE_NUMBER_OVERLAY_DURATION);
                        dirty = true;
                    }
                    ClientMessage::ResizePaneLeft => {
                        dirty |= windows.active_mut().resize_active(ResizeDirection::Left)?;
                    }
                    ClientMessage::ResizePaneRight => {
                        dirty |= windows.active_mut().resize_active(ResizeDirection::Right)?;
                    }
                    ClientMessage::ResizePaneUp => {
                        dirty |= windows.active_mut().resize_active(ResizeDirection::Up)?;
                    }
                    ClientMessage::ResizePaneDown => {
                        dirty |= windows.active_mut().resize_active(ResizeDirection::Down)?;
                    }
                    ClientMessage::PresetTwoColumns => {
                        dirty |= windows
                            .active_mut()
                            .apply_preset(LayoutPreset::TwoColumns)?;
                    }
                    ClientMessage::PresetThreeColumns => {
                        dirty |= windows
                            .active_mut()
                            .apply_preset(LayoutPreset::ThreeColumns)?;
                    }
                    ClientMessage::PresetQuadrants => {
                        dirty |= windows.active_mut().apply_preset(LayoutPreset::Quadrants)?;
                    }
                    ClientMessage::ScrollUp(amount) => {
                        dirty |= windows.active_mut().scroll_active_up(amount as usize);
                    }
                    ClientMessage::ScrollDown(amount) => {
                        dirty |= windows.active_mut().scroll_active_down(amount as usize);
                    }
                    ClientMessage::ScrollToBottom => {
                        dirty |= windows.active_mut().scroll_active_to_bottom();
                    }
                    ClientMessage::YankViewport => {
                        if let Some(text) = windows.active_mut().yank_active_viewport() {
                            clipboard_replies.push((index, text));
                        }
                    }
                    ClientMessage::ToggleZoom => {
                        dirty |= windows.active_mut().toggle_zoom()?;
                    }
                    ClientMessage::SearchBegin => {
                        dirty |= windows.active_mut().begin_search();
                    }
                    ClientMessage::SearchInput(bytes) => {
                        dirty |= windows.active_mut().search_input_bytes(&bytes);
                    }
                    ClientMessage::SearchCommit => {
                        dirty |= windows.active_mut().commit_search();
                    }
                    ClientMessage::SearchCancel => {
                        dirty |= windows.active_mut().cancel_search();
                    }
                    ClientMessage::SearchClear => {
                        dirty |= windows.active_mut().clear_search();
                    }
                    ClientMessage::SearchNext => {
                        dirty |= windows.active_mut().search_next();
                    }
                    ClientMessage::SearchPrev => {
                        dirty |= windows.active_mut().search_prev();
                    }
                    ClientMessage::BeginSelection(kind) => {
                        let mode = match kind {
                            crate::protocol::SelectionKind::Line => {
                                crate::workspace::SelectionMode::Line
                            }
                            crate::protocol::SelectionKind::Char => {
                                crate::workspace::SelectionMode::Char
                            }
                            crate::protocol::SelectionKind::Rect => {
                                crate::workspace::SelectionMode::Rect
                            }
                        };
                        dirty |= windows.active_mut().begin_selection(mode);
                    }
                    ClientMessage::ExtendSelection(direction) => {
                        let terminal_rows = clients[index].size.rows as usize;
                        let half_page = (terminal_rows / 2).max(1);
                        let full_page = terminal_rows.max(1);
                        let workspace = windows.active_mut();
                        dirty |= match direction {
                            crate::protocol::SelectionMove::LineUp => {
                                workspace.extend_selection_up(1)
                            }
                            crate::protocol::SelectionMove::LineDown => {
                                workspace.extend_selection_down(1)
                            }
                            crate::protocol::SelectionMove::HalfPageUp => {
                                workspace.extend_selection_up(half_page)
                            }
                            crate::protocol::SelectionMove::HalfPageDown => {
                                workspace.extend_selection_down(half_page)
                            }
                            crate::protocol::SelectionMove::FullPageUp => {
                                workspace.extend_selection_up(full_page)
                            }
                            crate::protocol::SelectionMove::FullPageDown => {
                                workspace.extend_selection_down(full_page)
                            }
                            crate::protocol::SelectionMove::BufferTop => {
                                workspace.extend_selection_to_top()
                            }
                            crate::protocol::SelectionMove::BufferBottom => {
                                workspace.extend_selection_to_bottom()
                            }
                            crate::protocol::SelectionMove::CharLeft => {
                                workspace.extend_selection_left(1)
                            }
                            crate::protocol::SelectionMove::CharRight => {
                                workspace.extend_selection_right(1)
                            }
                        };
                    }
                    ClientMessage::YankSelection => {
                        if let Some(text) = windows.active_mut().yank_selection() {
                            clipboard_replies.push((index, text));
                            dirty = true;
                        }
                    }
                    ClientMessage::ClearSelection => {
                        dirty |= windows.active_mut().clear_selection();
                    }
                    ClientMessage::SwapPaneNext => {
                        dirty |= windows.active_mut().swap_active_with_next()?;
                    }
                    ClientMessage::SwapPanePrevious => {
                        dirty |= windows.active_mut().swap_active_with_previous()?;
                    }
                    ClientMessage::RenameBegin => {
                        dirty |= windows.active_mut().begin_rename();
                    }
                    ClientMessage::RenameInput(bytes) => {
                        dirty |= windows.active_mut().rename_input_bytes(&bytes);
                    }
                    ClientMessage::RenameCommit => {
                        dirty |= windows.active_mut().commit_rename();
                    }
                    ClientMessage::RenameCancel => {
                        dirty |= windows.active_mut().cancel_rename();
                    }
                    ClientMessage::CommandPromptBegin(kind) => {
                        use crate::protocol::CommandPromptKind;
                        match kind {
                            CommandPromptKind::SplitColumns => {
                                dirty |= windows
                                    .active_mut()
                                    .begin_command_prompt(SplitOrientation::Columns);
                            }
                            CommandPromptKind::SplitRows => {
                                dirty |= windows
                                    .active_mut()
                                    .begin_command_prompt(SplitOrientation::Rows);
                            }
                            CommandPromptKind::General => {
                                dirty |= windows.active_mut().begin_general_command_prompt();
                            }
                        }
                    }
                    ClientMessage::CommandPromptInput(bytes) => {
                        dirty |= windows.active_mut().command_input_bytes(&bytes);
                    }
                    ClientMessage::CommandPromptCommit => {
                        let ws = windows.active_mut();
                        match ws.active_prompt_kind() {
                            Some(PromptKind::General) => {
                                // Consume the prompt buffer.
                                if let Some(line) = ws.commit_general_command_prompt()? {
                                    // Parse and dispatch the command.
                                    match parse_command(&line) {
                                        Err(e) => {
                                            windows.active_mut().set_prompt_error(e.to_string());
                                            dirty = true;
                                        }
                                        Ok(cmds) if cmds.is_empty() => {
                                            // Empty after trim — nothing to do.
                                        }
                                        Ok(mut cmds) => {
                                            let cmd = cmds.remove(0);
                                            let ws = windows.active_mut();
                                            let pane_id = ws.active_pane_id();
                                            let sname = ws.session_name().to_string();
                                            let fmt = FormatContext {
                                                session_name: sname,
                                                session_id: String::new(),
                                                window_index: 0,
                                                window_name: String::new(),
                                                window_id: String::new(),
                                                pane_index: 0,
                                                pane_id: format!("%{}", pane_id),
                                                pane_current_command: String::new(),
                                                host: env::var("HOSTNAME").unwrap_or_default(),
                                                host_short: env::var("HOSTNAME")
                                                    .unwrap_or_default()
                                                    .split('.')
                                                    .next()
                                                    .unwrap_or("")
                                                    .to_string(),
                                                user: env::var("USER").unwrap_or_default(),
                                            };
                                            let mut ctx = CommandContext {
                                                workspace: ws,
                                                caller_pane: pane_id,
                                                caller_client: ClientId(index as u32),
                                                format: fmt,
                                            };
                                            match crate::dispatch::dispatch(
                                                &cmd, registry, &mut ctx,
                                            ) {
                                                Err(e) => {
                                                    windows.active_mut().set_prompt_error(e);
                                                }
                                                Ok(out) => {
                                                    match out.side_effect {
                                                        SideEffect::None => {}
                                                        SideEffect::DisplayMessage(msg) => {
                                                            // Store message in prompt_error slot
                                                            // for now; a proper message overlay
                                                            // comes in a later task.
                                                            windows
                                                                .active_mut()
                                                                .set_prompt_error(msg);
                                                        }
                                                        SideEffect::Detach => {
                                                            detached_now = true;
                                                        }
                                                        SideEffect::ShutdownSession => {
                                                            shutdown = true;
                                                        }
                                                        SideEffect::SwitchSession(_) => {
                                                            // Not implemented yet.
                                                        }
                                                    }
                                                }
                                            }
                                            dirty = true;
                                        }
                                    }
                                } else {
                                    dirty = true;
                                }
                            }
                            _ => {
                                // SplitWith path (or no prompt active).
                                dirty |= windows.active_mut().commit_command_prompt()?;
                            }
                        }
                    }
                    ClientMessage::CommandPromptCancel => {
                        dirty |= windows.active_mut().cancel_command_prompt();
                    }
                    ClientMessage::NewWindow => {
                        // Size the new window to match what clients are
                        // already rendering so the first broadcast after
                        // the switch lands cleanly. `new_window` already
                        // uses the active window's size to spawn.
                        windows.new_window()?;
                        dirty = true;
                    }
                    ClientMessage::CloseWindow => {
                        if windows.close_active_window()? {
                            dirty = true;
                        }
                        // If only one window remains, this is a no-op;
                        // the user can still close panes via Ctrl-a x,
                        // and the server shuts down when the final
                        // shell exits.
                    }
                    ClientMessage::NextWindow => {
                        dirty |= windows.next_window();
                    }
                    ClientMessage::PreviousWindow => {
                        dirty |= windows.previous_window();
                    }
                    ClientMessage::LastWindow => {
                        dirty |= windows.toggle_last_window();
                    }
                    ClientMessage::ToggleSyncPanes => {
                        dirty |= windows.active_mut().toggle_sync_panes();
                    }
                    ClientMessage::CyclePreset => {
                        dirty |= windows.active_mut().cycle_preset()?;
                    }
                    ClientMessage::SelectWindow(index) => {
                        dirty |= windows.select_window(index as usize);
                    }
                    ClientMessage::PasteBuffer => {
                        dirty |= windows.active_mut().paste_buffer_into_active()?;
                    }
                    ClientMessage::Capture { pane_id, path } => {
                        // VT capture admin: open the file and hand it to
                        // the target pane's capture sink. Failures are
                        // logged but don't tear the client down — capture
                        // is a diagnostic aid, not a rendering path.
                        // Lookup walks every window so requests for
                        // background-window panes don't silently capture
                        // the active window's pane with the same id; see
                        // `WindowSet::find_pane_mut`.
                        match std::fs::File::create(&path) {
                            Ok(file) => {
                                if let Some(pane) = windows.find_pane_mut(pane_id as usize) {
                                    pane.attach_capture(Box::new(file));
                                } else {
                                    eprintln!("zmux capture: no pane {pane_id} in any window");
                                }
                            }
                            Err(err) => {
                                eprintln!("zmux capture: cannot create {path}: {err}");
                            }
                        }
                    }
                    ClientMessage::ListPanes => {
                        let summaries = windows
                            .pane_summaries_all()
                            .into_iter()
                            .map(|view| crate::protocol::PaneSummary {
                                pane_id: view.pane.pane_id,
                                label: view.pane.label,
                                state: view.pane.state.as_wire(),
                                last_command: view.pane.last_command,
                                last_exit: view.pane.last_exit,
                                size_cols: view.pane.size_cols,
                                size_rows: view.pane.size_rows,
                            })
                            .collect();
                        pane_list_replies.push((index, summaries));
                    }
                    ClientMessage::OpenSupervisor => {
                        windows.open_supervisor();
                        dirty = true;
                    }
                    ClientMessage::SetLabel { pane_id, label } => {
                        // Cross-window resolution via WindowSet — the
                        // CLI accepts a pane id without a window
                        // qualifier, and the supervisor overlay also
                        // routes through this path on `l` commit.
                        let changed = windows.set_pane_label(pane_id, label);
                        dirty |= changed;
                    }
                    ClientMessage::Detach => {
                        detached_now = true;
                        break;
                    }
                    ClientMessage::Shutdown => {
                        shutdown = true;
                        break;
                    }
                }
            }

            if detached_now {
                to_remove.push(index);
            }
        }

        if shutdown {
            return Ok(0);
        }

        // Send buffered Clipboard replies to the requesting clients. Errors
        // propagate the client into the removal list.
        for (index, text) in clipboard_replies {
            if to_remove.contains(&index) {
                continue;
            }
            if send_server_message(clients[index].stream_mut(), &ServerMessage::Clipboard(text))
                .is_err()
            {
                to_remove.push(index);
            }
        }

        // Same for buffered PaneList admin replies.
        for (index, rows) in pane_list_replies {
            if to_remove.contains(&index) {
                continue;
            }
            if send_server_message(clients[index].stream_mut(), &ServerMessage::PaneList(rows))
                .is_err()
            {
                to_remove.push(index);
            }
        }

        // Remove dropped clients in reverse index order. Recompute pane
        // size if the set changed.
        if !to_remove.is_empty() {
            to_remove.sort_unstable();
            to_remove.dedup();
            for index in to_remove.into_iter().rev() {
                clients.swap_remove(index);
            }
            if !clients.is_empty() {
                let new_size = min_client_size(clients);
                if windows.size() != new_size {
                    windows.resize(new_size)?;
                    dirty = true;
                }
            }
        }

        // Newly-accepted clients deserve an immediate frame so they see
        // the current workspace state rather than staring at a blank
        // terminal until something changes.
        if clients.len() > client_count_before {
            dirty = true;
        }

        // Drain fresh PTY output and reap exited shells. Folded into the
        // same dirty flag so a handler + background output still produces a
        // single frame write.
        if windows.ingest_available_output()? || windows.update_exit_statuses()? {
            dirty = true;
        }

        // Per-frame agent tick. Flips Working → Idle on quiet panes
        // and publishes `PaneStateChanged` across every window —
        // background windows must still report transitions so the
        // supervisor sees the whole session.
        let now = std::time::Instant::now();
        windows.tick_agents(now);

        // Synchronous MCP dispatch against the workspace. Deferred
        // handlers (e.g. spawn_pane wait_for_idle) push into
        // `pending_mcp`; their replies fire from `tick_pending` below.
        if let Some(rx) = mcp_rx
            && mcp::drain_requests(rx, windows, &mut pending_mcp, mcp_audit)
        {
            dirty = true;
        }
        // Complete any deferred requests whose condition has been
        // met or whose deadline has fired. Runs every iteration so
        // the wait granularity is bounded by SERVER_POLL_MS (20ms).
        if mcp::tick_pending(windows, &mut pending_mcp) {
            dirty = true;
        }

        // While the supervisor overlay is open, drain session-bus
        // events from OTHER windows into it so background panes stay
        // live in the dashboard (the active workspace mirrors only
        // its own events).
        if windows.pump_supervisor_events() {
            dirty = true;
        }

        // Clock tick: when any client is attached and the wall-clock
        // second has advanced, force a render so the status bar stays
        // live even during idle periods (no typing, no PTY output).
        if !clients.is_empty() {
            let current = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_secs())
                .unwrap_or(0);
            if current != last_clock_second {
                dirty = true;
                last_clock_second = current;
            }
        }

        if dirty {
            broadcast_frame(clients, windows)?;
        }

        if let Some(exit_code) = windows.exit_code_if_complete() {
            for client in clients.iter_mut() {
                let _ = send_server_message(
                    client.stream_mut(),
                    &ServerMessage::Exited { code: exit_code },
                );
            }
            return Ok(exit_code);
        }

        thread::sleep(Duration::from_millis(SERVER_POLL_MS));
    }
}

fn accept_pending_connections(listener: &UnixListener, clients: &mut Vec<AttachedClient>) {
    while poll_readable(listener.as_raw_fd(), 0).unwrap_or(false) {
        match listener.accept() {
            Ok((stream, _)) => clients.push(AttachedClient::new(stream)),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }
}

// Smallest size across attached clients. The workspace runs at this size
// so every attached client can render every pane; clients with larger
// terminals see empty columns / rows beyond the workspace, which beats
// clipping content for the smaller client.
fn min_client_size(clients: &[AttachedClient]) -> PtySize {
    if clients.is_empty() {
        return PtySize::new(24, 80);
    }
    let rows = clients
        .iter()
        .map(|c| c.size.rows)
        .min()
        .unwrap_or(24)
        .max(4);
    let cols = clients
        .iter()
        .map(|c| c.size.cols)
        .min()
        .unwrap_or(80)
        .max(8);
    PtySize::new(rows, cols)
}

// Render one frame and send it to every attached client. Any client
// that errors on the write gets dropped from the list so a broken
// peer doesn't repeatedly fail the broadcast.
fn broadcast_frame(
    clients: &mut Vec<AttachedClient>,
    windows: &crate::windows::WindowSet,
) -> io::Result<()> {
    if clients.is_empty() {
        return Ok(());
    }
    let frame = ServerMessage::Frame {
        size: windows.size(),
        mouse_tracking_mode: windows.mouse_tracking_mode(),
        lines: windows.active().render_frame(),
        cursor: windows.active().cursor_screen_position(),
    };
    let mut dead: Vec<usize> = Vec::new();
    for (index, client) in clients.iter_mut().enumerate() {
        if let Err(error) = send_server_message(client.stream_mut(), &frame) {
            if matches!(
                error.kind(),
                io::ErrorKind::BrokenPipe
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::UnexpectedEof
            ) {
                dead.push(index);
            } else {
                return Err(error);
            }
        }
    }
    for index in dead.into_iter().rev() {
        clients.swap_remove(index);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptMode {
    Rename,
    CommandColumns,
    CommandRows,
    CommandGeneral,
}

#[derive(Debug, PartialEq, Eq)]
enum ScrollKeyEffect {
    Send(ClientMessage),
    EnterSearchInput,
    EnterSelection(crate::protocol::SelectionKind),
    YankSelectionAndExit,
    CancelSelection,
    ExitScrollMode,
}

// Maps a byte slice (received while in scroll mode) into an ordered
// list of effects: client messages to send, plus mode transitions
// (entering search input, leaving scroll). Vim-style bindings: j/k
// line down/up, g/G top/bottom, Ctrl-U/Ctrl-D half-page, Space
// full-page down, b full-page up. `/` opens a search prompt; n/N
// navigate committed matches. Any other byte exits scroll mode.
fn translate_scroll_keys(bytes: &[u8], terminal_rows: usize) -> Vec<ScrollKeyEffect> {
    let mut effects: Vec<ScrollKeyEffect> = Vec::new();
    let half_page = (terminal_rows / 2).max(1) as u16;
    let full_page = terminal_rows.max(1) as u16;

    for &byte in bytes {
        let send = |msg: ClientMessage| ScrollKeyEffect::Send(msg);
        let next = match byte {
            b'k' => Some(send(ClientMessage::ScrollUp(1))),
            b'j' => Some(send(ClientMessage::ScrollDown(1))),
            b'g' => Some(send(ClientMessage::ScrollUp(u16::MAX))),
            b'G' => Some(send(ClientMessage::ScrollToBottom)),
            b' ' => Some(send(ClientMessage::ScrollDown(full_page))),
            b'b' => Some(send(ClientMessage::ScrollUp(full_page))),
            0x04 => Some(send(ClientMessage::ScrollDown(half_page))), // Ctrl-D
            0x15 => Some(send(ClientMessage::ScrollUp(half_page))),   // Ctrl-U
            b'/' => Some(ScrollKeyEffect::EnterSearchInput),
            b'n' => Some(send(ClientMessage::SearchNext)),
            b'N' => Some(send(ClientMessage::SearchPrev)),
            b'v' => Some(ScrollKeyEffect::EnterSelection(
                crate::protocol::SelectionKind::Char,
            )),
            b'V' => Some(ScrollKeyEffect::EnterSelection(
                crate::protocol::SelectionKind::Line,
            )),
            b'R' => Some(ScrollKeyEffect::EnterSelection(
                crate::protocol::SelectionKind::Rect,
            )),
            // Anything else (q, Escape, Enter, unrecognized) exits
            // scroll mode and stops processing further bytes in this
            // batch — they would be ambiguous after the mode flip.
            _ => None,
        };

        match next {
            Some(effect) => effects.push(effect),
            None => {
                effects.push(ScrollKeyEffect::ExitScrollMode);
                break;
            }
        }
    }

    effects
}

// Key translator for the selection sub-mode. Motion keys extend the
// line selection rather than scrolling — the server moves the selection
// cursor and auto-follows the viewport, so the visible effect is still
// "the view moves with j/k" plus a reverse-video range the user can
// see growing. `y` yanks and exits both modes; `v`/`Esc` cancel the
// selection but stay in scroll mode; anything else falls out entirely.
fn translate_selection_keys(bytes: &[u8]) -> Vec<ScrollKeyEffect> {
    use crate::protocol::SelectionMove;
    let mut effects: Vec<ScrollKeyEffect> = Vec::new();

    for &byte in bytes {
        let extend = |m: SelectionMove| ScrollKeyEffect::Send(ClientMessage::ExtendSelection(m));
        let next = match byte {
            b'k' => Some(extend(SelectionMove::LineUp)),
            b'j' => Some(extend(SelectionMove::LineDown)),
            b'h' => Some(extend(SelectionMove::CharLeft)),
            b'l' => Some(extend(SelectionMove::CharRight)),
            b'g' => Some(extend(SelectionMove::BufferTop)),
            b'G' => Some(extend(SelectionMove::BufferBottom)),
            b' ' => Some(extend(SelectionMove::FullPageDown)),
            b'b' => Some(extend(SelectionMove::FullPageUp)),
            0x04 => Some(extend(SelectionMove::HalfPageDown)), // Ctrl-D
            0x15 => Some(extend(SelectionMove::HalfPageUp)),   // Ctrl-U
            b'y' => Some(ScrollKeyEffect::YankSelectionAndExit),
            b'v' | b'V' | b'R' | 0x1b => Some(ScrollKeyEffect::CancelSelection),
            // Anything else falls all the way out: unexpected bytes
            // would be confusing here, better to snap back to live
            // output than to silently swallow keys.
            _ => None,
        };

        match next {
            Some(ScrollKeyEffect::YankSelectionAndExit) => {
                effects.push(ScrollKeyEffect::YankSelectionAndExit);
                break;
            }
            Some(ScrollKeyEffect::CancelSelection) => {
                effects.push(ScrollKeyEffect::CancelSelection);
                break;
            }
            Some(effect) => effects.push(effect),
            None => {
                effects.push(ScrollKeyEffect::ExitScrollMode);
                break;
            }
        }
    }

    effects
}

// Blocking in-client session picker. Paints a small centered overlay
// with numbered session names, reads keypresses, and returns the
// chosen session name (or None if cancelled). While the picker is up
// we intentionally don't pump server frames — any incoming bytes sit
// in the socket buffer and get processed after the caller resumes.
// A slow render from the current session while the user is staring at
// the picker would just be confusing.
fn run_session_picker(
    terminal: &mut TerminalGuard,
    entries: &[SessionEntry],
) -> io::Result<Option<String>> {
    // Zero other sessions: nothing to pick. Flash a brief message so
    // the user knows their keystroke registered and isn't confused by
    // silence, then clean up.
    if entries.is_empty() {
        let size = terminal.size()?;
        let message = "no other sessions  (Esc to dismiss)";
        let row = size.rows.saturating_sub(1) / 2 + 1;
        let col = size.cols.saturating_sub(message.chars().count() as u16) / 2 + 1;
        terminal.write_ansi(&format!("\x1b[{row};{col}H\x1b[7m {message} \x1b[0m"))?;
        // Wait for any keypress to dismiss.
        loop {
            let input = terminal.read_input(1000)?;
            if !input.is_empty() {
                break;
            }
        }
        terminal.invalidate_frame_cache();
        return Ok(None);
    }

    // Number entries starting at 1 so the user can pick with a single
    // digit. We only render the first 9 — sessions #10+ can still be
    // reached via `zmux attach <name>` from the shell.
    let visible: Vec<&SessionEntry> = entries.iter().take(9).collect();
    let max_name_width = visible
        .iter()
        .map(|e| e.name.chars().count())
        .max()
        .unwrap_or(0);
    // Box width = " N. <name>  " plus padding; ensure minimum readability.
    let inner_width = (max_name_width + 6).max(28);
    let box_width = inner_width + 2; // borders
    let box_height = visible.len() + 4; // title + blank + entries + hint

    let size = terminal.size()?;
    let start_row = size.rows.saturating_sub(box_height as u16) / 2 + 1;
    let start_col = size.cols.saturating_sub(box_width as u16) / 2 + 1;

    let draw = |terminal: &mut TerminalGuard| -> io::Result<()> {
        let mut out = String::new();
        // Top border.
        out.push_str(&format!("\x1b[{start_row};{start_col}H"));
        out.push_str("\x1b[7m");
        out.push('+');
        for _ in 0..inner_width {
            out.push('-');
        }
        out.push('+');
        out.push_str("\x1b[0m");

        // Title row.
        let title = "  Switch session  ";
        let title_pad = inner_width.saturating_sub(title.chars().count());
        let left = title_pad / 2;
        let right = title_pad - left;
        out.push_str(&format!("\x1b[{};{}H", start_row + 1, start_col));
        out.push_str("\x1b[7m|\x1b[0m\x1b[1m");
        for _ in 0..left {
            out.push(' ');
        }
        out.push_str(title);
        for _ in 0..right {
            out.push(' ');
        }
        out.push_str("\x1b[0m\x1b[7m|\x1b[0m");

        // Blank spacer.
        out.push_str(&format!("\x1b[{};{}H", start_row + 2, start_col));
        out.push_str("\x1b[7m|");
        for _ in 0..inner_width {
            out.push(' ');
        }
        out.push_str("|\x1b[0m");

        // Entry rows.
        for (index, entry) in visible.iter().enumerate() {
            let row = start_row + 3 + index as u16;
            out.push_str(&format!("\x1b[{row};{start_col}H"));
            out.push_str("\x1b[7m|\x1b[0m");
            let line = format!(" {}. {}", index + 1, entry.name);
            let pad = inner_width.saturating_sub(line.chars().count());
            out.push_str(&line);
            for _ in 0..pad {
                out.push(' ');
            }
            out.push_str("\x1b[7m|\x1b[0m");
        }

        // Bottom border.
        let bottom_row = start_row + box_height as u16 - 1;
        out.push_str(&format!("\x1b[{bottom_row};{start_col}H"));
        out.push_str("\x1b[7m+");
        for _ in 0..inner_width {
            out.push('-');
        }
        out.push_str("+\x1b[0m");

        terminal.write_ansi(&out)
    };

    draw(terminal)?;

    loop {
        let input = terminal.read_input(1000)?;
        if input.is_empty() {
            // Keep the overlay visible until the user presses something.
            continue;
        }
        for &byte in &input {
            match byte {
                // Esc / q / Ctrl-C cancels.
                0x1b | b'q' | 0x03 => return Ok(None),
                b'1'..=b'9' => {
                    let index = (byte - b'0') as usize - 1;
                    if let Some(entry) = visible.get(index) {
                        return Ok(Some(entry.name.clone()));
                    }
                    // Out of range digit: ignore, wait for another key.
                }
                _ => {
                    // Any other byte: ignore. The user can try again
                    // without the overlay tearing down unexpectedly.
                }
            }
        }
    }
}

fn send_client_message(stream: &mut UnixStream, message: &ClientMessage) -> io::Result<()> {
    let bytes = encode_client_message(message)?;
    stream.write_all(&bytes)
}

fn send_server_message(stream: &mut UnixStream, message: &ServerMessage) -> io::Result<()> {
    let bytes = encode_server_message(message)?;
    stream.write_all(&bytes)
}

fn read_socket_available(stream: &mut UnixStream) -> io::Result<Option<Vec<u8>>> {
    if !poll_readable(stream.as_raw_fd(), 0)? {
        return Ok(None);
    }

    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => {
                if buffer.is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "peer disconnected",
                    ));
                }
                break;
            }
            Ok(count) => buffer.extend_from_slice(&chunk[..count]),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) => return Err(error),
        }

        if !poll_readable(stream.as_raw_fd(), 0)? {
            break;
        }
    }

    Ok(Some(buffer))
}

fn ensure_session_root() -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let root = session_root();
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&root)?;
    // Re-apply 0o700 unconditionally so a pre-existing world-readable
    // directory from an older zmux can't leak socket discovery to other
    // local users sharing /tmp.
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn session_root() -> PathBuf {
    let user = env::var("USER").unwrap_or_else(|_| "user".to_string());
    env::temp_dir().join(format!("zmux-{user}"))
}

fn session_socket_path(name: &str) -> PathBuf {
    session_root().join(format!("{name}.sock"))
}

fn validate_session_name(name: &str) -> io::Result<()> {
    if name.is_empty() || name.contains('/') || name.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "session names must be non-empty and cannot contain '/'",
        ));
    }
    Ok(())
}

#[derive(Debug)]
pub enum AttachOutcome {
    Detached,
    Exited(i32),
    // User picked a different session from the session-picker overlay
    // (Ctrl-a s). The current session is detached cleanly; main.rs
    // re-enters `attach_session` with the new target.
    Switch(String),
}

#[derive(Debug)]
struct AttachedClient {
    stream: UnixStream,
    decoder: ClientDecoder,
    // Separate InputParser per client: two clients each mid-CSI would
    // otherwise stomp each other's partial state.
    input_parser: InputParser,
    // Last size this client reported (via Attach/Resize). Defaults to
    // 24x80 until the first Attach/Resize lands so `min_size` across
    // clients stays sensible.
    size: PtySize,
}

impl AttachedClient {
    fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            decoder: ClientDecoder::default(),
            input_parser: InputParser::default(),
            size: PtySize::new(24, 80),
        }
    }

    fn stream_mut(&mut self) -> &mut UnixStream {
        &mut self.stream
    }
}

#[derive(Debug)]
struct PrefixKeyParser {
    // Byte that flips us into "next key is a zmux binding" mode.
    // Defaults to Ctrl-a (0x01) but config can set a different one so
    // users whose workflows already steal Ctrl-a can stay in zmux.
    prefix_byte: u8,
    pending_prefix: bool,
}

impl Default for PrefixKeyParser {
    fn default() -> Self {
        Self {
            prefix_byte: crate::config::DEFAULT_PREFIX_BYTE,
            pending_prefix: false,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum AttachInput {
    Forward(Vec<u8>),
    Detach,
    LastWindow,
    SplitPaneColumns,
    SplitPaneRows,
    ClosePane,
    CyclePane,
    CyclePaneBackward,
    ShowPaneNumbers,
    OpenSupervisor,
    ResizePaneLeft,
    ResizePaneRight,
    ResizePaneUp,
    ResizePaneDown,
    CyclePreset,
    SelectWindow(u8),
    PasteBuffer,
    EnterScrollback,
    YankViewport,
    ToggleZoom,
    SwapPaneNext,
    SwapPanePrevious,
    ShowSessionPicker,
    BeginRename,
    BeginCommandPromptColumns,
    BeginCommandPromptRows,
    BeginCommandPromptGeneral,
    NewWindow,
    CloseWindow,
    NextWindow,
    PreviousWindow,
    ToggleSyncPanes,
}

impl PrefixKeyParser {
    fn with_prefix(prefix_byte: u8) -> Self {
        Self {
            prefix_byte,
            pending_prefix: false,
        }
    }

    fn push_bytes(&mut self, bytes: &[u8]) -> Vec<AttachInput> {
        let mut actions = Vec::new();
        let mut forward = Vec::new();

        for &byte in bytes {
            if self.pending_prefix {
                self.pending_prefix = false;
                // Double prefix (Ctrl-a Ctrl-a) toggles back to the
                // previously active window, GNU-screen style. The
                // literal prefix byte is still reachable for the
                // shell via `Ctrl-a a` below.
                if byte == self.prefix_byte {
                    if !forward.is_empty() {
                        actions.push(AttachInput::Forward(std::mem::take(&mut forward)));
                    }
                    actions.push(AttachInput::LastWindow);
                    continue;
                }
                // Ctrl-a a: forward one literal prefix byte (screen's
                // convention) so readline's beginning-of-line etc.
                // remain reachable inside panes.
                if byte == b'a' {
                    forward.push(self.prefix_byte);
                    continue;
                }
                let bound = match byte {
                    b'd' => Some(AttachInput::Detach),
                    b'|' => Some(AttachInput::SplitPaneColumns),
                    b'-' => Some(AttachInput::SplitPaneRows),
                    b'x' => Some(AttachInput::ClosePane),
                    b'o' => Some(AttachInput::CyclePane),
                    b'p' => Some(AttachInput::CyclePaneBackward),
                    b'q' => Some(AttachInput::ShowPaneNumbers),
                    b'A' => Some(AttachInput::OpenSupervisor),
                    b'H' => Some(AttachInput::ResizePaneLeft),
                    b'L' => Some(AttachInput::ResizePaneRight),
                    b'K' => Some(AttachInput::ResizePaneUp),
                    b'J' => Some(AttachInput::ResizePaneDown),
                    // Digits select a window by 1-based index (tmux
                    // convention): Ctrl-a 1 → window 0, Ctrl-a 9 →
                    // window 8. Layout presets are reachable via
                    // Ctrl-a Space, which cycles through them.
                    b'1'..=b'9' => Some(AttachInput::SelectWindow(byte - b'1')),
                    b' ' => Some(AttachInput::CyclePreset),
                    b']' => Some(AttachInput::PasteBuffer),
                    b'[' => Some(AttachInput::EnterScrollback),
                    b'y' => Some(AttachInput::YankViewport),
                    b'z' => Some(AttachInput::ToggleZoom),
                    b'{' => Some(AttachInput::SwapPanePrevious),
                    b'}' => Some(AttachInput::SwapPaneNext),
                    b's' => Some(AttachInput::ShowSessionPicker),
                    b',' => Some(AttachInput::BeginRename),
                    b'!' => Some(AttachInput::BeginCommandPromptColumns),
                    b'^' => Some(AttachInput::BeginCommandPromptRows),
                    b':' => Some(AttachInput::BeginCommandPromptGeneral),
                    b'c' => Some(AttachInput::NewWindow),
                    b'n' => Some(AttachInput::NextWindow),
                    b'P' => Some(AttachInput::PreviousWindow),
                    b'&' => Some(AttachInput::CloseWindow),
                    b'=' => Some(AttachInput::ToggleSyncPanes),
                    _ => None,
                };

                if let Some(action) = bound {
                    if !forward.is_empty() {
                        actions.push(AttachInput::Forward(std::mem::take(&mut forward)));
                    }
                    actions.push(action);
                    continue;
                }

                // Unrecognized prefix sequence: forward both bytes literally
                // so the remote shell still sees them.
                forward.push(self.prefix_byte);
            }

            if byte == self.prefix_byte {
                if !forward.is_empty() {
                    actions.push(AttachInput::Forward(std::mem::take(&mut forward)));
                }
                self.pending_prefix = true;
            } else {
                forward.push(byte);
            }
        }

        if !forward.is_empty() {
            actions.push(AttachInput::Forward(forward));
        }

        actions
    }
}

pub fn print_session_list(entries: &[SessionEntry], out: &mut dyn Write) -> io::Result<()> {
    if entries.is_empty() {
        writeln!(out, "no sessions")?;
        return Ok(());
    }

    for entry in entries {
        writeln!(out, "{}\t{}", entry.name, entry.socket_path.display())?;
    }
    Ok(())
}

/// Like `list_sessions`, but opens each live session's socket and
/// pairs it with the `PaneSummary` rows from `ClientMessage::ListPanes`.
/// `list_sessions` filters stale sockets, so an empty pane list here
/// means a race during the query or a live daemon with no panes.
pub fn list_sessions_verbose() -> io::Result<Vec<(SessionEntry, Vec<crate::protocol::PaneSummary>)>>
{
    let entries = list_sessions()?;
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        let panes = query_pane_summaries(&entry.socket_path).unwrap_or_default();
        out.push((entry, panes));
    }
    Ok(out)
}

/// Open a one-shot connection, send `ListPanes`, decode the first
/// `PaneList` reply, return its rows. Mirrors the firing pattern of
/// `send_admin_message` but adds a synchronous read with a small
/// timeout because the daemon polls every 20ms.
fn query_pane_summaries(socket_path: &Path) -> io::Result<Vec<crate::protocol::PaneSummary>> {
    let mut stream = UnixStream::connect(socket_path)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    send_client_message(&mut stream, &ClientMessage::ListPanes)?;
    let mut decoder = ServerDecoder::default();
    let mut buffer = [0u8; 4096];
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        if std::time::Instant::now() > deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "pane-list query timed out",
            ));
        }
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "daemon closed before pane-list reply",
            ));
        }
        let messages = decoder.push_bytes(&buffer[..count])?;
        for message in messages {
            if let ServerMessage::PaneList(rows) = message {
                return Ok(rows);
            }
            // The daemon may emit a Frame on the same socket because
            // the connection counts as a freshly attached client (see
            // `accept_pending_connections` and the "Newly-accepted
            // clients deserve an immediate frame" branch). Discard
            // anything that isn't the reply we asked for.
        }
    }
}

/// Verbose `zmux ls` printer. Renders one session header followed by
/// a tab-separated table of its panes. Empty pane lists print a stub
/// note so the user knows the session was reached but had nothing to
/// report.
pub fn print_session_list_verbose(
    entries: &[(SessionEntry, Vec<crate::protocol::PaneSummary>)],
    out: &mut dyn Write,
) -> io::Result<()> {
    if entries.is_empty() {
        writeln!(out, "no sessions")?;
        return Ok(());
    }
    for (entry, panes) in entries {
        writeln!(out, "{}\t{}", entry.name, entry.socket_path.display())?;
        if panes.is_empty() {
            writeln!(out, "  (no panes — session may be unreachable)")?;
            continue;
        }
        writeln!(
            out,
            "  pane\tlabel\tstate\tcols\trows\tlast_command\tlast_exit"
        )?;
        for pane in panes {
            let label = pane.label.as_deref().unwrap_or("-");
            let cmd = pane.last_command.as_deref().unwrap_or("-");
            let exit = pane
                .last_exit
                .map(|c| c.to_string())
                .unwrap_or_else(|| "-".to_string());
            writeln!(
                out,
                "  {}\t{}\t{}\t{}\t{}\t{}\t{}",
                pane.pane_id, label, pane.state, pane.size_cols, pane.size_rows, cmd, exit,
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{AttachInput, PrefixKeyParser, print_session_list};
    use crate::protocol::ClientMessage;
    use std::path::Path;

    #[test]
    fn ctrl_a_d_detaches() {
        let mut parser = PrefixKeyParser::default();
        let actions = parser.push_bytes(&[0x01, b'd']);

        assert_eq!(actions, vec![AttachInput::Detach]);
    }

    #[test]
    fn ctrl_a_pipe_triggers_vertical_split() {
        let mut parser = PrefixKeyParser::default();
        let actions = parser.push_bytes(&[0x01, b'|']);

        assert_eq!(actions, vec![AttachInput::SplitPaneColumns]);
    }

    #[test]
    fn ctrl_a_dash_triggers_horizontal_split() {
        let mut parser = PrefixKeyParser::default();
        let actions = parser.push_bytes(&[0x01, b'-']);

        assert_eq!(actions, vec![AttachInput::SplitPaneRows]);
    }

    #[test]
    fn ctrl_a_x_closes_active_pane() {
        let mut parser = PrefixKeyParser::default();
        let actions = parser.push_bytes(&[0x01, b'x']);

        assert_eq!(actions, vec![AttachInput::ClosePane]);
    }

    #[test]
    fn ctrl_a_o_cycles_focus() {
        let mut parser = PrefixKeyParser::default();
        let actions = parser.push_bytes(&[0x01, b'o']);

        assert_eq!(actions, vec![AttachInput::CyclePane]);
    }

    #[test]
    fn ctrl_a_p_cycles_focus_backward() {
        let mut parser = PrefixKeyParser::default();
        let actions = parser.push_bytes(&[0x01, b'p']);

        assert_eq!(actions, vec![AttachInput::CyclePaneBackward]);
    }

    #[test]
    fn ctrl_a_open_bracket_enters_scrollback_mode() {
        let mut parser = PrefixKeyParser::default();
        let actions = parser.push_bytes(&[0x01, b'[']);

        assert_eq!(actions, vec![AttachInput::EnterScrollback]);
    }

    #[test]
    fn scroll_key_translation_maps_vim_bindings() {
        use super::ScrollKeyEffect;

        let effects = super::translate_scroll_keys(b"kjG", 40);
        assert_eq!(
            effects,
            vec![
                ScrollKeyEffect::Send(ClientMessage::ScrollUp(1)),
                ScrollKeyEffect::Send(ClientMessage::ScrollDown(1)),
                ScrollKeyEffect::Send(ClientMessage::ScrollToBottom),
            ]
        );

        // Half-page vs full-page scale with terminal height.
        let half = super::translate_scroll_keys(&[0x04], 40);
        assert_eq!(
            half,
            vec![ScrollKeyEffect::Send(ClientMessage::ScrollDown(20))]
        );

        let full = super::translate_scroll_keys(b" ", 40);
        assert_eq!(
            full,
            vec![ScrollKeyEffect::Send(ClientMessage::ScrollDown(40))]
        );

        // 'q' or any unrecognized byte leaves scroll mode.
        let exit = super::translate_scroll_keys(b"q", 40);
        assert_eq!(exit, vec![ScrollKeyEffect::ExitScrollMode]);
    }

    #[test]
    fn scroll_key_translation_routes_search_keys() {
        use super::ScrollKeyEffect;

        // `/` opens the search prompt; n / N navigate matches.
        let effects = super::translate_scroll_keys(b"/nN", 40);
        assert_eq!(
            effects,
            vec![
                ScrollKeyEffect::EnterSearchInput,
                ScrollKeyEffect::Send(ClientMessage::SearchNext),
                ScrollKeyEffect::Send(ClientMessage::SearchPrev),
            ]
        );
    }

    #[test]
    fn ctrl_a_digit_selects_window_by_index() {
        let mut parser = PrefixKeyParser::default();
        let actions = parser.push_bytes(&[0x01, b'1']);
        assert_eq!(actions, vec![AttachInput::SelectWindow(0)]);

        let mut parser = PrefixKeyParser::default();
        let actions = parser.push_bytes(&[0x01, b'9']);
        assert_eq!(actions, vec![AttachInput::SelectWindow(8)]);
    }

    #[test]
    fn ctrl_a_space_cycles_layout_presets() {
        let mut parser = PrefixKeyParser::default();
        let actions = parser.push_bytes(&[0x01, b' ']);
        assert_eq!(actions, vec![AttachInput::CyclePreset]);
    }

    #[test]
    fn ctrl_a_q_shows_pane_numbers() {
        let mut parser = PrefixKeyParser::default();
        let actions = parser.push_bytes(&[0x01, b'q']);

        assert_eq!(actions, vec![AttachInput::ShowPaneNumbers]);
    }

    #[test]
    fn unbound_prefix_forwards_literal_bytes() {
        let mut parser = PrefixKeyParser::default();
        // '~' isn't bound; the parser should emit both bytes unchanged.
        let actions = parser.push_bytes(&[0x01, b'~']);

        assert_eq!(actions, vec![AttachInput::Forward(vec![0x01, b'~'])]);
    }

    #[test]
    fn prefix_parser_honors_configured_prefix_byte() {
        // Ctrl-s (0x13) as the prefix. With Ctrl-a no longer reserved,
        // the byte 0x01 must forward straight to the shell.
        let mut parser = PrefixKeyParser::with_prefix(0x13);
        let ctrl_a_passthrough = parser.push_bytes(&[0x01]);
        assert_eq!(
            ctrl_a_passthrough,
            vec![AttachInput::Forward(vec![0x01])],
            "Ctrl-a should pass through when the configured prefix is Ctrl-s",
        );

        let detach = parser.push_bytes(&[0x13, b'd']);
        assert_eq!(detach, vec![AttachInput::Detach]);
    }

    #[test]
    fn prefix_that_straddles_two_reads_still_binds() {
        // The pending-prefix flag lives on the parser, so a 0x01 that
        // arrives on its own in one read and a binding byte in the next
        // read must still resolve to the bound action.
        let mut parser = PrefixKeyParser::default();
        let first = parser.push_bytes(&[0x01]);
        let second = parser.push_bytes(b"d");

        assert!(first.is_empty());
        assert_eq!(second, vec![AttachInput::Detach]);
    }

    #[test]
    fn session_name_from_socket_path_ignores_mcp_bridge_sockets() {
        assert_eq!(
            super::session_name_from_socket_path(Path::new("/tmp/zmux-user/alpha.sock")).as_deref(),
            Some("alpha"),
        );
        assert_eq!(
            super::session_name_from_socket_path(Path::new("/tmp/zmux-user/alpha.mcp.sock")),
            None,
        );
        assert_eq!(
            super::session_name_from_socket_path(Path::new("/tmp/zmux-user/alpha.txt")),
            None,
        );
    }

    #[test]
    fn session_list_is_human_readable() {
        let entries = vec![super::SessionEntry {
            name: "alpha".into(),
            socket_path: "/tmp/zmux-user/alpha.sock".into(),
        }];
        let mut output = Vec::new();
        print_session_list(&entries, &mut output).expect("render session list");

        assert_eq!(
            String::from_utf8(output).expect("utf-8"),
            "alpha\t/tmp/zmux-user/alpha.sock\n"
        );
    }
}
