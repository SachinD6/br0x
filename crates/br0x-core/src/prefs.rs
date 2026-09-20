//! User preferences. Small and explicit; grows only when a real setting lands.

use crate::json_file;
use crate::search::SearchEngine;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Prefs {
    #[serde(default)]
    pub engine: SearchEngine,
    /// Restore the previous tab set on launch. Off by default: a browser
    /// that silently reloads your last session is surprising.
    #[serde(default)]
    pub restore_session: bool,
}

/// File backed prefs. Path is injected for testability.
#[derive(Debug, Clone)]
pub struct PrefsStore {
    path: PathBuf,
}

impl PrefsStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Missing or unreadable file yields defaults. Prefs must never block startup.
    /// Corrupt JSON is moved aside so the next save starts clean.
    pub fn load(&self) -> Prefs {
        match json_file::load(&self.path) {
            Ok(prefs) => prefs,
            Err(e) => {
                if e.kind() == std::io::ErrorKind::InvalidData {
                    self.quarantine_corrupt();
                }
                Prefs::default()
            }
        }
    }

    fn quarantine_corrupt(&self) {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let name = self.path.file_name().unwrap_or_default().to_string_lossy();
        let target = self.path.with_file_name(format!("{name}.corrupt.{stamp}"));
        match std::fs::rename(&self.path, &target) {
            Ok(()) => eprintln!("br0x: corrupt prefs moved to {}", target.display()),
            Err(e) => eprintln!("br0x: could not move corrupt prefs: {e}"),
        }
    }

    pub fn save(&self, prefs: &Prefs) -> std::io::Result<()> {
        json_file::save(&self.path, prefs)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_yields_defaults() {
        let store = PrefsStore::new("/tmp/br0x-no-such-dir/prefs.json");
        assert_eq!(store.load(), Prefs::default());
    }

    #[test]
    fn roundtrips_engine_choice() {
        let dir = std::env::temp_dir().join("br0x-test-prefs");
        let store = PrefsStore::new(dir.join("prefs.json"));
        let prefs = Prefs { engine: SearchEngine::Brave, restore_session: true };
        store.save(&prefs).unwrap();
        assert_eq!(store.load(), prefs);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_prefs_without_restore_flag_load() {
        let dir = std::env::temp_dir().join("br0x-test-prefs-old");
        let store = PrefsStore::new(dir.join("prefs.json"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("prefs.json"), r#"{"engine":"Google"}"#).unwrap();
        let prefs = store.load();
        assert_eq!(prefs.engine, SearchEngine::Google);
        assert!(!prefs.restore_session);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_file_yields_defaults_and_is_quarantined() {
        let dir = std::env::temp_dir().join("br0x-test-prefs-corrupt");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("prefs.json");
        std::fs::write(&path, b"{not json").unwrap();
        let store = PrefsStore::new(&path);
        assert_eq!(store.load(), Prefs::default());
        assert!(!path.exists());
        let quarantined = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().starts_with("prefs.json.corrupt."));
        assert!(quarantined);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
