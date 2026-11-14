//! Incremental syntax highlighting on top of syntect.
//!
//! Each line stores its token spans and, every 16 lines or so, the parser
//! state at its start. After an edit only the touched lines are re-parsed;
//! parsing stops as soon as the state at the start of an untouched line
//! matches what was stored there before (the edit did not change anything
//! further down). Work is done lazily and under a time budget so a huge file
//! never blocks a frame.

#![allow(dead_code)]

use crate::theme::Token;
use ropey::Rope;
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};

const PLAIN: &str = "Plain Text";
/// A parser state is kept at every line whose index is a multiple of this.
const STATE_EVERY: usize = 16;
/// Lines longer than this are not parsed (minified files, data blobs).
const MAX_LINE_BYTES: usize = 4000;

pub fn syntax_set() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(two_face::syntax::extra_newlines)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TokSpan {
    /// Byte range within the line text (newline excluded).
    pub start: u32,
    pub end: u32,
    pub tok: Token,
}

#[derive(Clone, PartialEq, Eq)]
struct State {
    parse: ParseState,
    scopes: ScopeStack,
}

impl State {
    fn fresh(syntax: &SyntaxReference) -> State {
        State { parse: ParseState::new(syntax), scopes: ScopeStack::new() }
    }
}

#[derive(Default)]
struct LineHl {
    spans: Vec<TokSpan>,
    /// Parser state at the start of this line (only on some lines).
    state: Option<Box<State>>,
}

pub struct Highlighter {
    lang: String,
    /// `None` for plain text: no spans, no work.
    syntax: Option<&'static SyntaxReference>,
    lines: Vec<LineHl>,
    /// Next line to parse; every line before it is up to date.
    dirty_from: usize,
    /// End (exclusive) of the lines that must be re-parsed unconditionally.
    dirty_to: usize,
    /// Lines in `max(dirty_from, dirty_to)..valid_upto` come from one earlier
    /// consistent pass and stay valid if the parser state converges.
    valid_upto: usize,
    /// Running state for the start of line `.0`, to resume without re-parsing.
    cur: Option<(usize, State)>,
    classify: HashMap<Scope, Option<Token>>,
}

// ------------------------------------------------------------ classification

/// Scope prefix → token class, ordered specific → general.
fn table() -> &'static [(Scope, Token)] {
    static TABLE: OnceLock<Vec<(Scope, Token)>> = OnceLock::new();
    TABLE.get_or_init(|| {
        use Token::*;
        const RAW: &[(&str, Token)] = &[
            ("comment.block.documentation", DocComment),
            ("comment.line.documentation", DocComment),
            ("comment", Comment),
            ("punctuation.definition.comment", Comment),
            ("constant.character.escape", Escape),
            ("constant.other.placeholder", Escape),
            ("punctuation.section.interpolation", Escape),
            ("punctuation.definition.string", String),
            ("string.regexp", Regex),
            ("string", String),
            ("constant.numeric", Number),
            ("constant.language", Language),
            ("variable.other.constant", Constant),
            ("constant", Constant),
            ("keyword.operator.word", Keyword),
            ("keyword.operator", Operator),
            ("keyword.control", Control),
            // newer syntaxes use this where older ones say `storage.type`
            ("keyword.declaration", Storage),
            ("keyword", Keyword),
            ("storage.type.function.arrow", Operator),
            ("storage.modifier.lifetime", Label),
            ("storage.modifier", Modifier),
            ("storage", Storage),
            ("entity.name.function", Function),
            ("support.function.builtin", Builtin),
            ("support.function", Call),
            ("variable.function", Call),
            ("entity.name.tag", Tag),
            ("punctuation.definition.tag", Punctuation),
            ("entity.other.attribute-name", Attribute),
            ("entity.name.namespace", Namespace),
            ("entity.name.module", Namespace),
            ("support.module", Namespace),
            ("entity.name.section", Heading),
            ("markup.heading", Heading),
            ("punctuation.definition.heading", Heading),
            ("entity.name.macro", Macro),
            ("support.macro", Macro),
            ("variable.annotation", Macro),
            ("punctuation.definition.annotation", Macro),
            ("meta.annotation", Macro),
            ("entity.name.label", Label),
            ("entity.name.constant", Constant),
            ("entity.other.inherited-class", Type),
            ("entity.name", Type),
            ("support.type.property-name", Property),
            ("meta.property-name", Property),
            ("support.type", Builtin),
            ("support.class", Builtin),
            ("support.constant", Constant),
            ("variable.parameter", Parameter),
            ("variable.language", This),
            ("variable.other.member", Property),
            ("variable.other.property", Property),
            ("meta.property.object", Property),
            ("variable", Variable),
            ("markup.bold", Strong),
            ("markup.italic", Emphasis),
            ("markup.underline.link", Link),
            ("markup.raw", Code),
            ("markup.quote", Quote),
            ("markup.inserted", Inserted),
            ("markup.deleted", Deleted),
            ("markup.changed", Constant),
            ("meta.diff.header", Keyword),
            ("meta.diff.range", Function),
            ("punctuation.definition.list_item", ListMarker),
            ("markup.list.numbered.bullet", ListMarker),
            ("punctuation.section", Bracket),
            ("punctuation.definition.generic", Bracket),
            ("punctuation.definition.group", Bracket),
            ("punctuation", Punctuation),
            ("invalid", Invalid),
        ];
        RAW.iter().filter_map(|(s, t)| Scope::new(s).ok().map(|sc| (sc, *t))).collect()
    })
}

fn mapping_key() -> Scope {
    static KEY: OnceLock<Scope> = OnceLock::new();
    *KEY.get_or_init(|| Scope::new("meta.mapping.key").unwrap())
}

fn classify_scope(cache: &mut HashMap<Scope, Option<Token>>, scope: Scope) -> Option<Token> {
    *cache
        .entry(scope)
        .or_insert_with(|| table().iter().find(|(prefix, _)| prefix.is_prefix_of(scope)).map(|(_, t)| *t))
}

/// Token class for a scope stack: the innermost scope that maps to one wins.
fn classify_stack(cache: &mut HashMap<Scope, Option<Token>>, stack: &ScopeStack) -> Option<Token> {
    let scopes = stack.as_slice();
    for s in scopes.iter().rev() {
        if let Some(tok) = classify_scope(cache, *s) {
            if tok == Token::String {
                let key = mapping_key();
                if scopes.iter().any(|x| key.is_prefix_of(*x)) {
                    return Some(Token::Property);
                }
            }
            // comment markers inside a doc comment belong to it
            if tok == Token::Comment && scopes.iter().any(|x| classify_scope(cache, *x) == Some(Token::DocComment)) {
                return Some(Token::DocComment);
            }
            return Some(tok);
        }
    }
    None
}

fn push_span(spans: &mut Vec<TokSpan>, start: usize, end: usize, tok: Token) {
    if end <= start {
        return;
    }
    if let Some(last) = spans.last_mut() {
        if last.tok == tok && last.end as usize == start {
            last.end = end as u32;
            return;
        }
    }
    spans.push(TokSpan { start: start as u32, end: end as u32, tok });
}

/// Line `i` as text for the parser: content plus exactly one "\n".
/// Returns the text and the content length (line break excluded).
fn line_text(rope: &Rope, i: usize) -> (Cow<'_, str>, usize) {
    let text: Cow<str> = rope.line(i).into();
    let content = text.trim_end_matches(['\n', '\r', '\u{0B}', '\u{0C}', '\u{85}', '\u{2028}', '\u{2029}']).len();
    if text.len() == content + 1 && text.ends_with('\n') {
        (text, content)
    } else {
        let mut s = String::with_capacity(content + 1);
        s.push_str(&text[..content]);
        s.push('\n');
        (Cow::Owned(s), content)
    }
}

impl Highlighter {
    pub fn new(lang: &str) -> Highlighter {
        let mut h = Highlighter {
            lang: PLAIN.to_string(),
            syntax: None,
            lines: Vec::new(),
            dirty_from: 0,
            dirty_to: 0,
            valid_upto: 0,
            cur: None,
            classify: HashMap::new(),
        };
        h.set_lang(lang);
        h
    }

    /// The syntax name in use ("Plain Text" if the requested one was unknown).
    pub fn lang(&self) -> &str {
        &self.lang
    }

    pub fn set_lang(&mut self, lang: &str) {
        match syntax_set().find_syntax_by_name(lang) {
            Some(s) if s.name != PLAIN => {
                self.lang = s.name.clone();
                self.syntax = Some(s);
            }
            _ => {
                self.lang = PLAIN.to_string();
                self.syntax = None;
            }
        }
        self.reset();
    }

    /// Forget everything (after a whole-buffer replacement).
    pub fn reset(&mut self) {
        self.lines.clear();
        self.dirty_from = 0;
        self.dirty_to = 0;
        self.valid_upto = 0;
        self.cur = None;
    }

    /// Record one edit. `line` is the line containing the edit start;
    /// `lines_removed` / `lines_inserted` count the line breaks removed and
    /// inserted. Call once per edit, in order.
    pub fn edit(&mut self, line: usize, lines_removed: usize, lines_inserted: usize) {
        if self.syntax.is_none() {
            return;
        }
        // Line `line` keeps its entry (its start state is still right and its
        // stale spans avoid a flash until it is re-parsed).
        if line < self.lines.len() {
            let from = line + 1;
            let to = (from + lines_removed).min(self.lines.len());
            self.lines.splice(from..to, (0..lines_inserted).map(|_| LineHl::default()));
        }
        let shift = |x: usize| {
            if x > line + lines_removed {
                x + lines_inserted - lines_removed
            } else if x > line {
                line + 1
            } else {
                x
            }
        };
        let old_from = self.dirty_from;
        let pending = self.dirty_to > old_from;
        let had_tail = old_from < self.valid_upto;
        let mut to = line + lines_inserted + 1;
        if pending {
            to = to.max(shift(self.dirty_to));
        }
        if line < old_from && had_tail {
            // The frontier moves back while lines beyond the old frontier are
            // still unverified: everything up to it must be re-parsed, so that
            // convergence is only ever tested against one consistent pass.
            to = to.max(shift(old_from));
        }
        self.valid_upto = shift(self.valid_upto);
        // lines at or past `valid_upto` are unparsed anyway
        self.dirty_to = to.min(self.valid_upto);
        self.dirty_from = old_from.min(line);
        if matches!(&self.cur, Some((l, _)) if *l > line) {
            self.cur = None;
        }
    }

    /// Parser state at the start of line `i` (all lines before `i` are valid).
    fn state_at(&mut self, rope: &Rope, syntax: &'static SyntaxReference, i: usize) -> State {
        if matches!(&self.cur, Some((l, _)) if *l == i) {
            return self.cur.take().unwrap().1;
        }
        self.cur = None;
        let mut j = i;
        let mut st = loop {
            if let Some(s) = &self.lines[j].state {
                break (**s).clone();
            }
            if j == 0 {
                break State::fresh(syntax);
            }
            j -= 1;
        };
        for k in j..i {
            self.parse(rope, k, &mut st, false);
        }
        st
    }

    /// Parse line `i`, advancing `st` to the start of the next line.
    fn parse(&mut self, rope: &Rope, i: usize, st: &mut State, want_spans: bool) -> Vec<TokSpan> {
        let mut spans = Vec::new();
        let (text, content) = line_text(rope, i);
        if text.len() > MAX_LINE_BYTES {
            return spans;
        }
        let Ok(ops) = st.parse.parse_line(&text, syntax_set()) else { return spans };
        let mut pos = 0usize;
        for (at, op) in &ops {
            let at = (*at).min(content);
            if want_spans && at > pos {
                if let Some(tok) = classify_stack(&mut self.classify, &st.scopes) {
                    push_span(&mut spans, pos, at, tok);
                }
            }
            pos = pos.max(at);
            let _ = st.scopes.apply(op);
        }
        if want_spans && pos < content {
            if let Some(tok) = classify_stack(&mut self.classify, &st.scopes) {
                push_span(&mut spans, pos, content, tok);
            }
        }
        spans
    }

    /// Parse forward until line `upto_line` (inclusive) is up to date or the
    /// budget runs out. Returns true if it ran out (call again next frame).
    /// At least one line is parsed per call, so a zero budget still advances.
    pub fn advance(&mut self, rope: &Rope, upto_line: usize, budget: Duration) -> bool {
        let Some(syntax) = self.syntax else { return false };
        let n = rope.len_lines();
        if self.lines.len() != n {
            if !self.lines.is_empty() {
                // Edits were not reported consistently; start over.
                self.reset();
            }
            self.lines.resize_with(n, LineHl::default);
        }
        self.valid_upto = self.valid_upto.min(n);
        self.dirty_from = self.dirty_from.min(self.valid_upto);
        let end = upto_line.saturating_add(1).min(n);
        if self.dirty_from >= end {
            return false;
        }
        let t0 = Instant::now();
        let mut st = self.state_at(rope, syntax, self.dirty_from);
        loop {
            let i = self.dirty_from;
            let spans = self.parse(rope, i, &mut st, true);
            self.lines[i].spans = spans;
            let next = i + 1;
            if next < n {
                let stored = self.lines[next].state.is_some();
                if stored
                    && next >= self.dirty_to
                    && next < self.valid_upto
                    && **self.lines[next].state.as_ref().unwrap() == st
                {
                    // Converged: nothing further down is affected.
                    self.dirty_from = self.valid_upto;
                    self.dirty_to = 0;
                    if self.dirty_from >= end {
                        return false;
                    }
                    st = self.state_at(rope, syntax, self.dirty_from);
                    if t0.elapsed() >= budget {
                        self.cur = Some((self.dirty_from, st));
                        return true;
                    }
                    continue;
                }
                if stored || next % STATE_EVERY == 0 {
                    self.lines[next].state = Some(Box::new(st.clone()));
                }
            }
            self.dirty_from = next;
            self.valid_upto = self.valid_upto.max(next);
            if next >= end {
                self.cur = Some((next, st));
                return false;
            }
            if t0.elapsed() >= budget {
                self.cur = Some((next, st));
                return true;
            }
        }
    }

    /// Spans for a line; may be stale or empty if not parsed yet. Sorted and
    /// non-overlapping; plain text ranges are simply absent.
    pub fn spans(&self, line: usize) -> &[TokSpan] {
        self.lines.get(line).map(|l| l.spans.as_slice()).unwrap_or(&[])
    }
}

// ------------------------------------------------------------ languages

/// Syntax name for a file: by file name, then extension, then first line.
pub fn detect_lang(path: Option<&Path>, first_line: &str) -> String {
    let ss = syntax_set();
    if let Some(p) = path {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
        let found = [name, ext]
            .into_iter()
            .filter(|s| !s.is_empty())
            .find_map(|s| ss.find_syntax_by_extension(s).or_else(|| ss.find_syntax_by_extension(&s.to_lowercase())));
        if let Some(s) = found {
            return s.name.clone();
        }
    }
    let first = first_line.trim_end_matches(['\n', '\r']);
    if !first.is_empty() {
        if let Some(s) = ss.find_syntax_by_first_line(first) {
            return s.name.clone();
        }
    }
    PLAIN.to_string()
}

/// Sorted, de-duplicated syntax names for a picker.
pub fn all_langs() -> Vec<String> {
    let mut v: Vec<String> = syntax_set().syntaxes().iter().filter(|s| !s.hidden).map(|s| s.name.clone()).collect();
    v.sort_by_key(|s| s.to_lowercase());
    v.dedup();
    v
}

/// Line-comment prefix and suffix for a syntax name. `("", "")` means the
/// format has no comments (plain text, data files).
pub fn comment_tokens(lang: &str) -> (&'static str, &'static str) {
    let l = lang.to_ascii_lowercase();
    let l = l.as_str();
    let has = |words: &[&str]| words.iter().any(|w| l.contains(w));
    let is = |names: &[&str]| names.iter().any(|n| l == *n);
    if l.is_empty()
        || has(&["separated values", "non-printables"])
        || is(&[
            "plain text", "diff", "email", "command help", "manpage", "private key", "gosum", "git log", "textile",
            "vimhelp", "r console", "http request and response",
        ])
    {
        return ("", "");
    }
    if is(&["java server page (jsp)"]) {
        return ("<%--", "--%>");
    }
    if is(&["jinja2"]) {
        return ("{#", "#}");
    }
    if has(&["html", "xml", "markdown", "svelte", "vue", "svg", "xsl", "plist", "mediawiki"]) || is(&["nant build file"]) {
        return ("<!--", "-->");
    }
    if is(&["css"]) {
        return ("/*", "*/");
    }
    if has(&["ocaml", "camlp4"]) || is(&["sml"]) {
        return ("(*", "*)");
    }
    if is(&["restructuredtext"]) {
        return ("..", "");
    }
    if is(&["ruby haml"]) {
        return ("-#", "");
    }
    if is(&["ruby slim"]) {
        return ("/", "");
    }
    if is(&["groff/troff"]) {
        return (".\\\"", "");
    }
    if is(&["asp"]) || has(&["visual basic", "vbscript", "vb.net"]) {
        return ("'", "");
    }
    if has(&["header"]) {
        return ("//", "");
    }
    if is(&[
        "r", "yaml", "toml", "perl", "makefile", "nix", "julia", "elixir", "crystal", "powershell", "tcl", "nim",
        "coffeescript", "livescript", "fish", "awk", "sed", "gnuplot", "puppet", "graphql", "terraform", "hcl",
        "dotenv", "crontab", "nginx", "fstab", "passwd", "group", "resolv", "ninja", "meson", "starlark", "rego",
        "jq", "vyper", "varlink", "orgmode", "robot framework", "requirements.txt", "todo.txt", "cpuinfo", "meminfo",
        "strace", "syslog", "log", "debsources", "regular expression", "protocol buffer (text)", "salt state (sls)",
    ]) || has(&[
        "python", "ruby", "shell", "bash", "zsh", "git ", "ssh", "hosts", "authorized keys", "apache", "properties",
        "systemd", "docker", "cmake", "gdscript",
    ]) {
        return ("#", "");
    }
    if is(&["lua", "elm", "ada", "vhdl", "applescript", "idris", "agda", "dhall", "purescript", "cabal"])
        || has(&["haskell", "sql", "lean"])
    {
        return ("--", "");
    }
    if is(&["ini", "clojure", "scheme", "racket", "autohotkey", "nsis", "llvm"]) || has(&["lisp", "assembly"]) {
        return (";", "");
    }
    if is(&["erlang", "matlab", "prolog", "postscript", "octave", "tex"]) || has(&["latex", "bibtex"]) {
        return ("%", "");
    }
    if is(&["viml"]) {
        return ("\"", "");
    }
    if has(&["batch"]) {
        return ("REM", "");
    }
    if has(&["fortran"]) {
        return ("!", "");
    }
    ("//", "")
}

// ------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;

    const LONG: Duration = Duration::from_secs(60);

    fn hl_all(lang: &str, text: &str) -> (Highlighter, Rope) {
        let rope = Rope::from_str(text);
        let mut h = Highlighter::new(lang);
        assert!(!h.advance(&rope, usize::MAX, LONG));
        (h, rope)
    }

    /// Token of the span covering the first occurrence of `needle` on `line`.
    fn tok_of(h: &Highlighter, rope: &Rope, line: usize, needle: &str) -> Option<Token> {
        let text = rope.line(line).to_string();
        let at = text.find(needle).unwrap_or_else(|| panic!("'{needle}' not on line {line}: {text:?}")) as u32;
        h.spans(line).iter().find(|s| s.start <= at && at < s.end).map(|s| s.tok)
    }

    fn all_spans(h: &Highlighter, rope: &Rope) -> Vec<Vec<TokSpan>> {
        (0..rope.len_lines()).map(|i| h.spans(i).to_vec()).collect()
    }

    /// Apply an edit to the rope and report it the way the buffer will.
    fn apply(h: &mut Highlighter, rope: &mut Rope, start: usize, end: usize, ins: &str) {
        let line = rope.char_to_line(start);
        let removed = rope.char_to_line(end) - line;
        rope.remove(start..end);
        rope.insert(start, ins);
        let inserted = rope.char_to_line(start + ins.chars().count()) - line;
        h.edit(line, removed, inserted);
    }

    fn assert_matches_fresh(h: &mut Highlighter, rope: &Rope, what: &str) {
        assert!(!h.advance(rope, usize::MAX, LONG));
        let mut fresh = Highlighter::new(h.lang());
        assert!(!fresh.advance(rope, usize::MAX, LONG));
        let (a, b) = (all_spans(h, rope), all_spans(&fresh, rope));
        for (i, (x, y)) in a.iter().zip(&b).enumerate() {
            assert_eq!(x, y, "{what}: line {i} differs: {:?}", rope.line(i).to_string());
        }
        assert_eq!(a.len(), b.len());
    }

    #[test]
    fn rust_tokens() {
        let (h, r) = hl_all("Rust", "fn main() {\n    let x = \"hi\"; // note\n    if x == 1 { return; }\n}\n");
        assert_eq!(tok_of(&h, &r, 0, "fn"), Some(Token::Storage));
        assert_eq!(tok_of(&h, &r, 0, "main"), Some(Token::Function));
        assert_eq!(tok_of(&h, &r, 1, "let"), Some(Token::Storage));
        assert_eq!(tok_of(&h, &r, 1, "hi"), Some(Token::String));
        assert_eq!(tok_of(&h, &r, 1, "// note"), Some(Token::Comment));
        assert_eq!(tok_of(&h, &r, 1, "note"), Some(Token::Comment));
        assert_eq!(tok_of(&h, &r, 2, "if"), Some(Token::Control));
        assert_eq!(tok_of(&h, &r, 2, "return"), Some(Token::Control));
        assert_eq!(tok_of(&h, &r, 2, "1"), Some(Token::Number));
        // spans are sorted and non-overlapping
        for line in 0..r.len_lines() {
            let s = h.spans(line);
            assert!(s.windows(2).all(|w| w[0].end <= w[1].start));
            assert!(s.iter().all(|x| x.start < x.end && x.tok != Token::Text));
        }
    }

    #[test]
    fn python_tokens() {
        let (h, r) = hl_all("Python", "def foo(a):\n    return \"s\"  # c\n");
        assert_eq!(tok_of(&h, &r, 0, "def"), Some(Token::Storage));
        assert_eq!(tok_of(&h, &r, 0, "foo"), Some(Token::Function));
        assert_eq!(tok_of(&h, &r, 0, "a"), Some(Token::Parameter));
        assert_eq!(tok_of(&h, &r, 1, "return"), Some(Token::Control));
        assert_eq!(tok_of(&h, &r, 1, "s"), Some(Token::String));
        assert_eq!(tok_of(&h, &r, 1, "# c"), Some(Token::Comment));
    }

    #[test]
    fn finer_classes() {
        let (h, r) = hl_all("Rust", "/// doc\npub fn f<'a>(v: &'a Vec<u8>) -> bool {\n    self.len(); true\n}\n");
        assert_eq!(tok_of(&h, &r, 0, "///"), Some(Token::DocComment));
        assert_eq!(tok_of(&h, &r, 0, " doc"), Some(Token::DocComment));
        assert_eq!(tok_of(&h, &r, 1, "pub"), Some(Token::Modifier));
        assert_eq!(tok_of(&h, &r, 1, "'a"), Some(Token::Label));
        assert_eq!(tok_of(&h, &r, 1, "Vec"), Some(Token::Builtin));
        assert_eq!(tok_of(&h, &r, 1, "{"), Some(Token::Bracket));
        assert_eq!(tok_of(&h, &r, 2, "self"), Some(Token::This));
        assert_eq!(tok_of(&h, &r, 2, "len"), Some(Token::Call));
        assert_eq!(tok_of(&h, &r, 2, "true"), Some(Token::Language));
    }

    #[test]
    fn json_keys_are_properties() {
        let (h, r) = hl_all("JSON", "{\"key\": \"value\", \"n\": 12}\n");
        assert_eq!(tok_of(&h, &r, 0, "key"), Some(Token::Property));
        assert_eq!(tok_of(&h, &r, 0, "value"), Some(Token::String));
        assert_eq!(tok_of(&h, &r, 0, "12"), Some(Token::Number));
    }

    #[test]
    fn plain_and_unknown() {
        let (h, r) = hl_all("No Such Language", "fn main() {}\n");
        assert_eq!(h.lang(), "Plain Text");
        assert!(h.spans(0).is_empty());
        assert!(h.spans(99).is_empty());
        let _ = r;
        let mut h = Highlighter::new("Rust");
        assert_eq!(h.lang(), "Rust");
        h.set_lang("Python");
        assert_eq!(h.lang(), "Python");
    }

    #[test]
    fn block_comment_opens_and_closes() {
        let mut src = String::from("/* open\n");
        for i in 0..60 {
            src.push_str(&format!("let v{i} = {i};\n"));
        }
        let (mut h, mut r) = hl_all("Rust", &src);
        for line in [1, 17, 40, 60] {
            assert_eq!(tok_of(&h, &r, line, "let"), Some(Token::Comment), "line {line}");
        }
        // close the comment on line 0
        let end = r.line(0).len_chars() - 1;
        apply(&mut h, &mut r, end, end, " */");
        assert!(!h.advance(&r, usize::MAX, LONG));
        for line in [1, 17, 40, 60] {
            assert_eq!(tok_of(&h, &r, line, "let"), Some(Token::Storage), "line {line}");
        }
        assert_matches_fresh(&mut h, &r, "closed");
        // and open it again
        let at = r.line_to_char(0) + r.line(0).len_chars() - 4;
        apply(&mut h, &mut r, at, at + 3, "");
        assert_matches_fresh(&mut h, &r, "reopened");
        assert_eq!(tok_of(&h, &r, 30, "let"), Some(Token::Comment));
    }

    #[test]
    fn converges_without_reparsing_everything() {
        let mut src = String::new();
        for i in 0..400 {
            src.push_str(&format!("fn f{i}() {{ let a = {i}; }}\n"));
        }
        let (mut h, mut r) = hl_all("Rust", &src);
        let at = r.line_to_char(10) + 3;
        apply(&mut h, &mut r, at, at, "x");
        // only asked for the edited line, but convergence validates the rest
        assert!(!h.advance(&r, 10, LONG));
        assert!(h.dirty_from <= 32, "stopped at {}", h.dirty_from);
        assert!(!h.advance(&r, 40, LONG));
        assert_eq!(h.dirty_from, r.len_lines());
        assert_matches_fresh(&mut h, &r, "single char edit");
    }

    #[test]
    fn incremental_matches_fresh() {
        let mut src = String::new();
        for i in 0..120 {
            match i % 6 {
                0 => src.push_str(&format!("fn func_{i}(arg: u32) -> u32 {{\n")),
                1 => src.push_str("    let s = \"text\"; // trailing\n"),
                2 => src.push_str(&format!("    if arg > {i} {{ return arg; }}\n")),
                3 => src.push_str("    /* inline */ let t = 'c';\n"),
                4 => src.push_str("    arg + 1\n"),
                _ => src.push_str("}\n"),
            }
        }
        let (mut h, mut r) = hl_all("Rust", &src);
        let inserts = ["/*", "*/", "\"", "x", "\n", "\n\n\n", "// c\n", "fn a() {}\n", "/* a\n b\n", " */ ", "r#\"", "\"#"];
        let mut seed = 0x2545F4914F6CDD1Du64;
        let mut rnd = move |m: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % m as u64) as usize
        };
        for step in 0..160 {
            let len = r.len_chars();
            let start = rnd(len + 1);
            let del = match rnd(4) {
                0 => rnd(60),
                1 => rnd(4),
                _ => 0,
            };
            let end = (start + del).min(len);
            let ins = if rnd(5) == 0 { "" } else { inserts[rnd(inserts.len())] };
            apply(&mut h, &mut r, start, end, ins);
            match step % 4 {
                // several edits may pile up before any parsing
                0 => {}
                // partial advances: only part of the file, sometimes with no budget
                1 => {
                    let upto = rnd(r.len_lines());
                    h.advance(&r, upto, LONG);
                }
                2 => {
                    let upto = rnd(r.len_lines());
                    h.advance(&r, upto, Duration::ZERO);
                }
                _ => assert_matches_fresh(&mut h, &r, &format!("step {step}")),
            }
        }
        assert_matches_fresh(&mut h, &r, "final");
    }

    #[test]
    fn zero_budget_makes_progress() {
        let mut src = String::new();
        for i in 0..20 {
            src.push_str(&format!("let v = {i};\n"));
        }
        let rope = Rope::from_str(&src);
        let mut h = Highlighter::new("Rust");
        assert!(h.advance(&rope, usize::MAX, Duration::ZERO));
        assert!(!h.spans(0).is_empty());
        assert!(h.spans(5).is_empty());
        let mut calls = 1;
        while h.advance(&rope, usize::MAX, Duration::ZERO) {
            calls += 1;
            assert!(calls < 100, "no progress");
        }
        assert!(!h.spans(19).is_empty());
        let mut fresh = Highlighter::new("Rust");
        fresh.advance(&rope, usize::MAX, LONG);
        assert_eq!(all_spans(&h, &rope), all_spans(&fresh, &rope));
    }

    #[test]
    fn long_lines_are_skipped() {
        let long = format!("let a = \"{}\";\nlet b = 1;\n", "x".repeat(5000));
        let (h, r) = hl_all("Rust", &long);
        assert!(h.spans(0).is_empty());
        assert_eq!(tok_of(&h, &r, 1, "let"), Some(Token::Storage));
    }

    #[test]
    fn detects_languages() {
        let d = |p: &str| detect_lang(Some(Path::new(p)), "");
        assert_eq!(d("src/main.rs"), "Rust");
        assert_eq!(d("Cargo.toml"), "TOML");
        assert_eq!(d("/tmp/x.py"), "Python");
        assert_eq!(d("Makefile"), "Makefile");
        assert_eq!(d("notes.unknownext"), "Plain Text");
        assert_eq!(detect_lang(None, "#!/usr/bin/env python3"), "Python");
        assert_eq!(detect_lang(Some(Path::new("script")), "#!/usr/bin/env python3\n"), "Python");
        assert_eq!(detect_lang(None, "just some words"), "Plain Text");
        assert_eq!(detect_lang(None, ""), "Plain Text");
    }

    #[test]
    fn lang_list_and_comments() {
        let langs = all_langs();
        assert!(langs.len() > 50);
        for name in ["Rust", "Python", "TOML", "JSON", "Markdown", "Plain Text"] {
            assert!(langs.iter().any(|l| l == name), "{name} missing");
        }
        assert!(langs.windows(2).all(|w| w[0] != w[1]));
        // every listed language resolves back to itself
        for l in &langs {
            assert_eq!(Highlighter::new(l).lang(), l);
        }
        assert_eq!(comment_tokens("Rust"), ("//", ""));
        assert_eq!(comment_tokens("C++"), ("//", ""));
        assert_eq!(comment_tokens("JavaScript"), ("//", ""));
        assert_eq!(comment_tokens("Python"), ("#", ""));
        assert_eq!(comment_tokens("TOML"), ("#", ""));
        assert_eq!(comment_tokens("YAML"), ("#", ""));
        assert_eq!(comment_tokens("Bourne Again Shell (bash)"), ("#", ""));
        assert_eq!(comment_tokens("Lua"), ("--", ""));
        assert_eq!(comment_tokens("SQL"), ("--", ""));
        assert_eq!(comment_tokens("Haskell"), ("--", ""));
        assert_eq!(comment_tokens("HTML"), ("<!--", "-->"));
        assert_eq!(comment_tokens("XML"), ("<!--", "-->"));
        assert_eq!(comment_tokens("Markdown"), ("<!--", "-->"));
        assert_eq!(comment_tokens("CSS"), ("/*", "*/"));
        assert_eq!(comment_tokens("INI"), (";", ""));
        assert_eq!(comment_tokens("Lisp"), (";", ""));
        assert_eq!(comment_tokens("LaTeX"), ("%", ""));
        assert_eq!(comment_tokens("Erlang"), ("%", ""));
        assert_eq!(comment_tokens("Plain Text"), ("", ""));
        assert_eq!(comment_tokens("TeX"), ("%", ""));
        assert_eq!(comment_tokens("reStructuredText"), ("..", ""));
        assert_eq!(comment_tokens("OCaml"), ("(*", "*)"));
        assert_eq!(comment_tokens("Dockerfile"), ("#", ""));
        assert_eq!(comment_tokens("Fish"), ("#", ""));
        assert_eq!(comment_tokens("Go"), ("//", ""));
        assert_eq!(comment_tokens("TypeScript"), ("//", ""));
        assert_eq!(comment_tokens("Comma Separated Values"), ("", ""));
        assert_eq!(comment_tokens("Some Future Language"), ("//", ""));
    }

    #[test]
    #[ignore]
    fn print_langs() {
        for l in all_langs() {
            println!("{l:40} {:?}", comment_tokens(&l));
        }
    }
}
