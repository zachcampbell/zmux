// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Cell-stamping primitives and the pane-number glyph table extracted
//! from `workspace.rs`. Pure functions over `&mut [Vec<Cell>]` frames;
//! no `Workspace` state.

use crate::layout::PaneLayout;
use crate::style::{Cell, Style, char_width, sever_wide_pair};

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

pub(crate) fn stamp_row_text(row: &mut [Cell], start: usize, end: usize, text: &str, style: Style) {
    let mut cursor = start;
    let limit = end.min(row.len());
    let mut last_base: Option<usize> = None;
    let mut wrote = false;
    for ch in text.chars() {
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
        let width = char_width(safe_ch);
        if width == 0 {
            if let Some(index) = last_base {
                row[index].append_suffix(safe_ch);
            }
            continue;
        }
        if last_base.is_some_and(|index| row[index].suffix_ends_with_joiner()) {
            if let Some(index) = last_base {
                row[index].append_suffix(safe_ch);
            }
            continue;
        }
        if cursor >= limit || cursor.saturating_add(width) > limit {
            break;
        }
        // First write: the stamp's left edge can land on the trailing
        // half of a wide char already in the frame (pane content under
        // an overlay) — blank the surviving base so the serialized row
        // keeps one cell per column.
        if !wrote {
            sever_wide_pair(row, cursor);
            wrote = true;
        }
        row[cursor] = Cell::styled(safe_ch, style.clone());
        last_base = Some(cursor);
        if width == 2 {
            row[cursor + 1] = Cell::styled('\0', style.clone());
        }
        cursor += width;
    }
    // Right edge: the cell just past the stamped span can be the
    // orphaned continuation of a wide char whose base we overwrote.
    if wrote {
        sever_wide_pair(row, cursor);
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
        // A wide base in the last writable column has nowhere to put
        // its continuation — copied as-is it would render two columns
        // wide, shoving the border / neighboring pane content one cell
        // to the right. Blank it instead, mirroring the edge rule
        // `clip_row_to_width` applies inside the terminal.
        let next = cursor + 1;
        let continuation_fits = next < row.len() && next < max;
        if !continuation_fits && cell.ch != '\0' && char_width(cell.ch) == 2 {
            row[cursor] = Cell::styled(' ', cell.style.clone());
        } else {
            row[cursor] = cell.clone();
        }
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
                    // Glyph pixels land on arbitrary cells over live
                    // pane content; blank the other half of any wide
                    // pair this single-cell write splits.
                    sever_wide_pair(row, col);
                    sever_wide_pair(row, col + 1);
                    row[col] = Cell::styled(ch, style.clone());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::stamp_row_text;
    use crate::style::{Cell, Style, serialize_row};

    #[test]
    fn stamp_cells_blanks_a_wide_base_cut_from_its_continuation() {
        use super::stamp_cells;
        // Pane row "A你" stamped into a 2-column region: the wide base
        // lands in the region's last column and its continuation is cut
        // off. Copying the base as-is would render it two columns wide,
        // shoving the border/neighbor content one cell right.
        let mut frame = vec![vec![Cell::BLANK; 4]];
        let cells = vec![Cell::new('A'), Cell::new('你'), Cell::new('\0')];
        stamp_cells(&mut frame, 0, 0, 2, &cells);
        assert_eq!(serialize_row(&frame[0]), "A   ");

        // Same cut at the frame row's right edge instead of the
        // region's.
        let mut frame = vec![vec![Cell::BLANK; 4]];
        let cells = vec![Cell::new('你'), Cell::new('\0')];
        stamp_cells(&mut frame, 3, 0, 8, &cells);
        assert_eq!(serialize_row(&frame[0]), "    ");
    }

    #[test]
    fn stamping_over_half_a_wide_char_blanks_the_other_half() {
        use crate::style::char_width;
        // Frame row holds 你 (cols 0-1) then X at col 2. Stamping "!"
        // over the continuation cell must blank the base, otherwise the
        // serialized row renders one column too wide.
        let mut row = vec![Cell::BLANK; 4];
        stamp_row_text(&mut row, 0, 4, "你X", Style::DEFAULT);
        stamp_row_text(&mut row, 1, 2, "!", Style::DEFAULT);
        assert_eq!(serialize_row(&row), " !X ");

        // And stamping over the base must blank the orphaned
        // continuation, otherwise the row renders one column short.
        let mut row = vec![Cell::BLANK; 4];
        stamp_row_text(&mut row, 0, 4, "你X", Style::DEFAULT);
        stamp_row_text(&mut row, 0, 1, "!", Style::DEFAULT);
        assert_eq!(serialize_row(&row), "! X ");
        assert!(
            row.iter()
                .all(|cell| char_width(cell.ch) != 0 || cell.ch != '\0')
        );
    }

    #[test]
    fn stamped_chrome_uses_terminal_cell_width() {
        let mut row = vec![Cell::BLANK; 4];
        stamp_row_text(&mut row, 0, 4, "A你B", Style::DEFAULT);

        assert_eq!(serialize_row(&row), "A你B");
        assert_eq!(row[0].ch, 'A');
        assert_eq!(row[1].ch, '你');
        assert_eq!(row[2].ch, '\0');
        assert_eq!(row[3].ch, 'B');
    }
}
