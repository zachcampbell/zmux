// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Cell-stamping primitives and the pane-number glyph table extracted
//! from `workspace.rs`. Pure functions over `&mut [Vec<Cell>]` frames;
//! no `Workspace` state.

use crate::layout::PaneLayout;
use crate::style::{Cell, Style};

pub(crate) fn stamp_text(
    frame: &mut [Vec<Cell>],
    x: u16,
    y: u16,
    width: u16,
    text: &str,
    style: Style,
) {
    let Some(row) = frame.get_mut(y as usize) else {
        return;
    };
    stamp_row_text(
        row,
        x as usize,
        (x.saturating_add(width)) as usize,
        text,
        style,
    );
}

// Explicit counter beats `.zip(start..)` here because we need to
// short-circuit on two independent bounds (row.len() and end) per
// iteration; the .zip form would require an awkward take_while.
#[allow(clippy::explicit_counter_loop)]
pub(crate) fn stamp_row_text(row: &mut [Cell], start: usize, end: usize, text: &str, style: Style) {
    let mut cursor = start;
    for ch in text.chars() {
        if cursor >= row.len() || cursor >= end {
            break;
        }
        // Defense in depth: every real pane's PTY output is parsed by
        // the VT100 state machine in terminal.rs before it ever becomes
        // a Cell, which strips control chars out of the printable
        // stream. Text stamped here (status bar label/clock, pane
        // headers, rename/command-prompt overlays) bypasses that parser
        // entirely and writes straight into cells - so a control char
        // that reaches this function (an ESC starting a raw escape
        // sequence, a NUL, etc.) would otherwise ride straight through
        // to `style::serialize_row` and out to a client's real
        // terminal. Callers are expected to validate their inputs
        // upstream too (see validate_session_name), but replacing
        // control chars with a space here means no current or future
        // text source reaching this shared choke point can inject
        // escape sequences into another client's screen.
        let safe_ch = if ch.is_control() { ' ' } else { ch };
        row[cursor] = Cell::styled(safe_ch, style.clone());
        cursor += 1;
    }
}

#[allow(clippy::explicit_counter_loop)]
pub(crate) fn stamp_cells(frame: &mut [Vec<Cell>], x: u16, y: u16, width: u16, cells: &[Cell]) {
    let Some(row) = frame.get_mut(y as usize) else {
        return;
    };

    let mut cursor = x as usize;
    let max = x.saturating_add(width) as usize;
    for cell in cells {
        if cursor >= row.len() || cursor >= max {
            break;
        }
        row[cursor] = cell.clone();
        cursor += 1;
    }
}

// 5-row by 3-column ASCII glyphs for the pane-numbers overlay. Each
// digit is a block of '#' pixels on a space background; the caller
// picks a center point and stamps them with `draw_big_digits`.
const BIG_DIGIT_ROWS: usize = 5;
const BIG_DIGIT_COLS: usize = 3;

const BIG_DIGITS: [[&str; BIG_DIGIT_ROWS]; 10] = [
    [
        "###", //
        "# #", //
        "# #", //
        "# #", //
        "###", //
    ],
    [
        "  #", //
        " ##", //
        "  #", //
        "  #", //
        "  #", //
    ],
    [
        "###", //
        "  #", //
        "###", //
        "#  ", //
        "###", //
    ],
    [
        "###", //
        "  #", //
        " ##", //
        "  #", //
        "###", //
    ],
    [
        "# #", //
        "# #", //
        "###", //
        "  #", //
        "  #", //
    ],
    [
        "###", //
        "#  ", //
        "###", //
        "  #", //
        "###", //
    ],
    [
        "###", //
        "#  ", //
        "###", //
        "# #", //
        "###", //
    ],
    [
        "###", //
        "  #", //
        "  #", //
        "  #", //
        "  #", //
    ],
    [
        "###", //
        "# #", //
        "###", //
        "# #", //
        "###", //
    ],
    [
        "###", //
        "# #", //
        "###", //
        "  #", //
        "###", //
    ],
];

// Centers the given numeric label (rendered with BIG_DIGITS) inside the
// pane's content rectangle. Each digit occupies 3 columns + a 1-column
// gap; we gracefully skip panes too small to host the glyph.
pub(crate) fn draw_big_digits(
    frame: &mut [Vec<Cell>],
    pane: &PaneLayout,
    label: &str,
    style: Style,
) {
    let digit_count = label.chars().count();
    if digit_count == 0 {
        return;
    }
    let total_width = digit_count * BIG_DIGIT_COLS + digit_count.saturating_sub(1);
    let pane_width = pane.content.width as usize;
    let pane_height = pane.content.height as usize;
    if pane_width < total_width || pane_height < BIG_DIGIT_ROWS {
        return;
    }

    let origin_x = pane.content.x as usize + (pane_width - total_width) / 2;
    let origin_y = pane.content.y as usize + (pane_height - BIG_DIGIT_ROWS) / 2;

    for (digit_index, ch) in label.chars().enumerate() {
        let Some(digit) = ch.to_digit(10) else {
            continue;
        };
        let glyph = &BIG_DIGITS[digit as usize];
        let glyph_origin = origin_x + digit_index * (BIG_DIGIT_COLS + 1);
        for (row_offset, glyph_row) in glyph.iter().enumerate() {
            let Some(row) = frame.get_mut(origin_y + row_offset) else {
                continue;
            };
            for (col_offset, ch) in glyph_row.chars().enumerate() {
                let col = glyph_origin + col_offset;
                if col < row.len() && ch != ' ' {
                    row[col] = Cell::styled(ch, style.clone());
                }
            }
        }
    }
}
