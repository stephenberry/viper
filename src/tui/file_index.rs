//! The project's files and directories, for `@` completion.

use std::path::Path;

/// Indexing stops here, so a huge tree (a home directory, say) stays quick to search.
const MAX_ENTRIES: usize = 50_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedPath {
    /// Relative to the indexed directory, `/`-separated, ending in `/` for directories.
    pub path: String,
    pub is_dir: bool,
    lower: String,
    /// Byte offset of the last component in `path`.
    name_start: usize,
}

#[derive(Debug, Default)]
pub struct FileIndex {
    entries: Vec<IndexedPath>,
}

impl FileIndex {
    /// Walk `root`, skipping what `.gitignore` and similar files exclude and the `.git` directory.
    pub fn build(root: &Path) -> FileIndex {
        let walker =
            ignore::WalkBuilder::new(root).hidden(false).filter_entry(|entry| entry.file_name() != ".git").build();
        let mut entries = Vec::new();
        for entry in walker.flatten() {
            if entries.len() >= MAX_ENTRIES {
                break;
            }
            let Ok(relative) = entry.path().strip_prefix(root) else { continue };
            if relative.as_os_str().is_empty() {
                continue;
            }
            let is_dir = entry.file_type().is_some_and(|kind| kind.is_dir());
            let parts: Vec<String> =
                relative.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
            let mut path = parts.join("/");
            if is_dir {
                path.push('/');
            }
            entries.push(IndexedPath::new(path, is_dir));
        }
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        FileIndex { entries }
    }

    #[cfg(test)]
    fn from_paths(paths: &[&str]) -> FileIndex {
        FileIndex {
            entries: paths.iter().map(|path| IndexedPath::new(path.to_string(), path.ends_with('/'))).collect(),
        }
    }

    /// The best `limit` matches for `query`: paths containing it first (best at the start of a
    /// name), then paths containing its characters in order, shorter paths first. An empty query
    /// lists the shallowest paths alphabetically.
    pub fn search(&self, query: &str, limit: usize) -> Vec<&IndexedPath> {
        let query = query.to_lowercase();
        let mut scored: Vec<(i64, &IndexedPath)> =
            self.entries.iter().filter_map(|entry| Some((entry.score(&query)?, entry))).collect();
        scored.sort_by(|(a, x), (b, y)| b.cmp(a).then_with(|| x.lower.cmp(&y.lower)));
        scored.into_iter().take(limit).map(|(_, entry)| entry).collect()
    }
}

impl IndexedPath {
    fn new(path: String, is_dir: bool) -> IndexedPath {
        let trimmed = path.trim_end_matches('/');
        let name_start = trimmed.rfind('/').map_or(0, |slash| slash + 1);
        IndexedPath { lower: path.to_lowercase(), path, is_dir, name_start }
    }

    fn score(&self, query: &str) -> Option<i64> {
        let length = self.lower.len() as i64;
        if query.is_empty() {
            let depth = self.lower.trim_end_matches('/').matches('/').count() as i64;
            return Some(-depth);
        }
        if let Some(position) = self.lower.find(query) {
            let bonus = if position == self.name_start {
                2_000
            } else if position == 0 {
                1_500
            } else {
                1_000
            };
            return Some(bonus - position as i64 - length);
        }
        self.fuzzy_score(query).map(|score| score - length)
    }

    /// Matches `query`'s characters in order, rewarding runs and the starts of words.
    fn fuzzy_score(&self, query: &str) -> Option<i64> {
        let mut wanted = query.chars().peekable();
        let (mut score, mut previous, mut run) = (0, '/', false);
        for (index, c) in self.lower.char_indices() {
            let Some(&next) = wanted.peek() else { break };
            if c == next {
                score += 10;
                if run {
                    score += 15;
                }
                if matches!(previous, '/' | '_' | '-' | '.' | ' ') {
                    score += 20;
                }
                if index >= self.name_start {
                    score += 5;
                }
                wanted.next();
                run = true;
            } else {
                run = false;
            }
            previous = c;
        }
        wanted.peek().is_none().then_some(score)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(index: &FileIndex, query: &str) -> Vec<String> {
        index.search(query, 3).into_iter().map(|entry| entry.path.clone()).collect()
    }

    #[test]
    fn ranks_names_then_paths_then_fuzzy_matches() {
        let index = FileIndex::from_paths(&[
            "README.md",
            "docs/",
            "docs/install.md",
            "src/",
            "src/main.rs",
            "src/tui/",
            "src/tui/mod.rs",
            "src/tui/prompt.rs",
        ]);
        assert_eq!(paths(&index, "main"), ["src/main.rs"]);
        assert_eq!(paths(&index, "src/tu"), ["src/tui/", "src/tui/mod.rs", "src/tui/prompt.rs"]);
        assert_eq!(paths(&index, "mainrs"), ["src/main.rs"]);
        assert_eq!(paths(&index, "tprompt"), ["src/tui/prompt.rs"]);
        assert_eq!(paths(&index, "INSTALL"), ["docs/install.md"]);
        assert!(paths(&index, "zzz").is_empty());
        // Nothing typed yet: the top level first, alphabetically.
        assert_eq!(paths(&index, ""), ["docs/", "README.md", "src/"]);
    }

    #[test]
    fn indexes_files_and_directories_respecting_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join("src/main.rs"), "").unwrap();
        std::fs::write(root.join("target/debug/out"), "").unwrap();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(root.join(".git/HEAD"), "").unwrap();
        // .gitignore applies inside a git repository.
        std::process::Command::new("git").arg("init").arg("-q").current_dir(root).status().ok();
        let index = FileIndex::build(root);
        let all: Vec<&str> = index.entries.iter().map(|entry| entry.path.as_str()).collect();
        assert_eq!(all, [".gitignore", "src/", "src/main.rs"]);
    }
}
