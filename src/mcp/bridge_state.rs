// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pure protocol-aware state machine for the stdio bridge.
//!
//! Tracks in-flight request ids, caches `initialize` for replay,
//! classifies incoming responses, and emits the frames required to
//! survive a daemon restart: synthesized errors for in-flight ids,
//! plus a replayed init (with a synthetic id, since the client
//! already has an init response from the prior daemon and would
//! reject a duplicate).
//!
//! Daemon-side state (pane ids, window state) is not preserved across
//! a restart; the bridge only restores the JSON-RPC transport.
//! Clients must re-discover via `list_panes`.

use std::collections::HashSet;

use serde_json::{Value, json};

#[derive(Default)]
pub(crate) struct BridgeState {
    cached_init: Option<Vec<u8>>,
    initialized_seen: bool,
    pending: HashSet<String>,
    /// Ids minted by the bridge for replayed inits; responses with
    /// these ids must not be forwarded to the client.
    synthetic_ids: HashSet<String>,
    replay_seq: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum IncomingDisposition {
    Forward,
    /// Response to a bridge-issued (init replay) request; must not
    /// reach the client.
    ConsumeSynthetic,
}

impl BridgeState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Caches `initialize`, marks `notifications/initialized`, and
    /// pends any request id so a disconnect can synthesize an error
    /// for it. Non-object / batch frames are passed through;
    /// `is_batch_frame` rejection happens upstream.
    pub fn observe_outgoing(&mut self, frame: &[u8]) {
        let parsed: Value = match serde_json::from_slice(frame) {
            Ok(v) => v,
            Err(_) => return,
        };
        if !parsed.is_object() {
            return;
        }
        let method = parsed.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id = parsed.get("id");
        let has_real_id = id.map(|i| !i.is_null()).unwrap_or(false);

        if method == "initialize" {
            self.cached_init = Some(frame.to_vec());
        }
        if method == "notifications/initialized" {
            self.initialized_seen = true;
        }
        if has_real_id
            && !method.is_empty()
            && let Some(id) = id
        {
            self.pending.insert(id_key(id));
        }
    }

    /// Returns `ConsumeSynthetic` when the response matches a
    /// bridge-issued replay id — caller must not forward it.
    pub fn observe_incoming(&mut self, frame: &[u8]) -> IncomingDisposition {
        let parsed: Value = match serde_json::from_slice(frame) {
            Ok(v) => v,
            Err(_) => return IncomingDisposition::Forward,
        };
        if !parsed.is_object() {
            return IncomingDisposition::Forward;
        }
        let method = parsed.get("method").and_then(|m| m.as_str());
        let id = parsed.get("id");
        // Responses carry an id and no method; notifications are the
        // inverse and pass through untouched.
        if method.is_none()
            && let Some(id) = id
            && !id.is_null()
        {
            let key = id_key(id);
            if self.synthetic_ids.remove(&key) {
                return IncomingDisposition::ConsumeSynthetic;
            }
            self.pending.remove(&key);
        }
        IncomingDisposition::Forward
    }

    pub fn synthesize_pending_errors(&mut self) -> Vec<Vec<u8>> {
        let pending = std::mem::take(&mut self.pending);
        pending
            .into_iter()
            .map(|key| {
                let id_value = parse_key_to_value(&key);
                let frame = json!({
                    "jsonrpc": "2.0",
                    "id": id_value,
                    "error": {
                        "code": -32603,
                        "message": "zmux bridge: daemon connection lost; request was not delivered"
                    }
                });
                let mut bytes = serde_json::to_vec(&frame).expect("synthesize error frame");
                bytes.push(b'\n');
                bytes
            })
            .collect()
    }

    /// Build the frames a freshly-reconnected daemon needs to restore
    /// protocol state. Empty until the client has sent its first
    /// `initialize`; otherwise `[init]` or `[init, initialized]`. The
    /// init's id is rewritten to a fresh synthetic id.
    pub fn replay_init_frames(&mut self) -> Vec<Vec<u8>> {
        let Some(cached) = self.cached_init.clone() else {
            return Vec::new();
        };
        let mut parsed: Value = match serde_json::from_slice(&cached) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        self.replay_seq += 1;
        let synthetic_id = format!("zmux-bridge-replay-{}", self.replay_seq);
        if let Some(obj) = parsed.as_object_mut() {
            obj.insert("id".to_string(), Value::String(synthetic_id.clone()));
        }
        self.synthetic_ids
            .insert(id_key(&Value::String(synthetic_id)));
        let mut frames: Vec<Vec<u8>> = Vec::new();
        let mut bytes = serde_json::to_vec(&parsed).expect("serialize replay init");
        bytes.push(b'\n');
        frames.push(bytes);
        if self.initialized_seen {
            let notif = json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            });
            let mut bytes = serde_json::to_vec(&notif).expect("serialize initialized notif");
            bytes.push(b'\n');
            frames.push(bytes);
        }
        frames
    }

    #[allow(dead_code)]
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    #[allow(dead_code)]
    pub fn cached_init_present(&self) -> bool {
        self.cached_init.is_some()
    }

    #[allow(dead_code)]
    pub fn initialized_seen(&self) -> bool {
        self.initialized_seen
    }
}

/// Canonicalize an id so number `1` and string `"1"` get distinct
/// keys, as required by the JSON-RPC spec.
fn id_key(id: &Value) -> String {
    id.to_string()
}

fn parse_key_to_value(key: &str) -> Value {
    serde_json::from_str(key).unwrap_or(Value::Null)
}

pub(crate) fn is_batch_frame(frame: &[u8]) -> bool {
    for &b in frame {
        match b {
            b' ' | b'\t' | b'\r' | b'\n' => continue,
            b'[' => return true,
            _ => return false,
        }
    }
    false
}

pub(crate) fn batch_rejection_frame() -> Vec<u8> {
    let frame = json!({
        "jsonrpc": "2.0",
        "id": null,
        "error": {
            "code": -32600,
            "message": "zmux bridge: batched requests are not supported"
        }
    });
    let mut bytes = serde_json::to_vec(&frame).expect("serialize batch rejection");
    bytes.push(b'\n');
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(frame: &[u8]) -> Value {
        let stripped = frame.strip_suffix(b"\n").unwrap_or(frame);
        serde_json::from_slice(stripped).expect("parse JSON")
    }

    #[test]
    fn new_state_is_empty() {
        let s = BridgeState::new();
        assert_eq!(s.pending_count(), 0);
        assert!(!s.cached_init_present());
        assert!(!s.initialized_seen());
    }

    #[test]
    fn outgoing_initialize_request_is_cached_and_pended() {
        let mut s = BridgeState::new();
        s.observe_outgoing(br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);
        assert!(s.cached_init_present());
        assert_eq!(s.pending_count(), 1);
    }

    #[test]
    fn incoming_response_clears_matching_pending_id() {
        let mut s = BridgeState::new();
        s.observe_outgoing(br#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#);
        let disp = s.observe_incoming(br#"{"jsonrpc":"2.0","id":1,"result":{}}"#);
        assert_eq!(disp, IncomingDisposition::Forward);
        assert_eq!(s.pending_count(), 0);
    }

    #[test]
    fn outgoing_notification_does_not_pend() {
        let mut s = BridgeState::new();
        s.observe_outgoing(br#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{}}"#);
        assert_eq!(s.pending_count(), 0);
    }

    #[test]
    fn outgoing_notifications_initialized_marks_seen() {
        let mut s = BridgeState::new();
        s.observe_outgoing(br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        assert!(s.initialized_seen());
    }

    #[test]
    fn outgoing_tools_call_is_pended() {
        let mut s = BridgeState::new();
        s.observe_outgoing(
            br#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"list_panes"}}"#,
        );
        assert_eq!(s.pending_count(), 1);
        assert!(
            !s.cached_init_present(),
            "tools/call must not be cached as init"
        );
    }

    #[test]
    fn synthesize_pending_errors_produces_one_frame_per_id_and_clears() {
        let mut s = BridgeState::new();
        s.observe_outgoing(br#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#);
        s.observe_outgoing(br#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#);
        s.observe_outgoing(br#"{"jsonrpc":"2.0","id":3,"method":"tools/call"}"#);
        let frames = s.synthesize_pending_errors();
        assert_eq!(frames.len(), 3);
        let mut ids: Vec<i64> = frames
            .iter()
            .map(|f| parse(f)["id"].as_i64().unwrap())
            .collect();
        ids.sort();
        assert_eq!(ids, vec![1, 2, 3]);
        for f in &frames {
            let v = parse(f);
            assert!(v["error"].is_object(), "must be a JSON-RPC error response");
            assert_eq!(v["error"]["code"], -32603);
            assert!(f.ends_with(b"\n"), "frame must be newline-terminated");
        }
        assert_eq!(s.pending_count(), 0);
    }

    #[test]
    fn replay_returns_empty_when_init_not_yet_seen() {
        let mut s = BridgeState::new();
        let frames = s.replay_init_frames();
        assert!(frames.is_empty());
    }

    #[test]
    fn replay_returns_init_only_when_initialized_not_yet_seen() {
        let mut s = BridgeState::new();
        s.observe_outgoing(br#"{"jsonrpc":"2.0","id":42,"method":"initialize","params":{}}"#);
        let frames = s.replay_init_frames();
        assert_eq!(frames.len(), 1);
        let v = parse(&frames[0]);
        assert_eq!(v["method"], "initialize");
        assert!(
            v["id"].as_str().unwrap().starts_with("zmux-bridge-replay-"),
            "id must be the synthetic replay id, not the client's original"
        );
        assert_ne!(v["id"], json!(42));
    }

    #[test]
    fn replay_returns_init_and_initialized_notification_when_both_seen() {
        let mut s = BridgeState::new();
        s.observe_outgoing(br#"{"jsonrpc":"2.0","id":42,"method":"initialize","params":{}}"#);
        s.observe_outgoing(br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        let frames = s.replay_init_frames();
        assert_eq!(frames.len(), 2);
        assert_eq!(parse(&frames[0])["method"], "initialize");
        assert_eq!(parse(&frames[1])["method"], "notifications/initialized");
        assert!(
            parse(&frames[1])
                .get("id")
                .map(|v| v.is_null())
                .unwrap_or(true)
        );
    }

    #[test]
    fn replay_response_is_consumed_not_forwarded() {
        let mut s = BridgeState::new();
        s.observe_outgoing(br#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#);
        let replay = s.replay_init_frames();
        assert_eq!(replay.len(), 1);
        let synthetic_id = parse(&replay[0])["id"].as_str().unwrap().to_string();
        let response = format!(r#"{{"jsonrpc":"2.0","id":"{synthetic_id}","result":{{}}}}"#);
        let disp = s.observe_incoming(response.as_bytes());
        assert_eq!(disp, IncomingDisposition::ConsumeSynthetic);
    }

    #[test]
    fn replay_increments_synthetic_id_across_reconnects() {
        let mut s = BridgeState::new();
        s.observe_outgoing(br#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#);
        let first = s.replay_init_frames();
        let second = s.replay_init_frames();
        assert_ne!(
            parse(&first[0])["id"],
            parse(&second[0])["id"],
            "second reconnect must use a fresh synthetic id so a stale response from the first replay can't collide"
        );
    }

    #[test]
    fn batch_array_frame_is_detected() {
        assert!(is_batch_frame(b"[]"));
        assert!(is_batch_frame(b"  [{}]"));
        assert!(is_batch_frame(b"\n\t[]"));
        assert!(!is_batch_frame(b"{}"));
        assert!(!is_batch_frame(b"  {}"));
        assert!(!is_batch_frame(b""));
    }

    #[test]
    fn batch_rejection_is_a_well_formed_jsonrpc_error() {
        let frame = batch_rejection_frame();
        assert!(frame.ends_with(b"\n"));
        let v = parse(&frame);
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["error"]["code"], -32600);
        assert!(v["id"].is_null());
    }

    #[test]
    fn malformed_frame_does_not_panic() {
        let mut s = BridgeState::new();
        s.observe_outgoing(b"not json");
        s.observe_outgoing(b"{partial");
        let disp = s.observe_incoming(b"also not json");
        assert_eq!(disp, IncomingDisposition::Forward);
        assert_eq!(s.pending_count(), 0);
    }

    #[test]
    fn string_id_is_distinct_from_number_id() {
        let mut s = BridgeState::new();
        s.observe_outgoing(br#"{"jsonrpc":"2.0","id":1,"method":"x"}"#);
        s.observe_outgoing(br#"{"jsonrpc":"2.0","id":"1","method":"y"}"#);
        assert_eq!(s.pending_count(), 2);
        s.observe_incoming(br#"{"jsonrpc":"2.0","id":1,"result":{}}"#);
        assert_eq!(s.pending_count(), 1, "string-1 should still be pending");
        s.observe_incoming(br#"{"jsonrpc":"2.0","id":"1","result":{}}"#);
        assert_eq!(s.pending_count(), 0);
    }

    #[test]
    fn server_to_client_notification_is_passed_through() {
        let mut s = BridgeState::new();
        let disp = s.observe_incoming(
            br#"{"jsonrpc":"2.0","method":"notifications/zmux/event","params":{}}"#,
        );
        assert_eq!(disp, IncomingDisposition::Forward);
    }
}
