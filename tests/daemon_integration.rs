// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use zmux::PtySize;
use zmux::protocol::{ClientMessage, ServerDecoder, ServerMessage, encode_client_message};

unsafe extern "C" {
    fn setsid() -> i32;
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const READ_TIMEOUT: Duration = Duration::from_secs(3);

fn socket_path(name: &str) -> PathBuf {
    let user = std::env::var("USER").unwrap_or_else(|_| "user".to_string());
    std::env::temp_dir()
        .join(format!("zmux-{user}"))
        .join(format!("{name}.sock"))
}

fn spawn_server_with_setsid(name: &str) -> Child {
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

    command.spawn().expect("spawn zmux serve")
}

fn wait_for_socket(path: &Path) {
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    while !path.exists() {
        if Instant::now() > deadline {
            panic!("timed out waiting for daemon socket at {}", path.display());
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for_removal(path: &Path) {
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    while path.exists() {
        if Instant::now() > deadline {
            panic!("socket was not removed after shutdown: {}", path.display());
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn connect(path: &Path) -> UnixStream {
    let stream = UnixStream::connect(path).expect("connect to daemon socket");
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .expect("set read timeout");
    stream
}

fn read_message(stream: &mut UnixStream) -> ServerMessage {
    let mut decoder = ServerDecoder::default();
    let mut buffer = [0u8; 8192];
    let deadline = Instant::now() + READ_TIMEOUT;
    loop {
        if Instant::now() > deadline {
            panic!("timed out waiting for server message");
        }
        match stream.read(&mut buffer) {
            Ok(0) => panic!("daemon closed the socket unexpectedly"),
            Ok(n) => {
                let messages = decoder.push_bytes(&buffer[..n]).expect("decode frame");
                if let Some(message) = messages.into_iter().next() {
                    return message;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(error) => panic!("socket read error: {error}"),
        }
    }
}

fn send(stream: &mut UnixStream, message: &ClientMessage) {
    let bytes = encode_client_message(message).expect("encode client message");
    stream.write_all(&bytes).expect("write to daemon");
}

#[test]
fn daemon_attach_detach_reattach_shutdown() {
    let name = format!("it-lifecycle-{}", std::process::id());
    let path = socket_path(&name);
    let _ = std::fs::remove_file(&path);

    // Spawning under setsid exercises the O_NOCTTY regression path: the
    // daemon becomes a session leader with no controlling terminal, so the
    // PTY slave open in spawn_two_pane must not claim the slave as the
    // daemon's ctty or the forked shell's TIOCSCTTY will fail with EPERM.
    let mut child = spawn_server_with_setsid(&name);
    wait_for_socket(&path);

    let size = PtySize::new(24, 80);

    // Attach and confirm the server sends a rendered frame.
    let mut first = connect(&path);
    send(&mut first, &ClientMessage::Attach { size });
    match read_message(&mut first) {
        ServerMessage::Frame {
            size: reported,
            lines,
            ..
        } => {
            assert_eq!(reported, size);
            assert_eq!(lines.len(), size.rows as usize);
        }
        other => panic!("expected Frame, got {other:?}"),
    }

    // A second concurrent attach is allowed — both clients see the
    // workspace. The server now broadcasts frames to all clients.
    let mut second_client = connect(&path);
    send(&mut second_client, &ClientMessage::Attach { size });
    match read_message(&mut second_client) {
        ServerMessage::Frame { .. } => {}
        other => panic!("expected Frame for second attach, got {other:?}"),
    }
    drop(second_client);

    // Detach, wait for the server poll loop to register the drop, then
    // reattach and confirm the session survived.
    send(&mut first, &ClientMessage::Detach);
    drop(first);
    thread::sleep(Duration::from_millis(150));

    let mut second = connect(&path);
    send(&mut second, &ClientMessage::Attach { size });
    match read_message(&mut second) {
        ServerMessage::Frame { .. } => {}
        other => panic!("expected Frame after reattach, got {other:?}"),
    }

    // Shutdown should exit the server and remove the socket file.
    send(&mut second, &ClientMessage::Shutdown);
    drop(second);

    wait_for_removal(&path);

    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        match child.try_wait().expect("poll child") {
            Some(status) => {
                assert!(status.success(), "daemon exited with {status}");
                break;
            }
            None => {
                if Instant::now() > deadline {
                    let _ = child.kill();
                    panic!("daemon did not exit after shutdown");
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

fn drain_frame(stream: &mut UnixStream) -> Option<ServerMessage> {
    match read_message(stream) {
        frame @ ServerMessage::Frame { .. } => Some(frame),
        other => panic!("expected Frame, got {other:?}"),
    }
}

// Strip ANSI SGR escapes so we can count visible characters.
fn strip_ansi(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            // Skip until a final byte in 0x40..=0x7e.
            let mut j = i + 2;
            while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                j += 1;
            }
            i = j + 1;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn count_panes_in_frame(frame: &ServerMessage) -> usize {
    let ServerMessage::Frame { lines, .. } = frame else {
        panic!("not a frame");
    };
    // Each pane header starts at column 0 with either '[' (unfocused) or '*['
    // (focused). Counting headers on the first row gives us the pane count
    // without needing to parse the whole layout. Strip ANSI so SGR escape
    // brackets don't inflate the count.
    lines
        .first()
        .map(|row| strip_ansi(row).matches('[').count())
        .unwrap_or(0)
}

// Pull the (active, total) pane indices out of the status bar, which lives
// on the last rendered row and contains a "{N} of {M}" segment between the
// label cluster on the left and the clock on the right. Returns 1-indexed
// values the way the server prints them.
fn parse_active_pane(frame: &ServerMessage) -> (usize, usize) {
    let ServerMessage::Frame { lines, .. } = frame else {
        panic!("not a frame");
    };
    let status = lines.last().map(|row| strip_ansi(row)).expect("status row");
    let status = &status;
    // Find the " of " anchor and walk backward/forward for the two numbers.
    let bytes = status.as_bytes();
    let of_pos = status
        .find(" of ")
        .unwrap_or_else(|| panic!("unexpected status bar: {status:?}"));

    // Walk left from `of_pos` across digits to read the active count.
    let mut cursor = of_pos;
    while cursor > 0 && bytes[cursor - 1].is_ascii_digit() {
        cursor -= 1;
    }
    let active: usize = status[cursor..of_pos]
        .parse()
        .unwrap_or_else(|_| panic!("active pane index in {status:?}"));

    // Walk right from after " of " across digits for the total.
    let total_start = of_pos + " of ".len();
    let mut end = total_start;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    let total: usize = status[total_start..end]
        .parse()
        .unwrap_or_else(|_| panic!("pane count in {status:?}"));

    (active, total)
}

#[test]
fn split_active_increases_pane_count_in_frames() {
    let name = format!("it-split-{}", std::process::id());
    let path = socket_path(&name);
    let _ = std::fs::remove_file(&path);

    let mut child = spawn_server_with_setsid(&name);
    wait_for_socket(&path);

    let size = PtySize::new(24, 80);
    let mut stream = connect(&path);
    send(&mut stream, &ClientMessage::Attach { size });

    let initial = drain_frame(&mut stream).expect("initial frame");
    assert_eq!(
        count_panes_in_frame(&initial),
        1,
        "workspace starts with a single pane"
    );

    send(&mut stream, &ClientMessage::SplitPaneColumns);
    let after_split = drain_frame(&mut stream).expect("frame after split");
    assert_eq!(
        count_panes_in_frame(&after_split),
        2,
        "split should produce a second pane"
    );

    send(&mut stream, &ClientMessage::ClosePane);
    let after_close = drain_frame(&mut stream).expect("frame after close");
    assert_eq!(
        count_panes_in_frame(&after_close),
        1,
        "close should drop the workspace back to a single pane"
    );

    // Clean up.
    send(&mut stream, &ClientMessage::Shutdown);
    drop(stream);
    wait_for_removal(&path);

    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        match child.try_wait().expect("poll child") {
            Some(status) => {
                assert!(status.success(), "daemon exited with {status}");
                break;
            }
            None => {
                if Instant::now() > deadline {
                    let _ = child.kill();
                    panic!("daemon did not exit after shutdown");
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

#[test]
fn horizontal_and_vertical_splits_produce_dividers_on_both_axes() {
    // Builds a 2x2-ish layout: start with two columns, then split the
    // right column horizontally. The rendered frame should contain both
    // a '|' (vertical divider) and a '-' (horizontal divider).
    let name = format!("it-cross-{}", std::process::id());
    let path = socket_path(&name);
    let _ = std::fs::remove_file(&path);

    let mut child = spawn_server_with_setsid(&name);
    wait_for_socket(&path);

    let size = PtySize::new(24, 80);
    let mut stream = connect(&path);
    send(&mut stream, &ClientMessage::Attach { size });
    let _ = drain_frame(&mut stream);

    // Build a mixed layout: first split into two columns, then split the
    // right column into two rows. Result should have both a '|' divider
    // (between the columns) and a '-' divider (between the stacked rows).
    send(&mut stream, &ClientMessage::SplitPaneColumns);
    let _ = drain_frame(&mut stream).expect("frame after column split");
    send(&mut stream, &ClientMessage::SplitPaneRows);
    let after = drain_frame(&mut stream).expect("frame after row split");
    let ServerMessage::Frame { lines, .. } = &after else {
        panic!("not a frame");
    };

    // Status row sits on the last row, so dividers should be present on
    // body rows before it.
    let body: String = lines
        .iter()
        .take(lines.len() - 1)
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        body.contains('|'),
        "expected vertical divider from the initial column split"
    );
    assert!(
        body.contains('-'),
        "expected horizontal divider from the nested row split"
    );

    // Three panes now: the original left column, and two stacked on the
    // right.
    assert_eq!(parse_active_pane(&after).1, 3);

    send(&mut stream, &ClientMessage::Shutdown);
    drop(stream);
    wait_for_removal(&path);

    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        match child.try_wait().expect("poll child") {
            Some(status) => {
                assert!(status.success(), "daemon exited with {status}");
                break;
            }
            None => {
                if Instant::now() > deadline {
                    let _ = child.kill();
                    panic!("daemon did not exit after shutdown");
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

#[test]
fn split_cycle_close_leaves_focus_on_a_sane_pane() {
    // Exercises the close_active focus-clamp logic end-to-end: split a few
    // times, cycle focus around, close the active pane, and confirm the
    // new active index is in range and the workspace still renders.
    let name = format!("it-focus-{}", std::process::id());
    let path = socket_path(&name);
    let _ = std::fs::remove_file(&path);

    let mut child = spawn_server_with_setsid(&name);
    wait_for_socket(&path);

    let size = PtySize::new(24, 80);
    let mut stream = connect(&path);
    send(&mut stream, &ClientMessage::Attach { size });

    let initial = drain_frame(&mut stream).expect("initial frame");
    let (active, total) = parse_active_pane(&initial);
    assert_eq!(total, 1, "fresh workspace starts single-pane");
    assert_eq!(active, 1);

    // Three splits: 1 → 2 → 3 → 4 panes. Each split focuses the newly
    // inserted pane to the right of the previously active one.
    send(&mut stream, &ClientMessage::SplitPaneColumns);
    let after_first = drain_frame(&mut stream).expect("frame after split 1");
    assert_eq!(parse_active_pane(&after_first), (2, 2));

    send(&mut stream, &ClientMessage::SplitPaneColumns);
    let after_second = drain_frame(&mut stream).expect("frame after split 2");
    assert_eq!(parse_active_pane(&after_second), (3, 3));

    send(&mut stream, &ClientMessage::SplitPaneColumns);
    let after_third = drain_frame(&mut stream).expect("frame after split 3");
    assert_eq!(parse_active_pane(&after_third), (4, 4));

    // Cycle backward twice to land on pane 2.
    send(&mut stream, &ClientMessage::CyclePaneBackward);
    let after_back1 = drain_frame(&mut stream).expect("frame after prev 1");
    assert_eq!(parse_active_pane(&after_back1), (3, 4));
    send(&mut stream, &ClientMessage::CyclePaneBackward);
    let after_back2 = drain_frame(&mut stream).expect("frame after prev 2");
    assert_eq!(parse_active_pane(&after_back2), (2, 4));

    // Close the focused pane. Focus should stay in bounds — the tree
    // collapse picks the first remaining leaf.
    send(&mut stream, &ClientMessage::ClosePane);
    let after_close = drain_frame(&mut stream).expect("frame after close");
    let (active_after, total_after) = parse_active_pane(&after_close);
    assert_eq!(total_after, 3, "one pane removed");
    assert!(
        (1..=total_after).contains(&active_after),
        "active={active_after} must stay within 1..={total_after}",
    );

    send(&mut stream, &ClientMessage::Shutdown);
    drop(stream);
    wait_for_removal(&path);

    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        match child.try_wait().expect("poll child") {
            Some(status) => {
                assert!(status.success(), "daemon exited with {status}");
                break;
            }
            None => {
                if Instant::now() > deadline {
                    let _ = child.kill();
                    panic!("daemon did not exit after shutdown");
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

#[test]
fn two_clients_both_receive_frames_when_state_changes() {
    // Multi-client attachment: both clients attach concurrently, both
    // get an initial frame, and a pane split from one of them produces
    // fresh frames on both.
    let name = format!("it-multi-{}", std::process::id());
    let path = socket_path(&name);
    let _ = std::fs::remove_file(&path);

    let mut child = spawn_server_with_setsid(&name);
    wait_for_socket(&path);

    let size = PtySize::new(24, 80);
    let mut a = connect(&path);
    let mut b = connect(&path);
    send(&mut a, &ClientMessage::Attach { size });
    send(&mut b, &ClientMessage::Attach { size });

    // Each client's initial Attach should produce a Frame for them.
    match read_message(&mut a) {
        ServerMessage::Frame { .. } => {}
        other => panic!("client A expected Frame, got {other:?}"),
    }
    match read_message(&mut b) {
        ServerMessage::Frame { .. } => {}
        other => panic!("client B expected Frame, got {other:?}"),
    }

    // A splits. Both clients should receive a new frame reflecting the
    // new pane count.
    send(&mut a, &ClientMessage::SplitPaneColumns);
    let frame_a = drain_frame(&mut a).expect("A frame after split");
    let frame_b = drain_frame(&mut b).expect("B frame after split");
    assert_eq!(count_panes_in_frame(&frame_a), 2);
    assert_eq!(count_panes_in_frame(&frame_b), 2);

    send(&mut a, &ClientMessage::Shutdown);
    drop(a);
    drop(b);
    wait_for_removal(&path);

    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        match child.try_wait().expect("poll child") {
            Some(status) => {
                assert!(status.success(), "daemon exited with {status}");
                break;
            }
            None => {
                if Instant::now() > deadline {
                    let _ = child.kill();
                    panic!("daemon did not exit after shutdown");
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

#[test]
fn daemon_exits_when_socket_is_removed_externally() {
    let name = format!("it-orphan-{}", std::process::id());
    let path = socket_path(&name);
    let _ = std::fs::remove_file(&path);

    let mut child = spawn_server_with_setsid(&name);
    wait_for_socket(&path);

    // Removing the socket file out-of-band used to leave the daemon spinning
    // forever with no way to kill it via the CLI. The server should notice
    // and shut down on its own.
    std::fs::remove_file(&path).expect("remove socket");

    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        match child.try_wait().expect("poll child") {
            Some(status) => {
                assert!(status.success(), "orphaned daemon exited with {status}");
                break;
            }
            None => {
                if Instant::now() > deadline {
                    let _ = child.kill();
                    panic!("daemon did not exit after socket was removed");
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

fn read_frame_with(stream: &mut UnixStream, needle: &str) -> Vec<String> {
    let deadline = Instant::now() + READ_TIMEOUT;
    loop {
        if Instant::now() > deadline {
            panic!("timed out waiting for frame containing {needle:?}");
        }
        match read_message(stream) {
            ServerMessage::Frame { lines, .. }
                if lines.iter().any(|line| line.contains(needle)) =>
            {
                return lines;
            }
            ServerMessage::Error(message) => panic!("server error: {message}"),
            ServerMessage::Exited { code } => panic!("server exited with code {code}"),
            _ => {}
        }
    }
}

#[test]
fn toggle_zoom_round_trips_over_the_socket() {
    let name = format!("it-zoom-{}", std::process::id());
    let path = socket_path(&name);
    let _ = std::fs::remove_file(&path);

    let mut child = spawn_server_with_setsid(&name);
    wait_for_socket(&path);

    let size = PtySize::new(24, 80);
    let mut client = connect(&path);
    send(&mut client, &ClientMessage::Attach { size });
    match read_message(&mut client) {
        ServerMessage::Frame { lines, .. } => {
            assert!(
                !lines.iter().any(|line| line.contains("[Z]")),
                "fresh session should not show [Z]",
            );
        }
        other => panic!("expected initial Frame, got {other:?}"),
    }

    send(&mut client, &ClientMessage::ToggleZoom);
    let lines = read_frame_with(&mut client, "[Z]");
    assert!(
        lines.iter().any(|line| line.contains("[Z]")),
        "zoom status tag missing from status bar",
    );

    send(&mut client, &ClientMessage::Shutdown);
    drop(client);
    wait_for_removal(&path);
    let _ = child.wait();
}

#[test]
fn toggle_zoom_after_split_shows_only_one_pane_header() {
    let name = format!("it-zoom-split-{}", std::process::id());
    let path = socket_path(&name);
    let _ = std::fs::remove_file(&path);

    let mut child = spawn_server_with_setsid(&name);
    wait_for_socket(&path);

    let size = PtySize::new(24, 80);
    let mut client = connect(&path);
    send(&mut client, &ClientMessage::Attach { size });
    let _ = read_message(&mut client);

    // After a split the frame must have two pane header lines
    // (lines starting with `*[pane-` or ` [pane-`). Drain frames until
    // we see the 2-header snapshot.
    send(&mut client, &ClientMessage::SplitPaneColumns);
    let two_pane = {
        let deadline = Instant::now() + READ_TIMEOUT;
        loop {
            if Instant::now() > deadline {
                panic!("timed out waiting for 2-pane frame");
            }
            if let ServerMessage::Frame { lines, .. } = read_message(&mut client) {
                let headers: usize = lines
                    .iter()
                    .map(|line| line.matches("[pane-").count())
                    .sum();
                if headers >= 2 {
                    break lines;
                }
            }
        }
    };
    assert!(
        two_pane.iter().any(|line| line.contains("[pane-1")),
        "split frame missing pane-1 header",
    );
    assert!(
        two_pane.iter().any(|line| line.contains("[pane-2")),
        "split frame missing pane-2 header",
    );

    // Zoom in. Only ONE pane header should remain, plus `[Z]` in the
    // status bar.
    send(&mut client, &ClientMessage::ToggleZoom);
    let zoomed = read_frame_with(&mut client, "[Z]");
    let zoomed_headers: usize = zoomed
        .iter()
        .map(|line| line.matches("[pane-").count())
        .sum();
    assert_eq!(
        zoomed_headers, 1,
        "zoomed layout should have exactly one pane header, got {zoomed_headers}: {zoomed:?}",
    );

    // Zoom back out. Both headers reappear and [Z] is gone.
    send(&mut client, &ClientMessage::ToggleZoom);
    let restored = {
        let deadline = Instant::now() + READ_TIMEOUT;
        loop {
            if Instant::now() > deadline {
                panic!("timed out waiting for restored 2-pane frame");
            }
            if let ServerMessage::Frame { lines, .. } = read_message(&mut client) {
                let headers: usize = lines
                    .iter()
                    .map(|line| line.matches("[pane-").count())
                    .sum();
                if headers >= 2 {
                    break lines;
                }
            }
        }
    };
    assert!(
        !restored.iter().any(|line| line.contains("[Z]")),
        "[Z] tag should be gone after unzoom",
    );

    send(&mut client, &ClientMessage::Shutdown);
    drop(client);
    wait_for_removal(&path);
    let _ = child.wait();
}

#[test]
fn new_window_and_cycle_over_the_socket() {
    let name = format!("it-windows-{}", std::process::id());
    let path = socket_path(&name);
    let _ = std::fs::remove_file(&path);

    let mut child = spawn_server_with_setsid(&name);
    wait_for_socket(&path);

    let size = PtySize::new(24, 80);
    let mut client = connect(&path);
    send(&mut client, &ClientMessage::Attach { size });
    // Initial frame — single window, no [w:...] indicator.
    match read_message(&mut client) {
        ServerMessage::Frame { lines, .. } => {
            assert!(
                !lines.iter().any(|line| line.contains("[w:")),
                "single-window session should not advertise a window indicator",
            );
        }
        other => panic!("expected initial Frame, got {other:?}"),
    }

    send(&mut client, &ClientMessage::NewWindow);
    let after_new = read_frame_with(&mut client, "[w:2/2]");
    assert!(
        after_new.iter().any(|line| line.contains("[w:2/2]")),
        "NewWindow must flip the indicator to 2/2: {after_new:?}",
    );

    send(&mut client, &ClientMessage::PreviousWindow);
    let after_prev = read_frame_with(&mut client, "[w:1/2]");
    assert!(
        after_prev.iter().any(|line| line.contains("[w:1/2]")),
        "PreviousWindow must move back to window 1/2: {after_prev:?}",
    );

    send(&mut client, &ClientMessage::NextWindow);
    let after_next = read_frame_with(&mut client, "[w:2/2]");
    assert!(after_next.iter().any(|line| line.contains("[w:2/2]")));

    send(&mut client, &ClientMessage::CloseWindow);
    // Wait for a frame that no longer has the indicator (only one
    // window left = single-window session again).
    let deadline = Instant::now() + READ_TIMEOUT;
    let mut saw_single = false;
    while Instant::now() < deadline && !saw_single {
        if let ServerMessage::Frame { lines, .. } = read_message(&mut client)
            && !lines.iter().any(|line| line.contains("[w:"))
        {
            saw_single = true;
        }
    }
    assert!(
        saw_single,
        "CloseWindow should bring us back to a single-window UI"
    );

    send(&mut client, &ClientMessage::Shutdown);
    drop(client);
    wait_for_removal(&path);
    let _ = child.wait();
}

#[test]
fn rename_wire_path_updates_pane_header() {
    let name = format!("it-rename-{}", std::process::id());
    let path = socket_path(&name);
    let _ = std::fs::remove_file(&path);

    let mut child = spawn_server_with_setsid(&name);
    wait_for_socket(&path);

    let size = PtySize::new(24, 80);
    let mut client = connect(&path);
    send(&mut client, &ClientMessage::Attach { size });
    let _ = read_message(&mut client);

    send(&mut client, &ClientMessage::RenameBegin);
    let lines = read_frame_with(&mut client, "rename:");
    assert!(
        lines.iter().any(|line| line.contains("rename:")),
        "status bar must show the rename prompt: {lines:?}",
    );

    send(&mut client, &ClientMessage::RenameInput(b"worker".to_vec()));
    send(&mut client, &ClientMessage::RenameCommit);
    let lines = read_frame_with(&mut client, "[worker");
    assert!(
        lines.iter().any(|line| line.contains("[worker")),
        "pane header must reflect the new title: {lines:?}",
    );

    send(&mut client, &ClientMessage::Shutdown);
    drop(client);
    wait_for_removal(&path);
    let _ = child.wait();
}

#[test]
fn swap_pane_next_reorders_pane_headers() {
    let name = format!("it-swap-{}", std::process::id());
    let path = socket_path(&name);
    let _ = std::fs::remove_file(&path);

    let mut child = spawn_server_with_setsid(&name);
    wait_for_socket(&path);

    let size = PtySize::new(24, 80);
    let mut client = connect(&path);
    send(&mut client, &ClientMessage::Attach { size });
    let _ = read_message(&mut client);

    // Side-by-side split so both pane headers live on row 0 — their
    // left-to-right order is visible in line 0 of the frame.
    send(&mut client, &ClientMessage::SplitPaneColumns);
    let header_line = |lines: &[String]| -> Option<String> {
        lines
            .iter()
            .find(|line| line.matches("[pane-").count() >= 2)
            .cloned()
    };

    let baseline = {
        let deadline = Instant::now() + READ_TIMEOUT;
        loop {
            if Instant::now() > deadline {
                panic!("timed out waiting for 2-pane frame");
            }
            if let ServerMessage::Frame { lines, .. } = read_message(&mut client)
                && let Some(row) = header_line(&lines)
            {
                break row;
            }
        }
    };
    // Baseline: pane-1 to the left of pane-2. Verify, so the reorder
    // assertion below is unambiguous.
    let p1 = baseline.find("[pane-1").expect("pane-1 in baseline");
    let p2 = baseline.find("[pane-2").expect("pane-2 in baseline");
    assert!(p1 < p2, "baseline should read pane-1 on the left");

    send(&mut client, &ClientMessage::SwapPaneNext);
    let swapped = {
        let deadline = Instant::now() + READ_TIMEOUT;
        loop {
            if Instant::now() > deadline {
                panic!("timed out waiting for swapped frame");
            }
            if let ServerMessage::Frame { lines, .. } = read_message(&mut client)
                && let Some(row) = header_line(&lines)
            {
                let sp1 = row.find("[pane-1").expect("pane-1 after swap");
                let sp2 = row.find("[pane-2").expect("pane-2 after swap");
                if sp2 < sp1 {
                    break row;
                }
            }
        }
    };
    let sp1 = swapped.find("[pane-1").unwrap();
    let sp2 = swapped.find("[pane-2").unwrap();
    assert!(sp2 < sp1, "swap should place pane-2 on the left");

    send(&mut client, &ClientMessage::Shutdown);
    drop(client);
    wait_for_removal(&path);
    let _ = child.wait();
}

#[test]
fn begin_selection_surfaces_sel_tag_and_yank_returns_clipboard() {
    let name = format!("it-select-{}", std::process::id());
    let path = socket_path(&name);
    let _ = std::fs::remove_file(&path);

    let mut child = spawn_server_with_setsid(&name);
    wait_for_socket(&path);

    let size = PtySize::new(24, 80);
    let mut client = connect(&path);
    send(&mut client, &ClientMessage::Attach { size });
    let _ = read_message(&mut client);

    // Starting a selection must advertise itself in the status bar so
    // a glance tells the user they're in copy mode. The line count is
    // 0 for a freshly-spawned pane (no scrollback yet), which is the
    // correct honest reading — don't claim "1 line" when the buffer
    // is empty.
    send(
        &mut client,
        &ClientMessage::BeginSelection(zmux::protocol::SelectionKind::Line),
    );
    let lines = read_frame_with(&mut client, "[SEL ");
    assert!(
        lines.iter().any(|line| line.contains("[SEL ")),
        "status bar must show [SEL ...] after BeginSelection: {lines:?}",
    );

    // Yank returns a Clipboard server message (even if the payload is
    // empty text — the shell's first row hasn't printed anything yet)
    // and must clear the [SEL ...] tag from subsequent frames.
    send(&mut client, &ClientMessage::YankSelection);
    let deadline = Instant::now() + READ_TIMEOUT;
    let mut saw_clipboard = false;
    let mut saw_clear_frame = false;
    while Instant::now() < deadline && !(saw_clipboard && saw_clear_frame) {
        match read_message(&mut client) {
            ServerMessage::Clipboard(_) => saw_clipboard = true,
            ServerMessage::Frame { lines, .. } => {
                if !lines.iter().any(|line| line.contains("[SEL ")) {
                    saw_clear_frame = true;
                }
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }
    assert!(saw_clipboard, "yank must produce a Clipboard reply");
    assert!(saw_clear_frame, "status bar must drop [SEL ...] after yank");

    send(&mut client, &ClientMessage::Shutdown);
    drop(client);
    wait_for_removal(&path);
    let _ = child.wait();
}

#[test]
fn search_begin_shows_prompt_in_status_bar() {
    let name = format!("it-search-{}", std::process::id());
    let path = socket_path(&name);
    let _ = std::fs::remove_file(&path);

    let mut child = spawn_server_with_setsid(&name);
    wait_for_socket(&path);

    let size = PtySize::new(24, 80);
    let mut client = connect(&path);
    send(&mut client, &ClientMessage::Attach { size });
    let _ = read_message(&mut client);

    send(&mut client, &ClientMessage::SearchBegin);
    let lines = read_frame_with(&mut client, "search:");
    assert!(
        lines.iter().any(|line| line.contains("search:")),
        "search prompt missing after SearchBegin: {lines:?}",
    );

    send(&mut client, &ClientMessage::SearchInput(b"abc".to_vec()));
    let lines = read_frame_with(&mut client, "search: abc");
    assert!(
        lines.iter().any(|line| line.contains("search: abc")),
        "typed characters missing from search prompt: {lines:?}",
    );

    send(&mut client, &ClientMessage::Shutdown);
    drop(client);
    wait_for_removal(&path);
    let _ = child.wait();
}

/// End-to-end: the Ctrl-a : flow sends `CommandPromptBegin(General)` then
/// `CommandPromptInput("display-message hi")` then `CommandPromptCommit`.
/// The dispatcher should resolve and store the output so the next rendered
/// frame's status bar contains the dispatched message text.
#[test]
fn runtime_prompt_display_message_round_trip() {
    use zmux::protocol::CommandPromptKind;

    let name = format!("it-cmd-prompt-{}", std::process::id());
    let path = socket_path(&name);
    let _ = std::fs::remove_file(&path);

    let mut child = spawn_server_with_setsid(&name);
    wait_for_socket(&path);

    let size = PtySize::new(24, 80);
    let mut client = connect(&path);
    send(&mut client, &ClientMessage::Attach { size });
    // Discard the initial frame.
    let _ = read_message(&mut client);

    // Open the general command prompt (client-side equivalent of Ctrl-a :).
    send(
        &mut client,
        &ClientMessage::CommandPromptBegin(CommandPromptKind::General),
    );
    // Wait for the frame showing the general prompt (":_" in status bar).
    let lines = read_frame_with(&mut client, ":_");
    assert!(
        lines.iter().any(|line| line.contains(":_")),
        "general command prompt not shown in status bar: {lines:?}",
    );

    // Type the command.
    send(
        &mut client,
        &ClientMessage::CommandPromptInput(b"display-message hi".to_vec()),
    );
    // Wait for the input to appear in the prompt.
    let _ = read_frame_with(&mut client, ":display-message hi_");

    // Commit — triggers dispatch of "display-message hi".
    send(&mut client, &ClientMessage::CommandPromptCommit);

    // The next frame should contain "hi" in the status bar (via prompt_error
    // slot, which is the temporary message display path for Tier 1).
    let lines = read_frame_with(&mut client, "hi");
    let status = lines.last().map(|row| strip_ansi(row)).expect("status row");
    assert!(
        status.contains("hi"),
        "status bar did not contain dispatched message after commit; status: {status:?}",
    );

    send(&mut client, &ClientMessage::Shutdown);
    drop(client);
    wait_for_removal(&path);
    let _ = child.wait();
}
