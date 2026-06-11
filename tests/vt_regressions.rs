// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

// Regression fixtures for crashes and hangs found by the VT fuzz
// harness (tests/vt_fuzz.rs). Each test is a minimized hostile input
// that previously panicked or spun the ingester; the assertion is
// that ingestion completes and the terminal stays usable.

use zmux::terminal::TerminalIngest;
use zmux::{Pane, PtySize};

fn ingest(rows: u16, cols: u16, bytes: &[u8]) -> (TerminalIngest, Pane) {
    let mut pane = Pane::new("regression", 512, rows as usize);
    let mut ingest = TerminalIngest::new(PtySize::new(rows, cols));
    ingest.ingest_bytes(&mut pane, bytes);
    // Exercise the render + flush paths too — several historical
    // crashes only surfaced when reading the grid back out.
    let _ = ingest.render_cells(&pane);
    let _ = ingest.render_lines(&pane);
    (ingest, pane)
}

// Found by fuzz seed 0x5eed2027: constructing a pane with 0 rows hit
// the `viewport height must be greater than zero` assert in
// ScrollbackBuffer. A layout split on a tiny terminal can legitimately
// hand a pane 0 rows, so the buffer now clamps to 1 instead.
#[test]
fn zero_row_pane_does_not_panic() {
    let (_, pane) = ingest(0, 3, b"hello\r\nworld\r\n");
    let _ = pane.visible_text();
}

#[test]
fn zero_size_pane_with_zero_scrollback_does_not_panic() {
    let mut pane = Pane::new("regression", 0, 0);
    let mut ingest = TerminalIngest::new(PtySize::new(0, 0));
    ingest.ingest_bytes(&mut pane, b"text\x1b[2Jmore\r\n");
    pane.set_viewport_height(0);
    let _ = ingest.render_lines(&pane);
}

// Found by the first fuzz run as a HANG, not a panic: CSI `b` (REP)
// looped over the raw parameter, so `CSI 18446744073709551615 b` spun
// for centuries at 100% CPU. parse_params now clamps every CSI
// parameter to 65535 (as xterm does). If this test takes more than a
// blink, the clamp regressed.
#[test]
fn rep_with_u64_max_count_terminates() {
    let (_, _) = ingest(4, 8, b"x\x1b[18446744073709551615b");
    let (_, _) = ingest(4, 8, b"x\x1b[4294967295b");
}

// Same hang class in the alternate screen: CNL/CPL (CSI E / CSI F)
// executed one linefeed per count. Capped at 2x rows — beyond that
// every additional linefeed leaves the screen unchanged.
#[test]
fn alt_screen_next_prev_line_with_huge_counts_terminate() {
    let (_, _) = ingest(4, 8, b"\x1b[?1049h\x1b[4294967295E\x1b[4294967295F");
}

// Overflow class: primary-screen CUD/CUF/CNL computed `cursor + count`
// with the raw parameter — with overflow checks on (every dev build),
// `CSI 18446744073709551615 B` panicked on the add once the cursor was
// past row 0. The parse-time clamp bounds the operand.
#[test]
fn primary_cursor_moves_with_huge_counts_do_not_overflow() {
    let (_, _) = ingest(
        4,
        8,
        b"a\r\n\x1b[18446744073709551615B\x1b[18446744073709551615C\
          \x1b[18446744073709551615E\x1b[18446744073709551615X\
          \x1b[18446744073709551615P\x1b[18446744073709551615@",
    );
}

// Sign-wrap class: alternate-screen CUU/CUD/CUF/CUB cast the raw count
// with `as isize`, so a count of 2^63 flipped negative and moved the
// cursor the wrong direction. With the clamp the cast is always
// positive; this fixture pins the behavioral half (cursor ends up
// clamped at the screen edge, not warped back to the origin side).
#[test]
fn alt_screen_cursor_moves_with_huge_counts_clamp_to_edges() {
    let (ingest, pane) = ingest(
        4,
        8,
        b"\x1b[?1049h\x1b[H\x1b[9223372036854775808Bok",
    );
    // Cursor must have moved DOWN to the last row (clamped), so "ok"
    // renders on row 3 — not row 0, which is where a negative
    // sign-wrapped move would have left it.
    let lines = ingest.render_lines(&pane);
    assert!(
        lines.last().is_some_and(|l| l.contains("ok")),
        "expected 'ok' on the bottom row, got {lines:?}"
    );
}

// Found by fuzz seed 0x5eed221c, minimized to `X CSI 6 E CSI L`: the
// primary grid grows lazily, so CNL can park the cursor past the
// grid's tail; IL then called `Vec::insert(cursor_row, ..)` with an
// index beyond len and panicked. IL past the tail is now a no-op
// (rows there are implicitly blank — there is nothing to shift).
#[test]
fn insert_line_with_cursor_beyond_grid_tail_does_not_panic() {
    let (_, _) = ingest(80, 4, b"X\x1b[6E\x1b[L");
    // Same shape via CUP + DL/IL interleaving for good measure.
    let (_, _) = ingest(24, 80, b"\x1b[20;1Hx\x1b[5;1H\x1b[24d\x1b[3L\x1b[3M");
}

// DECSTBM with degenerate and out-of-range margins, then scroll ops —
// exercises the scroll-region clamps on both screens.
#[test]
fn hostile_scroll_regions_do_not_panic() {
    let (_, _) = ingest(
        4,
        8,
        b"\x1b[65535;1r\x1b[0;0r\x1b[2;65535r\x1b[65535S\x1b[65535T\
          \x1b[?1049h\x1b[65535;1r\x1b[65535S\x1b[65535T",
    );
}
