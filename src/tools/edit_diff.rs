//! Text replacement and diff generation for the edit tool.
//!
//! Edits are matched against the original content, exactly first. If an edit only matches after
//! normalization (trailing whitespace, smart quotes, Unicode dashes and spaces), the edit runs in
//! normalized space and only the lines it touches are rewritten; all other lines keep their
//! original bytes.

use similar::{ChangeTag, TextDiff};

#[derive(Debug, Clone)]
pub struct Edit {
    pub old_text: String,
    pub new_text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    Lf,
    CrLf,
}

pub fn detect_line_ending(content: &str) -> LineEnding {
    match (content.find("\r\n"), content.find('\n')) {
        (Some(crlf), Some(lf)) if crlf < lf => LineEnding::CrLf,
        _ => LineEnding::Lf,
    }
}

pub fn normalize_to_lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

pub fn restore_line_endings(text: &str, ending: LineEnding) -> String {
    match ending {
        LineEnding::Lf => text.to_string(),
        LineEnding::CrLf => text.replace('\n', "\r\n"),
    }
}

fn normalize_char(c: char) -> char {
    match c {
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
        '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
        '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
        other => other,
    }
}

/// Normalization used for fuzzy matching. Preserves the number of lines.
pub fn normalize_for_fuzzy(text: &str) -> String {
    text.split('\n')
        .map(|line| line.trim_end().chars().map(normalize_char).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Debug, Clone)]
struct Replacement {
    edit_index: usize,
    start: usize,
    len: usize,
    new_text: String,
}

fn apply_replacements(content: &str, replacements: &[Replacement], offset: usize) -> String {
    let mut result = content.to_string();
    for r in replacements.iter().rev() {
        let start = r.start - offset;
        result.replace_range(start..start + r.len, &r.new_text);
    }
    result
}

/// Split into lines that keep their trailing `\n`.
fn lines_with_endings(content: &str) -> Vec<&str> {
    content.split_inclusive('\n').collect()
}

/// Byte span of each line (including its newline).
fn line_spans(content: &str) -> Vec<(usize, usize)> {
    let mut offset = 0;
    lines_with_endings(content)
        .into_iter()
        .map(|line| {
            let span = (offset, offset + line.len());
            offset += line.len();
            span
        })
        .collect()
}

fn replacement_line_range(spans: &[(usize, usize)], r: &Replacement) -> Result<(usize, usize), String> {
    let start = r.start;
    let end = r.start + r.len;
    let start_line = spans
        .iter()
        .position(|(s, e)| start >= *s && start < *e)
        .ok_or("Replacement range is outside the base content.")?;
    let mut end_line = start_line;
    while end_line < spans.len() && spans[end_line].1 < end {
        end_line += 1;
    }
    if end_line >= spans.len() {
        return Err("Replacement range is outside the base content.".into());
    }
    Ok((start_line, end_line + 1))
}

/// Apply replacements computed against `base` (a normalized view of `original`) while copying
/// untouched lines verbatim from `original`.
fn apply_preserving_unchanged_lines(
    original: &str,
    base: &str,
    replacements: &[Replacement],
) -> Result<String, String> {
    let original_lines = lines_with_endings(original);
    let spans = line_spans(base);
    if original_lines.len() != spans.len() {
        return Err("Cannot preserve unchanged lines because the normalized content has a different line count.".into());
    }
    let mut sorted = replacements.to_vec();
    sorted.sort_by_key(|r| r.start);

    let mut groups: Vec<(usize, usize, Vec<Replacement>)> = Vec::new();
    for r in sorted {
        let (start, end) = replacement_line_range(&spans, &r)?;
        match groups.last_mut() {
            Some(group) if start < group.1 => {
                group.1 = group.1.max(end);
                group.2.push(r);
            }
            _ => groups.push((start, end, vec![r])),
        }
    }

    let mut result = String::with_capacity(original.len());
    let mut next_line = 0;
    for (start, end, group) in groups {
        result.extend(original_lines[next_line..start].iter().copied());
        let from = spans[start].0;
        let to = spans[end - 1].1;
        result.push_str(&apply_replacements(&base[from..to], &group, from));
        next_line = end;
    }
    result.extend(original_lines[next_line..].iter().copied());
    Ok(result)
}

fn count_occurrences(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

/// Apply `edits` to LF-normalized `content`. Returns the new content or an error for the model.
pub fn apply_edits(content: &str, edits: &[Edit], path: &str) -> Result<String, String> {
    let total = edits.len();
    let edits: Vec<Edit> = edits
        .iter()
        .map(|e| Edit { old_text: normalize_to_lf(&e.old_text), new_text: normalize_to_lf(&e.new_text) })
        .collect();
    let label = |i: usize, single: &str, many: &str| {
        if total == 1 { single.to_string() } else { many.replace("{i}", &i.to_string()) }
    };

    for (i, edit) in edits.iter().enumerate() {
        if edit.old_text.is_empty() {
            return Err(label(
                i,
                &format!("oldText must not be empty in {path}."),
                &format!("edits[{{i}}].oldText must not be empty in {path}."),
            ));
        }
    }

    let needs_fuzzy = edits.iter().any(|e| !content.contains(&e.old_text));
    let normalized_content;
    let base: &str = if needs_fuzzy {
        normalized_content = normalize_for_fuzzy(content);
        &normalized_content
    } else {
        content
    };

    let mut replacements = Vec::with_capacity(edits.len());
    for (i, edit) in edits.iter().enumerate() {
        let needle = if needs_fuzzy && !base.contains(&edit.old_text) {
            normalize_for_fuzzy(&edit.old_text)
        } else {
            edit.old_text.clone()
        };
        let Some(start) = base.find(&needle) else {
            return Err(label(
                i,
                &format!(
                    "Could not find the exact text in {path}. The old text must match exactly including all whitespace and newlines."
                ),
                &format!(
                    "Could not find edits[{{i}}] in {path}. The oldText must match exactly including all whitespace and newlines."
                ),
            ));
        };
        let occurrences = count_occurrences(base, &needle);
        if occurrences > 1 {
            return Err(label(
                i,
                &format!(
                    "Found {occurrences} occurrences of the text in {path}. The text must be unique. Please provide more context to make it unique."
                ),
                &format!(
                    "Found {occurrences} occurrences of edits[{{i}}] in {path}. Each oldText must be unique. Please provide more context to make it unique."
                ),
            ));
        }
        replacements.push(Replacement { edit_index: i, start, len: needle.len(), new_text: edit.new_text.clone() });
    }

    replacements.sort_by_key(|r| r.start);
    for pair in replacements.windows(2) {
        if pair[0].start + pair[0].len > pair[1].start {
            return Err(format!(
                "edits[{}] and edits[{}] overlap in {path}. Merge them into one edit or target disjoint regions.",
                pair[0].edit_index, pair[1].edit_index
            ));
        }
    }

    let new_content = if needs_fuzzy {
        apply_preserving_unchanged_lines(content, base, &replacements)?
    } else {
        apply_replacements(base, &replacements, 0)
    };

    if new_content == content {
        return Err(if total == 1 {
            format!(
                "No changes made to {path}. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected."
            )
        } else {
            format!("No changes made to {path}. The replacements produced identical content.")
        });
    }
    Ok(new_content)
}

/// A display diff with line numbers (`+NN line`, `-NN line`, ` NN line`) and the first changed
/// line in the new file.
pub fn display_diff(old: &str, new: &str, context: usize) -> (String, Option<usize>) {
    let diff = TextDiff::from_lines(old, new);
    let width = old.lines().count().max(new.lines().count()).to_string().len();
    let mut out = Vec::new();
    let mut first_changed = None;
    for (i, group) in diff.grouped_ops(context).iter().enumerate() {
        if i > 0 {
            out.push(format!(" {:>width$} ...", ""));
        }
        for op in group {
            for change in diff.iter_changes(op) {
                let line = change.value().strip_suffix('\n').unwrap_or(change.value());
                match change.tag() {
                    ChangeTag::Equal => {
                        let n = change.new_index().unwrap_or(0) + 1;
                        out.push(format!(" {n:>width$} {line}"));
                    }
                    ChangeTag::Delete => {
                        let n = change.old_index().unwrap_or(0) + 1;
                        if first_changed.is_none() {
                            first_changed = Some(change.old_index().unwrap_or(0) + 1);
                        }
                        out.push(format!("-{n:>width$} {line}"));
                    }
                    ChangeTag::Insert => {
                        let n = change.new_index().unwrap_or(0) + 1;
                        if first_changed.is_none() {
                            first_changed = Some(n);
                        }
                        out.push(format!("+{n:>width$} {line}"));
                    }
                }
            }
        }
    }
    (out.join("\n"), first_changed)
}

/// Standard unified patch.
pub fn unified_patch(path: &str, old: &str, new: &str) -> String {
    TextDiff::from_lines(old, new).unified_diff().context_radius(4).header(path, path).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(old: &str, new: &str) -> Edit {
        Edit { old_text: old.into(), new_text: new.into() }
    }

    #[test]
    fn exact_single_and_multi_edits() {
        let content = "fn a() {}\nfn b() {}\nfn c() {}\n";
        assert_eq!(
            apply_edits(content, &[edit("fn b() {}", "fn bb() {}")], "x").unwrap(),
            "fn a() {}\nfn bb() {}\nfn c() {}\n"
        );
        let out = apply_edits(content, &[edit("fn a", "fn aa"), edit("fn c", "fn cc")], "x").unwrap();
        assert_eq!(out, "fn aa() {}\nfn b() {}\nfn cc() {}\n");
    }

    #[test]
    fn rejects_missing_duplicate_overlapping_and_noop() {
        let content = "x = 1\nx = 1\ny = 2\n";
        assert!(apply_edits(content, &[edit("z", "w")], "f").unwrap_err().contains("Could not find"));
        assert!(apply_edits(content, &[edit("x = 1", "x = 3")], "f").unwrap_err().contains("2 occurrences"));
        assert!(apply_edits("abcdef", &[edit("abc", "1"), edit("cde", "2")], "f").unwrap_err().contains("overlap"));
        assert!(apply_edits("abc", &[edit("b", "b")], "f").unwrap_err().contains("No changes"));
        assert!(apply_edits("abc", &[edit("", "b")], "f").unwrap_err().contains("must not be empty"));
    }

    #[test]
    fn fuzzy_match_preserves_untouched_lines() {
        let content = "keep \u{201C}smart\u{201D}   \nlet s = \u{2018}x\u{2019};  \ntail\n";
        let out = apply_edits(content, &[edit("let s = 'x';", "let s = 'y';")], "f").unwrap();
        assert_eq!(out, "keep \u{201C}smart\u{201D}   \nlet s = 'y';\ntail\n");
    }

    #[test]
    fn line_endings_round_trip() {
        let crlf = "a\r\nb\r\n";
        assert_eq!(detect_line_ending(crlf), LineEnding::CrLf);
        let lf = normalize_to_lf(crlf);
        assert_eq!(restore_line_endings(&lf, LineEnding::CrLf), crlf);
    }

    #[test]
    fn display_diff_numbers_lines() {
        let (diff, first) = display_diff("a\nb\nc\n", "a\nB\nc\n", 1);
        assert_eq!(diff, " 1 a\n-2 b\n+2 B\n 3 c");
        assert_eq!(first, Some(2));
    }
}
