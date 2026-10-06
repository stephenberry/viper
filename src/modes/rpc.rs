//! RPC mode: JSON-lines commands on stdin; responses and agent events on stdout.
//!
//! Every command may carry an `id`, echoed in its response:
//! `{"id": "1", "type": "response", "command": "prompt", "success": true, "data": ...}`.
//! Events are written as they happen, interleaved with responses.

use std::process::ExitCode;

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::agent::{Agent, AgentEvent, QueueMode};
use crate::config::ThinkingLevel;
use crate::message::ContentBlock;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImageInput {
    data: String,
    mime_type: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
enum Command {
    Prompt {
        message: String,
        #[serde(default)]
        images: Vec<ImageInput>,
        streaming_behavior: Option<StreamingBehavior>,
    },
    Steer {
        message: String,
        #[serde(default)]
        images: Vec<ImageInput>,
    },
    FollowUp {
        message: String,
        #[serde(default)]
        images: Vec<ImageInput>,
    },
    Abort,
    ClearQueue,
    NewSession,
    GetState,
    SetModel {
        provider: String,
        model_id: String,
    },
    GetAvailableModels,
    SetThinkingLevel {
        level: ThinkingLevel,
    },
    CycleThinkingLevel,
    GetAvailableThinkingLevels,
    Compact {
        custom_instructions: Option<String>,
    },
    SetAutoCompaction {
        enabled: bool,
    },
    /// Set the current model's auto-compact window: an exact token count, a size such as
    /// `"300k"`, or `null`/`"auto"` for the default. Applies to this process only.
    SetAutoCompactWindow {
        window: Value,
    },
    SetAutoRetry {
        enabled: bool,
    },
    Bash {
        command: String,
        #[serde(default)]
        exclude_from_context: bool,
    },
    AbortBash,
    GetSessionStats,
    ListSessions,
    SwitchSession {
        session_path: std::path::PathBuf,
    },
    GetLastAssistantText,
    SetSessionName {
        name: String,
    },
    GetMessages,
    GetCommands,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
enum StreamingBehavior {
    Steer,
    FollowUp,
}

fn content(message: String, images: Vec<ImageInput>) -> Vec<ContentBlock> {
    let images = images.into_iter().map(|i| ContentBlock::Image { data: i.data, mime_type: i.mime_type }).collect();
    crate::agent::user_content(&message, images)
}

struct Responder {
    out: mpsc::UnboundedSender<String>,
}

impl Responder {
    fn send(&self, value: Value) {
        let _ = self.out.send(value.to_string());
    }

    fn reply(&self, id: &Option<Value>, command: &str, result: Result<Option<Value>>) {
        let mut response = match result {
            Ok(data) => {
                let mut r = json!({"type": "response", "command": command, "success": true});
                if let Some(data) = data {
                    r["data"] = data;
                }
                r
            }
            Err(err) => json!({"type": "response", "command": command, "success": false, "error": format!("{err:#}")}),
        };
        if let Some(id) = id {
            response["id"] = id.clone();
        }
        self.send(response);
    }
}

pub async fn run(agent: Agent, mut events: mpsc::UnboundedReceiver<AgentEvent>) -> Result<ExitCode> {
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(line) = out_rx.recv().await {
            if stdout.write_all(line.as_bytes()).await.is_err() || stdout.write_all(b"\n").await.is_err() {
                break;
            }
            let _ = stdout.flush().await;
        }
    });
    let event_out = out_tx.clone();
    let pump = tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            match serde_json::to_string(&event) {
                Ok(line) => {
                    let _ = event_out.send(line);
                }
                Err(err) => eprintln!("viper: could not serialize event: {err}"),
            }
        }
    });

    let responder = std::sync::Arc::new(Responder { out: out_tx });
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await.context("could not read stdin")? {
        if line.trim().is_empty() {
            continue;
        }
        let raw: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(err) => {
                responder.reply(&None, "parse", Err(anyhow!("invalid JSON: {err}")));
                continue;
            }
        };
        let id = raw.get("id").cloned();
        let name = raw.get("type").and_then(Value::as_str).unwrap_or("unknown").to_string();
        let command: Command = match serde_json::from_value(raw) {
            Ok(command) => command,
            Err(err) => {
                responder.reply(&id, &name, Err(anyhow!("invalid command: {err}")));
                continue;
            }
        };
        handle(&agent, &responder, id, &name, command);
    }

    // stdin closed: stop work in progress and flush output.
    agent.abort();
    agent.wait_idle().await;
    drop(agent);
    let _ = pump.await;
    drop(responder);
    let _ = writer.await;
    Ok(ExitCode::SUCCESS)
}

fn handle(agent: &Agent, responder: &std::sync::Arc<Responder>, id: Option<Value>, name: &str, command: Command) {
    let reply = |result: Result<Option<Value>>| responder.reply(&id, name, result);
    match command {
        Command::Prompt { message, images, streaming_behavior } => {
            let mode = streaming_behavior.map(|b| match b {
                StreamingBehavior::Steer => QueueMode::Steer,
                StreamingBehavior::FollowUp => QueueMode::FollowUp,
            });
            reply(
                super::expand_skill(agent, content(message, images))
                    .and_then(|c| agent.prompt(c, mode))
                    .map(|d| Some(json!({"disposition": d}))),
            );
        }
        Command::Steer { message, images } => reply(
            agent.prompt(content(message, images), Some(QueueMode::Steer)).map(|d| Some(json!({"disposition": d}))),
        ),
        Command::FollowUp { message, images } => reply(
            agent.prompt(content(message, images), Some(QueueMode::FollowUp)).map(|d| Some(json!({"disposition": d}))),
        ),
        Command::Abort => {
            let returned = agent.abort();
            reply(Ok(Some(json!({"returnedMessages": returned}))));
        }
        Command::ClearQueue => {
            let (steering, follow_up) = agent.clear_queue();
            reply(Ok(Some(json!({"steering": steering, "followUp": follow_up}))));
        }
        Command::NewSession => reply(agent.new_session(true).map(|_| Some(json!(agent.snapshot())))),
        Command::GetState => reply(Ok(Some(json!(agent.snapshot())))),
        Command::SetModel { provider, model_id } => reply(
            agent
                .setup()
                .registry
                .find(&format!("{provider}/{model_id}"))
                .and_then(|model| agent.set_model(model.clone()).map(|_| Some(json!(model)))),
        ),
        Command::GetAvailableModels => {
            let models: Vec<_> = agent.setup().registry.available().into_iter().cloned().collect();
            reply(Ok(Some(json!({"models": models}))));
        }
        Command::SetThinkingLevel { level } => reply(agent.set_thinking(level).map(|l| Some(json!({"level": l})))),
        Command::CycleThinkingLevel => {
            reply(agent.cycle_thinking().map(|l| Some(json!(l.map(|level| json!({"level": level}))))))
        }
        Command::GetAvailableThinkingLevels => reply(Ok(Some(json!({"levels": agent.model().thinking_levels})))),
        Command::Compact { custom_instructions } => {
            let (agent, responder, name) = (agent.clone(), responder.clone(), name.to_string());
            tokio::spawn(async move {
                let result = agent.compact(custom_instructions.as_deref()).await.map(|r| Some(json!(r)));
                responder.reply(&id, &name, result);
            });
        }
        Command::SetAutoCompaction { enabled } => {
            agent.set_auto_compaction(enabled);
            reply(Ok(None));
        }
        Command::SetAutoCompactWindow { window } => {
            let window = match &window {
                Value::Null => Ok(None),
                Value::String(text) if text.trim().eq_ignore_ascii_case("auto") => Ok(None),
                Value::String(text) => crate::config::parse_auto_compact_window(text).map(Some),
                Value::Number(number) => match number.as_u64() {
                    Some(tokens) => crate::config::check_auto_compact_window(tokens).map(Some),
                    None => Err(anyhow::anyhow!("window must be a whole number of tokens")),
                },
                _ => Err(anyhow::anyhow!("window must be a token count, a size such as \"300k\", or null")),
            };
            reply(window.map(|window| {
                agent.set_auto_compact_window(window);
                let snapshot = agent.snapshot();
                Some(json!({
                    "autoCompactWindow": snapshot.auto_compact_window,
                    "compactionThreshold": snapshot.compaction_threshold,
                }))
            }));
        }
        Command::SetAutoRetry { enabled } => {
            agent.set_auto_retry(enabled);
            reply(Ok(None));
        }
        Command::Bash { command, exclude_from_context } => {
            let (agent, responder, name) = (agent.clone(), responder.clone(), name.to_string());
            tokio::spawn(async move {
                let result = agent.run_bash(&command, exclude_from_context, |_| {}).await.map(|m| Some(json!(m)));
                responder.reply(&id, &name, result);
            });
        }
        Command::AbortBash => {
            if agent.busy() == Some(crate::agent::Busy::Bash) {
                agent.abort();
            }
            reply(Ok(None));
        }
        Command::GetSessionStats => reply(Ok(Some(json!(agent.stats())))),
        Command::ListSessions => reply(Ok(Some(json!({"sessions": crate::session::list_sessions(agent.cwd())})))),
        Command::SwitchSession { session_path } => reply(
            agent
                .switch_session(&session_path)
                .map(|warnings| Some(json!({"warnings": warnings, "state": agent.snapshot()}))),
        ),
        Command::GetLastAssistantText => reply(Ok(Some(json!({"text": agent.last_assistant_text()})))),
        Command::SetSessionName { name } => reply(agent.set_session_name(&name).map(|_| None)),
        Command::GetMessages => reply(Ok(Some(json!({"messages": agent.messages()})))),
        Command::GetCommands => {
            let commands: Vec<Value> = agent
                .setup()
                .skills
                .iter()
                .map(|s| json!({"name": format!("skill:{}", s.name), "description": s.description, "source": "skill", "path": s.file_path}))
                .collect();
            reply(Ok(Some(json!({"commands": commands}))));
        }
    }
}
