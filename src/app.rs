//! The app core: documents, panes, focus, commands, input routing and
//! persistence. Drawing lives in `draw.rs`. The host (winit runner or the
//! snapshot harness) talks to this through the small API in `platform.rs`.

use crate::buffer::{EditKind, Sel};
use crate::cmd::{keymap, Cmd};
use crate::config::{config_dir, Config};
use crate::doc::{DiskState, DocId, Document};
use crate::editor::{self, Reveal, View};
use crate::find::{self, FindBar};
use crate::gfx::{BgFrame, SceneRaw};
use crate::highlight::{all_langs, comment_tokens};
use crate::overlay::{Choice, Confirm, OverlayEvent, PalAct, PalItem, PalMode, Palette, Settings};
use crate::pane::{Divider, Node, Pane};
use crate::platform::{AppOptions, DialogResult, Effect, FrameOut};
use crate::session::{self, SDoc, SNode, STab, Session};
use crate::text::TextSystem;
use crate::theme::{self, BgSpec, SyntaxColors, SyntaxTheme, Theme, UiColors};
use crate::ui::{Clip, Ctx, CursorKind, HitId, Input, Key, Mods};
use crate::util::{smoothstep, Rgba, Smooth};
use std::collections::HashMap;
use std::path::PathBuf;

pub(crate) enum Ov {
    Palette(Palette),
    Settings(Settings),
    Confirm(Confirm),
}

pub(crate) struct Overlay {
    pub ov: Ov,
    pub closing: bool,
    pub anim_id: u64,
    /// Theme to return to if a theme preview is cancelled.
    pub theme_before: usize,
    pub syntax_before: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kb {
    Editor,
    Find(u8),
}

pub(crate) enum Drag {
    /// mode: 1 = by character, 2 = by word, 3 = by line.
    Select { pane: u64, mode: u8, origin: (usize, usize) },
    Scrollbar { pane: u64, grab: f32 },
    Divider { id: u32 },
    Tab { pane: u64, idx: usize, grab: f32, x0: f32, moved: bool },
    Slider,
}

/// Something "Reopen Closed Tab" can bring back.
pub(crate) enum Closed {
    Path(PathBuf),
    /// A discarded untitled draft (kept in memory for this run only).
    Draft { text: String, lang: Option<String> },
}

pub(crate) struct Toast {
    pub id: u64,
    pub text: String,
    /// 0 info, 1 good, 2 warning, 3 error
    pub kind: u8,
    pub born: f64,
}

/// The palette and colours being faded away from during a theme change.
pub(crate) struct Fade {
    pub ui: UiColors,
    pub syn: SyntaxColors,
    pub spec: BgSpec,
    pub base: Rgba,
    pub phase: f32,
    pub t: f32,
}

pub struct App {
    pub cx: Ctx,
    pub(crate) cfg: Config,
    pub(crate) themes: Vec<Theme>,
    pub(crate) theme_idx: usize,
    pub(crate) syntaxes: Vec<SyntaxTheme>,
    /// 0 = the theme's own syntax colours, n = `syntaxes[n - 1]`.
    pub(crate) syntax_sel: usize,
    pub(crate) fade: Option<Fade>,
    pub(crate) bg_phase: f32,
    pub(crate) docs: HashMap<DocId, Document>,
    pub(crate) panes: HashMap<u64, Pane>,
    pub(crate) tree: Node,
    pub(crate) focus: u64,
    next_id: u64,
    next_untitled: u32,
    pub(crate) kb: Kb,
    pub(crate) find: FindBar,
    pub(crate) overlay: Option<Overlay>,
    pub(crate) toasts: Vec<Toast>,
    clip: Clip,
    mods: Mods,
    pub(crate) drag: Option<Drag>,
    last_click: (f64, f32, f32, u32),
    pub(crate) effects: Vec<Effect>,
    pub(crate) dirty: bool,
    pub(crate) animating: bool,
    pub(crate) wake: Option<f64>,
    pub(crate) now: f64,
    pub(crate) window_focused: bool,
    closed: Vec<Closed>,
    recent: Vec<PathBuf>,
    session_dirty: bool,
    session_saved_at: f64,
    cfg_dirty: bool,
    pub(crate) last_title: String,
    pub(crate) last_cursor: CursorKind,
    scale: f32,
    /// Whether to read/write the session and drafts at all.
    persist: bool,
    pub(crate) dividers: Vec<Divider>,
    /// Tab to close once a pending "save as" dialog succeeds: (pane, doc).
    pending_close: Option<(u64, DocId)>,
    pub(crate) last_input: f64,
    write_config: bool,
    draft_warned: bool,
}

impl App {
    pub fn new(opts: AppOptions) -> App {
        let (cfg, cfg_err) = Config::load();
        let (themes, theme_errs) = theme::load_all(Some(&config_dir().join("themes")));
        assert!(!themes.is_empty(), "no themes could be loaded");
        let theme_idx = themes.iter().position(|t| t.id == cfg.theme).unwrap_or(0);
        let (syntaxes, syntax_errs) = theme::load_syntax(Some(&config_dir().join("syntax")));
        let syntax_sel = match cfg.syntax.as_str() {
            "theme" => 0,
            id => syntaxes.iter().position(|s| s.id == id).map_or(if syntaxes.is_empty() { 0 } else { 1 }, |i| i + 1),
        };
        let text = TextSystem::new(cfg.ligatures);
        let mut cx = Ctx::new(text, themes[theme_idx].ui.clone(), themes[theme_idx].syntax.clone());
        cx.anims_on = cfg.animations;
        cx.set_metrics(1.0, cfg.font_size, cfg.line_height);
        let mut app = App {
            cx,
            cfg,
            themes,
            theme_idx,
            syntaxes,
            syntax_sel,
            fade: None,
            bg_phase: 40.0,
            docs: HashMap::new(),
            panes: HashMap::new(),
            tree: Node::Leaf(1),
            focus: 1,
            next_id: 1,
            next_untitled: 1,
            kb: Kb::Editor,
            find: FindBar::default(),
            overlay: None,
            toasts: Vec::new(),
            clip: Clip::new(opts.system_clipboard),
            mods: Mods::default(),
            drag: None,
            last_click: (-10.0, 0.0, 0.0, 0),
            effects: Vec::new(),
            dirty: true,
            animating: false,
            wake: None,
            now: 0.0,
            window_focused: true,
            closed: Vec::new(),
            recent: Vec::new(),
            session_dirty: false,
            session_saved_at: 0.0,
            cfg_dirty: false,
            last_title: String::new(),
            last_cursor: CursorKind::Default,
            scale: 1.0,
            persist: opts.restore_session,
            dividers: Vec::new(),
            pending_close: None,
            last_input: 0.0,
            write_config: opts.write_config,
            draft_warned: false,
        };
        app.panes.insert(1, Pane::new(1));
        if let Some(e) = cfg_err {
            app.toast(3, e);
        }
        for e in theme_errs.into_iter().chain(syntax_errs) {
            app.toast(3, e);
        }
        app.cx.syn = app.syntax_colors();
        if opts.restore_session {
            app.restore_session();
        }
        app.open_paths(opts.paths);
        if app.docs.is_empty() {
            app.new_tab();
        }
        app
    }

    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    pub(crate) fn theme(&self) -> &Theme {
        &self.themes[self.theme_idx]
    }

    /// The syntax colours in effect: the chosen palette, or the theme's own.
    pub(crate) fn syntax_colors(&self) -> SyntaxColors {
        match self.syntax_sel.checked_sub(1).and_then(|i| self.syntaxes.get(i)) {
            Some(s) => s.colors.clone(),
            None => self.theme().syntax.clone(),
        }
    }

    pub(crate) fn toast(&mut self, kind: u8, text: impl Into<String>) {
        let id = self.id();
        self.toasts.push(Toast { id, text: text.into(), kind, born: self.now });
        if self.toasts.len() > 5 {
            self.toasts.remove(0);
        }
        self.dirty = true;
    }

    // ------------------------------------------------------------ host API

    /// The host calls this before delivering input, so handlers running
    /// between frames (after an idle stretch) see the real time.
    pub fn set_time(&mut self, now: f64) {
        self.now = now;
    }

    /// Window size in physical pixels and the scale factor.
    pub fn resize(&mut self, w: f32, h: f32, scale: f32) {
        self.cx.size = (w.max(1.0), h.max(1.0));
        if (scale - self.scale).abs() > 1e-4 {
            self.scale = scale;
            self.cx.set_metrics(scale, self.cfg.font_size, self.cfg.line_height);
        }
        self.dirty = true;
    }

    pub fn needs_redraw(&self) -> bool {
        self.dirty
    }

    pub fn take_effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.effects)
    }

    pub fn open_paths(&mut self, paths: Vec<PathBuf>) {
        for p in paths {
            self.open_path(p);
        }
    }

    pub fn dialog_result(&mut self, r: DialogResult) {
        match r {
            DialogResult::Open(paths) => self.open_paths(paths),
            DialogResult::Save { doc, path } => {
                let pending = self.pending_close.take();
                let Some(path) = path else { return };
                let res = match self.docs.get_mut(&doc) {
                    Some(d) => d.save_as(&path).map(|_| d.title()),
                    None => return,
                };
                match res {
                    Ok(title) => {
                        self.after_save(doc);
                        self.add_recent(path);
                        self.toast(1, format!("Saved {title}"));
                        if let Some((pane, d)) = pending {
                            if d == doc {
                                self.close_doc_in_pane(pane, doc);
                            }
                        }
                    }
                    Err(e) => self.toast(3, e),
                }
            }
        }
        self.dirty = true;
    }

    /// Flush drafts, session and config. Call before exiting.
    pub fn shutdown(&mut self) {
        self.flush_drafts(true);
        self.save_session();
        if self.cfg_dirty && self.write_config {
            let _ = self.cfg.save();
        }
        self.cfg_dirty = false;
    }

    pub fn frame(&mut self, now: f64, dt: f32) -> FrameOut {
        self.now = now;
        let dt = dt.clamp(0.0, 0.05);
        let bg_on = self.cfg.bg_animate && (self.window_focused || !self.cfg.bg_pause_unfocused);
        let speed = self.cfg.bg_for(self.theme()).speed;
        if bg_on {
            self.bg_phase += dt * speed;
        }
        let mut fading = false;
        let syn_to = self.fade.is_some().then(|| self.syntax_colors());
        if let (Some(f), Some(syn_to)) = (&mut self.fade, syn_to) {
            if bg_on {
                f.phase += dt * f.spec.speed;
            }
            f.t = if self.cfg.animations { (f.t + dt / 0.55).min(1.0) } else { 1.0 };
            let e = smoothstep(f.t);
            let to = &self.themes[self.theme_idx];
            self.cx.ui = UiColors::lerp(&f.ui, &to.ui, e);
            self.cx.syn = SyntaxColors::lerp(&f.syn, &syn_to, e);
            self.dirty = true;
            fading = true;
            if f.t >= 1.0 {
                self.cx.ui = to.ui.clone();
                self.cx.syn = syn_to;
                self.fade = None;
            }
        }
        let house_wake = self.housekeeping();
        let due = self.wake.is_some_and(|w| now >= w);
        let mut scene_dirty = false;
        if self.dirty || self.animating || due {
            self.build(now, dt);
            scene_dirty = true;
        }
        let wake = match (self.wake, house_wake) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        FrameOut {
            scene_dirty,
            animating: self.animating || fading,
            wake,
            bg: self.bg_frame(),
            bg_animating: bg_on || fading,
            bg_fps: self.cfg.bg_fps,
        }
    }

    fn bg_frame(&self) -> BgFrame {
        let th = self.theme();
        let spec = self.cfg.bg_for(th);
        let cur = SceneRaw::new(&spec, th.ui.bg, self.bg_phase);
        let q = self.cfg.bg_quality;
        match &self.fade {
            Some(f) => BgFrame {
                a: SceneRaw::new(&f.spec, f.base, f.phase),
                b: cur,
                fade: smoothstep(f.t),
                scale: f.spec.kind.render_scale().max(spec.kind.render_scale()) * q,
                render: false,
                vignette: self.cfg.vignette,
                time: self.now as f32,
            },
            None => BgFrame {
                a: cur,
                b: cur,
                fade: 0.0,
                scale: spec.kind.render_scale() * q,
                render: false,
                vignette: self.cfg.vignette,
                time: self.now as f32,
            },
        }
    }

    /// Debounced background work. Returns when it next needs a frame.
    fn housekeeping(&mut self) -> Option<f64> {
        let now = self.now;
        let mut wake: Option<f64> = None;
        let mut bump = |t: f64| wake = Some(wake.map_or(t, |w: f64| w.min(t)));
        if self.persist {
            let pending = self.docs.values().filter(|d| d.buf.rev != d.draft_rev).map(|d| d.edited_at).fold(None, |a: Option<f64>, t| Some(a.map_or(t, |a| a.max(t))));
            if let Some(last) = pending {
                if now - last > 0.8 {
                    self.flush_drafts(false);
                } else {
                    bump(last + 0.85);
                }
            }
            if self.session_dirty {
                if now - self.session_saved_at > 1.5 {
                    self.save_session();
                } else {
                    bump(self.session_saved_at + 1.55);
                }
            }
        }
        if self.cfg_dirty && self.drag.is_none() {
            if self.write_config {
                if let Err(e) = self.cfg.save() {
                    log::warn!("could not save config: {e}");
                }
            }
            self.cfg_dirty = false;
        }
        wake
    }

    // ------------------------------------------------------------ persistence

    fn flush_drafts(&mut self, force: bool) {
        if !self.persist {
            return;
        }
        let now = self.now;
        let mut failed: Option<String> = None;
        let mut new_draft = false;
        for d in self.docs.values_mut() {
            if d.buf.rev == d.draft_rev || (!force && now - d.edited_at <= 0.8) {
                continue;
            }
            let want = match d.path {
                Some(_) => d.buf.is_dirty() || d.missing,
                None => d.buf.len() > 0,
            };
            if want {
                match session::write_draft(d.id, &d.contents()) {
                    Ok(()) => {
                        new_draft |= !d.has_draft_file;
                        d.has_draft_file = true;
                        d.draft_rev = d.buf.rev;
                    }
                    Err(e) => {
                        // leave it pending and try again in a few seconds
                        log::warn!("could not write draft: {e}");
                        failed = Some(e.to_string());
                        d.edited_at = now + 5.0;
                    }
                }
            } else {
                if d.has_draft_file {
                    session::remove_draft(d.id);
                    d.has_draft_file = false;
                }
                d.draft_rev = d.buf.rev;
            }
            self.session_dirty = true;
        }
        if new_draft {
            // record the new draft in the session file right away
            self.session_saved_at = f64::MIN;
        }
        if let Some(e) = failed {
            if !self.draft_warned {
                self.draft_warned = true;
                self.toast(3, format!("Could not write drafts: {e}"));
            }
        }
    }

    fn snapshot_node(&self, n: &Node) -> Option<SNode> {
        match n {
            Node::Leaf(id) => {
                let p = self.panes.get(id)?;
                Some(SNode::Leaf {
                    id: *id,
                    tabs: p
                        .tabs
                        .iter()
                        .map(|t| STab { doc: t.view.doc, anchor: t.view.sel.anchor, head: t.view.sel.head, scroll: t.view.scroll_y.target })
                        .collect(),
                    active: p.active,
                })
            }
            Node::Split(s) => {
                let a = self.snapshot_node(&s.a);
                let b = self.snapshot_node(&s.b);
                match (a, b) {
                    (Some(a), Some(b)) => Some(SNode::Split { horizontal: s.horizontal, ratio: s.ratio.target, a: Box::new(a), b: Box::new(b) }),
                    (a, b) => a.or(b),
                }
            }
        }
    }

    fn save_session(&mut self) {
        if !self.persist {
            return;
        }
        self.session_saved_at = self.now;
        self.session_dirty = false;
        let Some(layout) = self.snapshot_node(&self.tree) else { return };
        let mut docs: Vec<SDoc> = self
            .docs
            .values()
            .map(|d| SDoc {
                id: d.id,
                path: d.path.clone(),
                untitled_n: d.untitled_n,
                draft: d.has_draft_file,
                stamp: d.disk.map(|s| session::stamp_of(s.mtime, s.len)),
                lang: d.lang_forced.then(|| d.hl.lang().to_string()),
            })
            .collect();
        docs.sort_by_key(|d| d.id);
        let s = Session {
            version: 1,
            docs,
            layout,
            focus: self.focus,
            next_untitled: self.next_untitled,
            recent: self.recent.clone(),
            closed: self.closed.iter().filter_map(|c| if let Closed::Path(p) = c { Some(p.clone()) } else { None }).collect(),
        };
        if let Err(e) = session::save(&s) {
            log::warn!("could not save session: {e}");
        }
    }

    fn restore_node(&mut self, n: &SNode) -> Option<Node> {
        match n {
            SNode::Leaf { id, tabs, active } => {
                let mut pane = Pane::new(*id);
                for t in tabs {
                    let Some(d) = self.docs.get(&t.doc) else { continue };
                    let mut v = View::new(t.doc);
                    let len = d.buf.len();
                    v.sel = Sel::new(t.anchor.min(len), t.head.min(len));
                    v.scroll_y = Smooth::new(t.scroll.max(0.0));
                    let at = pane.tabs.len();
                    pane.insert_tab(at, v);
                }
                if pane.tabs.is_empty() {
                    return None;
                }
                pane.set_active((*active).min(pane.tabs.len() - 1));
                self.next_id = self.next_id.max(*id);
                self.panes.insert(*id, pane);
                Some(Node::Leaf(*id))
            }
            SNode::Split { horizontal, ratio, a, b } => {
                let na = self.restore_node(a);
                let nb = self.restore_node(b);
                match (na, nb) {
                    (Some(a), Some(b)) => {
                        let id = self.id() as u32;
                        Some(Node::Split(Box::new(crate::pane::Split {
                            id,
                            horizontal: *horizontal,
                            ratio: Smooth::new(ratio.clamp(0.1, 0.9)),
                            a,
                            b,
                            closing: None,
                            rect: crate::util::Rect::ZERO,
                        })))
                    }
                    (a, b) => a.or(b),
                }
            }
        }
    }

    fn restore_session(&mut self) {
        let Some(s) = session::load() else {
            // no (readable) session: unsaved drafts must still come back
            self.recover_orphans(&[]);
            return;
        };
        let tw = self.cfg.tab_width;
        let mut notes: Vec<(u8, String)> = Vec::new();
        for sd in &s.docs {
            self.next_id = self.next_id.max(sd.id);
            // read the draft even if the session did not record one yet (crash window)
            let draft = session::read_draft(sd.id);
            let old_stamp = sd.stamp.map(|s| {
                let (mtime, len) = session::stamp_to_time(s);
                crate::doc::DiskStamp { mtime, len }
            });
            let doc = match &sd.path {
                None => {
                    let mut d = Document::from_text(sd.id, draft.as_deref().unwrap_or(""), None, tw);
                    d.untitled_n = sd.untitled_n;
                    d.has_draft_file = draft.is_some();
                    Some(d)
                }
                Some(p) => {
                    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    let on_disk = crate::doc::DiskStamp::of(p);
                    match (draft, on_disk) {
                        (Some(text), Some(st)) if sd.stamp == Some(session::stamp_of(st.mtime, st.len)) => {
                            let mut d = Document::from_text(sd.id, &text, Some(p.clone()), tw);
                            d.disk = Some(st);
                            d.buf.mark_dirty();
                            d.has_draft_file = true;
                            Some(d)
                        }
                        (Some(text), Some(_)) => {
                            // the file changed behind our back: the disk wins
                            match Document::open(sd.id, p, tw) {
                                Ok(d) => {
                                    session::remove_draft(sd.id);
                                    notes.push((2, format!("{name} changed on disk; unsaved draft discarded")));
                                    Some(d)
                                }
                                Err(e) => {
                                    // unreadable right now: keep the draft rather than lose both
                                    notes.push((3, e));
                                    let mut d = Document::from_text(sd.id, &text, Some(p.clone()), tw);
                                    d.disk = old_stamp;
                                    d.buf.mark_dirty();
                                    d.has_draft_file = true;
                                    Some(d)
                                }
                            }
                        }
                        (Some(text), None) => {
                            // file not there (deleted, or its volume is not mounted): keep
                            // the old stamp so the draft survives if the same file returns
                            let mut d = Document::from_text(sd.id, &text, Some(p.clone()), tw);
                            d.disk = old_stamp;
                            d.buf.mark_dirty();
                            d.missing = true;
                            d.has_draft_file = true;
                            Some(d)
                        }
                        (None, Some(_)) => Document::open(sd.id, p, tw).map_err(|e| notes.push((3, e))).ok(),
                        (None, None) => {
                            notes.push((2, format!("{name} no longer exists")));
                            None
                        }
                    }
                }
            };
            if let Some(mut d) = doc {
                if let Some(l) = &sd.lang {
                    d.hl.set_lang(l);
                    d.lang_forced = true;
                }
                d.draft_rev = d.buf.rev;
                self.docs.insert(d.id, d);
            }
        }
        self.panes.clear();
        match self.restore_node(&s.layout) {
            Some(tree) => {
                self.tree = tree;
                let mut leaves = Vec::new();
                self.tree.leaves(&mut leaves);
                self.focus = if leaves.contains(&s.focus) { s.focus } else { leaves[0] };
            }
            None => {
                let id = self.id();
                self.panes.insert(id, Pane::new(id));
                self.tree = Node::Leaf(id);
                self.focus = id;
            }
        }
        // drop documents no tab refers to
        let used: Vec<u64> = self.panes.values().flat_map(|p| p.tabs.iter().map(|t| t.view.doc)).collect();
        self.docs.retain(|id, _| used.contains(id));
        self.next_untitled = s.next_untitled.max(1);
        self.recent = s.recent;
        self.closed = s.closed.into_iter().map(Closed::Path).collect();
        let known: Vec<u64> = s.docs.iter().map(|d| d.id).collect();
        self.recover_orphans(&known);
        for (k, t) in notes {
            self.toast(k, t);
        }
    }

    /// Draft files that no session entry claims (a crash before the session
    /// was written, or an unreadable session file) come back as untitled
    /// tabs. Drafts are never deleted just because they look stray.
    fn recover_orphans(&mut self, known: &[u64]) {
        let tw = self.cfg.tab_width;
        let mut n = 0;
        for id in session::list_drafts() {
            if known.contains(&id) || self.docs.contains_key(&id) {
                continue;
            }
            let Some(text) = session::read_draft(id) else { continue };
            if text.is_empty() {
                session::remove_draft(id);
                continue;
            }
            self.next_id = self.next_id.max(id);
            let mut d = Document::from_text(id, &text, None, tw);
            d.untitled_n = self.next_untitled;
            self.next_untitled += 1;
            d.has_draft_file = true;
            d.draft_rev = d.buf.rev;
            self.docs.insert(id, d);
            self.add_tab_for(id, false);
            n += 1;
        }
        if n > 0 {
            self.session_dirty = true;
            self.toast(2, format!("Recovered {n} unsaved draft{}", if n == 1 { "" } else { "s" }));
        }
    }

    /// Reload documents whose files changed on disk. The file always wins;
    /// the reload is an undoable step so nothing is lost for good.
    fn check_disk(&mut self) {
        let ids: Vec<DocId> = self.docs.keys().copied().collect();
        for id in ids {
            let (state, title) = match self.docs.get(&id) {
                Some(d) => (d.disk_state(), d.title()),
                None => continue,
            };
            match state {
                DiskState::Same => {}
                DiskState::Changed => self.reload_doc(id, &title),
                DiskState::Missing => {
                    if let Some(d) = self.docs.get_mut(&id) {
                        if !d.missing && d.disk.is_some() {
                            d.missing = true;
                            d.draft_rev = u64::MAX;
                            self.toast(2, format!("{title} was deleted on disk"));
                        }
                    }
                }
            }
        }
    }

    fn reload_doc(&mut self, id: DocId, title: &str) {
        // use one view's selection for the mapping; the rest follow the edit notes
        let mut actor: Option<(u64, usize)> = None;
        let mut sel = Sel::caret(0);
        for p in self.panes.values() {
            if let Some(i) = p.find_doc(id) {
                actor = Some((p.id, i));
                sel = p.tabs[i].view.sel;
                break;
            }
        }
        let Some(d) = self.docs.get_mut(&id) else { return };
        let was_dirty = d.buf.is_dirty();
        match d.reload(sel) {
            Ok(new_sel) => {
                if let Some((pid, i)) = actor {
                    if let Some(t) = self.panes.get_mut(&pid).and_then(|p| p.tabs.get_mut(i)) {
                        t.view.sel = new_sel;
                    }
                }
                self.sync_doc(id, actor);
                if let Some(d) = self.docs.get_mut(&id) {
                    d.draft_rev = u64::MAX; // re-evaluate the draft file
                }
                let msg = if was_dirty {
                    format!("{title} changed on disk; reloaded (undo restores your edits)")
                } else {
                    format!("{title} changed on disk; reloaded")
                };
                self.toast(if was_dirty { 2 } else { 0 }, msg);
            }
            Err(e) => self.toast(3, e),
        }
    }

    // ------------------------------------------------------------ helpers

    fn cur(&mut self) -> Option<(&mut View, &mut Document)> {
        let pane = self.panes.get_mut(&self.focus)?;
        let tab = pane.tabs.get_mut(pane.active)?;
        let doc = self.docs.get_mut(&tab.view.doc)?;
        Some((&mut tab.view, doc))
    }

    fn cur_doc_id(&self) -> Option<DocId> {
        let p = self.panes.get(&self.focus)?;
        p.tabs.get(p.active).map(|t| t.view.doc)
    }

    fn actor(&self) -> Option<(u64, usize)> {
        self.panes.get(&self.focus).map(|p| (p.id, p.active))
    }

    /// Propagate buffer changes to the highlighter and every view.
    fn sync_doc(&mut self, id: DocId, actor: Option<(u64, usize)>) {
        let Some(doc) = self.docs.get_mut(&id) else { return };
        let notes = doc.buf.take_notes();
        if notes.is_empty() {
            return;
        }
        for n in &notes {
            doc.hl.edit(n.line, n.lines_removed, n.lines_inserted);
        }
        doc.edited_at = self.now;
        let tuples: Vec<(usize, usize, usize)> = notes.iter().map(|n| (n.line, n.lines_removed, n.lines_inserted)).collect();
        let len = doc.buf.len();
        for (pid, pane) in self.panes.iter_mut() {
            for (ti, tab) in pane.tabs.iter_mut().enumerate() {
                if tab.view.doc != id {
                    continue;
                }
                tab.view.wrap.edit(&doc.buf.rope, &tuples);
                if actor != Some((*pid, ti)) {
                    for n in &notes {
                        tab.view.sel = n.map_sel(tab.view.sel);
                    }
                }
                tab.view.clamp(len);
            }
        }
        self.session_dirty = true;
        self.dirty = true;
    }

    /// Run an editing operation on the focused view and sync everything.
    fn edit(&mut self, f: impl FnOnce(&mut View, &mut Document)) {
        let now = self.now;
        let actor = self.actor();
        let Some((v, d)) = self.cur() else { return };
        f(v, d);
        v.goal = None;
        v.touch(now);
        let id = d.id;
        self.sync_doc(id, actor);
        self.dirty = true;
    }

    /// Run a caret movement on the focused view.
    fn nav(&mut self, f: impl FnOnce(&mut View, &Document)) {
        let now = self.now;
        if let Some((v, d)) = self.cur() {
            f(v, d);
            v.touch(now);
        }
        self.dirty = true;
    }

    fn view_count(&self, doc: DocId) -> usize {
        self.panes.values().map(|p| p.tabs.iter().filter(|t| t.view.doc == doc).count()).sum()
    }

    fn add_recent(&mut self, p: PathBuf) {
        self.recent.retain(|r| r != &p);
        self.recent.insert(0, p);
        self.recent.truncate(40);
        self.session_dirty = true;
    }

    fn overlay_active(&self) -> bool {
        self.overlay.as_ref().is_some_and(|o| !o.closing)
    }

    // ------------------------------------------------------------ tabs & panes

    fn focus_pane(&mut self, id: u64) {
        if self.panes.contains_key(&id) && self.focus != id {
            self.focus = id;
            self.find.invalidate();
        }
        self.dirty = true;
    }

    fn new_tab(&mut self) {
        let id = self.id();
        let n = self.next_untitled;
        self.next_untitled += 1;
        self.docs.insert(id, Document::untitled(id, n, self.cfg.tab_width));
        self.add_tab_for(id, false);
        self.kb = Kb::Editor;
    }

    /// Put a new tab for `doc` next to the active one in the focused pane.
    fn add_tab_for(&mut self, doc: DocId, replace_pristine: bool) {
        let now = self.now;
        let Some(pane) = self.panes.get(&self.focus) else { return };
        let mut at = if pane.tabs.is_empty() { 0 } else { pane.active + 1 };
        // an untouched empty draft gets replaced rather than left lying around
        if replace_pristine {
            if let Some(t) = pane.tabs.get(pane.active) {
                let old = t.view.doc;
                let pristine = self.docs.get(&old).is_some_and(|d| d.path.is_none() && d.buf.len() == 0 && !d.buf.is_dirty());
                if pristine && self.view_count(old) == 1 {
                    at = pane.active;
                    let pane = self.panes.get_mut(&self.focus).unwrap();
                    pane.remove_tab(at, String::new(), now - 10.0);
                    self.docs.remove(&old);
                    session::remove_draft(old);
                }
            }
        }
        if let Some(pane) = self.panes.get_mut(&self.focus) {
            let mut v = View::new(doc);
            v.blink_at = now;
            pane.insert_tab(at, v);
        }
        self.session_dirty = true;
        self.dirty = true;
    }

    fn show_doc(&mut self, doc: DocId) {
        let mut target = None;
        if let Some(p) = self.panes.get(&self.focus) {
            if let Some(i) = p.find_doc(doc) {
                target = Some((p.id, i));
            }
        }
        if target.is_none() {
            target = self.panes.values().find_map(|p| p.find_doc(doc).map(|i| (p.id, i)));
        }
        if let Some((pid, i)) = target {
            self.focus_pane(pid);
            if let Some(p) = self.panes.get_mut(&pid) {
                p.set_active(i);
            }
        }
    }

    fn open_path(&mut self, path: PathBuf) {
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        if let Some(id) = self.docs.values().find(|d| d.path.as_deref() == Some(path.as_path())).map(|d| d.id) {
            self.show_doc(id);
            return;
        }
        let id = self.id();
        let tw = self.cfg.tab_width;
        let doc = if path.exists() {
            match Document::open(id, &path, tw) {
                Ok(d) => d,
                Err(e) => {
                    self.toast(3, e);
                    return;
                }
            }
        } else {
            Document::from_text(id, "", Some(path.clone()), tw)
        };
        self.docs.insert(id, doc);
        self.add_recent(path);
        self.add_tab_for(id, true);
        self.kb = Kb::Editor;
    }

    fn close_tab(&mut self, pid: u64, idx: usize, force: bool) {
        let Some(doc_id) = self.panes.get(&pid).and_then(|p| p.tabs.get(idx)).map(|t| t.view.doc) else { return };
        let views = self.view_count(doc_id);
        let (dirty, title) = self.docs.get(&doc_id).map(|d| (d.is_dirty(), d.title())).unwrap_or((false, String::new()));
        if views <= 1 && dirty && !force {
            self.focus_pane(pid);
            if let Some(p) = self.panes.get_mut(&pid) {
                p.set_active(idx);
            }
            self.open_overlay(Ov::Confirm(Confirm::new(pid, doc_id, title)));
            return;
        }
        let now = self.now;
        let mut empty = false;
        if let Some(p) = self.panes.get_mut(&pid) {
            p.remove_tab(idx, title, now);
            empty = p.tabs.is_empty();
        }
        if views <= 1 {
            if let Some(d) = self.docs.remove(&doc_id) {
                let entry = match &d.path {
                    Some(p) => Some(Closed::Path(p.clone())),
                    // a discarded draft can still be reopened during this run
                    None if d.buf.len() > 0 => {
                        Some(Closed::Draft { text: d.contents(), lang: d.lang_forced.then(|| d.hl.lang().to_string()) })
                    }
                    None => None,
                };
                if let Some(e) = entry {
                    self.closed.push(e);
                    if self.closed.len() > 30 {
                        self.closed.remove(0);
                    }
                }
            }
            session::remove_draft(doc_id);
        }
        if empty && self.panes.len() > 1 {
            self.remove_pane(pid);
        }
        self.find.invalidate();
        self.session_dirty = true;
        self.dirty = true;
    }

    fn close_doc_in_pane(&mut self, pid: u64, doc: DocId) {
        if let Some(i) = self.panes.get(&pid).and_then(|p| p.find_doc(doc)) {
            self.close_tab(pid, i, true);
        }
    }

    /// Remove an (empty) pane and collapse its split.
    fn remove_pane(&mut self, pid: u64) {
        if self.panes.len() <= 1 {
            return;
        }
        // the split tree collapses the dead leaf on its next prune
        self.panes.remove(&pid);
        if self.focus == pid {
            let mut leaves = Vec::new();
            self.tree.leaves(&mut leaves);
            if let Some(n) = leaves.into_iter().find(|l| self.panes.contains_key(l)) {
                self.focus = n;
            }
        }
        self.session_dirty = true;
        self.dirty = true;
    }

    fn split(&mut self, horizontal: bool) {
        let view = self.panes.get(&self.focus).and_then(|p| p.tabs.get(p.active)).map(|t| t.view.duplicate());
        let new_pid = self.id();
        let split_id = self.id() as u32;
        if !self.tree.split(self.focus, new_pid, split_id, horizontal) {
            return;
        }
        let mut pane = Pane::new(new_pid);
        let had = view.is_some();
        if let Some(mut v) = view {
            v.reveal = Reveal::Near;
            pane.insert_tab(0, v);
        }
        self.panes.insert(new_pid, pane);
        self.focus = new_pid;
        if !had {
            self.new_tab();
        }
        self.session_dirty = true;
        self.dirty = true;
    }

    fn move_tab(&mut self, from: u64, idx: usize, to: u64, at: Option<usize>) {
        if from == to || !self.panes.contains_key(&to) {
            return;
        }
        let now = self.now;
        let Some(src) = self.panes.get_mut(&from) else { return };
        let Some(view) = src.remove_tab(idx, String::new(), now - 10.0) else { return };
        let empty = src.tabs.is_empty();
        let doc = view.doc;
        if let Some(dst) = self.panes.get_mut(&to) {
            match dst.find_doc(doc) {
                Some(i) => dst.set_active(i),
                None => {
                    let at = at.unwrap_or(dst.tabs.len());
                    dst.insert_tab(at, view);
                }
            }
        }
        if empty {
            self.remove_pane(from);
        }
        self.focus_pane(to);
        self.session_dirty = true;
    }

    fn pane_order(&self) -> Vec<u64> {
        let mut leaves = Vec::new();
        self.tree.leaves(&mut leaves);
        leaves.retain(|l| self.panes.contains_key(l));
        leaves
    }

    fn focus_dir(&mut self, dx: i32, dy: i32) {
        let Some(cur) = self.panes.get(&self.focus).map(|p| p.rect) else { return };
        let mut best: Option<(f32, u64)> = None;
        for p in self.panes.values() {
            if p.id == self.focus {
                continue;
            }
            let r = p.rect;
            let ok = if dx < 0 {
                r.right() <= cur.x + 2.0
            } else if dx > 0 {
                r.x >= cur.right() - 2.0
            } else if dy < 0 {
                r.bottom() <= cur.y + 2.0
            } else {
                r.y >= cur.bottom() - 2.0
            };
            if !ok {
                continue;
            }
            let d = (r.cx() - cur.cx()).abs() + (r.cy() - cur.cy()).abs();
            if best.is_none_or(|(bd, _)| d < bd) {
                best = Some((d, p.id));
            }
        }
        if let Some((_, id)) = best {
            self.focus_pane(id);
            self.kb = Kb::Editor;
        }
    }

    // ------------------------------------------------------------ saving

    fn after_save(&mut self, id: DocId) {
        if let Some(d) = self.docs.get_mut(&id) {
            if d.has_draft_file {
                session::remove_draft(id);
                d.has_draft_file = false;
            }
            d.draft_rev = d.buf.rev;
        }
        self.session_dirty = true;
        self.dirty = true;
    }

    fn ask_save_path(&mut self, id: DocId) {
        let Some(d) = self.docs.get(&id) else { return };
        let (dir, name) = match &d.path {
            Some(p) => (p.parent().map(|p| p.to_path_buf()), p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()),
            None => {
                let dir = self.recent.first().and_then(|p| p.parent().map(|p| p.to_path_buf())).or_else(dirs::home_dir);
                let stem: String = d
                    .title()
                    .chars()
                    .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_'))
                    .collect::<String>()
                    .trim()
                    .replace(' ', "-")
                    .to_lowercase();
                (dir, format!("{}.txt", if stem.is_empty() { "untitled" } else { &stem }))
            }
        };
        self.effects.push(Effect::SaveDialog { doc: id, dir, name });
    }

    fn save_doc(&mut self, id: DocId, then_close: Option<u64>) {
        let Some(d) = self.docs.get_mut(&id) else { return };
        if d.path.is_none() {
            self.pending_close = then_close.map(|p| (p, id));
            self.ask_save_path(id);
            return;
        }
        let title = d.title();
        match d.save() {
            Ok(()) => {
                self.after_save(id);
                self.toast(1, format!("Saved {title}"));
                if let Some(pane) = then_close {
                    self.close_doc_in_pane(pane, id);
                }
            }
            Err(e) => self.toast(3, e),
        }
    }

    // ------------------------------------------------------------ themes & config

    /// Start a cross-fade from whatever is on screen right now.
    fn begin_fade(&mut self) {
        let th = self.theme();
        let (spec, base, phase) = match &self.fade {
            Some(f) if f.t < 0.5 => (f.spec.clone(), f.base, f.phase),
            _ => (self.cfg.bg_for(th), th.ui.bg, self.bg_phase),
        };
        self.fade = Some(Fade { ui: self.cx.ui.clone(), syn: self.cx.syn.clone(), spec, base, phase, t: 0.0 });
    }

    /// Pick the syntax palette (0 = follow the theme).
    pub(crate) fn set_syntax(&mut self, sel: usize, persist: bool) {
        if sel > self.syntaxes.len() {
            return;
        }
        if sel != self.syntax_sel {
            self.begin_fade();
            self.syntax_sel = sel;
        }
        let id = sel.checked_sub(1).map_or("theme".to_string(), |i| self.syntaxes[i].id.clone());
        if persist && self.cfg.syntax != id {
            self.cfg.syntax = id;
            self.cfg_dirty = true;
        }
        self.dirty = true;
    }

    pub(crate) fn set_theme(&mut self, idx: usize, persist: bool) {
        if idx >= self.themes.len() {
            return;
        }
        if idx != self.theme_idx {
            self.begin_fade();
            self.theme_idx = idx;
        }
        if persist && self.cfg.theme != self.themes[idx].id {
            self.cfg.theme = self.themes[idx].id.clone();
            self.cfg_dirty = true;
        }
        self.dirty = true;
    }

    pub(crate) fn apply_cfg(&mut self) {
        self.cfg.sanitize();
        self.cx.text.set_ligatures(self.cfg.ligatures);
        self.cx.set_metrics(self.scale, self.cfg.font_size, self.cfg.line_height);
        self.cx.anims_on = self.cfg.animations;
        self.cfg_dirty = true;
        self.dirty = true;
    }

    // ------------------------------------------------------------ overlays

    pub(crate) fn open_overlay(&mut self, ov: Ov) {
        let anim_id = self.id();
        let (theme_before, syntax_before) = match &self.overlay {
            Some(o) if !o.closing => (o.theme_before, o.syntax_before),
            _ => (self.theme_idx, self.syntax_sel),
        };
        self.overlay = Some(Overlay { ov, closing: false, anim_id, theme_before, syntax_before });
        self.dirty = true;
    }

    fn close_overlay(&mut self, revert_theme: bool) {
        if let Some(o) = &mut self.overlay {
            o.closing = true;
            let (before, syn_before) = (o.theme_before, o.syntax_before);
            let mode = if let Ov::Palette(p) = &o.ov { Some(p.mode) } else { None };
            if revert_theme && mode == Some(PalMode::Themes) {
                self.set_theme(before, false);
            }
            if revert_theme && mode == Some(PalMode::Syntax) {
                self.set_syntax(syn_before, false);
            }
        }
        self.dirty = true;
    }

    fn open_palette(&mut self, mode: PalMode) {
        let (items, selected, hint): (Vec<PalItem>, usize, &str) = match mode {
            PalMode::Commands => (
                Cmd::ALL
                    .iter()
                    .filter(|c| !matches!(c, Cmd::Palette))
                    .map(|c| PalItem { label: c.title().to_string(), detail: c.keys().to_string(), act: PalAct::Cmd(*c) })
                    .collect(),
                0,
                "Type a command",
            ),
            PalMode::Files => {
                let mut items = Vec::new();
                let mut open: Vec<PathBuf> = Vec::new();
                for pid in self.pane_order() {
                    let p = &self.panes[&pid];
                    for (i, t) in p.tabs.iter().enumerate() {
                        if let Some(d) = self.docs.get(&t.view.doc) {
                            if let Some(path) = &d.path {
                                open.push(path.clone());
                            }
                            items.push(PalItem { label: d.title(), detail: d.display_path(), act: PalAct::Tab { pane: pid, idx: i } });
                        }
                    }
                }
                for r in &self.recent {
                    if open.contains(r) {
                        continue;
                    }
                    let name = r.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    items.push(PalItem { label: name, detail: format!("recent · {}", r.display()), act: PalAct::Path(r.clone()) });
                }
                (items, 0, "Go to a tab or recent file")
            }
            PalMode::Themes => (
                self.themes
                    .iter()
                    .enumerate()
                    .map(|(i, t)| PalItem { label: t.name.clone(), detail: format!("{:?}", t.bg.kind).to_lowercase(), act: PalAct::Theme(i) })
                    .collect(),
                self.theme_idx,
                "Pick a theme",
            ),
            PalMode::Langs => {
                let cur = self.cur_doc_id().and_then(|id| self.docs.get(&id)).map(|d| d.hl.lang().to_string()).unwrap_or_default();
                let langs = all_langs();
                let sel = langs.iter().position(|l| *l == cur).unwrap_or(0);
                (langs.into_iter().map(|l| PalItem { label: l.clone(), detail: String::new(), act: PalAct::Lang(l) }).collect(), sel, "Set the language of this tab")
            }
            PalMode::Syntax => {
                let mut items = vec![PalItem { label: "Match theme".into(), detail: "the theme's own colours".into(), act: PalAct::Syntax(0) }];
                items.extend(self.syntaxes.iter().enumerate().map(|(i, s)| PalItem { label: s.name.clone(), detail: String::new(), act: PalAct::Syntax(i + 1) }));
                (items, self.syntax_sel, "Pick syntax colours")
            }
            PalMode::Goto => (Vec::new(), 0, "Line number, or line:column"),
        };
        self.open_overlay(Ov::Palette(Palette::new(mode, items, selected, hint)));
    }

    fn run_act(&mut self, act: PalAct) {
        match act {
            PalAct::Cmd(c) => self.run(c),
            PalAct::Tab { pane, idx } => {
                self.focus_pane(pane);
                if let Some(p) = self.panes.get_mut(&pane) {
                    p.set_active(idx);
                }
                self.kb = Kb::Editor;
            }
            PalAct::Path(p) => self.open_path(p),
            PalAct::Theme(i) => {
                self.set_theme(i, true);
            }
            PalAct::Syntax(i) => self.set_syntax(i, true),
            PalAct::Lang(l) => {
                if let Some((_, d)) = self.cur() {
                    d.hl.set_lang(&l);
                    d.lang_forced = true;
                }
                self.session_dirty = true;
            }
        }
        self.dirty = true;
    }

    fn overlay_event(&mut self, ev: OverlayEvent) {
        match ev {
            OverlayEvent::None => {}
            OverlayEvent::Close => self.close_overlay(true),
            OverlayEvent::Act(act) => {
                self.close_overlay(false);
                self.run_act(act);
            }
            OverlayEvent::Preview(PalAct::Theme(i)) => self.set_theme(i, false),
            OverlayEvent::Preview(PalAct::Syntax(i)) => self.set_syntax(i, false),
            OverlayEvent::Preview(_) => {}
            OverlayEvent::Goto(s) => {
                self.close_overlay(false);
                self.goto(&s);
            }
            OverlayEvent::Confirm(choice) => {
                let target = match &self.overlay {
                    Some(Overlay { ov: Ov::Confirm(c), .. }) => Some((c.pane, c.doc)),
                    _ => None,
                };
                self.close_overlay(false);
                if let Some((pane, doc)) = target {
                    match choice {
                        Choice::DontSave => self.close_doc_in_pane(pane, doc),
                        Choice::Save => self.save_doc(doc, Some(pane)),
                        Choice::Cancel => {}
                    }
                }
            }
            OverlayEvent::Changed => self.apply_cfg(),
            OverlayEvent::PickTheme => self.open_palette(PalMode::Themes),
            OverlayEvent::ResetBg => {
                let id = self.theme().id.clone();
                self.cfg.bg.remove(&id);
                self.apply_cfg();
            }
        }
        self.dirty = true;
    }

    fn goto(&mut self, s: &str) {
        let mut parts = s.trim().split([':', ',', ' ']).filter(|p| !p.is_empty());
        let Some(line) = parts.next().and_then(|p| p.parse::<usize>().ok()) else { return };
        let col = parts.next().and_then(|p| p.parse::<usize>().ok()).unwrap_or(1);
        self.nav(|v, d| {
            let l = line.saturating_sub(1).min(d.buf.lines().saturating_sub(1));
            let start = d.buf.line_start(l);
            let end = d.buf.line_end(l);
            v.set_head(start.saturating_add(col.saturating_sub(1)).min(end), false);
            v.reveal = Reveal::Center;
        });
        self.kb = Kb::Editor;
    }

    // ------------------------------------------------------------ find

    fn open_find(&mut self, replace: bool) {
        let seed = self.cur().and_then(|(v, d)| {
            let (a, b) = (v.sel.start(), v.sel.end());
            (a != b && b - a < 200 && d.buf.line_of(a) == d.buf.line_of(b)).then(|| d.buf.slice(a, b))
        });
        self.find.open = true;
        if replace {
            self.find.show_replace = true;
        }
        if let Some(s) = seed {
            self.find.q.set(&s);
        }
        self.find.q.select_all();
        self.kb = Kb::Find(0);
        self.find.invalidate();
        self.refresh_find();
        self.dirty = true;
    }

    fn close_find(&mut self) {
        self.find.open = false;
        self.kb = Kb::Editor;
        self.dirty = true;
    }

    pub(crate) fn refresh_find(&mut self) {
        if !self.find.open {
            return;
        }
        let Some(id) = self.cur_doc_id() else { return };
        if let Some(d) = self.docs.get(&id) {
            self.find.refresh(d);
        }
        if let Some((v, _)) = self.cur() {
            let (a, b) = (v.sel.start(), v.sel.end());
            let cur = self.find.exact(a, b);
            self.find.current = cur;
        }
    }

    fn select_match(&mut self, i: usize) {
        let Some(&(a, b)) = self.find.matches.get(i) else { return };
        self.find.current = Some(i);
        self.nav(|v, _| {
            v.sel = Sel::new(a, b);
            v.goal = None;
            v.reveal = Reveal::Near;
        });
    }

    fn find_step(&mut self, forward: bool) {
        if !self.find.open {
            self.open_find(false);
            return;
        }
        self.refresh_find();
        let Some((v, _)) = self.cur() else { return };
        let (a, b) = (v.sel.start(), v.sel.end());
        let i = if forward { self.find.at_or_after(if a != b { a + 1 } else { a }) } else { self.find.before(a) };
        if let Some(i) = i {
            self.select_match(i);
        }
    }

    /// The query changed: jump to the first match from the selection start.
    fn find_changed(&mut self) {
        self.find.invalidate();
        self.refresh_find();
        let Some((v, _)) = self.cur() else { return };
        let a = v.sel.start();
        if let Some(i) = self.find.at_or_after(a) {
            self.select_match(i);
        }
        self.dirty = true;
    }

    fn replace_one(&mut self) {
        self.refresh_find();
        let Some(i) = self.find.current else {
            self.find_step(true);
            return;
        };
        let Some(&(a, b)) = self.find.matches.get(i) else { return };
        let rep = match self.cur_doc_id().and_then(|id| self.docs.get(&id)) {
            Some(d) => self.find.replacement_at(d, a, b),
            None => return,
        };
        let n = rep.chars().count();
        self.edit(|v, d| {
            d.buf.begin(v.sel);
            d.buf.replace(a, b, &rep);
            v.sel = Sel::caret(a + n);
            d.buf.commit(v.sel, EditKind::Other);
        });
        self.find.invalidate();
        self.refresh_find();
        if let Some(i) = self.find.at_or_after(a + n) {
            self.select_match(i);
        }
    }

    fn replace_all(&mut self) {
        self.refresh_find();
        // the highlighted list is capped; replace-all works on every match
        let reps = match self.cur_doc_id().and_then(|id| self.docs.get(&id)) {
            Some(d) => self.find.replacements(d),
            None => return,
        };
        if reps.is_empty() {
            return;
        }
        let count = reps.len();
        self.edit(|v, d| {
            // carry the caret through the edits
            let old = v.sel.head;
            let mut shift: isize = 0;
            let mut head = old as isize;
            for ((a, b), rep) in &reps {
                if *b <= old {
                    shift += rep.chars().count() as isize - (*b - *a) as isize;
                } else {
                    if *a < old {
                        head = *a as isize;
                    }
                    break;
                }
            }
            d.buf.begin(v.sel);
            for ((a, b), rep) in reps.iter().rev() {
                d.buf.replace(*a, *b, rep);
            }
            v.sel = Sel::caret(((head + shift).max(0) as usize).min(d.buf.len()));
            d.buf.commit(v.sel, EditKind::Other);
        });
        self.find.invalidate();
        self.refresh_find();
        self.toast(0, format!("Replaced {count} occurrence{}", if count == 1 { "" } else { "s" }));
    }

    fn find_button(&mut self, b: u8) {
        match b {
            find::B_CASE => self.find.case = !self.find.case,
            find::B_WORD => self.find.word = !self.find.word,
            find::B_REGEX => self.find.regex = !self.find.regex,
            find::B_PREV => return self.find_step(false),
            find::B_NEXT => return self.find_step(true),
            find::B_CLOSE => return self.close_find(),
            find::B_REPLACE => return self.replace_one(),
            find::B_ALL => return self.replace_all(),
            find::B_TOGGLE => {
                self.find.show_replace = !self.find.show_replace;
                if !self.find.show_replace && self.kb == Kb::Find(1) {
                    self.kb = Kb::Find(0);
                }
                self.dirty = true;
                return;
            }
            _ => return,
        }
        self.find_changed();
    }

    /// Keys while a find field has focus. Returns true if consumed.
    fn find_key(&mut self, f: u8, key: Key, mods: Mods, text: Option<&str>) -> bool {
        match key {
            Key::Escape => {
                self.close_find();
                return true;
            }
            Key::Enter => {
                if f == 0 {
                    self.find_step(!mods.shift);
                } else if mods.ctrl {
                    self.replace_all();
                } else {
                    self.replace_one();
                }
                return true;
            }
            Key::Tab => {
                if self.find.show_replace {
                    self.kb = Kb::Find(1 - f);
                }
                return true;
            }
            Key::Char(c) if mods.alt && !mods.ctrl => {
                let b = match c.to_ascii_lowercase() {
                    'c' => find::B_CASE,
                    'w' => find::B_WORD,
                    'r' => find::B_REGEX,
                    _ => return false,
                };
                self.find_button(b);
                return true;
            }
            _ => {}
        }
        let printable = text.is_some_and(|t| !t.is_empty() && !t.chars().any(|c| c.is_control())) && !mods.ctrl && !mods.alt;
        let field_key = match key {
            Key::Left | Key::Right | Key::Home | Key::End | Key::Backspace | Key::Delete => true,
            Key::Char(c) if mods.ctrl && !mods.alt && !mods.shift => matches!(c.to_ascii_lowercase(), 'a' | 'c' | 'v' | 'x'),
            _ => printable,
        };
        if !field_key {
            return false;
        }
        let field = if f == 0 { &mut self.find.q } else { &mut self.find.r };
        let changed = field.key(key, mods, text, &mut self.clip);
        if changed && f == 0 {
            self.find_changed();
        }
        true
    }

    // ------------------------------------------------------------ commands

    pub(crate) fn run(&mut self, cmd: Cmd) {
        self.dirty = true;
        let tw = self.cfg.tab_width;
        match cmd {
            Cmd::NewTab => self.new_tab(),
            Cmd::OpenFile => {
                let dir = self
                    .cur_doc_id()
                    .and_then(|id| self.docs.get(&id))
                    .and_then(|d| d.path.as_ref())
                    .and_then(|p| p.parent().map(|p| p.to_path_buf()))
                    .or_else(|| self.recent.first().and_then(|p| p.parent().map(|p| p.to_path_buf())));
                self.effects.push(Effect::OpenDialog { dir });
            }
            Cmd::Save => {
                if let Some(id) = self.cur_doc_id() {
                    self.save_doc(id, None);
                }
            }
            Cmd::SaveAs => {
                if let Some(id) = self.cur_doc_id() {
                    self.pending_close = None;
                    self.ask_save_path(id);
                }
            }
            Cmd::SaveAll => {
                let ids: Vec<DocId> = self.docs.values().filter(|d| d.path.is_some() && d.is_dirty()).map(|d| d.id).collect();
                let n = ids.len();
                for id in ids {
                    if let Some(d) = self.docs.get_mut(&id) {
                        match d.save() {
                            Ok(()) => self.after_save(id),
                            Err(e) => self.toast(3, e),
                        }
                    }
                }
                self.toast(1, format!("Saved {n} file{}", if n == 1 { "" } else { "s" }));
            }
            Cmd::CloseTab => {
                if let Some((pid, idx)) = self.actor() {
                    if self.panes[&pid].tabs.is_empty() {
                        self.remove_pane(pid);
                    } else {
                        self.close_tab(pid, idx, false);
                    }
                }
            }
            Cmd::CloseOthers => {
                let Some((pid, keep)) = self.actor() else { return };
                let Some(keep_doc) = self.panes[&pid].tabs.get(keep).map(|t| t.view.doc) else { return };
                let mut kept = 0;
                let mut i = 0;
                while let Some(doc) = self.panes.get(&pid).and_then(|p| p.tabs.get(i)).map(|t| t.view.doc) {
                    if doc == keep_doc {
                        i += 1;
                        continue;
                    }
                    let dirty = self.docs.get(&doc).is_some_and(|d| d.is_dirty()) && self.view_count(doc) <= 1;
                    if dirty {
                        kept += 1;
                        i += 1;
                    } else {
                        self.close_tab(pid, i, true);
                    }
                }
                if kept > 0 {
                    self.toast(0, format!("Kept {kept} unsaved tab{}", if kept == 1 { "" } else { "s" }));
                }
            }
            Cmd::ReopenTab => match self.closed.pop() {
                Some(Closed::Path(p)) => self.open_path(p),
                Some(Closed::Draft { text, lang }) => {
                    let id = self.id();
                    let mut d = Document::from_text(id, &text, None, tw);
                    d.untitled_n = self.next_untitled;
                    self.next_untitled += 1;
                    if let Some(l) = lang {
                        d.hl.set_lang(&l);
                        d.lang_forced = true;
                    }
                    d.draft_rev = u64::MAX; // write its draft file again
                    self.docs.insert(id, d);
                    self.add_tab_for(id, true);
                    self.kb = Kb::Editor;
                }
                None => self.toast(0, "No closed tab to reopen"),
            },
            Cmd::NextTab | Cmd::PrevTab => {
                if let Some(p) = self.panes.get_mut(&self.focus) {
                    let n = p.tabs.len();
                    if n > 0 {
                        let i = if cmd == Cmd::NextTab { (p.active + 1) % n } else { (p.active + n - 1) % n };
                        p.set_active(i);
                    }
                }
                self.find.invalidate();
            }
            Cmd::Undo | Cmd::Redo => self.edit(|v, d| {
                let s = if cmd == Cmd::Undo { d.buf.undo() } else { d.buf.redo() };
                if let Some(s) = s {
                    v.sel = s;
                }
            }),
            Cmd::Cut | Cmd::Copy => {
                let Some((v, d)) = self.cur() else { return };
                // with no selection, act on the whole line
                let (a, b) = if v.sel.is_empty() {
                    let l = d.buf.line_of(v.sel.head);
                    (d.buf.line_start(l), (d.buf.line_end(l) + 1).min(d.buf.len()))
                } else {
                    (v.sel.start(), v.sel.end())
                };
                let mut text = d.buf.slice(a, b);
                if v.sel.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                self.clip.set(&text);
                if cmd == Cmd::Cut {
                    self.edit(|v, d| {
                        if v.sel.is_empty() {
                            d.buf.delete_lines(&mut v.sel);
                        } else {
                            d.buf.insert(&mut v.sel, "", EditKind::Other);
                        }
                    });
                }
            }
            Cmd::Paste => {
                let text = self.clip.get();
                if !text.is_empty() {
                    self.edit(|v, d| d.buf.insert(&mut v.sel, &text, EditKind::Other));
                }
            }
            Cmd::SelectAll => self.nav(|v, d| {
                v.sel = Sel::new(0, d.buf.len());
                v.reveal = Reveal::None;
            }),
            Cmd::DuplicateLine => self.edit(|v, d| d.buf.duplicate_lines(&mut v.sel)),
            Cmd::MoveLineUp => self.edit(|v, d| d.buf.move_lines(&mut v.sel, true)),
            Cmd::MoveLineDown => self.edit(|v, d| d.buf.move_lines(&mut v.sel, false)),
            Cmd::DeleteLine => self.edit(|v, d| d.buf.delete_lines(&mut v.sel)),
            Cmd::ToggleComment => self.edit(|v, d| {
                let (p, s) = comment_tokens(d.hl.lang());
                if !p.is_empty() {
                    d.buf.toggle_comment(&mut v.sel, p, s);
                }
            }),
            Cmd::Indent => self.edit(|v, d| {
                let unit = d.indent_unit();
                d.buf.indent(&mut v.sel, &unit)
            }),
            Cmd::Outdent => self.edit(|v, d| {
                let w = d.indent_width;
                d.buf.outdent(&mut v.sel, w)
            }),
            Cmd::Find => self.open_find(false),
            Cmd::Replace => self.open_find(true),
            Cmd::FindNext => self.find_step(true),
            Cmd::FindPrev => self.find_step(false),
            Cmd::GotoLine => self.open_palette(PalMode::Goto),
            Cmd::Palette => self.open_palette(PalMode::Commands),
            Cmd::QuickOpen => self.open_palette(PalMode::Files),
            Cmd::Themes => self.open_palette(PalMode::Themes),
            Cmd::SyntaxColors => self.open_palette(PalMode::Syntax),
            Cmd::Languages => self.open_palette(PalMode::Langs),
            Cmd::Settings => self.open_overlay(Ov::Settings(Settings::new())),
            Cmd::SplitRight => self.split(true),
            Cmd::SplitDown => self.split(false),
            Cmd::ClosePane => {
                let order = self.pane_order();
                if order.len() < 2 {
                    self.toast(0, "Only one pane is open");
                    return;
                }
                let from = self.focus;
                let pos = order.iter().position(|p| *p == from).unwrap_or(0);
                let to = order[if pos == 0 { 1 } else { pos - 1 }];
                // merge its tabs into the neighbour so nothing is lost
                while self.panes.get(&from).is_some_and(|p| !p.tabs.is_empty()) {
                    self.move_tab(from, 0, to, None);
                }
                if self.panes.contains_key(&from) {
                    self.remove_pane(from);
                }
                self.focus_pane(to);
            }
            Cmd::FocusNextPane => {
                let order = self.pane_order();
                if let Some(pos) = order.iter().position(|p| *p == self.focus) {
                    self.focus_pane(order[(pos + 1) % order.len()]);
                    self.kb = Kb::Editor;
                }
            }
            Cmd::FocusLeft => self.focus_dir(-1, 0),
            Cmd::FocusRight => self.focus_dir(1, 0),
            Cmd::FocusUp => self.focus_dir(0, -1),
            Cmd::FocusDown => self.focus_dir(0, 1),
            Cmd::MoveTabToNextPane => {
                let order = self.pane_order();
                if order.len() < 2 {
                    self.toast(0, "No other pane; split first (Ctrl+\\)");
                    return;
                }
                if let (Some(pos), Some((from, idx))) = (order.iter().position(|p| *p == self.focus), self.actor()) {
                    self.move_tab(from, idx, order[(pos + 1) % order.len()], None);
                }
            }
            Cmd::ZoomIn | Cmd::ZoomOut | Cmd::ZoomReset => {
                self.cfg.font_size = match cmd {
                    Cmd::ZoomIn => self.cfg.font_size + 1.0,
                    Cmd::ZoomOut => self.cfg.font_size - 1.0,
                    _ => Config::default().font_size,
                };
                self.apply_cfg();
                for p in self.panes.values_mut() {
                    if let Some(v) = p.active_view() {
                        v.reveal = Reveal::Near;
                    }
                }
            }
            Cmd::ToggleWrap => {
                self.cfg.wrap = !self.cfg.wrap;
                self.apply_cfg();
                self.nav(|v, _| v.reveal = Reveal::Center);
                self.toast(0, if self.cfg.wrap { "Word wrap on" } else { "Word wrap off" });
            }
            Cmd::ToggleMdPreview => {
                self.cfg.md_preview = !self.cfg.md_preview;
                self.apply_cfg();
                self.nav(|v, _| v.reveal = Reveal::Center);
                self.toast(0, if self.cfg.md_preview { "Markdown preview on" } else { "Markdown preview off (raw text)" });
            }
            Cmd::ToggleLigatures => {
                self.cfg.ligatures = !self.cfg.ligatures;
                self.apply_cfg();
                self.toast(0, if self.cfg.ligatures { "Ligatures on" } else { "Ligatures off" });
            }
            Cmd::ToggleBgAnim => {
                self.cfg.bg_animate = !self.cfg.bg_animate;
                self.apply_cfg();
                self.toast(0, if self.cfg.bg_animate { "Background animation on" } else { "Background animation paused" });
            }
            Cmd::Fullscreen => self.effects.push(Effect::ToggleFullscreen),
            Cmd::NextTheme | Cmd::PrevTheme => {
                let n = self.themes.len();
                let i = if cmd == Cmd::NextTheme { (self.theme_idx + 1) % n } else { (self.theme_idx + n - 1) % n };
                self.set_theme(i, true);
                let name = self.themes[i].name.clone();
                self.toast(0, format!("Theme: {name}"));
            }
            Cmd::OpenConfigDir => {
                let dir = config_dir();
                let _ = std::fs::create_dir_all(dir.join("themes"));
                let sample = dir.join("themes").join("example-theme.toml.sample");
                if !sample.exists() {
                    let _ = std::fs::write(&sample, theme::EXAMPLE_THEME);
                }
                let _ = std::fs::create_dir_all(dir.join("syntax"));
                let sample = dir.join("syntax").join("example-syntax.toml.sample");
                if !sample.exists() {
                    let _ = std::fs::write(&sample, theme::EXAMPLE_SYNTAX);
                }
                if !Config::path().exists() && self.write_config {
                    let _ = self.cfg.save();
                }
                self.effects.push(Effect::Reveal(dir));
            }
            Cmd::Quit => self.effects.push(Effect::Quit),
        }
    }

    // ------------------------------------------------------------ keyboard

    pub fn input(&mut self, ev: Input) {
        if !matches!(ev, Input::MouseMove(..) | Input::MouseLeft) {
            self.last_input = self.now;
        }
        match ev {
            Input::Key { key, mods, text, repeat } => self.on_key(key, mods, text.as_deref(), repeat),
            Input::Text(s) => self.on_text(&s),
            Input::MouseMove(x, y) => self.on_mouse_move(x, y),
            Input::MouseDown { button, mods } => self.on_mouse_down(button, mods),
            Input::MouseUp { button } => self.on_mouse_up(button),
            Input::MouseLeft => {
                self.cx.mouse_inside = false;
                if self.cx.hot.is_some() {
                    self.cx.hot = None;
                    self.dirty = true;
                }
            }
            Input::Wheel { dx, dy, pixels, mods } => self.on_wheel(dx, dy, pixels, mods),
            Input::Focus(f) => {
                self.window_focused = f;
                self.cx.window_focused = f;
                if f {
                    self.check_disk();
                    let now = self.now;
                    if let Some((v, _)) = self.cur() {
                        v.blink_at = now;
                    }
                } else {
                    self.drag = None;
                }
                self.dirty = true;
            }
        }
    }

    fn on_text(&mut self, s: &str) {
        self.dirty = true;
        if self.overlay_active() {
            let ev = match &mut self.overlay.as_mut().unwrap().ov {
                Ov::Palette(p) => p.text(s),
                _ => OverlayEvent::None,
            };
            self.overlay_event(ev);
            return;
        }
        match self.kb {
            Kb::Find(f) => {
                let field = if f == 0 { &mut self.find.q } else { &mut self.find.r };
                field.insert(s);
                if f == 0 {
                    self.find_changed();
                }
            }
            Kb::Editor => self.edit(|v, d| d.buf.insert(&mut v.sel, s, EditKind::Type)),
        }
    }

    fn on_key(&mut self, key: Key, mods: Mods, text: Option<&str>, repeat: bool) {
        self.mods = mods;
        self.dirty = true;
        self.session_dirty = true;
        // tab indices may be about to change under a tab drag
        if matches!(self.drag, Some(Drag::Tab { .. })) {
            self.drag = None;
        }
        if self.overlay_active() {
            let theme_idx = self.theme_idx;
            let o = self.overlay.as_mut().unwrap();
            // a held key must never answer the "save changes?" prompt
            if repeat && matches!(o.ov, Ov::Confirm(_)) {
                return;
            }
            let ev = match &mut o.ov {
                Ov::Palette(p) => p.key(key, mods, text, &mut self.clip),
                Ov::Settings(s) => s.key(key, mods, &mut self.cfg, &self.themes[theme_idx]),
                Ov::Confirm(c) => c.key(key, mods),
            };
            self.overlay_event(ev);
            return;
        }
        if let Kb::Find(f) = self.kb {
            if self.find_key(f, key, mods, text) {
                return;
            }
        }
        if let Some(cmd) = keymap(key, mods) {
            // while a find field has the keyboard, editing shortcuts are not
            // for the document
            let edits = matches!(
                cmd,
                Cmd::Undo
                    | Cmd::Redo
                    | Cmd::Cut
                    | Cmd::Copy
                    | Cmd::Paste
                    | Cmd::SelectAll
                    | Cmd::DuplicateLine
                    | Cmd::MoveLineUp
                    | Cmd::MoveLineDown
                    | Cmd::DeleteLine
                    | Cmd::ToggleComment
                    | Cmd::Indent
                    | Cmd::Outdent
            );
            if !(edits && matches!(self.kb, Kb::Find(_))) {
                self.run(cmd);
            }
            return;
        }
        if mods.alt && !mods.ctrl {
            if let Key::Char(c @ '1'..='9') = key {
                let i = c as usize - '1' as usize;
                if let Some(p) = self.panes.get_mut(&self.focus) {
                    p.set_active(i.min(p.tabs.len().saturating_sub(1)));
                }
                return;
            }
        }
        if self.kb == Kb::Editor {
            self.editor_key(key, mods, text);
        }
    }

    fn editor_key(&mut self, key: Key, mods: Mods, text: Option<&str>) {
        let tw = self.cfg.tab_width;
        let now = self.now;
        let lh = self.cx.line_h;
        let ext = mods.shift;
        match key {
            Key::Left | Key::Right => self.nav(|v, d| v.move_h(&d.buf, key == Key::Right, mods.ctrl, ext)),
            Key::Up | Key::Down => {
                let dir: isize = if key == Key::Up { -1 } else { 1 };
                if mods.ctrl && !ext {
                    if let Some((v, _)) = self.cur() {
                        v.scroll_by(dir as f32 * lh, now);
                    }
                    self.dirty = true;
                } else {
                    self.nav(|v, d| v.move_v(&d.buf, dir, ext, tw));
                }
            }
            Key::PageUp | Key::PageDown => self.nav(|v, d| {
                let rows = v.page_rows as isize;
                v.move_v(&d.buf, if key == Key::PageUp { -rows } else { rows }, ext, tw);
            }),
            Key::Home => self.nav(|v, d| {
                if mods.ctrl {
                    v.set_head(0, ext);
                } else {
                    v.home(&d.buf, ext, tw);
                }
            }),
            Key::End => self.nav(|v, d| {
                if mods.ctrl {
                    v.set_head(d.buf.len(), ext);
                } else {
                    v.end(&d.buf, ext, tw);
                }
            }),
            Key::Enter => self.edit(|v, d| {
                let unit = d.indent_unit();
                d.buf.newline(&mut v.sel, &unit)
            }),
            Key::Tab => self.edit(|v, d| {
                if mods.shift {
                    let w = d.indent_width;
                    d.buf.outdent(&mut v.sel, w);
                } else {
                    let unit = d.indent_unit();
                    let w = d.indent_width;
                    d.buf.tab(&mut v.sel, &unit, w);
                }
            }),
            Key::Backspace => self.edit(|v, d| {
                let w = d.indent_width;
                d.buf.backspace(&mut v.sel, mods.ctrl, w)
            }),
            Key::Delete => self.edit(|v, d| d.buf.delete(&mut v.sel, mods.ctrl)),
            Key::Escape => {
                if self.find.open {
                    self.close_find();
                } else {
                    self.nav(|v, _| {
                        let h = v.sel.head;
                        v.set_head(h, false);
                    });
                }
            }
            _ => {
                if let Some(t) = text {
                    if !mods.ctrl && !mods.alt && !t.is_empty() && !t.chars().any(|c| c.is_control()) {
                        self.edit(|v, d| {
                            // a closing bracket typed on a blank line steps back one indent level
                            if matches!(t, "}" | ")" | "]") && v.sel.is_empty() {
                                let ls = d.buf.line_start(d.buf.line_of(v.sel.head));
                                let before = d.buf.slice(ls, v.sel.head);
                                if !before.is_empty() && before.chars().all(|c| c == ' ' || c == '\t') {
                                    let w = d.indent_width;
                                    d.buf.outdent(&mut v.sel, w);
                                }
                            }
                            d.buf.insert(&mut v.sel, t, EditKind::Type)
                        });
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------ mouse

    fn set_cursor(&mut self, c: CursorKind) {
        if c != self.last_cursor {
            self.last_cursor = c;
            self.effects.push(Effect::Cursor(c));
        }
    }

    fn on_mouse_move(&mut self, x: f32, y: f32) {
        self.cx.mouse = (x, y);
        self.cx.mouse_inside = true;
        let hit = self.cx.hit_at(x, y);
        let hot = hit.map(|h| h.id);
        if hot != self.cx.hot {
            self.cx.hot = hot;
            self.dirty = true;
        }
        let tw = self.cfg.tab_width;
        let md_on = self.cfg.md_preview;
        let mut cursor = hit.map(|h| h.cursor).unwrap_or_default();
        match &mut self.drag {
            None => {
                if self.overlay_active() {
                    let ev = match &mut self.overlay.as_mut().unwrap().ov {
                        Ov::Palette(p) => p.hover(hot),
                        Ov::Confirm(c) => {
                            c.hover(hot);
                            OverlayEvent::None
                        }
                        Ov::Settings(_) => OverlayEvent::None,
                    };
                    if ev != OverlayEvent::None {
                        self.overlay_event(ev);
                    }
                }
            }
            Some(Drag::Select { pane, mode, origin }) => {
                let (pane, mode, origin) = (*pane, *mode, *origin);
                cursor = CursorKind::Text;
                let now = self.now;
                if let Some(p) = self.panes.get_mut(&pane) {
                    if let Some(tab) = p.tabs.get_mut(p.active) {
                        if let Some(d) = self.docs.get(&tab.view.doc) {
                            let pos = editor::pos_at(&mut self.cx, &tab.view, d, x, y, tw, md_on && d.is_markdown());
                            let v = &mut tab.view;
                            match mode {
                                1 => v.sel.head = pos,
                                2 => {
                                    let (a, b) = d.buf.word_at(pos);
                                    if pos < origin.0 {
                                        v.sel = Sel::new(origin.1, a);
                                    } else {
                                        v.sel = Sel::new(origin.0, b.max(origin.1));
                                    }
                                }
                                _ => {
                                    let l = d.buf.line_of(pos);
                                    if pos < origin.0 {
                                        v.sel = Sel::new(origin.1, d.buf.line_start(l));
                                    } else {
                                        v.sel = Sel::new(origin.0, (d.buf.line_end(l) + 1).min(d.buf.len()).max(origin.1));
                                    }
                                }
                            }
                            v.goal = None;
                            v.touch(now);
                        }
                    }
                }
                self.dirty = true;
            }
            Some(Drag::Scrollbar { pane, grab }) => {
                let (pane, grab) = (*pane, *grab);
                cursor = CursorKind::Default;
                let now = self.now;
                if let Some(v) = self.panes.get_mut(&pane).and_then(|p| p.active_view()) {
                    let t = editor::scroll_for_thumb(&self.cx, v, y, grab);
                    v.scroll_y.snap(t);
                    v.scrolled_at = now;
                }
                self.dirty = true;
            }
            Some(Drag::Divider { id }) => {
                let id = *id;
                let min = self.cx.px(140.0);
                if let Some(s) = self.tree.find_split(id) {
                    let (pos, start, size) = if s.horizontal { (x, s.rect.x, s.rect.w) } else { (y, s.rect.y, s.rect.h) };
                    cursor = if s.horizontal { CursorKind::ResizeH } else { CursorKind::ResizeV };
                    if size > 2.0 * min {
                        let r = ((pos - start) / size).clamp(min / size, 1.0 - min / size);
                        s.ratio.snap(r);
                    }
                }
                self.session_dirty = true;
                self.dirty = true;
            }
            Some(Drag::Tab { pane, idx, grab, x0, moved }) => {
                cursor = CursorKind::Default;
                let left_strip = !self.panes.get(pane).is_some_and(|p| p.strip.contains(x, y));
                if !*moved && ((x - *x0).abs() > 6.0 * self.cx.s || left_strip) {
                    *moved = true;
                }
                if *moved {
                    if let Some(p) = self.panes.get_mut(pane) {
                        // only reorder while the tab is still docked in its strip
                        if p.strip.inset_xy(0.0, -self.cx.px(28.0)).contains(x, y) {
                            let w = p.slots.get(*idx).map(|s| s.w).unwrap_or(0.0);
                            *idx = p.reorder_for_drag(*idx, x - *grab + w * 0.5);
                        }
                    }
                    self.session_dirty = true;
                    self.dirty = true;
                }
            }
            Some(Drag::Slider) => {
                let theme_idx = self.theme_idx;
                let ev = match self.overlay.as_mut().map(|o| &mut o.ov) {
                    Some(Ov::Settings(s)) => s.drag(&self.cx, &mut self.cfg, &self.themes[theme_idx]),
                    _ => OverlayEvent::None,
                };
                if ev != OverlayEvent::None {
                    self.overlay_event(ev);
                }
            }
        }
        self.set_cursor(cursor);
    }

    fn on_mouse_down(&mut self, button: u8, mods: Mods) {
        self.mods = mods;
        self.dirty = true;
        self.session_dirty = true;
        let (x, y) = self.cx.mouse;
        let hit = self.cx.hit_at(x, y).map(|h| h.id);
        let now = self.now;
        let clicks = if button == 0 {
            let (t, lx, ly, n) = self.last_click;
            let n = if now - t < 0.4 && (x - lx).abs() < 5.0 && (y - ly).abs() < 5.0 { n % 3 + 1 } else { 1 };
            self.last_click = (now, x, y, n);
            n
        } else {
            1
        };

        if self.overlay_active() {
            if button != 0 {
                return;
            }
            let theme_idx = self.theme_idx;
            let o = self.overlay.as_mut().unwrap();
            let ev = match &mut o.ov {
                Ov::Palette(p) => p.click(hit),
                Ov::Settings(s) => {
                    self.drag = Some(Drag::Slider);
                    s.press(&self.cx, hit, &mut self.cfg, &self.themes[theme_idx])
                }
                Ov::Confirm(c) => c.click(hit),
            };
            self.overlay_event(ev);
            return;
        }

        let Some(hit) = hit else { return };
        if button == 1 {
            if let HitId::Tab(p, i) | HitId::TabClose(p, i) = hit {
                self.close_tab(p, i, false);
            }
            return;
        }
        if button != 0 {
            return;
        }
        let tw = self.cfg.tab_width;
        match hit {
            HitId::Tab(p, i) => {
                self.focus_pane(p);
                self.kb = Kb::Editor;
                if let Some(pane) = self.panes.get_mut(&p) {
                    pane.set_active(i);
                    let sx = pane.slots.get(i).map(|s| s.x).unwrap_or(x);
                    self.drag = Some(Drag::Tab { pane: p, idx: i, grab: x - sx, x0: x, moved: false });
                }
                self.find.invalidate();
            }
            HitId::TabClose(p, i) => self.close_tab(p, i, false),
            HitId::NewTab(p) => {
                self.focus_pane(p);
                self.new_tab();
            }
            HitId::Strip(p) => {
                self.focus_pane(p);
                if clicks == 2 {
                    self.new_tab();
                }
            }
            HitId::Editor(p) => {
                self.focus_pane(p);
                self.kb = Kb::Editor;
                // a click on a rendered task checkbox ticks it instead of moving the caret
                let check = self.panes.get(&p).and_then(|pn| pn.tabs.get(pn.active)).and_then(|t| t.view.checks.iter().find(|c| c.0.contains(x, y)).copied());
                if let Some((_, line, src)) = check {
                    self.edit(|v, d| {
                        let pos = d.buf.line_byte_to_pos(line, src);
                        let now = d.buf.slice(pos, pos + 1);
                        if now == " " || now.eq_ignore_ascii_case("x") {
                            d.buf.begin(v.sel);
                            d.buf.replace(pos, pos + 1, if now == " " { "x" } else { " " });
                            d.buf.commit(v.sel, EditKind::Other);
                        }
                    });
                    // ticking a box must not scroll the view to the caret
                    if let Some((v, _)) = self.cur() {
                        v.reveal = Reveal::None;
                    }
                    return;
                }
                let md_on = self.cfg.md_preview;
                let Some(pane) = self.panes.get_mut(&p) else { return };
                let Some(tab) = pane.tabs.get_mut(pane.active) else { return };
                let Some(d) = self.docs.get(&tab.view.doc) else { return };
                let pos = editor::pos_at(&mut self.cx, &tab.view, d, x, y, tw, md_on && d.is_markdown());
                let v = &mut tab.view;
                let origin = match clicks {
                    2 => {
                        let (a, b) = d.buf.word_at(pos);
                        v.sel = Sel::new(a, b);
                        (a, b)
                    }
                    3 => {
                        let l = d.buf.line_of(pos);
                        let (a, b) = (d.buf.line_start(l), (d.buf.line_end(l) + 1).min(d.buf.len()));
                        v.sel = Sel::new(a, b);
                        (a, b)
                    }
                    _ => {
                        v.set_head(pos, mods.shift);
                        (v.sel.anchor, v.sel.anchor)
                    }
                };
                v.goal = None;
                v.blink_at = now;
                self.drag = Some(Drag::Select { pane: p, mode: clicks as u8, origin });
            }
            HitId::Scrollbar(p) => {
                let Some(v) = self.panes.get_mut(&p).and_then(|p| p.active_view()) else { return };
                let grab = match editor::thumb_grab(&self.cx, v, y) {
                    Some(g) => g,
                    None => {
                        // jump so the thumb centres on the click
                        let th = (v.rect.h * v.rect.h / (v.max_scroll + v.rect.h)).max(self.cx.px(36.0)).min(v.rect.h);
                        let t = editor::scroll_for_thumb(&self.cx, v, y, th * 0.5);
                        v.scroll_y.target = t;
                        th * 0.5
                    }
                };
                v.scrolled_at = now;
                self.drag = Some(Drag::Scrollbar { pane: p, grab });
            }
            HitId::Divider(id) => self.drag = Some(Drag::Divider { id }),
            HitId::Status(i) => match i {
                0 => self.open_palette(PalMode::Goto),
                1 => self.run(Cmd::ToggleWrap),
                3 => self.open_palette(PalMode::Langs),
                4 => self.open_palette(PalMode::Themes),
                _ => {}
            },
            HitId::FindField(_) | HitId::FindBtn(_) if !self.find.open => {}
            HitId::FindField(f) => {
                self.kb = Kb::Find(f);
                let (field, r) = if f == 0 { (&mut self.find.q, self.find.q_rect) } else { (&mut self.find.r, self.find.r_rect) };
                field.click(&mut self.cx, r, x, mods.shift);
            }
            HitId::FindBtn(b) => self.find_button(b),
            HitId::Welcome(i) => {
                let cmds = [Cmd::NewTab, Cmd::OpenFile, Cmd::Palette, Cmd::Themes];
                if let Some(c) = cmds.get(i as usize) {
                    self.run(*c);
                }
            }
            _ => {}
        }
    }

    fn on_mouse_up(&mut self, button: u8) {
        if button != 0 {
            return;
        }
        self.dirty = true;
        match self.drag.take() {
            Some(Drag::Tab { pane, idx, moved: true, .. }) => {
                // dropped on another pane?
                let (x, y) = self.cx.mouse;
                let target = self.panes.values().find(|p| p.id != pane && p.rect.contains(x, y)).map(|p| {
                    let at = p.slots.iter().position(|s| x < s.cx()).unwrap_or(p.tabs.len());
                    (p.id, at)
                });
                if let Some((to, at)) = target {
                    self.move_tab(pane, idx, to, Some(at));
                }
            }
            Some(Drag::Slider) => {
                if let Some(Overlay { ov: Ov::Settings(s), .. }) = &mut self.overlay {
                    s.release();
                }
            }
            _ => {}
        }
    }

    fn on_wheel(&mut self, dx: f32, dy: f32, pixels: bool, mods: Mods) {
        self.dirty = true;
        if mods.ctrl {
            if dy != 0.0 {
                self.run(if dy > 0.0 { Cmd::ZoomIn } else { Cmd::ZoomOut });
            }
            return;
        }
        let lh = self.cx.line_h;
        if self.overlay_active() {
            let rows = if pixels { -dy / lh } else { -dy * 2.0 };
            match &mut self.overlay.as_mut().unwrap().ov {
                Ov::Palette(p) => p.wheel(rows),
                Ov::Settings(s) => s.wheel(rows),
                Ov::Confirm(_) => {}
            }
            return;
        }
        let (x, y) = self.cx.mouse;
        let now = self.now;
        let Some(hit) = self.cx.hit_at(x, y).map(|h| h.id) else { return };
        match hit {
            HitId::Strip(p) | HitId::Tab(p, _) | HitId::TabClose(p, _) | HitId::NewTab(p) => {
                let d = if pixels { -(dy + dx) } else { -(dy + dx) * self.cx.px(70.0) };
                if let Some(p) = self.panes.get_mut(&p) {
                    p.scroll_strip(d);
                }
            }
            HitId::Editor(p) | HitId::Scrollbar(p) => {
                let wrap = self.cfg.wrap;
                let cell = self.cx.cell;
                if let Some(v) = self.panes.get_mut(&p).and_then(|p| p.active_view()) {
                    let (mut sx, mut sy) = if pixels { (-dx, -dy) } else { (-dx * cell * 6.0, -dy * lh * 3.0) };
                    if mods.shift {
                        (sx, sy) = (sy, 0.0);
                    }
                    v.scroll_by(sy, now);
                    if !wrap && sx != 0.0 {
                        v.scroll_x.target = (v.scroll_x.target + sx).max(0.0);
                    }
                    if pixels {
                        // touchpads already deliver smooth deltas
                        v.scroll_y.v = v.scroll_y.target;
                        v.scroll_x.v = v.scroll_x.target;
                    }
                }
            }
            _ => {}
        }
    }
}
