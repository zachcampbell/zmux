// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::mem;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputAction {
    Forward(Vec<u8>),
    Mouse(MouseEvent),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseEvent {
    pub button: u16,
    pub col: u16,
    pub row: u16,
    pub final_byte: u8,
}

impl MouseEvent {
    pub fn is_scroll_up(self) -> bool {
        self.button & 64 != 0 && (self.button & 0b11) == 0
    }

    pub fn is_scroll_down(self) -> bool {
        self.button & 64 != 0 && (self.button & 0b11) == 1
    }

    pub fn wheel_lines(self, lines_per_event: usize) -> Option<usize> {
        if self.is_scroll_up() || self.is_scroll_down() {
            Some(lines_per_event.max(1))
        } else {
            None
        }
    }

    pub fn is_left_press(self) -> bool {
        self.final_byte == b'M'
            && self.button & 64 == 0
            && self.button & 32 == 0
            && (self.button & 0b11) == 0
    }

    // Motion event while the left button is held. In SGR 1002 (button-
    // tracking) mode the host terminal emits these as the user drags.
    // button & 32 marks "motion"; button & 0b11 == 0 says left-button.
    pub fn is_left_drag_motion(self) -> bool {
        self.final_byte == b'M'
            && self.button & 64 == 0
            && self.button & 32 != 0
            && (self.button & 0b11) == 0
    }

    // Button-up on the left button. Terminals signal release with
    // lowercase 'm' in SGR mode; the button field repeats what was
    // released. We only care about the left button here.
    pub fn is_left_release(self) -> bool {
        self.final_byte == b'm' && self.button & 64 == 0 && (self.button & 0b11) == 0
    }

    pub fn translate(self, origin_col: u16, origin_row: u16) -> Option<Self> {
        if self.col < origin_col || self.row < origin_row {
            return None;
        }

        Some(Self {
            col: self.col - origin_col,
            row: self.row - origin_row,
            ..self
        })
    }

    pub fn encode_sgr(self) -> Vec<u8> {
        format!(
            "\x1b[<{};{};{}{}",
            self.button,
            self.col.saturating_add(1),
            self.row.saturating_add(1),
            self.final_byte as char
        )
        .into_bytes()
    }
}

// `pending` holds at most one incomplete escape sequence between
// reads; real sequences are a handful of bytes. If it grows past this
// the "sequence" is garbage from a misbehaving terminal — forward it
// raw and reset rather than buffering it forever (the same
// degrade-don't-die stance as the VT ingester's CSI/OSC caps).
const MAX_PENDING_BYTES: usize = 8 * 1024;

#[derive(Debug, Default)]
pub struct InputParser {
    pending: Vec<u8>,
}

impl InputParser {
    pub fn push_bytes(&mut self, bytes: &[u8]) -> Vec<InputAction> {
        self.pending.extend_from_slice(bytes);

        let mut actions = Vec::new();
        while !self.pending.is_empty() {
            if self.pending[0] != 0x1b {
                let length = self
                    .pending
                    .iter()
                    .position(|byte| *byte == 0x1b)
                    .unwrap_or(self.pending.len());
                actions.push(InputAction::Forward(self.pending.drain(..length).collect()));
                continue;
            }

            if self.pending.len() == 1 {
                break;
            }

            if self.pending[1] != b'[' {
                // A second ESC landing right after the first isn't a
                // 2-byte (Alt-key style) sequence together with it — it's
                // either the start of a real escape sequence in its own
                // right (a stale lone ESC immediately followed by, say,
                // an arrow key's `ESC [ A`) or just another standalone
                // Escape keypress. Flush only the first byte and let the
                // loop re-examine the second ESC from scratch, instead of
                // consuming both as one malformed 2-byte Forward and
                // orphaning whatever follows.
                if self.pending[1] == 0x1b {
                    actions.push(InputAction::Forward(self.pending.drain(..1).collect()));
                    continue;
                }

                actions.push(InputAction::Forward(self.pending.drain(..2).collect()));
                continue;
            }

            if self.pending.starts_with(b"\x1b[M") {
                if self.pending.len() < 6 {
                    break;
                }
                let sequence: Vec<u8> = self.pending.drain(..6).collect();
                if let Some(mouse) = parse_x10_mouse_sequence(&sequence) {
                    actions.push(InputAction::Mouse(mouse));
                } else {
                    actions.push(InputAction::Forward(sequence));
                }
                continue;
            }

            let Some(final_offset) = self.pending[2..]
                .iter()
                .position(|byte| (0x40..=0x7e).contains(byte))
            else {
                break;
            };
            let final_index = final_offset + 2;
            let sequence: Vec<u8> = self.pending.drain(..=final_index).collect();

            if let Some(mouse) = parse_mouse_sequence(&sequence) {
                actions.push(InputAction::Mouse(mouse));
            } else {
                actions.push(InputAction::Forward(sequence));
            }
        }

        // The loop only leaves bytes in `pending` while waiting for
        // the rest of an escape sequence. Nothing legitimate is 8 KiB
        // of unfinished escape — flush it raw and start over.
        if self.pending.len() > MAX_PENDING_BYTES {
            actions.push(InputAction::Forward(mem::take(&mut self.pending)));
        }

        actions
    }

    // A lone ESC sitting in `pending` with nothing after it is
    // ambiguous: it could be the first byte of a multi-byte escape
    // sequence still in flight, or it could be the user pressing
    // Escape by itself — vim/Claude Code's exit-insert-mode key, say.
    // `push_bytes` alone can't tell these apart and, left to buffer
    // forever, a genuine lone ESC would never reach the pane until some
    // unrelated keypress arrived to disambiguate it.
    //
    // The caller resolves the ambiguity with timing: a complete escape
    // sequence arrives from a real terminal in a single burst, so if a
    // full poll tick passes with no more bytes from this client, nothing
    // else is coming — the ESC was real. Call this once per quiet tick
    // (see daemon.rs's client loop and its SERVER_POLL_MS, which doubles
    // as this escape-time) to flush it through. No-op, and safe to call
    // repeatedly, whenever `pending` doesn't hold exactly a lone ESC.
    pub fn flush_pending(&mut self) -> Option<InputAction> {
        if self.pending == [0x1b] {
            self.pending.clear();
            return Some(InputAction::Forward(vec![0x1b]));
        }
        None
    }
}

fn parse_mouse_sequence(sequence: &[u8]) -> Option<MouseEvent> {
    parse_sgr_mouse_sequence(sequence).or_else(|| parse_rxvt_mouse_sequence(sequence))
}

fn parse_sgr_mouse_sequence(sequence: &[u8]) -> Option<MouseEvent> {
    if sequence.len() < 6 || !sequence.starts_with(b"\x1b[<") {
        return None;
    }

    parse_semicolon_mouse_payload(&sequence[3..sequence.len() - 1], *sequence.last()?)
}

fn parse_rxvt_mouse_sequence(sequence: &[u8]) -> Option<MouseEvent> {
    if sequence.len() < 5 || !sequence.starts_with(b"\x1b[") || sequence.starts_with(b"\x1b[<") {
        return None;
    }

    parse_semicolon_mouse_payload(&sequence[2..sequence.len() - 1], *sequence.last()?)
}

fn parse_semicolon_mouse_payload(payload: &[u8], final_byte: u8) -> Option<MouseEvent> {
    if final_byte != b'M' && final_byte != b'm' {
        return None;
    }

    let payload = std::str::from_utf8(payload).ok()?;
    let mut fields = payload.split(';');
    let button = fields.next()?.parse::<u16>().ok()?;
    let column = fields.next()?.parse::<u16>().ok()?.saturating_sub(1);
    let row = fields.next()?.parse::<u16>().ok()?.saturating_sub(1);
    if fields.next().is_some() {
        return None;
    }

    Some(MouseEvent {
        button,
        col: column,
        row,
        final_byte,
    })
}

fn parse_x10_mouse_sequence(sequence: &[u8]) -> Option<MouseEvent> {
    if sequence.len() != 6 || !sequence.starts_with(b"\x1b[M") {
        return None;
    }

    Some(MouseEvent {
        button: sequence[3].saturating_sub(32) as u16,
        col: sequence[4].saturating_sub(33) as u16,
        row: sequence[5].saturating_sub(33) as u16,
        final_byte: b'M',
    })
}

#[cfg(test)]
mod tests {
    use super::{InputAction, InputParser, MouseEvent};

    #[test]
    fn forwards_plain_bytes() {
        let mut parser = InputParser::default();
        let actions = parser.push_bytes(b"ls -la\r");

        assert_eq!(actions, vec![InputAction::Forward(b"ls -la\r".to_vec())]);
    }

    #[test]
    fn detects_mouse_wheel_sequences() {
        let mut parser = InputParser::default();
        let actions = parser.push_bytes(b"\x1b[<64;42;7M\x1b[<65;42;7M");

        assert_eq!(
            actions,
            vec![
                InputAction::Mouse(MouseEvent {
                    button: 64,
                    col: 41,
                    row: 6,
                    final_byte: b'M'
                }),
                InputAction::Mouse(MouseEvent {
                    button: 65,
                    col: 41,
                    row: 6,
                    final_byte: b'M'
                })
            ]
        );
    }

    #[test]
    fn wheel_event_uses_the_configured_line_count() {
        let wheel = MouseEvent {
            button: 64,
            col: 0,
            row: 0,
            final_byte: b'M',
        };

        assert_eq!(wheel.wheel_lines(1), Some(1));
        assert_eq!(wheel.wheel_lines(5), Some(5));
    }

    #[test]
    fn detects_x10_mouse_wheel_sequences() {
        let mut parser = InputParser::default();
        let actions = parser.push_bytes(b"\x1b[M`*%");

        assert_eq!(
            actions,
            vec![InputAction::Mouse(MouseEvent {
                button: 64,
                col: 9,
                row: 4,
                final_byte: b'M'
            })]
        );
    }

    #[test]
    fn keeps_partial_x10_mouse_sequences_buffered_until_complete() {
        let mut parser = InputParser::default();

        let first = parser.push_bytes(b"\x1b[M`");
        let second = parser.push_bytes(b"*%");

        assert!(first.is_empty());
        assert_eq!(
            second,
            vec![InputAction::Mouse(MouseEvent {
                button: 64,
                col: 9,
                row: 4,
                final_byte: b'M'
            })]
        );
    }

    #[test]
    fn detects_rxvt_mouse_wheel_sequences() {
        let mut parser = InputParser::default();
        let actions = parser.push_bytes(b"\x1b[64;42;7M");

        assert_eq!(
            actions,
            vec![InputAction::Mouse(MouseEvent {
                button: 64,
                col: 41,
                row: 6,
                final_byte: b'M'
            })]
        );
    }

    #[test]
    fn forwards_other_escape_sequences_back_to_the_pty() {
        let mut parser = InputParser::default();
        let actions = parser.push_bytes(b"\x1b[A");

        assert_eq!(actions, vec![InputAction::Forward(b"\x1b[A".to_vec())]);
    }

    #[test]
    fn keeps_partial_mouse_sequences_buffered_until_complete() {
        let mut parser = InputParser::default();

        let first = parser.push_bytes(b"\x1b[<64;12");
        let second = parser.push_bytes(b";9M");

        assert!(first.is_empty());
        assert_eq!(
            second,
            vec![InputAction::Mouse(MouseEvent {
                button: 64,
                col: 11,
                row: 8,
                final_byte: b'M'
            })]
        );
    }

    #[test]
    fn parses_left_click_for_focus_routing() {
        let mut parser = InputParser::default();
        let actions = parser.push_bytes(b"\x1b[<0;3;2M");

        assert_eq!(
            actions,
            vec![InputAction::Mouse(MouseEvent {
                button: 0,
                col: 2,
                row: 1,
                final_byte: b'M'
            })]
        );
    }

    #[test]
    fn mouse_events_can_be_translated_and_reencoded() {
        let mouse = MouseEvent {
            button: 64,
            col: 18,
            row: 9,
            final_byte: b'M',
        };

        let translated = mouse.translate(10, 4).expect("translated event");

        assert_eq!(
            translated,
            MouseEvent {
                button: 64,
                col: 8,
                row: 5,
                final_byte: b'M'
            }
        );
        assert_eq!(translated.encode_sgr(), b"\x1b[<64;9;6M");
    }

    #[test]
    fn lone_esc_is_buffered_not_forwarded_immediately() {
        // A single ESC byte is ambiguous on its own — it might be the
        // first byte of a longer sequence still in flight — so
        // push_bytes must hold it rather than guessing.
        let mut parser = InputParser::default();
        let actions = parser.push_bytes(b"\x1b");

        assert!(actions.is_empty());
    }

    #[test]
    fn flush_pending_emits_a_buffered_lone_esc() {
        let mut parser = InputParser::default();
        assert!(parser.push_bytes(b"\x1b").is_empty());

        let flushed = parser.flush_pending();

        assert_eq!(flushed, Some(InputAction::Forward(vec![0x1b])));
    }

    #[test]
    fn flush_pending_is_a_no_op_with_nothing_buffered() {
        let mut parser = InputParser::default();

        assert_eq!(parser.flush_pending(), None);
    }

    #[test]
    fn flush_pending_does_not_double_flush() {
        let mut parser = InputParser::default();
        assert!(parser.push_bytes(b"\x1b").is_empty());

        assert_eq!(
            parser.flush_pending(),
            Some(InputAction::Forward(vec![0x1b]))
        );
        // The buffered ESC was already drained by the first flush;
        // a second call must not manufacture another one.
        assert_eq!(parser.flush_pending(), None);
    }

    #[test]
    fn flush_pending_ignores_a_sequence_still_in_flight() {
        // Two bytes of an unfinished CSI sequence (no final byte yet)
        // is not the "lone ESC" case flush_pending exists for — it's
        // still legitimately waiting on more bytes.
        let mut parser = InputParser::default();
        assert!(parser.push_bytes(b"\x1b[").is_empty());

        assert_eq!(parser.flush_pending(), None);
    }

    #[test]
    fn stale_lone_esc_followed_by_an_arrow_key_forwards_both_intact() {
        // The exact regression this bug describes: ESC arrives alone
        // in one read (buffered, nothing forwarded yet), then an Up
        // arrow (`ESC [ A`) arrives in a later read before any flush
        // happened. The stale ESC must come out as its own standalone
        // Forward, and the arrow key must survive as one intact
        // 3-byte Forward — not merged into a mangled `ESC ESC` pair
        // with the arrow's `[A` orphaned as literal text.
        let mut parser = InputParser::default();

        let first = parser.push_bytes(b"\x1b");
        assert!(first.is_empty());

        let second = parser.push_bytes(b"\x1b[A");
        assert_eq!(
            second,
            vec![
                InputAction::Forward(vec![0x1b]),
                InputAction::Forward(b"\x1b[A".to_vec()),
            ]
        );
    }

    #[test]
    fn split_arrow_sequence_still_reassembles_across_reads() {
        // A genuinely split (not stale-ESC-prefixed) arrow key must
        // still reassemble into one Forward, same as before this fix.
        let mut parser = InputParser::default();

        let first = parser.push_bytes(b"\x1b[");
        assert!(first.is_empty());

        let second = parser.push_bytes(b"A");
        assert_eq!(second, vec![InputAction::Forward(b"\x1b[A".to_vec())]);
    }

    #[test]
    fn consecutive_escape_keypresses_peel_off_one_at_a_time() {
        // Three real, separate Escape presses arriving in the same read
        // (fast typing, no intervening quiet tick) must come out as
        // standalone ESC forwards one at a time, never merged into a
        // multi-byte pair. The trailing ESC is still ambiguous on its
        // own and stays buffered for flush_pending, same as any other
        // lone ESC.
        let mut parser = InputParser::default();
        let actions = parser.push_bytes(b"\x1b\x1b\x1b");

        assert_eq!(
            actions,
            vec![
                InputAction::Forward(vec![0x1b]),
                InputAction::Forward(vec![0x1b]),
            ]
        );
        assert_eq!(
            parser.flush_pending(),
            Some(InputAction::Forward(vec![0x1b]))
        );
    }

    #[test]
    fn esc_followed_by_an_unrelated_byte_is_still_treated_as_a_meta_combo() {
        // Pre-existing behavior, unrelated to the flush fix: `ESC` then
        // an ordinary byte that isn't `[` or another ESC is forwarded
        // as a single 2-byte unit (the Alt/Meta-key convention many
        // terminals use). Only ESC-then-ESC and ESC-then-CSI needed to
        // change.
        let mut parser = InputParser::default();
        let actions = parser.push_bytes(b"\x1bx");

        assert_eq!(actions, vec![InputAction::Forward(b"\x1bx".to_vec())]);
    }
}
