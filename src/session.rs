//! Session persistence: which tabs are open where, plus unsaved drafts.
//!
//! `session.json` records the layout; `drafts/<id>.txt` holds the text of
//! every document with unsaved changes. Together they give hot exit:
//! closing the window never asks anything and everything comes back.

use crate::config::data_dir;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SDoc {
    pub id: u64,
    pub path: Option<PathBuf>,
    #[serde(default)]
    pub untitled_n: u32,
    /// A draft file exists for this document.
    #[serde(default)]
    pub draft: bool,
    /// File mtime (secs, nanos since the epoch) and length when the buffer
    /// last matched the disk. A draft is only trusted if these still match.
    #[serde(default)]
    pub stamp: Option<(u64, u32, u64)>,
    /// Language chosen by hand, if any.
    #[serde(default)]
    pub lang: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct STab {
    pub doc: u64,
    pub anchor: usize,
    pub head: usize,
    pub scroll: f32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum SNode {
    Leaf { id: u64, tabs: Vec<STab>, active: usize },
    Split { horizontal: bool, ratio: f32, a: Box<SNode>, b: Box<SNode> },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Session {
    pub version: u32,
    pub docs: Vec<SDoc>,
    pub layout: SNode,
    pub focus: u64,
    #[serde(default)]
    pub next_untitled: u32,
    #[serde(default)]
    pub recent: Vec<PathBuf>,
    #[serde(default)]
    pub closed: Vec<PathBuf>,
}

pub fn stamp_of(t: SystemTime, len: u64) -> (u64, u32, u64) {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    (d.as_secs(), d.subsec_nanos(), len)
}

pub fn session_path() -> PathBuf {
    data_dir().join("session.json")
}

pub fn draft_path(id: u64) -> PathBuf {
    data_dir().join("drafts").join(format!("{id}.txt"))
}

fn write_atomic(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(tmp, path)
}

pub fn load() -> Option<Session> {
    let s = std::fs::read_to_string(session_path()).ok()?;
    match serde_json::from_str::<Session>(&s) {
        Ok(s) if s.version == 1 => Some(s),
        Ok(_) => None,
        Err(e) => {
            log::warn!("ignoring unreadable session: {e}");
            // keep the broken file around rather than silently losing it
            let _ = std::fs::rename(session_path(), session_path().with_extension("json.bad"));
            None
        }
    }
}

pub fn save(s: &Session) -> std::io::Result<()> {
    let data = serde_json::to_vec_pretty(s).map_err(std::io::Error::other)?;
    write_atomic(&session_path(), &data)
}

pub fn write_draft(id: u64, text: &str) -> std::io::Result<()> {
    write_atomic(&draft_path(id), text.as_bytes())
}

pub fn read_draft(id: u64) -> Option<String> {
    std::fs::read_to_string(draft_path(id)).ok()
}

pub fn remove_draft(id: u64) {
    let _ = std::fs::remove_file(draft_path(id));
}

/// Ids of every draft file on disk. Drafts are never deleted wholesale: a
/// draft without a session entry (crash, unreadable session) is recovered
/// as an untitled tab instead.
pub fn list_drafts() -> Vec<u64> {
    let Ok(rd) = std::fs::read_dir(data_dir().join("drafts")) else { return Vec::new() };
    let mut ids: Vec<u64> = rd
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("txt"))
        .filter_map(|e| e.path().file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse::<u64>().ok()))
        .collect();
    ids.sort();
    ids
}

pub fn stamp_to_time(s: (u64, u32, u64)) -> (SystemTime, u64) {
    (UNIX_EPOCH + Duration::new(s.0, s.1), s.2)
}
