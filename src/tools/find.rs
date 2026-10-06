use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::listing::{GlobFilter, listing_text, walker};
use super::path::resolve_path;
use super::truncate::DEFAULT_MAX_BYTES;
use super::{Tool, ToolContext, ToolOutput, UpdateFn, parse_args};

pub struct FindTool;

const DEFAULT_LIMIT: usize = 1000;

#[derive(Deserialize)]
struct Args {
    pattern: String,
    path: Option<String>,
    limit: Option<usize>,
}

#[async_trait]
impl Tool for FindTool {
    fn name(&self) -> &'static str {
        "find"
    }

    fn description(&self) -> String {
        format!(
            "Search for files by glob pattern. Returns matching file paths relative to the search directory. Respects \
             .gitignore. Output is truncated to {DEFAULT_LIMIT} results or {}KB (whichever is hit first).",
            DEFAULT_MAX_BYTES / 1024
        )
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Glob pattern to match files, e.g. '*.ts', '**/*.json', or 'src/**/*.spec.ts'"},
                "path": {"type": "string", "description": "Directory to search in (default: current directory)"},
                "limit": {"type": "integer", "minimum": 1, "description": "Maximum number of results (default: 1000)"}
            },
            "required": ["pattern"],
            "additionalProperties": false
        })
    }

    fn snippet(&self) -> &'static str {
        "Find files by glob pattern (respects .gitignore)"
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn execute(&self, ctx: &ToolContext, args: Value, _update: UpdateFn) -> anyhow::Result<ToolOutput> {
        let args: Args = parse_args("find", args)?;
        let root = resolve_path(args.path.as_deref().unwrap_or("."), &ctx.cwd);
        if !root.is_dir() {
            anyhow::bail!("Path not found or not a directory: {}", root.display());
        }
        let glob = GlobFilter::new(&args.pattern)?;
        let limit = args.limit.unwrap_or(DEFAULT_LIMIT).max(1);
        let cancel = ctx.cancel.clone();

        let (results, limit_reached) = tokio::task::spawn_blocking(move || -> anyhow::Result<(Vec<String>, bool)> {
            let mut results = Vec::new();
            for entry in walker(&root) {
                if cancel.is_cancelled() {
                    anyhow::bail!("Operation aborted");
                }
                let Ok(entry) = entry else { continue };
                let Ok(relative) = entry.path().strip_prefix(&root) else { continue };
                if relative.as_os_str().is_empty() || !glob.matches(relative) {
                    continue;
                }
                if results.len() >= limit {
                    return Ok((results, true));
                }
                let mut display = relative.to_string_lossy().replace('\\', "/");
                if entry.file_type().is_some_and(|t| t.is_dir()) {
                    display.push('/');
                }
                results.push(display);
            }
            Ok((results, false))
        })
        .await??;

        if results.is_empty() {
            return Ok(ToolOutput::text("No files found matching pattern"));
        }
        let notes =
            Vec::from_iter(limit_reached.then(|| {
                format!("{limit} results limit reached. Use limit={} for more, or refine pattern", limit * 2)
            }));
        Ok(ToolOutput::text(listing_text(&results, notes))
            .with_details(json!({"count": results.len(), "limitReached": limit_reached})))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::tests_support::{context, noop};

    #[tokio::test]
    async fn matches_names_and_paths_respecting_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/nested")).unwrap();
        std::fs::write(dir.path().join("src/a.rs"), "").unwrap();
        std::fs::write(dir.path().join("src/nested/b.rs"), "").unwrap();
        std::fs::write(dir.path().join("ignored.rs"), "").unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored.rs\n").unwrap();
        let ctx = context(dir.path());
        let out = FindTool.execute(&ctx, json!({"pattern": "*.rs"}), noop()).await.unwrap();
        assert_eq!(crate::message::content_text(&out.content), "src/a.rs\nsrc/nested/b.rs");
        let out = FindTool.execute(&ctx, json!({"pattern": "src/*/b.rs"}), noop()).await.unwrap();
        assert_eq!(crate::message::content_text(&out.content), "src/nested/b.rs");
    }
}
