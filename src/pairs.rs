//! Bracket and quote help: closing pairs as you type, wrapping a selection,
//! typing through a closer, deleting an empty pair, and finding the bracket
//! that matches another.

use crate::buffer::{Buffer, EditKind, Sel};

fn closer_of(c: char) -> Option<char> {
    match c {
        '(' => Some(')'),
        '[' => Some(']'),
        '{' => Some('}'),
        _ => None,
    }
}

fn opener_of(c: char) -> Option<char> {
    match c {
        ')' => Some('('),
        ']' => Some('['),
        '}' => Some('{'),
        _ => None,
    }
}

fn is_quote(c: char) -> bool {
    matches!(c, '"' | '\'' | '`')
}

fn ch(buf: &Buffer, i: usize) -> Option<char> {
    (i < buf.len()).then(|| buf.rope.char(i))
}

/// How often `c` occurs on the line holding `pos`.
fn on_line(buf: &Buffer, pos: usize, c: char) -> usize {
    buf.line_text(buf.line_of(pos)).chars().filter(|x| *x == c).count()
}

/// Handle a typed character that takes part in a pair. Returns false when
/// the character should simply be inserted. `apostrophes`: whether `'` is a
/// quote in this language (it is not in prose, or for Rust lifetimes).
pub fn type_char(buf: &mut Buffer, sel: &mut Sel, c: char, apostrophes: bool) -> bool {
    let len = buf.len();
    let (s, e) = (sel.start().min(len), sel.end().min(len));
    let close = closer_of(c).or_else(|| (is_quote(c) && (c != '\'' || apostrophes)).then_some(c));

    // a selection gets wrapped
    if s != e {
        let Some(close) = close else { return false };
        let forward = sel.head >= sel.anchor;
        buf.begin(*sel);
        buf.replace(e, e, &close.to_string());
        buf.replace(s, s, &c.to_string());
        *sel = if forward { Sel::new(s + 1, e + 1) } else { Sel::new(e + 1, s + 1) };
        buf.commit(*sel, EditKind::Other);
        return true;
    }

    let next = ch(buf, s);
    let prev = if s > 0 { ch(buf, s - 1) } else { None };

    // typing a closer that is already there steps over it, unless the line
    // is still missing one
    if next == Some(c) {
        let over = match opener_of(c) {
            Some(open) => on_line(buf, s, c) >= on_line(buf, s, open),
            None => is_quote(c) && on_line(buf, s, c) % 2 == 0,
        };
        if over {
            *sel = Sel::caret(s + 1);
            return true;
        }
    }

    let Some(close) = close else { return false };
    if !next.is_none_or(|n| n.is_whitespace() || matches!(n, ')' | ']' | '}' | ',' | ';' | ':' | '.')) {
        return false;
    }
    if is_quote(c) {
        // not after a word (apostrophes), a quote or a backslash, and not
        // when this quote closes a string that is already open on the line
        if prev.is_some_and(|p| p.is_alphanumeric() || p == '_' || p == c || p == '\\') || on_line(buf, s, c) % 2 == 1 {
            return false;
        }
    }
    buf.begin(*sel);
    buf.replace(s, s, &format!("{c}{close}"));
    *sel = Sel::caret(s + 1);
    buf.commit(*sel, EditKind::Other);
    true
}

/// Backspace between an empty pair removes both halves.
pub fn backspace_pair(buf: &mut Buffer, sel: &mut Sel) -> bool {
    if !sel.is_empty() || sel.head == 0 {
        return false;
    }
    let p = sel.head;
    let (Some(a), Some(b)) = (ch(buf, p - 1), ch(buf, p)) else { return false };
    let pair = closer_of(a) == Some(b) || (is_quote(a) && a == b && on_line(buf, p, a) % 2 == 0);
    if !pair {
        return false;
    }
    buf.begin(*sel);
    buf.replace(p - 1, p + 1, "");
    *sel = Sel::caret(p - 1);
    buf.commit(*sel, EditKind::Other);
    true
}

const SCAN_LIMIT: usize = 200_000;

fn match_from(buf: &Buffer, i: usize) -> Option<(usize, usize)> {
    let c = ch(buf, i)?;
    if let Some(close) = closer_of(c) {
        let mut depth = 0usize;
        for (k, x) in buf.rope.chars_at(i).enumerate().take(SCAN_LIMIT) {
            if x == c {
                depth += 1;
            } else if x == close {
                depth -= 1;
                if depth == 0 {
                    return Some((i, i + k));
                }
            }
        }
        None
    } else if let Some(open) = opener_of(c) {
        let mut depth = 0usize;
        let mut it = buf.rope.chars_at(i + 1);
        let mut k = i + 1;
        while let Some(x) = it.prev() {
            k -= 1;
            if x == c {
                depth += 1;
            } else if x == open {
                depth -= 1;
                if depth == 0 {
                    return Some((k, i));
                }
            }
            if i - k > SCAN_LIMIT {
                break;
            }
        }
        None
    } else {
        None
    }
}

/// The bracket pair touching the caret: the bracket right after it, or
/// failing that the one right before. Returns (opener, closer) positions.
pub fn match_at(buf: &Buffer, pos: usize) -> Option<(usize, usize)> {
    match_from(buf, pos).or_else(|| pos.checked_sub(1).and_then(|p| match_from(buf, p)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(text: &str, at: usize, c: char) -> (String, Sel) {
        let mut b = Buffer::new(text);
        let mut s = Sel::caret(at);
        if !type_char(&mut b, &mut s, c, true) {
            b.insert(&mut s, &c.to_string(), EditKind::Type);
        }
        (b.text(), s)
    }

    #[test]
    fn closes_brackets_and_quotes() {
        assert_eq!(typed("", 0, '('), ("()".into(), Sel::caret(1)));
        assert_eq!(typed("f)", 1, '('), ("f())".into(), Sel::caret(2)));
        assert_eq!(typed("x = ", 4, '"'), ("x = \"\"".into(), Sel::caret(5)));
        // not in front of a word
        assert_eq!(typed("abc", 0, '('), ("(abc".into(), Sel::caret(1)));
        // apostrophes stay single
        assert_eq!(typed("don", 3, '\''), ("don'".into(), Sel::caret(4)));
        // closing an open string does not open another
        assert_eq!(typed("\"abc", 4, '"'), ("\"abc\"".into(), Sel::caret(5)));
    }

    #[test]
    fn types_through_closers_only_when_balanced() {
        assert_eq!(typed("f()", 2, ')'), ("f()".into(), Sel::caret(3)));
        assert_eq!(typed("f(g(x)", 5, ')'), ("f(g(x))".into(), Sel::caret(6)));
        assert_eq!(typed("\"\"", 1, '"'), ("\"\"".into(), Sel::caret(2)));
    }

    #[test]
    fn wraps_a_selection() {
        let mut b = Buffer::new("a word b");
        let mut s = Sel::new(2, 6);
        assert!(type_char(&mut b, &mut s, '[', true));
        assert_eq!(b.text(), "a [word] b");
        assert_eq!(s, Sel::new(3, 7));
        b.undo();
        assert_eq!(b.text(), "a word b");
    }

    #[test]
    fn backspace_removes_an_empty_pair() {
        let mut b = Buffer::new("f()");
        let mut s = Sel::caret(2);
        assert!(backspace_pair(&mut b, &mut s));
        assert_eq!((b.text().as_str(), s), ("f", Sel::caret(1)));
        let mut b = Buffer::new("f(x)");
        let mut s = Sel::caret(2);
        assert!(!backspace_pair(&mut b, &mut s));
    }

    #[test]
    fn finds_matching_brackets() {
        let b = Buffer::new("a(b[c]{d})e");
        assert_eq!(match_at(&b, 1), Some((1, 9)));
        assert_eq!(match_at(&b, 10), Some((1, 9)));
        assert_eq!(match_at(&b, 3), Some((3, 5)));
        assert_eq!(match_at(&b, 6), Some((6, 8)));
        assert_eq!(match_at(&b, 0), None);
        assert_eq!(match_at(&Buffer::new("(("), 0), None);
        assert_eq!(match_at(&Buffer::new("))"), 2), None);
    }
}
