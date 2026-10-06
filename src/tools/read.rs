use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::path::resolve_existing_path;
use super::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, format_size, truncate_head};
use super::{Tool, ToolContext, ToolOutput, UpdateFn, parse_args};
use crate::message::ContentBlock;

pub struct ReadTool;

#[derive(Deserialize)]
struct Args {
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &'static str {
        "read"
    }

    fn description(&self) -> String {
        format!(
            "Read the contents of a file. Supports text files and images (jpg, png, webp). Images are sent as \
             attachments. For text files, output is truncated to {DEFAULT_MAX_LINES} lines or {}KB (whichever is hit \
             first). Use offset/limit for large files. When you need the full file, continue with offset until complete.",
            DEFAULT_MAX_BYTES / 1024
        )
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the file to read (relative or absolute)"},
                "offset": {"type": "integer", "minimum": 1, "description": "Line number to start reading from (1-indexed)"},
                "limit": {"type": "integer", "minimum": 1, "description": "Maximum number of lines to read"}
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    fn snippet(&self) -> &'static str {
        "Read file contents"
    }

    fn read_only(&self) -> bool {
        true
    }

    fn guidelines(&self) -> &'static [&'static str] {
        &["Use read to examine files instead of cat or sed."]
    }

    async fn execute(&self, ctx: &ToolContext, args: Value, _update: UpdateFn) -> anyhow::Result<ToolOutput> {
        let args: Args = parse_args("read", args)?;
        let path = resolve_existing_path(&args.path, &ctx.cwd);
        let bytes =
            tokio::fs::read(&path).await.map_err(|err| anyhow::anyhow!("Could not read {}: {err}", args.path))?;

        if let Some(mime) = crate::images::detect_mime(&bytes) {
            let prepared = tokio::task::spawn_blocking(move || crate::images::prepare(&bytes)).await?;
            return Ok(match prepared {
                Ok(prepared) => {
                    let mut note = format!("Read image file [{mime}]");
                    for line in &prepared.notes {
                        note.push('\n');
                        note.push_str(line);
                    }
                    ToolOutput {
                        content: vec![ContentBlock::text(note), prepared.block],
                        details: None,
                        is_error: false,
                    }
                }
                Err(err) => {
                    ToolOutput::text(format!("Read image file [{mime}]\nThe image could not be processed: {err:#}"))
                }
            });
        }

        let text = String::from_utf8_lossy(&bytes);
        let text = text.strip_prefix('\u{FEFF}').unwrap_or(&text);
        // A trailing newline ends the last line rather than starting another.
        let lines: Vec<&str> = text.strip_suffix('\n').unwrap_or(text).split('\n').collect();
        let total = lines.len();
        let start = args.offset.unwrap_or(1).max(1) - 1;
        if start >= total {
            anyhow::bail!("Offset {} is beyond end of file ({total} lines total)", start + 1);
        }
        let end = match args.limit {
            Some(limit) => (start + limit).min(total),
            None => total,
        };
        let selected = lines[start..end].join("\n");
        let truncation = truncate_head(&selected, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
        let first = start + 1;

        let output = if truncation.first_line_exceeds_limit {
            format!(
                "[Line {first} is {}, exceeds {} limit. Use bash: sed -n '{first}p' {} | head -c {DEFAULT_MAX_BYTES}]",
                format_size(lines[start].len()),
                format_size(DEFAULT_MAX_BYTES),
                args.path
            )
        } else if truncation.truncated {
            let last = first + truncation.output_lines - 1;
            let limit_note = match truncation.truncated_by {
                Some(super::truncate::TruncatedBy::Bytes) => format!(" ({} limit)", format_size(DEFAULT_MAX_BYTES)),
                _ => String::new(),
            };
            format!(
                "{}\n\n[Showing lines {first}-{last} of {total}{limit_note}. Use offset={} to continue.]",
                truncation.content,
                last + 1
            )
        } else if end < total {
            format!(
                "{}\n\n[{} more lines in file. Use offset={} to continue.]",
                truncation.content,
                total - end,
                end + 1
            )
        } else {
            truncation.content.clone()
        };

        let details = truncation.truncated.then(|| json!({"truncation": truncation}));
        Ok(ToolOutput { content: vec![ContentBlock::text(output)], details, is_error: false })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::tests_support::context;

    #[tokio::test]
    async fn reads_with_offset_and_limit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "1\n2\n3\n4\n5\n").unwrap();
        let ctx = context(dir.path());
        let out = ReadTool
            .execute(&ctx, json!({"path": "f.txt", "offset": 2, "limit": 2}), crate::tools::tests_support::noop())
            .await
            .unwrap();
        assert_eq!(
            crate::message::content_text(&out.content),
            "2\n3\n\n[2 more lines in file. Use offset=4 to continue.]"
        );
        let err =
            ReadTool.execute(&ctx, json!({"path": "f.txt", "offset": 6}), crate::tools::tests_support::noop()).await;
        assert!(err.unwrap_err().to_string().contains("beyond end of file (5 lines total)"));
    }
}
