//! Session persistence as JSONL.
//!
//! The first line is a header; each following line is an entry. Sessions are linear: the model
//! context is rebuilt by replaying message entries, with the latest compaction entry replacing
//! everything before its `firstKeptEntryId` by a summary.
//!
//! Files live in `~/.viper/sessions/--<encoded cwd>--/<timestamp>_<id>.jsonl` and are created on
//! the first message, so sessions that never start leave nothing behind.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{ThinkingLevel, sessions_dir};
use crate::message::{CompactionSummaryMessage, ContentBlock, Message, Usage, content_text};

pub const SESSION_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHeader {
    pub version: u32,
    pub id: String,
    pub timestamp: String,
    pub cwd: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum Entry {
    Session(SessionHeader),
    Message {
        id: String,
        timestamp: String,
        message: Message,
    },
    ModelChange {
        id: String,
        timestamp: String,
        provider: String,
        model_id: String,
    },
    ThinkingLevelChange {
        id: String,
        timestamp: String,
        thinking_level: ThinkingLevel,
    },
    Compaction {
        id: String,
        timestamp: String,
        summary: String,
        /// First message entry kept verbatim after the summary. Equals this entry's own id when
        /// nothing before the compaction is kept.
        first_kept_entry_id: String,
        tokens_before: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
    },
    SessionInfo {
        id: String,
        timestamp: String,
        name: String,
    },
}

impl Entry {
    pub fn id(&self) -> &str {
        match self {
            Entry::Session(header) => &header.id,
            Entry::Message { id, .. }
            | Entry::ModelChange { id, .. }
            | Entry::ThinkingLevelChange { id, .. }
            | Entry::Compaction { id, .. }
            | Entry::SessionInfo { id, .. } => id,
        }
    }
}

pub fn new_entry_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
}

pub fn iso_now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// A message in the rebuilt context, with the entry it came from (`None` for the summary).
#[derive(Debug, Clone)]
pub struct ContextItem {
    pub entry_id: Option<String>,
    pub message: Message,
}

/// Directory holding sessions for `cwd`.
pub fn session_dir_for(cwd: &Path) -> PathBuf {
    let raw = cwd.to_string_lossy();
    let trimmed = raw.trim_start_matches(['/', '\\']);
    let encoded: String = trimmed.chars().map(|c| if matches!(c, '/' | '\\' | ':') { '-' } else { c }).collect();
    sessions_dir().join(format!("--{encoded}--"))
}

pub struct SessionStore {
    header: SessionHeader,
    entries: Vec<Entry>,
    path: Option<PathBuf>,
    file: Option<File>,
    persist: bool,
}

impl SessionStore {
    /// A new session for `cwd`. When `persist` is false the session stays in memory.
    pub fn create(cwd: &Path, persist: bool) -> SessionStore {
        let header = SessionHeader {
            version: SESSION_VERSION,
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: iso_now(),
            cwd: cwd.to_string_lossy().into_owned(),
        };
        let path = persist.then(|| {
            let stamp = chrono::Utc::now().format("%Y-%m-%dT%H-%M-%S-%3fZ");
            session_dir_for(cwd).join(format!("{stamp}_{}.jsonl", header.id))
        });
        SessionStore { header, entries: Vec::new(), path, file: None, persist }
    }

    /// Open an existing session file. Malformed lines are skipped with a warning.
    pub fn open(path: &Path) -> Result<(SessionStore, Vec<String>)> {
        let file = File::open(path).with_context(|| format!("could not open session {}", path.display()))?;
        let mut header = None;
        let mut entries = Vec::new();
        let mut warnings = Vec::new();
        for (number, line) in BufReader::new(file).lines().enumerate() {
            let line = line.with_context(|| format!("could not read {}", path.display()))?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Entry>(&line) {
                Ok(Entry::Session(h)) if header.is_none() => header = Some(h),
                Ok(entry) => entries.push(entry),
                Err(err) => warnings.push(format!("{}:{}: skipped invalid entry ({err})", path.display(), number + 1)),
            }
        }
        let header = header.with_context(|| format!("{} is not a viper session (missing header)", path.display()))?;
        let file = OpenOptions::new()
            .append(true)
            .open(path)
            .with_context(|| format!("could not open {} for writing", path.display()))?;
        Ok((
            SessionStore { header, entries, path: Some(path.to_path_buf()), file: Some(file), persist: true },
            warnings,
        ))
    }

    pub fn id(&self) -> &str {
        &self.header.id
    }

    /// The session file path (it may not exist yet if no message was recorded).
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    fn write_line(file: &mut File, value: &impl Serialize) -> Result<()> {
        let mut line = serde_json::to_string(value)?;
        line.push('\n');
        file.write_all(line.as_bytes())?;
        Ok(())
    }

    /// Record an entry. The file is created when the first message entry is appended.
    pub fn append(&mut self, entry: Entry) -> Result<()> {
        let is_message = matches!(entry, Entry::Message { .. });
        self.entries.push(entry);
        if !self.persist {
            return Ok(());
        }
        if let Some(file) = &mut self.file {
            Self::write_line(file, self.entries.last().expect("entry was just pushed"))?;
            return file.flush().context("could not write session file");
        }
        if !is_message {
            return Ok(());
        }
        let path = self.path.clone().expect("persistent sessions have a path");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("could not create {}", parent.display()))?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("could not create session file {}", path.display()))?;
        Self::write_line(&mut file, &Entry::Session(self.header.clone()))?;
        for entry in &self.entries {
            Self::write_line(&mut file, entry)?;
        }
        file.flush()?;
        self.file = Some(file);
        Ok(())
    }

    pub fn append_message(&mut self, message: Message) -> Result<String> {
        let id = new_entry_id();
        self.append(Entry::Message { id: id.clone(), timestamp: iso_now(), message })?;
        Ok(id)
    }

    /// Rebuild the model context from the entries.
    pub fn context(&self) -> Vec<ContextItem> {
        let compaction = self.entries.iter().enumerate().rev().find_map(|(index, entry)| match entry {
            Entry::Compaction { id, summary, first_kept_entry_id, tokens_before, timestamp, .. } => {
                Some((index, id, summary, first_kept_entry_id, *tokens_before, timestamp))
            }
            _ => None,
        });
        let message_item = |entry: &Entry| match entry {
            Entry::Message { id, message, .. } => {
                Some(ContextItem { entry_id: Some(id.clone()), message: message.clone() })
            }
            _ => None,
        };
        let Some((index, id, summary, first_kept, tokens_before, timestamp)) = compaction else {
            return self.entries.iter().filter_map(message_item).collect();
        };
        let timestamp = chrono::DateTime::parse_from_rfc3339(timestamp).map(|t| t.timestamp_millis()).unwrap_or(0);
        let mut items = vec![ContextItem {
            entry_id: None,
            message: Message::CompactionSummary(CompactionSummaryMessage {
                summary: summary.clone(),
                tokens_before,
                timestamp,
            }),
        }];
        if first_kept != id
            && let Some(start) = self.entries[..index].iter().position(|e| e.id() == first_kept)
        {
            items.extend(self.entries[start..index].iter().filter_map(message_item));
        }
        items.extend(self.entries[index + 1..].iter().filter_map(message_item));
        items
    }

    pub fn messages(&self) -> Vec<Message> {
        self.context().into_iter().map(|item| item.message).collect()
    }

    /// Every message ever recorded, including compacted ones (for display and statistics).
    pub fn all_messages(&self) -> impl Iterator<Item = &Message> {
        self.entries.iter().filter_map(|entry| match entry {
            Entry::Message { message, .. } => Some(message),
            _ => None,
        })
    }

    /// The most recent model selection, as `(provider, model id)`.
    pub fn last_model(&self) -> Option<(String, String)> {
        self.entries.iter().rev().find_map(|entry| match entry {
            Entry::ModelChange { provider, model_id, .. } => Some((provider.clone(), model_id.clone())),
            _ => None,
        })
    }

    pub fn last_thinking_level(&self) -> Option<ThinkingLevel> {
        self.entries.iter().rev().find_map(|entry| match entry {
            Entry::ThinkingLevelChange { thinking_level, .. } => Some(*thinking_level),
            _ => None,
        })
    }

    pub fn name(&self) -> Option<String> {
        self.entries.iter().rev().find_map(|entry| match entry {
            Entry::SessionInfo { name, .. } => Some(name.clone()),
            _ => None,
        })
    }

    /// Accumulated usage over the whole session, including compaction summaries.
    pub fn total_usage(&self) -> Usage {
        let mut total = Usage::default();
        for entry in &self.entries {
            match entry {
                Entry::Message { message: Message::Assistant(assistant), .. } => total.add(&assistant.usage),
                Entry::Compaction { usage: Some(usage), .. } => total.add(usage),
                _ => {}
            }
        }
        total
    }
}

/// Summary of a saved session for pickers and listings.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub path: PathBuf,
    pub id: String,
    pub name: Option<String>,
    pub first_message: String,
    pub message_count: usize,
    pub modified: chrono::DateTime<chrono::Local>,
}

fn summarize(path: &Path) -> Option<SessionSummary> {
    let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
    let (store, _) = SessionStore::open(path).ok()?;
    let mut first_message = String::new();
    let mut count = 0;
    for message in store.all_messages() {
        if let Message::User(user) = message {
            if first_message.is_empty() {
                first_message = content_text(&user.content);
                if first_message.is_empty() && user.content.iter().any(|b| matches!(b, ContentBlock::Image { .. })) {
                    first_message = "[image]".into();
                }
            }
            count += 1;
        } else if matches!(message, Message::Assistant(_)) {
            count += 1;
        }
    }
    Some(SessionSummary {
        path: path.to_path_buf(),
        id: store.header.id.clone(),
        name: store.name(),
        first_message,
        message_count: count,
        modified: modified.into(),
    })
}

/// Sessions saved for `cwd`, most recently modified first.
pub fn list_sessions(cwd: &Path) -> Vec<SessionSummary> {
    let Ok(entries) = std::fs::read_dir(session_dir_for(cwd)) else { return Vec::new() };
    let mut sessions: Vec<SessionSummary> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "jsonl"))
        .filter_map(|p| summarize(&p))
        .collect();
    sessions.sort_by_key(|s| std::cmp::Reverse(s.modified));
    sessions
}

/// The most recently modified session for `cwd`.
pub fn most_recent_session(cwd: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(session_dir_for(cwd)).ok()?;
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "jsonl"))
        .filter_map(|p| Some((std::fs::metadata(&p).and_then(|m| m.modified()).ok()?, p)))
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, path)| path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::UserMessage;

    fn user(text: &str) -> Message {
        Message::User(UserMessage::new(vec![ContentBlock::text(text)]))
    }

    fn in_memory() -> SessionStore {
        SessionStore::create(Path::new("/tmp/project"), false)
    }

    #[test]
    fn encodes_session_dirs() {
        let dir = session_dir_for(Path::new("/Users/me/my-proj"));
        assert!(dir.ends_with("--Users-me-my-proj--"));
    }

    #[test]
    fn context_replaces_compacted_history_with_summary() {
        let mut store = in_memory();
        store.append_message(user("one")).unwrap();
        let kept = store.append_message(user("two")).unwrap();
        store
            .append(Entry::Compaction {
                id: new_entry_id(),
                timestamp: iso_now(),
                summary: "S".into(),
                first_kept_entry_id: kept,
                tokens_before: 10,
                details: None,
                usage: None,
            })
            .unwrap();
        store.append_message(user("three")).unwrap();
        let messages = store.messages();
        assert_eq!(messages.len(), 3);
        assert!(matches!(&messages[0], Message::CompactionSummary(s) if s.summary == "S"));
        assert!(matches!(&messages[1], Message::User(u) if content_text(&u.content) == "two"));
        assert!(matches!(&messages[2], Message::User(u) if content_text(&u.content) == "three"));
    }

    #[test]
    fn persists_lazily_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let mut store = in_memory();
        store.persist = true;
        store.path = Some(path.clone());
        store
            .append(Entry::ModelChange {
                id: new_entry_id(),
                timestamp: iso_now(),
                provider: "anthropic".into(),
                model_id: "m".into(),
            })
            .unwrap();
        assert!(!path.exists());
        store.append_message(user("hi")).unwrap();
        store.append(Entry::SessionInfo { id: new_entry_id(), timestamp: iso_now(), name: "named".into() }).unwrap();
        assert!(path.exists());

        let (reopened, warnings) = SessionStore::open(&path).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(reopened.id(), store.id());
        assert_eq!(reopened.last_model(), Some(("anthropic".into(), "m".into())));
        assert_eq!(reopened.name().as_deref(), Some("named"));
        assert_eq!(reopened.messages().len(), 1);
    }
}
