//! Modal overlays: the command palette (with fuzzy matching), the settings
//! panel and the unsaved-changes confirm dialog. Each widget owns its own
//! state, draws itself on the modal layer and reports what happened as an
//! `OverlayEvent`; the app decides what to do with it.
#![allow(dead_code)]

use crate::cmd::Cmd;
use crate::config::Config;
use crate::scene::{Quad, L_MODAL};
use crate::text::{Span, TextSystem};
use crate::theme::{knob_key, Theme};
use crate::ui::{Clip, Ctx, CursorKind, HitId, Key, Mods, TextField};
use crate::util::{hash_str, lerp, mix_id, Rect, Rgba, Smooth};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PalMode {
    Commands,
    Files,
    Themes,
    Syntax,
    Langs,
    Goto,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PalAct {
    Cmd(Cmd),
    Tab { pane: u64, idx: usize },
    Path(std::path::PathBuf),
    Theme(usize),
    /// 0 = follow the theme, n = the n-th syntax palette.
    Syntax(usize),
    Lang(String),
}

pub struct PalItem {
    pub label: String,
    pub detail: String,
    pub act: PalAct,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Choice {
    DontSave,
    Save,
    Cancel,
}

#[derive(Clone, Debug, PartialEq)]
pub enum OverlayEvent {
    None,
    /// Dismissed: Esc or a click on the backdrop.
    Close,
    /// An item was chosen; the app runs it and closes the overlay.
    Act(PalAct),
    /// The highlighted item changed (live theme preview).
    Preview(PalAct),
    /// Enter in Goto mode: the raw input, e.g. "120" or "120:8".
    Goto(String),
    Confirm(Choice),
    /// Settings: the config was modified.
    Changed,
    /// Settings: the Theme row was activated.
    PickTheme,
    /// Settings: "Reset background" pressed.
    ResetBg,
}

// ---------------------------------------------------------------- fuzzy

/// Subsequence fuzzy match, case-insensitive. Returns the score and the
/// matched char indices into `text`. An empty query matches everything with
/// score 0. A contiguous substring always beats a scattered match; within
/// each class, word starts, consecutive runs, early positions and short
/// texts score higher.
pub fn fuzzy(query: &str, text: &str) -> Option<(i32, Vec<usize>)> {
    let lower = |c: char| c.to_lowercase().next().unwrap_or(c);
    let t: Vec<char> = text.chars().collect();
    let tl: Vec<char> = t.iter().map(|c| lower(*c)).collect();
    let q_full: Vec<char> = query.trim().chars().map(lower).collect();
    if q_full.is_empty() {
        return Some((0, Vec::new()));
    }
    let n = tl.len();
    let word_start = |i: usize| -> bool {
        i == 0
            || matches!(t[i - 1], ' ' | '-' | '_' | '/' | '.' | '\\' | ':')
            || (t[i - 1].is_lowercase() && t[i].is_uppercase())
    };
    let len_pen = (n as i32 / 4).min(40);

    // 1. contiguous substring
    if q_full.len() <= n {
        let ql = q_full.len();
        let mut best: Option<(i32, usize)> = None;
        for s in 0..=(n - ql) {
            if tl[s..s + ql] == q_full[..] {
                let mut sc = 10_000 - (s as i32).min(200) - len_pen;
                if word_start(s) {
                    sc += 300;
                }
                if s == 0 {
                    sc += 100;
                }
                if ql == n {
                    sc += 200;
                }
                if best.is_none_or(|b| sc > b.0) {
                    best = Some((sc, s));
                }
            }
        }
        if let Some((sc, s)) = best {
            return Some((sc, (s..s + ql).collect()));
        }
    }

    // 2. scattered subsequence; whitespace in the query is ignored
    let q: Vec<char> = q_full.into_iter().filter(|c| !c.is_whitespace()).collect();
    let m = q.len();
    if m == 0 {
        return Some((0, Vec::new()));
    }
    if m > n {
        return None;
    }
    const NEG: i32 = i32::MIN / 2;
    // best[j*n + i]: best score for q[..=j] with q[j] matched at text index i
    let mut best = vec![NEG; m * n];
    let mut from = vec![usize::MAX; m * n];
    for j in 0..m {
        // best of row j-1 over indices <= i-2 (a non-adjacent predecessor)
        let mut run: (i32, usize) = (NEG, usize::MAX);
        for i in 0..n {
            if j > 0 && i >= 2 {
                let v = best[(j - 1) * n + i - 2];
                if v > run.0 {
                    run = (v, i - 2);
                }
            }
            if tl[i] != q[j] {
                continue;
            }
            let mut bonus = 10;
            if word_start(i) {
                bonus += if i == 0 { 40 } else { 30 };
            }
            let (score, prev) = if j == 0 {
                (bonus - (i as i32).min(15) * 2, usize::MAX)
            } else {
                let mut cand = (NEG, usize::MAX);
                if i >= 1 {
                    let v = best[(j - 1) * n + i - 1];
                    if v > NEG {
                        cand = (v + bonus + 20, i - 1);
                    }
                }
                if run.0 > NEG && run.0 + bonus - 4 > cand.0 {
                    cand = (run.0 + bonus - 4, run.1);
                }
                cand
            };
            if score > NEG {
                best[j * n + i] = score;
                from[j * n + i] = prev;
            }
        }
    }
    let mut end = usize::MAX;
    let mut top = NEG;
    for i in 0..n {
        let v = best[(m - 1) * n + i];
        if v > top {
            top = v;
            end = i;
        }
    }
    if end == usize::MAX {
        return None;
    }
    let mut idx = vec![0usize; m];
    let mut i = end;
    for j in (0..m).rev() {
        idx[j] = i;
        i = from[j * n + i];
    }
    Some((top - len_pen, idx))
}

/// Byte-range spans covering the matched chars of `label`.
fn match_spans(label: &str, matched: &[usize], color: Rgba) -> Vec<Span> {
    let mut spans: Vec<Span> = Vec::new();
    let mut mi = 0;
    for (ci, (bi, ch)) in label.char_indices().enumerate() {
        if mi < matched.len() && matched[mi] == ci {
            let end = bi + ch.len_utf8();
            match spans.last_mut() {
                Some(s) if s.end == bi => s.end = end,
                _ => spans.push(Span { start: bi, end, color, bold: true, italic: false }),
            }
            mi += 1;
        }
    }
    spans
}

fn backdrop(cx: &mut Ctx, t: f32) {
    let full = Rect::new(0.0, 0.0, cx.size.0, cx.size.1);
    cx.quad(L_MODAL, Quad::new(full, Rgba::BLACK.a(0.38 * t)));
    cx.hit(full, HitId::Backdrop, CursorKind::Default);
}

// -------------------------------------------------------------- palette

const PAL_ROWS: usize = 10;
const PAL_ROW_H: f32 = 34.0;

pub struct Palette {
    pub mode: PalMode,
    items: Vec<PalItem>,
    field: TextField,
    placeholder: String,
    /// Filtered view: (index into `items`, matched char indices of its label).
    shown: Vec<(usize, Vec<usize>)>,
    /// Index into `shown`.
    sel: usize,
    /// List scroll, in rows.
    scroll: Smooth,
    /// Rows that fit, from the last draw.
    vis: usize,
    reveal: bool,
}

impl Palette {
    pub fn new(mode: PalMode, items: Vec<PalItem>, selected: usize, placeholder: &str) -> Palette {
        let shown: Vec<(usize, Vec<usize>)> = (0..items.len()).map(|i| (i, Vec::new())).collect();
        let sel = selected.min(shown.len().saturating_sub(1));
        let first = (sel + 1).saturating_sub(PAL_ROWS) as f32;
        Palette {
            mode,
            items,
            field: TextField::default(),
            placeholder: placeholder.to_string(),
            shown,
            sel,
            scroll: Smooth::new(first),
            vis: PAL_ROWS,
            reveal: true,
        }
    }

    fn sel_item(&self) -> Option<usize> {
        self.shown.get(self.sel).map(|s| s.0)
    }

    fn sel_act(&self) -> Option<PalAct> {
        self.sel_item().map(|i| self.items[i].act.clone())
    }

    fn refilter(&mut self) {
        let q = self.field.text.clone();
        if q.trim().is_empty() {
            self.shown = (0..self.items.len()).map(|i| (i, Vec::new())).collect();
        } else {
            let mut scored: Vec<(i32, usize, Vec<usize>)> = self
                .items
                .iter()
                .enumerate()
                .filter_map(|(i, it)| fuzzy(&q, &it.label).map(|(s, m)| (s, i, m)))
                .collect();
            scored.sort_by(|a, b| b.0.cmp(&a.0));
            self.shown = scored.into_iter().map(|(_, i, m)| (i, m)).collect();
        }
        self.sel = 0;
        self.scroll.target = 0.0;
        self.reveal = true;
    }

    /// Event for a selection that may have moved off `before`.
    fn preview_if_changed(&self, before: Option<usize>) -> OverlayEvent {
        match self.sel_item() {
            Some(i) if Some(i) != before => OverlayEvent::Preview(self.items[i].act.clone()),
            _ => OverlayEvent::None,
        }
    }

    fn select(&mut self, to: usize) -> OverlayEvent {
        let before = self.sel_item();
        self.sel = to.min(self.shown.len().saturating_sub(1));
        self.reveal = true;
        self.preview_if_changed(before)
    }

    pub fn key(&mut self, key: Key, mods: Mods, text: Option<&str>, clip: &mut Clip) -> OverlayEvent {
        let n = self.shown.len();
        match key {
            Key::Escape => return OverlayEvent::Close,
            Key::Enter => {
                if self.mode == PalMode::Goto {
                    return OverlayEvent::Goto(self.field.text.clone());
                }
                return match self.sel_act() {
                    Some(a) => OverlayEvent::Act(a),
                    None => OverlayEvent::None,
                };
            }
            Key::Up if n > 0 => return self.select(if self.sel == 0 { n - 1 } else { self.sel - 1 }),
            Key::Down if n > 0 => return self.select(if self.sel + 1 >= n { 0 } else { self.sel + 1 }),
            Key::PageUp if n > 0 => return self.select(self.sel.saturating_sub(self.vis.max(2) - 1)),
            Key::PageDown if n > 0 => return self.select(self.sel + self.vis.max(2) - 1),
            Key::Home if mods.ctrl && n > 0 => return self.select(0),
            Key::End if mods.ctrl && n > 0 => return self.select(n - 1),
            Key::Up | Key::Down | Key::PageUp | Key::PageDown | Key::Tab => return OverlayEvent::None,
            _ => {}
        }
        let before = self.sel_item();
        if self.field.key(key, mods, text, clip) {
            self.refilter();
            return self.preview_if_changed(before);
        }
        OverlayEvent::None
    }

    pub fn text(&mut self, s: &str) -> OverlayEvent {
        if s.is_empty() {
            return OverlayEvent::None;
        }
        let before = self.sel_item();
        self.field.insert(s);
        self.refilter();
        self.preview_if_changed(before)
    }

    pub fn wheel(&mut self, dy_rows: f32) {
        let max = self.shown.len().saturating_sub(self.vis) as f32;
        self.scroll.target = (self.scroll.target + dy_rows).clamp(0.0, max);
    }

    pub fn hover(&mut self, hit: Option<HitId>) -> OverlayEvent {
        if let Some(HitId::PalItem(i)) = hit {
            if i < self.shown.len() && i != self.sel {
                let before = self.sel_item();
                self.sel = i;
                return self.preview_if_changed(before);
            }
        }
        OverlayEvent::None
    }

    pub fn click(&mut self, hit: Option<HitId>) -> OverlayEvent {
        match hit {
            Some(HitId::PalItem(i)) if i < self.shown.len() => {
                self.sel = i;
                OverlayEvent::Act(self.items[self.shown[i].0].act.clone())
            }
            Some(HitId::Backdrop) => OverlayEvent::Close,
            _ => OverlayEvent::None,
        }
    }

    pub fn draw(&mut self, cx: &mut Ctx, t: f32) {
        let (w, h) = cx.size;
        backdrop(cx, t);

        let pw = cx.px(640.0).min(w - cx.px(48.0)).max(cx.px(160.0));
        let input_h = cx.px(48.0);
        let row_h = cx.px(PAL_ROW_H);
        let pad = cx.px(6.0);
        let sep = cx.s.round().max(1.0);
        let top = cx.px(72.0);
        let n = self.shown.len();
        let goto = self.mode == PalMode::Goto;
        // rows that fit in the window below the input
        let fit = ((h - top - input_h - sep - pad * 2.0 - cx.px(24.0)) / row_h).floor().max(1.0) as usize;
        self.vis = PAL_ROWS.min(fit);
        let list_h = if goto { cx.px(32.0) } else { row_h * n.clamp(1, self.vis) as f32 };
        let ph = input_h + sep + pad + list_h + pad;
        let x = ((w - pw) * 0.5).round();
        let y = (top - cx.px(12.0) * (1.0 - t)).round();
        let panel = Rect::new(x, y, pw, ph);
        cx.panel(L_MODAL, panel, cx.px(12.0), t);
        cx.hit(panel, HitId::Panel, CursorKind::Default);

        // input row
        let prompt_size = (cx.ui_font * 1.3 * 2.0).round() / 2.0;
        cx.label_in(
            L_MODAL,
            "›",
            prompt_size,
            true,
            Rect::new(x + cx.px(18.0), y, cx.px(20.0), input_h),
            panel,
            cx.ui.accent.fade(t),
        );
        let fr = Rect::new(x + cx.px(42.0), y, pw - cx.px(60.0), input_h);
        cx.hit(fr, HitId::Panel, CursorKind::Text);
        self.field.draw(cx, L_MODAL, fr, true, &self.placeholder, t, hash_str("palette-caret"));
        cx.quad(L_MODAL, Quad::new(Rect::new(x + sep, y + input_h, pw - 2.0 * sep, sep), cx.ui.border.fade(t)));

        let list = Rect::new(x + pad, y + input_h + sep + pad, pw - 2.0 * pad, list_h);
        let small = cx.small_font;
        let small_h = TextSystem::label_h(small);

        if goto {
            cx.label_in(
                L_MODAL,
                "Type a line number, optionally followed by :column",
                small,
                false,
                Rect::new(list.x + cx.px(14.0), list.y, list.w, list.h),
                list,
                cx.ui.text_faint.fade(t),
            );
            return;
        }
        if n == 0 {
            cx.label_in(
                L_MODAL,
                "No results",
                cx.ui_font,
                false,
                Rect::new(list.x + cx.px(14.0), list.y, list.w, list.h),
                list,
                cx.ui.text_faint.fade(t),
            );
            return;
        }

        // scrolling
        self.sel = self.sel.min(n - 1);
        let max_scroll = n.saturating_sub(self.vis) as f32;
        if self.reveal {
            let s = self.sel as f32;
            if s < self.scroll.target {
                self.scroll.target = s;
            }
            if s + 1.0 > self.scroll.target + self.vis as f32 {
                self.scroll.target = s + 1.0 - self.vis as f32;
            }
            self.reveal = false;
        }
        self.scroll.target = self.scroll.target.clamp(0.0, max_scroll);
        if self.scroll.v > max_scroll + 1.0 {
            self.scroll.v = max_scroll;
        }
        let sc = cx.tick(&mut self.scroll, 26.0, 0.004);

        // gliding selection highlight
        let sp = cx.anim(hash_str("palette-sel"), self.sel as f32, 30.0);
        let hy = list.y + (sp - sc) * row_h;
        let hl = Rect::new(list.x, hy + cx.px(1.0), list.w, row_h - cx.px(2.0));
        cx.quad(L_MODAL, Quad::new(hl, cx.ui.accent.a(0.13 * t)).radius(cx.px(7.0)).clip(list));
        let bar_w = cx.px(2.5).max(2.0);
        cx.quad(
            L_MODAL,
            Quad::new(Rect::new(hl.x + cx.px(3.0), hl.y + cx.px(8.0), bar_w, hl.h - cx.px(16.0)), cx.ui.accent.fade(t))
                .radius(bar_w * 0.5)
                .clip(list),
        );

        // rows
        let lh = TextSystem::label_h(cx.ui_font);
        let first = sc.floor().max(0.0) as usize;
        let last = (first + self.vis + 1).min(n);
        let accent = cx.ui.accent.fade(t);
        for i in first..last {
            let ry = list.y + (i as f32 - sc) * row_h;
            if ry + row_h <= list.y || ry >= list.bottom() {
                continue;
            }
            let rr = Rect::new(list.x, ry, list.w, row_h);
            cx.hit(rr.intersect(&list), HitId::PalItem(i), CursorKind::Pointer);
            let (idx, matched) = &self.shown[i];
            let item = &self.items[*idx];
            let base = if i == self.sel { cx.ui.text } else { cx.ui.text.lerp(cx.ui.text_dim, 0.55) }.fade(t);
            let spans = match_spans(&item.label, matched, accent);
            let key = cx.text.line(&item.label, &spans, base, cx.ui_font, lh, 4);
            let lx = list.x + cx.px(16.0);
            cx.scene.text(L_MODAL, key, lx.round(), (ry + (row_h - lh) * 0.5).round(), list, base);
            if !item.detail.is_empty() {
                let room = list.right() - cx.px(14.0) - (lx + cx.text.width(key) + cx.px(24.0));
                if room > cx.px(40.0) {
                    let dk = cx.text.label_fit(&item.detail, small, false, room);
                    let dw = cx.text.width(dk);
                    cx.scene.text(
                        L_MODAL,
                        dk,
                        (list.right() - cx.px(14.0) - dw).round(),
                        (ry + (row_h - small_h) * 0.5).round(),
                        list,
                        cx.ui.text_faint.fade(t),
                    );
                }
            }
        }

        // scroll indicator
        if n > self.vis {
            let th = (list.h * self.vis as f32 / n as f32).max(cx.px(18.0));
            let ty = list.y + (list.h - th) * (sc / max_scroll.max(1.0)).clamp(0.0, 1.0);
            let bw = cx.px(3.0);
            cx.quad(
                L_MODAL,
                Quad::new(Rect::new(list.right() - bw - cx.px(1.0), ty, bw, th), cx.ui.text_faint.a(0.6 * t)).radius(bw * 0.5),
            );
        }
    }
}

// ------------------------------------------------------------- settings

#[derive(Clone, Copy, PartialEq, Debug)]
enum SliderId {
    FontSize,
    LineHeight,
    TabWidth,
    Intensity,
    Speed,
    Scale,
    Knob(usize),
    Dim,
    Vignette,
    Fps,
    Quality,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum ToggleId {
    Ligatures,
    Animations,
    Blink,
    Wrap,
    BgAnimate,
    PauseUnfocused,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Row {
    Header(&'static str),
    Theme,
    Slider(SliderId),
    Toggle(ToggleId),
    ResetBg,
}

impl Row {
    fn interactive(&self) -> bool {
        !matches!(self, Row::Header(_))
    }
    /// Height in logical pixels.
    fn height(&self) -> f32 {
        if self.interactive() { SET_ROW_H } else { SET_HEAD_H }
    }
}

const SET_ROW_H: f32 = 34.0;
const SET_HEAD_H: f32 = 30.0;
const SET_TITLE_H: f32 = 46.0;

struct SliderSpec {
    label: String,
    min: f32,
    max: f32,
    /// Keyboard step.
    step: f32,
    /// Granularity when dragged with the mouse.
    fine: f32,
    decimals: usize,
}

fn rows_for(theme: &Theme) -> Vec<Row> {
    let mut r = vec![
        Row::Header("Appearance"),
        Row::Theme,
        Row::Slider(SliderId::FontSize),
        Row::Slider(SliderId::LineHeight),
        Row::Toggle(ToggleId::Ligatures),
        Row::Toggle(ToggleId::Animations),
        Row::Toggle(ToggleId::Blink),
        Row::Header("Editor"),
        Row::Toggle(ToggleId::Wrap),
        Row::Slider(SliderId::TabWidth),
        Row::Header("Background"),
        Row::Toggle(ToggleId::BgAnimate),
        Row::Slider(SliderId::Intensity),
        Row::Slider(SliderId::Speed),
        Row::Slider(SliderId::Scale),
    ];
    for (i, k) in theme.bg.kind.knobs().iter().enumerate() {
        if k.0 != "-" {
            r.push(Row::Slider(SliderId::Knob(i)));
        }
    }
    r.extend([
        Row::Slider(SliderId::Dim),
        Row::Slider(SliderId::Vignette),
        Row::Slider(SliderId::Fps),
        Row::Slider(SliderId::Quality),
        Row::Toggle(ToggleId::PauseUnfocused),
        Row::ResetBg,
    ]);
    r
}

fn slider_spec(id: SliderId, theme: &Theme) -> SliderSpec {
    let s = |label: &str, min: f32, max: f32, step: f32, fine: f32, decimals: usize| SliderSpec {
        label: label.to_string(),
        min,
        max,
        step,
        fine,
        decimals,
    };
    match id {
        SliderId::FontSize => s("Font size", 8.0, 32.0, 0.5, 0.5, 1),
        SliderId::LineHeight => s("Line height", 1.0, 2.2, 0.05, 0.05, 2),
        SliderId::TabWidth => s("Tab width", 1.0, 8.0, 1.0, 1.0, 0),
        SliderId::Intensity => s("Intensity", 0.0, 2.0, 0.05, 0.01, 2),
        SliderId::Speed => s("Speed", 0.0, 3.0, 0.05, 0.01, 2),
        SliderId::Scale => s("Scale", 0.3, 3.0, 0.05, 0.01, 2),
        SliderId::Knob(i) => s(theme.bg.kind.knobs()[i.min(3)].0, 0.0, 1.0, 0.05, 0.01, 2),
        SliderId::Dim => s("Dim behind text", 0.0, 1.5, 0.05, 0.01, 2),
        SliderId::Vignette => s("Vignette", 0.0, 1.0, 0.05, 0.01, 2),
        SliderId::Fps => s("Frame rate", 15.0, 120.0, 5.0, 5.0, 0),
        SliderId::Quality => s("Quality", 0.25, 1.0, 0.05, 0.05, 2),
    }
}

fn slider_get(id: SliderId, cfg: &Config, theme: &Theme) -> f32 {
    match id {
        SliderId::FontSize => cfg.font_size,
        SliderId::LineHeight => cfg.line_height,
        SliderId::TabWidth => cfg.tab_width as f32,
        SliderId::Intensity => cfg.bg_for(theme).intensity,
        SliderId::Speed => cfg.bg_for(theme).speed,
        SliderId::Scale => cfg.bg_for(theme).scale,
        SliderId::Knob(i) => cfg.bg_for(theme).knobs[i.min(3)],
        SliderId::Dim => cfg.dim,
        SliderId::Vignette => cfg.vignette,
        SliderId::Fps => cfg.bg_fps as f32,
        SliderId::Quality => cfg.bg_quality,
    }
}

/// Snap `v` to a multiple of `q` inside [min, max].
fn snap(v: f32, q: f32, min: f32, max: f32) -> f32 {
    let v = if q > 0.0 { (v / q).round() * q } else { v };
    ((v.clamp(min, max)) * 10_000.0).round() / 10_000.0
}

/// Write a slider value; returns true if it changed.
fn slider_set(id: SliderId, cfg: &mut Config, theme: &Theme, v: f32) -> bool {
    let old = slider_get(id, cfg, theme);
    if (old - v).abs() < 1e-5 {
        return false;
    }
    match id {
        SliderId::FontSize => cfg.font_size = v,
        SliderId::LineHeight => cfg.line_height = v,
        SliderId::TabWidth => cfg.tab_width = v.round().max(1.0) as usize,
        SliderId::Intensity => cfg.bg.entry(theme.id.clone()).or_default().intensity = Some(v),
        SliderId::Speed => cfg.bg.entry(theme.id.clone()).or_default().speed = Some(v),
        SliderId::Scale => cfg.bg.entry(theme.id.clone()).or_default().scale = Some(v),
        SliderId::Knob(i) => {
            let name = theme.bg.kind.knobs()[i.min(3)].0;
            cfg.bg.entry(theme.id.clone()).or_default().knobs.insert(knob_key(name), v);
        }
        SliderId::Dim => cfg.dim = v,
        SliderId::Vignette => cfg.vignette = v,
        SliderId::Fps => cfg.bg_fps = v.round().max(1.0) as u32,
        SliderId::Quality => cfg.bg_quality = v,
    }
    true
}

fn toggle_label(id: ToggleId) -> &'static str {
    match id {
        ToggleId::Ligatures => "Ligatures",
        ToggleId::Animations => "Animations",
        ToggleId::Blink => "Cursor blink",
        ToggleId::Wrap => "Word wrap",
        ToggleId::BgAnimate => "Animate",
        ToggleId::PauseUnfocused => "Pause when unfocused",
    }
}

fn toggle_ref(id: ToggleId, cfg: &mut Config) -> &mut bool {
    match id {
        ToggleId::Ligatures => &mut cfg.ligatures,
        ToggleId::Animations => &mut cfg.animations,
        ToggleId::Blink => &mut cfg.cursor_blink,
        ToggleId::Wrap => &mut cfg.wrap,
        ToggleId::BgAnimate => &mut cfg.bg_animate,
        ToggleId::PauseUnfocused => &mut cfg.bg_pause_unfocused,
    }
}

fn toggle_get(id: ToggleId, cfg: &Config) -> bool {
    match id {
        ToggleId::Ligatures => cfg.ligatures,
        ToggleId::Animations => cfg.animations,
        ToggleId::Blink => cfg.cursor_blink,
        ToggleId::Wrap => cfg.wrap,
        ToggleId::BgAnimate => cfg.bg_animate,
        ToggleId::PauseUnfocused => cfg.bg_pause_unfocused,
    }
}

pub struct Settings {
    /// Selected row index.
    sel: usize,
    /// Scroll offset in logical pixels.
    scroll: Smooth,
    /// Row index of the slider being dragged.
    dragging: Option<usize>,
    reveal: bool,
    /// Slider tracks from the last draw: (row index, track rect).
    tracks: Vec<(usize, Rect)>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings::new()
    }
}

impl Settings {
    pub fn new() -> Settings {
        Settings { sel: 1, scroll: Smooth::new(0.0), dragging: None, reveal: false, tracks: Vec::new() }
    }

    /// Clamp the selection onto an interactive row.
    fn fix_sel(&mut self, rows: &[Row]) {
        self.sel = self.sel.min(rows.len().saturating_sub(1));
        if !rows[self.sel].interactive() {
            self.sel = (self.sel..rows.len())
                .chain((0..self.sel).rev())
                .find(|i| rows[*i].interactive())
                .unwrap_or(self.sel);
        }
    }

    fn step_sel(&mut self, rows: &[Row], down: bool) {
        let n = rows.len();
        let mut i = self.sel;
        for _ in 0..n {
            i = if down { (i + 1) % n } else { (i + n - 1) % n };
            if rows[i].interactive() {
                self.sel = i;
                break;
            }
        }
        self.reveal = true;
    }

    /// Select the first row equal to `row` (no-op if absent).
    fn select_row(&mut self, theme: &Theme, row: Row) {
        if let Some(i) = rows_for(theme).iter().position(|r| *r == row) {
            self.sel = i;
            self.reveal = true;
        }
    }

    fn set_from_mouse(&self, row: usize, id: SliderId, mx: f32, cfg: &mut Config, theme: &Theme) -> bool {
        let Some((_, track)) = self.tracks.iter().find(|(i, _)| *i == row) else { return false };
        let spec = slider_spec(id, theme);
        let f = ((mx - track.x) / track.w.max(1.0)).clamp(0.0, 1.0);
        let v = snap(spec.min + f * (spec.max - spec.min), spec.fine, spec.min, spec.max);
        slider_set(id, cfg, theme, v)
    }

    pub fn key(&mut self, key: Key, mods: Mods, cfg: &mut Config, theme: &Theme) -> OverlayEvent {
        let rows = rows_for(theme);
        self.fix_sel(&rows);
        let row = rows[self.sel];
        match key {
            Key::Escape => return OverlayEvent::Close,
            Key::Up => self.step_sel(&rows, false),
            Key::Down | Key::Tab => self.step_sel(&rows, !(key == Key::Tab && mods.shift)),
            Key::Home => {
                self.sel = 0;
                self.fix_sel(&rows);
                self.reveal = true;
            }
            Key::End => {
                self.sel = (0..rows.len()).rev().find(|i| rows[*i].interactive()).unwrap_or(self.sel);
                self.reveal = true;
            }
            Key::PageUp | Key::PageDown => {
                for _ in 0..6 {
                    let before = self.sel;
                    self.step_sel(&rows, key == Key::PageDown);
                    // don't wrap around on paging
                    if (key == Key::PageDown) != (self.sel > before) {
                        self.sel = before;
                        break;
                    }
                }
            }
            Key::Left | Key::Right => {
                let right = key == Key::Right;
                match row {
                    Row::Slider(id) => {
                        let spec = slider_spec(id, theme);
                        let n = if mods.shift { 5.0 } else { 1.0 };
                        let cur = slider_get(id, cfg, theme);
                        let v = snap(cur + if right { spec.step * n } else { -spec.step * n }, spec.step, spec.min, spec.max);
                        if slider_set(id, cfg, theme, v) {
                            return OverlayEvent::Changed;
                        }
                    }
                    Row::Toggle(id) => {
                        let b = toggle_ref(id, cfg);
                        *b = !*b;
                        return OverlayEvent::Changed;
                    }
                    _ => {}
                }
            }
            Key::Enter | Key::Char(' ') => match row {
                Row::Toggle(id) => {
                    let b = toggle_ref(id, cfg);
                    *b = !*b;
                    return OverlayEvent::Changed;
                }
                Row::Theme => return OverlayEvent::PickTheme,
                Row::ResetBg => return OverlayEvent::ResetBg,
                _ => {}
            },
            _ => {}
        }
        OverlayEvent::None
    }

    pub fn wheel(&mut self, dy_rows: f32) {
        // clamped against the content height at draw time
        self.scroll.target = (self.scroll.target + dy_rows * SET_ROW_H).max(0.0);
    }

    pub fn press(&mut self, cx: &Ctx, hit: Option<HitId>, cfg: &mut Config, theme: &Theme) -> OverlayEvent {
        match hit {
            Some(HitId::SetRow(i)) => {
                let rows = rows_for(theme);
                let Some(row) = rows.get(i).copied() else { return OverlayEvent::None };
                if !row.interactive() {
                    return OverlayEvent::None;
                }
                self.sel = i;
                match row {
                    Row::Slider(id) => {
                        self.dragging = Some(i);
                        if self.set_from_mouse(i, id, cx.mouse.0, cfg, theme) {
                            return OverlayEvent::Changed;
                        }
                        OverlayEvent::None
                    }
                    Row::Toggle(id) => {
                        let b = toggle_ref(id, cfg);
                        *b = !*b;
                        OverlayEvent::Changed
                    }
                    Row::Theme => OverlayEvent::PickTheme,
                    Row::ResetBg => OverlayEvent::ResetBg,
                    Row::Header(_) => OverlayEvent::None,
                }
            }
            Some(HitId::Backdrop) => OverlayEvent::Close,
            _ => OverlayEvent::None,
        }
    }

    pub fn drag(&mut self, cx: &Ctx, cfg: &mut Config, theme: &Theme) -> OverlayEvent {
        let Some(i) = self.dragging else { return OverlayEvent::None };
        let rows = rows_for(theme);
        if let Some(Row::Slider(id)) = rows.get(i).copied() {
            if self.set_from_mouse(i, id, cx.mouse.0, cfg, theme) {
                return OverlayEvent::Changed;
            }
        }
        OverlayEvent::None
    }

    pub fn release(&mut self) {
        self.dragging = None;
    }

    pub fn draw(&mut self, cx: &mut Ctx, cfg: &Config, theme: &Theme, t: f32) {
        let (w, h) = cx.size;
        backdrop(cx, t);
        let rows = rows_for(theme);
        self.fix_sel(&rows);
        let s = cx.s;
        let sep = s.round().max(1.0);

        let content: f32 = rows.iter().map(|r| r.height()).sum::<f32>() + 10.0;
        let pw = cx.px(560.0).min(w - cx.px(48.0)).max(cx.px(200.0));
        let title_h = cx.px(SET_TITLE_H);
        let ph = (title_h + sep + content * s)
            .min(h - cx.px(96.0))
            .max(title_h + sep + cx.px(SET_ROW_H) * 2.0)
            .round();
        let x = ((w - pw) * 0.5).round();
        let y = ((h - ph) * 0.5 - cx.px(12.0) * (1.0 - t)).round();
        let panel = Rect::new(x, y, pw, ph);
        cx.panel(L_MODAL, panel, cx.px(12.0), t);
        cx.hit(panel, HitId::Panel, CursorKind::Default);

        // title bar
        let title_size = (cx.ui_font * 1.15 * 2.0).round() / 2.0;
        cx.label_in(
            L_MODAL,
            "Settings",
            title_size,
            true,
            Rect::new(x + cx.px(20.0), y, pw, title_h),
            panel,
            cx.ui.text.fade(t),
        );
        let small = cx.small_font;
        let hint = "Esc to close";
        let hw = cx.label_w(hint, small, false);
        cx.label_in(
            L_MODAL,
            hint,
            small,
            false,
            Rect::new(panel.right() - cx.px(20.0) - hw, y, hw, title_h),
            panel,
            cx.ui.text_faint.fade(t),
        );
        cx.quad(L_MODAL, Quad::new(Rect::new(x + sep, y + title_h, pw - 2.0 * sep, sep), cx.ui.border.fade(t)));

        // scrolling list
        let list = Rect::new(x, y + title_h + sep, pw, ph - title_h - sep - cx.px(6.0));
        let view_h = list.h / s;
        let max_scroll = (content - view_h).max(0.0);
        if self.reveal {
            let top: f32 = rows[..self.sel].iter().map(|r| r.height()).sum();
            // keep the section header visible when selecting its first row
            let lead = if self.sel > 0 && !rows[self.sel - 1].interactive() { SET_HEAD_H } else { 4.0 };
            if top - lead < self.scroll.target {
                self.scroll.target = top - lead;
            }
            if top + SET_ROW_H + 6.0 > self.scroll.target + view_h {
                self.scroll.target = top + SET_ROW_H + 6.0 - view_h;
            }
            self.reveal = false;
        }
        self.scroll.target = self.scroll.target.clamp(0.0, max_scroll);
        if self.scroll.v > max_scroll + SET_ROW_H {
            self.scroll.v = max_scroll;
        }
        let sc = cx.tick(&mut self.scroll, 24.0, 0.3);

        self.tracks.clear();
        let ui_font = cx.ui_font;
        let ui_h = TextSystem::label_h(ui_font);
        let small_h = TextSystem::label_h(small);
        let pad = cx.px(8.0);
        let mut oy = 0.0f32;
        for (i, row) in rows.iter().enumerate() {
            let rh = (row.height() * s).round();
            let ry = (list.y + (oy - sc) * s).round();
            oy += row.height();
            if ry + rh <= list.y || ry >= list.bottom() {
                continue;
            }
            if let Row::Header(name) = row {
                cx.label(
                    L_MODAL,
                    &name.to_uppercase(),
                    small,
                    true,
                    x + cx.px(20.0),
                    ry + rh - small_h - cx.px(4.0),
                    list,
                    cx.ui.text_faint.fade(t),
                );
                continue;
            }
            let rr = Rect::new(x + pad, ry, pw - 2.0 * pad, rh);
            let id = HitId::SetRow(i);
            let hot = cx.is_hot(id) || self.dragging == Some(i);
            let selected = i == self.sel;
            cx.hit(rr.intersect(&list), id, CursorKind::Pointer);

            // row highlight
            let key = mix_id(hash_str("set-row"), i as u64);
            let sel_a = cx.anim(key, if selected { 1.0 } else { 0.0 }, 22.0);
            let hot_a = cx.anim(key ^ 0x5f, if hot { 1.0 } else { 0.0 }, 22.0);
            let hl = rr.inset_xy(0.0, cx.px(1.0));
            if hot_a > 0.01 {
                cx.quad(L_MODAL, Quad::new(hl, cx.ui.text.a(0.05 * hot_a * t)).radius(cx.px(7.0)).clip(list));
            }
            if sel_a > 0.01 {
                cx.quad(L_MODAL, Quad::new(hl, cx.ui.accent.a(0.11 * sel_a * t)).radius(cx.px(7.0)).clip(list));
            }

            let label = match row {
                Row::Theme => "Theme".to_string(),
                Row::Slider(sid) => slider_spec(*sid, theme).label,
                Row::Toggle(tid) => toggle_label(*tid).to_string(),
                Row::ResetBg => "Reset background".to_string(),
                Row::Header(_) => String::new(),
            };
            let text_col = cx.ui.text.lerp(cx.ui.text_dim, 0.35 * (1.0 - sel_a.max(hot_a))).fade(t);
            cx.label(L_MODAL, &label, ui_font, false, rr.x + cx.px(12.0), ry + (rh - ui_h) * 0.5, list, text_col);

            let right = rr.right() - cx.px(12.0);
            let cy = ry + rh * 0.5;
            match row {
                Row::Theme => {
                    let val = format!("{}  ›", theme.name);
                    let vw = cx.label_w(&val, ui_font, false);
                    cx.label(L_MODAL, &val, ui_font, false, right - vw, ry + (rh - ui_h) * 0.5, list, cx.ui.text_dim.fade(t));
                }
                Row::Slider(sid) => {
                    let spec = slider_spec(*sid, theme);
                    let v = slider_get(*sid, cfg, theme);
                    let val = format!("{:.*}", spec.decimals, v);
                    let val_w = cx.px(46.0);
                    let vw = cx.label_w(&val, small, false);
                    cx.label(L_MODAL, &val, small, false, right - vw, ry + (rh - small_h) * 0.5, list, cx.ui.text_dim.fade(t));
                    let track_w = cx.px(180.0).min(rr.w * 0.42);
                    let th = cx.px(4.0);
                    let track = Rect::new(right - val_w - track_w, (cy - th * 0.5).round(), track_w, th);
                    self.tracks.push((i, track));
                    let f = ((v - spec.min) / (spec.max - spec.min).max(1e-6)).clamp(0.0, 1.0);
                    let fa = cx.anim(key ^ 0xa1, f, 40.0);
                    cx.quad(L_MODAL, Quad::new(track, cx.ui.text.a(0.12 * t)).radius(th * 0.5).clip(list));
                    cx.quad(
                        L_MODAL,
                        Quad::new(Rect::new(track.x, track.y, track.w * fa, th), cx.ui.accent.fade(0.9 * t)).radius(th * 0.5).clip(list),
                    );
                    let grow = cx.anim(key ^ 0xb2, if hot { 1.0 } else { 0.0 }, 24.0);
                    let r = cx.px(6.0) + cx.px(1.5) * grow;
                    let tx = track.x + track.w * fa;
                    if grow > 0.01 {
                        cx.quad(
                            L_MODAL,
                            Quad::new(Rect::new(tx - r, cy - r, 2.0 * r, 2.0 * r).inset(-cx.px(3.0)), cx.ui.accent.a(0.3 * grow * t))
                                .radius(r + cx.px(3.0))
                                .soft(cx.px(4.0))
                                .clip(list),
                        );
                    }
                    cx.quad(L_MODAL, Quad::new(Rect::new(tx - r, cy - r, 2.0 * r, 2.0 * r), cx.ui.text.fade(t)).radius(r).clip(list));
                }
                Row::Toggle(tid) => {
                    let on = toggle_get(*tid, cfg);
                    let a = cx.anim(key ^ 0xc3, if on { 1.0 } else { 0.0 }, 20.0);
                    let (tw, th) = (cx.px(38.0), cx.px(20.0));
                    let pill = Rect::new(right - tw, (cy - th * 0.5).round(), tw, th);
                    let off_col = cx.ui.text.a(0.14);
                    let on_col = cx.ui.accent.a(0.85);
                    cx.quad(L_MODAL, Quad::new(pill, off_col.lerp(on_col, a).fade(t)).radius(th * 0.5).clip(list));
                    let r = th * 0.5 - cx.px(3.0);
                    let tx = lerp(pill.x + th * 0.5, pill.right() - th * 0.5, a);
                    let thumb = cx.ui.text_dim.lerp(cx.ui.bg, a).lerp(cx.ui.text, 1.0 - a * 0.85);
                    cx.quad(L_MODAL, Quad::new(Rect::new(tx - r, cy - r, 2.0 * r, 2.0 * r), thumb.fade(t)).radius(r).clip(list));
                }
                Row::ResetBg => {
                    let txt = "Reset";
                    let bw = cx.label_w(txt, small, false) + cx.px(24.0);
                    let bh = cx.px(24.0);
                    let b = Rect::new(right - bw, (cy - bh * 0.5).round(), bw, bh);
                    cx.quad(
                        L_MODAL,
                        Quad::new(b, cx.ui.text.a((0.05 + 0.07 * hot_a) * t))
                            .radius(bh * 0.5)
                            .border(sep, cx.ui.border.lerp(cx.ui.accent, hot_a.max(sel_a) * 0.8).fade(t))
                            .clip(list),
                    );
                    let lw = cx.label_w(txt, small, false);
                    cx.label(L_MODAL, txt, small, false, b.x + (b.w - lw) * 0.5, b.y + (b.h - small_h) * 0.5, list, cx.ui.text.fade(t));
                }
                Row::Header(_) => {}
            }
        }

        // scroll indicator
        if max_scroll > 0.0 {
            let th = (list.h * view_h / content).max(cx.px(24.0));
            let ty = list.y + (list.h - th) * (sc / max_scroll).clamp(0.0, 1.0);
            let bw = cx.px(3.0);
            cx.quad(
                L_MODAL,
                Quad::new(Rect::new(panel.right() - bw - cx.px(3.0), ty, bw, th), cx.ui.text_faint.a(0.6 * t)).radius(bw * 0.5),
            );
        }
    }
}

// -------------------------------------------------------------- confirm

const CONFIRM_LABELS: [&str; 3] = ["Don't Save", "Save", "Cancel"];

pub struct Confirm {
    pub pane: u64,
    pub doc: u64,
    pub title: String,
    /// Focused button: 0 = Don't Save, 1 = Save, 2 = Cancel.
    focus: usize,
}

fn choice(i: usize) -> Choice {
    match i {
        0 => Choice::DontSave,
        1 => Choice::Save,
        _ => Choice::Cancel,
    }
}

impl Confirm {
    pub fn new(pane: u64, doc: u64, title: String) -> Confirm {
        Confirm { pane, doc, title, focus: 0 }
    }

    pub fn key(&mut self, key: Key, mods: Mods) -> OverlayEvent {
        match key {
            Key::Enter => return OverlayEvent::Confirm(choice(self.focus)),
            Key::Escape => return OverlayEvent::Confirm(Choice::Cancel),
            Key::Left => self.focus = (self.focus + 2) % 3,
            Key::Right => self.focus = (self.focus + 1) % 3,
            Key::Tab => self.focus = if mods.shift { (self.focus + 2) % 3 } else { (self.focus + 1) % 3 },
            Key::Char(c) if !mods.alt => match (c.to_ascii_lowercase(), mods.ctrl) {
                // pressing the close shortcut again closes without saving; plain
                // letters and Space never discard, so stray typing is harmless
                ('w', true) => return OverlayEvent::Confirm(Choice::DontSave),
                ('s', _) => return OverlayEvent::Confirm(Choice::Save),
                ('c', false) => return OverlayEvent::Confirm(Choice::Cancel),
                _ => {}
            },
            _ => {}
        }
        OverlayEvent::None
    }

    pub fn hover(&mut self, hit: Option<HitId>) {
        if let Some(HitId::Button(i)) = hit {
            if (i as usize) < 3 {
                self.focus = i as usize;
            }
        }
    }

    pub fn click(&mut self, hit: Option<HitId>) -> OverlayEvent {
        match hit {
            Some(HitId::Button(i)) if (i as usize) < 3 => {
                self.focus = i as usize;
                OverlayEvent::Confirm(choice(i as usize))
            }
            Some(HitId::Backdrop) => OverlayEvent::Confirm(Choice::Cancel),
            _ => OverlayEvent::None,
        }
    }

    pub fn draw(&mut self, cx: &mut Ctx, t: f32) {
        let (w, h) = cx.size;
        backdrop(cx, t);
        let sep = cx.s.round().max(1.0);
        let pw = cx.px(440.0).min(w - cx.px(48.0)).max(cx.px(220.0));
        let pad = cx.px(22.0);
        let title_size = (cx.ui_font * 1.08 * 2.0).round() / 2.0;
        let th = TextSystem::label_h(title_size);
        let small = cx.small_font;
        let sh = TextSystem::label_h(small);
        let btn_h = cx.px(32.0);
        let ph = (pad + th + cx.px(8.0) + sh + cx.px(22.0) + btn_h + cx.px(18.0)).round();
        let x = ((w - pw) * 0.5).round();
        let y = ((h - ph) * 0.42 - cx.px(12.0) * (1.0 - t)).round();
        let panel = Rect::new(x, y, pw, ph);
        cx.panel(L_MODAL, panel, cx.px(12.0), t);
        cx.hit(panel, HitId::Panel, CursorKind::Default);

        // question, with the document name shortened to fit
        let cell = cx.text.cell_w(title_size);
        let max_chars = ((pw - 2.0 * pad) / cell).floor().max(8.0) as usize;
        let fixed = "Save changes to “”?".chars().count();
        let room = max_chars.saturating_sub(fixed).max(4);
        let mut name: String = self.title.chars().take(room).collect();
        if self.title.chars().count() > room {
            name.pop();
            name.push('…');
        }
        let line = format!("Save changes to “{name}”?");
        cx.label(L_MODAL, &line, title_size, true, x + pad, y + pad, panel, cx.ui.text.fade(t));
        cx.label(
            L_MODAL,
            "Your changes will be lost if you don't save them.",
            small,
            false,
            x + pad,
            y + pad + th + cx.px(8.0),
            panel,
            cx.ui.text_dim.fade(t),
        );

        // buttons, right aligned
        let ui_font = cx.ui_font;
        let gap = cx.px(8.0);
        let widths: Vec<f32> = CONFIRM_LABELS.iter().map(|l| (cx.label_w(l, ui_font, false) + cx.px(28.0)).round()).collect();
        let total: f32 = widths.iter().sum::<f32>() + gap * 2.0;
        let by = panel.bottom() - cx.px(18.0) - btn_h;
        let mut bx = panel.right() - pad - total;
        let mut rects = [Rect::ZERO; 3];
        for i in 0..3 {
            rects[i] = Rect::new(bx.round(), by.round(), widths[i], btn_h);
            bx += widths[i] + gap;
        }
        let radius = cx.px(8.0);
        for i in 0..3 {
            let id = HitId::Button(i as u8);
            let hot = cx.is_hot(id);
            let ha = cx.anim(mix_id(hash_str("confirm-btn"), i as u64), if hot { 1.0 } else { 0.0 }, 22.0);
            cx.quad(
                L_MODAL,
                Quad::new(rects[i], cx.ui.text.a((0.05 + 0.06 * ha) * t)).radius(radius).border(sep, cx.ui.border.fade(t)),
            );
            cx.hit(rects[i], id, CursorKind::Pointer);
        }
        // focus ring glides between buttons
        let f = rects[self.focus];
        let fx = cx.anim(hash_str("confirm-ring-x"), f.x, 28.0);
        let fw = cx.anim(hash_str("confirm-ring-w"), f.w, 28.0);
        let ring = Rect::new(fx, f.y, fw, f.h);
        cx.quad(L_MODAL, Quad::new(ring.inset(-cx.px(2.0)), cx.ui.accent.a(0.22 * t)).radius(radius + cx.px(2.0)).soft(cx.px(6.0)));
        cx.quad(
            L_MODAL,
            Quad::new(ring, cx.ui.accent.a(0.10 * t)).radius(radius).border((1.5 * cx.s).max(1.0), cx.ui.accent.fade(t)),
        );
        for i in 0..3 {
            let col = if i == self.focus { cx.ui.text } else { cx.ui.text_dim };
            cx.label_mid(L_MODAL, CONFIRM_LABELS[i], ui_font, i == self.focus, rects[i], col.fade(t));
        }
    }
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    fn score(q: &str, t: &str) -> i32 {
        fuzzy(q, t).map(|r| r.0).unwrap_or(i32::MIN)
    }

    #[test]
    fn fuzzy_basics() {
        assert_eq!(fuzzy("", "anything"), Some((0, vec![])));
        assert_eq!(fuzzy("   ", "anything"), Some((0, vec![])));
        assert!(fuzzy("xyz", "Split Right").is_none());
        assert!(fuzzy("splitrightx", "Split Right").is_none());
        assert!(fuzzy("longer than text", "short").is_none());
        // case-insensitive both ways
        assert!(fuzzy("SPLIT", "split right").is_some());
        assert!(fuzzy("split", "SPLIT RIGHT").is_some());
    }

    #[test]
    fn fuzzy_indices_valid_and_increasing() {
        for (q, t) in [
            ("sr", "Split Right"),
            ("clot", "Close Other Tabs"),
            ("thm", "Change Theme…"),
            ("gtl", "Go to Line…"),
            ("split", "Split Right"),
            ("é", "café olé"),
            ("tw", "Toggle Word Wrap"),
        ] {
            let (_, idx) = fuzzy(q, t).unwrap_or_else(|| panic!("{q} should match {t}"));
            let n = t.chars().count();
            let qn = q.chars().filter(|c| !c.is_whitespace()).count();
            assert_eq!(idx.len(), qn, "{q} / {t}");
            for w in idx.windows(2) {
                assert!(w[0] < w[1], "{q} / {t}: {idx:?}");
            }
            assert!(idx.iter().all(|i| *i < n));
            // every index points at the matching char
            let tc: Vec<char> = t.chars().collect();
            let qc: Vec<char> = q.chars().filter(|c| !c.is_whitespace()).collect();
            for (k, i) in idx.iter().enumerate() {
                assert_eq!(tc[*i].to_lowercase().next(), qc[k].to_lowercase().next());
            }
        }
    }

    #[test]
    fn fuzzy_ranking() {
        // contiguous substring beats a scattered match
        assert!(score("spl", "Split Right") > score("spl", "Stop Playing Loud"));
        assert!(score("save", "Save As…") > score("save", "Split And Move"));
        // even a long scattered match never beats a substring
        assert!(score("toggle", "x toggle") > score("toggle", "Tab Or Go Go Left End"));
        // word start beats mid-word
        assert!(score("ri", "Split Right") > score("ri", "String"));
        assert!(score("sr", "Split Right") > score("sr", "Close Others"));
        // prefers word starts over the earliest occurrence
        let (_, idx) = fuzzy("sr", "Reset Search Results").unwrap();
        assert_eq!(idx, vec![6, 13]);
        // consecutive run beats a spread-out one
        assert!(score("abc", "xabcx") > score("abc", "xaxbxcx"));
        // exact / prefix / shorter wins
        assert!(score("save", "Save") > score("save", "Save All"));
        assert!(score("theme", "Themes") > score("theme", "Change Theme…"));
        // multi-word query with a space still matches as a subsequence
        assert!(fuzzy("split down", "Split Pane Down").is_some());
    }

    fn items() -> Vec<PalItem> {
        [("Split Right", Cmd::SplitRight), ("Split Down", Cmd::SplitDown), ("Close Tab", Cmd::CloseTab), ("Save", Cmd::Save)]
            .into_iter()
            .map(|(l, c)| PalItem { label: l.to_string(), detail: c.keys().to_string(), act: PalAct::Cmd(c) })
            .collect()
    }

    fn type_str(p: &mut Palette, clip: &mut Clip, s: &str) -> OverlayEvent {
        let mut last = OverlayEvent::None;
        for c in s.chars() {
            last = p.key(Key::Char(c), Mods::default(), Some(&c.to_string()), clip);
        }
        last
    }

    #[test]
    fn palette_filter_and_enter() {
        let mut clip = Clip::new(false);
        let mut p = Palette::new(PalMode::Commands, items(), 0, "Type a command");
        assert_eq!(p.shown.len(), 4);
        // first keystroke moves the selection to the best match and previews it
        let ev = p.key(Key::Char('s'), Mods::default(), Some("s"), &mut clip);
        assert_eq!(ev, OverlayEvent::Preview(PalAct::Cmd(Cmd::Save)));
        type_str(&mut p, &mut clip, "av");
        assert_eq!(p.shown.len(), 1);
        assert_eq!(p.sel, 0);
        assert_eq!(p.key(Key::Enter, Mods::default(), None, &mut clip), OverlayEvent::Act(PalAct::Cmd(Cmd::Save)));
        // clearing the query restores everything in original order
        for _ in 0..3 {
            p.key(Key::Backspace, Mods::default(), None, &mut clip);
        }
        assert_eq!(p.shown.iter().map(|s| s.0).collect::<Vec<_>>(), vec![0, 1, 2, 3]);
        assert_eq!(p.sel, 0);
        // no match: Enter does nothing
        type_str(&mut p, &mut clip, "zzz");
        assert!(p.shown.is_empty());
        assert_eq!(p.key(Key::Enter, Mods::default(), None, &mut clip), OverlayEvent::None);
        assert_eq!(p.key(Key::Down, Mods::default(), None, &mut clip), OverlayEvent::None);
    }

    #[test]
    fn palette_navigation() {
        let mut clip = Clip::new(false);
        let mut p = Palette::new(PalMode::Commands, items(), 0, "");
        let m = Mods::default();
        assert_eq!(p.key(Key::Up, m, None, &mut clip), OverlayEvent::Preview(PalAct::Cmd(Cmd::Save)));
        assert_eq!(p.sel, 3);
        assert_eq!(p.key(Key::Down, m, None, &mut clip), OverlayEvent::Preview(PalAct::Cmd(Cmd::SplitRight)));
        assert_eq!(p.sel, 0);
        p.key(Key::Down, m, None, &mut clip);
        assert_eq!(p.key(Key::Enter, m, None, &mut clip), OverlayEvent::Act(PalAct::Cmd(Cmd::SplitDown)));
        assert_eq!(p.key(Key::End, Mods { ctrl: true, ..m }, None, &mut clip), OverlayEvent::Preview(PalAct::Cmd(Cmd::Save)));
        p.key(Key::PageUp, m, None, &mut clip);
        assert_eq!(p.sel, 0);
        assert_eq!(p.key(Key::Escape, m, None, &mut clip), OverlayEvent::Close);
        // initial selection is honoured and clamped
        let p = Palette::new(PalMode::Themes, items(), 2, "");
        assert_eq!(p.sel, 2);
        let p = Palette::new(PalMode::Themes, items(), 99, "");
        assert_eq!(p.sel, 3);
    }

    #[test]
    fn palette_mouse() {
        let mut p = Palette::new(PalMode::Commands, items(), 0, "");
        assert_eq!(p.hover(Some(HitId::PalItem(0))), OverlayEvent::None);
        assert_eq!(p.hover(Some(HitId::PalItem(2))), OverlayEvent::Preview(PalAct::Cmd(Cmd::CloseTab)));
        assert_eq!(p.hover(Some(HitId::PalItem(9))), OverlayEvent::None);
        assert_eq!(p.click(Some(HitId::Panel)), OverlayEvent::None);
        assert_eq!(p.click(Some(HitId::PalItem(1))), OverlayEvent::Act(PalAct::Cmd(Cmd::SplitDown)));
        assert_eq!(p.click(Some(HitId::Backdrop)), OverlayEvent::Close);
        assert_eq!(p.click(None), OverlayEvent::None);
    }

    #[test]
    fn palette_goto_and_ime() {
        let mut clip = Clip::new(false);
        let mut p = Palette::new(PalMode::Goto, Vec::new(), 0, "Line");
        type_str(&mut p, &mut clip, "12:3");
        assert_eq!(p.key(Key::Enter, Mods::default(), None, &mut clip), OverlayEvent::Goto("12:3".to_string()));
        assert_eq!(p.key(Key::Down, Mods::default(), None, &mut clip), OverlayEvent::None);

        let mut p = Palette::new(PalMode::Commands, items(), 0, "");
        assert_eq!(p.text("close"), OverlayEvent::Preview(PalAct::Cmd(Cmd::CloseTab)));
        assert_eq!(p.shown.len(), 1);
    }

    #[test]
    fn confirm_keys() {
        let m = Mods::default();
        let ctrl = Mods { ctrl: true, ..m };
        let new = || Confirm::new(1, 2, "notes.txt".to_string());
        assert_eq!(new().key(Key::Enter, m), OverlayEvent::Confirm(Choice::DontSave));
        assert_eq!(new().key(Key::Char(' '), m), OverlayEvent::None);
        assert_eq!(new().key(Key::Char('w'), ctrl), OverlayEvent::Confirm(Choice::DontSave));
        assert_eq!(new().key(Key::Char('W'), ctrl), OverlayEvent::Confirm(Choice::DontSave));
        assert_eq!(new().key(Key::Char('w'), m), OverlayEvent::None);
        assert_eq!(new().key(Key::Char('d'), m), OverlayEvent::None);
        assert_eq!(new().key(Key::Char('s'), m), OverlayEvent::Confirm(Choice::Save));
        assert_eq!(new().key(Key::Char('s'), ctrl), OverlayEvent::Confirm(Choice::Save));
        assert_eq!(new().key(Key::Char('c'), m), OverlayEvent::Confirm(Choice::Cancel));
        assert_eq!(new().key(Key::Escape, m), OverlayEvent::Confirm(Choice::Cancel));

        let mut c = new();
        assert_eq!(c.key(Key::Right, m), OverlayEvent::None);
        assert_eq!(c.key(Key::Enter, m), OverlayEvent::Confirm(Choice::Save));
        let mut c = new();
        c.key(Key::Left, m);
        assert_eq!(c.key(Key::Enter, m), OverlayEvent::Confirm(Choice::Cancel));
        let mut c = new();
        c.key(Key::Tab, m);
        c.key(Key::Tab, m);
        c.key(Key::Tab, Mods { shift: true, ..m });
        assert_eq!(c.key(Key::Enter, m), OverlayEvent::Confirm(Choice::Save));
    }

    #[test]
    fn confirm_mouse() {
        let mut c = Confirm::new(1, 2, "x".to_string());
        c.hover(Some(HitId::Button(2)));
        assert_eq!(c.key(Key::Enter, Mods::default()), OverlayEvent::Confirm(Choice::Cancel));
        assert_eq!(c.click(Some(HitId::Button(1))), OverlayEvent::Confirm(Choice::Save));
        assert_eq!(c.click(Some(HitId::Button(0))), OverlayEvent::Confirm(Choice::DontSave));
        assert_eq!(c.click(Some(HitId::Backdrop)), OverlayEvent::Confirm(Choice::Cancel));
        assert_eq!(c.click(Some(HitId::Panel)), OverlayEvent::None);
    }

    #[test]
    fn settings_keys() {
        let theme = Theme::parse("t", "").unwrap();
        let mut cfg = Config::default();
        let m = Mods::default();
        let mut s = Settings::new();

        // starts on the Theme row
        assert_eq!(s.key(Key::Enter, m, &mut cfg, &theme), OverlayEvent::PickTheme);
        // Down -> Font size; Right bumps it by one step
        assert_eq!(s.key(Key::Down, m, &mut cfg, &theme), OverlayEvent::None);
        let before = cfg.font_size;
        assert_eq!(s.key(Key::Right, m, &mut cfg, &theme), OverlayEvent::Changed);
        assert!((cfg.font_size - (before + 0.5)).abs() < 1e-4);
        assert_eq!(s.key(Key::Left, Mods { shift: true, ..m }, &mut cfg, &theme), OverlayEvent::Changed);
        assert!((cfg.font_size - (before - 2.0)).abs() < 1e-4);
        // clamped at the ends: no change reported
        cfg.font_size = 32.0;
        assert_eq!(s.key(Key::Right, m, &mut cfg, &theme), OverlayEvent::None);

        // toggles
        s.select_row(&theme, Row::Toggle(ToggleId::Wrap));
        let w = cfg.wrap;
        assert_eq!(s.key(Key::Enter, m, &mut cfg, &theme), OverlayEvent::Changed);
        assert_eq!(cfg.wrap, !w);
        assert_eq!(s.key(Key::Char(' '), m, &mut cfg, &theme), OverlayEvent::Changed);
        assert_eq!(cfg.wrap, w);
        assert_eq!(s.key(Key::Right, m, &mut cfg, &theme), OverlayEvent::Changed);
        assert_eq!(cfg.wrap, !w);

        // per-theme background values land in the override table
        s.select_row(&theme, Row::Slider(SliderId::Intensity));
        assert!(cfg.bg.is_empty());
        assert_eq!(s.key(Key::Left, m, &mut cfg, &theme), OverlayEvent::Changed);
        let got = cfg.bg["t"].intensity.unwrap();
        assert!((got - 0.95).abs() < 1e-4, "{got}");
        assert!((cfg.bg_for(&theme).intensity - 0.95).abs() < 1e-4);

        // knobs are stored by name; unnamed ("-") knobs get no row
        let rows = rows_for(&theme);
        assert!(rows.contains(&Row::Slider(SliderId::Knob(0))));
        assert!(!rows.contains(&Row::Slider(SliderId::Knob(2))));
        s.select_row(&theme, Row::Slider(SliderId::Knob(0)));
        assert_eq!(s.key(Key::Right, m, &mut cfg, &theme), OverlayEvent::Changed);
        let name = knob_key(theme.bg.kind.knobs()[0].0);
        assert!((cfg.bg["t"].knobs[&name] - 0.55).abs() < 1e-4);
        assert!((cfg.bg_for(&theme).knobs[0] - 0.55).abs() < 1e-4);

        // integer-valued sliders
        s.select_row(&theme, Row::Slider(SliderId::TabWidth));
        cfg.tab_width = 4;
        s.key(Key::Right, m, &mut cfg, &theme);
        assert_eq!(cfg.tab_width, 5);
        s.select_row(&theme, Row::Slider(SliderId::Fps));
        cfg.bg_fps = 30;
        s.key(Key::Right, m, &mut cfg, &theme);
        assert_eq!(cfg.bg_fps, 35);

        // buttons and dismissal
        s.select_row(&theme, Row::ResetBg);
        assert_eq!(s.key(Key::Enter, m, &mut cfg, &theme), OverlayEvent::ResetBg);
        assert_eq!(s.key(Key::Escape, m, &mut cfg, &theme), OverlayEvent::Close);
    }

    #[test]
    fn settings_navigation_skips_headers() {
        let theme = Theme::parse("t", "").unwrap();
        let mut cfg = Config::default();
        let m = Mods::default();
        let rows = rows_for(&theme);
        let mut s = Settings::new();
        // walk all the way round: never rests on a header, visits every interactive row
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..rows.len() * 2 {
            assert!(rows[s.sel].interactive());
            seen.insert(s.sel);
            s.key(Key::Down, m, &mut cfg, &theme);
        }
        assert_eq!(seen.len(), rows.iter().filter(|r| r.interactive()).count());
        s.key(Key::Home, m, &mut cfg, &theme);
        assert_eq!(rows[s.sel], Row::Theme);
        s.key(Key::End, m, &mut cfg, &theme);
        assert_eq!(rows[s.sel], Row::ResetBg);
        s.key(Key::Up, m, &mut cfg, &theme);
        assert_eq!(rows[s.sel], Row::Toggle(ToggleId::PauseUnfocused));
        // paging stops at the ends instead of wrapping
        s.key(Key::End, m, &mut cfg, &theme);
        s.key(Key::PageDown, m, &mut cfg, &theme);
        assert_eq!(rows[s.sel], Row::ResetBg);
        s.key(Key::Home, m, &mut cfg, &theme);
        s.key(Key::PageUp, m, &mut cfg, &theme);
        assert_eq!(rows[s.sel], Row::Theme);
    }

    /// Draw every widget for a few frames at several window sizes and open
    /// amounts; catches panics and checks the hit regions that get registered.
    #[test]
    fn draw_smoke() {
        let theme = Theme::parse("t", "").unwrap();
        let cfg = Config::default();
        let mut cx = Ctx::new(TextSystem::new(false), theme.ui.clone(), theme.syntax.clone());
        cx.mouse_inside = true;
        let mut clip = Clip::new(false);
        let many: Vec<PalItem> = (0..40)
            .map(|i| PalItem { label: format!("Item number {i} — ünïcödé"), detail: "~/some/long/path/to/a/file.rs".into(), act: PalAct::Theme(i) })
            .collect();
        let mut pal = Palette::new(PalMode::Themes, many, 25, "Search");
        let mut goto = Palette::new(PalMode::Goto, Vec::new(), 0, "Line");
        let mut empty = Palette::new(PalMode::Commands, items(), 0, "");
        type_str(&mut empty, &mut clip, "zzzz");
        let mut set = Settings::new();
        let mut con = Confirm::new(1, 2, "a very long document title that will not fit in the dialog at all.txt".into());
        let mut now = 0.0;
        for (w, h, scale) in [(1280.0, 800.0, 1.0), (300.0, 200.0, 1.0), (2560.0, 1440.0, 2.0), (60.0, 40.0, 1.5)] {
            cx.size = (w, h);
            cx.set_metrics(scale, 14.0, 1.5);
            for t in [0.0, 0.4, 1.0, 1.0] {
                now += 0.016;
                cx.begin_frame(now, 0.016);
                pal.draw(&mut cx, t);
                cx.end_frame();
                assert!(cx.hits.iter().any(|h| h.id == HitId::Backdrop));
                assert!(cx.hits.iter().any(|h| h.id == HitId::Panel));

                cx.begin_frame(now, 0.016);
                goto.draw(&mut cx, t);
                empty.draw(&mut cx, t);
                cx.end_frame();

                cx.begin_frame(now, 0.016);
                set.draw(&mut cx, &cfg, &theme, t);
                cx.end_frame();

                cx.begin_frame(now, 0.016);
                con.draw(&mut cx, t);
                cx.end_frame();
                for b in 0..3u8 {
                    assert!(cx.hits.iter().any(|h| h.id == HitId::Button(b)));
                }
            }
            // keep things moving between sizes
            pal.key(Key::Down, Mods::default(), None, &mut clip);
            pal.wheel(3.0);
            set.key(Key::End, Mods::default(), &mut Config::default(), &theme);
            set.wheel(-2.0);
            con.key(Key::Right, Mods::default());
        }
        // at a normal size the palette lists rows and the selected one is on screen
        cx.size = (1280.0, 800.0);
        cx.set_metrics(1.0, 14.0, 1.5);
        let mut pal = Palette::new(PalMode::Themes, (0..40).map(|i| PalItem { label: format!("T{i}"), detail: String::new(), act: PalAct::Theme(i) }).collect(), 25, "");
        cx.begin_frame(now + 1.0, 0.016);
        pal.draw(&mut cx, 1.0);
        cx.end_frame();
        assert!(cx.hits.iter().any(|h| h.id == HitId::PalItem(25)), "initially selected row should be visible");
        assert!(!cx.hits.iter().any(|h| h.id == HitId::PalItem(0)));
        // settings rows register hits and slider tracks
        cx.begin_frame(now + 2.0, 0.016);
        set = Settings::new();
        set.draw(&mut cx, &cfg, &theme, 1.0);
        cx.end_frame();
        assert!(cx.hits.iter().any(|h| matches!(h.id, HitId::SetRow(_))));
        assert!(!set.tracks.is_empty());
    }

    #[test]
    fn settings_mouse_without_layout_is_safe() {
        let theme = Theme::parse("t", "").unwrap();
        let mut cfg = Config::default();
        let cx_theme = Theme::parse("t", "").unwrap();
        let cx = Ctx::new(TextSystem::new(false), cx_theme.ui.clone(), cx_theme.syntax.clone());
        let mut s = Settings::new();
        let rows = rows_for(&theme);
        let wrap_row = rows.iter().position(|r| *r == Row::Toggle(ToggleId::Wrap)).unwrap();
        let w = cfg.wrap;
        assert_eq!(s.press(&cx, Some(HitId::SetRow(wrap_row)), &mut cfg, &theme), OverlayEvent::Changed);
        assert_eq!(cfg.wrap, !w);
        // a slider pressed before any draw has no track yet: nothing changes, drag is armed
        let font_row = rows.iter().position(|r| *r == Row::Slider(SliderId::FontSize)).unwrap();
        assert_eq!(s.press(&cx, Some(HitId::SetRow(font_row)), &mut cfg, &theme), OverlayEvent::None);
        assert_eq!(s.dragging, Some(font_row));
        assert_eq!(s.drag(&cx, &mut cfg, &theme), OverlayEvent::None);
        s.release();
        assert_eq!(s.dragging, None);
        // with a known track the mouse position maps onto the range
        s.tracks.push((font_row, Rect::new(100.0, 0.0, 200.0, 4.0)));
        let mut cx = cx;
        cx.mouse = (200.0, 2.0);
        assert_eq!(s.press(&cx, Some(HitId::SetRow(font_row)), &mut cfg, &theme), OverlayEvent::Changed);
        assert!((cfg.font_size - 20.0).abs() < 1e-4);
        cx.mouse = (1000.0, 2.0);
        assert_eq!(s.drag(&cx, &mut cfg, &theme), OverlayEvent::Changed);
        assert!((cfg.font_size - 32.0).abs() < 1e-4);
        // headers and stray hits do nothing; the backdrop closes
        assert_eq!(s.press(&cx, Some(HitId::SetRow(0)), &mut cfg, &theme), OverlayEvent::None);
        assert_eq!(s.press(&cx, Some(HitId::Panel), &mut cfg, &theme), OverlayEvent::None);
        assert_eq!(s.press(&cx, Some(HitId::Backdrop), &mut cfg, &theme), OverlayEvent::Close);
    }
}
