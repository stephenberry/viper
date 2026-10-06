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
- **Shift+Tab** cycles the thinking level, **Ctrl+L** picks a model (only models with credentials are listed), **Ctrl+G** opens `$VISUAL`/`$EDITOR`, **Ctrl+V** pastes a clipboard image. Dragging an image file into the terminal attaches it.
- `!command` runs a shell command and adds its output to the conversation; `!!command` keeps it out of the model's context.
- `/help` lists commands: `/model`, `/thinking`, `/new`, `/resume`, `/session`, `/name`, `/compact`, `/autocompact`, `/login`, `/copy`, `/hotkeys`, `/quit`, and `/skill:<name>`.

## Tools

`read`, `write`, `edit`, `bash`, `grep`, `find`, and `ls`. Tool calls in one response run in order; consecutive read-only calls (`read`, `grep`, `find`, `ls`) run concurrently. `read` and `bash` output is truncated to 2000 lines or 50KB (`bash` keeps the end, and saves the full output to a temp file whose path is given to the model). `grep`, `find`, and `ls` stop at 100 matches, 1000 results, and 500 entries respectively (the model can raise this with `limit`), or 50KB. `grep` and `find` are built in (no ripgrep or fd needed) and respect `.gitignore`.

viper does not ask before running tools. Use it in a sandbox for untrusted work.

## Configuration

Everything lives in `~/.viper` (override with `VIPER_DIR`):

| Path | Purpose |
|---|---|
| `settings.json` | Settings; merged with `<project>/.viper/settings.json` |
| `models.json` | Providers and models (LiteLLM, base URL overrides); no secrets, safe to share |
| `auth.json` | API keys saved by `/login`, readable only by you |
| `AGENTS.md` | Instructions added to every session |
| `skills/` | User skills |
| `sessions/` | Saved sessions, grouped by working directory |

### Logging in

`/login <provider>` asks for the provider's base URL and API key, checks the key by listing the server's models, and saves it: the key to `auth.json` (mode 0600) and the URL to `models.json`. A key the server rejects is not saved. Logging in removes a plaintext `apiKey` from that provider in `models.json`, and the new key takes effect immediately. Without any configured key, the interactive UI still starts so you can run `/login`.

A provider's key is taken from, in order: `--api-key`, `auth.json`, `apiKey` in `models.json`, then (for the built-in `anthropic` provider) `ANTHROPIC_API_KEY` or `ANTHROPIC_AUTH_TOKEN`.

### Anthropic

The built-in `anthropic` provider reads `ANTHROPIC_API_KEY` (or `ANTHROPIC_AUTH_TOKEN`, sent as a bearer token) and honors `ANTHROPIC_BASE_URL`. To change its URL or key in configuration:

```json
{
  "providers": {
    "anthropic": { "baseUrl": "https://api.anthropic.com", "apiKey": "$MY_ANTHROPIC_KEY" }
  }
}
```

Without a `models` list, only `baseUrl`, `apiKey`, `authHeader`, and `headers` apply to the built-in Anthropic models. A `models` list under `anthropic` replaces the built-in models with the listed ones.

### LiteLLM

Run `/login litellm` (or add `baseUrl` yourself), then list the gateway's models in `models.json`. Each model talks to the gateway through either its Anthropic-compatible endpoint (`/v1/messages`) or its OpenAI-compatible endpoint (`/v1/chat/completions`), chosen per model with `api`:

```json
{
  "providers": {
    "litellm": {
      "baseUrl": "http://localhost:4000",
      "api": "openai-completions",
      "models": [
        { "id": "claude-opus-5-5", "api": "anthropic-messages" },
        { "id": "claude-opus-5-5", "alias": "claude-opus-5-5-openai", "name": "Claude Opus 5.5 (OpenAI API)" },
        { "id": "claude-sonnet", "base": "claude-sonnet-5-5", "api": "anthropic-messages" },
        { "id": "gpt-5", "contextWindow": 400000, "reasoning": "effort", "thinkingLevels": ["low", "medium", "high"] }
      ]
    }
  }
}
```

Model ids that name a built-in Claude model inherit its context window, output limit, thinking support, image support, and prices. The id can match exactly, after the last `/` (`anthropic/claude-opus-5-5`), or after the last `.` (Bedrock's `us.anthropic.claude-opus-5-5`). Use `base` to inherit from a built-in model under a different alias. Fields you set override inherited ones, which matters when a gateway prices or limits a model differently.

viper identifies models as `provider/id`. To list the same gateway model twice (for example through both endpoints), give one an `alias`: viper selects it as `provider/alias` and still sends `id` to the gateway.

Provider fields: `baseUrl`, `apiKey`, `api` (`anthropic-messages` or `openai-completions`), `authHeader` (`bearer`, the default for custom providers, or `xapikey`), `headers`.

Model fields: `id`, `alias`, `name`, `base`, `api`, `contextWindow`, `maxTokens`, `reasoning` (`adaptive`, `budget`, `effort`, `none`), `thinkingLevels`, `images`, `cost` (`input`, `output`, `cacheRead`, `cacheWrite` in dollars per million tokens), `cacheControl`, `eagerInputStreaming`, `headers`, and `extraBody` (fields merged into every request body).

`apiKey` and header values accept `$VAR` or `${VAR}`, `!command` (the command's output, run per request), or a literal.

### Settings

```json
{
  "defaultModel": "anthropic/claude-opus-5-5",
  "defaultThinkingLevel": "high",
  "tools": ["read", "bash", "edit", "write", "grep", "find", "ls"],
  "compaction": { "enabled": true, "reserveTokens": 16384, "keepRecentTokens": 20000, "autoCompactWindow": null },
  "retry": { "enabled": true, "maxRetries": 3, "baseDelayMs": 2000 },
  "shellPath": null,
  "appendSystemPrompt": null,
  "skillPaths": [],
  "enableSkills": true,
  "hideThinking": false,
  "toolOutputLines": 10
}
```

Choosing a model or thinking level in the interactive UI (`/model`, `/thinking`, **Ctrl+L**, **Shift+Tab**) also saves it as `defaultModel` or `defaultThinkingLevel`. Resuming a session and RPC commands do not change the defaults.

`modelSettings` holds per-model settings keyed by `provider/model-id`; `/autocompact` writes `autoCompactWindow` there.

## Context and skills

viper adds instruction files to the system prompt: the global one in `~/.viper`, then one from each directory between the filesystem root and the working directory. In each directory, the first of `AGENTS.override.md`, `AGENTS.md`, and `CLAUDE.md` is used.

Skills follow the [Agent Skills](https://agentskills.io/specification) format: a directory containing `SKILL.md` with `name` and `description` frontmatter. viper lists each skill's name and description in the system prompt and the model reads the file when a task matches. `/skill:name args` loads a skill explicitly; `disable-model-invocation: true` hides a skill from the model. Skills are discovered in `.viper/skills` and `.agents/skills` from the working directory up to the repository root, `skillPaths`, `~/.viper/skills`, and `~/.agents/skills`.

## Sessions and compaction

Sessions are saved as JSONL in `~/.viper/sessions/` once the first message is sent. `viper -c` continues the latest session for the directory, `viper -r` and `/resume` pick one, and `/name` labels the current one. Resuming restores the session's model and thinking level.

When the context approaches the model's window (within `reserveTokens`), viper summarizes older messages into a structured checkpoint and keeps roughly the last `keepRecentTokens` verbatim. `/compact [focus]` compacts on demand.

`/autocompact` changes when auto-compaction runs, as in Claude Code. `/autocompact 300k` compacts the current model's context at 300k tokens instead of near its full window, which keeps long sessions with large-window models cheaper. Sizes from 100k to 1M are accepted (`300k`, `1M`, or `300` for thousands), capped by the model's window. `/autocompact auto` returns to the default, `off` and `on` toggle auto-compaction for all models, and `/autocompact` alone shows the current setting. Sizes and `auto` are saved per model in `~/.viper/settings.json` (`modelSettings`); `off` and `on` set `compaction.enabled`, and setting a size turns auto-compaction back on. `compaction.autoCompactWindow` sets a default for models without their own size. The footer shows `compact at <size>` when a window is set. If a request overflows the context anyway, viper compacts and retries once. The full history stays in the session file.

Transient API failures (rate limits, overload, server errors, dropped connections) are retried with exponential backoff before any output has streamed.

## RPC protocol

`viper --mode rpc` reads one JSON command per line on stdin and writes responses and events as JSON lines on stdout. Each command may include an `id`, echoed in its response:

```json
{"id": "1", "type": "prompt", "message": "List the files"}
{"type": "response", "command": "prompt", "success": true, "data": {"disposition": {"status": "started"}}, "id": "1"}
```

Commands: `prompt` (`message`, optional `images` of `{data, mimeType}`, optional `streamingBehavior`: `steer` or `followUp` when busy), `steer`, `follow_up`, `abort`, `clear_queue`, `new_session`, `get_state`, `set_model` (`provider`, `modelId`), `get_available_models`, `set_thinking_level` (`level`), `cycle_thinking_level`, `get_available_thinking_levels`, `compact` (`customInstructions`), `set_auto_compaction` (`enabled`), `set_auto_compact_window` (`window`: tokens, a size such as `"300k"`, or `null` or `"auto"` for the default; not saved), `set_auto_retry` (`enabled`), `bash` (`command`, `excludeFromContext`), `abort_bash`, `get_session_stats`, `list_sessions`, `switch_session` (`sessionPath`), `get_last_assistant_text`, `set_session_name` (`name`), `get_messages`, `get_commands`.

Events (also the `--mode json` output, which starts with a `session` line giving the session id, file, model, and thinking level): `agent_start`, `agent_end`, `turn_start`, `turn_end`, `message_start`, `message_update` (with an `assistantMessageEvent` delta: `text_*`, `thinking_*`, `toolcall_*`), `message_end`, `tool_execution_start`, `tool_execution_update`, `tool_execution_end`, `compaction_start`, `compaction_end`, `auto_retry_start`, and `queue_update`.
