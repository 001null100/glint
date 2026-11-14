//! Text shaping with a content-addressed cache. Every visual row of the
//! editor and every UI label is shaped once and reused until it stops being
//! drawn, so scrolling or typing only shapes the rows that actually changed.

use crate::util::Rgba;
use glyphon::cosmic_text::{FeatureTag, FontFeatures};
use glyphon::{Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, Style, SwashCache, Weight};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

pub const FONT_LIGA: &str = "JetBrains Mono";
pub const FONT_PLAIN: &str = "JetBrains Mono NL";

const FONT_FILES: &[&[u8]] = &[
    include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMono-Italic.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMono-Bold.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMonoNL-Regular.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMonoNL-Italic.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMonoNL-Bold.ttf"),
];

/// A coloured, styled byte range of a line.
#[derive(Clone, Copy, Debug)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub color: Rgba,
    pub bold: bool,
    pub italic: bool,
}

struct Glyph {
    start: u32,
    end: u32,
    x: f32,
    w: f32,
}

pub struct Shaped {
    pub buffer: Buffer,
    pub width: f32,
    glyphs: Vec<Glyph>,
    last_used: u64,
}

pub struct TextSystem {
    pub fonts: FontSystem,
    pub swash: SwashCache,
    cache: HashMap<u64, Shaped>,
    frame: u64,
    pub ligatures: bool,
    cell_w: HashMap<u32, f32>,
}

fn to_color(c: Rgba) -> Color {
    let [r, g, b, a] = c.to_u8();
    Color::rgba(r, g, b, a)
}

impl TextSystem {
    pub fn new(ligatures: bool) -> Self {
        let mut fonts = FontSystem::new();
        for f in FONT_FILES {
            fonts.db_mut().load_font_data(f.to_vec());
        }
        TextSystem { fonts, swash: SwashCache::new(), cache: HashMap::new(), frame: 0, ligatures, cell_w: HashMap::new() }
    }

    fn family(&self) -> &'static str {
        if self.ligatures { FONT_LIGA } else { FONT_PLAIN }
    }

    pub fn set_ligatures(&mut self, on: bool) {
        if self.ligatures != on {
            self.ligatures = on;
            self.cache.clear();
            self.cell_w.clear();
        }
    }

    fn base_attrs(&self) -> Attrs<'static> {
        let mut a = Attrs::new().family(Family::Name(self.family()));
        if !self.ligatures {
            let mut ff = FontFeatures::new();
            ff.disable(FeatureTag::CONTEXTUAL_ALTERNATES).disable(FeatureTag::STANDARD_LIGATURES);
            a = a.font_features(ff);
        }
        a
    }

    fn finish(&mut self, key: u64, mut buffer: Buffer) {
        buffer.shape_until_scroll(&mut self.fonts, false);
        let mut width = 0.0f32;
        let mut glyphs = Vec::new();
        if let Some(run) = buffer.layout_runs().next() {
            width = run.line_w;
            glyphs.reserve(run.glyphs.len());
            for g in run.glyphs {
                glyphs.push(Glyph { start: g.start as u32, end: g.end as u32, x: g.x, w: g.w });
            }
        }
        self.cache.insert(key, Shaped { buffer, width, glyphs, last_used: self.frame });
    }

    /// Shape one visual row of editor text with baked syntax colours.
    pub fn line(&mut self, text: &str, spans: &[Span], default: Rgba, size: f32, line_h: f32, tab_w: u16) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        1u8.hash(&mut h);
        text.hash(&mut h);
        size.to_bits().hash(&mut h);
        line_h.to_bits().hash(&mut h);
        tab_w.hash(&mut h);
        default.to_u8().hash(&mut h);
        for s in spans {
            (s.start, s.end, s.color.to_u8(), s.bold, s.italic).hash(&mut h);
        }
        let key = h.finish();
        if let Some(s) = self.cache.get_mut(&key) {
            s.last_used = self.frame;
            return key;
        }
        let base = self.base_attrs().color(to_color(default));
        let mut buffer = Buffer::new(&mut self.fonts, Metrics::new(size, line_h));
        buffer.set_size(None, None);
        buffer.set_tab_width(tab_w.max(1));
        let cw = self.cell_w(size);
        buffer.set_monospace_width(Some(cw));
        if spans.is_empty() {
            buffer.set_text(text, &base, Shaping::Advanced, None);
        } else {
            let mut parts: Vec<(&str, Attrs)> = Vec::with_capacity(spans.len() * 2 + 1);
            let mut pos = 0usize;
            for s in spans {
                let (a, b) = (s.start.min(text.len()), s.end.min(text.len()));
                if a < pos || b <= a || !text.is_char_boundary(a) || !text.is_char_boundary(b) {
                    continue;
                }
                if a > pos {
                    parts.push((&text[pos..a], base.clone()));
                }
                let mut at = base.clone().color(to_color(s.color));
                if s.bold {
                    at = at.weight(Weight::BOLD);
                }
                if s.italic {
                    at = at.style(Style::Italic);
                }
                parts.push((&text[a..b], at));
                pos = b;
            }
            if pos < text.len() {
                parts.push((&text[pos..], base.clone()));
            }
            buffer.set_rich_text(parts, &base, Shaping::Advanced, None);
        }
        self.finish(key, buffer);
        key
    }

    /// Shape a UI label. Its colour comes from the draw call, so one shaped
    /// label can be drawn at any colour or opacity.
    pub fn label(&mut self, text: &str, size: f32, bold: bool) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        2u8.hash(&mut h);
        text.hash(&mut h);
        size.to_bits().hash(&mut h);
        bold.hash(&mut h);
        let key = h.finish();
        if let Some(s) = self.cache.get_mut(&key) {
            s.last_used = self.frame;
            return key;
        }
        let mut attrs = self.base_attrs();
        if bold {
            attrs = attrs.weight(Weight::BOLD);
        }
        let mut buffer = Buffer::new(&mut self.fonts, Metrics::new(size, (size * 1.35).round()));
        buffer.set_size(None, None);
        buffer.set_text(text, &attrs, Shaping::Advanced, None);
        self.finish(key, buffer);
        key
    }

    /// Label truncated with an ellipsis to fit `max_w` pixels.
    pub fn label_fit(&mut self, text: &str, size: f32, bold: bool, max_w: f32) -> u64 {
        let cw = self.cell_w(size);
        let max_chars = (max_w / cw).floor().max(1.0) as usize;
        if text.chars().count() <= max_chars {
            return self.label(text, size, bold);
        }
        let mut s: String = text.chars().take(max_chars.saturating_sub(1)).collect();
        s.push('…');
        self.label(&s, size, bold)
    }

    /// Split borrow for the renderer: mutable font state plus the read-only
    /// shaped-run cache.
    pub fn parts(&mut self) -> (&mut FontSystem, &mut SwashCache, &HashMap<u64, Shaped>) {
        (&mut self.fonts, &mut self.swash, &self.cache)
    }

    pub fn width(&self, key: u64) -> f32 {
        self.cache.get(&key).map(|s| s.width).unwrap_or(0.0)
    }

    /// Line height used by `label` for a given size.
    pub fn label_h(size: f32) -> f32 {
        (size * 1.35).round()
    }

    /// X offset of the caret placed before byte `byte` of a shaped row.
    pub fn x_at(&self, key: u64, byte: usize) -> f32 {
        let Some(s) = self.cache.get(&key) else { return 0.0 };
        let b = byte as u32;
        let mut best_end = 0.0f32;
        for g in &s.glyphs {
            if b >= g.start && b < g.end {
                let frac = (b - g.start) as f32 / (g.end - g.start).max(1) as f32;
                return g.x + g.w * frac;
            }
            if g.end <= b {
                best_end = best_end.max(g.x + g.w);
            }
        }
        best_end
    }

    /// Byte offset of the caret position nearest to `x`.
    pub fn byte_at(&self, key: u64, x: f32) -> usize {
        let Some(s) = self.cache.get(&key) else { return 0 };
        let mut best = (f32::MAX, 0usize);
        for g in &s.glyphs {
            for (pos, byte) in [(g.x, g.start), (g.x + g.w, g.end)] {
                let d = (pos - x).abs();
                if d < best.0 {
                    best = (d, byte as usize);
                }
            }
        }
        best.1
    }

    /// Advance width of one monospace cell at `size`.
    pub fn cell_w(&mut self, size: f32) -> f32 {
        if let Some(w) = self.cell_w.get(&size.to_bits()) {
            return *w;
        }
        let attrs = self.base_attrs();
        let mut buffer = Buffer::new(&mut self.fonts, Metrics::new(size, size * 1.4));
        buffer.set_size(None, None);
        buffer.set_text("0000000000", &attrs, Shaping::Advanced, None);
        buffer.shape_until_scroll(&mut self.fonts, false);
        let w = buffer.layout_runs().next().map(|r| r.line_w / 10.0).unwrap_or(size * 0.6);
        self.cell_w.insert(size.to_bits(), w);
        w
    }

    /// Call once per built scene; drops shaped runs that have not been drawn
    /// for a while.
    pub fn end_frame(&mut self) {
        self.frame += 1;
        if self.frame % 30 == 0 && self.cache.len() > 600 {
            let cutoff = self.frame.saturating_sub(120);
            self.cache.retain(|_, s| s.last_used >= cutoff);
        }
    }
}
