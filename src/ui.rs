//! Immediate-mode UI plumbing: the per-frame context (scene, text, hit
//! regions, keyed animations), input types and the single-line text field.

use crate::scene::{Quad, Scene};
use crate::text::TextSystem;
use crate::theme::{SyntaxColors, UiColors};
use crate::util::{Rect, Rgba, Smooth};
use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Mods {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    Char(char),
    Enter,
    Tab,
    Backspace,
    Delete,
    Escape,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    F(u8),
}

#[derive(Clone, Debug)]
pub enum Input {
    Key { key: Key, mods: Mods, text: Option<String>, repeat: bool },
    /// Committed text that did not come from a key press (IME).
    Text(String),
    MouseMove(f32, f32),
    MouseDown { button: u8, mods: Mods },
    MouseUp { button: u8 },
    MouseLeft,
    /// `pixels` is true for touchpad-style precise deltas.
    Wheel { dx: f32, dy: f32, pixels: bool, mods: Mods },
    Focus(bool),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum CursorKind {
    #[default]
    Default,
    Text,
    Pointer,
    ResizeH,
    ResizeV,
}

/// Everything the mouse can land on.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum HitId {
    Tab(u64, usize),
    TabClose(u64, usize),
    NewTab(u64),
    Strip(u64),
    Editor(u64),
    Scrollbar(u64),
    Divider(u32),
    Status(u8),
    Backdrop,
    Panel,
    PalItem(usize),
    FindField(u8),
    FindBtn(u8),
    SetRow(usize),
    Button(u8),
    Welcome(u8),
}

#[derive(Clone, Copy)]
pub struct Hit {
    pub rect: Rect,
    pub id: HitId,
    pub cursor: CursorKind,
}

pub struct Ctx {
    pub scene: Scene,
    pub text: TextSystem,
    pub hits: Vec<Hit>,
    pub ui: UiColors,
    pub syn: SyntaxColors,
    /// UI scale factor (physical pixels per logical pixel).
    pub s: f32,
    pub size: (f32, f32),
    pub mouse: (f32, f32),
    pub mouse_inside: bool,
    /// The window has keyboard focus (carets only blink while it does).
    pub window_focused: bool,
    /// Topmost hit under the mouse, from the previous frame's regions.
    pub hot: Option<HitId>,
    pub now: f64,
    pub dt: f32,
    /// Set by anything that is still moving; drives continuous redraw.
    pub animating: bool,
    /// Earliest time a redraw is needed even if nothing is animating.
    pub wake: Option<f64>,
    pub anims_on: bool,
    anims: HashMap<u64, (Smooth, u64)>,
    frame: u64,

    // editor metrics, physical pixels
    pub font: f32,
    pub line_h: f32,
    pub cell: f32,
    pub ui_font: f32,
    pub small_font: f32,
}

impl Ctx {
    pub fn new(text: TextSystem, ui: UiColors, syn: SyntaxColors) -> Ctx {
        Ctx {
            scene: Scene::default(),
            text,
            hits: Vec::new(),
            ui,
            syn,
            s: 1.0,
            size: (1280.0, 800.0),
            mouse: (-1.0, -1.0),
            mouse_inside: false,
            window_focused: true,
            hot: None,
            now: 0.0,
            dt: 0.0,
            animating: false,
            wake: None,
            anims_on: true,
            anims: HashMap::new(),
            frame: 0,
            font: 14.0,
            line_h: 22.0,
            cell: 8.4,
            ui_font: 13.0,
            small_font: 11.5,
        }
    }

    pub fn set_metrics(&mut self, scale: f32, font_size: f32, line_height: f32) {
        self.s = scale;
        self.font = (font_size * scale * 2.0).round() / 2.0;
        self.line_h = (self.font * line_height).round();
        self.cell = self.text.cell_w(self.font);
        self.ui_font = (13.0 * scale * 2.0).round() / 2.0;
        self.small_font = (11.5 * scale * 2.0).round() / 2.0;
    }

    /// Logical to physical pixels, rounded.
    pub fn px(&self, v: f32) -> f32 {
        (v * self.s).round()
    }

    pub fn begin_frame(&mut self, now: f64, dt: f32) {
        self.now = now;
        self.dt = dt;
        self.animating = false;
        self.wake = None;
        self.frame += 1;
        self.scene.clear();
        self.hits.clear();
    }

    pub fn end_frame(&mut self) {
        let f = self.frame;
        self.anims.retain(|_, (_, last)| *last == f);
        self.text.end_frame();
        self.hot = self.hit_at(self.mouse.0, self.mouse.1).map(|h| h.id);
    }

    pub fn hit_at(&self, x: f32, y: f32) -> Option<Hit> {
        if !self.mouse_inside {
            return None;
        }
        self.hits.iter().rev().find(|h| h.rect.contains(x, y)).copied()
    }

    pub fn wake_at(&mut self, t: f64) {
        self.wake = Some(self.wake.map_or(t, |w| w.min(t)));
    }

    /// Advance a caller-owned smooth value by this frame's dt.
    pub fn tick(&mut self, s: &mut Smooth, speed: f32, eps: f32) -> f32 {
        if !self.anims_on {
            s.v = s.target;
            return s.v;
        }
        if s.tick(self.dt, speed, eps) {
            self.animating = true;
        }
        s.v
    }

    /// Keyed animation: returns a value that eases towards `target`. A key
    /// seen for the first time starts at `target`.
    pub fn anim(&mut self, id: u64, target: f32, speed: f32) -> f32 {
        self.anim_from(id, target, target, speed)
    }

    /// Like `anim`, but a new key starts at `start` (for entrances).
    pub fn anim_from(&mut self, id: u64, start: f32, target: f32, speed: f32) -> f32 {
        let frame = self.frame;
        let (dt, on) = (self.dt, self.anims_on);
        let e = self.anims.entry(id).or_insert((Smooth::from_to(start, target), frame));
        e.1 = frame;
        e.0.target = target;
        if !on {
            e.0.v = target;
        } else if e.0.tick(dt, speed, 0.002) {
            self.animating = true;
        }
        e.0.v
    }

    pub fn hit(&mut self, rect: Rect, id: HitId, cursor: CursorKind) {
        self.hits.push(Hit { rect, id, cursor });
    }

    pub fn is_hot(&self, id: HitId) -> bool {
        self.hot == Some(id)
    }

    pub fn quad(&mut self, layer: usize, q: Quad) {
        self.scene.quad(layer, q);
    }

    /// Draw a label with its top-left at (x, y); returns its width.
    pub fn label(&mut self, layer: usize, s: &str, size: f32, bold: bool, x: f32, y: f32, clip: Rect, color: Rgba) -> f32 {
        if s.is_empty() || color.0[3] <= 0.003 {
            return 0.0;
        }
        let key = self.text.label(s, size, bold);
        self.scene.text(layer, key, x.round(), y.round(), clip, color);
        self.text.width(key)
    }

    pub fn label_w(&mut self, s: &str, size: f32, bold: bool) -> f32 {
        let key = self.text.label(s, size, bold);
        self.text.width(key)
    }

    /// Label vertically centred in `r`, left aligned at r.x.
    pub fn label_in(&mut self, layer: usize, s: &str, size: f32, bold: bool, r: Rect, clip: Rect, color: Rgba) -> f32 {
        let h = TextSystem::label_h(size);
        self.label(layer, s, size, bold, r.x, r.y + (r.h - h) * 0.5, clip, color)
    }

    /// Label centred both ways in `r`.
    pub fn label_mid(&mut self, layer: usize, s: &str, size: f32, bold: bool, r: Rect, color: Rgba) -> f32 {
        let w = self.label_w(s, size, bold);
        let h = TextSystem::label_h(size);
        self.label(layer, s, size, bold, r.x + (r.w - w) * 0.5, r.y + (r.h - h) * 0.5, Rect::HUGE, color)
    }

    /// Floating frosted panel with a drop shadow. `alpha` fades the whole thing.
    pub fn panel(&mut self, layer: usize, r: Rect, radius: f32, alpha: f32) {
        let s = self.s;
        self.quad(
            layer,
            Quad::new(r.translate(0.0, 8.0 * s).inset(4.0 * s), Rgba::BLACK.a(0.55 * alpha)).radius(radius).soft(22.0 * s),
        );
        let fill = self.ui.popup;
        if alpha >= 0.999 {
            self.quad(layer, Quad::new(r, fill).radius(radius).frost().border(s.max(1.0), self.ui.border));
        } else {
            self.quad(layer, Quad::new(r, fill.fade(alpha)).radius(radius).border(s.max(1.0), self.ui.border.fade(alpha)));
        }
    }
}

// ------------------------------------------------------------ clipboard

pub struct Clip {
    sys: Option<arboard::Clipboard>,
    local: String,
}

impl Clip {
    pub fn new(system: bool) -> Clip {
        Clip { sys: if system { arboard::Clipboard::new().ok() } else { None }, local: String::new() }
    }
    pub fn get(&mut self) -> String {
        if let Some(c) = &mut self.sys {
            if let Ok(s) = c.get_text() {
                return s;
            }
        }
        self.local.clone()
    }
    pub fn set(&mut self, s: &str) {
        self.local = s.to_string();
        if let Some(c) = &mut self.sys {
            let _ = c.set_text(s.to_string());
        }
    }
}

// ------------------------------------------------------------ text field

/// Single-line editable text (palette input, find/replace boxes).
#[derive(Default, Clone)]
pub struct TextField {
    pub text: String,
    /// Byte offsets.
    pub cursor: usize,
    pub anchor: usize,
    scroll: f32,
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

impl TextField {
    pub fn set(&mut self, s: &str) {
        self.text = s.to_string();
        self.cursor = self.text.len();
        self.anchor = self.cursor;
    }
    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.cursor = self.text.len();
    }
    fn range(&self) -> (usize, usize) {
        (self.cursor.min(self.anchor), self.cursor.max(self.anchor))
    }
    fn delete_sel(&mut self) -> bool {
        let (a, b) = self.range();
        if a == b {
            return false;
        }
        self.text.replace_range(a..b, "");
        self.cursor = a;
        self.anchor = a;
        true
    }
    pub fn insert(&mut self, s: &str) {
        self.delete_sel();
        let clean: String = s.chars().filter(|c| *c != '\n' && *c != '\r').collect();
        self.text.insert_str(self.cursor, &clean);
        self.cursor += clean.len();
        self.anchor = self.cursor;
    }
    fn prev(&self, word: bool) -> usize {
        let mut it = self.text[..self.cursor].char_indices().rev().peekable();
        let mut pos = self.cursor;
        if word {
            while let Some((i, c)) = it.peek().copied() {
                if is_word(c) {
                    break;
                }
                pos = i;
                it.next();
            }
            while let Some((i, c)) = it.peek().copied() {
                if !is_word(c) {
                    break;
                }
                pos = i;
                it.next();
            }
            pos
        } else {
            it.next().map(|(i, _)| i).unwrap_or(0)
        }
    }
    fn next(&self, word: bool) -> usize {
        let rest = &self.text[self.cursor..];
        let mut it = rest.char_indices().peekable();
        if word {
            let mut end = 0;
            while let Some((i, c)) = it.peek().copied() {
                if is_word(c) {
                    break;
                }
                end = i + c.len_utf8();
                it.next();
            }
            while let Some((i, c)) = it.peek().copied() {
                if !is_word(c) {
                    break;
                }
                end = i + c.len_utf8();
                it.next();
            }
            self.cursor + end
        } else {
            it.next().map(|(_, c)| self.cursor + c.len_utf8()).unwrap_or(self.text.len())
        }
    }

    /// Handle an editing key. Returns true if the text changed.
    pub fn key(&mut self, key: Key, mods: Mods, text: Option<&str>, clip: &mut Clip) -> bool {
        let mv = |f: &mut TextField, to: usize, extend: bool| {
            f.cursor = to;
            if !extend {
                f.anchor = to;
            }
        };
        match key {
            Key::Left => {
                let (a, b) = self.range();
                let to = if a != b && !mods.shift { a } else { self.prev(mods.ctrl) };
                mv(self, to, mods.shift);
            }
            Key::Right => {
                let (a, b) = self.range();
                let to = if a != b && !mods.shift { b } else { self.next(mods.ctrl) };
                mv(self, to, mods.shift);
            }
            Key::Home => mv(self, 0, mods.shift),
            Key::End => {
                let l = self.text.len();
                mv(self, l, mods.shift)
            }
            Key::Backspace => {
                if !self.delete_sel() {
                    let p = self.prev(mods.ctrl);
                    self.text.replace_range(p..self.cursor, "");
                    self.cursor = p;
                    self.anchor = p;
                }
                return true;
            }
            Key::Delete => {
                if !self.delete_sel() {
                    let n = self.next(mods.ctrl);
                    self.text.replace_range(self.cursor..n, "");
                }
                return true;
            }
            Key::Char(c) if mods.ctrl && !mods.alt => match c.to_ascii_lowercase() {
                'a' => self.select_all(),
                'c' | 'x' => {
                    let (a, b) = self.range();
                    if a != b {
                        clip.set(&self.text[a..b]);
                        if c.eq_ignore_ascii_case(&'x') {
                            self.delete_sel();
                            return true;
                        }
                    }
                }
                'v' => {
                    let s = clip.get();
                    if !s.is_empty() {
                        self.insert(&s);
                        return true;
                    }
                }
                _ => {}
            },
            _ => {
                if let Some(t) = text {
                    if !mods.ctrl && !mods.alt && !t.chars().any(|c| c.is_control()) && !t.is_empty() {
                        self.insert(t);
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Place the caret from a click at window x inside the field drawn at `r`.
    pub fn click(&mut self, cx: &mut Ctx, r: Rect, x: f32, extend: bool) {
        let key = cx.text.label(&self.text, cx.ui_font, false);
        let b = cx.text.byte_at(key, x - r.x + self.scroll);
        self.cursor = b.min(self.text.len());
        if !extend {
            self.anchor = self.cursor;
        }
    }

    /// Draw text, selection and caret inside `r` (no frame; the caller draws that).
    pub fn draw(&mut self, cx: &mut Ctx, layer: usize, r: Rect, focused: bool, placeholder: &str, alpha: f32, id: u64) {
        let size = cx.ui_font;
        let h = TextSystem::label_h(size);
        let y = r.y + (r.h - h) * 0.5;
        if self.text.is_empty() {
            cx.label(layer, placeholder, size, false, r.x, y, r, cx.ui.text_faint.fade(alpha));
        }
        let key = cx.text.label(&self.text, size, false);
        let cx_x = cx.text.x_at(key, self.cursor);
        // keep the caret in view
        if cx_x - self.scroll > r.w - 2.0 {
            self.scroll = cx_x - r.w + 2.0;
        }
        if cx_x - self.scroll < 0.0 {
            self.scroll = cx_x;
        }
        if cx.text.width(key) < r.w {
            self.scroll = 0.0;
        }
        let ox = r.x - self.scroll;
        let (a, b) = self.range();
        if a != b {
            let xa = cx.text.x_at(key, a);
            let xb = cx.text.x_at(key, b);
            let sel = Rect::new(ox + xa, y - 1.0, xb - xa, h + 2.0);
            cx.quad(layer, Quad::new(sel, cx.ui.selection.fade(alpha * if focused { 1.0 } else { 0.5 })).radius(3.0 * cx.s).clip(r));
        }
        if !self.text.is_empty() {
            cx.scene.text(layer, key, ox.round(), y.round(), r, cx.ui.text.fade(alpha));
        }
        if focused {
            let tx = cx.anim(id, ox + cx_x, 40.0);
            let blink = 0.55 + 0.45 * ((cx.now * 3.2).sin() as f32 * 0.5 + 0.5);
            if cx.window_focused {
                cx.wake_at(cx.now + 0.033);
            }
            let w = (1.6 * cx.s).max(1.0);
            cx.quad(layer, Quad::new(Rect::new(tx - w * 0.5, y, w, h), cx.ui.cursor.fade(alpha * blink)).radius(w * 0.5).clip(r.inset_xy(-2.0, 0.0)));
        }
    }
}
