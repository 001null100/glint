//! User settings (`~/.config/glint/config.toml`) and the app's directories.

use crate::theme::{knob_key, BgSpec, Theme};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Config and data roots. `GLINT_HOME` relocates both (used for testing so a
/// trial run never touches the real session).
pub fn config_dir() -> PathBuf {
    if let Some(h) = std::env::var_os("GLINT_HOME") {
        return PathBuf::from(h).join("config");
    }
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("glint")
}

pub fn data_dir() -> PathBuf {
    if let Some(h) = std::env::var_os("GLINT_HOME") {
        return PathBuf::from(h).join("data");
    }
    dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("glint")
}

pub fn runtime_dir() -> PathBuf {
    if let Some(h) = std::env::var_os("GLINT_HOME") {
        return PathBuf::from(h);
    }
    dirs::runtime_dir().unwrap_or_else(std::env::temp_dir)
}

/// Per-theme background tweaks made in the settings panel.
#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq)]
#[serde(default)]
pub struct BgOverride {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intensity: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scale: Option<f32>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub knobs: BTreeMap<String, f32>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    pub theme: String,
    /// Editor font size in logical pixels.
    pub font_size: f32,
    /// Line height as a multiple of the font size.
    pub line_height: f32,
    pub ligatures: bool,
    pub wrap: bool,
    pub tab_width: usize,
    pub cursor_blink: bool,
    pub animations: bool,

    pub bg_animate: bool,
    pub bg_fps: u32,
    /// Multiplier on each background's internal resolution (0.25..1).
    pub bg_quality: f32,
    pub bg_pause_unfocused: bool,
    /// How strongly the editor dims the background behind text (0..1.5).
    pub dim: f32,
    pub vignette: f32,

    /// Background overrides keyed by theme id.
    pub bg: BTreeMap<String, BgOverride>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            theme: "aurora".into(),
            font_size: 14.0,
            line_height: 1.55,
            ligatures: false,
            wrap: true,
            tab_width: 4,
            cursor_blink: true,
            animations: true,
            bg_animate: true,
            bg_fps: 30,
            bg_quality: 1.0,
            bg_pause_unfocused: true,
            dim: 1.0,
            vignette: 0.35,
            bg: BTreeMap::new(),
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        config_dir().join("config.toml")
    }

    pub fn load() -> (Config, Option<String>) {
        match std::fs::read_to_string(Self::path()) {
            Ok(s) => match toml::from_str::<Config>(&s) {
                Ok(mut c) => {
                    c.sanitize();
                    (c, None)
                }
                Err(e) => (Config::default(), Some(format!("config.toml: {e}"))),
            },
            Err(_) => (Config::default(), None),
        }
    }

    pub fn sanitize(&mut self) {
        self.font_size = self.font_size.clamp(7.0, 48.0);
        self.line_height = self.line_height.clamp(1.0, 2.5);
        self.tab_width = self.tab_width.clamp(1, 16);
        self.bg_fps = self.bg_fps.clamp(5, 144);
        self.bg_quality = self.bg_quality.clamp(0.25, 1.0);
        self.dim = self.dim.clamp(0.0, 1.5);
        self.vignette = self.vignette.clamp(0.0, 1.0);
    }

    pub fn save(&self) -> std::io::Result<()> {
        let dir = config_dir();
        std::fs::create_dir_all(&dir)?;
        let s = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        let tmp = dir.join("config.toml.tmp");
        std::fs::write(&tmp, s)?;
        std::fs::rename(tmp, Self::path())
    }

    /// A theme's background with this user's overrides applied.
    pub fn bg_for(&self, theme: &Theme) -> BgSpec {
        let mut spec = theme.bg.clone();
        if let Some(o) = self.bg.get(&theme.id) {
            if let Some(v) = o.intensity {
                spec.intensity = v;
            }
            if let Some(v) = o.speed {
                spec.speed = v;
            }
            if let Some(v) = o.scale {
                spec.scale = v;
            }
            let names = spec.kind.knobs();
            for i in 0..4 {
                if let Some(v) = o.knobs.get(&knob_key(names[i].0)) {
                    spec.knobs[i] = v.clamp(0.0, 1.0);
                }
            }
        }
        spec
    }
}
