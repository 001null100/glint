//! Scene building: turns the app state into quads, text and hit regions.

use crate::app::{App, Drag, Kb, Ov};
use crate::editor::{self, EditorArgs};
use crate::pane::{draw_strip, StripArgs, STRIP_H};
use crate::platform::Effect;
use crate::scene::{Quad, L_BASE, L_FLOAT, L_TOP};
use crate::text::TextSystem;
use crate::ui::{CursorKind, HitId};
use crate::util::{ease_out_cubic, hash_str, mix_id, Rect, Rgba};

const STATUS_H: f32 = 26.0;

impl App {
    pub(crate) fn build(&mut self, now: f64, dt: f32) {
        self.cx.begin_frame(now, dt);
        let (w, h) = self.cx.size;
        let status_h = self.cx.px(STATUS_H);
        let area = Rect::new(0.0, 0.0, w, (h - status_h).max(0.0));

        // layout the split tree
        let mut rects: Vec<(u64, Rect)> = Vec::new();
        self.dividers.clear();
        {
            let panes = &self.panes;
            self.tree.prune(&|id| panes.contains_key(&id));
            self.tree.layout(&mut self.cx, area, &|id| panes.contains_key(&id), &mut rects, &mut self.dividers);
        }
        self.refresh_find();

        for (pid, rect) in &rects {
            self.draw_pane(*pid, *rect);
        }
        self.draw_dividers();
        self.draw_status(Rect::new(0.0, h - status_h, w, status_h));
        self.draw_find();
        self.draw_toasts();
        self.draw_overlay();

        self.cx.end_frame();
        self.animating = self.cx.animating;
        self.wake = self.cx.wake;
        self.dirty = false;

        // window title and pointer shape
        let title = match self.panes.get(&self.focus).and_then(|p| p.tabs.get(p.active)).and_then(|t| self.docs.get(&t.view.doc)) {
            Some(d) => format!("{}{} — Glint", d.title(), if d.is_dirty() { " ●" } else { "" }),
            None => "Glint".to_string(),
        };
        if title != self.last_title {
            self.last_title = title.clone();
            self.effects.push(Effect::Title(title));
        }
        if self.drag.is_none() {
            let (mx, my) = self.cx.mouse;
            let c = self.cx.hit_at(mx, my).map(|h| h.cursor).unwrap_or_default();
            if c != self.last_cursor {
                self.last_cursor = c;
                self.effects.push(Effect::Cursor(c));
            }
        }
    }

    fn draw_pane(&mut self, pid: u64, rect: Rect) {
        let focused = pid == self.focus;
        let cx = &mut self.cx;
        let Some(pane) = self.panes.get_mut(&pid) else { return };
        let sh = cx.px(STRIP_H).min(rect.h);
        pane.rect = rect;
        pane.strip = Rect::new(rect.x, rect.y, rect.w, sh);
        pane.body = Rect::new(rect.x, rect.y + sh, rect.w, (rect.h - sh).max(0.0));
        let body = pane.body;

        let titles: Vec<(String, bool)> =
            pane.tabs.iter().map(|t| self.docs.get(&t.view.doc).map(|d| (d.title(), d.is_dirty())).unwrap_or_default()).collect();
        let (mx, my) = cx.mouse;
        let drag = match &self.drag {
            Some(Drag::Tab { pane: p, idx, grab, moved: true, .. }) if *p == pid => {
                let docked = pane.strip.inset_xy(0.0, -cx.px(28.0)).contains(mx, my);
                Some((*idx, mx - grab, if docked { None } else { Some(my - cx.px(16.0)) }))
            }
            _ => None,
        };

        // editor (drawn first so the strip's shadow line sits on top)
        let editor_focus = focused && self.kb == Kb::Editor && self.window_focused && !self.overlay.as_ref().is_some_and(|o| !o.closing);
        let scrollbar_held = matches!(&self.drag, Some(Drag::Scrollbar { pane: p, .. }) if *p == pid);
        let mut drew = false;
        if let Some(tab) = pane.tabs.get_mut(pane.active) {
            if let Some(doc) = self.docs.get_mut(&tab.view.doc) {
                let show = self.find.open && self.find.doc == Some(doc.id);
                let (matches, current): (&[(usize, usize)], Option<usize>) =
                    if show { (&self.find.matches, if focused { self.find.current } else { None }) } else { (&[], None) };
                editor::draw(
                    cx,
                    &mut tab.view,
                    doc,
                    &EditorArgs {
                        rect: body,
                        pane: pid,
                        focused: editor_focus,
                        wrap: self.cfg.wrap,
                        tab_w: self.cfg.tab_width,
                        dim: self.cfg.dim,
                        blink: self.cfg.cursor_blink,
                        matches,
                        current,
                        scrollbar_held,
                        // catch up faster on big files while the user is idle
                        hl_budget_ms: if self.now - self.last_input > 0.2 { 11 } else { 4 },
                    },
                );
                drew = true;
            }
        }
        if !drew {
            let veil = cx.ui.editor;
            cx.quad(L_BASE, Quad::new(body, veil.a((veil.0[3] * self.cfg.dim).min(1.0))));
            draw_welcome(cx, body);
        }
        draw_strip(cx, pane, &StripArgs { focused, titles: &titles, drag });

        // unfocused panes sit back a little
        let d = cx.anim(mix_id(hash_str("pane-dim"), pid), if focused { 0.0 } else { 1.0 }, 12.0);
        if d > 0.01 {
            cx.quad(L_FLOAT, Quad::new(rect, Rgba::BLACK.a(0.2 * d)));
        }
        // drop target highlight while a tab from another pane hovers here
        if let Some(Drag::Tab { pane: src, moved: true, .. }) = &self.drag {
            if *src != pid && rect.contains(mx, my) {
                let s = cx.s;
                cx.quad(L_FLOAT, Quad::new(rect.inset(3.0 * s), cx.ui.accent.a(0.07)).radius(8.0 * s).border(1.5 * s, cx.ui.accent.a(0.6)));
            }
        }
    }

    fn draw_dividers(&mut self) {
        let cx = &mut self.cx;
        for d in &self.dividers {
            let hid = HitId::Divider(d.id);
            let held = matches!(&self.drag, Some(Drag::Divider { id }) if *id == d.id);
            let hot = cx.anim(mix_id(hash_str("divider"), d.id as u64), if cx.is_hot(hid) || held { 1.0 } else { 0.0 }, 14.0);
            // the gap would otherwise show the raw, undimmed background
            cx.quad(L_BASE, Quad::new(d.rect, cx.ui.bg));
            cx.quad(L_FLOAT, Quad::new(d.rect, cx.ui.border));
            if hot > 0.01 {
                let grow = if d.horizontal { d.rect.inset_xy(-1.0 * cx.s, 0.0) } else { d.rect.inset_xy(0.0, -1.0 * cx.s) };
                cx.quad(L_FLOAT, Quad::new(grow, cx.ui.accent.a(0.35 * hot)).soft(5.0 * cx.s));
                cx.quad(L_FLOAT, Quad::new(grow, cx.ui.accent.a(0.9 * hot)));
            }
            let pad = cx.px(4.0);
            let (hr, cur) = if d.horizontal {
                (d.rect.inset_xy(-pad, 0.0), CursorKind::ResizeH)
            } else {
                (d.rect.inset_xy(0.0, -pad), CursorKind::ResizeV)
            };
            cx.hit(hr, hid, cur);
        }
    }

    fn draw_status(&mut self, r: Rect) {
        let cx = &mut self.cx;
        let s = cx.s;
        cx.quad(L_BASE, Quad::new(r, cx.ui.panel).frost());
        cx.quad(L_BASE, Quad::new(Rect::new(r.x, r.y, r.w, s.max(1.0)), cx.ui.border));
        cx.hit(r, HitId::Panel, CursorKind::Default);
        let size = cx.small_font;
        let info = self.panes.get(&self.focus).and_then(|p| p.tabs.get(p.active)).and_then(|t| self.docs.get(&t.view.doc).map(|d| (&t.view, d)));

        // right side: clickable segments, laid out from the right edge
        let mut items: Vec<(u8, String)> = Vec::new();
        if let Some((v, d)) = info {
            let head = v.sel.head.min(d.buf.len());
            let line = d.buf.line_of(head);
            let col = head - d.buf.line_start(line);
            let mut pos = format!("Ln {}, Col {}", line + 1, col + 1);
            if !v.sel.is_empty() {
                pos.push_str(&format!(" ({} selected)", v.sel.end() - v.sel.start()));
            }
            items.push((0, pos));
            items.push((1, if self.cfg.wrap { "Wrap".into() } else { "No wrap".into() }));
            items.push((2, if d.indent_tabs { "Tabs".into() } else { format!("Spaces: {}", d.indent_width) }));
            items.push((5, if d.crlf { "CRLF".into() } else { "LF".into() }));
            items.push((3, d.hl.lang().to_string()));
        }
        items.push((4, format!("✦ {}", self.themes[self.theme_idx].name)));
        let mut x = r.right() - cx.px(6.0);
        let mut left_limit = x;
        for (id, label) in items.iter().rev() {
            let w = cx.label_w(label, size, false) + cx.px(18.0);
            let ir = Rect::new(x - w, r.y, w, r.h);
            let clickable = matches!(id, 0 | 1 | 3 | 4);
            let hid = HitId::Status(*id);
            let hov = if clickable { cx.anim(hash_str("status") ^ *id as u64, if cx.is_hot(hid) { 1.0 } else { 0.0 }, 16.0) } else { 0.0 };
            if hov > 0.01 {
                cx.quad(L_BASE, Quad::new(ir.inset_xy(0.0, 3.0 * s), cx.ui.text.a(0.1 * hov)).radius(5.0 * s));
            }
            let col = if *id == 4 { cx.ui.accent.lerp(cx.ui.text, hov * 0.5) } else { cx.ui.text_dim.lerp(cx.ui.text, hov) };
            cx.label_mid(L_BASE, label, size, false, ir, col);
            if clickable {
                cx.hit(ir, hid, CursorKind::Pointer);
            }
            x -= w;
            left_limit = x;
        }

        // left side: where this document lives
        if let Some((_, d)) = info {
            let mut path = d.display_path();
            if d.missing {
                path.push_str("  (deleted on disk)");
            } else if d.is_dirty() && d.path.is_some() {
                path.push_str("  ● unsaved");
            }
            let max_w = (left_limit - r.x - cx.px(24.0)).max(0.0);
            if max_w > cx.cell * 4.0 {
                let key = cx.text.label_fit(&path, size, false, max_w);
                let h = TextSystem::label_h(size);
                cx.scene.text(L_BASE, key, (r.x + cx.px(12.0)).round(), (r.y + (r.h - h) * 0.5).round(), r, cx.ui.text_faint);
            }
        }
    }

    fn draw_find(&mut self) {
        let cx = &mut self.cx;
        let t = cx.anim_from(hash_str("find-open"), 0.0, if self.find.open { 1.0 } else { 0.0 }, 22.0);
        if t < 0.01 {
            return;
        }
        let Some(anchor) = self.panes.get(&self.focus).map(|p| p.body) else { return };
        let focus = match self.kb {
            Kb::Find(f) if self.find.open && !self.overlay.as_ref().is_some_and(|o| !o.closing) => Some(f),
            _ => None,
        };
        self.find.draw(cx, anchor, focus, ease_out_cubic(t));
    }

    fn draw_toasts(&mut self) {
        let cx = &mut self.cx;
        let now = cx.now;
        const LIFE: f64 = 3.6;
        self.toasts.retain(|t| now - t.born < LIFE);
        if self.toasts.is_empty() {
            return;
        }
        let s = cx.s;
        let (w, h) = cx.size;
        let size = cx.ui_font;
        let th = cx.px(36.0);
        let mut y = h - cx.px(STATUS_H) - cx.px(14.0);
        for t in self.toasts.iter().rev() {
            let age = (now - t.born) as f32;
            let appear = cx.anim_from(mix_id(hash_str("toast"), t.id), 0.0, 1.0, 14.0);
            let fade = ((LIFE as f32 - age) / 0.35).clamp(0.0, 1.0);
            let a = ease_out_cubic(appear) * fade;
            let tw = (cx.label_w(&t.text, size, false) + cx.px(44.0)).min(w - cx.px(28.0));
            y -= th;
            let r = Rect::new(w - tw - cx.px(14.0) + (1.0 - ease_out_cubic(appear)) * cx.px(24.0), y, tw, th);
            cx.panel(L_TOP, r, 9.0 * s, a);
            let col = match t.kind {
                1 => cx.ui.good,
                2 => cx.ui.warn,
                3 => cx.ui.bad,
                _ => cx.ui.accent,
            };
            let d = cx.px(7.0);
            let dot = Rect::new(r.x + cx.px(14.0), r.cy() - d * 0.5, d, d);
            cx.quad(L_TOP, Quad::new(dot.inset(-2.0 * s), col.a(0.35 * a)).radius(d).soft(4.0 * s));
            cx.quad(L_TOP, Quad::new(dot, col.fade(a)).radius(d * 0.5));
            let key = cx.text.label_fit(&t.text, size, false, tw - cx.px(44.0));
            let lh = TextSystem::label_h(size);
            cx.scene.text(L_TOP, key, (r.x + cx.px(30.0)).round(), (r.y + (r.h - lh) * 0.5).round(), r, cx.ui.text.fade(a));
            y -= cx.px(8.0);
            // schedule the fade-out and removal
            let end = t.born + LIFE;
            if (LIFE as f32 - age) <= 0.35 {
                cx.animating = true;
            } else {
                cx.wake_at(end - 0.35);
            }
        }
    }

    fn draw_overlay(&mut self) {
        let Some(o) = &mut self.overlay else { return };
        let cx = &mut self.cx;
        let t = cx.anim_from(mix_id(hash_str("overlay"), o.anim_id), 0.0, if o.closing { 0.0 } else { 1.0 }, 22.0);
        if o.closing && t < 0.02 {
            self.overlay = None;
            return;
        }
        let e = ease_out_cubic(t);
        match &mut o.ov {
            Ov::Palette(p) => p.draw(cx, e),
            Ov::Settings(s) => s.draw(cx, &self.cfg, &self.themes[self.theme_idx], e),
            Ov::Confirm(c) => c.draw(cx, e),
        }
    }
}

/// Shown in a pane with no tabs.
fn draw_welcome(cx: &mut crate::ui::Ctx, body: Rect) {
    let t = cx.anim_from(hash_str("welcome") ^ (body.x as u64) << 8, 0.0, 1.0, 5.0);
    let a = ease_out_cubic(t);
    let big = cx.px(46.0);
    let title_w = cx.label_w("glint", big, true);
    let star_w = cx.label_w("✦", big * 0.7, false);
    let total = title_w + star_w + cx.px(14.0);
    let rows = [("Ctrl+N", "New tab"), ("Ctrl+O", "Open a file"), ("Ctrl+Shift+P", "All commands"), ("Ctrl+T", "Themes")];
    let row_h = cx.px(30.0);
    let block_h = TextSystem::label_h(big) + cx.px(40.0) + rows.len() as f32 * row_h;
    let mut y = body.cy() - block_h * 0.5 + (1.0 - a) * cx.px(14.0);
    let x = body.cx() - total * 0.5;
    if body.h < block_h * 0.6 || body.w < total {
        return;
    }
    let glow = Rect::new(x - cx.px(30.0), y, total + cx.px(60.0), TextSystem::label_h(big));
    cx.quad(L_BASE, Quad::new(glow.inset_xy(0.0, cx.px(14.0)), cx.ui.accent.a(0.1 * a)).radius(40.0 * cx.s).soft(40.0 * cx.s).clip(body));
    cx.label(L_BASE, "✦", big * 0.7, false, x, y + TextSystem::label_h(big) * 0.16, body, cx.ui.accent.fade(a));
    cx.label(L_BASE, "glint", big, true, x + star_w + cx.px(14.0), y, body, cx.ui.text.fade(a));
    y += TextSystem::label_h(big) + cx.px(40.0);
    let key_col = body.cx() - cx.px(14.0);
    for (i, (k, label)) in rows.iter().enumerate() {
        let r = Rect::new(body.cx() - cx.px(150.0), y, cx.px(300.0), row_h);
        let hid = HitId::Welcome(i as u8);
        let hov = cx.anim(hash_str("welcome-row") ^ i as u64, if cx.is_hot(hid) { 1.0 } else { 0.0 }, 16.0);
        if hov > 0.01 {
            cx.quad(L_BASE, Quad::new(r, cx.ui.text.a(0.07 * hov * a)).radius(7.0 * cx.s).clip(body));
        }
        let kw = cx.label_w(k, cx.ui_font, false);
        let lh = TextSystem::label_h(cx.ui_font);
        cx.label(L_BASE, k, cx.ui_font, false, key_col - kw, y + (row_h - lh) * 0.5, body, cx.ui.accent.fade(a * 0.9));
        cx.label(L_BASE, label, cx.ui_font, false, key_col + cx.px(28.0), y + (row_h - lh) * 0.5, body, cx.ui.text_dim.lerp(cx.ui.text, hov).fade(a));
        cx.hit(r.intersect(&body), hid, CursorKind::Pointer);
        y += row_h;
    }
}
