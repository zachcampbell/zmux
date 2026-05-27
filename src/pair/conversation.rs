// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pure state machine for the pair conversation: message list,
//! pending tool calls, per-turn tool-loop counter, and the
//! event→context conversion. No I/O.

use serde::{Deserialize, Serialize};

/// OpenAI-shaped chat message; mirrors the wire shape Ollama expects
/// on `/v1/chat/completions`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    User {
        content: String,
    },
    Assistant {
        content: Option<String>,
        #[serde(skip_serializing_if = "Vec::is_empty", default)]
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ToolCallFunction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallFunction {
    pub name: String,
    /// OpenAI spec wraps arguments as a JSON-encoded string.
    pub arguments: String,
}

pub const MAX_TOOL_ROUNDS: u32 = 5;

#[derive(Debug, Default)]
#[allow(dead_code)]
pub struct ConversationState {
    pub history: Vec<Message>,
    /// Only `send_keys` parks here; `read_pane` is auto-approved.
    pub pending_confirm: Option<ToolCall>,
    /// Reset at the start of each user-driven turn.
    pub tool_rounds: u32,
}

impl ConversationState {
    pub fn new() -> Self {
        Self::default()
    }

    #[allow(dead_code)]
    pub fn push_user(&mut self, content: impl Into<String>) {
        self.history.push(Message::User {
            content: content.into(),
        });
        self.tool_rounds = 0;
    }

    #[allow(dead_code)]
    pub fn push_assistant_text(&mut self, content: impl Into<String>) {
        self.history.push(Message::Assistant {
            content: Some(content.into()),
            tool_calls: vec![],
        });
    }

    #[allow(dead_code)]
    pub fn push_assistant_tool_calls(&mut self, calls: Vec<ToolCall>) {
        self.history.push(Message::Assistant {
            content: None,
            tool_calls: calls,
        });
    }

    #[allow(dead_code)]
    pub fn push_tool_result(
        &mut self,
        tool_call_id: impl Into<String>,
        content: impl Into<String>,
    ) {
        self.history.push(Message::Tool {
            tool_call_id: tool_call_id.into(),
            content: content.into(),
        });
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriggerKind {
    StateChanged { from: String, to: String },
    Exited { exit_code: i32 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trigger {
    pub kind: TriggerKind,
    /// Snapshot taken in the watcher so it reflects the moment the
    /// event fired even if the mediator queue is busy.
    pub scrollback_tail: String,
}

pub fn synthesize_event_message(target_pane: u32, trig: &Trigger) -> String {
    let header = match &trig.kind {
        TriggerKind::StateChanged { from, to } => {
            format!("Pane {target_pane} transitioned {from} → {to}.")
        }
        TriggerKind::Exited { exit_code } => {
            format!("Pane {target_pane} exited with code {exit_code}.")
        }
    };
    format!(
        "<zmux-event>{header}\nLast 40 lines:\n{}\n</zmux-event>",
        trig.scrollback_tail.trim_end()
    )
}

impl ConversationState {
    pub fn push_trigger(&mut self, target_pane: u32, trig: &Trigger) {
        self.push_user(synthesize_event_message(target_pane, trig));
    }
}

/// True for tool calls that must be gated through a user [y/N]
/// prompt before execution.
pub fn requires_confirmation(call: &ToolCall) -> bool {
    matches!(call.function.name.as_str(), "send_keys")
}

impl ConversationState {
    /// Returns `Some(call)` when approved; on decline appends a
    /// synthetic tool_result and returns `None`.
    pub fn resolve_pending_confirm(&mut self, approved: bool) -> Option<ToolCall> {
        let call = self.pending_confirm.take()?;
        if approved {
            Some(call)
        } else {
            self.history.push(Message::Tool {
                tool_call_id: call.id.clone(),
                content: r#"{"declined":true,"reason":"user declined"}"#.to_string(),
            });
            None
        }
    }

    pub fn append_decline_tool_result(&mut self) {
        if let Some(call) = self.pending_confirm.take() {
            self.history.push(Message::Tool {
                tool_call_id: call.id,
                content: r#"{"declined":true,"reason":"user declined"}"#.to_string(),
            });
        }
    }

    /// Returns `Err` when the per-turn `MAX_TOOL_ROUNDS` cap would be
    /// exceeded; the mediator surfaces that as `[error] tool loop
    /// exceeded` and returns to the prompt.
    pub fn bump_tool_round(&mut self) -> Result<u32, String> {
        if self.tool_rounds >= MAX_TOOL_ROUNDS {
            return Err(format!(
                "tool loop exceeded ({MAX_TOOL_ROUNDS} rounds); aborting turn"
            ));
        }
        self.tool_rounds += 1;
        Ok(self.tool_rounds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value as JsonValue;

    #[test]
    fn empty_state_has_no_history() {
        let s = ConversationState::new();
        assert!(s.history.is_empty());
        assert_eq!(s.tool_rounds, 0);
        assert!(s.pending_confirm.is_none());
    }

    #[test]
    fn push_user_resets_tool_rounds() {
        let mut s = ConversationState::new();
        s.tool_rounds = 3;
        s.push_user("hello");
        assert_eq!(s.tool_rounds, 0);
        assert_eq!(s.history.len(), 1);
    }

    #[test]
    fn push_assistant_text_appends() {
        let mut s = ConversationState::new();
        s.push_user("hi");
        s.push_assistant_text("hello there");
        assert_eq!(s.history.len(), 2);
        match &s.history[1] {
            Message::Assistant {
                content,
                tool_calls,
            } => {
                assert_eq!(content.as_deref(), Some("hello there"));
                assert!(tool_calls.is_empty());
            }
            _ => panic!("expected Assistant"),
        }
    }

    #[test]
    fn push_assistant_tool_calls_records_calls() {
        let mut s = ConversationState::new();
        let call = ToolCall {
            id: "c1".to_string(),
            kind: "function".to_string(),
            function: ToolCallFunction {
                name: "read_pane".to_string(),
                arguments: r#"{"lines":80}"#.to_string(),
            },
        };
        s.push_assistant_tool_calls(vec![call.clone()]);
        match &s.history[0] {
            Message::Assistant {
                content,
                tool_calls,
            } => {
                assert!(content.is_none());
                assert_eq!(tool_calls, &vec![call]);
            }
            _ => panic!("expected Assistant"),
        }
    }

    #[test]
    fn push_tool_result_uses_tool_role() {
        let mut s = ConversationState::new();
        s.push_tool_result("c1", "Idle\nWorking");
        match &s.history[0] {
            Message::Tool {
                tool_call_id,
                content,
            } => {
                assert_eq!(tool_call_id, "c1");
                assert_eq!(content, "Idle\nWorking");
            }
            _ => panic!("expected Tool"),
        }
    }

    #[test]
    fn message_serializes_with_role_field() {
        let m = Message::User {
            content: "hi".into(),
        };
        let v: JsonValue = serde_json::to_value(&m).unwrap();
        assert_eq!(v["role"], "user");
        assert_eq!(v["content"], "hi");
    }

    #[test]
    fn synthesize_event_message_wraps_with_zmux_event_tags() {
        let trig = Trigger {
            kind: TriggerKind::StateChanged {
                from: "Working".into(),
                to: "Errored".into(),
            },
            scrollback_tail: "$ cargo test\nerror: foo\n".into(),
        };
        let msg = synthesize_event_message(2, &trig);
        assert!(msg.starts_with("<zmux-event>"));
        assert!(msg.ends_with("</zmux-event>"));
        assert!(msg.contains("Pane 2 transitioned Working → Errored"));
        assert!(msg.contains("$ cargo test"));
    }

    #[test]
    fn synthesize_event_message_for_exit_includes_code() {
        let trig = Trigger {
            kind: TriggerKind::Exited { exit_code: 1 },
            scrollback_tail: "(empty)".into(),
        };
        let msg = synthesize_event_message(7, &trig);
        assert!(msg.contains("Pane 7 exited with code 1"));
    }

    #[test]
    fn synthesize_event_message_for_awaiting_input_describes_prompt() {
        let trig = Trigger {
            kind: TriggerKind::StateChanged {
                from: "Working".into(),
                to: "AwaitingInput".into(),
            },
            scrollback_tail: "Are you sure? [y/N] ".into(),
        };
        let msg = synthesize_event_message(3, &trig);
        assert!(msg.contains("AwaitingInput"));
        assert!(msg.contains("Are you sure?"));
    }

    fn fake_call(name: &str, args_json: &str) -> ToolCall {
        ToolCall {
            id: "call-x".to_string(),
            kind: "function".to_string(),
            function: ToolCallFunction {
                name: name.to_string(),
                arguments: args_json.to_string(),
            },
        }
    }

    #[test]
    fn read_pane_does_not_require_confirmation() {
        assert!(!requires_confirmation(&fake_call("read_pane", "{}")));
    }

    #[test]
    fn send_keys_requires_confirmation() {
        assert!(requires_confirmation(&fake_call("send_keys", "{}")));
    }

    #[test]
    fn approve_pending_clears_state() {
        let mut s = ConversationState::new();
        s.pending_confirm = Some(fake_call("send_keys", r#"{"keys":"ls"}"#));
        let resolved = s.resolve_pending_confirm(true).expect("had pending");
        assert_eq!(resolved.function.name, "send_keys");
        assert!(s.pending_confirm.is_none());
    }

    #[test]
    fn decline_emits_synthetic_tool_result_and_clears_pending() {
        let mut s = ConversationState::new();
        s.pending_confirm = Some(fake_call("send_keys", r#"{"keys":"rm -rf /"}"#));
        s.append_decline_tool_result();
        assert!(s.pending_confirm.is_none());
        match s.history.last() {
            Some(Message::Tool {
                tool_call_id,
                content,
            }) => {
                assert_eq!(tool_call_id, "call-x");
                assert!(content.contains("declined"));
            }
            other => panic!("expected Tool result, got {other:?}"),
        }
    }

    #[test]
    fn tool_round_can_be_bumped_up_to_max() {
        let mut s = ConversationState::new();
        for _ in 0..MAX_TOOL_ROUNDS {
            assert!(s.bump_tool_round().is_ok());
        }
        let err = s.bump_tool_round().unwrap_err();
        assert!(err.contains("tool loop exceeded"), "got: {err}");
    }

    #[test]
    fn user_message_resets_tool_rounds_after_cap() {
        let mut s = ConversationState::new();
        for _ in 0..MAX_TOOL_ROUNDS {
            s.bump_tool_round().unwrap();
        }
        s.push_user("new turn");
        assert_eq!(s.tool_rounds, 0);
        assert!(s.bump_tool_round().is_ok());
    }
}
