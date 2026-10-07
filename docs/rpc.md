# RPC protocol

`viper --mode rpc` reads one JSON command per line on stdin and writes responses and events as JSON lines on stdout. Each command may include an `id`, echoed in its response:

```json
{"id": "1", "type": "prompt", "message": "List the files"}
{"type": "response", "command": "prompt", "success": true, "data": {"disposition": {"status": "started"}}, "id": "1"}
```

## Commands

`prompt` (`message`, optional `images` of `{data, mimeType}`, optional `streamingBehavior`: `steer` or `followUp` when busy), `steer`, `follow_up`, `abort`, `clear_queue`, `new_session`, `get_state`, `set_model` (`provider`, `modelId`), `get_available_models`, `set_thinking_level` (`level`), `cycle_thinking_level`, `get_available_thinking_levels`, `compact` (`customInstructions`), `set_auto_compaction` (`enabled`), `set_auto_compact_window` (`window`: tokens, a size such as `"300k"`, or `null` or `"auto"` for the default; not saved), `set_auto_retry` (`enabled`), `bash` (`command`, `excludeFromContext`), `abort_bash`, `get_session_stats`, `list_sessions`, `switch_session` (`sessionPath`), `get_last_assistant_text`, `set_session_name` (`name`), `get_messages`, `get_commands`.

RPC commands do not change the saved `defaultModel` or `defaultThinkingLevel`.

## Events

Events are also the `--mode json` output, which starts with a `session` line giving the session id, file, model, and thinking level: `agent_start`, `agent_end`, `turn_start`, `turn_end`, `message_start`, `message_update` (with an `assistantMessageEvent` delta: `text_*`, `thinking_*`, `toolcall_*`), `message_end`, `tool_execution_start`, `tool_execution_update`, `tool_execution_end`, `compaction_start`, `compaction_end`, `auto_retry_start`, and `queue_update`.
