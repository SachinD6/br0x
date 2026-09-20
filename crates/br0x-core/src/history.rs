//! Browsing history on SQLite.

use crate::search::is_search_results_url;
use rusqlite::{Connection, params};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Visit {
    pub url: String,
    pub title: String,
    pub visited_at: i64,
}

pub struct History {
    conn: Connection,
}

impl History {
    pub fn open(path: impl AsRef<Path>) -> rusqlite::Result<Self> {
        if let Some(parent) = path.as_ref().parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// In-memory instance for tests.
    pub fn open_in_memory() -> rusqlite::Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> rusqlite::Result<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS visits (
                 id INTEGER PRIMARY KEY,
                 url TEXT NOT NULL,
                 title TEXT NOT NULL,
                 visited_at INTEGER NOT NULL,
                 visit_count INTEGER NOT NULL DEFAULT 1
             );
             CREATE INDEX IF NOT EXISTS visits_time ON visits(visited_at DESC);
             CREATE INDEX IF NOT EXISTS visits_url ON visits(url);",
        )?;
        // Existing databases lack the count column; ignore the error when it
        // already exists.
        let _ = conn
            .execute("ALTER TABLE visits ADD COLUMN visit_count INTEGER NOT NULL DEFAULT 1", []);
        let _ = conn.execute(
            "CREATE INDEX IF NOT EXISTS visits_count ON visits(visit_count DESC, visited_at DESC)",
            [],
        );
        Ok(Self { conn })
    }

    /// Record a visit. Repeated visits bump the count and move the URL to the
    /// top instead of piling up duplicates.
    pub fn record(&self, url: &str, title: &str, visited_at: i64) -> rusqlite::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let updated = tx.execute(
            "UPDATE visits SET title = ?2, visited_at = ?3, visit_count = visit_count + 1 WHERE url = ?1",
            params![url, title, visited_at],
        )?;
        if updated == 0 {
            tx.execute(
                "INSERT INTO visits (url, title, visited_at, visit_count) VALUES (?1, ?2, ?3, 1)",
                params![url, title, visited_at],
            )?;
        }
        tx.commit()
    }

    pub fn recent(&self, limit: usize) -> rusqlite::Result<Vec<Visit>> {
        let mut stmt = self.conn.prepare(
            "SELECT url, title, visited_at FROM visits ORDER BY visited_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok(Visit { url: row.get(0)?, title: row.get(1)?, visited_at: row.get(2)? })
        })?;
        rows.collect()
    }

    pub fn search(&self, needle: &str, limit: usize) -> rusqlite::Result<Vec<Visit>> {
        let pattern = like_pattern(needle);
        let mut stmt = self.conn.prepare(
            "SELECT url, title, visited_at FROM visits
             WHERE url LIKE ?1 ESCAPE '\\' OR title LIKE ?1 ESCAPE '\\'
             ORDER BY visited_at DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![pattern, limit as i64], |row| {
            Ok(Visit { url: row.get(0)?, title: row.get(1)?, visited_at: row.get(2)? })
        })?;
        rows.collect()
    }

    /// Most frequently visited real pages, for the start page. Internal
    /// pages (new tab, history, blank) and search-engine result pages
    /// never surface here. Over-fetches then filters, since result
    /// detection needs per-engine matching SQLite cannot do.
    pub fn top(&self, limit: usize) -> rusqlite::Result<Vec<Visit>> {
        let fetch = (limit * 6).max(limit + 10);
        let mut stmt = self.conn.prepare(
            "SELECT url, title, visited_at FROM visits
             WHERE url NOT LIKE 'br0x://%' AND url != 'about:blank' AND url NOT LIKE 'file://%'
             ORDER BY visit_count DESC, visited_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![fetch as i64], |row| {
            Ok(Visit { url: row.get(0)?, title: row.get(1)?, visited_at: row.get(2)? })
        })?;
        let mut out = Vec::with_capacity(limit);
        for visit in rows.flatten() {
            if !is_search_results_url(&visit.url) {
                out.push(visit);
                if out.len() >= limit {
                    break;
                }
            }
        }
        Ok(out)
    }

    pub fn clear(&self) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM visits", [])?;
        Ok(())
    }
}

/// Wrap the needle in `%` and escape LIKE wildcards so they match literally.
fn like_pattern(needle: &str) -> String {
    let mut out = String::with_capacity(needle.len() + 2);
    out.push('%');
    for c in needle.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_lists_recent_first() {
        let h = History::open_in_memory().unwrap();
        h.record("https://a.example", "A", 100).unwrap();
        h.record("https://b.example", "B", 200).unwrap();
        let visits = h.recent(10).unwrap();
        assert_eq!(visits.len(), 2);
        assert_eq!(visits[0].url, "https://b.example");
    }

    #[test]
    fn repeat_visits_do_not_duplicate() {
        let h = History::open_in_memory().unwrap();
        h.record("https://a.example", "A old", 100).unwrap();
        h.record("https://b.example", "B", 150).unwrap();
        h.record("https://a.example", "A new", 200).unwrap();
        let visits = h.recent(10).unwrap();
        assert_eq!(visits.len(), 2);
        assert_eq!(visits[0].title, "A new");
    }

    #[test]
    fn searches_url_and_title() {
        let h = History::open_in_memory().unwrap();
        h.record("https://rust-lang.org", "Rust", 1).unwrap();
        h.record("https://example.com", "Example", 2).unwrap();
        assert_eq!(h.search("rust", 10).unwrap().len(), 1);
        assert_eq!(h.search("Example", 10).unwrap().len(), 1);
        assert_eq!(h.search("nothing", 10).unwrap().len(), 0);
    }

    #[test]
    fn search_treats_wildcards_literally() {
        let h = History::open_in_memory().unwrap();
        h.record("https://shop.example/100%_off", "Sale", 1).unwrap();
        h.record("https://example.com/other", "Other", 2).unwrap();
        assert_eq!(h.search("100%", 10).unwrap().len(), 1);
        assert_eq!(h.search("%_", 10).unwrap().len(), 1);
        assert_eq!(h.search("\\", 10).unwrap().len(), 0);
        assert_eq!(h.search("_", 10).unwrap().len(), 1);
    }

    #[test]
    fn clears() {
        let h = History::open_in_memory().unwrap();
        h.record("https://a.example", "A", 1).unwrap();
        h.clear().unwrap();
        assert!(h.recent(10).unwrap().is_empty());
    }

    #[test]
    fn repeat_visits_rank_higher_in_top() {
        let h = History::open_in_memory().unwrap();
        h.record("https://once.example", "Once", 1).unwrap();
        h.record("https://often.example", "Often", 2).unwrap();
        h.record("https://often.example", "Often", 3).unwrap();
        h.record("https://often.example", "Often", 4).unwrap();
        let top = h.top(10).unwrap();
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].url, "https://often.example");
    }

    #[test]
    fn top_skips_search_result_pages() {
        let h = History::open_in_memory().unwrap();
        h.record("https://www.google.com/search?q=rust", "rust - Google Search", 1).unwrap();
        h.record("https://www.google.com/search?q=rust", "rust - Google Search", 2).unwrap();
        h.record("https://www.google.com/search?q=rust", "rust - Google Search", 3).unwrap();
        h.record("https://rust-lang.org", "Rust", 4).unwrap();
        let top = h.top(10).unwrap();
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].url, "https://rust-lang.org");
    }

    #[test]
    fn top_skips_internal_pages() {
        let h = History::open_in_memory().unwrap();
        h.record("br0x://history", "History", 1).unwrap();
        h.record("about:blank", "Blank", 2).unwrap();
        h.record("https://real.example", "Real", 3).unwrap();
        let top = h.top(10).unwrap();
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].url, "https://real.example");
    }
}
