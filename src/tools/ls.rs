use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::path::resolve_path;
use super::truncate::{DEFAULT_MAX_BYTES, format_size, truncate_head};
use super::{Tool, ToolContext, ToolOutput, UpdateFn, parse_args};

pub struct LsTool;

const DEFAULT_LIMIT: usize = 500;

#[derive(Deserialize)]
struct Args {
    path: Option<String>,
    limit: Option<usize>,
}

#[async_trait]
impl Tool for LsTool {
    fn name(&self) -> &'static str {
        "ls"
    }

    fn description(&self) -> String {
        format!(
            "List directory contents. Returns entries sorted alphabetically, with '/' suffix for directories. Includes \
             dotfiles. Output is truncated to {DEFAULT_LIMIT} entries or {}KB (whichever is hit first).",
            DEFAULT_MAX_BYTES / 1024
        )
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Directory to list (default: current directory)"},
                "limit": {"type": "integer", "minimum": 1, "description": "Maximum number of entries to return (default: 500)"}
            },
            "additionalProperties": false
        })
    }

    fn snippet(&self) -> &'static str {
        "List directory contents"
    }

    async fn execute(&self, ctx: &ToolContext, args: Value, _update: UpdateFn) -> anyhow::Result<ToolOutput> {
        let args: Args = parse_args("ls", args)?;
        let dir = resolve_path(args.path.as_deref().unwrap_or("."), &ctx.cwd);
        let metadata =
            tokio::fs::metadata(&dir).await.map_err(|_| anyhow::anyhow!("Path not found: {}", dir.display()))?;
        if !metadata.is_dir() {
            anyhow::bail!("Not a directory: {}", dir.display());
        }
        let mut entries = Vec::new();
        let mut reader = tokio::fs::read_dir(&dir).await?;
        while let Some(entry) = reader.next_entry().await? {
            let mut name = entry.file_name().to_string_lossy().into_owned();
            // Follow symlinks so linked directories are marked as directories.
            if tokio::fs::metadata(entry.path()).await.map(|m| m.is_dir()).unwrap_or(false) {
                name.push('/');
            }
            entries.push(name);
        }
        entries.sort_by_key(|name| name.to_lowercase());
        if entries.is_empty() {
            return Ok(ToolOutput::text("(empty directory)"));
        }
        let limit = args.limit.unwrap_or(DEFAULT_LIMIT).max(1);
        let total = entries.len();
        let limit_reached = total > limit;
        entries.truncate(limit);
        let truncation = truncate_head(&entries.join("\n"), usize::MAX, DEFAULT_MAX_BYTES);
        let mut text = truncation.content.clone();
        let mut notes = Vec::new();
        if limit_reached {
            notes.push(format!("{limit} of {total} entries shown. Use limit={} for more", limit * 2));
        }
        if truncation.truncated {
            notes.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
        }
        if !notes.is_empty() {
            text.push_str(&format!("\n\n[{}]", notes.join(". ")));
        }
        Ok(ToolOutput::text(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::tests_support::{context, noop};

    #[tokio::test]
    async fn lists_sorted_with_dir_suffix() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Zdir")).unwrap();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        std::fs::write(dir.path().join(".hidden"), "").unwrap();
        let out = LsTool.execute(&context(dir.path()), json!({}), noop()).await.unwrap();
        assert_eq!(crate::message::content_text(&out.content), ".hidden\na.txt\nZdir/");
    }
}
