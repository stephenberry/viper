//! Rendering of transcript items into styled lines (unwrapped; the app wraps to the width).

use serde_json::Value;

use super::markdown::MarkdownRenderer;
use super::style::*;
use super::text::sanitize;
use crate::agent::ToolResultView;
use crate::message::{AssistantMessage, BashExecutionMessage, ContentBlock, Message, StopReason, content_text};

/// A logical line plus the prefixes used when it wraps.
#[derive(Debug, Clone)]
pub struct Line {
    pub text: String,
    pub first: &'static str,
    pub rest: &'static str,
}

impl Line {
    pub fn plain(text: impl Into<String>) -> Line {
        Line { text: text.into(), first: "", rest: "" }
    }

    pub fn indented(text: impl Into<String>, first: &'static str, rest: &'static str) -> Line {
        Line { text: text.into(), first, rest }
    }
}

pub const BODY_FIRST: &str = "  ⎿ ";
pub const BODY_REST: &str = "    ";

pub fn user_lines(content: &[ContentBlock]) -> Vec<Line> {
    let text = sanitize(&content_text(content));
    let images = content.iter().filter(|b| matches!(b, ContentBlock::Image { .. })).count();
    let mut lines: Vec<Line> = text
        .lines()
        .enumerate()
        .map(|(i, l)| {
            Line::indented(format!("{BOLD}{l}{RESET}"), if i == 0 { "\u{1b}[36m›\u{1b}[0m " } else { "  " }, "  ")
        })
        .collect();
    if images > 0 {
        let label = if images == 1 { "[1 image]".to_string() } else { format!("[{images} images]") };
        let prefix = if lines.is_empty() { "\u{1b}[36m›\u{1b}[0m " } else { "  " };
        lines.push(Line::indented(dim(&label), prefix, "  "));
    }
    lines
}

pub fn thinking_lines(text: &str) -> Vec<Line> {
    text.lines().map(|l| Line::indented(format!("{GRAY}{ITALIC}{}{RESET}", sanitize(l)), "  ", "  ")).collect()
}

pub fn markdown_lines(text: &str) -> Vec<Line> {
    let mut md = MarkdownRenderer::default();
    text.lines().map(|l| Line::plain(md.render_line(&sanitize(l)))).collect()
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

/// Short description of a tool call's arguments for its header line.
pub fn tool_summary(name: &str, args: &Value) -> String {
    let get = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or("");
    let summary = match name {
        "bash" => {
            let command = get("command");
            let more = if command.lines().count() > 1 { " …" } else { "" };
            format!("{}{more}", first_line(command))
        }
        "read" => {
            let mut s = get("path").to_string();
            let offset = args.get("offset").and_then(Value::as_u64);
            let limit = args.get("limit").and_then(Value::as_u64);
            match (offset, limit) {
                (Some(o), Some(l)) => s.push_str(&format!(":{o}-{}", o + l.saturating_sub(1))),
                (Some(o), None) => s.push_str(&format!(":{o}-")),
                (None, Some(l)) => s.push_str(&format!(":1-{l}")),
                (None, None) => {}
            }
            s
        }
        "edit" | "write" => get("path").to_string(),
        "grep" => {
            let mut s = format!("/{}/", get("pattern"));
            if let Some(glob) = args.get("glob").and_then(Value::as_str) {
                s.push_str(&format!(" {glob}"));
            }
            if let Some(path) = args.get("path").and_then(Value::as_str) {
                s.push_str(&format!(" in {path}"));
            }
            s
        }
        "find" => {
            let mut s = get("pattern").to_string();
            if let Some(path) = args.get("path").and_then(Value::as_str) {
                s.push_str(&format!(" in {path}"));
            }
            s
        }
        "ls" => args.get("path").and_then(Value::as_str).unwrap_or(".").to_string(),
        _ => serde_json::to_string(args).unwrap_or_default(),
    };
    sanitize(&summary).replace('\n', " ")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolState {
    Running,
    Done,
    Failed,
}

pub fn tool_header(name: &str, args: &Value, state: ToolState, spinner: &str) -> Line {
    let bullet = match state {
        ToolState::Running => format!("{YELLOW}{spinner}{RESET}"),
        ToolState::Done => format!("{GREEN}●{RESET}"),
        ToolState::Failed => format!("{RED}●{RESET}"),
    };
    Line::indented(format!("{bullet} {BOLD}{name}{RESET} {}", tool_summary(name, args)), "", "    ")
}

fn body(lines: impl IntoIterator<Item = String>) -> Vec<Line> {
    lines
        .into_iter()
        .enumerate()
        .map(|(i, l)| Line::indented(l, if i == 0 { BODY_FIRST } else { BODY_REST }, BODY_REST))
        .collect()
}

fn limited(text: &str, max: usize, tail: bool, style: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= max {
        return lines.iter().map(|l| paint(style, &sanitize(l))).collect();
    }
    let hidden = lines.len() - max;
    let note = dim(&format!("… +{hidden} lines"));
    if tail {
        let mut out = vec![note];
        out.extend(lines[hidden..].iter().map(|l| paint(style, &sanitize(l))));
        out
    } else {
        let mut out: Vec<String> = lines[..max].iter().map(|l| paint(style, &sanitize(l))).collect();
        out.push(note);
        out
    }
}

fn diff_lines(diff: &str, max: usize) -> Vec<String> {
    let mut out: Vec<String> = diff
        .lines()
        .take(max)
        .map(|l| {
            let l = sanitize(l);
            match l.chars().next() {
                Some('+') => paint(GREEN, &l),
                Some('-') => paint(RED, &l),
                _ => dim(&l),
            }
        })
        .collect();
    let total = diff.lines().count();
    if total > max {
        out.push(dim(&format!("… +{} lines", total - max)));
    }
    out
}

/// Body lines for a finished tool call.
pub fn tool_body(name: &str, args: &Value, result: &ToolResultView, is_error: bool, max_lines: usize) -> Vec<Line> {
    let text = content_text(&result.content);
    let images = result.content.iter().filter(|b| matches!(b, ContentBlock::Image { .. })).count();
    if is_error {
        return body(limited(&text, max_lines, name == "bash", RED));
    }
    let details = result.details.as_ref();
    match name {
        "bash" => {
            if text.trim().is_empty() || text == "(no output)" {
                body([dim("(no output)")])
            } else {
                body(limited(&text, max_lines, true, GRAY))
            }
        }
        "read" => {
            if images > 0 {
                body([dim(first_line(&text))])
            } else {
                let count = text.split("\n\n[").next().unwrap_or("").lines().count();
                body([dim(&format!("Read {count} lines"))])
            }
        }
        "edit" => match details.and_then(|d| d.get("diff")).and_then(Value::as_str) {
            Some(diff) => body(diff_lines(diff, max_lines * 3)),
            None => body([dim(&text)]),
        },
        "write" => {
            let content = args.get("content").and_then(Value::as_str).unwrap_or("");
            let mut lines = vec![dim(&format!("Wrote {} lines", content.lines().count()))];
            lines.extend(limited(content, max_lines, false, GRAY));
            body(lines)
        }
        _ => body(limited(&text, max_lines, false, GRAY)),
    }
}

pub fn bash_execution_lines(message: &BashExecutionMessage, max_lines: usize) -> Vec<Line> {
    let marker = if message.exclude_from_context { "!!" } else { "!" };
    let mut lines =
        vec![Line::indented(format!("{MAGENTA}{marker}{RESET} {BOLD}{}{RESET}", sanitize(&message.command)), "", "  ")];
    let mut body_lines = if message.output.is_empty() {
        vec![dim("(no output)")]
    } else {
        limited(&message.output, max_lines, true, GRAY)
    };
    if message.cancelled {
        body_lines.push(paint(YELLOW, "(cancelled)"));
    } else if let Some(code) = message.exit_code.filter(|c| *c != 0) {
        body_lines.push(paint(RED, &format!("exit code {code}")));
    }
    lines.extend(body(body_lines));
    lines
}

pub fn error_line(text: &str) -> Line {
    Line::indented(format!("{RED}✗ {}{RESET}", sanitize(text)), "", "  ")
}

pub fn notice_line(text: &str) -> Line {
    Line::indented(dim(text), "", "")
}

/// Lines for a whole assistant message (used when replaying a session).
pub fn assistant_lines(message: &AssistantMessage, hide_thinking: bool) -> Vec<Line> {
    let mut lines = Vec::new();
    for block in &message.content {
        match block {
            ContentBlock::Text { text } if !text.trim().is_empty() => {
                lines.push(Line::plain(""));
                lines.extend(markdown_lines(text.trim_end()));
            }
            ContentBlock::Thinking { thinking, redacted, .. } if !thinking.trim().is_empty() => {
                lines.push(Line::plain(""));
                if hide_thinking || *redacted {
                    lines.push(Line::indented(format!("{GRAY}{ITALIC}∴ Thinking…{RESET}"), "", ""));
                } else {
                    lines.extend(thinking_lines(thinking.trim_end()));
                }
            }
            _ => {}
        }
    }
    match message.stop_reason {
        StopReason::Error => lines.push(error_line(message.error_message.as_deref().unwrap_or("request failed"))),
        StopReason::Aborted => lines.push(Line::indented(paint(YELLOW, "⏹ Interrupted"), "", "")),
        StopReason::Length => lines.push(Line::indented(paint(YELLOW, "Response hit the output token limit"), "", "")),
        _ => {}
    }
    lines
}

/// Render a saved conversation for display after resuming.
pub fn transcript_lines(messages: &[Message], hide_thinking: bool, max_tool_lines: usize) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut calls: std::collections::HashMap<String, (String, Value)> = Default::default();
    for message in messages {
        match message {
            Message::User(user) => {
                lines.push(Line::plain(""));
                lines.extend(user_lines(&user.content));
            }
            Message::Assistant(assistant) => {
                for call in assistant.tool_calls() {
                    calls.insert(call.id.to_string(), (call.name.to_string(), call.arguments.clone()));
                }
                lines.extend(assistant_lines(assistant, hide_thinking));
            }
            Message::ToolResult(result) => {
                let (name, args) =
                    calls.get(&result.tool_call_id).cloned().unwrap_or((result.tool_name.clone(), Value::Null));
                let state = if result.is_error { ToolState::Failed } else { ToolState::Done };
                lines.push(Line::plain(""));
                lines.push(tool_header(&name, &args, state, ""));
                let view = ToolResultView { content: result.content.clone(), details: result.details.clone() };
                lines.extend(tool_body(&name, &args, &view, result.is_error, max_tool_lines));
            }
            Message::BashExecution(bash) => {
                lines.push(Line::plain(""));
                lines.extend(bash_execution_lines(bash, max_tool_lines));
            }
            Message::CompactionSummary(summary) => {
                lines.push(Line::plain(""));
                lines.push(notice_line(&format!(
                    "◇ Earlier conversation compacted ({} tokens summarized)",
                    summary.tokens_before
                )));
            }
        }
    }
    lines
}
