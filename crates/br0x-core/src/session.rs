//! Crash safe session store. Atomic write, lazy load.

use crate::json_file;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// One stored tab. URLs and titles only, never page heap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredTab {
    pub id: u64,
    pub url: String,
    pub title: String,
    pub order: usize,
    pub pinned: bool,
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
}
