//! Per-site tracker-blocker toggle. Unset domains default to enabled.
//! Missing file yields empty; corrupt JSON is quarantined.

use crate::json_file;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Lowercase, strip port, path, query and fragment.
/// `https://Example.COM:8080/x` becomes `example.com`.
pub fn normalize_domain(input: &str) -> String {
    let s = input.trim();
    let host = if let Some(pos) = s.find("://") {
        let rest = &s[pos + 3..];
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        &rest[..end]
    } else {
        let end = s.find(['/', '?', '#']).unwrap_or(s.len());
        &s[..end]
    };
    let host = host.trim();
    let without_port = if let Some(stripped) = host.strip_prefix('[') {
        // IPv6 literal like `[::1]` with optional `:port`.
        if let Some(end) = stripped.find(']') { &host[..=end + 1] } else { host }
    } else if host.matches(':').count() == 1 {
        host.split(':').next().unwrap_or(host)
    } else if host.contains(':') {
        // Bare IPv6 literal without brackets; leave intact.
        host
    } else {
        host
    };
    without_port.trim().trim_end_matches('.').to_lowercase()
}

/// File backed toggle store. Path is injected for testability.
#[derive(Debug, Clone, Default)]
pub struct ShieldStore {
    path: PathBuf,
    overrides: HashMap<String, bool>,
}

impl ShieldStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let overrides = load_overrides(&path);
        Self { path, overrides }
    }

    /// Set the blocker state for `domain`. Persists immediately.
    pub fn set(&mut self, domain: &str, enabled: bool) -> std::io::Result<()> {
        let domain = normalize_domain(domain);
        if domain.is_empty() {
            return Ok(());
        }
        self.overrides.insert(domain, enabled);
        self.persist()
    }

    /// Whether the blocker runs for `domain`. True when never set.
    pub fn is_enabled(&self, domain: &str) -> bool {
        let domain = normalize_domain(domain);
        self.overrides.get(&domain).copied().unwrap_or(true)
    }

    /// The explicit override for `domain`, if one is stored.
    pub fn get(&self, domain: &str) -> Option<bool> {
        let domain = normalize_domain(domain);
        self.overrides.get(&domain).copied()
    }

    /// Drop the override for `domain`. True when one existed.
    pub fn reset(&mut self, domain: &str) -> std::io::Result<bool> {
        let domain = normalize_domain(domain);
        if self.overrides.remove(&domain).is_some() {
            self.persist()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Every stored override, sorted by domain. Backs settings lists.
    pub fn overrides(&self) -> Vec<(String, bool)> {
        let mut out: Vec<(String, bool)> =
            self.overrides.iter().map(|(d, e)| (d.clone(), *e)).collect();
        out.sort();
        out
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn persist(&self) -> std::io::Result<()> {
        json_file::save(&self.path, &self.overrides)
    }
}

fn load_overrides(path: &Path) -> HashMap<String, bool> {
    match json_file::load(path) {
        Ok(raw) => normalize_loaded(raw),
        Err(e) => {
            if e.kind() == std::io::ErrorKind::InvalidData {
                match json_file::quarantine_corrupt(path) {
                    Ok(target) => {
                        eprintln!("br0x: corrupt shield moved to {}", target.display());
                    }
                    Err(e) => eprintln!("br0x: could not move corrupt shield: {e}"),
                }
            }
            HashMap::new()
        }
    }
}

fn normalize_loaded(raw: HashMap<String, bool>) -> HashMap<String, bool> {
    let mut out = HashMap::new();
    for (domain, enabled) in raw {
        let domain = normalize_domain(&domain);
        if !domain.is_empty() {
            out.insert(domain, enabled);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(name: &str) -> ShieldStore {
        let dir = std::env::temp_dir().join("br0x-test-shield").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        ShieldStore::new(dir.join("shield.json"))
    }

    #[test]
    fn unset_domain_defaults_to_enabled() {
        let store = store("default");
        assert!(store.is_enabled("example.com"));
        assert!(store.is_enabled("never-seen.example"));
    }

    #[test]
    fn set_and_read_toggle() {
        let mut store = store("toggle");
        store.set("example.com", false).unwrap();
        assert!(!store.is_enabled("example.com"));
        store.set("example.com", true).unwrap();
        assert!(store.is_enabled("example.com"));
        let _ = std::fs::remove_dir_all(store.path().parent().unwrap());
    }

    #[test]
    fn reset_restores_default() {
        let mut store = store("reset");
        store.set("example.com", false).unwrap();
        assert!(store.reset("example.com").unwrap());
        assert!(store.is_enabled("example.com"));
        assert!(!store.reset("example.com").unwrap());
        let _ = std::fs::remove_dir_all(store.path().parent().unwrap());
    }

    #[test]
    fn domain_normalization_applies() {
        let mut store = store("norm");
        store.set("Example.COM:8080", false).unwrap();
        assert!(!store.is_enabled("example.com"));
        assert!(!store.is_enabled("EXAMPLE.com:1234"));
        assert!(!store.is_enabled("https://example.com/page"));
        assert!(store.is_enabled("other.example"));
        let _ = std::fs::remove_dir_all(store.path().parent().unwrap());
    }

    #[test]
    fn normalization_strips_port_and_lowercases() {
        assert_eq!(normalize_domain("Example.COM:8080"), "example.com");
        assert_eq!(normalize_domain("  EXAMPLE.com.  "), "example.com");
        assert_eq!(normalize_domain("https://Example.COM:443/a?b#c"), "example.com");
    }

    #[test]
    fn persistence_round_trip() {
        let dir = std::env::temp_dir().join("br0x-test-shield").join("roundtrip");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("shield.json");
        {
            let mut store = ShieldStore::new(&path);
            store.set("Example.COM:8080", false).unwrap();
            store.set("other.example", true).unwrap();
        }
        let reopened = ShieldStore::new(&path);
        assert!(!reopened.is_enabled("example.com"));
        assert!(reopened.is_enabled("other.example"));
        assert!(reopened.is_enabled("unset.example"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_yields_default_enabled() {
        let store = store("missing");
        assert!(store.is_enabled("example.com"));
    }

    #[test]
    fn overrides_lists_stored_pairs_sorted() {
        let mut store = store("list");
        assert!(store.overrides().is_empty());
        assert_eq!(store.get("a.example"), None);
        store.set("b.example", false).unwrap();
        store.set("a.example", true).unwrap();
        assert_eq!(store.get("B.EXAMPLE:8080"), Some(false));
        assert_eq!(store.get("a.example"), Some(true));
        assert_eq!(store.get("unset.example"), None);
        assert_eq!(
            store.overrides(),
            vec![("a.example".to_string(), true), ("b.example".to_string(), false),]
        );
        let _ = std::fs::remove_dir_all(store.path().parent().unwrap());
    }
}
