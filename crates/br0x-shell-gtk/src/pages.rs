//! Internal pages: the start page, the history page, the error page, and the
//! reader extraction script.
//!
//! One seam for everything the shell renders outside a website. The shell
//! asks for markup and gets markup: no widget, no stylesheet, and no IO beyond
//! writing the start page file it owns. Page design, escaping, favicon
//! placeholders, and the colour-scheme token all live here, so a change to any
//! of them lands in one place instead of four page builders.

use br0x_core::history::{History, Visit};
use br0x_core::search::SearchEngine;
use gtk4::glib;
use std::cell::RefCell;

use crate::theme::Scheme;

pub fn html_escape(input: &str) -> String {
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
pub fn display_domain(url: &str) -> &str {
    url.strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .and_then(|s| s.split('/').next())
        .unwrap_or("local")
}

/// First letter for avatars, uppercased. Falls back to a bullet.
pub fn avatar_letter(name: &str) -> String {
    name.chars().next().unwrap_or('•').to_uppercase().to_string()
}

/// Day bucket label: Today, Yesterday, or a date.
pub fn history_day_label(visited_at: i64, today_day: i32) -> String {
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

pub fn history_time(visited_at: i64) -> String {
    glib::DateTime::from_unix_local(visited_at)
        .ok()
        .and_then(|dt| dt.format("%H:%M").ok())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

pub fn history_row(url: &str, title: &str, domain: &str, time: &str) -> String {
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
        fav = site_icon(domain, &letter),
        time = html_escape(time),
    )
}

/// The br0x://history page: a simple, clean list of past visits.
pub fn history_html(history: &History, query: Option<&str>, clear: bool, scheme: Scheme) -> String {
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
    :root {{ color-scheme: {scheme}; }}
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
      box-shadow: 0 1px 2px rgba(0, 0, 0, 0.05);
    }}
    .filter-box input:focus {{
      outline: none;
      border-color: AccentColor;
      box-shadow: 0 0 0 3px color-mix(in srgb, AccentColor 16%, transparent);
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
      background-color: inherit;
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
    const countEl = document.querySelector('.count');
    const totalLabel = countEl ? countEl.textContent : '';
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
          noMatches.style.display = visibleCount === 0 ? 'block' : 'none';
        }}
        // The header count must describe what is on screen, not what is
        // stored: leaving the old total there contradicts the filtered list.
        if (countEl) {{
          countEl.textContent = q === ''
            ? totalLabel
            : (visibleCount + ' of ' + rows.length);
        }}
      }});
    }}
  </script>
  {clear_script}
</body>
</html>"#,
        count_label = count_label,
        scheme = scheme.token(),
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

/// Write the start page file. A file keeps the page offline, instant, and
/// user editable. False when the write failed, so the caller keeps the page
/// marked stale and retries instead of serving the old one forever.
pub fn write_newtab_page(engine: SearchEngine, frequent: &[Visit], scheme: Scheme) -> bool {
    let path = start_page_path();
    if let Some(parent) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::write(&path, newtab_html(engine, frequent, scheme)).is_err() {
        eprintln!("br0x: could not write {path}");
        return false;
    }
    true
}

/// True when the start page file no longer matches the inputs it was built
/// from. Frequent embeds live history, so the engine alone is not a safe
/// key: titles and visit times shape the cards and order too.
pub fn newtab_page_stale(
    cached: Option<&(SearchEngine, Vec<Visit>, Scheme)>,
    engine: SearchEngine,
    frequent: &[Visit],
    scheme: Scheme,
) -> bool {
    cached.is_none_or(|(cached_engine, cached_frequent, cached_appearance)| {
        *cached_engine != engine
            || *cached_appearance != scheme
            || cached_frequent.as_slice() != frequent
    })
}

/// Write the start page only when it is stale, and return its file URL
/// either way. Repeats with an unchanged engine and history — every Ctrl+T —
/// cost a comparison instead of a rewrite of the whole page.
/// Where the start page file lives. One definition, so the shell's identity
/// checks can never drift from the file the page was written to.
pub fn start_page_path() -> String {
    crate::data_file("newtab.html")
}

/// The start page's `file://` URL, percent-encoded by the shell's helper.
pub fn start_page_url() -> String {
    crate::file_url(&start_page_path())
}

pub fn sync_newtab_page(
    last: &RefCell<Option<(SearchEngine, Vec<Visit>, Scheme)>>,
    engine: SearchEngine,
    frequent: &[Visit],
    scheme: Scheme,
) -> String {
    let stale = {
        let cached = last.borrow();
        newtab_page_stale(cached.as_ref(), engine, frequent, scheme)
    };
    if stale && write_newtab_page(engine, frequent, scheme) {
        last.replace(Some((engine, frequent.to_vec(), scheme)));
    }
    start_page_url()
}

/// One site tile: real favicon over a letter fallback, name and domain.
/// `key` adds a silent number-key shortcut; `class` extends the styling.
pub fn site_card(url: &str, name: &str, key: Option<&str>, class: &str) -> String {
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
        fav = site_icon(domain, &letter),
        name = html_escape(name),
        domain = html_escape(domain),
        key_badge = key_badge,
    )
}
/// Human name for a frequent URL. Raw URLs and URL-looking titles fall
/// back to the domain so tiles never show query strings.
pub fn frequent_name(visit: &Visit) -> String {
    if !visit.title.is_empty() && visit.title != visit.url && !visit.title.starts_with("http") {
        visit.title.clone()
    } else {
        display_domain(&visit.url).to_string()
    }
}

/// Local letter tile. Favicons once came from a remote icon service, which
/// disclosed every visited domain to a third party on each new tab.
pub fn avatar_tile(letter: &str) -> String {
    format!(
        r#"<span class="fav" aria-hidden="true"><span class="fav-letter">{letter}</span></span>"#,
        letter = html_escape(letter),
    )
}

/// The stored icon for a host, if one was saved when that site was last
/// visited. Served from the shell's own scheme, so a page can show real site
/// icons without asking any third party.
fn stored_icon(host: &str) -> Option<String> {
    let key = crate::favicon_key(host);
    if key.is_empty() || key == "local" {
        return None;
    }
    let path = crate::favicon_path(&key);
    std::path::Path::new(&path).exists().then(|| format!("br0x://favicon/{key}"))
}

/// A site's icon: the stored favicon when we have one, otherwise the letter
/// tile, which needs no network and always renders.
fn site_icon(host: &str, letter: &str) -> String {
    match stored_icon(host) {
        Some(url) => format!(
            r#"<span class="fav" aria-hidden="true"><span class="fav-letter">{letter}</span><img class="fav-img" src="{url}" alt="" onerror="this.remove()"></span>"#,
            letter = html_escape(letter),
            url = html_escape(&url),
        ),
        None => avatar_tile(letter),
    }
}

const SHORTCUTS: [(&str, &str, &str); 6] = [
    ("1", "GitHub", "https://github.com"),
    ("2", "YouTube", "https://youtube.com"),
    ("3", "Reddit", "https://reddit.com"),
    ("4", "Hacker News", "https://news.ycombinator.com"),
    ("5", "Wikipedia", "https://wikipedia.org"),
    ("6", "Mail", "https://mail.google.com"),
];

pub fn newtab_html(engine: SearchEngine, frequent: &[Visit], scheme: Scheme) -> String {
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
    :root {{ color-scheme: {scheme}; }}
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
      color: AccentColor;
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
      box-shadow: 0 1px 2px rgba(0, 0, 0, 0.06), 0 12px 32px rgba(0, 0, 0, 0.08);
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
      color: light-dark(#5f6672, #a9a9a9);
      background: light-dark(#f1f4f9, #3a3a3a);
      border: 1px solid light-dark(#e2e5ec, #4a4a4a);
      border-radius: 9999px;
      padding: 3px 9px;
      pointer-events: none;
      white-space: nowrap;
    }}
    .search-field:focus {{
      outline: none;
      border-color: AccentColor;
      box-shadow: 0 0 0 3px color-mix(in srgb, AccentColor 16%, transparent);
    }}
    .search-field::placeholder {{
      color: light-dark(#767676, #a9a9a9);
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
      box-shadow: 0 6px 18px rgba(0, 0, 0, 0.1);
    }}
    .section-count {{
      font-weight: 600;
      color: light-dark(#5f6672, #9e9e9e);
    }}
    .hint-card {{
      width: 100%;
      margin-top: 22px;
      text-align: center;
      border: 1px solid light-dark(rgba(0, 0, 0, 0.08), rgba(255, 255, 255, 0.09));
      border-radius: 14px;
      padding: 20px 18px;
      background: light-dark(rgba(0, 0, 0, 0.02), rgba(255, 255, 255, 0.03));
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
      color: light-dark(#5f6672, #a9a9a9);
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
      background-color: inherit;
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
      color: light-dark(#5f6672, #a9a9a9);
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
      visibility: hidden;
    }}
    .pin:hover .pin-remove {{
      visibility: visible;
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
      border-color: AccentColor;
      color: AccentColor;
    }}
    footer {{
      margin-top: 40px;
      color: light-dark(#767676, #a9a9a9);
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
      background: light-dark(rgba(0, 0, 0, 0.4), rgba(0, 0, 0, 0.65));
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
      border-color: AccentColor;
      box-shadow: 0 0 0 3px color-mix(in srgb, AccentColor 16%, transparent);
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
      background: AccentColor;
      color: AccentColorText;
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
          var iconUrl = pin.icon || '';
          if (iconUrl) {{
            var im = document.createElement('img');
            im.className = 'fav-img';
            im.alt = '';
            im.src = iconUrl;
            im.onerror = function() {{ this.remove(); }};
            fav.appendChild(im);
          }}
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
        return;
      }}
      // A modal dialog owns the keyboard while open: no shortcuts, and no
      // focusing the search field behind it.
      if (document.querySelector('#pin-modal.open')) return;
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
        scheme = scheme.token(),
        empty_hint = empty_hint,
    )
}

/// Dependency-free article extraction: score paragraphs by text length minus
/// link text, keep the winning container, drop chrome, return a clean page
/// (or empty when nothing article-like is found).
const READER_EXTRACT_TEMPLATE: &str = r##"(()=>{function textLen(n){return ((n.innerText||'').trim().length);}function score(p){var t=(p.innerText||'').trim();if(t.length<40){return 0;}var links=Array.prototype.reduce.call(p.querySelectorAll('a'),function(n,a){return n+((a.innerText||'').length);},0);return t.length-links*2;}var ps=Array.prototype.slice.call(document.querySelectorAll('p'));var buckets=new Map();ps.forEach(function(p){var s=score(p);if(s<=0){return;}var a=p.parentElement;for(var i=0;i<3&&a;i++){buckets.set(a,(buckets.get(a)||0)+s);a=a.parentElement;}});var root=null;var best=0;buckets.forEach(function(v,k){if(v>best){best=v;root=k;}});if(!root||best<200){return '';}var clone=root.cloneNode(true);Array.prototype.forEach.call(clone.querySelectorAll('nav,aside,footer,header,form,script,style,noscript,iframe,canvas,.ad,.ads,.sidebar,.comments,#comments'),function(n){n.remove();});var title=(document.title||'').replace(/</g,'&lt;');var body=clone.innerHTML||'';return '<!doctype html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>'+title+'</title><style>:root{color-scheme:light dark}body{margin:0 auto;max-width:38em;padding:2em 1.2em;font:18px/1.7 system-ui,sans-serif}img,video{max-width:100%;height:auto}pre{overflow:auto}</style></head><body><h1>'+title+'</h1>'+body+'</body></html>';})()"##;

/// Reader script with the current appearance baked in, so the article view
/// matches the shell chrome instead of only following the OS.
pub fn reader_extract_js(scheme: Scheme) -> String {
    READER_EXTRACT_TEMPLATE
        .replace("color-scheme:light dark", &format!("color-scheme:{}", scheme.token()))
}

/// Friendly inline page for failed loads and crashed processes.
pub fn error_html(heading: &str, message: &str, uri: &str, scheme: Scheme) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>{heading} — br0x</title>
<style>
:root {{ color-scheme: {scheme}; }}
body {{ margin: 0; font-family: system-ui, sans-serif; display: flex; min-height: 100vh;
  align-items: center; justify-content: center; text-align: center;
  background: light-dark(#fafafa, #1e1e1e); color: light-dark(#1c1c1c, #e8e8e8); }}
.card {{ max-width: 420px; padding: 32px 28px; }}
.icon {{ font-size: 40px; margin-bottom: 12px; }}
h1 {{ font-size: 20px; margin: 0 0 8px; }}
p {{ color: light-dark(#616161, #9e9e9e); font-size: 14px; margin: 0 0 6px; }}
.url {{ font-size: 12px; word-break: break-all; }}
button {{ margin-top: 18px; padding: 10px 22px; border-radius: 9999px; border: 0;
  background: AccentColor; color: AccentColorText; font: inherit; cursor: pointer; }}
</style></head>
<body><div class="card"><div class="icon">○</div><h1>{heading}</h1><p>{message}</p>
<p class="url">{uri}</p><button onclick="location.reload()">Reload</button></div></body></html>"#,
        heading = html_escape(heading),
        message = html_escape(message),
        uri = html_escape(uri),
        scheme = scheme.token(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use br0x_core::search::SearchEngine;

    fn visit(url: &str, title: &str, at: i64) -> Visit {
        Visit { url: url.to_string(), title: title.to_string(), visited_at: at }
    }

    /// The scheme token is the whole contract with the chrome: a page that
    /// disagrees with the shell renders light inside a dark window.
    #[test]
    fn every_page_carries_the_requested_scheme() {
        fn normalized(html: &str) -> String {
            html.replace("color-scheme: ", "color-scheme:")
        }
        for (scheme, token) in [(Scheme::Light, "light"), (Scheme::Dark, "dark")] {
            let start = normalized(&newtab_html(SearchEngine::DuckDuckGo, &[], scheme));
            assert!(start.contains(&format!("color-scheme:{token}")), "start page {token}");
            let error = normalized(&error_html("Failed", "Offline", "https://x.example", scheme));
            assert!(error.contains(&format!("color-scheme:{token}")), "error page {token}");
            let reader = normalized(&reader_extract_js(scheme));
            assert!(reader.contains(&format!("color-scheme:{token}")), "reader {token}");
        }
    }

    /// Offline first: the pages must not fetch anything on render, or the
    /// user's top sites leak to whoever serves the images.
    #[test]
    fn pages_make_no_third_party_requests() {
        let visits = vec![visit("https://secret.example/private", "Private", 10)];
        let start = newtab_html(SearchEngine::DuckDuckGo, &visits, Scheme::Light);
        for html in [start, history_html_stub(&visits)] {
            assert!(!html.contains("icons.duckduckgo.com"), "remote favicon in a page");
            for line in html.lines() {
                if let Some(pos) = line.find("src=\"http") {
                    panic!("remote asset at {pos}: {}", &line[pos..pos + 40.min(line.len() - pos)]);
                }
            }
        }
    }

    /// History needs a database, so this renders the same row builder the page
    /// uses rather than a fake store.
    fn history_html_stub(visits: &[Visit]) -> String {
        visits
            .iter()
            .map(|v| {
                let domain = display_domain(&v.url);
                history_row(&v.url, &v.title, domain, &history_time(v.visited_at))
            })
            .collect()
    }

    #[test]
    fn page_text_is_escaped() {
        let html = history_row(
            "https://x.example/?a=1&b=2",
            "<script>alert(1)</script>",
            "x.example",
            "Today",
        );
        assert!(!html.contains("<script>"), "script tag survived escaping");
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("&amp;b=2"), "query separator not escaped");
    }

    #[test]
    fn stale_when_any_input_changes() {
        let sites = vec![visit("https://a.example", "A", 10)];
        let written = (SearchEngine::DuckDuckGo, sites.clone(), Scheme::Light);
        assert!(newtab_page_stale(None, SearchEngine::DuckDuckGo, &sites, Scheme::Light));
        assert!(!newtab_page_stale(
            Some(&written),
            SearchEngine::DuckDuckGo,
            &sites,
            Scheme::Light
        ));
        assert!(newtab_page_stale(Some(&written), SearchEngine::Google, &sites, Scheme::Light));
        assert!(newtab_page_stale(Some(&written), SearchEngine::DuckDuckGo, &sites, Scheme::Dark));
    }

    #[test]
    fn start_page_file_name_is_stable() {
        assert!(start_page_path().ends_with("br0x/newtab.html"));
        assert!(start_page_url().starts_with("file://"));
    }
}
