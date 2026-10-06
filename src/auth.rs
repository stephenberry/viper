//! Credentials saved by `/login`, kept in `auth.json` apart from the shareable `models.json`.
//!
//! The file maps provider names to credentials and is written with owner-only permissions.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::agent_dir;

pub fn auth_path() -> PathBuf {
    agent_dir().join("auth.json")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Credential {
    ApiKey { key: String },
}

#[derive(Debug, Default)]
pub struct AuthStore {
    entries: BTreeMap<String, Credential>,
}

impl AuthStore {
    pub fn load() -> Result<AuthStore> {
        AuthStore::load_from(&auth_path())
    }

    pub fn load_from(path: &Path) -> Result<AuthStore> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let entries = serde_json::from_str(&text).with_context(|| format!("invalid {}", path.display()))?;
                Ok(AuthStore { entries })
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(AuthStore::default()),
            Err(err) => Err(err).with_context(|| format!("could not read {}", path.display())),
        }
    }

    pub fn api_key(&self, provider: &str) -> Option<&str> {
        match self.entries.get(provider)? {
            Credential::ApiKey { key } => Some(key),
        }
    }

    pub fn set_api_key(&mut self, provider: &str, key: &str) {
        self.entries.insert(provider.to_string(), Credential::ApiKey { key: key.to_string() });
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&auth_path())
    }

    /// Write atomically through a temporary file created with owner-only permissions, so the
    /// keys are never readable by others, even briefly.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        let dir = path.parent().context("auth path has no parent directory")?;
        std::fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
        let temp = dir.join(format!(".auth.json.{}.tmp", std::process::id()));
        let result = (|| {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            let mut file = options.open(&temp)?;
            file.write_all((serde_json::to_string_pretty(&self.entries)? + "\n").as_bytes())?;
            file.sync_all()?;
            std::fs::rename(&temp, path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result.with_context(|| format!("could not write {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_with_private_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        assert_eq!(AuthStore::load_from(&path).unwrap().api_key("gateway"), None);

        let mut store = AuthStore::default();
        store.set_api_key("gateway", "sk-test");
        store.save_to(&path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(r#""type": "api_key""#));
        assert_eq!(AuthStore::load_from(&path).unwrap().api_key("gateway"), Some("sk-test"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }
}
