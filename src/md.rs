//! Markdown live preview: a per-line block scan (headings, code fences,
//! rules) and an inline renderer that hides markup and reports how each
//! displayed byte maps back to the source line.
//!
//! The editor shows a line rendered unless the caret or selection is on it,
//! in which case it shows the raw text. Code blocks are always raw.

use ropey::Rope;

/// Line kinds from `scan`. 1..=6 are heading levels.
pub const TEXT: u8 = 0;
pub const FENCE: u8 = 7;
pub const CODE: u8 = 8;
pub const RULE: u8 = 9;

/// Font scale and height (in row units) for a line kind. Headings keep
/// their size whether rendered or raw, so the layout never jumps.
pub fn metrics(kind: u8) -> (f32, usize) {
    match kind {
        1 => (1.6, 2),
        2 => (1.35, 2),
        3 => (1.15, 1),
        _ => (1.0, 1),
    }
}

fn strip_eol(s: &str) -> &str {
    s.trim_end_matches(['\n', '\r', '\u{0B}', '\u{0C}', '\u{85}', '\u{2028}', '\u{2029}'])
}

fn is_rule(t: &str) -> bool {
    let mut ch = None;
    let mut n = 0;
    for c in t.chars() {
        if c == ' ' || c == '\t' {
            continue;
        }
        if !matches!(c, '-' | '*' | '_') || ch.is_some_and(|x| x != c) {
            return false;
        }
        ch = Some(c);
        n += 1;
    }
    n >= 3
}

fn heading_level(t: &str) -> u8 {
    let n = t.bytes().take_while(|b| *b == b'#').count();
    if (1..=6).contains(&n) && (t.len() == n || t.as_bytes()[n] == b' ') { n as u8 } else { 0 }
}

/// Classify every line of a document.
pub fn scan(rope: &Rope) -> Vec<u8> {
    let mut out = Vec::with_capacity(rope.len_lines());
    let mut fence: Option<(u8, usize)> = None;
    let mut front = false;
    for (i, l) in rope.lines().enumerate() {
        let text: std::borrow::Cow<str> = l.into();
        let text = strip_eol(&text);
        let t = text.trim_start();
        let indent = text.len() - t.len();
        if front {
            if t.trim_end() == "---" || t.trim_end() == "..." {
                front = false;
                out.push(FENCE);
            } else {
                out.push(CODE);
            }
            continue;
        }
        if let Some((ch, len)) = fence {
            let run = t.bytes().take_while(|b| *b == ch).count();
            if run >= len && t[run..].trim().is_empty() {
                fence = None;
                out.push(FENCE);
            } else {
                out.push(CODE);
            }
            continue;
        }
        if i == 0 && text.trim_end() == "---" {
            // YAML front matter
            front = true;
            out.push(FENCE);
            continue;
        }
        let b0 = t.as_bytes().first().copied().unwrap_or(0);
        if indent <= 3 && (b0 == b'`' || b0 == b'~') {
            let run = t.bytes().take_while(|b| *b == b0).count();
            if run >= 3 {
                fence = Some((b0, run));
                out.push(FENCE);
                continue;
            }
        }
        let h = if indent <= 3 { heading_level(t) } else { 0 };
        out.push(if h > 0 {
            h
        } else if indent <= 3 && is_rule(t) {
            RULE
        } else {
            TEXT
        });
    }
    out
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cls {
    Text,
    Heading,
    Link,
    Code,
    Marker,
    Dim,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct MdSpan {
    pub start: usize,
    pub end: usize,
    pub cls: Cls,
    pub bold: bool,
    pub italic: bool,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Deco {
    /// Background behind inline code (display byte range).
    CodeBg(usize, usize),
    Strike(usize, usize),
    /// `==highlight==`
    Mark(usize, usize),
    /// A task checkbox drawn at display byte `at`; `src` is the source byte
    /// of the character between the brackets.
    Check { at: usize, done: bool, src: usize },
}

#[derive(Default, Debug)]
pub struct Rendered {
    pub display: String,
    /// Source byte for each display byte, plus one entry for the end.
    pub map: Vec<u32>,
    pub spans: Vec<MdSpan>,
    pub decos: Vec<Deco>,
    /// Blockquote depth.
    pub quote: u8,
    pub rule: bool,
}

impl Rendered {
    /// Source byte for a display byte offset.
    pub fn src_of(&self, disp: usize) -> usize {
        self.map.get(disp).or(self.map.last()).copied().unwrap_or(0) as usize
    }
    /// First display byte whose source byte is at or after `src`.
    pub fn disp_of(&self, src: usize) -> usize {
        self.map.partition_point(|m| (*m as usize) < src).min(self.display.len())
    }
}

#[derive(Clone, Copy)]
struct St {
    cls: Cls,
    bold: bool,
    italic: bool,
}

struct B {
    r: Rendered,
}

impl B {
    /// Append text copied straight from the source at byte `src`.
    fn lit(&mut self, s: &str, src: usize, st: St) {
        if s.is_empty() {
            return;
        }
        let start = self.r.display.len();
        self.r.display.push_str(s);
        self.r.map.extend((0..s.len()).map(|i| (src + i) as u32));
        self.span(start, st);
    }
    /// Append replacement text that stands for the source byte `src`.
    fn sub(&mut self, s: &str, src: usize, st: St) {
        let start = self.r.display.len();
        self.r.display.push_str(s);
        self.r.map.extend(std::iter::repeat(src as u32).take(s.len()));
        self.span(start, st);
    }
    fn span(&mut self, start: usize, st: St) {
        let end = self.r.display.len();
        if st.cls == Cls::Text && !st.bold && !st.italic {
            return;
        }
        if let Some(last) = self.r.spans.last_mut() {
            if last.end == start && last.cls == st.cls && last.bold == st.bold && last.italic == st.italic {
                last.end = end;
                return;
            }
        }
        self.r.spans.push(MdSpan { start, end, cls: st.cls, bold: st.bold, italic: st.italic });
    }
}

fn is_alnum_before(text: &str, i: usize) -> bool {
    text[..i].chars().next_back().is_some_and(|c| c.is_alphanumeric())
}
fn is_alnum_at(text: &str, i: usize) -> bool {
    text[i..].chars().next().is_some_and(|c| c.is_alphanumeric())
}
fn ws_before(text: &str, i: usize) -> bool {
    text[..i].chars().next_back().is_none_or(|c| c.is_whitespace())
}
fn ws_at(text: &str, i: usize) -> bool {
    text[i..].chars().next().is_none_or(|c| c.is_whitespace())
}

/// Find a closing run of exactly `n` of `ch` starting the search at `from`.
fn find_close(text: &str, from: usize, ch: u8, n: usize) -> Option<usize> {
    let b = text.as_bytes();
    let mut j = from;
    while j < b.len() {
        if b[j] == b'\\' {
            j += 2;
            continue;
        }
        if b[j] == ch {
            let run = b[j..].iter().take_while(|x| **x == ch).count();
            if run == n && !ws_before(text, j) && (ch != b'_' || !is_alnum_at(text, j + run)) {
                return Some(j);
            }
            j += run;
            continue;
        }
        j += 1;
    }
    None
}

/// `[label](target)` starting at `[`: returns (label range, end after `)`).
fn link_at(text: &str, open: usize) -> Option<((usize, usize), usize)> {
    let b = text.as_bytes();
    let mut depth = 0i32;
    let mut j = open;
    let close = loop {
        match b.get(j)? {
            b'\\' => j += 1,
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    break j;
                }
            }
            _ => {}
        }
        j += 1;
    };
    if b.get(close + 1) != Some(&b'(') {
        return None;
    }
    let mut depth = 0i32;
    let mut k = close + 1;
    loop {
        match b.get(k)? {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(((open + 1, close), k + 1));
                }
            }
            _ => {}
        }
        k += 1;
    }
}

fn inline(b: &mut B, text: &str, base: usize, st: St, depth: u8) {
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut lit = 0;
    macro_rules! flush {
        () => {
            b.lit(&text[lit..i], base + lit, st)
        };
    }
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b'\\' if bytes.get(i + 1).is_some_and(|n| n.is_ascii_punctuation()) => {
                flush!();
                lit = i + 1;
                i += 2;
            }
            b'`' => {
                let n = bytes[i..].iter().take_while(|x| **x == b'`').count();
                let mut j = i + n;
                let mut close = None;
                while j < bytes.len() {
                    if bytes[j] == b'`' {
                        let run = bytes[j..].iter().take_while(|x| **x == b'`').count();
                        if run == n {
                            close = Some(j);
                            break;
                        }
                        j += run;
                    } else {
                        j += 1;
                    }
                }
                match close {
                    Some(j) => {
                        flush!();
                        let start = b.r.display.len();
                        b.lit(&text[i + n..j], base + i + n, St { cls: Cls::Code, bold: false, italic: false });
                        b.r.decos.push(Deco::CodeBg(start, b.r.display.len()));
                        i = j + n;
                        lit = i;
                    }
                    None => i += n,
                }
            }
            b'[' | b'!' if depth < 4 => {
                let open = if c == b'!' { i + 1 } else { i };
                if c == b'!' && bytes.get(open) != Some(&b'[') {
                    i += 1;
                    continue;
                }
                let link = St { cls: Cls::Link, ..st };
                if text[open..].starts_with("[[") {
                    // wiki link: [[target]] or [[target|alias]]
                    if let Some(end) = text[open + 2..].find("]]") {
                        let inner = &text[open + 2..open + 2 + end];
                        if !inner.is_empty() && !inner.contains('[') {
                            flush!();
                            let (shown, off) = match inner.find('|') {
                                Some(p) => (&inner[p + 1..], p + 1),
                                None => (inner, 0),
                            };
                            b.lit(shown, base + open + 2 + off, link);
                            i = open + 2 + end + 2;
                            lit = i;
                            continue;
                        }
                    }
                } else if let Some(((ls, le), end)) = link_at(text, open) {
                    flush!();
                    inline(b, &text[ls..le], base + ls, link, depth + 1);
                    i = end;
                    lit = i;
                    continue;
                }
                i += 1;
            }
            b'<' => {
                if let Some(end) = text[i + 1..].find('>') {
                    let inner = &text[i + 1..i + 1 + end];
                    if inner.contains("://") && !inner.contains(' ') {
                        flush!();
                        b.lit(inner, base + i + 1, St { cls: Cls::Link, ..st });
                        i += end + 2;
                        lit = i;
                        continue;
                    }
                }
                i += 1;
            }
            b'*' | b'_' if depth < 6 => {
                let run = bytes[i..].iter().take_while(|x| **x == c).count();
                let n = run.min(3);
                let can_open = run <= 3 && !ws_at(text, i + run) && (c != b'_' || !is_alnum_before(text, i));
                match can_open.then(|| find_close(text, i + n, c, n)).flatten() {
                    Some(j) if j > i + n => {
                        flush!();
                        let ns = St { bold: st.bold || n >= 2, italic: st.italic || n != 2, ..st };
                        inline(b, &text[i + n..j], base + i + n, ns, depth + 1);
                        i = j + n;
                        lit = i;
                    }
                    _ => i += run,
                }
            }
            b'~' | b'=' if bytes.get(i + 1) == Some(&c) && bytes.get(i + 2) != Some(&c) && depth < 6 => {
                let pat = if c == b'~' { "~~" } else { "==" };
                match text[i + 2..].find(pat) {
                    Some(end) if end > 0 && !ws_at(text, i + 2) => {
                        flush!();
                        let start = b.r.display.len();
                        let ns = if c == b'~' { St { cls: Cls::Dim, ..st } } else { st };
                        inline(b, &text[i + 2..i + 2 + end], base + i + 2, ns, depth + 1);
                        let range = (start, b.r.display.len());
                        b.r.decos.push(if c == b'~' { Deco::Strike(range.0, range.1) } else { Deco::Mark(range.0, range.1) });
                        i += end + 4;
                        lit = i;
                    }
                    _ => i += 2,
                }
            }
            b'h' if (text[i..].starts_with("http://") || text[i..].starts_with("https://")) && !is_alnum_before(text, i) => {
                let end = text[i..].find(|ch: char| ch.is_whitespace() || matches!(ch, ')' | '>' | ']')).unwrap_or(text.len() - i);
                flush!();
                b.lit(&text[i..i + end], base + i, St { cls: Cls::Link, ..st });
                i += end;
                lit = i;
            }
            _ => {
                // advance one whole character
                i += 1;
                while i < bytes.len() && !text.is_char_boundary(i) {
                    i += 1;
                }
            }
        }
    }
    i = bytes.len();
    flush!();
}

/// Render one source line (no newline) of the given kind.
pub fn render(text: &str, kind: u8) -> Rendered {
    let mut b = B { r: Rendered::default() };
    if kind == RULE {
        b.r.rule = true;
        b.r.map.push(text.len() as u32);
        return b.r;
    }
    let plain = St { cls: Cls::Text, bold: false, italic: false };
    let marker = St { cls: Cls::Marker, bold: false, italic: false };
    let mut st = plain;
    let lead = text.len() - text.trim_start().len();
    b.lit(&text[..lead], 0, plain);
    let mut pos = lead;
    let bytes = text.as_bytes();
    // blockquote markers
    while bytes.get(pos) == Some(&b'>') {
        b.r.quote += 1;
        pos += 1;
        if bytes.get(pos) == Some(&b' ') {
            pos += 1;
        }
    }
    if b.r.quote > 0 {
        st.italic = true;
    }
    if (1..=6).contains(&kind) && b.r.quote == 0 {
        pos += kind as usize;
        while bytes.get(pos) == Some(&b' ') {
            pos += 1;
        }
        st = St { cls: Cls::Heading, bold: true, italic: false };
    } else {
        let rest = &text[pos..];
        let rb = rest.as_bytes();
        if rb.len() >= 2 && matches!(rb[0], b'-' | b'*' | b'+') && rb[1] == b' ' {
            b.sub("• ", pos, marker);
            pos += 2;
            let t = &text[pos..];
            let done = t.starts_with("[x] ") || t.starts_with("[X] ");
            if done || t.starts_with("[ ] ") {
                let at = b.r.display.len();
                b.sub("   ", pos, plain);
                b.r.decos.push(Deco::Check { at, done, src: pos + 1 });
                pos += 4;
                if done {
                    st.cls = Cls::Dim;
                }
            }
        } else {
            let digits = rb.iter().take_while(|c| c.is_ascii_digit()).count();
            if digits > 0 && digits <= 9 && matches!(rb.get(digits), Some(b'.') | Some(b')')) && rb.get(digits + 1) == Some(&b' ') {
                b.lit(&rest[..digits + 2], pos, marker);
                pos += digits + 2;
            }
        }
    }
    inline(&mut b, &text[pos..], pos, st, 0);
    b.r.map.push(text.len() as u32);
    b.r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disp(s: &str) -> String {
        render(s, TEXT).display
    }

    #[test]
    fn scan_blocks() {
        let r = Rope::from_str("# One\ntext\n## Two\n```rust\n# not a heading\n```\n---\n#nope\n   ### three\n");
        assert_eq!(scan(&r), vec![1, TEXT, 2, FENCE, CODE, FENCE, RULE, TEXT, 3, TEXT]);
        let r = Rope::from_str("---\ntitle: x\n---\n# H\n");
        assert_eq!(scan(&r), vec![FENCE, CODE, FENCE, 1, TEXT]);
        // an unclosed fence runs to the end
        let r = Rope::from_str("~~~\n# a\n# b");
        assert_eq!(scan(&r), vec![FENCE, CODE, CODE]);
    }

    #[test]
    fn inline_markup_is_hidden() {
        assert_eq!(disp("plain text"), "plain text");
        assert_eq!(disp("a **bold** and *it* and ***both***"), "a bold and it and both");
        assert_eq!(disp("snake_case stays, _em_ works"), "snake_case stays, em works");
        assert_eq!(disp("`code *x*` then"), "code *x* then");
        assert_eq!(disp("see [the docs](https://x.y/a(b)) now"), "see the docs now");
        assert_eq!(disp("![alt text](img.png)"), "alt text");
        assert_eq!(disp("[[Page]] and [[Page|alias]]"), "Page and alias");
        assert_eq!(disp("~~gone~~ ==hot== \\*lit\\*"), "gone hot *lit*");
        assert_eq!(disp("2 * 3 * 4 and a_b_c and **"), "2 * 3 * 4 and a_b_c and **");
        assert_eq!(disp("<https://a.b> https://c.d/e."), "https://a.b https://c.d/e.");
        assert_eq!(disp("unclosed **bold and `tick"), "unclosed **bold and `tick");
        assert_eq!(disp("日本語 **太字** ok"), "日本語 太字 ok");
    }

    #[test]
    fn styles_and_blocks() {
        let r = render("a **b** *c*", TEXT);
        assert_eq!(r.spans.len(), 2);
        assert!(r.spans[0].bold && !r.spans[0].italic);
        assert!(r.spans[1].italic && !r.spans[1].bold);
        let r = render("## Title *x*", 2);
        assert_eq!(r.display, "Title x");
        assert!(r.spans.iter().all(|s| s.cls == Cls::Heading && s.bold));
        let r = render("> > quoted **b**", TEXT);
        assert_eq!((r.display.as_str(), r.quote), ("quoted b", 2));
        let r = render("  - [x] done thing", TEXT);
        assert_eq!(r.display, "  •    done thing");
        assert!(matches!(r.decos[0], Deco::Check { done: true, src: 5, .. }));
        let r = render("- [ ] todo", TEXT);
        assert!(matches!(r.decos[0], Deco::Check { done: false, src: 3, .. }));
        assert_eq!(render("12. item", TEXT).display, "12. item");
        assert!(render("* * *", RULE).rule);
    }

    #[test]
    fn mapping_round_trips() {
        let src = "x **bold** [t](u) `c`";
        let r = render(src, TEXT);
        assert_eq!(r.display, "x bold t c");
        assert_eq!(r.map.len(), r.display.len() + 1);
        // every display byte maps to the same byte in the source
        for (i, ch) in r.display.bytes().enumerate() {
            assert_eq!(src.as_bytes()[r.src_of(i)], ch);
        }
        assert!(r.map.windows(2).all(|w| w[0] <= w[1]));
        assert_eq!(r.src_of(r.display.len()), src.len());
        assert_eq!(r.src_of(2), 4); // 'b' of bold
        assert_eq!(r.disp_of(4), 2);
        assert_eq!(r.disp_of(0), 0);
        assert_eq!(r.disp_of(src.len()), r.display.len());
        // inside hidden markup lands on the next shown byte
        assert_eq!(r.disp_of(3), 2);
    }
}
