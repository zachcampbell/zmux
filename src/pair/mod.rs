// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `zmux pair` subcommand — hybrid AI co-pilot bound to a sibling pane.
//!
//! This module owns the CLI entrypoint and the mediator/REPL wiring;
//! pure state lives in [`conversation`], MCP transport in [`client`],
//! model HTTP in [`model`].

use std::sync::Arc;
use std::sync::mpsc;

use crate::events::Event;
use crate::pair::client::Client;
use crate::pair::conversation::{Trigger, TriggerKind};
use crate::pair::model::OllamaClient;

pub mod client;
pub mod conversation;
pub mod model;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairArgs {
    pub session: String,
    pub target: u32,
    pub model: String,
}

pub const DEFAULT_MODEL: &str = "minimax-m2.7:cloud";

/// Recognized flags (positional `args` includes `argv[0]`):
///   `--session <name>`   default: `zmux::default_session_name()`
///   `--target  <id>`     required, u32
///   `--model   <name>`   default: `DEFAULT_MODEL`
pub fn parse_args(args: &[String], default_session: &str) -> Result<PairArgs, String> {
    let mut session = default_session.to_string();
    let mut target: Option<u32> = None;
    let mut model = DEFAULT_MODEL.to_string();

    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--session" => {
                session = args
                    .get(i + 1)
                    .ok_or_else(|| "pair: --session requires a value".to_string())?
                    .clone();
                i += 2;
            }
            "--target" => {
                let raw = args
                    .get(i + 1)
                    .ok_or_else(|| "pair: --target requires a pane id".to_string())?;
                target = Some(
                    raw.parse()
                        .map_err(|_| format!("pair: --target must be an integer (got `{raw}`)"))?,
                );
                i += 2;
            }
            "--model" => {
                model = args
                    .get(i + 1)
                    .ok_or_else(|| "pair: --model requires a value".to_string())?
                    .clone();
                i += 2;
            }
            other => return Err(format!("pair: unknown argument `{other}`")),
        }
    }

    let target = target.ok_or_else(|| {
        "pair: missing --target <pane_id> (zmux pair --target <id> [--model <name>] [--session <name>])"
            .to_string()
    })?;

    Ok(PairArgs {
        session,
        target,
        model,
    })
}

pub fn run(args: PairArgs) -> Result<(), String> {
    let probe_client = Client::connect(&args.session)
        .map_err(|err| format!("cannot connect to <{}>.mcp.sock: {err}", args.session))?;
    probe_client
        .initialize()
        .map_err(|err| format!("MCP initialize failed: {err}"))?;
    let panes = probe_client
        .call_tool("list_panes", serde_json::json!({}))
        .map_err(|err| format!("list_panes failed: {err}"))?;
    let target_exists = panes["panes"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .any(|p| p["pane_id"].as_u64().map(|id| id as u32) == Some(args.target))
        })
        .unwrap_or(false);
    if !target_exists {
        return Err(format!(
            "pane {} not found in session `{}`",
            args.target, args.session
        ));
    }
    drop(probe_client);

    OllamaClient::new().probe()?;

    let (mediator_tx, mediator_rx) = mpsc::channel::<MediatorInput>();
    let (confirm_tx, confirm_rx) = mpsc::channel::<bool>();
    let (output_tx, output_rx) = mpsc::channel::<MediatorOutput>();
    let (signal_tx, signal_rx) = mpsc::channel::<WatcherSignal>();

    {
        let mediator_tx = mediator_tx.clone();
        std::thread::spawn(move || {
            for sig in signal_rx.iter() {
                let msg = match sig {
                    WatcherSignal::Trigger(t) => MediatorInput::Trigger(t),
                    WatcherSignal::TargetClosed => {
                        MediatorInput::Shutdown("[info] target pane closed; exiting".to_string())
                    }
                    WatcherSignal::Disconnected => MediatorInput::Shutdown(
                        "[error] zmux daemon disconnected; exiting".to_string(),
                    ),
                };
                if mediator_tx.send(msg).is_err() {
                    return;
                }
            }
        });
    }

    let _watcher = spawn_watcher(&args, signal_tx)?;

    let mediator_args = args.clone();
    let mediator_outbox = output_tx.clone();
    let mediator_handle = std::thread::spawn(move || {
        run_mediator(&mediator_args, mediator_rx, confirm_rx, mediator_outbox);
    });

    let repl_result = run_repl(args.clone(), mediator_tx.clone(), confirm_tx, output_rx);

    let _ = mediator_tx.send(MediatorInput::Quit);
    let _ = mediator_handle.join();

    repl_result
}

pub enum WatcherSignal {
    Trigger(Trigger),
    TargetClosed,
    Disconnected,
}

fn classify_event(target: u32, event: &Event) -> Option<TriggerKind> {
    match event {
        Event::PaneStateChanged { pane_id, from, to } if *pane_id == target => {
            if to == "Errored" || to == "AwaitingInput" {
                Some(TriggerKind::StateChanged {
                    from: from.clone(),
                    to: to.clone(),
                })
            } else {
                None
            }
        }
        Event::PaneExited { pane_id, exit_code } if *pane_id == target && *exit_code != 0 => {
            Some(TriggerKind::Exited {
                exit_code: *exit_code,
            })
        }
        _ => None,
    }
}

/// `watch_events` consumes the connection's reader, so the watcher
/// owns the long-lived `Client` and opens a second short-lived one
/// for snapshot scrollback fetches.
pub fn spawn_watcher(
    args: &PairArgs,
    signal_tx: mpsc::Sender<WatcherSignal>,
) -> Result<std::thread::JoinHandle<()>, String> {
    let target = args.target;
    let session = args.session.clone();

    let watch_client =
        Arc::new(Client::connect(&session).map_err(|err| format!("watcher connect: {err}"))?);
    watch_client
        .initialize()
        .map_err(|err| format!("watcher init: {err}"))?;
    let (event_tx, event_rx) = mpsc::channel::<Event>();
    watch_client
        .watch_events(event_tx)
        .map_err(|err| format!("watch_events: {err}"))?;

    Ok(std::thread::spawn(move || {
        loop {
            match event_rx.recv() {
                Ok(Event::PaneClosed { pane_id }) if pane_id == target => {
                    let _ = signal_tx.send(WatcherSignal::TargetClosed);
                    return;
                }
                Ok(event) => {
                    if let Some(kind) = classify_event(target, &event) {
                        let scrollback_tail = match snapshot_tail(&session, target) {
                            Ok(s) => s,
                            Err(_) => "(scrollback unavailable)".to_string(),
                        };
                        let trig = Trigger {
                            kind,
                            scrollback_tail,
                        };
                        if signal_tx.send(WatcherSignal::Trigger(trig)).is_err() {
                            return;
                        }
                    }
                }
                Err(_) => {
                    let _ = signal_tx.send(WatcherSignal::Disconnected);
                    return;
                }
            }
        }
    }))
}

pub fn snapshot_tail(session: &str, target: u32) -> Result<String, String> {
    let snap = Client::connect(session).map_err(|e| e.to_string())?;
    snap.initialize().map_err(|e| e.to_string())?;
    let result = snap
        .call_tool(
            "read_pane",
            serde_json::json!({
                "pane_id": target,
                "lines": 40,
                "mode": "scrollback",
                "strip_ansi": true
            }),
        )
        .map_err(|e| e.to_string())?;
    Ok(result["text"].as_str().unwrap_or("").to_string())
}

use crate::pair::conversation::{ConversationState, ToolCall, requires_confirmation};
use crate::pair::model::{ModelOutput, system_prompt, tool_schemas};

/// Confirm answers travel on a separate channel so a `Trigger`
/// arriving while we wait for a y/N response just queues on the
/// inbox instead of being treated as the answer.
pub enum MediatorInput {
    UserLine(String),
    Trigger(crate::pair::conversation::Trigger),
    Quit,
    Shutdown(String),
}

pub enum MediatorOutput {
    Print(String),
    ConfirmRequest { summary: String },
    Done,
}

fn inject_pane_id(target: u32, args_json: &str) -> serde_json::Value {
    let mut v: serde_json::Value =
        serde_json::from_str(args_json).unwrap_or_else(|_| serde_json::json!({}));
    if !v.is_object() {
        v = serde_json::json!({});
    }
    v["pane_id"] = serde_json::json!(target);
    v
}

fn dispatch_tool(session: &str, target: u32, call: &ToolCall) -> String {
    let args = inject_pane_id(target, &call.function.arguments);
    let snap = match Client::connect(session).and_then(|c| c.initialize().map(|_| c)) {
        Ok(c) => c,
        Err(err) => return format!("{{\"error\":\"connect: {err}\"}}"),
    };
    match snap.call_tool(&call.function.name, args) {
        Ok(v) => v.to_string(),
        Err(err) => format!("{{\"error\":\"{err}\"}}"),
    }
}

pub fn run_mediator(
    args: &PairArgs,
    inbox: mpsc::Receiver<MediatorInput>,
    confirm_rx: mpsc::Receiver<bool>,
    outbox: mpsc::Sender<MediatorOutput>,
) {
    let mut convo = ConversationState::new();
    let ollama = OllamaClient::new();
    let system = system_prompt(args.target);
    let tools = tool_schemas();

    loop {
        let input = match inbox.recv() {
            Ok(v) => v,
            Err(_) => return,
        };

        match input {
            MediatorInput::Quit => {
                let _ = outbox.send(MediatorOutput::Done);
                return;
            }
            MediatorInput::Shutdown(reason) => {
                let _ = outbox.send(MediatorOutput::Print(reason));
                let _ = outbox.send(MediatorOutput::Done);
                return;
            }
            MediatorInput::UserLine(line) => {
                if line.trim().is_empty() {
                    continue;
                }
                convo.push_user(line);
                drive_model_loop(
                    &ollama,
                    &args.session,
                    args.target,
                    &args.model,
                    &system,
                    &tools,
                    &mut convo,
                    &outbox,
                    TurnKind::Chat,
                    &confirm_rx,
                );
            }
            MediatorInput::Trigger(trig) => {
                convo.push_trigger(args.target, &trig);
                drive_model_loop(
                    &ollama,
                    &args.session,
                    args.target,
                    &args.model,
                    &system,
                    &tools,
                    &mut convo,
                    &outbox,
                    TurnKind::Watcher,
                    &confirm_rx,
                );
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum TurnKind {
    Chat,
    Watcher,
}

#[allow(clippy::too_many_arguments)]
fn drive_model_loop(
    ollama: &OllamaClient,
    session: &str,
    target: u32,
    model: &str,
    system: &str,
    tools: &[serde_json::Value],
    convo: &mut ConversationState,
    outbox: &mpsc::Sender<MediatorOutput>,
    turn_kind: TurnKind,
    confirm_rx: &mpsc::Receiver<bool>,
) {
    let turn_start = convo.history.len();
    loop {
        let out = match ollama.complete(model, system, &convo.history, tools) {
            Ok(v) => v,
            Err(err) => {
                let _ = outbox.send(MediatorOutput::Print(format!("[error] {err}")));
                // Rollback so retries don't start mid-turn.
                convo.history.truncate(turn_start);
                return;
            }
        };

        match out {
            ModelOutput::Text { content, finish } => {
                let prefix = match turn_kind {
                    TurnKind::Chat => "[claude]",
                    TurnKind::Watcher => "[watcher]",
                };
                let suffix = match finish {
                    crate::pair::model::FinishReason::Length => " [truncated]",
                    _ => "",
                };
                let _ = outbox.send(MediatorOutput::Print(format!("{prefix} {content}{suffix}")));
                convo.push_assistant_text(content);
                return;
            }
            ModelOutput::ToolCalls(calls) => {
                if let Err(err) = convo.bump_tool_round() {
                    let _ = outbox.send(MediatorOutput::Print(format!("[error] {err}")));
                    return;
                }
                convo.push_assistant_tool_calls(calls.clone());
                for call in calls {
                    if requires_confirmation(&call) {
                        let summary = format!(
                            "Co-pilot wants: {}({}). Approve?",
                            call.function.name, call.function.arguments
                        );
                        let _ = outbox.send(MediatorOutput::ConfirmRequest { summary });
                        convo.pending_confirm = Some(call.clone());

                        let approved = match confirm_rx.recv() {
                            Ok(b) => b,
                            Err(_) => {
                                let _ = outbox.send(MediatorOutput::Done);
                                return;
                            }
                        };
                        let resolved = convo.resolve_pending_confirm(approved);
                        match resolved {
                            Some(approved_call) => {
                                let result = dispatch_tool(session, target, &approved_call);
                                convo.push_tool_result(approved_call.id, result);
                            }
                            None => {
                                // Decline: tool_result was appended by resolve_pending_confirm.
                            }
                        }
                    } else {
                        let result = dispatch_tool(session, target, &call);
                        convo.push_tool_result(call.id, result);
                    }
                }
                continue;
            }
        }
    }
}

fn run_repl(
    args: PairArgs,
    mediator_tx: mpsc::Sender<MediatorInput>,
    confirm_tx: mpsc::Sender<bool>,
    output_rx: mpsc::Receiver<MediatorOutput>,
) -> Result<(), String> {
    use rustyline::error::ReadlineError;
    use rustyline::{DefaultEditor, ExternalPrinter};

    let mut rl = DefaultEditor::new().map_err(|e| format!("rustyline init: {e}"))?;

    // ExternalPrinter lets the output thread write above the prompt
    // without trampling rustyline's redraw.
    let printer = rl
        .create_external_printer()
        .map_err(|e| format!("external printer: {e}"))?;

    let (confirm_prompt_tx, confirm_prompt_rx) = mpsc::channel::<String>();
    {
        let mut printer = printer;
        std::thread::spawn(move || {
            for out in output_rx.iter() {
                match out {
                    MediatorOutput::Print(line) => {
                        let _ = printer.print(format!("{line}\n"));
                    }
                    MediatorOutput::ConfirmRequest { summary } => {
                        let _ =
                            printer.print(format!("[confirm] {summary} (press Enter to answer)\n"));
                        let _ = confirm_prompt_tx.send(summary);
                    }
                    MediatorOutput::Done => return,
                }
            }
        });
    }

    println!(
        "zmux pair: target=pane {}, model={}, session={}. Ctrl-C to exit.",
        args.target, args.model, args.session
    );

    loop {
        if let Ok(summary) = confirm_prompt_rx.try_recv() {
            let prompt = format!("[confirm] {summary} [y/N] ");
            let answer = match rl.readline(&prompt) {
                Ok(s) => matches!(s.trim().to_lowercase().as_str(), "y" | "yes"),
                Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => {
                    let _ = mediator_tx.send(MediatorInput::Quit);
                    return Ok(());
                }
                Err(err) => return Err(format!("readline: {err}")),
            };
            let _ = confirm_tx.send(answer);
            continue;
        }

        let line = match rl.readline("> ") {
            Ok(s) => s,
            Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => {
                let _ = mediator_tx.send(MediatorInput::Quit);
                return Ok(());
            }
            Err(err) => return Err(format!("readline: {err}")),
        };
        let _ = rl.add_history_entry(line.as_str());
        let _ = mediator_tx.send(MediatorInput::UserLine(line));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(extra: &[&str]) -> Vec<String> {
        let mut v: Vec<String> = vec!["zmux".into(), "pair".into()];
        v.extend(extra.iter().map(|s| s.to_string()));
        v
    }

    #[test]
    fn parse_args_requires_target() {
        let err = parse_args(&argv(&[]), "default").unwrap_err();
        assert!(err.contains("--target"), "got: {err}");
    }

    #[test]
    fn parse_args_target_must_be_integer() {
        let err = parse_args(&argv(&["--target", "abc"]), "default").unwrap_err();
        assert!(err.contains("integer"), "got: {err}");
    }

    #[test]
    fn parse_args_minimal_uses_defaults() {
        let parsed = parse_args(&argv(&["--target", "3"]), "demo").unwrap();
        assert_eq!(
            parsed,
            PairArgs {
                session: "demo".to_string(),
                target: 3,
                model: DEFAULT_MODEL.to_string(),
            }
        );
    }

    #[test]
    fn parse_args_full_flags() {
        let parsed = parse_args(
            &argv(&[
                "--session",
                "s1",
                "--target",
                "7",
                "--model",
                "qwen2.5-coder",
            ]),
            "default",
        )
        .unwrap();
        assert_eq!(parsed.session, "s1");
        assert_eq!(parsed.target, 7);
        assert_eq!(parsed.model, "qwen2.5-coder");
    }

    #[test]
    fn parse_args_rejects_unknown_flag() {
        let err = parse_args(&argv(&["--target", "1", "--bogus"]), "default").unwrap_err();
        assert!(err.contains("unknown argument"), "got: {err}");
    }

    use crate::events::Event;

    #[test]
    fn classify_ignores_panes_other_than_target() {
        let e = Event::PaneStateChanged {
            pane_id: 5,
            from: "Working".into(),
            to: "Errored".into(),
        };
        assert!(super::classify_event(2, &e).is_none());
    }

    #[test]
    fn classify_fires_on_target_errored() {
        let e = Event::PaneStateChanged {
            pane_id: 2,
            from: "Working".into(),
            to: "Errored".into(),
        };
        assert!(matches!(
            super::classify_event(2, &e),
            Some(super::TriggerKind::StateChanged { .. })
        ));
    }

    #[test]
    fn classify_fires_on_target_awaiting_input() {
        let e = Event::PaneStateChanged {
            pane_id: 2,
            from: "Working".into(),
            to: "AwaitingInput".into(),
        };
        assert!(matches!(
            super::classify_event(2, &e),
            Some(super::TriggerKind::StateChanged { .. })
        ));
    }

    #[test]
    fn classify_skips_zero_exit() {
        let e = Event::PaneExited {
            pane_id: 2,
            exit_code: 0,
        };
        assert!(super::classify_event(2, &e).is_none());
    }

    #[test]
    fn classify_fires_on_nonzero_exit() {
        let e = Event::PaneExited {
            pane_id: 2,
            exit_code: 1,
        };
        assert!(matches!(
            super::classify_event(2, &e),
            Some(super::TriggerKind::Exited { exit_code: 1 })
        ));
    }

    #[test]
    fn classify_skips_state_to_idle() {
        let e = Event::PaneStateChanged {
            pane_id: 2,
            from: "Working".into(),
            to: "Idle".into(),
        };
        assert!(super::classify_event(2, &e).is_none());
    }

    #[test]
    fn inject_pane_id_overrides_existing_pane_id() {
        let v = super::inject_pane_id(7, r#"{"pane_id":99,"lines":40}"#);
        assert_eq!(v["pane_id"], 7);
        assert_eq!(v["lines"], 40);
    }

    #[test]
    fn inject_pane_id_into_empty_object() {
        let v = super::inject_pane_id(3, "{}");
        assert_eq!(v["pane_id"], 3);
    }

    #[test]
    fn inject_pane_id_recovers_from_invalid_json() {
        let v = super::inject_pane_id(1, "not json");
        assert_eq!(v["pane_id"], 1);
    }
}
