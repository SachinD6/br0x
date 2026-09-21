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
        // already exists. A successful ALTER means the rows predate visit
        // counting and may repeat a URL, so collapse them once.
        let migrated = conn
            .execute("ALTER TABLE visits ADD COLUMN visit_count INTEGER NOT NULL DEFAULT 1", [])
            .is_ok();
        if migrated {
            collapse_legacy_duplicates(&conn);
        }
        let _ = conn.execute(
            "CREATE INDEX IF NOT EXISTS visits_count ON visits(visit_count DESC, visited_at DESC)",
            [],
        );
        // Cheap disks and sudden power loss must never corrupt history:
        // NORMAL under WAL only risks losing the last commits.
        let _ = conn.execute("PRAGMA synchronous=NORMAL", []);
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

    /// Search history. Address-bar needles are usually URL prefixes, so a
    /// prefix pass runs first; when it fills `limit` the full url+title
    /// substring scan is skipped.
    pub fn search(&self, needle: &str, limit: usize) -> rusqlite::Result<Vec<Visit>> {
        // Stored URLs always start with a scheme, so a needle that cannot
        // open one ("rust", "zzz-nothing") can never prefix-match: running
        // the extra scan would only double the cost of ordinary typing.
        let mut hits = if needle_can_prefix_match(needle) {
            self.search_url_prefix(needle, limit)?
        } else {
            Vec::new()
        };
        if hits.len() >= limit {
            return Ok(hits);
        }
        let pattern = like_pattern(needle);
        let mut stmt = self.conn.prepare(
            "SELECT url, title, visited_at FROM visits
             WHERE url LIKE ?1 ESCAPE '\\' OR title LIKE ?1 ESCAPE '\\'
             ORDER BY visited_at DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![pattern, limit as i64], |row| {
            Ok(Visit { url: row.get(0)?, title: row.get(1)?, visited_at: row.get(2)? })
        })?;
        for row in rows {
            let visit = row?;
            // Prefix hits already cover these rows and stay ranked first.
            if hits.contains(&visit) {
                continue;
            }
            hits.push(visit);
            if hits.len() >= limit {
                break;
            }
        }
        Ok(hits)
    }

    /// Visits whose URL starts with `needle`, newest first. The visited_at
    /// index order plus LIMIT lets the scan stop as soon as it has `limit`
    /// matches, which is the cheap path this exists for.
    fn search_url_prefix(&self, needle: &str, limit: usize) -> rusqlite::Result<Vec<Visit>> {
        let mut stmt = self.conn.prepare(
            "SELECT url, title, visited_at FROM visits
             WHERE url LIKE ?1 ESCAPE '\\'
             ORDER BY visited_at DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![like_prefix_pattern(needle), limit as i64], |row| {
            Ok(Visit { url: row.get(0)?, title: row.get(1)?, visited_at: row.get(2)? })
        })?;
        rows.collect()
    }

    /// Most frequently visited real pages, for the start page. Internal
    /// pages (new tab, history, blank) and search-engine result pages
    /// never surface here. Over-fetches then filters, since result
    /// detection needs per-engine matching SQLite cannot do.
    pub fn top(&self, limit: usize) -> rusqlite::Result<Vec<Visit>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let fetch = limit.saturating_mul(6).max(limit.saturating_add(10));
        let mut stmt = self.conn.prepare(
            "SELECT url, title, visited_at FROM visits
             WHERE url NOT LIKE 'br0x://%' AND url != 'about:blank' AND url NOT LIKE 'file://%'
             ORDER BY visit_count DESC, visited_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![fetch as i64], |row| {
            Ok(Visit { url: row.get(0)?, title: row.get(1)?, visited_at: row.get(2)? })
        })?;
        // No capacity hint: `limit` is caller supplied and may be absurd,
        // while the result is capped by the rows the query returns.
        let mut out = Vec::new();
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

    /// Total stored visits, for honest "latest N of M" labels.
    pub fn count(&self) -> rusqlite::Result<i64> {
        self.conn.query_row("SELECT COUNT(*) FROM visits", [], |row| row.get(0))
    }
}

/// Older versions stored one row per visit. Collapse them to one row per URL,
/// counting the visits, so reads stop repeating a URL.
fn collapse_legacy_duplicates(conn: &Connection) {
    let _ = conn.execute_batch(
        "UPDATE visits SET visit_count = (SELECT COUNT(*) FROM visits v WHERE v.url = visits.url)
             WHERE id IN (SELECT MAX(id) FROM visits GROUP BY url);
         DELETE FROM visits WHERE id NOT IN (SELECT MAX(id) FROM visits GROUP BY url);",
    );
}

/// Wrap the needle in `%` and escape LIKE wildcards so they match literally.
fn like_pattern(needle: &str) -> String {
    let mut out = String::with_capacity(needle.len() + 2);
    out.push('%');
    escape_like(needle, &mut out);
    out.push('%');
    out
}

/// Anchor the needle to the start of the URL, escaping LIKE wildcards so they
/// match literally.
fn like_prefix_pattern(needle: &str) -> String {
    let mut out = String::with_capacity(needle.len() + 1);
    escape_like(needle, &mut out);
    out.push('%');
    out
}

/// Whether `needle` could open a stored URL, i.e. is a prefix of one.
/// Stored URLs always start with a scheme, so plain words ("rust") can
/// never match the anchored pass — skip it and avoid a second scan.
fn needle_can_prefix_match(needle: &str) -> bool {
    let lower = needle.to_lowercase();
    lower.contains("://") || "https://".starts_with(&lower) || "http://".starts_with(&lower)
}

/// Append `needle` to `out`, backslashing LIKE wildcards so they match
/// literally under `ESCAPE '\'`.
fn escape_like(needle: &str, out: &mut String) {
    for c in needle.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
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
    fn search_ranks_url_prefix_matches_first() {
        let h = History::open_in_memory().unwrap();
        // Newer row matches only by title; the older row is a URL prefix.
        h.record("https://example.com/rust", "Rust at https://rust-lang.org", 300).unwrap();
        h.record("https://rust-lang.org/book", "Rust book", 100).unwrap();
        let hits = h.search("https://rust", 10).unwrap();
        assert_eq!(hits.len(), 2);
        // Older prefix hit outranks the newer substring-only hit.
        assert_eq!(hits[0].url, "https://rust-lang.org/book");
        assert_eq!(hits[1].url, "https://example.com/rust");
    }

    #[test]
    fn search_falls_back_to_substring_matches() {
        let h = History::open_in_memory().unwrap();
        h.record("https://example.com/a/rust", "A", 1).unwrap();
        h.record("https://example.com/b", "Rust notes", 2).unwrap();
        let hits = h.search("rust", 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].url, "https://example.com/b");
        assert_eq!(hits[1].url, "https://example.com/a/rust");
    }

    #[test]
    fn search_respects_limit() {
        let h = History::open_in_memory().unwrap();
        for i in 0..5 {
            h.record(&format!("https://prefix.example/{i}"), "Prefix", 100 + i).unwrap();
        }
        h.record("https://other.example/0", "See https://prefix.example", 500).unwrap();
        h.record("https://other.example/1", "See https://prefix.example", 600).unwrap();
        // The prefix pass alone fills the limit.
        let hits = h.search("https://prefix", 3).unwrap();
        assert_eq!(hits.len(), 3);
        assert!(hits.iter().all(|v| v.url.starts_with("https://prefix.example/")));
        // Merged prefix and substring hits are truncated to the limit.
        let hits = h.search("https://prefix", 6).unwrap();
        assert_eq!(hits.len(), 6);
        assert!(hits[..5].iter().all(|v| v.url.starts_with("https://prefix.example/")));
        assert_eq!(hits[5].url, "https://other.example/1");
    }

    #[test]
    fn search_treats_wildcards_literally_in_prefix_pass() {
        let h = History::open_in_memory().unwrap();
        h.record("https://100%_off.example/one", "Sale", 1).unwrap();
        h.record("https://100xoff.example/two", "Sale", 2).unwrap();
        h.record("https://example.com/100%_off", "Sale", 3).unwrap();
        // Anchored pass: only the URL that literally starts with it.
        let hits = h.search("https://100%_off", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "https://100%_off.example/one");
        // Scheme-less needles use the substring pass, newest first, with
        // `%` literal so `100xoff.example` never matches.
        let hits = h.search("100%_off", 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].url, "https://example.com/100%_off");
        assert_eq!(hits[1].url, "https://100%_off.example/one");
        // `_` is literal: nothing contains "100_".
        assert!(h.search("100_", 10).unwrap().is_empty());
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
    fn scheme_less_needles_skip_the_prefix_pass() {
        assert!(!needle_can_prefix_match("rust"));
        assert!(!needle_can_prefix_match("example.com"));
        assert!(needle_can_prefix_match("https://rust-lang.org"));
        assert!(needle_can_prefix_match("http"));
        assert!(needle_can_prefix_match("h"));
    }

    #[test]
    fn counts_rows() {
        let h = History::open_in_memory().unwrap();
        assert_eq!(h.count().unwrap(), 0);
        h.record("https://a.example", "A", 1).unwrap();
        h.record("https://a.example", "A", 2).unwrap();
        assert_eq!(h.count().unwrap(), 1);
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

    #[test]
    fn top_of_zero_returns_nothing() {
        let h = History::open_in_memory().unwrap();
        h.record("https://a.example", "A", 1).unwrap();
        assert!(h.top(0).unwrap().is_empty());
    }

    #[test]
    fn top_with_huge_limit_does_not_overflow() {
        let h = History::open_in_memory().unwrap();
        h.record("https://a.example", "A", 1).unwrap();
        assert_eq!(h.top(usize::MAX).unwrap().len(), 1);
    }

    #[test]
    fn legacy_duplicate_rows_collapse_on_open() {
        let dir = std::env::temp_dir().join("br0x-test-history-legacy");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE visits (
                     id INTEGER PRIMARY KEY,
                     url TEXT NOT NULL,
                     title TEXT NOT NULL,
                     visited_at INTEGER NOT NULL
                 );
                 INSERT INTO visits (url, title, visited_at) VALUES ('https://old.example', 'Old', 5);
                 INSERT INTO visits (url, title, visited_at) VALUES ('https://old.example', 'Older', 3);
                 INSERT INTO visits (url, title, visited_at) VALUES ('https://old.example', 'Newest', 7);
                 INSERT INTO visits (url, title, visited_at) VALUES ('https://once.example', 'Once', 9);",
            )
            .unwrap();
        }
        let h = History::open(&path).unwrap();
        let recent = h.recent(10).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent.iter().filter(|v| v.url == "https://old.example").count(), 1);
        let counted: i64 = Connection::open(&path)
            .unwrap()
            .query_row(
                "SELECT visit_count FROM visits WHERE url = 'https://old.example'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(counted, 3);
        // The three legacy rows count as three visits, so the URL outranks a
        // single visit.
        assert_eq!(h.top(10).unwrap()[0].url, "https://old.example");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
