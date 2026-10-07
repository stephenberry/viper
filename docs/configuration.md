# Configuration

viper keeps everything in `~/.viper` (override with `VIPER_DIR`):

| Path | Purpose |
|---|---|
| `settings.json` | Settings; merged with `<project>/.viper/settings.json` |
| `models.json` | Providers and models; no secrets, safe to share |
| `auth.json` | API keys saved by `/login`, readable only by you |
| `AGENTS.md` | Instructions added to every session |
| `skills/` | User skills |
| `sessions/` | Saved sessions, grouped by working directory |

## API keys

`/login <provider>` asks for the provider's base URL and API key, checks the key by listing the server's models, and saves the key to `auth.json` (mode 0600) and the URL to `models.json`. A key the server rejects is not saved. Logging in also removes any plaintext `apiKey` for that provider from `models.json`. Without any key, the interactive UI still starts so you can run `/login`.

viper looks for a provider's key in this order:

1. `--api-key`
2. `auth.json`
3. `apiKey` in `models.json`
4. `ANTHROPIC_API_KEY` or `ANTHROPIC_AUTH_TOKEN` (built-in `anthropic` provider only)

`apiKey` and header values in `models.json` accept `$VAR` or `${VAR}`, `!command` (the command's output, run per request), or a literal.

## Anthropic

The built-in `anthropic` provider reads `ANTHROPIC_API_KEY` (or `ANTHROPIC_AUTH_TOKEN`, sent as a bearer token) and honors `ANTHROPIC_BASE_URL`. To set its URL or key in `models.json`:

```json
{
  "providers": {
    "anthropic": { "baseUrl": "https://api.anthropic.com", "apiKey": "$MY_ANTHROPIC_KEY" }
  }
}
```

Without a `models` list, only `baseUrl`, `apiKey`, `authHeader`, and `headers` apply. Adding a `models` list under `anthropic` replaces the built-in models with the ones listed.

## LiteLLM and other providers

Any server with an Anthropic-compatible or OpenAI-compatible API can be added as a provider under a name of your choosing. The examples below use a LiteLLM gateway, which can front models from many vendors.

The quickest setup is `/login litellm`, which also adds the gateway's models (see [Setting up a model provider](install.md#setting-up-a-model-provider)). You can also list models yourself. Each model reaches the gateway through either its Anthropic-compatible endpoint (`/v1/messages`) or its OpenAI-compatible endpoint (`/v1/chat/completions`), chosen per model with `api`:

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

A model whose id names a built-in Claude model inherits its context window, output limit, thinking support, image support, and prices. The id can match exactly, after the last `/` (`anthropic/claude-opus-5-5`), or after the last `.` (Bedrock's `us.anthropic.claude-opus-5-5`). Use `base` to inherit from a built-in model under a different name. Fields you set override inherited ones, which matters when a gateway prices or limits a model differently.

Models are identified as `provider/id`. To list the same gateway model twice (for example through both endpoints), give one an `alias`: viper selects it as `provider/alias` and still sends `id` to the gateway.

### Syncing models

`/sync-models <provider>` (or `viper --sync-models <provider>`) lists the server's models (`GET /v1/models`) and adds the ones `models.json` does not have yet. With LiteLLM, each new entry also gets the context window, output limit, prices, and image and reasoning support that the gateway reports. Models named after a built-in Claude model keep the provider's `api` and inherit the rest; other models use `openai-completions`. Existing entries are never changed, and configured models the server no longer lists are only reported.

### Fields

**Provider:** `baseUrl`, `apiKey`, `api` (`anthropic-messages` or `openai-completions`), `authHeader` (`bearer`, the default for custom providers, or `xapikey`), `headers`.

**Model:** `id`, `alias`, `name`, `base`, `api`, `contextWindow`, `maxTokens`, `reasoning` (`adaptive`, `budget`, `effort`, `none`), `thinkingLevels`, `images`, `cost` (`input`, `output`, `cacheRead`, `cacheWrite` in dollars per million tokens), `cacheControl`, `eagerInputStreaming`, `headers`, and `extraBody` (fields merged into every request body).

## Settings

`settings.json` with its defaults:

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
  "toolOutputLines": 10,
  "autoUpdate": true
}
```

Picking a model or thinking level in the interactive UI (`/model`, `/thinking`, **Ctrl+L**, **Shift+Tab**) saves it as the default. Resuming a session and RPC commands do not.

`modelSettings` holds per-model settings keyed by `provider/model-id`; `/autocompact` writes `autoCompactWindow` there.

`retry` covers transient API failures (rate limits, overload, server errors, dropped connections), which are retried with exponential backoff as long as no output has streamed yet.

`autoUpdate: false` stops viper from installing new releases itself; it then only says when one is available (see [Updating](install.md#updating)).

## Skills

Besides the default locations, viper loads skills from the directories in `skillPaths`. `enableSkills: false` (or `--no-skills`) turns skills off. A skill with `disable-model-invocation: true` in its frontmatter is hidden from the model and only runs through `/skill:name`.

## Compaction

When the context comes within `reserveTokens` of the model's window, viper summarizes older messages into a structured checkpoint and keeps roughly the last `keepRecentTokens` verbatim. If the kept messages start partway through a long turn, the start of that turn (your request and the work so far) gets its own summary so the request is not lost. If a request overflows the context anyway, viper compacts and retries once. The full history always stays in the session file.

`/autocompact` changes when this happens:

| Command | Effect |
|---|---|
| `/autocompact 300k` | Compact the current model at 300k tokens instead of near its full window, keeping long sessions on large-window models cheaper. Accepts 100k to 1M (`300k`, `1M`, or `300` for thousands), capped by the window. Saved per model. |
| `/autocompact auto` | Return the current model to the default |
| `/autocompact off`, `on` | Turn auto-compaction off or on for all models (`compaction.enabled`) |
| `/autocompact` | Show the current setting |

Setting a size also turns auto-compaction back on. `compaction.autoCompactWindow` sets a default size for models without their own. The footer shows `compact at <size>` when a size is set.

## Tool output

`read` and `bash` output is cut at 2000 lines or 50KB. `bash` keeps the end and saves the full output to a temp file whose path is given to the model. `grep`, `find`, and `ls` stop at 100 matches, 1000 results, and 500 entries respectively (the model can raise this with `limit`), or 50KB. Consecutive read-only calls (`read`, `grep`, `find`, `ls`) run concurrently.

`toolOutputLines` sets how many lines of each tool's output the interactive UI shows.
