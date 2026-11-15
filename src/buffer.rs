//! Rope-backed text buffer with undo/redo and the editing operations the
//! editor needs.
//!
//! All offsets are char offsets into the rope unless stated otherwise, and the
//! text is LF-only (anything inserted is normalised). The buffer has no cursor
//! of its own: operations take the acting view's selection and update it, and
//! every primitive change is reported through `notes` so other views, the
//! highlighter and wrap maps can follow along.

#![allow(dead_code)]

use ropey::Rope;
use std::borrow::Cow;
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use unicode_segmentation::UnicodeSegmentation;

const UNDO_CAP: usize = 5000;
const MERGE_WINDOW: Duration = Duration::from_millis(1000);
/// `saved_id` value that no undo step can ever have.
const NEVER_SAVED: u64 = u64::MAX;
/// Chars of context looked at when finding a grapheme boundary.
const GRAPHEME_WINDOW: usize = 32;
const GRAPHEME_WINDOW_MAX: usize = 4096;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Sel {
    pub anchor: usize,
    pub head: usize,
}

impl Sel {
    pub fn new(anchor: usize, head: usize) -> Sel {
        Sel { anchor, head }
    }
    pub fn caret(p: usize) -> Sel {
        Sel { anchor: p, head: p }
    }
    pub fn start(&self) -> usize {
        self.anchor.min(self.head)
    }
    pub fn end(&self) -> usize {
        self.anchor.max(self.head)
    }
    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }
}

/// One primitive text change, in the order applied.
///
/// Line counts are measured the way the rope counts lines, so
/// `lines - lines_removed + lines_inserted` always equals the rope's new
/// line count. For LF-only text that is the number of `'\n'` removed/inserted.
#[derive(Clone, Copy, Debug)]
pub struct EditNote {
    /// Char offset where the change starts.
    pub pos: usize,
    /// Chars removed.
    pub removed: usize,
    /// Chars inserted.
    pub inserted: usize,
    /// Line index containing `pos`.
    pub line: usize,
    /// Line breaks removed.
    pub lines_removed: usize,
    /// Line breaks inserted.
    pub lines_inserted: usize,
}

impl EditNote {
    /// Map a char offset from before this edit to after it.
    pub fn map_pos(&self, p: usize) -> usize {
        if p <= self.pos {
            p
        } else if p >= self.pos + self.removed {
            p - self.removed + self.inserted
        } else {
            self.pos + self.inserted
        }
    }
    pub fn map_sel(&self, s: Sel) -> Sel {
        Sel { anchor: self.map_pos(s.anchor), head: self.map_pos(s.head) }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EditKind {
    Type,
    Backspace,
    Delete,
    Other,
}

#[derive(Debug)]
struct Change {
    pos: usize,
    removed: String,
    inserted: String,
}

/// One undo step.
#[derive(Debug)]
struct Txn {
    id: u64,
    changes: Vec<Change>,
    before: Sel,
    after: Sel,
    /// Secondary cursors before / after (multi-cursor edits only).
    xb: Vec<Sel>,
    xa: Vec<Sel>,
    kind: EditKind,
    at: Instant,
}

struct Open {
    before: Sel,
    xb: Vec<Sel>,
    changes: Vec<Change>,
}

pub struct Buffer {
    pub rope: Rope,
    /// Pending change notifications; the owner drains them with
    /// `take_notes()` after each operation (including undo/redo).
    pub notes: Vec<EditNote>,
    /// Bumped on every text change (also undo/redo).
    pub rev: u64,
    undo: VecDeque<Txn>,
    redo: Vec<Txn>,
    open: Option<Open>,
    next_id: u64,
    /// State id when the undo stack is empty (id of the last step that fell
    /// off the bottom of the capped history, or 0).
    base_id: u64,
    saved_id: u64,
    /// The next commit must start a new undo step.
    no_merge: bool,
    /// Inside `group_begin` .. `group_end`: nested begin/commit calls fold
    /// into the one open transaction.
    grouped: bool,
    /// Secondary cursors to restore after the last undo / redo.
    restored: Vec<Sel>,
}

fn is_break(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{0B}' | '\u{0C}' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

/// 0 whitespace, 1 punctuation, 2 word, 3 line break.
fn class(c: char) -> u8 {
    if is_break(c) {
        3
    } else if c.is_whitespace() {
        0
    } else if c.is_alphanumeric() || c == '_' {
        2
    } else {
        1
    }
}

fn is_regional(c: char) -> bool {
    ('\u{1F1E6}'..='\u{1F1FF}').contains(&c)
}

fn normalize(text: &str) -> Cow<'_, str> {
    if text.contains('\r') {
        Cow::Owned(text.replace("\r\n", "\n").replace('\r', "\n"))
    } else {
        Cow::Borrowed(text)
    }
}

/// Position after inserting `n` chars at `at`. `push_at` decides whether a
/// position sitting exactly on the insertion point moves with the text.
fn map_insert(p: usize, at: usize, n: usize, push_at: bool) -> usize {
    if p > at || (p == at && push_at) { p + n } else { p }
}

/// Position after removing `n` chars at `at`.
fn map_remove(p: usize, at: usize, n: usize) -> usize {
    if p <= at {
        p
    } else if p >= at + n {
        p - n
    } else {
        at
    }
}

fn leading_ws(text: &str) -> usize {
    text.chars().take_while(|c| c.is_whitespace()).count()
}

impl Buffer {
    pub fn new(text: &str) -> Buffer {
        Buffer {
            rope: Rope::from_str(text),
            notes: Vec::new(),
            rev: 0,
            undo: VecDeque::new(),
            redo: Vec::new(),
            open: None,
            next_id: 1,
            base_id: 0,
            saved_id: 0,
            no_merge: false,
            grouped: false,
            restored: Vec::new(),
        }
    }

    pub fn text(&self) -> String {
        self.rope.to_string()
    }

    /// Length in chars.
    pub fn len(&self) -> usize {
        self.rope.len_chars()
    }

    pub fn is_empty(&self) -> bool {
        self.rope.len_chars() == 0
    }

    pub fn lines(&self) -> usize {
        self.rope.len_lines()
    }

    fn state_id(&self) -> u64 {
        self.undo.back().map(|t| t.id).unwrap_or(self.base_id)
    }

    /// True when the text differs from the last `mark_saved` state. Undoing
    /// back to the saved state makes the buffer clean again.
    pub fn is_dirty(&self) -> bool {
        self.state_id() != self.saved_id
    }

    pub fn mark_saved(&mut self) {
        self.flush_open();
        self.saved_id = self.state_id();
        self.no_merge = true;
    }

    /// Force dirty until the next `mark_saved` (restoring an unsaved draft).
    pub fn mark_dirty(&mut self) {
        self.saved_id = NEVER_SAVED;
    }

    pub fn take_notes(&mut self) -> Vec<EditNote> {
        std::mem::take(&mut self.notes)
    }

    fn char_at(&self, i: usize) -> Option<char> {
        if i < self.rope.len_chars() { Some(self.rope.char(i)) } else { None }
    }

    fn clamp_sel(&self, s: Sel) -> Sel {
        let n = self.len();
        Sel { anchor: s.anchor.min(n), head: s.head.min(n) }
    }

    // ------------------------------------------------------------ changes

    /// The one place the rope is mutated. Returns the removed text.
    fn apply_raw(&mut self, pos: usize, del: usize, ins: &str) -> String {
        let len = self.rope.len_chars();
        let pos = pos.min(len);
        let end = pos.saturating_add(del).min(len);
        let line = self.rope.char_to_line(pos);
        let l0 = self.rope.len_lines();
        let removed = if end > pos {
            let s = self.rope.slice(pos..end).to_string();
            self.rope.remove(pos..end);
            s
        } else {
            String::new()
        };
        let l1 = self.rope.len_lines();
        if !ins.is_empty() {
            self.rope.insert(pos, ins);
        }
        let l2 = self.rope.len_lines();
        self.rev += 1;
        self.notes.push(EditNote {
            pos,
            removed: end - pos,
            inserted: ins.chars().count(),
            line,
            lines_removed: l0.saturating_sub(l1),
            lines_inserted: l2.saturating_sub(l1),
        });
        removed
    }

    /// Replace `[start, end)` inside the open transaction (opening one if
    /// needed). Returns the number of chars inserted.
    fn replace_inner(&mut self, start: usize, end: usize, text: &str) -> usize {
        let len = self.len();
        let (mut s, mut e) = (start.min(len), end.min(len));
        if s > e {
            std::mem::swap(&mut s, &mut e);
        }
        let text = normalize(text);
        if s == e && text.is_empty() {
            return 0;
        }
        if self.open.is_none() {
            self.open = Some(Open { before: Sel::caret(s), xb: Vec::new(), changes: Vec::new() });
        }
        let removed = self.apply_raw(s, e - s, &text);
        let n = text.chars().count();
        if let Some(open) = self.open.as_mut() {
            open.changes.push(Change { pos: s, removed, inserted: text.into_owned() });
        }
        n
    }

    /// Commit a transaction that was left open by mistake.
    fn flush_open(&mut self) {
        if let Some(open) = &self.open {
            let before = open.before;
            self.commit(before, EditKind::Other);
        }
    }

    /// Start a transaction: everything until `commit` is one undo step.
    pub fn begin(&mut self, before: Sel) {
        if self.grouped {
            return;
        }
        self.flush_open();
        self.open = Some(Open { before, xb: Vec::new(), changes: Vec::new() });
    }

    /// Start a multi-cursor step: every editing call until `group_end`
    /// becomes part of one undo step. `extra` are the secondary cursors.
    pub fn group_begin(&mut self, before: Sel, extra: Vec<Sel>) {
        self.grouped = false;
        self.flush_open();
        self.open = Some(Open { before, xb: extra, changes: Vec::new() });
        self.grouped = true;
    }

    pub fn group_end(&mut self, after: Sel, extra: Vec<Sel>, kind: EditKind) {
        self.grouped = false;
        self.commit_with(after, extra, kind);
    }

    /// Secondary cursors that the last undo / redo wants back.
    pub fn take_restored(&mut self) -> Vec<Sel> {
        std::mem::take(&mut self.restored)
    }

    /// Replace `[start, end)` with `text`. Applies immediately and records
    /// the change in the open transaction.
    pub fn replace(&mut self, start: usize, end: usize, text: &str) {
        self.replace_inner(start, end, text);
    }

    /// Finish the open transaction. A transaction without changes is dropped.
    pub fn commit(&mut self, after: Sel, kind: EditKind) {
        if self.grouped {
            return;
        }
        self.commit_with(after, Vec::new(), kind);
    }

    fn commit_with(&mut self, after: Sel, xa: Vec<Sel>, kind: EditKind) {
        let Some(open) = self.open.take() else { return };
        if open.changes.is_empty() {
            return;
        }
        let now = Instant::now();
        let no_merge = std::mem::replace(&mut self.no_merge, false);
        self.redo.clear();
        let saved_id = self.saved_id;

        // typing with several cursors: letters join the previous step the
        // same way single-cursor typing does
        if !no_merge && kind == EditKind::Type && open.changes.len() > 1 {
            if let Some(last) = self.undo.back_mut() {
                let recent = now.saturating_duration_since(last.at) < MERGE_WINDOW;
                let plain = |c: &Change| c.removed.is_empty() && !c.inserted.is_empty() && !c.inserted.contains('\n');
                let word = |c: &Change| !c.inserted.chars().next().is_some_and(|ch| ch.is_whitespace());
                if last.kind == EditKind::Type
                    && last.id != saved_id
                    && recent
                    && last.xa.len() == xa.len()
                    && last.xa.len() + 1 == open.changes.len()
                    && last.changes.len() >= open.changes.len()
                    && open.changes.iter().all(|c| plain(c) && word(c))
                    && last.changes.iter().all(plain)
                {
                    last.changes.extend(open.changes);
                    last.after = after;
                    last.xa = xa;
                    last.at = now;
                    return;
                }
            }
        }

        if !no_merge && open.changes.len() == 1 && kind != EditKind::Other {
            if let Some(last) = self.undo.back_mut() {
                let recent = now.saturating_duration_since(last.at) < MERGE_WINDOW;
                if last.kind == kind && last.changes.len() == 1 && last.id != saved_id && recent {
                    let new = &open.changes[0];
                    let lc = &mut last.changes[0];
                    let merged = match kind {
                        EditKind::Type => {
                            let boundary = new.inserted.chars().next().is_some_and(|c| c.is_whitespace())
                                && lc.inserted.chars().next_back().is_some_and(|c| !c.is_whitespace());
                            if new.removed.is_empty()
                                && !new.inserted.contains('\n')
                                && new.pos == lc.pos + lc.inserted.chars().count()
                                && !boundary
                            {
                                lc.inserted.push_str(&new.inserted);
                                true
                            } else {
                                false
                            }
                        }
                        EditKind::Backspace => {
                            if new.inserted.is_empty()
                                && lc.inserted.is_empty()
                                && !new.removed.contains('\n')
                                && new.pos + new.removed.chars().count() == lc.pos
                            {
                                lc.removed.insert_str(0, &new.removed);
                                lc.pos = new.pos;
                                true
                            } else {
                                false
                            }
                        }
                        EditKind::Delete => {
                            if new.inserted.is_empty()
                                && lc.inserted.is_empty()
                                && !new.removed.contains('\n')
                                && new.pos == lc.pos
                            {
                                lc.removed.push_str(&new.removed);
                                true
                            } else {
                                false
                            }
                        }
                        EditKind::Other => false,
                    };
                    if merged {
                        last.after = after;
                        last.at = now;
                        return;
                    }
                }
            }
        }

        let id = self.next_id;
        self.next_id += 1;
        self.undo.push_back(Txn { id, changes: open.changes, before: open.before, after, xb: open.xb, xa, kind, at: now });
        while self.undo.len() > UNDO_CAP {
            if let Some(t) = self.undo.pop_front() {
                self.base_id = t.id;
            }
        }
    }

    /// Undo one step; returns the selection to restore.
    pub fn undo(&mut self) -> Option<Sel> {
        self.flush_open();
        let t = self.undo.pop_back()?;
        for c in t.changes.iter().rev() {
            self.apply_raw(c.pos, c.inserted.chars().count(), &c.removed);
        }
        self.no_merge = true;
        let sel = self.clamp_sel(t.before);
        self.restored = t.xb.iter().map(|s| self.clamp_sel(*s)).collect();
        self.redo.push(t);
        Some(sel)
    }

    /// Redo one step; returns the selection to restore.
    pub fn redo(&mut self) -> Option<Sel> {
        self.flush_open();
        let t = self.redo.pop()?;
        for c in &t.changes {
            self.apply_raw(c.pos, c.removed.chars().count(), &c.inserted);
        }
        self.no_merge = true;
        let sel = self.clamp_sel(t.after);
        self.restored = t.xa.iter().map(|s| self.clamp_sel(*s)).collect();
        self.undo.push_back(t);
        Some(sel)
    }

    /// Replace the whole content as one undoable step (reload from disk).
    /// Only the differing middle is replaced. Returns the new selection.
    pub fn set_text(&mut self, sel: Sel, text: &str) -> Sel {
        self.flush_open();
        let new = normalize(text);
        let old = self.rope.to_string();
        let (ob, nb) = (old.as_bytes(), new.as_bytes());

        let mut p = 0;
        while p < ob.len() && p < nb.len() && ob[p] == nb[p] {
            p += 1;
        }
        while !old.is_char_boundary(p) {
            p -= 1;
        }
        let max_s = (ob.len() - p).min(nb.len() - p);
        let mut s = 0;
        while s < max_s && ob[ob.len() - 1 - s] == nb[nb.len() - 1 - s] {
            s += 1;
        }
        while !old.is_char_boundary(ob.len() - s) {
            s -= 1;
        }

        let old_mid_end = ob.len() - s;
        let new_mid = &new[p..nb.len() - s];
        if p == old_mid_end && new_mid.is_empty() {
            return self.clamp_sel(sel);
        }
        let start = self.rope.byte_to_char(p);
        let end = self.rope.byte_to_char(old_mid_end);
        self.begin(sel);
        let inserted = self.replace_inner(start, end, new_mid);
        // keep a caret inside the replaced range at the same offset if it fits
        let map = |q: usize| {
            if q <= start {
                q
            } else if q >= end {
                q - (end - start) + inserted
            } else {
                start + (q - start).min(inserted)
            }
        };
        let out = self.clamp_sel(Sel { anchor: map(sel.anchor), head: map(sel.head) });
        self.commit(out, EditKind::Other);
        out
    }

    // ---------------------------------------------------------- positions

    pub fn line_of(&self, pos: usize) -> usize {
        self.rope.char_to_line(pos.min(self.len()))
    }

    pub fn line_start(&self, line: usize) -> usize {
        self.rope.line_to_char(line.min(self.lines().saturating_sub(1)))
    }

    /// End of the line, excluding its line break.
    pub fn line_end(&self, line: usize) -> usize {
        let last = self.lines().saturating_sub(1);
        let line = line.min(last);
        let start = self.rope.line_to_char(line);
        let l = self.rope.line(line);
        let mut n = l.len_chars();
        if line < last && n > 0 {
            // every line but the last ends with a break
            if l.char(n - 1) == '\n' {
                n -= 1;
                if n > 0 && l.char(n - 1) == '\r' {
                    n -= 1;
                }
            } else {
                n -= 1;
            }
        }
        start + n
    }

    /// Text of the line without its line break.
    pub fn line_text(&self, line: usize) -> String {
        self.rope.slice(self.line_start(line)..self.line_end(line)).to_string()
    }

    /// (line, byte offset within the line text).
    pub fn pos_to_line_byte(&self, pos: usize) -> (usize, usize) {
        let pos = pos.min(self.len());
        let line = self.rope.char_to_line(pos);
        let start = self.rope.line_to_char(line);
        let end = self.line_end(line);
        let sb = self.rope.char_to_byte(start);
        let byte = self.rope.char_to_byte(pos.min(end)) - sb;
        (line, byte)
    }

    /// Clamped to the line and snapped down to a char boundary.
    pub fn line_byte_to_pos(&self, line: usize, byte: usize) -> usize {
        let start = self.line_start(line);
        let end = self.line_end(line);
        let slice = self.rope.slice(start..end);
        let byte = byte.min(slice.len_bytes());
        start + slice.byte_to_char(byte)
    }

    pub fn slice(&self, start: usize, end: usize) -> String {
        let len = self.len();
        let (mut s, mut e) = (start.min(len), end.min(len));
        if s > e {
            std::mem::swap(&mut s, &mut e);
        }
        self.rope.slice(s..e).to_string()
    }

    /// Char pos of the first non-whitespace char in the line (line end if blank).
    pub fn first_non_ws(&self, line: usize) -> usize {
        let start = self.line_start(line);
        let end = self.line_end(line);
        let n = self.rope.slice(start..end).chars().take_while(|c| c.is_whitespace()).count();
        start + n
    }

    // ------------------------------------------------------------- motion

    pub fn prev_grapheme(&self, pos: usize) -> usize {
        let pos = pos.min(self.len());
        if pos == 0 {
            return 0;
        }
        let mut win = GRAPHEME_WINDOW;
        loop {
            let mut start = pos.saturating_sub(win);
            // keep a run of regional indicators whole so flags pair up correctly
            let mut guard = 0;
            while start > 0
                && guard < GRAPHEME_WINDOW_MAX
                && is_regional(self.rope.char(start))
                && is_regional(self.rope.char(start - 1))
            {
                start -= 1;
                guard += 1;
            }
            let s = self.rope.slice(start..pos).to_string();
            let n = s.graphemes(true).next_back().map(|g| g.chars().count()).unwrap_or(1).max(1);
            if n >= pos - start && start > 0 && win < GRAPHEME_WINDOW_MAX {
                // the cluster may extend past the window; look further back
                win *= 4;
                continue;
            }
            return pos - n.min(pos);
        }
    }

    pub fn next_grapheme(&self, pos: usize) -> usize {
        let len = self.len();
        if pos >= len {
            return len;
        }
        let mut win = GRAPHEME_WINDOW;
        loop {
            let end = pos.saturating_add(win).min(len);
            let s = self.rope.slice(pos..end).to_string();
            let n = s.graphemes(true).next().map(|g| g.chars().count()).unwrap_or(1).max(1);
            if n >= end - pos && end < len && win < GRAPHEME_WINDOW_MAX {
                win *= 4;
                continue;
            }
            return (pos + n).min(len);
        }
    }

    /// Ctrl+Left: start of the previous word; stops at line boundaries (from
    /// column 0 it goes to the end of the previous line).
    pub fn word_left(&self, pos: usize) -> usize {
        let mut p = pos.min(self.len());
        if p == 0 {
            return 0;
        }
        if class(self.rope.char(p - 1)) == 3 {
            return p - 1;
        }
        while p > 0 && class(self.rope.char(p - 1)) == 0 {
            p -= 1;
        }
        if p == 0 {
            return 0;
        }
        let cls = class(self.rope.char(p - 1));
        if cls == 3 {
            return p;
        }
        while p > 0 && class(self.rope.char(p - 1)) == cls {
            p -= 1;
        }
        p
    }

    /// Ctrl+Right: skip whitespace, then the word; from the end of a line it
    /// goes to the start of the next one.
    pub fn word_right(&self, pos: usize) -> usize {
        let len = self.len();
        let mut p = pos.min(len);
        if p >= len {
            return len;
        }
        if class(self.rope.char(p)) == 3 {
            return p + 1;
        }
        while p < len && class(self.rope.char(p)) == 0 {
            p += 1;
        }
        if p >= len {
            return len;
        }
        let cls = class(self.rope.char(p));
        if cls == 3 {
            return p;
        }
        while p < len && class(self.rope.char(p)) == cls {
            p += 1;
        }
        p
    }

    /// The word (or run of same-class chars) under `pos`, for double-click.
    /// At a boundary a word wins over the whitespace/punctuation next to it.
    pub fn word_at(&self, pos: usize) -> (usize, usize) {
        let len = self.len();
        let pos = pos.min(len);
        let here = self.char_at(pos).map(class).unwrap_or(3);
        let prev = if pos > 0 { class(self.rope.char(pos - 1)) } else { 3 };
        let cls = if here == 2 || prev == 2 {
            2
        } else if here != 3 {
            here
        } else if prev != 3 {
            prev
        } else {
            return (pos, pos);
        };
        let (mut s, mut e) = (pos, pos);
        while s > 0 && class(self.rope.char(s - 1)) == cls {
            s -= 1;
        }
        while e < len && class(self.rope.char(e)) == cls {
            e += 1;
        }
        (s, e)
    }

    /// Inclusive line range a selection touches. A non-empty selection that
    /// ends at column 0 does not include that last line.
    pub fn selected_lines(&self, sel: Sel) -> (usize, usize) {
        let l0 = self.line_of(sel.start());
        let mut l1 = self.line_of(sel.end());
        if !sel.is_empty() && l1 > l0 && sel.end().min(self.len()) == self.line_start(l1) {
            l1 -= 1;
        }
        (l0, l1)
    }

    // ------------------------------------------------------------ editing

    /// Replace the selection with `text`; the caret ends after it.
    pub fn insert(&mut self, sel: &mut Sel, text: &str, kind: EditKind) {
        let len = self.len();
        let (s, e) = (sel.start().min(len), sel.end().min(len));
        if s == e && text.is_empty() {
            return;
        }
        self.begin(*sel);
        let n = self.replace_inner(s, e, text);
        *sel = Sel::caret(s + n);
        self.commit(*sel, kind);
    }

    fn delete_range(&mut self, sel: &mut Sel, start: usize, end: usize, kind: EditKind) {
        if start >= end {
            return;
        }
        self.begin(*sel);
        self.replace_inner(start, end, "");
        *sel = Sel::caret(start);
        self.commit(*sel, kind);
    }

    /// Backspace. With a selection: delete it. `word`: back to the previous
    /// word start. Inside leading spaces: back to the previous indent stop.
    /// Otherwise one grapheme.
    pub fn backspace(&mut self, sel: &mut Sel, word: bool, indent_width: usize) {
        *sel = self.clamp_sel(*sel);
        if !sel.is_empty() {
            let (a, b) = (sel.start(), sel.end());
            self.delete_range(sel, a, b, EditKind::Other);
            return;
        }
        let pos = sel.head;
        if pos == 0 {
            return;
        }
        let target = if word {
            self.word_left(pos)
        } else {
            let ls = self.line_start(self.line_of(pos));
            let col = pos - ls;
            let in_indent =
                col > 0 && indent_width > 0 && self.rope.slice(ls..pos).chars().all(|c| c == ' ');
            if in_indent { ls + ((col - 1) / indent_width) * indent_width } else { self.prev_grapheme(pos) }
        };
        self.delete_range(sel, target, pos, if word { EditKind::Other } else { EditKind::Backspace });
    }

    /// Forward delete: the selection, the next word, or one grapheme.
    pub fn delete(&mut self, sel: &mut Sel, word: bool) {
        *sel = self.clamp_sel(*sel);
        if !sel.is_empty() {
            let (a, b) = (sel.start(), sel.end());
            self.delete_range(sel, a, b, EditKind::Other);
            return;
        }
        let pos = sel.head;
        if pos >= self.len() {
            return;
        }
        let target = if word { self.word_right(pos) } else { self.next_grapheme(pos) };
        self.delete_range(sel, pos, target, if word { EditKind::Other } else { EditKind::Delete });
    }

    /// Insert a line break with auto-indent: the current line's leading
    /// whitespace is copied, one extra `indent_unit` is added after an
    /// opening `( [ {` or `:`, and a matching closer right after the caret is
    /// pushed onto its own line.
    pub fn newline(&mut self, sel: &mut Sel, indent_unit: &str) {
        *sel = self.clamp_sel(*sel);
        let (start, end) = (sel.start(), sel.end());
        let ls = self.line_start(self.line_of(start));
        let indent: String =
            self.rope.slice(ls..start).chars().take_while(|c| *c == ' ' || *c == '\t').collect();

        let mut p = start;
        while p > ls && matches!(self.rope.char(p - 1), ' ' | '\t') {
            p -= 1;
        }
        let before = if p > ls { Some(self.rope.char(p - 1)) } else { None };
        let closer = match before {
            Some('(') => Some(')'),
            Some('[') => Some(']'),
            Some('{') => Some('}'),
            _ => None,
        };
        let opens = closer.is_some() || before == Some(':');

        let mut text = String::with_capacity(2 + indent.len() * 2 + indent_unit.len());
        text.push('\n');
        text.push_str(&indent);
        if opens {
            text.push_str(indent_unit);
        }
        let caret = start + text.chars().count();
        if closer.is_some() && self.char_at(end) == closer {
            text.push('\n');
            text.push_str(&indent);
        }
        self.begin(*sel);
        self.replace_inner(start, end, &text);
        *sel = Sel::caret(caret);
        self.commit(*sel, EditKind::Other);
    }

    /// Tab key: indent the lines of a multi-line selection, otherwise insert
    /// a tab or spaces up to the next indent stop.
    pub fn tab(&mut self, sel: &mut Sel, indent_unit: &str, indent_width: usize) {
        *sel = self.clamp_sel(*sel);
        if !sel.is_empty() && self.line_of(sel.start()) != self.line_of(sel.end()) {
            self.indent(sel, indent_unit);
            return;
        }
        let text = if indent_unit.starts_with('\t') {
            "\t".to_string()
        } else {
            let w = indent_width.max(1);
            let start = sel.start();
            let col = start - self.line_start(self.line_of(start));
            " ".repeat(w - col % w)
        };
        self.insert(sel, &text, EditKind::Type);
    }

    /// Indent every non-empty line the selection touches.
    pub fn indent(&mut self, sel: &mut Sel, indent_unit: &str) {
        if indent_unit.is_empty() {
            return;
        }
        *sel = self.clamp_sel(*sel);
        let (l0, l1) = self.selected_lines(*sel);
        let push_at = sel.is_empty();
        let mut s = *sel;
        self.begin(*sel);
        for line in l0..=l1 {
            let ls = self.line_start(line);
            if self.line_end(line) == ls {
                continue;
            }
            let n = self.replace_inner(ls, ls, indent_unit);
            s = Sel { anchor: map_insert(s.anchor, ls, n, push_at), head: map_insert(s.head, ls, n, push_at) };
        }
        *sel = s;
        self.commit(s, EditKind::Other);
    }

    /// Remove one tab or up to `indent_width` leading spaces from every line
    /// the selection touches.
    pub fn outdent(&mut self, sel: &mut Sel, indent_width: usize) {
        *sel = self.clamp_sel(*sel);
        let (l0, l1) = self.selected_lines(*sel);
        let mut s = *sel;
        self.begin(*sel);
        for line in l0..=l1 {
            let ls = self.line_start(line);
            let le = self.line_end(line);
            let n = match self.char_at(ls) {
                Some('\t') if ls < le => 1,
                _ => self.rope.slice(ls..le).chars().take(indent_width).take_while(|c| *c == ' ').count(),
            };
            if n == 0 {
                continue;
            }
            self.replace_inner(ls, ls + n, "");
            s = Sel { anchor: map_remove(s.anchor, ls, n), head: map_remove(s.head, ls, n) };
        }
        *sel = s;
        self.commit(s, EditKind::Other);
    }

    /// Duplicate the selected lines below; the selection moves to the copy.
    pub fn duplicate_lines(&mut self, sel: &mut Sel) {
        *sel = self.clamp_sel(*sel);
        let (l0, l1) = self.selected_lines(*sel);
        let start = self.line_start(l0);
        let end = self.line_end(l1);
        let mut text = String::from("\n");
        text.push_str(&self.slice(start, end));
        let cur = *sel;
        self.begin(*sel);
        let delta = self.replace_inner(end, end, &text);
        *sel = self.clamp_sel(Sel { anchor: cur.anchor + delta, head: cur.head + delta });
        self.commit(*sel, EditKind::Other);
    }

    /// Move the selected lines up or down by one; the selection follows.
    pub fn move_lines(&mut self, sel: &mut Sel, up: bool) {
        *sel = self.clamp_sel(*sel);
        let (l0, l1) = self.selected_lines(*sel);
        let cur = *sel;
        let block_start = self.line_start(l0);
        let block_end = self.line_end(l1);
        let block = self.slice(block_start, block_end);
        if up {
            if l0 == 0 {
                return;
            }
            let prev_start = self.line_start(l0 - 1);
            let prev_end = self.line_end(l0 - 1);
            let prev = self.slice(prev_start, prev_end);
            let sep = self.slice(prev_end, block_start);
            let delta = block_start - prev_start;
            let text = format!("{block}{sep}{prev}");
            self.begin(*sel);
            self.replace_inner(prev_start, block_end, &text);
            *sel = self.clamp_sel(Sel {
                anchor: cur.anchor.saturating_sub(delta),
                head: cur.head.saturating_sub(delta),
            });
        } else {
            if l1 + 1 >= self.lines() {
                return;
            }
            let next_start = self.line_start(l1 + 1);
            let next_end = self.line_end(l1 + 1);
            let next = self.slice(next_start, next_end);
            let sep = self.slice(block_end, next_start);
            let delta = (next_end - next_start) + (next_start - block_end);
            let text = format!("{next}{sep}{block}");
            self.begin(*sel);
            self.replace_inner(block_start, next_end, &text);
            *sel = self.clamp_sel(Sel { anchor: cur.anchor + delta, head: cur.head + delta });
        }
        self.commit(*sel, EditKind::Other);
    }

    /// Delete the selected lines; the caret keeps its column where it can.
    pub fn delete_lines(&mut self, sel: &mut Sel) {
        *sel = self.clamp_sel(*sel);
        let (l0, l1) = self.selected_lines(*sel);
        let head = sel.head;
        let col = head - self.line_start(self.line_of(head));
        let start = self.line_start(l0);
        self.begin(*sel);
        let target_line = if l1 + 1 < self.lines() {
            // take the trailing line break with it
            let end = self.line_start(l1 + 1);
            self.replace_inner(start, end, "");
            l0
        } else {
            // the block includes the last line: take the preceding break
            let from = if l0 > 0 { self.line_end(l0 - 1) } else { 0 };
            let len = self.len();
            self.replace_inner(from, len, "");
            l0.saturating_sub(1)
        };
        let ls = self.line_start(target_line);
        let le = self.line_end(target_line);
        *sel = Sel::caret((ls + col).min(le));
        self.commit(*sel, EditKind::Other);
    }

    /// Toggle line comments on the selected lines. If every non-blank line is
    /// commented they are uncommented; otherwise all are commented at the
    /// smallest indent. `suffix` closes block-style comments (`-->`, `*/`).
    pub fn toggle_comment(&mut self, sel: &mut Sel, prefix: &str, suffix: &str) {
        if prefix.is_empty() {
            return;
        }
        *sel = self.clamp_sel(*sel);
        let (l0, l1) = self.selected_lines(*sel);
        let lines: Vec<String> = (l0..=l1).map(|l| self.line_text(l)).collect();
        let blank = |t: &str| t.trim().is_empty();
        let any_text = lines.iter().any(|t| !blank(t));
        let commented = |t: &str| {
            let body = t.trim();
            body.starts_with(prefix)
                && (suffix.is_empty() || (body.ends_with(suffix) && body.len() >= prefix.len() + suffix.len()))
        };
        let all_commented = any_text && lines.iter().filter(|t| !blank(t)).all(|t| commented(t));
        let p_chars = prefix.chars().count();
        let s_chars = suffix.chars().count();
        let push_at = sel.is_empty();
        let mut s = *sel;
        self.begin(*sel);

        if all_commented {
            for (i, line) in (l0..=l1).enumerate() {
                if blank(&lines[i]) {
                    continue;
                }
                let ls = self.line_start(line);
                if !suffix.is_empty() {
                    let t = &lines[i];
                    let trimmed = t.trim_end();
                    let end_c = trimmed.chars().count();
                    let mut from = end_c - s_chars;
                    if trimmed[..trimmed.len() - suffix.len()].ends_with(' ') {
                        from -= 1;
                    }
                    let n = end_c - from;
                    self.replace_inner(ls + from, ls + end_c, "");
                    s = Sel { anchor: map_remove(s.anchor, ls + from, n), head: map_remove(s.head, ls + from, n) };
                }
                // re-read: removing the suffix may have taken the space after the prefix
                let t = self.line_text(line);
                let ind = leading_ws(&t);
                let rest: String = t.chars().skip(ind + p_chars).take(1).collect();
                let n = p_chars + usize::from(rest == " ");
                self.replace_inner(ls + ind, ls + ind + n, "");
                s = Sel { anchor: map_remove(s.anchor, ls + ind, n), head: map_remove(s.head, ls + ind, n) };
            }
        } else {
            let considered = |t: &str| !any_text || !blank(t);
            let min_indent = lines.iter().filter(|t| considered(t)).map(|t| leading_ws(t)).min().unwrap_or(0);
            let open = format!("{prefix} ");
            let close = format!(" {suffix}");
            for (i, line) in (l0..=l1).enumerate() {
                if !considered(&lines[i]) {
                    continue;
                }
                let at = self.line_start(line) + min_indent;
                let n = self.replace_inner(at, at, &open);
                s = Sel { anchor: map_insert(s.anchor, at, n, push_at), head: map_insert(s.head, at, n, push_at) };
                if !suffix.is_empty() {
                    let le = self.line_end(line);
                    let n = self.replace_inner(le, le, &close);
                    s = Sel { anchor: map_insert(s.anchor, le, n, false), head: map_insert(s.head, le, n, false) };
                }
            }
        }
        *sel = s;
        self.commit(s, EditKind::Other);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf(s: &str) -> Buffer {
        Buffer::new(s)
    }

    fn type_str(b: &mut Buffer, sel: &mut Sel, s: &str) {
        for c in s.chars() {
            b.insert(sel, &c.to_string(), EditKind::Type);
        }
    }

    /// Make the latest undo step look older than the merge window.
    fn age_last(b: &mut Buffer) {
        if let Some(t) = b.undo.back_mut() {
            t.at = t.at.checked_sub(Duration::from_secs(5)).unwrap_or(t.at);
        }
    }

    /// Replay notes against a line count and check it matches the rope.
    fn check_notes(b: &mut Buffer, lines_before: usize) {
        let mut n = lines_before;
        for note in b.take_notes() {
            n = n - note.lines_removed + note.lines_inserted;
        }
        assert_eq!(n, b.lines());
    }

    #[test]
    fn sel_basics() {
        let s = Sel::new(5, 2);
        assert_eq!((s.start(), s.end()), (2, 5));
        assert!(!s.is_empty());
        assert!(Sel::caret(3).is_empty());
        assert_eq!(Sel::default(), Sel::caret(0));
    }

    #[test]
    fn insert_and_replace_selection() {
        let mut b = buf("hello world");
        let mut s = Sel::new(0, 5);
        b.insert(&mut s, "bye", EditKind::Type);
        assert_eq!(b.text(), "bye world");
        assert_eq!(s, Sel::caret(3));
        let mut s = Sel::new(9, 4); // backwards selection
        b.insert(&mut s, "", EditKind::Other);
        assert_eq!(b.text(), "bye ");
        assert_eq!(s, Sel::caret(4));
        // out of range selection is clamped
        let mut s = Sel::new(100, 200);
        b.insert(&mut s, "x", EditKind::Other);
        assert_eq!(b.text(), "bye x");
        assert_eq!(s, Sel::caret(5));
    }

    #[test]
    fn insert_normalises_line_endings() {
        let mut b = buf("");
        let mut s = Sel::caret(0);
        b.insert(&mut s, "a\r\nb\rc", EditKind::Other);
        assert_eq!(b.text(), "a\nb\nc");
        assert_eq!(s, Sel::caret(5));
        assert_eq!(b.lines(), 3);
    }

    #[test]
    fn typing_coalesces_into_one_undo_step() {
        let mut b = buf("");
        let mut s = Sel::caret(0);
        type_str(&mut b, &mut s, "abc");
        assert_eq!(b.text(), "abc");
        assert_eq!(b.undo(), Some(Sel::caret(0)));
        assert_eq!(b.text(), "");
        assert_eq!(b.undo(), None);
        assert_eq!(b.redo(), Some(Sel::caret(3)));
        assert_eq!(b.text(), "abc");
        assert_eq!(b.redo(), None);
    }

    #[test]
    fn typing_breaks_at_word_boundary() {
        let mut b = buf("");
        let mut s = Sel::caret(0);
        type_str(&mut b, &mut s, "hi  there you");
        b.undo();
        assert_eq!(b.text(), "hi  there");
        b.undo();
        assert_eq!(b.text(), "hi");
        b.undo();
        assert_eq!(b.text(), "");
    }

    #[test]
    fn typing_does_not_merge_newlines_or_gaps() {
        let mut b = buf("");
        let mut s = Sel::caret(0);
        type_str(&mut b, &mut s, "a\nb");
        b.undo();
        assert_eq!(b.text(), "a");

        // non-contiguous typing
        let mut b = buf("xy");
        let mut s = Sel::caret(0);
        b.insert(&mut s, "a", EditKind::Type);
        let mut s = Sel::caret(3);
        b.insert(&mut s, "b", EditKind::Type);
        assert_eq!(b.text(), "axyb");
        b.undo();
        assert_eq!(b.text(), "axy");

        // a pause starts a new step
        let mut b = buf("");
        let mut s = Sel::caret(0);
        type_str(&mut b, &mut s, "ab");
        age_last(&mut b);
        type_str(&mut b, &mut s, "cd");
        b.undo();
        assert_eq!(b.text(), "ab");
    }

    #[test]
    fn typing_over_selection_merges_following_chars() {
        let mut b = buf("hello");
        let mut s = Sel::new(1, 4);
        type_str(&mut b, &mut s, "xyz");
        assert_eq!(b.text(), "hxyzo");
        assert_eq!(b.undo(), Some(Sel::new(1, 4)));
        assert_eq!(b.text(), "hello");
    }

    #[test]
    fn backspace_and_delete_coalesce() {
        let mut b = buf("hello");
        let mut s = Sel::caret(5);
        for _ in 0..3 {
            b.backspace(&mut s, false, 4);
        }
        assert_eq!(b.text(), "he");
        assert_eq!(s, Sel::caret(2));
        assert_eq!(b.undo(), Some(Sel::caret(5)));
        assert_eq!(b.text(), "hello");

        let mut s = Sel::caret(0);
        b.delete(&mut s, false);
        b.delete(&mut s, false);
        assert_eq!(b.text(), "llo");
        assert_eq!(s, Sel::caret(0));
        b.undo();
        assert_eq!(b.text(), "hello");
        // backspace then delete are different kinds: two steps
        let mut s = Sel::caret(2);
        b.backspace(&mut s, false, 4);
        b.delete(&mut s, false);
        assert_eq!(b.text(), "hlo");
        b.undo();
        assert_eq!(b.text(), "hllo");
        b.undo();
        assert_eq!(b.text(), "hello");
    }

    #[test]
    fn other_never_merges_and_redo_is_cleared() {
        let mut b = buf("");
        let mut s = Sel::caret(0);
        b.insert(&mut s, "a", EditKind::Other);
        b.insert(&mut s, "b", EditKind::Other);
        b.undo();
        assert_eq!(b.text(), "a");
        b.insert(&mut s, "c", EditKind::Other); // s is stale (2) but clamped
        assert_eq!(b.redo(), None);
        assert_eq!(b.text(), "ac");
    }

    #[test]
    fn dirty_round_trips() {
        let mut b = buf("x");
        assert!(!b.is_dirty());
        let mut s = Sel::caret(1);
        type_str(&mut b, &mut s, "a");
        assert!(b.is_dirty());
        b.mark_saved();
        assert!(!b.is_dirty());
        // must not merge into the saved step
        type_str(&mut b, &mut s, "b");
        assert!(b.is_dirty());
        b.undo();
        assert_eq!(b.text(), "xa");
        assert!(!b.is_dirty());
        b.undo();
        assert_eq!(b.text(), "x");
        assert!(b.is_dirty());
        b.redo();
        assert!(!b.is_dirty());
        b.redo();
        assert!(b.is_dirty());

        b.mark_dirty();
        b.undo();
        assert!(b.is_dirty());
        b.mark_saved();
        assert!(!b.is_dirty());
    }

    #[test]
    fn typing_after_undo_does_not_merge_into_restored_step() {
        let mut b = buf("");
        let mut s = Sel::caret(0);
        type_str(&mut b, &mut s, "ab");
        b.mark_saved();
        type_str(&mut b, &mut s, "c");
        s = b.undo().unwrap();
        assert!(!b.is_dirty());
        type_str(&mut b, &mut s, "d");
        assert_eq!(b.text(), "abd");
        assert!(b.is_dirty());
        b.undo();
        assert_eq!(b.text(), "ab");
        assert!(!b.is_dirty());
    }

    #[test]
    fn undo_history_is_capped() {
        let mut b = buf("");
        let mut s = Sel::caret(0);
        for _ in 0..UNDO_CAP + 10 {
            b.insert(&mut s, "x", EditKind::Other);
        }
        let mut n = 0;
        while b.undo().is_some() {
            n += 1;
        }
        assert_eq!(n, UNDO_CAP);
        assert_eq!(b.len(), 10);
        // the original (saved) state can no longer be reached
        assert!(b.is_dirty());
    }

    #[test]
    fn graphemes() {
        // family emoji: 5 code points, one cluster
        let b = buf("a👨‍👩‍👧b");
        assert_eq!(b.len(), 7);
        assert_eq!(b.next_grapheme(0), 1);
        assert_eq!(b.next_grapheme(1), 6);
        assert_eq!(b.prev_grapheme(6), 1);
        assert_eq!(b.prev_grapheme(7), 6);
        assert_eq!(b.prev_grapheme(0), 0);
        assert_eq!(b.next_grapheme(7), 7);
        assert_eq!(b.next_grapheme(99), 7);

        // combining mark
        let mut b = buf("e\u{301}x");
        assert_eq!(b.next_grapheme(0), 2);
        assert_eq!(b.prev_grapheme(2), 0);
        let mut s = Sel::caret(2);
        b.backspace(&mut s, false, 4);
        assert_eq!(b.text(), "x");
        assert_eq!(s, Sel::caret(0));

        // flags pair up, also in runs longer than the lookup window
        let b = buf("🇩🇪🇫🇷");
        assert_eq!(b.next_grapheme(0), 2);
        assert_eq!(b.next_grapheme(2), 4);
        assert_eq!(b.prev_grapheme(4), 2);
        let long: String = "🇩🇪".repeat(40);
        let b = buf(&long);
        assert_eq!(b.prev_grapheme(80), 78);
        assert_eq!(b.prev_grapheme(78), 76);
        assert_eq!(b.next_grapheme(40), 42);

        // a cluster longer than the window
        let zalgo: String = std::iter::once('a').chain(std::iter::repeat_n('\u{301}', 100)).collect();
        let b = buf(&format!("x{zalgo}y"));
        assert_eq!(b.prev_grapheme(102), 1);
        assert_eq!(b.next_grapheme(1), 102);

        // emoji delete forward
        let mut b = buf("😀a");
        let mut s = Sel::caret(0);
        b.delete(&mut s, false);
        assert_eq!(b.text(), "a");
    }

    #[test]
    fn word_motions() {
        let b = buf("foo bar_baz  qux.quux\nnext");
        assert_eq!(b.word_right(0), 3);
        assert_eq!(b.word_right(3), 11);
        assert_eq!(b.word_right(11), 16);
        assert_eq!(b.word_right(16), 17);
        assert_eq!(b.word_right(17), 21);
        assert_eq!(b.word_right(21), 22);
        assert_eq!(b.word_right(22), 26);
        assert_eq!(b.word_right(26), 26);
        assert_eq!(b.word_right(500), 26);

        assert_eq!(b.word_left(26), 22);
        assert_eq!(b.word_left(22), 21);
        assert_eq!(b.word_left(21), 17);
        assert_eq!(b.word_left(17), 16);
        assert_eq!(b.word_left(16), 13);
        assert_eq!(b.word_left(13), 4);
        assert_eq!(b.word_left(4), 0);
        assert_eq!(b.word_left(0), 0);

        // leading whitespace: stop at the line start, then hop to the previous line
        let b = buf("  x\n  y");
        assert_eq!(b.word_left(6), 4);
        assert_eq!(b.word_left(4), 3);
        // trailing whitespace: stop at the line end
        let b = buf("x  \ny");
        assert_eq!(b.word_right(1), 3);
        assert_eq!(b.word_right(3), 4);
    }

    #[test]
    fn word_at_picks_the_word() {
        let b = buf("foo bar");
        assert_eq!(b.word_at(1), (0, 3));
        assert_eq!(b.word_at(3), (0, 3));
        assert_eq!(b.word_at(4), (4, 7));
        assert_eq!(b.word_at(7), (4, 7));
        assert_eq!(b.word_at(99), (4, 7));
        let b = buf("a   b");
        assert_eq!(b.word_at(2), (1, 4));
        let b = buf("x += y");
        assert_eq!(b.word_at(3), (2, 4));
        let b = buf("a\n\nb");
        assert_eq!(b.word_at(2), (2, 2));
        let b = buf("");
        assert_eq!(b.word_at(0), (0, 0));
    }

    #[test]
    fn word_delete() {
        let mut b = buf("foo bar baz");
        let mut s = Sel::caret(7);
        b.backspace(&mut s, true, 4);
        assert_eq!(b.text(), "foo  baz");
        assert_eq!(s, Sel::caret(4));
        b.delete(&mut s, true);
        assert_eq!(b.text(), "foo ");
        // word deletes are separate undo steps
        b.undo();
        assert_eq!(b.text(), "foo  baz");
        b.undo();
        assert_eq!(b.text(), "foo bar baz");
    }

    #[test]
    fn backspace_in_leading_spaces_goes_to_indent_stop() {
        let mut b = buf("      x");
        let mut s = Sel::caret(6);
        b.backspace(&mut s, false, 4);
        assert_eq!(b.text(), "    x");
        assert_eq!(s, Sel::caret(4));
        b.backspace(&mut s, false, 4);
        assert_eq!(b.text(), "x");
        // not in leading whitespace: one char
        let mut b = buf("a   b");
        let mut s = Sel::caret(4);
        b.backspace(&mut s, false, 4);
        assert_eq!(b.text(), "a  b");
        // tabs go one at a time
        let mut b = buf("\t\tx");
        let mut s = Sel::caret(2);
        b.backspace(&mut s, false, 4);
        assert_eq!(b.text(), "\tx");
        // joining lines
        let mut b = buf("a\nb");
        let mut s = Sel::caret(2);
        b.backspace(&mut s, false, 4);
        assert_eq!(b.text(), "ab");
        assert_eq!(s, Sel::caret(1));
    }

    #[test]
    fn edit_notes() {
        let mut b = buf("ab\ncd\nef");
        b.begin(Sel::caret(0));
        b.replace(1, 4, "X\nY\nZ");
        b.commit(Sel::caret(6), EditKind::Other);
        assert_eq!(b.text(), "aX\nY\nZd\nef");
        let notes = b.take_notes();
        assert_eq!(notes.len(), 1);
        let n = notes[0];
        assert_eq!((n.pos, n.removed, n.inserted), (1, 3, 5));
        assert_eq!((n.line, n.lines_removed, n.lines_inserted), (0, 1, 2));
        assert!(b.notes.is_empty());

        // undo reports the inverse
        b.undo();
        let notes = b.take_notes();
        assert_eq!(notes.len(), 1);
        let n = notes[0];
        assert_eq!((n.pos, n.removed, n.inserted), (1, 5, 3));
        assert_eq!((n.line, n.lines_removed, n.lines_inserted), (0, 2, 1));
        assert_eq!(b.text(), "ab\ncd\nef");

        // multi-line delete starting on a later line
        let mut b = buf("a\nb\nc\nd");
        let mut s = Sel::new(3, 6);
        b.insert(&mut s, "", EditKind::Other);
        assert_eq!(b.text(), "a\nbd");
        let n = b.take_notes()[0];
        assert_eq!((n.pos, n.removed, n.inserted), (3, 3, 0));
        assert_eq!((n.line, n.lines_removed, n.lines_inserted), (1, 2, 0));

        // insert at the very end after a trailing newline
        let mut b = buf("a\n");
        let mut s = Sel::caret(2);
        b.insert(&mut s, "b\nc\n", EditKind::Other);
        let n = b.take_notes()[0];
        assert_eq!((n.line, n.lines_removed, n.lines_inserted), (1, 0, 2));
        assert_eq!(b.lines(), 4);

        // rev bumps on every change, including undo
        let r = b.rev;
        b.undo();
        assert!(b.rev > r);
    }

    #[test]
    fn map_pos_and_sel() {
        let n = EditNote { pos: 5, removed: 3, inserted: 2, line: 0, lines_removed: 0, lines_inserted: 0 };
        assert_eq!(n.map_pos(4), 4);
        assert_eq!(n.map_pos(5), 5);
        assert_eq!(n.map_pos(6), 7);
        assert_eq!(n.map_pos(7), 7);
        assert_eq!(n.map_pos(8), 7);
        assert_eq!(n.map_pos(10), 9);
        assert_eq!(n.map_sel(Sel::new(10, 4)), Sel::new(9, 4));
        // pure insertion: a position at the insertion point stays put
        let n = EditNote { pos: 2, removed: 0, inserted: 3, line: 0, lines_removed: 0, lines_inserted: 0 };
        assert_eq!(n.map_pos(2), 2);
        assert_eq!(n.map_pos(3), 6);
    }

    #[test]
    fn positions() {
        let b = buf("añb\nc\n");
        assert_eq!(b.lines(), 3);
        assert_eq!(b.line_of(0), 0);
        assert_eq!(b.line_of(3), 0);
        assert_eq!(b.line_of(4), 1);
        assert_eq!(b.line_of(6), 2);
        assert_eq!(b.line_of(99), 2);
        assert_eq!(b.line_start(1), 4);
        assert_eq!(b.line_start(99), 6);
        assert_eq!(b.line_end(0), 3);
        assert_eq!(b.line_end(1), 5);
        assert_eq!(b.line_end(2), 6);
        assert_eq!(b.line_end(99), 6);
        assert_eq!(b.line_text(0), "añb");
        assert_eq!(b.line_text(2), "");
        assert_eq!(b.pos_to_line_byte(2), (0, 3));
        assert_eq!(b.pos_to_line_byte(3), (0, 4));
        assert_eq!(b.pos_to_line_byte(4), (1, 0));
        assert_eq!(b.pos_to_line_byte(99), (2, 0));
        assert_eq!(b.line_byte_to_pos(0, 1), 1);
        assert_eq!(b.line_byte_to_pos(0, 2), 1); // inside ñ: snapped down
        assert_eq!(b.line_byte_to_pos(0, 3), 2);
        assert_eq!(b.line_byte_to_pos(0, 99), 3);
        assert_eq!(b.line_byte_to_pos(1, 1), 5);
        assert_eq!(b.line_byte_to_pos(99, 0), 6);
        assert_eq!(b.slice(1, 3), "ñb");
        assert_eq!(b.slice(3, 1), "ñb");
        assert_eq!(b.slice(4, 99), "c\n");

        let b = buf("  \tfoo\n   \nbar");
        assert_eq!(b.first_non_ws(0), 3);
        assert_eq!(b.first_non_ws(1), 10); // blank: line end
        assert_eq!(b.first_non_ws(2), 11);

        // last line without a trailing newline
        let b = buf("a\nbc");
        assert_eq!(b.line_end(1), 4);
        assert_eq!(b.line_text(1), "bc");
    }

    #[test]
    fn selected_lines() {
        let b = buf("aa\nbb\ncc");
        assert_eq!(b.selected_lines(Sel::caret(4)), (1, 1));
        assert_eq!(b.selected_lines(Sel::new(1, 6)), (0, 1)); // ends at col 0 of line 2
        assert_eq!(b.selected_lines(Sel::new(7, 1)), (0, 2));
        assert_eq!(b.selected_lines(Sel::caret(6)), (2, 2));
        assert_eq!(b.selected_lines(Sel::new(3, 6)), (1, 1));
        assert_eq!(b.selected_lines(Sel::new(0, 99)), (0, 2));
    }

    #[test]
    fn newline_auto_indent() {
        let mut b = buf("    foo");
        let mut s = Sel::caret(7);
        b.newline(&mut s, "    ");
        assert_eq!(b.text(), "    foo\n    ");
        assert_eq!(s, Sel::caret(12));
        assert_eq!(b.undo(), Some(Sel::caret(7)));
        assert_eq!(b.text(), "    foo");

        // extra indent after an opener, trailing spaces ignored
        let mut b = buf("fn x() {  ");
        let mut s = Sel::caret(10);
        b.newline(&mut s, "    ");
        assert_eq!(b.text(), "fn x() {  \n    ");

        let mut b = buf("def f():");
        let mut s = Sel::caret(8);
        b.newline(&mut s, "\t");
        assert_eq!(b.text(), "def f():\n\t");
        assert_eq!(s, Sel::caret(10));

        // between brackets: closer goes on its own line
        let mut b = buf("  if (a) {}");
        let mut s = Sel::caret(10);
        b.newline(&mut s, "    ");
        assert_eq!(b.text(), "  if (a) {\n      \n  }");
        assert_eq!(s, Sel::caret(17));
        assert_eq!(b.undo(), Some(Sel::caret(10)));
        assert_eq!(b.text(), "  if (a) {}");

        // mismatched closer: no split
        let mut b = buf("foo(]");
        let mut s = Sel::caret(4);
        b.newline(&mut s, "  ");
        assert_eq!(b.text(), "foo(\n  ]");

        // caret inside the indentation copies only what is before it
        let mut b = buf("    foo");
        let mut s = Sel::caret(2);
        b.newline(&mut s, "    ");
        assert_eq!(b.text(), "  \n    foo");
        assert_eq!(s, Sel::caret(5));

        // replaces a selection
        let mut b = buf("  abXYcd");
        let mut s = Sel::new(4, 6);
        b.newline(&mut s, "    ");
        assert_eq!(b.text(), "  ab\n  cd");
        assert_eq!(s, Sel::caret(7));

        // empty buffer
        let mut b = buf("");
        let mut s = Sel::caret(0);
        b.newline(&mut s, "    ");
        assert_eq!(b.text(), "\n");
        assert_eq!(s, Sel::caret(1));
    }

    #[test]
    fn tab_key() {
        let mut b = buf("ab");
        let mut s = Sel::caret(2);
        b.tab(&mut s, "    ", 4);
        assert_eq!(b.text(), "ab  ");
        assert_eq!(s, Sel::caret(4));
        b.tab(&mut s, "    ", 4);
        assert_eq!(b.text(), "ab      ");
        let mut s = Sel::caret(0);
        b.tab(&mut s, "\t", 4);
        assert_eq!(b.text(), "\tab      ");
        assert_eq!(s, Sel::caret(1));

        // single-line selection is replaced
        let mut b = buf("abcd");
        let mut s = Sel::new(1, 3);
        b.tab(&mut s, "  ", 2);
        assert_eq!(b.text(), "a d");

        // multi-line selection indents, blank lines skipped
        let mut b = buf("a\n\nb\nc");
        let mut s = Sel::new(0, 4);
        b.tab(&mut s, "  ", 2);
        assert_eq!(b.text(), "  a\n\n  b\nc");
        assert_eq!(s, Sel::new(0, 8));
        assert_eq!(b.undo(), Some(Sel::new(0, 4)));
        assert_eq!(b.text(), "a\n\nb\nc");
    }

    #[test]
    fn indent_and_outdent() {
        let mut b = buf("foo\nbar");
        // caret travels with the text
        let mut s = Sel::caret(0);
        b.indent(&mut s, "    ");
        assert_eq!(b.text(), "    foo\nbar");
        assert_eq!(s, Sel::caret(4));
        let mut s = Sel::caret(6);
        b.indent(&mut s, "    ");
        assert_eq!(s, Sel::caret(10));
        b.outdent(&mut s, 4);
        b.outdent(&mut s, 4);
        assert_eq!(b.text(), "foo\nbar");
        assert_eq!(s, Sel::caret(2));
        // nothing left to remove: no undo step
        b.mark_saved();
        b.outdent(&mut s, 4);
        assert!(!b.is_dirty());

        let mut b = buf("    a\n  b\n\tc\nd\n     e");
        let mut s = Sel::new(0, b.len());
        b.outdent(&mut s, 4);
        assert_eq!(b.text(), "a\nb\nc\nd\n e");
        assert_eq!(s, Sel::new(0, b.len()));
        b.undo();
        assert_eq!(b.text(), "    a\n  b\n\tc\nd\n     e");

        // selection keeps covering the same text
        let mut b = buf("ab\ncd");
        let mut s = Sel::new(1, 4);
        b.indent(&mut s, "\t");
        assert_eq!(b.text(), "\tab\n\tcd");
        assert_eq!(b.slice(s.start(), s.end()), "b\n\tc");
        // outdent with the caret inside the removed indent
        let mut b = buf("    x");
        let mut s = Sel::caret(2);
        b.outdent(&mut s, 4);
        assert_eq!(s, Sel::caret(0));
    }

    #[test]
    fn duplicate_lines() {
        let mut b = buf("a\nb");
        let mut s = Sel::caret(1);
        b.duplicate_lines(&mut s);
        assert_eq!(b.text(), "a\na\nb");
        assert_eq!(s, Sel::caret(3));
        // last line, no trailing newline
        let mut s = Sel::caret(5);
        b.duplicate_lines(&mut s);
        assert_eq!(b.text(), "a\na\nb\nb");
        assert_eq!(s, Sel::caret(7));
        b.undo();
        assert_eq!(b.text(), "a\na\nb");

        // multi-line selection ending at col 0 of the next line
        let mut b = buf("one\ntwo\nthree");
        let mut s = Sel::new(0, 8);
        b.duplicate_lines(&mut s);
        assert_eq!(b.text(), "one\ntwo\none\ntwo\nthree");
        assert_eq!(b.slice(s.start(), s.end()), "one\ntwo\n");
        assert_eq!(s.start(), 8);

        let mut b = buf("");
        let mut s = Sel::caret(0);
        b.duplicate_lines(&mut s);
        assert_eq!(b.text(), "\n");
        assert_eq!(s, Sel::caret(1));
    }

    #[test]
    fn move_lines() {
        let mut b = buf("a\nb\nc");
        let mut s = Sel::caret(2);
        b.move_lines(&mut s, true);
        assert_eq!(b.text(), "b\na\nc");
        assert_eq!(s, Sel::caret(0));
        // already at the top: nothing happens, no undo step
        b.mark_saved();
        b.move_lines(&mut s, true);
        assert_eq!(b.text(), "b\na\nc");
        assert!(!b.is_dirty());
        b.move_lines(&mut s, false);
        b.move_lines(&mut s, false);
        assert_eq!(b.text(), "a\nc\nb");
        assert_eq!(s, Sel::caret(4));
        // already at the bottom
        b.move_lines(&mut s, false);
        assert_eq!(b.text(), "a\nc\nb");
        b.undo();
        assert_eq!(b.text(), "a\nb\nc");

        // a block
        let mut b = buf("aa\nbb\ncc\ndd");
        let mut s = Sel::new(1, 4);
        b.move_lines(&mut s, false);
        assert_eq!(b.text(), "cc\naa\nbb\ndd");
        assert_eq!(b.slice(s.start(), s.end()), "a\nb");
        b.move_lines(&mut s, false);
        assert_eq!(b.text(), "cc\ndd\naa\nbb");
        assert_eq!(b.slice(s.start(), s.end()), "a\nb");
        b.move_lines(&mut s, true);
        b.move_lines(&mut s, true);
        assert_eq!(b.text(), "aa\nbb\ncc\ndd");
        assert_eq!(s, Sel::new(1, 4));

        // selection ending at col 0 of the following line, moved to the end
        let mut b = buf("aa\nbb\ncc");
        let mut s = Sel::new(0, 6);
        b.move_lines(&mut s, false);
        assert_eq!(b.text(), "cc\naa\nbb");
        assert_eq!(s.start(), 3);
        assert!(s.end() <= b.len());

        // moving past a trailing empty line
        let mut b = buf("a\nb\n");
        let mut s = Sel::caret(2);
        b.move_lines(&mut s, false);
        assert_eq!(b.text(), "a\n\nb");
        assert_eq!(s, Sel::caret(3));
    }

    #[test]
    fn delete_lines() {
        let mut b = buf("a\nbcd\nc");
        let mut s = Sel::caret(4);
        b.delete_lines(&mut s);
        assert_eq!(b.text(), "a\nc");
        assert_eq!(s, Sel::caret(3)); // column kept where the new line allows
        // last line
        b.delete_lines(&mut s);
        assert_eq!(b.text(), "a");
        assert_eq!(s, Sel::caret(1));
        // only line
        b.delete_lines(&mut s);
        assert_eq!(b.text(), "");
        assert_eq!(s, Sel::caret(0));
        b.delete_lines(&mut s);
        assert_eq!(b.text(), "");
        b.undo();
        b.undo();
        b.undo();
        assert_eq!(b.text(), "a\nbcd\nc");

        // first line
        let mut b = buf("a\nb\nc");
        let mut s = Sel::caret(0);
        b.delete_lines(&mut s);
        assert_eq!(b.text(), "b\nc");
        // everything
        let mut s = Sel::new(0, 3);
        b.delete_lines(&mut s);
        assert_eq!(b.text(), "");
        // trailing newline: deleting the last real line keeps one empty line
        let mut b = buf("a\nb\n");
        let mut s = Sel::caret(2);
        b.delete_lines(&mut s);
        assert_eq!(b.text(), "a\n");
    }

    #[test]
    fn toggle_comment_lines() {
        let mut b = buf("foo\n  bar");
        let mut s = Sel::new(0, b.len());
        b.toggle_comment(&mut s, "//", "");
        assert_eq!(b.text(), "// foo\n//   bar");
        assert_eq!(s, Sel::new(0, b.len()));
        b.toggle_comment(&mut s, "//", "");
        assert_eq!(b.text(), "foo\n  bar");
        assert_eq!(s, Sel::new(0, b.len()));
        b.undo();
        assert_eq!(b.text(), "// foo\n//   bar");

        // commented at the smallest indent, blank lines skipped
        let mut b = buf("    a\n      b\n\n    c");
        let mut s = Sel::new(2, b.len());
        b.toggle_comment(&mut s, "#", "");
        assert_eq!(b.text(), "    # a\n    #   b\n\n    # c");
        b.toggle_comment(&mut s, "#", "");
        assert_eq!(b.text(), "    a\n      b\n\n    c");

        // mixed: comment everything
        let mut b = buf("// a\nb");
        let mut s = Sel::new(0, b.len());
        b.toggle_comment(&mut s, "//", "");
        assert_eq!(b.text(), "// // a\n// b");

        // no space after the prefix still uncomments
        let mut b = buf("//foo");
        let mut s = Sel::caret(0);
        b.toggle_comment(&mut s, "//", "");
        assert_eq!(b.text(), "foo");

        // caret moves with the text
        let mut b = buf("foo");
        let mut s = Sel::caret(0);
        b.toggle_comment(&mut s, "//", "");
        assert_eq!(s, Sel::caret(3));
        let mut s = Sel::caret(5);
        b.toggle_comment(&mut s, "//", "");
        assert_eq!(b.text(), "foo");
        assert_eq!(s, Sel::caret(2));

        // blank line gets a comment to type into
        let mut b = buf("");
        let mut s = Sel::caret(0);
        b.toggle_comment(&mut s, "//", "");
        assert_eq!(b.text(), "// ");
        assert_eq!(s, Sel::caret(3));

        // empty prefix: nothing
        let mut b = buf("x");
        let mut s = Sel::caret(0);
        b.toggle_comment(&mut s, "", "");
        assert_eq!(b.text(), "x");
    }

    #[test]
    fn toggle_comment_with_suffix() {
        let mut b = buf("  <p>hi</p>\n<b>");
        let mut s = Sel::new(0, b.len());
        b.toggle_comment(&mut s, "<!--", "-->");
        assert_eq!(b.text(), "<!--   <p>hi</p> -->\n<!-- <b> -->");
        b.toggle_comment(&mut s, "<!--", "-->");
        assert_eq!(b.text(), "  <p>hi</p>\n<b>");

        // caret at the end of the line stays before the suffix
        let mut b = buf("x");
        let mut s = Sel::caret(1);
        b.toggle_comment(&mut s, "/*", "*/");
        assert_eq!(b.text(), "/* x */");
        assert_eq!(s, Sel::caret(4));
        b.toggle_comment(&mut s, "/*", "*/");
        assert_eq!(b.text(), "x");
        assert_eq!(s, Sel::caret(1));

        // degenerate empty comment
        let mut b = buf("<!-- -->");
        let mut s = Sel::caret(0);
        b.toggle_comment(&mut s, "<!--", "-->");
        assert_eq!(b.text(), "");
        assert_eq!(s, Sel::caret(0));
    }

    #[test]
    fn set_text_replaces_only_the_difference() {
        let mut b = buf("hello world");
        let s = b.set_text(Sel::caret(8), "hello brave world");
        assert_eq!(b.text(), "hello brave world");
        assert_eq!(s, Sel::caret(14));
        let notes = b.take_notes();
        assert_eq!(notes.len(), 1);
        assert_eq!((notes[0].pos, notes[0].removed, notes[0].inserted), (6, 0, 6));
        assert!(b.is_dirty());
        assert_eq!(b.undo(), Some(Sel::caret(8)));
        assert_eq!(b.text(), "hello world");
        assert!(!b.is_dirty());

        // identical text: no step
        let s = b.set_text(Sel::caret(99), "hello world");
        assert_eq!(s, Sel::caret(11));
        assert!(!b.is_dirty());
        assert_eq!(b.undo(), None);

        // caret inside the replaced range
        let mut b = buf("abcXYZdef");
        let s = b.set_text(Sel::new(5, 8), "abcQdef");
        assert_eq!(b.text(), "abcQdef");
        assert_eq!(s, Sel::new(4, 6));

        // multibyte chars sharing a lead byte
        let mut b = buf("héllo");
        b.set_text(Sel::caret(0), "hèllo");
        assert_eq!(b.text(), "hèllo");
        let n = b.take_notes()[0];
        assert_eq!((n.pos, n.removed, n.inserted), (1, 1, 1));

        // CRLF input is normalised before diffing
        let mut b = buf("a\nb");
        b.set_text(Sel::caret(0), "a\r\nb\r\nc");
        assert_eq!(b.text(), "a\nb\nc");

        // to and from empty
        let mut b = buf("abc");
        b.set_text(Sel::caret(2), "");
        assert_eq!(b.text(), "");
        b.set_text(Sel::caret(0), "xyz");
        assert_eq!(b.text(), "xyz");
        b.undo();
        b.undo();
        assert_eq!(b.text(), "abc");
    }

    #[test]
    fn transactions_group_changes() {
        let mut b = buf("a b c");
        b.begin(Sel::caret(0));
        b.replace(4, 5, "C");
        b.replace(2, 3, "B");
        b.replace(0, 1, "A");
        b.commit(Sel::caret(5), EditKind::Other);
        assert_eq!(b.text(), "A B C");
        assert_eq!(b.take_notes().len(), 3);
        assert_eq!(b.undo(), Some(Sel::caret(0)));
        assert_eq!(b.text(), "a b c");
        assert_eq!(b.redo(), Some(Sel::caret(5)));
        assert_eq!(b.text(), "A B C");

        // empty transaction is dropped
        b.mark_saved();
        b.begin(Sel::caret(0));
        b.replace(2, 2, "");
        b.commit(Sel::caret(0), EditKind::Other);
        assert!(!b.is_dirty());

        // reversed and out-of-range arguments are tolerated
        b.begin(Sel::caret(0));
        b.replace(100, 3, "!");
        b.commit(Sel::caret(0), EditKind::Other);
        assert_eq!(b.text(), "A B!");
        // commit without begin is a no-op; replace without begin opens a step
        b.commit(Sel::caret(0), EditKind::Other);
        b.replace(0, 1, "a");
        b.undo();
        assert_eq!(b.text(), "A B!");
    }

    #[test]
    fn grouped_edits_are_one_undo_step_and_remember_cursors() {
        // two cursors, after "a" and after "b"
        let mut b = buf("a b");
        let (mut c1, mut c2) = (Sel::caret(1), Sel::caret(3));
        let mut type_both = |b: &mut Buffer, c1: &mut Sel, c2: &mut Sel, ch: &str| {
            b.group_begin(*c1, vec![*c2]);
            b.insert(c2, ch, EditKind::Type);
            b.insert(c1, ch, EditKind::Type);
            *c2 = Sel::caret(c2.head + 1); // the earlier insert pushed it along
            b.group_end(*c1, vec![*c2], EditKind::Type);
        };
        type_both(&mut b, &mut c1, &mut c2, "x");
        type_both(&mut b, &mut c1, &mut c2, "y");
        assert_eq!(b.text(), "axy bxy");
        b.take_notes();
        // both letters at both cursors come back out together
        assert_eq!(b.undo(), Some(Sel::caret(1)));
        assert_eq!(b.take_restored(), vec![Sel::caret(3)]);
        assert_eq!(b.text(), "a b");
        assert!(!b.is_dirty());
        assert_eq!(b.redo(), Some(Sel::caret(3)));
        assert_eq!(b.take_restored(), vec![Sel::caret(7)]);
        assert_eq!(b.text(), "axy bxy");
        // a space starts a new step, as with one cursor
        type_both(&mut b, &mut c1, &mut c2, " ");
        b.undo();
        assert_eq!(b.text(), "axy bxy");
    }

    #[test]
    fn empty_buffer_is_safe() {
        let mut b = buf("");
        let mut s = Sel::caret(0);
        b.backspace(&mut s, false, 4);
        b.backspace(&mut s, true, 4);
        b.delete(&mut s, false);
        b.delete(&mut s, true);
        b.indent(&mut s, "    ");
        b.outdent(&mut s, 4);
        b.move_lines(&mut s, true);
        b.move_lines(&mut s, false);
        b.delete_lines(&mut s);
        assert_eq!(b.text(), "");
        assert_eq!(b.undo(), None);
        assert_eq!(b.redo(), None);
        assert_eq!(b.word_left(0), 0);
        assert_eq!(b.word_right(0), 0);
        assert_eq!(b.prev_grapheme(5), 0);
        assert_eq!(b.next_grapheme(5), 0);
        assert_eq!(b.line_text(3), "");
        assert_eq!(b.pos_to_line_byte(9), (0, 0));
        assert_eq!(b.line_byte_to_pos(4, 4), 0);
        assert_eq!(b.first_non_ws(0), 0);
        assert_eq!(b.selected_lines(Sel::new(3, 9)), (0, 0));
        assert_eq!(b.set_text(Sel::caret(4), ""), Sel::caret(0));
        assert!(!b.is_dirty());
        assert_eq!((b.len(), b.lines()), (0, 1));

        // stale selections past the end
        let mut b = buf("ab");
        let mut s = Sel::new(10, 20);
        b.backspace(&mut s, false, 4);
        b.delete(&mut s, false);
        b.newline(&mut s, "  ");
        b.tab(&mut s, "  ", 2);
        b.duplicate_lines(&mut s);
        b.toggle_comment(&mut s, "//", "");
        b.move_lines(&mut s, true);
        b.delete_lines(&mut s);
        assert!(s.end() <= b.len());
    }

    #[test]
    fn random_edits_keep_notes_consistent_and_undo_restores() {
        let original = "fn main() {\n    let x = 1;\n\n    println!(\"{x}\");\n}\n";
        let mut b = buf(original);
        let mut seed = 0x2545F4914F6CDD1Du64;
        let mut rnd = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n.max(1) as u64) as usize
        };
        for _ in 0..600 {
            let len = b.len();
            let mut s = Sel::new(rnd(len + 1), rnd(len + 1));
            if rnd(3) > 0 {
                s = Sel::caret(s.head);
            }
            let lines = b.lines();
            match rnd(14) {
                0 => b.insert(&mut s, "x", EditKind::Type),
                1 => b.insert(&mut s, "a\nb\n", EditKind::Other),
                2 => b.backspace(&mut s, false, 4),
                3 => b.backspace(&mut s, true, 4),
                4 => b.delete(&mut s, false),
                5 => b.delete(&mut s, true),
                6 => b.newline(&mut s, "    "),
                7 => b.tab(&mut s, "    ", 4),
                8 => b.outdent(&mut s, 4),
                9 => b.duplicate_lines(&mut s),
                10 => b.move_lines(&mut s, rnd(2) == 0),
                11 => b.delete_lines(&mut s),
                12 => b.toggle_comment(&mut s, "//", ""),
                _ => {
                    if let Some(r) = if rnd(2) == 0 { b.undo() } else { b.redo() } {
                        s = r;
                    }
                }
            }
            assert!(s.anchor <= b.len() && s.head <= b.len());
            check_notes(&mut b, lines);
        }
        while b.undo().is_some() {}
        assert_eq!(b.text(), original);
        assert!(!b.is_dirty());
    }
}
