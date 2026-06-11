// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

// Deterministic fuzz harness for the VT ingester.
//
// Stable-Rust, zero extra dependencies: a seeded xorshift RNG drives a
// token-based byte-stream generator (structured CSI/OSC/DCS, UTF-8 runs,
// raw noise, fixture splices, Synchronized Output regions) interleaved
// with resizes and render calls — the same op surface the daemon
// exercises. Every case is fully determined by `(base_seed, index)`, so
// any crash is reproducible from the printed seed alone.
//
// On panic the harness minimizes the failing case (op-level then
// byte-level greedy shrink) and writes the repro to
// `target/vt-fuzz-crash-<seed>.json` before failing the test.
//
// Knobs (env):
//   ZMUX_VT_FUZZ_ITERS  cases to run            (default 512)
//   ZMUX_VT_FUZZ_SEED   base seed               (default 0x5EED_2026)
//   ZMUX_VT_FUZZ_BYTES  approx bytes per case   (default 4096)
//
// Long soak: ZMUX_VT_FUZZ_ITERS=200000 cargo test --test vt_fuzz --release
// (release drops overflow checks; run the default dev profile too.)

use std::env;
use std::fmt::Write as _;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use zmux::terminal::TerminalIngest;
use zmux::{Pane, PtySize};

const FIXTURE: &[u8] = include_bytes!("fixtures/gemini-startup.bin");

// ---------------------------------------------------------------- rng

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
    fn chance(&mut self, num: u64, den: u64) -> bool {
        self.below(den) < num
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }
}

// ---------------------------------------------------------------- ops

#[derive(Clone, Debug)]
enum Op {
    Feed(Vec<u8>),
    Resize(u16, u16),
    Render,
    RenderCells,
    GridText,
    Flush,
    SetViewport(usize),
}

#[derive(Clone, Debug)]
struct Case {
    rows: u16,
    cols: u16,
    scrollback: usize,
    ops: Vec<Op>,
}

fn run_case(case: &Case) {
    let mut pane = Pane::new("fuzz", case.scrollback, case.rows as usize);
    let mut ingest = TerminalIngest::new(PtySize::new(case.rows, case.cols));
    for op in &case.ops {
        match op {
            Op::Feed(bytes) => {
                let _replies = ingest.ingest_bytes(&mut pane, bytes);
            }
            Op::Resize(r, c) => {
                ingest.resize(PtySize::new(*r, *c));
                pane.set_viewport_height(*r as usize);
            }
            Op::Render => {
                let _ = ingest.render_lines(&pane);
                let _ = ingest.rendered_line_count(&pane);
                let _ = ingest.current_line();
            }
            Op::RenderCells => {
                let _ = ingest.render_cells(&pane);
            }
            Op::GridText => {
                let _ = ingest.primary_grid_text();
                let _ = pane.visible_text();
                let _ = pane.scrollback_text(64, true);
            }
            Op::Flush => ingest.flush_incomplete_line(&mut pane),
            Op::SetViewport(h) => pane.set_viewport_height(*h),
        }
    }
    // Every case ends with a full render + flush so latent grid
    // corruption surfaces even when the random ops skipped them.
    let _ = ingest.render_cells(&pane);
    let _ = ingest.render_lines(&pane);
    ingest.flush_incomplete_line(&mut pane);
    let _ = pane.visible_text();
}

// ------------------------------------------------------- generation

const DIMS: &[u16] = &[0, 1, 2, 3, 4, 8, 24, 25, 80, 81, 132, 200, 255];

// Wide chars, combining marks, ZWJ emoji, box drawing — the width
// table's audited edge cases plus things real agent CLIs emit.
const UNICODE_POOL: &[&str] = &[
    "é", "漢", "字", "🎉", "👍🏽", "👨‍👩‍👧‍👦", "│", "█", "╭", "─", "·", "\u{0301}", "\u{200D}",
    "ﬀ", "𝕫", "ｗ", "🇺🇸", "…",
];

fn gen_csi(rng: &mut Rng, out: &mut Vec<u8>) {
    out.extend_from_slice(b"\x1b[");
    if rng.chance(1, 4) {
        out.push(*rng.pick(b"?>=!"));
    }
    let params = rng.below(5);
    for i in 0..params {
        if i > 0 {
            out.push(*rng.pick(b";;:"));
        }
        match rng.below(6) {
            // Small common params.
            0..=2 => {
                let _ = write!(out_as_string(out), "{}", rng.below(100));
            }
            // Boundary-sized params: u16/i32/u32/u64 edges.
            3 => {
                let v = *rng.pick(&[
                    0u64,
                    1,
                    2,
                    65535,
                    65536,
                    2147483647,
                    2147483648,
                    4294967295,
                    4294967296,
                    18446744073709551615,
                ]);
                let _ = write!(out_as_string(out), "{v}");
            }
            // Mode numbers the ingester knows about.
            4 => {
                let v = *rng.pick(&[1u64, 4, 25, 47, 1000, 1002, 1003, 1004, 1006, 1047, 1048, 1049, 2004, 2026]);
                let _ = write!(out_as_string(out), "{v}");
            }
            // Empty param (consecutive separators).
            _ => {}
        }
    }
    if rng.chance(1, 5) {
        out.push(*rng.pick(b" !\"#$%&'()*+,-./"));
    }
    // Bias toward finals the ingester implements; sometimes anything.
    if rng.chance(4, 5) {
        out.push(*rng.pick(b"ABCDEFGHJKLMPSTXbcdfghlmnpqrstu@"));
    } else {
        out.push((0x20 + rng.below(0x5f)) as u8);
    }
}

fn gen_osc(rng: &mut Rng, out: &mut Vec<u8>) {
    out.extend_from_slice(b"\x1b]");
    let id = *rng.pick(&[0u64, 1, 2, 4, 7, 8, 10, 11, 52, 133, 633, 99999]);
    let _ = write!(out_as_string(out), "{id}");
    if rng.chance(4, 5) {
        out.push(b';');
        let len = rng.below(40);
        for _ in 0..len {
            out.push((0x20 + rng.below(0x5f)) as u8);
        }
    }
    match rng.below(4) {
        0 => out.push(0x07),                      // BEL
        1 => out.extend_from_slice(b"\x1b\\"),    // ST
        2 => {}                                   // unterminated — next token interrupts
        _ => out.push(0x9c),                      // 8-bit ST
    }
}

fn gen_token(rng: &mut Rng, out: &mut Vec<u8>) {
    match rng.below(20) {
        // Plain ASCII text runs.
        0..=4 => {
            let len = 1 + rng.below(60);
            for _ in 0..len {
                out.push((0x20 + rng.below(0x5f)) as u8);
            }
        }
        // Unicode runs (wide, combining, ZWJ).
        5..=6 => {
            let len = 1 + rng.below(20);
            for _ in 0..len {
                out.extend_from_slice(rng.pick(UNICODE_POOL).as_bytes());
            }
        }
        // Control characters.
        7..=8 => {
            let len = 1 + rng.below(8);
            for _ in 0..len {
                out.push(*rng.pick(b"\r\n\t\x08\x07\x0b\x0c\x00\x7f"));
            }
        }
        // CSI sequences.
        9..=12 => gen_csi(rng, out),
        // OSC sequences.
        13 => gen_osc(rng, out),
        // DCS / other string introducers.
        14 => {
            let intro: &[u8] = *rng.pick(&[b"\x1bP", b"\x1b^", b"\x1b_", b"\x1bX"]);
            out.extend_from_slice(intro);
            let len = rng.below(30);
            for _ in 0..len {
                out.push((rng.below(256)) as u8);
            }
            if rng.chance(2, 3) {
                out.extend_from_slice(b"\x1b\\");
            }
        }
        // Bare ESC + one byte (RIS, IND, NEL, DECSC/DECRC, charset...).
        15 => {
            out.push(0x1b);
            out.push((rng.below(256)) as u8);
        }
        // Synchronized Output open/close, sometimes unbalanced.
        16 => {
            out.extend_from_slice(b"\x1b[?2026h");
            let len = rng.below(120);
            for _ in 0..len {
                out.push((rng.below(256)) as u8);
            }
            if rng.chance(3, 4) {
                out.extend_from_slice(b"\x1b[?2026l");
            }
        }
        // Fixture splice: a random window of real agent-CLI startup bytes.
        17 => {
            let start = rng.below(FIXTURE.len() as u64) as usize;
            let len = rng.below(300) as usize;
            let end = (start + len).min(FIXTURE.len());
            out.extend_from_slice(&FIXTURE[start..end]);
        }
        // Raw noise, full byte range (invalid UTF-8, C1 controls).
        _ => {
            let len = 1 + rng.below(40);
            for _ in 0..len {
                out.push(rng.below(256) as u8);
            }
        }
    }
}

// `write!` needs a fmt::Write target; Vec<u8> only has io::Write in
// std. The generator only ever writes ASCII digits, so reuse a small
// shim instead of pulling in a dependency.
fn out_as_string(out: &mut Vec<u8>) -> AsciiSink<'_> {
    AsciiSink(out)
}

struct AsciiSink<'a>(&'a mut Vec<u8>);

impl std::fmt::Write for AsciiSink<'_> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

fn gen_case(seed: u64, byte_budget: usize) -> Case {
    let mut rng = Rng::new(seed);
    let rows = *rng.pick(DIMS);
    let cols = *rng.pick(DIMS);
    let scrollback = *rng.pick(&[0usize, 1, 16, 512, 8192]);
    let mut ops = Vec::new();
    let mut bytes_emitted = 0usize;
    while bytes_emitted < byte_budget {
        match rng.below(12) {
            0 => ops.push(Op::Resize(*rng.pick(DIMS), *rng.pick(DIMS))),
            1 => ops.push(Op::Render),
            2 => ops.push(Op::RenderCells),
            3 => ops.push(Op::GridText),
            4 => ops.push(Op::Flush),
            5 => ops.push(Op::SetViewport(rng.below(260) as usize)),
            _ => {
                // A feed of 1..~8 tokens, so escape sequences regularly
                // split across ingest_bytes calls.
                let mut buf = Vec::new();
                let tokens = 1 + rng.below(8);
                for _ in 0..tokens {
                    gen_token(&mut rng, &mut buf);
                }
                // Sometimes shear the feed mid-sequence into two ops.
                if buf.len() > 2 && rng.chance(1, 3) {
                    let cut = 1 + rng.below(buf.len() as u64 - 1) as usize;
                    let tail = buf.split_off(cut);
                    bytes_emitted += buf.len() + tail.len();
                    ops.push(Op::Feed(buf));
                    ops.push(Op::Feed(tail));
                } else {
                    bytes_emitted += buf.len();
                    ops.push(Op::Feed(buf));
                }
            }
        }
    }
    Case { rows, cols, scrollback, ops }
}

// ------------------------------------------------------ minimization

fn panics(case: &Case) -> Option<String> {
    let result = panic::catch_unwind(AssertUnwindSafe(|| run_case(case)));
    match result {
        Ok(()) => None,
        Err(payload) => Some(
            payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "non-string panic payload".to_string()),
        ),
    }
}

fn minimize(mut case: Case) -> Case {
    // Pass 1: drop whole ops, big strides first.
    let mut stride = case.ops.len().max(1);
    while stride >= 1 {
        let mut i = 0;
        while i < case.ops.len() {
            let mut trial = case.clone();
            let end = (i + stride).min(trial.ops.len());
            trial.ops.drain(i..end);
            if panics(&trial).is_some() {
                case = trial;
            } else {
                i += stride;
            }
        }
        if stride == 1 {
            break;
        }
        stride /= 2;
    }
    // Pass 2: shrink bytes inside each Feed, big chunks first.
    for idx in 0..case.ops.len() {
        let Op::Feed(bytes) = &case.ops[idx] else {
            continue;
        };
        let mut chunk = bytes.len().max(1);
        while chunk >= 1 {
            let mut i = 0;
            loop {
                let Op::Feed(bytes) = &case.ops[idx] else { break };
                if i >= bytes.len() {
                    break;
                }
                let mut trial = case.clone();
                if let Op::Feed(tb) = &mut trial.ops[idx] {
                    let end = (i + chunk).min(tb.len());
                    tb.drain(i..end);
                }
                if panics(&trial).is_some() {
                    case = trial;
                } else {
                    i += chunk;
                }
            }
            if chunk == 1 {
                break;
            }
            chunk /= 2;
        }
    }
    case
}

fn describe(case: &Case) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "rows={} cols={} scrollback={}",
        case.rows, case.cols, case.scrollback
    );
    for op in &case.ops {
        match op {
            Op::Feed(bytes) => {
                let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
                let printable: String = bytes
                    .iter()
                    .map(|&b| {
                        if (0x20..0x7f).contains(&b) {
                            b as char
                        } else {
                            '.'
                        }
                    })
                    .collect();
                let _ = writeln!(s, "feed {hex}  |{printable}|");
            }
            other => {
                let _ = writeln!(s, "{other:?}");
            }
        }
    }
    s
}

// ------------------------------------------------------------- tests

fn env_u64(key: &str, default: u64) -> u64 {
    env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

#[test]
fn vt_ingester_survives_fuzzed_byte_streams() {
    let iters = env_u64("ZMUX_VT_FUZZ_ITERS", 512);
    let base_seed = env_u64("ZMUX_VT_FUZZ_SEED", 0x5EED_2026);
    let byte_budget = env_u64("ZMUX_VT_FUZZ_BYTES", 4096) as usize;

    // Hang watchdog: a fuzz case that loops forever is as much a bug
    // as one that panics, and without this it would pin a core
    // silently until the CI timeout. The fuzz loop bumps `progress`
    // before every case; if it stays unchanged for too long, abort
    // the whole process with the stuck seed so the case is
    // reproducible. (10s is ~3 orders of magnitude above a normal
    // case, so false positives mean a real performance bug anyway.)
    let progress = Arc::new(AtomicU64::new(0));
    {
        let progress = Arc::clone(&progress);
        std::thread::spawn(move || {
            let mut last = progress.load(Ordering::Relaxed);
            let mut stalled = 0u32;
            loop {
                std::thread::sleep(Duration::from_secs(1));
                let now = progress.load(Ordering::Relaxed);
                if now == last {
                    stalled += 1;
                    if stalled >= 10 {
                        eprintln!(
                            "VT fuzz case HUNG (>10s) at seed {now:#x}; \
                             rerun with ZMUX_VT_FUZZ_SEED={now:#x} ZMUX_VT_FUZZ_ITERS=1"
                        );
                        std::process::abort();
                    }
                } else {
                    stalled = 0;
                    last = now;
                }
            }
        });
    }

    // Keep panic spew out of the fuzz loop; restore afterwards so the
    // final report (and other tests) print normally.
    let saved_hook = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));

    let mut failure: Option<(u64, String, Case)> = None;
    for i in 0..iters {
        let seed = base_seed.wrapping_add(i);
        progress.store(seed, Ordering::Relaxed);
        let case = gen_case(seed, byte_budget);
        if let Some(message) = panics(&case) {
            let minimized = minimize(case);
            failure = Some((seed, message, minimized));
            break;
        }
    }

    panic::set_hook(saved_hook);

    if let Some((seed, message, minimized)) = failure {
        let report = describe(&minimized);
        let path = format!(
            "{}/vt-fuzz-crash-{seed:#x}.txt",
            env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".into())
        );
        let _ = std::fs::write(&path, &report);
        panic!(
            "VT fuzz case panicked (seed {seed:#x}): {message}\n\
             minimized repro written to {path}\n{report}"
        );
    }
}
