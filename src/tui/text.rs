//! ANSI-aware text measurement and wrapping.

use unicode_width::UnicodeWidthChar;

enum Token<'a> {
    Escape(&'a str),
    Char(char),
}

/// Split into escape sequences and visible characters.
fn tokens(s: &str) -> impl Iterator<Item = Token<'_>> {
    let mut rest = s;
    std::iter::from_fn(move || {
        let c = rest.chars().next()?;
        if c == '\u{1b}' {
            let bytes = rest.as_bytes();
            let mut end = 1;
            if bytes.get(1) == Some(&b'[') {
                end = 2;
                while end < bytes.len() && !(0x40..=0x7e).contains(&bytes[end]) {
                    end += 1;
                }
                end = (end + 1).min(bytes.len());
            } else if bytes.get(1) == Some(&b']') {
                // OSC (e.g. hyperlinks) terminated by BEL or ESC \.
                end = 2;
                while end < bytes.len() {
                    if bytes[end] == 0x07 {
                        end += 1;
                        break;
                    }
                    if bytes[end] == 0x1b && bytes.get(end + 1) == Some(&b'\\') {
                        end += 2;
                        break;
                    }
                    end += 1;
                }
            } else if bytes.len() > 1 {
                end = 1 + rest[1..].chars().next().map(char::len_utf8).unwrap_or(0);
            }
            let (seq, tail) = rest.split_at(end);
            rest = tail;
            return Some(Token::Escape(seq));
        }
        rest = &rest[c.len_utf8()..];
        Some(Token::Char(c))
    })
}

pub fn char_width(c: char) -> usize {
    if c == '\t' { 4 } else { c.width().unwrap_or(0) }
}

/// Terminal columns occupied by `s`, ignoring escape sequences.
pub fn visible_width(s: &str) -> usize {
    tokens(s).map(|t| if let Token::Char(c) = t { char_width(c) } else { 0 }).sum()
}

fn is_reset(seq: &str) -> bool {
    seq == "\u{1b}[0m" || seq == "\u{1b}[m"
}

fn is_sgr(seq: &str) -> bool {
    seq.starts_with("\u{1b}[") && seq.ends_with('m')
}

/// Make `c` safe to print: tabs expand, other control characters are dropped.
fn push_visible(out: &mut String, c: char) {
    match c {
        '\t' => out.push_str("    "),
        c if c.is_control() => {}
        c => out.push(c),
    }
}

fn finish_line(mut line: String) -> String {
    if line.contains('\u{1b}') {
        line.push_str("\u{1b}[0m");
    }
    line
}

/// Word-wrap a single line (no newlines) to `width` columns, carrying active styles across
/// wrapped lines.
pub fn wrap(line: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![line.to_string()];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0;
    let mut style = String::new();
    // Last space on the current line: (byte index of the space, byte index after it, width after
    // it, style after it).
    let mut breakpoint: Option<(usize, usize, usize, String)> = None;

    for token in tokens(line) {
        match token {
            Token::Escape(seq) => {
                current.push_str(seq);
                if is_reset(seq) {
                    style.clear();
                } else if is_sgr(seq) {
                    style.push_str(seq);
                }
            }
            Token::Char(c) => {
                if c.is_control() && c != '\t' {
                    continue;
                }
                let w = char_width(c);
                if current_width + w > width && current_width > 0 {
                    if c == ' ' {
                        // Break at this space; it does not start the next line.
                        lines.push(finish_line(std::mem::take(&mut current)));
                        current = style.clone();
                        current_width = 0;
                        breakpoint = None;
                        continue;
                    }
                    match breakpoint.take() {
                        Some((space, after, at_width, break_style)) => {
                            let rest = current.split_off(after);
                            current.truncate(space);
                            lines.push(finish_line(std::mem::take(&mut current)));
                            current = break_style;
                            current.push_str(&rest);
                            current_width -= at_width;
                        }
                        None => {
                            lines.push(finish_line(std::mem::take(&mut current)));
                            current = style.clone();
                            current_width = 0;
                        }
                    }
                }
                let space = current.len();
                push_visible(&mut current, c);
                current_width += w;
                if c == ' ' {
                    breakpoint = Some((space, current.len(), current_width, style.clone()));
                }
            }
        }
    }
    lines.push(finish_line(current));
    lines
}

/// Wrap `line`, prefixing the first output line with `first` and the rest with `rest`.
pub fn wrap_prefixed(line: &str, width: usize, first: &str, rest: &str) -> Vec<String> {
    let indent = visible_width(first).max(visible_width(rest));
    wrap(line, width.saturating_sub(indent).max(10))
        .into_iter()
        .enumerate()
        .map(|(i, l)| format!("{}{l}", if i == 0 { first } else { rest }))
        .collect()
}

/// Cut `s` to at most `width` columns, ending with `…` when shortened.
pub fn truncate(s: &str, width: usize) -> String {
    if visible_width(s) <= width {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for token in tokens(s) {
        match token {
            Token::Escape(seq) => out.push_str(seq),
            Token::Char(c) => {
                let w = char_width(c);
                if used + w + 1 > width {
                    break;
                }
                push_visible(&mut out, c);
                used += w;
            }
        }
    }
    out.push('…');
    finish_line(out)
}

/// Remove control characters (including escapes) so untrusted text cannot drive the terminal.
pub fn sanitize(s: &str) -> String {
    let stripped = crate::util::strip_ansi(s);
    stripped.chars().filter(|c| !c.is_control() || *c == '\n' || *c == '\t').collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_without_escapes() {
        assert_eq!(visible_width("\u{1b}[1mab\u{1b}[0m日"), 4);
    }

    #[test]
    fn wraps_at_word_boundaries() {
        assert_eq!(wrap("hello world foo", 11), vec!["hello world", "foo"]);
        assert_eq!(wrap("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn carries_style_across_wraps() {
        let lines = wrap("\u{1b}[31mred text here\u{1b}[0m", 8);
        assert_eq!(lines[0], "\u{1b}[31mred text\u{1b}[0m");
        assert!(lines[1].starts_with("\u{1b}[31m"));
        assert_eq!(visible_width(&lines[1]), 4);
    }

    #[test]
    fn truncates_with_ellipsis() {
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("abc", 4), "abc");
    }
}
