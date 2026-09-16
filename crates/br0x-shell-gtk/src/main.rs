//! br0x shell: GTK4 + WebKitGTK 6.0 window.
//! Thin UI over br0x-core policy. All timing rules live in core.

use adw::prelude::*;
use br0x_core::policy;
use br0x_core::session::{Session, SessionStore, StoredTab};
use br0x_core::tab::{Action, TabId, TabSnapshot};
use gtk4::glib;
use gtk4::prelude::*;
use std::cell::RefCell;
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

    fn remove(&mut self, idx: usize) {
        if idx < self.metas.len() {
            self.metas.remove(idx);
        }
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

/// Turn raw entry text into something loadable.
/// Plain words become a search, hostnames gain https.
fn resolve_input(input: &str) -> String {
    let t = input.trim();
    if t.contains("://") {
        t.to_owned()
    } else if t.contains(' ') || !t.contains('.') {
        let q: Vec<&str> = t.split_whitespace().collect();
        format!("https://duckduckgo.com/?q={}", q.join("+"))
    } else {
        format!("https://{t}")
    }
}

fn view_at(notebook: &gtk4::Notebook, i: u32) -> Option<webkit6::WebView> {
    notebook.nth_page(Some(i)).and_then(|p| p.downcast().ok())
}

fn active_view(notebook: &gtk4::Notebook) -> Option<webkit6::WebView> {
    notebook.current_page().and_then(|i| view_at(notebook, i))
}

fn refresh_nav(notebook: &gtk4::Notebook, back: &gtk4::Button, fwd: &gtk4::Button) {
    let (can_back, can_fwd) = active_view(notebook)
        .map(|v| (v.can_go_back(), v.can_go_forward()))
        .unwrap_or((false, false));
    back.set_sensitive(can_back);
    fwd.set_sensitive(can_fwd);
}

/// Hide background tabs so WebKit throttles rAF and timers.
/// Audible tabs stay unmuted. Parked tabs reload on focus.
fn update_visibility(
    notebook: &gtk4::Notebook,
    tabs: &Rc<RefCell<Tabs>>,
    entry: &gtk4::Entry,
    back: &gtk4::Button,
    fwd: &gtk4::Button,
) {
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
    refresh_nav(notebook, back, fwd);
}

fn close_tab(
    notebook: &gtk4::Notebook,
    tabs: &Rc<RefCell<Tabs>>,
    session: &Rc<webkit6::NetworkSession>,
    entry: &gtk4::Entry,
    back: &gtk4::Button,
    fwd: &gtk4::Button,
    idx: u32,
) {
    notebook.remove_page(Some(idx));
    tabs.borrow_mut().remove(idx as usize);
    if notebook.n_pages() == 0 {
        add_tab(notebook, tabs, session, entry, back, fwd, "https://example.com", false);
    } else {
        update_visibility(notebook, tabs, entry, back, fwd);
    }
}

fn add_tab(
    notebook: &gtk4::Notebook,
    tabs: &Rc<RefCell<Tabs>>,
    session: &webkit6::NetworkSession,
    entry: &gtk4::Entry,
    back: &gtk4::Button,
    fwd: &gtk4::Button,
    url: &str,
    lazy: bool,
) {
    let view = new_view(session);
    tabs.borrow_mut().push(lazy.then(|| resolve_input(url)));
    if !lazy {
        view.load_uri(&resolve_input(url));
    } else {
        view.load_uri("about:blank");
    }
    let label = gtk4::Label::new(Some("New tab"));
    label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    label.set_max_width_chars(24);
    let label_clone = label.clone();
    let view_clone = view.clone();
    view.connect_title_notify(move |_| {
        if let Some(t) = view_clone.title() {
            label_clone.set_text(&t);
        }
    });
    let entry_clone = entry.clone();
    let view_uri = view.clone();
    view.connect_uri_notify(move |_| {
        if let Some(u) = view_uri.uri() {
            if u != "about:blank" {
                entry_clone.set_text(u.as_str());
            }
        }
    });
    view.connect_load_failed(|_, _, uri, err| {
        eprintln!("br0x: load failed {uri}: {err}");
        false
    });
    view.connect_web_process_terminated(|_, reason| {
        eprintln!("br0x: web process terminated: {reason:?}");
    });
    let close_btn = gtk4::Button::from_icon_name("window-close-symbolic");
    close_btn.add_css_class("flat");
    close_btn.add_css_class("circular");
    close_btn.set_tooltip_text(Some("Close tab"));
    let tab_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
    tab_box.append(&label);
    tab_box.append(&close_btn);
    let page_num = notebook.append_page(&view, Some(&tab_box));
    {
        let nb_weak = notebook.downgrade();
        let tabs_clone = tabs.clone();
        let sess_clone = Rc::new(session.clone());
        let entry_clone = entry.clone();
        let back_clone = back.clone();
        let fwd_clone = fwd.clone();
        close_btn.connect_clicked(move |_| {
            if let Some(nb) = nb_weak.upgrade() {
                close_tab(
                    &nb,
                    &tabs_clone,
                    &sess_clone,
                    &entry_clone,
                    &back_clone,
                    &fwd_clone,
                    page_num,
                );
            }
        });
    }
    {
        let nb_weak = notebook.downgrade();
        let back_clone = back.clone();
        let fwd_clone = fwd.clone();
        view.connect_load_changed(move |_, _| {
            if let Some(nb) = nb_weak.upgrade() {
                refresh_nav(&nb, &back_clone, &fwd_clone);
            }
        });
    }
    notebook.set_current_page(Some(page_num));
    update_visibility(notebook, tabs, entry, back, fwd);
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
                        .filter(|s| s != "about:blank")
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
    back: &gtk4::Button,
    fwd: &gtk4::Button,
) -> bool {
    let store = SessionStore::new(session_path());
    let Ok(mut stored) = store.load() else {
        return false;
    };
    stored.tabs.retain(|t| !t.url.is_empty());
    if stored.tabs.is_empty() {
        return false;
    }
    stored.tabs.sort_by_key(|t| t.order);
    for (idx, tab) in stored.tabs.iter().enumerate() {
        add_tab(notebook, tabs, session, entry, back, fwd, &tab.url, idx != 0);
    }
    notebook.set_current_page(Some(0));
    true
}

fn build_ui(app: &adw::Application) {
    let session = shared_session();
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("br0x")
        .default_width(1200)
        .default_height(800)
        .build();

    let back = gtk4::Button::from_icon_name("go-previous-symbolic");
    back.set_tooltip_text(Some("Back"));
    let fwd = gtk4::Button::from_icon_name("go-next-symbolic");
    fwd.set_tooltip_text(Some("Forward"));
    let reload = gtk4::Button::from_icon_name("view-refresh-symbolic");
    reload.set_tooltip_text(Some("Reload"));
    let entry = gtk4::Entry::new();
    entry.set_placeholder_text(Some("Search or address"));
    entry.set_hexpand(true);
    let new_btn = gtk4::Button::from_icon_name("tab-new-symbolic");
    new_btn.set_tooltip_text(Some("New tab"));

    let header = adw::HeaderBar::new();
    header.pack_start(&back);
    header.pack_start(&fwd);
    header.pack_start(&reload);
    header.set_title_widget(Some(&entry));
    header.pack_end(&new_btn);
    window.set_titlebar(Some(&header));

    let notebook = gtk4::Notebook::new();
    notebook.set_scrollable(true);
    notebook.set_vexpand(true);

    let layout = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    layout.append(&notebook);
    window.set_content(Some(&layout));

    let tabs = Rc::new(RefCell::new(Tabs::new()));
    let sess = Rc::new(session);

    {
        let nb_weak = notebook.downgrade();
        let tabs_clone = tabs.clone();
        let sess_clone = sess.clone();
        let entry_clone = entry.clone();
        let back_clone = back.clone();
        let fwd_clone = fwd.clone();
        entry.connect_activate(move |e| {
            if let Some(nb) = nb_weak.upgrade() {
                let url = e.text().to_string();
                add_tab(
                    &nb,
                    &tabs_clone,
                    &sess_clone,
                    &entry_clone,
                    &back_clone,
                    &fwd_clone,
                    url.trim(),
                    false,
                );
            }
        });
    }
    {
        let nb_weak = notebook.downgrade();
        let tabs_clone = tabs.clone();
        let sess_clone = sess.clone();
        let entry_clone = entry.clone();
        let back_clone = back.clone();
        let fwd_clone = fwd.clone();
        new_btn.connect_clicked(move |_| {
            if let Some(nb) = nb_weak.upgrade() {
                add_tab(
                    &nb,
                    &tabs_clone,
                    &sess_clone,
                    &entry_clone,
                    &back_clone,
                    &fwd_clone,
                    "https://example.com",
                    false,
                );
            }
        });
    }
    {
        let tabs_clone = tabs.clone();
        let entry_clone = entry.clone();
        let back_clone = back.clone();
        let fwd_clone = fwd.clone();
        notebook.connect_switch_page(move |nb, _, _| {
            update_visibility(nb, &tabs_clone, &entry_clone, &back_clone, &fwd_clone);
        });
    }
    {
        let nb_weak = notebook.downgrade();
        back.connect_clicked(move |_| {
            if let Some(nb) = nb_weak.upgrade() {
                if let Some(v) = active_view(&nb) {
                    v.go_back();
                }
            }
        });
    }
    {
        let nb_weak = notebook.downgrade();
        fwd.connect_clicked(move |_| {
            if let Some(nb) = nb_weak.upgrade() {
                if let Some(v) = active_view(&nb) {
                    v.go_forward();
                }
            }
        });
    }
    {
        let nb_weak = notebook.downgrade();
        reload.connect_clicked(move |_| {
            if let Some(nb) = nb_weak.upgrade() {
                if let Some(v) = active_view(&nb) {
                    v.reload();
                }
            }
        });
    }

    if !restore_session(&notebook, &tabs, &sess, &entry, &back, &fwd) {
        add_tab(&notebook, &tabs, &sess, &entry, &back, &fwd, "https://example.com", false);
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
        let tick = std::cell::Cell::new(0u32);
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
