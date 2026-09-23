//! Saved-login store. File backend keeps 0600 permissions.
//! Callers depend on [`VaultStore`] so a future OS-keyring backend can
//! replace [`FileVaultStore`] without touching them. Secrets are never
//! logged; [`LoginEntry`]'s `Debug` impl redacts the secret.

use crate::json_file;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Trim, lowercase, strip trailing slashes.
/// `HTTPS://Example.COM/` becomes `https://example.com`.
pub fn normalize_origin(input: &str) -> String {
    let lower = input.trim().to_lowercase();
    let stripped = lower.trim_end_matches('/');
    if stripped.is_empty() { lower } else { stripped.to_owned() }
}

/// One saved login. The secret serializes to disk but never formats.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginEntry {
    pub origin: String,
    pub username: String,
    pub secret: String,
}

impl std::fmt::Debug for LoginEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginEntry")
            .field("origin", &self.origin)
            .field("username", &self.username)
            .field("secret", &"[redacted]")
            .finish()
    }
}

/// Storage seam. File and future keyring backends share this.
pub trait VaultStore {
    /// Insert or update the login for (`origin`, `username`).
    /// Rejects an empty secret.
    fn save(&mut self, origin: &str, username: &str, secret: &str) -> std::io::Result<()>;

    /// Logins for `origin`, sorted by username. Empty when none.
    fn credentials_for(&self, origin: &str) -> Vec<LoginEntry>;

    /// Remove one login. True when something was removed.
    fn remove(&mut self, origin: &str, username: &str) -> std::io::Result<bool>;

    /// Every origin with at least one login, sorted.
    fn all_origins(&self) -> Vec<String>;
}

/// JSON file backend. The file is always chmod 0600 on Unix.
#[derive(Debug, Clone, Default)]
pub struct FileVaultStore {
    path: PathBuf,
    entries: HashMap<String, HashMap<String, String>>,
}

impl FileVaultStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let entries = load_entries(&path);
        if path.exists() {
            let _ = enforce_0600(&path);
        }
        Self { path, entries }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn persist(&self) -> std::io::Result<()> {
        json_file::save(&self.path, &self.entries)?;
        enforce_0600(&self.path)
    }
}

impl VaultStore for FileVaultStore {
    fn save(&mut self, origin: &str, username: &str, secret: &str) -> std::io::Result<()> {
        if secret.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "secret must not be empty",
            ));
        }
        let origin = normalize_origin(origin);
        if origin.is_empty() || username.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "origin and username must not be empty",
            ));
        }
        // Usernames stay case sensitive; secrets are stored exactly.
        self.entries.entry(origin).or_default().insert(username.to_owned(), secret.to_owned());
        self.persist()
    }

    fn credentials_for(&self, origin: &str) -> Vec<LoginEntry> {
        let origin = normalize_origin(origin);
        match self.entries.get(&origin) {
            None => Vec::new(),
            Some(users) => {
                let mut out: Vec<LoginEntry> = users
                    .iter()
                    .map(|(username, secret)| LoginEntry {
                        origin: origin.clone(),
                        username: username.clone(),
                        secret: secret.clone(),
                    })
                    .collect();
                out.sort_by(|a, b| a.username.cmp(&b.username));
                out
            }
        }
    }

    fn remove(&mut self, origin: &str, username: &str) -> std::io::Result<bool> {
        let origin = normalize_origin(origin);
        if let Some(users) = self.entries.get_mut(&origin)
            && users.remove(username).is_some()
        {
            if users.is_empty() {
                self.entries.remove(&origin);
            }
            self.persist()?;
            return Ok(true);
        }
        Ok(false)
    }

    fn all_origins(&self) -> Vec<String> {
        let mut origins: Vec<String> = self.entries.keys().cloned().collect();
        origins.sort();
        origins
    }
}

fn load_entries(path: &Path) -> HashMap<String, HashMap<String, String>> {
    match json_file::load(path) {
        Ok(raw) => normalize_loaded(raw),
        Err(e) => {
            if e.kind() == std::io::ErrorKind::InvalidData {
                match json_file::quarantine_corrupt(path) {
                    Ok(target) => {
                        eprintln!("br0x: corrupt vault moved to {}", target.display());
                    }
                    Err(e) => eprintln!("br0x: could not move corrupt vault: {e}"),
                }
            }
            HashMap::new()
        }
    }
}

fn normalize_loaded(
    raw: HashMap<String, HashMap<String, String>>,
) -> HashMap<String, HashMap<String, String>> {
    let mut out: HashMap<String, HashMap<String, String>> = HashMap::new();
    for (origin, users) in raw {
        let origin = normalize_origin(&origin);
        if origin.is_empty() {
            continue;
        }
        let slot = out.entry(origin).or_default();
        for (username, secret) in users {
            if username.is_empty() || secret.is_empty() {
                continue;
            }
            slot.insert(username, secret);
        }
    }
    out.retain(|_, users| !users.is_empty());
    out
}

#[cfg(unix)]
fn enforce_0600(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn enforce_0600(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(name: &str) -> FileVaultStore {
        let dir = std::env::temp_dir().join("br0x-test-vault").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        FileVaultStore::new(dir.join("vault.json"))
    }

    fn cleanup(store: &FileVaultStore) {
        if let Some(parent) = store.path().parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn saves_and_reads_credentials() {
        let mut store = store("crud");
        VaultStore::save(&mut store, "https://example.com", "alice", "pw-test-1").unwrap();
        let creds = VaultStore::credentials_for(&store, "https://example.com");
        assert_eq!(creds.len(), 1);
        assert_eq!(creds[0].username, "alice");
        assert_eq!(creds[0].secret, "pw-test-1");
        cleanup(&store);
    }

    #[test]
    fn save_updates_existing_secret() {
        let mut store = store("update");
        VaultStore::save(&mut store, "https://example.com", "alice", "pw-test-1").unwrap();
        VaultStore::save(&mut store, "https://example.com", "alice", "pw-test-2").unwrap();
        let creds = VaultStore::credentials_for(&store, "https://example.com");
        assert_eq!(creds.len(), 1);
        assert_eq!(creds[0].secret, "pw-test-2");
        cleanup(&store);
    }

    #[test]
    fn removes_single_login() {
        let mut store = store("remove");
        VaultStore::save(&mut store, "https://example.com", "alice", "pw-test-1").unwrap();
        VaultStore::save(&mut store, "https://example.com", "bob", "pw-test-2").unwrap();
        assert!(VaultStore::remove(&mut store, "https://example.com", "alice").unwrap());
        let creds = VaultStore::credentials_for(&store, "https://example.com");
        assert_eq!(creds.len(), 1);
        assert_eq!(creds[0].username, "bob");
        assert!(!VaultStore::remove(&mut store, "https://example.com", "alice").unwrap());
        assert_eq!(VaultStore::all_origins(&store), vec!["https://example.com".to_string()]);
        assert!(VaultStore::remove(&mut store, "https://example.com", "bob").unwrap());
        assert!(VaultStore::all_origins(&store).is_empty());
        cleanup(&store);
    }

    #[test]
    fn lists_origins_sorted() {
        let mut store = store("origins");
        VaultStore::save(&mut store, "https://b.example", "alice", "pw-test-1").unwrap();
        VaultStore::save(&mut store, "https://a.example", "bob", "pw-test-2").unwrap();
        assert_eq!(
            VaultStore::all_origins(&store),
            vec!["https://a.example".to_string(), "https://b.example".to_string()]
        );
        cleanup(&store);
    }

    #[test]
    fn origin_normalization_applies() {
        let mut store = store("norm");
        VaultStore::save(&mut store, "HTTPS://Example.COM/", "alice", "pw-test-1").unwrap();
        assert_eq!(VaultStore::credentials_for(&store, "https://example.com").len(), 1);
        assert_eq!(VaultStore::credentials_for(&store, "https://EXAMPLE.com///").len(), 1);
        assert_eq!(VaultStore::all_origins(&store), vec!["https://example.com".to_string()]);
        assert_eq!(normalize_origin("HTTPS://Example.COM/"), "https://example.com");
        cleanup(&store);
    }

    #[test]
    fn save_rejects_empty_secret() {
        let mut store = store("empty-secret");
        let err = VaultStore::save(&mut store, "https://example.com", "alice", "").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(VaultStore::credentials_for(&store, "https://example.com").is_empty());
        cleanup(&store);
    }

    #[test]
    fn debug_redacts_secret() {
        let entry = LoginEntry {
            origin: "https://example.com".to_string(),
            username: "alice".to_string(),
            secret: "pw-test-1".to_string(),
        };
        let rendered = format!("{entry:?}");
        assert!(rendered.contains("alice"));
        assert!(!rendered.contains("pw-test-1"));
        assert!(rendered.contains("[redacted]"));
    }

    #[test]
    fn persistence_round_trip() {
        let dir = std::env::temp_dir().join("br0x-test-vault").join("roundtrip");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("vault.json");
        {
            let mut store = FileVaultStore::new(&path);
            VaultStore::save(&mut store, "HTTPS://Example.COM/", "alice", "pw-test-1").unwrap();
        }
        let reopened = FileVaultStore::new(&path);
        let creds = VaultStore::credentials_for(&reopened, "https://example.com");
        assert_eq!(creds.len(), 1);
        assert_eq!(creds[0].username, "alice");
        assert_eq!(creds[0].secret, "pw-test-1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn vault_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let mut store = store("perms");
        VaultStore::save(&mut store, "https://example.com", "alice", "pw-test-1").unwrap();
        let mode = std::fs::metadata(store.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        cleanup(&store);
    }
}
