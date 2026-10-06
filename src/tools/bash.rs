use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::shell::run_shell_command;
use super::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncatedBy, format_size};
use super::{Tool, ToolContext, ToolOutput, UpdateFn, parse_args};

pub struct BashTool;

#[derive(Deserialize)]
struct Args {
    command: String,
    /// Seconds.
    timeout: Option<f64>,
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "bash"
    }

    fn description(&self) -> String {
        format!(
            "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated \
             to last {DEFAULT_MAX_LINES} lines or {}KB (whichever is hit first). If truncated, full output is saved to \
             a temp file. Optionally provide a timeout in seconds.",
            DEFAULT_MAX_BYTES / 1024
        )
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Shell command to execute"},
                "timeout": {"type": "number", "description": "Timeout in seconds (optional, no default timeout)"}
            },
            "required": ["command"],
            "additionalProperties": false
        })
    }

    fn snippet(&self) -> &'static str {
        "Execute bash commands (ls, grep, find, etc.)"
    }

    async fn execute(&self, ctx: &ToolContext, args: Value, update: UpdateFn) -> anyhow::Result<ToolOutput> {
        let args: Args = parse_args("bash", args)?;
        let timeout = match args.timeout {
            None => None,
            Some(secs) if secs.is_finite() && secs > 0.0 && secs <= 86_400.0 * 7.0 => {
                Some(Duration::from_secs_f64(secs))
            }
            Some(_) => anyhow::bail!("Invalid timeout: must be a positive number of seconds"),
        };
        let result = run_shell_command(&ctx.shell, &args.command, &ctx.cwd, timeout, &ctx.cancel, |snapshot| {
            update(ToolOutput::text(snapshot.content.clone()));
        })
        .await?;

        let mut text = result.output;
        let t = &result.truncation;
        if t.truncated {
            let path = result
                .full_output_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(unavailable)".into());
            let start = t.total_lines.saturating_sub(t.output_lines) + 1;
            let note = if t.last_line_partial {
                format!("[Showing last {} of line {}. Full output: {path}]", format_size(t.output_bytes), t.total_lines)
            } else if t.truncated_by == Some(TruncatedBy::Lines) {
                format!("[Showing lines {start}-{} of {}. Full output: {path}]", t.total_lines, t.total_lines)
            } else {
                format!(
                    "[Showing lines {start}-{} of {} ({} limit). Full output: {path}]",
                    t.total_lines,
                    t.total_lines,
                    format_size(DEFAULT_MAX_BYTES)
                )
            };
            text = format!("{text}\n\n{note}");
        }
        let details = json!({
            "exitCode": result.exit_code,
            "durationMs": result.duration.as_millis() as u64,
            "truncation": t.truncated.then_some(t),
            "fullOutputPath": result.full_output_path,
        });
        // A failed command reports why after its output.
        let failure = if result.cancelled {
            Some("Command aborted".to_string())
        } else if result.timed_out {
            Some(format!("Command timed out after {} seconds", args.timeout.unwrap_or_default()))
        } else {
            match result.exit_code {
                Some(0) => None,
                Some(code) => Some(format!("Command exited with code {code}")),
                None => Some("Command terminated without an exit code".to_string()),
            }
        };
        let output = match failure {
            None if text.is_empty() => ToolOutput::text("(no output)"),
            None => ToolOutput::text(text),
            Some(status) if text.is_empty() => ToolOutput::error(status),
            Some(status) => ToolOutput::error(format!("{text}\n\n{status}")),
        };
        Ok(output.with_details(details))
    }
}
