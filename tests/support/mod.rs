// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared test-support helpers for zmux's process-spawning integration
//! tests (`mcp_server.rs`, `daemon_integration.rs`, `pair_integration.rs`).
//!
//! The centerpiece is [`TestSession`], a panic-safe RAII guard around a
//! spawned `zmux serve <name>` daemon. Tests used to tear the daemon
//! down by hand at the very end of the test body (`child.kill();
//! child.wait();`), which meant a failed `assert!`/`panic!` anywhere
//! before that line leaked the daemon forever. That's not
//! hypothetical: this machine had seven `zmux serve` processes
//! (`mcp-inflight-*`, `mcp-reconnect-*`, `mcp-spawn-win-*`) that had
//! been running for weeks after test panics, each burning CPU
//! continuously. `TestSession::drop` always runs — success, `assert!`
//! failure, or `panic!` — so every test that goes through it tears its
//! daemon down unconditionally.
//!
//! This file lives in `tests/support/` (not directly in `tests/`) so
//! cargo does not treat it as its own test binary. Each of the three
//! files above pulls it in with `mod support;` and gets its own copy
//! compiled in as part of that binary's crate. Because of that, not
//! every helper here is exercised by every test binary —
//! `#![allow(dead_code)]` below is expected, not a sign of unused
//! cruft.
//!
//! Deliberately NOT changed: the shared `$TMPDIR/zmux-$USER/` socket
//! directory. Unique session names (see [`unique_name`]) are the
//! existing concurrency strategy for parallel `cargo test` runs, and
//! this module keeps using it rather than trying to sandbox each test
//! into its own directory.

#![allow(dead_code)]

use std::ffi::OsStr;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

unsafe extern "C" {
    fn setsid() -> i32;
    fn kill(pid: i32, sig: i32) -> i32;
}

const SIGKILL: i32 = 9;

/// How long we'll wait for a freshly spawned daemon's socket file to
/// appear before giving up.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// The shared directory every zmux session's sockets live under.
pub fn session_root() -> PathBuf {
    let user = std::env::var("USER").unwrap_or_else(|_| "user".to_string());
    std::env::temp_dir().join(format!("zmux-{user}"))
}

pub fn client_socket_path(name: &str) -> PathBuf {
    session_root().join(format!("{name}.sock"))
}

pub fn mcp_socket_path(name: &str) -> PathBuf {
    session_root().join(format!("{name}.mcp.sock"))
}

/// Generate a unique session name per test so parallel runs (and
/// repeated invocations of the same test within one binary) don't
/// step on each other.
pub fn unique_name(prefix: &str) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{pid}-{n}")
}

pub fn wait_for_socket(path: &Path) {
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    while !path.exists() {
        if Instant::now() > deadline {
            panic!("timed out waiting for socket at {}", path.display());
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn raw_spawn_serve<I, K, V>(name: &str, envs: I) -> Child
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    let exe = env!("CARGO_BIN_EXE_zmux");
    let mut command = Command::new(exe);
    command
        .arg("serve")
        .arg(name)
        .envs(envs)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: pre_exec runs post-fork/pre-exec in the child only, and
    // this closure only calls the async-signal-safe setsid(2).
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

/// Spawn `zmux serve <name>` and wrap it in a panic-safe [`TestSession`].
pub fn spawn_serve(name: &str) -> TestSession {
    spawn_serve_with_envs(name, std::iter::empty::<(&str, &str)>())
}

/// Same as [`spawn_serve`], but with extra environment variables set on
/// the daemon process (e.g. `ZMUX_STATE_DIR` for the audit-log test).
pub fn spawn_serve_with_envs<I, K, V>(name: &str, envs: I) -> TestSession
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    TestSession {
        name: name.to_string(),
        child: Some(raw_spawn_serve(name, envs)),
        extra: Vec::new(),
    }
}

/// Panic-safe RAII guard around a live zmux test session.
///
/// Owns the session's name and, when we have them, process handles for
/// the daemon (`child`) and any subsidiary processes the test spawned
/// alongside it (`extra` — e.g. a `zmux mcp --session` stdio bridge, or
/// a second daemon spun up mid-test to exercise reconnect). `Drop`:
///
/// 1. Reaps any adopted `extra` children (e.g. a stdio bridge) first,
///    so nothing is left running that could still auto-spawn a fresh
///    replacement daemon out from under the steps below.
/// 2. Repeatedly sends a graceful `Shutdown` to the session **by
///    name** via [`zmux::kill_session`] *and* sweeps `/proc` by exact
///    argv match for any live `zmux serve <name>` process, until a
///    few consecutive scans in a row find nothing (bounded by a hard
///    deadline). This covers daemons we never held a `Child` handle
///    for — the stdio bridge's auto-start path, or a reconnect-spawned
///    replacement — including one that's still between `fork()` and
///    binding its socket, where socket-file state alone would be a
///    false negative.
/// 3. Escalates to `SIGKILL` for any process handle we still hold
///    directly.
/// 4. One more `/proc` sweep, in case a stray spawn landed right after
///    step 2 gave up.
/// 5. Sweeps both socket files unconditionally, since a `SIGKILL`
///    bypasses the daemon's own cleanup-on-exit code.
///
/// Every step tolerates an already-dead / never-started session
/// silently (errors are discarded) — some tests deliberately kill the
/// daemon mid-test to exercise failure paths (e.g. the bridge
/// daemon-death tests), and `Drop` must not itself panic while
/// unwinding from a test's own panic.
pub struct TestSession {
    name: String,
    child: Option<Child>,
    extra: Vec<Child>,
}

impl TestSession {
    /// Build a guard for a session this test never held a `Child`
    /// handle for (e.g. one the stdio bridge auto-started on first
    /// connect). `Drop` still tears it down by name.
    pub fn for_name(name: &str) -> TestSession {
        TestSession {
            name: name.to_string(),
            child: None,
            extra: Vec::new(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The daemon's pid, if we're holding a live `Child` handle for
    /// it. Used by the Drop-on-panic verification test to confirm the
    /// process is actually gone (not just the socket files).
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }

    /// Adopt another child process (a stdio bridge, typically) so a
    /// panic anywhere after this call still reaps it.
    pub fn adopt(&mut self, child: Child) {
        self.extra.push(child);
    }

    /// Explicitly tear the current daemon process down *now* (not at
    /// Drop time) and remove its socket files. Used by tests that
    /// deliberately kill the daemon mid-test to exercise reconnect /
    /// daemon-death paths and need the sockets gone before a
    /// replacement daemon starts under the same name.
    pub fn kill_now(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_file(client_socket_path(&self.name));
        let _ = std::fs::remove_file(mcp_socket_path(&self.name));
    }

    /// Spawn a fresh `zmux serve` under this session's name (e.g. as
    /// the "daemon B" half of a restart test) and adopt it as the
    /// guard's primary child.
    pub fn respawn(&mut self) {
        self.child = Some(raw_spawn_serve(
            &self.name,
            std::iter::empty::<(&str, &str)>(),
        ));
    }

    /// Poll the currently-held daemon child for up to `timeout`,
    /// returning its exit status once it's gone. On timeout, sends a
    /// `SIGKILL` and panics.
    ///
    /// Several `daemon_integration` tests send an explicit wire-level
    /// `Shutdown` and then want to assert the daemon actually exited
    /// with status 0 — that's a real correctness check (did the
    /// daemon shut down cleanly?), not just teardown, so it belongs
    /// in the test body rather than folded into `Drop` (which only
    /// cares that the process is gone, not how it got there, and
    /// discards the exit status).
    ///
    /// Leaves the exited child's status cached in `self.child` — a
    /// later `Drop` still runs the usual escalation path against it,
    /// but `kill`/`wait` on an already-reaped child are no-ops.
    pub fn wait_for_exit(&mut self, timeout: Duration) -> std::process::ExitStatus {
        let child = self
            .child
            .as_mut()
            .expect("wait_for_exit called with no daemon child held by this TestSession");
        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait().expect("poll child") {
                Some(status) => return status,
                None => {
                    if Instant::now() > deadline {
                        let _ = child.kill();
                        panic!("daemon did not exit within {timeout:?}");
                    }
                    thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }
}

impl Drop for TestSession {
    fn drop(&mut self) {
        // 1. Reap adopted children (typically an `zmux mcp --session`
        //    stdio bridge) FIRST, before touching the session by
        //    name. This ordering matters: the bridge's reconnect
        //    logic auto-starts a replacement daemon under this same
        //    session name the moment it observes the socket vanish
        //    (see `connect_with_autostart` in src/mcp/stdio.rs), which
        //    is exactly what happens when a test kills the daemon
        //    mid-flight to exercise the daemon-death path. If we did
        //    the by-name kill first, we could race a bridge that
        //    hasn't reacted yet and spawns a *fresh* daemon just
        //    after we checked — leaking it silently. Killing the
        //    bridge first means nothing is left that could spawn yet
        //    another replacement out from under the check below.
        //
        //    Note this only bounds *future* spawns: `handle_disconnect`
        //    writes its synthesized error to stdout — which is what
        //    lets a test's blocking `read_line()` return — *before*
        //    calling `reconnect_with_backoff`, so a test can reach this
        //    Drop while the bridge is still about to (or has just)
        //    called `Command::spawn()` for a replacement daemon. Once
        //    that fork has happened the child is a fully independent,
        //    `setsid`'d process; SIGKILLing the bridge here cannot
        //    un-spawn it. Steps 2 covers that daemon once it exists.
        for mut child in self.extra.drain(..) {
            if matches!(child.try_wait(), Ok(Some(_))) {
                continue;
            }
            let _ = child.kill();
            let _ = child.wait();
        }

        // 2. Ask the session to shut down by name AND sweep `/proc` for
        //    any live process whose argv is exactly `zmux serve
        //    <name>`, repeatedly, until a few consecutive scans in a
        //    row come up empty (or a hard deadline passes).
        //
        //    Two mechanisms, not one, and deliberately not relying on
        //    socket-file presence as the "is anything still running"
        //    signal: `kill_session` is fast in the common case (it
        //    connects to the client socket and sends a graceful
        //    Shutdown, which lets the daemon unlink its own socket
        //    files on the way out), but a daemon the bridge just forked
        //    off (see step 1's note) can still be between fork() and
        //    binding its socket when we get here — at that instant
        //    *neither* socket file exists yet, which is NOT evidence
        //    that nothing is running, only that nothing has answered
        //    yet. A single "both files absent -> done" check is exactly
        //    the bug that let a replacement daemon slip through under
        //    full-suite (parallel, CPU-contended) test runs even though
        //    isolated single-test loops never reproduced it. The `/proc`
        //    sweep doesn't care about socket state at all, only process
        //    identity by exact argv match, so it catches that daemon as
        //    soon as it's `exec`'d into place — independent of how far
        //    along its own socket bind is.
        let deadline = Instant::now() + Duration::from_millis(2_000);
        let mut clean_scans = 0u32;
        loop {
            let _ = zmux::kill_session(&self.name);
            if kill_stray_daemons_by_name(&self.name) {
                clean_scans = 0;
            } else {
                clean_scans += 1;
            }
            if clean_scans >= 3 || Instant::now() > deadline {
                break;
            }
            thread::sleep(Duration::from_millis(40));
        }

        // 3. Escalate: SIGKILL any Child handle we still hold
        //    directly (the primary daemon, when the test never called
        //    `kill_now`). `kill`/`wait` on an already-exited child
        //    just errors, which we discard — Drop must tolerate an
        //    already-dead session silently.
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }

        // 4. Final `/proc` sweep in case the very last stray spawn
        //    landed after the loop in step 2 already declared victory.
        kill_stray_daemons_by_name(&self.name);

        // 5. Belt-and-suspenders: a SIGKILL bypasses the daemon's own
        //    remove-on-exit cleanup, so sweep both files ourselves
        //    regardless of which path above actually fired.
        let _ = std::fs::remove_file(client_socket_path(&self.name));
        let _ = std::fs::remove_file(mcp_socket_path(&self.name));
    }
}

/// Best-effort: `SIGKILL` every live process whose argv is exactly
/// `<something> serve <name>` (i.e. every `zmux serve <name>` we could
/// have spawned, regardless of which binary path invoked it), found by
/// scanning `/proc/*/cmdline` directly. Returns whether any matches
/// were found, so [`Drop`] can debounce on a few consecutive empty
/// scans instead of guessing a fixed sleep.
///
/// This exists *alongside* (not instead of) [`zmux::kill_session`] — see
/// the comment in `Drop` for why socket-file state alone is an
/// unreliable signal here. Matching is done positionally (last two argv
/// entries), not by substring search over the joined command line, so a
/// binary path that happens to contain "serve" somewhere can't produce
/// a false match.
fn kill_stray_daemons_by_name(name: &str) -> bool {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    let mut found = false;
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let Ok(raw) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let args: Vec<&str> = raw
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .filter_map(|s| std::str::from_utf8(s).ok())
            .collect();
        if args.len() >= 2 && args[args.len() - 2] == "serve" && args[args.len() - 1] == name {
            found = true;
            // SAFETY: kill(2) with a plain PID and a numeric signal is
            // always safe to call; failure (already dead, permission)
            // is a plain -1/ESRCH return, not UB, and we don't inspect
            // it — Drop tolerates an already-dead process silently.
            unsafe {
                kill(pid, SIGKILL);
            }
        }
    }
    found
}
