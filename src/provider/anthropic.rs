//! Anthropic Messages API (`POST /v1/messages`), used for the Anthropic API directly and for
//! gateways that expose an Anthropic-compatible endpoint (such as LiteLLM).

use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::{
    DeltaSink, ProviderError, Request, StreamDelta, apply_auth, finish_tool_call, merge_extra_body, normalize_messages,
    same_model, send, sse,
};
use crate::config::{Model, Reasoning, ThinkingLevel};
use crate::message::{AssistantMessage, ContentBlock, Message, StopReason};

pub(super) const API_VERSION: &str = "2023-06-01";
const INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";

pub(super) fn endpoint(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/v1") { format!("{base}/messages") } else { format!("{base}/v1/messages") }
}

fn effort(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Off | ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::Xhigh => "xhigh",
        ThinkingLevel::Max => "max",
    }
}

fn thinking_budget(level: ThinkingLevel) -> u64 {
    match level {
        ThinkingLevel::Off => 0,
        ThinkingLevel::Minimal => 1_024,
        ThinkingLevel::Low => 4_096,
        ThinkingLevel::Medium => 10_240,
        ThinkingLevel::High => 20_480,
        ThinkingLevel::Xhigh | ThinkingLevel::Max => 32_000,
    }
}

/// Anthropic tool ids must match `^[a-zA-Z0-9_-]{1,64}$`; ids from other providers may not.
fn sanitize_tool_id(id: &str) -> String {
    let cleaned: String =
        id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).take(64).collect();
    if cleaned.is_empty() { "tool".to_string() } else { cleaned }
}

fn image_block(data: &str, mime_type: &str) -> Value {
    json!({"type": "image", "source": {"type": "base64", "media_type": mime_type, "data": data}})
}

fn user_blocks(content: &[ContentBlock]) -> Vec<Value> {
    content
        .iter()
        .filter_map(|block| match block {
            // The API rejects whitespace-only text blocks.
            ContentBlock::Text { text } if !text.trim().is_empty() => Some(json!({"type": "text", "text": text})),
            ContentBlock::Image { data, mime_type } => Some(image_block(data, mime_type)),
            _ => None,
        })
        .collect()
}

fn assistant_blocks(message: &AssistantMessage, model: &Model) -> Vec<Value> {
    let same = same_model(message, model);
    let mut blocks = Vec::new();
    for block in &message.content {
        match block {
            ContentBlock::Thinking { thinking, signature, redacted } => {
                if same {
                    if *redacted {
                        if let Some(data) = signature {
                            blocks.push(json!({"type": "redacted_thinking", "data": data}));
                        }
                    } else if let Some(signature) = signature {
                        blocks.push(json!({"type": "thinking", "thinking": thinking, "signature": signature}));
                    }
                } else if !redacted && !thinking.trim().is_empty() {
                    // Another model's reasoning cannot be replayed as thinking; keep it as text.
                    blocks.push(json!({"type": "text", "text": thinking}));
                }
            }
            ContentBlock::Text { text } if !text.trim().is_empty() => {
                blocks.push(json!({"type": "text", "text": text}))
            }
            ContentBlock::ToolCall { id, name, arguments, .. } => {
                blocks.push(json!({"type": "tool_use", "id": sanitize_tool_id(id), "name": name, "input": arguments}));
            }
            _ => {}
        }
    }
    blocks
}

fn convert_messages(messages: &[Message], model: &Model) -> Vec<Value> {
    // (role, blocks); consecutive messages with the same role are merged.
    let mut out: Vec<(&'static str, Vec<Value>)> = Vec::new();
    let mut push = |role: &'static str, blocks: Vec<Value>| {
        if blocks.is_empty() {
            return;
        }
        match out.last_mut() {
            Some((last_role, last_blocks)) if *last_role == role => last_blocks.extend(blocks),
            _ => out.push((role, blocks)),
        }
    };

    for message in normalize_messages(messages, model) {
        match &message {
            Message::User(user) => push("user", user_blocks(&user.content)),
            Message::Assistant(assistant) => push("assistant", assistant_blocks(assistant, model)),
            Message::ToolResult(result) => {
                let mut content = user_blocks(&result.content);
                if content.is_empty() {
                    content.push(json!({"type": "text", "text": "(no output)"}));
                }
                push(
                    "user",
                    vec![json!({
                        "type": "tool_result",
                        "tool_use_id": sanitize_tool_id(&result.tool_call_id),
                        "content": content,
                        "is_error": result.is_error,
                    })],
                );
            }
            Message::BashExecution(bash) => push("user", vec![json!({"type": "text", "text": bash.to_context_text()})]),
            Message::CompactionSummary(summary) => {
                push("user", vec![json!({"type": "text", "text": summary.to_context_text()})])
            }
        }
    }

    // Tool results must lead the user turn that follows a tool_use turn.
    for (role, blocks) in &mut out {
        if *role == "user" {
            blocks.sort_by_key(|block| block.get("type").and_then(Value::as_str) != Some("tool_result"));
        }
    }
    out.into_iter().map(|(role, content)| json!({"role": role, "content": content})).collect()
}

fn add_cache_breakpoint(messages: &mut [Value]) {
    let Some(last) = messages.last_mut() else { return };
    if let Some(Value::Object(block)) = last.get_mut("content").and_then(Value::as_array_mut).and_then(|c| c.last_mut())
    {
        block.insert("cache_control".into(), json!({"type": "ephemeral"}));
    }
}

pub(super) fn build_body(request: &Request<'_>) -> (Value, Vec<&'static str>) {
    let model = request.model;
    let mut betas = Vec::new();
    let mut max_tokens = request.max_tokens.unwrap_or(model.max_tokens);
    let mut body = Map::new();
    body.insert("model".into(), json!(model.id));
    body.insert("stream".into(), json!(true));

    if !request.system_prompt.is_empty() {
        let mut system = json!({"type": "text", "text": request.system_prompt});
        if model.cache_control {
            system["cache_control"] = json!({"type": "ephemeral"});
        }
        body.insert("system".into(), json!([system]));
    }

    let mut messages = convert_messages(request.messages, model);
    if model.cache_control {
        add_cache_breakpoint(&mut messages);
    }
    body.insert("messages".into(), Value::Array(messages));

    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                let mut spec =
                    json!({"name": tool.name, "description": tool.description, "input_schema": tool.parameters});
                if model.eager_input_streaming {
                    spec["eager_input_streaming"] = json!(true);
                }
                spec
            })
            .collect();
        body.insert("tools".into(), Value::Array(tools));
    }

    let level = model.clamp_thinking(request.thinking);
    match model.reasoning {
        Reasoning::Adaptive => {
            if level == ThinkingLevel::Off {
                body.insert("thinking".into(), json!({"type": "disabled"}));
            } else {
                body.insert("thinking".into(), json!({"type": "adaptive", "display": "summarized"}));
                body.insert("output_config".into(), json!({"effort": effort(level)}));
            }
        }
        Reasoning::Budget if level != ThinkingLevel::Off => {
            let budget = thinking_budget(level);
            max_tokens = max_tokens.max(budget + 4_096);
            body.insert("thinking".into(), json!({"type": "enabled", "budget_tokens": budget}));
            betas.push(INTERLEAVED_THINKING_BETA);
        }
        _ => {}
    }
    body.insert("max_tokens".into(), json!(max_tokens));

    let mut body = Value::Object(body);
    merge_extra_body(&mut body, model);
    (body, betas)
}

fn map_stop_reason(reason: &str, out: &mut AssistantMessage, stop_details: Option<&Value>) {
    out.stop_reason = match reason {
        "max_tokens" | "model_context_window_exceeded" => StopReason::Length,
        "tool_use" => StopReason::ToolUse,
        "refusal" => {
            let category = stop_details.and_then(|d| d.get("category")).and_then(Value::as_str);
            let explanation = stop_details.and_then(|d| d.get("explanation")).and_then(Value::as_str);
            let mut message = "The model declined to continue (refusal".to_string();
            if let Some(category) = category {
                message.push_str(&format!(": {category}"));
            }
            message.push(')');
            if let Some(explanation) = explanation {
                message.push_str(&format!(". {explanation}"));
            }
            out.error_message = Some(message);
            StopReason::Error
        }
        _ => StopReason::Stop,
    };
}

fn read_usage(usage: &Value, out: &mut AssistantMessage) {
    let get = |key: &str| usage.get(key).and_then(Value::as_u64);
    if let Some(v) = get("input_tokens") {
        out.usage.input = v;
    }
    if let Some(v) = get("cache_read_input_tokens") {
        out.usage.cache_read = v;
    }
    if let Some(v) = get("cache_creation_input_tokens") {
        out.usage.cache_write = v;
    }
    if let Some(v) = get("output_tokens") {
        out.usage.output = v;
    }
    out.usage.total_tokens = out.usage.input + out.usage.output + out.usage.cache_read + out.usage.cache_write;
}

/// Per-response stream state: maps Anthropic block indices to our content indices.
#[derive(Default)]
struct BlockState {
    /// Anthropic index -> (content index, accumulated tool JSON).
    blocks: Vec<Option<(usize, String)>>,
}

impl BlockState {
    fn set(&mut self, index: usize, content_index: usize) {
        if self.blocks.len() <= index {
            self.blocks.resize(index + 1, None);
        }
        self.blocks[index] = Some((content_index, String::new()));
    }

    fn get(&mut self, index: usize) -> Option<&mut (usize, String)> {
        self.blocks.get_mut(index).and_then(Option::as_mut)
    }
}

fn handle_event(
    data: &Value,
    out: &mut AssistantMessage,
    state: &mut BlockState,
    on_delta: &mut DeltaSink<'_>,
) -> Result<bool, ProviderError> {
    let index = data.get("index").and_then(Value::as_u64).map(|i| i as usize);
    match data.get("type").and_then(Value::as_str).unwrap_or_default() {
        "message_start" => {
            if let Some(usage) = data.pointer("/message/usage") {
                read_usage(usage, out);
            }
        }
        "content_block_start" => {
            let (Some(index), Some(block)) = (index, data.get("content_block")) else { return Ok(true) };
            let content_index = out.content.len();
            let delta = match block.get("type").and_then(Value::as_str).unwrap_or_default() {
                "text" => {
                    out.content.push(ContentBlock::text(block.get("text").and_then(Value::as_str).unwrap_or_default()));
                    StreamDelta::TextStart { content_index }
                }
                "thinking" => {
                    out.content.push(ContentBlock::Thinking {
                        thinking: block.get("thinking").and_then(Value::as_str).unwrap_or_default().to_string(),
                        signature: block.get("signature").and_then(Value::as_str).map(str::to_string),
                        redacted: false,
                    });
                    StreamDelta::ThinkingStart { content_index }
                }
                "redacted_thinking" => {
                    out.content.push(ContentBlock::Thinking {
                        thinking: "[Reasoning redacted]".to_string(),
                        signature: block.get("data").and_then(Value::as_str).map(str::to_string),
                        redacted: true,
                    });
                    StreamDelta::ThinkingStart { content_index }
                }
                "tool_use" => {
                    let id = block.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
                    let name = block.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
                    out.content.push(ContentBlock::ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        arguments: Value::Object(Default::default()),
                        invalid_arguments: None,
                    });
                    StreamDelta::ToolcallStart { content_index, id, tool_name: name }
                }
                // Server tool blocks and future block types are not part of viper's model.
                _ => return Ok(true),
            };
            state.set(index, content_index);
            on_delta(out, delta);
        }
        "content_block_delta" => {
            let (Some(index), Some(delta)) = (index, data.get("delta")) else { return Ok(true) };
            let Some((content_index, json_buf)) = state.get(index) else { return Ok(true) };
            let content_index = *content_index;
            let delta_type = delta.get("type").and_then(Value::as_str).unwrap_or_default();
            let block = &mut out.content[content_index];
            let event = match (delta_type, block) {
                ("text_delta", ContentBlock::Text { text }) => {
                    let piece = delta.get("text").and_then(Value::as_str).unwrap_or_default();
                    text.push_str(piece);
                    StreamDelta::TextDelta { content_index, delta: piece.to_string() }
                }
                ("thinking_delta", ContentBlock::Thinking { thinking, .. }) => {
                    let piece = delta.get("thinking").and_then(Value::as_str).unwrap_or_default();
                    thinking.push_str(piece);
                    StreamDelta::ThinkingDelta { content_index, delta: piece.to_string() }
                }
                ("signature_delta", ContentBlock::Thinking { signature, .. }) => {
                    let piece = delta.get("signature").and_then(Value::as_str).unwrap_or_default();
                    signature.get_or_insert_with(String::new).push_str(piece);
                    return Ok(true);
                }
                ("input_json_delta", ContentBlock::ToolCall { .. }) => {
                    let piece = delta.get("partial_json").and_then(Value::as_str).unwrap_or_default();
                    json_buf.push_str(piece);
                    StreamDelta::ToolcallDelta { content_index, delta: piece.to_string() }
                }
                _ => return Ok(true),
            };
            on_delta(out, event);
        }
        "content_block_stop" => {
            let Some(index) = index else { return Ok(true) };
            let Some((content_index, json_buf)) = state.get(index) else { return Ok(true) };
            let content_index = *content_index;
            let raw = std::mem::take(json_buf);
            let event = match &out.content[content_index] {
                ContentBlock::Text { .. } => StreamDelta::TextEnd { content_index },
                ContentBlock::Thinking { .. } => StreamDelta::ThinkingEnd { content_index },
                ContentBlock::ToolCall { .. } => {
                    finish_tool_call(out, content_index, &raw);
                    StreamDelta::ToolcallEnd { content_index }
                }
                ContentBlock::Image { .. } => return Ok(true),
            };
            on_delta(out, event);
        }
        "message_delta" => {
            if let Some(usage) = data.get("usage") {
                read_usage(usage, out);
            }
            if let Some(reason) = data.pointer("/delta/stop_reason").and_then(Value::as_str) {
                map_stop_reason(reason, out, data.pointer("/delta/stop_details"));
            }
        }
        "message_stop" => return Ok(false),
        "error" => {
            let message = super::extract_error_message(&data.to_string());
            return Err(ProviderError::from_stream_error(message));
        }
        _ => {}
    }
    Ok(true)
}

pub(super) async fn stream(
    client: &reqwest::Client,
    request: &Request<'_>,
    api_key: &str,
    out: &mut AssistantMessage,
    on_delta: &mut DeltaSink<'_>,
    cancel: &CancellationToken,
) -> Result<(), ProviderError> {
    let (body, betas) = build_body(request);
    let mut builder = client.post(endpoint(&request.model.base_url)).header("accept", "text/event-stream").json(&body);
    if !betas.is_empty() {
        builder = builder.header("anthropic-beta", betas.join(","));
    }
    let builder = apply_auth(builder, request.model, api_key)?;
    let response = send(builder, cancel).await?;

    let mut state = BlockState::default();
    let mut saw_stop = false;
    sse::for_each_event(response.bytes_stream(), cancel, |event| {
        if event.data.trim().is_empty() {
            return Ok(true);
        }
        let data: Value = serde_json::from_str(&event.data)
            .map_err(|err| ProviderError::retryable(format!("invalid stream event: {err}")))?;
        let keep_going = handle_event(&data, out, &mut state, on_delta)?;
        if !keep_going {
            saw_stop = true;
        }
        Ok(keep_going)
    })
    .await?;

    if !saw_stop {
        return Err(ProviderError::retryable("stream ended before message_stop"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Api, builtin_anthropic_models};
    use crate::message::{ToolResultMessage, UserMessage};

    fn opus() -> Model {
        builtin_anthropic_models().into_iter().find(|m| m.id == "claude-opus-5-5").unwrap()
    }

    fn assistant(model: &Model, content: Vec<ContentBlock>, stop: StopReason) -> AssistantMessage {
        let mut message = super::super::new_assistant_message(model, ThinkingLevel::High);
        message.content = content;
        message.stop_reason = stop;
        message
    }

    #[test]
    fn endpoint_handles_v1_suffix() {
        assert_eq!(endpoint("https://api.anthropic.com"), "https://api.anthropic.com/v1/messages");
        assert_eq!(endpoint("http://localhost:4000/v1/"), "http://localhost:4000/v1/messages");
    }

    #[test]
    fn builds_adaptive_thinking_request_with_cache_breakpoints() {
        let model = opus();
        let messages = vec![Message::User(UserMessage::new(vec![ContentBlock::text("hi")]))];
        let request = Request {
            model: &model,
            system_prompt: "sys",
            messages: &messages,
            tools: &[],
            thinking: ThinkingLevel::Off,
            max_tokens: None,
        };
        let (body, betas) = build_body(&request);
        assert!(betas.is_empty());
        // Opus 5.5 cannot disable thinking, so "off" clamps to low effort.
        assert_eq!(body["thinking"], json!({"type": "adaptive", "display": "summarized"}));
        assert_eq!(body["output_config"], json!({"effort": "low"}));
        assert_eq!(body["system"][0]["cache_control"], json!({"type": "ephemeral"}));
        assert_eq!(body["messages"][0]["content"][0]["cache_control"], json!({"type": "ephemeral"}));
    }

    #[test]
    fn converts_tool_round_trip_and_drops_foreign_signatures() {
        let model = opus();
        let mut foreign = model.clone();
        foreign.id = "claude-sonnet-5-5".into();
        let messages = vec![
            Message::User(UserMessage::new(vec![ContentBlock::text("list files")])),
            Message::Assistant(assistant(
                &foreign,
                vec![
                    ContentBlock::Thinking { thinking: "plan".into(), signature: Some("sig".into()), redacted: false },
                    // Some OpenAI-compatible models emit whitespace-only text before a tool call.
                    ContentBlock::text("\n"),
                    ContentBlock::ToolCall {
                        id: "call|1".into(),
                        name: "ls".into(),
                        arguments: json!({}),
                        invalid_arguments: None,
                    },
                ],
                StopReason::ToolUse,
            )),
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "call|1".into(),
                tool_name: "ls".into(),
                content: vec![ContentBlock::text("a\nb")],
                is_error: false,
                details: None,
                timestamp: 0,
                duration_ms: None,
            }),
            Message::User(UserMessage::new(vec![ContentBlock::text("thanks")])),
        ];
        let converted = convert_messages(&messages, &model);
        assert_eq!(converted.len(), 3);
        assert_eq!(converted[1]["content"][0], json!({"type": "text", "text": "plan"}));
        assert_eq!(converted[1]["content"][1]["id"], json!("call_1"));
        assert_eq!(converted[2]["content"][0]["type"], json!("tool_result"));
        assert_eq!(converted[2]["content"][0]["tool_use_id"], json!("call_1"));
        assert_eq!(converted[2]["content"][1]["text"], json!("thanks"));
    }

    #[test]
    fn orphaned_tool_calls_get_synthetic_results_and_aborted_turns_are_dropped() {
        let model = opus();
        let call = ContentBlock::ToolCall {
            id: "t1".into(),
            name: "bash".into(),
            arguments: json!({}),
            invalid_arguments: None,
        };
        let messages = vec![
            Message::User(UserMessage::new(vec![ContentBlock::text("go")])),
            Message::Assistant(assistant(&model, vec![call.clone()], StopReason::ToolUse)),
            Message::User(UserMessage::new(vec![ContentBlock::text("never mind")])),
            Message::Assistant(assistant(&model, vec![ContentBlock::text("partial")], StopReason::Aborted)),
        ];
        let converted = convert_messages(&messages, &model);
        assert_eq!(converted.len(), 3);
        assert_eq!(converted[2]["content"][0]["type"], json!("tool_result"));
        assert_eq!(converted[2]["content"][0]["is_error"], json!(true));
        assert_eq!(converted[2]["content"][1]["text"], json!("never mind"));
        assert_eq!(model.api, Api::AnthropicMessages);
    }

    #[test]
    fn parses_streamed_events() {
        let model = opus();
        let mut out = super::super::new_assistant_message(&model, ThinkingLevel::High);
        let mut state = BlockState::default();
        let mut deltas = Vec::new();
        let mut sink = |_: &AssistantMessage, delta: StreamDelta| deltas.push(delta);
        let events = [
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 10, "cache_read_input_tokens": 5, "output_tokens": 1}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "hmm"}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "abc"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "t1", "name": "read", "input": {}}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"path\":"}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "\"a.rs\"}"}}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 42}}),
        ];
        for event in &events {
            assert!(handle_event(event, &mut out, &mut state, &mut sink).unwrap());
        }
        assert!(!handle_event(&json!({"type": "message_stop"}), &mut out, &mut state, &mut sink).unwrap());
        assert_eq!(out.stop_reason, StopReason::ToolUse);
        assert_eq!(out.usage.output, 42);
        assert_eq!(out.usage.cache_read, 5);
        assert_eq!(
            out.content[0],
            ContentBlock::Thinking { thinking: "hmm".into(), signature: Some("abc".into()), redacted: false }
        );
        assert_eq!(
            out.content[1],
            ContentBlock::ToolCall {
                id: "t1".into(),
                name: "read".into(),
                arguments: json!({"path": "a.rs"}),
                invalid_arguments: None
            }
        );
        assert_eq!(deltas.len(), 7);
    }
}
