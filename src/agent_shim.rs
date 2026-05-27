// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared prompt-in/stdout-out shims for interactive agent CLIs.
//!
//! Agent-specific modules parse their own CLI flags, then hand an
//! [`AgentShimArgs`] here. The runner drives a live pane over MCP,
//! waits for a marker contract, and prints either plain text or a
//! worker-friendly JSON result.

use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::mcp::session_mcp_socket_path;
use crate::pair::client::Client;
use crate::state_paths::{safe_component, shell_quote, state_dir};

pub const DEFAULT_TIMEOUT_MS: u64 = 600_000;
pub const DEFAULT_WAIT_LINES: u32 = 4_000;
const MAX_MCP_WAIT_MS: u64 = 60_000;
const CONNECT_WAIT_MS: u64 = 1_500;
const OUTPUT_POLL_MS: u64 = 100;
const RENDERED_POLL_MS: u64 = 500;
const OUTPUT_CAPTURE_MAX_BYTES: usize = crate::pane::OUTPUT_RING_CAPACITY;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Text,
    WorkerJson,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResultCapture {
    TerminalOnly,
    ClaudeHooks(crate::claude_hooks::ClaudeHookCapture),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZmuxResumeTarget {
    pub session: String,
    pub pane_id: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentShimArgs {
    pub agent_name: &'static str,
    pub result_type: &'static str,
    pub session: String,
    pub command: String,
    pub label: String,
    pub timeout_ms: u64,
    pub wait_lines: u32,
    pub force_new: bool,
    pub kill_after: bool,
    pub output_mode: OutputMode,
    pub result_capture: ResultCapture,
    pub resume_target: Option<ZmuxResumeTarget>,
    pub startup_args: Vec<String>,
    pub prompt: String,
}

pub fn value_after(args: &[String], index: usize, message: &str) -> Result<String, String> {
    args.get(index + 1)
        .cloned()
        .ok_or_else(|| message.to_string())
}

pub fn parse_wrapper_output_format(agent_name: &str, raw: &str) -> Result<OutputMode, String> {
    match raw {
        "text" => Ok(OutputMode::Text),
        "json" => Ok(OutputMode::WorkerJson),
        other => Err(format!(
            "{agent_name}: --output-format `{other}` is not supported by zmux {agent_name} (supported: text, json)"
        )),
    }
}

pub fn parse_zmux_session_id(agent_name: &str, raw: &str) -> Result<ZmuxResumeTarget, String> {
    let body = raw
        .strip_prefix("zmux:")
        .ok_or_else(|| format!("{agent_name}: invalid zmux session id `{raw}`"))?;
    let (session, pane) = body
        .rsplit_once(':')
        .ok_or_else(|| format!("{agent_name}: invalid zmux session id `{raw}`"))?;
    if session.is_empty() {
        return Err(format!("{agent_name}: invalid zmux session id `{raw}`"));
    }
    let pane_id = pane
        .parse::<u32>()
        .map_err(|_| format!("{agent_name}: invalid zmux pane id in `{raw}`"))?;
    Ok(ZmuxResumeTarget {
        session: session.to_string(),
        pane_id,
    })
}

pub fn zmux_session_id(session: &str, pane_id: u32) -> String {
    format!("zmux:{session}:{pane_id}")
}

pub fn flag_has_inline_value(flag: &str) -> bool {
    flag.starts_with("--") && flag.contains('=')
}

pub fn run(args: AgentShimArgs) -> Result<(), String> {
    let client = connect_initialized(&args.session).map_err(|err| {
        format!(
            "{}: connect to session `{}`: {err}",
            args.agent_name, args.session
        )
    })?;
    let spawn_command =
        spawn_command_for_current_dir(args.agent_name, &args.command, &args.startup_args)?;
    let pane_id = if let Some(target) = &args.resume_target {
        find_resume_pane(&client, args.agent_name, target.pane_id)?
    } else if args.force_new {
        None
    } else {
        find_reusable_pane(&client, args.agent_name, &args.label, &spawn_command)?
    }
    .map(Ok)
    .unwrap_or_else(|| spawn_agent_pane(&client, &args, &spawn_command))?;

    let _turn_lock = PaneTurnLock::acquire(args.agent_name, &args.session, pane_id)?;
    let nonce = make_nonce();
    let begin = format!("ZMUX_RESULT_BEGIN_{nonce}");
    let end = format!("ZMUX_RESULT_END_{nonce}");
    let prompt = wrap_prompt(&args.prompt, &begin, &end);
    let output_start = output_cursor(&client, args.agent_name, pane_id)?;
    let hook_cursor = match &args.result_capture {
        ResultCapture::ClaudeHooks(capture) => {
            Some(crate::claude_hooks::event_cursor(&capture.events_path)?)
        }
        ResultCapture::TerminalOnly => None,
    };

    client
        .call_tool(
            "send_keys",
            json!({
                "pane_id": pane_id,
                "keys": prompt,
                "enter": true,
                "clear_input": true
            }),
        )
        .map_err(|err| format!("{}: send prompt to pane {pane_id}: {err}", args.agent_name))?;

    let hook_poller = match (&args.result_capture, hook_cursor) {
        (ResultCapture::ClaudeHooks(capture), Some(cursor)) => Some(
            crate::claude_hooks::ClaudeHookPoller::new(capture.clone(), cursor, nonce.clone()),
        ),
        _ => None,
    };
    let captured = wait_for_result(
        &client,
        args.agent_name,
        pane_id,
        output_start,
        &begin,
        &end,
        args.timeout_ms,
        args.wait_lines,
        hook_poller,
    )?;
    let result = match captured {
        CapturedResult::Hook(result) => result,
        CapturedResult::Terminal(marker_text) => {
            let rendered_text =
                read_rendered_marker_text(&client, args.agent_name, pane_id, args.wait_lines)
                    .unwrap_or_default();
            extract_last_marked_result(&rendered_text, &begin, &end)
                .or_else(|| extract_last_marked_result(&marker_text, &begin, &end))
                .ok_or_else(|| {
                    format!(
                        "{}: saw end marker in pane {pane_id}, but could not extract result block; try `zmux attach {}`",
                        args.agent_name, args.session
                    )
                })?
        }
    };
    let result = clean_result_text(&result);
    match args.output_mode {
        OutputMode::Text => println!("{result}"),
        OutputMode::WorkerJson => println!("{}", worker_json_result(&args, pane_id, &result)),
    }

    if args.kill_after {
        client
            .call_tool("kill_pane", json!({ "pane_id": pane_id }))
            .map_err(|err| format!("{}: cleanup pane {pane_id}: {err}", args.agent_name))?;
    }

    Ok(())
}

fn connect_initialized(session: &str) -> io::Result<Client> {
    match Client::connect(session) {
        Ok(client) => {
            client.initialize()?;
            Ok(client)
        }
        Err(err) if is_no_daemon(err.kind()) => {
            ensure_session_running(session)?;
            wait_for_mcp_socket(session)?;
            let client = Client::connect(session)?;
            client.initialize()?;
            Ok(client)
        }
        Err(err) => Err(err),
    }
}

fn ensure_session_running(session: &str) -> io::Result<()> {
    match crate::create_session(session) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(err) => Err(err),
    }
}

fn wait_for_mcp_socket(session: &str) -> io::Result<()> {
    let path = session_mcp_socket_path(session);
    let deadline = Instant::now() + Duration::from_millis(CONNECT_WAIT_MS);
    while Instant::now() < deadline {
        if path.exists() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(20));
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!("timed out waiting for MCP socket {}", path.display()),
    ))
}

fn is_no_daemon(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
}

fn find_reusable_pane(
    client: &Client,
    agent_name: &str,
    label: &str,
    command: &str,
) -> Result<Option<u32>, String> {
    let payload = client
        .call_tool("list_panes", json!({}))
        .map_err(|err| format!("{agent_name}: list panes: {err}"))?;
    let panes = payload
        .get("panes")
        .and_then(|v| v.as_array())
        .ok_or_else(|| format!("{agent_name}: list_panes response missing panes array"))?;
    for pane in panes.iter().rev() {
        let matches_label = pane.get("label").and_then(|v| v.as_str()) == Some(label);
        let matches_command = pane.get("last_command").and_then(|v| v.as_str()) == Some(command);
        let state = pane.get("state").and_then(|v| v.as_str()).unwrap_or("");
        let reusable_state = !state.starts_with("Exited") && state != "Working";
        if matches_label
            && matches_command
            && reusable_state
            && let Some(id) = pane.get("pane_id").and_then(|v| v.as_u64())
        {
            return Ok(Some(id as u32));
        }
    }
    Ok(None)
}

fn find_resume_pane(
    client: &Client,
    agent_name: &str,
    pane_id: u32,
) -> Result<Option<u32>, String> {
    let payload = client
        .call_tool("list_panes", json!({}))
        .map_err(|err| format!("{agent_name}: list panes: {err}"))?;
    let panes = payload
        .get("panes")
        .and_then(|v| v.as_array())
        .ok_or_else(|| format!("{agent_name}: list_panes response missing panes array"))?;
    let pane = panes
        .iter()
        .find(|pane| pane.get("pane_id").and_then(|v| v.as_u64()) == Some(pane_id as u64))
        .ok_or_else(|| format!("{agent_name}: no pane {pane_id} for zmux resume id"))?;
    let state = pane.get("state").and_then(|v| v.as_str()).unwrap_or("");
    if state.starts_with("Exited") {
        return Err(format!(
            "{agent_name}: pane {pane_id} has exited and cannot be resumed"
        ));
    }
    if state == "Working" {
        return Err(format!("{agent_name}: pane {pane_id} is still working"));
    }
    Ok(Some(pane_id))
}

fn spawn_agent_pane(client: &Client, args: &AgentShimArgs, command: &str) -> Result<u32, String> {
    let max_wait_ms = args.timeout_ms.min(MAX_MCP_WAIT_MS) as u32;
    let payload = client
        .call_tool(
            "spawn_pane",
            json!({
                "command": command,
                "split": "window",
                "label": args.label,
                "wait_for_idle": true,
                "max_wait_ms": max_wait_ms
            }),
        )
        .map_err(|err| format!("{}: spawn `{command}`: {err}", args.agent_name))?;
    if payload
        .get("timed_out")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return Err(format!(
            "{}: `{command}` did not settle before startup timeout",
            args.agent_name
        ));
    }
    payload
        .get("pane_id")
        .and_then(|v| v.as_u64())
        .map(|id| id as u32)
        .ok_or_else(|| format!("{}: spawn_pane response missing pane_id", args.agent_name))
}

pub fn spawn_command(command: &str, startup_args: &[String]) -> String {
    let mut command = command.to_string();
    for arg in startup_args {
        command.push(' ');
        command.push_str(&shell_quote(arg));
    }
    command
}

pub fn spawn_command_in_dir(command: &str, startup_args: &[String], cwd: &Path) -> String {
    format!(
        "cd {} && {}",
        shell_quote(&cwd.to_string_lossy()),
        spawn_command(command, startup_args)
    )
}

fn spawn_command_for_current_dir(
    agent_name: &str,
    command: &str,
    startup_args: &[String],
) -> Result<String, String> {
    let cwd = env::current_dir().map_err(|err| format!("{agent_name}: read current dir: {err}"))?;
    Ok(spawn_command_in_dir(command, startup_args, &cwd))
}

pub fn worker_json_result(args: &AgentShimArgs, pane_id: u32, result: &str) -> Value {
    json!({
        "type": args.result_type,
        "result": result,
        "session_id": if args.kill_after {
            Value::Null
        } else {
            Value::String(zmux_session_id(&args.session, pane_id))
        },
        "zmux_session": args.session.as_str(),
        "pane_id": pane_id,
        "killed_after": args.kill_after,
    })
}

enum CapturedResult {
    Hook(String),
    Terminal(String),
}

struct PaneTurnLock {
    path: PathBuf,
}

impl PaneTurnLock {
    fn acquire(agent_name: &str, session: &str, pane_id: u32) -> Result<Self, String> {
        let dir = state_dir()
            .join("locks")
            .join(safe_component(session))
            .join(safe_component(agent_name));
        fs::create_dir_all(&dir).map_err(|err| {
            format!(
                "{agent_name}: create pane lock dir {}: {err}",
                dir.display()
            )
        })?;
        let path = dir.join(format!("pane-{pane_id}.lock"));
        match create_lock_file(&path) {
            Ok(()) => Ok(Self { path }),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                if lock_is_stale(&path) {
                    let _ = fs::remove_file(&path);
                    create_lock_file(&path).map_err(|err| {
                        format!(
                            "{agent_name}: claim pane {pane_id} after stale lock cleanup: {err}"
                        )
                    })?;
                    Ok(Self { path })
                } else {
                    Err(format!(
                        "{agent_name}: pane {pane_id} is already claimed by another zmux invocation"
                    ))
                }
            }
            Err(err) => Err(format!("{agent_name}: claim pane {pane_id}: {err}")),
        }
    }
}

impl Drop for PaneTurnLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn create_lock_file(path: &PathBuf) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    writeln!(file, "pid={}", std::process::id())?;
    Ok(())
}

fn lock_is_stale(path: &PathBuf) -> bool {
    let Ok(content) = fs::read_to_string(path) else {
        return false;
    };
    let Some(pid) = content
        .lines()
        .find_map(|line| line.strip_prefix("pid="))
        .and_then(|raw| raw.parse::<u32>().ok())
    else {
        return false;
    };
    !pid_is_alive(pid)
}

fn pid_is_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        std::path::Path::new("/proc").join(pid.to_string()).exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        true
    }
}

fn output_cursor(client: &Client, agent_name: &str, pane_id: u32) -> Result<u64, String> {
    let payload = client
        .call_tool(
            "read_pane_output",
            json!({
                "pane_id": pane_id,
                "max_bytes": 0
            }),
        )
        .map_err(|err| format!("{agent_name}: read output cursor for pane {pane_id}: {err}"))?;
    payload
        .get("byte_cursor")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| format!("{agent_name}: read_pane_output response missing byte_cursor"))
}

fn read_rendered_marker_text(
    client: &Client,
    agent_name: &str,
    pane_id: u32,
    wait_lines: u32,
) -> Result<String, String> {
    let payload = client
        .call_tool(
            "read_pane",
            json!({
                "pane_id": pane_id,
                "mode": "scrollback",
                "lines": wait_lines,
                "strip_ansi": true
            }),
        )
        .map_err(|err| format!("{agent_name}: read rendered pane {pane_id}: {err}"))?;
    Ok(payload
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string())
}

#[allow(clippy::too_many_arguments)]
fn wait_for_result(
    client: &Client,
    agent_name: &str,
    pane_id: u32,
    since_byte: u64,
    begin: &str,
    end: &str,
    timeout_ms: u64,
    wait_lines: u32,
    mut hook_poller: Option<crate::claude_hooks::ClaudeHookPoller>,
) -> Result<CapturedResult, String> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut last_text = String::new();
    let mut hook_hint: Option<String> = None;
    let mut last_rendered_check = Instant::now()
        .checked_sub(Duration::from_millis(RENDERED_POLL_MS))
        .unwrap_or_else(Instant::now);
    loop {
        let now = Instant::now();
        if now >= deadline {
            let mut message = format!(
                "{agent_name}: timed out waiting for result marker in pane {pane_id}; last output tail:\n{}",
                tail(&last_text, 20)
            );
            if let Some(hint) =
                hook_hint.or_else(|| hook_poller.as_ref().and_then(|p| p.status_hint()))
            {
                message.push('\n');
                message.push_str(&hint);
            }
            return Err(message);
        }

        if let Some(poller) = hook_poller.as_mut() {
            match poller.poll(begin, end) {
                Ok(Some(result)) => return Ok(CapturedResult::Hook(result)),
                Ok(None) => hook_hint = poller.status_hint(),
                Err(err) => hook_hint = Some(err),
            }
        }

        let payload = client
            .call_tool(
                "read_pane_output",
                json!({
                    "pane_id": pane_id,
                    "since_byte": since_byte,
                    "max_bytes": OUTPUT_CAPTURE_MAX_BYTES,
                    "strip_ansi": true
                }),
            )
            .map_err(|err| format!("{agent_name}: read output for pane {pane_id}: {err}"))?;
        let text = payload
            .get("text")
            .and_then(|v| v.as_str())
            .map(clean_transcript_text)
            .unwrap_or_default();
        last_text = text.clone();
        if text.contains(end) {
            if let Some(poller) = hook_poller.as_mut()
                && let Ok(Some(result)) = poller.poll(begin, end)
            {
                return Ok(CapturedResult::Hook(result));
            }
            return Ok(CapturedResult::Terminal(text));
        }
        if last_rendered_check.elapsed() >= Duration::from_millis(RENDERED_POLL_MS) {
            last_rendered_check = Instant::now();
            if let Ok(rendered) = read_rendered_marker_text(client, agent_name, pane_id, wait_lines)
            {
                if rendered.contains(end) {
                    if let Some(poller) = hook_poller.as_mut()
                        && let Ok(Some(result)) = poller.poll(begin, end)
                    {
                        return Ok(CapturedResult::Hook(result));
                    }
                    return Ok(CapturedResult::Terminal(rendered));
                }
                if last_text.is_empty() {
                    last_text = rendered;
                }
            }
        }
        if payload
            .get("truncated")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            return Err(format!(
                "{agent_name}: output transcript for pane {pane_id} exceeded {} bytes before the result marker; last output tail:\n{}",
                OUTPUT_CAPTURE_MAX_BYTES,
                tail(&last_text, 20)
            ));
        }

        let remaining = deadline.saturating_duration_since(now);
        thread::sleep(remaining.min(Duration::from_millis(OUTPUT_POLL_MS)));
    }
}

fn clean_transcript_text(text: &str) -> String {
    let stripped = crate::pane::strip_ansi_inplace(text);
    let mut out = String::with_capacity(stripped.len());
    let mut chars = stripped.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    continue;
                }
                if !out.ends_with('\n') {
                    out.push('\n');
                }
            }
            '\n' | '\t' => out.push(ch),
            '\u{0008}' => {
                let _ = out.pop();
            }
            ch if ch.is_control() => {}
            ch => out.push(ch),
        }
    }
    out.lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

fn clean_result_text(text: &str) -> String {
    clean_transcript_text(text).trim().to_string()
}

fn make_nonce() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:x}_{:x}", std::process::id(), now)
}

pub fn wrap_prompt(prompt: &str, begin: &str, end: &str) -> String {
    let begin_suffix = begin.strip_prefix("ZMUX_RESULT_BEGIN_").unwrap_or(begin);
    let end_suffix = end.strip_prefix("ZMUX_RESULT_END_").unwrap_or(end);
    debug_assert_eq!(begin_suffix, end_suffix);
    format!(
        "{prompt}

Automation contract: when your final answer is ready, print a marker line by concatenating `ZMUX_RESULT_BEGIN_` with `{begin_suffix}`, then print your final answer, then print a marker line by concatenating `ZMUX_RESULT_END_` with `{end_suffix}`. Do not print either concatenated marker line until the answer is complete."
    )
}

pub fn extract_last_marked_result(text: &str, begin: &str, end: &str) -> Option<String> {
    let end_pos = text.rfind(end)?;
    let before_end = &text[..end_pos];
    let begin_pos = before_end.rfind(begin)?;
    let after_begin = &before_end[begin_pos + begin.len()..];
    let after_marker_line = if let Some(rest) = after_begin.strip_prefix("\r\n") {
        rest
    } else if let Some(rest) = after_begin.strip_prefix('\n') {
        rest
    } else if let Some(rest) = after_begin.strip_prefix('\r') {
        rest
    } else if let Some((_, rest)) = after_begin.split_once('\n') {
        rest
    } else {
        after_begin
    };
    Some(after_marker_line.trim().to_string())
}

fn tail(text: &str, lines: usize) -> String {
    let mut tail: Vec<&str> = text.lines().rev().take(lines).collect();
    tail.reverse();
    tail.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_args() -> AgentShimArgs {
        AgentShimArgs {
            agent_name: "agent",
            result_type: "zmux.agent.result",
            session: "default".into(),
            command: "agent".into(),
            label: "zmux-agent".into(),
            timeout_ms: DEFAULT_TIMEOUT_MS,
            wait_lines: DEFAULT_WAIT_LINES,
            force_new: false,
            kill_after: false,
            output_mode: OutputMode::Text,
            result_capture: ResultCapture::TerminalOnly,
            resume_target: None,
            startup_args: Vec::new(),
            prompt: "hello".into(),
        }
    }

    #[test]
    fn spawn_command_quotes_args_for_shell() {
        let args = vec!["--model".into(), "sonnet".into(), "it's fine".into()];
        assert_eq!(
            spawn_command("agent", &args),
            "agent '--model' 'sonnet' 'it'\\''s fine'"
        );
    }

    #[test]
    fn spawn_command_in_dir_includes_quoted_cwd() {
        let args = vec!["--model".into(), "sonnet".into()];
        let cwd = Path::new("/tmp/zmux dir/it's-here");
        assert_eq!(
            spawn_command_in_dir("agent", &args, cwd),
            "cd '/tmp/zmux dir/it'\\''s-here' && agent '--model' 'sonnet'"
        );
    }

    #[test]
    fn worker_json_result_matches_worker_shape() {
        let args = AgentShimArgs {
            session: "work".into(),
            output_mode: OutputMode::WorkerJson,
            ..base_args()
        };
        let payload = worker_json_result(&args, 10042, "done");
        assert_eq!(
            payload.get("result").and_then(|value| value.as_str()),
            Some("done")
        );
        assert_eq!(
            payload.get("session_id").and_then(|value| value.as_str()),
            Some("zmux:work:10042")
        );
        assert_eq!(
            payload.get("pane_id").and_then(|value| value.as_u64()),
            Some(10042)
        );
        assert_eq!(
            payload.get("type").and_then(|value| value.as_str()),
            Some("zmux.agent.result")
        );
    }

    #[test]
    fn worker_json_result_omits_session_id_when_killing_pane() {
        let args = AgentShimArgs {
            kill_after: true,
            output_mode: OutputMode::WorkerJson,
            ..base_args()
        };
        let payload = worker_json_result(&args, 10042, "done");
        assert!(
            payload
                .get("session_id")
                .is_some_and(|value| value.is_null())
        );
    }

    #[test]
    fn wrapper_does_not_include_exact_markers() {
        let prompt = wrap_prompt("hello", "ZMUX_RESULT_BEGIN_abc", "ZMUX_RESULT_END_abc");
        assert!(!prompt.contains("ZMUX_RESULT_BEGIN_abc"));
        assert!(!prompt.contains("ZMUX_RESULT_END_abc"));
        assert!(prompt.contains("ZMUX_RESULT_BEGIN_"));
        assert!(prompt.contains("ZMUX_RESULT_END_"));
        assert!(prompt.contains("abc"));
    }

    #[test]
    fn clean_transcript_strips_ansi_and_terminal_controls() {
        let text = "\x1b[31mred\x1b[0m\r\nhel\u{0008}lo\x07";
        assert_eq!(clean_transcript_text(text), "red\nhelo");
    }

    #[test]
    fn extract_uses_last_marker_pair() {
        let text = "prompt ZMUX_RESULT_BEGIN_a old ZMUX_RESULT_END_a\nanswer\nZMUX_RESULT_BEGIN_a\nnew result\nZMUX_RESULT_END_a";
        assert_eq!(
            extract_last_marked_result(text, "ZMUX_RESULT_BEGIN_a", "ZMUX_RESULT_END_a").unwrap(),
            "new result"
        );
    }

    #[test]
    fn extract_discards_begin_marker_line_chrome() {
        let text = "ZMUX_RESULT_BEGIN_a›Use /skills to list available skills
actual answer
ZMUX_RESULT_END_a";
        assert_eq!(
            extract_last_marked_result(text, "ZMUX_RESULT_BEGIN_a", "ZMUX_RESULT_END_a").unwrap(),
            "actual answer"
        );
    }

    #[test]
    fn extract_discards_padded_begin_marker_line() {
        let text = "● ZMUX_RESULT_BEGIN_a          
actual answer
ZMUX_RESULT_END_a";
        assert_eq!(
            extract_last_marked_result(text, "ZMUX_RESULT_BEGIN_a", "ZMUX_RESULT_END_a").unwrap(),
            "actual answer"
        );
    }
}
