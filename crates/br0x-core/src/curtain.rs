//! Per-site hidden-element store. Domain maps to CSS selectors.
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

/// File backed store. Path is injected for testability.
#[derive(Debug, Clone, Default)]
pub struct CurtainStore {
    path: PathBuf,
    entries: HashMap<String, Vec<String>>,
}

impl CurtainStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let entries = load_entries(&path);
        Self { path, entries }
    }

    /// Hide `selector` on `domain`. True when a new selector was added.
    pub fn hide(&mut self, domain: &str, selector: &str) -> std::io::Result<bool> {
        let domain = normalize_domain(domain);
        let selector = selector.trim().to_owned();
        if domain.is_empty() || selector.is_empty() {
            return Ok(false);
        }
        let list = self.entries.entry(domain).or_default();
        if list.contains(&selector) {
            return Ok(false);
        }
        list.push(selector);
        self.persist()?;
        Ok(true)
    }

    /// Stop hiding `selector` on `domain`. True when something was removed.
    pub fn unhide(&mut self, domain: &str, selector: &str) -> std::io::Result<bool> {
        let domain = normalize_domain(domain);
        let selector = selector.trim();
        if let Some(list) = self.entries.get_mut(&domain) {
            let before = list.len();
            list.retain(|s| s != selector);
            if list.len() != before {
                if list.is_empty() {
                    self.entries.remove(&domain);
                }
                self.persist()?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Selectors hidden on `domain`. Empty when none.
    pub fn selectors_for(&self, domain: &str) -> Vec<String> {
        let domain = normalize_domain(domain);
        self.entries.get(&domain).cloned().unwrap_or_default()
    }

    /// Forget every selector for `domain`. True when the domain existed.
    pub fn clear_domain(&mut self, domain: &str) -> std::io::Result<bool> {
        let domain = normalize_domain(domain);
        if self.entries.remove(&domain).is_some() {
            self.persist()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Every domain with at least one selector, sorted.
    pub fn all_domains(&self) -> Vec<String> {
        let mut domains: Vec<String> = self.entries.keys().cloned().collect();
        domains.sort();
        domains
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn persist(&self) -> std::io::Result<()> {
        json_file::save(&self.path, &self.entries)
    }
}

fn load_entries(path: &Path) -> HashMap<String, Vec<String>> {
    match json_file::load(path) {
        Ok(raw) => normalize_loaded(raw),
        Err(e) => {
            if e.kind() == std::io::ErrorKind::InvalidData {
                match json_file::quarantine_corrupt(path) {
                    Ok(target) => {
                        eprintln!("br0x: corrupt curtain moved to {}", target.display());
                    }
                    Err(e) => eprintln!("br0x: could not move corrupt curtain: {e}"),
                }
            }
            HashMap::new()
        }
    }
}

fn normalize_loaded(raw: HashMap<String, Vec<String>>) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for (domain, selectors) in raw {
        let domain = normalize_domain(&domain);
        if domain.is_empty() {
            continue;
        }
        let list = out.entry(domain).or_default();
        for selector in selectors {
            let selector = selector.trim().to_owned();
            if !selector.is_empty() && !list.contains(&selector) {
                list.push(selector);
            }
        }
    }
    out.retain(|_, v| !v.is_empty());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(name: &str) -> CurtainStore {
        let dir = std::env::temp_dir().join("br0x-test-curtain").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        CurtainStore::new(dir.join("curtain.json"))
    }

    #[test]
    fn adds_and_lists_selectors() {
        let mut store = store("add");
        assert!(store.hide("example.com", ".ad-banner").unwrap());
        assert!(store.hide("example.com", "#popup").unwrap());
        // Duplicate hide is a no-op.
        assert!(!store.hide("example.com", ".ad-banner").unwrap());
        let selectors = store.selectors_for("example.com");
        assert_eq!(selectors, vec![".ad-banner".to_string(), "#popup".to_string()]);
        let _ = std::fs::remove_dir_all(store.path().parent().unwrap());
    }

    #[test]
    fn removes_selectors_and_prunes_domain() {
        let mut store = store("remove");
        store.hide("example.com", ".ad").unwrap();
        assert!(store.unhide("example.com", ".ad").unwrap());
        assert!(store.selectors_for("example.com").is_empty());
        assert!(store.all_domains().is_empty());
        // Second remove is a no-op.
        assert!(!store.unhide("example.com", ".ad").unwrap());
        let _ = std::fs::remove_dir_all(store.path().parent().unwrap());
    }

    #[test]
    fn lookup_normalizes_domain() {
        let mut store = store("lookup-norm");
        store.hide("Example.COM:8080", ".ad").unwrap();
        assert_eq!(store.selectors_for("example.com"), vec![".ad".to_string()]);
        assert_eq!(store.selectors_for("EXAMPLE.com:1234"), vec![".ad".to_string()]);
        assert_eq!(store.selectors_for("https://example.com/page"), vec![".ad".to_string()]);
        let _ = std::fs::remove_dir_all(store.path().parent().unwrap());
    }

    #[test]
    fn normalization_strips_port_and_lowercases() {
        assert_eq!(normalize_domain("Example.COM:8080"), "example.com");
        assert_eq!(normalize_domain("  EXAMPLE.com.  "), "example.com");
        assert_eq!(normalize_domain("https://Example.COM:443/a?b#c"), "example.com");
        assert_eq!(normalize_domain("example.com/popup"), "example.com");
    }

    #[test]
    fn clear_domain_and_all_domains() {
        let mut store = store("clear");
        store.hide("b.example", ".x").unwrap();
        store.hide("a.example", ".y").unwrap();
        assert_eq!(store.all_domains(), vec!["a.example".to_string(), "b.example".to_string()]);
        assert!(store.clear_domain("A.EXAMPLE:1234").unwrap());
        assert_eq!(store.all_domains(), vec!["b.example".to_string()]);
        assert!(!store.clear_domain("a.example").unwrap());
        let _ = std::fs::remove_dir_all(store.path().parent().unwrap());
    }

    #[test]
    fn persistence_round_trip() {
        let dir = std::env::temp_dir().join("br0x-test-curtain").join("roundtrip");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("curtain.json");
        {
            let mut store = CurtainStore::new(&path);
            store.hide("Example.COM:8080", ".ad").unwrap();
            store.hide("other.example", "#banner").unwrap();
        }
        let reopened = CurtainStore::new(&path);
        assert_eq!(reopened.selectors_for("example.com"), vec![".ad".to_string()]);
        assert_eq!(reopened.selectors_for("other.example"), vec!["#banner".to_string()]);
        assert_eq!(
            reopened.all_domains(),
            vec!["example.com".to_string(), "other.example".to_string()]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_yields_empty() {
        let store = store("missing");
        assert!(store.selectors_for("example.com").is_empty());
        assert!(store.all_domains().is_empty());
    }
}
