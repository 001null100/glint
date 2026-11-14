//! The boundary between the app core and whatever hosts it (the winit
//! runner, or the headless snapshot mode).

use crate::gfx::BgFrame;
use crate::ui::CursorKind;
use std::path::PathBuf;

pub struct AppOptions {
    /// Use the system clipboard (false in snapshot mode).
    pub system_clipboard: bool,
    /// Restore tabs and drafts from the session file.
    pub restore_session: bool,
    /// Allow writing config.toml (false for throwaway test runs).
    pub write_config: bool,
    /// Files given on the command line.
    pub paths: Vec<PathBuf>,
}

/// Things the app asks its host to do. Drained once per frame.
#[derive(Clone, Debug)]
pub enum Effect {
    Title(String),
    Cursor(CursorKind),
    /// Show a native "open files" dialog; answer with `DialogResult::Open`.
    OpenDialog { dir: Option<PathBuf> },
    /// Show a native "save as" dialog; answer with `DialogResult::Save`
    /// (path None if cancelled).
    SaveDialog { doc: u64, dir: Option<PathBuf>, name: String },
    ToggleFullscreen,
    /// Open a folder in the file manager.
    Reveal(PathBuf),
    Quit,
}

#[derive(Clone, Debug)]
pub enum DialogResult {
    Open(Vec<PathBuf>),
    Save { doc: u64, path: Option<PathBuf> },
}

/// What `App::frame` reports back to the host.
pub struct FrameOut {
    /// The draw list changed since the previous frame.
    pub scene_dirty: bool,
    /// UI animations are running: keep presenting every vsync.
    pub animating: bool,
    /// Absolute time (same clock as `frame`'s `now`) of the next needed
    /// redraw when not animating.
    pub wake: Option<f64>,
    /// Background state for the renderer. `render` is left false; the host
    /// decides when to re-render the background (fps cap).
    pub bg: BgFrame,
    /// The background wants to animate (enabled and not paused).
    pub bg_animating: bool,
    pub bg_fps: u32,
}
