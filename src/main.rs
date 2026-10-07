mod agent;
mod auth;
mod cli;
mod compaction;
mod config;
mod context;
mod images;
mod message;
mod model_sync;
mod modes;
mod provider;
mod session;
#[cfg(test)]
mod testing;
mod time;
mod tools;
mod tui;
mod update;
mod util;

use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::Parser;

use crate::agent::{Agent, AgentSetup};
use crate::cli::{Cli, Mode};
use crate::config::{ConfigValue, ModelRegistry, Settings, ThinkingLevel, has_credentials};
use crate::message::ContentBlock;
use crate::session::SessionStore;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("viper: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn list_models(registry: &ModelRegistry, filter: &str) {
    let filter = filter.to_lowercase();
    let models: Vec<_> = registry
        .all()
        .iter()
        .filter(|m| {
            filter.is_empty() || m.key().to_lowercase().contains(&filter) || m.name.to_lowercase().contains(&filter)
        })
        .collect();
    let width = models.iter().map(|m| m.key().len()).max().unwrap_or(0);
    for model in models {
        let thinking = if model.thinking_levels.is_empty() {
            "-".to_string()
        } else {
            model.thinking_levels.iter().map(|l| l.as_str()).collect::<Vec<_>>().join(",")
        };
        let auth = if has_credentials(model) { "" } else { "  (no credentials)" };
        println!(
            "{:<width$}  {}  ctx {:>4}k  out {:>3}k  thinking {}{auth}",
            model.key(),
            model.api,
            model.context_window / 1000,
            model.max_tokens / 1000,
            thinking,
        );
    }
}

/// Turn positional arguments into initial messages. `@path` arguments attach files to the
/// first message; other arguments are sent as separate messages in order.
fn initial_messages(args: &[String], cwd: &Path, stdin: Option<String>) -> Result<Vec<Vec<ContentBlock>>> {
    let mut attachments = Vec::new();
    let mut texts = Vec::new();
    for arg in args {
        let Some(file) = arg.strip_prefix('@') else {
            texts.push(arg.clone());
            continue;
        };
        let path = tools::resolve_path(file, cwd);
        if images::is_image_file(&path) {
            let prepared = images::load_file(&path)?;
            attachments.push(prepared.block);
        } else {
            let content =
                std::fs::read_to_string(&path).with_context(|| format!("could not read {}", path.display()))?;
            attachments.push(ContentBlock::text(format!(
                "<file name=\"{}\">\n{}\n</file>",
                path.display(),
                content.trim_end()
            )));
        }
    }
    if let Some(stdin) = stdin.filter(|s| !s.trim().is_empty()) {
        attachments.insert(0, ContentBlock::text(stdin));
    }
    let mut messages: Vec<Vec<ContentBlock>> = texts.into_iter().map(|t| vec![ContentBlock::text(t)]).collect();
    if !attachments.is_empty() {
        match messages.first_mut() {
            Some(first) => {
                attachments.append(first);
                *first = attachments;
            }
            None => messages.push(attachments),
        }
    }
    Ok(messages)
}

async fn run(cli: Cli) -> Result<ExitCode> {
    let cwd = match &cli.cwd {
        Some(dir) => dir.clone(),
        None => std::env::current_dir().context("could not determine the working directory")?,
    };
    let cwd = std::fs::canonicalize(&cwd).with_context(|| format!("invalid working directory {}", cwd.display()))?;
    let settings = Settings::load(&cwd)?;
    let mut registry = ModelRegistry::load()?;

    if let Some(filter) = &cli.list_models {
        list_models(&registry, filter);
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(provider) = &cli.sync_models {
        let report = model_sync::sync(&provider::http_client()?, &registry, provider).await?;
        println!("{}", report.describe(provider));
        return Ok(ExitCode::SUCCESS);
    }

    let interactive = cli.mode == Mode::Text && !cli.print;
    if interactive && !std::io::stdout().is_terminal() {
        bail!("interactive mode needs a terminal; use --print, --mode json, or --mode rpc");
    }

    // Session selection.
    let persist = !cli.no_session;
    let session_path: Option<PathBuf> = if let Some(path) = &cli.session {
        Some(path.clone())
    } else if cli.continue_session {
        session::most_recent_session(&cwd)
    } else if cli.resume {
        if !interactive {
            bail!("--resume needs interactive mode; use --session <path> instead");
        }
        match tui::pick_session(&cwd).await? {
            Some(path) => Some(path),
            None => return Ok(ExitCode::SUCCESS),
        }
    } else {
        None
    };
    let (session, mut session_warnings) = match &session_path {
        Some(path) if persist => SessionStore::open(path)?,
        Some(_) => bail!("--no-session cannot be combined with resuming a session"),
        None => (SessionStore::create(&cwd, persist), Vec::new()),
    };

    // Model and thinking level: CLI, then the session's last choice, then settings.
    let mut model = match &cli.model {
        Some(query) => registry.find(query)?,
        // A resumed session's model is skipped when its credentials are gone.
        None => match session
            .last_model()
            .and_then(|(provider, id)| registry.find(&format!("{provider}/{id}")).ok())
            .filter(has_credentials)
        {
            Some(model) => model,
            None => match registry.default_model(&settings) {
                Ok(model) => model,
                // An explicit key makes the first built-in model usable; interactive mode starts
                // without credentials so `/login` can add them.
                Err(_) if cli.api_key.is_some() || interactive => registry.all()[0].clone(),
                Err(err) => return Err(err),
            },
        },
    };
    if let Some(key) = &cli.api_key {
        registry.set_api_key(&model.provider, ConfigValue::Literal(key.clone()));
        model.api_key = Some(ConfigValue::Literal(key.clone()));
    }
    if let Err(err) = crate::config::ensure_credentials(&model) {
        if !interactive || cli.model.is_some() {
            return Err(err);
        }
        session_warnings.push(format!(
            "No API key is configured for {}. Use /login <provider> to add one, or /model to choose another model.",
            model.provider
        ));
    }
    let thinking = cli
        .thinking
        .or_else(|| session.last_thinking_level())
        .or(settings.default_thinking_level)
        .unwrap_or(ThinkingLevel::High);

    // Tools, context, skills, system prompt.
    let tool_names: Vec<String> =
        if cli.no_tools { Vec::new() } else { cli.tools.clone().unwrap_or_else(|| settings.tools.clone()) };
    let tools = tools::select_tools(&tool_names)?;
    let context_files = context::load_context_files(&cwd);
    let (skills, skill_warnings) =
        if cli.no_skills { (Vec::new(), Vec::new()) } else { context::load_skills(&cwd, &settings) };
    let custom_prompt = cli.system_prompt.as_deref().map(|p| context::resolve_prompt_text(p, &cwd)).transpose()?;
    let append = cli
        .append_system_prompt
        .as_deref()
        .or(settings.append_system_prompt.as_deref())
        .map(|p| context::resolve_prompt_text(p, &cwd))
        .transpose()?;
    let system_prompt = context::build_system_prompt(&context::PromptInputs {
        cwd: &cwd,
        tools: &tools,
        custom_prompt: custom_prompt.as_deref(),
        append: append.as_deref(),
        context_files: &context_files,
        skills: &skills,
    });

    let stdin = if !interactive && cli.mode != Mode::Rpc && !std::io::stdin().is_terminal() {
        let mut buffer = String::new();
        std::io::stdin().read_to_string(&mut buffer).context("could not read stdin")?;
        Some(buffer)
    } else {
        None
    };
    let initial = initial_messages(&cli.messages, &cwd, stdin)?;

    let setup = AgentSetup { cwd, settings, tools, system_prompt, context_files, skills, skill_warnings };
    let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel();
    let agent = Agent::new(setup, registry, model, thinking, session, events_tx)?;

    match (cli.mode, cli.print) {
        (Mode::Rpc, _) => modes::rpc::run(agent, events_rx).await,
        (Mode::Json, _) => modes::print::run(agent, events_rx, initial, modes::print::Output::Json).await,
        (Mode::Text, true) => modes::print::run(agent, events_rx, initial, modes::print::Output::Text).await,
        (Mode::Text, false) => tui::run(agent, events_rx, initial, session_warnings).await,
    }
}
