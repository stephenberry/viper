//! Path resolution for tool arguments.

use std::path::{Path, PathBuf};

const NARROW_NO_BREAK_SPACE: char = '\u{202F}';

/// Resolve a model-supplied path against `cwd`. Handles `~`, a leading `@` (from file mentions),
/// and unusual Unicode spaces that models sometimes emit.
pub fn resolve_path(raw: &str, cwd: &Path) -> PathBuf {
    let trimmed = raw.trim();
    let without_at = trimmed.strip_prefix('@').unwrap_or(trimmed);
    let normalized: String = without_at
        .chars()
        .map(|c| match c {
            '\u{00A0}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
            other => other,
        })
        .collect();
    let path = crate::config::expand_home(&normalized);
    if path.is_absolute() { path } else { cwd.join(path) }
}

/// Like [`resolve_path`], but if the file does not exist, try variants macOS uses in file names
/// (narrow no-break space before AM/PM in screenshot names, curly apostrophes).
pub fn resolve_existing_path(raw: &str, cwd: &Path) -> PathBuf {
    let resolved = resolve_path(raw, cwd);
    if resolved.exists() {
        return resolved;
    }
    let text = resolved.to_string_lossy().to_string();
    let am_pm = text
        .replace(" AM.", &format!("{NARROW_NO_BREAK_SPACE}AM."))
        .replace(" PM.", &format!("{NARROW_NO_BREAK_SPACE}PM."));
    let curly = text.replace('\'', "\u{2019}");
    for candidate in [am_pm, curly] {
        let candidate = PathBuf::from(candidate);
        if candidate != resolved && candidate.exists() {
            return candidate;
        }
    }
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_relative_and_at_prefixed_paths() {
        let cwd = Path::new("/work");
        assert_eq!(resolve_path("src/a.rs", cwd), PathBuf::from("/work/src/a.rs"));
        assert_eq!(resolve_path("@src/a.rs", cwd), PathBuf::from("/work/src/a.rs"));
        assert_eq!(resolve_path("/abs/x", cwd), PathBuf::from("/abs/x"));
        assert_eq!(resolve_path("a\u{00A0}b", cwd), PathBuf::from("/work/a b"));
    }
}
