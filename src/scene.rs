//! The retained draw list the UI produces each time something changes:
//! a few layers, each holding rounded quads and positioned text.

use crate::util::{Rect, Rgba};
use bytemuck::{Pod, Zeroable};

/// One instanced rounded rectangle. Layout matches `quad.wgsl`.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug)]
pub struct Quad {
    rect: [f32; 4],
    color: [f32; 4],
    color2: [f32; 4],
    border_color: [f32; 4],
    radii: [f32; 4],
    /// border width, softness, frost, gradient direction
    params: [f32; 4],
    clip: [f32; 4],
}

impl Quad {
    pub fn new(r: Rect, c: Rgba) -> Self {
        Quad {
            rect: [r.x, r.y, r.w, r.h],
            color: c.0,
            color2: c.0,
            border_color: [0.0; 4],
            radii: [0.0; 4],
            params: [0.0; 4],
            clip: [-1e6, -1e6, 1e6, 1e6],
        }
    }
    pub fn radius(mut self, r: f32) -> Self {
        self.radii = [r; 4];
        self
    }
    /// Corner radii: top-left, top-right, bottom-right, bottom-left.
    pub fn radii(mut self, tl: f32, tr: f32, br: f32, bl: f32) -> Self {
        self.radii = [tl, tr, br, bl];
        self
    }
    pub fn border(mut self, w: f32, c: Rgba) -> Self {
        self.params[0] = w;
        self.border_color = c.0;
        self
    }
    /// Blurred edge, for shadows and glows.
    pub fn soft(mut self, s: f32) -> Self {
        self.params[1] = s;
        self
    }
    /// Frosted glass: the fill tints a blurred copy of the background.
    pub fn frost(mut self) -> Self {
        self.params[2] = 1.0;
        self
    }
    #[allow(dead_code)]
    pub fn grad_v(mut self, bottom: Rgba) -> Self {
        self.color2 = bottom.0;
        self.params[3] = 1.0;
        self
    }
    #[allow(dead_code)]
    pub fn grad_h(mut self, right: Rgba) -> Self {
        self.color2 = right.0;
        self.params[3] = 2.0;
        self
    }
    pub fn clip(mut self, r: Rect) -> Self {
        self.clip = [r.x, r.y, r.right(), r.bottom()];
        self
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TextDraw {
    /// Key of a shaped run in the `TextSystem` cache.
    pub key: u64,
    pub x: f32,
    pub y: f32,
    pub clip: Rect,
    /// Used for glyphs without a baked colour (UI labels).
    pub color: Rgba,
}

#[derive(Default)]
pub struct Layer {
    pub quads: Vec<Quad>,
    pub texts: Vec<TextDraw>,
}

/// Layer indices, back to front.
pub const L_BASE: usize = 0;
pub const L_FLOAT: usize = 1;
pub const L_MODAL: usize = 2;
pub const L_TOP: usize = 3;
pub const LAYERS: usize = 4;

#[derive(Default)]
pub struct Scene {
    pub layers: [Layer; LAYERS],
}

impl Scene {
    pub fn clear(&mut self) {
        for l in &mut self.layers {
            l.quads.clear();
            l.texts.clear();
        }
    }
    pub fn quad(&mut self, layer: usize, q: Quad) {
        self.layers[layer].quads.push(q);
    }
    pub fn text(&mut self, layer: usize, key: u64, x: f32, y: f32, clip: Rect, color: Rgba) {
        self.layers[layer].texts.push(TextDraw { key, x, y, clip, color });
    }
}
