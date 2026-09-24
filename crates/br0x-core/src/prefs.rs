//! User preferences. Small and explicit; grows only when a real setting lands.

use crate::json_file;
use crate::search::SearchEngine;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Shell chrome + internal page scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Appearance {
    /// Follow the OS theme.
    #[default]
    System,
    Light,
    Dark,
}

impl Appearance {
    pub const ALL: [Appearance; 3] = [Appearance::System, Appearance::Light, Appearance::Dark];

    pub fn name(self) -> &'static str {
        match self {
            Appearance::System => "System",
            Appearance::Light => "Light",
            Appearance::Dark => "Dark",
        }
    }

    /// Index into [`Self::ALL`], for settings rows.
    pub fn index(self) -> u32 {
        Self::ALL.iter().position(|a| *a == self).unwrap_or(0) as u32
    }
}

/// Idle time before a background tab is released. `Never` disables the
/// time-based release; pressure-driven parking still applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SleepTimeout {
    Min10,
    #[default]
    Min30,
    Hour,
    Never,
}

impl SleepTimeout {
    pub const ALL: [SleepTimeout; 4] =
        [SleepTimeout::Min10, SleepTimeout::Min30, SleepTimeout::Hour, SleepTimeout::Never];

    pub fn name(self) -> &'static str {
        match self {
            SleepTimeout::Min10 => "10 minutes",
            SleepTimeout::Min30 => "30 minutes",
            SleepTimeout::Hour => "1 hour",
            SleepTimeout::Never => "Never",
        }
    }

    /// Wall-clock seconds before sleep. `None` means never.
    pub fn secs(self) -> Option<u64> {
        match self {
            SleepTimeout::Min10 => Some(600),
            SleepTimeout::Min30 => Some(1800),
            SleepTimeout::Hour => Some(3600),
            SleepTimeout::Never => None,
        }
    }

    /// Index into [`Self::ALL`], for settings rows.
    pub fn index(self) -> u32 {
        Self::ALL.iter().position(|s| *s == self).unwrap_or(1) as u32
    }
}

/// The blocker default is on: missing key must mean protected.
fn blocker_on() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Prefs {
    #[serde(default)]
    pub engine: SearchEngine,
    /// Restore the previous tab set on launch. Off by default: a browser
    /// that silently reloads your last session is surprising.
    #[serde(default)]
    pub restore_session: bool,
    /// Shell chrome + internal page scheme. System by default.
    #[serde(default)]
    pub appearance: Appearance,
    /// Idle time before background tabs sleep. 30 minutes by default.
    #[serde(default)]
    pub sleep_timeout: SleepTimeout,
    /// Tracker blocker default for sites without a per-site override.
    #[serde(default = "blocker_on")]
    pub blocker_enabled: bool,
    /// Vertical tab sidebar visibility.
    #[serde(default)]
    pub sidebar_visible: bool,
    /// Sidebar collapsed to an icon rail.
    #[serde(default)]
    pub sidebar_collapsed: bool,
}

/// File backed prefs. Path is injected for testability.
#[derive(Debug, Clone)]
pub struct PrefsStore {
    path: PathBuf,
}

/// Struct `Default` must match the serde defaults above: the blocker ships
/// on, so a fresh profile is protected before any file exists.
impl Default for Prefs {
    fn default() -> Self {
        Self {
            engine: SearchEngine::default(),
            restore_session: false,
            appearance: Appearance::default(),
            sleep_timeout: SleepTimeout::default(),
            blocker_enabled: true,
            sidebar_visible: false,
            sidebar_collapsed: false,
        }
    }
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
        match json_file::quarantine_corrupt(&self.path) {
            Ok(target) => eprintln!("br0x: corrupt prefs moved to {}", target.display()),
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
    fn new_fields_default_when_absent() {
        assert_eq!(Prefs::default().appearance, Appearance::System);
        assert_eq!(Prefs::default().sleep_timeout, SleepTimeout::Min30);
        assert!(Prefs::default().blocker_enabled);
        assert!(!Prefs::default().sidebar_visible);
        assert!(!Prefs::default().sidebar_collapsed);
    }

    #[test]
    fn roundtrips_engine_choice() {
        let dir = std::env::temp_dir().join("br0x-test-prefs");
        let store = PrefsStore::new(dir.join("prefs.json"));
        let prefs =
            Prefs { engine: SearchEngine::Brave, restore_session: true, ..Default::default() };
        store.save(&prefs).unwrap();
        assert_eq!(store.load(), prefs);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn roundtrips_new_fields() {
        let dir = std::env::temp_dir().join("br0x-test-prefs-new");
        let _ = std::fs::remove_dir_all(&dir);
        let store = PrefsStore::new(dir.join("prefs.json"));
        let prefs = Prefs {
            engine: SearchEngine::Brave,
            restore_session: true,
            appearance: Appearance::Dark,
            sleep_timeout: SleepTimeout::Hour,
            blocker_enabled: false,
            sidebar_visible: true,
            sidebar_collapsed: true,
        };
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
    fn old_prefs_gain_new_defaults() {
        let dir = std::env::temp_dir().join("br0x-test-prefs-old2");
        let _ = std::fs::remove_dir_all(&dir);
        let store = PrefsStore::new(dir.join("prefs.json"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("prefs.json"), r#"{"engine":"Google","restore_session":true}"#)
            .unwrap();
        let prefs = store.load();
        assert_eq!(prefs.appearance, Appearance::System);
        assert_eq!(prefs.sleep_timeout, SleepTimeout::Min30);
        assert!(prefs.blocker_enabled);
        assert!(!prefs.sidebar_visible);
        assert!(!prefs.sidebar_collapsed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sleep_timeout_secs_match_menu_labels() {
        assert_eq!(SleepTimeout::Min10.secs(), Some(600));
        assert_eq!(SleepTimeout::Min30.secs(), Some(1800));
        assert_eq!(SleepTimeout::Hour.secs(), Some(3600));
        assert_eq!(SleepTimeout::Never.secs(), None);
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
