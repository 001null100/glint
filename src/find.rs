//! Find / replace bar: search state, matching and its floating panel.

use crate::doc::Document;
use crate::scene::{Quad, L_FLOAT};
use crate::ui::{Ctx, CursorKind, HitId, TextField};
use crate::util::{hash_str, Rect, Rgba};
use regex::{Regex, RegexBuilder};

const MAX_MATCHES: usize = 50_000;

#[derive(Default)]
pub struct FindBar {
    pub open: bool,
    pub show_replace: bool,
    pub q: TextField,
    pub r: TextField,
    pub case: bool,
    pub word: bool,
    pub regex: bool,
    /// Char ranges in the document `doc`, sorted.
    pub matches: Vec<(usize, usize)>,
    pub current: Option<usize>,
    pub doc: Option<u64>,
    pub error: bool,
    key: Option<(u64, u64, String, bool, bool, bool)>,
    re: Option<Regex>,
    // field rectangles from the last draw, for click placement
    pub q_rect: Rect,
    pub r_rect: Rect,
}

// button ids used with HitId::FindBtn
pub const B_CASE: u8 = 0;
pub const B_WORD: u8 = 1;
pub const B_REGEX: u8 = 2;
pub const B_PREV: u8 = 3;
pub const B_NEXT: u8 = 4;
pub const B_CLOSE: u8 = 5;
pub const B_REPLACE: u8 = 6;
pub const B_ALL: u8 = 7;
pub const B_TOGGLE: u8 = 8;

impl FindBar {
    fn build(&self) -> Option<Regex> {
        if self.q.text.is_empty() {
            return None;
        }
        let mut pat = if self.regex { self.q.text.clone() } else { regex::escape(&self.q.text) };
        if self.word {
            pat = format!(r"\b(?:{pat})\b");
        }
        RegexBuilder::new(&pat).case_insensitive(!self.case).multi_line(true).build().ok()
    }

    /// Recompute matches if the document, its text or the query changed.
    pub fn refresh(&mut self, doc: &Document) {
        let key = (doc.id, doc.buf.rev, self.q.text.clone(), self.case, self.word, self.regex);
        if self.key.as_ref() == Some(&key) {
            return;
        }
        self.key = Some(key);
        self.doc = Some(doc.id);
        self.matches.clear();
        self.current = None;
        self.re = self.build();
        self.error = self.re.is_none() && !self.q.text.is_empty();
        let Some(re) = &self.re else { return };
        let text = doc.buf.text();
        let rope = &doc.buf.rope;
        for m in re.find_iter(&text) {
            if m.start() == m.end() {
                continue;
            }
            self.matches.push((rope.byte_to_char(m.start()), rope.byte_to_char(m.end())));
            if self.matches.len() >= MAX_MATCHES {
                break;
            }
        }
    }

    pub fn invalidate(&mut self) {
        self.key = None;
    }

    /// Index of the first match starting at or after `pos` (wrapping).
    pub fn at_or_after(&self, pos: usize) -> Option<usize> {
        if self.matches.is_empty() {
            return None;
        }
        let i = self.matches.partition_point(|m| m.0 < pos);
        Some(if i >= self.matches.len() { 0 } else { i })
    }

    /// Index of the last match starting before `pos` (wrapping).
    pub fn before(&self, pos: usize) -> Option<usize> {
        if self.matches.is_empty() {
            return None;
        }
        let i = self.matches.partition_point(|m| m.0 < pos);
        Some(if i == 0 { self.matches.len() - 1 } else { i - 1 })
    }

    /// Index of the match that is exactly the range `a..b`.
    pub fn exact(&self, a: usize, b: usize) -> Option<usize> {
        let i = self.matches.partition_point(|m| m.0 < a);
        (self.matches.get(i) == Some(&(a, b))).then_some(i)
    }

    /// Every match (not capped like `matches`) with its replacement text.
    pub fn replacements(&self, doc: &Document) -> Vec<((usize, usize), String)> {
        let Some(re) = &self.re else { return Vec::new() };
        let text = doc.buf.text();
        let rope = &doc.buf.rope;
        let mut out = Vec::new();
        for caps in re.captures_iter(&text) {
            let Some(m) = caps.get(0) else { continue };
            if m.start() == m.end() {
                continue;
            }
            let rep = if self.regex {
                let mut s = String::new();
                caps.expand(&self.r.text, &mut s);
                s
            } else {
                self.r.text.clone()
            };
            out.push(((rope.byte_to_char(m.start()), rope.byte_to_char(m.end())), rep));
        }
        out
    }

    /// Replacement for the match at char range `a..b`. In regex mode `$1`
    /// etc. are expanded against the match in its real surroundings.
    pub fn replacement_at(&self, doc: &Document, a: usize, b: usize) -> String {
        if self.regex {
            if let Some(re) = &self.re {
                let text = doc.buf.text();
                let (ba, bb) = (doc.buf.rope.char_to_byte(a), doc.buf.rope.char_to_byte(b));
                if let Some(caps) = re.captures_at(&text, ba) {
                    if caps.get(0).is_some_and(|m| m.start() == ba && m.end() == bb) {
                        let mut s = String::new();
                        caps.expand(&self.r.text, &mut s);
                        return s;
                    }
                }
            }
        }
        self.r.text.clone()
    }

    fn button(cx: &mut Ctx, r: Rect, id: u8, label: &str, on: bool, alpha: f32) {
        let hid = HitId::FindBtn(id);
        let hov = cx.anim(hash_str("findbtn") ^ id as u64, if cx.is_hot(hid) { 1.0 } else { 0.0 }, 18.0);
        let s = cx.s;
        if on {
            cx.quad(L_FLOAT, Quad::new(r, cx.ui.accent.a(0.2 * alpha)).radius(6.0 * s).border(s.max(1.0), cx.ui.accent.a(0.55 * alpha)));
        } else if hov > 0.01 {
            cx.quad(L_FLOAT, Quad::new(r, cx.ui.text.a(0.1 * hov * alpha)).radius(6.0 * s));
        }
        let col = if on { cx.ui.accent } else { cx.ui.text_dim.lerp(cx.ui.text, hov) };
        cx.label_mid(L_FLOAT, label, cx.small_font, false, r, col.fade(alpha));
        cx.hit(r, hid, CursorKind::Pointer);
    }

    fn field(cx: &mut Ctx, f: &mut TextField, r: Rect, focused: bool, placeholder: &str, alpha: f32, id: u64, bad: bool, which: u8) {
        let s = cx.s;
        let foc = cx.anim(id ^ 0x77, if focused { 1.0 } else { 0.0 }, 18.0);
        let border = if bad { cx.ui.bad.a(0.8) } else { cx.ui.border.lerp(cx.ui.accent.a(0.7), foc) };
        cx.quad(L_FLOAT, Quad::new(r, Rgba::BLACK.a(0.28 * alpha)).radius(6.0 * s).border(s.max(1.0), border.fade(alpha)));
        cx.hit(r, HitId::FindField(which), CursorKind::Text);
        f.draw(cx, L_FLOAT, r.inset_xy(cx.px(9.0), 0.0), focused, placeholder, alpha, id);
    }

    /// Draw the bar at the top right of `anchor`. `focus` is the field that
    /// has the keyboard, `t` the open amount.
    pub fn draw(&mut self, cx: &mut Ctx, anchor: Rect, focus: Option<u8>, t: f32) {
        let s = cx.s;
        let w = cx.px(456.0).min(anchor.w - cx.px(24.0)).max(cx.px(220.0));
        let row = cx.px(30.0);
        let pad = cx.px(7.0);
        let h = pad * 2.0 + row + if self.show_replace { row + pad } else { 0.0 };
        let x = anchor.right() - w - cx.px(18.0);
        let y = anchor.y + cx.px(8.0) - (1.0 - t) * cx.px(16.0);
        let panel = Rect::new(x.max(anchor.x + cx.px(6.0)), y, w, h);
        cx.panel(L_FLOAT, panel, 10.0 * s, t);
        cx.hit(panel, HitId::Panel, CursorKind::Default);

        let b = cx.px(26.0);
        let gap = cx.px(3.0);
        let by = panel.y + pad + (row - b) * 0.5;
        // right-side buttons, laid out from the right edge
        let mut bx = panel.right() - pad - b;
        Self::button(cx, Rect::new(bx, by, b, b), B_CLOSE, "×", false, t);
        bx -= b + gap;
        Self::button(cx, Rect::new(bx, by, b, b), B_NEXT, "↓", false, t);
        bx -= b + gap;
        Self::button(cx, Rect::new(bx, by, b, b), B_PREV, "↑", false, t);
        bx -= b + gap + cx.px(4.0);
        Self::button(cx, Rect::new(bx, by, b, b), B_REGEX, ".*", self.regex, t);
        bx -= b + gap;
        Self::button(cx, Rect::new(bx, by, b, b), B_WORD, "W", self.word, t);
        bx -= b + gap;
        Self::button(cx, Rect::new(bx, by, b, b), B_CASE, "Aa", self.case, t);

        // match counter
        let (count, ccol) = if self.error {
            ("bad regex".to_string(), cx.ui.bad)
        } else if self.q.text.is_empty() {
            (String::new(), cx.ui.text_faint)
        } else if self.matches.is_empty() {
            ("no results".to_string(), cx.ui.warn)
        } else {
            let n = self.matches.len();
            let more = if n >= MAX_MATCHES { "+" } else { "" };
            match self.current {
                Some(i) => (format!("{}/{n}{more}", i + 1), cx.ui.text_dim),
                None => (format!("{n}{more}"), cx.ui.text_dim),
            }
        };
        let cw = if count.is_empty() { 0.0 } else { cx.label_w(&count, cx.small_font, false) + cx.px(10.0) };
        let toggle = Rect::new(panel.x + pad, by, cx.px(20.0), b);
        Self::button(cx, toggle, B_TOGGLE, if self.show_replace { "▾" } else { "▸" }, false, t);
        let fx = toggle.right() + gap;
        let q_rect = Rect::new(fx, panel.y + pad, (bx - gap - cx.px(4.0) - fx).max(cx.px(40.0)), row);
        self.q_rect = q_rect.inset_xy(cx.px(9.0), 0.0);
        let bad = self.error;
        Self::field(cx, &mut self.q, q_rect, focus == Some(0), "Find", t, hash_str("find-q"), bad, 0);
        if cw > 0.0 {
            let cr = Rect::new(q_rect.right() - cw, q_rect.y, cw, row);
            cx.label_in(L_FLOAT, &count, cx.small_font, false, cr, cr, ccol.fade(t));
        }

        if self.show_replace {
            let ry = panel.y + pad + row + pad;
            let aw = cx.px(38.0);
            let rw = cx.px(66.0);
            let all = Rect::new(panel.right() - pad - aw, ry + (row - b) * 0.5, aw, b);
            let one = Rect::new(all.x - gap - rw, all.y, rw, b);
            Self::button(cx, all, B_ALL, "All", false, t);
            Self::button(cx, one, B_REPLACE, "Replace", false, t);
            let r_rect = Rect::new(fx, ry, (one.x - gap - cx.px(4.0) - fx).max(cx.px(40.0)), row);
            self.r_rect = r_rect.inset_xy(cx.px(9.0), 0.0);
            Self::field(cx, &mut self.r, r_rect, focus == Some(1), "Replace with", t, hash_str("find-r"), false, 1);
        }
    }
}
