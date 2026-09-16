//! br0x shell: GTK4 + WebKitGTK 6.0 window.
//! Thin UI over br0x-core policy. All timing rules live in core.

use adw::prelude::*;
use br0x_core::policy;
use br0x_core::session::{Session, SessionStore, StoredTab};
use br0x_core::tab::{Action, TabId, TabSnapshot};
use gtk4::glib;
use gtk4::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Instant;
use webkit6::prelude::*;

/// Minimal first party tracker block list in WebKit content extension JSON.
const BASE_FILTER_JSON: &str = r#"[
{"action":{"type":"block"},"trigger":{"url-filter":"doubleclick\\.net"}},
{"action":{"type":"block"},"trigger":{"url-filter":"googlesyndication\\.com"}},
{"action":{"type":"block"},"trigger":{"url-filter":"google-analytics\\.com"}},
{"action":{"type":"block"},"trigger":{"url-filter":"facebook\\.net/tr"}},
{"action":{"type":"block"},"trigger":{"url-filter":"hotjar\\.com"}}
]"#;

/// Per tab bookkeeping the policy needs. WebKit state is read live.
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

struct Tabs {
    metas: Vec<TabMeta>,
    next_id: u64,
}

impl Tabs {
    fn new() -> Self {
        Self { metas: Vec::new(), next_id: 1 }
    }

    fn push(&mut self, parked_url: Option<String>) -> TabId {
        let id = TabId(self.next_id);
        self.next_id += 1;
        let mut meta = TabMeta::fresh(id);
        if let Some(url) = parked_url {
            meta.parked = true;
            meta.pending_url = Some(url);
        }
        self.metas.push(meta);
        id
    }
}

fn data_dir() -> String {
    let base = std::env::var("XDG_DATA_HOME")
        .unwrap_or_else(|_| format!("{}/.local/share", std::env::var("HOME").unwrap()));
    format!("{base}/br0x/webdata")
}

fn cache_dir() -> String {
    let base = std::env::var("XDG_CACHE_HOME")
        .unwrap_or_else(|_| format!("{}/.cache", std::env::var("HOME").unwrap()));
    format!("{base}/br0x/webcache")
}

fn session_path() -> String {
    let base = std::env::var("XDG_DATA_HOME")
        .unwrap_or_else(|_| format!("{}/.local/share", std::env::var("HOME").unwrap()));
    format!("{base}/br0x/session.json")
}

fn shared_session() -> webkit6::NetworkSession {
    for dir in [data_dir(), cache_dir()] {
        let _ = std::fs::create_dir_all(&dir);
    }
    webkit6::NetworkSession::new(Some(&data_dir()), Some(&cache_dir()))
}

fn new_view(session: &webkit6::NetworkSession) -> webkit6::WebView {
    let view = webkit6::WebView::builder().network_session(session).build();
    apply_filter(&view);
    view
}

fn filter_store() -> webkit6::UserContentFilterStore {
    let dir = format!("{}/br0x/filters", data_dir());
    let _ = std::fs::create_dir_all(&dir);
    webkit6::UserContentFilterStore::new(&dir)
}

/// Compile the base filter once, then attach it to every view.
fn ensure_filter(notebook: &gtk4::Notebook) {
    let store = filter_store();
    let json = glib::Bytes::from_owned(BASE_FILTER_JSON.as_bytes().to_vec());
    let nb_weak = notebook.downgrade();
    glib::spawn_future_local(async move {
        match store.save_future("br0x-base", &json).await {
            Ok(filter) => {
                if let Some(nb) = nb_weak.upgrade() {
                    attach_filter(&nb, &filter);
                }
            }
            Err(e) => eprintln!("br0x: filter compile failed: {e}"),
        }
    });
}

fn attach_filter(notebook: &gtk4::Notebook, filter: &webkit6::UserContentFilter) {
    for i in 0..notebook.n_pages() {
        let Some(page) = notebook.nth_page(Some(i)) else {
            continue;
        };
        if let Ok(view) = page.downcast::<webkit6::WebView>() {
            if let Some(ucm) = view.user_content_manager() {
                ucm.add_filter(filter);
            }
        }
    }
}

fn apply_filter(view: &webkit6::WebView) {
    let store = filter_store();
    let json = glib::Bytes::from_owned(BASE_FILTER_JSON.as_bytes().to_vec());
    let view_weak = view.downgrade();
    glib::spawn_future_local(async move {
        if let Ok(filter) = store.save_future("br0x-base", &json).await {
            if let Some(view) = view_weak.upgrade() {
                if let Some(ucm) = view.user_content_manager() {
                    ucm.add_filter(&filter);
                }
            }
        }
    });
}

fn with_scheme(input: &str) -> String {
    if input.contains("://") { input.to_owned() } else { format!("https://{input}") }
}

fn view_at(notebook: &gtk4::Notebook, i: u32) -> Option<webkit6::WebView> {
    notebook.nth_page(Some(i)).and_then(|p| p.downcast().ok())
}

/// Hide background tabs so WebKit throttles rAF and timers.
/// Audible tabs stay unmuted. Parked tabs reload on focus.
fn update_visibility(notebook: &gtk4::Notebook, tabs: &Rc<RefCell<Tabs>>, entry: &gtk4::Entry) {
    let current = notebook.current_page();
    let mut state = tabs.borrow_mut();
    for (idx, meta) in state.metas.iter_mut().enumerate() {
        let i = idx as u32;
        let Some(view) = view_at(notebook, i) else {
            continue;
        };
        let active = Some(i) == current;
        if active {
            meta.last_active = Instant::now();
            if meta.parked {
                meta.parked = false;
                meta.restored_at = Some(Instant::now());
                if let Some(url) = meta.pending_url.take() {
                    view.load_uri(&url);
                }
            }
            view.set_visible(true);
            view.set_is_muted(false);
            if let Some(u) = view.uri() {
                entry.set_text(u.as_str());
            }
        } else {
            view.set_visible(false);
            if !view.is_playing_audio() {
                view.set_is_muted(true);
            }
        }
    }
}

fn add_tab(
    notebook: &gtk4::Notebook,
    tabs: &Rc<RefCell<Tabs>>,
    session: &webkit6::NetworkSession,
    entry: &gtk4::Entry,
    url: &str,
    lazy: bool,
) {
    let view = new_view(session);
    tabs.borrow_mut().push(lazy.then(|| with_scheme(url)));
    if !lazy {
        view.load_uri(&with_scheme(url));
    } else {
        view.load_uri("about:blank");
    }
    let label = gtk4::Label::new(Some("New tab"));
    let label_clone = label.clone();
    let view_clone = view.clone();
    view.connect_title_notify(move |_| {
        if let Some(t) = view_clone.title() {
            label_clone.set_text(&t);
        }
    });
    notebook.append_page(&view, Some(&label));
    notebook.set_current_page(Some(notebook.n_pages() - 1));
    update_visibility(notebook, tabs, entry);
}

/// 5s tick: sample pressure, ask core, freeze or park background tabs.
fn enforce_tick(notebook: &gtk4::Notebook, tabs: &Rc<RefCell<Tabs>>) {
    let sys = br0x_core::sampler::sample(notebook.n_pages() as usize);
    let snaps = collect_snapshots(notebook, tabs);
    for (id, action) in policy::sweep(&snaps, &sys) {
        apply_action(notebook, tabs, id, action);
    }
}

fn collect_snapshots(notebook: &gtk4::Notebook, tabs: &Rc<RefCell<Tabs>>) -> Vec<TabSnapshot> {
    let state = tabs.borrow();
    state
        .metas
        .iter()
        .enumerate()
        .filter_map(|(idx, meta)| {
            let view = view_at(notebook, idx as u32)?;
            Some(meta.snapshot(view.is_playing_audio()))
        })
        .collect()
}

fn apply_action(notebook: &gtk4::Notebook, tabs: &Rc<RefCell<Tabs>>, id: TabId, action: Action) {
    let mut state = tabs.borrow_mut();
    let Some(idx) = state.metas.iter().position(|m| m.id == id) else {
        return;
    };
    let Some(view) = view_at(notebook, idx as u32) else {
        return;
    };
    if Some(idx as u32) == notebook.current_page() {
        return;
    }
    match action {
        Action::Keep => {}
        Action::Freeze => {
            view.set_visible(false);
            if !view.is_playing_audio() {
                view.set_is_muted(true);
            }
        }
        Action::Park => {
            if state.metas[idx].parked {
                return;
            }
            if let Some(uri) = view.uri() {
                state.metas[idx].pending_url = Some(uri.to_string());
            }
            state.metas[idx].parked = true;
            view.load_uri("about:blank");
        }
    }
}

fn save_session(notebook: &gtk4::Notebook, tabs: &Rc<RefCell<Tabs>>) {
    let state = tabs.borrow();
    let stored = state
        .metas
        .iter()
        .enumerate()
        .map(|(idx, meta)| {
            let (url, title) = view_at(notebook, idx as u32)
                .map(|v| {
                    let u = v
                        .uri()
                        .map(|s| s.to_string())
                        .or_else(|| meta.pending_url.clone())
                        .unwrap_or_default();
                    let t = v.title().map(|s| s.to_string()).unwrap_or_default();
                    (u, t)
                })
                .unwrap_or_default();
            StoredTab { id: meta.id.0, url, title, order: idx, pinned: meta.pinned, scroll_y: 0 }
        })
        .collect();
    let store = SessionStore::new(session_path());
    if let Err(e) = store.save(&Session { tabs: stored }) {
        eprintln!("br0x: session save failed: {e}");
    }
}

fn restore_session(
    notebook: &gtk4::Notebook,
    tabs: &Rc<RefCell<Tabs>>,
    session: &webkit6::NetworkSession,
    entry: &gtk4::Entry,
) -> bool {
    let store = SessionStore::new(session_path());
    let Ok(mut stored) = store.load() else {
        return false;
    };
    if stored.tabs.is_empty() {
        return false;
    }
    stored.tabs.sort_by_key(|t| t.order);
    for (idx, tab) in stored.tabs.iter().enumerate() {
        add_tab(notebook, tabs, session, entry, &tab.url, idx != 0);
    }
    true
}

fn build_ui(app: &adw::Application) {
    let session = shared_session();
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("br0x")
        .default_width(1100)
        .default_height(750)
        .build();

    let entry = gtk4::Entry::new();
    entry.set_placeholder_text(Some("Search or address"));
    entry.set_hexpand(true);

    let new_btn = gtk4::Button::with_label("New tab");
    let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    header.append(&entry);
    header.append(&new_btn);

    let notebook = gtk4::Notebook::new();
    notebook.set_scrollable(true);

    let layout = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    layout.append(&header);
    layout.append(&notebook);
    window.set_content(Some(&layout));

    let tabs = Rc::new(RefCell::new(Tabs::new()));
    let sess = Rc::new(session);

    {
        let nb_weak = notebook.downgrade();
        let tabs_clone = tabs.clone();
        let sess_clone = sess.clone();
        let entry_clone = entry.clone();
        entry.connect_activate(move |e| {
            if let Some(nb) = nb_weak.upgrade() {
                let url = e.text().to_string();
                add_tab(&nb, &tabs_clone, &sess_clone, &entry_clone, url.trim(), false);
            }
        });
    }
    {
        let nb_weak = notebook.downgrade();
        let tabs_clone = tabs.clone();
        let sess_clone = sess.clone();
        let entry_clone = entry.clone();
        new_btn.connect_clicked(move |_| {
            if let Some(nb) = nb_weak.upgrade() {
                add_tab(&nb, &tabs_clone, &sess_clone, &entry_clone, "https://example.com", false);
            }
        });
    }
    {
        let nb_weak = notebook.downgrade();
        let tabs_clone = tabs.clone();
        let entry_clone = entry.clone();
        notebook.connect_switch_page(move |_, _, _| {
            if let Some(nb) = nb_weak.upgrade() {
                update_visibility(&nb, &tabs_clone, &entry_clone);
            }
        });
    }

    if !restore_session(&notebook, &tabs, &sess, &entry) {
        add_tab(&notebook, &tabs, &sess, &entry, "https://example.com", false);
    }
    ensure_filter(&notebook);

    {
        let nb_weak = notebook.downgrade();
        let tabs_clone = tabs.clone();
        glib::timeout_add_seconds_local(5, move || {
            if let Some(nb) = nb_weak.upgrade() {
                enforce_tick(&nb, &tabs_clone);
            }
            glib::ControlFlow::Continue
        });
    }
    {
        let tick = Cell::new(0u32);
        let nb_weak = notebook.downgrade();
        let tabs_clone = tabs.clone();
        glib::timeout_add_seconds_local(5, move || {
            tick.set(tick.get() + 1);
            if tick.get() % 6 == 0 {
                if let Some(nb) = nb_weak.upgrade() {
                    save_session(&nb, &tabs_clone);
                }
            }
            glib::ControlFlow::Continue
        });
    }
    {
        let nb_weak = notebook.downgrade();
        let tabs_clone = tabs.clone();
        window.connect_close_request(move |_| {
            if let Some(nb) = nb_weak.upgrade() {
                save_session(&nb, &tabs_clone);
            }
            glib::Propagation::Proceed
        });
    }

    window.present();
}

fn main() {
    let app = adw::Application::builder().application_id("org.br0x.Browser").build();
    app.connect_activate(build_ui);
    app.run();
}
