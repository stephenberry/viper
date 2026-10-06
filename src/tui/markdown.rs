//! Line-by-line Markdown styling for streamed assistant text.
//!
//! Each completed line is styled once and never re-rendered, so output can go straight to
//! scrollback while the response streams. Block state (code fences) carries across lines.

use super::style::*;

#[derive(Default)]
pub struct MarkdownRenderer {
    in_code: bool,
}

impl MarkdownRenderer {
    pub fn render_line(&mut self, line: &str) -> String {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            self.in_code = !self.in_code;
            return paint(GRAY, line);
        }
        if self.in_code {
            return paint(CYAN, line);
        }
        if trimmed.is_empty() {
            return String::new();
        }
        let indent = &line[..line.len() - trimmed.len()];

        if let Some(level) = heading_level(trimmed) {
            let text = trimmed[level..].trim();
            let style = if level <= 2 { format!("{BOLD}{UNDERLINE}{MAGENTA}") } else { format!("{BOLD}{MAGENTA}") };
            return format!("{indent}{style}{}{RESET}", inline(text, &style));
        }
        if is_rule(trimmed) {
            return paint(GRAY, &"─".repeat(40));
        }
        if let Some(rest) = trimmed.strip_prefix('>') {
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            return format!("{indent}{GRAY}│{RESET} {ITALIC}{}{RESET}", inline(rest, ITALIC));
        }
        for marker in ["- [ ] ", "* [ ] "] {
            if let Some(rest) = trimmed.strip_prefix(marker) {
                return format!("{indent}{GRAY}☐{RESET} {}", inline(rest, ""));
            }
        }
        for marker in ["- [x] ", "* [x] ", "- [X] "] {
            if let Some(rest) = trimmed.strip_prefix(marker) {
                return format!("{indent}{GREEN}☑{RESET} {}", inline(rest, ""));
            }
        }
        for marker in ["- ", "* ", "+ "] {
            if let Some(rest) = trimmed.strip_prefix(marker) {
                return format!("{indent}{CYAN}•{RESET} {}", inline(rest, ""));
            }
        }
        if let Some((number, rest)) = ordered_item(trimmed) {
            return format!("{indent}{CYAN}{number}.{RESET} {}", inline(rest, ""));
        }
        format!("{indent}{}", inline(trimmed, ""))
    }

    /// Style a partial (still streaming) line without advancing block state.
    pub fn preview_line(&self, line: &str) -> String {
        if self.in_code { paint(CYAN, line) } else { line.to_string() }
    }
}

fn heading_level(line: &str) -> Option<usize> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    ((1..=6).contains(&hashes) && line[hashes..].starts_with(' ')).then_some(hashes)
}

fn is_rule(line: &str) -> bool {
    let compact: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    compact.len() >= 3
        && (compact.chars().all(|c| c == '-') || compact.chars().all(|c| c == '*') || compact.chars().all(|c| c == '_'))
}

fn ordered_item(line: &str) -> Option<(&str, &str)> {
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 || digits > 4 {
        return None;
    }
    let rest = line[digits..].strip_prefix(". ").or_else(|| line[digits..].strip_prefix(") "))?;
    Some((&line[..digits], rest))
}

/// Find `marker` closing an inline span starting at `from`.
fn find_close(chars: &[char], from: usize, marker: &[char]) -> Option<usize> {
    let mut i = from;
    while i + marker.len() <= chars.len() {
        if chars[i..i + marker.len()] == *marker && i > from && !chars[i - 1].is_whitespace() {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Style inline code, bold, italic, and links. `base` is re-applied after each span.
pub fn inline(text: &str, base: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let restore = |out: &mut String| {
        out.push_str(RESET);
        out.push_str(base);
    };
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '`'
            && let Some(end) = chars[i + 1..].iter().position(|ch| *ch == '`').map(|p| p + i + 1)
        {
            out.push_str(YELLOW);
            out.extend(&chars[i + 1..end]);
            restore(&mut out);
            i = end + 1;
            continue;
        }
        if (c == '*' || c == '_')
            && chars.get(i + 1) == Some(&c)
            && chars.get(i + 2).is_some_and(|n| !n.is_whitespace())
            && let Some(end) = find_close(&chars, i + 2, &[c, c])
        {
            out.push_str(BOLD);
            out.push_str(&inline(&chars[i + 2..end].iter().collect::<String>(), &format!("{base}{BOLD}")));
            restore(&mut out);
            i = end + 2;
            continue;
        }
        let word_start = i == 0 || !chars[i - 1].is_alphanumeric();
        if (c == '*' || (c == '_' && word_start))
            && chars.get(i + 1).is_some_and(|n| !n.is_whitespace() && *n != c)
            && let Some(end) = find_close(&chars, i + 1, &[c])
        {
            let closes_word = c == '*' || chars.get(end + 1).is_none_or(|n| !n.is_alphanumeric());
            if closes_word {
                out.push_str(ITALIC);
                out.push_str(&inline(&chars[i + 1..end].iter().collect::<String>(), &format!("{base}{ITALIC}")));
                restore(&mut out);
                i = end + 1;
                continue;
            }
        }
        if c == '['
            && let Some(close) = chars[i + 1..].iter().position(|ch| *ch == ']').map(|p| p + i + 1)
            && chars.get(close + 1) == Some(&'(')
            && let Some(paren) = chars[close + 2..].iter().position(|ch| *ch == ')').map(|p| p + close + 2)
        {
            let label: String = chars[i + 1..close].iter().collect();
            let url: String = chars[close + 2..paren].iter().collect();
            out.push_str(&format!("{UNDERLINE}{BLUE}{label}"));
            restore(&mut out);
            if url != label {
                out.push_str(&format!("{GRAY} ({url})"));
                restore(&mut out);
            }
            i = paren + 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::text::visible_width;

    fn plain(s: &str) -> String {
        crate::util::strip_ansi(s)
    }

    #[test]
    fn styles_blocks() {
        let mut md = MarkdownRenderer::default();
        assert_eq!(plain(&md.render_line("## Title")), "Title");
        assert_eq!(plain(&md.render_line("- item")), "• item");
        assert_eq!(plain(&md.render_line("12. step")), "12. step");
        md.render_line("```rust");
        assert_eq!(md.render_line("- not a list"), paint(CYAN, "- not a list"));
        md.render_line("```");
        assert_eq!(plain(&md.render_line("- list")), "• list");
    }

    #[test]
    fn styles_inline_spans() {
        assert_eq!(plain(&inline("a `b` **c** *d* [e](http://x)", "")), "a b c d e (http://x)");
        assert_eq!(plain(&inline("2 * 3 * 4", "")), "2 * 3 * 4");
        assert_eq!(plain(&inline("snake_case_name", "")), "snake_case_name");
        assert_eq!(visible_width(&inline("**bold**", "")), 4);
    }
}
