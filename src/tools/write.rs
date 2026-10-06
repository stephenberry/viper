use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::path::resolve_path;
use super::{Tool, ToolContext, ToolOutput, UpdateFn, parse_args};

pub struct WriteTool;

#[derive(Deserialize)]
struct Args {
    path: String,
    content: String,
}

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &'static str {
        "write"
    }

    fn description(&self) -> String {
        "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates \
         parent directories."
            .to_string()
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the file to write (relative or absolute)"},
                "content": {"type": "string", "description": "Content to write to the file"}
            },
            "required": ["path", "content"],
            "additionalProperties": false
        })
    }

    fn snippet(&self) -> &'static str {
        "Create or overwrite files"
    }

    fn guidelines(&self) -> &'static [&'static str] {
        &["Use write only for new files or complete rewrites."]
    }

    async fn execute(&self, ctx: &ToolContext, args: Value, _update: UpdateFn) -> anyhow::Result<ToolOutput> {
        let args: Args = parse_args("write", args)?;
        let path = resolve_path(&args.path, &ctx.cwd);
        if ctx.cancel.is_cancelled() {
            anyhow::bail!("Operation aborted");
        }
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|err| anyhow::anyhow!("Could not create directory {}: {err}", parent.display()))?;
        }
        let existed = path.exists();
        tokio::fs::write(&path, &args.content)
            .await
            .map_err(|err| anyhow::anyhow!("Could not write {}: {err}", args.path))?;
        let lines = args.content.lines().count();
        Ok(ToolOutput::text(format!("Successfully wrote {} bytes to {}", args.content.len(), args.path))
            .with_details(json!({"created": !existed, "lines": lines})))
    }
}
