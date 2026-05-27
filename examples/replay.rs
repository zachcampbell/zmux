// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

// Replay tool: feed raw PTY bytes through TerminalIngest and print the
// resulting screen contents. Reads dumps produced by either:
//   - the ZMUX_PTY_DUMP env var (always-on, all panes), or
//   - `zmux capture <session> <pane> <path>` (per-pane, on demand).
//
// Both paths capture the same byte stream — exactly what the terminal
// ingester would see — so the replay is bit-identical to live rendering
// and lets us bisect VT bugs offline.
//
// Usage: cargo run --example replay -- <capture.bin> [rows] [cols]

use std::env;
use std::fs;
use std::io;

use zmux::Pane;
use zmux::PtySize;
use zmux::terminal::TerminalIngest;

fn main() -> io::Result<()> {
    let path = env::args()
        .nth(1)
        .expect("usage: replay <capture.bin> [rows] [cols]");
    let rows: u16 = env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(40);
    let cols: u16 = env::args()
        .nth(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(177);

    let bytes = fs::read(&path)?;
    let size = PtySize::new(rows, cols);
    let mut pane = Pane::new("replay", 8_192, rows as usize);
    let mut ingest = TerminalIngest::new(size);

    ingest.ingest_bytes(&mut pane, &bytes);

    eprintln!(
        "replayed {} bytes from {path}; mode={:?}",
        bytes.len(),
        pane.screen_mode()
    );
    let lines = ingest.render_lines(&pane);
    for (i, line) in lines.iter().enumerate() {
        let plain: String = strip_escapes(line);
        println!("{:>3}: {}", i, plain);
    }
    // Dump the raw bytes of one row so we can inspect ANSI transitions.
    if let Some(row) = lines.get(13) {
        eprintln!("--- row 13 raw bytes ---");
        for chunk in row.as_bytes().chunks(32) {
            eprint!("  ");
            for byte in chunk {
                eprint!("{:02X} ", byte);
            }
            eprintln!();
        }
    }
    Ok(())
}

fn strip_escapes(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            let mut j = i + 2;
            while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                j += 1;
            }
            i = j + 1;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}
