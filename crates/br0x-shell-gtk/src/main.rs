//! br0x shell: GTK4 + WebKitGTK 6.0 window.
//! Thin UI over br0x-core policy. All timing rules live in core.

use adw::prelude::*;
use gtk4::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;
use webkit6::prelude::*;

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

fn shared_session() -> webkit6::NetworkSession {
    for dir in [data_dir(), cache_dir()] {
        let _ = std::fs::create_dir_all(&dir);
    }
    webkit6::NetworkSession::new(Some(&data_dir()), Some(&cache_dir()))
}

fn new_view(session: &webkit6::NetworkSession) -> webkit6::WebView {
    webkit6::WebView::builder().property("network-session", session).build()
}

fn with_scheme(input: &str) -> String {
    if input.contains("://") { input.to_owned() } else { format!("https://{input}") }
}

/// Freeze background tabs: hide them so WebKit throttles rAF and timers.
/// Audible tabs stay unmuted. All others are muted until focused.
fn update_visibility(notebook: &gtk4::Notebook) {
    let current = notebook.current_page();
    let n = notebook.n_pages();
    for i in 0..n {
        let Some(page) = notebook.nth_page(Some(i)) else {
            continue;
        };
        let active = Some(i) == current;
        page.set_visible(active);
        if let Ok(view) = page.downcast::<webkit6::WebView>() {
            if active {
                view.set_is_muted(false);
            } else if !view.is_playing_audio() {
                view.set_is_muted(true);
            }
        }
    }
}

fn add_tab(
    notebook: &gtk4::Notebook,
    session: &webkit6::NetworkSession,
    entry: &gtk4::Entry,
    url: &str,
) {
    let view = new_view(session);
    view.load_uri(&with_scheme(url));
    let label = gtk4::Label::new(Some("New tab"));
    let view_clone = view.clone();
    view.connect_title_notify(move |_| {
        if let Some(t) = view_clone.title() {
            label.set_text(&t);
        }
    });
    let entry_clone = entry.clone();
    let view_uri = view.clone();
    view.connect_uri_notify(move |_| {
        if view_uri.has_focus() {
            return;
        }
        if let Some(u) = view_uri.uri() {
            entry_clone.set_text(u.as_str());
        }
    });
    notebook.append_page(&view, Some(&label));
    notebook.set_current_page(Some(notebook.n_pages() - 1));
    update_visibility(notebook);
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

    let nb = Rc::new(RefCell::new(notebook));
    let sess = Rc::new(session);

    {
        let nb_clone = nb.clone();
        let sess_clone = sess.clone();
        let entry_clone = entry.clone();
        entry.connect_activate(move |e| {
            let nb = nb_clone.borrow();
            let url = e.text().to_string();
            add_tab(&nb, &sess_clone, &entry_clone, url.trim());
        });
    }
    {
        let nb_clone = nb.clone();
        let sess_clone = sess.clone();
        let entry_clone = entry.clone();
        new_btn.connect_clicked(move |_| {
            let nb = nb_clone.borrow();
            add_tab(&nb, &sess_clone, &entry_clone, "https://example.com");
        });
    }
    {
        let nb_clone = nb.clone();
        nb.borrow().connect_switch_page(move |_, _, _| {
            let nb = nb_clone.borrow();
            update_visibility(&nb);
        });
    }

    add_tab(&nb.borrow(), &sess, &entry, "https://example.com");
    window.present();
}

fn main() {
    let app = adw::Application::builder().application_id("org.br0x.Browser").build();
    app.connect_activate(build_ui);
    app.run();
}
