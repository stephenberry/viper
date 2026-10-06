//! Built-in tools available to the model.

mod bash;
mod edit;
mod edit_diff;
mod find;
mod grep;
mod ls;
mod path;
mod read;
pub mod shell;
pub mod truncate;
mod write;

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::message::ContentBlock;
use crate::provider::ToolSpec;

/// Result of a tool execution.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    /// Content returned to the model.
    pub content: Vec<ContentBlock>,
    /// Structured data for the UI and JSON/RPC consumers.
    pub details: Option<Value>,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn text(text: impl Into<String>) -> Self {
        Self { content: vec![ContentBlock::text(text)], details: None, is_error: false }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self { content: vec![ContentBlock::text(text)], details: None, is_error: true }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }
}

/// Callback for streaming partial output (used by bash).
pub type UpdateFn = Arc<dyn Fn(ToolOutput) + Send + Sync>;

pub struct ToolContext {
    pub cwd: PathBuf,
    pub cancel: CancellationToken,
    pub shell: shell::ShellConfig,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> String;
    /// JSON schema of the arguments object.
    fn parameters(&self) -> Value;
    /// One-line summary for the system prompt's tool list.
    fn snippet(&self) -> &'static str;
    /// Usage rules added to the system prompt when this tool is enabled.
    fn guidelines(&self) -> &'static [&'static str] {
        &[]
    }
    /// Whether the tool only observes the workspace. Read-only calls run concurrently; any other
    /// call runs alone, after the calls before it and before the calls after it.
    fn read_only(&self) -> bool {
        false
    }
    /// Execute with raw arguments. Errors become error results for the model.
    async fn execute(&self, ctx: &ToolContext, args: Value, update: UpdateFn) -> anyhow::Result<ToolOutput>;

    fn spec(&self) -> ToolSpec {
        ToolSpec { name: self.name().to_string(), description: self.description(), parameters: self.parameters() }
    }
}

/// Deserialize tool arguments, producing an error message the model can act on.
pub fn parse_args<T: DeserializeOwned>(tool: &str, args: Value) -> anyhow::Result<T> {
    serde_json::from_value(args).map_err(|err| anyhow::anyhow!("Invalid arguments for {tool}: {err}"))
}

pub fn all_tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(read::ReadTool),
        Arc::new(bash::BashTool),
        Arc::new(edit::EditTool),
        Arc::new(write::WriteTool),
        Arc::new(grep::GrepTool),
        Arc::new(find::FindTool),
        Arc::new(ls::LsTool),
    ]
}

/// Tools enabled by name, in the canonical order. Unknown names are reported as errors.
pub fn select_tools(names: &[String]) -> anyhow::Result<Vec<Arc<dyn Tool>>> {
    let all = all_tools();
    for name in names {
        if !all.iter().any(|tool| tool.name() == name) {
            let known: Vec<&str> = all.iter().map(|t| t.name()).collect();
            anyhow::bail!("unknown tool '{name}' (available: {})", known.join(", "));
        }
    }
    Ok(all.into_iter().filter(|tool| names.iter().any(|n| n == tool.name())).collect())
}

pub use path::resolve_path;
pub use shell::{ShellConfig, run_shell_command};

#[cfg(test)]
pub(crate) mod tests_support {
    use std::path::Path;

    use super::*;

    pub fn context(cwd: &Path) -> ToolContext {
        ToolContext { cwd: cwd.to_path_buf(), cancel: CancellationToken::new(), shell: ShellConfig::resolve(None) }
    }

    pub fn noop() -> UpdateFn {
        Arc::new(|_| {})
    }
}
