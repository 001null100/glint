//! One view onto a document: selection, scrolling, soft-wrap map and the
//! drawing / hit-testing of the text area.

use crate::buffer::{Buffer, Sel};
use crate::doc::{DocId, Document};
use crate::md;
use crate::scene::{Quad, L_BASE, L_FLOAT};
use crate::text::{Span, TextSystem};
use crate::ui::{Ctx, CursorKind, HitId};
use crate::util::{hash_str, lerp, mix_id, Rect, Smooth};
use crate::wrap::{byte_at_cell, cells, wrap_line, RowSeg, WrapMap};
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Reveal {
    None,
    Near,
    Center,
}

/// Rows longer than this are only shaped up to this many bytes.
const MAX_ROW_BYTES: usize = 4000;

pub struct View {
    pub doc: DocId,
    pub sel: Sel,
    /// Remembered column (in cells) for vertical movement.
    pub goal: Option<usize>,
    pub scroll_y: Smooth,
    pub scroll_x: Smooth,
    pub wrap: WrapMap,
    pub reveal: Reveal,
    caret: Option<(Smooth, Smooth)>,
    pub blink_at: f64,
    pub scrolled_at: f64,
    // layout from the last draw, used for hit-testing and paging
    pub rect: Rect,
    pub text_rect: Rect,
    pub max_scroll: f32,
    pub page_rows: usize,
    /// Rendered task checkboxes from the last draw: (rect, line, source byte).
    pub checks: Vec<(Rect, usize, usize)>,
}

/// Where a position sits in the wrapped layout of its line.
pub struct RowInfo {
    pub line: usize,
    pub byte: usize,
    pub text: String,
    pub segs: Vec<RowSeg>,
    pub sub: usize,
}

pub fn row_info(wrap: &WrapMap, buf: &Buffer, pos: usize, tab_w: usize) -> RowInfo {
    let (line, byte) = buf.pos_to_line_byte(pos);
    let text = buf.line_text(line);
    let mut segs = Vec::new();
    wrap_line(&text, wrap.cols_for(line), tab_w, &mut segs);
    let mut sub = 0;
    for (i, s) in segs.iter().enumerate() {
        if s.start <= byte {
            sub = i;
        }
    }
    RowInfo { line, byte, text, segs, sub }
}

fn prev_boundary(s: &str, mut i: usize) -> usize {
    if i == 0 {
        return 0;
    }
    i -= 1;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn clamp_seg(text: &str, seg: &RowSeg) -> RowSeg {
    if seg.end - seg.start <= MAX_ROW_BYTES {
        return *seg;
    }
    let mut end = seg.start + MAX_ROW_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    RowSeg { start: seg.start, end, indent: seg.indent }
}

/// Shape one visual row with the document's syntax colours.
fn shape_row(cx: &mut Ctx, doc: &Document, line: usize, text: &str, seg: &RowSeg, tab_w: usize, font: f32, row_h: f32) -> u64 {
    let row = &text[seg.start..seg.end];
    let mut spans: Vec<Span> = Vec::new();
    for t in doc.hl.spans(line) {
        let (a, b) = (t.start as usize, t.end as usize);
        if b <= seg.start {
            continue;
        }
        if a >= seg.end {
            break;
        }
        spans.push(Span {
            start: a.max(seg.start) - seg.start,
            end: b.min(seg.end) - seg.start,
            color: cx.syn.color(t.tok),
            bold: t.tok.bold(),
            italic: t.tok.italic(),
        });
    }
    cx.text.line(row, &spans, cx.syn.text, font, row_h, tab_w as u16)
}

fn scaled_font(cx: &Ctx, scale: f32) -> f32 {
    if scale == 1.0 { cx.font } else { (cx.font * scale * 2.0).round() / 2.0 }
}

/// Top of a box of height `h` centred in a row starting at `y`.
fn y_mid(y: f32, row_h: f32, h: f32) -> f32 {
    y + (row_h - h) * 0.5
}

impl View {
    pub fn new(doc: DocId) -> View {
        View {
            doc,
            sel: Sel::caret(0),
            goal: None,
            scroll_y: Smooth::new(0.0),
            scroll_x: Smooth::new(0.0),
            wrap: WrapMap::new(),
            reveal: Reveal::None,
            caret: None,
            blink_at: 0.0,
            scrolled_at: -10.0,
            rect: Rect::ZERO,
            text_rect: Rect::ZERO,
            max_scroll: 0.0,
            page_rows: 20,
            checks: Vec::new(),
        }
    }

    /// A second view of the same document (for splits).
    pub fn duplicate(&self) -> View {
        let mut v = View::new(self.doc);
        v.sel = self.sel;
        v.scroll_y = Smooth::new(self.scroll_y.target);
        v
    }

    /// Note caret activity: restart the blink and keep the caret on screen.
    pub fn touch(&mut self, now: f64) {
        self.blink_at = now;
        if self.reveal == Reveal::None {
            self.reveal = Reveal::Near;
        }
    }

    pub fn set_head(&mut self, pos: usize, extend: bool) {
        self.sel.head = pos;
        if !extend {
            self.sel.anchor = pos;
        }
        self.goal = None;
    }

    pub fn clamp(&mut self, len: usize) {
        self.sel.anchor = self.sel.anchor.min(len);
        self.sel.head = self.sel.head.min(len);
    }

    pub fn move_h(&mut self, buf: &Buffer, right: bool, word: bool, extend: bool) {
        if !extend && !self.sel.is_empty() && !word {
            let p = if right { self.sel.end() } else { self.sel.start() };
            self.set_head(p, false);
            return;
        }
        let h = self.sel.head;
        let p = match (right, word) {
            (false, false) => buf.prev_grapheme(h),
            (true, false) => buf.next_grapheme(h),
            (false, true) => buf.word_left(h),
            (true, true) => buf.word_right(h),
        };
        self.set_head(p, extend);
    }

    pub fn move_v(&mut self, buf: &Buffer, rows: isize, extend: bool, tab_w: usize) {
        let ri = row_info(&self.wrap, buf, self.sel.head, tab_w);
        let seg = ri.segs[ri.sub];
        let goal = self.goal.unwrap_or_else(|| seg.indent + cells(&ri.text[seg.start..ri.byte.clamp(seg.start, seg.end)], tab_w));
        let units = self.wrap.units(ri.line) as isize;
        let cur_row = self.wrap.row_of_line(ri.line) + ri.sub * units as usize;
        let total = self.wrap.total_rows(&buf.rope).max(1);
        // a tall (heading) row spans several row units; step over all of them
        let step = if rows > 0 { rows + units - 1 } else { rows };
        let target = (cur_row as isize + step).clamp(0, total as isize - 1) as usize;
        if target == cur_row || (rows > 0 && target < cur_row + units as usize) {
            // already on the first / last row: go to the very start / end
            let p = if rows < 0 { 0 } else { buf.len() };
            self.set_head(p, extend);
            self.goal = Some(goal);
            return;
        }
        let (line, sub) = self.wrap.line_at_row(target);
        let text = buf.line_text(line);
        let mut segs = Vec::new();
        wrap_line(&text, self.wrap.cols_for(line), tab_w, &mut segs);
        let si = (sub / self.wrap.units(line)).min(segs.len() - 1);
        let s2 = segs[si];
        let mut b = s2.start + byte_at_cell(&text[s2.start..s2.end], goal.saturating_sub(s2.indent), tab_w);
        if si + 1 < segs.len() && b >= s2.end {
            b = prev_boundary(&text, s2.end).max(s2.start);
        }
        let p = buf.line_byte_to_pos(line, b);
        self.set_head(p, extend);
        self.goal = Some(goal);
    }

    /// Home: start of the visual row; on the first row it toggles between
    /// the first non-blank character and column 0.
    pub fn home(&mut self, buf: &Buffer, extend: bool, tab_w: usize) {
        let ri = row_info(&self.wrap, buf, self.sel.head, tab_w);
        let p = if ri.sub > 0 && ri.byte > ri.segs[ri.sub].start {
            buf.line_byte_to_pos(ri.line, ri.segs[ri.sub].start)
        } else {
            let first = buf.first_non_ws(ri.line);
            let start = buf.line_start(ri.line);
            if self.sel.head == first { start } else { first }
        };
        self.set_head(p, extend);
    }

    pub fn end(&mut self, buf: &Buffer, extend: bool, tab_w: usize) {
        let ri = row_info(&self.wrap, buf, self.sel.head, tab_w);
        let seg = ri.segs[ri.sub];
        let p = if ri.sub + 1 < ri.segs.len() {
            let b = prev_boundary(&ri.text, seg.end).max(seg.start);
            if ri.byte >= b { buf.line_end(ri.line) } else { buf.line_byte_to_pos(ri.line, b) }
        } else {
            buf.line_end(ri.line)
        };
        self.set_head(p, extend);
    }

    pub fn scroll_by(&mut self, dy: f32, now: f64) {
        self.scroll_y.target = (self.scroll_y.target + dy).clamp(0.0, self.max_scroll);
        self.scrolled_at = now;
    }
}

pub struct EditorArgs<'a> {
    pub rect: Rect,
    pub pane: u64,
    pub focused: bool,
    pub wrap: bool,
    pub tab_w: usize,
    pub dim: f32,
    pub blink: bool,
    /// Find matches (char ranges, sorted) and the index of the current one.
    pub matches: &'a [(usize, usize)],
    pub current: Option<usize>,
    pub scrollbar_held: bool,
    /// Markdown live preview is on for this document.
    pub md: bool,
    /// Milliseconds of highlighting work allowed this frame.
    pub hl_budget_ms: u64,
}

const PAD_TOP: f32 = 8.0;

/// Whether a line of a previewed Markdown document is shown rendered.
fn md_rendered(kind: u8, line: usize, active: (usize, usize)) -> bool {
    kind != md::FENCE && kind != md::CODE && !(line >= active.0 && line <= active.1)
}

fn md_color(cx: &Ctx, cls: md::Cls) -> crate::util::Rgba {
    match cls {
        md::Cls::Text => cx.syn.text,
        md::Cls::Heading => cx.syn.heading,
        md::Cls::Link => cx.syn.link,
        md::Cls::Code => cx.syn.code,
        md::Cls::Marker => cx.syn.list_marker,
        md::Cls::Dim => cx.syn.comment,
    }
}

/// Shape one visual row of a rendered Markdown line.
fn md_row(cx: &mut Ctx, r: &md::Rendered, seg: &RowSeg, kind: u8, font: f32, row_h: f32, tab_w: usize) -> u64 {
    let quoted = r.quote > 0;
    let mut spans: Vec<Span> = Vec::new();
    for sp in &r.spans {
        if sp.end <= seg.start {
            continue;
        }
        if sp.start >= seg.end {
            break;
        }
        let color = if quoted && sp.cls == md::Cls::Text { cx.syn.quote } else { md_color(cx, sp.cls) };
        spans.push(Span { start: sp.start.max(seg.start) - seg.start, end: sp.end.min(seg.end) - seg.start, color, bold: sp.bold, italic: sp.italic });
    }
    let default = if quoted {
        cx.syn.quote
    } else if (1..=6).contains(&kind) {
        cx.syn.heading
    } else {
        cx.syn.text
    };
    cx.text.line(&r.display[seg.start..seg.end], &spans, default, font, row_h, tab_w as u16)
}

pub fn draw(cx: &mut Ctx, v: &mut View, doc: &mut Document, a: &EditorArgs) {
    let s = cx.s;
    let rect = a.rect;
    let lh = cx.line_h;
    let cell = cx.cell;
    if a.md {
        doc.md_sync();
    }
    let buf = &doc.buf;
    let lines = buf.lines();
    let digits = (lines.max(1).ilog10() as usize + 1).max(3);
    let gutter_w = (cx.px(16.0) + digits as f32 * cell + cx.px(14.0)).round();
    let sb_w = cx.px(12.0);
    let pad_top = cx.px(PAD_TOP);
    let text_rect = Rect::new(rect.x + gutter_w, rect.y, (rect.w - gutter_w - sb_w).max(cell), rect.h);
    let tab_w = a.tab_w;
    let cols = if a.wrap { ((text_rect.w - cell) / cell).floor().max(8.0) as usize } else { 0 };
    if a.md {
        v.wrap.sync_kinds(&buf.rope, cols, tab_w, &doc.md_kinds, buf.rev.wrapping_add(1));
    } else {
        v.wrap.sync(&buf.rope, cols, tab_w);
    }
    let total_rows = v.wrap.total_rows(&buf.rope);
    let content_h = total_rows as f32 * lh + pad_top;
    let max_scroll = if content_h <= rect.h { 0.0 } else { content_h - rect.h * 0.5 };
    v.rect = rect;
    v.text_rect = text_rect;
    v.max_scroll = max_scroll;
    v.page_rows = ((rect.h / lh) as usize).saturating_sub(2).max(1);
    v.clamp(buf.len());
    v.checks.clear();

    // ---- keep the caret on screen
    if v.reveal != Reveal::None {
        let ri = row_info(&v.wrap, buf, v.sel.head, tab_w);
        let units = v.wrap.units(ri.line);
        let scale = v.wrap.scale(ri.line);
        let row = v.wrap.row_of_line(ri.line) + ri.sub * units;
        let y = row as f32 * lh + pad_top;
        let rh = lh * units as f32;
        let margin = (lh * 2.0).min(rect.h * 0.25);
        let mut t = v.scroll_y.target;
        if y < t + margin || y + rh > t + rect.h - margin {
            if v.reveal == Reveal::Center || y < t - rect.h || y > t + 2.0 * rect.h {
                t = y - rect.h * 0.42;
            } else if y < t + margin {
                t = y - margin;
            } else {
                t = y + rh - rect.h + margin;
            }
        }
        t = t.clamp(0.0, max_scroll);
        // long jumps start close to the destination so the glide stays short
        let d = t - v.scroll_y.v;
        if d.abs() > rect.h * 2.5 {
            v.scroll_y.v = t - d.signum() * rect.h * 1.2;
        }
        v.scroll_y.target = t;
        if cols == 0 {
            let seg = clamp_seg(&ri.text, &ri.segs[ri.sub]);
            let font = scaled_font(cx, scale);
            let key = shape_row(cx, doc, ri.line, &ri.text, &seg, tab_w, font, rh);
            let x = cx.text.x_at(key, ri.byte.saturating_sub(seg.start));
            let mut tx = v.scroll_x.target;
            if x < tx + cell * 2.0 {
                tx = x - cell * 8.0;
            }
            if x > tx + text_rect.w - cell * 3.0 {
                tx = x - text_rect.w + cell * 8.0;
            }
            v.scroll_x.target = tx.max(0.0);
        }
        v.reveal = Reveal::None;
    }
    if cols != 0 {
        v.scroll_x.snap(0.0);
    }
    v.scroll_y.target = v.scroll_y.target.clamp(0.0, max_scroll);
    if v.scroll_y.v > max_scroll + rect.h {
        v.scroll_y.v = max_scroll;
    }
    let sy = cx.tick(&mut v.scroll_y, 24.0, 0.5).round();
    let sx = cx.tick(&mut v.scroll_x, 24.0, 0.5).round();

    // ---- visible range
    let first_row = ((sy - pad_top) / lh).floor().max(0.0) as usize;
    let y0 = rect.y + pad_top + first_row as f32 * lh - sy;
    let n_rows = ((rect.bottom() - y0) / lh).ceil().max(0.0) as usize + 1;
    let (first_line, first_sub) = v.wrap.line_at_row(first_row);
    let (last_line, _) = v.wrap.line_at_row(first_row + n_rows);
    if doc.hl.advance(&doc.buf.rope, last_line + 40, Duration::from_millis(a.hl_budget_ms)) {
        cx.animating = true;
    }
    let buf = &doc.buf;

    // ---- veil behind the text
    let veil = cx.ui.editor;
    cx.quad(L_BASE, Quad::new(rect, veil.a((veil.0[3] * a.dim).min(1.0))));
    cx.hit(rect, HitId::Editor(a.pane), CursorKind::Text);

    // ---- selection / caret / match positions
    let (sel_a, sel_b) = (v.sel.start(), v.sel.end());
    let has_sel = sel_a != sel_b;
    let (sa_line, sa_byte) = buf.pos_to_line_byte(sel_a);
    let (sb_line, sb_byte) = buf.pos_to_line_byte(sel_b);
    let (c_line, c_byte) = buf.pos_to_line_byte(v.sel.head);
    let active = (sa_line, sb_line);

    // matches that touch the visible lines, split per line
    let mut vis: Vec<(usize, usize, usize, bool)> = Vec::new();
    if !a.matches.is_empty() && first_line < lines {
        let from = buf.line_start(first_line);
        let to = buf.line_end(last_line.min(lines - 1));
        let i0 = a.matches.partition_point(|m| m.1 < from);
        for (i, m) in a.matches.iter().enumerate().skip(i0) {
            if m.0 > to || vis.len() > 4000 {
                break;
            }
            let (la, ba) = buf.pos_to_line_byte(m.0);
            let (lb, bb) = buf.pos_to_line_byte(m.1);
            for l in la..=lb {
                let st = if l == la { ba } else { 0 };
                let en = if l == lb { bb } else { usize::MAX };
                vis.push((l, st, en, a.current == Some(i)));
            }
        }
    }
    let mut mi = 0usize;

    let sel_col = cx.ui.selection.fade(if a.focused { 1.0 } else { 0.55 });
    let num_size = (cx.font * 0.86 * 2.0).round() / 2.0;
    let num_h = TextSystem::label_h(num_size);
    let gutter_clip = Rect::new(rect.x, rect.y, gutter_w, rect.h);
    // caret target: x, first row unit, row height, font scale
    let mut caret_target: Option<(f32, usize, f32, f32)> = None;

    let mut line = first_line;
    let mut row = first_row;
    let mut y = y0;
    let mut segs: Vec<RowSeg> = Vec::new();
    let mut dsegs: Vec<RowSeg> = Vec::new();
    while line < lines && y < rect.bottom() {
        let text = buf.line_text(line);
        let units = v.wrap.units(line);
        let scale = v.wrap.scale(line);
        let row_h = lh * units as f32;
        let font = scaled_font(cx, scale);
        let cw = cell * scale;
        let kind = if a.md { doc.md_kinds.get(line).copied().unwrap_or(0) } else { 0 };
        wrap_line(&text, v.wrap.cols_for(line), tab_w, &mut segs);
        let n_segs = segs.len();
        let mut seg0 = 0;
        if line == first_line {
            // the view may start part-way through a tall (heading) row
            seg0 = (first_sub / units).min(n_segs - 1);
            let part = first_sub - seg0 * units;
            y -= part as f32 * lh;
            row -= part.min(row);
        }
        let block_h = (n_segs - seg0) as f32 * row_h;
        if seg0 == 0 {
            let n = (line + 1).to_string();
            let key = cx.text.label(&n, num_size, false);
            let w = cx.text.width(key);
            let col = if line == c_line { cx.ui.text_dim } else { cx.ui.text_faint };
            cx.scene.text(L_BASE, key, (rect.x + gutter_w - cx.px(14.0) - w).round(), (y + (row_h - num_h) * 0.5).round(), gutter_clip, col);
        }
        while mi < vis.len() && vis[mi].0 < line {
            mi += 1;
        }
        if a.md && (kind == md::FENCE || kind == md::CODE) {
            // code blocks and front matter sit on a faint panel
            let r = Rect::new(text_rect.x - cx.px(8.0), y, text_rect.w + cx.px(4.0), block_h);
            cx.quad(L_BASE, Quad::new(r, cx.ui.text.a(0.045)).clip(rect));
        }

        if a.md && md_rendered(kind, line, active) {
            // ---- rendered Markdown: markup hidden, laid out on its own
            let r = md::render(&text, kind);
            if r.rule {
                let ry = (y + row_h * 0.5).round();
                cx.quad(L_BASE, Quad::new(Rect::new(text_rect.x, ry, text_rect.w - cx.px(8.0), s.max(1.0)), cx.ui.text_faint.a(0.7)).clip(text_rect));
            } else {
                wrap_line(&r.display, v.wrap.cols_for(line), tab_w, &mut dsegs);
                for q in 0..r.quote.min(6) {
                    let bx = text_rect.x - cx.px(10.0) + q as f32 * cx.px(5.0);
                    cx.quad(L_BASE, Quad::new(Rect::new(bx, y + 2.0 * s, 2.5 * s, block_h - 4.0 * s), cx.ui.accent.a(0.7)).radius(s).clip(rect));
                }
                for (di, dseg) in dsegs.iter().enumerate().take(n_segs).skip(seg0) {
                    let ry = y + (di - seg0) as f32 * row_h;
                    if ry >= rect.bottom() {
                        break;
                    }
                    let key = md_row(cx, &r, dseg, kind, font, row_h, tab_w);
                    let ox = text_rect.x + dseg.indent as f32 * cw - sx;
                    let xr = |cx: &Ctx, from: usize, to: usize| -> Option<(f32, f32)> {
                        let (f, t) = (from.max(dseg.start), to.min(dseg.end));
                        (f < t).then(|| (cx.text.x_at(key, f - dseg.start), cx.text.x_at(key, t - dseg.start)))
                    };
                    for d in &r.decos {
                        match *d {
                            md::Deco::CodeBg(f, t) => {
                                if let Some((x0, x1)) = xr(cx, f, t) {
                                    let q = Rect::new(ox + x0 - 3.0 * s, y_mid(ry, row_h, lh * 0.8), x1 - x0 + 6.0 * s, lh * 0.8);
                                    cx.quad(L_BASE, Quad::new(q, cx.ui.text.a(0.09)).radius(4.0 * s).clip(text_rect));
                                }
                            }
                            md::Deco::Mark(f, t) => {
                                if let Some((x0, x1)) = xr(cx, f, t) {
                                    let q = Rect::new(ox + x0 - s, y_mid(ry, row_h, lh * 0.8), x1 - x0 + 2.0 * s, lh * 0.8);
                                    cx.quad(L_BASE, Quad::new(q, cx.ui.warn.a(0.28)).radius(3.0 * s).clip(text_rect));
                                }
                            }
                            md::Deco::Strike(f, t) => {
                                if let Some((x0, x1)) = xr(cx, f, t) {
                                    let q = Rect::new(ox + x0, (ry + row_h * 0.53).round(), x1 - x0, s.max(1.0));
                                    cx.quad(L_FLOAT, Quad::new(q, cx.syn.comment).clip(text_rect));
                                }
                            }
                            md::Deco::Check { at, done, src } => {
                                if at >= dseg.start && at < dseg.end {
                                    let size = (font * 0.92).round();
                                    let bx = ox + cx.text.x_at(key, at - dseg.start) + (cw * 2.0 - size) * 0.5;
                                    let b = Rect::new(bx.round(), y_mid(ry, row_h, size).round(), size, size);
                                    if done {
                                        cx.quad(L_BASE, Quad::new(b, cx.ui.accent.a(0.9)).radius(3.5 * s).clip(text_rect));
                                        let tick = cx.text.label("✓", (size * 0.82 * 2.0).round() / 2.0, true);
                                        let (tw, th) = (cx.text.width(tick), TextSystem::label_h((size * 0.82 * 2.0).round() / 2.0));
                                        cx.scene.text(L_BASE, tick, (b.cx() - tw * 0.5).round(), (b.cy() - th * 0.5).round(), text_rect, cx.ui.bg);
                                    } else {
                                        cx.quad(L_BASE, Quad::new(b, cx.ui.text.a(0.04)).radius(3.5 * s).border(1.5 * s, cx.ui.text_dim).clip(text_rect));
                                    }
                                    v.checks.push((b.inset(-3.0 * s), line, src));
                                }
                            }
                        }
                    }
                    // find matches, mapped onto the rendered text
                    let mut j = mi;
                    while j < vis.len() && vis[j].0 == line {
                        let (_, st, en, cur) = vis[j];
                        let (f, t) = (r.disp_of(st), r.disp_of(en.min(text.len())));
                        if let Some((x0, x1)) = xr(cx, f, t) {
                            let col = if cur { cx.ui.find_current } else { cx.ui.find };
                            cx.quad(L_BASE, Quad::new(Rect::new(ox + x0, ry + 1.0, x1 - x0, row_h - 2.0), col).radius(3.0 * s).clip(text_rect));
                        }
                        j += 1;
                    }
                    cx.scene.text(L_BASE, key, ox, ry, text_rect, cx.syn.text);
                }
            }
            y += block_h;
            row += (n_segs - seg0) * units;
            line += 1;
            continue;
        }

        // ---- raw text
        for si in seg0..n_segs {
            if y >= rect.bottom() {
                break;
            }
            let last_seg = si + 1 == n_segs;
            let seg = clamp_seg(&text, &segs[si]);
            let key = shape_row(cx, doc, line, &text, &seg, tab_w, font, row_h);
            let ox = text_rect.x + seg.indent as f32 * cw - sx;

            // find matches
            let mut j = mi;
            while j < vis.len() && vis[j].0 == line {
                let (_, st, en, cur) = vis[j];
                let (ma, mb) = (st.max(seg.start), en.min(seg.end));
                if ma < mb {
                    let x0 = cx.text.x_at(key, ma - seg.start);
                    let x1 = cx.text.x_at(key, mb - seg.start);
                    let col = if cur { cx.ui.find_current } else { cx.ui.find };
                    cx.quad(L_BASE, Quad::new(Rect::new(ox + x0, y + 1.0, x1 - x0, row_h - 2.0), col).radius(3.0 * s).clip(text_rect));
                }
                j += 1;
            }

            // selection
            if has_sel && line >= sa_line && line <= sb_line {
                let from = if line == sa_line { sa_byte } else { 0 };
                let to = if line == sb_line { sb_byte } else { usize::MAX };
                let (xa, xb) = (from.max(seg.start), to.min(seg.end));
                let continues = to > seg.end && last_seg;
                if from <= seg.end && to >= seg.start && (xa < xb || (continues && xa <= seg.end)) {
                    let x0 = cx.text.x_at(key, xa - seg.start);
                    let x1 = cx.text.x_at(key, xb.max(xa) - seg.start) + if continues { cw * 0.6 } else { 0.0 };
                    cx.quad(L_BASE, Quad::new(Rect::new(ox + x0, y, x1 - x0, row_h), sel_col).radius(3.0 * s).clip(text_rect));
                }
            }

            // caret
            if line == c_line && c_byte >= segs[si].start && (last_seg || c_byte < segs[si + 1].start) {
                let b = c_byte.min(seg.end) - seg.start;
                caret_target = Some((seg.indent as f32 * cw + cx.text.x_at(key, b), row, row_h, scale));
            }

            cx.scene.text(L_BASE, key, ox, y, text_rect, cx.syn.text);
            y += row_h;
            row += units;
        }
        line += 1;
    }

    // ---- caret and current-line highlight
    if let Some((tx, trow, row_h, scale)) = caret_target {
        let ty = pad_top + trow as f32 * lh;
        let (mut ax, mut ay) = v.caret.unwrap_or((Smooth::new(tx), Smooth::new(ty)));
        if (ay.v - ty).abs() > rect.h * 1.5 {
            ax.snap(tx);
            ay.snap(ty);
        }
        ax.target = tx;
        ay.target = ty;
        let gx = cx.tick(&mut ax, 36.0, 0.3);
        let gy = cx.tick(&mut ay, 36.0, 0.3);
        v.caret = Some((ax, ay));

        let screen_y = rect.y + gy - sy;
        if a.focused && !has_sel {
            cx.quad(L_BASE, Quad::new(Rect::new(rect.x, screen_y, rect.w, row_h), cx.ui.line).clip(rect));
        }
        let alpha = if a.focused && a.blink {
            let t = (cx.now - v.blink_at) as f32;
            if t < 0.55 {
                cx.wake_at(v.blink_at + 0.55);
                1.0
            } else if t < 25.0 {
                cx.wake_at(cx.now + 1.0 / 30.0);
                0.25 + 0.75 * (0.5 + 0.5 * ((t - 0.55) * 5.4).cos())
            } else {
                1.0
            }
        } else {
            1.0
        };
        let w = (2.0 * s).round().max(1.0);
        let ch = (lh * 0.82 * scale).round();
        // the caret stretches from its gliding tail to where it is headed
        let (x0, x1) = if (gy - ty).abs() < 0.5 { (gx.min(tx), gx.max(tx)) } else { (gx, gx) };
        let cr = Rect::new(text_rect.x + x0 - sx - w * 0.5, screen_y + (row_h - ch) * 0.5, x1 - x0 + w, ch);
        let clip = Rect::new(text_rect.x - w, rect.y, text_rect.w + w, rect.h);
        if a.focused {
            cx.quad(L_BASE, Quad::new(cr.inset(-2.0 * s), cx.ui.accent.a(0.28 * alpha)).radius(4.0 * s).soft(5.0 * s).clip(clip));
            cx.quad(L_BASE, Quad::new(cr, cx.ui.cursor.fade(alpha)).radius(w * 0.5).clip(clip));
        } else {
            cx.quad(L_BASE, Quad::new(cr, cx.ui.cursor.fade(0.3)).radius(w * 0.5).clip(clip));
        }
    }

    // ---- scrollbar
    if max_scroll > 0.0 {
        let track = Rect::new(rect.right() - sb_w, rect.y, sb_w, rect.h);
        let th = (rect.h * rect.h / (max_scroll + rect.h)).max(cx.px(36.0)).min(rect.h);
        let ty = rect.y + (rect.h - th) * (sy / max_scroll).clamp(0.0, 1.0);
        let hot = cx.is_hot(HitId::Scrollbar(a.pane)) || a.scrollbar_held;
        let since = (cx.now - v.scrolled_at) as f32;
        let recent = since < 1.2;
        if recent {
            cx.wake_at(v.scrolled_at + 1.2);
        }
        let id = mix_id(hash_str("scrollbar"), a.pane);
        let grow = cx.anim(id, if hot { 1.0 } else { 0.0 }, 18.0);
        let vis_a = cx.anim(id ^ 1, if hot { 0.75 } else if recent { 0.45 } else { 0.16 }, 10.0);
        let w = lerp(3.0, 7.0, grow) * s;
        let thumb = Rect::new(track.right() - w - 2.0 * s, ty + 2.0 * s, w, th - 4.0 * s);
        cx.quad(L_FLOAT, Quad::new(thumb, cx.ui.text_dim.a(vis_a)).radius(w * 0.5));
        cx.hit(track, HitId::Scrollbar(a.pane), CursorKind::Default);
    }
}

/// Document position under a window coordinate. `md` says whether the
/// document is shown as a Markdown preview.
pub fn pos_at(cx: &mut Ctx, v: &View, doc: &Document, x: f32, y: f32, tab_w: usize, md: bool) -> usize {
    let lh = cx.line_h;
    let pad_top = cx.px(PAD_TOP);
    let sy = v.scroll_y.v.round();
    let sx = v.scroll_x.v.round();
    let row = ((y - v.rect.y + sy - pad_top) / lh).floor().max(0.0) as usize;
    let total = v.wrap.total_rows(&doc.buf.rope);
    if row >= total {
        return doc.buf.len();
    }
    let (line, sub) = v.wrap.line_at_row(row);
    let units = v.wrap.units(line);
    let scale = v.wrap.scale(line);
    let (font, row_h, cw) = (scaled_font(cx, scale), lh * units as f32, cx.cell * scale);
    let text = doc.buf.line_text(line);
    let mut segs = Vec::new();
    wrap_line(&text, v.wrap.cols_for(line), tab_w, &mut segs);
    let si = (sub / units).min(segs.len() - 1);
    let kind = if md { doc.md_kinds.get(line).copied().unwrap_or(0) } else { 0 };
    let active = (doc.buf.line_of(v.sel.start()), doc.buf.line_of(v.sel.end()));
    if md && md_rendered(kind, line, active) {
        // map the click through the rendered text back to the source
        let r = md::render(&text, kind);
        if r.rule {
            return doc.buf.line_start(line);
        }
        let mut dsegs = Vec::new();
        wrap_line(&r.display, v.wrap.cols_for(line), tab_w, &mut dsegs);
        let Some(dseg) = dsegs.get(sub / units) else { return doc.buf.line_end(line) };
        let key = md_row(cx, &r, dseg, kind, font, row_h, tab_w);
        let lx = x - v.text_rect.x - dseg.indent as f32 * cw + sx;
        let b = dseg.start + cx.text.byte_at(key, lx);
        return doc.buf.line_byte_to_pos(line, r.src_of(b));
    }
    let seg = clamp_seg(&text, &segs[si]);
    let key = shape_row(cx, doc, line, &text, &seg, tab_w, font, row_h);
    let lx = x - v.text_rect.x - seg.indent as f32 * cw + sx;
    let mut b = seg.start + cx.text.byte_at(key, lx);
    if si + 1 < segs.len() && b >= segs[si].end {
        b = prev_boundary(&text, segs[si].end).max(seg.start);
    }
    doc.buf.line_byte_to_pos(line, b)
}

/// Scroll offset for a scrollbar thumb dragged to window y (thumb centre).
pub fn scroll_for_thumb(cx: &Ctx, v: &View, y: f32, grab: f32) -> f32 {
    let rect = v.rect;
    let th = (rect.h * rect.h / (v.max_scroll + rect.h)).max(cx.px(36.0)).min(rect.h);
    let span = (rect.h - th).max(1.0);
    ((y - grab - rect.y) / span).clamp(0.0, 1.0) * v.max_scroll
}

/// Offset of window y within the scrollbar thumb, or None if outside it.
pub fn thumb_grab(cx: &Ctx, v: &View, y: f32) -> Option<f32> {
    let rect = v.rect;
    if v.max_scroll <= 0.0 {
        return None;
    }
    let th = (rect.h * rect.h / (v.max_scroll + rect.h)).max(cx.px(36.0)).min(rect.h);
    let ty = rect.y + (rect.h - th) * (v.scroll_y.v / v.max_scroll).clamp(0.0, 1.0);
    if y >= ty && y <= ty + th { Some(y - ty) } else { None }
}
