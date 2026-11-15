//! Soft word-wrap computed on a monospace cell grid.
//!
//! Wrapping never shapes text: it only counts display cells (unicode-width,
//! tabs to the next tab stop), so the row count of a whole file is cheap to
//! keep up to date. Each visual row is shaped separately when drawn.

#![allow(dead_code)]

use ropey::Rope;
use std::borrow::Cow;
use unicode_width::UnicodeWidthChar;

/// One visual row of a line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowSeg {
    /// Byte range within the line text.
    pub start: usize,
    pub end: usize,
    /// Hanging indent in cells (0 for the first row of a line).
    pub indent: usize,
}

/// Width of `ch` in cells when it starts at column `col` of its row text.
#[inline]
fn char_w(ch: char, col: usize, tab_w: usize) -> usize {
    if ch == '\t' {
        let t = tab_w.max(1);
        t - (col % t)
    } else {
        UnicodeWidthChar::width(ch).unwrap_or(0)
    }
}

/// Display width of a string starting at column 0.
pub fn cells(text: &str, tab_w: usize) -> usize {
    let mut col = 0;
    for ch in text.chars() {
        col += char_w(ch, col, tab_w);
    }
    col
}

/// Byte offset of the char boundary nearest to `cell` (a position at or past
/// the middle of a char rounds to its end), clamped to `text.len()`.
pub fn byte_at_cell(text: &str, cell: usize, tab_w: usize) -> usize {
    let mut col = 0;
    for (i, ch) in text.char_indices() {
        let w = char_w(ch, col, tab_w);
        if cell < col + w {
            return if (cell - col) * 2 < w { i } else { i + ch.len_utf8() };
        }
        col += w;
    }
    text.len()
}

/// The shared wrapping walk. Calls `emit` for every row and returns the count.
///
/// A row holds at most `cols` cells, except that one whitespace character
/// starting at or before the edge may hang past it — otherwise a word that
/// exactly fills a row would push its trailing space onto a row of its own.
fn wrap_impl(text: &str, cols: usize, tab_w: usize, mut emit: impl FnMut(RowSeg)) -> usize {
    let len = text.len();
    if cols == 0 || (len <= cols && !text.as_bytes().contains(&b'\t')) {
        emit(RowSeg { start: 0, end: len, indent: 0 });
        return 1;
    }
    // Hanging indent for continuation rows: the line's own leading whitespace.
    let mut hang = 0;
    for ch in text.chars() {
        if ch != ' ' && ch != '\t' {
            break;
        }
        hang += char_w(ch, hang, tab_w);
    }
    let hang = hang.min(cols / 2);

    let mut count = 0;
    let mut row_start = 0;
    let mut row_indent = 0;
    let mut col = 0; // cells used in the current row, hanging indent included
    let mut last_break: Option<usize> = None; // byte just after the last whitespace of this row
    let mut i = 0;
    while i < len {
        let ch = text[i..].chars().next().unwrap();
        let w = char_w(ch, col - row_indent, tab_w);
        let hangs = ch.is_whitespace() && col <= cols;
        if col + w > cols && i > row_start && !hangs {
            let brk = match last_break {
                Some(b) if b > row_start => b,
                _ => i,
            };
            emit(RowSeg { start: row_start, end: brk, indent: row_indent });
            count += 1;
            row_start = brk;
            row_indent = hang;
            col = hang;
            last_break = None;
            // re-scan what was carried over: it may not fit the narrower row
            i = brk;
            continue;
        }
        col += w;
        i += ch.len_utf8();
        if ch.is_whitespace() {
            last_break = Some(i);
        }
    }
    emit(RowSeg { start: row_start, end: len, indent: row_indent });
    count + 1
}

/// Split a line (without its newline) into visual rows. `cols == 0` means no
/// wrapping. Breaks after whitespace when possible, else mid-word. There is
/// always at least one row, and only an empty line has an empty row.
pub fn wrap_line(text: &str, cols: usize, tab_w: usize, out: &mut Vec<RowSeg>) {
    out.clear();
    wrap_impl(text, cols, tab_w, |s| out.push(s));
}

/// `wrap_line(..).len()` without allocating.
pub fn count_rows(text: &str, cols: usize, tab_w: usize) -> usize {
    wrap_impl(text, cols, tab_w, |_| {})
}

fn strip_eol(s: &str) -> &str {
    s.trim_end_matches(['\n', '\r', '\u{0B}', '\u{0C}', '\u{85}', '\u{2028}', '\u{2029}'])
}


const TOUCHED: u32 = u32::MAX;

/// Maps between document lines and visual rows for one view.
pub struct WrapMap {
    cols: usize,
    tab_w: usize,
    built: bool,
    /// Line count as of the last sync / edit (the whole map when not wrapping).
    lines: usize,
    /// Visual rows per line (empty when not wrapping).
    rows: Vec<u32>,
    /// `prefix[i]` = first visual row of line `i`; one extra entry for the total.
    prefix: Vec<u32>,
    /// Markdown line kinds (empty when not previewing): headings are set
    /// larger, so they fit fewer columns and take more row units.
    kinds: Vec<u8>,
    key: u64,
}

impl Default for WrapMap {
    fn default() -> Self {
        Self::new()
    }
}

impl WrapMap {
    pub fn new() -> WrapMap {
        WrapMap { cols: 0, tab_w: 4, built: false, lines: 1, rows: Vec::new(), prefix: Vec::new(), kinds: Vec::new(), key: 0 }
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    /// Make the map valid for `(rope, cols, tab_w)`. Rebuilds fully if the
    /// width or tab size changed, it was never built, or the line count
    /// disagrees; otherwise does nothing.
    pub fn sync(&mut self, rope: &Rope, cols: usize, tab_w: usize) {
        self.sync_kinds(rope, cols, tab_w, &[], 0);
    }

    /// Like `sync`, with Markdown line kinds. `key` identifies the kinds
    /// (e.g. the buffer revision); a new key forces a rebuild.
    pub fn sync_kinds(&mut self, rope: &Rope, cols: usize, tab_w: usize, kinds: &[u8], key: u64) {
        let n = rope.len_lines();
        if cols == 0 && kinds.is_empty() {
            self.cols = 0;
            self.tab_w = tab_w;
            self.built = true;
            self.lines = n;
            self.rows.clear();
            self.prefix.clear();
            self.kinds.clear();
            self.key = 0;
            return;
        }
        if self.built && self.cols == cols && self.tab_w == tab_w && self.rows.len() == n && self.key == key {
            return;
        }
        self.cols = cols;
        self.tab_w = tab_w;
        self.key = key;
        self.kinds.clear();
        self.kinds.extend_from_slice(kinds);
        self.rebuild(rope);
    }

    fn kind(&self, line: usize) -> u8 {
        self.kinds.get(line).copied().unwrap_or(0)
    }

    /// Font scale of a line (headings in a Markdown preview are larger).
    pub fn scale(&self, line: usize) -> f32 {
        crate::md::metrics(self.kind(line)).0
    }

    /// Height of each visual row of a line, in row units.
    pub fn units(&self, line: usize) -> usize {
        crate::md::metrics(self.kind(line)).1
    }

    /// Columns available to a line (0 = no wrapping).
    pub fn cols_for(&self, line: usize) -> usize {
        let s = self.scale(line);
        if self.cols == 0 || s == 1.0 { self.cols } else { ((self.cols as f32 / s).floor() as usize).max(8) }
    }

    fn rows_of(&self, rope: &Rope, line: usize) -> u32 {
        let text: Cow<str> = rope.line(line).into();
        (count_rows(strip_eol(&text), self.cols_for(line), self.tab_w) * self.units(line)) as u32
    }

    fn rebuild(&mut self, rope: &Rope) {
        let mut rows = std::mem::take(&mut self.rows);
        rows.clear();
        for (i, l) in rope.lines().enumerate() {
            let text: Cow<str> = l.into();
            rows.push((count_rows(strip_eol(&text), self.cols_for(i), self.tab_w) * self.units(i)) as u32);
        }
        self.rows = rows;
        self.lines = self.rows.len();
        self.built = true;
        self.rebuild_prefix(0);
    }

    fn rebuild_prefix(&mut self, from: usize) {
        let n = self.rows.len();
        self.prefix.resize(n + 1, 0);
        let from = from.min(n);
        if from == 0 {
            self.prefix[0] = 0;
        }
        for i in from..n {
            self.prefix[i + 1] = self.prefix[i] + self.rows[i];
        }
    }

    /// Incremental update after edits. Each tuple is `(line, lines_removed,
    /// lines_inserted)` of one edit, in order; `rope` is the final text.
    pub fn edit(&mut self, rope: &Rope, notes: &[(usize, usize, usize)]) {
        let n = rope.len_lines();
        if !self.wrapping() {
            self.lines = n;
            return;
        }
        let mut first = usize::MAX;
        for &(line, removed, inserted) in notes {
            if line >= self.rows.len() {
                // edits were not reported consistently; start over
                self.rebuild(rope);
                return;
            }
            self.rows[line] = TOUCHED;
            let from = line + 1;
            let to = (from + removed).min(self.rows.len());
            self.rows.splice(from..to, std::iter::repeat(TOUCHED).take(inserted));
            first = first.min(line);
        }
        if self.rows.len() != n {
            self.rebuild(rope);
            return;
        }
        self.lines = n;
        if first == usize::MAX {
            return;
        }
        for i in first..n {
            if self.rows[i] == TOUCHED {
                self.rows[i] = self.rows_of(rope, i);
            }
        }
        self.rebuild_prefix(first);
    }

    fn wrapping(&self) -> bool {
        self.built && !self.rows.is_empty()
    }

    pub fn total_rows(&self, rope: &Rope) -> usize {
        if self.wrapping() { self.prefix[self.rows.len()] as usize } else { rope.len_lines() }
    }

    /// First visual row of a line.
    pub fn row_of_line(&self, line: usize) -> usize {
        if self.wrapping() { self.prefix[line.min(self.rows.len())] as usize } else { line }
    }

    /// `(line, sub-row within the line)` for a visual row, clamped to the last row.
    pub fn line_at_row(&self, row: usize) -> (usize, usize) {
        if !self.wrapping() {
            return (row.min(self.lines.saturating_sub(1)), 0);
        }
        let n = self.rows.len();
        let total = self.prefix[n] as usize;
        let row = row.min(total.saturating_sub(1));
        // last line whose first row is <= row
        let line = self.prefix[..n].partition_point(|&p| p as usize <= row).saturating_sub(1);
        (line, row - self.prefix[line] as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wrap(text: &str, cols: usize, tab_w: usize) -> Vec<RowSeg> {
        let mut v = Vec::new();
        wrap_line(text, cols, tab_w, &mut v);
        // invariants that hold for every result
        assert!(!v.is_empty());
        assert_eq!(v[0].start, 0);
        assert_eq!(v[0].indent, 0);
        assert_eq!(v.last().unwrap().end, text.len());
        for w in v.windows(2) {
            assert_eq!(w[0].end, w[1].start, "rows must be contiguous");
        }
        for s in &v {
            assert!(text.is_char_boundary(s.start) && text.is_char_boundary(s.end));
            assert!(s.end > s.start || text.is_empty(), "empty row in {text:?}");
        }
        assert_eq!(count_rows(text, cols, tab_w), v.len(), "count_rows disagrees for {text:?} @ {cols}");
        v
    }

    fn rows<'a>(text: &'a str, cols: usize) -> Vec<&'a str> {
        wrap(text, cols, 4).iter().map(|s| &text[s.start..s.end]).collect()
    }

    #[test]
    fn plain_words() {
        assert_eq!(rows("the quick brown fox", 10), ["the quick ", "brown fox"]);
        assert_eq!(rows("the quick brown fox", 9), ["the quick ", "brown fox"]); // the space hangs
        assert_eq!(rows("the quick brown fox", 8), ["the ", "quick ", "brown ", "fox"]);
        assert_eq!(rows("aaaa bbbb", 4), ["aaaa ", "bbbb"]);
        assert_eq!(rows("short", 80), ["short"]);
        assert_eq!(rows("exactly10!", 10), ["exactly10!"]);
    }

    #[test]
    fn long_words_hard_break() {
        assert_eq!(rows("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(rows("ab cdefghijkl", 5), ["ab ", "cdefg", "hijkl"]);
        assert_eq!(rows("abc", 1), ["a", "b", "c"]);
    }

    #[test]
    fn empty_and_no_wrap() {
        assert_eq!(wrap("", 10, 4), [RowSeg { start: 0, end: 0, indent: 0 }]);
        assert_eq!(wrap("", 0, 4), [RowSeg { start: 0, end: 0, indent: 0 }]);
        let long = "word ".repeat(100);
        assert_eq!(wrap(&long, 0, 4), [RowSeg { start: 0, end: long.len(), indent: 0 }]);
    }

    #[test]
    fn hanging_indent() {
        let text = "    let value = compute(alpha, beta, gamma);";
        let v = wrap(text, 20, 4);
        assert_eq!(v[0].indent, 0);
        assert!(v.len() >= 3);
        for s in &v[1..] {
            assert_eq!(s.indent, 4);
            // continuation rows fit in cols - indent (plus one hanging space)
            assert!(cells(&text[s.start..s.end], 4) <= 20 - 4 + 1, "{:?}", &text[s.start..s.end]);
        }
        // the indent is capped at half the width
        let deep = format!("{}{}", " ".repeat(30), "word ".repeat(10));
        let v = wrap(&deep, 20, 4);
        assert!(v.len() > 2);
        assert!(v[1..].iter().all(|s| s.indent == 10));
        // a carried word wider than the narrower continuation row is re-wrapped
        let v = wrap("        aaaa bbbbbbbbbbbbbbbbbb", 20, 4);
        for s in &v[1..] {
            assert!(cells(&"        aaaa bbbbbbbbbbbbbbbbbb"[s.start..s.end], 4) <= 12 + 1);
        }
    }

    #[test]
    fn tabs() {
        assert_eq!(cells("\t", 4), 4);
        assert_eq!(cells("a\tb", 4), 5);
        assert_eq!(cells("abcd\te", 4), 9);
        // a short line with a tab still goes through the slow path correctly
        assert_eq!(rows("\tabc", 10), ["\tabc"]);
        // tab indent hangs: 4 cells
        let text = "\tone two three four five";
        let v = wrap(text, 12, 4);
        assert!(v.len() >= 3);
        assert!(v[1..].iter().all(|s| s.indent == 4));
        assert_eq!(&text[v[0].start..v[0].end], "\tone two ");
    }

    #[test]
    fn wide_chars() {
        assert_eq!(cells("日本語", 4), 6);
        assert_eq!(rows("日本語日本語", 6), ["日本語", "日本語"]);
        assert_eq!(rows("日本語日本語", 5), ["日本", "語日", "本語"]);
        // a wide char never splits and a row always holds at least one char
        assert_eq!(rows("日本", 1), ["日", "本"]);
        // bytes > cols but cells <= cols must not wrap
        assert_eq!(rows("ééééé", 5), ["ééééé"]);
    }

    #[test]
    fn rows_never_exceed_width() {
        let samples = [
            "Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor.",
            "    if x { return some_really_long_function_name(argument_one, argument_two); }",
            "\t\tdeeply\tindented\ttabbed text that goes on for quite a while indeed",
            "no_spaces_at_all_just_one_enormous_identifier_that_keeps_going_and_going",
            "mixed 日本語 and ascii words 中文字符 spread across several rows of text",
            "trailing spaces          ",
            "                                        ",
        ];
        for text in samples {
            for cols in 2..40 {
                for seg in wrap(text, cols, 4) {
                    let row = &text[seg.start..seg.end];
                    let body = row.trim_end_matches([' ', '\t']);
                    // a row always takes at least one char, even if it is too wide
                    let lone = body.chars().count() <= 1;
                    assert!(
                        seg.indent + cells(body, 4) <= cols || lone,
                        "{row:?} overflows {cols} (indent {})",
                        seg.indent
                    );
                    // at most one whitespace char hangs past the edge
                    assert!(seg.indent + cells(row, 4) <= cols + 4 || lone, "{row:?} hangs too far at {cols}");
                }
            }
        }
    }

    #[test]
    fn byte_at_cell_round_trips() {
        for text in ["hello world", "a\tb\tc", "日本語 text", "", "éàü", "x"] {
            for (b, _) in text.char_indices().chain([(text.len(), ' ')]) {
                let c = cells(&text[..b], 4);
                assert_eq!(byte_at_cell(text, c, 4), b, "{text:?} byte {b} cell {c}");
            }
            assert_eq!(byte_at_cell(text, 10_000, 4), text.len());
        }
        // rounding inside wide chars and tabs
        assert_eq!(byte_at_cell("日本", 1, 4), 3);
        assert_eq!(byte_at_cell("\tx", 1, 4), 0);
        assert_eq!(byte_at_cell("\tx", 2, 4), 1);
        assert_eq!(byte_at_cell("\tx", 3, 4), 1);
    }

    fn assert_same(a: &WrapMap, b: &WrapMap, rope: &Rope, what: &str) {
        assert_eq!(a.rows, b.rows, "{what}: rows");
        assert_eq!(a.prefix, b.prefix, "{what}: prefix");
        assert_eq!(a.total_rows(rope), b.total_rows(rope), "{what}: total");
        assert_eq!(a.lines, b.lines, "{what}: lines");
    }

    /// Apply an edit to the rope and return its note the way the buffer will.
    fn apply(rope: &mut Rope, start: usize, end: usize, ins: &str) -> (usize, usize, usize) {
        let line = rope.char_to_line(start);
        let removed = rope.char_to_line(end) - line;
        rope.remove(start..end);
        rope.insert(start, ins);
        let inserted = rope.char_to_line(start + ins.chars().count()) - line;
        (line, removed, inserted)
    }

    fn sample() -> String {
        let mut s = String::new();
        for i in 0..60 {
            s.push_str(&format!("line {i}: {}\n", "some words to wrap ".repeat(i % 7)));
        }
        s
    }

    #[test]
    fn map_basics() {
        let rope = Rope::from_str("short\nthis line is long enough to wrap twice at ten\n\nend");
        let mut m = WrapMap::new();
        m.sync(&rope, 10, 4);
        assert_eq!(m.cols(), 10);
        let per_line: Vec<usize> = (0..rope.len_lines())
            .map(|i| count_rows(strip_eol(&rope.line(i).to_string()), 10, 4))
            .collect();
        assert_eq!(per_line[0], 1);
        assert!(per_line[1] > 3);
        assert_eq!(m.total_rows(&rope), per_line.iter().sum::<usize>());
        assert_eq!(m.row_of_line(0), 0);
        assert_eq!(m.row_of_line(1), 1);
        assert_eq!(m.row_of_line(2), 1 + per_line[1]);
        // inverses
        for line in 0..rope.len_lines() {
            for sub in 0..per_line[line] {
                assert_eq!(m.line_at_row(m.row_of_line(line) + sub), (line, sub));
            }
        }
        // clamped past the end
        let last = rope.len_lines() - 1;
        assert_eq!(m.line_at_row(10_000), (last, per_line[last] - 1));
    }

    #[test]
    fn map_without_wrapping() {
        let mut rope = Rope::from_str("a\nb\nc\n");
        let mut m = WrapMap::new();
        m.sync(&rope, 0, 4);
        assert_eq!(m.cols(), 0);
        assert_eq!(m.total_rows(&rope), 4);
        assert_eq!(m.row_of_line(2), 2);
        assert_eq!(m.line_at_row(2), (2, 0));
        assert_eq!(m.line_at_row(99), (3, 0));
        let note = apply(&mut rope, 0, 0, "x\ny\n");
        m.edit(&rope, &[note]);
        assert_eq!(m.total_rows(&rope), 6);
        assert_eq!(m.line_at_row(99), (5, 0));
        // switching wrapping on rebuilds
        m.sync(&rope, 5, 4);
        assert_eq!(m.total_rows(&rope), 6);
        assert_eq!(m.line_at_row(99), (5, 0));
    }

    #[test]
    fn trailing_newline_and_empty() {
        let mut m = WrapMap::new();
        let rope = Rope::from_str("");
        m.sync(&rope, 10, 4);
        assert_eq!(m.total_rows(&rope), 1);
        assert_eq!(m.line_at_row(0), (0, 0));
        let rope = Rope::from_str("abc\n");
        m.sync(&rope, 10, 4);
        assert_eq!(m.total_rows(&rope), 2);
        assert_eq!(m.line_at_row(5), (1, 0));
    }

    #[test]
    fn incremental_matches_rebuild() {
        let mut rope = Rope::from_str(&sample());
        let mut m = WrapMap::new();
        m.sync(&rope, 24, 4);
        let check = |m: &WrapMap, rope: &Rope, what: &str| {
            let mut fresh = WrapMap::new();
            fresh.sync(rope, 24, 4);
            assert_same(m, &fresh, rope, what);
        };
        check(&m, &rope, "initial");

        // edit within a line (makes it longer)
        let at = rope.line_to_char(3) + 2;
        let n = apply(&mut rope, at, at, " inserted text that makes this row quite a bit longer");
        m.edit(&rope, &[n]);
        check(&m, &rope, "in-line insert");

        // insert lines
        let at = rope.line_to_char(10);
        let n = apply(&mut rope, at, at, "new one\nnew two is a long line that wraps around\nnew three\n");
        m.edit(&rope, &[n]);
        check(&m, &rope, "insert lines");

        // delete across several lines
        let (a, b) = (rope.line_to_char(20) + 3, rope.line_to_char(26) + 1);
        let n = apply(&mut rope, a, b, "");
        m.edit(&rope, &[n]);
        check(&m, &rope, "delete lines");

        // replace a multi-line range with another multi-line text
        let (a, b) = (rope.line_to_char(5), rope.line_to_char(8) + 4);
        let n = apply(&mut rope, a, b, "x\ny\n");
        m.edit(&rope, &[n]);
        check(&m, &rope, "replace");

        // several notes in one call (as a multi-change transaction would report)
        let mut notes = Vec::new();
        for line in [2usize, 9, 30] {
            let at = rope.line_to_char(line);
            notes.push(apply(&mut rope, at, at, "    indented words that will wrap around somewhere\n"));
        }
        let (a, b) = (rope.line_to_char(1), rope.line_to_char(4));
        notes.push(apply(&mut rope, a, b, ""));
        m.edit(&rope, &notes);
        check(&m, &rope, "batched notes");

        // edits at the very start and very end
        let n = apply(&mut rope, 0, 0, "first\n");
        m.edit(&rope, &[n]);
        let end = rope.len_chars();
        let n = apply(&mut rope, end, end, "\nlast line with enough words to wrap a few times over");
        m.edit(&rope, &[n]);
        check(&m, &rope, "start and end");

        // delete everything
        let end = rope.len_chars();
        let n = apply(&mut rope, 0, end, "");
        m.edit(&rope, &[n]);
        check(&m, &rope, "delete all");
        assert_eq!(m.total_rows(&rope), 1);
    }

    #[test]
    fn random_edits_match_rebuild() {
        let mut rope = Rope::from_str(&sample());
        let mut m = WrapMap::new();
        m.sync(&rope, 17, 4);
        let inserts = ["x", " ", "\n", "word word word ", "\n\n", "a long piece of text\nspanning lines\n", "\t"];
        let mut seed = 0x9E3779B97F4A7C15u64;
        let mut rnd = move |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        for step in 0..300 {
            let len = rope.len_chars();
            let start = rnd(len + 1);
            let end = (start + if rnd(3) == 0 { rnd(80) } else { 0 }).min(len);
            let n = apply(&mut rope, start, end, inserts[rnd(inserts.len())]);
            m.edit(&rope, &[n]);
            let mut fresh = WrapMap::new();
            fresh.sync(&rope, 17, 4);
            assert_same(&m, &fresh, &rope, &format!("step {step}"));
        }
        // sync is a no-op when nothing changed, and rebuilds on a width change
        m.sync(&rope, 17, 4);
        let before = m.total_rows(&rope);
        m.sync(&rope, 40, 4);
        assert!(m.total_rows(&rope) <= before);
        let mut fresh = WrapMap::new();
        fresh.sync(&rope, 40, 4);
        assert_same(&m, &fresh, &rope, "width change");
    }

    #[test]
    fn unreported_edits_recover() {
        let mut rope = Rope::from_str("one\ntwo\nthree\n");
        let mut m = WrapMap::new();
        m.sync(&rope, 10, 4);
        rope.insert(0, "zero\nmore\n"); // not reported
        m.sync(&rope, 10, 4);
        assert_eq!(m.total_rows(&rope), rope.len_lines());
        // a note pointing past the map falls back to a rebuild
        let mut m2 = WrapMap::new();
        m2.sync(&Rope::from_str("a"), 10, 4);
        m2.edit(&rope, &[(40, 0, 1)]);
        assert_eq!(m2.total_rows(&rope), rope.len_lines());
    }
}
