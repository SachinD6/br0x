//! Crash safe session store. Atomic write, lazy load.

use crate::json_file;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// One stored tab. URLs and titles only, never page heap.
/// Everything except the URL defaults, so a session file written before a
/// field existed still loads instead of losing every tab.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredTab {
    #[serde(default)]
    pub id: u64,
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub order: usize,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub scroll_y: i32,
}

/// Full session file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub tabs: Vec<StoredTab>,
}

/// File backed store. Path is injected for testability.
#[derive(Debug, Clone)]
pub struct SessionStore {
    path: PathBuf,
}

impl SessionStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn save(&self, session: &Session) -> std::io::Result<()> {
        json_file::save(&self.path, session)
    }

    pub fn load(&self) -> std::io::Result<Session> {
        json_file::load(&self.path)
    }

    pub fn clear(&self) -> std::io::Result<()> {
        if self.path.exists() {
            std::fs::remove_file(&self.path)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Session {
        Session {
            tabs: vec![StoredTab {
                id: 1,
                url: "https://example.com".into(),
                title: "Example".into(),
                order: 0,
                pinned: false,
                scroll_y: 120,
            }],
        }
    }

    #[test]
    fn roundtrips_session() {
        let dir = std::env::temp_dir().join("br0x-test-session");
        let store = SessionStore::new(dir.join("session.json"));
        let _ = store.clear();
        store.save(&sample()).unwrap();
        assert_eq!(store.load().unwrap(), sample());
        store.clear().unwrap();
    }

    #[test]
    fn clear_missing_file_is_ok() {
        let store = SessionStore::new("/tmp/br0x-nope-missing/session.json");
        let _ = store.clear();
    }

    #[test]
    fn old_session_file_without_new_fields_loads() {
        let dir = std::env::temp_dir().join("br0x-test-session-old");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.json");
        std::fs::write(&path, r#"{"tabs":[{"id":4,"url":"https://a.example","order":0}]}"#)
            .unwrap();
        let store = SessionStore::new(&path);
        let session = store.load().unwrap();
        assert_eq!(session.tabs.len(), 1);
        assert_eq!(session.tabs[0].url, "https://a.example");
        assert!(!session.tabs[0].pinned);
        assert_eq!(session.tabs[0].scroll_y, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tab_without_url_is_rejected() {
        let dir = std::env::temp_dir().join("br0x-test-session-nourl");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.json");
        std::fs::write(&path, r#"{"tabs":[{"id":1,"title":"x"}]}"#).unwrap();
        assert!(SessionStore::new(&path).load().is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
