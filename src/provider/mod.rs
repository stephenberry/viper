//! LLM providers.
//!
//! A provider turns a [`Request`] into a streamed [`AssistantMessage`]. Providers mutate the
//! message in place as server events arrive and report each change through a [`StreamDelta`]
//! callback, so the agent can forward deltas to the UI without re-sending the whole message.

mod anthropic;
mod openai;
mod sse;

use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::config::{Api, AuthHeader, Model, ThinkingLevel};
use crate::message::{AssistantMessage, ContentBlock, Message, StopReason, Usage, now_ms};

/// A tool declaration sent to the model.
#[derive(Debug, Clone, Serialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

pub struct Request<'a> {
    pub model: &'a Model,
    pub system_prompt: &'a str,
    pub messages: &'a [Message],
    pub tools: &'a [ToolSpec],
    pub thinking: ThinkingLevel,
    /// Overrides the model's default output token limit.
    pub max_tokens: Option<u64>,
}

/// Incremental change to the streamed assistant message. `index` is the content block index.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum StreamDelta {
    TextStart { content_index: usize },
    TextDelta { content_index: usize, delta: String },
    TextEnd { content_index: usize },
    ThinkingStart { content_index: usize },
    ThinkingDelta { content_index: usize, delta: String },
    ThinkingEnd { content_index: usize },
    ToolcallStart { content_index: usize, id: String, tool_name: String },
    ToolcallDelta { content_index: usize, delta: String },
    ToolcallEnd { content_index: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// The request exceeded the model's context window.
    ContextOverflow,
    /// Transient failure (rate limit, overload, server error, connection drop).
    Retryable,
    /// Authentication, validation, or other permanent failure.
    Fatal,
}

#[derive(Debug, Clone)]
pub struct ProviderError {
    pub kind: ErrorKind,
    pub message: String,
    pub retry_after: Option<Duration>,
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ProviderError {}

impl ProviderError {
    pub fn fatal(message: impl Into<String>) -> Self {
        Self { kind: ErrorKind::Fatal, message: message.into(), retry_after: None }
    }

    pub fn retryable(message: impl Into<String>) -> Self {
        Self { kind: ErrorKind::Retryable, message: message.into(), retry_after: None }
    }

    /// Classify an HTTP error response.
    fn from_status(status: reqwest::StatusCode, body: &str, retry_after: Option<Duration>) -> Self {
        let message = format!("HTTP {}: {}", status.as_u16(), extract_error_message(body));
        let kind = if is_context_overflow(&message) {
            ErrorKind::ContextOverflow
        } else if status.as_u16() == 429 || status.as_u16() == 408 || status.as_u16() == 409 || status.is_server_error()
        {
            ErrorKind::Retryable
        } else {
            ErrorKind::Fatal
        };
        Self { kind, message, retry_after }
    }

    /// Classify an error reported inside the event stream.
    fn from_stream_error(message: String) -> Self {
        let lower = message.to_lowercase();
        let kind = if is_context_overflow(&message) {
            ErrorKind::ContextOverflow
        } else if lower.contains("overloaded")
            || lower.contains("rate limit")
            || lower.contains("rate_limit")
            || lower.contains("internal server error")
            || lower.contains("api_error")
            || lower.contains("timeout")
        {
            ErrorKind::Retryable
        } else {
            ErrorKind::Fatal
        };
        Self { kind, message, retry_after: None }
    }
}

/// Pull a human-readable message out of a JSON error body, falling back to the raw text.
fn extract_error_message(body: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        let error = value.get("error").unwrap_or(&value);
        if let Some(message) = error.get("message").and_then(Value::as_str) {
            let kind = error.get("type").and_then(Value::as_str);
            return match kind {
                Some(kind) => format!("{kind}: {message}"),
                None => message.to_string(),
            };
        }
        if let Some(message) = error.as_str() {
            return message.to_string();
        }
    }
    let trimmed = body.trim();
    if trimmed.is_empty() { "(empty response body)".to_string() } else { trimmed.chars().take(2000).collect() }
}

/// `path` under the API's `/v1` prefix, which `base_url` may already end with.
pub(crate) fn v1_url(base_url: &str, path: &str) -> String {
    let base = base_url.trim_end_matches('/');
    let base = base.strip_suffix("/v1").unwrap_or(base);
    format!("{base}/v1/{path}")
}

/// Recognize context-window overflow errors across Anthropic, OpenAI, and LiteLLM.
fn is_context_overflow(message: &str) -> bool {
    let lower = message.to_lowercase();
    [
        "prompt is too long",
        "context window",
        "context_length_exceeded",
        "maximum context length",
        "input is too long",
        "too many tokens",
        "exceeds the context",
        "model_context_window_exceeded",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// Build an empty assistant message for `model`.
pub fn new_assistant_message(model: &Model, thinking: ThinkingLevel) -> AssistantMessage {
    AssistantMessage {
        content: Vec::new(),
        api: model.api,
        provider: model.provider.clone(),
        model: model.id.clone(),
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        thinking_level: (!model.thinking_levels.is_empty()).then_some(thinking),
        timestamp: now_ms(),
        duration_ms: None,
    }
}

pub type DeltaSink<'a> = dyn FnMut(&AssistantMessage, StreamDelta) + Send + 'a;

/// The HTTP client for model servers.
pub fn http_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .user_agent(concat!("viper/", env!("CARGO_PKG_VERSION")))
        .build()
}

/// Stream one model response into `out`.
///
/// On success `out.stop_reason` is set from the provider. On failure `out` holds whatever was
/// received before the error; the caller decides whether to retry.
pub async fn stream(
    client: &reqwest::Client,
    request: &Request<'_>,
    out: &mut AssistantMessage,
    on_delta: &mut DeltaSink<'_>,
    cancel: &CancellationToken,
) -> Result<(), ProviderError> {
    let api_key = match &request.model.api_key {
        Some(source) => source.resolve().map_err(|err| ProviderError::fatal(format!("{err:#}")))?,
        None => None,
    };
    let Some(api_key) = api_key else {
        let hint = request.model.api_key.as_ref().map(|s| s.describe()).unwrap_or_else(|| "an apiKey".into());
        return Err(ProviderError::fatal(format!(
            "no API key for provider '{}' (expected {hint})",
            request.model.provider
        )));
    };
    let result = match request.model.api {
        Api::AnthropicMessages => anthropic::stream(client, request, &api_key, out, on_delta, cancel).await,
        Api::OpenAiCompletions => openai::stream(client, request, &api_key, out, on_delta, cancel).await,
    };
    request.model.compute_cost(&mut out.usage);
    result
}

/// Apply authentication and custom headers. Headers whose environment variable is unset are omitted.
pub(crate) fn apply_auth(
    builder: reqwest::RequestBuilder,
    model: &Model,
    api_key: &str,
) -> Result<reqwest::RequestBuilder, ProviderError> {
    let mut builder = match model.auth_header {
        AuthHeader::XApiKey => builder.header("x-api-key", api_key),
        AuthHeader::Bearer => builder.bearer_auth(api_key),
    };
    if model.api == Api::AnthropicMessages {
        builder = builder.header("anthropic-version", anthropic::API_VERSION);
    }
    for (name, value) in &model.headers {
        let value = value.resolve().map_err(|e| ProviderError::fatal(format!("header '{name}': {e:#}")))?;
        if let Some(value) = value {
            builder = builder.header(name, value);
        }
    }
    Ok(builder)
}

/// Outcome of checking an API key with `check_credentials`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialCheck {
    /// The server accepted the key and reported this many models.
    Accepted(usize),
    /// The server rejected the key (HTTP 401 or 403).
    Rejected(String),
    /// The key could not be checked: the server was unreachable or does not list models.
    Unverified(String),
}

/// Check `api_key` against the model's server by listing its models (`GET /v1/models`, which
/// Anthropic, OpenAI-compatible servers, and LiteLLM provide).
pub async fn check_credentials(client: &reqwest::Client, model: &Model, api_key: &str) -> CredentialCheck {
    let builder = client.get(v1_url(&model.base_url, "models")).timeout(Duration::from_secs(20));
    let builder = match apply_auth(builder, model, api_key) {
        Ok(builder) => builder,
        Err(err) => return CredentialCheck::Unverified(err.message),
    };
    let response = match builder.send().await {
        Ok(response) => response,
        Err(err) => return CredentialCheck::Unverified(format!("request failed: {}", error_chain(&err))),
    };
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        let body = response.text().await.unwrap_or_default();
        return CredentialCheck::Rejected(ProviderError::from_status(status, &body, None).message);
    }
    if !status.is_success() {
        return CredentialCheck::Unverified(format!("listing models returned HTTP {status}"));
    }
    match response.json::<Value>().await {
        Ok(body) => CredentialCheck::Accepted(body.get("data").and_then(Value::as_array).map_or(0, Vec::len)),
        Err(err) => CredentialCheck::Unverified(format!("unexpected model list: {err}")),
    }
}

fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers.get("retry-after")?.to_str().ok()?;
    value.trim().parse::<f64>().ok().filter(|secs| *secs >= 0.0).map(Duration::from_secs_f64)
}

/// Send the request and return the response if it succeeded.
pub(crate) async fn send(
    builder: reqwest::RequestBuilder,
    cancel: &CancellationToken,
) -> Result<reqwest::Response, ProviderError> {
    let response = tokio::select! {
        _ = cancel.cancelled() => return Err(ProviderError::fatal("aborted")),
        response = builder.send() => response,
    };
    let response =
        response.map_err(|err| ProviderError::retryable(format!("request failed: {}", error_chain(&err))))?;
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let retry_after = retry_after(response.headers());
    let body = response.text().await.unwrap_or_default();
    Err(ProviderError::from_status(status, &body, retry_after))
}

fn error_chain(err: &dyn std::error::Error) -> String {
    let mut message = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}

/// Merge model-level `extraBody` fields into a request body.
fn merge_extra_body(body: &mut Value, model: &Model) {
    if let (Some(Value::Object(extra)), Value::Object(target)) = (&model.extra_body, body) {
        for (key, value) in extra {
            target.insert(key.clone(), value.clone());
        }
    }
}

/// Parse accumulated tool-call JSON strictly. Empty input means no arguments.
fn parse_tool_arguments(raw: &str) -> Result<Value, String> {
    if raw.trim().is_empty() {
        return Ok(Value::Object(Default::default()));
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(value @ Value::Object(_)) => Ok(value),
        _ => Err(raw.to_string()),
    }
}

/// Fill in the final arguments of the tool call at `index` from its raw JSON.
fn finish_tool_call(out: &mut AssistantMessage, index: usize, raw: &str) {
    if let Some(ContentBlock::ToolCall { arguments, invalid_arguments, .. }) = out.content.get_mut(index) {
        match parse_tool_arguments(raw) {
            Ok(value) => *arguments = value,
            Err(raw) => {
                *arguments = Value::Object(Default::default());
                *invalid_arguments = Some(raw);
            }
        }
    }
}

/// Messages ready for a provider: errored/aborted assistant turns dropped, orphaned tool calls
/// answered with synthetic error results, and images removed for text-only models.
pub(crate) fn normalize_messages(messages: &[Message], model: &Model) -> Vec<Message> {
    use crate::message::ToolResultMessage;

    let mut result: Vec<Message> = Vec::with_capacity(messages.len());
    let mut pending: Vec<(String, String)> = Vec::new();
    let mut answered: std::collections::HashSet<String> = Default::default();

    let close = |result: &mut Vec<Message>,
                 pending: &mut Vec<(String, String)>,
                 answered: &mut std::collections::HashSet<String>| {
        for (id, name) in pending.drain(..) {
            if !answered.contains(&id) {
                result.push(Message::ToolResult(ToolResultMessage {
                    tool_call_id: id,
                    tool_name: name,
                    content: vec![ContentBlock::text("No result provided")],
                    is_error: true,
                    details: None,
                    timestamp: now_ms(),
                    duration_ms: None,
                }));
            }
        }
        answered.clear();
    };

    for message in messages {
        match message {
            Message::Assistant(assistant) => {
                close(&mut result, &mut pending, &mut answered);
                if matches!(assistant.stop_reason, StopReason::Error | StopReason::Aborted) {
                    continue;
                }
                pending = assistant.tool_calls().map(|call| (call.id.to_string(), call.name.to_string())).collect();
                result.push(message.clone());
            }
            Message::ToolResult(tool_result) => {
                answered.insert(tool_result.tool_call_id.clone());
                result.push(message.clone());
            }
            Message::BashExecution(bash) if bash.exclude_from_context => {}
            _ => {
                close(&mut result, &mut pending, &mut answered);
                result.push(message.clone());
            }
        }
    }
    close(&mut result, &mut pending, &mut answered);

    if !model.images {
        for message in &mut result {
            let content = match message {
                Message::User(m) => &mut m.content,
                Message::ToolResult(m) => &mut m.content,
                _ => continue,
            };
            for block in content.iter_mut() {
                if matches!(block, ContentBlock::Image { .. }) {
                    *block = ContentBlock::text("(image omitted: model does not support images)");
                }
            }
        }
    }
    result
}

/// Whether an assistant message was produced by `model`, so its thinking signatures are valid.
fn same_model(message: &AssistantMessage, model: &Model) -> bool {
    message.provider == model.provider && message.model == model.id && message.api == model.api
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v1_urls_accept_bases_with_or_without_v1() {
        assert_eq!(v1_url("https://api.anthropic.com", "messages"), "https://api.anthropic.com/v1/messages");
        assert_eq!(v1_url("http://localhost:4000/v1/", "models"), "http://localhost:4000/v1/models");
    }

    #[test]
    fn overflow_detection() {
        assert!(is_context_overflow(
            "HTTP 400: invalid_request_error: prompt is too long: 250000 tokens > 200000 maximum"
        ));
        assert!(is_context_overflow("This model's maximum context length is 131072 tokens"));
        assert!(!is_context_overflow("HTTP 401: authentication_error: invalid x-api-key"));
    }

    #[test]
    fn error_message_extraction() {
        let body = r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
        assert_eq!(extract_error_message(body), "overloaded_error: Overloaded");
        assert_eq!(extract_error_message("plain text"), "plain text");
    }

    #[test]
    fn tool_argument_parsing() {
        assert_eq!(parse_tool_arguments("").unwrap(), serde_json::json!({}));
        assert_eq!(parse_tool_arguments(r#"{"a":1}"#).unwrap(), serde_json::json!({"a": 1}));
        assert!(parse_tool_arguments(r#"{"a":"#).is_err());
        assert!(parse_tool_arguments("[1]").is_err());
    }
}
