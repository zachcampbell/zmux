// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `zmux codex` shim: drive a live interactive Codex pane through MCP.
//!
//! This mirrors `zmux claude`: prompt-in/stdout-out for workers, with
//! the actual Codex TUI kept alive and attachable in a zmux pane.

use crate::agent_shim::{
    self, AgentShimArgs, DEFAULT_TIMEOUT_MS, DEFAULT_WAIT_LINES, OutputMode, ResultCapture,
    ZmuxResumeTarget,
};

const AGENT_NAME: &str = "codex";
const DEFAULT_LABEL: &str = "zmux-codex";
const DEFAULT_COMMAND: &str = "codex";
const RESULT_TYPE: &str = "zmux.codex.result";

pub type CodexArgs = AgentShimArgs;

pub fn parse_args(args: &[String], default_session: &str) -> Result<CodexArgs, String> {
    let mut session = default_session.to_string();
    let mut command = DEFAULT_COMMAND.to_string();
    let mut label = DEFAULT_LABEL.to_string();
    let mut timeout_ms = DEFAULT_TIMEOUT_MS;
    let mut wait_lines = DEFAULT_WAIT_LINES;
    let mut force_new = false;
    let mut kill_after = false;
    let mut output_mode = OutputMode::Text;
    let mut resume_target: Option<ZmuxResumeTarget> = None;
    let mut startup_args: Vec<String> = Vec::new();
    let mut prompt_parts: Vec<String> = Vec::new();

    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--session" => {
                session = agent_shim::value_after(args, i, "codex: --session requires a value")?;
                i += 2;
            }
            "--command" => {
                command = agent_shim::value_after(args, i, "codex: --command requires a value")?;
                i += 2;
            }
            "--label" => {
                label = agent_shim::value_after(args, i, "codex: --label requires a value")?;
                i += 2;
            }
            "--timeout-ms" => {
                let raw = agent_shim::value_after(args, i, "codex: --timeout-ms requires a value")?;
                timeout_ms = raw
                    .parse()
                    .map_err(|_| format!("codex: --timeout-ms must be an integer (got `{raw}`)"))?;
                i += 2;
            }
            "--wait-lines" => {
                let raw = agent_shim::value_after(args, i, "codex: --wait-lines requires a value")?;
                wait_lines = raw
                    .parse()
                    .map_err(|_| format!("codex: --wait-lines must be an integer (got `{raw}`)"))?;
                if wait_lines == 0 {
                    return Err("codex: --wait-lines must be >= 1".into());
                }
                i += 2;
            }
            "--new" => {
                force_new = true;
                i += 1;
            }
            "--kill" => {
                kill_after = true;
                i += 1;
            }
            "--keep" => {
                kill_after = false;
                i += 1;
            }
            "--worker-json" | "--json" => {
                output_mode = OutputMode::WorkerJson;
                i += 1;
            }
            "--output-format" => {
                let raw =
                    agent_shim::value_after(args, i, "codex: --output-format requires a value")?;
                output_mode = agent_shim::parse_wrapper_output_format(AGENT_NAME, &raw)?;
                i += 2;
            }
            other if other.starts_with("--output-format=") => {
                let raw = other
                    .split_once('=')
                    .map(|(_, value)| value)
                    .unwrap_or_default();
                output_mode = agent_shim::parse_wrapper_output_format(AGENT_NAME, raw)?;
                i += 1;
            }
            "--resume" => {
                let raw =
                    agent_shim::value_after(args, i, "codex: --resume requires a zmux session id")?;
                if raw.starts_with("zmux:") {
                    let target = agent_shim::parse_zmux_session_id(AGENT_NAME, &raw)?;
                    session = target.session.clone();
                    resume_target = Some(target);
                    i += 2;
                } else {
                    return Err(
                        "codex: --resume only accepts zmux:<session>:<pane_id>; use --command \"codex resume ...\" for native Codex resume".into(),
                    );
                }
            }
            other if other.starts_with("--resume=") => {
                let raw = other
                    .split_once('=')
                    .map(|(_, value)| value)
                    .unwrap_or_default();
                if raw.starts_with("zmux:") {
                    let target = agent_shim::parse_zmux_session_id(AGENT_NAME, raw)?;
                    session = target.session.clone();
                    resume_target = Some(target);
                } else {
                    return Err(
                        "codex: --resume only accepts zmux:<session>:<pane_id>; use --command \"codex resume ...\" for native Codex resume".into(),
                    );
                }
                i += 1;
            }
            "--" => {
                prompt_parts.extend(args[i + 1..].iter().cloned());
                break;
            }
            "--help" | "-h" => return Err(usage()),
            other if other.starts_with('-') => {
                let flag = other.to_string();
                startup_args.push(flag.clone());
                if agent_shim::flag_has_inline_value(&flag) {
                    i += 1;
                } else if codex_flag_consumes_many(&flag) {
                    i += 1;
                    while i < args.len() && !args[i].starts_with('-') && i + 1 < args.len() {
                        startup_args.push(args[i].clone());
                        i += 1;
                    }
                } else if codex_flag_requires_value(&flag) {
                    let value = agent_shim::value_after(
                        args,
                        i,
                        &format!("codex: {flag} requires a value"),
                    )?;
                    startup_args.push(value);
                    i += 2;
                } else {
                    i += 1;
                }
            }
            _ => {
                prompt_parts.extend(args[i..].iter().cloned());
                break;
            }
        }
    }

    let prompt = prompt_parts.join(" ").trim().to_string();
    if prompt.is_empty() {
        return Err("codex: missing prompt (usage: zmux codex [flags] \"prompt\")".into());
    }
    if command.trim().is_empty() {
        return Err("codex: --command must not be empty".into());
    }
    if label.trim().is_empty() {
        return Err("codex: --label must not be empty".into());
    }
    if timeout_ms == 0 {
        return Err("codex: --timeout-ms must be >= 1".into());
    }

    Ok(AgentShimArgs {
        agent_name: AGENT_NAME,
        result_type: RESULT_TYPE,
        session,
        command,
        label,
        timeout_ms,
        wait_lines,
        force_new,
        kill_after,
        output_mode,
        result_capture: ResultCapture::TerminalOnly,
        resume_target,
        startup_args,
        prompt,
    })
}

fn codex_flag_requires_value(flag: &str) -> bool {
    matches!(
        flag,
        "-a" | "--ask-for-approval"
            | "--add-dir"
            | "-c"
            | "--config"
            | "-C"
            | "--cd"
            | "-m"
            | "--model"
            | "--local-provider"
            | "-p"
            | "--profile"
            | "--profile-v2"
            | "--remote"
            | "--remote-auth-token-env"
            | "-s"
            | "--sandbox"
    )
}

fn codex_flag_consumes_many(flag: &str) -> bool {
    matches!(flag, "-i" | "--image")
}

pub fn usage() -> String {
    "codex usage: zmux codex [zmux flags] [codex flags] \"prompt\"
  zmux flags: --session <name> --command <cmd> --label <label> --timeout-ms <ms> --wait-lines <n> --new --kill --keep --worker-json
  output: default is plain text; --worker-json or --output-format json emits {result,session_id,...}
  codex flags are passed to the spawned Codex pane, e.g. --model gpt-5.4 -C /repo --dangerously-bypass-approvals-and-sandbox
  zmux resume ids use --resume zmux:<session>:<pane_id>; use --command \"codex resume ...\" for native Codex resume".into()
}

pub fn run(args: CodexArgs) -> Result<(), String> {
    agent_shim::run(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_args() -> CodexArgs {
        CodexArgs {
            agent_name: AGENT_NAME,
            result_type: RESULT_TYPE,
            session: "default".into(),
            command: "codex".into(),
            label: DEFAULT_LABEL.into(),
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
    fn parse_collects_prompt_and_flags() {
        let args = vec![
            "zmux".into(),
            "codex".into(),
            "--session".into(),
            "work".into(),
            "--new".into(),
            "hello".into(),
            "there".into(),
        ];
        let parsed = parse_args(&args, "default").unwrap();
        assert_eq!(parsed.session, "work");
        assert!(parsed.force_new);
        assert_eq!(parsed.output_mode, OutputMode::Text);
        assert!(parsed.resume_target.is_none());
        assert!(parsed.startup_args.is_empty());
        assert_eq!(parsed.prompt, "hello there");
    }

    #[test]
    fn parse_worker_json_output_modes() {
        let args = vec![
            "zmux".into(),
            "codex".into(),
            "--worker-json".into(),
            "hello".into(),
        ];
        let parsed = parse_args(&args, "default").unwrap();
        assert_eq!(parsed.output_mode, OutputMode::WorkerJson);
        assert!(parsed.startup_args.is_empty());
        assert_eq!(parsed.prompt, "hello");

        let args = vec![
            "zmux".into(),
            "codex".into(),
            "--output-format=json".into(),
            "hello".into(),
        ];
        let parsed = parse_args(&args, "default").unwrap();
        assert_eq!(parsed.output_mode, OutputMode::WorkerJson);
    }

    #[test]
    fn parse_rejects_unsupported_output_format() {
        let args = vec![
            "zmux".into(),
            "codex".into(),
            "--output-format".into(),
            "stream-json".into(),
            "hello".into(),
        ];
        let err = parse_args(&args, "default").unwrap_err();
        assert!(err.contains("not supported"));
    }

    #[test]
    fn parse_passes_codex_startup_flags() {
        let args = vec![
            "zmux".into(),
            "codex".into(),
            "--model".into(),
            "gpt-5.4".into(),
            "-C".into(),
            "/tmp/project".into(),
            "--add-dir".into(),
            "/tmp".into(),
            "--dangerously-bypass-approvals-and-sandbox".into(),
            "hello there".into(),
        ];
        let parsed = parse_args(&args, "default").unwrap();
        assert_eq!(
            parsed.startup_args,
            vec![
                "--model".to_string(),
                "gpt-5.4".to_string(),
                "-C".to_string(),
                "/tmp/project".to_string(),
                "--add-dir".to_string(),
                "/tmp".to_string(),
                "--dangerously-bypass-approvals-and-sandbox".to_string(),
            ],
        );
        assert_eq!(parsed.prompt, "hello there");
    }

    #[test]
    fn profile_short_p_is_codex_startup_flag() {
        let args = vec![
            "zmux".into(),
            "codex".into(),
            "-p".into(),
            "work".into(),
            "hello".into(),
        ];
        let parsed = parse_args(&args, "default").unwrap();
        assert_eq!(
            parsed.startup_args,
            vec!["-p".to_string(), "work".to_string()]
        );
        assert_eq!(parsed.prompt, "hello");
    }

    #[test]
    fn parse_zmux_resume_targets_existing_pane() {
        let args = vec![
            "zmux".into(),
            "codex".into(),
            "--resume".into(),
            "zmux:work:10042".into(),
            "continue here".into(),
        ];
        let parsed = parse_args(&args, "default").unwrap();
        assert_eq!(parsed.session, "work");
        assert_eq!(
            parsed.resume_target,
            Some(ZmuxResumeTarget {
                session: "work".into(),
                pane_id: 10042,
            })
        );
        assert!(parsed.startup_args.is_empty());
        assert_eq!(parsed.prompt, "continue here");
    }

    #[test]
    fn parse_rejects_plain_codex_resume_flag() {
        let args = vec![
            "zmux".into(),
            "codex".into(),
            "--resume".into(),
            "abc123".into(),
            "hello".into(),
        ];
        let err = parse_args(&args, "default").unwrap_err();
        assert!(err.contains("--command"));
    }

    #[test]
    fn spawn_command_quotes_codex_args_for_shell() {
        let args = CodexArgs {
            startup_args: vec!["--model".into(), "gpt-5.4".into(), "it's fine".into()],
            ..base_args()
        };
        assert_eq!(
            agent_shim::spawn_command(&args.command, &args.startup_args),
            "codex '--model' 'gpt-5.4' 'it'\\''s fine'"
        );
    }

    #[test]
    fn worker_json_result_matches_codex_worker_shape() {
        let args = CodexArgs {
            session: "work".into(),
            output_mode: OutputMode::WorkerJson,
            ..base_args()
        };
        let payload = agent_shim::worker_json_result(&args, 10042, "done");
        assert_eq!(
            payload.get("result").and_then(|value| value.as_str()),
            Some("done")
        );
        assert_eq!(
            payload.get("session_id").and_then(|value| value.as_str()),
            Some("zmux:work:10042")
        );
        assert_eq!(
            payload.get("type").and_then(|value| value.as_str()),
            Some("zmux.codex.result")
        );
    }
}
