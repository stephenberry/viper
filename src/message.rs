//! Conversation message model shared by providers, the agent loop, sessions, and the UI.
//!
//! Messages are provider-neutral. Each provider converts them to its wire format when building a
//! request. The serialized form (camelCase, tagged by `role` / `type`) is also the session file and
//! JSON/RPC event format.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        /// Provider-issued signature that must be replayed unchanged to continue the conversation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
        /// Redacted thinking: `signature` carries the opaque payload and `thinking` is a placeholder.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        redacted: bool,
    },
    Image {
        /// Base64-encoded image bytes.
        data: String,
        mime_type: String,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
        /// Raw argument text when the model produced JSON that could not be parsed. The tool is not
        /// executed; the model receives an error result instead.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        invalid_arguments: Option<String>,
    },
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        ContentBlock::Text { text: text.into() }
    }
}

/// Concatenate the text blocks of `content` with newlines.
pub fn content_text(content: &[ContentBlock]) -> String {
    let mut out = String::new();
    for block in content {
        if let ContentBlock::Text { text } = block {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        }
    }
    out
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    Stop,
    Length,
    ToolUse,
    Error,
    Aborted,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Cost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub total: f64,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Usage {
    /// Uncached input tokens.
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub total_tokens: u64,
    pub cost: Cost,
}

impl Usage {
    /// Tokens occupying the context window after this response.
    pub fn context_tokens(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write
    }

    pub fn add(&mut self, other: &Usage) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
        self.total_tokens += other.total_tokens;
        self.cost.input += other.cost.input;
        self.cost.output += other.cost.output;
        self.cost.cache_read += other.cost.cache_read;
        self.cost.cache_write += other.cost.cache_write;
        self.cost.total += other.cost.total;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserMessage {
    pub content: Vec<ContentBlock>,
    pub timestamp: i64,
}

impl UserMessage {
    pub fn new(content: Vec<ContentBlock>) -> Self {
        Self { content, timestamp: now_ms() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    pub content: Vec<ContentBlock>,
    pub api: crate::config::Api,
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub usage: Usage,
    pub stop_reason: StopReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<crate::config::ThinkingLevel>,
    pub timestamp: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

impl AssistantMessage {
    pub fn tool_calls(&self) -> impl Iterator<Item = ToolCallRef<'_>> {
        self.content.iter().filter_map(|block| match block {
            ContentBlock::ToolCall { id, name, arguments, invalid_arguments } => {
                Some(ToolCallRef { id, name, arguments, invalid_arguments: invalid_arguments.as_deref() })
            }
            _ => None,
        })
    }

    pub fn text(&self) -> String {
        content_text(&self.content)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ToolCallRef<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub arguments: &'a Value,
    pub invalid_arguments: Option<&'a str>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultMessage {
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: Vec<ContentBlock>,
    pub is_error: bool,
    /// Structured data for the UI and programmatic consumers. Never sent to the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    pub timestamp: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

/// Output of a user-initiated `!command`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashExecutionMessage {
    pub command: String,
    pub output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub cancelled: bool,
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
    /// `!!command`: shown to the user but not sent to the model.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub exclude_from_context: bool,
    pub timestamp: i64,
}

impl BashExecutionMessage {
    /// The user-message text the model sees for this execution.
    pub fn to_context_text(&self) -> String {
        let mut text = format!("Ran `{}`\n", self.command);
        if self.output.is_empty() {
            text.push_str("(no output)");
        } else {
            text.push_str(&format!("```\n{}\n```", self.output));
        }
        if self.cancelled {
            text.push_str("\n\n(command cancelled)");
        } else if let Some(code) = self.exit_code.filter(|code| *code != 0) {
            text.push_str(&format!("\n\nCommand exited with code {code}"));
        }
        if self.truncated
            && let Some(path) = &self.full_output_path
        {
            text.push_str(&format!("\n\n[Output truncated. Full output: {path}]"));
        }
        text
    }
}

/// Summary that replaces compacted history in the model context.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionSummaryMessage {
    pub summary: String,
    pub tokens_before: u64,
    pub timestamp: i64,
}

pub const COMPACTION_SUMMARY_PREFIX: &str =
    "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";

impl CompactionSummaryMessage {
    pub fn to_context_text(&self) -> String {
        format!("{COMPACTION_SUMMARY_PREFIX}{}{COMPACTION_SUMMARY_SUFFIX}", self.summary)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum Message {
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
    BashExecution(BashExecutionMessage),
    CompactionSummary(CompactionSummaryMessage),
}

impl Message {
    /// Rough token estimate (4 characters per token, images at a fixed cost).
    pub fn estimate_tokens(&self) -> u64 {
        fn blocks(content: &[ContentBlock]) -> u64 {
            content
                .iter()
                .map(|block| match block {
                    ContentBlock::Text { text } => text.len() as u64,
                    ContentBlock::Thinking { thinking, .. } => thinking.len() as u64,
                    // Anthropic bills roughly (w*h)/750 tokens; ~1.2k tokens is typical after resizing.
                    ContentBlock::Image { .. } => 4800,
                    ContentBlock::ToolCall { name, arguments, .. } => (name.len() + arguments.to_string().len()) as u64,
                })
                .sum()
        }
        let chars = match self {
            Message::User(m) => blocks(&m.content),
            Message::Assistant(m) => blocks(&m.content),
            Message::ToolResult(m) => blocks(&m.content),
            Message::BashExecution(m) => (m.command.len() + m.output.len()) as u64,
            Message::CompactionSummary(m) => m.summary.len() as u64,
        };
        chars.div_ceil(4)
    }
}
