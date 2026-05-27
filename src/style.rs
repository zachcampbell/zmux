// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::fmt::Write;
use std::sync::Arc;

// Color palette we render. Default means "whatever the terminal thinks is
// the default foreground / background" — emitted as SGR 39 or 49 so the
// user's theme shows through. Indexed is the standard ANSI 0..15 (the
// first 8 + 8 bright variants). Rgb is 24-bit truecolor for programs like
// btop that emit SGR 38;2;r;g;b. 256-color indexed (SGR 38;5;N) gets
// mapped into Rgb so we don't need a third variant.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Color {
    #[default]
    Default,
    Indexed(u8), // 0..=15
    Rgb(u8, u8, u8),
}

// Attribute bits we track. Bold and dim share SGR slots (1, 2) but are
// visually distinct on most terminals — keep them separate. Reverse is
// what htop uses to highlight the selected row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Attrs {
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
}

// Style is no longer Copy because it carries an optional OSC 8 hyperlink
// target, which lives behind an Arc so identical URLs across many cells
// share one allocation. Clones are still cheap — Arc::clone is one
// atomic bump, and None clones are free.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub attrs: Attrs,
    pub hyperlink: Option<Arc<str>>,
}

impl Style {
    pub const DEFAULT: Style = Style {
        fg: Color::Default,
        bg: Color::Default,
        attrs: Attrs {
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            reverse: false,
        },
        hyperlink: None,
    };

    // Applies one SGR numeric parameter to the running style. Returns
    // `consumed_extra`: how many additional params this one ate (for
    // 38;5;N and 38;2;r;g;b forms). Callers pass `remaining` so this
    // method can peek ahead for color payloads.
    pub fn apply_sgr(&mut self, param: u16, remaining: &[u16]) -> usize {
        match param {
            0 => *self = Style::DEFAULT,
            1 => self.attrs.bold = true,
            2 => self.attrs.dim = true,
            3 => self.attrs.italic = true,
            4 => self.attrs.underline = true,
            7 => self.attrs.reverse = true,
            22 => {
                self.attrs.bold = false;
                self.attrs.dim = false;
            }
            23 => self.attrs.italic = false,
            24 => self.attrs.underline = false,
            27 => self.attrs.reverse = false,
            30..=37 => self.fg = Color::Indexed((param - 30) as u8),
            38 => return apply_extended_color(&mut self.fg, remaining),
            39 => self.fg = Color::Default,
            40..=47 => self.bg = Color::Indexed((param - 40) as u8),
            48 => return apply_extended_color(&mut self.bg, remaining),
            49 => self.bg = Color::Default,
            90..=97 => self.fg = Color::Indexed((param - 90 + 8) as u8),
            100..=107 => self.bg = Color::Indexed((param - 100 + 8) as u8),
            _ => {} // ignore unknown SGR params rather than panicking
        }
        0
    }
}

fn apply_extended_color(slot: &mut Color, remaining: &[u16]) -> usize {
    match remaining.first().copied() {
        Some(5) => {
            // 256-color indexed: SGR 38;5;N  →  map N into RGB.
            if let Some(&n) = remaining.get(1) {
                *slot = color_from_256(n as u8);
                return 2;
            }
            0
        }
        Some(2) => {
            // Truecolor: SGR 38;2;r;g;b
            if let (Some(&r), Some(&g), Some(&b)) =
                (remaining.get(1), remaining.get(2), remaining.get(3))
            {
                *slot = Color::Rgb(r as u8, g as u8, b as u8);
                return 4;
            }
            0
        }
        _ => 0,
    }
}

// Convert an xterm 256-color palette index to approximate RGB. The first
// 16 slots are the base ANSI colors; 16..232 are the 6x6x6 color cube;
// 232..256 are 24 grayscale steps. Keeping this in-tree saves a libc
// lookup and gives us full fidelity when we re-emit truecolor SGR.
fn color_from_256(n: u8) -> Color {
    if n < 16 {
        return Color::Indexed(n);
    }
    match rgb_from_256(n) {
        Some((r, g, b)) => Color::Rgb(r, g, b),
        None => Color::Indexed(n),
    }
}

// Display width of a single character in terminal cells. 0 for control
// chars and our wide-char continuation sentinel; 2 for East Asian Wide,
// Fullwidth, most emoji, and a few other double-width ranges; 1 for
// everything else. Matches the practical behavior of xterm/wcwidth
// closely enough for CJK text and common emoji without pulling in a
// 100 KB Unicode table.
pub fn char_width(ch: char) -> usize {
    // Our internal sentinel for "this cell is the trailing half of a
    // wide character written to the preceding cell." Zero-width so
    // layout math stays consistent with display.
    if ch == '\0' {
        return 0;
    }
    let c = ch as u32;
    // ASCII fast path.
    if c < 0x7F {
        return 1;
    }
    // Sparse emoji-presentation Dingbats (U+2700..U+27BF). Most chars in
    // this block are single-cell symbols, but a handful render as
    // double-width emoji (✅, ❌, ✔ when in emoji form, …). Listing the
    // specific codepoints keeps the rest at width 1. Sourced from the
    // Unicode Emoji_Presentation property (UCD `emoji-data.txt`):
    // every Dingbat-block codepoint whose Emoji_Presentation = Yes is
    // listed below. ✨ (U+2728) was the original gap that triggered
    // this sweep — agent CLIs use it for tip/feature callouts.
    if (0x2700..=0x27BF).contains(&c) {
        const DINGBAT_EMOJI: &[u32] = &[
            0x2702, // ✂
            0x2705, // ✅
            0x2708, // ✈
            0x2709, // ✉
            0x270A, // ✊
            0x270B, // ✋
            0x270C, // ✌
            0x270D, // ✍
            0x270F, // ✏
            0x2712, // ✒
            0x2714, // ✔
            0x2716, // ✖
            0x271D, // ✝
            0x2721, // ✡
            0x2728, // ✨
            0x2733, // ✳
            0x2734, // ✴
            0x2744, // ❄
            0x2747, // ❇
            0x274C, // ❌
            0x274E, // ❎
            0x2753, // ❓
            0x2754, // ❔
            0x2755, // ❕
            0x2757, // ❗
            0x2763, // ❣
            0x2764, // ❤
            0x2795, // ➕
            0x2796, // ➖
            0x2797, // ➗
            0x27A1, // ➡
            0x27B0, // ➰
            0x27BF, // ➿
        ];
        if DINGBAT_EMOJI.contains(&c) {
            return 2;
        }
    }
    // Must remain sorted by `lo` ascending. The lookup below uses an
    // early `c < lo` break that depends on this ordering.
    const WIDE_RANGES: &[(u32, u32)] = &[
        (0x1100, 0x115F),   // Hangul Jamo
        (0x2329, 0x232A),   // Angle brackets
        (0x2E80, 0x303E),   // CJK Radicals, Kangxi
        (0x3041, 0x33FF),   // Hiragana, Katakana, Bopomofo, CJK strokes
        (0x3400, 0x4DBF),   // CJK Ext A
        (0x4E00, 0x9FFF),   // CJK Unified Ideographs
        (0xA000, 0xA4CF),   // Yi Syllables
        (0xAC00, 0xD7A3),   // Hangul Syllables
        (0xF900, 0xFAFF),   // CJK Compatibility Ideographs
        (0xFE30, 0xFE4F),   // CJK Compatibility Forms
        (0xFF00, 0xFF60),   // Fullwidth Forms
        (0xFFE0, 0xFFE6),   // Fullwidth signs
        (0x1F300, 0x1F64F), // Emoji: symbols + pictographs + emoticons
        (0x1F680, 0x1F6FF), // Emoji: transport, misc
        (0x1F900, 0x1F9FF), // Emoji: supplemental symbols
        (0x1FA70, 0x1FAFF), // Emoji: more symbols and pictographs
        (0x20000, 0x2FFFD), // CJK Ext B-F + supplementary
        (0x30000, 0x3FFFD), // CJK Ext G-H
    ];
    for &(lo, hi) in WIDE_RANGES {
        if c >= lo && c <= hi {
            return 2;
        }
        if c < lo {
            break;
        }
    }
    1
}

// Approximate RGB triple for any 256-color palette index. Returns None
// for the first 16 entries, which belong to the user's terminal theme
// rather than a fixed RGB — callers that need an RGB answer for those
// (OSC 4 replies) should pick a reasonable default themselves.
pub fn rgb_from_256(n: u8) -> Option<(u8, u8, u8)> {
    if n < 16 {
        // Standard xterm defaults for the base 16. Close enough for
        // OSC 4 probe replies; a real palette query would need to track
        // actual theme RGBs, which we don't own.
        const BASE: [(u8, u8, u8); 16] = [
            (0, 0, 0),
            (205, 0, 0),
            (0, 205, 0),
            (205, 205, 0),
            (0, 0, 238),
            (205, 0, 205),
            (0, 205, 205),
            (229, 229, 229),
            (127, 127, 127),
            (255, 0, 0),
            (0, 255, 0),
            (255, 255, 0),
            (92, 92, 255),
            (255, 0, 255),
            (0, 255, 255),
            (255, 255, 255),
        ];
        return Some(BASE[n as usize]);
    }
    if (16..232).contains(&n) {
        let base = n - 16;
        let r = base / 36;
        let g = (base / 6) % 6;
        let b = base % 6;
        let scale = |v: u8| -> u8 {
            match v {
                0 => 0,
                1 => 95,
                2 => 135,
                3 => 175,
                4 => 215,
                _ => 255,
            }
        };
        return Some((scale(r), scale(g), scale(b)));
    }
    let step = (n - 232) * 10 + 8;
    Some((step, step, step))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub style: Style,
}

impl Cell {
    pub const BLANK: Cell = Cell {
        ch: ' ',
        style: Style::DEFAULT,
    };

    pub fn new(ch: char) -> Self {
        Self {
            ch,
            style: Style::DEFAULT,
        }
    }

    pub fn styled(ch: char, style: Style) -> Self {
        Self { ch, style }
    }
}

// Emits one row of cells as a string that contains the visible chars
// separated by SGR transitions whenever the active style changes. A
// trailing `\x1b[0m` resets after the row so subsequent writes (status
// bar, next row) start clean. OSC 8 hyperlinks bracket runs of cells
// that share the same URL so modern terminals render the run as a
// clickable link.
pub fn serialize_row(cells: &[Cell]) -> String {
    let mut buffer = String::with_capacity(cells.len() + 16);
    let mut active = Style::DEFAULT;
    let mut first = true;
    let mut active_link: Option<&Arc<str>> = None;

    for cell in cells {
        // Skip the trailing half of a wide character. The wide glyph in
        // the previous cell already occupies two display columns; emitting
        // the sentinel would either overwrite the right half with a space
        // or desync SGR transitions.
        if cell.ch == '\0' {
            continue;
        }
        if first || cell.style != active {
            emit_sgr_transition(&mut buffer, &active, &cell.style);
            let next_link = cell.style.hyperlink.as_ref();
            if next_link.map(Arc::as_ptr) != active_link.map(Arc::as_ptr) {
                emit_hyperlink_transition(&mut buffer, next_link);
                active_link = next_link;
            }
            active = cell.style.clone();
            first = false;
        }
        buffer.push(cell.ch);
    }

    if active_link.is_some() {
        emit_hyperlink_transition(&mut buffer, None);
    }
    if active != Style::DEFAULT {
        buffer.push_str("\x1b[0m");
    }
    buffer
}

fn emit_hyperlink_transition(buffer: &mut String, to: Option<&Arc<str>>) {
    // OSC 8 with an empty URL closes the current link. Opening a new link
    // implicitly closes the previous one, but terminals that don't
    // implement OSC 8 at all handle the empty-URL close cleanly too.
    match to {
        Some(url) => {
            let _ = write!(buffer, "\x1b]8;;{}\x1b\\", url);
        }
        None => buffer.push_str("\x1b]8;;\x1b\\"),
    }
}

fn emit_sgr_transition(buffer: &mut String, from: &Style, to: &Style) {
    // For simplicity and correctness: reset and re-apply the full `to`
    // style. Row-level churn is small; terminals handle back-to-back SGR
    // cheaply. Avoids subtle bugs from partial transitions (e.g.
    // clearing only one attribute when many are changing).
    if to.fg == Color::Default && to.bg == Color::Default && to.attrs == Attrs::default() {
        if from.fg != Color::Default || from.bg != Color::Default || from.attrs != Attrs::default()
        {
            buffer.push_str("\x1b[0m");
        }
        return;
    }
    buffer.push_str("\x1b[0m");
    let mut params: Vec<String> = Vec::new();
    let attrs = to.attrs;
    if attrs.bold {
        params.push("1".to_string());
    }
    if attrs.dim {
        params.push("2".to_string());
    }
    if attrs.italic {
        params.push("3".to_string());
    }
    if attrs.underline {
        params.push("4".to_string());
    }
    if attrs.reverse {
        params.push("7".to_string());
    }
    match to.fg {
        Color::Default => {}
        Color::Indexed(n) if n < 8 => params.push(format!("{}", 30 + n)),
        Color::Indexed(n) => params.push(format!("{}", 90 + n - 8)),
        Color::Rgb(r, g, b) => params.push(format!("38;2;{r};{g};{b}")),
    }
    match to.bg {
        Color::Default => {}
        Color::Indexed(n) if n < 8 => params.push(format!("{}", 40 + n)),
        Color::Indexed(n) => params.push(format!("{}", 100 + n - 8)),
        Color::Rgb(r, g, b) => params.push(format!("48;2;{r};{g};{b}")),
    }
    if params.is_empty() {
        return;
    }
    let _ = write!(buffer, "\x1b[{}m", params.join(";"));
}

// Standard base64 encoder used by the OSC 52 clipboard path. Zero-dep
// and tight enough to inline in the style module rather than spin up a
// separate helper crate.
pub fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let mut index = 0;
    while index + 3 <= input.len() {
        let a = input[index] as u32;
        let b = input[index + 1] as u32;
        let c = input[index + 2] as u32;
        let triple = (a << 16) | (b << 8) | c;
        out.push(ALPHABET[((triple >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3F) as usize] as char);
        out.push(ALPHABET[((triple >> 6) & 0x3F) as usize] as char);
        out.push(ALPHABET[(triple & 0x3F) as usize] as char);
        index += 3;
    }
    // Final 1 or 2 bytes, padded with '='.
    let remaining = input.len() - index;
    if remaining == 1 {
        let a = input[index] as u32;
        let triple = a << 16;
        out.push(ALPHABET[((triple >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3F) as usize] as char);
        out.push('=');
        out.push('=');
    } else if remaining == 2 {
        let a = input[index] as u32;
        let b = input[index + 1] as u32;
        let triple = (a << 16) | (b << 8);
        out.push(ALPHABET[((triple >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3F) as usize] as char);
        out.push(ALPHABET[((triple >> 6) & 0x3F) as usize] as char);
        out.push('=');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{Cell, Color, Style, apply_extended_color, color_from_256, serialize_row};

    #[test]
    fn serialize_plain_row_has_no_escapes() {
        let cells = "hello".chars().map(Cell::new).collect::<Vec<_>>();
        assert_eq!(serialize_row(&cells), "hello");
    }

    #[test]
    fn serialize_styled_row_emits_transitions() {
        let mut red = Style::DEFAULT;
        red.fg = Color::Indexed(1);
        let mut blue = Style::DEFAULT;
        blue.fg = Color::Indexed(4);

        let cells = vec![
            Cell::styled('H', red.clone()),
            Cell::styled('i', red),
            Cell::styled(' ', Style::DEFAULT),
            Cell::styled('B', blue.clone()),
            Cell::styled('y', blue),
        ];
        let out = serialize_row(&cells);
        assert!(
            out.contains("\x1b[0m\x1b[31m"),
            "expected red start in {out:?}"
        );
        assert!(out.contains("Hi"));
        assert!(out.contains("\x1b[0m"), "expected reset in {out:?}");
        assert!(out.contains("\x1b[34m"), "expected blue in {out:?}");
        assert!(out.contains("By"));
    }

    #[test]
    fn sgr_0_resets_and_other_params_update_style() {
        let mut style = Style::DEFAULT;
        style.apply_sgr(1, &[]); // bold
        style.apply_sgr(31, &[]); // red
        assert!(style.attrs.bold);
        assert_eq!(style.fg, Color::Indexed(1));

        style.apply_sgr(0, &[]);
        assert_eq!(style, Style::DEFAULT);
    }

    #[test]
    fn sgr_truecolor_consumes_four_params() {
        let mut fg = Color::Default;
        let consumed = apply_extended_color(&mut fg, &[2, 200, 50, 10]);
        assert_eq!(consumed, 4);
        assert_eq!(fg, Color::Rgb(200, 50, 10));
    }

    #[test]
    fn base64_encodes_empty_and_padded_input() {
        use super::base64_encode;
        assert_eq!(base64_encode(b""), "");
        // RFC 4648 test vectors.
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn serialize_row_brackets_hyperlink_runs_with_osc_8() {
        use std::sync::Arc;
        let url: Arc<str> = Arc::from("https://example.com");
        let mut linked = Style::DEFAULT;
        linked.hyperlink = Some(url.clone());

        let cells = vec![
            Cell::new('a'),
            Cell::styled('b', linked.clone()),
            Cell::styled('c', linked),
            Cell::new('d'),
        ];
        let out = serialize_row(&cells);

        // Opens a link before 'b', closes after 'c'.
        let open = "\x1b]8;;https://example.com\x1b\\";
        let close = "\x1b]8;;\x1b\\";
        assert!(out.contains(open), "missing OSC 8 open in {out:?}");
        assert!(out.contains(close), "missing OSC 8 close in {out:?}");
        let open_pos = out.find(open).unwrap();
        let close_pos = out.find(close).unwrap();
        assert!(open_pos < close_pos, "open must precede close in {out:?}");

        // Plain chars `a` and `d` must still appear.
        assert!(out.contains('a'));
        assert!(out.contains('d'));
    }

    #[test]
    fn char_width_covers_cjk_and_emoji_at_width_two() {
        use super::char_width;
        // ASCII is width-1; null is width-0 (the wide-char continuation
        // sentinel); CJK and common emoji are width-2.
        assert_eq!(char_width(' '), 1);
        assert_eq!(char_width('a'), 1);
        assert_eq!(char_width('\0'), 0);
        assert_eq!(char_width('你'), 2);
        assert_eq!(char_width('好'), 2);
        assert_eq!(char_width('あ'), 2);
        assert_eq!(char_width('한'), 2);
        assert_eq!(char_width('🚀'), 2);
        // Latin-1 supplemental is still width-1.
        assert_eq!(char_width('é'), 1);
    }

    #[test]
    fn box_drawing_chars_are_width_one() {
        // Box drawing (U+2500..U+257F) sits between the angle-bracket and
        // CJK ranges in our table. They render as single-cell glyphs in
        // every monospaced font; treating them as width-2 would shear the
        // borders TUI agents draw around their viewports.
        use super::char_width;
        for ch in ['─', '│', '┌', '┐', '└', '┘', '├', '┤', '┬', '┴', '┼'] {
            assert_eq!(char_width(ch), 1, "box drawing {:?}", ch);
        }
    }

    #[test]
    fn cjk_unified_ideographs_are_width_two() {
        use super::char_width;
        assert_eq!(char_width('字'), 2);
        assert_eq!(char_width('한'), 2);
    }

    #[test]
    fn common_emoji_are_width_two() {
        use super::char_width;
        // ✅ (U+2705) and ❌ (U+274C) live in Misc Symbols which the
        // hand-rolled table needs explicit ranges for to match
        // xterm's double-width rendering.
        assert_eq!(char_width('🚀'), 2);
        assert_eq!(char_width('✅'), 2);
        assert_eq!(char_width('❌'), 2);
    }

    #[test]
    fn dingbat_block_emoji_presentation_codepoints_are_width_two() {
        use super::char_width;
        // ✨ (U+2728) was the missing one that triggered the Dingbat
        // sweep — agent CLIs use it for "tip" and "new-feature"
        // callouts, and a width-1 mismatch desyncs the cursor for the
        // rest of the line. Spot-check a few more from the additions
        // (✈, ✔, ❤, ➡) so a future cleanup that drops one fails loudly.
        assert_eq!(char_width('✨'), 2);
        assert_eq!(char_width('✈'), 2);
        assert_eq!(char_width('✔'), 2);
        assert_eq!(char_width('❤'), 2);
        assert_eq!(char_width('➡'), 2);
    }

    #[test]
    fn color_from_256_handles_cube_and_grayscale() {
        assert_eq!(color_from_256(0), Color::Indexed(0));
        assert_eq!(color_from_256(15), Color::Indexed(15));
        // First color in the 6x6x6 cube should be pure black.
        assert_eq!(color_from_256(16), Color::Rgb(0, 0, 0));
        // 231 is the last cube entry (5,5,5) = white.
        assert_eq!(color_from_256(231), Color::Rgb(255, 255, 255));
        // First grayscale entry.
        let Color::Rgb(r, g, b) = color_from_256(232) else {
            panic!("expected grayscale RGB");
        };
        assert_eq!(r, g);
        assert_eq!(g, b);
    }
}
