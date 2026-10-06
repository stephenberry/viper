//! Small text helpers shared across modules.

/// A piece of terminal text: an escape sequence or a visible character.
pub enum AnsiToken<'a> {
    Escape(&'a str),
    Char(char),
}

/// Split into escape sequences (CSI, OSC, and two-byte escapes) and visible characters.
pub fn ansi_tokens(s: &str) -> impl Iterator<Item = AnsiToken<'_>> {
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
            return Some(AnsiToken::Escape(seq));
        }
        rest = &rest[c.len_utf8()..];
        Some(AnsiToken::Char(c))
    })
}

/// Remove ANSI escape sequences and normalize carriage returns so tool output is clean for the
/// model and the terminal.
pub fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut tokens = ansi_tokens(input).peekable();
    while let Some(token) = tokens.next() {
        match token {
            AnsiToken::Escape(_) => {}
            // A bare carriage return (progress bars) starts the line over; keep a newline instead.
            AnsiToken::Char('\r') => {
                if !matches!(tokens.peek(), Some(AnsiToken::Char('\n'))) {
                    out.push('\n');
                }
            }
            AnsiToken::Char(c) => out.push(c),
        }
    }
    out
}

/// Escape text for inclusion in XML-like prompt sections.
pub fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

/// Replace the home directory prefix with `~` for display.
pub fn tildify(path: &std::path::Path) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(rest) = path.strip_prefix(&home)
    {
        if rest.as_os_str().is_empty() {
            return "~".into();
        }
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_escape_sequences() {
        assert_eq!(strip_ansi("\u{1b}[1;31mred\u{1b}[0m plain"), "red plain");
        assert_eq!(strip_ansi("\u{1b}]0;title\u{7}text"), "text");
        assert_eq!(strip_ansi("a\r\nb"), "a\nb");
    }
}
