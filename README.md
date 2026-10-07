# viper

A minimal coding agent for the terminal, written in Rust.

viper is modeled on [pi](https://github.com/earendil-works/pi) but keeps only the essentials:

- Anthropic models, directly or through a LiteLLM gateway
- Seven built-in tools: `read`, `write`, `edit`, `bash`, `grep`, `find`, `ls`
- Saved sessions with automatic compaction
- `AGENTS.md` instructions and skills

## Install

Prebuilt binaries for Linux, macOS, and Windows are on the [releases page](https://github.com/stephenberry/viper/releases). See [docs/install.md](docs/install.md) for step-by-step instructions.

To build from source (Rust 1.96 or newer):

```bash
cargo install --path .
```

## Quick start

```bash
export ANTHROPIC_API_KEY=sk-ant-...
cd your/project
viper
```

Or start `viper` without a key and run `/login anthropic`. To use a LiteLLM gateway, see [Setting up a model provider](docs/install.md#setting-up-a-model-provider).

The default model is Claude Opus 5.5 with `high` thinking. `viper --list-models` shows every configured model.

> [!WARNING]
> viper does not ask before running tools, including `bash`. Use it in a sandbox for untrusted work.

## Usage

| Command | What it does |
|---|---|
| `viper` | Start the interactive UI |
| `viper -c` | Continue the latest session in this directory |
| `viper -r` | Pick a session to resume |
| `viper -p "prompt"` | Print the final response and exit |
| `viper --mode json "prompt"` | Stream agent events as JSON lines |
| `viper --mode rpc` | Drive viper over stdin/stdout ([protocol](docs/rpc.md)) |

Attach files with `@`, for example `viper @src/main.rs "explain this"`. In print and JSON modes, piped stdin is added to the prompt. Run `viper --help` for all options.

### Interactive UI

Output goes to your terminal's normal scrollback, so scrolling and copying work as usual.

| Key | Action |
|---|---|
| Enter | Send. While the agent works, steer it instead |
| Alt+Enter | Queue a follow-up for when the agent finishes |
| Esc | Interrupt |
| Shift+Enter or Ctrl+J | New line |
| Shift+Tab | Cycle the thinking level |
| Ctrl+L | Pick a model |
| Ctrl+G | Edit the message in `$VISUAL` or `$EDITOR` |
| Ctrl+V | Paste an image (or drag an image file in) |

Type `!command` to run a shell command and share its output with the model, or `!!command` to run it without sharing. Type `/help` for slash commands such as `/model`, `/resume`, `/compact`, and `/login`.

## Instructions and skills

viper adds `AGENTS.md` files to the system prompt: the global one in `~/.viper`, then one from each directory between the filesystem root and the working directory. `AGENTS.override.md` takes precedence over `AGENTS.md`, and `CLAUDE.md` is used when neither exists.

Skills use the [Agent Skills](https://agentskills.io/specification) format. The model sees each skill's name and description and reads the full skill when a task calls for it; `/skill:name` loads one explicitly. viper finds skills in `.viper/skills` and `.agents/skills` from the working directory up to the repository root, and in `~/.viper/skills` and `~/.agents/skills`.

## Sessions

Sessions are saved in `~/.viper/sessions/`. When the context nears the model's limit, viper summarizes older messages and keeps recent ones verbatim; the full history stays in the session file. Run `/compact` to do this on demand, or `/autocompact 300k` to compact earlier and keep long sessions cheaper.

## Configuration

Settings, providers, and API keys live in `~/.viper`. See [docs/configuration.md](docs/configuration.md) for LiteLLM models, settings, and compaction options.

## Development

`cargo test` runs the tests; CI also checks `cargo fmt` and `cargo clippy`. Pushes and pull requests are tested on Linux; run the CI workflow manually to also test macOS or Windows.

To release, set `version` in `Cargo.toml`, commit, and push a matching tag (`git tag v0.2.0 && git push origin v0.2.0`).

## License

[MIT](LICENSE)
