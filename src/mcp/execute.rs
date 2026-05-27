// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Workspace-mutation layer for MCP tool calls.
//!
//! `execute_*` helpers run on the daemon's render thread holding
//! `WindowSet` exclusively, so they must stay synchronous. Handlers
//! that need to wait for a condition (e.g. `spawn_pane` with
//! `wait_for_idle`) return [`Outcome::Defer`] with a closure;
//! [`tick_pending`] resolves the deferral on a future loop iteration
//! so the render loop keeps ingesting PTY output in the meantime.

use std::sync::mpsc::{Receiver, Sender};
use std::time::Instant;

use serde_json::{Map, Value, json};

use super::dispatch::{McpCall, ReadMode, SpawnSplit};
use crate::agent::AgentState;
use crate::events::Event;
use crate::layout::SplitOrientation;
use crate::windows::WindowSet;

pub enum McpRequest {
    ToolCall {
        call: McpCall,
        reply: Sender<McpResponse>,
    },
    Subscribe {
        reply: Sender<Receiver<Event>>,
    },
}

#[derive(Debug)]
pub enum McpResponse {
    Ok(Value),
    Err(String),
}

pub enum Outcome {
    /// Reply now. `bool` is the dirty flag.
    Done(McpResponse, bool),
    /// Park the request and reply later via the build closure. The
    /// trailing `bool` is the *immediate* dirty flag for any state
    /// mutation the synchronous portion has already done; the closure
    /// returns its own dirty flag for completion-time mutations.
    Defer(Pending, bool),
}

pub struct Pending {
    pub reply: Sender<McpResponse>,
    pub deadline: Instant,
    pub condition: WaitCondition,
    pub state: PendingState,
    #[allow(clippy::type_complexity)]
    pub build: Box<dyn FnOnce(&mut WindowSet, WaitOutcome) -> (McpResponse, bool) + Send + 'static>,
}

#[derive(Default)]
pub struct PendingState {
    /// True once we've observed the pane in `AgentState::Working`.
    /// `PaneSettled` requires this so a freshly spawned pane (which
    /// starts in Idle before producing output) doesn't settle
    /// immediately.
    pub has_been_working: bool,
    /// First moment we observed Idle/AwaitingInput after Working.
    /// Resets on any subsequent Working transition so multi-phase
    /// TUIs (e.g. an agent that emits, pauses for an HTTP roundtrip,
    /// then emits again) don't settle on the inter-phase quiet
    /// window.
    pub idle_since: Option<Instant>,
    pub deferred_enter_sent: bool,
}

/// Minimum continuous-Idle duration before `PaneSettled` completes.
/// Stacks with the workspace's idle threshold so we don't snapshot
/// mid-redraw on slow multi-phase TUIs.
const PANE_SETTLE_MIN_MS: u64 = 250;

pub enum WaitCondition {
    /// Settles once the pane has been Working at some point since
    /// deferral and is now Idle/AwaitingInput. Also completes
    /// immediately on error/exit or if the pane disappears.
    PaneSettled { pane_id: u32 },
    /// Like `PaneSettled`, but for already-running panes after input
    /// injection. `deferred_enter_at` lets plain-mode inputs preserve
    /// the delayed-CR behavior without a second pending request.
    PaneSettledAfterOutput {
        pane_id: u32,
        since: Instant,
        deferred_enter_at: Option<Instant>,
        expect_text: Option<String>,
        wait_lines: usize,
    },
    /// Just wait for the deadline. Used to deliver CR after typed
    /// text so the receiving app's input loop doesn't coalesce them.
    Elapsed,
}

#[derive(Debug, Clone, Copy)]
pub enum WaitOutcome {
    Met,
    Timeout,
}

/// Drain queued MCP requests against `windows`. Called once per
/// daemon poll cycle. Non-blocking — returns immediately if no
/// requests are pending. Returns `true` if any tool mutated workspace
/// state and the caller should mark the next frame dirty. Deferred
/// requests are pushed into `pending` and not counted as dirty here;
/// `tick_pending` reports their completion-time mutations separately.
pub fn drain_requests(
    rx: &Receiver<McpRequest>,
    windows: &mut WindowSet,
    pending: &mut Vec<Pending>,
) -> bool {
    let mut dirty = false;
    while let Ok(request) = rx.try_recv() {
        match request {
            McpRequest::ToolCall { call, reply } => match execute_call(call, windows) {
                Outcome::Done(response, mutated) => {
                    let _ = reply.send(response);
                    dirty |= mutated;
                }
                Outcome::Defer(mut p, immediate_dirty) => {
                    p.reply = reply;
                    pending.push(p);
                    dirty |= immediate_dirty;
                }
            },
            McpRequest::Subscribe { reply } => {
                let sub = windows.subscribe_events();
                let _ = reply.send(sub);
            }
        }
    }
    dirty
}

/// Called once per daemon poll cycle, after PTY ingest and
/// `tick_agents`, so `PaneSettled` waiters see the freshest
/// `agent_state` the loop has drained.
pub fn tick_pending(windows: &mut WindowSet, pending: &mut Vec<Pending>) -> bool {
    if pending.is_empty() {
        return false;
    }
    let now = Instant::now();
    let mut dirty = false;
    let mut index = 0;
    while index < pending.len() {
        let entry = &mut pending[index];
        let met = condition_met(&entry.condition, &mut entry.state, windows);
        let timed_out = !met && now >= entry.deadline;
        if met || timed_out {
            let entry = pending.swap_remove(index);
            let outcome = if met {
                WaitOutcome::Met
            } else {
                WaitOutcome::Timeout
            };
            let (response, mutated) = (entry.build)(windows, outcome);
            let _ = entry.reply.send(response);
            dirty |= mutated;
        } else {
            index += 1;
        }
    }
    dirty
}

fn condition_met(cond: &WaitCondition, state: &mut PendingState, windows: &mut WindowSet) -> bool {
    match cond {
        WaitCondition::PaneSettled { pane_id } => {
            let pane_state = windows
                .find_pane_mut(*pane_id as usize)
                .map(|p| p.agent_state.clone());
            let pane_state = match pane_state {
                Some(s) => s,
                None => return true,
            };
            match pane_state {
                AgentState::Working => {
                    state.has_been_working = true;
                    state.idle_since = None;
                    false
                }
                AgentState::Idle | AgentState::AwaitingInput => {
                    if !state.has_been_working {
                        return false;
                    }
                    let now = Instant::now();
                    match state.idle_since {
                        None => {
                            state.idle_since = Some(now);
                            false
                        }
                        Some(started) => {
                            now.saturating_duration_since(started)
                                >= std::time::Duration::from_millis(PANE_SETTLE_MIN_MS)
                        }
                    }
                }
                AgentState::Errored | AgentState::Exited(_) => true,
            }
        }
        WaitCondition::PaneSettledAfterOutput {
            pane_id,
            since,
            deferred_enter_at,
            expect_text,
            wait_lines,
        } => {
            if let Some(send_at) = deferred_enter_at {
                if !state.deferred_enter_sent && Instant::now() >= *send_at {
                    let _ = windows.send_pty_input(*pane_id, b"\r");
                    state.deferred_enter_sent = true;
                }
                if !state.deferred_enter_sent {
                    return false;
                }
            }
            let pane = windows.find_pane_mut(*pane_id as usize);
            let (pane_state, output_after_start) = match pane {
                Some(pane) => (
                    pane.agent_state.clone(),
                    pane.last_output_at.saturating_duration_since(*since)
                        > std::time::Duration::ZERO,
                ),
                None => return true,
            };
            if matches!(pane_state, AgentState::Errored | AgentState::Exited(_)) {
                return true;
            }
            let expect_seen = expect_text.as_ref().is_none_or(|needle| {
                pane_wait_text(windows, *pane_id, *wait_lines).contains(needle)
            });
            if !expect_seen {
                return false;
            }
            match pane_state {
                AgentState::Working => {
                    state.has_been_working = true;
                    state.idle_since = None;
                    false
                }
                AgentState::Idle | AgentState::AwaitingInput => {
                    if !state.has_been_working && !output_after_start {
                        return false;
                    }
                    let now = Instant::now();
                    match state.idle_since {
                        None => {
                            state.idle_since = Some(now);
                            false
                        }
                        Some(started) => {
                            now.saturating_duration_since(started)
                                >= std::time::Duration::from_millis(PANE_SETTLE_MIN_MS)
                        }
                    }
                }
                AgentState::Errored | AgentState::Exited(_) => true,
            }
        }
        WaitCondition::Elapsed => false,
    }
}

fn execute_call(call: McpCall, windows: &mut WindowSet) -> Outcome {
    match call {
        McpCall::ListPanes => Outcome::Done(McpResponse::Ok(json_list_panes(windows)), false),
        McpCall::SpawnPane {
            command,
            label,
            split,
            target_pane,
            wait_for_idle,
            max_wait_ms,
        } => execute_spawn_pane(
            windows,
            &command,
            label,
            split,
            target_pane,
            wait_for_idle,
            max_wait_ms,
        ),
        McpCall::SendKeys {
            pane_id,
            keys,
            enter,
            clear_input,
            wait_for_idle,
            max_wait_ms,
            expect_text,
            wait_lines,
        } => execute_send_keys(
            windows,
            pane_id,
            &keys,
            enter,
            clear_input,
            wait_for_idle,
            max_wait_ms,
            expect_text,
            wait_lines as usize,
        ),
        McpCall::WaitPane {
            pane_id,
            max_wait_ms,
            expect_text,
            wait_lines,
        } => execute_wait_pane(
            windows,
            pane_id,
            max_wait_ms,
            expect_text,
            wait_lines as usize,
        ),
        McpCall::ReadPane {
            pane_id,
            lines,
            strip_ansi,
            mode,
        } => execute_read_pane(windows, pane_id, lines, strip_ansi, mode),
        McpCall::ReadPaneOutput {
            pane_id,
            since_byte,
            max_bytes,
            strip_ansi,
        } => execute_read_pane_output(windows, pane_id, since_byte, max_bytes, strip_ansi),
        McpCall::KillPane { pane_id } => execute_kill_pane(windows, pane_id),
        McpCall::SetLabel { pane_id, label } => execute_set_label(windows, pane_id, label),
    }
}

fn execute_kill_pane(windows: &mut WindowSet, pane_id: u32) -> Outcome {
    match windows.kill_pane_by_id(pane_id) {
        Ok(true) => Outcome::Done(McpResponse::Ok(json!({"ok": true})), true),
        Ok(false) => Outcome::Done(
            McpResponse::Err(format!(
                "kill_pane: refused to close pane {pane_id} (last remaining pane in the only window)"
            )),
            false,
        ),
        Err(err) => Outcome::Done(McpResponse::Err(format!("kill_pane failed: {err}")), false),
    }
}

fn execute_set_label(windows: &mut WindowSet, pane_id: u32, label: String) -> Outcome {
    let label_arg = if label.is_empty() { None } else { Some(label) };
    // Existence check up front so a missing pane returns a structured
    // error instead of looking like an idempotent set
    // (set_pane_label returns false for both cases).
    if windows.find_pane_mut(pane_id as usize).is_none() {
        return Outcome::Done(
            McpResponse::Err(format!(
                "set_label: no pane with id {pane_id} in any window"
            )),
            false,
        );
    }
    let changed = windows.set_pane_label(pane_id, label_arg);
    Outcome::Done(McpResponse::Ok(json!({"ok": true})), changed)
}

fn execute_read_pane(
    windows: &mut WindowSet,
    pane_id: u32,
    lines: u32,
    strip_ansi: bool,
    mode: ReadMode,
) -> Outcome {
    // Non-mutating read: flushing the grid into scrollback would
    // break TUIs that revisit primary-grid rows with CUU/CHA.
    let lines_vec_opt = match mode {
        ReadMode::Visible => windows.snapshot_visible_lines(pane_id),
        ReadMode::Scrollback => windows.snapshot_scrollback_lines(pane_id, lines as usize),
    };
    let lines_vec = match lines_vec_opt {
        Some(v) => v,
        None => {
            return Outcome::Done(
                McpResponse::Err(format!(
                    "read_pane: no pane with id {pane_id} in any window"
                )),
                false,
            );
        }
    };
    let cursor_at_bottom = windows
        .find_pane_mut(pane_id as usize)
        .map(|p| p.viewport_following_live())
        .unwrap_or(true);
    // Snapshot helpers already strip wide-char continuation sentinels;
    // honor the strip_ansi knob for forward-compat with a future raw-byte
    // snapshot mode. Today's snapshot is escape-free so this is a no-op
    // when strip_ansi=true and a passthrough when false. Matches the
    // `Pane::scrollback_text` contract.
    let lines_vec = if strip_ansi {
        lines_vec
            .into_iter()
            .map(|line| crate::pane::strip_ansi_inplace(&line))
            .collect()
    } else {
        lines_vec
    };
    let text = lines_vec.join("\n");
    Outcome::Done(
        McpResponse::Ok(json!({
            "text": text,
            "cursor_at_bottom": cursor_at_bottom,
        })),
        false,
    )
}

fn execute_read_pane_output(
    windows: &mut WindowSet,
    pane_id: u32,
    since_byte: u64,
    max_bytes: usize,
    strip_ansi: bool,
) -> Outcome {
    let slice = match windows.pane_output_since(pane_id, since_byte, max_bytes) {
        Some(slice) => slice,
        None => {
            return Outcome::Done(
                McpResponse::Err(format!(
                    "read_pane_output: no pane with id {pane_id} in any window"
                )),
                false,
            );
        }
    };
    let mut text = String::from_utf8_lossy(&slice.bytes).into_owned();
    if strip_ansi {
        text = crate::pane::strip_ansi_inplace(&text);
    }
    Outcome::Done(
        McpResponse::Ok(json!({
            "pane_id": pane_id,
            "start_byte": slice.start_byte,
            "byte_cursor": slice.byte_cursor,
            "text": text,
            "truncated": slice.truncated,
        })),
        false,
    )
}

fn pane_wait_text(windows: &WindowSet, pane_id: u32, lines: usize) -> String {
    let snapshot = windows
        .snapshot_scrollback_lines(pane_id, lines)
        .or_else(|| windows.snapshot_visible_lines(pane_id))
        .unwrap_or_default();
    let stripped: Vec<String> = snapshot
        .into_iter()
        .map(|line| crate::pane::strip_ansi_inplace(&line))
        .collect();
    stripped.join("\n")
}

fn pane_settled_response(
    windows: &mut WindowSet,
    pane_id: u32,
    outcome: WaitOutcome,
    include_ok: bool,
    expect_text: Option<&str>,
    wait_lines: usize,
) -> McpResponse {
    let text = pane_wait_text(windows, pane_id, wait_lines);
    let matched_expect = expect_text.is_none_or(|needle| text.contains(needle));
    let state = windows
        .find_pane_mut(pane_id as usize)
        .map(|p| p.agent_state.as_wire())
        .unwrap_or_else(|| "Unknown".into());
    let mut payload = Map::new();
    if include_ok {
        payload.insert("ok".into(), Value::Bool(true));
    }
    payload.insert("pane_id".into(), json!(pane_id));
    payload.insert("text".into(), json!(text));
    payload.insert("state".into(), json!(state));
    payload.insert("matched_expect".into(), json!(matched_expect));
    payload.insert(
        "timed_out".into(),
        json!(matches!(outcome, WaitOutcome::Timeout)),
    );
    McpResponse::Ok(Value::Object(payload))
}

fn pane_settled_pending(
    pane_id: u32,
    max_wait_ms: u32,
    condition: WaitCondition,
    include_ok: bool,
    expect_text: Option<String>,
    wait_lines: usize,
) -> Pending {
    Pending {
        reply: dummy_reply_sender(),
        deadline: Instant::now() + std::time::Duration::from_millis(max_wait_ms as u64),
        condition,
        state: PendingState::default(),
        build: Box::new(move |windows, outcome| {
            (
                pane_settled_response(
                    windows,
                    pane_id,
                    outcome,
                    include_ok,
                    expect_text.as_deref(),
                    wait_lines,
                ),
                false,
            )
        }),
    }
}

fn execute_wait_pane(
    windows: &mut WindowSet,
    pane_id: u32,
    max_wait_ms: u32,
    expect_text: Option<String>,
    wait_lines: usize,
) -> Outcome {
    let pane_state = match windows.find_pane_mut(pane_id as usize) {
        Some(pane) => pane.agent_state.clone(),
        None => {
            return Outcome::Done(
                McpResponse::Err(format!(
                    "wait_pane: no pane with id {pane_id} in any window"
                )),
                false,
            );
        }
    };
    let matched_now = expect_text
        .as_ref()
        .is_none_or(|needle| pane_wait_text(windows, pane_id, wait_lines).contains(needle));
    if matched_now
        && matches!(
            pane_state,
            AgentState::Idle
                | AgentState::AwaitingInput
                | AgentState::Errored
                | AgentState::Exited(_)
        )
    {
        return Outcome::Done(
            pane_settled_response(
                windows,
                pane_id,
                WaitOutcome::Met,
                true,
                expect_text.as_deref(),
                wait_lines,
            ),
            false,
        );
    }

    let wait_start = Instant::now();
    Outcome::Defer(
        pane_settled_pending(
            pane_id,
            max_wait_ms,
            WaitCondition::PaneSettledAfterOutput {
                pane_id,
                since: wait_start,
                deferred_enter_at: None,
                expect_text: expect_text.clone(),
                wait_lines,
            },
            true,
            expect_text,
            wait_lines,
        ),
        false,
    )
}

/// Two paths depending on the destination shell's input mode:
///
/// 1. Bracketed-paste (DECSET 2004 active): wrap `keys` in
///    `\x1b[200~ ... \x1b[201~` and append CR in a single PTY write.
///    The close marker unambiguously ends paste mode so the trailing
///    CR reads as a fresh keystroke.
/// 2. Plain mode: write `keys`, then defer CR by 75ms. Many React/Ink
///    TUIs treat a single write ending in CR as paste content, so the
///    CR becomes a literal newline inside the input box instead of a
///    submit. The gap forces the app to drain the text as keystrokes
///    first.
///
/// CR not LF in both cases: real terminals send `\r` for Enter, and
/// raw-mode TUIs disable the kernel's icrnl translation.
#[allow(clippy::too_many_arguments)]
fn execute_send_keys(
    windows: &mut WindowSet,
    pane_id: u32,
    keys: &str,
    enter: bool,
    clear_input: bool,
    wait_for_idle: bool,
    max_wait_ms: u32,
    expect_text: Option<String>,
    wait_lines: usize,
) -> Outcome {
    let wait_start = Instant::now();
    if windows.pane_bracketed_paste(pane_id).unwrap_or(false) {
        return execute_send_keys_bracketed(
            windows,
            pane_id,
            keys,
            enter,
            clear_input,
            wait_for_idle,
            max_wait_ms,
            wait_start,
            expect_text,
            wait_lines,
        );
    }
    if clear_input && let Err(err) = windows.send_pty_input(pane_id, b"\x15") {
        return Outcome::Done(McpResponse::Err(format!("send_keys failed: {err}")), false);
    }
    if !keys.is_empty()
        && let Err(err) = windows.send_pty_input(pane_id, keys.as_bytes())
    {
        return Outcome::Done(McpResponse::Err(format!("send_keys failed: {err}")), false);
    }
    if !enter {
        if wait_for_idle {
            return Outcome::Defer(
                pane_settled_pending(
                    pane_id,
                    max_wait_ms,
                    WaitCondition::PaneSettledAfterOutput {
                        pane_id,
                        since: wait_start,
                        deferred_enter_at: None,
                        expect_text: expect_text.clone(),
                        wait_lines,
                    },
                    true,
                    expect_text,
                    wait_lines,
                ),
                true,
            );
        }
        return Outcome::Done(McpResponse::Ok(json!({"ok": true})), true);
    }
    // No typed text → CR right now; the paste-vs-type hazard only
    // exists when text precedes the CR.
    if keys.is_empty() {
        return match windows.send_pty_input(pane_id, b"\r") {
            Ok(()) if wait_for_idle => Outcome::Defer(
                pane_settled_pending(
                    pane_id,
                    max_wait_ms,
                    WaitCondition::PaneSettledAfterOutput {
                        pane_id,
                        since: wait_start,
                        deferred_enter_at: None,
                        expect_text: expect_text.clone(),
                        wait_lines,
                    },
                    true,
                    expect_text,
                    wait_lines,
                ),
                true,
            ),
            Ok(()) => Outcome::Done(McpResponse::Ok(json!({"ok": true})), true),
            Err(err) => Outcome::Done(McpResponse::Err(format!("send_keys failed: {err}")), false),
        };
    }
    // Defer CR so the receiving app sees the typed text as
    // keystrokes before the submit, not as paste content.
    let deadline = Instant::now() + std::time::Duration::from_millis(75);
    if wait_for_idle {
        return Outcome::Defer(
            pane_settled_pending(
                pane_id,
                max_wait_ms,
                WaitCondition::PaneSettledAfterOutput {
                    pane_id,
                    since: wait_start,
                    deferred_enter_at: Some(deadline),
                    expect_text: expect_text.clone(),
                    wait_lines,
                },
                true,
                expect_text,
                wait_lines,
            ),
            true,
        );
    }
    Outcome::Defer(
        Pending {
            reply: dummy_reply_sender(),
            deadline,
            condition: WaitCondition::Elapsed,
            state: PendingState::default(),
            build: Box::new(move |windows, _outcome| {
                match windows.send_pty_input(pane_id, b"\r") {
                    Ok(()) => (McpResponse::Ok(json!({"ok": true})), true),
                    Err(err) => (McpResponse::Err(format!("send_keys failed: {err}")), false),
                }
            }),
        },
        true,
    )
}

#[allow(clippy::too_many_arguments)]
fn execute_send_keys_bracketed(
    windows: &mut WindowSet,
    pane_id: u32,
    keys: &str,
    enter: bool,
    clear_input: bool,
    wait_for_idle: bool,
    max_wait_ms: u32,
    wait_start: Instant,
    expect_text: Option<String>,
    wait_lines: usize,
) -> Outcome {
    let mut payload: Vec<u8> = Vec::with_capacity(keys.len() + 14);
    if clear_input {
        payload.push(0x15);
    }
    if !keys.is_empty() {
        payload.extend_from_slice(b"\x1b[200~");
        payload.extend_from_slice(keys.as_bytes());
        payload.extend_from_slice(b"\x1b[201~");
    }
    if enter {
        payload.push(b'\r');
    }
    if payload.is_empty() {
        return Outcome::Done(McpResponse::Ok(json!({"ok": true})), true);
    }
    match windows.send_pty_input(pane_id, &payload) {
        Ok(()) if wait_for_idle => Outcome::Defer(
            pane_settled_pending(
                pane_id,
                max_wait_ms,
                WaitCondition::PaneSettledAfterOutput {
                    pane_id,
                    since: wait_start,
                    deferred_enter_at: None,
                    expect_text: expect_text.clone(),
                    wait_lines,
                },
                true,
                expect_text,
                wait_lines,
            ),
            true,
        ),
        Ok(()) => Outcome::Done(McpResponse::Ok(json!({"ok": true})), true),
        Err(err) => Outcome::Done(McpResponse::Err(format!("send_keys failed: {err}")), false),
    }
}

fn execute_spawn_pane(
    windows: &mut WindowSet,
    command: &str,
    label: Option<String>,
    split: SpawnSplit,
    target_pane: Option<u32>,
    wait_for_idle: bool,
    max_wait_ms: u32,
) -> Outcome {
    let result = match split {
        SpawnSplit::Horizontal => windows.active_mut().spawn_pane_with_command(
            command,
            SplitOrientation::Columns,
            target_pane,
        ),
        SpawnSplit::Vertical => windows.active_mut().spawn_pane_with_command(
            command,
            SplitOrientation::Rows,
            target_pane,
        ),
        SpawnSplit::NewWindow => {
            // `target_pane` is meaningless for a fresh window;
            // surface it as a tool-level error so the caller doesn't
            // silently miss a misuse.
            if target_pane.is_some() {
                return Outcome::Done(
                    McpResponse::Err(
                        "spawn_pane: `target_pane` is not valid with split=\"window\"".into(),
                    ),
                    false,
                );
            }
            windows.new_window_with_command(command)
        }
    };
    let pane_id = match result {
        Ok(id) => id,
        Err(err) => {
            return Outcome::Done(McpResponse::Err(format!("spawn_pane failed: {err}")), false);
        }
    };
    if let Some(label) = label {
        // Cross-window setter; for split=window the active window
        // changed underfoot.
        windows.set_pane_label(pane_id, Some(label));
    }
    if !wait_for_idle {
        return Outcome::Done(McpResponse::Ok(json!({"pane_id": pane_id})), true);
    }
    let deadline = Instant::now() + std::time::Duration::from_millis(max_wait_ms as u64);
    Outcome::Defer(
        Pending {
            reply: dummy_reply_sender(),
            deadline,
            condition: WaitCondition::PaneSettled { pane_id },
            state: PendingState::default(),
            build: Box::new(move |windows, outcome| {
                let lines = windows.snapshot_visible_lines(pane_id);
                let text = lines.map(|v| {
                    let stripped: Vec<String> = v
                        .into_iter()
                        .map(|line| crate::pane::strip_ansi_inplace(&line))
                        .collect();
                    stripped.join("\n")
                });
                let state = windows
                    .find_pane_mut(pane_id as usize)
                    .map(|p| p.agent_state.as_wire())
                    .unwrap_or_else(|| "Unknown".into());
                (
                    McpResponse::Ok(json!({
                        "pane_id": pane_id,
                        "text": text.unwrap_or_default(),
                        "state": state,
                        "timed_out": matches!(outcome, WaitOutcome::Timeout),
                    })),
                    false,
                )
            }),
        },
        true,
    )
}

/// MCP 2025-06-18 requires `structuredContent` to be an object, so
/// the panes array is wrapped under `panes`.
pub(super) fn json_list_panes(windows: &mut WindowSet) -> Value {
    let summaries = windows.pane_summaries_all();
    let rows: Vec<Value> = summaries
        .into_iter()
        .map(|s| {
            json!({
                "pane_id": s.pane.pane_id,
                "window_index": s.window_index,
                "active_window": s.active_window,
                "label": s.pane.label,
                "state": s.pane.state.as_wire(),
                "last_command": s.pane.last_command,
                "last_exit": s.pane.last_exit,
                "size_cols": s.pane.size_cols,
                "size_rows": s.pane.size_rows,
            })
        })
        .collect();
    json!({ "panes": Value::Array(rows) })
}

// Placeholder Sender: the real reply channel arrives via
// `McpRequest::ToolCall`, and `drain_requests` overwrites this field
// before pushing the Pending — so the discarded Receiver is never
// observed.
fn dummy_reply_sender() -> Sender<McpResponse> {
    let (tx, _rx) = std::sync::mpsc::channel();
    tx
}
