//! Small shared types: rectangles, colours and animation primitives.

#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const ZERO: Rect = Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 };
    /// A rectangle that never clips anything.
    pub const HUGE: Rect = Rect { x: -1e6, y: -1e6, w: 2e6, h: 2e6 };

    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }
    pub fn right(&self) -> f32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
    pub fn cx(&self) -> f32 {
        self.x + self.w * 0.5
    }
    pub fn cy(&self) -> f32 {
        self.y + self.h * 0.5
    }
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }
    pub fn inset(&self, d: f32) -> Rect {
        Rect::new(self.x + d, self.y + d, (self.w - 2.0 * d).max(0.0), (self.h - 2.0 * d).max(0.0))
    }
    pub fn inset_xy(&self, dx: f32, dy: f32) -> Rect {
        Rect::new(self.x + dx, self.y + dy, (self.w - 2.0 * dx).max(0.0), (self.h - 2.0 * dy).max(0.0))
    }
    pub fn intersect(&self, o: &Rect) -> Rect {
        let x = self.x.max(o.x);
        let y = self.y.max(o.y);
        let r = self.right().min(o.right());
        let b = self.bottom().min(o.bottom());
        Rect::new(x, y, (r - x).max(0.0), (b - y).max(0.0))
    }
    pub fn translate(&self, dx: f32, dy: f32) -> Rect {
        Rect::new(self.x + dx, self.y + dy, self.w, self.h)
    }
}

/// Non-premultiplied sRGB colour.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Rgba(pub [f32; 4]);

impl Rgba {
    pub const BLACK: Rgba = Rgba([0.0, 0.0, 0.0, 1.0]);

    /// Parse `#rgb`, `#rrggbb` or `#rrggbbaa`.
    pub fn hex(s: &str) -> Option<Rgba> {
        let s = s.trim().trim_start_matches('#');
        let v = |i: usize| u8::from_str_radix(s.get(i..i + 2)?, 16).ok().map(|b| b as f32 / 255.0);
        match s.len() {
            3 => {
                let d = |i: usize| u8::from_str_radix(s.get(i..i + 1)?, 16).ok().map(|b| (b * 17) as f32 / 255.0);
                Some(Rgba([d(0)?, d(1)?, d(2)?, 1.0]))
            }
            6 => Some(Rgba([v(0)?, v(2)?, v(4)?, 1.0])),
            8 => Some(Rgba([v(0)?, v(2)?, v(4)?, v(6)?])),
            _ => None,
        }
    }
    pub fn a(self, a: f32) -> Rgba {
        Rgba([self.0[0], self.0[1], self.0[2], a])
    }
    /// Multiply the existing alpha.
    pub fn fade(self, f: f32) -> Rgba {
        Rgba([self.0[0], self.0[1], self.0[2], self.0[3] * f])
    }
    pub fn lerp(self, o: Rgba, t: f32) -> Rgba {
        let mut c = [0.0; 4];
        for i in 0..4 {
            c[i] = self.0[i] + (o.0[i] - self.0[i]) * t;
        }
        Rgba(c)
    }
    pub fn to_u8(self) -> [u8; 4] {
        let f = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        [f(self.0[0]), f(self.0[1]), f(self.0[2]), f(self.0[3])]
    }
}

pub fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

pub fn ease_out_cubic(t: f32) -> f32 {
    let t = 1.0 - t.clamp(0.0, 1.0);
    1.0 - t * t * t
}

pub fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Exponentially smoothed value that chases a target.
#[derive(Clone, Copy, Debug, Default)]
pub struct Smooth {
    pub v: f32,
    pub target: f32,
}

impl Smooth {
    pub fn new(v: f32) -> Self {
        Self { v, target: v }
    }
    pub fn from_to(v: f32, target: f32) -> Self {
        Self { v, target }
    }
    pub fn snap(&mut self, v: f32) {
        self.v = v;
        self.target = v;
    }
    pub fn settled(&self) -> bool {
        self.v == self.target
    }
    /// Advance; returns true while still moving. `speed` is in 1/seconds
    /// (higher is snappier), `eps` is the snap distance.
    pub fn tick(&mut self, dt: f32, speed: f32, eps: f32) -> bool {
        if self.v == self.target {
            return false;
        }
        let k = 1.0 - (-dt * speed).exp();
        let next = self.v + (self.target - self.v) * k;
        // at very large magnitudes the step can round away to nothing
        let stalled = next == self.v;
        self.v = next;
        if stalled || (self.target - self.v).abs() < eps {
            self.v = self.target;
            return false;
        }
        true
    }
}

/// FNV-1a, used for cheap stable ids of UI elements.
pub fn hash_str(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

pub fn mix_id(a: u64, b: u64) -> u64 {
    let mut h = a ^ b.wrapping_mul(0x9E3779B97F4A7C15);
    h ^= h >> 32;
    h = h.wrapping_mul(0xD6E8FEB86659FD93);
    h ^ (h >> 32)
}
