//! Subresource blocker. Predicate only, no I/O in hot path.

/// Navigation stays allowed. Only subresources are filtered by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadKind {
    Navigation,
    Subresource,
}

/// Small substring matcher for v0.1.
/// Later this seam gains an adblock-rust plus WebKit filter adapter
/// without changing callers.
#[derive(Debug, Default)]
pub struct Blocker {
    rules: Vec<String>,
    blocked_total: u64,
}

impl Blocker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pure predicate. Never blocks top level navigation in v0.1.
    pub fn should_block(&self, url: &str, kind: LoadKind) -> bool {
        if kind == LoadKind::Navigation {
            return false;
        }
        let hay = url.to_lowercase();
        self.rules.iter().any(|r| hay.contains(r))
    }

    /// Replace the rule set. Returns compiled rule count.
    /// A bad blob keeps old rules and returns an error.
    pub fn update_rules(&mut self, blob: &[u8]) -> Result<usize, String> {
        let text = std::str::from_utf8(blob).map_err(|e| e.to_string())?;
        let rules = parse_rules(text);
        if rules.is_empty() && !text.trim().is_empty() {
            return Err("no usable rules".into());
        }
        let n = rules.len();
        self.rules = rules;
        Ok(n)
    }

    pub fn count(&self) -> u64 {
        self.blocked_total
    }

    /// Record a block for stats. Called by the shell after `should_block`.
    pub fn note_blocked(&mut self) {
        self.blocked_total += 1;
    }
}

fn parse_rules(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('!') && !l.starts_with('#'))
        .map(|l| l.to_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_subresource_but_not_navigation() {
        let mut b = Blocker::new();
        b.update_rules(b"ads-tracker\n").unwrap();
        assert!(b.should_block("https://x/ads-tracker.js", LoadKind::Subresource));
        assert!(!b.should_block("https://x/ads-tracker.js", LoadKind::Navigation));
    }

    #[test]
    fn bad_blob_keeps_old_rules() {
        let mut b = Blocker::new();
        b.update_rules(b"good-rule\n").unwrap();
        assert!(b.update_rules(b"!!!\n???\n").is_ok());
    }

    #[test]
    fn counts_blocks() {
        let mut b = Blocker::new();
        b.note_blocked();
        assert_eq!(b.count(), 1);
    }

    #[test]
    fn unreadable_blob_keeps_old_rules() {
        let mut b = Blocker::new();
        b.update_rules(b"ads.example\n").unwrap();
        assert!(b.update_rules(&[0xff, 0xfe]).is_err());
        assert!(b.should_block("https://ads.example/x.js", LoadKind::Subresource));
    }

    #[test]
    fn empty_blob_clears_rules() {
        let mut b = Blocker::new();
        b.update_rules(b"ads.example\n").unwrap();
        assert_eq!(b.update_rules(b"\n  \n").unwrap(), 0);
        assert!(!b.should_block("https://ads.example/x.js", LoadKind::Subresource));
    }
}
