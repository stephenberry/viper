//! Interactive terminal UI.
//!
//! Output flows into the terminal's normal scrollback; only the bottom "live region" (streaming
//! text, running tools, status, editor, and footer) is redrawn. See [`terminal::Screen`].

mod editor;
mod markdown;
mod render;
mod select;
mod style;
mod terminal;
mod text;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::agent::{Agent, AgentEvent, Busy, CompactionReason, PromptDisposition, QueueMode};
use crate::config::{Settings, ThinkingLevel, has_credentials, parse_auto_compact_window};
use crate::message::{BashExecutionMessage, ContentBlock, Message, StopReason};
use crate::provider::StreamDelta;
use editor::Editor;
use markdown::MarkdownRenderer;
use render::{Line, ToolState};
use select::{Item, SelectAction, Selector};
use style::*;
use terminal::{Screen, TerminalGuard};
use text::{sanitize, truncate, visible_width, wrap_prefixed};

const COMMANDS: &[(&str, &str, bool)] = &[
    ("model", "Select a model (or /model <query>)", false),
    ("thinking", "Set the thinking level (or /thinking <level>)", false),
    ("new", "Start a new session", false),
    ("resume", "Resume a saved session", false),
    ("session", "Show session information", false),
    ("name", "Name the session: /name <name>", true),
    ("compact", "Compact the context: /compact [focus]", false),
    ("autocompact", "Set when auto-compaction runs: /autocompact <size>|auto|off|on", false),
    ("copy", "Copy the last response to the clipboard", false),
    ("hotkeys", "Show keyboard shortcuts", false),
    ("help", "Show commands", false),
    ("quit", "Exit viper", false),
];

const HOTKEYS: &[(&str, &str)] = &[
    ("Enter", "send (steers the agent while it works)"),
    ("Alt+Enter", "queue a follow-up while working, newline otherwise"),
    ("Shift+Enter, Ctrl+J, \\+Enter", "newline"),
    ("Esc", "interrupt the agent (queued messages return to the editor)"),
    ("Alt+Up", "move queued messages back into the editor"),
    ("Ctrl+C", "clear the editor; twice to exit"),
    ("Ctrl+D", "exit when the editor is empty"),
    ("Shift+Tab", "cycle thinking level"),
    ("Ctrl+L", "select model"),
    ("Ctrl+T", "show/hide thinking for new output"),
    ("Ctrl+O", "expand/collapse tool output for new output"),
    ("Ctrl+G", "edit the prompt in $VISUAL / $EDITOR"),
    ("Ctrl+V", "paste an image from the clipboard"),
    ("Tab", "complete commands and paths"),
    ("Up/Down", "move between lines; browse history at the edges"),
    ("!cmd / !!cmd", "run a shell command (!! keeps output out of context)"),
];

/// Messages from background tasks the app started.
enum AppMsg {
    BashUpdate(String),
    BashDone(Result<BashExecutionMessage>),
    CompactDone(Result<()>),
}

enum StreamKind {
    Text,
    Thinking,
}

struct StreamBlock {
    kind: StreamKind,
    partial: String,
    markdown: MarkdownRenderer,
    started: bool,
    had_content: bool,
}

struct RunningTool {
    id: String,
    name: String,
    args: Value,
    started: Instant,
    output: String,
}

/// An open selector and the values its items stand for, in display order.
enum Overlay {
    Model(Selector, Vec<crate::config::Model>),
    Thinking(Selector, Vec<ThinkingLevel>),
    Session(Selector, Vec<PathBuf>),
}

struct App {
    agent: Agent,
    guard: TerminalGuard,
    screen: Screen,
    editor: Editor,
    attachments: Vec<ContentBlock>,
    pending: Vec<String>,
    last_blank: bool,
    blocks: BTreeMap<usize, StreamBlock>,
    streaming_tool: Option<(String, usize)>,
    running_tools: Vec<RunningTool>,
    busy_since: Option<Instant>,
    status: Option<String>,
    bash_live: Option<(String, String)>,
    queued: (Vec<String>, Vec<String>),
    overlay: Option<Overlay>,
    suggestion: usize,
    notice: Option<String>,
    spinner: usize,
    hide_thinking: bool,
    expanded: bool,
    tool_lines: usize,
    footer: String,
    git_branch: Option<String>,
    last_ctrl_c: Option<Instant>,
    app_tx: mpsc::UnboundedSender<AppMsg>,
    quit: bool,
}

fn history_path() -> PathBuf {
    crate::config::agent_dir().join("history.jsonl")
}

fn load_history() -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(history_path()) else { return Vec::new() };
    let mut entries: Vec<String> = text.lines().filter_map(|l| serde_json::from_str::<String>(l).ok()).collect();
    let excess = entries.len().saturating_sub(1000);
    entries.drain(..excess);
    entries
}

fn append_history(entry: &str) {
    use std::io::Write;
    let path = history_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path)
        && let Ok(line) = serde_json::to_string(entry)
    {
        let _ = writeln!(file, "{line}");
    }
}

fn git_branch(cwd: &Path) -> Option<String> {
    let root = crate::context::find_git_root(cwd)?;
    let dot_git = root.join(".git");
    let git_dir = if dot_git.is_file() {
        let text = std::fs::read_to_string(&dot_git).ok()?;
        let dir = PathBuf::from(text.trim().strip_prefix("gitdir:")?.trim());
        if dir.is_absolute() { dir } else { root.join(dir) }
    } else {
        dot_git
    };
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    match head.strip_prefix("ref: refs/heads/") {
        Some(branch) => Some(branch.to_string()),
        None => Some(head.chars().take(7).collect()),
    }
}

fn format_tokens(n: u64) -> String {
    let scaled = |value: f64, unit: &str| {
        let text = if value < 10.0 { format!("{value:.1}") } else { format!("{value:.0}") };
        format!("{}{unit}", text.strip_suffix(".0").unwrap_or(&text))
    };
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => scaled(n as f64 / 1_000.0, "k"),
        _ => scaled(n as f64 / 1_000_000.0, "M"),
    }
}

fn relative_time(time: chrono::DateTime<chrono::Local>) -> String {
    let secs = (chrono::Local::now() - time).num_seconds().max(0);
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{}m ago", secs / 60),
        3600..86400 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

/// Interpret pasted text that is a path to an image (drag and drop).
fn pasted_image_path(text: &str, cwd: &Path) -> Option<PathBuf> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.contains('\n') {
        return None;
    }
    let unquoted = trimmed.trim_matches(|c| c == '\'' || c == '"');
    let unescaped = unquoted.replace("\\ ", " ");
    let unescaped = unescaped.strip_prefix("file://").unwrap_or(&unescaped);
    let path = crate::tools::resolve_path(unescaped, cwd);
    (path.is_file() && crate::images::is_image_file(&path)).then_some(path)
}

impl App {
    fn new(agent: Agent, guard: TerminalGuard, app_tx: mpsc::UnboundedSender<AppMsg>) -> App {
        let settings = &agent.setup().settings;
        let mut editor = Editor::default();
        editor.set_history(load_history());
        let mut app = App {
            hide_thinking: settings.hide_thinking,
            tool_lines: settings.tool_output_lines.max(1),
            git_branch: git_branch(agent.cwd()),
            agent,
            guard,
            screen: Screen::new(),
            editor,
            attachments: Vec::new(),
            pending: Vec::new(),
            last_blank: true,
            blocks: BTreeMap::new(),
            streaming_tool: None,
            running_tools: Vec::new(),
            busy_since: None,
            status: None,
            bash_live: None,
            queued: (Vec::new(), Vec::new()),
            overlay: None,
            suggestion: 0,
            notice: None,
            spinner: 0,
            expanded: false,
            footer: String::new(),
            last_ctrl_c: None,
            app_tx,
            quit: false,
        };
        app.refresh_footer();
        app
    }

    // --- Output -------------------------------------------------------------------------------

    fn width(&self) -> usize {
        terminal::size().0
    }

    fn commit(&mut self, lines: Vec<Line>) {
        let width = self.width();
        for line in lines {
            let is_blank = line.text.is_empty() && line.first.is_empty();
            if is_blank && self.last_blank {
                continue;
            }
            self.last_blank = is_blank;
            if line.text.is_empty() {
                self.pending.push(line.first.to_string());
                continue;
            }
            self.pending.extend(wrap_prefixed(&line.text, width, line.first, line.rest));
        }
    }

    fn gap(&mut self) {
        self.commit(vec![Line::plain("")]);
    }

    fn notice(&mut self, text: &str) {
        self.gap();
        self.commit(vec![render::notice_line(text)]);
    }

    fn error(&mut self, text: &str) {
        self.gap();
        self.commit(vec![render::error_line(text)]);
    }

    fn tool_max_lines(&self) -> usize {
        if self.expanded { self.tool_lines * 20 } else { self.tool_lines }
    }

    fn refresh_footer(&mut self) {
        let stats = self.agent.stats();
        let model = self.agent.model();
        let thinking = self.agent.thinking();
        let mut left = crate::util::tildify(self.agent.cwd());
        if let Some(branch) = &self.git_branch {
            left.push_str(&format!(" ({branch})"));
        }
        if let Some(name) = self.agent.session_name() {
            left.push_str(&format!(" · {name}"));
        }
        let percent = if stats.context_window > 0 {
            stats.context_tokens as f64 * 100.0 / stats.context_window as f64
        } else {
            0.0
        };
        let percent = if percent < 10.0 { format!("{percent:.1}") } else { format!("{percent:.0}") };
        let mut right = format!(
            "{percent}%/{} · ↑{} ↓{}",
            format_tokens(stats.context_window),
            format_tokens(stats.tokens.input + stats.tokens.cache_read + stats.tokens.cache_write),
            format_tokens(stats.tokens.output)
        );
        if stats.auto_compact_window.is_some() {
            right.push_str(&format!(" · compact at {}", format_tokens(stats.compaction_threshold)));
        }
        if stats.cost > 0.0 {
            right.push_str(&format!(" · ${:.2}", stats.cost));
        }
        right.push_str(&format!(" · {}", model.id));
        if !model.thinking_levels.is_empty() {
            right.push_str(&format!(" · {thinking}"));
        }
        self.footer = format!("{left}\u{0}{right}");
    }

    fn footer_line(&self, width: usize) -> String {
        let (left, right) = self.footer.split_once('\u{0}').unwrap_or((&self.footer, ""));
        let right_width = visible_width(right);
        if right_width + 4 >= width {
            return dim(&truncate(right, width));
        }
        let left = truncate(left, width - right_width - 2);
        let pad = width - visible_width(&left) - right_width;
        dim(&format!("{left}{}{right}", " ".repeat(pad)))
    }

    // --- Live region --------------------------------------------------------------------------

    fn spinner_frame(&self) -> &'static str {
        SPINNER[self.spinner % SPINNER.len()]
    }

    fn suggestions(&self) -> Vec<(String, String)> {
        let text = self.editor.text();
        if !text.starts_with('/') || text.contains(char::is_whitespace) {
            return Vec::new();
        }
        let typed = &text[1..];
        let mut out: Vec<(String, String)> = COMMANDS
            .iter()
            .filter(|(name, _, _)| name.starts_with(typed))
            .map(|(name, desc, _)| (format!("/{name}"), desc.to_string()))
            .collect();
        for skill in &self.agent.setup().skills {
            let name = format!("skill:{}", skill.name);
            if name.starts_with(typed) {
                out.push((format!("/{name}"), skill.description.clone()));
            }
        }
        out
    }

    fn live(&self) -> (Vec<String>, Option<(usize, usize)>) {
        let width = self.width();
        let mut lines: Vec<String> = Vec::new();
        let push_wrapped =
            |lines: &mut Vec<String>, line: Line| lines.extend(wrap_prefixed(&line.text, width, line.first, line.rest));

        for block in self.blocks.values() {
            match block.kind {
                StreamKind::Text if !block.partial.is_empty() => {
                    push_wrapped(&mut lines, Line::plain(block.markdown.preview_line(&sanitize(&block.partial))));
                }
                StreamKind::Thinking if self.hide_thinking && block.had_content => {
                    lines.push(format!("{GRAY}{ITALIC}∴ Thinking…{RESET}"));
                }
                StreamKind::Thinking if !block.partial.is_empty() => {
                    push_wrapped(
                        &mut lines,
                        Line::indented(format!("{GRAY}{ITALIC}{}{RESET}", sanitize(&block.partial)), "  ", "  "),
                    );
                }
                _ => {}
            }
        }
        if let Some((name, bytes)) = &self.streaming_tool {
            let size = if *bytes > 0 {
                format!(" {GRAY}({}){RESET}", crate::tools::truncate::format_size(*bytes))
            } else {
                String::new()
            };
            lines
                .push(truncate(&format!("{YELLOW}{}{RESET} {BOLD}{name}{RESET} …{size}", self.spinner_frame()), width));
        }
        for tool in &self.running_tools {
            let header = render::tool_header(&tool.name, &tool.args, ToolState::Running, self.spinner_frame());
            let elapsed = tool.started.elapsed().as_secs();
            let suffix = if elapsed >= 2 { format!(" {GRAY}{elapsed}s{RESET}") } else { String::new() };
            lines.push(truncate(&format!("{}{suffix}", header.text), width));
            if !tool.output.is_empty() {
                let tail: Vec<&str> = tool.output.lines().rev().take(5).collect();
                for (i, line) in tail.iter().rev().enumerate() {
                    let prefix = if i == 0 { render::BODY_FIRST } else { render::BODY_REST };
                    lines.push(truncate(&format!("{prefix}{}", dim(&sanitize(line))), width));
                }
            }
        }
        if let Some((command, output)) = &self.bash_live {
            lines.push(truncate(
                &format!("{YELLOW}{}{RESET} {BOLD}{}{RESET}", self.spinner_frame(), sanitize(command)),
                width,
            ));
            let tail: Vec<&str> = output.lines().rev().take(5).collect();
            for (i, line) in tail.iter().rev().enumerate() {
                let prefix = if i == 0 { render::BODY_FIRST } else { render::BODY_REST };
                lines.push(truncate(&format!("{prefix}{}", dim(&sanitize(line))), width));
            }
        }

        let busy = self.agent.busy();
        if busy.is_some() || self.status.is_some() {
            lines.push(String::new());
            let elapsed = self.busy_since.map(|t| format!(" {}s", t.elapsed().as_secs())).unwrap_or_default();
            let label = match (&self.status, busy) {
                (Some(status), _) => status.clone(),
                (None, Some(Busy::Compacting)) => "Compacting context…".to_string(),
                (None, Some(Busy::Bash)) => "Running command…".to_string(),
                _ => "Working…".to_string(),
            };
            lines.push(truncate(
                &format!("{CYAN}{}{RESET} {label}{GRAY}{elapsed} · esc to interrupt{RESET}", self.spinner_frame()),
                width,
            ));
        }
        for text in &self.queued.0 {
            lines.push(truncate(&dim(&format!("↳ steer: {}", sanitize(text).replace('\n', " "))), width));
        }
        for text in &self.queued.1 {
            lines.push(truncate(&dim(&format!("↳ follow-up: {}", sanitize(text).replace('\n', " "))), width));
        }
        if let Some(notice) = &self.notice {
            for line in notice.lines() {
                lines.push(truncate(&dim(line), width));
            }
        }

        let border_color = thinking_color(self.agent.thinking());
        let border = format!("{border_color}{}{RESET}", "─".repeat(width));

        if let Some(overlay) = &self.overlay {
            lines.push(border.clone());
            let selector = match overlay {
                Overlay::Model(s, _) | Overlay::Thinking(s, _) | Overlay::Session(s, _) => s,
            };
            lines.extend(selector.render(width));
            lines.push(border);
            lines.push(self.footer_line(width));
            return (lines, None);
        }

        if !self.attachments.is_empty() {
            let labels: Vec<String> = (1..=self.attachments.len()).map(|i| format!("[image #{i}]")).collect();
            lines
                .push(format!("{CYAN}{}{RESET} {GRAY}(backspace on an empty prompt removes){RESET}", labels.join(" ")));
        }
        lines.push(border.clone());
        let content_width = width.saturating_sub(2).max(4);
        let rows = self.editor.rows(content_width);
        let (cursor_row, cursor_col) = self.editor.cursor_position(content_width);
        let editor_top = lines.len();
        for (i, row) in rows.iter().enumerate() {
            let prefix = if i == 0 { format!("{CYAN}›{RESET} ") } else { "  ".to_string() };
            lines.push(format!("{prefix}{row}"));
        }
        if self.editor.is_empty() {
            let placeholder = if busy == Some(Busy::Running) {
                "steer the agent, or Alt+Enter to queue a follow-up"
            } else {
                "ask anything · / for commands · ! to run a shell command"
            };
            lines[editor_top] = format!("{CYAN}›{RESET} {GRAY}{}{RESET}", truncate(placeholder, content_width));
        }
        lines.push(border);
        let suggestions = self.suggestions();
        if suggestions.is_empty() {
            lines.push(self.footer_line(width));
        } else {
            let selected = self.suggestion.min(suggestions.len() - 1);
            for (i, (name, desc)) in suggestions.iter().enumerate().take(8) {
                let line = if i == selected {
                    format!("{CYAN}{BOLD}{name}{RESET}  {GRAY}{desc}{RESET}")
                } else {
                    format!("{name}  {GRAY}{desc}{RESET}")
                };
                lines.push(truncate(&line, width));
            }
        }
        (lines, Some((editor_top + cursor_row, cursor_col + 2)))
    }

    fn draw(&mut self) -> Result<()> {
        let (live, cursor) = self.live();
        let pending = std::mem::take(&mut self.pending);
        self.screen.draw(&pending, &live, cursor)?;
        Ok(())
    }

    fn animating(&self) -> bool {
        self.agent.busy().is_some()
            || !self.running_tools.is_empty()
            || self.streaming_tool.is_some()
            || self.bash_live.is_some()
    }

    // --- Agent events -------------------------------------------------------------------------

    fn flush_block(&mut self, index: usize) {
        let Some(mut block) = self.blocks.remove(&index) else { return };
        if !block.partial.is_empty() {
            let line = std::mem::take(&mut block.partial);
            self.emit_block_line(&mut block, &line);
        }
        if matches!(block.kind, StreamKind::Thinking) && self.hide_thinking && block.had_content {
            self.gap();
            self.commit(vec![Line::plain(format!("{GRAY}{ITALIC}∴ Thinking…{RESET}"))]);
        }
    }

    fn emit_block_line(&mut self, block: &mut StreamBlock, line: &str) {
        let line = sanitize(line);
        if !block.started && line.trim().is_empty() {
            return;
        }
        block.had_content = true;
        let rendered = match block.kind {
            StreamKind::Text => Line::plain(block.markdown.render_line(&line)),
            StreamKind::Thinking if self.hide_thinking => return,
            StreamKind::Thinking => Line::indented(format!("{GRAY}{ITALIC}{line}{RESET}"), "  ", "  "),
        };
        if !block.started {
            block.started = true;
            self.gap();
        }
        self.commit(vec![rendered]);
    }

    fn on_delta(&mut self, delta: StreamDelta) {
        match delta {
            StreamDelta::TextStart { content_index } | StreamDelta::ThinkingStart { content_index } => {
                let kind = if matches!(delta, StreamDelta::TextStart { .. }) {
                    StreamKind::Text
                } else {
                    StreamKind::Thinking
                };
                self.blocks.insert(
                    content_index,
                    StreamBlock {
                        kind,
                        partial: String::new(),
                        markdown: MarkdownRenderer::default(),
                        started: false,
                        had_content: false,
                    },
                );
            }
            StreamDelta::TextDelta { content_index, delta } | StreamDelta::ThinkingDelta { content_index, delta } => {
                let Some(mut block) = self.blocks.remove(&content_index) else { return };
                block.partial.push_str(&delta);
                if !delta.trim().is_empty() {
                    block.had_content = true;
                }
                while let Some(pos) = block.partial.find('\n') {
                    let line: String = block.partial.drain(..=pos).collect();
                    self.emit_block_line(&mut block, line.trim_end_matches('\n'));
                }
                self.blocks.insert(content_index, block);
            }
            StreamDelta::TextEnd { content_index } | StreamDelta::ThinkingEnd { content_index } => {
                self.flush_block(content_index)
            }
            StreamDelta::ToolcallStart { tool_name, .. } => self.streaming_tool = Some((tool_name, 0)),
            StreamDelta::ToolcallDelta { delta, .. } => {
                if let Some((_, bytes)) = &mut self.streaming_tool {
                    *bytes += delta.len();
                }
            }
            StreamDelta::ToolcallEnd { .. } => self.streaming_tool = None,
        }
    }

    fn on_agent(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::AgentStart => self.busy_since = Some(Instant::now()),
            AgentEvent::AgentEnd { .. } => {
                self.busy_since = None;
                self.status = None;
                self.streaming_tool = None;
                let indices: Vec<usize> = self.blocks.keys().copied().collect();
                for index in indices {
                    self.flush_block(index);
                }
                self.git_branch = git_branch(self.agent.cwd());
                self.refresh_footer();
            }
            AgentEvent::MessageStart { message } => match &message {
                Message::User(user) => {
                    self.gap();
                    self.commit(render::user_lines(&user.content));
                }
                Message::Assistant(_) => self.blocks.clear(),
                _ => {}
            },
            AgentEvent::MessageUpdate { assistant_message_event, .. } => {
                self.status = None;
                self.on_delta(assistant_message_event);
            }
            AgentEvent::MessageEnd { message: Message::Assistant(assistant) } => {
                let indices: Vec<usize> = self.blocks.keys().copied().collect();
                for index in indices {
                    self.flush_block(index);
                }
                self.streaming_tool = None;
                match assistant.stop_reason {
                    StopReason::Error => self.error(assistant.error_message.as_deref().unwrap_or("request failed")),
                    StopReason::Aborted => {
                        self.gap();
                        self.commit(vec![Line::plain(paint(YELLOW, "⏹ Interrupted"))]);
                    }
                    StopReason::Length => self.notice("Response hit the output token limit."),
                    _ => {}
                }
                self.refresh_footer();
            }
            AgentEvent::MessageEnd { .. } | AgentEvent::TurnStart => {}
            AgentEvent::TurnEnd { .. } => self.refresh_footer(),
            AgentEvent::ToolExecutionStart { tool_call_id, tool_name, args } => {
                self.running_tools.push(RunningTool {
                    id: tool_call_id,
                    name: tool_name,
                    args,
                    started: Instant::now(),
                    output: String::new(),
                });
            }
            AgentEvent::ToolExecutionUpdate { tool_call_id, partial_result, .. } => {
                if let Some(tool) = self.running_tools.iter_mut().find(|t| t.id == tool_call_id) {
                    tool.output = crate::message::content_text(&partial_result.content);
                }
            }
            AgentEvent::ToolExecutionEnd { tool_call_id, tool_name, result, is_error, .. } => {
                let args = match self.running_tools.iter().position(|t| t.id == tool_call_id) {
                    Some(index) => self.running_tools.remove(index).args,
                    None => Value::Null,
                };
                let state = if is_error { ToolState::Failed } else { ToolState::Done };
                self.gap();
                let mut lines = vec![render::tool_header(&tool_name, &args, state, "")];
                lines.extend(render::tool_body(&tool_name, &args, &result, is_error, self.tool_max_lines()));
                self.commit(lines);
            }
            AgentEvent::CompactionStart { reason } => {
                self.status = Some(match reason {
                    CompactionReason::Manual => "Compacting context…".into(),
                    CompactionReason::Threshold => "Context is nearly full; compacting…".into(),
                    CompactionReason::Overflow => "Context overflowed; compacting and retrying…".into(),
                });
            }
            AgentEvent::CompactionEnd { result, error, .. } => {
                self.status = None;
                match (result, error) {
                    (Some(result), _) => self.notice(&format!(
                        "◇ Compacted context ({} tokens summarized)",
                        format_tokens(result.tokens_before)
                    )),
                    (None, Some(error)) => self.error(&format!("Compaction failed: {error}")),
                    _ => {}
                }
                self.refresh_footer();
            }
            AgentEvent::AutoRetryStart { attempt, max_attempts, delay_ms, error_message } => {
                self.status = Some(format!("Retrying in {:.0}s ({attempt}/{max_attempts})", delay_ms as f64 / 1000.0));
                self.notice(&format!("{} — retrying ({attempt}/{max_attempts})", sanitize(&error_message)));
            }
            AgentEvent::QueueUpdate { steering, follow_up } => self.queued = (steering, follow_up),
        }
    }

    fn on_app(&mut self, msg: AppMsg) {
        match msg {
            AppMsg::BashUpdate(output) => {
                if let Some((_, live)) = &mut self.bash_live {
                    *live = output;
                }
            }
            AppMsg::BashDone(result) => {
                self.bash_live = None;
                match result {
                    Ok(message) => {
                        self.gap();
                        self.commit(render::bash_execution_lines(&message, self.tool_max_lines()));
                    }
                    Err(err) => self.error(&format!("{err:#}")),
                }
                self.refresh_footer();
            }
            AppMsg::CompactDone(result) => {
                // Success and failure are reported through compaction events; only report
                // errors that happened before compaction started.
                if let Err(err) = result
                    && self.status.is_none()
                {
                    self.error(&format!("{err:#}"));
                }
                self.refresh_footer();
            }
        }
    }

    // --- Input --------------------------------------------------------------------------------

    fn content_width(&self) -> usize {
        self.width().saturating_sub(2).max(4)
    }

    async fn on_terminal(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                let result = if self.overlay.is_some() {
                    self.on_overlay_key(key)
                } else {
                    self.notice = None;
                    self.on_key(key).await
                };
                // A failed action (switching models, saving settings, ...) is reported and the
                // session continues; terminal failures surface from drawing instead.
                if let Err(err) = result {
                    self.error(&format!("{err:#}"));
                }
            }
            Event::Paste(text) => {
                if self.overlay.is_some() {
                    return Ok(());
                }
                if let Some(path) = pasted_image_path(&text, self.agent.cwd()) {
                    match crate::images::load_file(&path) {
                        Ok(image) => self.attachments.push(image.block),
                        Err(err) => self.notice = Some(format!("Could not attach {}: {err:#}", path.display())),
                    }
                } else {
                    self.editor.insert(&text.replace("\r\n", "\n").replace('\r', "\n"));
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn on_key(&mut self, key: KeyEvent) -> Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        if !(ctrl && key.code == KeyCode::Char('c')) {
            self.last_ctrl_c = None;
        }
        let suggestions = self.suggestions();
        match key.code {
            KeyCode::Enter if shift => self.editor.insert_newline(),
            KeyCode::Enter if alt => {
                if self.agent.busy() == Some(Busy::Running) {
                    self.submit(Some(QueueMode::FollowUp)).await?;
                } else {
                    self.editor.insert_newline();
                }
            }
            KeyCode::Char('j') if ctrl => self.editor.insert_newline(),
            KeyCode::Enter => {
                if self.editor.char_before_cursor() == Some('\\') {
                    self.editor.backspace();
                    self.editor.insert_newline();
                } else if !suggestions.is_empty()
                    && !COMMANDS.iter().any(|(n, _, _)| self.editor.text() == format!("/{n}"))
                {
                    let (name, _) = suggestions[self.suggestion.min(suggestions.len() - 1)].clone();
                    let takes_args = name.starts_with("/skill:")
                        || COMMANDS.iter().any(|(n, _, args)| *args && name == format!("/{n}"));
                    self.editor.set_text(&name);
                    self.suggestion = 0;
                    if takes_args {
                        self.editor.insert(" ");
                    } else {
                        self.submit(Some(QueueMode::Steer)).await?;
                    }
                } else {
                    self.submit(Some(QueueMode::Steer)).await?;
                }
            }
            KeyCode::Tab => self.complete(&suggestions),
            KeyCode::BackTab => match self.agent.cycle_thinking()? {
                Some(level) => {
                    self.notice = Some(format!("Thinking level: {level}"));
                    self.refresh_footer();
                }
                None => self.notice = Some("This model has no thinking levels.".into()),
            },
            KeyCode::Esc => {
                if self.agent.busy().is_some() {
                    let restored = self.agent.abort();
                    if !restored.is_empty() {
                        let mut text = restored.join("\n\n");
                        if !self.editor.is_empty() {
                            text = format!("{text}\n\n{}", self.editor.text());
                        }
                        self.editor.set_text(&text);
                    }
                } else if !suggestions.is_empty() {
                    self.editor.take();
                }
            }
            KeyCode::Char('c') if ctrl => {
                if !self.editor.is_empty() || !self.attachments.is_empty() {
                    self.editor.take();
                    self.attachments.clear();
                } else if self.agent.busy().is_some() {
                    self.agent.abort();
                } else if self.last_ctrl_c.is_some_and(|t| t.elapsed() < Duration::from_secs(2)) {
                    self.quit = true;
                } else {
                    self.last_ctrl_c = Some(Instant::now());
                    self.notice = Some("Press Ctrl+C again to exit".into());
                }
            }
            KeyCode::Char('d') if ctrl => {
                if self.editor.is_empty() && self.agent.busy().is_none() {
                    self.quit = true;
                } else {
                    self.editor.delete();
                }
            }
            KeyCode::Char('l') if ctrl => self.open_model_selector(),
            KeyCode::Char('t') if ctrl => {
                self.hide_thinking = !self.hide_thinking;
                self.notice = Some(if self.hide_thinking { "Thinking hidden" } else { "Thinking shown" }.into());
            }
            KeyCode::Char('o') if ctrl => {
                self.expanded = !self.expanded;
                self.notice = Some(if self.expanded { "Tool output expanded" } else { "Tool output collapsed" }.into());
            }
            KeyCode::Char('g') if ctrl => self.external_editor()?,
            KeyCode::Char('v') if ctrl => self.paste_clipboard_image(),
            KeyCode::Up if alt => {
                let (steering, follow_up) = self.agent.clear_queue();
                let mut parts: Vec<String> = steering.into_iter().chain(follow_up).collect();
                if !parts.is_empty() {
                    if !self.editor.is_empty() {
                        parts.push(self.editor.text().to_string());
                    }
                    self.editor.set_text(&parts.join("\n\n"));
                }
            }
            KeyCode::Up => {
                if !suggestions.is_empty() {
                    self.suggestion = self.suggestion.checked_sub(1).unwrap_or(suggestions.len().min(8) - 1);
                } else if !self.editor.up(self.content_width()) {
                    self.editor.history_prev();
                }
            }
            KeyCode::Down => {
                if !suggestions.is_empty() {
                    self.suggestion = (self.suggestion + 1) % suggestions.len().min(8);
                } else if !self.editor.down(self.content_width()) {
                    self.editor.history_next();
                }
            }
            KeyCode::Left if alt || ctrl => self.editor.word_left(),
            KeyCode::Right if alt || ctrl => self.editor.word_right(),
            KeyCode::Char('b') if alt => self.editor.word_left(),
            KeyCode::Char('f') if alt => self.editor.word_right(),
            KeyCode::Left => self.editor.left(),
            KeyCode::Right => self.editor.right(),
            KeyCode::Home => self.editor.home(),
            KeyCode::End => self.editor.end(),
            KeyCode::Char('a') if ctrl => self.editor.home(),
            KeyCode::Char('e') if ctrl => self.editor.end(),
            KeyCode::Char('w') if ctrl => self.editor.delete_word_back(),
            KeyCode::Backspace if alt || ctrl => self.editor.delete_word_back(),
            KeyCode::Char('u') if ctrl => self.editor.delete_to_line_start(),
            KeyCode::Char('k') if ctrl => self.editor.delete_to_line_end(),
            KeyCode::Backspace => {
                if self.editor.is_empty() {
                    self.attachments.pop();
                } else {
                    self.editor.backspace();
                }
            }
            KeyCode::Delete => self.editor.delete(),
            KeyCode::Char(c) if !ctrl => {
                let mut buf = [0u8; 4];
                self.editor.insert(c.encode_utf8(&mut buf));
                self.suggestion = 0;
            }
            _ => {}
        }
        Ok(())
    }

    fn complete(&mut self, suggestions: &[(String, String)]) {
        if !suggestions.is_empty() {
            let (name, _) = &suggestions[self.suggestion.min(suggestions.len() - 1)];
            self.editor.set_text(&format!("{name} "));
            self.suggestion = 0;
            return;
        }
        let (start, token) = self.editor.token_before_cursor();
        let (at, token) = match token.strip_prefix('@') {
            Some(rest) => ("@", rest.to_string()),
            None => ("", token.to_string()),
        };
        let (dir_part, prefix) = match token.rfind('/') {
            Some(i) => (token[..=i].to_string(), token[i + 1..].to_string()),
            None => (String::new(), token.clone()),
        };
        let dir = crate::tools::resolve_path(if dir_part.is_empty() { "." } else { &dir_part }, self.agent.cwd());
        let Ok(entries) = std::fs::read_dir(&dir) else { return };
        let mut matches: Vec<(String, bool)> = entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                (name.starts_with(&prefix) && (prefix.starts_with('.') || !name.starts_with('.')))
                    .then(|| (name, e.path().is_dir()))
            })
            .collect();
        matches.sort();
        match matches.as_slice() {
            [] => {}
            [(name, is_dir)] => {
                let suffix = if *is_dir { "/" } else { " " };
                self.editor.replace_before_cursor(start, &format!("{at}{dir_part}{name}{suffix}"));
            }
            many => {
                let first = &many[0].0;
                let common = many.iter().fold(first.len(), |len, (name, _)| {
                    first
                        .chars()
                        .zip(name.chars())
                        .take_while(|(a, b)| a == b)
                        .map(|(a, _)| a.len_utf8())
                        .sum::<usize>()
                        .min(len)
                });
                if common > prefix.len() {
                    self.editor.replace_before_cursor(start, &format!("{at}{dir_part}{}", &first[..common]));
                }
                let names: Vec<String> =
                    many.iter().take(30).map(|(n, d)| if *d { format!("{n}/") } else { n.clone() }).collect();
                self.notice = Some(names.join("  "));
            }
        }
    }

    fn paste_clipboard_image(&mut self) {
        let result = arboard::Clipboard::new()
            .and_then(|mut clipboard| clipboard.get_image())
            .map_err(anyhow::Error::from)
            .and_then(|image| {
                crate::images::from_rgba(image.width as u32, image.height as u32, image.bytes.into_owned())
            });
        match result {
            Ok(image) => self.attachments.push(image.block),
            Err(_) => self.notice = Some("No image on the clipboard".into()),
        }
    }

    fn external_editor(&mut self) -> Result<()> {
        let editor = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).unwrap_or_else(|_| "vi".into());
        let path = std::env::temp_dir().join(format!("viper-prompt-{}.md", uuid::Uuid::new_v4().simple()));
        std::fs::write(&path, self.editor.text())?;
        self.screen.clear()?;
        self.guard.suspend()?;
        let status = tokio::task::block_in_place(|| {
            std::process::Command::new("sh").arg("-c").arg(format!("{editor} \"$1\"")).arg("sh").arg(&path).status()
        });
        self.guard.resume()?;
        match status {
            Ok(status) if status.success() => {
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                self.editor.set_text(text.trim_end_matches('\n'));
            }
            Ok(status) => self.notice = Some(format!("Editor exited with {status}")),
            Err(err) => self.notice = Some(format!("Could not run {editor}: {err}")),
        }
        let _ = std::fs::remove_file(&path);
        Ok(())
    }

    async fn submit(&mut self, queue: Option<QueueMode>) -> Result<()> {
        let text = self.editor.text().trim_end().to_string();
        if text.trim().is_empty() && self.attachments.is_empty() {
            return Ok(());
        }
        self.editor.take();
        self.editor.push_history(&text);
        append_history(&text);

        if let Some(rest) = text.strip_prefix('/') {
            let name = rest.split_whitespace().next().unwrap_or("");
            if COMMANDS.iter().any(|(n, _, _)| *n == name) {
                let args = rest[name.len()..].trim().to_string();
                return self.command(name, &args).await;
            }
        }
        if let Some(command) = text.strip_prefix('!') {
            let (exclude, command) = match command.strip_prefix('!') {
                Some(rest) => (true, rest.trim()),
                None => (false, command.trim()),
            };
            if command.is_empty() {
                return Ok(());
            }
            if self.agent.busy().is_some() {
                self.editor.set_text(&text);
                self.notice = Some("Wait for the agent to finish (or press Esc) before running a command".into());
                return Ok(());
            }
            self.bash_live = Some((command.to_string(), String::new()));
            let (agent, tx, command) = (self.agent.clone(), self.app_tx.clone(), command.to_string());
            tokio::spawn(async move {
                let update_tx = tx.clone();
                let result = agent
                    .run_bash(&command, exclude, move |output| {
                        let _ = update_tx.send(AppMsg::BashUpdate(output.to_string()));
                    })
                    .await;
                let _ = tx.send(AppMsg::BashDone(result));
            });
            return Ok(());
        }

        let content = crate::agent::user_content(&text, std::mem::take(&mut self.attachments));
        let content = match crate::modes::expand_skill(&self.agent, content) {
            Ok(content) => content,
            Err(err) => {
                self.error(&format!("{err:#}"));
                return Ok(());
            }
        };
        match self.agent.prompt(content, queue) {
            Ok(PromptDisposition::Started) => {}
            Ok(PromptDisposition::Queued(_)) => {}
            Err(err) => self.error(&format!("{err:#}")),
        }
        Ok(())
    }

    // --- Commands -----------------------------------------------------------------------------

    async fn command(&mut self, name: &str, args: &str) -> Result<()> {
        match name {
            "help" => {
                let mut lines = vec![Line::plain(bold("Commands"))];
                for (name, desc, _) in COMMANDS {
                    lines.push(Line::plain(format!("  {CYAN}/{name:<10}{RESET} {desc}")));
                }
                for skill in &self.agent.setup().skills {
                    lines.push(Line::indented(
                        format!("{CYAN}/skill:{}{RESET} {GRAY}{}{RESET}", skill.name, skill.description),
                        "  ",
                        "    ",
                    ));
                }
                self.gap();
                self.commit(lines);
            }
            "hotkeys" => {
                let mut lines = vec![Line::plain(bold("Keyboard shortcuts"))];
                for (keys, desc) in HOTKEYS {
                    lines.push(Line::indented(format!("{CYAN}{keys:<30}{RESET} {desc}"), "  ", "  "));
                }
                self.gap();
                self.commit(lines);
            }
            "quit" => self.quit = true,
            "model" if args.is_empty() => self.open_model_selector(),
            "model" => match self.agent.setup().registry.find(args) {
                Ok(model) => self.apply_model(model, false)?,
                Err(err) => self.error(&format!("{err:#}")),
            },
            "thinking" if args.is_empty() => self.open_thinking_selector(),
            "thinking" => match args.parse::<ThinkingLevel>() {
                Ok(level) => {
                    let applied = self.agent.set_thinking(level)?;
                    self.notice(&format!("Thinking level: {applied}"));
                    self.refresh_footer();
                }
                Err(err) => self.error(&format!("{err:#}")),
            },
            "new" => match self.agent.new_session(self.agent.session_path().is_some()) {
                Ok(()) => {
                    self.notice("New session started.");
                    self.refresh_footer();
                }
                Err(err) => self.error(&format!("{err:#}")),
            },
            "resume" => self.open_session_selector(),
            "session" => self.show_session(),
            "name" => match self.agent.set_session_name(args) {
                Ok(()) => {
                    self.notice(&format!("Session named \"{args}\"."));
                    self.refresh_footer();
                }
                Err(err) => self.error(&format!("{err:#}")),
            },
            "compact" => {
                let (agent, tx, args) = (self.agent.clone(), self.app_tx.clone(), args.to_string());
                tokio::spawn(async move {
                    let instructions = (!args.is_empty()).then_some(args);
                    let result = agent.compact(instructions.as_deref()).await.map(|_| ());
                    let _ = tx.send(AppMsg::CompactDone(result));
                });
            }
            "autocompact" => self.autocompact(args),
            "copy" => match self.agent.last_assistant_text() {
                Some(text) => match arboard::Clipboard::new().and_then(|mut c| c.set_text(text)) {
                    Ok(()) => self.notice = Some("Copied the last response.".into()),
                    Err(err) => self.error(&format!("Could not copy: {err}")),
                },
                None => self.notice = Some("Nothing to copy yet.".into()),
            },
            _ => {}
        }
        Ok(())
    }

    /// `/autocompact [<size>|auto|off|on]`: change when auto-compaction runs for the current
    /// model, saving the choice to the global settings like Claude Code does.
    fn autocompact(&mut self, args: &str) {
        let key = self.agent.model().key();
        let arg = args.trim().to_ascii_lowercase();
        let saved = match arg.as_str() {
            "" => Ok(()),
            "on" | "off" => {
                let enabled = arg == "on";
                self.agent.set_auto_compaction(enabled);
                Settings::save_auto_compaction(enabled)
            }
            "auto" => {
                self.agent.set_auto_compact_window(None);
                Settings::save_auto_compact_window(&key, None)
            }
            size => match parse_auto_compact_window(size) {
                Ok(window) => {
                    self.agent.set_auto_compact_window(Some(window));
                    let enable = (!self.agent.snapshot().auto_compaction_enabled).then(|| {
                        self.agent.set_auto_compaction(true);
                        Settings::save_auto_compaction(true)
                    });
                    Settings::save_auto_compact_window(&key, Some(window)).and(enable.unwrap_or(Ok(())))
                }
                Err(err) => {
                    self.error(&format!("{err:#}"));
                    return;
                }
            },
        };

        let snapshot = self.agent.snapshot();
        let threshold = format_tokens(snapshot.compaction_threshold);
        let mut text = if !snapshot.auto_compaction_enabled {
            "Auto-compaction is off.".to_string()
        } else {
            match snapshot.auto_compact_window {
                Some(window) if window > snapshot.compaction_threshold => format!(
                    "Auto-compaction runs at {threshold} tokens for {key} ({} requested; capped by the model's {} window).",
                    format_tokens(window),
                    format_tokens(snapshot.model.context_window)
                ),
                Some(_) => format!("Auto-compaction runs at {threshold} tokens for {key}."),
                None => format!(
                    "Auto-compaction runs at {threshold} tokens for {key} (auto: the {} window minus room for the response).",
                    format_tokens(snapshot.model.context_window)
                ),
            }
        };
        if arg.is_empty() {
            text.push_str(" Change it with /autocompact <size> (100k–1M), auto, off, or on.");
        }
        self.notice(&text);
        if let Err(err) = saved {
            self.error(&format!("The change applies to this session but could not be saved: {err:#}"));
        }
        self.refresh_footer();
    }

    fn show_session(&mut self) {
        let stats = self.agent.stats();
        let mut lines = vec![Line::plain(bold("Session"))];
        let file = stats.session_file.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "(not saved)".into());
        let rows = [
            ("id", stats.session_id.clone()),
            ("file", file),
            ("name", self.agent.session_name().unwrap_or_else(|| "-".into())),
            (
                "messages",
                format!(
                    "{} user, {} assistant, {} tool results",
                    stats.user_messages, stats.assistant_messages, stats.tool_results
                ),
            ),
            (
                "context",
                format!("{} of {} tokens", format_tokens(stats.context_tokens), format_tokens(stats.context_window)),
            ),
            (
                "tokens",
                format!(
                    "{} in, {} out, {} cache read, {} cache write",
                    format_tokens(stats.tokens.input),
                    format_tokens(stats.tokens.output),
                    format_tokens(stats.tokens.cache_read),
                    format_tokens(stats.tokens.cache_write)
                ),
            ),
            ("cost", format!("${:.4}", stats.cost)),
        ];
        for (key, value) in rows {
            lines.push(Line::indented(format!("{GRAY}{key:<9}{RESET}{value}"), "  ", "           "));
        }
        self.gap();
        self.commit(lines);
    }

    fn apply_model(&mut self, model: crate::config::Model, save: bool) -> Result<()> {
        let key = model.key();
        self.agent.set_model(model)?;
        if save {
            Settings::save_global(|map| {
                map.insert("defaultModel".into(), Value::String(key.clone()));
            })?;
        }
        self.notice(&format!("Model: {key}{}", if save { " (saved as default)" } else { "" }));
        self.refresh_footer();
        Ok(())
    }

    fn open_model_selector(&mut self) {
        let current = self.agent.model().key();
        // Models without credentials are listed only when nothing is usable, to explain why.
        let registry = &self.agent.setup().registry;
        let available: Vec<&crate::config::Model> = registry.available();
        let models: Vec<crate::config::Model> =
            if available.is_empty() { registry.all().to_vec() } else { available.into_iter().cloned().collect() };
        let items = models
            .iter()
            .map(|m| Item {
                label: m.key(),
                detail: format!(
                    "{} · {}k ctx{}",
                    m.name,
                    m.context_window / 1000,
                    if has_credentials(m) { "" } else { " · no credentials" }
                ),
            })
            .collect();
        let initial = models.iter().position(|m| m.key() == current).unwrap_or(0);
        self.overlay = Some(Overlay::Model(
            Selector::new(
                "Model",
                items,
                initial,
                "↑↓ select · enter use · ctrl+s use and save as default · esc cancel",
            ),
            models,
        ));
    }

    fn open_thinking_selector(&mut self) {
        let model = self.agent.model();
        if model.thinking_levels.is_empty() {
            self.notice = Some("This model has no thinking levels.".into());
            return;
        }
        let levels = model.thinking_levels.clone();
        let items = levels.iter().map(|l| Item { label: l.to_string(), detail: String::new() }).collect();
        let initial = levels.iter().position(|l| *l == self.agent.thinking()).unwrap_or(0);
        self.overlay = Some(Overlay::Thinking(
            Selector::new("Thinking level", items, initial, "enter use · ctrl+s use and save as default · esc cancel"),
            levels,
        ));
    }

    fn open_session_selector(&mut self) {
        let sessions = crate::session::list_sessions(self.agent.cwd());
        if sessions.is_empty() {
            self.notice = Some("No saved sessions for this directory.".into());
            return;
        }
        let items = sessions.iter().map(session_item).collect();
        let paths = sessions.into_iter().map(|s| s.path).collect();
        self.overlay =
            Some(Overlay::Session(Selector::new("Resume session", items, 0, "enter resume · esc cancel"), paths));
    }

    fn on_overlay_key(&mut self, key: KeyEvent) -> Result<()> {
        let Some(overlay) = &mut self.overlay else { return Ok(()) };
        let selector = match overlay {
            Overlay::Model(s, _) | Overlay::Thinking(s, _) | Overlay::Session(s, _) => s,
        };
        let (index, save) = match selector.handle_key(key) {
            SelectAction::None => return Ok(()),
            SelectAction::Cancel => {
                self.overlay = None;
                return Ok(());
            }
            SelectAction::Choose { index, save } => (index, save),
        };
        match self.overlay.take().expect("overlay checked above") {
            Overlay::Model(_, mut models) => self.apply_model(models.swap_remove(index), save)?,
            Overlay::Thinking(_, levels) => {
                let applied = self.agent.set_thinking(levels[index])?;
                if save {
                    Settings::save_global(|map| {
                        map.insert("defaultThinkingLevel".into(), Value::String(applied.to_string()));
                    })?;
                }
                self.notice(&format!("Thinking level: {applied}{}", if save { " (saved as default)" } else { "" }));
                self.refresh_footer();
            }
            Overlay::Session(_, paths) => match self.agent.switch_session(&paths[index]) {
                Ok(warnings) => {
                    self.notice(&format!("Resumed {}", paths[index].display()));
                    for warning in warnings {
                        self.error(&warning);
                    }
                    let lines = render::transcript_lines(
                        &self.agent.session_messages(),
                        self.hide_thinking,
                        self.tool_max_lines(),
                    );
                    self.commit(lines);
                    self.refresh_footer();
                }
                Err(err) => self.error(&format!("{err:#}")),
            },
        }
        Ok(())
    }

    fn welcome(&mut self, warnings: &[String]) {
        let setup = self.agent.setup();
        let model = self.agent.model();
        let mut lines = vec![Line::plain(format!(
            "{BOLD}{CYAN}viper{RESET} {GRAY}v{}{RESET}  {} {GRAY}({}){RESET}",
            env!("CARGO_PKG_VERSION"),
            model.name,
            model.key()
        ))];
        if !setup.context_files.is_empty() {
            let files: Vec<String> = setup.context_files.iter().map(|f| crate::util::tildify(&f.path)).collect();
            lines.push(Line::indented(dim(&format!("context: {}", files.join(", "))), "", "  "));
        }
        if !setup.skills.is_empty() {
            let names: Vec<&str> = setup.skills.iter().map(|s| s.name.as_str()).collect();
            lines.push(Line::indented(dim(&format!("skills: {}", names.join(", "))), "", "  "));
        }
        let mut problems: Vec<String> = warnings.to_vec();
        problems.extend(setup.skill_warnings.iter().cloned());
        for problem in problems {
            lines.push(Line::indented(paint(YELLOW, &format!("warning: {problem}")), "", "  "));
        }
        lines.push(Line::plain(dim(
            "esc interrupt · ctrl+c clear/exit · shift+tab thinking · ctrl+l model · /help commands · /hotkeys",
        )));
        self.commit(lines);
    }
}

fn session_item(summary: &crate::session::SessionSummary) -> Item {
    let title = summary.name.clone().unwrap_or_else(|| summary.first_message.replace('\n', " "));
    let title = if title.trim().is_empty() { "(empty)".to_string() } else { title };
    Item {
        label: truncate(&sanitize(&title), 60),
        detail: format!("{} · {} messages", relative_time(summary.modified), summary.message_count),
    }
}

/// Run the interactive UI until the user exits.
pub async fn run(
    agent: Agent,
    mut events: mpsc::UnboundedReceiver<AgentEvent>,
    initial: Vec<Vec<ContentBlock>>,
    warnings: Vec<String>,
) -> Result<ExitCode> {
    let guard = TerminalGuard::enter().context("could not set up the terminal")?;
    let (app_tx, mut app_rx) = mpsc::unbounded_channel();
    let mut app = App::new(agent.clone(), guard, app_tx);
    app.welcome(&warnings);
    let history = agent.session_messages();
    if !history.is_empty() {
        let lines = render::transcript_lines(&history, app.hide_thinking, app.tool_max_lines());
        app.commit(lines);
        app.notice("Resumed session.");
    }
    for (i, content) in initial.into_iter().enumerate() {
        let content = match crate::modes::expand_skill(&agent, content) {
            Ok(content) => content,
            Err(err) => {
                app.error(&format!("{err:#}"));
                continue;
            }
        };
        let queue = if i == 0 { None } else { Some(QueueMode::FollowUp) };
        if let Err(err) = agent.prompt(content, queue) {
            app.error(&format!("{err:#}"));
        }
    }

    let mut terminal_events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(80));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut dirty = true;
    loop {
        if dirty {
            app.draw()?;
            dirty = false;
        }
        if app.quit {
            break;
        }
        tokio::select! {
            event = terminal_events.next() => match event {
                Some(Ok(event)) => {
                    app.on_terminal(event).await?;
                    dirty = true;
                }
                Some(Err(err)) => return Err(err).context("terminal input failed"),
                None => break,
            },
            Some(event) = events.recv() => {
                app.on_agent(event);
                while let Ok(event) = events.try_recv() {
                    app.on_agent(event);
                }
                dirty = true;
            }
            Some(msg) = app_rx.recv() => {
                app.on_app(msg);
                dirty = true;
            }
            _ = tick.tick() => {
                if app.animating() || app.status.is_some() {
                    app.spinner += 1;
                    dirty = true;
                }
            }
        }
    }

    if agent.busy().is_some() {
        agent.abort();
        let _ = tokio::time::timeout(Duration::from_secs(3), agent.wait_idle()).await;
    }
    app.screen.clear()?;
    let session_path = agent.session_path().filter(|p| p.exists());
    drop(app);
    if let Some(path) = session_path {
        println!("{GRAY}Session saved to {}{RESET}", crate::util::tildify(&path));
        println!("{GRAY}Continue with: viper -c{RESET}");
    }
    Ok(ExitCode::SUCCESS)
}

/// Let the user pick a saved session before starting (`viper --resume`).
pub async fn pick_session(cwd: &Path) -> Result<Option<PathBuf>> {
    let sessions = crate::session::list_sessions(cwd);
    if sessions.is_empty() {
        eprintln!("No saved sessions for {}", cwd.display());
        return Ok(None);
    }
    let _guard = TerminalGuard::enter()?;
    let mut screen = Screen::new();
    let items = sessions.iter().map(session_item).collect();
    let mut selector = Selector::new("Resume session", items, 0, "↑↓ select · enter resume · esc cancel");
    let mut events = EventStream::new();
    let choice = loop {
        screen.draw(&[], &selector.render(screen.width().max(terminal::size().0)), None)?;
        match events.next().await {
            Some(Ok(Event::Key(key))) if key.kind != KeyEventKind::Release => match selector.handle_key(key) {
                SelectAction::None => {}
                SelectAction::Cancel => break None,
                SelectAction::Choose { index, .. } => break Some(sessions[index].path.clone()),
            },
            Some(Ok(_)) => {}
            Some(Err(err)) => return Err(err.into()),
            None => break None,
        }
    };
    screen.clear()?;
    Ok(choice)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_token_counts() {
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(4_500), "4.5k");
        assert_eq!(format_tokens(12_345), "12k");
        assert_eq!(format_tokens(1_000_000), "1M");
        assert_eq!(format_tokens(128_000), "128k");
    }
}
