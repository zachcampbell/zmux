// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::hash_map::DefaultHasher;
use std::fs::{self, File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

use crate::agent_shim::extract_last_marked_result;
use crate::state_paths::{ensure_private_dir, safe_component, shell_quote, state_dir};

// Upper bound on a single un-terminated hook-events line held in the
// poller's `partial_line` buffer. Real records (a prompt, an
// assistant message) are a few KB; 4 MiB is generous headroom while
// still bounding memory if a writer never emits a newline.
const MAX_HOOK_LINE_BYTES: usize = 4 * 1024 * 1024;

const HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "Notification",
    "Stop",
    "StopFailure",
    "SessionEnd",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeHookCapture {
    pub events_path: PathBuf,
    pub settings_path: PathBuf,
}

pub fn prepare_capture(
    zmux_session: &str,
    startup_args: &mut Vec<String>,
) -> Result<ClaudeHookCapture, String> {
    let user_settings = take_settings_arg(startup_args)?;
    let session_component = safe_component(zmux_session);
    let state_root = state_dir();
    ensure_private_dir(&state_root)
        .map_err(|err| format!("secure zmux state dir {}: {err}", state_root.display()))?;
    let root = state_root.join("claude").join(session_component);
    ensure_private_dir(&root)
        .map_err(|err| format!("secure Claude state dir {}: {err}", root.display()))?;
    let events_path = root.join("events.jsonl");
    let settings_key = settings_key(user_settings.as_deref());
    let settings_path = root
        .join("settings")
        .join(settings_key)
        .join("settings.json");

    write_settings(&settings_path, &events_path, user_settings.as_deref())?;
    startup_args.push("--settings".into());
    startup_args.push(settings_path.to_string_lossy().into_owned());

    Ok(ClaudeHookCapture {
        events_path,
        settings_path,
    })
}

pub fn run_cli(args: &[String]) -> Result<(), String> {
    if let Err(err) = append_cli_event(args) {
        eprintln!("zmux claude-hook: {err}");
    }
    Ok(())
}

fn append_cli_event(args: &[String]) -> Result<(), String> {
    let events_path = hook_events_arg(args)?;
    let mut input = String::new();
    io::stdin()
        .read_to_string(&mut input)
        .map_err(|err| format!("read hook stdin: {err}"))?;
    append_hook_event_from_str(&events_path, &input)
}

fn hook_events_arg(args: &[String]) -> Result<PathBuf, String> {
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--events" => {
                return args
                    .get(i + 1)
                    .map(PathBuf::from)
                    .ok_or_else(|| "claude-hook: --events requires a path".into());
            }
            other if other.starts_with("--events=") => {
                let value = other.split_once('=').map(|(_, value)| value).unwrap_or("");
                if value.is_empty() {
                    return Err("claude-hook: --events requires a path".into());
                }
                return Ok(PathBuf::from(value));
            }
            _ => i += 1,
        }
    }
    Err("claude-hook: missing --events path".into())
}

pub fn append_hook_event_from_str(events_path: &Path, input: &str) -> Result<(), String> {
    let mut value = match serde_json::from_str::<Value>(input) {
        Ok(Value::Object(map)) => Value::Object(map),
        Ok(other) => json!({ "zmux_raw_hook_value": other }),
        Err(_) => json!({ "zmux_raw_hook_stdin": input }),
    };
    if let Value::Object(map) = &mut value {
        map.insert("zmux_recorded_at_ms".into(), json!(now_ms()));
    }
    if let Some(parent) = events_path.parent() {
        ensure_private_dir(parent)
            .map_err(|err| format!("create private hook event dir {}: {err}", parent.display()))?;
    }
    let _lock = HookEventLock::acquire(events_path)?;
    let mut line =
        serde_json::to_vec(&value).map_err(|err| format!("serialize hook event JSON: {err}"))?;
    line.push(b'\n');
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(events_path)
        .map_err(|err| format!("open hook event file {}: {err}", events_path.display()))?;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|err| format!("secure hook event file {}: {err}", events_path.display()))?;
    file.write_all(&line)
        .map_err(|err| format!("write hook event line: {err}"))?;
    file.flush()
        .map_err(|err| format!("flush hook event file {}: {err}", events_path.display()))?;
    Ok(())
}

struct HookEventLock {
    path: PathBuf,
}

impl HookEventLock {
    fn acquire(events_path: &Path) -> Result<Self, String> {
        let path = event_lock_path(events_path);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match create_hook_lock_file(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                    if hook_lock_is_stale(&path) {
                        let _ = fs::remove_file(&path);
                        continue;
                    }
                    if Instant::now() >= deadline {
                        return Err(format!(
                            "timed out waiting for hook event lock {}",
                            path.display()
                        ));
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(err) => {
                    return Err(format!("create hook event lock {}: {err}", path.display()));
                }
            }
        }
    }
}

impl Drop for HookEventLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn event_lock_path(events_path: &Path) -> PathBuf {
    let file_name = events_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("events.jsonl");
    events_path.with_file_name(format!("{file_name}.lock"))
}

fn create_hook_lock_file(path: &Path) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    writeln!(file, "pid={}", std::process::id())?;
    Ok(())
}

fn hook_lock_is_stale(path: &Path) -> bool {
    let Ok(content) = fs::read_to_string(path) else {
        return true;
    };
    let Some(pid) = content
        .lines()
        .find_map(|line| line.strip_prefix("pid="))
        .and_then(|pid| pid.parse::<u32>().ok())
    else {
        return true;
    };
    !Path::new("/proc").join(pid.to_string()).exists()
}

pub fn event_cursor(path: &Path) -> Result<u64, String> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.len()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(err) => Err(format!("read hook event cursor {}: {err}", path.display())),
    }
}

#[derive(Debug)]
pub struct ClaudeHookPoller {
    events_path: PathBuf,
    cursor: u64,
    nonce: String,
    bound_session_id: Option<String>,
    bound_transcript_path: Option<String>,
    partial_line: String,
    saw_prompt: bool,
    last_failure: Option<String>,
}

impl ClaudeHookPoller {
    pub fn new(capture: ClaudeHookCapture, cursor: u64, nonce: String) -> Self {
        Self {
            events_path: capture.events_path,
            cursor,
            nonce,
            bound_session_id: None,
            bound_transcript_path: None,
            partial_line: String::new(),
            saw_prompt: false,
            last_failure: None,
        }
    }

    pub fn poll(&mut self, begin: &str, end: &str) -> Result<Option<String>, String> {
        let mut file = match File::open(&self.events_path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(format!(
                    "open Claude hook events {}: {err}",
                    self.events_path.display()
                ));
            }
        };
        file.seek(SeekFrom::Start(self.cursor))
            .map_err(|err| format!("seek Claude hook events: {err}"))?;
        let mut chunk = String::new();
        let read = file
            .read_to_string(&mut chunk)
            .map_err(|err| format!("read Claude hook events: {err}"))?;
        if read == 0 {
            return Ok(None);
        }
        self.cursor += read as u64;
        self.partial_line.push_str(&chunk);

        // A well-formed events file is newline-delimited JSON; a single
        // record is at most a few KB. If the buffer grows past the cap
        // without a newline the file is corrupt or being written by
        // something other than the hook appender — drop the runaway
        // partial so a missing terminator can't grow the buffer to OOM.
        if self.partial_line.len() > MAX_HOOK_LINE_BYTES && !self.partial_line.contains('\n') {
            self.partial_line.clear();
            return Ok(None);
        }

        while let Some(newline) = self.partial_line.find('\n') {
            let line = self.partial_line[..newline].trim().to_string();
            self.partial_line.drain(..=newline);
            if line.is_empty() {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if let Some(result) = self.process_event(&value, begin, end) {
                return Ok(Some(result));
            }
        }
        Ok(None)
    }

    pub fn status_hint(&self) -> Option<String> {
        if let Some(failure) = &self.last_failure {
            Some(format!("last Claude hook failure: {failure}"))
        } else if self.saw_prompt {
            Some("Claude hook saw this prompt but has not emitted a matching Stop event".into())
        } else {
            None
        }
    }

    fn process_event(&mut self, value: &Value, begin: &str, end: &str) -> Option<String> {
        let event = value.get("hook_event_name").and_then(Value::as_str)?;
        match event {
            "UserPromptSubmit" => {
                let prompt = value.get("prompt").and_then(Value::as_str).unwrap_or("");
                if prompt.contains(&self.nonce) {
                    self.saw_prompt = true;
                    self.bound_session_id = value
                        .get("session_id")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    self.bound_transcript_path = value
                        .get("transcript_path")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                }
                None
            }
            "Stop" if self.matches_bound(value) => value
                .get("last_assistant_message")
                .and_then(Value::as_str)
                .and_then(|text| extract_last_marked_result(text, begin, end)),
            "StopFailure" if self.matches_bound(value) => {
                self.last_failure = Some(compact_event(value));
                None
            }
            _ => None,
        }
    }

    fn matches_bound(&self, value: &Value) -> bool {
        if !self.saw_prompt {
            return false;
        }
        if let Some(bound) = &self.bound_session_id
            && value.get("session_id").and_then(Value::as_str) != Some(bound.as_str())
        {
            return false;
        }
        if let Some(bound) = &self.bound_transcript_path
            && value.get("transcript_path").and_then(Value::as_str) != Some(bound.as_str())
        {
            return false;
        }
        true
    }
}

fn take_settings_arg(startup_args: &mut Vec<String>) -> Result<Option<String>, String> {
    let mut cleaned = Vec::with_capacity(startup_args.len());
    let mut settings = None;
    let mut i = 0;
    while i < startup_args.len() {
        let arg = &startup_args[i];
        if arg == "--settings" {
            let value = startup_args
                .get(i + 1)
                .cloned()
                .ok_or_else(|| "claude: --settings requires a value".to_string())?;
            settings = Some(value);
            i += 2;
        } else if let Some(value) = arg.strip_prefix("--settings=") {
            settings = Some(value.to_string());
            i += 1;
        } else {
            cleaned.push(arg.clone());
            i += 1;
        }
    }
    *startup_args = cleaned;
    Ok(settings)
}

fn settings_key(user_settings: Option<&str>) -> String {
    let mut hasher = DefaultHasher::new();
    user_settings.unwrap_or("default").hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn write_settings(
    settings_path: &Path,
    events_path: &Path,
    user_settings: Option<&str>,
) -> Result<(), String> {
    let mut settings = match user_settings {
        Some(raw) => load_settings(raw)?,
        None => json!({}),
    };
    let Some(settings_object) = settings.as_object_mut() else {
        return Err("claude: --settings must resolve to a JSON object".into());
    };
    let command = hook_command(events_path)?;
    for event in HOOK_EVENTS {
        append_hook(settings_object, event, &command);
    }
    let rendered = serde_json::to_string_pretty(&settings)
        .map_err(|err| format!("render generated Claude settings: {err}"))?;
    if fs::read_to_string(settings_path).is_ok_and(|existing| existing == rendered) {
        fs::set_permissions(settings_path, fs::Permissions::from_mode(0o600)).map_err(|err| {
            format!(
                "secure generated Claude settings {}: {err}",
                settings_path.display()
            )
        })?;
        return Ok(());
    }
    if let Some(parent) = settings_path.parent() {
        ensure_private_dir(parent).map_err(|err| {
            format!(
                "create private Claude settings dir {}: {err}",
                parent.display()
            )
        })?;
    }
    let tmp = settings_path.with_extension(format!("json.tmp.{}", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|err| format!("open generated Claude settings {}: {err}", tmp.display()))?;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|err| format!("secure generated Claude settings {}: {err}", tmp.display()))?;
    file.write_all(rendered.as_bytes())
        .map_err(|err| format!("write generated Claude settings {}: {err}", tmp.display()))?;
    file.flush()
        .map_err(|err| format!("flush generated Claude settings {}: {err}", tmp.display()))?;
    drop(file);
    fs::rename(&tmp, settings_path).map_err(|err| {
        format!(
            "install generated Claude settings {}: {err}",
            settings_path.display()
        )
    })?;
    Ok(())
}

fn load_settings(raw: &str) -> Result<Value, String> {
    let trimmed = raw.trim_start();
    let content = if trimmed.starts_with('{') {
        raw.to_string()
    } else {
        fs::read_to_string(raw)
            .map_err(|err| format!("read Claude --settings file {raw}: {err}"))?
    };
    serde_json::from_str::<Value>(&content)
        .map_err(|err| format!("parse Claude --settings JSON: {err}"))
}

fn append_hook(settings: &mut Map<String, Value>, event: &str, command: &str) {
    let hooks = settings.entry("hooks").or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    let hooks = hooks.as_object_mut().expect("hooks object just created");
    let event_hooks = hooks.entry(event.to_string()).or_insert_with(|| json!([]));
    if !event_hooks.is_array() {
        *event_hooks = json!([]);
    }
    event_hooks
        .as_array_mut()
        .expect("event hooks array just created")
        .push(json!({
            "hooks": [
                {
                    "type": "command",
                    "command": command,
                }
            ]
        }));
}

fn hook_command(events_path: &Path) -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|err| format!("resolve zmux executable: {err}"))?;
    Ok(format!(
        "{} claude-hook --events {}",
        shell_quote(&exe.to_string_lossy()),
        shell_quote(&events_path.to_string_lossy())
    ))
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn compact_event(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "<unserializable hook event>".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Barrier, Mutex};
    use std::thread;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn temp_dir(name: &str) -> PathBuf {
        let path = env::temp_dir().join(format!("zmux-claude-hooks-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn with_state_dir<T>(path: &Path, f: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prior = env::var("ZMUX_STATE_DIR").ok();
        unsafe {
            env::set_var("ZMUX_STATE_DIR", path);
        }
        let out = f();
        unsafe {
            match prior {
                Some(value) => env::set_var("ZMUX_STATE_DIR", value),
                None => env::remove_var("ZMUX_STATE_DIR"),
            }
        }
        out
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn capture_state_is_private_even_with_a_permissive_umask() {
        let dir = temp_dir("private-modes");
        // The fixture intentionally starts world-traversable, matching a
        // common umask-created XDG state directory. prepare_capture must
        // tighten it rather than assuming the caller already did.
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();

        with_state_dir(&dir, || {
            let mut args = Vec::new();
            let capture = prepare_capture("work", &mut args).unwrap();
            append_hook_event_from_str(
                &capture.events_path,
                r#"{"hook_event_name":"UserPromptSubmit","prompt":"secret"}"#,
            )
            .unwrap();

            assert_eq!(mode(&dir), 0o700, "state root must not be traversable");
            assert_eq!(mode(capture.settings_path.parent().unwrap()), 0o700);
            assert_eq!(mode(&capture.settings_path), 0o600);
            assert_eq!(mode(capture.events_path.parent().unwrap()), 0o700);
            assert_eq!(mode(&capture.events_path), 0o600);
        });
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn prepare_capture_adds_generated_settings_and_preserves_other_args() {
        let dir = temp_dir("prepare");
        with_state_dir(&dir, || {
            let mut args = vec!["--model".into(), "sonnet".into()];
            let capture = prepare_capture("work/session", &mut args).unwrap();
            assert_eq!(args[0], "--model");
            assert_eq!(args[1], "sonnet");
            assert_eq!(args[2], "--settings");
            assert_eq!(PathBuf::from(&args[3]), capture.settings_path);
            assert!(capture.events_path.ends_with("events.jsonl"));
            let settings = fs::read_to_string(&capture.settings_path).unwrap();
            assert!(settings.contains("UserPromptSubmit"));
            assert!(settings.contains("StopFailure"));
            assert!(settings.contains("claude-hook"));
        });
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn prepare_capture_merges_existing_settings_and_removes_user_settings_arg() {
        let dir = temp_dir("merge");
        let user_settings = dir.join("user-settings.json");
        fs::write(
            &user_settings,
            r#"{
                "model": "sonnet",
                "hooks": {
                    "Stop": [{"hooks": [{"type": "command", "command": "echo user"}]}]
                }
            }"#,
        )
        .unwrap();
        with_state_dir(&dir, || {
            let mut args = vec![
                "--settings".into(),
                user_settings.to_string_lossy().into_owned(),
                "--permission-mode".into(),
                "plan".into(),
            ];
            let capture = prepare_capture("work", &mut args).unwrap();
            assert_eq!(args[0], "--permission-mode");
            assert_eq!(args[1], "plan");
            assert_eq!(args[2], "--settings");
            let settings: Value =
                serde_json::from_str(&fs::read_to_string(&capture.settings_path).unwrap()).unwrap();
            assert_eq!(
                settings.get("model").and_then(Value::as_str),
                Some("sonnet")
            );
            let stop_hooks = settings
                .pointer("/hooks/Stop")
                .and_then(Value::as_array)
                .unwrap();
            assert_eq!(stop_hooks.len(), 2);
        });
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn hook_event_append_writes_one_json_line() {
        let dir = temp_dir("append");
        let events = dir.join("events.jsonl");
        append_hook_event_from_str(&events, r#"{"hook_event_name":"Stop","session_id":"s"}"#)
            .unwrap();
        let lines = fs::read_to_string(&events).unwrap();
        let value: Value = serde_json::from_str(lines.trim()).unwrap();
        assert_eq!(
            value.get("hook_event_name").and_then(Value::as_str),
            Some("Stop")
        );
        assert!(value.get("zmux_recorded_at_ms").is_some());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn hook_event_append_serializes_concurrent_writers() {
        let dir = temp_dir("append-concurrent");
        let events = dir.join("events.jsonl");
        let writers = 16;
        let barrier = Arc::new(Barrier::new(writers));
        let mut handles = Vec::new();
        for i in 0..writers {
            let events = events.clone();
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                append_hook_event_from_str(
                    &events,
                    &format!(r#"{{"hook_event_name":"Stop","session_id":"s-{i}"}}"#),
                )
                .unwrap();
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }

        let content = fs::read_to_string(&events).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), writers);
        let mut seen = Vec::new();
        for line in lines {
            let value: Value = serde_json::from_str(line).unwrap();
            seen.push(
                value
                    .get("session_id")
                    .and_then(Value::as_str)
                    .unwrap()
                    .to_string(),
            );
        }
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), writers);
        assert!(!event_lock_path(&events).exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn poller_binds_prompt_nonce_and_extracts_matching_stop() {
        let dir = temp_dir("poller");
        let events = dir.join("events.jsonl");
        let capture = ClaudeHookCapture {
            events_path: events.clone(),
            settings_path: dir.join("settings.json"),
        };
        let cursor = event_cursor(&events).unwrap();
        let mut poller = ClaudeHookPoller::new(capture, cursor, "nonce-1".into());

        append_hook_event_from_str(
            &events,
            r#"{"hook_event_name":"UserPromptSubmit","session_id":"other","transcript_path":"/tmp/other","prompt":"nonce-1"}"#,
        )
        .unwrap();
        append_hook_event_from_str(
            &events,
            r#"{"hook_event_name":"Stop","session_id":"wrong","transcript_path":"/tmp/other","last_assistant_message":"ZMUX_RESULT_BEGIN_nonce-1\nwrong\nZMUX_RESULT_END_nonce-1"}"#,
        )
        .unwrap();
        assert!(
            poller
                .poll("ZMUX_RESULT_BEGIN_nonce-1", "ZMUX_RESULT_END_nonce-1")
                .unwrap()
                .is_none()
        );

        append_hook_event_from_str(
            &events,
            r#"{"hook_event_name":"Stop","session_id":"other","transcript_path":"/tmp/other","last_assistant_message":"ZMUX_RESULT_BEGIN_nonce-1\nright\nZMUX_RESULT_END_nonce-1"}"#,
        )
        .unwrap();
        assert_eq!(
            poller
                .poll("ZMUX_RESULT_BEGIN_nonce-1", "ZMUX_RESULT_END_nonce-1")
                .unwrap(),
            Some("right".into())
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn poller_uses_start_cursor_to_ignore_old_turns() {
        let dir = temp_dir("cursor");
        let events = dir.join("events.jsonl");
        append_hook_event_from_str(
            &events,
            r#"{"hook_event_name":"UserPromptSubmit","session_id":"s","transcript_path":"/tmp/s","prompt":"nonce-old"}"#,
        )
        .unwrap();
        append_hook_event_from_str(
            &events,
            r#"{"hook_event_name":"Stop","session_id":"s","transcript_path":"/tmp/s","last_assistant_message":"ZMUX_RESULT_BEGIN_nonce-old\nold\nZMUX_RESULT_END_nonce-old"}"#,
        )
        .unwrap();
        let cursor = event_cursor(&events).unwrap();
        let capture = ClaudeHookCapture {
            events_path: events.clone(),
            settings_path: dir.join("settings.json"),
        };
        let mut poller = ClaudeHookPoller::new(capture, cursor, "nonce-new".into());
        append_hook_event_from_str(
            &events,
            r#"{"hook_event_name":"UserPromptSubmit","session_id":"s","transcript_path":"/tmp/s","prompt":"nonce-new"}"#,
        )
        .unwrap();
        append_hook_event_from_str(
            &events,
            r#"{"hook_event_name":"Stop","session_id":"s","transcript_path":"/tmp/s","last_assistant_message":"ZMUX_RESULT_BEGIN_nonce-new\nnew\nZMUX_RESULT_END_nonce-new"}"#,
        )
        .unwrap();
        assert_eq!(
            poller
                .poll("ZMUX_RESULT_BEGIN_nonce-new", "ZMUX_RESULT_END_nonce-new")
                .unwrap(),
            Some("new".into())
        );
        let _ = fs::remove_dir_all(dir);
    }
}
