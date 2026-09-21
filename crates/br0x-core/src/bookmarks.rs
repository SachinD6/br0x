//! Bookmarks: user pinned pages. Small JSON list, atomic writes.
//! Shared by the shell star button and the start page section.

use crate::json_file;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// One saved page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bookmark {
    pub url: String,
    pub title: String,
}

/// File backed store. Path is injected for testability.
#[derive(Debug, Clone)]
pub struct BookmarkStore {
    path: PathBuf,
}

impl BookmarkStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Missing file yields an empty list. Bookmarks must never block startup.
    /// Corrupt JSON is moved aside so the next save cannot overwrite it.
    pub fn load(&self) -> Vec<Bookmark> {
        match json_file::load(&self.path) {
            Ok(bookmarks) => bookmarks,
            Err(e) => {
                if e.kind() == std::io::ErrorKind::InvalidData {
                    self.quarantine_corrupt();
                }
                Vec::new()
            }
        }
    }

    fn quarantine_corrupt(&self) {
        match json_file::quarantine_corrupt(&self.path) {
            Ok(target) => eprintln!("br0x: corrupt bookmarks moved to {}", target.display()),
            Err(e) => eprintln!("br0x: could not move corrupt bookmarks: {e}"),
        }
    }

    pub fn save(&self, bookmarks: &[Bookmark]) -> std::io::Result<()> {
        json_file::save(&self.path, bookmarks)
    }

    /// Add or update by URL. Returns true when a new entry was added.
    pub fn upsert(bookmarks: &mut Vec<Bookmark>, url: &str, title: &str) -> bool {
        if let Some(found) = bookmarks.iter_mut().find(|b| b.url == url) {
            if !title.is_empty() {
                found.title = title.to_owned();
            }
            false
        } else {
            bookmarks.push(Bookmark {
                url: url.to_owned(),
                title: if title.is_empty() { url.to_owned() } else { title.to_owned() },
            });
            true
        }
    }

    /// Remove by URL. Returns true when something was removed.
    pub fn remove(bookmarks: &mut Vec<Bookmark>, url: &str) -> bool {
        let before = bookmarks.len();
        bookmarks.retain(|b| b.url != url);
        bookmarks.len() != before
    }

    pub fn is_bookmarked(bookmarks: &[Bookmark], url: &str) -> bool {
        bookmarks.iter().any(|b| b.url == url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(name: &str) -> BookmarkStore {
        let dir = std::env::temp_dir().join("br0x-test-bookmarks").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        BookmarkStore::new(dir.join("bookmarks.json"))
    }

    #[test]
    fn missing_file_yields_empty() {
        assert!(store("missing").load().is_empty());
    }

    #[test]
    fn upsert_adds_then_updates_title() {
        let mut list = Vec::new();
        assert!(BookmarkStore::upsert(&mut list, "https://a.example", "A"));
        assert!(!BookmarkStore::upsert(&mut list, "https://a.example", "A new"));
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].title, "A new");
        assert!(BookmarkStore::is_bookmarked(&list, "https://a.example"));
    }

    #[test]
    fn remove_drops_entry() {
        let mut list = Vec::new();
        BookmarkStore::upsert(&mut list, "https://a.example", "A");
        assert!(BookmarkStore::remove(&mut list, "https://a.example"));
        assert!(!BookmarkStore::remove(&mut list, "https://a.example"));
        assert!(list.is_empty());
    }

    #[test]
    fn roundtrips_through_disk() {
        let store = store("roundtrip");
        let mut list = Vec::new();
        BookmarkStore::upsert(&mut list, "https://a.example", "A");
        store.save(&list).unwrap();
        assert_eq!(store.load(), list);
    }

    #[test]
    fn corrupt_file_is_quarantined_not_overwritten() {
        let dir = std::env::temp_dir().join("br0x-test-bookmarks").join("corrupt");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bookmarks.json");
        std::fs::write(&path, b"[not json").unwrap();
        let store = BookmarkStore::new(&path);
        assert!(store.load().is_empty());
        assert!(!path.exists());
        let quarantined: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(quarantined.len(), 1);
        assert!(quarantined[0].starts_with("bookmarks.json.corrupt."));
        assert_eq!(std::fs::read(dir.join(&quarantined[0])).unwrap(), b"[not json");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
