// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

// Regression tests for xterm-parity render bugs found in a 2026-07
// audit of the VT interpreter and compositor. Each test began life as
// a failing repro against the shipped behavior; the "Suspect N" labels
// map to the audit's findings:
//   1: wide-char pair integrity on overwrite / range edits
//   2: selection coordinates vs the actually-rendered window
//   3: IL/DL confinement to the DECSTBM scroll region
//   4: BCE (background color erase) on the primary screen
//   5: DECSTBM oversized-bottom clamping
//   6: alt-screen clear timing for modes 47 / 1047 / 1049

use zmux::pane::Pane;
use zmux::pty::PtySize;
use zmux::terminal::TerminalIngest;

fn plain(lines: &[Vec<zmux::style::Cell>]) -> Vec<String> {
    lines
        .iter()
        .map(|row| {
            let mut s = String::new();
            for c in row {
                if c.ch != '\0' {
                    s.push(c.ch);
                }
            }
            s
        })
        .collect()
}

// Suspect 1a: overwriting the LEFT half of a wide char with a narrow
// char must blank the orphaned continuation cell. Real terminals show
// "a X"; if the orphan '\0' survives, zmux renders "aX" (X shifted
// one column left of where the app put it).
#[test]
fn narrow_over_wide_left_half_primary() {
    let mut pane = Pane::new("t", 64, 4);
    let mut ingest = TerminalIngest::new(PtySize::new(4, 16));
    // 你 at cols 0-1, X at col 2. Then CR + 'a' over 你's left half.
    ingest.ingest_bytes(&mut pane, "你X\ra".as_bytes());
    let lines = ingest.render_lines(&pane);
    // Expected (xterm): "a X" — orphan half blanked, X stays at col 2.
    assert_eq!(lines[0], "a X", "got {:?}", lines[0]);
}

// Suspect 1b: same on the alternate screen.
#[test]
fn narrow_over_wide_left_half_alt() {
    let mut pane = Pane::new("t", 64, 4);
    let mut ingest = TerminalIngest::new(PtySize::new(4, 16));
    ingest.ingest_bytes(&mut pane, "\x1b[?1049h\x1b[H你X\ra".as_bytes());
    let lines = ingest.render_lines(&pane);
    assert_eq!(lines[0], "a X", "got {:?}", lines[0]);
}

// Suspect 1c: overwriting the RIGHT half (continuation cell) must blank
// the wide base too, otherwise the base still renders 2 cols wide and
// pushes everything right.
#[test]
fn narrow_over_wide_right_half_primary() {
    let mut pane = Pane::new("t", 64, 4);
    let mut ingest = TerminalIngest::new(PtySize::new(4, 16));
    // 你 at 0-1, X at 2; then CUP col 2 (1-based: col 2 = index 1) 'a'
    ingest.ingest_bytes(&mut pane, "你X\r\x1b[2Ga".as_bytes());
    let lines = ingest.render_lines(&pane);
    // Expected (xterm): " aX" — base blanked, a at col 1, X at col 2.
    assert_eq!(lines[0], " aX", "got {:?}", lines[0]);
}

// Suspect 1d: DCH deleting the left half of a wide char must blank the
// continuation that shifts into its place, and DCH landing on the
// right half must blank the base it leaves behind.
#[test]
fn dch_splitting_wide_pair_blanks_orphans() {
    // Cursor on the base: delete it. The continuation shifts left into
    // the cursor cell and must be blanked -> " X".
    let mut pane = Pane::new("t", 64, 4);
    let mut ingest = TerminalIngest::new(PtySize::new(4, 16));
    ingest.ingest_bytes(&mut pane, "你X\r\x1b[1P".as_bytes());
    let lines = ingest.render_lines(&pane);
    assert_eq!(lines[0], " X", "delete-left-half got {:?}", lines[0]);

    // Cursor on the continuation: delete it. The base survives to the
    // left and must be blanked -> " X".
    let mut pane = Pane::new("t", 64, 4);
    let mut ingest = TerminalIngest::new(PtySize::new(4, 16));
    ingest.ingest_bytes(&mut pane, "你X\r\x1b[2G\x1b[1P".as_bytes());
    let lines = ingest.render_lines(&pane);
    assert_eq!(lines[0], " X", "delete-right-half got {:?}", lines[0]);
}

// Suspect 1e: ICH inserting between the two halves of a wide char
// splits the pair; both halves must blank.
#[test]
fn ich_splitting_wide_pair_blanks_both_halves() {
    let mut pane = Pane::new("t", 64, 4);
    let mut ingest = TerminalIngest::new(PtySize::new(4, 16));
    // 你 at 0-1, X at 2; cursor to col 2 (on the continuation), insert
    // one blank cell. xterm: "  ", blank, then X -> "   X".
    ingest.ingest_bytes(&mut pane, "你X\r\x1b[2G\x1b[1@".as_bytes());
    let lines = ingest.render_lines(&pane);
    assert_eq!(lines[0], "   X", "cols 0-2 must all blank");
    let cells = &ingest.render_cells(&pane)[0];
    assert_eq!(
        cells.iter().position(|c| c.ch == 'X'),
        Some(3),
        "X must stay at visual col 3, got {:?}",
        cells.iter().map(|c| c.ch).collect::<String>()
    );
}

// Suspect 1f: ECH starting on the right half of a wide char must blank
// the base to its left.
#[test]
fn ech_on_wide_right_half_blanks_base() {
    let mut pane = Pane::new("t", 64, 4);
    let mut ingest = TerminalIngest::new(PtySize::new(4, 16));
    // 你 at 0-1, X at 2; cursor to col 2, erase 1 cell.
    ingest.ingest_bytes(&mut pane, "你X\r\x1b[2G\x1b[1X".as_bytes());
    let lines = ingest.render_lines(&pane);
    assert_eq!(lines[0], "  X", "got {:?}", lines[0]);
}

// Suspect 1g: same class on the alternate screen — DCH landing on the
// continuation cell must blank the surviving base.
#[test]
fn alt_dch_on_wide_right_half_blanks_base() {
    let mut pane = Pane::new("t", 64, 4);
    let mut ingest = TerminalIngest::new(PtySize::new(4, 16));
    ingest.ingest_bytes(&mut pane, "\x1b[?1049h\x1b[H你X\x1b[2G\x1b[1P".as_bytes());
    let lines = ingest.render_lines(&pane);
    assert_eq!(lines[0].trim_end(), " X", "got {:?}", lines[0]);
}

// Suspect 2: after ED2 ("clear" without 3J) with scrollback present,
// follow-mode render is top-aligned but scrollback_viewport_top()
// claims a bottom-anchored window -> mouse selection/highlight
// coordinates point at the wrong lines.
#[test]
fn viewport_top_matches_rendered_rows_after_ed2() {
    let mut pane = Pane::new("t", 256, 8); // viewport height 8
    let mut ingest = TerminalIngest::new(PtySize::new(8, 32));
    // 30 lines -> 22 in scrollback, 8 live in the grid.
    let mut bytes = Vec::new();
    for i in 0..30 {
        bytes.extend_from_slice(format!("line-{i}\r\n").as_bytes());
    }
    ingest.ingest_bytes(&mut pane, &bytes);
    // ED2: clear the grid but not scrollback.
    ingest.ingest_bytes(&mut pane, b"\x1b[H\x1b[2Jprompt$ ");
    assert!(pane.follow_output());

    let rendered = plain(&ingest.render_cells(&pane));
    let top = ingest.rendered_viewport_origin(&pane);
    // Selection code maps screen rows to combined lines through
    // `rendered_viewport_origin`; rendered row 0 must show exactly the
    // combined line that origin names, in every render mode.
    let combined_at_top = ingest.combined_line_cells(&pane, top);
    let mut combined_text = String::new();
    for c in &combined_at_top {
        if c.ch != '\0' {
            combined_text.push(c.ch);
        }
    }
    assert_eq!(
        rendered[0].trim_end(),
        combined_text.trim_end(),
        "rendered row 0 = {:?} but combined_line_cells(origin={}) = {:?}",
        rendered[0],
        top,
        combined_text
    );
}

// Suspect 3: IL/DL on the primary screen must respect the DECSTBM
// region. Rows below the region's bottom margin must not move.
#[test]
fn primary_dl_respects_scroll_region() {
    let mut pane = Pane::new("t", 256, 6);
    let mut ingest = TerminalIngest::new(PtySize::new(6, 32));
    // Fill 6 rows: r0..r5.
    ingest.ingest_bytes(&mut pane, b"r0\r\nr1\r\nr2\r\nr3\r\nr4\r\nr5");
    // Region rows 1-4 (1-based 2..5). Cursor to row 2, delete 1 line.
    ingest.ingest_bytes(&mut pane, b"\x1b[2;5r\x1b[2;1H\x1b[M");
    // Reset region for rendering sanity.
    ingest.ingest_bytes(&mut pane, b"\x1b[r");
    let lines = ingest.render_lines(&pane);
    // xterm: r1 deleted, r2..r4 shift up inside region, blank at row 4,
    // r5 (below region) unmoved: r0, r2, r3, r4, <blank>, r5.
    assert_eq!(
        lines
            .iter()
            .map(|l| l.trim_end().to_string())
            .collect::<Vec<_>>(),
        vec!["r0", "r2", "r3", "r4", "", "r5"],
        "got {:?}",
        lines
    );
}

#[test]
fn primary_il_respects_scroll_region() {
    let mut pane = Pane::new("t", 256, 6);
    let mut ingest = TerminalIngest::new(PtySize::new(6, 32));
    ingest.ingest_bytes(&mut pane, b"r0\r\nr1\r\nr2\r\nr3\r\nr4\r\nr5");
    // Region rows 2..5 (1-based), cursor row 2, insert 1 line.
    ingest.ingest_bytes(&mut pane, b"\x1b[2;5r\x1b[2;1H\x1b[L");
    ingest.ingest_bytes(&mut pane, b"\x1b[r");
    let lines = ingest.render_lines(&pane);
    // xterm: blank inserted at row 1, r1..r3 shift down, r4 falls out
    // of the region, r5 unmoved: r0, <blank>, r1, r2, r3, r5.
    assert_eq!(
        lines
            .iter()
            .map(|l| l.trim_end().to_string())
            .collect::<Vec<_>>(),
        vec!["r0", "", "r1", "r2", "r3", "r5"],
        "got {:?}",
        lines
    );
}

// Suspect 6a: mode 47 is a plain buffer switch — xterm never clears
// the alt screen for it, so content drawn in a previous alt session
// survives exit + re-entry (the entering app repaints or deliberately
// reuses the old frame).
#[test]
fn mode_47_reenters_alt_screen_without_clearing() {
    let mut pane = Pane::new("t", 64, 4);
    let mut ingest = TerminalIngest::new(PtySize::new(4, 16));
    ingest.ingest_bytes(&mut pane, b"\x1b[?47h\x1b[HKEEP");
    ingest.ingest_bytes(&mut pane, b"\x1b[?47l");
    ingest.ingest_bytes(&mut pane, b"\x1b[?47h");
    let lines = ingest.render_lines(&pane);
    assert_eq!(
        lines.first().map(|l| l.trim_end()),
        Some("KEEP"),
        "47h must not clear the alt screen, got {lines:?}"
    );
}

// Suspect 6b: mode 1047 clears the alt screen on EXIT (not entry), so
// stale content can't leak into the next plain 47h session.
#[test]
fn mode_1047_clears_alt_screen_on_exit() {
    let mut pane = Pane::new("t", 64, 4);
    let mut ingest = TerminalIngest::new(PtySize::new(4, 16));
    ingest.ingest_bytes(&mut pane, b"\x1b[?1047h\x1b[HGONE");
    ingest.ingest_bytes(&mut pane, b"\x1b[?1047l");
    ingest.ingest_bytes(&mut pane, b"\x1b[?47h");
    let lines = ingest.render_lines(&pane);
    assert!(
        lines.iter().all(|l| l.trim_end().is_empty()),
        "1047l must clear the alt screen on exit, got {lines:?}"
    );
}

// Suspect 6c (pin): 1049 keeps clearing on entry — that part of the
// old behavior was already xterm-correct.
#[test]
fn mode_1049_clears_alt_screen_on_entry() {
    let mut pane = Pane::new("t", 64, 4);
    let mut ingest = TerminalIngest::new(PtySize::new(4, 16));
    ingest.ingest_bytes(&mut pane, b"\x1b[?47h\x1b[HSTALE\x1b[?47l");
    ingest.ingest_bytes(&mut pane, b"\x1b[?1049h");
    let lines = ingest.render_lines(&pane);
    assert!(
        lines.iter().all(|l| l.trim_end().is_empty()),
        "1049h must clear the alt screen on entry, got {lines:?}"
    );
}

// Suspect 4: EL2 with a colored background must paint the row with
// that background (BCE), not leave default-styled blanks.
#[test]
fn primary_el2_keeps_background_color() {
    let mut pane = Pane::new("t", 64, 4);
    let mut ingest = TerminalIngest::new(PtySize::new(4, 8));
    ingest.ingest_bytes(&mut pane, b"hello");
    // Red background, erase whole line.
    ingest.ingest_bytes(&mut pane, b"\x1b[41m\x1b[2K");
    let cells = ingest.render_cells(&pane);
    let has_red_bg = cells
        .first()
        .map(|row| {
            !row.is_empty()
                && row
                    .iter()
                    .all(|c| c.style.bg == zmux::style::Color::Indexed(1))
        })
        .unwrap_or(false);
    assert!(
        has_red_bg,
        "EL2 should leave red-bg blanks, got row {:?}",
        cells.first().map(|r| r.len())
    );
}

// Suspect 4b: EL 0 with a colored background must paint cursor→eol
// even past the stored end of the line (implicit cells).
#[test]
fn primary_el0_paints_background_past_line_end() {
    let mut pane = Pane::new("t", 64, 4);
    let mut ingest = TerminalIngest::new(PtySize::new(4, 8));
    // "hi" leaves the cursor at col 2, past the stored tail.
    ingest.ingest_bytes(&mut pane, b"hi\x1b[41m\x1b[K");
    let cells = ingest.render_cells(&pane);
    let row = &cells[0];
    assert!(
        row.len() >= 8
            && row[2..8]
                .iter()
                .all(|c| c.style.bg == zmux::style::Color::Indexed(1)),
        "EL0 must paint red-bg blanks to eol, got len {} styles {:?}",
        row.len(),
        row.iter().map(|c| c.style.bg).collect::<Vec<_>>()
    );
}

// Suspect 4c: ED 0 with a colored background must paint every row
// below the cursor, and ED 2 must paint the whole screen.
#[test]
fn primary_ed_paints_background_rows() {
    let mut pane = Pane::new("t", 64, 4);
    let mut ingest = TerminalIngest::new(PtySize::new(4, 8));
    ingest.ingest_bytes(&mut pane, b"a\r\nb\r\nc\r\nd\x1b[H\x1b[41m\x1b[J");
    let cells = ingest.render_cells(&pane);
    assert_eq!(cells.len(), 4);
    for (index, row) in cells.iter().enumerate().skip(1) {
        assert!(
            !row.is_empty()
                && row
                    .iter()
                    .all(|c| c.style.bg == zmux::style::Color::Indexed(1)),
            "ED0 must paint row {index} red, got {:?}",
            row.iter().map(|c| c.style.bg).collect::<Vec<_>>()
        );
    }

    let mut pane = Pane::new("t", 64, 4);
    let mut ingest = TerminalIngest::new(PtySize::new(4, 8));
    ingest.ingest_bytes(&mut pane, b"a\r\nb\x1b[44m\x1b[2J");
    let cells = ingest.render_cells(&pane);
    assert_eq!(cells.len(), 4);
    for (index, row) in cells.iter().enumerate() {
        assert!(
            !row.is_empty()
                && row
                    .iter()
                    .all(|c| c.style.bg == zmux::style::Color::Indexed(4)),
            "ED2 must paint row {index} blue, got {:?}",
            row.iter().map(|c| c.style.bg).collect::<Vec<_>>()
        );
    }
}

// Suspect 5b: the alt screen must clamp an oversized DECSTBM bottom
// the same way.
#[test]
fn alt_decstbm_clamps_oversized_bottom() {
    let mut pane = Pane::new("t", 256, 6);
    let mut ingest = TerminalIngest::new(PtySize::new(6, 32));
    ingest.ingest_bytes(
        &mut pane,
        b"\x1b[?1049h\x1b[Hr0\r\nr1\r\nr2\r\nr3\r\nr4\r\nr5",
    );
    ingest.ingest_bytes(&mut pane, b"\x1b[3;999r\x1b[1S\x1b[r");
    let lines = ingest.render_lines(&pane);
    assert_eq!(
        lines
            .iter()
            .map(|l| l.trim_end().to_string())
            .collect::<Vec<_>>(),
        vec!["r0", "r1", "r3", "r4", "r5"],
        "got {:?}",
        lines
    );
}

// Suspect 5: `CSI 1;999r` (bottom beyond screen) should clamp, not be
// ignored. xterm clamps the bottom margin to the last row.
#[test]
fn decstbm_clamps_oversized_bottom() {
    let mut pane = Pane::new("t", 256, 6);
    let mut ingest = TerminalIngest::new(PtySize::new(6, 32));
    ingest.ingest_bytes(&mut pane, b"r0\r\nr1\r\nr2\r\nr3\r\nr4\r\nr5");
    // Set region rows 3..999 -> xterm clamps to 3..6. Then SU 1 within
    // the region: r2 removed (rows 3..6 shift up), r0/r1 untouched.
    ingest.ingest_bytes(&mut pane, b"\x1b[3;999r\x1b[1S\x1b[r");
    let lines = ingest.render_lines(&pane);
    assert_eq!(
        lines
            .iter()
            .map(|l| l.trim_end().to_string())
            .collect::<Vec<_>>(),
        vec!["r0", "r1", "r3", "r4", "r5"],
        "got {:?}",
        lines
    );
}
