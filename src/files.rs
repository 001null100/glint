//! The file finder behind Ctrl+P.
//!
//! One query box searches, in order of trust: open tabs, recent files, the
//! current file's neighbours, folders files were opened from before, and a
//! background index of the home directory. The query language:
//!
//! - `app` — fuzzy match on file names;
//! - `glint/ app` or `glint/app` — a folder whose path contains "glint",
//!   then a file matching "app" somewhere below it;
//! - `~/`, `/`, `./`, `../` — browse that folder directly.
//!
//! What gets picked for a query is remembered and ranked first next time.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const INDEX_CAP: usize = 400_000;
/// No single top-level folder may fill more than this much of the index.
const PER_TOP_CAP: usize = 120_000;
const MAX_HITS: usize = 200;
const REFRESH_AFTER_SECS: u64 = 300;

/// Folders that are never worth walking into.
const SKIP_DIRS: &[&str] = &[
    "node_modules", "target", "__pycache__", "site-packages", "drive_c", "dosdevices", "steamapps", "pfx", "Intermediate",
    "DerivedDataCache", "Binaries", "venv", "dist-newstyle", "CMakeFiles",
];

/// Extensions of files a text editor has no use for.
const SKIP_EXT: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "tga", "dds", "exr", "hdr", "psd", "kra", "xcf", "tif", "tiff", "heic", "avif",
    "mp3", "wav", "flac", "ogg", "opus", "m4a", "aac", "mid", "mp4", "mkv", "webm", "avi", "mov", "wmv", "flv", "zip", "tar", "gz", "xz",
    "zst", "bz2", "7z", "rar", "iso", "img", "deb", "rpm", "appimage", "jar", "apk", "exe", "dll", "so", "o", "a", "lib", "bin", "dat",
    "pak", "pck", "class", "pyc", "pyo", "rlib", "rmeta", "wasm", "pdb", "ttf", "otf", "woff", "woff2", "pdf", "doc", "docx", "xls",
    "xlsx", "ppt", "pptx", "odt", "ods", "blend", "blend1", "fbx", "glb", "uasset", "umap", "ubulk", "uexp", "db", "sqlite", "sqlite3",
    "vst3", "clap", "dylib", "node", "cache", "safetensors", "gguf", "onnx", "pt", "pth", "ckpt", "npy", "npz", "parquet", "bk2", "ktx",
    "ktx2", "swp", "bnk", "wem", "nib", "dmg", "msi", "cab", "sys", "icns", "ani", "cur",
];

fn skip_file(name: &str) -> bool {
    match name.rsplit_once('.') {
        Some((_, ext)) => SKIP_EXT.iter().any(|e| ext.eq_ignore_ascii_case(e)),
        None => false,
    }
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// ------------------------------------------------------------------ index

/// File paths under a root folder, gathered on a background thread.
pub struct Index {
    root: PathBuf,
    /// Paths relative to `root`.
    files: Mutex<Vec<Box<str>>>,
    loading: AtomicBool,
    /// Bumped whenever `files` changes.
    generation: AtomicUsize,
    finished: Mutex<Option<Instant>>,
}

impl Index {
    pub fn start(root: PathBuf) -> Arc<Index> {
        let ix = Arc::new(Index {
            root,
            files: Mutex::new(Vec::new()),
            loading: AtomicBool::new(false),
            generation: AtomicUsize::new(0),
            finished: Mutex::new(None),
        });
        ix.spawn(false);
        ix
    }

    /// Walk again if the last walk is old. The current list stays usable
    /// until the new one is complete.
    pub fn refresh(self: &Arc<Self>) {
        let old = self.finished.lock().unwrap().is_some_and(|t| t.elapsed().as_secs() > REFRESH_AFTER_SECS);
        if old {
            self.spawn(true);
        }
    }

    fn spawn(self: &Arc<Self>, replace: bool) {
        if self.loading.swap(true, Ordering::SeqCst) {
            return;
        }
        let ix = self.clone();
        let _ = std::thread::Builder::new().name("glint-index".into()).spawn(move || {
            let t0 = Instant::now();
            ix.walk(replace);
            log::info!("file index: {} files in {:.2}s", ix.files.lock().unwrap().len(), t0.elapsed().as_secs_f32());
            *ix.finished.lock().unwrap() = Some(Instant::now());
            ix.loading.store(false, Ordering::SeqCst);
            ix.generation.fetch_add(1, Ordering::SeqCst);
        });
    }

    fn walk(&self, replace: bool) {
        let mut all: Vec<Box<str>> = Vec::new();
        let mut batch: Vec<Box<str>> = Vec::new();
        let mut total = 0usize;
        let mut tops: Vec<PathBuf> = match std::fs::read_dir(&self.root) {
            Ok(rd) => rd.flatten().map(|e| e.path()).collect(),
            Err(_) => return,
        };
        tops.sort();
        let remote = remote_mounts(&self.root);
        let flush = |batch: &mut Vec<Box<str>>, all: &mut Vec<Box<str>>| {
            if replace {
                all.append(batch);
            } else {
                self.files.lock().unwrap().append(batch);
                self.generation.fetch_add(1, Ordering::SeqCst);
            }
        };
        for top in tops {
            let name = top.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            // hidden folders stay out, except the one that holds config files
            let config = name == ".config";
            if SKIP_DIRS.contains(&name.as_str()) || (name.starts_with('.') && !config && !top.is_file()) {
                continue;
            }
            let cap = if config { PER_TOP_CAP / 4 } else { PER_TOP_CAP };
            let mut here = 0usize;
            let walker = ignore::WalkBuilder::new(&top)
                .hidden(!config)
                .follow_links(false)
                .max_depth(Some(14))
                .filter_entry({
                    let remote = remote.clone();
                    move |e| !e.file_name().to_str().is_some_and(|n| SKIP_DIRS.contains(&n)) && !remote.iter().any(|m| m == e.path())
                })
                .build();
            for entry in walker.flatten() {
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    continue;
                }
                let Some(name) = entry.file_name().to_str() else { continue };
                if skip_file(name) {
                    continue;
                }
                let Ok(rel) = entry.path().strip_prefix(&self.root) else { continue };
                let Some(rel) = rel.to_str() else { continue };
                batch.push(rel.into());
                here += 1;
                total += 1;
                if batch.len() >= 4096 {
                    flush(&mut batch, &mut all);
                }
                if here >= cap || total >= INDEX_CAP {
                    break;
                }
            }
            if total >= INDEX_CAP {
                break;
            }
        }
        flush(&mut batch, &mut all);
        if replace {
            *self.files.lock().unwrap() = all;
        }
    }

    pub fn loading(&self) -> bool {
        self.loading.load(Ordering::SeqCst)
    }

    pub fn generation(&self) -> usize {
        self.generation.load(Ordering::SeqCst)
    }
}

/// Network and FUSE mounts under `root` (a phone, a cloud drive, a share).
/// Walking them is slow enough to hold up everything behind them, so the
/// index leaves them out; browsing into them by path still works.
fn remote_mounts(root: &Path) -> Vec<PathBuf> {
    let Ok(text) = std::fs::read_to_string("/proc/mounts") else { return Vec::new() };
    parse_remote_mounts(&text, root)
}

fn parse_remote_mounts(text: &str, root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut f = line.split(' ');
        let (Some(_), Some(at), Some(kind)) = (f.next(), f.next(), f.next()) else { continue };
        let remote = kind == "fuse" || kind.starts_with("fuse.") || matches!(kind, "nfs" | "nfs4" | "cifs" | "smb3" | "smbfs" | "sshfs" | "9p" | "davfs" | "afs" | "ceph");
        if !remote {
            continue;
        }
        let at = PathBuf::from(unescape_mount(at));
        if at.starts_with(root) && at != root {
            out.push(at);
        }
    }
    out
}

/// /proc/mounts writes spaces and friends as octal escapes (`\040`).
fn unescape_mount(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1..i + 4].iter().all(|d| (b'0'..=b'7').contains(d)) {
            let v = b[i + 1..i + 4].iter().fold(0u32, |a, d| a * 8 + (d - b'0') as u32);
            out.push(v as u8);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ------------------------------------------------------------------ store

/// What the finder has learned: folders files were opened from, and which
/// file was picked for which query.
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Store {
    /// Folder -> (times a file in it was opened, unix time of the last one).
    dirs: HashMap<String, (u32, u64)>,
    /// (query, path), newest last.
    picks: Vec<(String, String)>,
    #[serde(skip)]
    pub dirty: bool,
}

impl Store {
    pub fn load(path: &Path) -> Store {
        std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    pub fn save(&mut self, path: &Path) {
        self.dirty = false;
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(s) = serde_json::to_string(self) {
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, s).is_ok() {
                let _ = std::fs::rename(tmp, path);
            }
        }
    }

    /// A file was opened: its folder becomes more familiar.
    pub fn visit(&mut self, file: &Path) {
        let Some(dir) = file.parent().and_then(|d| d.to_str()) else { return };
        let e = self.dirs.entry(dir.to_string()).or_insert((0, 0));
        e.0 = e.0.saturating_add(1);
        e.1 = unix_now();
        if self.dirs.len() > 300 {
            // forget the folder that has gone unused the longest
            if let Some(k) = self.dirs.iter().min_by_key(|(_, v)| v.1).map(|(k, _)| k.clone()) {
                self.dirs.remove(&k);
            }
        }
        self.dirty = true;
    }

    /// `path` was chosen for `query`.
    pub fn learn(&mut self, query: &str, path: &Path) {
        let q = query.trim().to_lowercase();
        let Some(p) = path.to_str() else { return };
        if q.is_empty() || is_path_query(&q) {
            return;
        }
        self.picks.retain(|(k, _)| *k != q);
        self.picks.push((q, p.to_string()));
        if self.picks.len() > 400 {
            self.picks.remove(0);
        }
        self.dirty = true;
    }

    /// Familiar folders with a weight: used often and recently scores high.
    fn top_dirs(&self, n: usize) -> Vec<(PathBuf, i32)> {
        let now = unix_now();
        let mut v: Vec<(PathBuf, i32)> = self
            .dirs
            .iter()
            .map(|(d, (count, last))| {
                let days = now.saturating_sub(*last) as f32 / 86_400.0;
                let w = (*count as f32).min(40.0) * 30.0 / (1.0 + days / 14.0);
                (PathBuf::from(d), w as i32)
            })
            .collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        v.truncate(n);
        v
    }
}

// ----------------------------------------------------------------- finder

#[derive(Clone, Debug, PartialEq)]
pub enum HitKind {
    Tab { pane: u64, idx: usize },
    File,
    /// A folder: choosing it browses into it.
    Dir,
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub path: PathBuf,
    pub label: String,
    pub detail: String,
    pub kind: HitKind,
}

/// A file the finder knows about without the index.
struct Known {
    label: String,
    path: Option<PathBuf>,
    tag: &'static str,
    kind: HitKind,
    bonus: i32,
}

pub struct Finder {
    index: Arc<Index>,
    known: Vec<Known>,
    dirs: Vec<(PathBuf, i32)>,
    picks: HashMap<String, String>,
    /// Folder that `./` and `../` are relative to.
    base: PathBuf,
    home: PathBuf,
    seen: usize,
}

/// Does this query name a place rather than search for a name?
pub fn is_path_query(q: &str) -> bool {
    q.starts_with('/') || q.starts_with('~') || q.starts_with("./") || q.starts_with("../") || q == "." || q == ".."
}

fn find_ci(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > hay.len() {
        return None;
    }
    let first = needle[0];
    (0..=hay.len() - needle.len()).find(|&i| hay[i].to_ascii_lowercase() == first && hay[i..i + needle.len()].iter().zip(needle).all(|(a, b)| a.to_ascii_lowercase() == *b))
}

fn subseq_ci(hay: &[u8], needle: &[u8]) -> bool {
    let mut it = hay.iter();
    needle.iter().all(|n| it.any(|h| h.to_ascii_lowercase() == *n))
}

/// A parsed query: name terms and folder terms, all lower-case.
#[derive(Default, Debug, PartialEq)]
struct Terms {
    names: Vec<Vec<u8>>,
    dirs: Vec<Vec<Vec<u8>>>,
}

fn parse(q: &str) -> Terms {
    let mut t = Terms::default();
    for tok in q.split_whitespace() {
        let tok = tok.to_lowercase();
        if tok.contains('/') {
            let mut parts: Vec<Vec<u8>> = tok.split('/').filter(|p| !p.is_empty()).map(|p| p.as_bytes().to_vec()).collect();
            if !tok.ends_with('/') {
                if let Some(name) = parts.pop() {
                    t.names.push(name);
                }
            }
            if !parts.is_empty() {
                t.dirs.push(parts);
            }
        } else {
            t.names.push(tok.into_bytes());
        }
    }
    t
}

fn name_score(name: &[u8], t: &[u8]) -> Option<i32> {
    if let Some(i) = find_ci(name, t) {
        let mut s = 8000 - (i as i32).min(40) * 10 - (name.len() as i32).min(80);
        if i == 0 {
            s += 1500;
            match name.get(t.len()) {
                None => s += 2500,
                Some(b'.') => s += 2000,
                _ => {}
            }
        } else if matches!(name[i - 1], b'-' | b'_' | b'.' | b' ') || (name[i].is_ascii_uppercase() && name[i - 1].is_ascii_lowercase()) {
            s += 800;
        }
        Some(s)
    } else if subseq_ci(name, t) {
        Some(2500 - (name.len() as i32).min(80) * 5)
    } else {
        None
    }
}

/// Score a path (with `/` separators) against the query, or None.
fn score(path: &str, terms: &Terms) -> Option<i32> {
    let bytes = path.as_bytes();
    let cut = path.rfind('/').map(|i| i + 1).unwrap_or(0);
    let (dir, name) = (&bytes[..cut], &bytes[cut..]);
    let mut total = 0;
    for parts in &terms.dirs {
        let mut at = 0;
        for part in parts {
            let i = at + find_ci(&dir[at..], part)?;
            // a part that starts a folder name, or is one, counts for more
            if i == 0 || dir[i - 1] == b'/' {
                total += 150;
                if dir.get(i + part.len()) == Some(&b'/') {
                    total += 250;
                }
            }
            at = i + part.len();
        }
        total += 400;
    }
    for t in &terms.names {
        total += match name_score(name, t) {
            Some(s) => s,
            None => find_ci(dir, t).map(|_| 600)?,
        };
    }
    Some(total - dir.iter().filter(|b| **b == b'/').count() as i32 * 12)
}

impl Finder {
    /// `tabs`: (title, path, pane, index) of every open tab, the focused
    /// pane first. `current`: the file being edited.
    pub fn new(index: Arc<Index>, store: &Store, tabs: Vec<(String, Option<PathBuf>, u64, usize)>, recent: &[PathBuf], current: Option<&Path>) -> Finder {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        let base = current.and_then(|p| p.parent()).map(|p| p.to_path_buf()).unwrap_or_else(|| home.clone());
        let mut known: Vec<Known> = Vec::new();
        let mut seen: HashSet<PathBuf> = HashSet::new();
        let mut folders: Vec<PathBuf> = Vec::new();
        for (title, path, pane, idx) in tabs {
            if let Some(p) = &path {
                seen.insert(p.clone());
                if let Some(d) = p.parent() {
                    if !folders.iter().any(|f| f == d) {
                        folders.push(d.to_path_buf());
                    }
                }
            }
            known.push(Known { label: title, path, tag: "open", kind: HitKind::Tab { pane, idx }, bonus: 5000 });
        }
        let mut add = |known: &mut Vec<Known>, p: PathBuf, tag: &'static str, bonus: i32| {
            if seen.insert(p.clone()) {
                let label = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                known.push(Known { label, path: Some(p), tag, kind: HitKind::File, bonus });
            }
        };
        for r in recent {
            add(&mut known, r.clone(), "recent", 3500);
        }
        // neighbours of the current file first, then of the other open tabs
        if let Some(pos) = folders.iter().position(|f| *f == base) {
            folders.swap(0, pos);
        }
        for (i, f) in folders.iter().enumerate() {
            for p in list_files(f, 1500) {
                add(&mut known, p, "", if i == 0 { 3000 } else { 2000 });
            }
        }
        let dirs = store.top_dirs(40);
        for (d, w) in &dirs {
            if !folders.contains(d) {
                for p in list_files(d, 400) {
                    add(&mut known, p, "", 1000 + *w);
                }
            }
        }
        let picks = store.picks.iter().cloned().collect();
        let seen = index.generation();
        Finder { index, known, dirs, picks, base, home, seen }
    }

    pub fn loading(&self) -> bool {
        self.index.loading()
    }

    /// The index has grown since the last query.
    pub fn stale(&self) -> bool {
        self.index.generation() != self.seen
    }

    fn tilde(&self, p: &Path) -> String {
        match p.strip_prefix(&self.home) {
            Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => p.display().to_string(),
        }
    }

    /// Text for the query box that browses `dir`.
    pub fn browse_text(&self, dir: &Path) -> String {
        let s = self.tilde(dir);
        if s.ends_with('/') { s } else { format!("{s}/") }
    }

    fn detail(&self, path: &Path, tag: &str) -> String {
        let dir = path.parent().map(|d| self.tilde(d)).unwrap_or_default();
        if tag.is_empty() { dir } else { format!("{tag} · {dir}") }
    }

    pub fn query(&mut self, q: &str) -> Vec<Hit> {
        self.seen = self.index.generation();
        let q = q.trim_start();
        let t0 = Instant::now();
        let hits = if is_path_query(q) { self.browse(q) } else { self.search(q) };
        log::debug!("file query {q:?}: {} hits in {:.1}ms", hits.len(), t0.elapsed().as_secs_f32() * 1000.0);
        hits
    }

    /// Split a path query into the folder to list and the partial name.
    fn resolve(&self, q: &str) -> (PathBuf, String) {
        let q = match q {
            "~" => "~/",
            "." => "./",
            ".." => "../",
            _ => q,
        };
        let (dir, partial) = q.rsplit_once('/').map(|(d, p)| (format!("{d}/"), p.to_string())).unwrap_or((q.to_string(), String::new()));
        let path = if let Some(rest) = dir.strip_prefix("~/") {
            self.home.join(rest)
        } else if dir.starts_with('/') {
            PathBuf::from(&dir)
        } else {
            self.base.join(&dir)
        };
        // fold away `.` and `..`
        let mut out = PathBuf::new();
        for c in path.components() {
            match c {
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    out.pop();
                }
                c => out.push(c),
            }
        }
        (out, partial)
    }

    fn browse(&self, q: &str) -> Vec<Hit> {
        let (dir, partial) = self.resolve(q);
        let Ok(rd) = std::fs::read_dir(&dir) else { return Vec::new() };
        let p = partial.to_lowercase().into_bytes();
        let mut rows: Vec<(i32, bool, String, PathBuf)> = Vec::new();
        for e in rd.flatten().take(20_000) {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') && !partial.starts_with('.') {
                continue;
            }
            let is_dir = e.file_type().map(|t| t.is_dir() || (t.is_symlink() && e.path().is_dir())).unwrap_or(false);
            let s = if p.is_empty() {
                0
            } else {
                match name_score(name.as_bytes(), &p) {
                    Some(s) => s,
                    None => continue,
                }
            };
            rows.push((s, is_dir, name, e.path()));
        }
        // best match first; without a partial name: folders, then files, by name
        rows.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then_with(|| a.2.to_lowercase().cmp(&b.2.to_lowercase())));
        let mut hits = Vec::new();
        if p.is_empty() {
            if let Some(up) = dir.parent() {
                hits.push(Hit { path: up.to_path_buf(), label: "../".into(), detail: self.tilde(up), kind: HitKind::Dir });
            }
        }
        let here = self.tilde(&dir);
        for (_, is_dir, name, path) in rows.into_iter().take(MAX_HITS * 3) {
            if is_dir {
                hits.push(Hit { path, label: format!("{name}/"), detail: here.clone(), kind: HitKind::Dir });
            } else {
                hits.push(Hit { path, label: name, detail: here.clone(), kind: HitKind::File });
            }
        }
        hits
    }

    fn search(&self, q: &str) -> Vec<Hit> {
        let terms = parse(q);
        let known_hit = |k: &Known| Hit {
            path: k.path.clone().unwrap_or_default(),
            label: k.label.clone(),
            detail: match &k.path {
                Some(p) => self.detail(p, k.tag),
                None => "unsaved draft".to_string(),
            },
            kind: k.kind.clone(),
        };
        if terms.names.is_empty() && terms.dirs.is_empty() {
            return self.known.iter().take(MAX_HITS).map(known_hit).collect();
        }
        let learned = self.picks.get(&q.trim().to_lowercase());

        enum Src {
            Known(usize),
            Dir(usize),
            Index(usize),
        }
        let mut found: Vec<(i32, Src)> = Vec::new();
        for (i, k) in self.known.iter().enumerate() {
            let s = match &k.path {
                Some(p) => p.to_str().and_then(|p| score(p, &terms)),
                None if terms.dirs.is_empty() => score(&k.label, &terms),
                None => None,
            };
            if let Some(mut s) = s {
                if learned.is_some_and(|l| k.path.as_deref().and_then(|p| p.to_str()) == Some(l.as_str())) {
                    s += 30_000;
                }
                found.push((s + k.bonus, Src::Known(i)));
            }
        }
        // a query made only of folder terms also offers the folders themselves
        if terms.names.is_empty() {
            for (i, (d, w)) in self.dirs.iter().enumerate() {
                if let Some(s) = d.to_str().and_then(|d| score(&format!("{d}/"), &terms)) {
                    found.push((s + 9000 + *w, Src::Dir(i)));
                }
            }
        }
        let known_rel: HashSet<&str> =
            self.known.iter().filter_map(|k| k.path.as_deref()).filter_map(|p| p.strip_prefix(&self.index.root).ok()).filter_map(|p| p.to_str()).collect();
        let files = self.index.files.lock().unwrap();
        let learned_rel = learned.and_then(|l| Path::new(l).strip_prefix(&self.index.root).ok()).and_then(|p| p.to_str());
        // the index is big: score it on a few threads
        let scan = |from: usize, part: &[Box<str>]| -> Vec<(i32, usize)> {
            let mut out = Vec::new();
            for (i, rel) in part.iter().enumerate() {
                if let Some(mut s) = score(rel, &terms) {
                    if known_rel.contains(&**rel) {
                        continue;
                    }
                    if learned_rel == Some(&**rel) {
                        s += 30_000;
                    }
                    out.push((s, from + i));
                }
            }
            out
        };
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).clamp(1, 6);
        let chunk = files.len().div_ceil(threads).max(20_000);
        let parts: Vec<Vec<(i32, usize)>> = std::thread::scope(|sc| {
            let hs: Vec<_> = files.chunks(chunk).enumerate().map(|(k, part)| sc.spawn(move || scan(k * chunk, part))).collect();
            hs.into_iter().map(|h| h.join().unwrap_or_default()).collect()
        });
        for (s, i) in parts.into_iter().flatten() {
            found.push((s, Src::Index(i)));
        }
        if found.len() > MAX_HITS {
            found.select_nth_unstable_by(MAX_HITS, |a, b| b.0.cmp(&a.0));
            found.truncate(MAX_HITS);
        }
        found.sort_by(|a, b| b.0.cmp(&a.0));
        found
            .into_iter()
            .map(|(_, src)| match src {
                Src::Known(i) => known_hit(&self.known[i]),
                Src::Dir(i) => {
                    let d = &self.dirs[i].0;
                    let name = d.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    Hit { path: d.clone(), label: format!("{name}/"), detail: self.detail(d, "folder"), kind: HitKind::Dir }
                }
                Src::Index(i) => {
                    let path = self.index.root.join(&*files[i]);
                    let label = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    let detail = self.detail(&path, "");
                    Hit { path, label, detail, kind: HitKind::File }
                }
            })
            .collect()
    }

    /// The part of the query to highlight in file names.
    pub fn highlight(q: &str) -> String {
        if is_path_query(q.trim_start()) {
            return q.rsplit('/').next().unwrap_or("").to_string();
        }
        parse(q).names.last().map(|n| String::from_utf8_lossy(n).into_owned()).unwrap_or_default()
    }
}

/// Plain files directly inside `dir`, sorted by name.
fn list_files(dir: &Path, cap: usize) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut v: Vec<PathBuf> = rd
        .flatten()
        .take(cap * 4)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter(|e| e.file_name().to_str().is_some_and(|n| !n.starts_with('.') && !skip_file(n)))
        .map(|e| e.path())
        .collect();
    v.sort();
    v.truncate(cap);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_remote_mounts() {
        let text = "/dev/nvme0n1p2 / btrfs rw 0 0\n\
                    phone: /home/u/Orbit/One\\040Plus fuse.rclone rw 0 0\n\
                    nas:/x /home/u/nas nfs4 rw 0 0\n\
                    /dev/sdb1 /home/u/Games ext4 rw 0 0\n\
                    portal /run/user/1000/doc fuse.portal rw 0 0\n";
        let got = parse_remote_mounts(text, Path::new("/home/u"));
        assert_eq!(got, vec![PathBuf::from("/home/u/Orbit/One Plus"), PathBuf::from("/home/u/nas")]);
    }

    #[test]
    fn parses_names_and_folders() {
        assert_eq!(parse("App"), Terms { names: vec![b"app".to_vec()], dirs: vec![] });
        assert_eq!(parse("glint/ app"), Terms { names: vec![b"app".to_vec()], dirs: vec![vec![b"glint".to_vec()]] });
        assert_eq!(parse("glint/src/app"), Terms { names: vec![b"app".to_vec()], dirs: vec![vec![b"glint".to_vec(), b"src".to_vec()]] });
        assert_eq!(parse("src/"), Terms { names: vec![], dirs: vec![vec![b"src".to_vec()]] });
    }

    #[test]
    fn ranks_names_sensibly() {
        let t = parse("app");
        let exact = score("Rust/glint/src/app.rs", &t).unwrap();
        let prefix = score("Rust/glint/src/apply.rs", &t).unwrap();
        let inner = score("Rust/glint/src/wrapper.rs", &t);
        let word = score("Rust/glint/src/my_app.rs", &t).unwrap();
        let scattered = score("Rust/glint/src/a_pane_p.rs", &t).unwrap();
        assert!(exact > prefix && prefix > word && word > scattered);
        assert!(inner.is_none_or(|s| s < word));
        assert_eq!(score("Rust/glint/src/main.rs", &t), None);
    }

    #[test]
    fn folder_terms_narrow_the_search() {
        let t = parse("glint/ app");
        assert!(score("Rust/glint/src/app.rs", &t).is_some());
        assert!(score("Rust/other/src/app.rs", &t).is_none());
        // a whole folder name beats a partial one
        let t = parse("src/ main");
        assert!(score("a/src/main.rs", &t).unwrap() > score("a/src-old/main.rs", &t).unwrap());
        // a name term may also match a folder when no file name does
        assert!(score("Rust/glint/README.md", &parse("glint readme")).is_some());
    }

    #[test]
    fn path_queries() {
        for q in ["/", "~", "~/Rust", "./x", "../", ".."] {
            assert!(is_path_query(q), "{q}");
        }
        for q in ["app", ".gitignore", "src/ app"] {
            assert!(!is_path_query(q), "{q}");
        }
    }

    /// Timing on the real home folder: `cargo test --release -- --ignored home_timing --nocapture`.
    #[test]
    #[ignore]
    fn home_timing() {
        let t0 = Instant::now();
        let ix = Index::start(dirs::home_dir().unwrap());
        while ix.loading() {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let n = ix.files.lock().unwrap().len();
        println!("indexed {n} files in {:.2}s", t0.elapsed().as_secs_f32());
        let mut per_top: HashMap<String, usize> = HashMap::new();
        for f in ix.files.lock().unwrap().iter() {
            *per_top.entry(f.split('/').next().unwrap_or("").to_string()).or_default() += 1;
        }
        let mut tops: Vec<_> = per_top.into_iter().collect();
        tops.sort_by(|a, b| b.1.cmp(&a.1));
        println!("largest: {:?}", &tops[..tops.len().min(12)]);
        let mut f = Finder::new(ix, &Store::default(), vec![], &[], None);
        for q in ["a", "app", "main.rs", "glint/ ov", "rust/ src/ app", "readme", "unreal/ engine/ actor.h", "zzzzqq"] {
            let t = Instant::now();
            let hits = f.query(q);
            println!("{q:?}: {} hits in {:.1}ms  top: {:?}", hits.len(), t.elapsed().as_secs_f32() * 1000.0, hits.iter().take(3).map(|h| h.path.display().to_string()).collect::<Vec<_>>());
        }
    }

    /// Queries while the home index is still filling must stay fast.
    #[test]
    #[ignore]
    fn home_query_while_loading() {
        let ix = Index::start(dirs::home_dir().unwrap());
        let mut f = Finder::new(ix.clone(), &Store::default(), vec![], &[], None);
        let mut worst = 0.0f32;
        let mut n = 0;
        while ix.loading() {
            for q in ["g", "gl", "glint/ over", ""] {
                let t = Instant::now();
                let hits = f.query(q);
                let ms = t.elapsed().as_secs_f32() * 1000.0;
                worst = worst.max(ms);
                n += 1;
                if ms > 100.0 {
                    println!("slow: {q:?} {ms:.0}ms ({} hits)", hits.len());
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        println!("{n} queries while loading, worst {worst:.1}ms");
    }

    #[test]
    fn index_and_finder_on_a_real_tree() {
        let root = std::env::temp_dir().join(format!("glint-files-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for f in ["proj/src/app.rs", "proj/src/main.rs", "proj/notes/todo.md", "other/app.py", "other/pic.png", "proj/node_modules/x/app.js"] {
            let p = root.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "x").unwrap();
        }
        let ix = Index::start(root.clone());
        while ix.loading() {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(ix.files.lock().unwrap().len(), 4);
        let mut store = Store::default();
        let cur = root.join("proj/src/main.rs");
        let mut f = Finder::new(ix.clone(), &store, vec![("main.rs".into(), Some(cur.clone()), 1, 0)], &[], Some(&cur));
        // empty query: the open tab, then its neighbour
        let labels: Vec<String> = f.query("").iter().map(|h| h.label.clone()).collect();
        assert_eq!(labels, ["main.rs", "app.rs"]);
        // the neighbour outranks the same name elsewhere
        let hits = f.query("app");
        assert_eq!(hits[0].path, root.join("proj/src/app.rs"));
        assert_eq!(hits[1].path, root.join("other/app.py"));
        // folder term
        let hits = f.query("other/ app");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, root.join("other/app.py"));
        // browsing relative to the current file
        let labels: Vec<String> = f.query("../").iter().map(|h| h.label.clone()).collect();
        assert_eq!(labels, ["../", "node_modules/", "notes/", "src/"]);
        let labels: Vec<String> = f.query("../notes/t").iter().map(|h| h.label.clone()).collect();
        assert_eq!(labels, ["todo.md"]);
        // a learned pick comes first next time
        store.learn("app", &root.join("other/app.py"));
        let mut f = Finder::new(ix, &store, vec![], &[], Some(&cur));
        assert_eq!(f.query("app")[0].path, root.join("other/app.py"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
