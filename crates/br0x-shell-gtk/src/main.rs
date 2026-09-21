//! br0x shell: GTK4 + WebKitGTK 6.0 window.
//! Thin UI over br0x-core policy. All timing rules live in core.

use adw::prelude::*;
use br0x_core::bookmarks::{Bookmark, BookmarkStore};
use br0x_core::history::{History, Visit};
use br0x_core::policy;
use br0x_core::prefs::{Prefs, PrefsStore};
use br0x_core::search::{self, SearchEngine};
use br0x_core::session::{Session, SessionStore, StoredTab};
use br0x_core::tab::{Action, TabId, TabSnapshot};
use gtk4::gio;
use gtk4::glib;
use std::cell::RefCell;
use std::collections::HashSet;
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
    pending_url: Option<String>,
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
            pending_url: None,
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

fn newtab_url() -> String {
    format!("file://{}", newtab_path())
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
    format!(
        r#"<tr class="history-row">
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
    let mut last_section = String::new();
    for v in &visits {
        let section = history_day_label(v.visited_at, today_day);
        let domain = display_domain(&v.url).to_string();
        let title = if v.title.is_empty() { v.url.clone() } else { v.title.clone() };
        let row = history_row(&v.url, &title, &domain, &history_time(v.visited_at));
        if section != last_section {
            if !last_section.is_empty()
                && let Some((_, body)) = sections.last_mut()
            {
                body.push_str("</tbody></table>");
            }
            sections.push((
                section.clone(),
                format!("<h2 class=\"day\">{section}</h2><table><tbody>{row}"),
            ));
            last_section = section;
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
      background:
        radial-gradient(900px 420px at 50% -6%, light-dark(#eef2fb, #2a3140) 0%, transparent 60%),
        light-dark(#ffffff, #1e1e1e);
      color: light-dark(#1c1c1c, #e8e8e8);
      font-family: system-ui, -apple-system, "Segoe UI", sans-serif;
      font-size: 14px;
      line-height: 1.5;
    }}
    .wrap {{
      max-width: 760px;
      margin: 0 auto;
      padding: 40px 24px 80px;
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
      color: light-dark(#8a8f9c, #8e8e8e);
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
        <span class="count">{count} entries</span>
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
          const text = row.textContent.toLowerCase();
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
</body>
</html>"#,
        count = visits.len(),
        q = html_escape(q),
        empty_state = empty_state,
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
            let Some(name) =
                std::path::Path::new(suggested).file_name().filter(|name| !name.is_empty())
            else {
                toasts_bad.add_toast(adw::Toast::new("Download failed: unsafe file name"));
                return false;
            };
            match dir.join(name).to_str() {
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

/// Write the start page for the chosen engine and return its file URL.
/// A file keeps the page offline, instant, and user editable.
/// Always rewrites: Frequent and Bookmarks embed live data, so a cache
/// by engine alone would serve stale sites.
fn write_newtab_page(engine: SearchEngine, frequent: &[Visit], bookmarks: &[Bookmark]) -> String {
    let path = newtab_path();
    if let Some(parent) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::write(&path, newtab_html(engine, frequent, bookmarks)).is_err() {
        eprintln!("br0x: could not write {path}");
    }
    newtab_url()
}

/// One site tile: real favicon over a letter fallback, name and domain.
/// `key` adds a silent number-key shortcut; `class` extends the styling.
fn site_card(url: &str, name: &str, key: Option<&str>, class: &str) -> String {
    let domain = display_domain(url);
    let letter = avatar_letter(name);
    let key_attr = key.map(|k| format!(" data-key=\"{}\"", html_escape(k))).unwrap_or_default();
    format!(
        r#"<a class="card {class}" href="{url}" title="{url}"{key_attr}>
            {fav}
            <span class="card-text"><span class="card-name">{name}</span><span class="card-domain">{domain}</span></span>
        </a>"#,
        class = html_escape(class),
        url = html_escape(url),
        key_attr = key_attr,
        fav = favicon_img(domain, &letter),
        name = html_escape(name),
        domain = html_escape(domain),
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

fn newtab_html(engine: SearchEngine, frequent: &[Visit], bookmarks: &[Bookmark]) -> String {
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
    let bookmark_cards: String = bookmarks
        .iter()
        .map(|b| {
            let name = if b.title.is_empty() {
                display_domain(&b.url).to_string()
            } else {
                b.title.clone()
            };
            site_card(&b.url, &name, None, "bookmark-card")
        })
        .collect();
    let bookmark_section = if bookmark_cards.is_empty() {
        String::new()
    } else {
        format!(
            r#"<h2 class="section-title">Bookmarks <span class="section-count">· {count}</span></h2><nav class="grid" id="bookmarks-grid">{bookmark_cards}</nav>"#,
            count = bookmarks.len(),
        )
    };
    let empty_hint = if frequent_cards.is_empty() && bookmark_cards.is_empty() {
        r#"<div class="hint-card"><p class="hint-title">A fresh start</p><p class="hint-sub">Star pages with Ctrl+D — they will appear here.</p></div>"#
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
      background:
        radial-gradient(1100px 520px at 50% -8%, light-dark(#e8eefc, #2b3346) 0%, transparent 62%),
        light-dark(#ffffff, #1e1e1e);
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
      position: relative;
      display: flex;
      flex-direction: column;
      align-items: center;
      text-align: center;
      z-index: 0;
    }}
    .hero::before {{
      content: "";
      position: absolute;
      inset: -36px -80px -20px;
      z-index: -1;
      pointer-events: none;
      background: radial-gradient(
        320px 120px at 50% 30%,
        color-mix(in srgb, AccentColor 22%, transparent),
        transparent 70%
      );
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
      color: light-dark(#8a8f9c, #8e8e8e);
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
      grid-template-columns: repeat(3, 1fr);
      gap: 10px;
      width: 100%;
      margin-top: 12px;
    }}
    @media (max-width: 640px) {{
      .wrap {{ padding-top: 7vh; }}
      .grid {{ grid-template-columns: repeat(3, 1fr); }}
    }}
    @media (max-width: 480px) {{
      .grid {{ grid-template-columns: repeat(2, 1fr); }}
      .search-field {{ padding: 12px 84px 12px 15px; font-size: 14px; }}
    }}
    a.card {{
      display: flex;
      align-items: center;
      gap: 10px;
      color: inherit;
      text-decoration: none;
      background-color: light-dark(#ffffff, #262626);
      border: 1px solid light-dark(#e8eaf0, #383838);
      border-radius: 16px;
      padding: 12px 14px;
      transition: transform 120ms ease, box-shadow 120ms ease;
    }}
    a.card:hover {{
      transform: translateY(-1px);
      border-color: color-mix(in srgb, AccentColor 55%, transparent);
      box-shadow: 0 6px 18px rgba(15, 23, 42, 0.1), 0 0 0 3px color-mix(in srgb, AccentColor 16%, transparent);
    }}
    .section-count {{
      font-weight: 600;
      color: light-dark(#a0a5b1, #6e6e6e);
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
      color: light-dark(#8a8f9c, #8e8e8e);
      white-space: nowrap;
      overflow: hidden;
      text-overflow: ellipsis;
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
    .hint {{
      color: light-dark(#b0b0b0, #666);
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
      color: white;
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
        <input class="search-field" id="search-input" name="{param}" placeholder="Search {engine_name} or enter address" autofocus autocomplete="off" spellcheck="false">
        <span class="engine-badge">{engine_name}</span>
      </div>
    </form>
    {bookmark_section}
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
      if (active && (active.tagName === 'INPUT' || active.tagName === 'TEXTAREA')) {{
        if (e.key === 'Escape') active.blur();
        return;
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
// otherwise early tabs would browse unprotected.
fn ensure_filter(tabs: Rc<RefCell<Tabs>>) {
    if FILTER_CACHE.get().is_some() {
        for entry in tabs.borrow().entries.iter() {
            attach_filter(&entry.view);
        }
        return;
    }
    let store = filter_store();
    let json = glib::Bytes::from_owned(BASE_FILTER_JSON.as_bytes().to_vec());
    glib::spawn_future_local(async move {
        match store.save_future("br0x-base", &json).await {
            Ok(filter) => {
                let _ = FILTER_CACHE.set(CachedFilter(filter));
                for entry in tabs.borrow().entries.iter() {
                    attach_filter(&entry.view);
                }
            }
            Err(e) => eprintln!("br0x: filter compile failed: {e}"),
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

fn selected_view(tab_view: &adw::TabView) -> Option<webkit6::WebView> {
    tab_view.selected_page().and_then(|p| p.child().downcast::<webkit6::WebView>().ok())
}

fn is_blank_uri(uri: &str) -> bool {
    uri == "about:blank" || uri == newtab_url()
}

/// Park `entry` if nothing blocks it. Returns the view whose process the
/// caller terminates once it has dropped its borrow of the tab list.
fn park_entry(entry: &mut TabEntry) -> Option<webkit6::WebView> {
    if entry.meta.parked {
        return None;
    }
    // Never park a page that is still loading: there is nothing to free and
    // the load would be interrupted.
    if entry.view.is_loading() {
        return None;
    }
    match entry.view.uri() {
        Some(uri) if !is_blank_uri(&uri) => entry.meta.pending_url = Some(uri.to_string()),
        // A fresh new tab has nothing to free. A restored tab parked on
        // about:blank still holds a process.
        Some(_) if entry.meta.pending_url.is_none() => return None,
        _ => {}
    }
    entry.meta.parked = true;
    let title = entry.page.title().to_string();
    if !title.is_empty() && !title.ends_with("• Parked") {
        entry.page.set_title(&format!("{title} • Parked"));
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

/// Rebuild the suggestion rows for `needle`. Always ends with an engine
/// row, so true means the popover has something to show. `urls` stays
/// parallel to the history rows: index == urls.len() is the engine row.
fn fill_suggestions(
    needle: &str,
    history: &Rc<RefCell<Option<History>>>,
    list: &gtk4::ListBox,
    urls: &Rc<RefCell<Vec<String>>>,
    engine: SearchEngine,
) -> bool {
    list.remove_all();
    let found = history.borrow().as_ref().and_then(|h| h.search(needle, SUGGESTION_LIMIT).ok());
    let visits = found.unwrap_or_default();
    let mut store = urls.borrow_mut();
    store.clear();
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
        store.push(visit.url);
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
  background: light-dark(#2f6fed, #7aa6ff); color: white; font: inherit; cursor: pointer; }}
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

struct Shell {
    tab_view: adw::TabView,
    entry: gtk4::Entry,
    window: adw::ApplicationWindow,
    back: gtk4::Button,
    fwd: gtk4::Button,
    reload: gtk4::Button,
    progress: gtk4::ProgressBar,
    engine_btn: gtk4::MenuButton,
    bookmark_btn: gtk4::Button,
    find_bar: gtk4::SearchBar,
    find_entry: gtk4::SearchEntry,
    find_status: gtk4::Label,
    toasts: adw::ToastOverlay,
    zoom_toast: RefCell<Option<adw::Toast>>,
    suggest_pop: gtk4::Popover,
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
        (view, page)
    }

    /// Open the start page in a fresh tab. Used by Ctrl+T, the new tab
    /// button, and the empty state.
    fn add_blank_tab(self: &Rc<Self>) {
        let (view, _) = self.create_tab(None, true);
        let engine = self.prefs.borrow().engine;
        let url = write_newtab_page(engine, &self.frequent_sites(), &self.bookmarks.borrow());
        view.load_uri(&url);
    }

    /// Top sites for the start page. Empty when history is unavailable.
    fn frequent_sites(&self) -> Vec<Visit> {
        self.history.borrow().as_ref().and_then(|h| h.top(8).ok()).unwrap_or_default()
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
        // An empty query would highlight every node in the page.
        if query.trim().is_empty() {
            return;
        }
        let Some(view) = selected_view(&self.tab_view) else {
            return;
        };
        let Some(controller) = view.find_controller() else {
            return;
        };
        if fresh {
            let options =
                (webkit6::FindOptions::CASE_INSENSITIVE | webkit6::FindOptions::WRAP_AROUND).bits();
            controller.search(query, options, u32::MAX);
        } else if forward {
            controller.search_next();
        } else {
            controller.search_previous();
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
        // The start page embeds bookmarks, so rewrite it and refresh any
        // open start pages: otherwise the change stays invisible.
        let snapshot = self.bookmarks.borrow().clone();
        write_newtab_page(self.prefs.borrow().engine, &self.frequent_sites(), &snapshot);
        self.reload_start_pages();
    }

    /// Reload tabs currently showing the start page (fresh data after a
    /// bookmark toggle or engine switch).
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
        let bookmarks_snapshot = self.bookmarks.borrow().clone();
        write_newtab_page(engine, &self.frequent_sites(), &bookmarks_snapshot);
        self.reload_start_pages();
        self.engine_btn.set_label(engine.name());
        self.entry.set_placeholder_text(Some("Search or type a URL"));
        self.toasts.add_toast(adw::Toast::new(&format!("Search engine: {}", engine.name())));
    }

    fn connect_view(self: &Rc<Self>, view: &webkit6::WebView, page: &adw::TabPage) {
        let page_clone = page.clone();
        let window_weak = self.window.downgrade();
        view.connect_title_notify(move |v| {
            if let Some(t) = v.title() {
                page_clone.set_title(&t);
                if page_clone.is_selected()
                    && let Some(w) = window_weak.upgrade()
                {
                    w.set_title(Some(&format!("{t} — br0x")));
                }
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
        // makes WebKit load the request into it. Without a handler they
        // would spawn invisible views outside the tab strip.
        let shell_weak = Rc::downgrade(self);
        view.connect_create(move |source, action| {
            let shell = shell_weak.upgrade()?;
            let uri = action.request().and_then(|r| r.uri());
            let (new_view, _) = shell.create_tab(Some(source), true);
            if let Some(url) = uri {
                new_view.load_uri(&url);
            }
            Some(new_view.upcast())
        });
    }

    /// Selected tab changed: sync entry, title, nav, resume parked tabs.
    fn on_selection_changed(&self) {
        // Decide under the borrow, act after it: loading a URI re-enters this
        // handler through signals, and a held borrow would panic.
        let resume = {
            let mut tabs = self.tabs.borrow_mut();
            self.tab_view.selected_page().and_then(|page| {
                let entry = tabs.entry_mut(&page)?;
                entry.meta.last_active = Instant::now();
                let url = if entry.meta.parked {
                    entry.meta.parked = false;
                    entry.meta.restored_at = Some(Instant::now());
                    entry.meta.pending_url.take()
                } else {
                    None
                };
                entry.view.set_is_muted(false);
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
        let sys = br0x_core::sampler::sample(self.tab_view.n_pages() as usize);
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
                        entry.view.set_is_muted(true);
                    }
                    Action::Park => {
                        if let Some(view) = park_entry(entry) {
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
        let mut open: HashSet<String> = self
            .tabs
            .borrow()
            .entries
            .iter()
            .filter_map(|e| e.view.uri().map(|uri| uri.to_string()))
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
        write_newtab_page(
            self.prefs.borrow().engine,
            &self.frequent_sites(),
            &self.bookmarks.borrow(),
        );
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
    write_newtab_page(prefs.engine, &[], &[]);

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
    engine_btn.set_tooltip_text(Some("Choose search engine (applies to new tabs)"));
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
    let suggest_urls: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));

    // Star toggle for bookmarks, at the right end of the address bar.
    let bookmark_btn = gtk4::Button::from_icon_name("non-starred-symbolic");
    bookmark_btn.set_tooltip_text(Some("Bookmark this page (Ctrl+D)"));
    bookmark_btn.add_css_class("flat");

    let omnibox_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
    omnibox_box.add_css_class("omnibox-frame");
    omnibox_box.set_hexpand(true);
    omnibox_box.set_size_request(480, 40);
    omnibox_box.append(&engine_btn);
    omnibox_box.append(&entry);
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
    menu.append(Some("Quit"), Some("win.quit"));
    let menu_btn = gtk4::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&menu)
        .tooltip_text("Menu")
        .build();

    let header = adw::HeaderBar::new();
    header.pack_start(&back);
    header.pack_start(&fwd);
    header.pack_start(&reload);
    header.set_title_widget(Some(&omnibox_box));
    header.pack_end(&menu_btn);
    header.pack_end(&new_btn);

    let progress = gtk4::ProgressBar::new();
    progress.add_css_class("hairline-progress");
    progress.set_visible(false);

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
    toolbar.add_top_bar(&progress);
    toolbar.add_top_bar(&find_bar);
    toolbar.set_content(Some(&tab_view));

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
        engine_btn: engine_btn.clone(),
        bookmark_btn: bookmark_btn.clone(),
        find_bar: find_bar.clone(),
        find_entry: find_entry.clone(),
        find_status,
        toasts: toasts.clone(),
        zoom_toast: RefCell::new(None),
        suggest_pop: suggest_popover.clone(),
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
        // Live find as you type, plus Enter to jump to the next match.
        let s = shell.clone();
        find_entry.connect_search_changed(move |e| {
            let text = e.text().to_string();
            if text.is_empty() {
                s.find_status.set_text("");
                return;
            }
            s.find_in_page(&text, true, true);
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
        entry.connect_activate(move |e| {
            let url = e.text().to_string();
            if url.trim().is_empty() {
                return;
            }
            if let Some(v) = selected_view(&s.tab_view) {
                v.load_uri(&search::resolve(&url, s.prefs.borrow().engine));
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
            if fill_suggestions(needle, &history, &list, &urls, s.prefs.borrow().engine) {
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
            let history_len = urls.borrow().len();
            // Trailing engine row searches the typed text, like Enter.
            let target = match index {
                Some(i) if i < history_len => urls.borrow().get(i).cloned(),
                Some(i) if i == history_len => {
                    let query = s.entry.text().to_string();
                    if query.trim().is_empty() {
                        None
                    } else {
                        Some(search::resolve(&query, s.prefs.borrow().engine))
                    }
                }
                _ => None,
            };
            popover.popdown();
            if let Some(url) = target
                && let Some(view) = selected_view(&s.tab_view)
            {
                view.load_uri(&url);
                view.grab_focus();
            }
        });
    }
    {
        let list = suggest_list.clone();
        let popover = suggest_popover.clone();
        let keys = gtk4::EventControllerKey::new();
        keys.connect_key_pressed(move |_, keyval, _, _| {
            if !popover.is_visible() {
                return glib::Propagation::Proceed;
            }
            if keyval == gtk4::gdk::Key::Down {
                if let Some(row) = list.row_at_index(0) {
                    row.grab_focus();
                }
                glib::Propagation::Stop
            } else if keyval == gtk4::gdk::Key::Escape {
                // Swallowed on purpose: Escape closes the list instead of
                // reaching the win.stop action that halts page loads.
                popover.popdown();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
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
    {
        let s = shell.clone();
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
            if tv.n_pages() == 0 {
                s.add_blank_tab();
                s.entry.grab_focus();
            }
        });
    }
    {
        let s = shell.clone();
        window.connect_close_request(move |_| {
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

    shell.start_session();
    ensure_filter(shell.tabs.clone());
    shell.install_actions(app);
    shell.on_selection_changed();
    if let Some(v) = selected_view(&shell.tab_view) {
        v.grab_focus();
    }
    window.present();
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
    fn blank_uris_are_recognised() {
        assert!(is_blank_uri("about:blank"));
        assert!(is_blank_uri(&newtab_url()));
        assert!(!is_blank_uri(""));
        assert!(!is_blank_uri("br0x://history"));
        assert!(!is_blank_uri("https://example.com"));
    }
}
