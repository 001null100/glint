//! Themes: a UI palette, syntax colours and an animated background spec.
//! Built-in themes are embedded TOML; user themes live in
//! `~/.config/glint/themes/*.toml` and use the same format.

use crate::util::Rgba;
use serde::Deserialize;
use std::path::Path;

/// Generates a palette struct, its TOML mirror (all fields optional) and a
/// per-field lerp used for theme cross-fades.
macro_rules! palette {
    ($name:ident, $raw:ident { $($f:ident = $default:expr),* $(,)? }) => {
        #[derive(Clone, Debug)]
        pub struct $name { $(pub $f: Rgba),* }

        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct $raw { $($f: Option<String>),* }

        impl $name {
            fn from_raw(r: &$raw) -> Self {
                Self { $($f: r.$f.as_deref().and_then(Rgba::hex)
                    .unwrap_or_else(|| Rgba::hex($default).unwrap())),* }
            }
            pub fn lerp(a: &Self, b: &Self, t: f32) -> Self {
                Self { $($f: a.$f.lerp(b.$f, t)),* }
            }
        }
    };
}

palette!(UiColors, RawUi {
    // Base colour the background shader builds on.
    bg = "#05070c",
    // Tab strip / status bar tint (alpha = how much of the frosted backdrop is covered).
    panel = "#0a0e16cc",
    // Veil behind editor text.
    editor = "#05070cb0",
    // Floating popups (palette, find bar, dialogs).
    popup = "#0c111cee",
    text = "#e8f0ff",
    text_dim = "#93a1b8",
    text_faint = "#56627a",
    accent = "#5ef0c0",
    accent2 = "#9d7bff",
    selection = "#5ef0c038",
    line = "#ffffff0a",
    cursor = "#ffffff",
    border = "#ffffff14",
    find = "#ffd27a40",
    find_current = "#ffd27aa0",
    good = "#6ff0a0",
    warn = "#ffcf6a",
    bad = "#ff6a7a",
});

palette!(SyntaxColors, RawSyntax {
    text = "#e8f0ff",
    comment = "#5b6b85",
    string = "#8ff0b0",
    escape = "#ffb86b",
    number = "#ffb86b",
    constant = "#ff9bd0",
    keyword = "#c08bff",
    storage = "#b79bff",
    operator = "#a9bdd6",
    function = "#6fd3ff",
    r#type = "#5ef0c0",
    variable = "#e8f0ff",
    parameter = "#ffd9a0",
    property = "#9fd0ff",
    punctuation = "#8c9ab3",
    tag = "#ff8fa8",
    attribute = "#ffd27a",
    namespace = "#9fe8d8",
    r#macro = "#ffcf6a",
    heading = "#6fd3ff",
    link = "#6fd3ff",
    inserted = "#6ff0a0",
    deleted = "#ff6a7a",
    invalid = "#ff4d6a",
    // finer classes; when a file leaves these out they follow a broader one
    control = "#ffffff",
    call = "#ffffff",
    builtin = "#ffffff",
    language = "#ffffff",
    this = "#ffffff",
    modifier = "#ffffff",
    label = "#ffffff",
    bracket = "#ffffff",
    doc_comment = "#ffffff",
    regex = "#ffffff",
    quote = "#ffffff",
    code = "#ffffff",
    list_marker = "#ffffff",
});

/// Syntax token classes produced by the highlighter.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum Token {
    #[allow(dead_code)]
    Text,
    Comment,
    String,
    Escape,
    Number,
    Constant,
    Keyword,
    Storage,
    Operator,
    Function,
    Type,
    Variable,
    Parameter,
    Property,
    Punctuation,
    Tag,
    Attribute,
    Namespace,
    Macro,
    Heading,
    Link,
    Emphasis,
    Strong,
    Inserted,
    Deleted,
    Invalid,
    Control,
    Call,
    Builtin,
    Language,
    This,
    Modifier,
    Label,
    Bracket,
    DocComment,
    Regex,
    Quote,
    Code,
    ListMarker,
}

impl SyntaxColors {
    pub fn color(&self, t: Token) -> Rgba {
        match t {
            Token::Text | Token::Emphasis | Token::Strong => self.text,
            Token::Comment => self.comment,
            Token::String => self.string,
            Token::Escape => self.escape,
            Token::Number => self.number,
            Token::Constant => self.constant,
            Token::Keyword => self.keyword,
            Token::Storage => self.storage,
            Token::Operator => self.operator,
            Token::Function => self.function,
            Token::Type => self.r#type,
            Token::Variable => self.variable,
            Token::Parameter => self.parameter,
            Token::Property => self.property,
            Token::Punctuation => self.punctuation,
            Token::Tag => self.tag,
            Token::Attribute => self.attribute,
            Token::Namespace => self.namespace,
            Token::Macro => self.r#macro,
            Token::Heading => self.heading,
            Token::Link => self.link,
            Token::Inserted => self.inserted,
            Token::Deleted => self.deleted,
            Token::Invalid => self.invalid,
            Token::Control => self.control,
            Token::Call => self.call,
            Token::Builtin => self.builtin,
            Token::Language => self.language,
            Token::This => self.this,
            Token::Modifier => self.modifier,
            Token::Label => self.label,
            Token::Bracket => self.bracket,
            Token::DocComment => self.doc_comment,
            Token::Regex => self.regex,
            Token::Quote => self.quote,
            Token::Code => self.code,
            Token::ListMarker => self.list_marker,
        }
    }
}

impl Token {
    pub fn italic(self) -> bool {
        matches!(self, Token::Comment | Token::DocComment | Token::Emphasis | Token::Quote | Token::This)
    }
    pub fn bold(self) -> bool {
        matches!(self, Token::Strong | Token::Heading)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum BgKind {
    Plain,
    Aurora,
    Starfield,
    Nebula,
    Embers,
    Grid,
    Rain,
    Abyss,
    Contour,
}

/// A tunable knob of a background: (name, default). All knobs run 0..1.
pub type Knob = (&'static str, f32);

impl BgKind {
    pub fn from_name(s: &str) -> Option<BgKind> {
        Some(match s.to_ascii_lowercase().as_str() {
            "plain" | "none" => BgKind::Plain,
            "aurora" => BgKind::Aurora,
            "starfield" | "stars" => BgKind::Starfield,
            "nebula" => BgKind::Nebula,
            "embers" => BgKind::Embers,
            "grid" => BgKind::Grid,
            "rain" => BgKind::Rain,
            "abyss" | "waves" => BgKind::Abyss,
            "contour" => BgKind::Contour,
            _ => return None,
        })
    }
    pub fn id(self) -> u32 {
        self as u32
    }
    /// The four scene-specific knobs.
    pub fn knobs(self) -> [Knob; 4] {
        match self {
            BgKind::Plain => [("Glow", 0.5), ("Grain", 0.3), ("-", 0.5), ("-", 0.5)],
            BgKind::Aurora => [("Curtains", 0.6), ("Rays", 0.5), ("Height", 0.55), ("Stars", 0.5)],
            BgKind::Starfield => [("Density", 0.6), ("Twinkle", 0.6), ("Nebula", 0.5), ("Shooting stars", 0.5)],
            BgKind::Nebula => [("Warp", 0.6), ("Contrast", 0.5), ("Stars", 0.4), ("Detail", 0.6)],
            BgKind::Embers => [("Density", 0.5), ("Sway", 0.5), ("Floor glow", 0.6), ("Size", 0.5)],
            BgKind::Grid => [("Horizon", 0.42), ("Sun", 0.5), ("Stars", 0.5), ("Haze", 0.5)],
            BgKind::Rain => [("Density", 0.7), ("Trail", 0.5), ("Flicker", 0.5), ("Head glow", 0.6)],
            BgKind::Abyss => [("Light rays", 0.6), ("Caustics", 0.5), ("Particles", 0.5), ("Depth", 0.5)],
            BgKind::Contour => [("Lines", 0.5), ("Thickness", 0.4), ("Drift", 0.5), ("Shimmer", 0.5)],
        }
    }
    /// Scenes with fine detail are rendered at a higher internal resolution.
    pub fn render_scale(self) -> f32 {
        match self {
            BgKind::Rain | BgKind::Grid | BgKind::Contour => 1.0,
            BgKind::Starfield | BgKind::Embers => 0.75,
            _ => 0.5,
        }
    }
}

#[derive(Clone, Debug)]
pub struct BgSpec {
    pub kind: BgKind,
    pub intensity: f32,
    pub speed: f32,
    pub scale: f32,
    pub knobs: [f32; 4],
    pub colors: [Rgba; 4],
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawBg {
    kind: Option<String>,
    intensity: Option<f32>,
    speed: Option<f32>,
    scale: Option<f32>,
    colors: Vec<String>,
    /// Scene knobs by (case-insensitive) name, e.g. `rays = 0.7`.
    knobs: std::collections::BTreeMap<String, f32>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawTheme {
    name: Option<String>,
    ui: RawUi,
    syntax: RawSyntax,
    background: RawBg,
}

#[derive(Clone, Debug)]
pub struct Theme {
    /// Stable id (file stem), used in config and overrides.
    pub id: String,
    pub name: String,
    pub ui: UiColors,
    pub syntax: SyntaxColors,
    pub bg: BgSpec,
}

impl SyntaxColors {
    /// Build from a `[syntax]` table. Anything left out follows a broader
    /// class (`control` follows `keyword`, `call` follows `function`, …).
    fn resolve(r: &RawSyntax, text: Option<Rgba>) -> SyntaxColors {
        let mut c = SyntaxColors::from_raw(r);
        if let (None, Some(t)) = (&r.text, text) {
            c.text = t;
        }
        if r.variable.is_none() {
            c.variable = c.text;
        }
        if r.control.is_none() {
            c.control = c.keyword;
        }
        if r.call.is_none() {
            c.call = c.function;
        }
        if r.builtin.is_none() {
            c.builtin = c.r#type;
        }
        if r.language.is_none() {
            c.language = c.constant;
        }
        if r.this.is_none() {
            c.this = c.constant;
        }
        if r.modifier.is_none() {
            c.modifier = c.storage;
        }
        if r.label.is_none() {
            c.label = c.constant;
        }
        if r.bracket.is_none() {
            c.bracket = c.punctuation;
        }
        if r.doc_comment.is_none() {
            c.doc_comment = c.comment;
        }
        if r.regex.is_none() {
            c.regex = c.string;
        }
        if r.quote.is_none() {
            c.quote = c.comment;
        }
        if r.code.is_none() {
            c.code = c.string;
        }
        if r.list_marker.is_none() {
            c.list_marker = c.operator;
        }
        c
    }
}

/// A set of syntax colours that can be picked independently of the theme.
#[derive(Clone, Debug)]
pub struct SyntaxTheme {
    pub id: String,
    pub name: String,
    pub colors: SyntaxColors,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawSyntaxTheme {
    name: Option<String>,
    syntax: RawSyntax,
}

impl SyntaxTheme {
    pub fn parse(id: &str, src: &str) -> Result<SyntaxTheme, String> {
        let raw: RawSyntaxTheme = toml::from_str(src).map_err(|e| e.to_string())?;
        Ok(SyntaxTheme { id: id.to_string(), name: raw.name.unwrap_or_else(|| id.to_string()), colors: SyntaxColors::resolve(&raw.syntax, None) })
    }
}

const BUILTIN_SYNTAX: &[(&str, &str)] = &[
    ("cyberpunk", include_str!("../assets/syntax/cyberpunk.toml")),
    ("outrun", include_str!("../assets/syntax/outrun.toml")),
    ("candy", include_str!("../assets/syntax/candy.toml")),
    ("ice", include_str!("../assets/syntax/ice.toml")),
];

/// Built-in syntax palettes, then the user's (`~/.config/glint/syntax/*.toml`).
pub fn load_syntax(user_dir: Option<&Path>) -> (Vec<SyntaxTheme>, Vec<String>) {
    let mut out: Vec<SyntaxTheme> = Vec::new();
    let mut errors = Vec::new();
    for (id, src) in BUILTIN_SYNTAX {
        match SyntaxTheme::parse(id, src) {
            Ok(t) => out.push(t),
            Err(e) => errors.push(format!("built-in syntax palette {id}: {e}")),
        }
    }
    if let Some(rd) = user_dir.and_then(|d| std::fs::read_dir(d).ok()) {
        let mut paths: Vec<_> = rd.flatten().map(|e| e.path()).collect();
        paths.sort();
        for p in paths {
            if p.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let Some(id) = p.file_stem().and_then(|s| s.to_str()) else { continue };
            match std::fs::read_to_string(&p).map_err(|e| e.to_string()).and_then(|s| SyntaxTheme::parse(id, &s)) {
                Ok(t) => match out.iter_mut().find(|x| x.id == t.id) {
                    Some(existing) => *existing = t,
                    None => out.push(t),
                },
                Err(e) => errors.push(format!("syntax palette {}: {e}", p.display())),
            }
        }
    }
    (out, errors)
}

pub const EXAMPLE_SYNTAX: &str = include_str!("../assets/syntax/cyberpunk.toml");

pub fn knob_key(name: &str) -> String {
    name.to_ascii_lowercase().replace(' ', "_")
}

impl Theme {
    pub fn parse(id: &str, src: &str) -> Result<Theme, String> {
        let raw: RawTheme = toml::from_str(src).map_err(|e| e.to_string())?;
        let ui = UiColors::from_raw(&raw.ui);
        let syntax = SyntaxColors::resolve(&raw.syntax, Some(ui.text));
        let kind = raw
            .background
            .kind
            .as_deref()
            .map(|k| BgKind::from_name(k).ok_or_else(|| format!("unknown background kind '{k}'")))
            .transpose()?
            .unwrap_or(BgKind::Plain);
        let defaults = kind.knobs();
        let mut knobs = [0.0; 4];
        for i in 0..4 {
            knobs[i] = raw
                .background
                .knobs
                .get(&knob_key(defaults[i].0))
                .copied()
                .unwrap_or(defaults[i].1)
                .clamp(0.0, 1.0);
        }
        let fallback = [ui.accent, ui.accent2, ui.text_dim, ui.bg];
        let mut colors = fallback;
        for (i, c) in raw.background.colors.iter().take(4).enumerate() {
            colors[i] = Rgba::hex(c).ok_or_else(|| format!("bad background colour '{c}'"))?;
        }
        Ok(Theme {
            id: id.to_string(),
            name: raw.name.unwrap_or_else(|| id.to_string()),
            ui,
            syntax,
            bg: BgSpec {
                kind,
                intensity: raw.background.intensity.unwrap_or(1.0),
                speed: raw.background.speed.unwrap_or(1.0),
                scale: raw.background.scale.unwrap_or(1.0),
                knobs,
                colors,
            },
        })
    }

}

const BUILTIN: &[(&str, &str)] = &[
    ("aurora", include_str!("../assets/themes/aurora.toml")),
    ("deep-space", include_str!("../assets/themes/deep-space.toml")),
    ("nebula", include_str!("../assets/themes/nebula.toml")),
    ("ember", include_str!("../assets/themes/ember.toml")),
    ("synthwave", include_str!("../assets/themes/synthwave.toml")),
    ("cascade", include_str!("../assets/themes/cascade.toml")),
    ("abyss", include_str!("../assets/themes/abyss.toml")),
    ("void", include_str!("../assets/themes/void.toml")),
];

/// Load built-in themes, then user themes (which may replace built-ins by id).
/// Returns the themes and any load errors worth showing.
pub fn load_all(user_dir: Option<&Path>) -> (Vec<Theme>, Vec<String>) {
    let mut themes: Vec<Theme> = Vec::new();
    let mut errors = Vec::new();
    for (id, src) in BUILTIN {
        match Theme::parse(id, src) {
            Ok(t) => themes.push(t),
            Err(e) => errors.push(format!("built-in theme {id}: {e}")),
        }
    }
    if let Some(dir) = user_dir {
        if let Ok(rd) = std::fs::read_dir(dir) {
            let mut paths: Vec<_> = rd.flatten().map(|e| e.path()).collect();
            paths.sort();
            for p in paths {
                if p.extension().and_then(|e| e.to_str()) != Some("toml") {
                    continue;
                }
                let Some(id) = p.file_stem().and_then(|s| s.to_str()) else { continue };
                match std::fs::read_to_string(&p).map_err(|e| e.to_string()).and_then(|s| Theme::parse(id, &s)) {
                    Ok(t) => {
                        if let Some(existing) = themes.iter_mut().find(|x| x.id == t.id) {
                            *existing = t;
                        } else {
                            themes.push(t);
                        }
                    }
                    Err(e) => errors.push(format!("theme {}: {e}", p.display())),
                }
            }
        }
    }
    (themes, errors)
}

/// The example written to the user themes folder on first run.
pub const EXAMPLE_THEME: &str = include_str!("../assets/themes/aurora.toml");
