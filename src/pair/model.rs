// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Ollama HTTP client (OpenAI-compatible chat-completions surface).

use serde_json::{Value, json};

/// `{target}` is substituted at startup.
pub const SYSTEM_PROMPT_TEMPLATE: &str = "\
You are zmux pair, a co-pilot bound to pane {target} of a zmux session.

Two modes interleave:
- INTERACTIVE: user types at a chat prompt. Answer normally.
- PROACTIVE: <zmux-event> messages arrive when pane {target} errors,
  exits, or waits for input. Respond with 1-2 sentences. Don't call
  tools unless explicitly asked.

Tools (target pane only):
- read_pane: current screen or scrollback
- send_keys: type into pane {target} \u{2014} always confirmed by user,
  may be declined

Style: concise, plain prose, no headings. No emojis unless the user
starts. Stay terse on proactive notes; the user is reading them
mid-task.";

pub fn system_prompt(target: u32) -> String {
    SYSTEM_PROMPT_TEMPLATE.replace("{target}", &target.to_string())
}

/// `pane_id` is intentionally omitted from each schema — the
/// mediator injects `pane_id = target` before the MCP call, so the
/// model cannot address other panes.
pub fn tool_schemas() -> Vec<Value> {
    vec![
        json!({
            "type": "function",
            "function": {
                "name": "read_pane",
                "description": "Read the target pane's current screen or scrollback.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "lines": {"type": "integer", "default": 200, "minimum": 1},
                        "mode": {"type": "string", "enum": ["visible", "scrollback"], "default": "visible"},
                        "strip_ansi": {"type": "boolean", "default": true}
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "send_keys",
                "description": "Type into the target pane. Always confirmed by the user; may be declined.",
                "parameters": {
                    "type": "object",
                    "required": ["keys"],
                    "properties": {
                        "keys": {"type": "string"},
                        "enter": {"type": "boolean", "default": false}
                    }
                }
            }
        }),
    ]
}

use crate::pair::conversation::{Message, ToolCall, ToolCallFunction};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishReason {
    Stop,
    ToolCalls,
    Length,
    Other(String),
}

impl FinishReason {
    fn from_str(s: &str) -> Self {
        match s {
            "stop" => FinishReason::Stop,
            "tool_calls" => FinishReason::ToolCalls,
            "length" => FinishReason::Length,
            other => FinishReason::Other(other.to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelOutput {
    Text {
        content: String,
        finish: FinishReason,
    },
    ToolCalls(Vec<ToolCall>),
}

pub fn parse_response(raw: &Value) -> Result<ModelOutput, String> {
    let choices = raw
        .get("choices")
        .and_then(|c| c.as_array())
        .ok_or_else(|| "response: missing `choices` array".to_string())?;
    let choice = choices
        .first()
        .ok_or_else(|| "response: no choices".to_string())?;
    let message = choice
        .get("message")
        .ok_or_else(|| "response: choice missing `message`".to_string())?;
    let finish = choice
        .get("finish_reason")
        .and_then(|s| s.as_str())
        .map(FinishReason::from_str)
        .unwrap_or(FinishReason::Other("unknown".into()));

    if let Some(tc_arr) = message.get("tool_calls").and_then(|v| v.as_array())
        && !tc_arr.is_empty()
    {
        let calls: Vec<ToolCall> = tc_arr
            .iter()
            .map(|c| {
                let id = c
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let fn_name = c
                    .pointer("/function/name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let fn_args = c
                    .pointer("/function/arguments")
                    .and_then(|v| v.as_str())
                    .unwrap_or("{}")
                    .to_string();
                ToolCall {
                    id,
                    kind: "function".to_string(),
                    function: ToolCallFunction {
                        name: fn_name,
                        arguments: fn_args,
                    },
                }
            })
            .collect();
        return Ok(ModelOutput::ToolCalls(calls));
    }

    let content = message
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Ok(ModelOutput::Text { content, finish })
}

pub fn build_request_body(
    model: &str,
    system: &str,
    history: &[Message],
    tools: &[Value],
) -> Value {
    // OpenAI expects `system` as a message, not a top-level field.
    let mut messages: Vec<Value> = vec![json!({ "role": "system", "content": system })];
    for m in history {
        messages.push(serde_json::to_value(m).unwrap_or(json!(null)));
    }
    json!({
        "model": model,
        "messages": messages,
        "tools": tools,
        "stream": false,
        "max_tokens": 1024
    })
}

#[allow(dead_code)]
pub struct OllamaClient {
    base_url: String,
    timeout: std::time::Duration,
    agent: ureq::Agent,
}

#[allow(dead_code)]
impl OllamaClient {
    pub fn new() -> Self {
        let secs = std::env::var("ZMUX_PAIR_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(60);
        Self {
            base_url: "http://localhost:11434".to_string(),
            timeout: std::time::Duration::from_secs(secs),
            agent: ureq::AgentBuilder::new()
                .timeout(std::time::Duration::from_secs(secs))
                .build(),
        }
    }

    pub fn probe(&self) -> Result<(), String> {
        let url = format!("{}/api/tags", self.base_url);
        match self.agent.get(&url).call() {
            Ok(_) => Ok(()),
            Err(err) => Err(format!(
                "no Ollama server at {} ({err}) — is 'ollama serve' running?",
                self.base_url
            )),
        }
    }

    pub fn complete(
        &self,
        model: &str,
        system: &str,
        history: &[Message],
        tools: &[Value],
    ) -> Result<ModelOutput, String> {
        let url = format!("{}/v1/chat/completions", self.base_url);
        let body = build_request_body(model, system, history, tools);
        let resp = self
            .agent
            .post(&url)
            .set("content-type", "application/json")
            .send_json(body)
            .map_err(|err| format!("model call failed: {err}"))?;
        let raw: Value = resp
            .into_json()
            .map_err(|err| format!("model response: invalid JSON: {err}"))?;
        parse_response(&raw)
    }
}

impl Default for OllamaClient {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for OllamaClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OllamaClient")
            .field("base_url", &self.base_url)
            .field("timeout", &self.timeout)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_prompt_substitutes_target() {
        let p = system_prompt(7);
        assert!(p.contains("pane 7"));
        assert!(!p.contains("{target}"));
    }

    #[test]
    fn tool_schemas_omit_pane_id() {
        let schemas = tool_schemas();
        for s in &schemas {
            let props = &s["function"]["parameters"]["properties"];
            assert!(
                props.get("pane_id").is_none(),
                "schema {} must NOT expose pane_id to the model",
                s["function"]["name"]
            );
        }
    }

    #[test]
    fn tool_schemas_have_expected_names() {
        let schemas = tool_schemas();
        let names: Vec<&str> = schemas
            .iter()
            .map(|s| s["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["read_pane", "send_keys"]);
    }

    #[test]
    fn read_pane_schema_has_no_required_fields() {
        let schemas = tool_schemas();
        let read_pane = &schemas[0];
        assert!(
            read_pane["function"]["parameters"]
                .get("required")
                .is_none(),
            "read_pane should have no required fields"
        );
    }

    #[test]
    fn send_keys_schema_requires_keys() {
        let schemas = tool_schemas();
        let send_keys = &schemas[1];
        let req = send_keys["function"]["parameters"]["required"]
            .as_array()
            .unwrap();
        assert_eq!(req, &vec![Value::from("keys")]);
    }

    #[test]
    fn parse_text_response_returns_assistant_text() {
        let raw = json!({
            "choices": [{
                "message": {"role":"assistant","content":"hello there"},
                "finish_reason": "stop"
            }]
        });
        let parsed = parse_response(&raw).unwrap();
        match parsed {
            ModelOutput::Text { content, finish } => {
                assert_eq!(content, "hello there");
                assert_eq!(finish, FinishReason::Stop);
            }
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn parse_tool_call_response_returns_calls() {
        let raw = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "c1",
                        "type": "function",
                        "function": { "name": "read_pane", "arguments": "{\"lines\":80}" }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        let parsed = parse_response(&raw).unwrap();
        match parsed {
            ModelOutput::ToolCalls(calls) => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].function.name, "read_pane");
                assert_eq!(calls[0].id, "c1");
            }
            _ => panic!("expected ToolCalls"),
        }
    }

    #[test]
    fn parse_response_truncated_marks_finish_reason() {
        let raw = json!({
            "choices": [{
                "message": {"role":"assistant","content":"partial..."},
                "finish_reason": "length"
            }]
        });
        let parsed = parse_response(&raw).unwrap();
        match parsed {
            ModelOutput::Text { finish, .. } => assert_eq!(finish, FinishReason::Length),
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn parse_response_empty_choices_errors() {
        let raw = json!({ "choices": [] });
        let err = parse_response(&raw).unwrap_err();
        assert!(err.contains("no choices"), "got: {err}");
    }

    #[test]
    fn build_request_body_includes_system_first() {
        use crate::pair::conversation::Message;
        let history = vec![Message::User {
            content: "hi".into(),
        }];
        let body = build_request_body("m", "SYS", &history, &tool_schemas());
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "SYS");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "hi");
        assert_eq!(body["model"], "m");
        assert_eq!(body["stream"], false);
        assert_eq!(body["max_tokens"], 1024);
    }
}
