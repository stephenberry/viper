use std::path::{Path, PathBuf};

use async_trait::async_trait;
use regex::{Regex, RegexBuilder};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::listing::{GlobFilter, listing_text, walker};
use super::path::resolve_path;
use super::truncate::{DEFAULT_MAX_BYTES, GREP_MAX_LINE_CHARS, truncate_line};
use super::{Tool, ToolContext, ToolOutput, UpdateFn, parse_args};

pub struct GrepTool;

const DEFAULT_LIMIT: usize = 100;
/// Files larger than this are skipped.
const MAX_FILE_BYTES: u64 = 20 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Args {
    pattern: String,
    path: Option<String>,
    glob: Option<String>,
    #[serde(default)]
    ignore_case: bool,
    #[serde(default)]
    literal: bool,
    context: Option<usize>,
    limit: Option<usize>,
}

struct Search {
    regex: Regex,
    context: usize,
    limit: usize,
    glob: Option<GlobFilter>,
}

struct SearchResult {
    lines: Vec<String>,
    matches: usize,
    limit_reached: bool,
    lines_truncated: bool,
}

fn read_text(path: &Path) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    if metadata.len() > MAX_FILE_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if bytes[..bytes.len().min(8192)].contains(&0) {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn search_file(search: &Search, file: &Path, display: &str, result: &mut SearchResult) {
    let Some(text) = read_text(file) else { return };
    let lines: Vec<&str> = text.lines().collect();
    let mut printed_until: Option<usize> = None;
    for (index, line) in lines.iter().enumerate() {
        if !search.regex.is_match(line) {
            continue;
        }
        if result.matches >= search.limit {
            result.limit_reached = true;
            return;
        }
        result.matches += 1;
        let start = index.saturating_sub(search.context);
        let end = (index + search.context).min(lines.len().saturating_sub(1));
        let from = match printed_until {
            Some(last) if last + 1 >= start => last + 1,
            Some(_) => {
                result.lines.push("--".into());
                start
            }
            None => start,
        };
        for (i, line) in lines.iter().enumerate().take(end + 1).skip(from) {
            let (text, cut) = truncate_line(line, GREP_MAX_LINE_CHARS);
            result.lines_truncated |= cut;
            let sep = if i == index || search.regex.is_match(line) { ':' } else { '-' };
            result.lines.push(format!("{display}{sep}{}{sep} {text}", i + 1));
        }
        printed_until = Some(end.max(printed_until.unwrap_or(0)));
    }
}

fn run(search: Search, root: PathBuf, cancel: CancellationToken) -> anyhow::Result<SearchResult> {
    let mut result = SearchResult { lines: Vec::new(), matches: 0, limit_reached: false, lines_truncated: false };
    if root.is_file() {
        let display = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        search_file(&search, &root, &display, &mut result);
        return Ok(result);
    }
    for entry in walker(&root) {
        if cancel.is_cancelled() {
            anyhow::bail!("Operation aborted");
        }
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let relative = entry.path().strip_prefix(&root).unwrap_or(entry.path());
        if let Some(glob) = &search.glob
            && !glob.matches(relative)
        {
            continue;
        }
        search_file(&search, entry.path(), &relative.to_string_lossy(), &mut result);
        if result.limit_reached {
            break;
        }
    }
    Ok(result)
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn description(&self) -> String {
        format!(
            "Search file contents for a pattern. Returns matching lines with file paths and line numbers. Respects \
             .gitignore. Output is truncated to {DEFAULT_LIMIT} matches or {}KB (whichever is hit first). Long lines \
             are truncated to {GREP_MAX_LINE_CHARS} chars.",
            DEFAULT_MAX_BYTES / 1024
        )
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Search pattern (regex or literal string)"},
                "path": {"type": "string", "description": "Directory or file to search (default: current directory)"},
                "glob": {"type": "string", "description": "Filter files by glob pattern, e.g. '*.ts' or '**/*.spec.ts'"},
                "ignoreCase": {"type": "boolean", "description": "Case-insensitive search (default: false)"},
                "literal": {"type": "boolean", "description": "Treat pattern as literal string instead of regex (default: false)"},
                "context": {"type": "integer", "minimum": 0, "description": "Number of lines to show before and after each match (default: 0)"},
                "limit": {"type": "integer", "minimum": 1, "description": "Maximum number of matches to return (default: 100)"}
            },
            "required": ["pattern"],
            "additionalProperties": false
        })
    }

    fn snippet(&self) -> &'static str {
        "Search file contents for patterns (respects .gitignore)"
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn execute(&self, ctx: &ToolContext, args: Value, _update: UpdateFn) -> anyhow::Result<ToolOutput> {
        let args: Args = parse_args("grep", args)?;
        let root = resolve_path(args.path.as_deref().unwrap_or("."), &ctx.cwd);
        if !root.exists() {
            anyhow::bail!("Path not found: {}", root.display());
        }
        let pattern = if args.literal { regex::escape(&args.pattern) } else { args.pattern.clone() };
        let regex = RegexBuilder::new(&pattern)
            .case_insensitive(args.ignore_case)
            .build()
            .map_err(|err| anyhow::anyhow!("Invalid regex: {err}"))?;
        let limit = args.limit.unwrap_or(DEFAULT_LIMIT).max(1);
        let search = Search {
            regex,
            context: args.context.unwrap_or(0),
            limit,
            glob: args.glob.as_deref().map(GlobFilter::new).transpose()?,
        };
        let cancel = ctx.cancel.clone();
        let result = tokio::task::spawn_blocking(move || run(search, root, cancel)).await??;

        if result.matches == 0 {
            return Ok(ToolOutput::text("No matches found"));
        }
        let mut notes = Vec::new();
        if result.limit_reached {
            notes.push(format!("{limit} matches limit reached. Use limit={} for more, or refine pattern", limit * 2));
        }
        if result.lines_truncated {
            notes.push(format!("Some lines truncated to {GREP_MAX_LINE_CHARS} chars. Use read to see full lines"));
        }
        Ok(ToolOutput::text(listing_text(&result.lines, notes))
            .with_details(json!({"matches": result.matches, "limitReached": result.limit_reached})))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::tests_support::{context, noop};

    #[tokio::test]
    async fn finds_matches_with_context_and_glob() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "one\ntwo\nthree\nfour\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "two\n").unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/x.rs"), "two\n").unwrap();
        let ctx = context(dir.path());
        let out =
            GrepTool.execute(&ctx, json!({"pattern": "tw.", "glob": "*.rs", "context": 1}), noop()).await.unwrap();
        assert_eq!(crate::message::content_text(&out.content), "a.rs-1- one\na.rs:2: two\na.rs-3- three");
        let out = GrepTool.execute(&ctx, json!({"pattern": "TWO", "ignoreCase": true}), noop()).await.unwrap();
        assert_eq!(crate::message::content_text(&out.content), "a.rs:2: two\nb.txt:1: two");
    }

    #[tokio::test]
    async fn respects_limit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x\nx\nx\n").unwrap();
        let out = GrepTool.execute(&context(dir.path()), json!({"pattern": "x", "limit": 2}), noop()).await.unwrap();
        let text = crate::message::content_text(&out.content);
        assert!(text.starts_with("a.txt:1: x\na.txt:2: x\n\n[2 matches limit reached"));
    }
}
