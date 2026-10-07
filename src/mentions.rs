//! Files attached to a message by mentioning them as `@path`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::message::ContentBlock;
use crate::tools::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, format_size, truncate_head};

const OPEN_PREFIX: &str = "<file name=\"";
const CLOSE: &str = "\n</file>";

/// The content block attaching the file at `path`, called `name` for the model: the image, or the
/// text in a `<file>` element, cut to the read tool's limits.
pub fn attach(path: &Path, name: &str) -> Result<ContentBlock> {
    if crate::images::is_image_file(path) {
        return Ok(crate::images::load_file(path)?.block);
    }
    let bytes = std::fs::read(path).with_context(|| format!("could not read {}", path.display()))?;
    let text = String::from_utf8_lossy(&bytes);
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(&text).trim_end();
    let truncation = truncate_head(text, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
    let body = if truncation.first_line_exceeds_limit {
        format!(
            "[The first line is longer than {}; use the read tool or bash to see parts of it.]",
            format_size(DEFAULT_MAX_BYTES)
        )
    } else if truncation.truncated {
        format!(
            "{}\n\n[Showing lines 1-{} of {}. Use the read tool with offset={} for the rest.]",
            truncation.content,
            truncation.output_lines,
            truncation.total_lines,
            truncation.output_lines + 1
        )
    } else {
        truncation.content
    };
    Ok(ContentBlock::text(format!("{OPEN_PREFIX}{name}\">\n{body}{CLOSE}")))
}

/// The files that `@path` words in `text` name, relative to `cwd`, attached once each. Other `@`
/// words, such as directories and handles, stay plain text.
pub fn attachments(text: &str, cwd: &Path) -> Result<Vec<ContentBlock>> {
    let mut attached: Vec<PathBuf> = Vec::new();
    let mut blocks = Vec::new();
    for word in text.split_whitespace() {
        let Some((name, path)) = word.strip_prefix('@').and_then(|mention| mentioned_file(mention, cwd)) else {
            continue;
        };
        if !attached.contains(&path) {
            blocks.push(attach(&path, name)?);
            attached.push(path);
        }
    }
    Ok(blocks)
}

/// The file a mention names, allowing punctuation after it, as at the end of a sentence. The name
/// without that punctuation is tried first: Windows ignores trailing dots, so `notes.md.` would
/// otherwise open `notes.md` under the wrong name.
fn mentioned_file<'a>(mention: &'a str, cwd: &Path) -> Option<(&'a str, PathBuf)> {
    let trimmed = mention.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}', '"', '\'']);
    [trimmed, mention].into_iter().filter(|name| !name.is_empty()).find_map(|name| {
        let path = crate::tools::resolve_path(name, cwd);
        path.is_file().then_some((name, path))
    })
}

/// The name of the file a text block attaches, if it is an attachment made by `attach`.
pub fn attachment_name(text: &str) -> Option<&str> {
    let rest = text.strip_prefix(OPEN_PREFIX)?;
    let (name, _) = rest.split_once("\">\n")?;
    text.ends_with(CLOSE).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(blocks: &[ContentBlock]) -> Vec<String> {
        blocks
            .iter()
            .map(|block| match block {
                ContentBlock::Text { text, .. } => text.clone(),
                other => panic!("unexpected block {other:?}"),
            })
            .collect()
    }

    #[test]
    fn attaches_mentioned_files_once() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.path().join("notes.md"), "notes").unwrap();

        let text = "Compare @src/main.rs with @notes.md. Ask @someone, see @src, then @src/main.rs again";
        let blocks = attachments(text, dir.path()).unwrap();
        assert_eq!(
            texts(&blocks),
            ["<file name=\"src/main.rs\">\nfn main() {}\n</file>", "<file name=\"notes.md\">\nnotes\n</file>"]
        );
        assert_eq!(attachment_name(&texts(&blocks)[0]), Some("src/main.rs"));
        assert_eq!(attachment_name("<file name=\"x\"> typed by hand"), None);
        assert!(attachments("no mentions here, me@example.com", dir.path()).unwrap().is_empty());
    }

    #[test]
    fn cuts_long_files_to_the_read_limit() {
        let dir = tempfile::tempdir().unwrap();
        let lines: Vec<String> = (1..=3000).map(|i| format!("line {i}")).collect();
        std::fs::write(dir.path().join("long.txt"), lines.join("\n")).unwrap();
        let block = attach(&dir.path().join("long.txt"), "long.txt").unwrap();
        let text = &texts(&[block])[0];
        assert!(text.contains("line 2000\n\n[Showing lines 1-2000 of 3000. Use the read tool with offset=2001"));
        assert!(!text.contains("line 2001"));
    }
}
