//! Address bar input resolution and search engines.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SearchEngine {
    #[default]
    DuckDuckGo,
    Google,
    Brave,
    Bing,
    Startpage,
    Wikipedia,
}

impl SearchEngine {
    pub const ALL: [SearchEngine; 6] = [
        SearchEngine::DuckDuckGo,
        SearchEngine::Google,
        SearchEngine::Brave,
        SearchEngine::Bing,
        SearchEngine::Startpage,
        SearchEngine::Wikipedia,
    ];

    pub fn name(self) -> &'static str {
        match self {
            SearchEngine::DuckDuckGo => "DuckDuckGo",
            SearchEngine::Google => "Google",
            SearchEngine::Brave => "Brave",
            SearchEngine::Bing => "Bing",
            SearchEngine::Startpage => "Startpage",
            SearchEngine::Wikipedia => "Wikipedia",
        }
    }

    /// Base search URL and query parameter name.
    pub fn form(self) -> (&'static str, &'static str) {
        match self {
            SearchEngine::DuckDuckGo => ("https://duckduckgo.com/", "q"),
            SearchEngine::Google => ("https://www.google.com/search", "q"),
            SearchEngine::Brave => ("https://search.brave.com/search", "q"),
            SearchEngine::Bing => ("https://www.bing.com/search", "q"),
            SearchEngine::Startpage => ("https://www.startpage.com/sp/search", "query"),
            SearchEngine::Wikipedia => ("https://en.wikipedia.org/w/index.php", "search"),
        }
    }

    pub fn query_url(self, query: &str) -> String {
        let (base, param) = self.form();
        format!("{base}?{param}={}", encode_query(query))
    }
}

/// True for search-engine result pages. Those flood history after every
/// query but make useless Frequent tiles, so the start page skips them.
pub fn is_search_results_url(url: &str) -> bool {
    let lower = url.to_lowercase();
    SearchEngine::ALL.iter().any(|engine| {
        let (base, param) = engine.form();
        lower.starts_with(&base.to_lowercase()) && lower.contains(&format!("{param}="))
    })
}
/// Percent-encode everything outside the unreserved set.
pub fn encode_query(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Turn address bar text into something loadable.
/// Real URLs and about: pass through, localhost gets http, single words and
/// phrases go to the chosen search engine, other hosts gain https.
pub fn resolve(input: &str, engine: SearchEngine) -> String {
    let t = input.trim();
    let lower = t.to_lowercase();
    // Spaces are illegal in a URI; the search branch escapes them via the
    // query encoder.
    let escaped = t.replace(' ', "%20");
    // Tolerate a mangled scheme: "https//host" and "https:/host" never
    // contain "://", so without this they would gain a second prefix and
    // load as "https://https//host".
    for (broken, fixed) in [("https//", "https://"), ("http//", "http://")] {
        if lower.starts_with(broken) {
            return format!("{fixed}{}", &escaped[broken.len()..]);
        }
    }
    for (broken, fixed) in [("https:/", "https://"), ("http:/", "http://")] {
        if lower.starts_with(broken) && !lower.starts_with(fixed) {
            return format!("{fixed}{}", &escaped[broken.len()..]);
        }
    }
    if lower.contains("://") || lower.starts_with("about:") {
        escaped
    } else if is_loopback(&lower) {
        format!("http://{escaped}")
    } else if t.contains(' ') || !t.contains('.') {
        engine.query_url(t)
    } else {
        format!("https://{escaped}")
    }
}

/// `localhost` as the host part, with an optional port, path or trailing dot.
fn is_loopback(lower: &str) -> bool {
    let host = lower.split([':', '/', '?', '#']).next().unwrap_or_default();
    host == "localhost" || host == "localhost."
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_reserved_characters() {
        assert_eq!(encode_query("a b&c=d"), "a%20b%26c%3Dd");
        assert_eq!(encode_query("c++ rust"), "c%2B%2B%20rust");
    }

    #[test]
    fn passes_urls_through() {
        assert_eq!(
            resolve("https://example.com/a b", SearchEngine::DuckDuckGo),
            "https://example.com/a%20b"
        );
        assert_eq!(resolve("about:blank", SearchEngine::Google), "about:blank");
        assert_eq!(resolve("file:///tmp/x.html", SearchEngine::Google), "file:///tmp/x.html");
    }

    #[test]
    fn passthrough_escapes_spaces_and_keeps_case() {
        assert_eq!(
            resolve("  HTTPS://Example.com/a b c  ", SearchEngine::Bing),
            "HTTPS://Example.com/a%20b%20c"
        );
    }

    #[test]
    fn scheme_detection_ignores_case() {
        assert_eq!(resolve("HTTPS://example.com", SearchEngine::Google), "HTTPS://example.com");
        assert_eq!(resolve("About:blank", SearchEngine::Google), "About:blank");
        assert_eq!(resolve("LOCALHOST:8080", SearchEngine::Google), "http://LOCALHOST:8080");
    }

    #[test]
    fn words_become_search() {
        assert_eq!(
            resolve("rust webkit", SearchEngine::DuckDuckGo),
            "https://duckduckgo.com/?q=rust%20webkit"
        );
        assert_eq!(
            resolve("rustlang", SearchEngine::Google),
            "https://www.google.com/search?q=rustlang"
        );
    }

    #[test]
    fn engine_choice_is_respected() {
        assert_eq!(
            resolve("borrow checker", SearchEngine::Wikipedia),
            "https://en.wikipedia.org/w/index.php?search=borrow%20checker"
        );
        assert_eq!(
            resolve("fast browser", SearchEngine::Startpage),
            "https://www.startpage.com/sp/search?query=fast%20browser"
        );
    }

    #[test]
    fn hosts_gain_https() {
        assert_eq!(resolve("example.com", SearchEngine::Bing), "https://example.com");
        assert_eq!(resolve("  sxch.dev  ", SearchEngine::Bing), "https://sxch.dev");
    }

    #[test]
    fn mangled_schemes_gain_exactly_one_prefix() {
        assert_eq!(resolve("https//example.com", SearchEngine::Bing), "https://example.com");
        assert_eq!(resolve("http//example.com/a", SearchEngine::Bing), "http://example.com/a");
        assert_eq!(resolve("https:/example.com", SearchEngine::Bing), "https://example.com");
        assert_eq!(resolve("HTTPS//Example.COM", SearchEngine::Bing), "https://Example.COM");
    }

    #[test]
    fn localhost_matches_on_boundary_only() {
        assert_eq!(resolve("localhost", SearchEngine::Bing), "http://localhost");
        assert_eq!(resolve("localhost:3000/x", SearchEngine::Bing), "http://localhost:3000/x");
        assert_eq!(
            resolve("localhosts", SearchEngine::DuckDuckGo),
            "https://duckduckgo.com/?q=localhosts"
        );
        assert_eq!(resolve("localhostfoo.com", SearchEngine::Bing), "https://localhostfoo.com");
    }

    #[test]
    fn localhost_gains_http() {
        assert_eq!(resolve("localhost:8080", SearchEngine::Bing), "http://localhost:8080");
    }

    #[test]
    fn localhost_with_trailing_dot_gains_http() {
        assert_eq!(resolve("localhost.", SearchEngine::Bing), "http://localhost.");
        assert_eq!(resolve("localhost.:3000/x", SearchEngine::Bing), "http://localhost.:3000/x");
    }

    #[test]
    fn localhost_path_spaces_are_escaped() {
        assert_eq!(
            resolve("localhost:3000/my file.html", SearchEngine::Bing),
            "http://localhost:3000/my%20file.html"
        );
    }

    #[test]
    fn ip_hosts_keep_the_https_default() {
        assert_eq!(resolve("192.168.0.1", SearchEngine::Bing), "https://192.168.0.1");
        assert_eq!(resolve("127.0.0.1:8000", SearchEngine::Bing), "https://127.0.0.1:8000");
    }

    #[test]
    fn result_pages_are_detected_per_engine() {
        assert!(is_search_results_url("https://www.google.com/search?q=rust&sei=abc"));
        assert!(is_search_results_url("https://duckduckgo.com/?q=rust&t=h_"));
        assert!(is_search_results_url("https://search.brave.com/search?q=rust"));
        assert!(is_search_results_url("https://www.bing.com/search?q=rust"));
        assert!(is_search_results_url("https://www.startpage.com/sp/search?query=rust"));
        assert!(is_search_results_url("https://en.wikipedia.org/w/index.php?search=rust"));
    }

    #[test]
    fn homepages_and_articles_are_not_results() {
        assert!(!is_search_results_url("https://www.google.com/"));
        assert!(!is_search_results_url("https://duckduckgo.com/"));
        assert!(!is_search_results_url("https://en.wikipedia.org/wiki/Rust"));
        assert!(!is_search_results_url("https://github.com/rust-lang/rust"));
    }
}
