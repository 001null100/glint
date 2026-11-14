//! A document: text buffer plus its file, language and on-disk bookkeeping.
//! Views (cursor, scroll) live in tabs, so one document can be open in
//! several panes at once.

use crate::buffer::{Buffer, Sel};
use crate::highlight::{detect_lang, Highlighter};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub type DocId = u64;

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct DiskStamp {
    pub mtime: SystemTime,
    pub len: u64,
}

impl DiskStamp {
    pub fn of(path: &Path) -> Option<DiskStamp> {
        let m = std::fs::metadata(path).ok()?;
        Some(DiskStamp { mtime: m.modified().ok()?, len: m.len() })
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum DiskState {
    Same,
    Changed,
    Missing,
}

pub struct Document {
    pub id: DocId,
    pub buf: Buffer,
    pub path: Option<PathBuf>,
    /// Number shown for untitled documents with no text yet.
    pub untitled_n: u32,
    pub hl: Highlighter,
    pub lang_forced: bool,
    pub crlf: bool,
    pub bom: bool,
    pub indent_tabs: bool,
    pub indent_width: usize,
    /// What the file looked like when buffer and disk last agreed.
    pub disk: Option<DiskStamp>,
    pub missing: bool,
    /// `buf.rev` at the last draft write (or when it matched the disk).
    pub draft_rev: u64,
    pub has_draft_file: bool,
    pub edited_at: f64,
}

const MAX_FILE: u64 = 256 * 1024 * 1024;

fn detect_indent(text: &str, default_width: usize) -> (bool, usize) {
    let (mut tabs, mut spaces) = (0usize, 0usize);
    let mut hist = [0usize; 9];
    let mut prev = 0usize;
    for line in text.lines().take(600) {
        if line.starts_with('\t') {
            tabs += 1;
            continue;
        }
        let n = line.len() - line.trim_start_matches(' ').len();
        if line.trim().is_empty() {
            continue;
        }
        if n > 0 {
            spaces += 1;
        }
        let d = n.abs_diff(prev);
        if (1..=8).contains(&d) {
            hist[d] += 1;
        }
        prev = n;
    }
    if tabs > spaces {
        return (true, default_width);
    }
    if spaces == 0 {
        return (false, default_width);
    }
    let mut best = default_width;
    let mut best_n = 0;
    for w in [2usize, 4, 8, 3] {
        if hist[w] > best_n {
            best_n = hist[w];
            best = w;
        }
    }
    (false, best)
}

impl Document {
    pub fn untitled(id: DocId, n: u32, tab_width: usize) -> Document {
        Document {
            id,
            buf: Buffer::new(""),
            path: None,
            untitled_n: n,
            hl: Highlighter::new("Plain Text"),
            lang_forced: false,
            crlf: false,
            bom: false,
            indent_tabs: false,
            indent_width: tab_width,
            disk: None,
            missing: false,
            draft_rev: 0,
            has_draft_file: false,
            edited_at: 0.0,
        }
    }

    /// Build a document from raw text (as read from a file or a draft).
    pub fn from_text(id: DocId, raw: &str, path: Option<PathBuf>, tab_width: usize) -> Document {
        let mut d = Document::untitled(id, 0, tab_width);
        let (text, crlf, bom) = normalize(raw);
        let (tabs, width) = detect_indent(&text, tab_width);
        d.crlf = crlf;
        d.bom = bom;
        d.indent_tabs = tabs;
        d.indent_width = width;
        d.hl = Highlighter::new(&detect_lang(path.as_deref(), text.lines().next().unwrap_or("")));
        d.buf = Buffer::new(&text);
        d.path = path;
        d
    }

    pub fn read_file(path: &Path) -> Result<String, String> {
        let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
        if meta.is_dir() {
            return Err(format!("{} is a folder", path.display()));
        }
        if meta.len() > MAX_FILE {
            return Err(format!("{} is too large to open", path.display()));
        }
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        if bytes.iter().take(8192).any(|b| *b == 0) {
            return Err(format!("{} looks like a binary file", path.display()));
        }
        Ok(match String::from_utf8(bytes) {
            Ok(s) => s,
            Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
        })
    }

    pub fn open(id: DocId, path: &Path, tab_width: usize) -> Result<Document, String> {
        let raw = Self::read_file(path)?;
        let mut d = Document::from_text(id, &raw, Some(path.to_path_buf()), tab_width);
        d.disk = DiskStamp::of(path);
        Ok(d)
    }

    pub fn title(&self) -> String {
        if let Some(p) = &self.path {
            return p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string());
        }
        // untitled drafts are named after their first line of text
        let lines = self.buf.lines().min(20);
        for i in 0..lines {
            let t = self.buf.line_text(i);
            let t = t.trim();
            if !t.is_empty() {
                let mut s: String = t.chars().take(28).collect();
                if t.chars().count() > 28 {
                    s.push('…');
                }
                return s;
            }
        }
        if self.untitled_n > 1 { format!("untitled {}", self.untitled_n) } else { "untitled".to_string() }
    }

    pub fn display_path(&self) -> String {
        match &self.path {
            Some(p) => {
                let s = p.display().to_string();
                if let Some(home) = dirs::home_dir() {
                    let h = home.display().to_string();
                    if let Some(rest) = s.strip_prefix(&h) {
                        return format!("~{rest}");
                    }
                }
                s
            }
            None => "unsaved draft".to_string(),
        }
    }

    pub fn indent_unit(&self) -> String {
        if self.indent_tabs { "\t".to_string() } else { " ".repeat(self.indent_width) }
    }

    /// Dirty in the sense the user cares about: would closing lose text?
    pub fn is_dirty(&self) -> bool {
        match self.path {
            Some(_) => self.buf.is_dirty() || self.missing,
            None => self.buf.len() > 0,
        }
    }

    /// Buffer contents with the file's line endings and BOM restored.
    pub fn contents(&self) -> String {
        let mut s = self.buf.text();
        if self.crlf {
            s = s.replace('\n', "\r\n");
        }
        if self.bom {
            s.insert(0, '\u{feff}');
        }
        s
    }

    pub fn save(&mut self) -> Result<(), String> {
        let Some(path) = self.path.clone() else { return Err("no file name".into()) };
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
            }
        }
        write_file(&path, self.contents().as_bytes()).map_err(|e| format!("{}: {e}", path.display()))?;
        self.buf.mark_saved();
        self.disk = DiskStamp::of(&path);
        self.missing = false;
        Ok(())
    }

    pub fn save_as(&mut self, path: &Path) -> Result<(), String> {
        let old = self.path.replace(path.to_path_buf());
        if let Err(e) = self.save() {
            self.path = old;
            return Err(e);
        }
        if !self.lang_forced {
            let lang = detect_lang(Some(path), &self.buf.line_text(0));
            if lang != self.hl.lang() {
                self.hl.set_lang(&lang);
            }
        }
        Ok(())
    }

    pub fn disk_state(&self) -> DiskState {
        let Some(path) = &self.path else { return DiskState::Same };
        match DiskStamp::of(path) {
            None => DiskState::Missing,
            Some(now) => {
                if Some(now) == self.disk { DiskState::Same } else { DiskState::Changed }
            }
        }
    }

    /// Replace the buffer with the file's current contents as one undoable
    /// step, so a reload that clobbers unsaved edits can still be undone.
    pub fn reload(&mut self, sel: Sel) -> Result<Sel, String> {
        let Some(path) = self.path.clone() else { return Err("no file".into()) };
        let raw = Self::read_file(&path)?;
        let (text, crlf, bom) = normalize(&raw);
        self.crlf = crlf;
        self.bom = bom;
        let sel = self.buf.set_text(sel, &text);
        self.buf.mark_saved();
        self.disk = DiskStamp::of(&path);
        self.missing = false;
        Ok(sel)
    }
}

/// Write via a temporary file and rename, so a failed save (disk full)
/// leaves the old contents intact. Falls back to writing in place when the
/// folder itself is not writable.
fn write_file(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = target.with_file_name(format!(".{name}.glint-tmp"));
    match std::fs::write(&tmp, data) {
        Ok(()) => {
            if let Ok(m) = std::fs::metadata(&target) {
                let _ = std::fs::set_permissions(&tmp, m.permissions());
            }
            std::fs::rename(&tmp, &target).inspect_err(|_| {
                let _ = std::fs::remove_file(&tmp);
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            let _ = std::fs::remove_file(&tmp);
            std::fs::write(&target, data)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Strip a BOM and convert CRLF to LF. Returns (text, had_crlf, had_bom).
fn normalize(raw: &str) -> (String, bool, bool) {
    let (raw, bom) = match raw.strip_prefix('\u{feff}') {
        Some(r) => (r, true),
        None => (raw, false),
    };
    if raw.contains("\r\n") { (raw.replace("\r\n", "\n"), true, bom) } else { (raw.to_string(), false, bom) }
}
