//! Print mode (`-p`): run the prompt and print the final response. JSON mode (`--mode json`)
//! runs the same way but writes every agent event as a JSON line.

use std::io::Write;
use std::process::ExitCode;

use anyhow::{Result, bail};
use serde_json::json;
use tokio::sync::mpsc;

use crate::agent::{Agent, AgentEvent};
use crate::message::{ContentBlock, Message, StopReason};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    Text,
    Json,
}

fn write_line(line: &str) {
    let mut stdout = std::io::stdout().lock();
    // A closed stdout (e.g. piped into `head`) is not worth crashing over.
    let _ = writeln!(stdout, "{line}");
    let _ = stdout.flush();
}

pub async fn run(
    agent: Agent,
    mut events: mpsc::UnboundedReceiver<AgentEvent>,
    initial: Vec<Vec<ContentBlock>>,
    output: Output,
) -> Result<ExitCode> {
    if initial.is_empty() {
        bail!("no prompt given (pass a message argument or pipe text on stdin)");
    }
    if output == Output::Json {
        let snapshot = agent.snapshot();
        write_line(
            &json!({
                "type": "session",
                "id": snapshot.session_id,
                "cwd": agent.cwd(),
                "sessionFile": snapshot.session_file,
                "model": snapshot.model.key(),
                "thinkingLevel": snapshot.thinking_level,
            })
            .to_string(),
        );
    }

    let pump = tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            match output {
                Output::Json => match serde_json::to_string(&event) {
                    Ok(line) => write_line(&line),
                    Err(err) => eprintln!("viper: could not serialize event: {err}"),
                },
                Output::Text => {
                    if let AgentEvent::AutoRetryStart { attempt, max_attempts, delay_ms, error_message } = &event {
                        eprintln!(
                            "viper: {error_message} (retry {attempt}/{max_attempts} in {:.1}s)",
                            *delay_ms as f64 / 1000.0
                        );
                    }
                }
            }
        }
    });

    let mut failed = false;
    for content in initial {
        let content = super::expand_skill(&agent, content)?;
        agent.prompt(content, None)?;
        tokio::select! {
            _ = agent.wait_idle() => {}
            _ = tokio::signal::ctrl_c() => {
                agent.abort();
                agent.wait_idle().await;
                failed = true;
                break;
            }
        }
        let last = agent.messages().into_iter().rev().find_map(|m| match m {
            Message::Assistant(a) => Some(a),
            _ => None,
        });
        if let Some(last) = last
            && matches!(last.stop_reason, StopReason::Error | StopReason::Aborted)
        {
            eprintln!("viper: {}", last.error_message.as_deref().unwrap_or("request failed"));
            failed = true;
            break;
        }
    }

    let final_text = agent.last_assistant_text();
    drop(agent);
    // The event channel closes once the agent is dropped; flush remaining events.
    let _ = pump.await;
    if output == Output::Text
        && !failed
        && let Some(text) = final_text
    {
        write_line(&text);
    }
    Ok(if failed { ExitCode::FAILURE } else { ExitCode::SUCCESS })
}
