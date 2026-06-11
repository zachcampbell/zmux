// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

// Fuzz the client-side InputParser: it consumes raw bytes from the
// attached terminal (keystrokes, SGR/X10/rxvt mouse reports, escape
// fragments split across reads), which is the same untrusted-byte
// profile as the PTY ingester. Deterministic seeded RNG; reproduce a
// failure from the printed seed.
//
// Knobs: ZMUX_INPUT_FUZZ_ITERS (default 2000), ZMUX_INPUT_FUZZ_SEED.

use std::env;

use zmux::{InputAction, InputParser};

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

fn env_u64(key: &str, default: u64) -> u64 {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

// Regression: a terminal that opens an escape sequence and never
// finishes it (`ESC [` then digits forever) used to grow `pending`
// without bound — a slow OOM one layer above the VT ingester's
// now-capped CSI/OSC buffers. Past 8 KiB the parser must flush the
// garbage raw and reset instead of buffering it across reads.
#[test]
fn never_finalized_escape_does_not_grow_pending_forever() {
    let mut parser = InputParser::default();
    let mut forwarded = 0usize;
    let mut pushed = 0usize;
    // 1 MiB of unfinished CSI body, fed in 1 KiB slices.
    let slice = [b'1'; 1024];
    let actions = parser.push_bytes(b"\x1b[");
    assert!(
        actions.is_empty(),
        "incomplete escape should buffer at first"
    );
    pushed += 2;
    for _ in 0..1024 {
        pushed += slice.len();
        for action in parser.push_bytes(&slice) {
            if let InputAction::Forward(bytes) = action {
                forwarded += bytes.len();
            }
        }
    }
    // Nearly everything must have been flushed back out; the residue
    // still buffered is at most one cap's worth, not a megabyte.
    assert!(
        pushed - forwarded <= 16 * 1024,
        "pending grew unbounded: pushed {pushed}, forwarded {forwarded}"
    );
    // And the parser still works afterwards.
    let actions = parser.push_bytes(b"plain");
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, InputAction::Forward(b) if b.ends_with(b"plain"))),
        "parser must keep functioning after an overflow flush"
    );
}

#[test]
fn input_parser_survives_fuzzed_terminal_bytes() {
    let iters = env_u64("ZMUX_INPUT_FUZZ_ITERS", 2000);
    let base_seed = env_u64("ZMUX_INPUT_FUZZ_SEED", 0x1217);

    for i in 0..iters {
        let seed = base_seed.wrapping_add(i);
        let mut rng = Rng::new(seed);
        let mut parser = InputParser::default();
        let mut total_forwarded = 0usize;
        let mut total_pushed = 0usize;

        let feeds = 1 + rng.below(30);
        for _ in 0..feeds {
            let mut buf = Vec::new();
            let tokens = 1 + rng.below(6);
            for _ in 0..tokens {
                match rng.below(8) {
                    // Mouse-report shaped fragments with hostile params.
                    0 => {
                        buf.extend_from_slice(b"\x1b[<");
                        for _ in 0..rng.below(5) {
                            for _ in 0..rng.below(8) {
                                buf.push(b'0' + (rng.below(10)) as u8);
                            }
                            buf.push(b';');
                        }
                        buf.push(if rng.below(2) == 0 { b'M' } else { b'm' });
                    }
                    // X10 mouse, possibly truncated.
                    1 => {
                        buf.extend_from_slice(b"\x1b[M");
                        for _ in 0..rng.below(4) {
                            buf.push(rng.below(256) as u8);
                        }
                    }
                    // CSI with arbitrary body, sometimes never finalized.
                    2 => {
                        buf.extend_from_slice(b"\x1b[");
                        for _ in 0..rng.below(20) {
                            buf.push((0x20 + rng.below(0x20)) as u8);
                        }
                        if rng.below(3) > 0 {
                            buf.push((0x40 + rng.below(0x3f)) as u8);
                        }
                    }
                    // Bare ESC runs (alt-keys, split sequences).
                    3 => {
                        let count = 1 + rng.below(4) as usize;
                        buf.extend(std::iter::repeat_n(0x1b, count));
                    }
                    // Plain text.
                    4..=5 => {
                        for _ in 0..1 + rng.below(30) {
                            buf.push((0x20 + rng.below(0x5f)) as u8);
                        }
                    }
                    // Raw noise.
                    _ => {
                        for _ in 0..1 + rng.below(20) {
                            buf.push(rng.below(256) as u8);
                        }
                    }
                }
            }
            total_pushed += buf.len();
            for action in parser.push_bytes(&buf) {
                if let InputAction::Forward(bytes) = action {
                    total_forwarded += bytes.len();
                }
            }
        }

        // Nothing the parser emits can exceed what went in — Forward
        // actions are byte-for-byte slices of the input stream.
        assert!(
            total_forwarded <= total_pushed,
            "seed {seed:#x}: parser fabricated bytes ({total_forwarded} > {total_pushed})"
        );
    }
}
