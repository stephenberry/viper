use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::edit_diff::{self, Edit};
use super::path::resolve_path;
use super::{Tool, ToolContext, ToolOutput, UpdateFn, parse_args};

pub struct EditTool;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Args {
    path: String,
    #[serde(default)]
    edits: Vec<Edit>,
}

/// Accept common argument shapes models produce: `edits` as a JSON string or a single object,
/// and top-level `oldText`/`newText`.
fn prepare_arguments(mut args: Value) -> Value {
    let Some(object) = args.as_object_mut() else { return args };
    if let Some(Value::String(raw)) = object.get("edits")
        && let Ok(parsed) = serde_json::from_str::<Value>(raw)
    {
        object.insert("edits".into(), parsed);
    }
    if let Some(single @ Value::Object(_)) = object.get("edits").cloned() {
        object.insert("edits".into(), Value::Array(vec![single]));
    }
    if let (Some(Value::String(old)), Some(Value::String(new))) =
        (object.get("oldText").cloned(), object.get("newText").cloned())
    {
        object.remove("oldText");
        object.remove("newText");
        let edits = object.entry("edits").or_insert_with(|| Value::Array(Vec::new()));
        if let Value::Array(list) = edits {
            list.push(json!({"oldText": old, "newText": new}));
        }
    }
    args
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }

    fn description(&self) -> String {
        "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping \
         region of the original file. If two changes affect the same block or nearby lines, merge them into one edit \
         instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes."
            .to_string()
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the file to edit (relative or absolute)"},
                "edits": {
                    "type": "array",
                    "description": "One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "oldText": {"type": "string", "description": "Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call."},
                            "newText": {"type": "string", "description": "Replacement text for this targeted edit."}
                        },
                        "required": ["oldText", "newText"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["path", "edits"],
            "additionalProperties": false
        })
    }

    fn snippet(&self) -> &'static str {
        "Make precise file edits with exact text replacement, including multiple disjoint edits in one call"
    }

    fn guidelines(&self) -> &'static [&'static str] {
        &[
            "Use edit for precise changes (edits[].oldText must match exactly)",
            "When changing multiple separate locations in one file, use one edit call with multiple entries in edits[] instead of multiple edit calls",
            "Each edits[].oldText is matched against the original file, not after earlier edits are applied. Do not emit overlapping or nested edits. Merge nearby changes into one edit.",
            "Keep edits[].oldText as small as possible while still being unique in the file. Do not pad with large unchanged regions.",
        ]
    }

    async fn execute(&self, ctx: &ToolContext, args: Value, _update: UpdateFn) -> anyhow::Result<ToolOutput> {
        let args: Args = parse_args("edit", prepare_arguments(args))?;
        if args.edits.is_empty() {
            anyhow::bail!("Edit tool input is invalid. edits must contain at least one replacement.");
        }
        let path = resolve_path(&args.path, &ctx.cwd);
        if ctx.cancel.is_cancelled() {
            anyhow::bail!("Operation aborted");
        }

        let raw = tokio::fs::read(&path)
            .await
            .map_err(|err| anyhow::anyhow!("Could not edit file: {}. {err}.", args.path))?;
        let raw = String::from_utf8(raw)
            .map_err(|_| anyhow::anyhow!("Could not edit file: {}. The file is not valid UTF-8.", args.path))?;
        let (bom, content) = match raw.strip_prefix('\u{FEFF}') {
            Some(rest) => ("\u{FEFF}", rest),
            None => ("", raw.as_str()),
        };
        let ending = edit_diff::detect_line_ending(content);
        let normalized = edit_diff::normalize_to_lf(content);
        let new_content = edit_diff::apply_edits(&normalized, &args.edits, &args.path).map_err(anyhow::Error::msg)?;

        let final_content = format!("{bom}{}", edit_diff::restore_line_endings(&new_content, ending));
        tokio::fs::write(&path, final_content)
            .await
            .map_err(|err| anyhow::anyhow!("Could not write {}: {err}", args.path))?;

        let (diff, first_changed_line) = edit_diff::display_diff(&normalized, &new_content, 4);
        let patch = edit_diff::unified_patch(&args.path, &normalized, &new_content);
        Ok(ToolOutput::text(format!("Successfully replaced {} block(s) in {}.", args.edits.len(), args.path))
            .with_details(json!({"diff": diff, "patch": patch, "firstChangedLine": first_changed_line})))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_argument_shapes() {
        let args = prepare_arguments(json!({"path": "a", "edits": "[{\"oldText\":\"x\",\"newText\":\"y\"}]"}));
        assert_eq!(args["edits"][0]["oldText"], "x");
        let args = prepare_arguments(json!({"path": "a", "edits": {"oldText": "x", "newText": "y"}}));
        assert_eq!(args["edits"][0]["newText"], "y");
        let args = prepare_arguments(json!({"path": "a", "oldText": "x", "newText": "y"}));
        assert_eq!(args["edits"], json!([{"oldText": "x", "newText": "y"}]));
    }

    #[tokio::test]
    async fn edits_file_preserving_crlf_and_bom() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "\u{FEFF}one\r\ntwo\r\n").unwrap();
        let ctx = crate::tools::tests_support::context(dir.path());
        let out = EditTool
            .execute(
                &ctx,
                json!({"path": "a.txt", "edits": [{"oldText": "two", "newText": "2"}]}),
                crate::tools::tests_support::noop(),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "\u{FEFF}one\r\n2\r\n");
    }
}
