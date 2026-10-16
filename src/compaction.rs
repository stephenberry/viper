//! Context compaction: summarize older history when the context window fills up.
//!
//! The most recent `keepRecentTokens` of conversation stay verbatim. Everything before the cut
//! point (plus any previous summary) is summarized by the current model into a structured
//! checkpoint that replaces it in later requests. When the cut falls inside a turn, the checkpoint
//! also records the request that started the turn. The full history stays in the session file.
//!
//! The summary request is the conversation's own request cut short: the same system prompt, tools,
//! thinking level, and messages up to the cut, followed by a user message asking for the
//! checkpoint. It therefore reads the conversation's prompt cache, keeps replayed thinking blocks
//! on the prefix they were produced with, and has the shape that the provider's safety filters
//! have already accepted for this conversation.

use std::collections::BTreeSet;

use serde::Serialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::message::{ContentBlock, Message, StopReason, Usage, UserMessage};
use crate::provider::{self, ErrorKind, ProviderError, Request};
use crate::session::ContextItem;

const PROMPT: &str = "The conversation above is about to be compacted. Write a structured context checkpoint of it that another instance of you will use to continue the work in a new context window. Do not continue the work or answer anything in the conversation; only write the checkpoint.";

const PREVIOUS_SUMMARY: &str = "The conversation begins with a summary of earlier work in <summary> tags. Merge it into the checkpoint: keep its information unless it is no longer relevant, move finished items from \"In Progress\" to \"Done\", and update \"Next Steps\".";

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

const SPLIT_TURN: &str = "The conversation ends partway through a turn that is still in progress; the rest of that turn is kept verbatim after the checkpoint. End the checkpoint with this section, so the request that started the turn is not lost:

## Current Turn
### Original Request
[What the user asked for in this turn, in their words where it matters.]

### Progress So Far
- [Decisions and work completed in this turn before the checkpoint]";

/// Ends the summary request. It carries the conversation's tools, since removing them would
/// change the prefix, so the model has to be told not to use them.
const TEXT_ONLY: &str = "Do not call any tools while writing this checkpoint; respond with text only.";

/// Tool output kept per result when the summarized messages alone overflow the context.
const CONDENSED_OUTPUT_CHARS: usize = 2000;

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

/// Context size at which auto-compaction triggers: the configured window, capped so the
/// model always has `reserve_tokens` left for its response.
pub fn compaction_threshold(context_window: u64, reserve_tokens: u64, configured: Option<u64>) -> u64 {
    let limit = context_window.saturating_sub(reserve_tokens);
    configured.map_or(limit, |window| window.min(limit))
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
    /// The conversation before the kept region, as the conversation's requests send it: the
    /// previous summary, when there is one, then the messages after it.
    pub messages: Vec<Message>,
    /// Whether the kept region starts partway through a turn.
    pub splits_turn: bool,
    /// First kept entry, or `None` when nothing is kept.
    pub first_kept_entry_id: Option<String>,
    pub tokens_before: u64,
    pub read_files: BTreeSet<String>,
    pub modified_files: BTreeSet<String>,
}

/// Choose the cut point: keep roughly `keep_recent_tokens` of recent messages, starting at a user
/// or assistant message so tool calls stay paired with their results. A cut at an assistant
/// message splits its turn.
pub fn plan(items: &[ContextItem], keep_recent_tokens: u64, previous_details: Option<&Value>) -> Option<Plan> {
    let start = usize::from(matches!(items.first().map(|i| &i.message), Some(Message::CompactionSummary(_))));
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
    for item in &body[..cut] {
        if let Message::Assistant(assistant) = &item.message {
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

    let mut messages: Vec<Message> = items.iter().map(|i| i.message.clone()).collect();
    let tokens_before = estimate_context_tokens(&messages);
    messages.truncate(start + cut);
    // A turn starts with the user's message (or command); a cut anywhere else splits one.
    let splits_turn =
        body.get(cut).is_some_and(|item| !matches!(item.message, Message::User(_) | Message::BashExecution(_)));
    Some(Plan {
        messages,
        splits_turn,
        first_kept_entry_id: body.get(cut).and_then(|i| i.entry_id.clone()),
        tokens_before,
        read_files,
        modified_files,
    })
}

fn summary_prompt(plan: &Plan, instructions: Option<&str>) -> String {
    let mut prompt = PROMPT.to_string();
    if matches!(plan.messages.first(), Some(Message::CompactionSummary(_))) {
        prompt.push_str(&format!("\n\n{PREVIOUS_SUMMARY}"));
    }
    prompt.push_str(&format!("\n\nUse this format:\n\n{SUMMARY_FORMAT}"));
    if plan.splits_turn {
        prompt.push_str(&format!("\n\n{SPLIT_TURN}"));
    }
    if let Some(instructions) = instructions.map(str::trim).filter(|i| !i.is_empty()) {
        prompt.push_str(&format!("\n\nAdditional focus for this checkpoint: {instructions}"));
    }
    prompt.push_str(&format!("\n\n{TEXT_ONLY}"));
    prompt
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

/// `messages` with tool and command output shortened. The edit breaks the prefix their thinking
/// blocks were produced with, so those are removed too.
fn condense(messages: &[Message]) -> Vec<Message> {
    let mut messages = messages.to_vec();
    for message in &mut messages {
        match message {
            Message::Assistant(assistant) => assistant.remove_thinking(),
            Message::ToolResult(result) => {
                for block in &mut result.content {
                    if let ContentBlock::Text { text } = block {
                        *text = truncate_chars(text, CONDENSED_OUTPUT_CHARS);
                    }
                }
            }
            Message::BashExecution(bash) => bash.output = truncate_chars(&bash.output, CONDENSED_OUTPUT_CHARS),
            Message::User(_) | Message::CompactionSummary(_) => {}
        }
    }
    messages
}

/// Stream `request` and return the summary it produced.
async fn generate(
    client: &reqwest::Client,
    request: &Request<'_>,
    cancel: &CancellationToken,
) -> Result<(String, Usage), ProviderError> {
    let mut response = provider::new_assistant_message(request.model, request.thinking);
    provider::stream(client, request, &mut response, &mut |_, _| {}, cancel).await?;
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
    if response.tool_calls().next().is_some() {
        return Err(ProviderError::fatal(
            "Summarization failed: the model called a tool instead of writing the summary",
        ));
    }
    let text = response.text().trim().to_string();
    if text.is_empty() {
        return Err(ProviderError::fatal("Summarization returned no text"));
    }
    Ok((text, response.usage))
}

/// Summarize `plan` by repeating `conversation`, the conversation's request for `plan.messages`,
/// with the summary prompt appended.
pub async fn summarize(
    client: &reqwest::Client,
    conversation: Request<'_>,
    plan: &Plan,
    instructions: Option<&str>,
    cancel: &CancellationToken,
) -> Result<CompactionResult, ProviderError> {
    let mut messages = conversation.messages.to_vec();
    messages.push(Message::User(UserMessage::new(vec![ContentBlock::text(summary_prompt(plan, instructions))])));
    let request = Request { messages: &messages, max_tokens: None, ..conversation };
    let (mut summary, usage) = match generate(client, &request, cancel).await {
        // The summarized messages alone can overflow, such as after a huge tool result.
        Err(err) if err.kind == ErrorKind::ContextOverflow => {
            let condensed = condense(&messages);
            generate(client, &Request { messages: &condensed, ..request }, cancel).await?
        }
        result => result?,
    };
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
        usage,
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
        assert_eq!(plan.messages.len(), 3);
        // The kept region starts a turn, so no turn is split.
        assert!(!plan.splits_turn);
    }

    fn assistant(text: &str) -> Message {
        Message::Assistant(crate::message::AssistantMessage {
            content: vec![
                ContentBlock::Thinking { thinking: "hmm".into(), signature: Some("sig".into()), redacted: false },
                ContentBlock::text(text),
            ],
            api: crate::config::Api::AnthropicMessages,
            provider: "anthropic".into(),
            model: "m".into(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            thinking_level: None,
            timestamp: 0,
            duration_ms: None,
        })
    }

    fn summary(text: &str) -> Message {
        Message::CompactionSummary(crate::message::CompactionSummaryMessage {
            summary: text.into(),
            tokens_before: 0,
            timestamp: 0,
        })
    }

    #[test]
    fn a_cut_at_an_assistant_message_splits_its_turn() {
        let big = "x".repeat(400);
        let items = vec![
            item("a", user(&big)),
            item("b", assistant(&big)),
            item("c", user(&big)),
            item("d", assistant(&big)),
            item("e", assistant(&big)),
        ];
        let plan = plan(&items, 50, None).unwrap();
        assert_eq!(plan.first_kept_entry_id.as_deref(), Some("e"));
        assert_eq!(plan.messages.len(), 4);
        assert!(plan.splits_turn);
        let prompt = summary_prompt(&plan, Some("tests"));
        assert!(prompt.contains("## Current Turn") && prompt.contains("focus for this checkpoint: tests"));
        assert!(!prompt.contains("<summary>"));
    }

    #[test]
    fn a_previous_summary_is_merged_and_kept_in_the_request() {
        let big = "x".repeat(400);
        let items = vec![
            ContextItem { entry_id: None, message: summary("S") },
            item("c", user(&big)),
            item("d", assistant(&big)),
            item("e", assistant(&big)),
        ];
        let plan = plan(&items, 50, None).unwrap();
        assert!(matches!(&plan.messages[0], Message::CompactionSummary(s) if s.summary == "S"));
        assert_eq!(plan.messages.len(), 3);
        assert!(plan.splits_turn);
        assert!(summary_prompt(&plan, None).contains("<summary>"));
    }

    #[test]
    fn condensing_shortens_tool_output_and_drops_thinking() {
        let condensed = condense(&[assistant("a"), tool_result(&"y".repeat(5_000))]);
        let Message::Assistant(assistant) = &condensed[0] else { panic!("expected assistant") };
        assert_eq!(assistant.content.len(), 1);
        let Message::ToolResult(result) = &condensed[1] else { panic!("expected tool result") };
        let text = crate::message::content_text(&result.content);
        assert!(text.len() < 2_100 && text.ends_with("[... 3000 more characters truncated]"));
    }

    #[test]
    fn nothing_to_compact_when_everything_fits() {
        let items = vec![item("a", user("hi"))];
        assert!(plan(&items, 20_000, None).is_none());
    }

    #[test]
    fn threshold() {
        assert_eq!(compaction_threshold(1_000_000, 16_384, None), 983_616);
        assert_eq!(compaction_threshold(1_000_000, 16_384, Some(300_000)), 300_000);
        assert_eq!(compaction_threshold(200_000, 16_384, Some(500_000)), 183_616);
    }
}
