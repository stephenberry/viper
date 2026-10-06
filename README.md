# viper

A minimal coding agent for the terminal, written in Rust. viper is modeled on [pi](https://github.com/earendil-works/pi) but keeps only the essentials: Anthropic models (directly or through a LiteLLM gateway), seven built-in tools, sessions, compaction, `AGENTS.md`, and skills.

## Install

```bash
cargo install --path .
```

Requires Rust 1.96 or newer.

## Quick start

```bash
export ANTHROPIC_API_KEY=sk-ant-...
cd your/project
viper
```

The default model is Claude Opus 5.5 at the `high` thinking level. `viper --list-models` shows every configured model.

## Modes

| Command | What it does |
|---|---|
| `viper [message]` | Interactive terminal UI |
| `viper -p "prompt"` | Run the prompt, print the final response, exit |
| `viper --mode json "prompt"` | Run the prompt and stream agent events as JSON lines |
| `viper --mode rpc` | JSON-lines command protocol on stdin/stdout |

Arguments starting with `@` attach files (`viper @src/main.rs "explain this"`); images are sent as images. In print and JSON modes, piped stdin is prepended to the prompt.

Common options: `-m/--model <provider/id>`, `--thinking <level>`, `-c/--continue`, `-r/--resume`, `--session <file>`, `--no-session`, `--tools read,bash,...`, `--no-tools`, `--no-skills`, `--system-prompt`, `--append-system-prompt`, `--api-key`, `--cwd`. See `viper --help`.

## Interactive use

Output flows into your terminal's normal scrollback, so scrolling, selection, and copying work as usual. Only the editor and status lines at the bottom are redrawn.

- **Enter** sends. While the agent works, Enter steers it (the message is injected after the current tool calls) and **Alt+Enter** queues a follow-up for when it finishes.
- **Esc** interrupts; queued messages return to the editor.
- **Shift+Enter**, **Ctrl+J**, or `\` then Enter inserts a newline.
- **Shift+Tab** cycles the thinking level, **Ctrl+L** picks a model, **Ctrl+G** opens `$VISUAL`/`$EDITOR`, **Ctrl+V** pastes a clipboard image. Dragging an image file into the terminal attaches it.
- `!command` runs a shell command and adds its output to the conversation; `!!command` keeps it out of the model's context.
- `/help` lists commands: `/model`, `/thinking`, `/new`, `/resume`, `/session`, `/name`, `/compact`, `/copy`, `/hotkeys`, `/quit`, and `/skill:<name>`.

## Tools

`read`, `write`, `edit`, `bash`, `grep`, `find`, and `ls`. Tool calls in one response run concurrently; writes to the same file are serialized. Output is truncated to 2000 lines or 50KB; truncated `bash` output is saved to a temp file whose path is given to the model. `grep` and `find` are built in (no ripgrep or fd needed) and respect `.gitignore`.

viper does not ask before running tools. Use it in a sandbox for untrusted work.

## Configuration

Everything lives in `~/.viper` (override with `VIPER_DIR`):

| Path | Purpose |
|---|---|
| `settings.json` | Settings; merged with `<project>/.viper/settings.json` |
| `models.json` | Providers and models (LiteLLM, base URL overrides) |
| `AGENTS.md` | Instructions added to every session |
| `skills/` | User skills |
| `sessions/` | Saved sessions, grouped by working directory |

### Anthropic

The built-in `anthropic` provider reads `ANTHROPIC_API_KEY` (or `ANTHROPIC_AUTH_TOKEN`, sent as a bearer token) and honors `ANTHROPIC_BASE_URL`. To change its URL or key in configuration:

```json
{
  "providers": {
    "anthropic": { "baseUrl": "https://api.anthropic.com", "apiKey": "$MY_ANTHROPIC_KEY" }
  }
}
```

### LiteLLM

Add the gateway as a provider in `models.json`. Each model talks to the gateway through either its Anthropic-compatible endpoint (`/v1/messages`) or its OpenAI-compatible endpoint (`/v1/chat/completions`), chosen per model with `api`:

```json
{
  "providers": {
    "litellm": {
      "baseUrl": "http://localhost:4000",
      "apiKey": "$LITELLM_API_KEY",
      "api": "openai-completions",
      "models": [
        { "id": "claude-opus-5-5", "api": "anthropic-messages" },
        { "id": "claude-sonnet", "base": "claude-sonnet-5-5", "api": "anthropic-messages" },
        { "id": "gpt-5", "contextWindow": 400000, "reasoning": "effort", "thinkingLevels": ["low", "medium", "high"] }
      ]
    }
  }
}
```

Model ids that match a built-in Claude model (exactly, or after the last `/`, as in `anthropic/claude-opus-5-5`) inherit its context window, output limit, thinking support, image support, and prices. Use `base` to inherit from a built-in model under a different alias.

Provider fields: `baseUrl`, `apiKey`, `api` (`anthropic-messages` or `openai-completions`), `authHeader` (`bearer`, the default for custom providers, or `xapikey`), `headers`.

Model fields: `id`, `name`, `base`, `api`, `contextWindow`, `maxTokens`, `reasoning` (`adaptive`, `budget`, `effort`, `none`), `thinkingLevels`, `images`, `cost` (`input`, `output`, `cacheRead`, `cacheWrite` in dollars per million tokens), `cacheControl`, `eagerInputStreaming`, `headers`, and `extraBody` (fields merged into every request body).

`apiKey` and header values accept `$VAR` or `${VAR}`, `!command` (the command's output, run per request), or a literal.

### Settings

```json
{
  "defaultModel": "anthropic/claude-opus-5-5",
  "defaultThinkingLevel": "high",
  "tools": ["read", "bash", "edit", "write", "grep", "find", "ls"],
  "compaction": { "enabled": true, "reserveTokens": 16384, "keepRecentTokens": 20000 },
  "retry": { "enabled": true, "maxRetries": 3, "baseDelayMs": 2000 },
  "shellPath": null,
  "appendSystemPrompt": null,
  "skillPaths": [],
  "enableSkills": true,
  "hideThinking": false,
  "toolOutputLines": 10
}
```

In the model and thinking pickers, **Ctrl+S** saves the choice as the default.

## Context and skills

viper adds `AGENTS.md` (or `CLAUDE.md`) files to the system prompt: the global one in `~/.viper`, then one from each directory between the filesystem root and the working directory.

Skills follow the [Agent Skills](https://agentskills.io/specification) format: a directory containing `SKILL.md` with `name` and `description` frontmatter. viper lists each skill's name and description in the system prompt and the model reads the file when a task matches. `/skill:name args` loads a skill explicitly; `disable-model-invocation: true` hides a skill from the model. Skills are discovered in `.viper/skills` and `.agents/skills` from the working directory up to the repository root, `skillPaths`, `~/.viper/skills`, and `~/.agents/skills`.

## Sessions and compaction

Sessions are saved as JSONL in `~/.viper/sessions/` once the first message is sent. `viper -c` continues the latest session for the directory, `viper -r` and `/resume` pick one, and `/name` labels the current one. Resuming restores the session's model and thinking level.

When the context approaches the model's window (within `reserveTokens`), viper summarizes older messages into a structured checkpoint and keeps roughly the last `keepRecentTokens` verbatim. `/compact [focus]` compacts on demand. If a request overflows the context anyway, viper compacts and retries once. The full history stays in the session file.

Transient API failures (rate limits, overload, server errors, dropped connections) are retried with exponential backoff before any output has streamed.

## RPC protocol

`viper --mode rpc` reads one JSON command per line on stdin and writes responses and events as JSON lines on stdout. Each command may include an `id`, echoed in its response:

```json
{"id": "1", "type": "prompt", "message": "List the files"}
{"type": "response", "command": "prompt", "success": true, "data": {"disposition": {"status": "started"}}, "id": "1"}
```

Commands: `prompt` (`message`, optional `images` of `{data, mimeType}`, optional `streamingBehavior`: `steer` or `followUp` when busy), `steer`, `follow_up`, `abort`, `clear_queue`, `new_session`, `get_state`, `set_model` (`provider`, `modelId`), `get_available_models`, `set_thinking_level` (`level`), `cycle_thinking_level`, `get_available_thinking_levels`, `compact` (`customInstructions`), `set_auto_compaction` (`enabled`), `set_auto_retry` (`enabled`), `bash` (`command`, `excludeFromContext`), `abort_bash`, `get_session_stats`, `list_sessions`, `switch_session` (`sessionPath`), `get_last_assistant_text`, `set_session_name` (`name`), `get_messages`, `get_commands`.

Events (also the `--mode json` output): `agent_start`, `agent_end`, `turn_start`, `turn_end`, `message_start`, `message_update` (with an `assistantMessageEvent` delta: `text_*`, `thinking_*`, `toolcall_*`), `message_end`, `tool_execution_start`, `tool_execution_update`, `tool_execution_end`, `compaction_start`, `compaction_end`, `auto_retry_start`, and `queue_update`.
