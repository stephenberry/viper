//! Helpers shared by the tools that list paths or matches: `grep`, `find`, and `ls`.

use std::path::Path;

use globset::GlobMatcher;

use super::truncate::{DEFAULT_MAX_BYTES, format_size, truncate_head};

/// Build a walker over `root` that respects ignore files, includes dotfiles, and skips `.git`.
pub(super) fn walker(root: &Path) -> ignore::Walk {
    ignore::WalkBuilder::new(root)
        .hidden(false)
        .require_git(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .sort_by_file_path(|a, b| a.cmp(b))
        .build()
}

/// A glob that matches file names, or relative paths when the pattern contains `/`.
pub(super) struct GlobFilter {
    matcher: GlobMatcher,
    match_path: bool,
}

impl GlobFilter {
    pub fn new(pattern: &str) -> anyhow::Result<GlobFilter> {
        let glob = globset::GlobBuilder::new(pattern)
            .literal_separator(false)
            .build()
            .map_err(|err| anyhow::anyhow!("Invalid glob '{pattern}': {err}"))?;
        Ok(GlobFilter { matcher: glob.compile_matcher(), match_path: pattern.contains('/') })
    }

    pub fn matches(&self, relative: &Path) -> bool {
        if self.match_path {
            self.matcher.is_match(relative)
        } else {
            relative.file_name().is_some_and(|name| self.matcher.is_match(name))
        }
    }
}

/// Join listing lines, cut at the byte limit, and append the notes (plus one for the byte
/// limit when it was hit) in brackets.
pub(super) fn listing_text(lines: &[String], mut notes: Vec<String>) -> String {
    let truncation = truncate_head(&lines.join("\n"), usize::MAX, DEFAULT_MAX_BYTES);
    let mut text = truncation.content;
    if truncation.truncated {
        notes.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
    }
    if !notes.is_empty() {
        text.push_str(&format!("\n\n[{}]", notes.join(". ")));
    }
    text
}
