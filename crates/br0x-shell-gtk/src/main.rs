//! br0x shell: GTK4 + WebKitGTK 6.0 window.
//! Thin UI over br0x-core policy. All timing rules live in core.

mod bench;
use adw::prelude::*;
use br0x_core::bookmarks::{Bookmark, BookmarkStore};
use br0x_core::history::{History, Visit};
use br0x_core::json_file;
use br0x_core::policy;
use br0x_core::prefs::{Prefs, PrefsStore};
use br0x_core::search::{self, SearchEngine};
use br0x_core::session::{Session, SessionStore, StoredTab};
use br0x_core::tab::{Action, TabId, TabSnapshot};
use gtk4::gio;
use gtk4::glib;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::Instant;
use webkit6::prelude::*;

static FILTER_CACHE: OnceLock<CachedFilter> = OnceLock::new();

/// Main-thread-only holder so the compiled filter can live in a static.
struct CachedFilter(webkit6::UserContentFilter);
// SAFETY: only touched on the GTK main thread.
unsafe impl Send for CachedFilter {}
unsafe impl Sync for CachedFilter {}

/// Minimal first party tracker block list in WebKit content extension JSON.
const BASE_FILTER_JSON: &str = r#"[
{"action":{"type":"block"},"trigger":{"url-filter":"doubleclick\\.net"}},
{"action":{"type":"block"},"trigger":{"url-filter":"googlesyndication\\.com"}},
{"action":{"type":"block"},"trigger":{"url-filter":"google-analytics\\.com"}},
{"action":{"type":"block"},"trigger":{"url-filter":"facebook\\.net/tr"}},
{"action":{"type":"block"},"trigger":{"url-filter":"hotjar\\.com"}}
]"#;

/// Per tab bookkeeping the policy needs.
struct TabMeta {
    id: TabId,
    last_active: Instant,
    restored_at: Option<Instant>,
    pinned: bool,
    keep_alive: bool,
    parked: bool,
    sleeping: bool,
    pending_url: Option<String>,
    reader_src: Option<String>,
    has_password: bool,
}

impl TabMeta {
    fn fresh(id: TabId) -> Self {
        Self {
            id,
            last_active: Instant::now(),
            restored_at: None,
            pinned: false,
            keep_alive: false,
            parked: false,
            sleeping: false,
            pending_url: None,
            reader_src: None,
            has_password: false,
        }
    }

    fn snapshot(&self, audible: bool) -> TabSnapshot {
        TabSnapshot {
            id: self.id,
            last_active_secs_ago: self.last_active.elapsed().as_secs(),
            audible,
            capturing: false,
            downloading: false,
            form_dirty: false,
            pinned: self.pinned,
            keep_alive: self.keep_alive,
            restored_secs_ago: self.restored_at.map(|t| t.elapsed().as_secs()),
        }
    }
}

/// One open tab: policy state plus the widgets that carry it.
struct TabEntry {
    meta: TabMeta,
    view: webkit6::WebView,
    page: adw::TabPage,
}

#[derive(Default)]
struct Tabs {
    entries: Vec<TabEntry>,
    next_id: u64,
    closed: Vec<String>,
}

impl Tabs {
    fn new() -> Self {
        Self { entries: Vec::new(), next_id: 1, closed: Vec::new() }
    }

    fn new_id(&mut self) -> TabId {
        let id = TabId(self.next_id);
        self.next_id += 1;
        id
    }

    fn push(&mut self, meta: TabMeta, view: webkit6::WebView, page: adw::TabPage) {
        self.entries.push(TabEntry { meta, view, page });
    }

    fn entry_mut(&mut self, page: &adw::TabPage) -> Option<&mut TabEntry> {
        self.entries.iter_mut().find(|e| &e.page == page)
    }

    fn entry_by_id(&mut self, id: TabId) -> Option<&mut TabEntry> {
        self.entries.iter_mut().find(|e| e.meta.id == id)
    }

    fn remove_page(&mut self, page: &adw::TabPage) {
        self.entries.retain(|e| &e.page != page);
    }
}

/// XDG data home via glib, with a /tmp fallback: a missing HOME must never
/// stop the browser from starting.
fn data_home() -> PathBuf {
    xdg_home(glib::user_data_dir(), ".local/share")
}

fn cache_home() -> PathBuf {
    xdg_home(glib::user_cache_dir(), ".cache")
}

/// glib resolves $XDG_*_HOME and $HOME and yields an empty path when both are
/// unset; $HOME then /tmp keep the store writable in that case.
fn xdg_home(dir: PathBuf, home_suffix: &str) -> PathBuf {
    if !dir.as_os_str().is_empty() {
        return dir;
    }
    match std::env::var_os("HOME").filter(|home| !home.is_empty()) {
        Some(home) => PathBuf::from(home).join(home_suffix),
        None => PathBuf::from("/tmp"),
    }
}

/// `$XDG_DATA_HOME/br0x/<name>`, as a string for the APIs that take `&str`.
fn data_file(name: &str) -> String {
    data_home().join("br0x").join(name).to_string_lossy().into_owned()
}

fn data_dir() -> String {
    data_file("webdata")
}

fn cache_dir() -> String {
    cache_home().join("br0x").join("webcache").to_string_lossy().into_owned()
}

fn session_path() -> String {
    data_file("session.json")
}

fn prefs_path() -> String {
    data_file("prefs.json")
}

fn newtab_path() -> String {
    data_file("newtab.html")
}

/// file:// URL with the path percent-encoded: a space or non-ASCII word
/// in $HOME must not break start-page identity checks elsewhere.
fn file_url(path: &str) -> String {
    let mut encoded = String::with_capacity(path.len() + 7);
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                encoded.push(byte as char);
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    format!("file://{encoded}")
}

fn newtab_url() -> String {
    static CACHED: OnceLock<String> = OnceLock::new();
    CACHED.get_or_init(|| file_url(&newtab_path())).clone()
}

fn history_path() -> String {
    data_file("history.db")
}

fn bookmarks_path() -> String {
    data_file("bookmarks.json")
}

/// Star state for the address bar button. Empty and internal pages are
/// never shown as bookmarked.
fn refresh_bookmark_icon(btn: &gtk4::Button, bookmarks: &[Bookmark], uri: &str) {
    let marked =
        !uri.is_empty() && !is_blank_uri(uri) && BookmarkStore::is_bookmarked(bookmarks, uri);
    btn.set_icon_name(if marked { "starred-symbolic" } else { "non-starred-symbolic" });
    btn.set_tooltip_text(Some(if marked {
        "Bookmarked — click to remove (Ctrl+D)"
    } else {
        "Bookmark this page (Ctrl+D)"
    }));
}

fn html_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Display domain for cards and history rows. One helper shared by both
/// pages so the strip-prefix logic lives in a single place.
fn display_domain(url: &str) -> &str {
    url.strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .and_then(|s| s.split('/').next())
        .unwrap_or("local")
}

/// First letter for avatars, uppercased. Falls back to a bullet.
fn avatar_letter(name: &str) -> String {
    name.chars().next().unwrap_or('•').to_uppercase().to_string()
}

/// Day bucket label: Today, Yesterday, or a date.
fn history_day_label(visited_at: i64, today_day: i32) -> String {
    let day = (visited_at / 86_400) as i32;
    let diff = today_day - day;
    if diff <= 0 {
        "Today".to_string()
    } else if diff == 1 {
        "Yesterday".to_string()
    } else if let Ok(dt) = glib::DateTime::from_unix_local(visited_at)
        && let Ok(s) = dt.format("%B %e, %Y")
    {
        s.to_string()
    } else {
        "Earlier".to_string()
    }
}

fn history_time(visited_at: i64) -> String {
    glib::DateTime::from_unix_local(visited_at)
        .ok()
        .and_then(|dt| dt.format("%H:%M").ok())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

fn history_row(url: &str, title: &str, domain: &str, time: &str) -> String {
    let letter = avatar_letter(domain);
    // Pre-lowered haystack: the live filter reads the attribute instead of
    // re-lowercasing the row text on every keystroke.
    let haystack = format!("{title} {domain} {time}").to_lowercase();
    format!(
        r#"<tr class="history-row" data-search="{haystack}">
            <td class="col-avatar">{fav}</td>
            <td class="col-main">
                <a class="entry-title" href="{url}">{title}</a>
                <div class="entry-url">{domain}</div>
            </td>
            <td class="col-when">{time}</td>
        </tr>"#,
        url = html_escape(url),
        title = html_escape(title),
        domain = html_escape(domain),
        haystack = html_escape(&haystack),
        fav = favicon_img(domain, &letter),
        time = html_escape(time),
    )
}

/// The br0x://history page: a simple, clean list of past visits.
fn history_html(history: &History, query: Option<&str>, clear: bool) -> String {
    if clear {
        let _ = history.clear();
    }
    let visits = match query {
        Some(q) if !q.is_empty() => history.search(q, 300).unwrap_or_default(),
        _ => history.recent(300).unwrap_or_default(),
    };
    let today_day = glib::DateTime::now_local().map(|d| (d.to_unix() / 86_400) as i32).unwrap_or(0);
    let mut sections: Vec<(String, String)> = Vec::new();
    let mut last_day = i32::MIN;
    for v in &visits {
        // Cheap integer grouping; the formatted label is built only when a
        // new day section actually starts.
        let day = (v.visited_at / 86_400) as i32;
        let domain = display_domain(&v.url).to_string();
        let title = if v.title.is_empty() { v.url.clone() } else { v.title.clone() };
        let row = history_row(&v.url, &title, &domain, &history_time(v.visited_at));
        if day != last_day {
            let section = history_day_label(v.visited_at, today_day);
            if last_day != i32::MIN
                && let Some((_, body)) = sections.last_mut()
            {
                body.push_str("</tbody></table>");
            }
            sections.push((
                section.clone(),
                format!("<h2 class=\"day\">{section}</h2><table><tbody>{row}"),
            ));
            last_day = day;
        } else if let Some((_, body)) = sections.last_mut() {
            body.push_str(&row);
        }
    }
    if let Some((_, body)) = sections.last_mut() {
        body.push_str("</tbody></table>");
    }
    let rows: String = sections
        .into_iter()
        .map(|(_, body)| format!("<section class=\"day-group\">{body}</section>"))
        .collect();
    let q = query.unwrap_or_default();
    let total = history.count().unwrap_or(visits.len() as i64) as usize;
    let count_label = if total > visits.len() {
        format!("latest {} of {total} entries", visits.len())
    } else {
        format!("{} entries", visits.len())
    };
    let empty_state = if visits.is_empty() {
        r#"<div class="empty-notice">
          <p class="empty-title">No history yet</p>
          <p class="empty-sub">Pages you visit will show up here.</p>
        </div>"#
            .to_string()
    } else {
        String::new()
    };
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>History — br0x</title>
  <style>
    :root {{ color-scheme: light dark; }}
    * {{ box-sizing: border-box; }}
    body {{
      margin: 0;
      background-color: light-dark(#ffffff, #1e1e1e);
      color: light-dark(#1c1c1c, #e8e8e8);
      font-family: system-ui, -apple-system, "Segoe UI", sans-serif;
      font-size: 14px;
      line-height: 1.5;
    }}
    .wrap {{
      max-width: 760px;
      margin: 0 auto;
      padding: 40px 24px 80px;
      animation: fade 150ms ease-out;
    }}
    @keyframes fade {{
      from {{ opacity: 0; }}
      to {{ opacity: 1; }}
    }}
    @media (prefers-reduced-motion: reduce) {{
      .wrap {{ animation: none; }}
    }}
    .header-panel {{
      position: sticky;
      top: 0;
      z-index: 5;
      display: flex;
      flex-wrap: wrap;
      justify-content: space-between;
      align-items: center;
      gap: 12px;
      padding: 16px 0 12px;
      margin-bottom: 8px;
      background: color-mix(in srgb, light-dark(#ffffff, #1e1e1e) 88%, transparent);
      backdrop-filter: blur(12px);
      border-bottom: 1px solid light-dark(#eef0f4, #2d2d2d);
    }}
    .title-group {{
      display: flex;
      align-items: baseline;
      gap: 10px;
    }}
    h1 {{
      font-size: 26px;
      font-weight: 750;
      letter-spacing: -0.4px;
      margin: 0;
    }}
    .count {{
      font-size: 13px;
      color: light-dark(#616161, #9e9e9e);
    }}
    .btn-clear {{
      color: light-dark(#616161, #9e9e9e);
      text-decoration: none;
      font-size: 13px;
      padding: 7px 14px;
      border-radius: 9999px;
      border: 1px solid light-dark(#e0e0e0, #3d3d3d);
      background: light-dark(#ffffff, #262626);
    }}
    .btn-clear:hover {{
      background-color: light-dark(#f5f5f5, #2d2d2d);
    }}
    .filter-box {{
      margin: 12px 0 20px;
    }}
    .filter-box input {{
      width: 100%;
      background-color: light-dark(#ffffff, #262626);
      border: 1px solid light-dark(#e2e5ec, #3d3d3d);
      border-radius: 14px;
      padding: 11px 15px;
      color: inherit;
      font: inherit;
      box-shadow: 0 1px 2px rgba(15, 23, 42, 0.05);
    }}
    .filter-box input:focus {{
      outline: none;
      border-color: light-dark(#2f6fed, #7aa6ff);
      box-shadow: 0 0 0 3px light-dark(rgba(47, 111, 237, 0.14), rgba(122, 166, 255, 0.2));
    }}
    h2.day {{
      font-size: 12px;
      font-weight: 700;
      text-transform: uppercase;
      letter-spacing: 0.6px;
      color: light-dark(#5f6672, #9e9e9e);
      margin: 26px 0 6px;
    }}
    table {{
      width: 100%;
      border-collapse: collapse;
      background: light-dark(#ffffff, #242424);
      border: 1px solid light-dark(#eaf0f4, #333);
      border-radius: 14px;
      overflow: hidden;
    }}
    tr.history-row {{
      border-bottom: 1px solid light-dark(#f0f2f6, #2e2e2e);
    }}
    tr.history-row:last-child {{
      border-bottom: none;
    }}
    tr.history-row:hover {{
      background-color: light-dark(#f7f9fc, #2b2b2b);
    }}
    td {{
      padding: 10px 10px;
      vertical-align: middle;
    }}
    td.col-avatar {{
      width: 40px;
    }}
    .avatar {{
      display: none;
    }}
    .fav {{
      position: relative;
      display: inline-flex;
      align-items: center;
      justify-content: center;
      width: 28px;
      height: 28px;
      flex: none;
      border-radius: 8px;
      background-color: light-dark(#eef1f6, #333);
      overflow: hidden;
      font-size: 12px;
      font-weight: 700;
      color: light-dark(#5b6472, #c9c9c9);
    }}
    .fav-letter {{
      line-height: 1;
    }}
    .fav-img {{
      position: absolute;
      inset: 0;
      width: 100%;
      height: 100%;
      object-fit: cover;
      background-color: light-dark(#ffffff, #242424);
    }}
    td.col-when {{
      color: light-dark(#757575, #9e9e9e);
      font-size: 12px;
      white-space: nowrap;
      width: 56px;
      text-align: right;
    }}
    td.col-main {{
      word-break: break-all;
    }}
    a.entry-title {{
      color: inherit;
      text-decoration: none;
      font-weight: 550;
    }}
    a.entry-title:hover {{
      text-decoration: underline;
    }}
    .entry-url {{
      color: light-dark(#757575, #9e9e9e);
      font-size: 12px;
      margin-top: 1px;
    }}
    .empty-notice {{
      text-align: center;
      padding: 48px 0;
    }}
    .empty-title {{
      font-size: 16px;
      font-weight: 600;
      margin: 0 0 4px;
    }}
    .empty-sub {{
      color: light-dark(#757575, #9e9e9e);
      font-size: 13px;
      margin: 0;
    }}
    @media (max-width: 560px) {{
      .wrap {{ padding: 24px 14px 60px; }}
      td.col-avatar {{ display: none; }}
      td.col-when {{ width: 44px; }}
      h1 {{ font-size: 22px; }}
    }}
  </style>
</head>
<body>
  <div class="wrap">
    <div class="header-panel">
      <div class="title-group">
        <h1>History</h1>
        <span class="count">{count_label}</span>
      </div>
      <div class="actions">
        <a class="btn-clear" href="br0x://history?clear=1" onclick="return confirm('Clear entire local browsing history?');">Clear…</a>
      </div>
    </div>
    <form method="get" action="br0x://history">
      <div class="filter-box">
        <input id="history-filter" name="q" value="{q}" placeholder="Filter history…" autocomplete="off">
      </div>
    </form>
    <div id="history-tbody">
      {rows}
    </div>
    {empty_state}
    <div id="no-matches" class="empty-notice" style="display: none;">
      <p class="empty-title">No matching entries</p>
      <p class="empty-sub">Try a different filter.</p>
    </div>
  </div>
  <script>
    const filter = document.getElementById('history-filter');
    const root = document.getElementById('history-tbody');
    const noMatches = document.getElementById('no-matches');
    if (filter && root) {{
      filter.addEventListener('input', () => {{
        const q = filter.value.trim().toLowerCase();
        const rows = root.querySelectorAll('.history-row');
        let visibleCount = 0;
        rows.forEach(row => {{
          const text = row.dataset.search || row.textContent.toLowerCase();
          const match = q === '' || text.includes(q);
          row.style.display = match ? '' : 'none';
          if (match) visibleCount++;
        }});
        root.querySelectorAll('.day-group').forEach(group => {{
          const anyVisible = Array.from(group.querySelectorAll('.history-row'))
            .some(r => r.style.display !== 'none');
          group.style.display = anyVisible ? '' : 'none';
        }});
        if (noMatches) {{
          noMatches.style.display = (rows.length > 0 && visibleCount === 0) ? 'block' : 'none';
        }}
      }});
    }}
  </script>
  {clear_script}
</body>
</html>"#,
        count_label = count_label,
        q = html_escape(q),
        empty_state = empty_state,
        // After a wipe the tab URL still carries ?clear=1, which would wipe
        // again on reload or Back. Drop the query so the URL is disarmed.
        clear_script = if clear {
            "<script>history.replaceState({}, '', 'br0x://history');</script>"
        } else {
            ""
        },
    )
}

/// Decode one query component: `%XX` escapes, and `+` as space.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                match (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2])) {
                    (Some(hi), Some(lo)) => {
                        out.push(hi * 16 + lo);
                        i += 3;
                    }
                    _ => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Decoded query parameters of a URI. Order independent, so `?clear=1&q=x`
/// and `?q=x&clear=1` mean the same thing.
fn query_params(uri: &str) -> Vec<(String, String)> {
    let Some((_, query)) = uri.split_once('?') else {
        return Vec::new();
    };
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((key, value)) => (percent_decode(key), percent_decode(value)),
            None => (percent_decode(pair), String::new()),
        })
        .collect()
}

/// Search text and clear flag of a br0x://history URI. Only an exact
/// `clear=1` parameter clears, so searching for the text "clear=1" cannot
/// wipe the log, and the parameters may come in any order.
fn history_request(uri: &str) -> (Option<String>, bool) {
    let params = query_params(uri);
    let clear = params.iter().any(|(key, value)| key == "clear" && value == "1");
    let query = params.iter().find(|(key, _)| key == "q").map(|(_, value)| value.clone());
    (query, clear)
}

/// Serve br0x://history from the local database.
fn register_br0x_scheme(context: &webkit6::WebContext, history: Rc<RefCell<Option<History>>>) {
    context.register_uri_scheme("br0x", move |request| {
        let uri = request.uri().map(|u| u.to_string()).unwrap_or_default();
        let (query, clear) = history_request(&uri);
        let html = match history.borrow_mut().as_mut() {
            Some(h) => history_html(h, query.as_deref(), clear),
            None => "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
                <style>:root{color-scheme:light dark}body{font-family:system-ui,sans-serif;\
                display:flex;min-height:100vh;align-items:center;justify-content:center;\
                margin:0;color:light-dark(#616161,#9e9e9e)}</style></head>\
                <body><p>History unavailable</p></body></html>"
                .to_string(),
        };
        let bytes = glib::Bytes::from_owned(html.into_bytes());
        let stream = gio::MemoryInputStream::from_bytes(&bytes);
        let response = webkit6::URISchemeResponse::new(&stream, bytes.len() as i64);
        response.set_content_type("text/html");
        request.finish_with_response(&response);
    });
}

/// Last path component of a server-suggested file name. `None` for names
/// that are empty or have no component (".", "..", "a/..").
fn safe_file_name(suggested: &str) -> Option<&str> {
    std::path::Path::new(suggested)
        .file_name()
        .filter(|name| !name.is_empty())
        .and_then(|name| name.to_str())
}

/// First free name in the download folder: `photo.jpg`, `photo (1).jpg`, …
/// WebKit picks a free name for its own destination, but a destination set
/// by hand is used as is and fails on an existing file, since overwriting
/// is off.
fn free_file_name(name: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(name) {
        return name.to_owned();
    }
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem, format!(".{extension}")),
        _ => (name, String::new()),
    };
    (1..1000)
        .map(|n| format!("{stem} ({n}){extension}"))
        .find(|candidate| !taken(candidate))
        .unwrap_or_else(|| name.to_owned())
}

/// Save downloads to the Downloads directory and tell the user with a toast.
fn connect_downloads(session: &webkit6::NetworkSession, toasts: &adw::ToastOverlay) {
    let toasts = toasts.clone();
    session.connect_download_started(move |_, download| {
        download.set_allow_overwrite(false);
        // No HOME (or empty Downloads lookup) must never land files in the
        // process working directory: fall back to the temp dir instead.
        let dir = glib::user_special_dir(glib::UserDirectory::Downloads).unwrap_or_else(|| {
            std::env::var_os("HOME")
                .filter(|home| !home.is_empty())
                .map(|home| PathBuf::from(home).join("Downloads"))
                .unwrap_or_else(std::env::temp_dir)
        });
        let _ = std::fs::create_dir_all(&dir);
        let toasts_bad = toasts.clone();
        download.connect_decide_destination(move |d, suggested| {
            // Keep only the last path component: a suggested "../../.bashrc"
            // must not escape the Downloads directory.
            let Some(name) = safe_file_name(suggested) else {
                toasts_bad.add_toast(adw::Toast::new("Download failed: unsafe file name"));
                return false;
            };
            let name = free_file_name(name, |candidate| dir.join(candidate).exists());
            match dir.join(&name).to_str() {
                Some(dest) => {
                    d.set_destination(dest);
                    true
                }
                None => {
                    toasts_bad.add_toast(adw::Toast::new("Download failed: unsupported path"));
                    false
                }
            }
        });
        let toasts_done = toasts.clone();
        download.connect_finished(move |d| {
            let name = d
                .destination()
                .and_then(|p| {
                    std::path::Path::new(p.as_str())
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                })
                .unwrap_or_else(|| "file".to_string());
            toasts_done.add_toast(adw::Toast::new(&format!("Saved {name}")));
        });
        let toasts_fail = toasts.clone();
        download.connect_failed(move |_, error| {
            toasts_fail.add_toast(adw::Toast::new(&format!("Download failed: {error}")));
        });
        toasts.add_toast(adw::Toast::new("Download started"));
    });
}

/// Write the start page file. A file keeps the page offline, instant, and
/// user editable. False when the write failed, so the caller keeps the page
/// marked stale and retries instead of serving the old one forever.
fn write_newtab_page(engine: SearchEngine, frequent: &[Visit]) -> bool {
    let path = newtab_path();
    if let Some(parent) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::write(&path, newtab_html(engine, frequent)).is_err() {
        eprintln!("br0x: could not write {path}");
        return false;
    }
    true
}

/// True when the start page file no longer matches the inputs it was built
/// from. Frequent embeds live history, so the engine alone is not a safe
/// key: titles and visit times shape the cards and order too.
fn newtab_page_stale(
    cached: Option<&(SearchEngine, Vec<Visit>)>,
    engine: SearchEngine,
    frequent: &[Visit],
) -> bool {
    cached.is_none_or(|(cached_engine, cached_frequent)| {
        *cached_engine != engine || cached_frequent.as_slice() != frequent
    })
}

/// Write the start page only when it is stale, and return its file URL
/// either way. Repeats with an unchanged engine and history — every Ctrl+T —
/// cost a comparison instead of a rewrite of the whole page.
fn sync_newtab_page(
    last: &RefCell<Option<(SearchEngine, Vec<Visit>)>>,
    engine: SearchEngine,
    frequent: &[Visit],
) -> String {
    let stale = {
        let cached = last.borrow();
        newtab_page_stale(cached.as_ref(), engine, frequent)
    };
    if stale && write_newtab_page(engine, frequent) {
        last.replace(Some((engine, frequent.to_vec())));
    }
    newtab_url()
}

/// One site tile: real favicon over a letter fallback, name and domain.
/// `key` adds a silent number-key shortcut; `class` extends the styling.
fn site_card(url: &str, name: &str, key: Option<&str>, class: &str) -> String {
    let domain = display_domain(url);
    let letter = avatar_letter(name);
    let key_attr = key.map(|k| format!(" data-key=\"{}\"", html_escape(k))).unwrap_or_default();
    let key_badge = key
        .map(|k| format!("<kbd class=\"key-badge\">{}</kbd>", html_escape(k)))
        .unwrap_or_default();
    format!(
        r#"<a class="card {class}" href="{url}" title="{url}"{key_attr}>
            {fav}
            <span class="card-text"><span class="card-name">{name}</span><span class="card-domain">{domain}</span></span>
            {key_badge}
        </a>"#,
        class = html_escape(class),
        url = html_escape(url),
        key_attr = key_attr,
        fav = favicon_img(domain, &letter),
        name = html_escape(name),
        domain = html_escape(domain),
        key_badge = key_badge,
    )
}
/// Human name for a frequent URL. Raw URLs and URL-looking titles fall
/// back to the domain so tiles never show query strings.
fn frequent_name(visit: &Visit) -> String {
    if !visit.title.is_empty() && visit.title != visit.url && !visit.title.starts_with("http") {
        visit.title.clone()
    } else {
        display_domain(&visit.url).to_string()
    }
}

/// Favicon via a lightweight icon service, with a letter fallback when the
/// image fails to load (offline or unknown host).
fn favicon_img(domain: &str, letter: &str) -> String {
    format!(
        r#"<span class="fav" aria-hidden="true"><span class="fav-letter">{letter}</span><img class="fav-img" src="https://icons.duckduckgo.com/ip3/{domain}.ico" alt="" loading="lazy" onerror="this.remove()"></span>"#,
        domain = html_escape(domain),
        letter = html_escape(letter),
    )
}

const SHORTCUTS: [(&str, &str, &str); 6] = [
    ("1", "GitHub", "https://github.com"),
    ("2", "YouTube", "https://youtube.com"),
    ("3", "Reddit", "https://reddit.com"),
    ("4", "Hacker News", "https://news.ycombinator.com"),
    ("5", "Wikipedia", "https://wikipedia.org"),
    ("6", "Mail", "https://mail.google.com"),
];

fn newtab_html(engine: SearchEngine, frequent: &[Visit]) -> String {
    let (action, param) = engine.form();
    let items: String =
        SHORTCUTS.iter().map(|(key, name, url)| site_card(url, name, Some(key), "")).collect();
    let frequent_cards: String = frequent
        .iter()
        .take(8)
        .map(|v| site_card(&v.url, &frequent_name(v), None, "frequent-card"))
        .collect();
    let frequent_count = frequent.iter().take(8).len();
    let frequent_section = if frequent_cards.is_empty() {
        String::new()
    } else {
        format!(
            r#"<h2 class="section-title">Frequent <span class="section-count">· {frequent_count}</span></h2><nav class="grid" id="frequent-grid">{frequent_cards}</nav>"#
        )
    };
    let empty_hint = if frequent_cards.is_empty() {
        r#"<div class="hint-card"><p class="hint-title">A fresh start</p><p class="hint-sub">Sites you visit will appear here. Star pages with Ctrl+D to find them in the header menu.</p></div>"#
            .to_string()
    } else {
        String::new()
    };
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>New Tab — br0x</title>
  <style>
    :root {{ color-scheme: light dark; }}
    * {{ box-sizing: border-box; }}
    body {{
      margin: 0;
      background-color: light-dark(#ffffff, #1e1e1e);
      color: light-dark(#1c1c1c, #e8e8e8);
      font-family: system-ui, -apple-system, "Segoe UI", sans-serif;
      font-size: 14px;
      line-height: 1.5;
      min-height: 100vh;
      display: flex;
      flex-direction: column;
    }}
    .wrap {{
      max-width: 640px;
      margin: 0 auto;
      padding: 11vh 24px 40px;
      width: 100%;
      flex: 1;
      display: flex;
      flex-direction: column;
      align-items: center;
    }}
    .wordmark {{
      font-size: clamp(32px, 6vw, 42px);
      font-weight: 800;
      letter-spacing: -1.2px;
      margin: 0;
    }}
    .hero {{
      display: flex;
      flex-direction: column;
      align-items: center;
      text-align: center;
      animation: rise 200ms ease-out both;
    }}
    @keyframes rise {{
      from {{ opacity: 0; transform: translateY(8px); }}
      to {{ opacity: 1; transform: none; }}
    }}
    @keyframes pop {{
      from {{ opacity: 0; transform: scale(0.96) translateY(4px); }}
      to {{ opacity: 1; transform: none; }}
    }}
    .wordmark .zero {{
      color: light-dark(#2f6fed, #7aa6ff);
    }}
    .subtitle {{
      color: light-dark(#616161, #9e9e9e);
      font-size: 14px;
      margin: 6px 0 0;
    }}
    .date-line {{
      color: light-dark(#5f6672, #9e9e9e);
      font-size: 13px;
      margin: 4px 0 0;
    }}
    form {{
      width: 100%;
      margin: 30px 0 8px;
    }}
    .search-wrap {{
      position: relative;
      width: 100%;
    }}
    .search-icon {{
      position: absolute;
      left: 16px;
      top: 50%;
      transform: translateY(-50%);
      opacity: 0.45;
      pointer-events: none;
    }}
    .search-field {{
      width: 100%;
      background-color: light-dark(#ffffff, #2d2d2d);
      border: 1px solid light-dark(#e2e5ec, #3d3d3d);
      border-radius: 16px;
      padding: 14px 92px 14px 42px;
      color: inherit;
      font: inherit;
      font-size: 15px;
      box-shadow: 0 1px 2px rgba(15, 23, 42, 0.06), 0 12px 32px rgba(15, 23, 42, 0.08);
      transition: border-color 150ms ease, box-shadow 150ms ease;
    }}
    .engine-badge {{
      position: absolute;
      right: 12px;
      top: 50%;
      transform: translateY(-50%);
      font-size: 11px;
      font-weight: 600;
      letter-spacing: 0.2px;
      color: light-dark(#8a8f9c, #8e8e8e);
      background: light-dark(#f1f4f9, #3a3a3a);
      border: 1px solid light-dark(#e2e5ec, #4a4a4a);
      border-radius: 9999px;
      padding: 3px 9px;
      pointer-events: none;
      white-space: nowrap;
    }}
    .search-field:focus {{
      outline: none;
      border-color: light-dark(#2f6fed, #7aa6ff);
      box-shadow: 0 0 0 3px light-dark(rgba(47, 111, 237, 0.16), rgba(122, 166, 255, 0.22));
    }}
    .search-field::placeholder {{
      color: light-dark(#9e9e9e, #757575);
    }}
    .grid {{
      display: grid;
      grid-template-columns: repeat(3, minmax(0, 1fr));
      gap: 10px;
      width: 100%;
      margin-top: 12px;
    }}
    @media (max-width: 640px) {{
      .wrap {{ padding-top: 7vh; }}
    }}
    @media (max-width: 480px) {{
      .grid {{ grid-template-columns: repeat(2, minmax(0, 1fr)); }}
      .search-field {{ padding: 12px 84px 12px 15px; font-size: 14px; }}
    }}
    a.card {{
      display: flex;
      align-items: center;
      gap: 10px;
      min-width: 0;
      color: inherit;
      text-decoration: none;
      background-color: light-dark(#ffffff, #262626);
      border: 1px solid light-dark(#e8eaf0, #383838);
      border-radius: 16px;
      padding: 12px 14px;
      transition: transform 130ms ease-out, box-shadow 130ms ease-out, border-color 130ms ease-out;
      animation: rise 200ms ease-out both;
    }}
    .grid .card:nth-child(2) {{ animation-delay: 25ms; }}
    .grid .card:nth-child(3) {{ animation-delay: 50ms; }}
    .grid .card:nth-child(4) {{ animation-delay: 75ms; }}
    .grid .card:nth-child(5) {{ animation-delay: 100ms; }}
    .grid .card:nth-child(6) {{ animation-delay: 125ms; }}
    .grid .card:nth-child(7) {{ animation-delay: 150ms; }}
    .grid .card:nth-child(8) {{ animation-delay: 175ms; }}
    a.card:hover {{
      transform: translateY(-1px);
      border-color: light-dark(#c3ccd9, #4a4a4a);
      box-shadow: 0 6px 18px rgba(15, 23, 42, 0.1);
    }}
    .section-count {{
      font-weight: 600;
      color: light-dark(#5f6672, #9e9e9e);
    }}
    .hint-card {{
      width: 100%;
      margin-top: 22px;
      text-align: center;
      border: 1px dashed light-dark(#c9cfda, #4a4a4a);
      border-radius: 16px;
      padding: 20px 16px;
      background: color-mix(in srgb, AccentColor 7%, transparent);
    }}
    .hint-title {{
      margin: 0 0 4px;
      font-size: 14px;
      font-weight: 600;
    }}
    .hint-sub {{
      margin: 0;
      font-size: 13px;
      color: light-dark(#616161, #9e9e9e);
    }}
    .section-title {{
      width: 100%;
      font-size: 12px;
      font-weight: 700;
      text-transform: uppercase;
      letter-spacing: 0.6px;
      color: light-dark(#8a8f9c, #8e8e8e);
      margin: 26px 0 10px;
    }}
    .fav {{
      position: relative;
      display: inline-flex;
      align-items: center;
      justify-content: center;
      width: 32px;
      height: 32px;
      flex: none;
      border-radius: 10px;
      background-color: light-dark(#eef1f6, #333);
      overflow: hidden;
      font-size: 13px;
      font-weight: 700;
      color: light-dark(#5b6472, #c9c9c9);
    }}
    .fav-letter {{
      line-height: 1;
    }}
    .fav-img {{
      position: absolute;
      inset: 0;
      width: 100%;
      height: 100%;
      object-fit: cover;
      background-color: light-dark(#ffffff, #262626);
    }}
    .card-text {{
      display: flex;
      flex-direction: column;
      min-width: 0;
      flex: 1;
    }}
    .card-name {{
      font-size: 14px;
      font-weight: 550;
      white-space: nowrap;
      overflow: hidden;
      text-overflow: ellipsis;
    }}
    .card-domain {{
      font-size: 11px;
      color: light-dark(#5f6672, #9e9e9e);
      white-space: nowrap;
      overflow: hidden;
      text-overflow: ellipsis;
    }}
    .key-badge {{
      margin-left: auto;
      flex: none;
      font-size: 11px;
      font-weight: 600;
      color: light-dark(#8a8f9c, #8e8e8e);
      border: 1px solid light-dark(#e2e5ec, #3d3d3d);
      border-radius: 6px;
      padding: 1px 6px;
    }}
    .pin {{
      position: relative;
    }}
    .pin-remove {{
      position: absolute;
      top: 6px;
      right: 6px;
      width: 20px;
      height: 20px;
      border: 0;
      border-radius: 50%;
      background: light-dark(rgba(0, 0, 0, 0.06), rgba(255, 255, 255, 0.12));
      color: inherit;
      font-size: 12px;
      line-height: 1;
      cursor: pointer;
      opacity: 0;
    }}
    .pin:hover .pin-remove {{
      opacity: 1;
    }}
    button.add-card {{
      display: flex;
      align-items: center;
      justify-content: center;
      gap: 8px;
      width: 100%;
      border: 1px dashed light-dark(#c9cfda, #4a4a4a);
      border-radius: 16px;
      padding: 12px 14px;
      background: transparent;
      color: light-dark(#6b7280, #a0a0a0);
      font: inherit;
      font-size: 13px;
      cursor: pointer;
    }}
    button.add-card:hover {{
      border-color: light-dark(#2f6fed, #7aa6ff);
      color: light-dark(#2f6fed, #7aa6ff);
    }}
    footer {{
      margin-top: 40px;
      color: light-dark(#9e9e9e, #757575);
      font-size: 12px;
      display: flex;
      flex-wrap: wrap;
      justify-content: center;
      gap: 8px 16px;
    }}
    footer a {{
      color: inherit;
      text-decoration: none;
    }}
    footer a:hover {{
      text-decoration: underline;
    }}
    @media (prefers-reduced-motion: reduce) {{
      .hero, .grid .card, .modal-backdrop.open .modal {{
        animation: none;
      }}
      a.card, .search-field, button.add-card {{
        transition: none;
      }}
    }}
    .hint {{
      color: light-dark(#5f6672, #9e9e9e);
    }}
    .modal-backdrop {{
      position: fixed;
      inset: 0;
      display: none;
      align-items: center;
      justify-content: center;
      padding: 20px;
      background: rgba(0, 0, 0, 0.4);
      z-index: 50;
    }}
    .modal-backdrop.open {{
      display: flex;
    }}
    .modal-backdrop.open .modal {{
      animation: pop 160ms ease-out;
    }}
    .modal {{
      width: 100%;
      max-width: 380px;
      background: light-dark(#ffffff, #262626);
      border: 1px solid light-dark(#e8eaf0, #3d3d3d);
      border-radius: 20px;
      padding: 22px;
      box-shadow: 0 24px 64px rgba(0, 0, 0, 0.25);
    }}
    .modal h3 {{
      margin: 0 0 4px;
      font-size: 16px;
    }}
    .modal p {{
      margin: 0 0 14px;
      font-size: 13px;
      color: light-dark(#616161, #9e9e9e);
    }}
    .modal label {{
      display: block;
      font-size: 12px;
      font-weight: 600;
      margin: 10px 0 4px;
      color: light-dark(#424242, #c9c9c9);
    }}
    .modal input {{
      width: 100%;
      padding: 10px 12px;
      border-radius: 12px;
      border: 1px solid light-dark(#e2e5ec, #3d3d3d);
      background: light-dark(#f7f8fa, #1e1e1e);
      color: inherit;
      font: inherit;
      font-size: 14px;
    }}
    .modal input:focus {{
      outline: none;
      border-color: light-dark(#2f6fed, #7aa6ff);
      box-shadow: 0 0 0 3px light-dark(rgba(47, 111, 237, 0.16), rgba(122, 166, 255, 0.22));
    }}
    .modal-error {{
      display: none;
      font-size: 12px;
      color: light-dark(#b3261e, #f2a8a8);
      margin-top: 8px;
    }}
    .modal-actions {{
      display: flex;
      gap: 10px;
      margin-top: 18px;
    }}
    .btn-primary, .btn-ghost {{
      flex: 1;
      padding: 10px;
      border-radius: 9999px;
      font: inherit;
      font-size: 14px;
      font-weight: 600;
      cursor: pointer;
    }}
    .btn-primary {{
      border: 0;
      background: light-dark(#2f6fed, #7aa6ff);
      color: light-dark(#ffffff, #0b1b33);
    }}
    .btn-ghost {{
      border: 1px solid light-dark(#e2e5ec, #3d3d3d);
      background: transparent;
      color: inherit;
    }}
  </style>
</head>
<body>
  <div class="wrap">
    <div class="hero">
      <h1 class="wordmark">br<span class="zero">0</span>x</h1>
      <p class="subtitle" id="greeting">A calm place to start browsing</p>
      <p class="date-line" id="date-line"></p>
    </div>
    <form action="{action}" method="get">
      <div class="search-wrap">
        <svg class="search-icon" width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><circle cx="7" cy="7" r="5" fill="none" stroke="currentColor" stroke-width="1.6"/><line x1="11" y1="11" x2="14.5" y2="14.5" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/></svg>
        <input class="search-field" id="search-input" name="{param}" placeholder="Search {engine_name} or enter address" autocomplete="off" spellcheck="false">
        <span class="engine-badge">{engine_name}</span>
      </div>
    </form>
    {frequent_section}
    {empty_hint}
    <h2 class="section-title">Shortcuts <span class="section-count">· 6</span></h2>
    <nav class="grid">
      {items}
    </nav>
    <h2 class="section-title">Your shortcuts <span class="section-count" id="pins-count"></span></h2>
    <nav class="grid" id="pins-grid"></nav>
    <div style="width:100%;margin-top:10px"><button class="add-card" id="add-pin" type="button">+ Add shortcut</button></div>
    <footer>
      <span>{engine_name}</span>
      <a href="br0x://history">History</a>
      <span class="hint">Ctrl+L address · Ctrl+T new tab · / search</span>
    </footer>
  </div>
  <div class="modal-backdrop" id="pin-modal" role="dialog" aria-modal="true" aria-labelledby="pin-modal-title">
    <div class="modal">
      <h3 id="pin-modal-title">Add shortcut</h3>
      <p>Pin a site to this start page.</p>
      <label for="pin-name">Name</label>
      <input id="pin-name" maxlength="40" placeholder="Example" autocomplete="off">
      <label for="pin-url">Website address</label>
      <input id="pin-url" placeholder="https://…" inputmode="url" autocomplete="off">
      <div class="modal-error" id="pin-error">Enter a valid address, like https://example.com</div>
      <div class="modal-actions">
        <button class="btn-ghost" id="pin-cancel" type="button">Cancel</button>
        <button class="btn-primary" id="pin-save" type="button">Add</button>
      </div>
    </div>
  </div>
  <script>
    (function() {{
      var h = new Date().getHours();
      var g = h < 5 ? "Good night" : h < 12 ? "Good morning" : h < 18 ? "Good afternoon" : "Good evening";
      var el = document.getElementById('greeting');
      if (el) el.textContent = g + " — a calm place to start";
      var dateEl = document.getElementById('date-line');
      if (dateEl) dateEl.textContent = new Date().toLocaleDateString(undefined, {{ weekday: 'long', month: 'long', day: 'numeric' }});
    }})();
    (function() {{
      var KEY = 'br0x-pins';
      function load() {{
        try {{ return JSON.parse(localStorage.getItem(KEY) || '[]'); }}
        catch (e) {{ return []; }}
      }}
      function save(pins) {{
        try {{ localStorage.setItem(KEY, JSON.stringify(pins)); }}
        catch (e) {{}}
      }}
      function domainOf(url) {{
        var m = /^https?:\/\/([^/]+)/i.exec(url || '');
        return m ? m[1] : url;
      }}
      function render() {{
        var grid = document.getElementById('pins-grid');
        if (!grid) return;
        grid.textContent = '';
        var pins = load();
        var count = document.getElementById('pins-count');
        if (count) count.textContent = pins.length ? '· ' + pins.length : '';
        pins.forEach(function(pin, idx) {{
          var a = document.createElement('a');
          a.className = 'card pin';
          a.href = pin.url;
          a.title = pin.url;
          var domain = domainOf(pin.url);
          var letter = (pin.name || domain).charAt(0).toUpperCase();
          var fav = document.createElement('span');
          fav.className = 'fav';
          fav.setAttribute('aria-hidden', 'true');
          var fl = document.createElement('span');
          fl.className = 'fav-letter';
          fl.textContent = letter;
          fav.appendChild(fl);
          var img = document.createElement('img');
          img.className = 'fav-img';
          img.loading = 'lazy';
          img.alt = '';
          img.src = 'https://icons.duckduckgo.com/ip3/' + domain + '.ico';
          img.onerror = function() {{ this.remove(); }};
          fav.appendChild(img);
          var text = document.createElement('span');
          text.className = 'card-text';
          var nm = document.createElement('span');
          nm.className = 'card-name';
          nm.textContent = pin.name || domain;
          var dm = document.createElement('span');
          dm.className = 'card-domain';
          dm.textContent = domain;
          text.appendChild(nm);
          text.appendChild(dm);
          var x = document.createElement('button');
          x.className = 'pin-remove';
          x.type = 'button';
          x.title = 'Remove';
          x.textContent = '×';
          x.onclick = function(ev) {{
            ev.preventDefault();
            ev.stopPropagation();
            var pins = load();
            pins.splice(idx, 1);
            save(pins);
            render();
          }};
          a.appendChild(fav);
          a.appendChild(text);
          a.appendChild(x);
          grid.appendChild(a);
        }});
      }}
      function validUrl(raw) {{
        var url = (raw || '').trim();
        if (!url) return '';
        if (!/^https?:\/\//i.test(url)) url = 'https://' + url;
        try {{
          var u = new URL(url);
          if (u.protocol !== 'http:' && u.protocol !== 'https:') return '';
          return u.href;
        }} catch (e) {{ return ''; }}
      }}
      function openModal() {{
        var modal = document.getElementById('pin-modal');
        var err = document.getElementById('pin-error');
        if (err) err.style.display = 'none';
        if (!modal) return;
        modal.classList.add('open');
        var name = document.getElementById('pin-name');
        var url = document.getElementById('pin-url');
        if (name) name.value = '';
        if (url) url.value = '';
        setTimeout(function() {{ if (url) url.focus(); }}, 30);
      }}
      function closeModal() {{
        var modal = document.getElementById('pin-modal');
        if (modal) modal.classList.remove('open');
      }}
      function submitModal() {{
        var nameEl = document.getElementById('pin-name');
        var urlEl = document.getElementById('pin-url');
        var err = document.getElementById('pin-error');
        var url = validUrl(urlEl && urlEl.value);
        if (!url) {{
          if (err) err.style.display = 'block';
          if (urlEl) urlEl.focus();
          return;
        }}
        var domain = domainOf(url);
        var name = (nameEl && nameEl.value.trim()) || domain;
        var pins = load();
        pins.push({{ name: name, url: url }});
        save(pins);
        render();
        closeModal();
      }}
      var add = document.getElementById('add-pin');
      if (add) add.onclick = openModal;
      var cancel = document.getElementById('pin-cancel');
      if (cancel) cancel.onclick = closeModal;
      var saveBtn = document.getElementById('pin-save');
      if (saveBtn) saveBtn.onclick = submitModal;
      var backdrop = document.getElementById('pin-modal');
      if (backdrop) backdrop.addEventListener('click', function(e) {{
        if (e.target === backdrop) closeModal();
      }});
      document.addEventListener('keydown', function(e) {{
        var modal = document.getElementById('pin-modal');
        if (modal && modal.classList.contains('open')) {{
          if (e.key === 'Escape') closeModal();
          if (e.key === 'Enter' && document.activeElement &&
              (document.activeElement.id === 'pin-name' || document.activeElement.id === 'pin-url')) {{
            e.preventDefault();
            submitModal();
            return;
          }}
        }}
      }});
      render();
    }})();
    document.addEventListener('keydown', function(e) {{
      var active = document.activeElement;
      var typing = active && (active.tagName === 'INPUT' || active.tagName === 'TEXTAREA');
      if (typing) {{
        if (e.key === 'Escape') active.blur();
        // Number shortcuts still work while the search box is empty.
        if (active.value !== '' || !/^[1-6]$/.test(e.key)) return;
      }}
      if (e.key === '/') {{
        e.preventDefault();
        var input = document.getElementById('search-input');
        if (input) {{ input.focus(); input.select(); }}
        return;
      }}
      var card = document.querySelector('.card[data-key="' + CSS.escape(e.key) + '"]');
      if (card) {{
        window.location.href = card.href;
      }}
    }});
  </script>
</body>
</html>"#,
        engine_name = engine.name(),
        empty_hint = empty_hint,
    )
}

/// WebKitGTK's default user agent is flagged by Google as a bot, which
/// forces an "unusual traffic" captcha on every search. Send a current
/// Chrome-on-Linux string instead.
const BROWSER_USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

/// Settings for every view. Related (window.open) views do not inherit
/// settings from their source, so each one needs its own copy.
fn browser_settings() -> webkit6::Settings {
    webkit6::Settings::builder().user_agent(BROWSER_USER_AGENT).build()
}

fn shared_session() -> webkit6::NetworkSession {
    for dir in [data_dir(), cache_dir()] {
        let _ = std::fs::create_dir_all(&dir);
    }
    let session = webkit6::NetworkSession::new(Some(&data_dir()), Some(&cache_dir()));
    // The favicon database is off by default in WebKitGTK; without it the
    // favicon property never updates and tabs cannot show site icons.
    if let Some(manager) = session.website_data_manager() {
        manager.set_favicons_enabled(true);
    }
    // Google treats cookie-less clients as bots and serves a captcha on
    // every search, so accept and persist cookies explicitly.
    if let Some(cookies) = session.cookie_manager() {
        cookies.set_accept_policy(webkit6::CookieAcceptPolicy::Always);
        cookies.set_persistent_storage(
            &format!("{}/cookies.sqlite", data_dir()),
            webkit6::CookiePersistentStorage::Sqlite,
        );
    }
    session
}

/// One context for every tab, tuned to shed caches instead of letting
/// single pages balloon. A page that trips the conservative threshold gives
/// back caches; only a page past the kill threshold loses its process.
fn shared_context() -> webkit6::WebContext {
    let mut pressure = webkit6::MemoryPressureSettings::new();
    pressure.set_kill_threshold(0.95);
    pressure.set_strict_threshold(0.7);
    pressure.set_conservative_threshold(0.5);
    pressure.set_memory_limit(1024);
    pressure.set_poll_interval(5.0);
    let context = webkit6::WebContext::builder().memory_pressure_settings(&pressure).build();
    // No in-memory resource cache. The disk cache still serves repeats;
    // this saves memory in every web process.
    context.set_cache_model(webkit6::CacheModel::DocumentViewer);
    context
}

fn filter_store() -> webkit6::UserContentFilterStore {
    let dir = format!("{}/br0x/filters", data_dir());
    let _ = std::fs::create_dir_all(&dir);
    webkit6::UserContentFilterStore::new(&dir)
}

/// Compile the base filter once at startup; per-tab code only attaches it.
/// Tabs created before compilation finishes get the filter attached here,
// otherwise early tabs would browse unprotected. Shield-off sites stay
/// unfiltered.
fn ensure_filter(tabs: Rc<RefCell<Tabs>>, shield: Rc<RefCell<ShellShield>>) {
    if FILTER_CACHE.get().is_some() {
        for entry in tabs.borrow().entries.iter() {
            let uri = entry.view.uri().map(|u| u.to_string()).unwrap_or_default();
            set_view_filter(&entry.view, shield.borrow().is_enabled(&domain_of(&uri)));
        }
        return;
    }
    let store = filter_store();
    let json = glib::Bytes::from_owned(BASE_FILTER_JSON.as_bytes().to_vec());
    glib::spawn_future_local(async move {
        // The compiled list survives restarts: load it instead of paying
        // for a recompile (and disk write) on every launch.
        let compiled = match store.load_future("br0x-base").await {
            Ok(filter) => Some(filter),
            Err(_) => match store.save_future("br0x-base", &json).await {
                Ok(filter) => Some(filter),
                Err(e) => {
                    eprintln!("br0x: filter compile failed: {e}");
                    None
                }
            },
        };
        if let Some(filter) = compiled {
            let _ = FILTER_CACHE.set(CachedFilter(filter));
            for entry in tabs.borrow().entries.iter() {
                let uri = entry.view.uri().map(|u| u.to_string()).unwrap_or_default();
                set_view_filter(&entry.view, shield.borrow().is_enabled(&domain_of(&uri)));
            }
        }
    });
}

fn attach_filter(view: &webkit6::WebView) {
    let Some(cached) = FILTER_CACHE.get() else {
        return;
    };
    if let Some(ucm) = view.user_content_manager() {
        ucm.add_filter(&cached.0);
    }
}

/// (Re)apply the compiled blocker to one view. Shield-off sites keep no
/// filters, so their subresources load unfiltered.
fn set_view_filter(view: &webkit6::WebView, enabled: bool) {
    if let Some(ucm) = view.user_content_manager() {
        ucm.remove_all_filters();
        if enabled {
            attach_filter(view);
        }
    }
}

fn selected_view(tab_view: &adw::TabView) -> Option<webkit6::WebView> {
    tab_view.selected_page().and_then(|p| p.child().downcast::<webkit6::WebView>().ok())
}

fn is_blank_uri(uri: &str) -> bool {
    uri == "about:blank" || uri == newtab_url()
}

/// Release `entry` if nothing blocks it. Sleeping is the time-based release
/// without pressure; parking is the pressure-driven one. Same mechanism,
/// distinct badge. Returns the view whose process the caller terminates
/// once it has dropped its borrow of the tab list.
fn release_entry(entry: &mut TabEntry, sleeping: bool) -> Option<webkit6::WebView> {
    if entry.meta.parked || entry.meta.sleeping {
        return None;
    }
    // Never release a page that is still loading: there is nothing to free
    // and the load would be interrupted.
    if entry.view.is_loading() {
        return None;
    }
    match entry.view.uri() {
        Some(uri) if !is_blank_uri(&uri) => entry.meta.pending_url = Some(uri.to_string()),
        // A fresh new tab has nothing to free. A restored tab sitting on
        // about:blank still holds a process.
        Some(_) if entry.meta.pending_url.is_none() => return None,
        _ => {}
    }
    entry.meta.parked = !sleeping;
    entry.meta.sleeping = sleeping;
    let badge = if sleeping { "• Sleeping" } else { "• Parked" };
    let title = entry.page.title().to_string();
    if !title.is_empty() && !title.ends_with(badge) {
        entry.page.set_title(&format!("{title} {badge}"));
    }
    Some(entry.view.clone())
}

fn refresh_nav(tab_view: &adw::TabView, back: &gtk4::Button, fwd: &gtk4::Button) {
    let (can_back, can_fwd) = selected_view(tab_view)
        .map(|v| (v.can_go_back(), v.can_go_forward()))
        .unwrap_or((false, false));
    back.set_sensitive(can_back);
    fwd.set_sensitive(can_fwd);
}

/// Shortest typed text that opens address-bar suggestions.
const SUGGESTION_MIN_CHARS: usize = 2;
/// Most suggestions shown at once.
const SUGGESTION_LIMIT: usize = 8;

/// Rebuild the suggestion rows: open-tab hits first, then history, always
/// ending with an engine row. True means the popover has something to show.
/// `urls` stays parallel to every row but the last: index == urls.len() is
/// the engine row.
fn render_suggestions(
    tab_hits: &[(String, String, adw::TabPage)],
    visits: &[Visit],
    needle: &str,
    list: &gtk4::ListBox,
    urls: &Rc<RefCell<Vec<SuggestHit>>>,
    engine: SearchEngine,
) -> bool {
    list.remove_all();
    let mut store = urls.borrow_mut();
    store.clear();
    for (title, url, page) in tab_hits {
        let name =
            if title.trim().is_empty() { display_domain(url).to_owned() } else { title.clone() };
        let stack = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        stack.set_margin_top(4);
        stack.set_margin_bottom(4);
        let title_label = gtk4::Label::new(Some(&name));
        title_label.set_xalign(0.0);
        title_label.set_hexpand(true);
        title_label.set_max_width_chars(64);
        title_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        let hint = gtk4::Label::new(Some(&format!("Open tab · {url}")));
        hint.set_xalign(0.0);
        hint.set_hexpand(true);
        hint.set_max_width_chars(64);
        hint.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        hint.add_css_class("dim-label");
        stack.append(&title_label);
        stack.append(&hint);
        let row = gtk4::ListBoxRow::new();
        row.set_child(Some(&stack));
        list.append(&row);
        store.push(SuggestHit { url: url.clone(), page: Some(page.clone()) });
    }
    for visit in visits {
        let title = if visit.title.trim().is_empty() {
            display_domain(&visit.url).to_owned()
        } else {
            visit.title.clone()
        };
        let stack = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        stack.set_margin_top(4);
        stack.set_margin_bottom(4);
        let url_label = gtk4::Label::new(Some(&visit.url));
        url_label.set_xalign(0.0);
        url_label.set_hexpand(true);
        url_label.set_max_width_chars(64);
        url_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        let title_label = gtk4::Label::new(Some(&title));
        title_label.set_xalign(0.0);
        title_label.set_hexpand(true);
        title_label.set_max_width_chars(64);
        title_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        title_label.add_css_class("dim-label");
        stack.append(&url_label);
        stack.append(&title_label);
        let row = gtk4::ListBoxRow::new();
        row.set_child(Some(&stack));
        list.append(&row);
        store.push(SuggestHit { url: visit.url.clone(), page: None });
    }
    // Trailing engine row runs the typed text as a search.
    let line = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    line.set_margin_top(4);
    line.set_margin_bottom(4);
    let icon = gtk4::Image::from_icon_name("system-search-symbolic");
    let label =
        gtk4::Label::new(Some(&format!("Search {} for \"{}\"", engine.name(), needle.trim())));
    label.set_xalign(0.0);
    label.set_hexpand(true);
    label.set_max_width_chars(64);
    label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    line.append(&icon);
    line.append(&label);
    let row = gtk4::ListBoxRow::new();
    row.set_child(Some(&line));
    list.append(&row);
    true
}

/// Resolve a suggestion row. Index == urls.len() is the trailing engine
/// row, which searches the typed text like Enter does. A row carrying a
/// page switches to that tab instead of loading.
fn suggestion_target(
    urls: &[SuggestHit],
    index: Option<usize>,
    query: &str,
    engine: SearchEngine,
) -> Option<SuggestHit> {
    match index {
        Some(i) if i < urls.len() => urls.get(i).cloned(),
        Some(i) if i == urls.len() => {
            if query.trim().is_empty() {
                None
            } else {
                Some(SuggestHit { url: search::resolve(query, engine), page: None })
            }
        }
        _ => None,
    }
}

/// Row count of the suggestion list (always small: history hits plus the
/// engine row).
fn suggest_row_count(list: &gtk4::ListBox) -> usize {
    let mut count = 0;
    while list.row_at_index(count).is_some() {
        count += 1;
    }
    count as usize
}

/// Lowercase host of a URI without port, path, query or fragment.
/// Shell-side mirror of the core shield/curtain `normalize_domain`
/// (those modules exist but are not declared in core `lib.rs` yet, so the
/// shell cannot import them; swap to them when they land).
fn domain_of(uri: &str) -> String {
    let s = uri.trim();
    let rest = match s.find("://") {
        Some(pos) => &s[pos + 3..],
        None => s,
    };
    let host = match rest.find(['/', '?', '#']) {
        Some(end) => &rest[..end],
        None => rest,
    };
    let host = host.trim();
    let bare =
        if host.matches(':').count() == 1 { host.split(':').next().unwrap_or(host) } else { host };
    bare.trim().trim_end_matches('.').to_lowercase()
}

/// Origin (`scheme://host`, lowercased) for login matching.
/// Shell-side mirror of the core vault `normalize_origin`.
fn origin_of(uri: &str) -> String {
    let s = uri.trim();
    let (scheme, rest) = match s.find("://") {
        Some(pos) => (s[..pos].to_lowercase(), &s[pos + 3..]),
        None => return String::new(),
    };
    if scheme != "http" && scheme != "https" {
        return String::new();
    }
    let host = domain_of(s);
    if host.is_empty() {
        return String::new();
    }
    let _ = rest;
    format!("{scheme}://{host}")
}

/// Escape text for interpolation into a single-quoted JS string literal.
fn js_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '<' => out.push_str("\\x3c"),
            _ => out.push(c),
        }
    }
    out
}

/// Tab title without the sleep/park badge the shell appends.
fn strip_state_badges(title: &str) -> String {
    title
        .strip_suffix(" • Parked")
        .or_else(|| title.strip_suffix(" • Sleeping"))
        .unwrap_or(title)
        .to_owned()
}

/// Shell-side per-site blocker toggle. Same JSON shape as the core shield
/// store (`{domain: enabled}` with unset defaulting to on); only disabled
/// domains are written, which loads identically. Swap to
/// `br0x_core::shield::ShieldStore` once that module is declared.
struct ShellShield {
    path: String,
    off: HashSet<String>,
}

impl ShellShield {
    fn load(path: String) -> Self {
        let raw: HashMap<String, bool> =
            json_file::load(std::path::Path::new(&path)).unwrap_or_default();
        let off = raw
            .into_iter()
            .filter(|(_, enabled)| !enabled)
            .map(|(domain, _)| domain_of(&domain))
            .filter(|domain| !domain.is_empty())
            .collect();
        Self { path, off }
    }

    fn is_enabled(&self, domain: &str) -> bool {
        !self.off.contains(&domain_of(domain))
    }

    fn set(&mut self, domain: &str, enabled: bool) {
        let domain = domain_of(domain);
        if domain.is_empty() {
            return;
        }
        if enabled {
            self.off.remove(&domain);
        } else {
            self.off.insert(domain);
        }
        self.persist();
    }

    fn persist(&self) {
        let map: HashMap<String, bool> =
            self.off.iter().map(|domain| (domain.clone(), false)).collect();
        let _ = json_file::save(std::path::Path::new(&self.path), &map);
    }
}

/// Shell-side per-site hidden-element store. Same JSON shape as the core
/// curtain store (`{domain: [selectors]}`). Swap to
/// `br0x_core::curtain::CurtainStore` once that module is declared.
struct ShellCurtain {
    path: String,
    map: HashMap<String, Vec<String>>,
}

impl ShellCurtain {
    fn load(path: String) -> Self {
        let raw: HashMap<String, Vec<String>> =
            json_file::load(std::path::Path::new(&path)).unwrap_or_default();
        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        for (domain, selectors) in raw {
            let domain = domain_of(&domain);
            if domain.is_empty() {
                continue;
            }
            let list = map.entry(domain).or_default();
            for selector in selectors {
                let selector = selector.trim().to_owned();
                if !selector.is_empty() && !list.contains(&selector) {
                    list.push(selector);
                }
            }
        }
        map.retain(|_, selectors| !selectors.is_empty());
        Self { path, map }
    }

    fn selectors_for(&self, domain: &str) -> Vec<String> {
        self.map.get(&domain_of(domain)).cloned().unwrap_or_default()
    }

    fn hide(&mut self, domain: &str, selector: &str) -> bool {
        let domain = domain_of(domain);
        let selector = selector.trim().to_owned();
        if domain.is_empty() || selector.is_empty() {
            return false;
        }
        let list = self.map.entry(domain).or_default();
        if list.contains(&selector) {
            return false;
        }
        list.push(selector);
        self.persist();
        true
    }

    fn clear_domain(&mut self, domain: &str) -> bool {
        let removed = self.map.remove(&domain_of(domain)).is_some();
        if removed {
            self.persist();
        }
        removed
    }

    fn persist(&self) {
        let _ = json_file::save(std::path::Path::new(&self.path), &self.map);
    }
}

/// Clearly-marked fallback login store. The `secret-service` crate cannot be
/// added without touching the shell manifest (frozen for this change), and
/// the core vault module is not declared yet, so logins live in a JSON file
/// (`{origin: {username: secret}}`, like the core file vault) that is always
/// chmod 0600 on Unix. Swap to Secret Service or the core vault when either
/// becomes available; secrets are never logged.
struct ShellVault {
    path: String,
    map: HashMap<String, HashMap<String, String>>,
}

impl ShellVault {
    fn load(path: String) -> Self {
        let raw: HashMap<String, HashMap<String, String>> =
            match json_file::load(std::path::Path::new(&path)) {
                Ok(raw) => raw,
                Err(e) => {
                    if e.kind() == std::io::ErrorKind::InvalidData {
                        let _ = json_file::quarantine_corrupt(std::path::Path::new(&path));
                    }
                    HashMap::new()
                }
            };
        let mut map: HashMap<String, HashMap<String, String>> = HashMap::new();
        for (origin, users) in raw {
            let origin = origin.trim().to_lowercase();
            let origin = origin.trim_end_matches('/').to_owned();
            if origin.is_empty() {
                continue;
            }
            let slot = map.entry(origin).or_default();
            for (username, secret) in users {
                if !username.is_empty() && !secret.is_empty() {
                    slot.insert(username, secret);
                }
            }
        }
        map.retain(|_, users| !users.is_empty());
        let store = Self { path, map };
        store.enforce_private();
        store
    }

    fn credentials_for(&self, origin: &str) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .map
            .get(origin)
            .map(|users| users.iter().map(|(u, p)| (u.clone(), p.clone())).collect())
            .unwrap_or_default();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    fn save(&mut self, origin: &str, username: &str, secret: &str) -> bool {
        if origin.is_empty() || username.is_empty() || secret.is_empty() {
            return false;
        }
        self.map
            .entry(origin.to_owned())
            .or_default()
            .insert(username.to_owned(), secret.to_owned());
        self.persist()
    }

    fn persist(&self) -> bool {
        let ok = json_file::save(std::path::Path::new(&self.path), &self.map).is_ok();
        if ok {
            self.enforce_private();
        }
        ok
    }

    #[cfg(unix)]
    fn enforce_private(&self) {
        use std::os::unix::fs::PermissionsExt;
        if std::path::Path::new(&self.path).exists() {
            let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));
        }
    }

    #[cfg(not(unix))]
    fn enforce_private(&self) {}
}

/// One suggestion row: an open tab to switch to, or a URL to load.
#[derive(Clone, PartialEq)]
struct SuggestHit {
    url: String,
    page: Option<adw::TabPage>,
}

/// Hiding stylesheet for the site's curtain selectors. Applied as a user
/// stylesheet at document start so hidden elements never flash.
fn curtain_css(selectors: &[String]) -> String {
    selectors.iter().map(|s| format!("{s}{{display:none!important}}")).collect()
}

/// Arms the curtain picker: the next click captures a stable selector (id
/// preferred, then a short class path), hides the element immediately, and
/// stashes the selector on `window.__br0xPick` for the shell to collect.
const CURTAIN_ARM_JS: &str = r##"window.__br0xPick=null;
if(!window.__br0xPickHandler){window.__br0xPickHandler=function(e){e.preventDefault();e.stopPropagation();
var el=e.target;var cur=el;var parts=[];
if(cur&&cur.id){window.__br0xPick='#'+CSS.escape(cur.id);}
else{var depth=0;while(cur&&cur.nodeType===1&&cur!==document.documentElement&&depth<5){
var s=cur.tagName.toLowerCase();
var cls=(cur.className&&typeof cur.className==='string')?cur.className.trim().split(/\s+/).filter(function(c){return /^[A-Za-z_-][\w-]*$/.test(c);}).slice(0,2):[];
if(cls.length){s+='.'+cls.map(function(c){return CSS.escape(c);}).join('.');}
var par=cur.parentElement;
if(par){var sibs=Array.prototype.filter.call(par.children,function(c){return c.tagName===cur.tagName;});if(sibs.length>1){s+=':nth-of-type('+(sibs.indexOf(cur)+1)+')';}}
parts.unshift(s);cur=par;depth++;}
window.__br0xPick=parts.join(' > ');}
try{el.style.setProperty('display','none','important');}catch(_){}
};document.addEventListener('click',window.__br0xPickHandler,true);}"##;

/// Poll target while the picker is armed: empty until an element is picked.
const CURTAIN_POLL_JS: &str = "window.__br0xPick||''";

/// Removes the picker listener without saving anything.
const CURTAIN_DISARM_JS: &str = r##"if(window.__br0xPickHandler){document.removeEventListener('click',window.__br0xPickHandler,true);window.__br0xPickHandler=null;}window.__br0xPick=null;"##;

/// First playing video URL, else the first video URL, else empty. Blob and
/// DRM URLs come back as-is; the shell reports when they cannot be reused.
const VIDEO_FIND_JS: &str = r##"(()=>{var vs=Array.prototype.slice.call(document.querySelectorAll('video'));var playing=vs.filter(function(v){return !v.paused&&!v.ended&&v.readyState>2;});var v=playing[0]||vs[0];if(!v){return '';}return v.currentSrc||v.src||'';})()"##;

/// Scroll fraction of the page, or -1 when the page does not scroll.
const SCROLL_JS: &str = r##"(()=>{var h=document.documentElement;var m=h.scrollHeight-h.clientHeight;if(m<=0){return -1;}return h.scrollTop/m;})()"##;

/// Whether the page currently shows a password field.
const HAS_PASSWORD_JS: &str =
    r##"(()=>{return !!document.querySelector('input[type="password"]');})()"##;

/// `encodeURIComponent(user)|encodeURIComponent(pass)` of the login form, or
/// empty when there is nothing worth saving.
const READ_LOGIN_JS: &str = r##"(()=>{var p=document.querySelector('input[type="password"]');if(!p||!p.value){return '';}var u='';if(p.form){var t=p.form.querySelector('input[type="email"],input[type="text"]');if(t){u=t.value||'';}}if(!u){var t2=document.querySelector('input[type="email"],input[type="text"]');if(t2){u=t2.value||'';}}return encodeURIComponent(u)+'|'+encodeURIComponent(p.value);})()"##;

/// Dependency-free article extraction: score paragraphs by text length minus
/// link text, keep the winning container, drop chrome, return a clean page
/// (or empty when nothing article-like is found).
const READER_EXTRACT_JS: &str = r##"(()=>{function textLen(n){return ((n.innerText||'').trim().length);}function score(p){var t=(p.innerText||'').trim();if(t.length<40){return 0;}var links=Array.prototype.reduce.call(p.querySelectorAll('a'),function(n,a){return n+((a.innerText||'').length);},0);return t.length-links*2;}var ps=Array.prototype.slice.call(document.querySelectorAll('p'));var buckets=new Map();ps.forEach(function(p){var s=score(p);if(s<=0){return;}var a=p.parentElement;for(var i=0;i<3&&a;i++){buckets.set(a,(buckets.get(a)||0)+s);a=a.parentElement;}});var root=null;var best=0;buckets.forEach(function(v,k){if(v>best){best=v;root=k;}});if(!root||best<200){return '';}var clone=root.cloneNode(true);Array.prototype.forEach.call(clone.querySelectorAll('nav,aside,footer,header,form,script,style,noscript,iframe,canvas,.ad,.ads,.sidebar,.comments,#comments'),function(n){n.remove();});var title=(document.title||'').replace(/</g,'&lt;');var body=clone.innerHTML||'';return '<!doctype html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>'+title+'</title><style>:root{color-scheme:light dark}body{margin:0 auto;max-width:38em;padding:2em 1.2em;font:18px/1.7 system-ui,sans-serif}img,video{max-width:100%;height:auto}pre{overflow:auto}</style></head><body><h1>'+title+'</h1>'+body+'</body></html>';})()"##;

/// Evaluate `script`, handing its string value (empty on failure) to `done`.
fn eval_text(view: &webkit6::WebView, script: &str, done: impl FnOnce(String) + 'static) {
    let pending = view.evaluate_javascript_future(script, None, None);
    glib::spawn_future_local(async move {
        match pending.await {
            Ok(value) => done(value.to_str().to_string()),
            Err(_) => done(String::new()),
        }
    });
}

/// Evaluate `script`, handing its boolean value (false on failure) to `done`.
fn eval_flag(view: &webkit6::WebView, script: &str, done: impl FnOnce(bool) + 'static) {
    let pending = view.evaluate_javascript_future(script, None, None);
    glib::spawn_future_local(async move {
        match pending.await {
            Ok(value) => done(value.to_boolean()),
            Err(_) => done(false),
        }
    });
}

/// Evaluate `script`, handing its numeric value (NaN on failure) to `done`.
fn eval_ratio(view: &webkit6::WebView, script: &str, done: impl FnOnce(f64) + 'static) {
    let pending = view.evaluate_javascript_future(script, None, None);
    glib::spawn_future_local(async move {
        match pending.await {
            Ok(value) => done(value.to_double()),
            Err(_) => done(f64::NAN),
        }
    });
}
/// Whether the keyboard sits in the entry rather than in the page or in the
/// suggestion list. GTK focuses the entry's inner text widget, so the entry
/// itself is never the focus widget.
fn typing_in_entry(entry: &gtk4::Entry, popover: &gtk4::Popover) -> bool {
    entry
        .root()
        .and_then(|root| root.focus())
        .is_some_and(|focus| focus.is_ancestor(entry) && !focus.is_ancestor(popover))
}

/// Address bar text and security icon for the selected tab.
/// A blank or start page clears the text so a new tab never shows the
/// previous URL.
fn sync_entry_to_selection(tab_view: &adw::TabView, entry: &gtk4::Entry) {
    let Some(view) = selected_view(tab_view) else {
        return;
    };
    match view.uri() {
        Some(uri) if !is_blank_uri(&uri) => {
            entry.set_text(uri.as_str());
            set_security_icon(entry, uri.as_str());
        }
        _ => {
            entry.set_text("");
            set_security_icon(entry, "");
        }
    }
}

/// Friendly inline page for failed loads and crashed processes.
fn error_html(heading: &str, message: &str, uri: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>{heading} — br0x</title>
<style>
:root {{ color-scheme: light dark; }}
body {{ margin: 0; font-family: system-ui, sans-serif; display: flex; min-height: 100vh;
  align-items: center; justify-content: center; text-align: center;
  background: light-dark(#fafafa, #1e1e1e); color: light-dark(#1c1c1c, #e8e8e8); }}
.card {{ max-width: 420px; padding: 32px 28px; }}
.icon {{ font-size: 40px; margin-bottom: 12px; }}
h1 {{ font-size: 20px; margin: 0 0 8px; }}
p {{ color: light-dark(#616161, #9e9e9e); font-size: 14px; margin: 0 0 6px; }}
.url {{ font-size: 12px; word-break: break-all; }}
button {{ margin-top: 18px; padding: 10px 22px; border-radius: 9999px; border: 0;
  background: light-dark(#2f6fed, #7aa6ff); color: light-dark(#ffffff, #0b1b33); font: inherit; cursor: pointer; }}
</style></head>
<body><div class="card"><div class="icon">○</div><h1>{heading}</h1><p>{message}</p>
<p class="url">{uri}</p><button onclick="location.reload()">Reload</button></div></body></html>"#,
        heading = html_escape(heading),
        message = html_escape(message),
        uri = html_escape(uri),
    )
}
/// Friendly leading icon: magnifier on empty pages, lock on https,
/// warning on http. Tapping it explains the site security.
fn set_security_icon(entry: &gtk4::Entry, uri: &str) {
    let icon = if uri.starts_with("https://") {
        Some("channel-secure-symbolic")
    } else if uri.starts_with("http://") {
        Some("channel-insecure-symbolic")
    } else {
        Some("system-search-symbolic")
    };
    entry.set_icon_from_icon_name(gtk4::EntryIconPosition::Primary, icon);
    entry.set_icon_sensitive(gtk4::EntryIconPosition::Primary, !uri.is_empty());
    let tip = if uri.starts_with("https://") {
        Some("Secure connection — click for details")
    } else if uri.starts_with("http://") {
        Some("Not secure — click for details")
    } else if uri.is_empty() {
        Some("Search or enter an address")
    } else {
        None
    };
    entry.set_icon_tooltip_text(gtk4::EntryIconPosition::Primary, tip);
}

fn window_title_for(tab_view: &adw::TabView) -> String {
    let title = tab_view.selected_page().map(|p| p.title().to_string()).unwrap_or_default();
    if title.is_empty() { "br0x".to_owned() } else { format!("{title} — br0x") }
}

const MAX_CLOSED_TABS: usize = 10;

/// What one find request should do. `active` is the text the selected tab's
/// controller is searching for right now.
#[derive(Debug, PartialEq, Eq)]
enum FindStep {
    Start,
    Next,
    Previous,
    Clear,
}

/// Decide the step for `query`. A query the controller is not searching yet
/// must start a search: next and previous before a search are a WebKit
/// programming error, so after a tab switch they would do nothing.
fn find_step(query: &str, active: &str, forward: bool, fresh: bool) -> FindStep {
    if query.trim().is_empty() {
        FindStep::Clear
    } else if fresh || active != query {
        FindStep::Start
    } else if forward {
        FindStep::Next
    } else {
        FindStep::Previous
    }
}

struct Shell {
    tab_view: adw::TabView,
    entry: gtk4::Entry,
    window: adw::ApplicationWindow,
    back: gtk4::Button,
    fwd: gtk4::Button,
    reload: gtk4::Button,
    progress: gtk4::ProgressBar,
    read_progress: gtk4::ProgressBar,
    engine_btn: gtk4::MenuButton,
    bookmark_btn: gtk4::Button,
    shield_btn: gtk4::ToggleButton,
    key_btn: gtk4::Button,
    reader_btn: gtk4::Button,
    find_bar: gtk4::SearchBar,
    find_entry: gtk4::SearchEntry,
    find_status: gtk4::Label,
    toasts: adw::ToastOverlay,
    zoom_toast: RefCell<Option<adw::Toast>>,
    last_session: RefCell<Vec<StoredTab>>,
    last_sample: RefCell<Option<(Instant, br0x_core::tab::SysState)>>,
    /// Engine and frequent list the start page file was last built from.
    last_newtab: RefCell<Option<(SearchEngine, Vec<Visit>)>>,
    suggest_pop: gtk4::Popover,
    bm_list: gtk4::ListBox,
    sidebar_box: gtk4::Box,
    sidebar_list: gtk4::ListBox,
    sidebar_pages: RefCell<Vec<adw::TabPage>>,
    sidebar_visible: RefCell<bool>,
    sidebar_rail: RefCell<bool>,
    key_pop: gtk4::Popover,
    key_list: gtk4::ListBox,
    shield: Rc<RefCell<ShellShield>>,
    curtain: Rc<RefCell<ShellCurtain>>,
    vault: Rc<RefCell<ShellVault>>,
    picker_armed: RefCell<bool>,
    shield_sync: RefCell<bool>,
    popouts: RefCell<Vec<gtk4::Window>>,
    history: Rc<RefCell<Option<History>>>,
    bookmarks: Rc<RefCell<Vec<Bookmark>>>,
    bookmarks_store: BookmarkStore,
    context: webkit6::WebContext,
    session: webkit6::NetworkSession,
    prefs: RefCell<Prefs>,
    prefs_store: PrefsStore,
    tabs: Rc<RefCell<Tabs>>,
}

impl Shell {
    /// Create a tab, record it, connect its signals, optionally select it.
    /// Loading is left to callers. `related` shares the source tab's web
    /// process, as WebKit requires for window.open and target=_blank.
    fn create_tab(
        self: &Rc<Self>,
        related: Option<&webkit6::WebView>,
        select: bool,
    ) -> (webkit6::WebView, adw::TabPage) {
        // Views related to another view inherit its context and session;
        // passing them again makes WebKit complain and ignore the values.
        // Settings are not inherited, so both branches set them explicitly.
        let settings = browser_settings();
        let view = match related {
            Some(source) => {
                webkit6::WebView::builder().related_view(source).settings(&settings).build()
            }
            None => webkit6::WebView::builder()
                .web_context(&self.context)
                .network_session(&self.session)
                .settings(&settings)
                .build(),
        };
        attach_filter(&view);
        let page = self.tab_view.append(&view);
        page.set_title("New Tab");
        let id = self.tabs.borrow_mut().new_id();
        let meta = TabMeta::fresh(id);
        self.connect_view(&view, &page);
        self.tabs.borrow_mut().push(meta, view.clone(), page.clone());
        if select {
            self.tab_view.set_selected_page(&page);
        }
        self.refresh_sidebar();
        (view, page)
    }

    /// Open the start page in a fresh tab. Used by Ctrl+T, the new tab
    /// button, and the empty state.
    fn add_blank_tab(self: &Rc<Self>) {
        let (view, _) = self.create_tab(None, true);
        let engine = self.prefs.borrow().engine;
        let url = self.refresh_start_page(engine);
        view.load_uri(&url);
    }

    /// Top sites for the start page. Empty when history is unavailable.
    fn frequent_sites(&self) -> Vec<Visit> {
        self.history.borrow().as_ref().and_then(|h| h.top(8).ok()).unwrap_or_default()
    }

    /// Start page for `engine` and the current frequent sites, rewritten
    /// only when either changed since the last write. Returns its file URL.
    fn refresh_start_page(self: &Rc<Self>, engine: SearchEngine) -> String {
        let frequent = self.frequent_sites();
        sync_newtab_page(&self.last_newtab, engine, &frequent)
    }

    fn add_tab(self: &Rc<Self>, url: &str, select: bool, load: bool, title: Option<&str>) {
        let (view, page) = self.create_tab(None, select);
        if let Some(t) = title
            && !t.is_empty()
        {
            page.set_title(t);
        }
        if load {
            view.load_uri(&search::resolve(url, self.prefs.borrow().engine));
        } else {
            let pending = search::resolve(url, self.prefs.borrow().engine);
            if let Some(entry) = self.tabs.borrow_mut().entries.last_mut() {
                entry.meta.parked = true;
                entry.meta.pending_url = Some(pending);
            }
            view.load_uri("about:blank");
        }
    }

    fn find_in_page(&self, query: &str, forward: bool, fresh: bool) {
        let Some(view) = selected_view(&self.tab_view) else {
            return;
        };
        let Some(controller) = view.find_controller() else {
            return;
        };
        let active = controller.text().map(|text| text.to_string()).unwrap_or_default();
        match find_step(query, &active, forward, fresh) {
            // An empty query would highlight every node in the page, so it
            // finishes the search instead: that is what drops the highlights.
            FindStep::Clear => controller.search_finish(),
            FindStep::Start => {
                let options = (webkit6::FindOptions::CASE_INSENSITIVE
                    | webkit6::FindOptions::WRAP_AROUND)
                    .bits();
                controller.search(query, options, u32::MAX);
            }
            FindStep::Next => controller.search_next(),
            FindStep::Previous => controller.search_previous(),
        }
    }

    /// Stop/Reload morph for the shared header button.
    fn update_reload_icon(&self) {
        let loading = selected_view(&self.tab_view).map(|v| v.is_loading()).unwrap_or(false);
        self.reload.set_icon_name(if loading {
            "process-stop-symbolic"
        } else {
            "view-refresh-symbolic"
        });
        self.reload.set_tooltip_text(Some(if loading {
            "Stop (Escape)"
        } else {
            "Reload (Ctrl+R / F5)"
        }));
    }

    /// Star on/off for the current page. Internal pages cannot be saved.
    fn toggle_bookmark(self: &Rc<Self>) {
        let Some(view) = selected_view(&self.tab_view) else {
            return;
        };
        let Some(uri) = view.uri().map(|u| u.to_string()) else {
            return;
        };
        if is_blank_uri(&uri) || uri.starts_with("br0x://") {
            self.toasts.add_toast(adw::Toast::new("Open a website first, then bookmark it"));
            return;
        }
        let title = view.title().map(|t| t.to_string()).unwrap_or_default();
        {
            let mut bookmarks = self.bookmarks.borrow_mut();
            let removed = BookmarkStore::is_bookmarked(&bookmarks, &uri);
            if removed {
                BookmarkStore::remove(&mut bookmarks, &uri);
                self.toasts.add_toast(adw::Toast::new("Bookmark removed"));
            } else {
                BookmarkStore::upsert(&mut bookmarks, &uri, &title);
                self.toasts.add_toast(adw::Toast::new("Bookmarked"));
            };
            if let Err(e) = self.bookmarks_store.save(&bookmarks) {
                eprintln!("br0x: bookmarks save failed: {e}");
            }
            refresh_bookmark_icon(&self.bookmark_btn, &bookmarks, &uri);
        }
        self.refresh_bookmarks_menu();
    }

    /// Rebuild the header bookmarks menu from the store. Called at startup
    /// and after every toggle, so the menu never shows stale entries.
    fn refresh_bookmarks_menu(&self) {
        let bookmarks = self.bookmarks.borrow();
        while let Some(row) = self.bm_list.row_at_index(0) {
            self.bm_list.remove(&row);
        }
        if bookmarks.is_empty() {
            let label = gtk4::Label::new(Some("No bookmarks yet — press Ctrl+D"));
            label.add_css_class("dim-label");
            label.set_margin_top(8);
            label.set_margin_bottom(8);
            label.set_margin_start(12);
            label.set_margin_end(12);
            // Placeholder, not a row: never selectable, never activated.
            self.bm_list.set_placeholder(Some(&label));
            return;
        }
        self.bm_list.set_placeholder(None::<&gtk4::Widget>);
        for mark in bookmarks.iter() {
            let name = if mark.title.is_empty() {
                display_domain(&mark.url).to_owned()
            } else {
                mark.title.clone()
            };
            let stack = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
            stack.set_margin_top(4);
            stack.set_margin_bottom(4);
            let title = gtk4::Label::new(Some(&name));
            title.set_xalign(0.0);
            title.set_hexpand(true);
            title.set_max_width_chars(48);
            title.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            let domain = gtk4::Label::new(Some(display_domain(&mark.url)));
            domain.set_xalign(0.0);
            domain.set_hexpand(true);
            domain.set_max_width_chars(48);
            domain.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            domain.add_css_class("dim-label");
            stack.append(&title);
            stack.append(&domain);
            let row = gtk4::ListBoxRow::new();
            row.set_child(Some(&stack));
            row.set_activatable(true);
            self.bm_list.append(&row);
        }
    }

    /// Reload tabs currently showing the start page (fresh data after an
    /// engine switch).
    fn reload_start_pages(&self) {
        let start = newtab_url();
        for entry in self.tabs.borrow().entries.iter() {
            if entry.view.uri().is_some_and(|u| u.as_str() == start) {
                entry.view.reload();
            }
        }
    }

    /// Sync the star with the selected tab. Call after navigation and
    /// selection changes.
    fn refresh_bookmark(&self) {
        let uri = selected_view(&self.tab_view)
            .and_then(|v| v.uri().map(|u| u.to_string()))
            .unwrap_or_default();
        refresh_bookmark_icon(&self.bookmark_btn, &self.bookmarks.borrow(), &uri);
    }

    /// Open tabs matching `needle` for the palette: (title, url, page).
    fn open_tab_hits(&self, needle: &str) -> Vec<(String, String, adw::TabPage)> {
        let query = needle.to_lowercase();
        let tabs = self.tabs.borrow();
        let mut out = Vec::new();
        for entry in tabs.entries.iter() {
            let title = entry.page.title().to_string();
            let url = entry
                .view
                .uri()
                .map(|uri| uri.to_string())
                .or_else(|| entry.meta.pending_url.clone())
                .unwrap_or_default();
            if title.to_lowercase().contains(&query) || url.to_lowercase().contains(&query) {
                out.push((title, url, entry.page.clone()));
                if out.len() >= 4 {
                    break;
                }
            }
        }
        out
    }

    /// Apply the current domain's blocker state to `view`. Navigations call
    /// this on Started; the toggle calls it for the current view.
    fn apply_shield_to_view(&self, view: &webkit6::WebView) {
        let uri = view.uri().map(|u| u.to_string()).unwrap_or_default();
        set_view_filter(view, self.shield.borrow().is_enabled(&domain_of(&uri)));
    }

    /// Reflect the selected tab's domain in the shield toggle.
    fn refresh_shield_ui(&self) {
        let uri = selected_view(&self.tab_view)
            .and_then(|v| v.uri().map(|u| u.to_string()))
            .unwrap_or_default();
        let domain = domain_of(&uri);
        let enabled = self.shield.borrow().is_enabled(&domain);
        *self.shield_sync.borrow_mut() = true;
        self.shield_btn.set_active(enabled);
        *self.shield_sync.borrow_mut() = false;
        let state = if enabled { "on" } else { "off" };
        self.shield_btn.set_tooltip_text(Some(&format!(
            "Tracker blocker {state} for {domain} — click to flip"
        )));
    }

    /// Flip the blocker for the selected tab's domain (default on).
    fn toggle_shield_to(self: &Rc<Self>, enabled: bool) {
        let Some(view) = selected_view(&self.tab_view) else {
            self.refresh_shield_ui();
            return;
        };
        let domain = domain_of(&view.uri().map(|u| u.to_string()).unwrap_or_default());
        if domain.is_empty() {
            self.toasts.add_toast(adw::Toast::new("Open a website first, then flip its blocker"));
            self.refresh_shield_ui();
            return;
        }
        self.shield.borrow_mut().set(&domain, enabled);
        self.apply_shield_to_view(&view);
        self.refresh_shield_ui();
        let state = if enabled { "on" } else { "off" };
        self.toasts.add_toast(adw::Toast::new(&format!("Blocker {state} for {domain}")));
    }

    /// Install the site's curtain selectors as an early user stylesheet so
    /// hidden elements never flash on future loads.
    fn apply_curtain_to_view(&self, view: &webkit6::WebView) {
        let uri = view.uri().map(|u| u.to_string()).unwrap_or_default();
        let selectors = self.curtain.borrow().selectors_for(&domain_of(&uri));
        if let Some(ucm) = view.user_content_manager() {
            ucm.remove_all_style_sheets();
            if !selectors.is_empty() {
                ucm.add_style_sheet(&webkit6::UserStyleSheet::new(
                    &curtain_css(&selectors),
                    webkit6::UserContentInjectedFrames::AllFrames,
                    webkit6::UserStyleLevel::User,
                    &[],
                    &[],
                ));
            }
        }
    }

    /// Arm or cancel the curtain picker (Ctrl+Shift+H).
    fn curtain_pick_toggle(self: &Rc<Self>) {
        if *self.picker_armed.borrow() {
            self.disarm_picker();
            self.toasts.add_toast(adw::Toast::new("Picker cancelled"));
            return;
        }
        let Some(view) = selected_view(&self.tab_view) else {
            self.toasts.add_toast(adw::Toast::new("Open a website first, then pick an element"));
            return;
        };
        *self.picker_armed.borrow_mut() = true;
        eval_text(&view, CURTAIN_ARM_JS, |_| {});
        self.toasts.add_toast(adw::Toast::new("Picker on — click the element to hide"));
        self.poll_picker(view);
    }

    fn disarm_picker(&self) {
        *self.picker_armed.borrow_mut() = false;
        if let Some(view) = selected_view(&self.tab_view) {
            eval_text(&view, CURTAIN_DISARM_JS, |_| {});
        }
    }

    /// Collect the picked selector: the page hides the element at click
    /// time, the shell persists it per site.
    fn poll_picker(self: &Rc<Self>, view: webkit6::WebView) {
        let shell = self.clone();
        let deadline = Instant::now() + std::time::Duration::from_secs(65);
        glib::timeout_add_local(std::time::Duration::from_millis(500), move || {
            if !*shell.picker_armed.borrow() {
                return glib::ControlFlow::Break;
            }
            if Instant::now() > deadline {
                shell.disarm_picker();
                shell.toasts.add_toast(adw::Toast::new("Picker timed out"));
                return glib::ControlFlow::Break;
            }
            let domain = domain_of(&view.uri().map(|u| u.to_string()).unwrap_or_default());
            let shell_next = shell.clone();
            let view_next = view.clone();
            eval_text(&view, CURTAIN_POLL_JS, move |picked| {
                let picked = picked.trim().to_owned();
                if picked.is_empty() || !*shell_next.picker_armed.borrow() {
                    return;
                }
                *shell_next.picker_armed.borrow_mut() = false;
                let added = shell_next.curtain.borrow_mut().hide(&domain, &picked);
                shell_next.apply_curtain_to_view(&view_next);
                shell_next.toasts.add_toast(adw::Toast::new(if added {
                    "Element hidden on this site"
                } else {
                    "Already hidden on this site"
                }));
            });
            glib::ControlFlow::Continue
        });
    }

    /// Forget every hidden selector on the current site and reload it.
    fn curtain_clear_site(self: &Rc<Self>) {
        let Some(view) = selected_view(&self.tab_view) else {
            return;
        };
        let domain = domain_of(&view.uri().map(|u| u.to_string()).unwrap_or_default());
        if self.curtain.borrow_mut().clear_domain(&domain) {
            self.apply_curtain_to_view(&view);
            view.reload();
            self.toasts.add_toast(adw::Toast::new("Unhidden — reloading this site"));
        } else {
            self.toasts.add_toast(adw::Toast::new("Nothing hidden on this site"));
        }
    }

    /// Reader button state follows the selected tab.
    fn refresh_reader_button(&self) {
        let active = self
            .tab_view
            .selected_page()
            .and_then(|page| {
                self.tabs
                    .borrow()
                    .entries
                    .iter()
                    .find(|e| e.page == page)
                    .map(|e| e.meta.reader_src.is_some())
            })
            .unwrap_or(false);
        if active {
            self.reader_btn.add_css_class("suggested-action");
        } else {
            self.reader_btn.remove_css_class("suggested-action");
        }
        self.reader_btn.set_tooltip_text(Some(if active {
            "Reader mode on — click to return (Ctrl+Shift+R)"
        } else {
            "Reader mode (Ctrl+Shift+R)"
        }));
    }

    /// Toggle the article view. Toggling again returns to the stored URL.
    fn toggle_reader(self: &Rc<Self>) {
        let resume = {
            let mut tabs = self.tabs.borrow_mut();
            self.tab_view.selected_page().and_then(|page| {
                tabs.entry_mut(&page).and_then(|entry| {
                    entry.meta.reader_src.take().map(|src| (entry.view.clone(), src))
                })
            })
        };
        if let Some((view, src)) = resume {
            view.load_uri(&src);
            self.refresh_reader_button();
            return;
        }
        let Some(view) = selected_view(&self.tab_view) else {
            self.toasts.add_toast(adw::Toast::new("Open an article first"));
            return;
        };
        let uri = view.uri().map(|u| u.to_string()).unwrap_or_default();
        if !uri.starts_with("http://") && !uri.starts_with("https://") {
            self.toasts.add_toast(adw::Toast::new("Reader works on web articles"));
            return;
        }
        let page = self.tab_view.selected_page();
        let shell = self.clone();
        let eval_view = view.clone();
        eval_text(&eval_view, READER_EXTRACT_JS, move |html| {
            if html.is_empty() {
                shell.toasts.add_toast(adw::Toast::new("No article found on this page"));
                return;
            }
            if let Some(page) = page
                && let Some(entry) = shell.tabs.borrow_mut().entry_mut(&page)
            {
                entry.meta.reader_src = Some(uri.clone());
            }
            view.load_alternate_html(&html, &uri, None);
            shell.refresh_reader_button();
        });
    }

    /// Open the first playing video in a small pop-out window reusing the
    /// same video URL with autoplay.
    fn popout_video(self: &Rc<Self>) {
        let Some(view) = selected_view(&self.tab_view) else {
            self.toasts.add_toast(adw::Toast::new("Open a page with video first"));
            return;
        };
        let shell = self.clone();
        eval_text(&view, VIDEO_FIND_JS, move |url| {
            if url.is_empty() {
                shell.toasts.add_toast(adw::Toast::new("No video found on this page"));
            } else if url.starts_with("blob:") {
                shell
                    .toasts
                    .add_toast(adw::Toast::new("This video cannot leave its page (stream or DRM)"));
            } else {
                shell.open_popout(&url);
            }
        });
    }

    fn open_popout(&self, url: &str) {
        // GTK4 dropped always-on-top window hints, so the pop-out is a small
        // transient window the user can keep visible instead.
        self.popouts.borrow_mut().retain(|win| win.is_visible());
        let win = gtk4::Window::new();
        win.set_title(Some("br0x video"));
        win.set_default_size(480, 300);
        win.set_transient_for(Some(&self.window));
        let settings = browser_settings();
        let player = webkit6::WebView::builder()
            .web_context(&self.context)
            .network_session(&self.session)
            .settings(&settings)
            .build();
        let safe = html_escape(url);
        let html = format!(
            "<!doctype html><html><body style=\"margin:0;background:#000\">\
            <video src=\"{safe}\" controls autoplay \
            style=\"width:100vw;height:100vh;object-fit:contain\"></video>\
            </body></html>"
        );
        player.load_html(&html, Some(url));
        win.set_child(Some(&player));
        win.present();
        self.popouts.borrow_mut().push(win);
    }

    /// Show the key icon only on tabs whose page has a password field.
    fn refresh_key_button(&self) {
        let selected = self.tab_view.selected_page();
        let show = self
            .tabs
            .borrow()
            .entries
            .iter()
            .find(|e| Some(&e.page) == selected.as_ref())
            .is_some_and(|e| e.meta.has_password);
        self.key_btn.set_visible(show);
    }

    /// Rebuild the key popover: saved usernames for this origin plus a row
    /// that saves the current form. Usernames are offered here (from the
    /// address-bar key) rather than as an in-page dropdown.
    fn rebuild_key_pop(&self) {
        while let Some(row) = self.key_list.row_at_index(0) {
            self.key_list.remove(&row);
        }
        let origin = selected_view(&self.tab_view)
            .and_then(|v| v.uri().map(|u| u.to_string()))
            .map(|uri| origin_of(&uri))
            .unwrap_or_default();
        for (username, _) in self.vault.borrow().credentials_for(&origin) {
            let label = gtk4::Label::new(Some(&format!("Fill login as {username}")));
            label.set_xalign(0.0);
            label.set_margin_top(6);
            label.set_margin_bottom(6);
            label.set_margin_start(12);
            label.set_margin_end(12);
            let row = gtk4::ListBoxRow::new();
            row.set_child(Some(&label));
            self.key_list.append(&row);
        }
        let save = gtk4::Label::new(Some("Save this login"));
        save.set_xalign(0.0);
        save.set_margin_top(6);
        save.set_margin_bottom(6);
        save.set_margin_start(12);
        save.set_margin_end(12);
        let row = gtk4::ListBoxRow::new();
        row.set_child(Some(&save));
        self.key_list.append(&row);
    }

    fn fill_login(&self, username: &str, secret: &str) {
        let Some(view) = selected_view(&self.tab_view) else {
            return;
        };
        let user = js_string(username);
        let pass = js_string(secret);
        let script = format!(
            "(()=>{{var u='{user}';var p='{pass}';\
            var pw=document.querySelector('input[type=\"password\"]');if(!pw){{return;}}\
            var t=pw.form?pw.form.querySelector('input[type=\"email\"],input[type=\"text\"]')\
            :document.querySelector('input[type=\"email\"],input[type=\"text\"]');\
            if(t){{t.focus();t.value=u;t.dispatchEvent(new Event('input',{{bubbles:true}}));}}\
            pw.focus();pw.value=p;pw.dispatchEvent(new Event('input',{{bubbles:true}}));}})()"
        );
        eval_text(&view, &script, |_| {});
        self.key_pop.popdown();
        self.toasts.add_toast(adw::Toast::new(&format!("Filled login as {username}")));
    }

    fn save_login(self: &Rc<Self>) {
        let Some(view) = selected_view(&self.tab_view) else {
            return;
        };
        let origin = origin_of(&view.uri().map(|u| u.to_string()).unwrap_or_default());
        if origin.is_empty() {
            return;
        }
        let shell = self.clone();
        eval_text(&view, READ_LOGIN_JS, move |packed| {
            let Some((user, pass)) = packed.split_once('|') else {
                shell.toasts.add_toast(adw::Toast::new("Type your login first, then save"));
                return;
            };
            let username = percent_decode(user);
            let secret = percent_decode(pass);
            if secret.is_empty() {
                shell.toasts.add_toast(adw::Toast::new("Type your password first, then save"));
            } else if shell.vault.borrow_mut().save(&origin, &username, &secret) {
                shell.toasts.add_toast(adw::Toast::new("Login saved on this device"));
            } else {
                shell.toasts.add_toast(adw::Toast::new("Could not save this login"));
            }
            shell.key_pop.popdown();
        });
    }

    /// Rebuild the sidebar rows in tab-strip order. Pinned tabs (and every
    /// tab in rail mode) shrink to icons; the top tab bar keeps working.
    fn refresh_sidebar(&self) {
        if !*self.sidebar_visible.borrow() {
            return;
        }
        while let Some(row) = self.sidebar_list.row_at_index(0) {
            self.sidebar_list.remove(&row);
        }
        let rail = *self.sidebar_rail.borrow();
        let selected = self.tab_view.selected_page();
        let ordered: Vec<adw::TabPage> = {
            let tabs = self.tabs.borrow();
            let mut positioned: Vec<(i32, adw::TabPage)> = tabs
                .entries
                .iter()
                .map(|e| (self.tab_view.page_position(&e.page), e.page.clone()))
                .collect();
            positioned.sort_by_key(|(pos, _)| *pos);
            positioned.into_iter().map(|(_, page)| page).collect()
        };
        let mut pages = Vec::with_capacity(ordered.len());
        for page in ordered {
            pages.push(page.clone());
            let slot = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
            slot.set_margin_top(4);
            slot.set_margin_bottom(4);
            slot.set_margin_start(8);
            slot.set_margin_end(8);
            let icon = page
                .icon()
                .map(|gicon| gtk4::Image::from_gicon(&gicon))
                .unwrap_or_else(|| gtk4::Image::from_icon_name("web-browser-symbolic"));
            slot.append(&icon);
            if !rail && !page.is_pinned() {
                let bare = strip_state_badges(&page.title());
                let label = gtk4::Label::new(None);
                label.set_text(if bare.is_empty() { "New Tab" } else { &bare });
                label.set_xalign(0.0);
                label.set_hexpand(true);
                label.set_max_width_chars(28);
                label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
                slot.append(&label);
            }
            let row = gtk4::ListBoxRow::new();
            row.set_child(Some(&slot));
            self.sidebar_list.append(&row);
            if Some(&page) == selected.as_ref() {
                self.sidebar_list.select_row(Some(&row));
            }
        }
        *self.sidebar_pages.borrow_mut() = pages;
    }

    /// Show or hide the tab sidebar.
    fn toggle_sidebar(self: &Rc<Self>) {
        let visible = !*self.sidebar_visible.borrow();
        *self.sidebar_visible.borrow_mut() = visible;
        self.sidebar_box.set_visible(visible);
        if visible {
            self.refresh_sidebar();
        }
    }

    /// Collapse the sidebar to a thin icon rail, or expand it back.
    fn toggle_rail(&self) {
        let rail = !*self.sidebar_rail.borrow();
        *self.sidebar_rail.borrow_mut() = rail;
        self.sidebar_box.set_size_request(if rail { 52 } else { 220 }, -1);
        self.refresh_sidebar();
    }

    /// One transient toast at a time: rapid repeats (zoom keys) dismiss the
    /// previous toast instead of queueing a trail of them.
    fn show_transient(&self, msg: &str) {
        if let Some(old) = self.zoom_toast.borrow_mut().take() {
            old.dismiss();
        }
        let toast = adw::Toast::new(msg);
        self.zoom_toast.borrow_mut().replace(toast.clone());
        self.toasts.add_toast(toast);
    }

    /// Switch search engine, persist it, and refresh the start page.
    fn set_engine(self: &Rc<Self>, engine: SearchEngine) {
        {
            let mut prefs = self.prefs.borrow_mut();
            if prefs.engine == engine {
                return;
            }
            prefs.engine = engine;
            if let Err(e) = self.prefs_store.save(&prefs) {
                eprintln!("br0x: prefs save failed: {e}");
            }
        }
        // The start page names the engine and posts to its form.
        self.refresh_start_page(engine);
        self.reload_start_pages();
        self.engine_btn.set_label(engine.name());
        self.entry.set_placeholder_text(Some("Search or type a URL"));
        self.toasts.add_toast(adw::Toast::new(&format!("Search engine: {}", engine.name())));
    }

    fn connect_view(self: &Rc<Self>, view: &webkit6::WebView, page: &adw::TabPage) {
        let page_clone = page.clone();
        let window_weak = self.window.downgrade();
        let shell_weak = Rc::downgrade(self);
        view.connect_title_notify(move |v| {
            if let Some(t) = v.title() {
                page_clone.set_title(&t);
                if page_clone.is_selected()
                    && let Some(w) = window_weak.upgrade()
                {
                    w.set_title(Some(&format!("{t} — br0x")));
                }
            }
            if let Some(shell) = shell_weak.upgrade() {
                shell.refresh_sidebar();
            }
        });

        let entry_clone = self.entry.clone();
        let page_clone = page.clone();
        let bookmark_clone = self.bookmark_btn.clone();
        let bookmarks_clone = self.bookmarks.clone();
        view.connect_uri_notify(move |v| {
            if page_clone.is_selected()
                && let Some(u) = v.uri()
            {
                if is_blank_uri(&u) {
                    entry_clone.set_text("");
                    set_security_icon(&entry_clone, "");
                } else {
                    entry_clone.set_text(u.as_str());
                    set_security_icon(&entry_clone, u.as_str());
                }
                refresh_bookmark_icon(&bookmark_clone, &bookmarks_clone.borrow(), u.as_str());
            }
        });

        // Real favicons in the tab strip. WebKit hands us a texture; an
        // in-memory PNG wrapped in a BytesIcon is what AdwTabPage accepts.
        let page_clone = page.clone();
        view.connect_favicon_notify(move |v| {
            let Some(texture) = v.favicon() else {
                return;
            };
            if texture.width() == 0 || texture.height() == 0 {
                return;
            }
            let icon = gio::BytesIcon::new(&texture.save_to_png_bytes());
            page_clone.set_icon(Some(&icon));
        });

        // Match count for the find bar, only while this tab is selected.
        if let Some(controller) = view.find_controller() {
            let status = self.find_status.clone();
            let page_clone = page.clone();
            controller.connect_found_text(move |_, count| {
                if page_clone.is_selected() {
                    status.set_text(&format!("{count} matches"));
                }
            });
            let status = self.find_status.clone();
            let page_clone = page.clone();
            controller.connect_failed_to_find_text(move |_| {
                if page_clone.is_selected() {
                    status.set_text("No matches");
                }
            });
        }

        let page_clone = page.clone();
        let prog = self.progress.clone();
        let tv_weak = self.tab_view.downgrade();
        let back_clone = self.back.clone();
        let fwd_clone = self.fwd.clone();
        let reload_clone = self.reload.clone();
        let history = self.history.clone();
        let shell_started = self.clone();
        let page_started = page.clone();
        view.connect_load_changed(move |v, event| {
            let loading = event != webkit6::LoadEvent::Finished;
            page_clone.set_loading(loading);
            if page_clone.is_selected() {
                prog.set_visible(loading);
                if loading {
                    prog.set_fraction(v.estimated_load_progress());
                }
                reload_clone.set_icon_name(if loading {
                    "process-stop-symbolic"
                } else {
                    "view-refresh-symbolic"
                });
            }
            // Nav buttons only depend on history state, so refresh them on
            // loading transitions instead of every progress event.
            let transition =
                event == webkit6::LoadEvent::Started || event == webkit6::LoadEvent::Finished;
            if transition
                && page_clone.is_selected()
                && let Some(tv) = tv_weak.upgrade()
            {
                refresh_nav(&tv, &back_clone, &fwd_clone);
            }
            if event == webkit6::LoadEvent::Started {
                // New navigation on this domain: apply its blocker state and
                // its curtain list early, drop stale per-page flags. The
                // reader page itself carries its source URL, so returning to
                // it never clears the stored source.
                if let Some(entry) = shell_started.tabs.borrow_mut().entry_mut(&page_started) {
                    if let Some(src) = entry.meta.reader_src.clone()
                        && v.uri().is_some_and(|uri| uri.as_str() != src)
                    {
                        entry.meta.reader_src = None;
                    }
                    entry.meta.has_password = false;
                }
                shell_started.apply_shield_to_view(v);
                shell_started.apply_curtain_to_view(v);
                shell_started.refresh_reader_button();
                shell_started.refresh_key_button();
            }
            if event == webkit6::LoadEvent::Finished
                && let Some(uri) = v.uri()
                && !is_blank_uri(&uri)
                && !uri.as_str().starts_with("br0x://")
                && let Some(h) = history.borrow().as_ref()
            {
                let title = v.title().map(|t| t.to_string()).unwrap_or_default();
                let now = glib::DateTime::now_local().map(|d| d.to_unix()).unwrap_or(0);
                let _ = h.record(uri.as_str(), &title, now);
            }
            if event == webkit6::LoadEvent::Finished {
                let shell_done = shell_started.clone();
                let page_done = page_started.clone();
                eval_flag(v, HAS_PASSWORD_JS, move |has| {
                    if let Some(entry) = shell_done.tabs.borrow_mut().entry_mut(&page_done) {
                        entry.meta.has_password = has;
                    }
                    shell_done.refresh_key_button();
                });
            }
        });

        // Smooth progress between load_changed events; hides when done.
        {
            let prog = self.progress.clone();
            let page_clone = page.clone();
            view.connect_estimated_load_progress_notify(move |v| {
                if page_clone.is_selected() {
                    if v.is_loading() {
                        prog.set_visible(true);
                        prog.set_fraction(v.estimated_load_progress());
                    } else {
                        prog.set_visible(false);
                    }
                }
            });
        }

        {
            let prog = self.progress.clone();
            let page_clone = page.clone();
            let reload_clone = self.reload.clone();
            let toasts_failed = self.toasts.clone();
            view.connect_load_failed(move |v, _, uri, err| {
                eprintln!("br0x: load failed {uri}: {err}");
                if page_clone.is_selected() {
                    prog.set_visible(false);
                    reload_clone.set_icon_name("view-refresh-symbolic");
                }
                // A cancelled load is the user or a redirect stopping it, not
                // a failure worth a toast.
                if !err.matches(webkit6::NetworkError::Cancelled)
                    && !err.matches(gio::IOErrorEnum::Cancelled)
                {
                    toasts_failed.add_toast(adw::Toast::new("Load failed"));
                    v.load_alternate_html(
                        &error_html(
                            "Could not open this page",
                            "Check the address and your connection, then try again.",
                            uri,
                        ),
                        uri,
                        None,
                    );
                    return true;
                }
                false
            });
        }
        let toasts_crashed = self.toasts.clone();
        view.connect_web_process_terminated(move |v, reason| {
            eprintln!("br0x: web process terminated: {reason:?}");
            // Parking terminates the process on purpose; only crashes and OOM
            // kills are worth telling the user about.
            if reason != webkit6::WebProcessTerminationReason::TerminatedByApi {
                toasts_crashed.add_toast(adw::Toast::new("Page crashed"));
                let uri = v.uri().map(|u| u.to_string()).unwrap_or_default();
                v.load_alternate_html(
                    &error_html(
                        "This page crashed",
                        "The page used too much memory or hit a bug. Your other tabs are safe.",
                        &uri,
                    ),
                    &uri,
                    None,
                );
            }
        });

        // Clicking the page dismisses the suggestion list. Capture phase
        // only observes: the handler returns nothing, so page clicks
        // behave exactly as before.
        let pop = self.suggest_pop.clone();
        let page_click = gtk4::GestureClick::new();
        page_click.set_propagation_phase(gtk4::PropagationPhase::Capture);
        page_click.connect_pressed(move |_, _, _, _| {
            pop.popdown();
        });
        view.add_controller(page_click);

        // target=_blank and window.open land here. Returning the new view
        // makes WebKit load the request into it, form submission and all;
        // loading the URI here as well would navigate the tab twice.
        let shell_weak = Rc::downgrade(self);
        view.connect_create(move |source, _| {
            let shell = shell_weak.upgrade()?;
            let (new_view, _) = shell.create_tab(Some(source), true);
            Some(new_view.upcast())
        });
    }

    /// Selected tab changed: sync entry, title, nav, resume released tabs.
    fn on_selection_changed(&self) {
        // Decide under the borrow, act after it: loading a URI re-enters this
        // handler through signals, and a held borrow would panic.
        let resume = {
            let mut tabs = self.tabs.borrow_mut();
            self.tab_view.selected_page().and_then(|page| {
                let entry = tabs.entry_mut(&page)?;
                entry.meta.last_active = Instant::now();
                // Sleeping and parked tabs restore the same way; only the
                // badge differs.
                let url = if entry.meta.parked || entry.meta.sleeping {
                    entry.meta.parked = false;
                    entry.meta.sleeping = false;
                    entry.meta.restored_at = Some(Instant::now());
                    entry.meta.pending_url.take()
                } else {
                    None
                };
                entry.view.set_is_muted(false);
                entry.page.set_indicator_icon(None::<&gio::ThemedIcon>);
                entry.page.set_indicator_tooltip("");
                Some((entry.view.clone(), url))
            })
        };
        if let Some((view, Some(url))) = resume {
            view.load_uri(&url);
        }
        self.find_status.set_text("");
        sync_entry_to_selection(&self.tab_view, &self.entry);
        refresh_nav(&self.tab_view, &self.back, &self.fwd);
        self.refresh_bookmark();
        self.refresh_shield_ui();
        self.refresh_key_button();
        self.refresh_reader_button();
        self.refresh_sidebar();
        self.read_progress.set_visible(false);
        self.window.set_title(Some(&window_title_for(&self.tab_view)));
        self.update_reload_icon();
        // Keep the hairline progress in sync when switching tabs.
        if let Some(view) = selected_view(&self.tab_view) {
            if view.is_loading() {
                self.progress.set_visible(true);
                self.progress.set_fraction(view.estimated_load_progress());
            } else {
                self.progress.set_visible(false);
            }
        } else {
            self.progress.set_visible(false);
        }
    }

    /// 5s tick: sample pressure, ask core, freeze or park background tabs.
    fn enforce_tick(&self) {
        // /proc reads every tick add up: reuse a sample younger than 15 s.
        let tabs = self.tab_view.n_pages() as usize;
        let sys = match &mut *self.last_sample.borrow_mut() {
            Some((at, sys)) if at.elapsed().as_secs() < 15 => {
                sys.tab_count = tabs;
                *sys
            }
            slot => {
                let sys = br0x_core::sampler::sample(tabs);
                *slot = Some((Instant::now(), sys));
                sys
            }
        };
        let selected = self.tab_view.selected_page();
        // Decide under the borrow, kill processes after it: terminating a
        // process re-enters the shell through signals.
        let to_park = {
            let mut tabs = self.tabs.borrow_mut();
            let snaps: Vec<TabSnapshot> =
                tabs.entries.iter().map(|e| e.meta.snapshot(e.view.is_playing_audio())).collect();
            let mut to_park = Vec::new();
            for (id, action) in policy::sweep(&snaps, &sys) {
                let Some(entry) = tabs.entry_by_id(id) else {
                    continue;
                };
                if Some(&entry.page) == selected.as_ref() {
                    continue;
                }
                match action {
                    Action::Keep => {}
                    Action::Freeze => {
                        // Mute frozen background tabs; selection unmutes.
                        // The speaker badge explains the silence.
                        entry.view.set_is_muted(true);
                        entry.page.set_indicator_icon(Some(&gio::ThemedIcon::new(
                            "audio-volume-muted-symbolic",
                        )));
                        entry.page.set_indicator_tooltip(
                            "Muted while this tab is frozen — open it to resume",
                        );
                    }
                    Action::Park => {
                        // Timed release without pressure sleeps; a release
                        // under pressure parks. Ask core with the sleep
                        // threshold pinned to the park timeout so the sleep
                        // decision, not the clock alone, picks the badge.
                        let sleeping = snaps.iter().find(|s| s.id == id).is_some_and(|snap| {
                            let params = policy::params_for(sys.tab_count);
                            let sleepy = br0x_core::tab::PolicyParams {
                                sleep_secs: params.park_secs,
                                ..params
                            };
                            policy::decide_with_params(snap, &sys, &sleepy) == Action::Sleep
                        });
                        if let Some(view) = release_entry(entry, sleeping) {
                            to_park.push(view);
                        }
                    }
                    Action::Sleep => {
                        if let Some(view) = release_entry(entry, true) {
                            to_park.push(view);
                        }
                    }
                }
            }
            to_park
        };
        for view in to_park {
            view.terminate_web_process();
        }
    }

    fn save_session(&self) {
        let tabs = self.tabs.borrow();
        let stored: Vec<StoredTab> = tabs
            .entries
            .iter()
            .map(|e| {
                // A parked tab has no URI: its identity lives in pending_url.
                let url = match e.view.uri() {
                    Some(uri) if !is_blank_uri(&uri) => uri.to_string(),
                    _ => e.meta.pending_url.clone().unwrap_or_default(),
                };
                StoredTab {
                    id: e.meta.id.0,
                    url,
                    title: e.page.title().to_string(),
                    order: self.tab_view.page_position(&e.page).max(0) as usize,
                    pinned: e.page.is_pinned(),
                    scroll_y: 0,
                }
            })
            // Start pages and internal pages are not worth restoring.
            .filter(|t| !t.url.is_empty() && !is_blank_uri(&t.url) && !t.url.starts_with("br0x://"))
            .collect();
        // The timer fires every 30 s whether or not anything changed: skip
        // the write (and its two fsyncs) when the tab set is identical.
        if stored == *self.last_session.borrow() {
            return;
        }
        *self.last_session.borrow_mut() = stored.clone();
        let store = SessionStore::new(session_path());
        if let Err(e) = store.save(&Session { tabs: stored }) {
            eprintln!("br0x: session save failed: {e}");
        }
    }

    /// Load the saved tab set into this window. Returns how many tabs it
    /// added, skipping URLs that are already open.
    fn restore_previous_session(self: &Rc<Self>) -> usize {
        let store = SessionStore::new(session_path());
        let Ok(mut session) = store.load() else {
            return 0;
        };
        session.tabs.retain(|t| !t.url.is_empty());
        session.tabs.sort_by_key(|t| t.order);
        // A parked tab sits on about:blank, so its URL has to come from
        // pending_url as well: otherwise a restore would add a second copy
        // of every parked tab.
        let mut open: HashSet<String> = self
            .tabs
            .borrow()
            .entries
            .iter()
            .flat_map(|e| [e.view.uri().map(|uri| uri.to_string()), e.meta.pending_url.clone()])
            .flatten()
            .filter(|uri| !is_blank_uri(uri))
            .collect();
        let window_empty = self.tab_view.n_pages() == 0;
        let mut added = 0;
        for tab in &session.tabs {
            if !open.insert(tab.url.clone()) {
                continue;
            }
            let select = window_empty && added == 0;
            self.add_tab(&tab.url, select, select, Some(&tab.title));
            if tab.pinned {
                let page = self.tabs.borrow_mut().entries.last_mut().map(|entry| {
                    entry.meta.pinned = true;
                    entry.page.clone()
                });
                if let Some(page) = page {
                    self.tab_view.set_page_pinned(&page, true);
                }
            }
            added += 1;
        }
        added
    }

    /// Startup: restore only when the user asked for it.
    fn start_session(self: &Rc<Self>) {
        let engine = self.prefs.borrow().engine;
        self.refresh_start_page(engine);
        let restore = self.prefs.borrow().restore_session;
        if !restore || self.restore_previous_session() == 0 {
            self.add_blank_tab();
        }
    }

    fn set_restore_session(self: &Rc<Self>, restore: bool) {
        let mut prefs = self.prefs.borrow_mut();
        prefs.restore_session = restore;
        if let Err(e) = self.prefs_store.save(&prefs) {
            eprintln!("br0x: prefs save failed: {e}");
        }
    }

    fn install_actions(self: &Rc<Self>, app: &adw::Application) {
        let add = |name: &str, accels: &[&str], f: Box<dyn Fn()>| {
            let action = gio::SimpleAction::new(name, None);
            action.connect_activate(move |_, _| f());
            self.window.add_action(&action);
            app.set_accels_for_action(&format!("win.{name}"), accels);
        };

        let s = self.clone();
        add(
            "new-tab",
            &["<Control>t"],
            Box::new(move || {
                s.add_blank_tab();
                s.entry.grab_focus();
            }),
        );
        let s = self.clone();
        add(
            "reopen-tab",
            &["<Control><Shift>t"],
            Box::new(move || {
                let url = s.tabs.borrow_mut().closed.pop();
                if let Some(url) = url {
                    s.add_tab(&url, true, true, None);
                } else {
                    s.toasts.add_toast(adw::Toast::new("No closed tabs"));
                }
            }),
        );
        let s = self.clone();
        add(
            "history",
            &["<Control>h"],
            Box::new(move || {
                s.add_tab("br0x://history", true, true, Some("History"));
            }),
        );
        let s = self.clone();
        add(
            "restore-prev",
            &[],
            Box::new(move || {
                let count = s.restore_previous_session();
                s.toasts.add_toast(adw::Toast::new(&if count == 0 {
                    "No saved session".to_string()
                } else {
                    format!("Restored {count} tabs")
                }));
            }),
        );
        let s = self.clone();
        add(
            "toggle-pin",
            &["<Control><Shift>p"],
            Box::new(move || {
                if let Some(page) = s.tab_view.selected_page() {
                    let pinned = !page.is_pinned();
                    s.tab_view.set_page_pinned(&page, pinned);
                    if let Some(entry) = s.tabs.borrow_mut().entry_mut(&page) {
                        entry.meta.pinned = pinned;
                    }
                    s.refresh_sidebar();
                    s.toasts.add_toast(adw::Toast::new(if pinned {
                        "Tab pinned, exempt from parking"
                    } else {
                        "Tab unpinned"
                    }));
                }
            }),
        );
        let restore_action = gio::SimpleAction::new_stateful(
            "restore-session",
            None,
            &self.prefs.borrow().restore_session.to_variant(),
        );
        {
            let s = self.clone();
            restore_action.connect_activate(move |action, _| {
                let new = !action.state().and_then(|v| v.get::<bool>()).unwrap_or(false);
                action.set_state(&new.to_variant());
                s.set_restore_session(new);
            });
        }
        self.window.add_action(&restore_action);
        let engine_action = gio::SimpleAction::new("set-engine", Some(glib::VariantTy::UINT32));
        {
            let s = self.clone();
            engine_action.connect_activate(move |_, param| {
                if let Some(idx) = param.and_then(|p| p.get::<u32>())
                    && let Some(engine) = SearchEngine::ALL.get(idx as usize)
                {
                    s.set_engine(*engine);
                }
            });
        }
        self.window.add_action(&engine_action);
        let s = self.clone();
        add(
            "close-tab",
            &["<Control>w"],
            Box::new(move || {
                if let Some(page) = s.tab_view.selected_page() {
                    s.tab_view.close_page(&page);
                }
            }),
        );
        let s = self.clone();
        add(
            "next-tab",
            &["<Control>Tab"],
            Box::new(move || {
                s.tab_view.select_next_page();
            }),
        );
        let s = self.clone();
        add(
            "prev-tab",
            &["<Control><Shift>Tab"],
            Box::new(move || {
                s.tab_view.select_previous_page();
            }),
        );
        let s = self.clone();
        add(
            "quit",
            &["<Control>q"],
            Box::new(move || {
                s.window.close();
            }),
        );
        let s = self.clone();
        add(
            "open-address-new-tab",
            &["<Alt>Return"],
            Box::new(move || {
                let url = s.entry.text().to_string();
                if !url.trim().is_empty() {
                    s.add_tab(&url, true, true, None);
                }
            }),
        );
        let s = self.clone();
        add(
            "reload",
            &["<Control>r", "F5"],
            Box::new(move || {
                if let Some(v) = selected_view(&s.tab_view) {
                    v.reload();
                }
            }),
        );
        let s = self.clone();
        add(
            "find",
            &["<Control>f"],
            Box::new(move || {
                s.find_bar.set_search_mode(true);
                s.find_entry.grab_focus();
                let text = s.find_entry.text().to_string();
                if !text.is_empty() {
                    s.find_in_page(&text, true, true);
                }
            }),
        );
        let s = self.clone();
        add(
            "find-next",
            &["<Control>g"],
            Box::new(move || {
                let text = s.find_entry.text().to_string();
                s.find_in_page(&text, true, false);
            }),
        );
        let s = self.clone();
        add(
            "find-prev",
            &["<Control><Shift>g"],
            Box::new(move || {
                let text = s.find_entry.text().to_string();
                s.find_in_page(&text, false, false);
            }),
        );
        let s = self.clone();
        add(
            "stop",
            &["Escape"],
            Box::new(move || {
                if s.find_bar.is_search_mode() {
                    s.find_bar.set_search_mode(false);
                    if let Some(v) = selected_view(&s.tab_view) {
                        v.grab_focus();
                    }
                    return;
                }
                if let Some(v) = selected_view(&s.tab_view) {
                    v.stop_loading();
                }
            }),
        );
        let s = self.clone();
        add(
            "focus-url",
            &["<Control>l", "<Control>k"],
            Box::new(move || {
                s.entry.grab_focus();
                s.entry.select_region(0, -1);
            }),
        );
        let s = self.clone();
        add(
            "bookmark-page",
            &["<Control>d"],
            Box::new(move || {
                s.toggle_bookmark();
            }),
        );
        let s = self.clone();
        add(
            "toggle-shield",
            &["<Control><Shift>s"],
            Box::new(move || {
                let enabled = selected_view(&s.tab_view)
                    .and_then(|v| v.uri().map(|u| u.to_string()))
                    .map(|uri| !s.shield.borrow().is_enabled(&domain_of(&uri)))
                    .unwrap_or(true);
                s.toggle_shield_to(enabled);
            }),
        );
        let s = self.clone();
        add(
            "curtain-pick",
            &["<Control><Shift>h"],
            Box::new(move || {
                s.curtain_pick_toggle();
            }),
        );
        let s = self.clone();
        add(
            "curtain-clear",
            &[],
            Box::new(move || {
                s.curtain_clear_site();
            }),
        );
        let s = self.clone();
        add(
            "reader",
            &["<Control><Shift>r"],
            Box::new(move || {
                s.toggle_reader();
            }),
        );
        let s = self.clone();
        add(
            "popout",
            &["<Control><Shift>o"],
            Box::new(move || {
                s.popout_video();
            }),
        );
        let s = self.clone();
        add(
            "toggle-sidebar",
            &["F9"],
            Box::new(move || {
                s.toggle_sidebar();
            }),
        );
        let s = self.clone();
        add(
            "copy-url",
            &[],
            Box::new(move || {
                if let Some(v) = selected_view(&s.tab_view)
                    && let Some(uri) = v.uri()
                {
                    s.window.clipboard().set_text(uri.as_str());
                    s.toasts.add_toast(adw::Toast::new("Address copied"));
                }
            }),
        );
        let s = self.clone();
        add(
            "back",
            &["<Alt>Left"],
            Box::new(move || {
                if let Some(v) = selected_view(&s.tab_view) {
                    v.go_back();
                }
            }),
        );
        let s = self.clone();
        add(
            "forward",
            &["<Alt>Right"],
            Box::new(move || {
                if let Some(v) = selected_view(&s.tab_view) {
                    v.go_forward();
                }
            }),
        );
        let s = self.clone();
        add(
            "zoom-in",
            &["<Control>plus", "<Control>equal"],
            Box::new(move || {
                if let Some(v) = selected_view(&s.tab_view) {
                    let level = (v.zoom_level() + 0.1).min(5.0);
                    v.set_zoom_level(level);
                    s.show_transient(&format!("Zoom {}%", (level * 100.0).round() as i32));
                }
            }),
        );
        let s = self.clone();
        add(
            "zoom-out",
            &["<Control>minus"],
            Box::new(move || {
                if let Some(v) = selected_view(&s.tab_view) {
                    let level = (v.zoom_level() - 0.1).max(0.25);
                    v.set_zoom_level(level);
                    s.show_transient(&format!("Zoom {}%", (level * 100.0).round() as i32));
                }
            }),
        );
        let s = self.clone();
        add(
            "zoom-reset",
            &["<Control>0"],
            Box::new(move || {
                if let Some(v) = selected_view(&s.tab_view) {
                    v.set_zoom_level(1.0);
                    s.show_transient("Zoom 100%");
                }
            }),
        );
    }
}

/// Calm chrome that follows the system theme, with a soft omnibox.
fn install_theme() {
    adw::StyleManager::default().set_color_scheme(adw::ColorScheme::Default);
    let provider = gtk4::CssProvider::new();
    provider.load_from_string(
        r#"
        .omnibox-frame {
            border-radius: 20px;
            padding: 2px 8px 2px 6px;
            min-height: 40px;
            min-width: 220px;
            background-color: var(--view-bg-color);
            border: 1px solid color-mix(in srgb, currentColor 14%, transparent);
            box-shadow: 0 1px 2px color-mix(in srgb, currentColor 6%, transparent);
        }

        .omnibox-frame:focus-within {
            border-color: color-mix(in srgb, var(--accent-color) 60%, transparent);
            box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent-color) 20%, transparent);
        }

        .omnibox-frame entry {
            background: transparent;
            outline: none;
        }

        /* Single calm ring lives on the frame; the inner entry never
        draws its own focus outline (the doubled rectangle in bug reports). */
        .omnibox-frame entry:focus {
            outline: none;
            border: none;
            box-shadow: none;
        }

        .omnibox-frame entry image.left {
            opacity: 0.75;
        }

        .omnibox-frame menubutton button {
            border-radius: 9999px;
            padding: 4px 10px;
            font-size: 12px;
            font-weight: 600;
            background: color-mix(in srgb, currentColor 8%, transparent);
        }

        .omnibox-frame menubutton button:hover {
            background: color-mix(in srgb, currentColor 13%, transparent);
        }

        headerbar button.flat {
            border-radius: 9999px;
        }

        tabbar tab:selected {
            font-weight: 600;
        }

        .hairline-progress {
            min-height: 2px;
            padding: 0;
            margin: 0;
            border: none;
        }

        .hairline-progress trough {
            min-height: 2px;
            background: transparent;
            border: none;
            border-radius: 0;
        }

        .hairline-progress progress {
            min-height: 2px;
            background-color: var(--accent-color);
            border: none;
            border-radius: 0;
        }
        "#,
    );
    if let Some(display) = gtk4::gdk::Display::default() {
        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

fn build_ui(app: &adw::Application) {
    let prefs_store = PrefsStore::new(prefs_path());
    let prefs = prefs_store.load();
    // Handed to the shell below, so its startup write can skip this page
    // when the engine and history still match what was written here.
    let last_newtab: RefCell<Option<(SearchEngine, Vec<Visit>)>> = RefCell::new(None);
    sync_newtab_page(&last_newtab, prefs.engine, &[]);

    let session = shared_session();
    let context = shared_context();
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("br0x")
        .default_width(1200)
        .default_height(800)
        .build();

    install_theme();

    let tab_view = adw::TabView::new();
    tab_view.set_default_icon(&gio::ThemedIcon::new("web-browser-symbolic"));
    let tab_bar = adw::TabBar::new();
    tab_bar.set_view(Some(&tab_view));
    tab_bar.set_hexpand(true);
    // Tabs hug their content instead of stretching across the window.
    tab_bar.set_expand_tabs(false);
    tab_bar.set_autohide(false);

    let back = gtk4::Button::from_icon_name("go-previous-symbolic");
    back.set_tooltip_text(Some("Back (Alt+Left)"));
    back.add_css_class("flat");
    let fwd = gtk4::Button::from_icon_name("go-next-symbolic");
    fwd.set_tooltip_text(Some("Forward (Alt+Right)"));
    fwd.add_css_class("flat");
    let reload = gtk4::Button::from_icon_name("view-refresh-symbolic");
    reload.set_tooltip_text(Some("Reload (Ctrl+R / F5)"));
    reload.add_css_class("flat");

    // Search engine picker in the address bar.
    let engine_btn = gtk4::MenuButton::new();
    engine_btn.set_label(prefs.engine.name());
    engine_btn.add_css_class("flat");
    engine_btn.set_always_show_arrow(true);
    engine_btn.set_tooltip_text(Some("Choose search engine (address bar and start page)"));
    {
        let engine_menu = gio::Menu::new();
        for (idx, engine) in SearchEngine::ALL.iter().enumerate() {
            let item = gio::MenuItem::new(Some(engine.name()), Some("win.set-engine"));
            item.set_action_and_target_value(
                Some("win.set-engine"),
                Some(&(idx as u32).to_variant()),
            );
            engine_menu.append_item(&item);
        }
        engine_btn.set_menu_model(Some(&engine_menu));
    }

    let entry = gtk4::Entry::new();
    entry.set_placeholder_text(Some("Search or type a URL"));
    entry.set_hexpand(true);
    entry.add_css_class("flat");
    entry.set_icon_from_icon_name(gtk4::EntryIconPosition::Secondary, None);
    entry.set_icon_tooltip_text(gtk4::EntryIconPosition::Secondary, Some("Clear"));
    entry.set_menu_entry_icon_text(gtk4::EntryIconPosition::Primary, "Connection security");

    // Suggestion dropdown under the address bar, refilled from history on
    // every keystroke. Deliberately NOT autohide: an autohide popover
    // grabs the keyboard on popup and typing stops reaching the entry.
    // Dismissal is explicit instead (Esc, pick, Enter, tab switch,
    // navigation rewrite, page click).
    let suggest_popover = gtk4::Popover::new();
    suggest_popover.set_parent(&entry);
    suggest_popover.set_position(gtk4::PositionType::Bottom);
    suggest_popover.set_autohide(false);
    let suggest_list = gtk4::ListBox::new();
    suggest_list.set_selection_mode(gtk4::SelectionMode::Single);
    suggest_list.set_show_separators(true);
    suggest_popover.set_child(Some(&suggest_list));
    let suggest_urls: Rc<RefCell<Vec<SuggestHit>>> = Rc::new(RefCell::new(Vec::new()));

    // Star toggle for bookmarks, at the right end of the address bar.
    let bookmark_btn = gtk4::Button::from_icon_name("non-starred-symbolic");
    bookmark_btn.set_tooltip_text(Some("Bookmark this page (Ctrl+D)"));
    bookmark_btn.add_css_class("flat");

    // Per-site blocker toggle in the address bar (default on).
    let shield_btn = gtk4::ToggleButton::new();
    shield_btn.set_label("Shield");
    shield_btn.set_active(true);
    shield_btn.add_css_class("flat");

    // Key icon shown on pages with password fields.
    let key_btn = gtk4::Button::from_icon_name("dialog-password-symbolic");
    key_btn.set_tooltip_text(Some("Logins for this site"));
    key_btn.add_css_class("flat");
    key_btn.set_visible(false);

    // Saved-username popover anchored to the key icon.
    let key_pop = gtk4::Popover::new();
    key_pop.set_parent(&key_btn);
    let key_list = gtk4::ListBox::new();
    key_list.set_selection_mode(gtk4::SelectionMode::Single);
    key_pop.set_child(Some(&key_list));

    let omnibox_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
    omnibox_box.add_css_class("omnibox-frame");
    omnibox_box.set_hexpand(true);
    omnibox_box.set_size_request(-1, 40);
    omnibox_box.append(&engine_btn);
    omnibox_box.append(&entry);
    omnibox_box.append(&key_btn);
    omnibox_box.append(&shield_btn);
    omnibox_box.append(&bookmark_btn);

    let new_btn = gtk4::Button::from_icon_name("list-add-symbolic");
    new_btn.set_tooltip_text(Some("New tab (Ctrl+T)"));
    new_btn.add_css_class("flat");
    new_btn.add_css_class("suggested-action");

    let menu = gio::Menu::new();
    menu.append(Some("New Tab"), Some("win.new-tab"));
    menu.append(Some("Bookmark This Page"), Some("win.bookmark-page"));
    menu.append(Some("Find in Page"), Some("win.find"));
    menu.append(Some("Zoom In"), Some("win.zoom-in"));
    menu.append(Some("Zoom Out"), Some("win.zoom-out"));
    menu.append(Some("Reset Zoom"), Some("win.zoom-reset"));
    menu.append(Some("Reopen Closed Tab"), Some("win.reopen-tab"));
    menu.append(Some("Close Tab"), Some("win.close-tab"));
    menu.append(Some("Pin Tab"), Some("win.toggle-pin"));
    menu.append(Some("Copy Address"), Some("win.copy-url"));
    menu.append(Some("History"), Some("win.history"));
    menu.append(Some("Restore Previous Session"), Some("win.restore-prev"));
    menu.append(Some("Restore Tabs on Startup"), Some("win.restore-session"));
    menu.append(Some("Blocker for This Site"), Some("win.toggle-shield"));
    menu.append(Some("Reader Mode"), Some("win.reader"));
    menu.append(Some("Pop Out Video"), Some("win.popout"));
    menu.append(Some("Hide Element on Site"), Some("win.curtain-pick"));
    menu.append(Some("Unhide All on Site"), Some("win.curtain-clear"));
    menu.append(Some("Sidebar"), Some("win.toggle-sidebar"));
    menu.append(Some("Quit"), Some("win.quit"));
    let menu_btn = gtk4::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&menu)
        .tooltip_text("Menu")
        .build();

    // Bookmarks menu in the header, next to the app menu: the browser
    // convention for reaching saved pages without opening a new tab.
    // Scrolled with a height cap so large collections stay on screen.
    let bm_list = gtk4::ListBox::new();
    bm_list.set_selection_mode(gtk4::SelectionMode::Single);
    let bm_scroll = gtk4::ScrolledWindow::new();
    bm_scroll.set_child(Some(&bm_list));
    bm_scroll.set_max_content_height(420);
    bm_scroll.set_propagate_natural_height(true);
    let bm_popover = gtk4::Popover::new();
    bm_popover.set_child(Some(&bm_scroll));
    let bm_menu_btn =
        gtk4::MenuButton::builder().icon_name("starred-symbolic").tooltip_text("Bookmarks").build();
    bm_menu_btn.set_popover(Some(&bm_popover));
    bm_menu_btn.add_css_class("flat");

    let header = adw::HeaderBar::new();
    let sidebar_btn = gtk4::Button::from_icon_name("sidebar-show-symbolic");
    sidebar_btn.set_tooltip_text(Some("Tabs sidebar (F9)"));
    sidebar_btn.add_css_class("flat");
    let reader_btn = gtk4::Button::from_icon_name("document-open-symbolic");
    reader_btn.set_tooltip_text(Some("Reader mode (Ctrl+Shift+R)"));
    reader_btn.add_css_class("flat");
    let popout_btn = gtk4::Button::from_icon_name("window-new-symbolic");
    popout_btn.set_tooltip_text(Some("Pop out video (Ctrl+Shift+O)"));
    popout_btn.add_css_class("flat");
    header.pack_start(&sidebar_btn);
    header.pack_start(&back);
    header.pack_start(&fwd);
    header.pack_start(&reload);
    header.set_title_widget(Some(&omnibox_box));
    header.pack_end(&menu_btn);
    header.pack_end(&bm_menu_btn);
    header.pack_end(&new_btn);
    header.pack_end(&reader_btn);
    header.pack_end(&popout_btn);

    let progress = gtk4::ProgressBar::new();
    progress.add_css_class("hairline-progress");
    progress.set_visible(false);

    // Thin reading-progress line under the active tab, driven by scroll.
    let read_progress = gtk4::ProgressBar::new();
    read_progress.add_css_class("hairline-progress");
    read_progress.set_visible(false);

    // Toggleable left tab sidebar. Hidden by default; the top tab bar keeps
    // working either way.
    let sidebar_label = gtk4::Label::new(Some("Tabs"));
    sidebar_label.set_xalign(0.0);
    sidebar_label.set_hexpand(true);
    let sidebar_collapse = gtk4::Button::from_icon_name("sidebar-hide-symbolic");
    sidebar_collapse.set_tooltip_text(Some("Collapse to icon rail"));
    sidebar_collapse.add_css_class("flat");
    let sidebar_head = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    sidebar_head.set_margin_top(6);
    sidebar_head.set_margin_bottom(6);
    sidebar_head.set_margin_start(8);
    sidebar_head.set_margin_end(8);
    sidebar_head.append(&sidebar_label);
    sidebar_head.append(&sidebar_collapse);
    let sidebar_list = gtk4::ListBox::new();
    sidebar_list.set_selection_mode(gtk4::SelectionMode::Single);
    let sidebar_scroll = gtk4::ScrolledWindow::new();
    sidebar_scroll.set_child(Some(&sidebar_list));
    sidebar_scroll.set_vexpand(true);
    let sidebar_box = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    sidebar_box.set_size_request(220, -1);
    sidebar_box.set_visible(false);
    sidebar_box.append(&sidebar_head);
    sidebar_box.append(&sidebar_scroll);
    tab_view.set_hexpand(true);
    tab_view.set_vexpand(true);
    let content_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    content_box.append(&sidebar_box);
    content_box.append(&tab_view);

    // Find in page bar, revealed with Ctrl+F.
    let find_entry = gtk4::SearchEntry::new();
    find_entry.set_hexpand(true);
    find_entry.set_placeholder_text(Some("Find in page"));
    let find_status = gtk4::Label::new(None);
    find_status.add_css_class("dim-label");
    let find_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    find_box.set_margin_top(4);
    find_box.set_margin_bottom(4);
    find_box.set_margin_start(8);
    find_box.set_margin_end(8);
    find_box.append(&find_entry);
    find_box.append(&find_status);
    let find_bar = gtk4::SearchBar::new();
    find_bar.set_child(Some(&find_box));
    find_bar.set_show_close_button(true);
    find_bar.connect_entry(&find_entry);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.add_top_bar(&tab_bar);
    toolbar.add_top_bar(&read_progress);
    toolbar.add_top_bar(&progress);
    toolbar.add_top_bar(&find_bar);
    toolbar.set_content(Some(&content_box));

    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&toolbar));
    window.set_content(Some(&toasts));

    let history = match History::open(history_path()) {
        Ok(h) => Some(h),
        Err(e) => {
            eprintln!("br0x: history unavailable: {e}");
            None
        }
    };
    let history = Rc::new(RefCell::new(history));
    register_br0x_scheme(&context, history.clone());
    connect_downloads(&session, &toasts);

    let bookmarks_store = BookmarkStore::new(bookmarks_path());
    let bookmarks = Rc::new(RefCell::new(bookmarks_store.load()));

    let shell = Rc::new(Shell {
        tab_view: tab_view.clone(),
        entry: entry.clone(),
        window: window.clone(),
        back: back.clone(),
        fwd: fwd.clone(),
        reload: reload.clone(),
        progress,
        read_progress,
        engine_btn: engine_btn.clone(),
        bookmark_btn: bookmark_btn.clone(),
        shield_btn,
        key_btn,
        reader_btn,
        find_bar: find_bar.clone(),
        find_entry: find_entry.clone(),
        find_status,
        toasts: toasts.clone(),
        zoom_toast: RefCell::new(None),
        last_session: RefCell::new(Vec::new()),
        last_sample: RefCell::new(None),
        last_newtab,
        suggest_pop: suggest_popover.clone(),
        bm_list: bm_list.clone(),
        sidebar_box,
        sidebar_list: sidebar_list.clone(),
        sidebar_pages: RefCell::new(Vec::new()),
        sidebar_visible: RefCell::new(false),
        sidebar_rail: RefCell::new(false),
        key_pop,
        key_list: key_list.clone(),
        shield: Rc::new(RefCell::new(ShellShield::load(data_file("shield.json")))),
        curtain: Rc::new(RefCell::new(ShellCurtain::load(data_file("curtain.json")))),
        vault: Rc::new(RefCell::new(ShellVault::load(data_file("vault.json")))),
        picker_armed: RefCell::new(false),
        shield_sync: RefCell::new(false),
        popouts: RefCell::new(Vec::new()),
        history,
        bookmarks,
        bookmarks_store,
        context,
        session: session.clone(),
        prefs: RefCell::new(prefs),
        prefs_store,
        tabs: Rc::new(RefCell::new(Tabs::new())),
    });

    {
        let s = shell.clone();
        bookmark_btn.connect_clicked(move |_| {
            s.toggle_bookmark();
        });
    }
    {
        // Guarded: refresh_shield_ui drives the toggle programmatically on
        // every tab switch, which must not flip the stored state.
        let s = shell.clone();
        let btn = s.shield_btn.clone();
        btn.connect_toggled(move |toggled| {
            if *s.shield_sync.borrow() {
                return;
            }
            s.toggle_shield_to(toggled.is_active());
        });
    }
    {
        let s = shell.clone();
        let btn = s.key_btn.clone();
        let pop = s.key_pop.clone();
        btn.connect_clicked(move |_| {
            s.rebuild_key_pop();
            pop.popup();
        });
    }
    {
        let s = shell.clone();
        let list = s.key_list.clone();
        list.connect_row_activated(move |_, row| {
            let origin = selected_view(&s.tab_view)
                .and_then(|v| v.uri().map(|u| u.to_string()))
                .map(|uri| origin_of(&uri))
                .unwrap_or_default();
            let creds = s.vault.borrow().credentials_for(&origin);
            match usize::try_from(row.index()).ok() {
                Some(i) if i < creds.len() => {
                    s.fill_login(&creds[i].0, &creds[i].1);
                }
                _ => s.save_login(),
            }
        });
    }
    {
        let s = shell.clone();
        let btn = s.reader_btn.clone();
        btn.connect_clicked(move |_| {
            s.toggle_reader();
        });
    }
    {
        let s = shell.clone();
        popout_btn.connect_clicked(move |_| {
            s.popout_video();
        });
    }
    {
        let s = shell.clone();
        sidebar_btn.connect_clicked(move |_| {
            s.toggle_sidebar();
        });
    }
    {
        let s = shell.clone();
        sidebar_collapse.connect_clicked(move |_| {
            s.toggle_rail();
        });
    }
    {
        let s = shell.clone();
        sidebar_list.connect_row_activated(move |_, row| {
            let page = usize::try_from(row.index())
                .ok()
                .and_then(|i| s.sidebar_pages.borrow().get(i).cloned());
            if let Some(page) = page {
                s.tab_view.set_selected_page(&page);
                if let Some(v) = selected_view(&s.tab_view) {
                    v.grab_focus();
                }
            }
        });
    }
    {
        let s = shell.clone();
        let list = bm_list.clone();
        let popover = bm_popover.clone();
        list.connect_row_activated(move |_, row| {
            // Positional mapping: refresh_bookmarks_menu rebuilds rows in
            // store order, so row N is bookmark N. The empty state is a
            // placeholder, not a row, and never reaches here.
            let index = usize::try_from(row.index()).ok();
            let url = index.and_then(|i| s.bookmarks.borrow().get(i).map(|mark| mark.url.clone()));
            // The empty-state label is not a bookmark row: guard by child.
            let usable = row.child().is_some_and(|child| child.is::<gtk4::Box>());
            popover.popdown();
            if usable
                && let Some(url) = url
                && let Some(view) = selected_view(&s.tab_view)
            {
                view.load_uri(&url);
                view.grab_focus();
            }
        });
    }
    shell.refresh_bookmarks_menu();

    {
        // Live find as you type, plus Enter to jump to the next match.
        let s = shell.clone();
        find_entry.connect_search_changed(move |e| {
            let text = e.text().to_string();
            if text.is_empty() {
                s.find_status.set_text("");
                s.find_in_page(&text, true, false);
                return;
            }
            s.find_in_page(&text, true, true);
        });
    }
    {
        // Hiding the find bar drops the page highlights and the stale count;
        // WebKit keeps both until the search is finished.
        let s = shell.clone();
        find_bar.connect_search_mode_enabled_notify(move |bar| {
            if bar.is_search_mode() {
                return;
            }
            if let Some(view) = selected_view(&s.tab_view)
                && let Some(controller) = view.find_controller()
            {
                controller.search_finish();
            }
            s.find_status.set_text("");
        });
    }
    {
        let s = shell.clone();
        find_entry.connect_activate(move |_| {
            let text = s.find_entry.text().to_string();
            s.find_in_page(&text, true, false);
        });
    }
    {
        let s = shell.clone();
        find_entry.connect_next_match(move |_| {
            let text = s.find_entry.text().to_string();
            s.find_in_page(&text, true, false);
        });
    }
    {
        let s = shell.clone();
        find_entry.connect_previous_match(move |_| {
            let text = s.find_entry.text().to_string();
            s.find_in_page(&text, false, false);
        });
    }
    {
        let s = shell.clone();
        let urls = suggest_urls.clone();
        let popover = suggest_popover.clone();
        let list = suggest_list.clone();
        entry.connect_activate(move |e| {
            let query = e.text().to_string();
            if query.trim().is_empty() {
                return;
            }
            // A highlighted row wins over the raw text; with no highlight
            // but a tab hit on top, Enter switches to that tab; otherwise
            // Enter searches the typed text. Either way the list must not
            // sit over the page until the load commits, or forever on
            // failure.
            let engine = s.prefs.borrow().engine;
            let selected = list.selected_row().and_then(|row| usize::try_from(row.index()).ok());
            let fallback = SuggestHit { url: search::resolve(&query, engine), page: None };
            let target = match suggestion_target(&urls.borrow(), selected, &query, engine) {
                Some(hit) => hit,
                None if selected.is_none() && popover.is_visible() => urls
                    .borrow()
                    .first()
                    .filter(|hit| hit.page.is_some())
                    .cloned()
                    .unwrap_or(fallback),
                None => fallback,
            };
            popover.popdown();
            if let Some(page) = target.page {
                s.tab_view.set_selected_page(&page);
                if let Some(v) = selected_view(&s.tab_view) {
                    v.grab_focus();
                }
            } else if let Some(v) = selected_view(&s.tab_view) {
                v.load_uri(&target.url);
                v.grab_focus();
            }
        });
    }
    {
        let toasts_clone = toasts.clone();
        let shell_clone = shell.clone();
        entry.connect_icon_release(move |e, pos| {
            if pos == gtk4::EntryIconPosition::Secondary {
                e.set_text("");
                e.grab_focus();
            } else if let Some(v) = selected_view(&shell_clone.tab_view)
                && let Some(uri) = v.uri()
            {
                let msg = if uri.starts_with("https://") {
                    "Secure connection (https)"
                } else if uri.starts_with("http://") {
                    "Not secure — this site uses http"
                } else {
                    return;
                };
                toasts_clone.add_toast(adw::Toast::new(msg));
            }
        });
    }
    {
        entry.connect_changed(|e| {
            let has = !e.text().is_empty();
            e.set_icon_from_icon_name(
                gtk4::EntryIconPosition::Secondary,
                if has { Some("edit-clear-symbolic") } else { None },
            );
        });
    }
    {
        let s = shell.clone();
        let history = shell.history.clone();
        let list = suggest_list.clone();
        let urls = suggest_urls.clone();
        let popover = suggest_popover.clone();
        entry.connect_changed(move |e| {
            // Only while typing: the address bar also gets rewritten when a
            // page commits or a tab is switched, and neither is a search.
            // Programmatic rewrites always close the list.
            let needle = e.text();
            let needle = needle.trim();
            let page_url = selected_view(&s.tab_view).and_then(|v| v.uri());
            let on_page = page_url.is_some_and(|uri| uri.as_str() == needle);
            if needle.chars().count() < SUGGESTION_MIN_CHARS
                || on_page
                || !typing_in_entry(e, &popover)
            {
                popover.popdown();
                return;
            }
            // No focus games here: without autohide the popup never takes
            // the keyboard, so typing keeps flowing into the entry.
            // Widget rebuilds cost more than the query: when the result
            // URLs match the rows already shown, leave them alone.
            // Open tabs match first, then history, then the search row.
            let tab_hits = s.open_tab_hits(needle);
            let visits = history
                .borrow()
                .as_ref()
                .and_then(|h| h.search(needle, SUGGESTION_LIMIT).ok())
                .unwrap_or_default();
            let fresh: Vec<SuggestHit> = tab_hits
                .iter()
                .map(|(_, url, page)| SuggestHit { url: url.clone(), page: Some(page.clone()) })
                .chain(visits.iter().map(|v| SuggestHit { url: v.url.clone(), page: None }))
                .collect();
            if fresh == *urls.borrow() {
                if !popover.is_visible() && !fresh.is_empty() {
                    popover.popup();
                }
                return;
            }
            if render_suggestions(&tab_hits, &visits, needle, &list, &urls, s.prefs.borrow().engine)
            {
                // Line the panel up with the field it belongs to.
                popover.set_size_request(e.width(), -1);
                popover.popup();
            } else {
                popover.popdown();
            }
        });
    }
    {
        let s = shell.clone();
        let urls = suggest_urls.clone();
        let popover = suggest_popover.clone();
        suggest_list.connect_row_activated(move |_, row| {
            let index = usize::try_from(row.index()).ok();
            let query = s.entry.text().to_string();
            let target = suggestion_target(&urls.borrow(), index, &query, s.prefs.borrow().engine);
            popover.popdown();
            if let Some(hit) = target {
                if let Some(page) = &hit.page {
                    s.tab_view.set_selected_page(page);
                }
                if let Some(view) = selected_view(&s.tab_view) {
                    if hit.page.is_none() {
                        view.load_uri(&hit.url);
                    }
                    view.grab_focus();
                }
            }
        });
    }
    {
        // The caret never leaves the entry: Down/Up move the highlight,
        // Enter picks it up (see connect_activate), typing keeps working.
        let list = suggest_list.clone();
        let popover = suggest_popover.clone();
        let keys = gtk4::EventControllerKey::new();
        keys.connect_key_pressed(move |_, keyval, _, _| {
            if !popover.is_visible() {
                return glib::Propagation::Proceed;
            }
            let step = match keyval {
                gtk4::gdk::Key::Down => 1,
                gtk4::gdk::Key::Up => -1,
                gtk4::gdk::Key::Escape => {
                    // Swallowed on purpose: Escape closes the list instead of
                    // reaching the win.stop action that halts page loads.
                    popover.popdown();
                    list.unselect_all();
                    return glib::Propagation::Stop;
                }
                _ => return glib::Propagation::Proceed,
            };
            let current = list
                .selected_row()
                .and_then(|row| usize::try_from(row.index()).ok())
                .map(|i| i as isize)
                .unwrap_or(if step > 0 { -1 } else { suggest_row_count(&list) as isize });
            let next = current + step;
            if next >= 0
                && let Some(row) = list.row_at_index(next as i32)
            {
                list.select_row(Some(&row));
            } else {
                list.unselect_all();
            }
            glib::Propagation::Stop
        });
        entry.add_controller(keys);
    }
    {
        let s = shell.clone();
        new_btn.connect_clicked(move |_| {
            s.add_blank_tab();
            s.entry.grab_focus();
        });
    }
    {
        let s = shell.clone();
        back.connect_clicked(move |_| {
            if let Some(v) = selected_view(&s.tab_view) {
                v.go_back();
            }
        });
    }
    {
        let s = shell.clone();
        fwd.connect_clicked(move |_| {
            if let Some(v) = selected_view(&s.tab_view) {
                v.go_forward();
            }
        });
    }
    {
        let s = shell.clone();
        reload.connect_clicked(move |_| {
            if let Some(v) = selected_view(&s.tab_view) {
                if v.is_loading() {
                    v.stop_loading();
                } else {
                    v.reload();
                }
            }
        });
    }
    {
        let s = shell.clone();
        let popover = suggest_popover.clone();
        tab_view.connect_selected_page_notify(move |_| {
            s.on_selection_changed();
            popover.popdown();
        });
    }
    {
        tab_view.connect_close_page(move |_, page| {
            // Stopping media on close matters: a WebView that is being
            // destroyed can otherwise keep its web process playing audio.
            if let Ok(view) = page.child().downcast::<webkit6::WebView>()
                && view.is_playing_audio()
            {
                view.set_is_muted(true);
                view.terminate_web_process();
            }
            glib::Propagation::Proceed
        });
    }
    // Set on close-request. Tab teardown detaches every page too, so the
    // empty strip during teardown must not read as "the user closed the last
    // tab" and open a fresh one inside a disposing tab view.
    let closing = Rc::new(std::cell::Cell::new(false));
    {
        let s = shell.clone();
        let closing = closing.clone();
        tab_view.connect_page_detached(move |tv, page, _| {
            {
                let mut tabs = s.tabs.borrow_mut();
                // A parked tab sits on about:blank, so its real URL has to
                // come from pending_url or reopening it would restore a blank.
                let uri = tabs.entry_mut(page).and_then(|entry| match entry.view.uri() {
                    Some(uri) if !is_blank_uri(&uri) => Some(uri.to_string()),
                    _ => entry.meta.pending_url.clone(),
                });
                if let Some(uri) = uri {
                    if tabs.closed.len() >= MAX_CLOSED_TABS {
                        tabs.closed.remove(0);
                    }
                    tabs.closed.push(uri);
                }
                tabs.remove_page(page);
            }
            // Borrow released: a new tab touches the same RefCell.
            if tv.n_pages() == 0 && !closing.get() {
                s.add_blank_tab();
                s.entry.grab_focus();
            }
            s.refresh_sidebar();
        });
    }
    {
        let s = shell.clone();
        window.connect_close_request(move |_| {
            closing.set(true);
            s.save_session();
            glib::Propagation::Proceed
        });
    }
    {
        let s = shell.clone();
        let tick = std::cell::Cell::new(0u32);
        glib::timeout_add_seconds_local(5, move || {
            s.enforce_tick();
            tick.set(tick.get() + 1);
            if tick.get().is_multiple_of(6) {
                s.save_session();
            }
            glib::ControlFlow::Continue
        });
    }
    {
        // Reading progress: the selected view reports its scroll fraction
        // once a second into the thin line under the tab bar.
        let s = shell.clone();
        glib::timeout_add_seconds_local(1, move || {
            let usable = selected_view(&s.tab_view)
                .and_then(|v| v.uri().map(|u| (v, u.to_string())))
                .filter(|(_, uri)| uri.starts_with("http://") || uri.starts_with("https://"));
            match usable {
                Some((view, _)) if !view.is_loading() => {
                    let bar = s.read_progress.clone();
                    eval_ratio(&view, SCROLL_JS, move |ratio| {
                        if ratio.is_finite() && ratio > 0.0 {
                            bar.set_visible(true);
                            bar.set_fraction(ratio.clamp(0.0, 1.0));
                        } else {
                            bar.set_visible(false);
                        }
                    });
                }
                _ => s.read_progress.set_visible(false),
            }
            glib::ControlFlow::Continue
        });
    }

    shell.start_session();
    start_bench_server(&shell);
    ensure_filter(shell.tabs.clone(), shell.shield.clone());
    shell.install_actions(app);
    shell.on_selection_changed();
    if let Some(v) = selected_view(&shell.tab_view) {
        v.grab_focus();
    }
    window.present();
}

/// One isolated bench tab: a real WebView that is never appended to the tab
/// strip and never recorded in the session, history, or suggestions.
struct BenchTab {
    id: u64,
    view: webkit6::WebView,
    loaded: Rc<std::cell::Cell<bool>>,
}

type Slot = std::sync::Arc<std::sync::Mutex<Option<String>>>;

/// One main-thread job from a bench socket thread. Replies travel back over
/// std channels; the UI thread never blocks, only the socket caller does.
enum BenchJob {
    Open { url: String, reply: std::sync::mpsc::Sender<Result<u64, String>> },
    IsLoaded { id: u64, reply: std::sync::mpsc::Sender<bool> },
    Eval { id: u64, script: String, reply: std::sync::mpsc::Sender<Result<Slot, String>> },
    Snapshot { id: u64, path: String, reply: std::sync::mpsc::Sender<Result<(), String>> },
    Probe { reply: std::sync::mpsc::Sender<(u64, bool)> },
    Tabs { reply: std::sync::mpsc::Sender<Vec<bench::TabInfo>> },
    Close { target: bench::CloseTarget, reply: std::sync::mpsc::Sender<Result<(), String>> },
}

/// Socket-side handle: only the job queue crosses threads.
#[derive(Clone)]
struct ShellBench {
    queue: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<BenchJob>>>,
}

impl ShellBench {
    /// Caller-side timeout. Generous: first loads compile filters and shaders.
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

    fn ask<T: Send + 'static>(
        &self,
        build: impl FnOnce(std::sync::mpsc::Sender<T>) -> BenchJob,
    ) -> Result<T, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.queue.lock().map_err(|_| "bench queue unavailable".to_string())?.push_back(build(tx));
        rx.recv_timeout(Self::TIMEOUT).map_err(|_| "bench request timed out".to_string())
    }

    /// Run `script` in the tab and poll for its string result.
    fn eval_poll(&self, id: u64, script: String) -> Result<String, String> {
        let slot = self.ask(|reply| BenchJob::Eval { id, script, reply })??;
        let deadline = std::time::Instant::now() + Self::TIMEOUT;
        loop {
            if let Some(text) = slot.lock().map_err(|_| "bench slot poisoned".to_string())?.clone()
            {
                return Ok(text);
            }
            if std::time::Instant::now() >= deadline {
                return Err("bench eval timed out".to_string());
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    fn is_loaded(&self, id: u64) -> Result<bool, String> {
        self.ask(|reply| BenchJob::IsLoaded { id, reply })
    }
}

impl bench::BenchHandler for ShellBench {
    fn open(&mut self, url: &str) -> Result<u64, String> {
        if url.is_empty() {
            return Err("empty url".to_string());
        }
        self.ask(|reply| BenchJob::Open { url: url.to_string(), reply })?
    }

    fn wait(&mut self, id: u64) -> Result<(), String> {
        let deadline = std::time::Instant::now() + Self::TIMEOUT;
        loop {
            if self.is_loaded(id)? {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err("bench wait timed out".to_string());
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    fn text(&mut self, id: u64) -> Result<String, String> {
        self.eval_poll(
            id,
            "document.documentElement ? document.documentElement.innerText : ''".to_string(),
        )
    }

    fn shot(&mut self, id: u64, path: &str) -> Result<(), String> {
        self.ask(|reply| BenchJob::Snapshot { id, path: path.to_string(), reply })?
    }

    fn click(&mut self, id: u64, selector: &str) -> Result<(), String> {
        if selector.is_empty() {
            return Err("empty selector".to_string());
        }
        match self.eval_poll(id, click_script(selector)) {
            Ok(found) if found == "1" => Ok(()),
            Ok(_) => Err("no such element".to_string()),
            Err(e) => Err(e),
        }
    }

    fn probe(&mut self) -> bench::WindowState {
        // Bench tabs never take the selection; selected stays empty.
        let (tabs, visible) = self.ask(|reply| BenchJob::Probe { reply }).unwrap_or((0, false));
        bench::WindowState { tabs, visible, selected: None }
    }

    fn close(&mut self, target: bench::CloseTarget) -> Result<(), String> {
        self.ask(|reply| BenchJob::Close { target, reply })?
    }

    fn tabs(&mut self) -> Vec<bench::TabInfo> {
        self.ask(|reply| BenchJob::Tabs { reply }).unwrap_or_default()
    }
}

/// JS that clicks the first match and reports back '1', or '0' when absent.
fn click_script(selector: &str) -> String {
    let mut script = String::from("(function(){var el=document.querySelector(\"");
    bench::escape_into(&mut script, selector);
    script.push_str("\");if(!el){return '0';}el.click();return '1';})()");
    script
}

fn find_bench(tabs: &[BenchTab], id: u64) -> Option<&BenchTab> {
    tabs.iter().find(|t| t.id == id)
}

/// Start the bench server when BR0X_BENCH=1. All GTK/WebKit work stays on
/// the main thread: socket threads enqueue jobs and a 25ms pump drains them.
/// Bench tabs live in a private registry that user-tab code never reads.
fn start_bench_server(shell: &Rc<Shell>) {
    if !bench::is_enabled() {
        return;
    }
    let tabs: Rc<RefCell<Vec<BenchTab>>> = Rc::new(RefCell::new(Vec::new()));
    let next_id: Rc<std::cell::Cell<u64>> = Rc::new(std::cell::Cell::new(0));
    let context = shell.context.clone();
    let session = shell.session.clone();
    let window = shell.window.clone();
    let queue: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<BenchJob>>> =
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
    let pump = queue.clone();
    glib::timeout_add_local(std::time::Duration::from_millis(25), move || {
        while let Some(job) = pump.lock().ok().and_then(|mut guard| guard.pop_front()) {
            handle_bench_job(job, &tabs, &next_id, &context, &session, &window);
        }
        glib::ControlFlow::Continue
    });
    let handler = ShellBench { queue };
    let server = bench::BenchServer::new();
    eprintln!("br0x: bench listening on {}", server.path().display());
    if let Err(e) = server.start(handler) {
        eprintln!("br0x: bench server failed: {e}");
    }
}

fn handle_bench_job(
    job: BenchJob,
    tabs: &Rc<RefCell<Vec<BenchTab>>>,
    next_id: &Rc<std::cell::Cell<u64>>,
    context: &webkit6::WebContext,
    session: &webkit6::NetworkSession,
    window: &adw::ApplicationWindow,
) {
    match job {
        BenchJob::Open { url, reply } => {
            next_id.set(next_id.get() + 1);
            let id = next_id.get();
            let view = webkit6::WebView::builder()
                .web_context(context)
                .network_session(session)
                .settings(&browser_settings())
                .build();
            attach_filter(&view);
            let loaded = Rc::new(std::cell::Cell::new(false));
            let flag = loaded.clone();
            view.connect_load_changed(move |_, event| {
                if event == webkit6::LoadEvent::Finished {
                    flag.set(true);
                }
            });
            view.load_uri(&url);
            tabs.borrow_mut().push(BenchTab { id, view, loaded });
            let _ = reply.send(Ok(id));
        }
        BenchJob::IsLoaded { id, reply } => {
            let done = find_bench(&tabs.borrow(), id).is_some_and(|t| t.loaded.get());
            let _ = reply.send(done);
        }
        BenchJob::Eval { id, script, reply } => match find_bench(&tabs.borrow(), id) {
            Some(tab) => {
                let slot: Slot = std::sync::Arc::new(std::sync::Mutex::new(None));
                let fill = slot.clone();
                eval_text(&tab.view, &script, move |text| {
                    if let Ok(mut guard) = fill.lock() {
                        *guard = Some(text);
                    }
                });
                let _ = reply.send(Ok(slot));
            }
            None => {
                let _ = reply.send(Err("no such tab".to_string()));
            }
        },
        BenchJob::Snapshot { id, path, reply } => match find_bench(&tabs.borrow(), id) {
            Some(tab) => {
                let view = tab.view.clone();
                view.snapshot(
                    webkit6::SnapshotRegion::FullDocument,
                    webkit6::SnapshotOptions::all(),
                    None::<&gio::Cancellable>,
                    move |result| {
                        let answer = match result {
                            Ok(texture) => match texture.save_to_png(&path) {
                                Ok(()) => Ok(()),
                                Err(e) => Err(format!("snapshot save failed: {e}")),
                            },
                            Err(e) => Err(format!("snapshot failed: {e}")),
                        };
                        let _ = reply.send(answer);
                    },
                );
            }
            None => {
                let _ = reply.send(Err("no such tab".to_string()));
            }
        },
        BenchJob::Probe { reply } => {
            let _ = reply.send((tabs.borrow().len() as u64, window.is_visible()));
        }
        BenchJob::Tabs { reply } => {
            let list = tabs
                .borrow()
                .iter()
                .map(|t| bench::TabInfo {
                    id: t.id,
                    url: t.view.uri().map(|u| u.to_string()).unwrap_or_default(),
                    loaded: t.loaded.get(),
                })
                .collect();
            let _ = reply.send(list);
        }
        BenchJob::Close { target, reply } => {
            // Bench registry only: user tabs are untouched by construction.
            let mut tabs = tabs.borrow_mut();
            let result = match target {
                bench::CloseTarget::All => {
                    tabs.clear();
                    Ok(())
                }
                bench::CloseTarget::One(id) => {
                    if let Some(pos) = tabs.iter().position(|t| t.id == id) {
                        tabs.remove(pos);
                        Ok(())
                    } else {
                        Err("no such tab".to_string())
                    }
                }
            };
            let _ = reply.send(result);
        }
    }
}

fn main() {
    let app = adw::Application::builder().application_id("org.br0x.Browser").build();
    app.connect_activate(build_ui);
    app.run();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decodes_escapes_and_plus() {
        assert_eq!(percent_decode("rust%20web+kit"), "rust web kit");
        assert_eq!(percent_decode("100%25%2B"), "100%+");
        // Malformed escapes survive instead of vanishing.
        assert_eq!(percent_decode("%zz%"), "%zz%");
    }

    #[test]
    fn click_script_reports_found_and_missing() {
        let script = click_script("#ok");
        assert!(script.contains("#ok") && script.contains("el.click()"));
        assert_eq!(
            click_script("a\"b"),
            "(function(){var el=document.querySelector(\"a\\\"b\");\
             if(!el){return '0';}el.click();return '1';})()"
        );
    }

    #[test]
    fn history_request_reads_parameters_in_any_order() {
        assert_eq!(history_request("br0x://history"), (None, false));
        assert_eq!(history_request("br0x://history?q=rust+web"), (Some("rust web".into()), false));
        assert_eq!(history_request("br0x://history?clear=1&q=rust"), (Some("rust".into()), true));
        assert_eq!(history_request("br0x://history?q=rust&clear=1"), (Some("rust".into()), true));
    }

    #[test]
    fn clear_inside_the_search_text_does_not_clear() {
        assert_eq!(history_request("br0x://history?q=clear%3D1"), (Some("clear=1".into()), false));
    }

    #[test]
    fn start_page_is_stale_only_when_its_inputs_change() {
        fn visit(url: &str, title: &str, at: i64) -> Visit {
            Visit { url: url.to_string(), title: title.to_string(), visited_at: at }
        }
        let sites = vec![visit("https://a.example", "A", 10)];
        let cached = (SearchEngine::DuckDuckGo, sites.clone());
        // Nothing written yet: the page has to be built.
        assert!(newtab_page_stale(None, SearchEngine::DuckDuckGo, &sites));
        assert!(!newtab_page_stale(Some(&cached), SearchEngine::DuckDuckGo, &sites));
        // Engine, list content, and list order all change the page.
        assert!(newtab_page_stale(Some(&cached), SearchEngine::Google, &sites));
        let mut more = sites.clone();
        more.push(visit("https://b.example", "B", 20));
        assert!(newtab_page_stale(Some(&cached), SearchEngine::DuckDuckGo, &more));
        let reordered = vec![more[1].clone(), sites[0].clone()];
        assert!(newtab_page_stale(Some(&cached), SearchEngine::DuckDuckGo, &reordered));
        assert!(newtab_page_stale(Some(&cached), SearchEngine::DuckDuckGo, &[]));
        // Titles are card text, so a retitle must rewrite.
        let retitled = vec![visit("https://a.example", "A renamed", 10)];
        assert!(newtab_page_stale(Some(&cached), SearchEngine::DuckDuckGo, &retitled));
    }

    #[test]
    fn blank_uris_are_recognised() {
        assert!(is_blank_uri("about:blank"));
        assert!(is_blank_uri(&newtab_url()));
        assert!(!is_blank_uri(""));
        assert!(!is_blank_uri("br0x://history"));
        assert!(!is_blank_uri("https://example.com"));
    }

    #[test]
    fn shell_domains_normalize_like_core() {
        assert_eq!(domain_of("https://Example.COM:8080/x?q=1"), "example.com");
        assert_eq!(domain_of("http://example.com./"), "example.com");
        assert_eq!(domain_of("example.com/path"), "example.com");
        assert_eq!(domain_of(""), "");
    }

    #[test]
    fn shell_origins_keep_scheme_and_host() {
        assert_eq!(origin_of("https://Example.COM/a?b=c"), "https://example.com");
        assert_eq!(origin_of("http://example.com:3000/x"), "http://example.com");
        assert_eq!(origin_of("br0x://history"), "");
        assert_eq!(origin_of("about:blank"), "");
    }

    #[test]
    fn js_strings_escape_for_single_quotes() {
        assert_eq!(js_string("o'brien\\x"), "o\\'brien\\\\x");
        assert_eq!(js_string("a\nb"), "a\\nb");
        assert_eq!(js_string("</script>"), "\\x3c/script>");
    }

    #[test]
    fn curtain_css_hides_every_selector() {
        let css = curtain_css(&["#ad".to_string(), ".pop > .x".to_string()]);
        assert_eq!(css, "#ad{display:none!important}.pop > .x{display:none!important}");
        assert_eq!(curtain_css(&[]), "");
    }

    #[test]
    fn state_badges_strip_cleanly() {
        assert_eq!(strip_state_badges("News • Sleeping"), "News");
        assert_eq!(strip_state_badges("News • Parked"), "News");
        assert_eq!(strip_state_badges("News"), "News");
    }

    #[test]
    fn find_starts_before_it_moves() {
        // A tab whose controller is not searching the query yet (fresh tab,
        // switched tab) has to start a search: next before a search is an
        // error in WebKit and does nothing.
        assert_eq!(find_step("rust", "", true, false), FindStep::Start);
        assert_eq!(find_step("rust", "other", false, false), FindStep::Start);
        assert_eq!(find_step("rust", "rust", true, false), FindStep::Next);
        assert_eq!(find_step("rust", "rust", false, false), FindStep::Previous);
        assert_eq!(find_step("rust", "rust", true, true), FindStep::Start);
        // An empty query clears instead of highlighting the whole page.
        assert_eq!(find_step("   ", "rust", true, false), FindStep::Clear);
    }

    #[test]
    fn suggested_names_keep_only_the_last_component() {
        assert_eq!(safe_file_name("report.pdf"), Some("report.pdf"));
        assert_eq!(safe_file_name("/etc/passwd"), Some("passwd"));
        assert_eq!(safe_file_name("../../.bashrc"), Some(".bashrc"));
        assert_eq!(safe_file_name("a/b/"), Some("b"));
        assert_eq!(safe_file_name(""), None);
        assert_eq!(safe_file_name(".."), None);
        assert_eq!(safe_file_name("a/.."), None);
    }

    #[test]
    fn download_names_never_collide() {
        let taken = ["photo.jpg", "photo (1).jpg"];
        assert_eq!(free_file_name("photo.jpg", |name| taken.contains(&name)), "photo (2).jpg");
        assert_eq!(free_file_name("photo.jpg", |_| false), "photo.jpg");
        assert_eq!(free_file_name("notes", |name| name == "notes"), "notes (1)");
        // A dotfile has no stem to split, so the counter lands at the end.
        assert_eq!(free_file_name(".bashrc", |name| name == ".bashrc"), ".bashrc (1)");
    }
}
