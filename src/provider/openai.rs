//! OpenAI-compatible Chat Completions (`POST /v1/chat/completions`), used with LiteLLM.
//!
//! LiteLLM extensions are supported where they matter for Claude models: `reasoning_content` and
//! `thinking_blocks` carry reasoning (with signatures replayed on later turns), and
//! `cache_control` on content parts enables prompt caching.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::{
    DeltaSink, ProviderError, Request, StreamDelta, apply_auth, finish_tool_call, merge_extra_body, normalize_messages,
    same_model, send, sse, v1_url,
};
use crate::config::{Model, Reasoning, ThinkingLevel};
use crate::message::{AssistantMessage, ContentBlock, Message, StopReason, content_text};

fn content_parts(content: &[ContentBlock]) -> Vec<Value> {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } if !text.is_empty() => Some(json!({"type": "text", "text": text})),
            ContentBlock::Image { data, mime_type } => {
                Some(json!({"type": "image_url", "image_url": {"url": format!("data:{mime_type};base64,{data}")}}))
            }
            _ => None,
        })
        .collect()
}

fn text_message(role: &str, text: String) -> Value {
    json!({"role": role, "content": [{"type": "text", "text": text}]})
}

fn assistant_message(message: &AssistantMessage, model: &Model) -> Option<Value> {
    let same = same_model(message, model);
    let mut text = String::new();
    let mut thinking_blocks = Vec::new();
    let mut tool_calls = Vec::new();
    for block in &message.content {
        match block {
            ContentBlock::Text { text: t } => text.push_str(t),
            ContentBlock::Thinking { thinking, signature: Some(signature), redacted } if same => {
                if *redacted {
                    thinking_blocks.push(json!({"type": "redacted_thinking", "data": signature}));
                } else {
                    thinking_blocks.push(json!({"type": "thinking", "thinking": thinking, "signature": signature}));
                }
            }
            ContentBlock::ToolCall { id, name, arguments, .. } => tool_calls.push(json!({
                "id": id,
                "type": "function",
                "function": {"name": name, "arguments": arguments.to_string()},
            })),
            _ => {}
        }
    }
    if text.is_empty() && tool_calls.is_empty() {
        return None;
    }
    let mut out = json!({"role": "assistant", "content": if text.is_empty() { Value::Null } else { json!(text) }});
    if !tool_calls.is_empty() {
        out["tool_calls"] = Value::Array(tool_calls);
    }
    if !thinking_blocks.is_empty() {
        out["thinking_blocks"] = Value::Array(thinking_blocks);
    }
    Some(out)
}

/// Tool messages carry only text, so images from tool results follow them as a user message.
fn flush_tool_images(out: &mut Vec<Value>, images: &mut Vec<Value>) {
    if images.is_empty() {
        return;
    }
    let mut parts = vec![json!({"type": "text", "text": "Images from the tool results above:"})];
    parts.append(images);
    out.push(json!({"role": "user", "content": parts}));
}

fn convert_messages(request: &Request<'_>) -> Vec<Value> {
    let model = request.model;
    let mut out = Vec::new();
    if !request.system_prompt.is_empty() {
        out.push(text_message("system", request.system_prompt.to_string()));
    }
    let mut pending_images: Vec<Value> = Vec::new();
    for message in normalize_messages(request.messages, model) {
        if !matches!(message, Message::ToolResult(_)) {
            flush_tool_images(&mut out, &mut pending_images);
        }
        match &message {
            Message::User(user) => {
                let parts = content_parts(&user.content);
                if !parts.is_empty() {
                    out.push(json!({"role": "user", "content": parts}));
                }
            }
            Message::Assistant(assistant) => out.extend(assistant_message(assistant, model)),
            Message::ToolResult(result) => {
                let mut text = content_text(&result.content);
                pending_images
                    .extend(content_parts(&result.content).into_iter().filter(|part| part["type"] == "image_url"));
                if text.is_empty() {
                    text.push_str("(no output)");
                }
                if result.is_error {
                    text = format!("Error: {text}");
                }
                out.push(json!({"role": "tool", "tool_call_id": result.tool_call_id, "content": text}));
            }
            Message::BashExecution(bash) => out.push(text_message("user", bash.to_context_text())),
            Message::CompactionSummary(summary) => out.push(text_message("user", summary.to_context_text())),
        }
    }
    flush_tool_images(&mut out, &mut pending_images);
    out
}

fn add_cache_breakpoints(messages: &mut [Value]) {
    let mark = |message: &mut Value| {
        if let Some(Value::Object(part)) =
            message.get_mut("content").and_then(Value::as_array_mut).and_then(|c| c.last_mut())
        {
            part.insert("cache_control".into(), json!({"type": "ephemeral"}));
        }
    };
    if let Some(first) = messages.first_mut().filter(|m| m["role"] == "system") {
        mark(first);
    }
    if let Some(last) = messages.iter_mut().rev().find(|m| m["content"].is_array()) {
        mark(last);
    }
}

fn reasoning_effort(level: ThinkingLevel) -> Option<&'static str> {
    (level != ThinkingLevel::Off).then(|| level.as_str())
}

pub(super) fn build_body(request: &Request<'_>) -> Value {
    let model = request.model;
    let mut body = Map::new();
    body.insert("model".into(), json!(model.id));
    body.insert("stream".into(), json!(true));
    body.insert("stream_options".into(), json!({"include_usage": true}));
    body.insert("max_tokens".into(), json!(request.max_tokens.unwrap_or(model.max_tokens)));

    let mut messages = convert_messages(request);
    if model.cache_control {
        add_cache_breakpoints(&mut messages);
    }
    body.insert("messages".into(), Value::Array(messages));

    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {"name": tool.name, "description": tool.description, "parameters": tool.parameters},
                })
            })
            .collect();
        body.insert("tools".into(), Value::Array(tools));
    }

    if model.reasoning == Reasoning::Effort
        && let Some(effort) = reasoning_effort(model.clamp_thinking(request.thinking))
    {
        body.insert("reasoning_effort".into(), json!(effort));
    }

    let mut body = Value::Object(body);
    merge_extra_body(&mut body, model);
    body
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum OpenBlock {
    #[default]
    None,
    Text(usize),
    Thinking(usize),
}

#[derive(Default)]
struct StreamState {
    open: OpenBlock,
    /// Tool call index from the server -> (content index, raw JSON arguments).
    tools: BTreeMap<u64, (usize, String)>,
    finish_reason: Option<String>,
}

impl StreamState {
    fn close(&mut self, out: &AssistantMessage, on_delta: &mut DeltaSink<'_>) {
        match self.open {
            OpenBlock::Text(content_index) => on_delta(out, StreamDelta::TextEnd { content_index }),
            OpenBlock::Thinking(content_index) => on_delta(out, StreamDelta::ThinkingEnd { content_index }),
            OpenBlock::None => {}
        }
        self.open = OpenBlock::None;
    }

    fn append_text(&mut self, piece: &str, out: &mut AssistantMessage, on_delta: &mut DeltaSink<'_>) {
        let content_index = match self.open {
            OpenBlock::Text(index) => index,
            _ => {
                self.close(out, on_delta);
                out.content.push(ContentBlock::text(""));
                let index = out.content.len() - 1;
                self.open = OpenBlock::Text(index);
                on_delta(out, StreamDelta::TextStart { content_index: index });
                index
            }
        };
        if let ContentBlock::Text { text } = &mut out.content[content_index] {
            text.push_str(piece);
        }
        on_delta(out, StreamDelta::TextDelta { content_index, delta: piece.to_string() });
    }

    fn thinking_block(&mut self, out: &mut AssistantMessage, on_delta: &mut DeltaSink<'_>, redacted: bool) -> usize {
        match self.open {
            OpenBlock::Thinking(index) if !redacted => index,
            _ => {
                self.close(out, on_delta);
                out.content.push(ContentBlock::Thinking {
                    thinking: if redacted { "[Reasoning redacted]".into() } else { String::new() },
                    signature: None,
                    redacted,
                });
                let index = out.content.len() - 1;
                self.open = OpenBlock::Thinking(index);
                on_delta(out, StreamDelta::ThinkingStart { content_index: index });
                index
            }
        }
    }

    fn append_thinking(&mut self, piece: &str, out: &mut AssistantMessage, on_delta: &mut DeltaSink<'_>) {
        let content_index = self.thinking_block(out, on_delta, false);
        if let ContentBlock::Thinking { thinking, .. } = &mut out.content[content_index] {
            thinking.push_str(piece);
        }
        on_delta(out, StreamDelta::ThinkingDelta { content_index, delta: piece.to_string() });
    }
}

fn read_usage(usage: &Value, out: &mut AssistantMessage) {
    let get = |pointer: &str| usage.pointer(pointer).and_then(Value::as_u64);
    let prompt = get("/prompt_tokens").unwrap_or(0);
    let completion = get("/completion_tokens").unwrap_or(0);
    let cache_read =
        get("/cache_read_input_tokens").or_else(|| get("/prompt_tokens_details/cached_tokens")).unwrap_or(0);
    let cache_write = get("/cache_creation_input_tokens").unwrap_or(0);
    out.usage.input = prompt.saturating_sub(cache_read + cache_write);
    out.usage.output = completion;
    out.usage.cache_read = cache_read;
    out.usage.cache_write = cache_write;
    out.usage.total_tokens = out.usage.input + out.usage.output + cache_read + cache_write;
}

fn handle_chunk(
    chunk: &Value,
    out: &mut AssistantMessage,
    state: &mut StreamState,
    on_delta: &mut DeltaSink<'_>,
) -> Result<(), ProviderError> {
    if let Some(error) = chunk.get("error") {
        return Err(ProviderError::from_stream_error(super::extract_error_message(
            &json!({"error": error}).to_string(),
        )));
    }
    if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
        read_usage(usage, out);
    }
    let Some(choice) = chunk.get("choices").and_then(Value::as_array).and_then(|c| c.first()) else {
        return Ok(());
    };
    if let Some(delta) = choice.get("delta") {
        let reasoning = ["reasoning_content", "reasoning"]
            .iter()
            .find_map(|key| delta.get(*key).and_then(Value::as_str).filter(|s| !s.is_empty()));
        if let Some(piece) = reasoning {
            state.append_thinking(piece, out, on_delta);
        }
        if let Some(blocks) = delta.get("thinking_blocks").and_then(Value::as_array) {
            for block in blocks {
                if block.get("type").and_then(Value::as_str) == Some("redacted_thinking") {
                    let index = state.thinking_block(out, on_delta, true);
                    if let ContentBlock::Thinking { signature, .. } = &mut out.content[index] {
                        *signature = block.get("data").and_then(Value::as_str).map(str::to_string);
                    }
                    state.close(out, on_delta);
                    continue;
                }
                if reasoning.is_none()
                    && let Some(piece) = block.get("thinking").and_then(Value::as_str).filter(|s| !s.is_empty())
                {
                    state.append_thinking(piece, out, on_delta);
                }
                if let Some(sig) = block.get("signature").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                    let index = state.thinking_block(out, on_delta, false);
                    if let ContentBlock::Thinking { signature, .. } = &mut out.content[index] {
                        signature.get_or_insert_with(String::new).push_str(sig);
                    }
                }
            }
        }
        if let Some(piece) = delta.get("content").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            state.append_text(piece, out, on_delta);
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let tool_index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
                let function = call.get("function");
                let args = function.and_then(|f| f.get("arguments")).and_then(Value::as_str).unwrap_or_default();
                if !state.tools.contains_key(&tool_index) {
                    state.close(out, on_delta);
                    let id = call.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
                    let name =
                        function.and_then(|f| f.get("name")).and_then(Value::as_str).unwrap_or_default().to_string();
                    // Some servers omit the id; the agent needs one to pair the call with its result.
                    let id = if id.is_empty() { format!("call_{}", uuid::Uuid::new_v4().simple()) } else { id };
                    out.content.push(ContentBlock::ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        arguments: Value::Object(Default::default()),
                        invalid_arguments: None,
                    });
                    let content_index = out.content.len() - 1;
                    state.tools.insert(tool_index, (content_index, String::new()));
                    on_delta(out, StreamDelta::ToolcallStart { content_index, id, tool_name: name });
                }
                let (content_index, raw) = state.tools.get_mut(&tool_index).expect("tool call registered above");
                if !args.is_empty() {
                    raw.push_str(args);
                    let content_index = *content_index;
                    on_delta(out, StreamDelta::ToolcallDelta { content_index, delta: args.to_string() });
                }
            }
        }
    }
    if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
        state.finish_reason = Some(reason.to_string());
    }
    Ok(())
}

fn finish(out: &mut AssistantMessage, state: &mut StreamState, on_delta: &mut DeltaSink<'_>) {
    state.close(out, on_delta);
    for (content_index, raw) in std::mem::take(&mut state.tools).into_values() {
        finish_tool_call(out, content_index, &raw);
        on_delta(out, StreamDelta::ToolcallEnd { content_index });
    }
    let has_tools = out.content.iter().any(|b| matches!(b, ContentBlock::ToolCall { .. }));
    out.stop_reason = match state.finish_reason.as_deref() {
        Some("length") => StopReason::Length,
        Some("content_filter") => {
            out.error_message = Some("The response was blocked by a content filter".into());
            StopReason::Error
        }
        _ if has_tools => StopReason::ToolUse,
        _ => StopReason::Stop,
    };
}

pub(super) async fn stream(
    client: &reqwest::Client,
    request: &Request<'_>,
    api_key: &str,
    out: &mut AssistantMessage,
    on_delta: &mut DeltaSink<'_>,
    cancel: &CancellationToken,
) -> Result<(), ProviderError> {
    let body = build_body(request);
    let builder = client
        .post(v1_url(&request.model.base_url, "chat/completions"))
        .header("accept", "text/event-stream")
        .json(&body);
    let response = send(apply_auth(builder, request.model, api_key)?, cancel).await?;

    let mut state = StreamState::default();
    let mut done = false;
    sse::for_each_event(response.bytes_stream(), cancel, |event| {
        let data = event.data.trim();
        if data.is_empty() {
            return Ok(true);
        }
        if data == "[DONE]" {
            done = true;
            return Ok(false);
        }
        let chunk: Value = serde_json::from_str(data)
            .map_err(|err| ProviderError::retryable(format!("invalid stream chunk: {err}")))?;
        handle_chunk(&chunk, out, &mut state, on_delta)?;
        Ok(true)
    })
    .await?;

    if !done && state.finish_reason.is_none() {
        return Err(ProviderError::retryable("stream ended before completion"));
    }
    finish(out, &mut state, on_delta);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Api, builtin_anthropic_models};
    use crate::message::{ToolResultMessage, UserMessage};

    fn gateway_model() -> Model {
        let mut model = builtin_anthropic_models().remove(0);
        model.provider = "litellm".into();
        model.api = Api::OpenAiCompletions;
        model.reasoning = Reasoning::Effort;
        model.thinking_levels = vec![ThinkingLevel::Low, ThinkingLevel::Medium, ThinkingLevel::High];
        model
    }

    #[test]
    fn builds_request_with_reasoning_and_image_tool_results() {
        let model = gateway_model();
        let mut assistant = super::super::new_assistant_message(&model, ThinkingLevel::High);
        assistant.stop_reason = StopReason::ToolUse;
        assistant.content = vec![
            ContentBlock::Thinking { thinking: "t".into(), signature: Some("s".into()), redacted: false },
            ContentBlock::ToolCall {
                id: "c1".into(),
                name: "read".into(),
                arguments: json!({"path": "x.png"}),
                invalid_arguments: None,
            },
        ];
        let messages = vec![
            Message::User(UserMessage::new(vec![ContentBlock::text("look")])),
            Message::Assistant(assistant),
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "c1".into(),
                tool_name: "read".into(),
                content: vec![
                    ContentBlock::text("Read image"),
                    ContentBlock::Image { data: "AAA".into(), mime_type: "image/png".into() },
                ],
                is_error: false,
                details: None,
                timestamp: 0,
                duration_ms: None,
            }),
        ];
        let request = Request {
            model: &model,
            system_prompt: "sys",
            messages: &messages,
            tools: &[],
            thinking: ThinkingLevel::Xhigh,
            max_tokens: None,
        };
        let body = build_body(&request);
        assert_eq!(body["reasoning_effort"], json!("high"));
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 5);
        assert_eq!(msgs[2]["thinking_blocks"][0]["signature"], json!("s"));
        assert_eq!(msgs[2]["tool_calls"][0]["function"]["arguments"], json!("{\"path\":\"x.png\"}"));
        assert_eq!(msgs[3]["role"], json!("tool"));
        assert_eq!(msgs[4]["content"][1]["type"], json!("image_url"));
        assert_eq!(msgs[4]["content"][1]["cache_control"], json!({"type": "ephemeral"}));
    }

    #[test]
    fn parses_streamed_chunks() {
        let model = gateway_model();
        let mut out = super::super::new_assistant_message(&model, ThinkingLevel::High);
        let mut state = StreamState::default();
        let mut sink = |_: &AssistantMessage, _: StreamDelta| {};
        let chunks = [
            json!({"choices": [{"delta": {"reasoning_content": "think", "thinking_blocks": [{"type": "thinking", "thinking": "think"}]}}]}),
            json!({"choices": [{"delta": {"thinking_blocks": [{"type": "thinking", "thinking": "", "signature": "sig"}]}}]}),
            json!({"choices": [{"delta": {"content": "Hello"}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "call_1", "function": {"name": "ls", "arguments": "{\"pa"}}]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": "th\":\".\"}"}}]}, "finish_reason": "tool_calls"}]}),
            json!({"choices": [], "usage": {"prompt_tokens": 100, "completion_tokens": 20, "prompt_tokens_details": {"cached_tokens": 60}}}),
        ];
        for chunk in &chunks {
            handle_chunk(chunk, &mut out, &mut state, &mut sink).unwrap();
        }
        finish(&mut out, &mut state, &mut sink);
        assert_eq!(out.stop_reason, StopReason::ToolUse);
        assert_eq!(
            out.content[0],
            ContentBlock::Thinking { thinking: "think".into(), signature: Some("sig".into()), redacted: false }
        );
        assert_eq!(out.content[1], ContentBlock::text("Hello"));
        assert_eq!(
            out.content[2],
            ContentBlock::ToolCall {
                id: "call_1".into(),
                name: "ls".into(),
                arguments: json!({"path": "."}),
                invalid_arguments: None
            }
        );
        assert_eq!(out.usage.input, 40);
        assert_eq!(out.usage.cache_read, 60);
    }

    #[test]
    fn tool_calls_without_ids_get_one_in_the_message_and_the_event() {
        let model = gateway_model();
        let mut out = super::super::new_assistant_message(&model, ThinkingLevel::High);
        let mut state = StreamState::default();
        let mut started = Vec::new();
        let mut sink = |_: &AssistantMessage, delta: StreamDelta| {
            if let StreamDelta::ToolcallStart { id, .. } = delta {
                started.push(id);
            }
        };
        let chunk = json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"name": "ls", "arguments": "{}"}}]}}]});
        handle_chunk(&chunk, &mut out, &mut state, &mut sink).unwrap();
        let ContentBlock::ToolCall { id, .. } = &out.content[0] else { panic!("expected a tool call") };
        assert!(id.starts_with("call_"));
        assert_eq!(started, std::slice::from_ref(id));
    }
}
