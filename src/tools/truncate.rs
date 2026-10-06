//! Output truncation shared by the tools. Two limits apply and whichever is hit first wins:
//! a line limit and a byte limit. Head truncation never returns partial lines.

use serde::Serialize;

pub const DEFAULT_MAX_LINES: usize = 2000;
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;
pub const GREP_MAX_LINE_CHARS: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum TruncatedBy {
    Lines,
    Bytes,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Truncation {
    #[serde(skip)]
    pub content: String,
    pub truncated: bool,
    pub truncated_by: Option<TruncatedBy>,
    pub total_lines: usize,
    pub total_bytes: usize,
    pub output_lines: usize,
    pub output_bytes: usize,
    /// Tail truncation kept only the end of an over-long final line.
    pub last_line_partial: bool,
    /// Head truncation could not include even the first line.
    pub first_line_exceeds_limit: bool,
}

fn split_lines(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = content.split('\n').collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    lines
}

fn untruncated(content: &str, total_lines: usize) -> Truncation {
    Truncation {
        content: content.to_string(),
        truncated: false,
        truncated_by: None,
        total_lines,
        total_bytes: content.len(),
        output_lines: total_lines,
        output_bytes: content.len(),
        last_line_partial: false,
        first_line_exceeds_limit: false,
    }
}

/// Keep the first lines of `content` (for file reads).
pub fn truncate_head(content: &str, max_lines: usize, max_bytes: usize) -> Truncation {
    let lines = split_lines(content);
    let total_lines = lines.len();
    if total_lines <= max_lines && content.len() <= max_bytes {
        return untruncated(content, total_lines);
    }
    if lines.first().is_some_and(|first| first.len() > max_bytes) {
        return Truncation {
            content: String::new(),
            truncated: true,
            truncated_by: Some(TruncatedBy::Bytes),
            total_lines,
            total_bytes: content.len(),
            output_lines: 0,
            output_bytes: 0,
            last_line_partial: false,
            first_line_exceeds_limit: true,
        };
    }
    let mut kept = 0;
    let mut bytes = 0;
    let mut truncated_by = TruncatedBy::Lines;
    for (i, line) in lines.iter().enumerate().take(max_lines) {
        let line_bytes = line.len() + usize::from(i > 0);
        if bytes + line_bytes > max_bytes {
            truncated_by = TruncatedBy::Bytes;
            break;
        }
        bytes += line_bytes;
        kept += 1;
    }
    if kept >= max_lines && bytes <= max_bytes {
        truncated_by = TruncatedBy::Lines;
    }
    let output = lines[..kept].join("\n");
    Truncation {
        output_bytes: output.len(),
        content: output,
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines,
        total_bytes: content.len(),
        output_lines: kept,
        last_line_partial: false,
        first_line_exceeds_limit: false,
    }
}

/// Keep the last lines of `content` (for command output, where errors are at the end).
pub fn truncate_tail(content: &str, max_lines: usize, max_bytes: usize) -> Truncation {
    let lines = split_lines(content);
    let total_lines = lines.len();
    if total_lines <= max_lines && content.len() <= max_bytes {
        return untruncated(content, total_lines);
    }
    let mut kept: Vec<String> = Vec::new();
    let mut bytes = 0;
    let mut truncated_by = TruncatedBy::Lines;
    let mut last_line_partial = false;
    for line in lines.iter().rev() {
        if kept.len() >= max_lines {
            break;
        }
        let line_bytes = line.len() + usize::from(!kept.is_empty());
        if bytes + line_bytes > max_bytes {
            truncated_by = TruncatedBy::Bytes;
            if kept.is_empty() {
                let tail = tail_bytes(line, max_bytes);
                bytes = tail.len();
                kept.push(tail.to_string());
                last_line_partial = true;
            }
            break;
        }
        bytes += line_bytes;
        kept.push(line.to_string());
    }
    if kept.len() >= max_lines && bytes <= max_bytes {
        truncated_by = TruncatedBy::Lines;
    }
    kept.reverse();
    let output = kept.join("\n");
    Truncation {
        output_bytes: output.len(),
        content: output,
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines,
        total_bytes: content.len(),
        output_lines: kept.len(),
        last_line_partial,
        first_line_exceeds_limit: false,
    }
}

/// The longest suffix of `s` that fits in `max_bytes` and starts on a character boundary.
pub fn tail_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut start = s.len() - max_bytes;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// Shorten a single line to `max_chars` characters with a marker.
pub fn truncate_line(line: &str, max_chars: usize) -> (String, bool) {
    match line.char_indices().nth(max_chars) {
        Some((byte, _)) => (format!("{}... [truncated]", &line[..byte]), true),
        None => (line.to_string(), false),
    }
}

pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_respects_line_limit() {
        let t = truncate_head("a\nb\nc\nd\n", 2, 1000);
        assert!(t.truncated);
        assert_eq!(t.content, "a\nb");
        assert_eq!(t.truncated_by, Some(TruncatedBy::Lines));
        assert_eq!(t.total_lines, 4);
    }

    #[test]
    fn head_respects_byte_limit_without_partial_lines() {
        let t = truncate_head("aaaa\nbbbb\ncccc", 100, 9);
        assert_eq!(t.content, "aaaa\nbbbb");
        assert_eq!(t.truncated_by, Some(TruncatedBy::Bytes));
        let t = truncate_head("aaaaaaaaaaaa\nb", 100, 5);
        assert!(t.first_line_exceeds_limit);
    }

    #[test]
    fn tail_keeps_end_and_partial_last_line() {
        let t = truncate_tail("1\n2\n3\n4", 2, 1000);
        assert_eq!(t.content, "3\n4");
        let t = truncate_tail("short\nééééé", 100, 5);
        assert!(t.last_line_partial);
        assert_eq!(t.content, "éé");
    }

    #[test]
    fn untruncated_passthrough() {
        let t = truncate_head("x\ny", 10, 100);
        assert!(!t.truncated);
        assert_eq!(t.content, "x\ny");
    }

    #[test]
    fn line_truncation_is_char_safe() {
        assert_eq!(truncate_line("héllo", 2), ("hé... [truncated]".to_string(), true));
        assert_eq!(truncate_line("hi", 2), ("hi".to_string(), false));
    }
}
