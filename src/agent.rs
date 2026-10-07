//! The agent runtime: owns the conversation, runs the model/tool loop, and emits events.
//!
//! [`Agent`] is a cheap, cloneable handle. A run executes on a background task so the UI or RPC
//! client can keep reading state, queueing messages, and aborting while the model works. State
//! lives behind a mutex that is only held for short, non-async sections.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::cache_warming;
use crate::compaction::{self, CompactionResult};
use crate::config::{CacheRetention, CacheWarming, Model, ModelRegistry, Settings, ThinkingLevel};
use crate::context::{ContextFile, Skill};
use crate::message::{
    AssistantMessage, BashExecutionMessage, ContentBlock, Message, StopReason, ToolCallRef, ToolResultMessage, Usage,
    UserMessage, now_ms,
};
use crate::provider::{self, ErrorKind, Request, StreamDelta, ToolSpec};
use crate::session::{Entry, SessionStore, UsageKind, iso_now, new_entry_id};
use crate::tools::{ShellConfig, Tool, ToolContext, ToolOutput, UpdateFn};

// ---------------------------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionReason {
    Manual,
    Threshold,
    Overflow,
}

/// Events emitted while the agent works. Serialized as the JSON/RPC event stream.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum AgentEvent {
    AgentStart,
    AgentEnd {
        messages: Vec<Message>,
    },
    TurnStart,
    TurnEnd {
        message: Message,
        tool_results: Vec<Message>,
    },
    MessageStart {
        message: Message,
    },
    MessageUpdate {
        assistant_message_event: StreamDelta,
        usage: Usage,
    },
    MessageEnd {
        message: Message,
    },
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
        args: Value,
    },
    ToolExecutionUpdate {
        tool_call_id: String,
        tool_name: String,
        partial_result: ToolOutput,
    },
    ToolExecutionEnd {
        tool_call_id: String,
        tool_name: String,
        result: ToolOutput,
        is_error: bool,
        duration_ms: u64,
    },
    CompactionStart {
        reason: CompactionReason,
    },
    CompactionEnd {
        reason: CompactionReason,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<CompactionResult>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    AutoRetryStart {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        error_message: String,
    },
    /// A queued steering or follow-up message was consumed into the conversation.
    QueueUpdate {
        steering: Vec<String>,
        follow_up: Vec<String>,
    },
    /// The prompt cache was refreshed before it expired, at this cost (see `cache_warming`).
    CacheWarm {
        usage: Usage,
    },
}

// ---------------------------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Busy {
    Running,
    Compacting,
    Bash,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum QueueMode {
    Steer,
    FollowUp,
}

/// Everything fixed for the lifetime of the agent.
pub struct AgentSetup {
    pub cwd: PathBuf,
    pub settings: Settings,
    pub tools: Vec<Arc<dyn Tool>>,
    pub system_prompt: String,
    pub context_files: Vec<ContextFile>,
    pub skills: Vec<Skill>,
    pub skill_warnings: Vec<String>,
}

struct State {
    /// Configured models; replaced when credentials change (`/login`).
    registry: Arc<ModelRegistry>,
    model: Model,
    thinking: ThinkingLevel,
    session: SessionStore,
    messages: Vec<Message>,
    busy: Option<Busy>,
    cancel: Option<CancellationToken>,
    steering: VecDeque<UserMessage>,
    follow_up: VecDeque<UserMessage>,
    auto_compaction: bool,
    /// Auto-compact windows set per model (`provider/model-id`), from settings and `/autocompact`.
    auto_compact_windows: BTreeMap<String, u64>,
    auto_retry: bool,
    /// Replaces the cache lifetime and refresh delay of the configured retention.
    #[cfg(test)]
    cache_timing: Option<cache_warming::Timing>,
}

/// A model request as sent, so it can be repeated to refresh its cached prefix.
struct SentRequest {
    model: Model,
    thinking: ThinkingLevel,
    messages: Vec<Message>,
    retention: CacheRetention,
    /// When the request was sent, so when its cache entry was written or last read.
    started: Instant,
}

struct Inner {
    setup: AgentSetup,
    client: reqwest::Client,
    shell: ShellConfig,
    tool_specs: Vec<ToolSpec>,
    state: Mutex<State>,
    events: mpsc::UnboundedSender<AgentEvent>,
    idle: watch::Sender<bool>,
}

#[derive(Clone)]
pub struct Agent {
    inner: Arc<Inner>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSnapshot {
    pub model: Model,
    pub thinking_level: ThinkingLevel,
    pub is_streaming: bool,
    pub is_compacting: bool,
    pub session_file: Option<PathBuf>,
    pub session_id: String,
    pub session_name: Option<String>,
    pub auto_compaction_enabled: bool,
    /// Configured context size at which auto-compaction triggers; `None` means the model default.
    pub auto_compact_window: Option<u64>,
    /// Context size at which auto-compaction actually triggers for the current model.
    pub compaction_threshold: u64,
    pub auto_retry_enabled: bool,
    pub message_count: usize,
    pub pending_message_count: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStats {
    pub session_file: Option<PathBuf>,
    pub session_id: String,
    pub user_messages: usize,
    pub assistant_messages: usize,
    pub tool_calls: usize,
    pub tool_results: usize,
    pub total_messages: usize,
    pub tokens: Usage,
    pub cost: f64,
    /// Estimated tokens in the current context and the model's window.
    pub context_tokens: u64,
    pub context_window: u64,
    /// Configured auto-compact window for the current model, if any.
    pub auto_compact_window: Option<u64>,
    /// Context size at which auto-compaction triggers for the current model.
    pub compaction_threshold: u64,
}

fn text_of(message: &UserMessage) -> String {
    crate::message::content_text(&message.content)
}

impl Agent {
    pub fn new(
        setup: AgentSetup,
        registry: ModelRegistry,
        model: Model,
        thinking: ThinkingLevel,
        session: SessionStore,
        events: mpsc::UnboundedSender<AgentEvent>,
    ) -> Result<Agent> {
        let client = provider::http_client()?;
        let shell = ShellConfig::resolve(setup.settings.shell_path.as_deref());
        let tool_specs = setup.tools.iter().map(|t| t.spec()).collect();
        let messages = session.messages();
        let thinking = model.clamp_thinking(thinking);
        let state = State {
            auto_compaction: setup.settings.compaction.enabled,
            auto_compact_windows: setup
                .settings
                .model_settings
                .iter()
                .filter_map(|(key, settings)| Some((key.clone(), settings.auto_compact_window?)))
                .collect(),
            auto_retry: setup.settings.retry.enabled,
            registry: Arc::new(registry),
            model,
            thinking,
            session,
            messages,
            busy: None,
            cancel: None,
            steering: VecDeque::new(),
            follow_up: VecDeque::new(),
            #[cfg(test)]
            cache_timing: None,
        };
        let (idle, _) = watch::channel(true);
        let agent = Agent {
            inner: Arc::new(Inner { setup, client, shell, tool_specs, state: Mutex::new(state), events, idle }),
        };
        agent.record_model_if_changed()?;
        Ok(agent)
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.inner.state.lock().expect("agent state poisoned")
    }

    fn emit(&self, event: AgentEvent) {
        // The receiver going away (UI shutting down) is not an error for the agent.
        let _ = self.inner.events.send(event);
    }

    pub fn setup(&self) -> &AgentSetup {
        &self.inner.setup
    }

    pub fn cwd(&self) -> &Path {
        &self.inner.setup.cwd
    }

    /// Persist the current model and thinking level if they differ from the session's record.
    fn record_model_if_changed(&self) -> Result<()> {
        let mut state = self.state();
        let model_key = (state.model.provider.clone(), state.model.local_id().to_string());
        if state.session.last_model().as_ref() != Some(&model_key) {
            state.session.append(Entry::ModelChange {
                id: new_entry_id(),
                timestamp: iso_now(),
                provider: model_key.0,
                model_id: model_key.1,
            })?;
        }
        let thinking = state.thinking;
        if state.session.last_thinking_level() != Some(thinking) {
            state.session.append(Entry::ThinkingLevelChange {
                id: new_entry_id(),
                timestamp: iso_now(),
                thinking_level: thinking,
            })?;
        }
        Ok(())
    }

    // --- Accessors ----------------------------------------------------------------------------

    pub fn model(&self) -> Model {
        self.state().model.clone()
    }

    pub fn thinking(&self) -> ThinkingLevel {
        self.state().thinking
    }

    pub fn busy(&self) -> Option<Busy> {
        self.state().busy
    }

    pub fn messages(&self) -> Vec<Message> {
        self.state().messages.clone()
    }

    /// Every message recorded in the session, including compacted history.
    pub fn session_messages(&self) -> Vec<Message> {
        self.state().session.all_messages().cloned().collect()
    }

    pub fn session_path(&self) -> Option<PathBuf> {
        self.state().session.path().map(Path::to_path_buf)
    }

    pub fn session_name(&self) -> Option<String> {
        self.state().session.name()
    }

    pub fn queued(&self) -> (Vec<String>, Vec<String>) {
        let state = self.state();
        (state.steering.iter().map(text_of).collect(), state.follow_up.iter().map(text_of).collect())
    }

    pub fn snapshot(&self) -> AgentSnapshot {
        let state = self.state();
        AgentSnapshot {
            model: state.model.clone(),
            thinking_level: state.thinking,
            is_streaming: state.busy == Some(Busy::Running),
            is_compacting: state.busy == Some(Busy::Compacting),
            session_file: state.session.path().map(Path::to_path_buf),
            session_id: state.session.id().to_string(),
            session_name: state.session.name(),
            auto_compaction_enabled: state.auto_compaction,
            auto_compact_window: self.configured_window(&state),
            compaction_threshold: self.threshold(&state),
            auto_retry_enabled: state.auto_retry,
            message_count: state.messages.len(),
            pending_message_count: state.steering.len() + state.follow_up.len(),
        }
    }

    pub fn stats(&self) -> SessionStats {
        let state = self.state();
        let mut stats = SessionStats {
            session_file: state.session.path().map(Path::to_path_buf),
            session_id: state.session.id().to_string(),
            user_messages: 0,
            assistant_messages: 0,
            tool_calls: 0,
            tool_results: 0,
            total_messages: 0,
            tokens: state.session.total_usage(),
            cost: 0.0,
            context_tokens: compaction::estimate_context_tokens(&state.messages),
            context_window: state.model.context_window,
            auto_compact_window: self.configured_window(&state),
            compaction_threshold: self.threshold(&state),
        };
        stats.cost = stats.tokens.cost.total;
        for message in state.session.all_messages() {
            stats.total_messages += 1;
            match message {
                Message::User(_) => stats.user_messages += 1,
                Message::Assistant(a) => {
                    stats.assistant_messages += 1;
                    stats.tool_calls += a.tool_calls().count();
                }
                Message::ToolResult(_) => stats.tool_results += 1,
                _ => {}
            }
        }
        stats
    }

    pub fn last_assistant_text(&self) -> Option<String> {
        self.state().messages.iter().rev().find_map(|m| match m {
            Message::Assistant(a) if !a.text().is_empty() => Some(a.text()),
            _ => None,
        })
    }

    // --- Settings -----------------------------------------------------------------------------

    pub fn registry(&self) -> Arc<ModelRegistry> {
        self.state().registry.clone()
    }

    /// Replace the configured models (after `/login`). The current model is refreshed from the
    /// new registry so it uses the new base URL and key.
    pub fn set_registry(&self, registry: ModelRegistry) {
        let mut state = self.state();
        let key = state.model.key();
        if let Some(model) = registry.all().iter().find(|m| m.key() == key) {
            state.model = model.clone();
        }
        state.registry = Arc::new(registry);
    }

    /// The HTTP client used for model requests.
    pub fn http_client(&self) -> &reqwest::Client {
        &self.inner.client
    }

    /// Switch models. Fails without changing anything when the model has no API key.
    pub fn set_model(&self, model: Model) -> Result<()> {
        crate::config::ensure_credentials(&model)?;
        {
            let mut state = self.state();
            state.thinking = model.clamp_thinking(state.thinking);
            state.model = model;
        }
        self.record_model_if_changed()
    }

    /// Set the thinking level, clamped to what the model supports. Returns the applied level.
    pub fn set_thinking(&self, level: ThinkingLevel) -> Result<ThinkingLevel> {
        let applied = {
            let mut state = self.state();
            state.thinking = state.model.clamp_thinking(level);
            state.thinking
        };
        self.record_model_if_changed()?;
        Ok(applied)
    }

    /// Advance to the next supported thinking level, wrapping around.
    pub fn cycle_thinking(&self) -> Result<Option<ThinkingLevel>> {
        let next = {
            let state = self.state();
            let levels = &state.model.thinking_levels;
            if levels.len() < 2 {
                return Ok(None);
            }
            let index = levels.iter().position(|l| *l == state.thinking).unwrap_or(0);
            levels[(index + 1) % levels.len()]
        };
        self.set_thinking(next).map(Some)
    }

    pub fn set_auto_compaction(&self, enabled: bool) {
        self.state().auto_compaction = enabled;
    }

    /// Set the current model's auto-compact window for this agent; `None` returns to the
    /// default. Persisting it is the caller's choice.
    pub fn set_auto_compact_window(&self, window: Option<u64>) {
        let mut state = self.state();
        let key = state.model.key();
        match window {
            Some(window) => state.auto_compact_windows.insert(key, window),
            None => state.auto_compact_windows.remove(&key),
        };
    }

    fn configured_window(&self, state: &State) -> Option<u64> {
        state.auto_compact_windows.get(&state.model.key()).copied().or(self
            .inner
            .setup
            .settings
            .compaction
            .auto_compact_window)
    }

    fn threshold(&self, state: &State) -> u64 {
        compaction::compaction_threshold(
            state.model.context_window,
            self.inner.setup.settings.compaction.reserve_tokens,
            self.configured_window(state),
        )
    }

    pub fn set_auto_retry(&self, enabled: bool) {
        self.state().auto_retry = enabled;
    }

    pub fn set_session_name(&self, name: &str) -> Result<()> {
        let name = name.trim();
        if name.is_empty() {
            bail!("session name must not be empty");
        }
        self.state().session.append(Entry::SessionInfo {
            id: new_entry_id(),
            timestamp: iso_now(),
            name: name.to_string(),
        })
    }

    // --- Sessions -----------------------------------------------------------------------------

    fn require_idle(state: &State) -> Result<()> {
        match state.busy {
            None => Ok(()),
            Some(Busy::Running) => bail!("the agent is busy; abort or wait for it to finish"),
            Some(Busy::Compacting) => bail!("compaction is in progress"),
            Some(Busy::Bash) => bail!("a command is running"),
        }
    }

    /// Start a new session, saved to disk only if the current one is.
    pub fn new_session(&self) -> Result<()> {
        {
            let mut state = self.state();
            Self::require_idle(&state)?;
            let persist = state.session.path().is_some();
            state.session = SessionStore::create(&self.inner.setup.cwd, persist);
            state.messages.clear();
            state.steering.clear();
            state.follow_up.clear();
        }
        self.record_model_if_changed()
    }

    /// Load a saved session, restoring its model and thinking level when available.
    pub fn switch_session(&self, path: &Path) -> Result<Vec<String>> {
        let (session, warnings) = SessionStore::open(path)?;
        {
            let mut state = self.state();
            Self::require_idle(&state)?;
            if let Some((provider, id)) = session.last_model()
                && let Ok(model) = state.registry.find(&format!("{provider}/{id}"))
            {
                state.model = model;
            }
            if let Some(level) = session.last_thinking_level() {
                state.thinking = state.model.clamp_thinking(level);
            }
            state.messages = session.messages();
            state.session = session;
            state.steering.clear();
            state.follow_up.clear();
        }
        self.record_model_if_changed()?;
        Ok(warnings)
    }

    // --- Prompting ----------------------------------------------------------------------------

    /// Send a user message. When the agent is running, `queue` decides whether the message
    /// steers the current run or waits as a follow-up; without it, a busy agent is an error.
    pub fn prompt(&self, content: Vec<ContentBlock>, queue: Option<QueueMode>) -> Result<PromptDisposition> {
        let message = UserMessage::new(content);
        let mut state = self.state();
        match state.busy {
            None => {
                self.spawn_run(&mut state, vec![Message::User(message)]);
                Ok(PromptDisposition::Started)
            }
            Some(Busy::Running) => {
                let Some(mode) = queue else {
                    bail!("the agent is busy; send the message as a steer or follow-up");
                };
                match mode {
                    QueueMode::Steer => state.steering.push_back(message),
                    QueueMode::FollowUp => state.follow_up.push_back(message),
                }
                drop(state);
                self.emit_queue();
                Ok(PromptDisposition::Queued(mode))
            }
            Some(_) => {
                // Compaction or a user command: queue as a follow-up so nothing is lost.
                state.follow_up.push_back(message);
                drop(state);
                self.emit_queue();
                Ok(PromptDisposition::Queued(QueueMode::FollowUp))
            }
        }
    }

    fn emit_queue(&self) {
        let (steering, follow_up) = self.queued();
        self.emit(AgentEvent::QueueUpdate { steering, follow_up });
    }

    /// Remove all queued messages, returning their text.
    pub fn clear_queue(&self) -> (Vec<String>, Vec<String>) {
        let queued = {
            let mut state = self.state();
            let steering = state.steering.drain(..).map(|m| text_of(&m)).collect();
            let follow_up = state.follow_up.drain(..).map(|m| text_of(&m)).collect();
            (steering, follow_up)
        };
        self.emit_queue();
        queued
    }

    /// Abort the current run or command. Queued messages are removed and returned.
    pub fn abort(&self) -> Vec<String> {
        if let Some(cancel) = self.state().cancel.clone() {
            cancel.cancel();
        }
        let (mut steering, follow_up) = self.clear_queue();
        steering.extend(follow_up);
        steering
    }

    /// Wait until no run, compaction, or command is in progress.
    pub async fn wait_idle(&self) {
        let mut idle = self.inner.idle.subscribe();
        // An error means the sender was dropped, which cannot happen while `self` is alive.
        let _ = idle.wait_for(|idle| *idle).await;
    }

    /// Mark the agent busy and return the token that cancels the work. The idle signal changes
    /// while the state lock is held, so it always agrees with `busy`.
    fn begin(&self, state: &mut State, busy: Busy) -> CancellationToken {
        let cancel = CancellationToken::new();
        state.busy = Some(busy);
        state.cancel = Some(cancel.clone());
        self.inner.idle.send_replace(false);
        cancel
    }

    fn finish_busy(&self) {
        let mut state = self.state();
        state.busy = None;
        state.cancel = None;
        self.inner.idle.send_replace(true);
    }

    /// Start a run with `messages` on a background task.
    fn spawn_run(&self, state: &mut State, messages: Vec<Message>) {
        let cancel = self.begin(state, Busy::Running);
        let agent = self.clone();
        tokio::spawn(async move { agent.run(messages, cancel).await });
    }

    /// Start the queued follow-ups if the agent is idle (used after compaction or commands).
    fn start_queued_if_idle(&self) {
        let mut state = self.state();
        if state.busy.is_some() || state.follow_up.is_empty() {
            return;
        }
        let pending: Vec<Message> = state.follow_up.drain(..).map(Message::User).collect();
        self.spawn_run(&mut state, pending);
        drop(state);
        self.emit_queue();
    }

    fn append_message(&self, message: Message) {
        let mut state = self.state();
        if let Err(err) = state.session.append_message(message.clone()) {
            eprintln!("warning: failed to save session: {err:#}");
        }
        state.messages.push(message);
    }

    // --- The run loop -------------------------------------------------------------------------

    async fn run(self, initial: Vec<Message>, cancel: CancellationToken) {
        self.emit(AgentEvent::AgentStart);
        let mut new_messages = Vec::new();
        let mut pending = initial;
        let mut recovered_overflow = false;

        loop {
            for message in pending.drain(..) {
                self.emit(AgentEvent::MessageStart { message: message.clone() });
                self.append_message(message.clone());
                self.emit(AgentEvent::MessageEnd { message: message.clone() });
                new_messages.push(message);
            }
            if cancel.is_cancelled() {
                break;
            }

            self.compact_if_needed(&cancel).await;

            self.emit(AgentEvent::TurnStart);
            let (assistant, error, sent) = self.stream_response(&cancel).await;
            let overflow = error == Some(ErrorKind::ContextOverflow);
            let message = Message::Assistant(assistant.clone());
            self.append_message(message.clone());
            self.emit(AgentEvent::MessageEnd { message: message.clone() });
            new_messages.push(message.clone());

            if overflow && !recovered_overflow && self.state().auto_compaction {
                // Compact and retry the request once.
                recovered_overflow = true;
                self.emit(AgentEvent::TurnEnd { message, tool_results: Vec::new() });
                if self.compact_inner(CompactionReason::Overflow, None, &cancel).await.is_ok() {
                    continue;
                }
                break;
            }
            if matches!(assistant.stop_reason, StopReason::Error | StopReason::Aborted) {
                self.emit(AgentEvent::TurnEnd { message, tool_results: Vec::new() });
                break;
            }

            let has_tool_calls = assistant.tool_calls().next().is_some();
            let tool_results = if has_tool_calls {
                let tools = self.execute_tools(&assistant, &cancel);
                tokio::select! {
                    biased;
                    results = tools => results,
                    never = self.keep_cache_warm(&sent, &assistant.usage, &cancel) => match never {},
                }
            } else {
                Vec::new()
            };
            for result in &tool_results {
                self.append_message(result.clone());
                new_messages.push(result.clone());
            }
            self.emit(AgentEvent::TurnEnd { message, tool_results });

            if cancel.is_cancelled() {
                break;
            }
            let steering: Vec<Message> = self.state().steering.drain(..).map(Message::User).collect();
            if !steering.is_empty() {
                self.emit_queue();
            }
            if has_tool_calls || !steering.is_empty() {
                pending = steering;
                continue;
            }
            let follow_up: Vec<Message> = self.state().follow_up.drain(..).map(Message::User).collect();
            if !follow_up.is_empty() {
                self.emit_queue();
                pending = follow_up;
                continue;
            }
            break;
        }

        self.emit(AgentEvent::AgentEnd { messages: new_messages });
        self.finish_busy();
    }

    /// Stream one assistant response, retrying transient failures that happen before any
    /// content arrives. Failures are encoded in the returned message, with their kind beside it,
    /// and the request is returned as sent.
    async fn stream_response(&self, cancel: &CancellationToken) -> (AssistantMessage, Option<ErrorKind>, SentRequest) {
        let (model, thinking, messages, auto_retry) = {
            let state = self.state();
            (state.model.clone(), state.thinking, state.messages.clone(), state.auto_retry)
        };
        let settings = &self.inner.setup.settings;
        let retry = &settings.retry;
        let max_retries = if auto_retry { retry.max_retries } else { 0 };
        let retention = settings.cache_retention;
        let request = Request {
            model: &model,
            system_prompt: &self.inner.setup.system_prompt,
            messages: &messages,
            tools: &self.inner.tool_specs,
            thinking,
            max_tokens: None,
            cache_retention: retention,
        };

        let mut message = provider::new_assistant_message(&model, thinking);
        self.emit(AgentEvent::MessageStart { message: Message::Assistant(message.clone()) });
        let started = Instant::now();
        let mut attempt = 0;
        let error = loop {
            message = provider::new_assistant_message(&model, thinking);
            let events = self.inner.events.clone();
            let mut on_delta = |partial: &AssistantMessage, delta: StreamDelta| {
                let _ = events.send(AgentEvent::MessageUpdate { assistant_message_event: delta, usage: partial.usage });
            };
            let result = provider::stream(&self.inner.client, &request, &mut message, &mut on_delta, cancel).await;
            match result {
                Ok(()) => break None,
                Err(_) if cancel.is_cancelled() => {
                    message.stop_reason = StopReason::Aborted;
                    message.error_message = Some("Request aborted".into());
                    break None;
                }
                Err(err) if err.kind == ErrorKind::Retryable && message.content.is_empty() && attempt < max_retries => {
                    attempt += 1;
                    let backoff = Duration::from_millis(retry.base_delay_ms.saturating_mul(1 << (attempt - 1).min(16)));
                    let delay = err.retry_after.unwrap_or(backoff).min(Duration::from_secs(120));
                    self.emit(AgentEvent::AutoRetryStart {
                        attempt,
                        max_attempts: max_retries,
                        delay_ms: delay.as_millis() as u64,
                        error_message: err.message.clone(),
                    });
                    tokio::select! {
                        _ = cancel.cancelled() => {
                            message.stop_reason = StopReason::Aborted;
                            message.error_message = Some("Request aborted".into());
                            break None;
                        }
                        _ = tokio::time::sleep(delay) => {}
                    }
                }
                Err(err) => {
                    message.stop_reason = StopReason::Error;
                    message.error_message = Some(err.message);
                    break Some(err.kind);
                }
            }
        };
        message.timestamp = now_ms() - started.elapsed().as_millis() as i64;
        message.duration_ms = Some(started.elapsed().as_millis() as u64);
        let sent = SentRequest { model, thinking, messages, retention, started };
        (message, error, sent)
    }

    /// While tools run, refresh the cached prefix of `sent` shortly before it expires whenever
    /// that is cheaper than rewriting it (see `cache_warming`). Never returns; the caller drops it
    /// when the tools finish.
    async fn keep_cache_warm(
        &self,
        sent: &SentRequest,
        usage: &Usage,
        cancel: &CancellationToken,
    ) -> std::convert::Infallible {
        let enabled = self.inner.setup.settings.cache_warming == CacheWarming::Streaming;
        let retention = cache_warming::applied_retention(sent.retention, usage);
        let prompt_tokens = usage.input + usage.cache_read + usage.cache_write;
        let timing = self.cache_timing(retention);
        if let Some(timing) = timing
            .filter(|_| enabled && cache_warming::worthwhile(&sent.model, sent.thinking, retention, prompt_tokens))
        {
            let mut last_used = sent.started;
            loop {
                let due = last_used + timing.delay;
                if due > sent.started + cache_warming::MAX_WARMING_AGE {
                    break;
                }
                tokio::time::sleep_until(due.into()).await;
                let model_changed = self.state().model.key() != sent.model.key();
                if Instant::now() > timing.deadline(due) || model_changed {
                    break;
                }
                let refreshed = Instant::now();
                if !self.refresh_cache(sent, cancel).await {
                    break;
                }
                last_used = refreshed;
            }
        }
        std::future::pending().await
    }

    fn cache_timing(&self, retention: CacheRetention) -> Option<cache_warming::Timing> {
        #[cfg(test)]
        if let Some(timing) = self.state().cache_timing {
            return Some(timing);
        }
        cache_warming::Timing::for_ttl(retention.ttl())
    }

    /// Repeat `sent` with a one-token output limit, which reads its cached prefix and so renews
    /// the cache. Records the cost in the session; returns whether the request succeeded.
    async fn refresh_cache(&self, sent: &SentRequest, cancel: &CancellationToken) -> bool {
        let request = Request {
            model: &sent.model,
            system_prompt: &self.inner.setup.system_prompt,
            messages: &sent.messages,
            tools: &self.inner.tool_specs,
            thinking: sent.thinking,
            max_tokens: Some(1),
            cache_retention: sent.retention,
        };
        let mut response = provider::new_assistant_message(&sent.model, sent.thinking);
        let result = provider::stream(&self.inner.client, &request, &mut response, &mut |_, _| {}, cancel).await;
        if result.is_err() || matches!(response.stop_reason, StopReason::Error | StopReason::Aborted) {
            return false;
        }
        let usage = response.usage;
        let model = &sent.model;
        if let Err(err) =
            self.state().session.append_usage(UsageKind::CacheWarm, &model.provider, model.local_id(), usage)
        {
            eprintln!("warning: failed to save session: {err:#}");
        }
        self.emit(AgentEvent::CacheWarm { usage });
        true
    }

    /// Execute the tool calls of `assistant` in order. Consecutive read-only calls run
    /// concurrently; every other call runs alone, so a command that depends on an earlier write or
    /// edit in the same response sees its result. Results are returned in call order.
    async fn execute_tools(&self, assistant: &AssistantMessage, cancel: &CancellationToken) -> Vec<Message> {
        let calls: Vec<PendingCall> = assistant
            .tool_calls()
            .map(|call| PendingCall {
                tool: self.inner.setup.tools.iter().find(|t| t.name() == call.name).cloned(),
                call,
            })
            .collect();
        let ctx = Arc::new(ToolContext {
            cwd: self.inner.setup.cwd.clone(),
            cancel: cancel.clone(),
            shell: self.inner.shell.clone(),
        });
        let truncated = assistant.stop_reason == StopReason::Length;

        let mut results = Vec::with_capacity(calls.len());
        let mut start = 0;
        while start < calls.len() {
            let end = if calls[start].concurrent() {
                start + calls[start..].iter().take_while(|call| call.concurrent()).count()
            } else {
                start + 1
            };
            let batch = &calls[start..end];
            for call in batch {
                self.emit(AgentEvent::ToolExecutionStart {
                    tool_call_id: call.call.id.to_string(),
                    tool_name: call.call.name.to_string(),
                    args: call.call.arguments.clone(),
                });
            }
            let mut running: FuturesUnordered<_> = batch
                .iter()
                .enumerate()
                .map(|(index, call)| {
                    let ctx = ctx.clone();
                    let events = self.inner.events.clone();
                    async move {
                        let started = Instant::now();
                        let output = call.execute(&ctx, events, truncated).await;
                        (index, output, started.elapsed())
                    }
                })
                .collect();
            let mut batch_results: Vec<Option<Message>> = vec![None; batch.len()];
            while let Some((index, output, elapsed)) = running.next().await {
                let call = &batch[index];
                self.emit(AgentEvent::ToolExecutionEnd {
                    tool_call_id: call.call.id.to_string(),
                    tool_name: call.call.name.to_string(),
                    result: output.clone(),
                    is_error: output.is_error,
                    duration_ms: elapsed.as_millis() as u64,
                });
                batch_results[index] = Some(Message::ToolResult(ToolResultMessage {
                    tool_call_id: call.call.id.to_string(),
                    tool_name: call.call.name.to_string(),
                    content: output.content,
                    is_error: output.is_error,
                    details: output.details,
                    timestamp: now_ms(),
                    duration_ms: Some(elapsed.as_millis() as u64),
                }));
            }
            results.extend(batch_results.into_iter().flatten());
            start = end;
        }
        results
    }

    // --- Compaction ---------------------------------------------------------------------------

    async fn compact_if_needed(&self, cancel: &CancellationToken) {
        let (enabled, tokens, threshold) = {
            let state = self.state();
            (state.auto_compaction, compaction::estimate_context_tokens(&state.messages), self.threshold(&state))
        };
        if enabled && tokens > threshold {
            // Failures are reported through events; the run continues with the full context.
            let _ = self.compact_inner(CompactionReason::Threshold, None, cancel).await;
        }
    }

    /// Manually compact the conversation (`/compact`) on a background task. Fails at once when
    /// the agent is busy; once started, progress and failures are also reported through
    /// compaction events.
    pub fn compact(&self, instructions: Option<String>) -> Result<JoinHandle<Result<CompactionResult>>> {
        let cancel = {
            let mut state = self.state();
            Self::require_idle(&state)?;
            self.begin(&mut state, Busy::Compacting)
        };
        let agent = self.clone();
        Ok(tokio::spawn(async move {
            let result = agent.compact_inner(CompactionReason::Manual, instructions.as_deref(), &cancel).await;
            agent.finish_busy();
            agent.start_queued_if_idle();
            result
        }))
    }

    async fn compact_inner(
        &self,
        reason: CompactionReason,
        instructions: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<CompactionResult> {
        self.emit(AgentEvent::CompactionStart { reason });
        let result = self.do_compact(instructions, cancel).await;
        match &result {
            Ok(result) => self.emit(AgentEvent::CompactionEnd { reason, result: Some(result.clone()), error: None }),
            Err(err) => self.emit(AgentEvent::CompactionEnd { reason, result: None, error: Some(format!("{err:#}")) }),
        }
        result
    }

    async fn do_compact(&self, instructions: Option<&str>, cancel: &CancellationToken) -> Result<CompactionResult> {
        let settings = &self.inner.setup.settings.compaction;
        let (model, plan) = {
            let state = self.state();
            let items = state.session.context();
            let previous_details = state.session.entries().iter().rev().find_map(|e| match e {
                Entry::Compaction { details, .. } => Some(details.clone()),
                _ => None,
            });
            let plan = compaction::plan(&items, settings.keep_recent_tokens, previous_details.flatten().as_ref());
            (state.model.clone(), plan)
        };
        let plan = plan.ok_or_else(|| {
            anyhow!(
                "nothing to compact: the conversation fits within the {} most recent tokens that compaction keeps \
                 (compaction.keepRecentTokens)",
                settings.keep_recent_tokens
            )
        })?;
        let result =
            compaction::summarize(&self.inner.client, &model, &plan, instructions, settings.reserve_tokens, cancel)
                .await
                .map_err(|err| anyhow!(err.message))?;

        let mut state = self.state();
        let id = new_entry_id();
        state.session.append(Entry::Compaction {
            first_kept_entry_id: result.first_kept_entry_id.clone().unwrap_or_else(|| id.clone()),
            id,
            timestamp: iso_now(),
            summary: result.summary.clone(),
            tokens_before: result.tokens_before,
            details: Some(result.details.clone()),
            usage: Some(result.usage),
        })?;
        state.messages = state.session.messages();
        Ok(result)
    }

    // --- User shell commands ------------------------------------------------------------------

    /// Run a `!command` (or `!!command` with `exclude_from_context`) and record its output.
    pub async fn run_bash(
        &self,
        command: &str,
        exclude_from_context: bool,
        on_update: impl Fn(&str),
    ) -> Result<BashExecutionMessage> {
        let cancel = {
            let mut state = self.state();
            Self::require_idle(&state)?;
            self.begin(&mut state, Busy::Bash)
        };
        let result = crate::tools::run_shell_command(
            &self.inner.shell,
            command,
            &self.inner.setup.cwd,
            None,
            &cancel,
            |snapshot| on_update(&snapshot.content),
        )
        .await;
        let message = result.map(|result| BashExecutionMessage {
            command: command.to_string(),
            output: result.output,
            exit_code: result.exit_code,
            cancelled: result.cancelled,
            truncated: result.truncation.truncated,
            full_output_path: result.full_output_path.map(|p| p.display().to_string()),
            exclude_from_context,
            timestamp: now_ms(),
        });
        // Record the output before going idle so a prompt sent right after it sees the output.
        if let Ok(message) = &message {
            self.append_message(Message::BashExecution(message.clone()));
        }
        self.finish_busy();
        self.start_queued_if_idle();
        message
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", tag = "status", content = "mode")]
pub enum PromptDisposition {
    Started,
    Queued(QueueMode),
}

/// A tool call from an assistant message, resolved to its tool.
struct PendingCall<'a> {
    call: ToolCallRef<'a>,
    tool: Option<Arc<dyn Tool>>,
}

impl PendingCall<'_> {
    /// Whether this call may run alongside others. Calls that cannot run (invalid arguments,
    /// unknown tool) have no side effects.
    fn concurrent(&self) -> bool {
        self.call.invalid_arguments.is_some() || self.tool.as_ref().is_none_or(|tool| tool.read_only())
    }

    async fn execute(
        &self,
        ctx: &ToolContext,
        events: mpsc::UnboundedSender<AgentEvent>,
        truncated: bool,
    ) -> ToolOutput {
        if let Some(raw) = self.call.invalid_arguments {
            let note =
                if truncated { " The response hit the output token limit before the call was complete." } else { "" };
            return ToolOutput::error(format!(
                "{{\"INVALID_JSON\": {}}}\nThe tool arguments were not valid JSON, so the tool did not run.{note}",
                Value::String(raw.to_string())
            ));
        }
        if ctx.cancel.is_cancelled() {
            return ToolOutput::error("Operation aborted");
        }
        let Some(tool) = &self.tool else {
            return ToolOutput::error(format!("Tool {} not found", self.call.name));
        };
        let update: UpdateFn = {
            let (id, name) = (self.call.id.to_string(), self.call.name.to_string());
            Arc::new(move |partial: ToolOutput| {
                let _ = events.send(AgentEvent::ToolExecutionUpdate {
                    tool_call_id: id.clone(),
                    tool_name: name.clone(),
                    partial_result: partial,
                });
            })
        };
        match tool.execute(ctx, self.call.arguments.clone(), update).await {
            Ok(output) => output,
            Err(err) => ToolOutput::error(format!("{err:#}")),
        }
    }
}

/// Build user message content from text and images.
pub fn user_content(text: &str, images: Vec<ContentBlock>) -> Vec<ContentBlock> {
    let mut content = Vec::with_capacity(images.len() + 1);
    if !text.is_empty() {
        content.push(ContentBlock::text(text));
    }
    content.extend(images);
    content
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Api, ConfigValue, Reasoning, builtin_anthropic_models};
    use crate::testing::{MockResponse, MockServer};
    use serde_json::json;

    fn model(url: &str, api: Api) -> Model {
        let mut model = builtin_anthropic_models().into_iter().find(|m| m.id == "claude-opus-5-5").unwrap();
        model.base_url = url.to_string();
        model.api_key = Some(ConfigValue::Literal("test-key".into()));
        if api == Api::OpenAiCompletions {
            model.provider = "litellm".into();
            model.api = api;
            model.reasoning = Reasoning::Effort;
            model.thinking_levels = vec![ThinkingLevel::Low, ThinkingLevel::Medium, ThinkingLevel::High];
        }
        model
    }

    fn agent(model: Model, cwd: &Path) -> (Agent, mpsc::UnboundedReceiver<AgentEvent>) {
        let tools = crate::tools::select_tools(&["bash".into(), "read".into()]).unwrap();
        agent_with_tools(model, cwd, tools)
    }

    fn agent_with_tools(
        model: Model,
        cwd: &Path,
        tools: Vec<Arc<dyn Tool>>,
    ) -> (Agent, mpsc::UnboundedReceiver<AgentEvent>) {
        let mut settings = Settings::default();
        settings.retry.base_delay_ms = 10;
        let setup = AgentSetup {
            cwd: cwd.to_path_buf(),
            settings,
            tools,
            system_prompt: "test system prompt".into(),
            context_files: Vec::new(),
            skills: Vec::new(),
            skill_warnings: Vec::new(),
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let session = SessionStore::create(cwd, false);
        let registry = ModelRegistry::from_models(vec![model.clone()]);
        (Agent::new(setup, registry, model, ThinkingLevel::High, session, tx).unwrap(), rx)
    }

    fn anthropic_tool_call(id: &str, command: &str) -> MockResponse {
        anthropic_tool_calls(&[(id, "bash", json!({"command": command}))])
    }

    /// One assistant response containing several tool calls of `(id, tool, arguments)`.
    fn anthropic_tool_calls(calls: &[(&str, &str, Value)]) -> MockResponse {
        let mut events =
            vec![json!({"type": "message_start", "message": {"usage": {"input_tokens": 100, "output_tokens": 1}}})];
        for (index, (id, name, args)) in calls.iter().enumerate() {
            events.extend([
                json!({"type": "content_block_start", "index": index, "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}}),
                json!({"type": "content_block_delta", "index": index, "delta": {"type": "input_json_delta", "partial_json": args.to_string()}}),
                json!({"type": "content_block_stop", "index": index}),
            ]);
        }
        events.extend([
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 20}}),
            json!({"type": "message_stop"}),
        ]);
        MockResponse::anthropic(&events)
    }

    fn anthropic_text(text: &str) -> MockResponse {
        MockResponse::anthropic(&[
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 150, "output_tokens": 1}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 5}}),
            json!({"type": "message_stop"}),
        ])
    }

    fn roles(messages: &[Message]) -> Vec<&'static str> {
        messages
            .iter()
            .map(|m| match m {
                Message::User(_) => "user",
                Message::Assistant(_) => "assistant",
                Message::ToolResult(_) => "toolResult",
                Message::BashExecution(_) => "bash",
                Message::CompactionSummary(_) => "summary",
            })
            .collect()
    }

    /// Stands in for a long-running command: finishes once `done` holds.
    struct WaitTool {
        done: Box<dyn Fn() -> bool + Send + Sync>,
    }

    #[async_trait::async_trait]
    impl Tool for WaitTool {
        fn name(&self) -> &'static str {
            "wait"
        }
        fn description(&self) -> String {
            "Wait".into()
        }
        fn parameters(&self) -> Value {
            json!({"type": "object"})
        }
        fn snippet(&self) -> &'static str {
            "Wait"
        }
        async fn execute(&self, _: &ToolContext, _: Value, _: UpdateFn) -> anyhow::Result<ToolOutput> {
            while !(self.done)() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Ok(ToolOutput::text("waited"))
        }
    }

    #[tokio::test]
    async fn keeps_the_prompt_cache_warm_while_a_tool_runs() {
        // The request before the tool call wrote a 100k-token cache entry: worth keeping.
        let tool_call = MockResponse::anthropic(&[
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 10, "cache_creation_input_tokens": 100_000, "output_tokens": 1}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "wait", "input": {}}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 5}}),
            json!({"type": "message_stop"}),
        ]);
        let refresh = MockResponse::anthropic(&[
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 0, "cache_read_input_tokens": 100_010, "output_tokens": 1}}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "max_tokens"}, "usage": {"output_tokens": 1}}),
            json!({"type": "message_stop"}),
        ]);
        let server = MockServer::start(vec![tool_call, refresh, anthropic_text("done")]).await;
        let dir = tempfile::tempdir().unwrap();
        let agent_slot: Arc<std::sync::OnceLock<Agent>> = Arc::default();
        let slot = agent_slot.clone();
        // The tool runs until the refresh has been recorded.
        let tool =
            WaitTool { done: Box::new(move || slot.get().is_some_and(|agent| agent.stats().tokens.cache_read > 0)) };
        let (agent, mut events) =
            agent_with_tools(model(&server.url, Api::AnthropicMessages), dir.path(), vec![Arc::new(tool)]);
        agent.state().cache_timing =
            Some(cache_warming::Timing { ttl: Duration::from_secs(5), delay: Duration::from_millis(200) });
        agent_slot.set(agent.clone()).ok();

        agent.prompt(vec![ContentBlock::text("wait")], None).unwrap();
        // Without a refresh, the tool would wait forever.
        tokio::time::timeout(Duration::from_secs(10), agent.wait_idle()).await.expect("the cache was not refreshed");

        assert_eq!(agent.last_assistant_text().as_deref(), Some("done"));
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        // The refresh repeats the request before the tool call, asking for a single token.
        assert_eq!(requests[1]["max_tokens"], json!(1));
        assert_eq!(requests[1]["messages"], requests[0]["messages"]);
        assert_eq!(requests[1]["system"], requests[0]["system"]);
        assert_eq!(requests[2]["messages"][2]["content"][0]["tool_use_id"], json!("toolu_1"));
        // Its cost counts toward the session.
        assert_eq!(agent.stats().tokens.cache_read, 100_010);
        let mut kinds = Vec::new();
        while let Ok(event) = events.try_recv() {
            kinds.push(serde_json::to_value(&event).unwrap()["type"].as_str().unwrap().to_string());
        }
        assert_eq!(kinds.iter().filter(|kind| *kind == "cache_warm").count(), 1);
    }

    #[tokio::test]
    async fn runs_tool_loop_against_anthropic_api() {
        let server = MockServer::start(vec![anthropic_tool_call("toolu_1", "echo hi"), anthropic_text("done")]).await;
        let dir = tempfile::tempdir().unwrap();
        let (agent, mut events) = agent(model(&server.url, Api::AnthropicMessages), dir.path());
        agent.prompt(vec![ContentBlock::text("say hi")], None).unwrap();
        agent.wait_idle().await;

        let messages = agent.messages();
        assert_eq!(roles(&messages), ["user", "assistant", "toolResult", "assistant"]);
        let Message::ToolResult(result) = &messages[2] else { unreachable!() };
        assert_eq!(crate::message::content_text(&result.content), "hi");
        assert_eq!(agent.last_assistant_text().as_deref(), Some("done"));

        let requests = server.requests.lock().unwrap();
        assert_eq!(requests[0]["system"][0]["text"], json!("test system prompt"));
        assert_eq!(requests[0]["tools"][1]["name"], json!("bash"));
        assert_eq!(requests[1]["messages"][2]["content"][0]["tool_use_id"], json!("toolu_1"));
        assert_eq!(requests[1]["messages"][2]["content"][0]["content"][0]["text"], json!("hi"));

        let mut kinds = Vec::new();
        while let Ok(event) = events.try_recv() {
            kinds.push(serde_json::to_value(&event).unwrap()["type"].as_str().unwrap().to_string());
        }
        assert_eq!(kinds.first().map(String::as_str), Some("agent_start"));
        assert_eq!(kinds.last().map(String::as_str), Some("agent_end"));
        assert!(kinds.iter().any(|k| k == "tool_execution_end"));
    }

    #[tokio::test]
    async fn runs_tool_loop_against_openai_completions() {
        let chunk =
            |delta: Value, finish: Option<&str>| json!({"choices": [{"delta": delta, "finish_reason": finish}]});
        let server = MockServer::start(vec![
            MockResponse::sse(&[
                chunk(json!({"tool_calls": [{"index": 0, "id": "call_1", "function": {"name": "bash", "arguments": "{\"command\":"}}]}), None),
                chunk(json!({"tool_calls": [{"index": 0, "function": {"arguments": "\"echo hi\"}"}}]}), Some("tool_calls")),
                json!({"choices": [], "usage": {"prompt_tokens": 50, "completion_tokens": 10}}),
                json!("[DONE]"),
            ]),
            MockResponse::sse(&[chunk(json!({"content": "all good"}), Some("stop"))]),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let (agent, _events) = agent(model(&server.url, Api::OpenAiCompletions), dir.path());
        agent.prompt(vec![ContentBlock::text("go")], None).unwrap();
        agent.wait_idle().await;

        assert_eq!(roles(&agent.messages()), ["user", "assistant", "toolResult", "assistant"]);
        assert_eq!(agent.last_assistant_text().as_deref(), Some("all good"));
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests[0]["reasoning_effort"], json!("high"));
        assert_eq!(requests[1]["messages"][3]["role"], json!("tool"));
        assert_eq!(requests[1]["messages"][3]["content"], json!("hi"));
    }

    #[tokio::test]
    async fn mutating_tool_calls_run_in_order() {
        // The read must observe the file the slower bash call writes before it.
        let server = MockServer::start(vec![
            anthropic_tool_calls(&[
                ("t1", "bash", json!({"command": "sleep 0.3 && printf written > out.txt"})),
                ("t2", "read", json!({"path": "out.txt"})),
                ("t3", "ls", json!({})),
            ]),
            anthropic_text("done"),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let (agent, mut events) = agent(model(&server.url, Api::AnthropicMessages), dir.path());
        agent.prompt(vec![ContentBlock::text("go")], None).unwrap();
        agent.wait_idle().await;

        let messages = agent.messages();
        let Message::ToolResult(read) = &messages[3] else { panic!("expected the read result") };
        assert_eq!(crate::message::content_text(&read.content), "written");

        let mut order = Vec::new();
        while let Ok(event) = events.try_recv() {
            match event {
                AgentEvent::ToolExecutionStart { tool_call_id, .. } => order.push(format!("start {tool_call_id}")),
                AgentEvent::ToolExecutionEnd { tool_call_id, .. } => order.push(format!("end {tool_call_id}")),
                _ => {}
            }
        }
        assert_eq!(&order[..3], ["start t1", "end t1", "start t2"]);
        assert_eq!(order[3], "start t3", "read-only calls start together");
    }

    #[test]
    fn auto_compact_window_is_per_model_and_capped() {
        let dir = tempfile::tempdir().unwrap();
        let mut small = model("http://unused", Api::AnthropicMessages);
        small.context_window = 200_000;
        let (agent, _events) = agent(small.clone(), dir.path());
        let reserve = agent.setup().settings.compaction.reserve_tokens;
        assert_eq!(agent.snapshot().compaction_threshold, 200_000 - reserve);

        agent.set_auto_compact_window(Some(150_000));
        assert_eq!(agent.snapshot().compaction_threshold, 150_000);
        agent.set_auto_compact_window(Some(500_000));
        assert_eq!(agent.snapshot().auto_compact_window, Some(500_000));
        assert_eq!(agent.snapshot().compaction_threshold, 200_000 - reserve);

        // The window belongs to the model it was set for.
        let other = Model { id: "other".into(), ..small };
        agent.set_model(other).unwrap();
        assert_eq!(agent.snapshot().auto_compact_window, None);
        agent.set_auto_compact_window(None);
        assert_eq!(agent.snapshot().compaction_threshold, 200_000 - reserve);
    }

    #[tokio::test]
    async fn retries_transient_errors() {
        let overloaded = r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
        let server = MockServer::start(vec![MockResponse::error(529, overloaded), anthropic_text("recovered")]).await;
        let dir = tempfile::tempdir().unwrap();
        let (agent, mut events) = agent(model(&server.url, Api::AnthropicMessages), dir.path());
        agent.prompt(vec![ContentBlock::text("hello")], None).unwrap();
        agent.wait_idle().await;
        assert_eq!(agent.last_assistant_text().as_deref(), Some("recovered"));
        let mut retried = false;
        while let Ok(event) = events.try_recv() {
            retried |= matches!(event, AgentEvent::AutoRetryStart { attempt: 1, .. });
        }
        assert!(retried);
    }

    #[tokio::test]
    async fn fatal_errors_end_the_run() {
        let server = MockServer::start(vec![MockResponse::error(
            401,
            r#"{"error":{"type":"authentication_error","message":"invalid x-api-key"}}"#,
        )])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let (agent, _events) = agent(model(&server.url, Api::AnthropicMessages), dir.path());
        agent.prompt(vec![ContentBlock::text("hello")], None).unwrap();
        agent.wait_idle().await;
        let Some(Message::Assistant(last)) = agent.messages().pop() else { panic!("expected assistant message") };
        assert_eq!(last.stop_reason, StopReason::Error);
        assert!(last.error_message.unwrap().contains("authentication_error"));
    }

    #[tokio::test]
    async fn follow_ups_run_after_the_current_task() {
        let server = MockServer::start(vec![
            anthropic_tool_call("t1", "sleep 0.3"),
            anthropic_text("first"),
            anthropic_text("second"),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let (agent, _events) = agent(model(&server.url, Api::AnthropicMessages), dir.path());
        agent.prompt(vec![ContentBlock::text("one")], None).unwrap();
        let disposition = agent.prompt(vec![ContentBlock::text("two")], Some(QueueMode::FollowUp)).unwrap();
        assert_eq!(disposition, PromptDisposition::Queued(QueueMode::FollowUp));
        agent.wait_idle().await;
        assert_eq!(roles(&agent.messages()), ["user", "assistant", "toolResult", "assistant", "user", "assistant"]);
        assert_eq!(agent.last_assistant_text().as_deref(), Some("second"));
    }

    #[tokio::test]
    async fn compaction_inside_a_turn_summarizes_its_request_separately() {
        // Each command prints ~11k tokens, so the 20k tokens kept start inside the second turn.
        let big = "head -c 46000 /dev/zero | tr '\\0' x";
        let server = MockServer::start(vec![
            anthropic_text("earlier answer"),
            anthropic_tool_call("t1", big),
            anthropic_tool_call("t2", big),
            anthropic_text("built"),
            anthropic_text("summary one"),
            anthropic_text("summary two"),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let (agent, _events) = agent(model(&server.url, Api::AnthropicMessages), dir.path());
        for text in ["earlier question", "build it"] {
            agent.prompt(vec![ContentBlock::text(text)], None).unwrap();
            agent.wait_idle().await;
        }
        let result = agent.compact(None).unwrap().await.unwrap().unwrap();
        assert!(result.summary.contains("\n\n---\n\n**Turn Context (split turn):**\n\n"));
        // The kept region starts at the second tool call, inside the "build it" turn.
        assert_eq!(roles(&agent.messages()), ["summary", "assistant", "toolResult", "assistant"]);

        // The history and the turn's beginning were summarized by separate requests.
        let requests = server.requests.lock().unwrap();
        let prompts: Vec<&str> =
            requests[4..].iter().map(|r| r["messages"][0]["content"][0]["text"].as_str().unwrap()).collect();
        let turn = prompts.iter().find(|p| p.contains("## Original Request")).unwrap();
        let history = prompts.iter().find(|p| !p.contains("## Original Request")).unwrap();
        assert!(turn.contains("build it") && !turn.contains("earlier answer"));
        assert!(history.contains("earlier answer") && !history.contains("build it"));
    }

    #[tokio::test]
    async fn prompts_sent_during_a_command_run_after_it() {
        let server = MockServer::start(vec![anthropic_text("done")]).await;
        let dir = tempfile::tempdir().unwrap();
        let (agent, _events) = agent(model(&server.url, Api::AnthropicMessages), dir.path());
        let command = {
            let agent = agent.clone();
            tokio::spawn(async move { agent.run_bash("sleep 0.3", false, |_| {}).await })
        };
        while agent.busy() != Some(Busy::Bash) {
            tokio::task::yield_now().await;
        }
        // Compaction is refused at once rather than reported later.
        assert!(agent.compact(None).is_err());
        let disposition = agent.prompt(vec![ContentBlock::text("next")], None).unwrap();
        assert_eq!(disposition, PromptDisposition::Queued(QueueMode::FollowUp));
        command.await.unwrap().unwrap();
        agent.wait_idle().await;
        assert_eq!(roles(&agent.messages()), ["bash", "user", "assistant"]);
    }

    #[tokio::test]
    async fn new_sessions_stay_in_memory_when_the_current_one_does() {
        let dir = tempfile::tempdir().unwrap();
        let (agent, _events) = agent(model("http://127.0.0.1:9", Api::AnthropicMessages), dir.path());
        assert_eq!(agent.session_path(), None);
        agent.new_session().unwrap();
        assert_eq!(agent.session_path(), None);
    }

    #[tokio::test]
    async fn manual_compaction_summarizes_history() {
        let summary = "## Goal\nTest compaction";
        let server = MockServer::start(vec![
            anthropic_text("first answer"),
            anthropic_text("second answer"),
            anthropic_text(summary),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let (agent, _events) = agent(model(&server.url, Api::AnthropicMessages), dir.path());
        for text in ["a", "b"] {
            agent.prompt(vec![ContentBlock::text(text.repeat(100_000))], None).unwrap();
            agent.wait_idle().await;
        }
        let result = agent.compact(Some("focus on tests".into())).unwrap().await.unwrap().unwrap();
        assert!(result.summary.starts_with(summary));
        // The summary replaces the first exchange; the second is kept verbatim.
        assert_eq!(roles(&agent.messages()), ["summary", "user", "assistant"]);
        let requests = server.requests.lock().unwrap();
        let prompt = requests[2]["messages"][0]["content"][0]["text"].as_str().unwrap();
        assert!(prompt.contains("<conversation>"));
        assert!(prompt.contains("first answer"));
        assert!(!prompt.contains("second answer"));
        assert!(prompt.contains("focus on tests"));
    }
}
