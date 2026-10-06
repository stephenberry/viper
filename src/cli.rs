//! Command-line interface.

use std::path::PathBuf;

use clap::{Parser, ValueEnum};

use crate::config::ThinkingLevel;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Mode {
    /// Interactive terminal UI (default), or plain text output with --print.
    Text,
    /// Run the prompt and stream agent events as JSON lines.
    Json,
    /// JSON-lines command protocol on stdin/stdout.
    Rpc,
}

#[derive(Debug, Parser)]
#[command(name = "viper", version, about = "A minimal coding agent for the terminal")]
pub struct Cli {
    /// Initial messages. Arguments starting with @ attach files (text or images).
    pub messages: Vec<String>,

    /// Run the prompt, print the final response, and exit.
    #[arg(short, long)]
    pub print: bool,

    /// Output mode.
    #[arg(long, value_enum, default_value_t = Mode::Text)]
    pub mode: Mode,

    /// Model as provider/id, id, or a unique part of either.
    #[arg(short, long)]
    pub model: Option<String>,

    /// Thinking level: off, minimal, low, medium, high, xhigh, max.
    #[arg(long, value_parser = parse_thinking)]
    pub thinking: Option<ThinkingLevel>,

    /// Continue the most recent session in this directory.
    #[arg(short = 'c', long = "continue", conflicts_with_all = ["resume", "session"])]
    pub continue_session: bool,

    /// Pick a session to resume.
    #[arg(short, long, conflicts_with = "session")]
    pub resume: bool,

    /// Open a specific session file.
    #[arg(long)]
    pub session: Option<PathBuf>,

    /// Do not save the session.
    #[arg(long)]
    pub no_session: bool,

    /// API key for the selected model's provider (overrides configuration).
    #[arg(long)]
    pub api_key: Option<String>,

    /// Replace the default system prompt (text or a file path).
    #[arg(long)]
    pub system_prompt: Option<String>,

    /// Append to the system prompt (text or a file path).
    #[arg(long)]
    pub append_system_prompt: Option<String>,

    /// Comma-separated tools to enable (read,bash,edit,write,grep,find,ls).
    #[arg(long, value_delimiter = ',')]
    pub tools: Option<Vec<String>>,

    /// Disable all tools.
    #[arg(long, conflicts_with = "tools")]
    pub no_tools: bool,

    /// Do not load skills.
    #[arg(long)]
    pub no_skills: bool,

    /// List available models (optionally filtered) and exit.
    #[arg(long, num_args = 0..=1, default_missing_value = "")]
    pub list_models: Option<String>,

    /// Working directory.
    #[arg(long)]
    pub cwd: Option<PathBuf>,
}

fn parse_thinking(value: &str) -> Result<ThinkingLevel, String> {
    value.parse().map_err(|err: anyhow::Error| err.to_string())
}
