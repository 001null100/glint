//! Panes (editor groups), their tab strips, and the split tree that lays
//! panes out side by side or stacked.

use crate::editor::View;
use crate::scene::{Quad, L_BASE, L_FLOAT};
use crate::text::TextSystem;
use crate::ui::{Ctx, CursorKind, HitId};
use crate::util::{ease_out_cubic, hash_str, mix_id, Rect, Rgba, Smooth};

pub struct Tab {
    pub view: View,
    /// Animated x offset inside the strip.
    x: Smooth,
    placed: bool,
}

impl Tab {
    pub fn new(view: View) -> Tab {
        Tab { view, x: Smooth::new(0.0), placed: false }
    }
}

/// A just-closed tab that is still fading out.
struct Ghost {
    title: String,
    x: f32,
    w: f32,
    died: f64,
}

pub struct Pane {
    pub id: u64,
    pub tabs: Vec<Tab>,
    pub active: usize,
    pub strip_scroll: Smooth,
    /// Scroll the strip so the active tab is visible on the next draw.
    pub reveal_tab: bool,
    ghosts: Vec<Ghost>,
    ind_x: Smooth,
    ind_w: Smooth,
    ind_placed: bool,
    // layout from the last draw
    pub rect: Rect,
    pub strip: Rect,
    pub body: Rect,
    /// Slot of each tab in window coordinates (where it rests, not where
    /// its animation currently is).
    pub slots: Vec<Rect>,
    slot_base: f32,
    max_strip_scroll: f32,
}

pub const STRIP_H: f32 = 38.0;

impl Pane {
    pub fn new(id: u64) -> Pane {
        Pane {
            id,
            tabs: Vec::new(),
            active: 0,
            strip_scroll: Smooth::new(0.0),
            reveal_tab: true,
            ghosts: Vec::new(),
            ind_x: Smooth::new(0.0),
            ind_w: Smooth::new(0.0),
            ind_placed: false,
            rect: Rect::ZERO,
            strip: Rect::ZERO,
            body: Rect::ZERO,
            slots: Vec::new(),
            slot_base: 0.0,
            max_strip_scroll: 0.0,
        }
    }

    pub fn active_view(&mut self) -> Option<&mut View> {
        self.tabs.get_mut(self.active).map(|t| &mut t.view)
    }

    pub fn set_active(&mut self, idx: usize) {
        if idx < self.tabs.len() {
            self.active = idx;
            self.reveal_tab = true;
        }
    }

    pub fn insert_tab(&mut self, idx: usize, view: View) {
        let idx = idx.min(self.tabs.len());
        self.tabs.insert(idx, Tab::new(view));
        self.active = idx;
        self.reveal_tab = true;
    }

    /// Remove a tab, leaving a fading ghost behind. Returns its view.
    pub fn remove_tab(&mut self, idx: usize, title: String, now: f64) -> Option<View> {
        if idx >= self.tabs.len() {
            return None;
        }
        if let Some(slot) = self.slots.get(idx) {
            self.ghosts.push(Ghost { title, x: slot.x, w: slot.w, died: now });
        }
        let tab = self.tabs.remove(idx);
        if self.active > idx || self.active >= self.tabs.len() {
            self.active = self.active.saturating_sub(1);
        }
        self.reveal_tab = true;
        Some(tab.view)
    }

    pub fn find_doc(&self, doc: u64) -> Option<usize> {
        self.tabs.iter().position(|t| t.view.doc == doc)
    }

    pub fn scroll_strip(&mut self, d: f32) {
        self.strip_scroll.target = (self.strip_scroll.target + d).clamp(0.0, self.max_strip_scroll);
    }

    /// Move the dragged tab to wherever its centre now sits among the slots.
    /// Returns the tab's new index.
    pub fn reorder_for_drag(&mut self, idx: usize, centre_x: f32) -> usize {
        let mut to = idx;
        for (i, s) in self.slots.iter().enumerate() {
            if i < idx && centre_x < s.cx() {
                to = i;
                break;
            }
            if i > idx && centre_x > s.cx() {
                to = i;
            }
        }
        if to != idx && idx < self.tabs.len() && to < self.tabs.len() {
            let t = self.tabs.remove(idx);
            self.tabs.insert(to, t);
            if self.active == idx {
                self.active = to;
            } else if idx < self.active && to >= self.active {
                self.active -= 1;
            } else if idx > self.active && to <= self.active {
                self.active += 1;
            }
            // keep the slot list consistent until the next draw
            let sl = self.slots.remove(idx);
            self.slots.insert(to, sl);
            let mut x = self.slot_base;
            for s in &mut self.slots {
                s.x = x;
                x += s.w;
            }
        }
        to
    }
}

pub struct StripArgs<'a> {
    pub focused: bool,
    /// (title, dirty) per tab.
    pub titles: &'a [(String, bool)],
    /// Tab being dragged: index, window x of its left edge, and its top y
    /// when it has been pulled out of the strip (None while docked).
    pub drag: Option<(usize, f32, Option<f32>)>,
}

fn draw_tab(cx: &mut Ctx, layer: usize, r: Rect, clip: Rect, title: &str, dirty: bool, act: f32, hov: f32, alpha: f32, focused: bool, close_hot: f32) {
    let s = cx.s;
    let fill = cx.ui.text.a((0.05 * hov + 0.085 * act) * alpha);
    cx.quad(layer, Quad::new(r, fill).radii(8.0 * s, 8.0 * s, 0.0, 0.0).clip(clip));
    let dim = if focused { 1.0 } else { 0.75 };
    let col = cx.ui.text_dim.lerp(cx.ui.text, act.max(hov * 0.5)).fade(alpha * dim);
    let pad = cx.px(14.0);
    let max_w = (r.w - pad - cx.px(30.0)).max(cx.cell);
    let key = cx.text.label_fit(title, cx.ui_font, false, max_w);
    let h = TextSystem::label_h(cx.ui_font);
    cx.scene.text(layer, key, (r.x + pad).round(), (r.y + (r.h - h) * 0.5).round(), r.intersect(&clip), col);
    // close button / dirty dot
    let c = Rect::new(r.right() - cx.px(28.0), r.cy() - cx.px(11.0), cx.px(22.0), cx.px(22.0));
    let show_x = hov.max(act * 0.55);
    if close_hot > 0.01 {
        cx.quad(layer, Quad::new(c, cx.ui.text.a(0.12 * close_hot * alpha)).radius(6.0 * s).clip(clip));
    }
    if dirty && close_hot < 0.5 {
        let d = cx.px(7.0);
        let dot = Rect::new(c.cx() - d * 0.5, c.cy() - d * 0.5, d, d);
        cx.quad(layer, Quad::new(dot.inset(-2.0 * s), cx.ui.accent.a(0.3 * alpha)).radius(d).soft(3.0 * s).clip(clip));
        cx.quad(layer, Quad::new(dot, cx.ui.accent.fade(alpha)).radius(d * 0.5).clip(clip));
    } else if show_x > 0.01 {
        let col = cx.ui.text_dim.lerp(cx.ui.text, close_hot).fade(show_x * alpha);
        let w = cx.label_w("×", cx.ui_font, false);
        cx.label(layer, "×", cx.ui_font, false, c.cx() - w * 0.5, c.cy() - h * 0.5, clip, col);
    }
}

pub fn draw_strip(cx: &mut Ctx, pane: &mut Pane, a: &StripArgs) {
    let s = cx.s;
    let strip = pane.strip;
    cx.quad(L_BASE, Quad::new(strip, cx.ui.panel).frost());
    cx.quad(L_BASE, Quad::new(Rect::new(strip.x, strip.bottom() - s.max(1.0), strip.w, s.max(1.0)), cx.ui.border));
    cx.hit(strip, HitId::Strip(pane.id), CursorKind::Default);

    let n = pane.tabs.len();
    let plus_w = cx.px(36.0);
    let left = cx.px(6.0);
    let avail = (strip.w - plus_w - left).max(cx.px(60.0));
    let mut widths: Vec<f32> = (0..n)
        .map(|i| {
            let t = a.titles.get(i).map(|t| t.0.as_str()).unwrap_or("");
            (cx.label_w(t, cx.ui_font, false) + cx.px(52.0)).clamp(cx.px(96.0), cx.px(230.0)).round()
        })
        .collect();
    let mut total: f32 = widths.iter().sum();
    if total > avail && n > 0 {
        let k = avail / total;
        for w in &mut widths {
            *w = (*w * k).max(cx.px(88.0)).min(*w).round();
        }
        total = widths.iter().sum();
    }
    pane.max_strip_scroll = (total - avail).max(0.0);

    // slots
    pane.slots.clear();
    let mut x = 0.0;
    for w in &widths {
        pane.slots.push(Rect::new(x, 0.0, *w, 0.0));
        x += w;
    }
    if pane.reveal_tab {
        if let Some(sl) = pane.slots.get(pane.active) {
            let t = pane.strip_scroll.target;
            if sl.x < t {
                pane.strip_scroll.target = sl.x;
            } else if sl.right() > t + avail {
                pane.strip_scroll.target = sl.right() - avail;
            }
        }
        pane.reveal_tab = false;
    }
    pane.strip_scroll.target = pane.strip_scroll.target.clamp(0.0, pane.max_strip_scroll);
    let sc = cx.tick(&mut pane.strip_scroll, 20.0, 0.5).round();
    let clip = Rect::new(strip.x, strip.y, left + avail, strip.h);
    let top = cx.px(6.0);
    let th = strip.h - top;
    let base_x = strip.x + left - sc;
    pane.slot_base = base_x;
    for sl in &mut pane.slots {
        *sl = Rect::new(base_x + sl.x, strip.y + top, sl.w, th);
    }

    // ghosts of closed tabs
    let now = cx.now;
    pane.ghosts.retain(|g| now - g.died < 0.16);
    for gi in 0..pane.ghosts.len() {
        let (t, gx, gw, title) = {
            let g = &pane.ghosts[gi];
            (1.0 - ((now - g.died) / 0.16) as f32, g.x, g.w, g.title.clone())
        };
        let r = Rect::new(gx, strip.y + top + (1.0 - t) * cx.px(10.0), gw, th);
        draw_tab(cx, L_BASE, r, clip, &title, false, 0.5, 0.0, t * 0.8, a.focused, 0.0);
        cx.animating = true;
    }

    let mut active_rect: Option<Rect> = None;
    for i in 0..n {
        let slot = pane.slots[i];
        let tab = &mut pane.tabs[i];
        tab.x.target = slot.x - base_x;
        if !tab.placed {
            tab.x.snap(tab.x.target);
            tab.placed = true;
        }
        let mut tx = tab.x;
        let ax = cx.tick(&mut tx, 24.0, 0.5);
        pane.tabs[i].x = tx;
        let doc = pane.tabs[i].view.doc;
        let id = mix_id(mix_id(hash_str("tab"), pane.id), doc);
        let is_active = i == pane.active;
        let close_hit = HitId::TabClose(pane.id, i);
        let hot = cx.is_hot(HitId::Tab(pane.id, i)) || cx.is_hot(close_hit);
        let appear = cx.anim_from(id ^ 0xA1, 0.0, 1.0, 13.0);
        let hov = cx.anim(id ^ 0xB2, if hot { 1.0 } else { 0.0 }, 16.0);
        let act = cx.anim(id ^ 0xC3, if is_active { 1.0 } else { 0.0 }, 18.0);
        let ch = cx.anim(id ^ 0xD4, if cx.is_hot(close_hit) { 1.0 } else { 0.0 }, 20.0);
        let (title, dirty) = a.titles.get(i).map(|t| (t.0.as_str(), t.1)).unwrap_or(("", false));

        let dragged = a.drag.filter(|d| d.0 == i);
        let r = match dragged {
            Some((_, dx, None)) => Rect::new(dx.clamp(strip.x, (strip.right() - slot.w).max(strip.x)), slot.y, slot.w, th),
            Some((_, dx, Some(dy))) => Rect::new(dx, dy, slot.w, th),
            None => Rect::new(base_x + ax, slot.y + (1.0 - ease_out_cubic(appear)) * cx.px(12.0), slot.w, th),
        };
        if is_active {
            active_rect = Some(r);
        }
        if dragged.is_some() {
            cx.quad(L_FLOAT, Quad::new(r.translate(0.0, 4.0 * s), Rgba::BLACK.a(0.5)).radius(8.0 * s).soft(12.0 * s));
            cx.quad(L_FLOAT, Quad::new(r, cx.ui.popup).radii(8.0 * s, 8.0 * s, 0.0, 0.0).frost());
            draw_tab(cx, L_FLOAT, r, Rect::HUGE, title, dirty, 1.0, 1.0, 1.0, true, 0.0);
        } else {
            draw_tab(cx, L_BASE, r, clip, title, dirty, act, hov, appear, a.focused, ch);
            let hr = r.intersect(&clip);
            if hr.w > 0.0 {
                cx.hit(hr, HitId::Tab(pane.id, i), CursorKind::Default);
                let c = Rect::new(r.right() - cx.px(28.0), r.cy() - cx.px(11.0), cx.px(22.0), cx.px(22.0)).intersect(&clip);
                if c.w > 0.0 {
                    cx.hit(c, close_hit, CursorKind::Pointer);
                }
            }
        }
    }

    // active tab indicator: an accent bar that glides between tabs
    if let Some(r) = active_rect {
        pane.ind_x.target = r.x + cx.px(10.0);
        pane.ind_w.target = (r.w - cx.px(20.0)).max(4.0);
        if !pane.ind_placed {
            pane.ind_x.snap(pane.ind_x.target);
            pane.ind_w.snap(pane.ind_w.target);
            pane.ind_placed = true;
        }
        let dragging = a.drag.is_some();
        let speed = if dragging { 60.0 } else { 20.0 };
        let ix = cx.tick(&mut pane.ind_x, speed, 0.5);
        let iw = cx.tick(&mut pane.ind_w, speed, 0.5);
        let bar = Rect::new(ix, strip.bottom() - cx.px(2.0), iw, cx.px(2.0));
        let layer = if dragging { L_FLOAT } else { L_BASE };
        let col = if a.focused { cx.ui.accent } else { cx.ui.text_faint };
        if a.focused {
            cx.quad(layer, Quad::new(bar.inset_xy(0.0, -2.0 * s), col.a(0.4)).radius(3.0 * s).soft(5.0 * s).clip(clip));
        }
        cx.quad(layer, Quad::new(bar, col).radius(s).clip(clip));
    } else {
        pane.ind_placed = false;
    }

    // new-tab button
    let px = (base_x + total + cx.px(5.0)).min(strip.x + left + avail + cx.px(5.0));
    let b = Rect::new(px, strip.y + top + (th - cx.px(26.0)) * 0.5, cx.px(26.0), cx.px(26.0));
    let hid = HitId::NewTab(pane.id);
    let hov = cx.anim(mix_id(hash_str("newtab"), pane.id), if cx.is_hot(hid) { 1.0 } else { 0.0 }, 16.0);
    cx.quad(L_BASE, Quad::new(b, cx.ui.text.a(0.1 * hov)).radius(6.0 * s));
    cx.label_mid(L_BASE, "+", cx.ui_font * 1.15, false, b, cx.ui.text_dim.lerp(cx.ui.text, hov));
    cx.hit(b, hid, CursorKind::Pointer);
}

// ---------------------------------------------------------------- tree

pub struct Split {
    pub id: u32,
    /// true: children sit side by side; false: stacked.
    pub horizontal: bool,
    /// Fraction of the space given to `a`.
    pub ratio: Smooth,
    pub a: Node,
    pub b: Node,
    /// Some(true): `a` is collapsing away; Some(false): `b` is.
    pub closing: Option<bool>,
    pub rect: Rect,
}

pub enum Node {
    Leaf(u64),
    Split(Box<Split>),
}

pub struct Divider {
    pub id: u32,
    pub rect: Rect,
    pub horizontal: bool,
}

impl Node {
    pub fn leaves(&self, out: &mut Vec<u64>) {
        match self {
            Node::Leaf(id) => out.push(*id),
            Node::Split(s) => {
                s.a.leaves(out);
                s.b.leaves(out);
            }
        }
    }

    /// Replace leaf `target` with a split holding it and a new leaf.
    pub fn split(&mut self, target: u64, new_leaf: u64, split_id: u32, horizontal: bool) -> bool {
        match self {
            Node::Leaf(id) if *id == target => {
                *self = Node::Split(Box::new(Split {
                    id: split_id,
                    horizontal,
                    ratio: Smooth::from_to(1.0, 0.5),
                    a: Node::Leaf(target),
                    b: Node::Leaf(new_leaf),
                    closing: None,
                    rect: Rect::ZERO,
                }));
                true
            }
            Node::Leaf(_) => false,
            Node::Split(s) => s.a.split(target, new_leaf, split_id, horizontal) || s.b.split(target, new_leaf, split_id, horizontal),
        }
    }

    fn dead(&self, live: &dyn Fn(u64) -> bool) -> bool {
        match self {
            Node::Leaf(id) => !live(*id),
            Node::Split(s) => s.a.dead(live) && s.b.dead(live),
        }
    }

    /// Collapse the parts of the tree whose panes are gone. A split with one
    /// dead side animates shut and is then replaced by its surviving child;
    /// a split with two dead sides is collapsed by its parent the same way.
    pub fn prune(&mut self, live: &dyn Fn(u64) -> bool) {
        if let Node::Split(s) = self {
            s.a.prune(live);
            s.b.prune(live);
            let (da, db) = (s.a.dead(live), s.b.dead(live));
            if da != db {
                let a_goes = da;
                if s.closing != Some(a_goes) {
                    s.closing = Some(a_goes);
                    s.ratio.target = if a_goes { 0.0 } else { 1.0 };
                }
                if s.ratio.settled() {
                    let keep = std::mem::replace(if a_goes { &mut s.b } else { &mut s.a }, Node::Leaf(0));
                    *self = keep;
                }
            }
        }
    }

    pub fn find_split(&mut self, id: u32) -> Option<&mut Split> {
        match self {
            Node::Leaf(_) => None,
            Node::Split(s) => {
                if s.id == id {
                    return Some(s);
                }
                if let Some(f) = s.a.find_split(id) {
                    return Some(f);
                }
                s.b.find_split(id)
            }
        }
    }

    /// Assign rectangles. `live` says whether a leaf id still has a pane.
    pub fn layout(&mut self, cx: &mut Ctx, r: Rect, live: &dyn Fn(u64) -> bool, out: &mut Vec<(u64, Rect)>, divs: &mut Vec<Divider>) {
        match self {
            Node::Leaf(id) => {
                if live(*id) {
                    out.push((*id, r));
                }
            }
            Node::Split(s) => {
                s.rect = r;
                let ratio = cx.tick(&mut s.ratio, 16.0, 0.002).clamp(0.0, 1.0);
                let gap = if s.closing.is_some() { 0.0 } else { cx.s.max(1.0).round() };
                if s.horizontal {
                    let aw = ((r.w - gap) * ratio).round();
                    s.a.layout(cx, Rect::new(r.x, r.y, aw, r.h), live, out, divs);
                    s.b.layout(cx, Rect::new(r.x + aw + gap, r.y, (r.w - aw - gap).max(0.0), r.h), live, out, divs);
                    if s.closing.is_none() {
                        divs.push(Divider { id: s.id, rect: Rect::new(r.x + aw, r.y, gap, r.h), horizontal: true });
                    }
                } else {
                    let ah = ((r.h - gap) * ratio).round();
                    s.a.layout(cx, Rect::new(r.x, r.y, r.w, ah), live, out, divs);
                    s.b.layout(cx, Rect::new(r.x, r.y + ah + gap, r.w, (r.h - ah - gap).max(0.0)), live, out, divs);
                    if s.closing.is_none() {
                        divs.push(Divider { id: s.id, rect: Rect::new(r.x, r.y + ah, r.w, gap), horizontal: false });
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaves(n: &Node) -> Vec<u64> {
        let mut v = Vec::new();
        n.leaves(&mut v);
        v
    }

    #[test]
    fn split_and_close() {
        let mut t = Node::Leaf(1);
        assert!(t.split(1, 2, 10, true));
        assert!(t.split(2, 3, 11, false));
        assert_eq!(leaves(&t), vec![1, 2, 3]);
        assert!(!t.split(9, 4, 12, true));
        // pane 2 goes away: its split starts closing, then collapses
        let live = |id: u64| id != 2;
        t.prune(&live);
        assert_eq!(leaves(&t), vec![1, 2, 3]);
        t.find_split(11).unwrap().ratio.snap(0.0);
        t.prune(&live);
        assert_eq!(leaves(&t), vec![1, 3]);
    }

    #[test]
    fn sibling_closed_while_collapsing() {
        // Split(Split(1, 3), 2): close 3, then 1 before the animation ends
        let mut t = Node::Leaf(1);
        assert!(t.split(1, 2, 10, true));
        assert!(t.split(1, 3, 11, true));
        assert_eq!(leaves(&t), vec![1, 3, 2]);
        t.find_split(11).unwrap().ratio.snap(0.5);
        t.prune(&|id| id != 3);
        assert_eq!(leaves(&t), vec![1, 3, 2]);
        let live = |id: u64| id == 2;
        t.prune(&live);
        // the whole inner split is dead now; the outer one collapses onto pane 2
        t.find_split(10).unwrap().ratio.snap(0.0);
        t.prune(&live);
        assert_eq!(leaves(&t), vec![2]);
    }
}
