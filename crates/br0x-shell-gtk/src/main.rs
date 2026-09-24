//! br0x shell: GTK4 + WebKitGTK 6.0 window.
//! Thin UI over br0x-core policy. All timing rules live in core.

mod bench;
mod pages;
mod theme;
use adw::prelude::*;
use br0x_core::bookmarks::{Bookmark, BookmarkStore};
use br0x_core::curtain::CurtainStore;
use br0x_core::history::{History, Visit};
use br0x_core::policy;
use br0x_core::prefs::{Appearance, Prefs, PrefsStore, SleepTimeout};
use br0x_core::search::{self, SearchEngine};
use br0x_core::session::{Session, SessionStore, StoredTab};
use br0x_core::shield::{self, ShieldStore};
use br0x_core::tab::{Action, TabId, TabSnapshot};
use br0x_core::vault::{FileVaultStore, VaultStore};
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

/// Settings window type. Adwaita deprecated `PreferencesWindow` in favor of
/// `PreferencesDialog`, but the shell spec pins the former, so its uses are
/// allowed at the dialog builders below.
#[allow(deprecated)]
type SettingsWindow = adw::PreferencesWindow;

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
/// file:// URL with the path percent-encoded: a space or non-ASCII word
/// in $HOME must not break start-page identity checks elsewhere.
pub(crate) fn file_url(path: &str) -> String {
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

pub(crate) fn data_file(name: &str) -> String {
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

fn history_path() -> String {
    data_file("history.db")
}

fn bookmarks_path() -> String {
    data_file("bookmarks.json")
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
            Some(h) => pages::history_html(h, query.as_deref(), clear, crate::theme::current()),
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
// otherwise early tabs would browse unprotected. Sites the blocker is off
/// for stay unfiltered.
fn ensure_filter(shell: &Rc<Shell>) {
    if FILTER_CACHE.get().is_some() {
        for entry in shell.tabs.borrow().entries.iter() {
            let uri = entry.view.uri().map(|u| u.to_string()).unwrap_or_default();
            set_view_filter(&entry.view, shell.blocker_active(&domain_of(&uri)));
        }
        return;
    }
    let store = filter_store();
    let json = glib::Bytes::from_owned(BASE_FILTER_JSON.as_bytes().to_vec());
    let shell = shell.clone();
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
            for entry in shell.tabs.borrow().entries.iter() {
                let uri = entry.view.uri().map(|u| u.to_string()).unwrap_or_default();
                set_view_filter(&entry.view, shell.blocker_active(&domain_of(&uri)));
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
    uri == "about:blank" || uri == pages::start_page_url()
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
    // Strip first: the title may still carry the other badge, and stacking
    // them leaks into the window title, palette, and saved session.
    let base = strip_state_badges(entry.page.title().as_ref());
    if !base.is_empty() {
        entry.page.set_title(&format!("{base} {badge}"));
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
        let name = if title.trim().is_empty() {
            pages::display_domain(url).to_owned()
        } else {
            title.clone()
        };
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
            pages::display_domain(&visit.url).to_owned()
        } else {
            visit.title.clone()
        };
        let stack = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        stack.set_margin_top(4);
        stack.set_margin_bottom(4);
        // Title first, address second: the same hierarchy as open-tab rows
        // and every browser address dropdown.
        let title_label = gtk4::Label::new(Some(&title));
        title_label.set_xalign(0.0);
        title_label.set_hexpand(true);
        title_label.set_max_width_chars(64);
        title_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        let url_label = gtk4::Label::new(Some(&visit.url));
        url_label.set_xalign(0.0);
        url_label.set_hexpand(true);
        url_label.set_max_width_chars(64);
        url_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        url_label.add_css_class("dim-label");
        stack.append(&title_label);
        stack.append(&url_label);
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

/// Subsequence fuzzy score, lower is better. Matches case-insensitively in
/// order; each skipped character costs, so prefix and tight matches win.
/// Empty needle matches everything at zero: the zero-typing list.
fn fuzzy_score(needle: &str, haystack: &str) -> Option<u32> {
    let needle: Vec<char> = needle.to_lowercase().chars().collect();
    if needle.is_empty() {
        return Some(0);
    }
    let hay: Vec<char> = haystack.to_lowercase().chars().collect();
    let mut h = 0;
    let mut score = 0u32;
    for (n, nc) in needle.iter().enumerate() {
        let pos = hay[h..].iter().position(|c| c == nc)?;
        score += pos as u32 + if n == 0 { 0 } else { 1 };
        h += pos + 1;
    }
    Some(score)
}

/// One palette row: a tab to switch to, a place to open, or an action. Every
/// row carries an icon, so the list reads as controls instead of a word list.
#[derive(Clone)]
enum PaletteHit {
    Tab { title: String, url: String, page: adw::TabPage },
    Go { title: String, url: String, icon: &'static str },
    Action { icon: &'static str, label: String, hint: &'static str, action: &'static str },
}

/// Every palette-runnable action: label, shortcut hint, win.* action name.
const PALETTE_ACTIONS: &[(&str, &str, &str, &str)] = &[
    ("tab-new-symbolic", "New Tab", "Ctrl+T", "new-tab"),
    ("edit-undo-symbolic", "Reopen Closed Tab", "Ctrl+Shift+T", "reopen-tab"),
    ("view-fullscreen-symbolic", "Focus Mode", "Ctrl+Shift+F", "focus-mode"),
    ("edit-find-symbolic", "Find in Page", "Ctrl+F", "find"),
    ("star-new-symbolic", "Bookmark This Page", "Ctrl+D", "bookmark-page"),
    ("view-pin-symbolic", "Pin Tab", "Ctrl+Shift+P", "toggle-pin"),
    ("accessories-dictionary-symbolic", "Reader Mode", "Ctrl+Shift+R", "reader"),
    ("view-conceal-symbolic", "Hide Element", "Ctrl+Shift+H", "curtain-pick"),
    ("preferences-system-privacy-symbolic", "Ad Blocker for Site", "Ctrl+Shift+S", "toggle-shield"),
    ("view-list-symbolic", "Toggle Sidebar", "F9", "toggle-sidebar"),
    ("document-open-recent-symbolic", "History", "Ctrl+H", "history"),
    ("edit-copy-symbolic", "Copy URL", "", "copy-url"),
    ("go-previous-symbolic", "Go Back", "Alt+Left", "back"),
    ("go-next-symbolic", "Go Forward", "Alt+Right", "forward"),
    ("view-refresh-symbolic", "Reload", "Ctrl+R", "reload"),
    ("zoom-in-symbolic", "Zoom In", "Ctrl++", "zoom-in"),
    ("zoom-out-symbolic", "Zoom Out", "Ctrl+-", "zoom-out"),
    ("zoom-original-symbolic", "Reset Zoom", "Ctrl+0", "zoom-reset"),
    ("preferences-system-symbolic", "Settings", "Ctrl+,", "settings"),
];

/// Most palette rows shown at once.
const PALETTE_LIMIT: usize = 9;

/// Lowercase host of a URI without port, path, query or fragment.
/// Delegates to the core shield normalizer so the shell and the store agree.
fn domain_of(uri: &str) -> String {
    shield::normalize_domain(uri)
}

/// Origin (`scheme://host`, lowercased) for login matching. Host scoped on
/// purpose: logins stay filed per site, and the file shape matches the core
/// file vault (`{origin: {username: secret}}`) so the store can be swapped
/// without migrating saved logins.
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
    // Loop: a re-parked tab can stack "... • Parked • Sleeping".
    let mut out = title;
    loop {
        let next = out
            .strip_suffix(" • Parked")
            .or_else(|| out.strip_suffix(" • Sleeping"))
            .unwrap_or(out);
        if next.len() == out.len() {
            return out.to_owned();
        }
        out = next;
    }
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

/// Arms the curtain picker: the next click captures a stable selector,
/// hides the element immediately, and stashes the selector on
/// `window.__br0xPick` for the shell to collect. Anchors on stable ids and
/// test attributes, verifies single-element resolve, and refuses the page
/// itself (`!refuse:page`) instead of blanking the site.
const CURTAIN_ARM_JS: &str = r##"window.__br0xPick=null;
if(!window.__br0xPickHandler){window.__br0xPickHandler=function(e){e.preventDefault();e.stopPropagation();
var el=e.target;
if(!el||el===document.documentElement||el===document.body){window.__br0xPick='!refuse:page';return;}
function seg(n){var s=n.tagName.toLowerCase();var dn=null;var attrs=['data-testid','data-test','data-qa'];for(var k=0;k<attrs.length;k++){if(n.getAttribute&&n.getAttribute(attrs[k])){dn=attrs[k];break;}}if(dn){return {one:s+'['+dn+'="'+String(n.getAttribute(dn)).replace(/"/g,'')+'"]',stop:1};}var id=n.id||'';if(id&&/^[A-Za-z_][\w:.-]*$/.test(id)&&!/\d{4,}/.test(id)&&!/^(ad|ads|banner|popup|modal|cookie)/i.test(id)){return {one:'#'+CSS.escape(id),stop:1};}var cls=(n.className&&typeof n.className==='string')?n.className.trim().split(/\s+/).filter(function(c){return /^[A-Za-z_-][\w-]*$/.test(c)&&!/^(ad|ads)/i.test(c);}).slice(0,2):[];if(cls.length){s+='.'+cls.map(function(c){return CSS.escape(c);}).join('.');}var par=n.parentElement;if(par){var sibs=Array.prototype.filter.call(par.children,function(c){return c.tagName===n.tagName;});if(sibs.length>1){s+=':nth-of-type('+(sibs.indexOf(n)+1)+')';}}return {one:s,stop:0};}
var cur=el;var parts=[];var depth=0;var done=false;
while(cur&&cur.nodeType===1&&cur!==document.documentElement&&cur!==document.body&&depth<6){var r=seg(cur);parts.unshift(r.one);if(r.stop){var cand=parts.join(' > ');try{if(document.querySelectorAll(cand).length===1){window.__br0xPick=cand;done=true;}}catch(_){}if(done){break;}}cur=cur.parentElement;depth++;}
if(!done){if(!parts.length){window.__br0xPick='!refuse:page';return;}window.__br0xPick=parts.join(' > ');}
try{el.style.setProperty('display','none','important');}catch(_){}};document.addEventListener('click',window.__br0xPickHandler,true);}"##;

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
    let title = tab_view
        .selected_page()
        .map(|p| strip_state_badges(p.title().as_ref()))
        .unwrap_or_default();
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

/// One sweep over background tabs with the user's sleep timeout. `None`
/// disables the time-based release; pressure-driven parking still applies.
fn sweep_with_sleep(
    tabs: &[TabSnapshot],
    sys: &br0x_core::tab::SysState,
    sleep_secs: Option<u64>,
) -> Vec<(TabId, Action)> {
    let base = policy::params_for(sys.tab_count);
    let params =
        br0x_core::tab::PolicyParams { sleep_secs: sleep_secs.unwrap_or(u64::MAX), ..base };
    tabs.iter()
        .map(|t| (t.id, policy::decide_with_params(t, sys, &params)))
        .filter(|(_, a)| *a != Action::Keep)
        .collect()
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
    key_btn: gtk4::Button,
    star_btn: gtk4::Button,
    engine_btn: gtk4::MenuButton,
    find_bar: gtk4::SearchBar,
    find_entry: gtk4::SearchEntry,
    find_status: gtk4::Label,
    toasts: adw::ToastOverlay,
    transient_toast: RefCell<Option<adw::Toast>>,
    last_session: RefCell<Vec<StoredTab>>,
    last_save: RefCell<Option<Instant>>,
    last_sample: RefCell<Option<(Instant, br0x_core::tab::SysState)>>,
    /// Engine, frequent list and scheme the start page file was last built from.
    last_newtab: RefCell<Option<(SearchEngine, Vec<Visit>, theme::Scheme)>>,
    /// Address bar suggestion list, dismissed when the palette summons.
    suggest_pop: gtk4::Popover,
    sidebar_reveal: gtk4::Revealer,
    sidebar_box: gtk4::Box,
    sidebar_pins: gtk4::FlowBox,
    sidebar_list: gtk4::ListBox,
    sidebar_head_label: gtk4::Label,
    sidebar_collapse_btn: gtk4::Button,
    header_sidebar_btn: gtk4::Button,
    toolbar: adw::ToolbarView,
    palette_card: gtk4::Box,
    palette_entry: gtk4::SearchEntry,
    palette_list: gtk4::ListBox,
    palette_store: RefCell<Vec<PaletteHit>>,
    /// Menu rows that name the current state instead of a fixed verb.
    pin_label: gtk4::Label,
    shield_label: gtk4::Label,
    /// Settings switches mirrored from outside (F9, rail button) so an
    /// open window never shows the opposite of the truth.
    settings_sidebar_row: RefCell<Option<adw::SwitchRow>>,
    settings_rail_row: RefCell<Option<adw::SwitchRow>>,
    sidebar_pages: RefCell<Vec<adw::TabPage>>,
    sidebar_visible: RefCell<bool>,
    sidebar_rail: RefCell<bool>,
    /// Tab selected before the current one, so deselection stamps idle time.
    prev_selected: RefCell<Option<adw::TabPage>>,
    key_pop: gtk4::Popover,
    key_list: gtk4::ListBox,
    shield: Rc<RefCell<ShieldStore>>,
    curtain: Rc<RefCell<CurtainStore>>,
    vault: Rc<RefCell<FileVaultStore>>,
    picker_armed: RefCell<bool>,
    /// The exact view the picker was armed on. Disarm, timeout, and cancel
    /// must evaluate on this view: with tab switches in between, the
    /// selected view is a different page and disarming it leaves a live
    /// click handler behind that hides elements nobody persists.
    picker_view: RefCell<Option<webkit6::WebView>>,
    settings_win: RefCell<Option<SettingsWindow>>,
    popouts: RefCell<Vec<gtk4::Window>>,
    history: Rc<RefCell<Option<History>>>,
    bookmarks: Rc<RefCell<Vec<Bookmark>>>,
    bookmarks_store: BookmarkStore,
    context: webkit6::WebContext,
    session: webkit6::NetworkSession,
    prefs: Rc<RefCell<Prefs>>,
    prefs_store: PrefsStore,
    tabs: Rc<RefCell<Tabs>>,
}

/// One tab's sidebar state, snapshotted under a single borrow so row
/// building never touches the tab list.
struct SidebarItem {
    page: adw::TabPage,
    title: String,
    pinned: bool,
    loading: bool,
    sleeping: bool,
    parked: bool,
    attention: bool,
    selected: bool,
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
        let scheme = theme::current();
        pages::sync_newtab_page(&self.last_newtab, engine, &frequent, scheme)
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

    /// Menu rows that name the current state: Unpin vs Pin, blocker on or
    /// off for the selected tab's site.
    fn refresh_menu_labels(&self) {
        let pinned = self.tab_view.selected_page().is_some_and(|p| p.is_pinned());
        self.pin_label.set_text(if pinned { "Unpin Tab" } else { "Pin Tab" });
        let domain = selected_view(&self.tab_view)
            .and_then(|v| v.uri().map(|u| u.to_string()))
            .map(|uri| domain_of(&uri))
            .unwrap_or_default();
        self.shield_label.set_text(if domain.is_empty() {
            "Blocker for This Site"
        } else if self.blocker_active(&domain) {
            "Blocker On for This Site"
        } else {
            "Blocker Off for This Site"
        });
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
        }
        self.refresh_star_button();
    }

    /// Star button mirrors the selected page: filled when saved, outline
    /// otherwise, quiet on pages that cannot be saved.
    fn refresh_star_button(&self) {
        let uri = selected_view(&self.tab_view).and_then(|v| v.uri().map(|u| u.to_string()));
        let saveable = uri.as_ref().is_some_and(|u| !is_blank_uri(u) && !u.starts_with("br0x://"));
        let starred = saveable
            && uri
                .as_ref()
                .is_some_and(|u| BookmarkStore::is_bookmarked(&self.bookmarks.borrow(), u));
        self.star_btn.set_icon_name(if starred {
            "starred-symbolic"
        } else {
            "non-starred-symbolic"
        });
        self.star_btn.set_tooltip_text(Some(if starred {
            "Bookmarked — click to remove (Ctrl+D)"
        } else {
            "Bookmark this page (Ctrl+D)"
        }));
        self.star_btn.set_sensitive(saveable);
    }

    /// Reload tabs currently showing the start page (fresh data after an
    /// engine switch).
    fn reload_start_pages(&self) {
        let start = pages::start_page_url();
        for entry in self.tabs.borrow().entries.iter() {
            if entry.view.uri().is_some_and(|u| u.as_str() == start) {
                entry.view.reload();
            }
        }
    }

    /// Reload open History pages (their color scheme bakes in at serve
    /// time, so an appearance switch must repaint them too).
    fn reload_history_pages(&self) {
        for entry in self.tabs.borrow().entries.iter() {
            if entry.view.uri().is_some_and(|u| u.as_str().starts_with("br0x://history")) {
                entry.view.reload();
            }
        }
    }

    /// Every tab with its live identity: title plus real URL even when the
    /// view sits on about:blank after a release.
    fn all_tabs(&self) -> Vec<(String, String, adw::TabPage)> {
        let tabs = self.tabs.borrow();
        tabs.entries
            .iter()
            .map(|entry| {
                let title = strip_state_badges(entry.page.title().as_ref());
                // A released tab sits on about:blank: match its real URL.
                let live = entry.view.uri().map(|uri| uri.to_string()).unwrap_or_default();
                let url = if live.is_empty() || is_blank_uri(&live) {
                    entry.meta.pending_url.clone().unwrap_or_default()
                } else {
                    live
                };
                (title, url, entry.page.clone())
            })
            .collect()
    }

    /// Open tabs matching `needle` for the address bar: (title, url, page).
    fn open_tab_hits(&self, needle: &str) -> Vec<(String, String, adw::TabPage)> {
        let query = needle.to_lowercase();
        self.all_tabs()
            .into_iter()
            .filter(|(title, url, _)| {
                title.to_lowercase().contains(&query) || url.to_lowercase().contains(&query)
            })
            .take(4)
            .collect()
    }

    /// Ranked palette rows over tabs, actions, history, and bookmarks.
    /// Empty needle lists open tabs only: the zero-typing tab switcher.
    fn palette_hits(&self, needle: &str) -> Vec<PaletteHit> {
        let query = needle.trim();
        let mut scored: Vec<(u32, u8, PaletteHit)> = Vec::new();
        for (title, url, page) in self.all_tabs() {
            let name = if title.trim().is_empty() {
                pages::display_domain(&url).to_owned()
            } else {
                title.clone()
            };
            if let Some(score) = fuzzy_score(query, &name).or_else(|| fuzzy_score(query, &url)) {
                scored.push((score, 0, PaletteHit::Tab { title: name, url, page }));
            }
        }
        if !query.is_empty() {
            for (icon, label, hint, action) in PALETTE_ACTIONS {
                if let Some(score) = fuzzy_score(query, label) {
                    scored.push((
                        score,
                        1,
                        PaletteHit::Action { icon, label: label.to_string(), hint, action },
                    ));
                }
            }
            if let Some(history) = self.history.borrow().as_ref() {
                for visit in history.recent(60).unwrap_or_default() {
                    let name = if visit.title.trim().is_empty() {
                        pages::display_domain(&visit.url).to_owned()
                    } else {
                        visit.title.clone()
                    };
                    if let Some(score) =
                        fuzzy_score(query, &name).or_else(|| fuzzy_score(query, &visit.url))
                    {
                        scored.push((
                            score,
                            2,
                            PaletteHit::Go {
                                title: name,
                                url: visit.url,
                                icon: "document-open-recent-symbolic",
                            },
                        ));
                    }
                }
            }
            for mark in self.bookmarks.borrow().iter() {
                let name = if mark.title.trim().is_empty() {
                    mark.url.clone()
                } else {
                    mark.title.clone()
                };
                if let Some(score) =
                    fuzzy_score(query, &name).or_else(|| fuzzy_score(query, &mark.url))
                {
                    scored.push((
                        score,
                        2,
                        PaletteHit::Go {
                            title: name,
                            url: mark.url.clone(),
                            icon: "user-bookmarks-symbolic",
                        },
                    ));
                }
            }
        }
        scored.sort_by_key(|s| (s.0, s.1));
        scored.truncate(PALETTE_LIMIT);
        let mut hits: Vec<PaletteHit> = scored.into_iter().map(|(_, _, hit)| hit).collect();
        // The typed text stays runnable no matter how many rows match:
        // trailing search row, never truncated away.
        if !needle.trim().is_empty() {
            let engine = self.prefs.borrow().engine;
            hits.push(PaletteHit::Go {
                title: format!("Search {} for \"{}\"", engine.name(), needle.trim()),
                url: search::resolve(needle, engine),
                icon: "system-search-symbolic",
            });
        }
        hits
    }

    /// Render the palette rows for the current entry text.
    fn render_palette(&self) {
        let query = self.palette_entry.text().to_string();
        let hits = self.palette_hits(&query);
        self.palette_list.remove_all();
        for hit in &hits {
            let (icon, name, hint) = match hit {
                PaletteHit::Tab { title, url, .. } => {
                    ("web-browser-symbolic", title.clone(), format!("Open tab · {url}"))
                }
                PaletteHit::Go { title, url, icon } => (*icon, title.clone(), url.clone()),
                PaletteHit::Action { icon, label, hint, .. } => {
                    (*icon, label.clone(), hint.to_string())
                }
            };
            let row_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
            row_box.set_margin_start(12);
            let image = gtk4::Image::from_icon_name(icon);
            image.set_pixel_size(16);
            image.set_valign(gtk4::Align::Center);
            let stack = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
            stack.set_margin_top(5);
            stack.set_margin_bottom(5);
            stack.set_margin_start(2);
            stack.set_margin_end(12);
            stack.set_hexpand(true);
            let title_label = gtk4::Label::new(Some(&name));
            title_label.set_xalign(0.0);
            title_label.set_hexpand(true);
            title_label.set_max_width_chars(60);
            title_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            let hint_label = gtk4::Label::new(Some(&hint));
            hint_label.set_xalign(0.0);
            hint_label.set_hexpand(true);
            hint_label.set_max_width_chars(60);
            hint_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            hint_label.add_css_class("dim-label");
            stack.append(&title_label);
            stack.append(&hint_label);
            row_box.append(&image);
            row_box.append(&stack);
            let row = gtk4::ListBoxRow::new();
            // Rows never take focus: clicks must not move it out of the
            // entry, or the focus-leave dismiss wins the race against
            // activation. Arrows still move the selection.
            row.set_focusable(false);
            row.set_child(Some(&row_box));
            self.palette_list.append(&row);
        }
        *self.palette_store.borrow_mut() = hits;
        if self.palette_list.row_at_index(0).is_some() {
            self.palette_list.select_row(self.palette_list.row_at_index(0).as_ref());
        }
    }

    /// Run the selected palette row, the first row, or the typed text.
    fn activate_palette(self: &Rc<Self>) {
        let hit = {
            let hits = self.palette_store.borrow();
            self.palette_list
                .selected_row()
                .and_then(|row| usize::try_from(row.index()).ok())
                .filter(|i| *i < hits.len())
                .and_then(|i| hits.get(i).cloned())
        };
        self.hide_palette();
        match hit {
            Some(PaletteHit::Tab { page, .. }) => {
                self.tab_view.set_selected_page(&page);
                if let Some(v) = selected_view(&self.tab_view) {
                    v.grab_focus();
                }
            }
            Some(PaletteHit::Go { url, .. }) => {
                if let Some(v) = selected_view(&self.tab_view) {
                    v.load_uri(&url);
                    v.grab_focus();
                }
            }
            Some(PaletteHit::Action { action, .. }) => {
                gio::prelude::ActionGroupExt::activate_action(&self.window, action, None);
            }
            None => {
                let query = self.palette_entry.text().to_string();
                if !query.trim().is_empty()
                    && let Some(v) = selected_view(&self.tab_view)
                {
                    v.load_uri(&search::resolve(&query, self.prefs.borrow().engine));
                    v.grab_focus();
                }
            }
        }
    }

    /// Summon the palette over the page, listing open tabs immediately.
    fn toggle_palette(self: &Rc<Self>) {
        if self.palette_card.is_visible() {
            self.hide_palette();
            if let Some(v) = selected_view(&self.tab_view) {
                v.grab_focus();
            }
            return;
        }
        self.suggest_pop.popdown();
        self.palette_entry.set_text("");
        self.render_palette();
        self.palette_card.set_visible(true);
        self.palette_entry.grab_focus();
    }

    fn hide_palette(&self) {
        self.palette_card.set_visible(false);
    }

    /// Immersive mode: the top bars slide away and the page owns the
    /// window. Session-only, so a restart never traps anyone chromeless.
    fn toggle_focus_mode(self: &Rc<Self>) {
        let hiding = self.toolbar.reveals_top_bars();
        self.toolbar.set_reveal_top_bars(!hiding);
        self.toasts.add_toast(adw::Toast::new(if hiding {
            "Focus mode on — Ctrl+Shift+F brings the chrome back"
        } else {
            "Focus mode off"
        }));
    }

    /// Apply the blocker's effective state to `view`: an explicit per-site
    /// override wins, otherwise the global default decides. Navigations call
    /// this on Started; the toggle and the default switch call it directly.
    fn apply_shield_to_view(&self, view: &webkit6::WebView) {
        let uri = view.uri().map(|u| u.to_string()).unwrap_or_default();
        set_view_filter(view, self.blocker_active(&domain_of(&uri)));
    }

    /// Effective blocker state for `domain`: explicit override, else global.
    fn blocker_active(&self, domain: &str) -> bool {
        self.shield.borrow().get(domain).unwrap_or_else(|| self.prefs.borrow().blocker_enabled)
    }

    /// Per-site overrides, sorted by domain, for the settings list.
    fn shield_exceptions(&self) -> Vec<(String, bool)> {
        self.shield.borrow().overrides()
    }

    /// Flip the blocker for the selected tab's domain (global default when
    /// unset). Reachable from the menu and the settings Privacy page.
    fn toggle_shield_to(self: &Rc<Self>, enabled: bool) {
        let Some(view) = selected_view(&self.tab_view) else {
            return;
        };
        let domain = domain_of(&view.uri().map(|u| u.to_string()).unwrap_or_default());
        if domain.is_empty() {
            self.toasts.add_toast(adw::Toast::new("Open a website first, then flip its blocker"));
            return;
        }
        if let Err(e) = self.shield.borrow_mut().set(&domain, enabled) {
            eprintln!("br0x: shield save failed: {e}");
        }
        self.apply_shield_to_view(&view);
        let state = if enabled { "on" } else { "off" };
        self.refresh_menu_labels();
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
        if domain_of(&view.uri().map(|u| u.to_string()).unwrap_or_default()).is_empty() {
            self.toasts.add_toast(adw::Toast::new("Hiding works on websites, not internal pages"));
            return;
        }
        *self.picker_armed.borrow_mut() = true;
        *self.picker_view.borrow_mut() = Some(view.clone());
        eval_text(&view, CURTAIN_ARM_JS, |_| {});
        self.toasts.add_toast(adw::Toast::new("Picker on — click the element to hide"));
        self.poll_picker(view);
    }

    fn disarm_picker(&self) {
        *self.picker_armed.borrow_mut() = false;
        if let Some(view) = self.picker_view.borrow().as_ref() {
            eval_text(view, CURTAIN_DISARM_JS, |_| {});
        }
        *self.picker_view.borrow_mut() = None;
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
            let shell_next = shell.clone();
            let view_next = view.clone();
            eval_text(&view, CURTAIN_POLL_JS, move |picked| {
                let picked = picked.trim().to_owned();
                if picked.is_empty() || !*shell_next.picker_armed.borrow() {
                    return;
                }
                if picked == "!refuse:page" {
                    eval_text(&view_next, "window.__br0xPick=null", |_| {});
                    shell_next.toasts.add_toast(adw::Toast::new(
                        "Can't hide the whole page — pick something smaller",
                    ));
                    return;
                }
                // Read the URI at collect time: a navigation mid-pick must
                // not file the selector under the old domain.
                let domain = domain_of(&view_next.uri().map(|u| u.to_string()).unwrap_or_default());
                if domain.is_empty() {
                    shell_next.disarm_picker();
                    shell_next
                        .toasts
                        .add_toast(adw::Toast::new("Hiding works on websites, not internal pages"));
                    return;
                }
                *shell_next.picker_armed.borrow_mut() = false;
                *shell_next.picker_view.borrow_mut() = None;
                let added = shell_next.curtain.borrow_mut().hide(&domain, &picked);
                shell_next.apply_curtain_to_view(&view_next);
                shell_next.toasts.add_toast(adw::Toast::new(match added {
                    Ok(true) => "Element hidden on this site",
                    Ok(false) => "Already hidden on this site",
                    Err(e) => {
                        eprintln!("br0x: curtain save failed: {e}");
                        "Could not hide this element"
                    }
                }));
            });
            glib::ControlFlow::Continue
        });
    }

    /// Re-apply the curtain stylesheet and reload every open tab on
    /// `domain`. Hiding is injected at document start, so store changes
    /// only take effect on loaded pages through an explicit reload.
    fn reload_domain_tabs(&self, domain: &str) {
        for entry in self.tabs.borrow().entries.iter() {
            let uri = entry.view.uri().map(|u| u.to_string()).unwrap_or_default();
            if domain_of(&uri) == domain {
                self.apply_curtain_to_view(&entry.view);
                entry.view.reload();
            }
        }
    }

    /// Forget every hidden selector on the current site and reload it.
    fn curtain_clear_site(self: &Rc<Self>) {
        let Some(view) = selected_view(&self.tab_view) else {
            return;
        };
        let domain = domain_of(&view.uri().map(|u| u.to_string()).unwrap_or_default());
        match self.curtain.borrow_mut().clear_domain(&domain) {
            Ok(true) => {
                self.reload_domain_tabs(&domain);
                self.toasts.add_toast(adw::Toast::new("Unhidden — reloading this site"));
            }
            Ok(false) => {
                self.toasts.add_toast(adw::Toast::new("Nothing hidden on this site"));
            }
            Err(e) => {
                eprintln!("br0x: curtain save failed: {e}");
                self.toasts.add_toast(adw::Toast::new("Could not unhide this site"));
            }
        }
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
        let script = pages::reader_extract_js(theme::current());
        eval_text(&eval_view, &script, move |html| {
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
        let safe = pages::html_escape(url);
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
        let saved = self.vault.borrow().credentials_for(&origin);
        if saved.is_empty() {
            let hint = gtk4::Label::new(Some("No logins saved for this site"));
            hint.set_xalign(0.0);
            hint.add_css_class("dim-label");
            hint.set_margin_top(8);
            hint.set_margin_bottom(4);
            hint.set_margin_start(12);
            hint.set_margin_end(12);
            let row = gtk4::ListBoxRow::new();
            row.set_child(Some(&hint));
            row.set_selectable(false);
            row.set_activatable(false);
            self.key_list.append(&row);
        }
        for entry in saved {
            let label = gtk4::Label::new(Some(&format!("Fill login as {}", entry.username)));
            label.set_xalign(0.0);
            label.set_margin_top(6);
            label.set_margin_bottom(6);
            label.set_margin_start(12);
            label.set_margin_end(12);
            let row = gtk4::ListBoxRow::new();
            row.set_child(Some(&label));
            self.key_list.append(&row);
        }
        let save =
            gtk4::Label::new(Some(if self.vault.borrow().credentials_for(&origin).is_empty() {
                "Save this login"
            } else {
                "Save another login"
            }));
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
            } else {
                match shell.vault.borrow_mut().save(&origin, &username, &secret) {
                    Ok(()) => {
                        shell.toasts.add_toast(adw::Toast::new("Login saved on this device"));
                    }
                    Err(e) => {
                        eprintln!("br0x: vault save failed: {e}");
                        shell.toasts.add_toast(adw::Toast::new("Could not save this login"));
                    }
                }
            }
            shell.key_pop.popdown();
        });
    }

    /// Rebuild the sidebar in tab-strip order: pinned tabs as an icon-only
    /// row at the top, the rest as favicon + title + close rows. The top
    /// tab bar keeps working untouched. Called on add/close/title/icon/
    /// loading/pin/selection changes.
    fn refresh_sidebar(&self) {
        if !*self.sidebar_visible.borrow() {
            return;
        }
        while let Some(row) = self.sidebar_list.row_at_index(0) {
            self.sidebar_list.remove(&row);
        }
        while let Some(child) = self.sidebar_pins.first_child() {
            self.sidebar_pins.remove(&child);
        }
        let rail = *self.sidebar_rail.borrow();
        let selected = self.tab_view.selected_page();
        // Snapshot under one borrow; widgets are built after it.
        let items: Vec<SidebarItem> = {
            let tabs = self.tabs.borrow();
            let mut positioned: Vec<(i32, SidebarItem)> = tabs
                .entries
                .iter()
                .map(|e| {
                    (
                        self.tab_view.page_position(&e.page),
                        SidebarItem {
                            page: e.page.clone(),
                            title: strip_state_badges(&e.page.title()),
                            pinned: e.page.is_pinned(),
                            loading: e.page.is_loading(),
                            sleeping: e.meta.sleeping,
                            parked: e.meta.parked,
                            attention: e.page.needs_attention(),
                            selected: Some(&e.page) == selected.as_ref(),
                        },
                    )
                })
                .collect();
            positioned.sort_by_key(|(pos, _)| *pos);
            positioned.into_iter().map(|(_, item)| item).collect()
        };
        let tab_view = self.tab_view.clone();
        let mut pages = Vec::new();
        for item in &items {
            if item.pinned && !rail {
                let icon = item
                    .page
                    .icon()
                    .map(|gicon| gtk4::Image::from_gicon(&gicon))
                    .unwrap_or_else(|| gtk4::Image::from_icon_name("web-browser-symbolic"));
                icon.set_pixel_size(16);
                let btn = gtk4::Button::new();
                btn.set_child(Some(&icon));
                btn.add_css_class("flat");
                let title =
                    if item.title.is_empty() { "New Tab".to_owned() } else { item.title.clone() };
                btn.set_tooltip_text(Some(&title));
                if item.selected {
                    btn.add_css_class("sidebar-pin-active");
                }
                let page = item.page.clone();
                let tv = tab_view.clone();
                btn.connect_clicked(move |_| {
                    tv.set_selected_page(&page);
                    if let Some(v) = selected_view(&tv) {
                        v.grab_focus();
                    }
                });
                // Middle-click closes a pinned tab without selecting it.
                let page = item.page.clone();
                let tv = tab_view.clone();
                let middle = gtk4::GestureClick::new();
                middle.set_button(2);
                middle.connect_pressed(move |_, _, _, _| {
                    tv.close_page(&page);
                });
                btn.add_controller(middle);
                self.sidebar_pins.insert(&btn, -1);
            } else {
                pages.push(item.page.clone());
                self.sidebar_list.append(&self.sidebar_row(item, &tab_view));
            }
        }
        self.sidebar_pins.set_visible(items.iter().any(|i| i.pinned && !rail));
        *self.sidebar_pages.borrow_mut() = pages;
        self.sync_sidebar_head();
    }

    /// Rail mode hides the label and offers expansion; expanded mode offers
    /// collapse. Called on every refresh so the header can never lie.
    fn sync_sidebar_head(&self) {
        let rail = *self.sidebar_rail.borrow();
        let count = self.tab_view.n_pages();
        self.sidebar_head_label.set_text(&if count == 1 {
            "1 tab".to_owned()
        } else {
            format!("{count} tabs")
        });
        self.sidebar_head_label.set_visible(!rail);
        self.sidebar_collapse_btn.set_icon_name(if rail {
            "pan-end-symbolic"
        } else {
            "pan-start-symbolic"
        });
        self.sidebar_collapse_btn.set_tooltip_text(Some(if rail {
            "Expand sidebar"
        } else {
            "Collapse sidebar to icons"
        }));
    }

    /// One unpinned sidebar row: a single fixed-height line of unread dot,
    /// favicon, title, state badge, and close button. Nothing here may wrap
    /// or change the row metrics, so badges and dots never shift the layout.
    fn sidebar_row(&self, item: &SidebarItem, tab_view: &adw::TabView) -> gtk4::ListBoxRow {
        let rail = *self.sidebar_rail.borrow();
        let slot = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        slot.set_margin_top(6);
        slot.set_margin_bottom(6);
        slot.set_margin_start(12);
        slot.set_margin_end(6);
        // Attention rides on the favicon as a corner dot: the title never
        // shifts, and no gutter is reserved when nothing is unread.
        let icon = item
            .page
            .icon()
            .map(|gicon| gtk4::Image::from_gicon(&gicon))
            .unwrap_or_else(|| gtk4::Image::from_icon_name("web-browser-symbolic"));
        // Capped: favicon textures range from 16 to 180 px by site.
        icon.set_pixel_size(16);
        icon.set_valign(gtk4::Align::Center);
        icon.add_css_class("favicon");
        if item.attention && !rail {
            let dot = gtk4::Label::new(Some("•"));
            dot.add_css_class("sidebar-dot");
            dot.set_halign(gtk4::Align::End);
            dot.set_valign(gtk4::Align::Start);
            let badge = gtk4::Overlay::new();
            badge.set_child(Some(&icon));
            badge.add_overlay(&dot);
            slot.append(&badge);
        } else {
            slot.append(&icon);
        }
        // Rail rows carry no close button at all.
        let mut close_btn: Option<gtk4::Button> = None;
        if !rail {
            let title = gtk4::Label::new(None);
            let name =
                if item.title.is_empty() { "New Tab".to_owned() } else { item.title.clone() };
            title.set_text(&name);
            title.set_xalign(0.0);
            title.set_hexpand(true);
            title.set_max_width_chars(34);
            title.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            title.set_valign(gtk4::Align::Center);
            slot.append(&title);
            let badge = if item.sleeping {
                "Sleeping"
            } else if item.parked {
                "Parked"
            } else if item.loading {
                "Loading"
            } else {
                ""
            };
            if !badge.is_empty() {
                let badge_label = gtk4::Label::new(Some(badge));
                badge_label.set_valign(gtk4::Align::Center);
                badge_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
                badge_label.set_max_width_chars(10);
                badge_label.add_css_class("sidebar-badge");
                slot.append(&badge_label);
            }
            let close = gtk4::Button::from_icon_name("window-close-symbolic");
            close.set_tooltip_text(Some("Close tab (Ctrl+W)"));
            close.add_css_class("flat");
            close.add_css_class("close-btn");
            close.set_valign(gtk4::Align::Center);
            close.set_opacity(0.0);
            close.set_can_target(false);
            let page = item.page.clone();
            let tv = tab_view.clone();
            close.connect_clicked(move |_| {
                tv.close_page(&page);
            });
            slot.append(&close);
            close_btn = Some(close);
        }
        let row = gtk4::ListBoxRow::new();
        row.set_child(Some(&slot));
        // Hover or keyboard focus reveals the close button. Driven here,
        // not in CSS: a hidden widget takes no clicks, opacity would not.
        if let Some(close) = close_btn {
            let show = close.downgrade();
            let hide = close.downgrade();
            let motion = gtk4::EventControllerMotion::new();
            motion.connect_enter(move |_, _, _| {
                if let Some(c) = show.upgrade() {
                    c.set_opacity(1.0);
                    c.set_can_target(true);
                }
            });
            motion.connect_leave(move |_| {
                if let Some(c) = hide.upgrade() {
                    c.set_opacity(0.0);
                    c.set_can_target(false);
                }
            });
            row.add_controller(motion);
            let focus_show = close.downgrade();
            let focus_hide = close.downgrade();
            let focus = gtk4::EventControllerFocus::new();
            focus.connect_enter(move |_| {
                if let Some(c) = focus_show.upgrade() {
                    c.set_opacity(1.0);
                    c.set_can_target(true);
                }
            });
            focus.connect_leave(move |_| {
                if let Some(c) = focus_hide.upgrade() {
                    c.set_opacity(0.0);
                    c.set_can_target(false);
                }
            });
            row.add_controller(focus);
        }
        row.add_css_class("sidebar-row");
        if item.selected {
            row.add_css_class("sidebar-row-active");
        }
        // Honor reduced motion: the loading shimmer stays off when the
        // toolkit animations are disabled.
        let animations = gtk4::Settings::default()
            .map(|s| s.property::<bool>("gtk-enable-animations"))
            .unwrap_or(true);
        if item.loading && animations {
            row.add_css_class("sidebar-loading");
        }
        let mut tip = if item.title.is_empty() { "New Tab".to_owned() } else { item.title.clone() };
        if item.sleeping || item.parked {
            tip.push_str(" — click to restore");
        }
        row.set_tooltip_text(Some(&tip));
        // Middle-click closes without selecting first.
        let page = item.page.clone();
        let tv = tab_view.clone();
        let middle = gtk4::GestureClick::new();
        middle.set_button(2);
        middle.connect_pressed(move |_, _, _, _| {
            tv.close_page(&page);
        });
        row.add_controller(middle);
        row
    }

    /// Show or hide the tab sidebar with a toolkit slide animation, and
    /// persist the choice.
    fn toggle_sidebar(self: &Rc<Self>) {
        let visible = !*self.sidebar_visible.borrow();
        self.set_sidebar_visible(visible);
    }

    fn set_sidebar_visible(self: &Rc<Self>, visible: bool) {
        *self.sidebar_visible.borrow_mut() = visible;
        self.prefs.borrow_mut().sidebar_visible = visible;
        self.save_prefs();
        self.sync_header_sidebar_btn();
        // Mirrored both ways with an equality guard at each end, so the
        // switch and the F9 path can never recurse into each other.
        if let Some(row) = self.settings_sidebar_row.borrow().as_ref() {
            row.set_active(visible);
        }
        if let Some(row) = self.settings_rail_row.borrow().as_ref() {
            row.set_sensitive(visible);
        }
        if visible {
            self.refresh_sidebar();
        }
        self.sidebar_reveal.set_reveal_child(visible);
    }

    /// Header sidebar button mirrors visibility, so its icon can never lie
    /// about the persisted state (including across restarts).
    fn sync_header_sidebar_btn(&self) {
        let visible = *self.sidebar_visible.borrow();
        self.header_sidebar_btn.set_icon_name("sidebar-show-symbolic");
        self.header_sidebar_btn.set_tooltip_text(Some(if visible {
            "Hide tab sidebar (F9)"
        } else {
            "Show tab sidebar (F9)"
        }));
    }

    /// Collapse the sidebar to a thin icon rail, or expand it back, and
    /// persist the choice.
    fn toggle_rail(&self) {
        let rail = !*self.sidebar_rail.borrow();
        self.set_sidebar_collapsed(rail);
    }

    fn set_sidebar_collapsed(&self, rail: bool) {
        *self.sidebar_rail.borrow_mut() = rail;
        self.prefs.borrow_mut().sidebar_collapsed = rail;
        self.save_prefs();
        self.sidebar_box.set_size_request(if rail { 56 } else { 260 }, -1);
        if let Some(row) = self.settings_rail_row.borrow().as_ref() {
            row.set_active(rail);
        }
        self.refresh_sidebar();
    }

    /// One transient toast at a time: rapid repeats (zoom keys, repeated
    /// downloads) replace the previous toast instead of queueing a trail.
    /// Identical repeats are dropped outright, since the message is already
    /// on screen.
    fn show_transient(&self, msg: &str) {
        if let Some(old) = self.transient_toast.borrow_mut().take() {
            if old.title().is_some_and(|t| t.as_str() == msg) {
                self.toasts.add_toast(old);
                return;
            }
            old.dismiss();
        }
        let toast = adw::Toast::new(msg);
        self.transient_toast.borrow_mut().replace(toast.clone());
        self.toasts.add_toast(toast);
    }

    fn save_prefs(&self) {
        if let Err(e) = self.prefs_store.save(&self.prefs.borrow()) {
            eprintln!("br0x: prefs save failed: {e}");
        }
    }

    /// Switch appearance, persist it, and apply it to the chrome plus every
    /// internal page immediately. Always re-applies: if a previous switch
    /// was swallowed by the toolkit, clicking the same mode retries it.
    fn set_appearance(self: &Rc<Self>, appearance: Appearance) {
        self.prefs.borrow_mut().appearance = appearance;
        self.save_prefs();
        let applied = theme::apply(appearance);
        let verified = applied == theme::wanted(appearance);
        let engine = self.prefs.borrow().engine;
        self.refresh_start_page(engine);
        self.reload_start_pages();
        self.reload_history_pages();
        let msg = if verified {
            format!("Appearance: {}", appearance.name())
        } else {
            "Appearance saved, but the system theme overrode it — click again to retry".to_owned()
        };
        self.toasts.add_toast(adw::Toast::new(&msg));
    }

    /// Switch the sleep timeout; the 5 s policy tick picks it up from prefs.
    fn set_sleep_timeout(&self, timeout: SleepTimeout) {
        if self.prefs.borrow().sleep_timeout == timeout {
            return;
        }
        self.prefs.borrow_mut().sleep_timeout = timeout;
        self.save_prefs();
        self.toasts.add_toast(adw::Toast::new(&format!("Tabs sleep after {}", timeout.name())));
    }

    /// Switch the blocker default and re-apply it to every open tab whose
    /// domain has no per-site override.
    fn set_blocker_default(self: &Rc<Self>, enabled: bool) {
        if self.prefs.borrow().blocker_enabled == enabled {
            return;
        }
        self.prefs.borrow_mut().blocker_enabled = enabled;
        self.save_prefs();
        for entry in self.tabs.borrow().entries.iter() {
            self.apply_shield_to_view(&entry.view);
        }
        let state = if enabled { "on" } else { "off" };
        self.toasts.add_toast(adw::Toast::new(&format!("Blocker default {state}")));
    }

    /// Clamped reorder target for keyboard tab moves. Pure for testability.
    fn move_target(pos: usize, delta: i32, count: usize) -> usize {
        if count == 0 {
            return 0;
        }
        (pos as i32 + delta).clamp(0, count as i32 - 1) as usize
    }

    /// Move the selected tab by `delta` positions (Ctrl+Shift+PageUp/Down).
    fn move_selected_tab(&self, delta: i32) {
        let Some(page) = self.tab_view.selected_page() else {
            return;
        };
        let count = self.tab_view.n_pages() as usize;
        let pos = self.tab_view.page_position(&page).max(0) as usize;
        let target = Self::move_target(pos, delta, count);
        if target != pos {
            self.tab_view.reorder_page(&page, target as i32);
            self.refresh_sidebar();
        }
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
        self.engine_btn.set_tooltip_text(Some(&format!(
            "Search engine: {} (address bar + start page)",
            engine.name()
        )));
        self.toasts.add_toast(adw::Toast::new(&format!("Search engine: {}", engine.name())));
    }

    fn connect_view(self: &Rc<Self>, view: &webkit6::WebView, page: &adw::TabPage) {
        let page_clone = page.clone();
        let window_weak = self.window.downgrade();
        let shell_weak = Rc::downgrade(self);
        view.connect_title_notify(move |v| {
            if let Some(t) = v.title() {
                page_clone.set_title(&t);
                // Background tabs that finish loading a title want attention;
                // selection clears it, and the sidebar shows the dot.
                if !page_clone.is_selected() && !t.is_empty() {
                    page_clone.set_needs_attention(true);
                }
                if page_clone.is_selected()
                    && let Some(w) = window_weak.upgrade()
                {
                    w.set_title(Some(&format!("{} — br0x", strip_state_badges(t.as_ref()))));
                }
            }
            if let Some(shell) = shell_weak.upgrade() {
                shell.refresh_sidebar();
            }
        });

        let entry_clone = self.entry.clone();
        let page_clone = page.clone();
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
            }
        });

        // Real favicons in the tab strip. WebKit hands us a texture; an
        // in-memory PNG wrapped in a BytesIcon is what AdwTabPage accepts.
        let page_clone = page.clone();
        let shell_weak = Rc::downgrade(self);
        view.connect_favicon_notify(move |v| {
            let Some(texture) = v.favicon() else {
                return;
            };
            if texture.width() == 0 || texture.height() == 0 {
                return;
            }
            let icon = gio::BytesIcon::new(&texture.save_to_png_bytes());
            page_clone.set_icon(Some(&icon));
            if let Some(shell) = shell_weak.upgrade() {
                shell.refresh_sidebar();
            }
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
                shell_started.refresh_key_button();
                shell_started.refresh_menu_labels();
                shell_started.refresh_star_button();
                shell_started.refresh_sidebar();
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
                shell_started.refresh_sidebar();
                shell_started.refresh_menu_labels();
                shell_started.refresh_star_button();
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
                        &pages::error_html(
                            "Could not open this page",
                            "Check the address and your connection, then try again.",
                            uri,
                            theme::current(),
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
                    &pages::error_html(
                        "This page crashed",
                        "The page used too much memory or hit a bug. Your other tabs are safe.",
                        &uri,
                        theme::current(),
                    ),
                    &uri,
                    None,
                );
            }
        });

        // Suggestion dismissal lives window-wide in build_ui: one capture
        // observer on the toast overlay covers header, sidebar, tab bar,
        // and page, so per-view handlers would only double up.

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
        // Idle time runs while a tab is backgrounded, so stamp the tab we
        // are leaving: otherwise a tab left on screen for 20 minutes parks
        // on the very next tick after you switch away from it.
        let current = self.tab_view.selected_page();
        if self.prev_selected.borrow().as_ref() != current.as_ref() {
            if let Some(prev) = self.prev_selected.borrow().clone()
                && let Some(entry) = self.tabs.borrow_mut().entry_mut(&prev)
            {
                entry.meta.last_active = Instant::now();
            }
            *self.prev_selected.borrow_mut() = current.clone();
        }
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
        if let Some(page) = self.tab_view.selected_page() {
            page.set_needs_attention(false);
        }
        self.refresh_key_button();
        self.refresh_menu_labels();
        self.refresh_star_button();
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
        let sleep_secs = self.prefs.borrow().sleep_timeout.secs();
        let to_park = {
            let mut tabs = self.tabs.borrow_mut();
            let snaps: Vec<TabSnapshot> =
                tabs.entries.iter().map(|e| e.meta.snapshot(e.view.is_playing_audio())).collect();
            let mut to_park = Vec::new();
            for (id, action) in sweep_with_sleep(&snaps, &sys, sleep_secs) {
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
                        if let Some(view) = release_entry(entry, false) {
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

    /// Debounced session write: at most one write per few seconds no matter
    /// how many tab events fire. The quit path uses [`Self::flush_session`].
    fn save_session(&self) {
        // The 30 s timer plus tab add/close events all land here: skip the
        // write (and its two fsyncs) when nothing changed or the last write
        // is only seconds old.
        if self.last_save.borrow().is_some_and(|at| at.elapsed().as_secs() < 5) {
            return;
        }
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
                    title: strip_state_badges(&e.page.title()),
                    order: self.tab_view.page_position(&e.page).max(0) as usize,
                    pinned: e.page.is_pinned(),
                    scroll_y: 0,
                }
            })
            // Start pages and internal pages are not worth restoring.
            .filter(|t| !t.url.is_empty() && !is_blank_uri(&t.url) && !t.url.starts_with("br0x://"))
            .collect();
        if stored == *self.last_session.borrow() {
            return;
        }
        *self.last_session.borrow_mut() = stored.clone();
        *self.last_save.borrow_mut() = Some(Instant::now());
        let store = SessionStore::new(session_path());
        if let Err(e) = store.save(&Session { tabs: stored }) {
            eprintln!("br0x: session save failed: {e}");
        }
    }

    /// Unconditional write for the quit path. Debouncing must never lose
    /// the final tab set.
    fn flush_session(&self) {
        *self.last_save.borrow_mut() = None;
        *self.last_session.borrow_mut() = Vec::new();
        self.save_session();
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

    /// Bookmarks dialog: the header menu replaced the bookmarks button, so
    /// saved pages open from here (and from the start-page hint). Built
    /// fresh on every open, so it never shows stale entries.
    #[allow(deprecated)]
    fn show_bookmarks(self: &Rc<Self>) {
        let dialog = SettingsWindow::new();
        dialog.set_transient_for(Some(&self.window));
        dialog.set_title(Some("Bookmarks"));
        dialog.set_search_enabled(true);
        let page = adw::PreferencesPage::new();
        page.set_title("Bookmarks");
        page.set_icon_name(Some("starred-symbolic"));
        let group = adw::PreferencesGroup::new();
        group.set_title("Saved pages");
        let bookmarks = self.bookmarks.borrow().clone();
        if bookmarks.is_empty() {
            let row = adw::ActionRow::new();
            row.set_title("No bookmarks yet");
            row.set_subtitle("Press Ctrl+D on any page to save it here.");
            group.add(&row);
        }
        for mark in &bookmarks {
            let row = adw::ActionRow::new();
            let name = if mark.title.is_empty() {
                pages::display_domain(&mark.url).to_owned()
            } else {
                mark.title.clone()
            };
            row.set_title(&name);
            row.set_subtitle(&mark.url);
            let open = gtk4::Button::with_label("Open");
            open.set_tooltip_text(Some("Open this bookmark in the current tab"));
            open.add_css_class("flat");
            let shell = self.clone();
            let url = mark.url.clone();
            let dialog_weak = dialog.downgrade();
            open.connect_clicked(move |_| {
                if let Some(view) = selected_view(&shell.tab_view) {
                    view.load_uri(&url);
                    view.grab_focus();
                }
                if let Some(dialog) = dialog_weak.upgrade() {
                    dialog.close();
                }
            });
            row.add_suffix(&open);
            let remove = gtk4::Button::from_icon_name("user-trash-symbolic");
            remove.set_tooltip_text(Some("Remove this bookmark"));
            remove.add_css_class("flat");
            let shell = self.clone();
            let url = mark.url.clone();
            let row_weak = row.downgrade();
            let group_weak = group.downgrade();
            remove.connect_clicked(move |_| {
                BookmarkStore::remove(&mut shell.bookmarks.borrow_mut(), &url);
                if let Err(e) = shell.bookmarks_store.save(&shell.bookmarks.borrow()) {
                    eprintln!("br0x: bookmarks save failed: {e}");
                }
                if let (Some(row), Some(group)) = (row_weak.upgrade(), group_weak.upgrade()) {
                    group.remove(&row);
                }
                shell.toasts.add_toast(adw::Toast::new("Bookmark removed"));
            });
            row.add_suffix(&remove);
            group.add(&row);
        }
        page.add(&group);
        dialog.add(&page);
        dialog.present();
    }

    /// Settings window (Ctrl+, and the header menu). One window at a time,
    /// rebuilt fresh on every open so rows never show stale prefs.
    #[allow(deprecated)]
    fn open_settings(self: &Rc<Self>) {
        // Take in its own statement: an if-let scrutinee borrow would live
        // through the body, and the synchronous close-request below
        // borrows the same slot. Taken first, the handler only clears an
        // already-empty slot in either timing.
        let old = self.settings_win.borrow_mut().take();
        if let Some(win) = old {
            win.close();
        }
        let win = SettingsWindow::new();
        win.set_transient_for(Some(&self.window));
        win.set_title(Some("Settings"));
        win.set_search_enabled(true);
        win.set_default_size(640, 520);
        self.settings_appearance_page(&win);
        self.settings_tabs_page(&win);
        self.settings_privacy_page(&win);
        self.settings_search_page(&win);
        self.settings_passwords_page(&win);
        self.settings_about_page(&win);
        {
            let shell = self.clone();
            win.connect_close_request(move |_| {
                *shell.settings_win.borrow_mut() = None;
                glib::Propagation::Proceed
            });
        }
        *self.settings_win.borrow_mut() = Some(win.clone());
        win.present();
    }

    /// Segmented System / Light / Dark row. Applies instantly via the style
    /// manager: chrome and internal pages switch with zero flicker.
    #[allow(deprecated)]
    fn settings_appearance_page(self: &Rc<Self>, win: &SettingsWindow) {
        let page = adw::PreferencesPage::new();
        page.set_title("Appearance");
        page.set_icon_name(Some("display-brightness-symbolic"));
        let group = adw::PreferencesGroup::new();
        group.set_title("Theme");
        group.set_description(Some(
            "Follows the system by default. Internal pages match the chrome.",
        ));
        let row = adw::ActionRow::new();
        row.set_title("Appearance");
        let segmented = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        segmented.add_css_class("linked");
        let current = self.prefs.borrow().appearance;
        let mut first: Option<gtk4::ToggleButton> = None;
        for mode in Appearance::ALL {
            let btn = gtk4::ToggleButton::with_label(mode.name());
            btn.set_tooltip_text(Some(&format!("Use the {} theme", mode.name().to_lowercase())));
            if let Some(first) = &first {
                btn.set_group(Some(first));
            } else {
                first = Some(btn.clone());
            }
            btn.set_active(mode == current);
            let shell = self.clone();
            btn.connect_toggled(move |toggled| {
                if toggled.is_active() {
                    shell.set_appearance(mode);
                }
            });
            segmented.append(&btn);
        }
        row.add_suffix(&segmented);
        group.add(&row);
        page.add(&group);
        win.add(&page);
    }

    #[allow(deprecated)]
    fn settings_tabs_page(self: &Rc<Self>, win: &SettingsWindow) {
        let page = adw::PreferencesPage::new();
        page.set_title("Tabs");
        page.set_icon_name(Some("tab-new-symbolic"));
        let group = adw::PreferencesGroup::new();
        group.set_title("Background tabs");
        group.set_description(Some(
            "Idle tabs release their web process. Sleeping is time-based; parking is pressure-driven.",
        ));
        let sleep = adw::ComboRow::new();
        sleep.set_title("Sleep inactive tabs after");
        sleep
            .set_subtitle("Never disables the timer; parking under memory pressure still applies.");
        sleep.set_model(Some(&gtk4::StringList::new(&SleepTimeout::ALL.map(|s| s.name()))));
        sleep.set_selected(self.prefs.borrow().sleep_timeout.index());
        {
            let shell = self.clone();
            sleep.connect_selected_notify(move |row| {
                if let Some(timeout) = SleepTimeout::ALL.get(row.selected() as usize) {
                    shell.set_sleep_timeout(*timeout);
                }
            });
        }
        group.add(&sleep);
        let restore = adw::SwitchRow::new();
        restore.set_title("Restore tabs on startup");
        restore.set_subtitle("Only the selected tab loads; the rest wait until you open them.");
        restore.set_active(self.prefs.borrow().restore_session);
        {
            let shell = self.clone();
            restore.connect_active_notify(move |row| {
                shell.set_restore_session(row.is_active());
            });
        }
        group.add(&restore);
        let sidebar = adw::SwitchRow::new();
        sidebar.set_title("Show vertical tabs");
        sidebar.set_subtitle("Icon rail with pinned tabs plus full rows (F9).");
        sidebar.set_active(*self.sidebar_visible.borrow());
        {
            let shell = self.clone();
            sidebar.connect_active_notify(move |row| {
                if row.is_active() != *shell.sidebar_visible.borrow() {
                    shell.set_sidebar_visible(row.is_active());
                }
            });
        }
        group.add(&sidebar);
        *self.settings_sidebar_row.borrow_mut() = Some(sidebar);
        let rail = adw::SwitchRow::new();
        rail.set_title("Collapse sidebar to icons");
        rail.set_subtitle("Slim rail instead of full titles.");
        rail.set_active(*self.sidebar_rail.borrow());
        rail.set_sensitive(*self.sidebar_visible.borrow());
        {
            let shell = self.clone();
            rail.connect_active_notify(move |row| {
                if row.is_active() != *shell.sidebar_rail.borrow() {
                    shell.set_sidebar_collapsed(row.is_active());
                }
            });
        }
        group.add(&rail);
        *self.settings_rail_row.borrow_mut() = Some(rail);
        page.add(&group);
        win.add(&page);
    }

    #[allow(deprecated)]
    fn settings_privacy_page(self: &Rc<Self>, win: &SettingsWindow) {
        let page = adw::PreferencesPage::new();
        page.set_title("Privacy");
        page.set_icon_name(Some("security-high-symbolic"));
        let blocker = adw::PreferencesGroup::new();
        blocker.set_title("Tracker blocker");
        let def = adw::SwitchRow::new();
        def.set_title("Block trackers by default");
        def.set_subtitle("Sites below keep their own setting either way.");
        def.set_active(self.prefs.borrow().blocker_enabled);
        {
            let shell = self.clone();
            def.connect_active_notify(move |row| {
                shell.set_blocker_default(row.is_active());
            });
        }
        blocker.add(&def);
        for (domain, enabled) in self.shield_exceptions() {
            let row = adw::ActionRow::new();
            row.set_title(&domain);
            row.set_subtitle(if enabled {
                "Blocker on for this site"
            } else {
                "Blocker off for this site"
            });
            let remove = gtk4::Button::with_label("Remove");
            remove.set_tooltip_text(Some("Forget this site's setting, use the default"));
            remove.add_css_class("flat");
            let shell = self.clone();
            let row_weak = row.downgrade();
            let blocker_weak = blocker.downgrade();
            remove.connect_clicked(move |_| {
                let _ = shell.shield.borrow_mut().reset(&domain);
                if let (Some(row), Some(group)) = (row_weak.upgrade(), blocker_weak.upgrade()) {
                    group.remove(&row);
                }
                shell
                    .toasts
                    .add_toast(adw::Toast::new(&format!("Blocker default restored for {domain}")));
            });
            row.add_suffix(&remove);
            blocker.add(&row);
        }
        page.add(&blocker);
        let curtain = adw::PreferencesGroup::new();
        curtain.set_title("Hidden elements");
        curtain.set_description(Some("Pick elements with Ctrl+Shift+H; clear them per site here."));
        let domains = self.curtain.borrow().all_domains();
        if domains.is_empty() {
            let row = adw::ActionRow::new();
            row.set_title("Nothing hidden");
            row.set_subtitle("Hidden page elements will be listed here.");
            curtain.add(&row);
        }
        for domain in domains {
            let selectors = self.curtain.borrow().selectors_for(&domain);
            let row = adw::ActionRow::new();
            row.set_title(&domain);
            row.set_subtitle(&format!(
                "{} hidden element{}",
                selectors.len(),
                if selectors.len() == 1 { "" } else { "s" }
            ));
            let clear = gtk4::Button::with_label("Clear");
            clear.set_tooltip_text(Some("Unhide every element on this site"));
            clear.add_css_class("flat");
            // Rows created below remove themselves; the Clear button takes
            // the header plus every selector row with it.
            let owned: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));
            let shell = self.clone();
            let owned_clear = owned.clone();
            let row_weak = row.downgrade();
            let curtain_weak = curtain.downgrade();
            let domain_clear = domain.clone();
            clear.connect_clicked(move |_| {
                let _ = shell.curtain.borrow_mut().clear_domain(&domain_clear);
                if let Some(group) = curtain_weak.upgrade() {
                    if let Some(header) = row_weak.upgrade() {
                        group.remove(&header);
                    }
                    for owned_row in owned_clear.borrow().iter() {
                        group.remove(owned_row);
                    }
                }
                shell.reload_domain_tabs(&domain_clear);
                shell.toasts.add_toast(adw::Toast::new(&format!("Unhidden on {domain_clear}")));
            });
            row.add_suffix(&clear);
            curtain.add(&row);
            // Remaining count shared with the per-selector remove buttons:
            // each removal updates the header subtitle, and the last one
            // takes the header with it so no stale "1 hidden element" lingers.
            let remaining: Rc<std::cell::Cell<usize>> =
                Rc::new(std::cell::Cell::new(selectors.len()));
            let header_weak = row.downgrade();
            for selector in selectors {
                let item = adw::ActionRow::new();
                item.set_title(&selector);
                item.set_subtitle("Hidden element — remove to show it again");
                let remove = gtk4::Button::from_icon_name("window-close-symbolic");
                remove.set_tooltip_text(Some("Unhide this element"));
                remove.add_css_class("flat");
                let shell = self.clone();
                let item_weak = item.downgrade();
                let curtain_weak = curtain.downgrade();
                let header_item = header_weak.clone();
                let remaining_item = remaining.clone();
                let domain_item = domain.clone();
                let selector_item = selector.clone();
                remove.connect_clicked(move |_| {
                    let _ = shell.curtain.borrow_mut().unhide(&domain_item, &selector_item);
                    if let (Some(item), Some(group)) = (item_weak.upgrade(), curtain_weak.upgrade())
                    {
                        group.remove(&item);
                        let left = remaining_item.get().saturating_sub(1);
                        remaining_item.set(left);
                        if let Some(header) = header_item.upgrade() {
                            if left == 0 {
                                group.remove(&header);
                            } else {
                                header.set_subtitle(&format!(
                                    "{left} hidden element{}",
                                    if left == 1 { "" } else { "s" }
                                ));
                            }
                        }
                    }
                    shell.reload_domain_tabs(&domain_item);
                    shell.toasts.add_toast(adw::Toast::new("Element unhidden — reloading"));
                });
                item.add_suffix(&remove);
                curtain.add(&item);
                owned.borrow_mut().push(item);
            }
        }
        page.add(&curtain);
        let history_group = adw::PreferencesGroup::new();
        history_group.set_title("History");
        let clear_row = adw::ActionRow::new();
        clear_row.set_title("Clear browsing history");
        let count = self.history.borrow().as_ref().and_then(|h| h.count().ok()).unwrap_or(0);
        clear_row.set_subtitle(&format!("{count} entries stored on this device."));
        let clear = gtk4::Button::with_label("Clear…");
        clear.set_tooltip_text(Some("Delete all locally stored history"));
        clear.add_css_class("destructive-action");
        let shell = self.clone();
        let row_weak = clear_row.downgrade();
        clear.connect_clicked(move |_| {
            // Same guard as the history page: one mis-click must never
            // wipe the local store.
            let dialog = adw::AlertDialog::builder()
                .heading("Clear browsing history?")
                .body("Every locally stored visit is deleted. This cannot be undone.")
                .build();
            dialog.add_responses(&[("cancel", "_Cancel"), ("clear", "_Clear")]);
            dialog.set_response_appearance("clear", adw::ResponseAppearance::Destructive);
            dialog.set_default_response(Some("cancel"));
            dialog.set_close_response("cancel");
            let confirmed = shell.clone();
            let row_confirmed = row_weak.clone();
            dialog.connect_response(Some("clear"), move |_, _| {
                if let Some(history) = confirmed.history.borrow().as_ref() {
                    match history.clear() {
                        Ok(()) => {
                            if let Some(row) = row_confirmed.upgrade() {
                                row.set_subtitle("0 entries stored on this device.");
                            }
                            confirmed.toasts.add_toast(adw::Toast::new("History cleared"));
                        }
                        Err(e) => {
                            eprintln!("br0x: history clear failed: {e}");
                            confirmed.toasts.add_toast(adw::Toast::new("Could not clear history"));
                        }
                    }
                }
            });
            dialog.present(Some(&shell.window));
        });
        clear_row.add_suffix(&clear);
        history_group.add(&clear_row);
        page.add(&history_group);
        win.add(&page);
    }

    #[allow(deprecated)]
    fn settings_search_page(self: &Rc<Self>, win: &SettingsWindow) {
        let page = adw::PreferencesPage::new();
        page.set_title("Search");
        page.set_icon_name(Some("system-search-symbolic"));
        let group = adw::PreferencesGroup::new();
        group.set_title("Search engine");
        group.set_description(Some("Used by the address bar and the start page."));
        let row = adw::ComboRow::new();
        row.set_title("Engine");
        row.set_model(Some(&gtk4::StringList::new(&SearchEngine::ALL.map(SearchEngine::name))));
        row.set_selected(
            SearchEngine::ALL.iter().position(|e| *e == self.prefs.borrow().engine).unwrap_or(0)
                as u32,
        );
        {
            let shell = self.clone();
            row.connect_selected_notify(move |row| {
                if let Some(engine) = SearchEngine::ALL.get(row.selected() as usize) {
                    shell.set_engine(*engine);
                }
            });
        }
        group.add(&row);
        page.add(&group);
        win.add(&page);
    }

    /// Saved logins by origin + username with delete. Secrets never render.
    #[allow(deprecated)]
    fn settings_passwords_page(self: &Rc<Self>, win: &SettingsWindow) {
        let page = adw::PreferencesPage::new();
        page.set_title("Passwords");
        page.set_icon_name(Some("dialog-password-symbolic"));
        let group = adw::PreferencesGroup::new();
        group.set_title("Saved logins");
        group.set_description(Some("Usernames only — secrets are never shown."));
        let vault = self.vault.borrow();
        let mut origins = vault.all_origins();
        origins.sort();
        if origins.is_empty() {
            let row = adw::ActionRow::new();
            row.set_title("No saved logins");
            row.set_subtitle("Save one from the key icon on a login page.");
            group.add(&row);
        }
        for origin in origins {
            for entry in vault.credentials_for(&origin) {
                let row = adw::ActionRow::new();
                row.set_title(&entry.username);
                row.set_subtitle(&entry.origin);
                let delete = gtk4::Button::from_icon_name("user-trash-symbolic");
                delete.set_tooltip_text(Some("Delete this saved login"));
                delete.add_css_class("flat");
                let shell = self.clone();
                let row_weak = row.downgrade();
                let group_weak = group.downgrade();
                let (origin, username) = (entry.origin.clone(), entry.username.clone());
                delete.connect_clicked(move |_| {
                    match shell.vault.borrow_mut().remove(&origin, &username) {
                        Ok(true) => {
                            if let (Some(row), Some(group)) =
                                (row_weak.upgrade(), group_weak.upgrade())
                            {
                                group.remove(&row);
                            }
                            shell.toasts.add_toast(adw::Toast::new("Login deleted"));
                        }
                        _ => shell.toasts.add_toast(adw::Toast::new("Could not delete this login")),
                    }
                });
                row.add_suffix(&delete);
                group.add(&row);
            }
        }
        drop(vault);
        page.add(&group);
        win.add(&page);
    }

    #[allow(deprecated)]
    fn settings_about_page(&self, win: &SettingsWindow) {
        let page = adw::PreferencesPage::new();
        page.set_title("About");
        page.set_icon_name(Some("help-about-symbolic"));
        let group = adw::PreferencesGroup::new();
        group.set_title("br0x");
        let app = adw::ActionRow::new();
        app.set_title("br0x");
        app.set_subtitle(&format!("Version {}", env!("CARGO_PKG_VERSION")));
        group.add(&app);
        let memory = adw::ActionRow::new();
        memory.set_title("Memory posture");
        memory.set_subtitle(MEMORY_REPORT);
        group.add(&memory);
        page.add(&group);
        win.add(&page);
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
                    s.refresh_menu_labels();
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
        // Stateful so both engine menus render the active engine checked.
        let initial =
            SearchEngine::ALL.iter().position(|e| *e == self.prefs.borrow().engine).unwrap_or(0)
                as u32;
        let engine_action = gio::SimpleAction::new_stateful(
            "set-engine",
            Some(glib::VariantTy::UINT32),
            &initial.to_variant(),
        );
        {
            let s = self.clone();
            let action = engine_action.clone();
            engine_action.connect_activate(move |_, param| {
                if let Some(idx) = param.and_then(|p| p.get::<u32>())
                    && let Some(engine) = SearchEngine::ALL.get(idx as usize)
                {
                    action.set_state(&idx.to_variant());
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
            &["<Control>l"],
            Box::new(move || {
                s.entry.grab_focus();
                s.entry.select_region(0, -1);
            }),
        );
        let s = self.clone();
        add(
            "palette",
            &["<Control>k"],
            Box::new(move || {
                s.toggle_palette();
            }),
        );
        let s = self.clone();
        add(
            "focus-mode",
            &["<Control><Shift>f"],
            Box::new(move || {
                s.toggle_focus_mode();
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
                    .map(|uri| !s.blocker_active(&domain_of(&uri)))
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
        let s = self.clone();
        add(
            "settings",
            &["<Control>comma"],
            Box::new(move || {
                s.open_settings();
            }),
        );
        let s = self.clone();
        add(
            "show-bookmarks",
            &[],
            Box::new(move || {
                s.show_bookmarks();
            }),
        );
        let s = self.clone();
        add(
            "move-tab-up",
            &["<Control><Shift>Page_Up"],
            Box::new(move || {
                s.move_selected_tab(-1);
            }),
        );
        let s = self.clone();
        add(
            "move-tab-down",
            &["<Control><Shift>Page_Down"],
            Box::new(move || {
                s.move_selected_tab(1);
            }),
        );
    }
}

/// Memory posture, reported in the settings About page and on stdout.
const MEMORY_REPORT: &str = "1 shared WebContext + 1 shared NetworkSession for all tabs and popouts; \
    content filter compiled once and shared; cache model DocumentViewer (no memory cache); \
    WebKit pressure: kill 0.95 / strict 0.7 / conservative 0.5 / 1024 MB limit / 5 s poll; \
    no web-process-model API in the webkit6 bindings, so the shared context is the whole story";

/// Register the shell stylesheet. All chrome color lives in `theme`.
fn install_theme(display: &gtk4::gdk::Display) {
    theme::install(display);
}

fn build_ui(app: &adw::Application) {
    let prefs_store = PrefsStore::new(prefs_path());
    let prefs = Rc::new(RefCell::new(prefs_store.load()));
    // Handed to the shell below, so its startup write can skip this page
    // when the engine, history and scheme still match what was written here.
    let last_newtab: RefCell<Option<(SearchEngine, Vec<Visit>, theme::Scheme)>> =
        RefCell::new(None);
    {
        let loaded = prefs.borrow();
        pages::sync_newtab_page(&last_newtab, loaded.engine, &[], theme::current());
    }

    let session = shared_session();
    let context = shared_context();
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("br0x")
        .default_width(1200)
        .default_height(800)
        .build();
    window.add_css_class("br0x-window");

    if let Some(display) = gtk4::gdk::Display::default() {
        install_theme(&display);
    }
    theme::apply(prefs.borrow().appearance);
    eprintln!("br0x: memory: {MEMORY_REPORT}");

    let tab_view = adw::TabView::new();
    tab_view.set_default_icon(&gio::ThemedIcon::new("web-browser-symbolic"));
    let tab_bar = adw::TabBar::new();
    tab_bar.set_view(Some(&tab_view));
    tab_bar.set_hexpand(true);
    // Tabs hug their content instead of stretching across the window.
    tab_bar.set_expand_tabs(false);
    tab_bar.set_autohide(false);
    tab_bar.add_css_class("br0x-tabbar");

    let back = gtk4::Button::from_icon_name("go-previous-symbolic");
    back.set_tooltip_text(Some("Back (Alt+Left)"));
    back.add_css_class("flat");
    let fwd = gtk4::Button::from_icon_name("go-next-symbolic");
    fwd.set_tooltip_text(Some("Forward (Alt+Right)"));
    fwd.add_css_class("flat");
    let reload = gtk4::Button::from_icon_name("view-refresh-symbolic");
    reload.set_tooltip_text(Some("Reload (Ctrl+R / F5)"));
    reload.add_css_class("flat");

    let entry = gtk4::Entry::new();
    entry.set_placeholder_text(Some("Search or type a URL"));
    entry.set_tooltip_text(Some("Search or type a URL (Ctrl+L)"));
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

    // Key icon shown on pages with password fields.
    let key_btn = gtk4::Button::from_icon_name("dialog-password-symbolic");
    key_btn.set_tooltip_text(Some("Logins for this site"));
    key_btn.add_css_class("flat");
    key_btn.set_visible(false);
    if let Some(icon) = key_btn.child().and_downcast::<gtk4::Image>() {
        icon.set_pixel_size(16);
    }

    // Star button for the current page, kept in sync on every switch.
    let star_btn = gtk4::Button::from_icon_name("non-starred-symbolic");
    star_btn.set_tooltip_text(Some("Bookmark this page (Ctrl+D)"));
    star_btn.add_css_class("flat");
    if let Some(icon) = star_btn.child().and_downcast::<gtk4::Image>() {
        icon.set_pixel_size(16);
    }

    /// One shared engine menu: the address-bar picker and the hamburger
    /// submenu show the same items, so they can never disagree.
    fn engine_menu_model() -> gio::Menu {
        let engine_menu = gio::Menu::new();
        for (idx, engine) in SearchEngine::ALL.iter().enumerate() {
            let item = gio::MenuItem::new(Some(engine.name()), None);
            item.set_action_and_target_value(
                Some("win.set-engine"),
                Some(&(idx as u32).to_variant()),
            );
            engine_menu.append_item(&item);
        }
        engine_menu
    }

    // Saved-username popover anchored to the key icon.
    let key_pop = gtk4::Popover::new();
    key_pop.set_parent(&key_btn);
    let key_list = gtk4::ListBox::new();
    key_list.set_selection_mode(gtk4::SelectionMode::Single);
    key_pop.set_child(Some(&key_list));

    let omnibox_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
    omnibox_box.add_css_class("omnibox-frame");
    omnibox_box.set_hexpand(true);
    // The bar keeps a single address field, the bookmark star, the engine
    // picker badge, and the contextual key icon (hidden unless the page
    // has a login form). Everything else lives in the hamburger menu.
    omnibox_box.append(&entry);
    omnibox_box.append(&star_btn);
    let engine_btn = gtk4::MenuButton::new();
    engine_btn.set_icon_name("system-search-symbolic");
    engine_btn.set_tooltip_text(Some("Search engine (address bar + start page)"));
    engine_btn.add_css_class("engine-picker");
    engine_btn.set_popover(Some(&gtk4::PopoverMenu::from_model(Some(&engine_menu_model()))));
    omnibox_box.append(&engine_btn);
    omnibox_box.append(&key_btn);

    // Hamburger menu. A custom popover, not a gio::Menu: GTK hides images
    // on modelled menu items, so icons require real rows. Every row is a
    // flat button holding an icon, a label, and its shortcut.
    let menu_pop = gtk4::Popover::new();
    menu_pop.add_css_class("menu-popover");
    let menu_box = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    menu_box.set_margin_top(6);
    menu_box.set_margin_bottom(6);
    menu_box.set_margin_start(6);
    menu_box.set_margin_end(6);
    // A menu taller than the window cannot be shown at all, so it scrolls.
    let menu_scroll = gtk4::ScrolledWindow::new();
    menu_scroll.set_child(Some(&menu_box));
    menu_scroll.set_propagate_natural_height(true);
    menu_scroll.set_max_content_height(560);
    menu_scroll.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
    menu_pop.set_child(Some(&menu_scroll));

    let row_for = |icon: &str, label: &str, accel: &str, action: &str| {
        let row = gtk4::Button::new();
        row.add_css_class("menu-row");
        let line = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
        let image = gtk4::Image::from_icon_name(icon);
        image.set_pixel_size(16);
        let text = gtk4::Label::new(Some(label));
        text.set_xalign(0.0);
        text.set_hexpand(true);
        line.append(&image);
        line.append(&text);
        if !accel.is_empty() {
            let hint = gtk4::Label::new(Some(accel));
            hint.add_css_class("menu-accel");
            hint.set_xalign(1.0);
            line.append(&hint);
        }
        row.set_child(Some(&line));
        let win = window.clone();
        let pop = menu_pop.clone();
        let action = action.to_string();
        row.connect_clicked(move |_| {
            pop.popdown();
            gio::prelude::ActionGroupExt::activate_action(&win, &action, None);
        });
        menu_box.append(&row);
        (row, text)
    };

    let section_break = || {
        let sep = gtk4::Separator::new(gtk4::Orientation::Horizontal);
        sep.add_css_class("menu-sep");
        menu_box.append(&sep);
    };

    row_for("tab-new-symbolic", "New Tab", "Ctrl+T", "new-tab");
    row_for("edit-undo-symbolic", "Reopen Closed Tab", "Ctrl+Shift+T", "reopen-tab");
    row_for("window-close-symbolic", "Close Tab", "Ctrl+W", "close-tab");
    let (_, pin_label) = row_for("view-pin-symbolic", "Pin Tab", "Ctrl+Shift+P", "toggle-pin");
    section_break();
    row_for("edit-find-symbolic", "Find in Page", "Ctrl+F", "find");
    row_for("view-app-grid-symbolic", "Command Palette", "Ctrl+K", "palette");
    row_for("zoom-in-symbolic", "Zoom In", "Ctrl++", "zoom-in");
    row_for("zoom-out-symbolic", "Zoom Out", "Ctrl+-", "zoom-out");
    row_for("zoom-original-symbolic", "Reset Zoom", "Ctrl+0", "zoom-reset");
    section_break();
    row_for("star-new-symbolic", "Bookmark This Page", "Ctrl+D", "bookmark-page");
    row_for("user-bookmarks-symbolic", "Bookmarks…", "", "show-bookmarks");
    let (_, shield_label) = row_for(
        "preferences-system-privacy-symbolic",
        "Blocker for This Site",
        "Ctrl+Shift+S",
        "toggle-shield",
    );
    row_for("accessories-dictionary-symbolic", "Reader Mode", "Ctrl+Shift+R", "reader");
    row_for("video-display-symbolic", "Pop Out Video", "Ctrl+Shift+O", "popout");
    row_for("view-conceal-symbolic", "Hide Element on Site", "Ctrl+Shift+H", "curtain-pick");
    row_for("view-reveal-symbolic", "Unhide All on Site", "", "curtain-clear");
    section_break();
    row_for("document-open-recent-symbolic", "History", "Ctrl+H", "history");
    row_for("document-revert-symbolic", "Restore Previous Session", "", "restore-prev");
    row_for("view-restore-symbolic", "Restore Tabs on Startup", "", "restore-session");
    section_break();
    section_break();
    row_for("view-list-symbolic", "Sidebar", "F9", "toggle-sidebar");
    row_for("view-fullscreen-symbolic", "Focus Mode", "Ctrl+Shift+F", "focus-mode");
    row_for("preferences-system-symbolic", "Settings…", "Ctrl+,", "settings");
    row_for("application-exit-symbolic", "Quit", "Ctrl+Q", "quit");

    let menu_btn = gtk4::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .popover(&menu_pop)
        .tooltip_text("Menu")
        .build();

    let header = adw::HeaderBar::new();
    let sidebar_btn = gtk4::Button::from_icon_name("sidebar-show-symbolic");
    sidebar_btn.set_tooltip_text(Some("Tabs sidebar (F9)"));
    sidebar_btn.add_css_class("flat");
    header.pack_start(&sidebar_btn);
    header.pack_start(&back);
    header.pack_start(&fwd);
    header.pack_start(&reload);
    header.set_title_widget(Some(&omnibox_box));
    header.pack_end(&menu_btn);

    let progress = gtk4::ProgressBar::new();
    progress.add_css_class("hairline-progress");
    progress.set_visible(false);

    // Thin reading-progress line under the active tab, driven by scroll.
    // Dimmer than the accent load bar by design, so the two never confuse.
    let read_progress = gtk4::ProgressBar::new();
    read_progress.add_css_class("hairline-progress");
    read_progress.add_css_class("hairline-dim");
    read_progress.set_visible(false);

    // Vertical tab sidebar in a slide revealer: the show/hide animation is
    // the toolkit's own, no manual timers. The top tab bar keeps working
    // either way. Pinned tabs get their own icon row above the list.
    let sidebar_label = gtk4::Label::new(Some("1 tab"));
    sidebar_label.set_xalign(0.0);
    sidebar_label.add_css_class("br0x-sidebar-title");
    let sidebar_collapse = gtk4::Button::from_icon_name("pan-start-symbolic");
    sidebar_collapse.set_tooltip_text(Some("Collapse sidebar to icons"));
    sidebar_collapse.add_css_class("flat");
    sidebar_collapse.set_size_request(28, 28);
    // CenterBox, not Box: a plain box ORs its children's expand flags, which
    // leaked expansion into the revealer and stretched the sidebar to half
    // the window. CenterBox never computes expand, so the child's 220 px
    // request above is a real width, not a floor.
    let sidebar_head = gtk4::CenterBox::new();
    sidebar_head.add_css_class("br0x-sidebar-head");
    sidebar_head.set_margin_top(6);
    sidebar_head.set_margin_bottom(6);
    sidebar_head.set_margin_start(12);
    sidebar_head.set_margin_end(8);
    sidebar_head.set_start_widget(Some(&sidebar_label));
    sidebar_head.set_end_widget(Some(&sidebar_collapse));
    // Icon-only pinned tabs wrap instead of widening the strip; every icon
    // is capped so content can never drive the sidebar width.
    let sidebar_pins = gtk4::FlowBox::new();
    sidebar_pins.set_selection_mode(gtk4::SelectionMode::None);
    sidebar_pins.set_homogeneous(true);
    sidebar_pins.set_margin_start(8);
    sidebar_pins.set_margin_end(8);
    sidebar_pins.set_visible(false);
    let sidebar_list = gtk4::ListBox::new();
    // Single highlight only: the row class marks selection, so the list
    // itself selects nothing.
    sidebar_list.set_selection_mode(gtk4::SelectionMode::None);
    let sidebar_scroll = gtk4::ScrolledWindow::new();
    sidebar_scroll.set_child(Some(&sidebar_list));
    sidebar_scroll.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
    sidebar_scroll.set_vexpand(true);
    let sidebar_box = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    sidebar_box.add_css_class("br0x-sidebar");
    // Width lives on the revealer's child: a request on the revealer itself
    // is a hard minimum even while hidden, permanently stealing 220 px.
    sidebar_box.set_size_request(260, -1);
    sidebar_box.append(&sidebar_head);
    sidebar_box.append(&sidebar_pins);
    sidebar_box.append(&sidebar_scroll);
    let sidebar_reveal = gtk4::Revealer::new();
    sidebar_reveal.set_transition_type(gtk4::RevealerTransitionType::SlideRight);
    sidebar_reveal.set_transition_duration(200);
    sidebar_reveal.set_child(Some(&sidebar_box));
    sidebar_reveal.set_reveal_child(false);
    tab_view.set_hexpand(true);
    tab_view.set_vexpand(true);

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

    // Centered command palette: entry plus ranked rows on a floating card.
    // An overlay centers it over the page; hidden costs nothing.
    let palette_entry = gtk4::SearchEntry::new();
    palette_entry.set_placeholder_text(Some("Type a command, tab, or address"));
    palette_entry.set_hexpand(true);
    let palette_list = gtk4::ListBox::new();
    palette_list.set_selection_mode(gtk4::SelectionMode::Single);
    let palette_scroll = gtk4::ScrolledWindow::new();
    palette_scroll.set_child(Some(&palette_list));
    palette_scroll.set_propagate_natural_height(true);
    palette_scroll.set_max_content_height(430);
    palette_scroll.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
    let palette_head = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    palette_head.set_margin_top(8);
    palette_head.set_margin_start(8);
    palette_head.set_margin_end(8);
    palette_head.append(&palette_entry);
    let palette_card = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    palette_card.add_css_class("palette-card");
    palette_card.append(&palette_head);
    palette_card.append(&palette_scroll);
    palette_card.set_halign(gtk4::Align::Center);
    palette_card.set_valign(gtk4::Align::Start);
    palette_card.set_margin_top(64);
    palette_card.set_size_request(560, -1);
    palette_card.set_visible(false);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.add_top_bar(&tab_bar);
    let hairlines = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    hairlines.append(&read_progress);
    hairlines.append(&progress);
    toolbar.add_top_bar(&hairlines);
    toolbar.add_top_bar(&find_bar);
    let content_overlay = gtk4::Overlay::new();
    content_overlay.set_child(Some(&tab_view));
    content_overlay.add_overlay(&palette_card);
    toolbar.set_content(Some(&content_overlay));

    // Sidebar beside the page column, not inside it: the tab strip would
    // otherwise run across the top of the sidebar and cut its height.
    let root = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    root.append(&sidebar_reveal);
    root.append(&toolbar);
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&root));
    window.set_content(Some(&toasts));

    // Window-wide suggestion dismissal: the list keeps autohide off so
    // typing never loses the keyboard, and it lives on its own surface,
    // so any capture click reaching the main surface is outside the list
    // by construction. Clicks back into the entry only close it until the
    // next keystroke reopens it. Losing window focus closes it too.
    {
        let pop = suggest_popover.clone();
        let dismiss = gtk4::GestureClick::new();
        dismiss.set_propagation_phase(gtk4::PropagationPhase::Capture);
        dismiss.connect_pressed(move |_, _, _, _| {
            pop.popdown();
        });
        toasts.add_controller(dismiss);
        let pop = suggest_popover.clone();
        window.connect_notify_local(Some("is-active"), move |w, _| {
            if !w.is_active() {
                pop.popdown();
            }
        });
    }
    // History opens after first paint: suggestions, Frequent tiles and the
    // history page simply see an empty store until the database is ready,
    // instead of blocking the window on SQLite.
    let history: Rc<RefCell<Option<History>>> = Rc::new(RefCell::new(None));
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
        key_btn,
        star_btn: star_btn.clone(),
        engine_btn: engine_btn.clone(),
        find_bar: find_bar.clone(),
        find_entry: find_entry.clone(),
        find_status,
        toasts: toasts.clone(),
        transient_toast: RefCell::new(None),
        last_session: RefCell::new(Vec::new()),
        last_save: RefCell::new(None),
        last_sample: RefCell::new(None),
        last_newtab,
        suggest_pop: suggest_popover.clone(),
        sidebar_reveal: sidebar_reveal.clone(),
        sidebar_box: sidebar_box.clone(),
        sidebar_pins: sidebar_pins.clone(),
        sidebar_list: sidebar_list.clone(),
        sidebar_head_label: sidebar_label.clone(),
        sidebar_collapse_btn: sidebar_collapse.clone(),
        header_sidebar_btn: sidebar_btn.clone(),
        toolbar: toolbar.clone(),
        palette_card: palette_card.clone(),
        palette_entry: palette_entry.clone(),
        palette_list: palette_list.clone(),
        palette_store: RefCell::new(Vec::new()),
        pin_label: pin_label.clone(),
        shield_label: shield_label.clone(),
        settings_sidebar_row: RefCell::new(None),
        settings_rail_row: RefCell::new(None),
        sidebar_pages: RefCell::new(Vec::new()),
        sidebar_visible: RefCell::new(prefs.borrow().sidebar_visible),
        sidebar_rail: RefCell::new(prefs.borrow().sidebar_collapsed),
        prev_selected: RefCell::new(None),
        key_pop,
        key_list: key_list.clone(),
        shield: Rc::new(RefCell::new(ShieldStore::new(data_file("shield.json")))),
        curtain: Rc::new(RefCell::new(CurtainStore::new(data_file("curtain.json")))),
        vault: Rc::new(RefCell::new(FileVaultStore::new(data_file("vault.json")))),
        picker_armed: RefCell::new(false),
        picker_view: RefCell::new(None),
        settings_win: RefCell::new(None),
        popouts: RefCell::new(Vec::new()),
        history,
        bookmarks,
        bookmarks_store,
        context,
        session: session.clone(),
        prefs: prefs.clone(),
        prefs_store,
        tabs: Rc::new(RefCell::new(Tabs::new())),
    });
    // Restore the persisted sidebar shape before first paint.
    shell.sidebar_box.set_size_request(if *shell.sidebar_rail.borrow() { 56 } else { 260 }, -1);
    shell.sidebar_reveal.set_reveal_child(*shell.sidebar_visible.borrow());
    shell.sync_header_sidebar_btn();

    // Palette wiring: typing re-ranks, arrows move, Enter runs, Escape
    // dismisses back to the page. Clicking a row runs it directly.
    {
        let s = shell.clone();
        let entry = s.palette_entry.clone();
        entry.connect_changed(move |_| {
            s.render_palette();
        });
    }
    {
        let s = shell.clone();
        let entry = s.palette_entry.clone();
        entry.connect_activate(move |_| {
            s.activate_palette();
        });
    }
    {
        let s = shell.clone();
        let entry = s.palette_entry.clone();
        let focus = gtk4::EventControllerFocus::new();
        focus.connect_leave(move |_| {
            s.hide_palette();
        });
        entry.add_controller(focus);
    }
    {
        let s = shell.clone();
        let list = s.palette_list.clone();
        let keys = gtk4::EventControllerKey::new();
        // Capture: the search entry claims Escape for stop-search at target
        // phase, so a bubble handler would never see the dismiss key.
        keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
        keys.connect_key_pressed(move |_, keyval, _, _| match keyval {
            gtk4::gdk::Key::Escape => {
                s.hide_palette();
                if let Some(v) = selected_view(&s.tab_view) {
                    v.grab_focus();
                }
                glib::Propagation::Stop
            }
            gtk4::gdk::Key::Down | gtk4::gdk::Key::Up => {
                let count = suggest_row_count(&list);
                if count == 0 {
                    return glib::Propagation::Proceed;
                }
                let current = list
                    .selected_row()
                    .and_then(|row| usize::try_from(row.index()).ok())
                    .map(|i| i as isize);
                let next = match (keyval, current) {
                    (gtk4::gdk::Key::Down, Some(i)) => (i + 1) % count as isize,
                    (gtk4::gdk::Key::Down, None) => 0,
                    (_, Some(i)) => (i - 1 + count as isize) % count as isize,
                    (_, None) => count as isize - 1,
                };
                list.select_row(list.row_at_index(next as i32).as_ref());
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        });
        entry.add_controller(keys);
    }
    {
        let s = shell.clone();
        let list = s.palette_list.clone();
        list.connect_row_activated(move |list, row| {
            list.select_row(Some(row));
            s.activate_palette();
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
        let btn = s.star_btn.clone();
        btn.connect_clicked(move |_| {
            s.toggle_bookmark();
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
                    s.fill_login(&creds[i].username, &creds[i].secret);
                }
                _ => s.save_login(),
            }
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
            // Positional mapping: refresh_sidebar lists unpinned tabs in
            // order, so row N is unpinned tab N. Pinned tabs have their own
            // buttons and never reach here.
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
                None if selected.is_none() && popover.is_visible() => {
                    urls.borrow().first().cloned().unwrap_or(fallback)
                }
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
            // Debounced: bursts of closes cost at most one write per 5 s.
            s.save_session();
        });
    }
    {
        let s = shell.clone();
        window.connect_close_request(move |_| {
            closing.set(true);
            s.flush_session();
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
    ensure_filter(&shell);
    shell.install_actions(app);
    shell.on_selection_changed();
    if *shell.sidebar_visible.borrow() {
        shell.refresh_sidebar();
    }
    {
        // Deferred history open: first paint already happened above, so the
        // SQLite open plus the Frequent query land here. The start page is
        // then rewritten with real Frequent tiles and reloaded.
        let s = shell.clone();
        glib::idle_add_local_once(move || {
            match History::open(history_path()) {
                Ok(history) => *s.history.borrow_mut() = Some(history),
                Err(e) => eprintln!("br0x: history unavailable: {e}"),
            }
            let engine = s.prefs.borrow().engine;
            s.refresh_start_page(engine);
            s.reload_start_pages();
        });
    }
    if let Some(v) = selected_view(&shell.tab_view) {
        v.grab_focus();
    }
    window.present();

    // Dev lever, not product surface: BR0X_SHOT=<path> renders the window to
    // a PNG after first paint and exits. It is the only way to review chrome
    // on a headless box, so UI changes ship with before/after captures from
    // it. Optional BR0X_SHOT_DELAY_MS tunes how long the window settles
    // (default 1200 ms, enough for fonts, revealers, and the first frame).
    if let Some(path) = std::env::var_os("BR0X_SHOT") {
        let path = path.to_string_lossy().to_string();
        let delay = std::env::var("BR0X_SHOT_DELAY_MS")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(1200);
        // BR0X_SHOT_OPEN opens a transient surface first. A popover draws on
        // its own surface, so it never appears in a window snapshot: the
        // capture target becomes the popover's content instead.
        let open = std::env::var("BR0X_SHOT_OPEN").unwrap_or_default();
        let menu_btn = menu_btn.clone();
        let menu_body = menu_scroll.clone();
        let card = palette_card.clone();
        let shell_for_shot = shell.clone();
        let win = window.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(delay as u64), move || {
            let probes: [(&str, &gtk4::Widget); 8] = [
                ("menu", menu_btn.upcast_ref()),
                ("engine", engine_btn.upcast_ref()),
                ("star", star_btn.upcast_ref()),
                ("entry", entry.upcast_ref()),
                ("sidebar-toggle", sidebar_btn.upcast_ref()),
                ("palette-entry", palette_entry.upcast_ref()),
                ("sidebar-head", sidebar_head.upcast_ref()),
                ("tab-bar", tab_bar.upcast_ref()),
            ];
            for (name, widget) in probes {
                match widget.compute_bounds(&window) {
                    Some(rect) => eprintln!(
                        "br0x: bounds {name} x={} y={} w={} h={}",
                        rect.x() as i32,
                        rect.y() as i32,
                        rect.width() as i32,
                        rect.height() as i32
                    ),
                    None => eprintln!("br0x: bounds {name} unbounded"),
                }
            }
            if std::env::var_os("BR0X_SHOT_DUMP").is_some() {
                for (name, widget) in probes {
                    match widget.compute_bounds(&win) {
                        Some(rect) => eprintln!(
                            "br0x: bounds {name} x={} y={} w={} h={}",
                            rect.x() as i32,
                            rect.y() as i32,
                            rect.width() as i32,
                            rect.height() as i32
                        ),
                        None => eprintln!("br0x: bounds {name} unbounded"),
                    }
                }
            }
            // BR0X_SHOT_HOLD keeps the surface up so an external capture can
            // grab the popup's own X window instead of painting it in-process.
            if let Ok(hold) = std::env::var("BR0X_SHOT_HOLD") {
                let seconds = hold.parse::<u32>().unwrap_or(6);
                match open.as_str() {
                    "menu" => menu_btn.popup(),
                    "palette" => shell_for_shot.toggle_palette(),
                    _ => {}
                }
                glib::timeout_add_local_once(
                    std::time::Duration::from_secs(seconds as u64),
                    || std::process::exit(0),
                );
                return;
            }
            let target: gtk4::Widget = match open.as_str() {
                "menu" => {
                    menu_btn.popup();
                    // A popover's surface is not paintable in-process, so
                    // allocate its content explicitly and paint that.
                    let (_, natural) = menu_body.preferred_size();
                    menu_body.allocate(natural.width(), natural.height(), -1, None);
                    eprintln!(
                        "br0x: popup visible={} size={}x{}",
                        menu_btn.popover().map(|p| p.is_visible()).unwrap_or(false),
                        natural.width(),
                        natural.height()
                    );
                    menu_body.upcast()
                }
                "palette" => {
                    shell_for_shot.toggle_palette();
                    card.upcast()
                }
                _ => win.clone().upcast(),
            };
            // One more beat for the new surface to allocate before painting.
            glib::timeout_add_local_once(std::time::Duration::from_millis(400), move || {
                let w = target.width().max(1);
                let h = target.height().max(1);
                let renderer = target.native().and_then(|n| n.renderer());
                let paintable = gtk4::WidgetPaintable::new(Some(&target));
                let snapshot = gtk4::Snapshot::new();
                paintable.snapshot(&snapshot, w as f64, h as f64);
                match (snapshot.to_node(), renderer) {
                    (Some(node), Some(renderer)) => {
                        let texture = renderer.render_texture(&node, None);
                        match texture.save_to_png(&path) {
                            Ok(()) => eprintln!("br0x: shot saved {path} ({w}x{h})"),
                            Err(e) => eprintln!("br0x: shot save failed: {e}"),
                        }
                    }
                    (None, _) => eprintln!("br0x: shot produced no node"),
                    (_, None) => eprintln!("br0x: shot has no renderer"),
                }
                std::process::exit(0);
            });
        });
    }
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
    // Our appearance pref owns this process's theme. A user-set GTK_THEME
    // makes libadwaita ignore color-scheme switches entirely, which reads
    // as "light mode does nothing", so it goes before any adw init.
    // SAFETY: first statement of main; no other thread exists yet.
    unsafe {
        std::env::remove_var("GTK_THEME");
    }
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
        use theme::Scheme as S;
        fn visit(url: &str, title: &str, at: i64) -> Visit {
            Visit { url: url.to_string(), title: title.to_string(), visited_at: at }
        }
        let sites = vec![visit("https://a.example", "A", 10)];
        let cached = (SearchEngine::DuckDuckGo, sites.clone(), S::Light);
        // Nothing written yet: the page has to be built.
        assert!(pages::newtab_page_stale(None, SearchEngine::DuckDuckGo, &sites, S::Light));
        assert!(!pages::newtab_page_stale(
            Some(&cached),
            SearchEngine::DuckDuckGo,
            &sites,
            S::Light
        ));
        // Engine, list content, list order and scheme all change the page.
        assert!(pages::newtab_page_stale(Some(&cached), SearchEngine::Google, &sites, S::Light));
        assert!(pages::newtab_page_stale(Some(&cached), SearchEngine::DuckDuckGo, &sites, S::Dark));
        let mut more = sites.clone();
        more.push(visit("https://b.example", "B", 20));
        assert!(pages::newtab_page_stale(Some(&cached), SearchEngine::DuckDuckGo, &more, S::Light));
        let reordered = vec![more[1].clone(), sites[0].clone()];
        assert!(pages::newtab_page_stale(
            Some(&cached),
            SearchEngine::DuckDuckGo,
            &reordered,
            S::Light
        ));
        assert!(pages::newtab_page_stale(Some(&cached), SearchEngine::DuckDuckGo, &[], S::Light));
        // Titles are card text, so a retitle must rewrite.
        let retitled = vec![visit("https://a.example", "A renamed", 10)];
        assert!(pages::newtab_page_stale(
            Some(&cached),
            SearchEngine::DuckDuckGo,
            &retitled,
            S::Light
        ));
    }

    #[test]
    fn reader_script_carries_the_scheme() {
        assert!(pages::reader_extract_js(theme::Scheme::Light).contains("color-scheme:light}"));
        assert!(pages::reader_extract_js(theme::Scheme::Dark).contains("color-scheme:dark}"));
    }

    #[test]
    fn sleep_timeout_is_honored_by_the_sweep() {
        use br0x_core::tab::{SysState, TabId, TabSnapshot};
        fn snap(idle_secs: u64) -> TabSnapshot {
            TabSnapshot {
                id: TabId(1),
                last_active_secs_ago: idle_secs,
                audible: false,
                capturing: false,
                downloading: false,
                form_dirty: false,
                pinned: false,
                keep_alive: false,
                restored_secs_ago: None,
            }
        }
        let sys = SysState { tab_count: 9, mem_used_percent: 40.0 };
        // 10-minute timeout sleeps where the 30-minute default only parks.
        let quick = sweep_with_sleep(&[snap(700)], &sys, SleepTimeout::Min10.secs());
        assert_eq!(quick, vec![(TabId(1), Action::Sleep)]);
        let normal = sweep_with_sleep(&[snap(700)], &sys, SleepTimeout::Min30.secs());
        assert_eq!(normal, vec![(TabId(1), Action::Park)]);
        // Never disables the timer; pressure still parks old tabs.
        let never = sweep_with_sleep(&[snap(5000)], &sys, SleepTimeout::Never.secs());
        assert_eq!(never, vec![(TabId(1), Action::Park)]);
        let busy = SysState { tab_count: 9, mem_used_percent: 90.0 };
        assert_eq!(
            sweep_with_sleep(&[snap(70)], &busy, SleepTimeout::Never.secs()),
            vec![(TabId(1), Action::Park)]
        );
        // Recent tabs are kept under every setting.
        assert!(sweep_with_sleep(&[snap(10)], &sys, SleepTimeout::Min10.secs()).is_empty());
    }

    #[test]
    fn tab_moves_clamp_to_the_strip() {
        assert_eq!(Shell::move_target(0, -1, 3), 0);
        assert_eq!(Shell::move_target(0, 1, 3), 1);
        assert_eq!(Shell::move_target(2, 1, 3), 2);
        assert_eq!(Shell::move_target(1, -5, 3), 0);
        assert_eq!(Shell::move_target(1, 5, 3), 2);
        assert_eq!(Shell::move_target(0, 1, 0), 0);
    }

    #[test]
    fn blank_uris_are_recognised() {
        assert!(is_blank_uri("about:blank"));
        assert!(is_blank_uri(&pages::start_page_url()));
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
        assert_eq!(strip_state_badges("News • Parked • Sleeping"), "News");
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
