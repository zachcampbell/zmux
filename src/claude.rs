// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `zmux claude` shim: drive a live interactive Claude pane through MCP.
//!
//! The goal is to make the common `claude -p "prompt"` shape feel close
//! enough while keeping the session non-headless and human-attachable.

use crate::agent_shim::{
    self, AgentShimArgs, DEFAULT_TIMEOUT_MS, DEFAULT_WAIT_LINES, OutputMode, ResultCapture,
    ZmuxResumeTarget,
};

const AGENT_NAME: &str = "claude";
const DEFAULT_LABEL: &str = "zmux-claude";
const DEFAULT_COMMAND: &str = "claude";
const RESULT_TYPE: &str = "zmux.claude.result";

pub type ClaudeArgs = AgentShimArgs;

pub fn parse_args(args: &[String], default_session: &str) -> Result<ClaudeArgs, String> {
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
                session = agent_shim::value_after(args, i, "claude: --session requires a value")?;
                i += 2;
            }
            "--command" => {
                command = agent_shim::value_after(args, i, "claude: --command requires a value")?;
                i += 2;
            }
            "--label" => {
                label = agent_shim::value_after(args, i, "claude: --label requires a value")?;
                i += 2;
            }
            "--timeout-ms" => {
                let raw =
                    agent_shim::value_after(args, i, "claude: --timeout-ms requires a value")?;
                timeout_ms = raw.parse().map_err(|_| {
                    format!("claude: --timeout-ms must be an integer (got `{raw}`)")
                })?;
                i += 2;
            }
            "--wait-lines" => {
                let raw =
                    agent_shim::value_after(args, i, "claude: --wait-lines requires a value")?;
                wait_lines = raw.parse().map_err(|_| {
                    format!("claude: --wait-lines must be an integer (got `{raw}`)")
                })?;
                if wait_lines == 0 {
                    return Err("claude: --wait-lines must be >= 1".into());
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
                    agent_shim::value_after(args, i, "claude: --output-format requires a value")?;
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
            "--resume" | "-r" => {
                let flag = args[i].clone();
                if let Some(raw) = args.get(i + 1)
                    && raw.starts_with("zmux:")
                {
                    let target = agent_shim::parse_zmux_session_id(AGENT_NAME, raw)?;
                    session = target.session.clone();
                    resume_target = Some(target);
                    i += 2;
                } else {
                    startup_args.push(flag);
                    if i + 2 < args.len() && !args[i + 1].starts_with('-') {
                        startup_args.push(args[i + 1].clone());
                        i += 2;
                    } else {
                        i += 1;
                    }
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
                    startup_args.push(other.to_string());
                }
                i += 1;
            }
            "--" => {
                prompt_parts.extend(args[i + 1..].iter().cloned());
                break;
            }
            "--help" | "-h" => return Err(usage()),
            "--print" | "-p" => {
                i += 1;
            }
            other if other.starts_with('-') => {
                let flag = other.to_string();
                startup_args.push(flag.clone());
                if agent_shim::flag_has_inline_value(&flag) {
                    i += 1;
                } else if claude_flag_consumes_many(&flag) {
                    i += 1;
                    while i < args.len() && !args[i].starts_with('-') && i + 1 < args.len() {
                        startup_args.push(args[i].clone());
                        i += 1;
                    }
                } else if claude_flag_requires_value(&flag) {
                    let value = agent_shim::value_after(
                        args,
                        i,
                        &format!("claude: {flag} requires a value"),
                    )?;
                    startup_args.push(value);
                    i += 2;
                } else if claude_flag_accepts_optional_value(&flag)
                    && i + 2 < args.len()
                    && !args[i + 1].starts_with('-')
                {
                    startup_args.push(args[i + 1].clone());
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
        return Err("claude: missing prompt (usage: zmux claude [flags] \"prompt\")".into());
    }
    if command.trim().is_empty() {
        return Err("claude: --command must not be empty".into());
    }
    if label.trim().is_empty() {
        return Err("claude: --label must not be empty".into());
    }
    if timeout_ms == 0 {
        return Err("claude: --timeout-ms must be >= 1".into());
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

fn claude_flag_requires_value(flag: &str) -> bool {
    matches!(
        flag,
        "--agent"
            | "--agents"
            | "--append-system-prompt"
            | "--debug-file"
            | "--effort"
            | "--fallback-model"
            | "--input-format"
            | "--json-schema"
            | "--max-budget-usd"
            | "--model"
            | "-n"
            | "--name"
            | "--output-format"
            | "--permission-mode"
            | "--remote-control-session-name-prefix"
            | "--session-id"
            | "--setting-sources"
            | "--settings"
            | "--system-prompt"
    )
}

fn claude_flag_accepts_optional_value(flag: &str) -> bool {
    matches!(
        flag,
        "-d" | "--debug"
            | "--from-pr"
            | "-r"
            | "--resume"
            | "--remote-control"
            | "--tmux"
            | "-w"
            | "--worktree"
    )
}

fn claude_flag_consumes_many(flag: &str) -> bool {
    matches!(
        flag,
        "--add-dir"
            | "--allowedTools"
            | "--allowed-tools"
            | "--betas"
            | "--disallowedTools"
            | "--disallowed-tools"
            | "--file"
            | "--mcp-config"
            | "--plugin-dir"
            | "--plugin-url"
            | "--tools"
    )
}

pub fn usage() -> String {
    "claude usage: zmux claude [zmux flags] [claude flags] \"prompt\"
  zmux flags: --session <name> --command <cmd> --label <label> --timeout-ms <ms> --wait-lines <n> --new --kill --keep --worker-json
  output: default is plain text; --worker-json or --output-format json emits {result,session_id,...}
  claude flags are passed to the spawned Claude pane, e.g. --model sonnet --permission-mode bypassPermissions --resume <id>".into()
}

pub fn run(mut args: ClaudeArgs) -> Result<(), String> {
    let capture = crate::claude_hooks::prepare_capture(&args.session, &mut args.startup_args)?;
    args.result_capture = ResultCapture::ClaudeHooks(capture);
    agent_shim::run(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_args() -> ClaudeArgs {
        ClaudeArgs {
            agent_name: AGENT_NAME,
            result_type: RESULT_TYPE,
            session: "default".into(),
            command: "claude".into(),
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
            "claude".into(),
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
            "claude".into(),
            "--worker-json".into(),
            "hello".into(),
        ];
        let parsed = parse_args(&args, "default").unwrap();
        assert_eq!(parsed.output_mode, OutputMode::WorkerJson);
        assert!(parsed.startup_args.is_empty());
        assert_eq!(parsed.prompt, "hello");

        let args = vec![
            "zmux".into(),
            "claude".into(),
            "--output-format".into(),
            "json".into(),
            "hello".into(),
        ];
        let parsed = parse_args(&args, "default").unwrap();
        assert_eq!(parsed.output_mode, OutputMode::WorkerJson);
        assert!(parsed.startup_args.is_empty());

        let args = vec![
            "zmux".into(),
            "claude".into(),
            "--output-format=text".into(),
            "hello".into(),
        ];
        let parsed = parse_args(&args, "default").unwrap();
        assert_eq!(parsed.output_mode, OutputMode::Text);
    }

    #[test]
    fn parse_rejects_unsupported_output_format() {
        let args = vec![
            "zmux".into(),
            "claude".into(),
            "--output-format".into(),
            "stream-json".into(),
            "hello".into(),
        ];
        let err = parse_args(&args, "default").unwrap_err();
        assert!(err.contains("not supported"));
    }

    #[test]
    fn parse_passes_claude_model_and_permission_flags() {
        let args = vec![
            "zmux".into(),
            "claude".into(),
            "--model".into(),
            "sonnet".into(),
            "--dangerously-skip-permissions".into(),
            "hello there".into(),
        ];
        let parsed = parse_args(&args, "default").unwrap();
        assert_eq!(
            parsed.startup_args,
            vec![
                "--model".to_string(),
                "sonnet".to_string(),
                "--dangerously-skip-permissions".to_string(),
            ],
        );
        assert_eq!(parsed.prompt, "hello there");
    }

    #[test]
    fn parse_resume_accepts_optional_session_id() {
        let args = vec![
            "zmux".into(),
            "claude".into(),
            "--resume".into(),
            "abc123".into(),
            "pick this back up".into(),
        ];
        let parsed = parse_args(&args, "default").unwrap();
        assert_eq!(
            parsed.startup_args,
            vec!["--resume".to_string(), "abc123".to_string()],
        );
        assert_eq!(parsed.prompt, "pick this back up");
        assert!(parsed.resume_target.is_none());

        let args = vec![
            "zmux".into(),
            "claude".into(),
            "--resume".into(),
            "resume newest".into(),
        ];
        let parsed = parse_args(&args, "default").unwrap();
        assert_eq!(parsed.startup_args, vec!["--resume".to_string()]);
        assert_eq!(parsed.prompt, "resume newest");
    }

    #[test]
    fn parse_zmux_resume_targets_existing_pane() {
        let args = vec![
            "zmux".into(),
            "claude".into(),
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

        let args = vec![
            "zmux".into(),
            "claude".into(),
            "--resume=zmux:work:10042".into(),
            "again".into(),
        ];
        let parsed = parse_args(&args, "default").unwrap();
        assert_eq!(parsed.session, "work");
        assert_eq!(
            parsed.resume_target.as_ref().map(|target| target.pane_id),
            Some(10042)
        );
        assert_eq!(parsed.prompt, "again");
    }

    #[test]
    fn print_flag_is_compatibility_sugar() {
        let args = vec![
            "zmux".into(),
            "claude".into(),
            "-p".into(),
            "--model".into(),
            "opus".into(),
            "hello".into(),
        ];
        let parsed = parse_args(&args, "default").unwrap();
        assert_eq!(
            parsed.startup_args,
            vec!["--model".to_string(), "opus".to_string()]
        );
        assert_eq!(parsed.prompt, "hello");
    }

    #[test]
    fn spawn_command_quotes_claude_args_for_shell() {
        let args = ClaudeArgs {
            startup_args: vec!["--model".into(), "sonnet".into(), "it's fine".into()],
            ..base_args()
        };
        assert_eq!(
            agent_shim::spawn_command(&args.command, &args.startup_args),
            "claude '--model' 'sonnet' 'it'\\''s fine'"
        );
    }

    #[test]
    fn worker_json_result_matches_claude_worker_shape() {
        let args = ClaudeArgs {
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
            payload.get("pane_id").and_then(|value| value.as_u64()),
            Some(10042)
        );
        assert_eq!(
            payload.get("type").and_then(|value| value.as_str()),
            Some("zmux.claude.result")
        );
    }

    #[test]
    fn worker_json_result_omits_session_id_when_killing_pane() {
        let args = ClaudeArgs {
            kill_after: true,
            output_mode: OutputMode::WorkerJson,
            ..base_args()
        };
        let payload = agent_shim::worker_json_result(&args, 10042, "done");
        assert!(
            payload
                .get("session_id")
                .is_some_and(|value| value.is_null())
        );
    }

    #[test]
    fn wrapper_does_not_include_exact_markers() {
        let prompt =
            agent_shim::wrap_prompt("hello", "ZMUX_RESULT_BEGIN_abc", "ZMUX_RESULT_END_abc");
        assert!(!prompt.contains("ZMUX_RESULT_BEGIN_abc"));
        assert!(!prompt.contains("ZMUX_RESULT_END_abc"));
        assert!(prompt.contains("ZMUX_RESULT_BEGIN_"));
        assert!(prompt.contains("ZMUX_RESULT_END_"));
        assert!(prompt.contains("abc"));
    }

    #[test]
    fn extract_uses_last_marker_pair() {
        let text = "prompt ZMUX_RESULT_BEGIN_a old ZMUX_RESULT_END_a\nanswer\nZMUX_RESULT_BEGIN_a\nnew result\nZMUX_RESULT_END_a";
        assert_eq!(
            agent_shim::extract_last_marked_result(
                text,
                "ZMUX_RESULT_BEGIN_a",
                "ZMUX_RESULT_END_a"
            )
            .unwrap(),
            "new result"
        );
    }
}
