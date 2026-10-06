//! Context compaction: summarize older history when the context window fills up.
//!
//! The most recent `keepRecentTokens` of conversation stay verbatim. Everything before the cut
//! point (plus any previous summary) is summarized by the current model into a structured
//! checkpoint that replaces it in later requests. The full history stays in the session file.

use std::collections::BTreeSet;

use serde::Serialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::config::{Model, ThinkingLevel};
use crate::message::{ContentBlock, Message, StopReason, Usage, UserMessage, content_text};
use crate::provider::{self, ProviderError, Request};
use crate::session::ContextItem;

const SUMMARIZATION_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.

Do NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";

const SUMMARY_FORMAT: &str = "## Goal
[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned by user]
- [Or \"(none)\" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Current work]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [Ordered list of what should happen next]

## Critical Context
- [Any data, examples, or references needed to continue]
- [Or \"(none)\" if not applicable]

Keep each section concise. Preserve exact file paths, function names, and error messages.";

const UPDATE_RULES: &str = "Update the existing structured summary with new information. RULES:
- PRESERVE all existing information from the previous summary
- ADD new progress, decisions, and context from the new messages
- UPDATE the Progress section: move items from \"In Progress\" to \"Done\" when completed
- UPDATE \"Next Steps\" based on what was accomplished
- PRESERVE exact file paths, function names, and error messages
- If something is no longer relevant, you may remove it";

const TOOL_RESULT_MAX_CHARS: usize = 2000;

/// Estimate the tokens the current context occupies: the last successful response's reported
/// usage plus an estimate for messages added after it.
pub fn estimate_context_tokens(messages: &[Message]) -> u64 {
    let last_usage = messages.iter().enumerate().rev().find_map(|(index, message)| match message {
        Message::Assistant(a)
            if !matches!(a.stop_reason, StopReason::Error | StopReason::Aborted) && a.usage.context_tokens() > 0 =>
        {
            Some((index, a.usage.context_tokens()))
        }
        _ => None,
    });
    match last_usage {
        Some((index, tokens)) => tokens + messages[index + 1..].iter().map(Message::estimate_tokens).sum::<u64>(),
        None => messages.iter().map(Message::estimate_tokens).sum(),
    }
}

pub fn should_compact(context_tokens: u64, context_window: u64, reserve_tokens: u64) -> bool {
    context_tokens > context_window.saturating_sub(reserve_tokens)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionResult {
    pub summary: String,
    pub first_kept_entry_id: Option<String>,
    pub tokens_before: u64,
    pub details: Value,
    pub usage: Usage,
}

/// What a compaction will summarize and keep.
pub struct Plan {
    pub to_summarize: Vec<Message>,
    pub previous_summary: Option<String>,
    /// First kept entry, or `None` when nothing is kept.
    pub first_kept_entry_id: Option<String>,
    pub tokens_before: u64,
    pub read_files: BTreeSet<String>,
    pub modified_files: BTreeSet<String>,
}

/// Choose the cut point: keep roughly `keep_recent_tokens` of recent messages, starting at a user
/// or assistant message so tool calls stay paired with their results.
pub fn plan(items: &[ContextItem], keep_recent_tokens: u64, previous_details: Option<&Value>) -> Option<Plan> {
    let (previous_summary, start) = match items.first().map(|i| &i.message) {
        Some(Message::CompactionSummary(summary)) => (Some(summary.summary.clone()), 1),
        _ => (None, 0),
    };
    let body = &items[start..];
    if body.is_empty() {
        return None;
    }

    let mut kept_tokens = 0;
    let mut cut = body.len();
    for (index, item) in body.iter().enumerate().rev() {
        kept_tokens += item.message.estimate_tokens();
        cut = index;
        if kept_tokens >= keep_recent_tokens {
            break;
        }
    }
    // Move forward to a valid boundary (never start the kept region with a tool result).
    while cut < body.len() && matches!(body[cut].message, Message::ToolResult(_)) {
        cut += 1;
    }
    if cut == 0 {
        return None;
    }

    let messages: Vec<Message> = items.iter().map(|i| i.message.clone()).collect();
    let mut read_files = BTreeSet::new();
    let mut modified_files = BTreeSet::new();
    if let Some(details) = previous_details {
        let strings = |key: &str| {
            details
                .get(key)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        };
        read_files.extend(strings("readFiles"));
        modified_files.extend(strings("modifiedFiles"));
    }
    let to_summarize: Vec<Message> = body[..cut].iter().map(|i| i.message.clone()).collect();
    for message in &to_summarize {
        if let Message::Assistant(assistant) = message {
            for call in assistant.tool_calls() {
                let Some(path) = call.arguments.get("path").and_then(Value::as_str) else { continue };
                match call.name {
                    "read" => {
                        read_files.insert(path.to_string());
                    }
                    "write" | "edit" => {
                        modified_files.insert(path.to_string());
                    }
                    _ => {}
                }
            }
        }
    }
    read_files.retain(|f| !modified_files.contains(f));

    Some(Plan {
        to_summarize,
        previous_summary,
        first_kept_entry_id: body.get(cut).and_then(|i| i.entry_id.clone()),
        tokens_before: estimate_context_tokens(&messages),
        read_files,
        modified_files,
    })
}

fn truncate_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((byte, _)) => {
            let remaining = text[byte..].chars().count();
            format!("{}\n\n[... {remaining} more characters truncated]", &text[..byte])
        }
        None => text.to_string(),
    }
}

/// Render messages as plain text so the summarizer does not treat them as a live conversation.
pub fn serialize_conversation(messages: &[Message]) -> String {
    let mut parts = Vec::new();
    for message in messages {
        match message {
            Message::User(user) => {
                let text = content_text(&user.content);
                if !text.is_empty() {
                    parts.push(format!("[User]: {text}"));
                }
            }
            Message::Assistant(assistant) => {
                let thinking: Vec<&str> = assistant
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Thinking { thinking, redacted: false, .. } if !thinking.is_empty() => {
                            Some(thinking.as_str())
                        }
                        _ => None,
                    })
                    .collect();
                if !thinking.is_empty() {
                    parts.push(format!("[Assistant thinking]: {}", thinking.join("\n")));
                }
                let text = assistant.text();
                if !text.is_empty() {
                    parts.push(format!("[Assistant]: {text}"));
                }
                let calls: Vec<String> = assistant
                    .tool_calls()
                    .map(|call| {
                        let args = match call.arguments {
                            Value::Object(map) => {
                                map.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(", ")
                            }
                            other => other.to_string(),
                        };
                        format!("{}({args})", call.name)
                    })
                    .collect();
                if !calls.is_empty() {
                    parts.push(format!("[Assistant tool calls]: {}", calls.join("; ")));
                }
            }
            Message::ToolResult(result) => {
                let text = content_text(&result.content);
                if !text.is_empty() {
                    parts.push(format!("[Tool result]: {}", truncate_chars(&text, TOOL_RESULT_MAX_CHARS)));
                }
            }
            Message::BashExecution(bash) if !bash.exclude_from_context => {
                parts.push(format!(
                    "[User ran command]: {}",
                    truncate_chars(&bash.to_context_text(), TOOL_RESULT_MAX_CHARS)
                ));
            }
            Message::BashExecution(_) => {}
            Message::CompactionSummary(summary) => parts.push(format!("[Earlier summary]: {}", summary.summary)),
        }
    }
    parts.join("\n\n")
}

fn summarization_prompt(plan: &Plan, instructions: Option<&str>) -> String {
    let mut prompt = format!("<conversation>\n{}\n</conversation>\n\n", serialize_conversation(&plan.to_summarize));
    match &plan.previous_summary {
        Some(previous) => {
            prompt.push_str(&format!("<previous-summary>\n{previous}\n</previous-summary>\n\n"));
            prompt.push_str("The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.\n\n");
            prompt.push_str(UPDATE_RULES);
            prompt.push_str("\n\nUse this EXACT format:\n\n");
        }
        None => {
            prompt.push_str("The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.\n\nUse this EXACT format:\n\n");
        }
    }
    prompt.push_str(SUMMARY_FORMAT);
    if let Some(instructions) = instructions.filter(|i| !i.trim().is_empty()) {
        prompt.push_str(&format!("\n\nAdditional focus for this summary: {}", instructions.trim()));
    }
    prompt
}

/// Generate the summary for `plan` with `model`.
pub async fn summarize(
    client: &reqwest::Client,
    model: &Model,
    plan: &Plan,
    instructions: Option<&str>,
    reserve_tokens: u64,
    cancel: &CancellationToken,
) -> Result<CompactionResult, ProviderError> {
    let messages =
        vec![Message::User(UserMessage::new(vec![ContentBlock::text(summarization_prompt(plan, instructions))]))];
    let request = Request {
        model,
        system_prompt: SUMMARIZATION_SYSTEM_PROMPT,
        messages: &messages,
        tools: &[],
        // Summaries need little reasoning; use the cheapest level the model supports.
        thinking: model.clamp_thinking(ThinkingLevel::Off),
        max_tokens: Some((reserve_tokens * 4 / 5).max(4_096).min(model.max_tokens.max(4_096))),
    };
    let mut response = provider::new_assistant_message(model, request.thinking);
    provider::stream(client, &request, &mut response, &mut |_, _| {}, cancel).await?;
    match response.stop_reason {
        StopReason::Error => {
            return Err(ProviderError::fatal(format!(
                "Summarization failed: {}",
                response.error_message.as_deref().unwrap_or("unknown error")
            )));
        }
        StopReason::Length => {
            return Err(ProviderError::fatal(
                "Summarization hit the output token limit; the summary would be incomplete",
            ));
        }
        _ => {}
    }
    let mut summary = response.text().trim().to_string();
    if summary.is_empty() {
        return Err(ProviderError::fatal("Summarization returned no text"));
    }
    if !plan.read_files.is_empty() {
        summary.push_str(&format!(
            "\n\n<read-files>\n{}\n</read-files>",
            plan.read_files.iter().cloned().collect::<Vec<_>>().join("\n")
        ));
    }
    if !plan.modified_files.is_empty() {
        summary.push_str(&format!(
            "\n\n<modified-files>\n{}\n</modified-files>",
            plan.modified_files.iter().cloned().collect::<Vec<_>>().join("\n")
        ));
    }
    Ok(CompactionResult {
        summary,
        first_kept_entry_id: plan.first_kept_entry_id.clone(),
        tokens_before: plan.tokens_before,
        details: json!({"readFiles": plan.read_files, "modifiedFiles": plan.modified_files}),
        usage: response.usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ToolResultMessage;

    fn item(id: &str, message: Message) -> ContextItem {
        ContextItem { entry_id: Some(id.into()), message }
    }

    fn user(text: &str) -> Message {
        Message::User(UserMessage::new(vec![ContentBlock::text(text)]))
    }

    fn tool_result(text: &str) -> Message {
        Message::ToolResult(ToolResultMessage {
            tool_call_id: "t".into(),
            tool_name: "bash".into(),
            content: vec![ContentBlock::text(text)],
            is_error: false,
            details: None,
            timestamp: 0,
            duration_ms: None,
        })
    }

    #[test]
    fn cut_point_skips_tool_results() {
        let big = "x".repeat(400);
        let items =
            vec![item("a", user(&big)), item("b", user(&big)), item("c", tool_result(&big)), item("d", user(&big))];
        // Each message is ~100 tokens; keeping 150 lands on the tool result, which moves forward.
        let plan = plan(&items, 150, None).unwrap();
        assert_eq!(plan.first_kept_entry_id.as_deref(), Some("d"));
        assert_eq!(plan.to_summarize.len(), 3);
    }

    #[test]
    fn nothing_to_compact_when_everything_fits() {
        let items = vec![item("a", user("hi"))];
        assert!(plan(&items, 20_000, None).is_none());
    }

    #[test]
    fn threshold() {
        assert!(should_compact(990_000, 1_000_000, 16_384));
        assert!(!should_compact(900_000, 1_000_000, 16_384));
    }
}
