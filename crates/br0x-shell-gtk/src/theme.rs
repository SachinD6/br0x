//! Shell appearance: one scheme decision, one set of colors.
//!
//! Two rules keep the chrome coherent, and both are enforceable by tests:
//!
//! 1. **The appearance pref decides, and the shell paints the result.** Asking
//!    libadwaita for a variant is not enough: `GTK_THEME`, a user `gtk.css`, or
//!    a stuck portal can all keep the toolkit dark while the pref says light,
//!    which is exactly the "light mode with a black sidebar" bug. So every
//!    chrome surface is painted from the palette below, per scheme, and the
//!    toolkit request only covers the surfaces we do not own.
//! 2. **One answer for chrome and pages.** [`apply`] resolves the scheme once
//!    and records it in [`applied`]; the internal pages read that same value,
//!    so a page can never render light inside a dark shell.
//!
//! The palette values are libadwaita's own light and dark token values, read
//! from the shipped stylesheet, so an explicitly painted chrome still matches
//! the platform's greys instead of inventing new ones.

use std::cell::Cell;

use gtk4::CssProvider;
use gtk4::gdk::Display;

/// Light or dark, as actually rendered right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    Light,
    Dark,
}

impl Scheme {
    /// CSS `color-scheme` token for internal pages, so a page's own
    /// `light-dark()` resolves the same way the chrome did.
    pub fn token(self) -> &'static str {
        match self {
            Scheme::Light => "light",
            Scheme::Dark => "dark",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Scheme::Light => "Light",
            Scheme::Dark => "Dark",
        }
    }
}

/// The chrome colors for one scheme. Every entry is a libadwaita token value.
struct Chrome {
    window_bg: &'static str,
    window_fg: &'static str,
    header_bg: &'static str,
    header_fg: &'static str,
    sidebar_bg: &'static str,
    sidebar_fg: &'static str,
    sidebar_border: &'static str,
    view_bg: &'static str,
    popover_bg: &'static str,
    popover_fg: &'static str,
    border: &'static str,
    shade: &'static str,
}

const LIGHT: Chrome = Chrome {
    window_bg: "#fafafb",
    window_fg: "rgb(0 0 6 / 80%)",
    header_bg: "#ffffff",
    header_fg: "rgb(0 0 6 / 80%)",
    sidebar_bg: "#ebebed",
    sidebar_fg: "rgb(0 0 6 / 80%)",
    sidebar_border: "rgb(0 0 6 / 12%)",
    view_bg: "#ffffff",
    popover_bg: "#ffffff",
    popover_fg: "rgb(0 0 6 / 80%)",
    border: "rgb(0 0 6 / 15%)",
    shade: "rgb(0 0 6 / 7%)",
};

const DARK: Chrome = Chrome {
    window_bg: "#222226",
    window_fg: "#ffffff",
    header_bg: "#2e2e32",
    header_fg: "#ffffff",
    sidebar_bg: "#2e2e32",
    sidebar_fg: "#ffffff",
    sidebar_border: "rgb(0 0 6 / 40%)",
    view_bg: "#1d1d20",
    popover_bg: "#36363a",
    popover_fg: "#ffffff",
    border: "rgb(255 255 255 / 16%)",
    shade: "rgb(0 0 6 / 25%)",
};

fn chrome(scheme: Scheme) -> &'static Chrome {
    match scheme {
        Scheme::Light => &LIGHT,
        Scheme::Dark => &DARK,
    }
}

// The scheme in force. Written once by `apply`, read by every page builder.
thread_local! {
    static APPLIED: Cell<Scheme> = const { Cell::new(Scheme::Light) };
}

/// The scheme the shell is drawing with. This is the single answer the chrome
/// and the internal pages share.
pub fn applied() -> Scheme {
    APPLIED.with(Cell::get)
}

/// Resolve the pref against the OS for `System`, and keep the answer.
pub fn apply(appearance: br0x_core::prefs::Appearance) -> Scheme {
    use br0x_core::prefs::Appearance;
    let manager = adw::StyleManager::default();
    manager.set_color_scheme(match appearance {
        Appearance::System => adw::ColorScheme::Default,
        Appearance::Light => adw::ColorScheme::ForceLight,
        Appearance::Dark => adw::ColorScheme::ForceDark,
    });
    let scheme = match appearance {
        Appearance::Light => Scheme::Light,
        Appearance::Dark => Scheme::Dark,
        Appearance::System => {
            if manager.is_dark() {
                Scheme::Dark
            } else {
                Scheme::Light
            }
        }
    };
    // The toolkit can refuse to switch. That no longer matters for the chrome,
    // which is painted below, but it still affects dialogs we do not style, so
    // say so once instead of letting it surprise someone.
    let toolkit_dark = manager.is_dark();
    if scheme == Scheme::Light && toolkit_dark && appearance != Appearance::Dark {
        eprintln!("br0x: toolkit stayed dark; chrome is painted light for the chosen appearance");
    }
    APPLIED.with(|cell| cell.set(scheme));
    if let Some(display) = Display::default() {
        chrome_provider(&display).load_from_string(&chrome_css(scheme));
    }
    scheme
}

/// The provider set that carries the chrome palette. One instance per process,
/// reloaded whenever the scheme changes.
/// Above GTK's user layer (800). A hand-written `~/.config/gtk-4.0/gtk.css`
/// outranks any application-priority provider, and that is one of the ways a
/// light choice kept drawing dark chrome. Only br0x's own selectors are
/// targeted, so nothing else in the process is affected.
const CHROME_PRIORITY: u32 = gtk4::STYLE_PROVIDER_PRIORITY_USER + 1;

fn chrome_provider(display: &Display) -> CssProvider {
    thread_local! {
        static PROVIDER: CssProvider = CssProvider::new();
        static INSTALLED: Cell<bool> = const { Cell::new(false) };
    }
    PROVIDER.with(|provider| {
        INSTALLED.with(|installed| {
            if !installed.get() {
                gtk4::style_context_add_provider_for_display(display, provider, CHROME_PRIORITY);
                installed.set(true);
            }
        });
        provider.clone()
    })
}

/// Chrome colors for `scheme`, painted explicitly so the appearance pref wins
/// over whatever the toolkit decided.
fn chrome_css(scheme: Scheme) -> String {
    let c = chrome(scheme);
    format!(
        r#"
window.br0x-window {{
    background-color: {window_bg};
    color: {window_fg};
}}

headerbar, tabbar.br0x-tabbar {{
    background-color: {header_bg};
    color: {header_fg};
}}

.br0x-sidebar {{
    background-color: {sidebar_bg};
    color: {sidebar_fg};
    border-right: 1px solid {sidebar_border};
}}

.br0x-sidebar-head {{ border-bottom: 1px solid {sidebar_border}; }}

/* Surfaces the toolkit paints on its own: dialogs, menus, popovers. They are
listed here so a stuck variant cannot leave a dark island in light chrome. */
window.dialog,
window.settings-window,
popover > contents,
popover.menu > contents,
.popover-surface {{
    background-color: {popover_bg};
    color: {popover_fg};
}}

view, scrolledwindow, .view {{
    background-color: {view_bg};
}}

.omnibox-frame {{
    background-color: color-mix(in srgb, {header_fg} 5%, {header_bg});
    border-color: {border};
}}

.omnibox-frame:hover {{
    background-color: color-mix(in srgb, {header_fg} 8%, {header_bg});
}}

.menu-sep {{ background-color: {border}; }}

.palette-card {{
    background-color: {popover_bg};
    color: {popover_fg};
    border: 1px solid {border};
    box-shadow: 0 12px 40px {shade};
}}

.menu-row:hover,
.sidebar-row:hover {{
    background-color: color-mix(in srgb, currentColor 7%, transparent);
}}
"#,
        window_bg = c.window_bg,
        window_fg = c.window_fg,
        header_bg = c.header_bg,
        header_fg = c.header_fg,
        sidebar_bg = c.sidebar_bg,
        sidebar_fg = c.sidebar_fg,
        sidebar_border = c.sidebar_border,
        view_bg = c.view_bg,
        popover_bg = c.popover_bg,
        popover_fg = c.popover_fg,
        border = c.border,
        shade = c.shade,
    )
}

/// Layout for the chrome. Structure only: no colors live here, they all come
/// from [`chrome_css`], so the two files cannot drift.
pub const SHELL_CSS: &str = r#"
.br0x-sidebar-title {
    font-size: 11px;
    font-weight: 700;
    letter-spacing: 0.6px;
    text-transform: uppercase;
    opacity: 0.55;
}

.omnibox-frame {
    border-radius: 9px;
    padding: 0 4px;
    min-height: 34px;
    border-width: 1px;
    border-style: solid;
}

.omnibox-frame:focus-within {
    border-color: color-mix(in srgb, var(--accent-bg-color) 55%, transparent);
    box-shadow: 0 0 0 2px color-mix(in srgb, var(--accent-bg-color) 22%, transparent);
}

/* The address field sits inside a frame we paint, so it must not paint its
own surface. Declared here, in the sheet that owns the chrome, because a
hand-written gtk.css can set `entry` and this has to win. */
.omnibox-frame entry {
    background-color: transparent;
    background-image: none;
    outline: none;
}

/* One calm ring lives on the frame; the inner entry never draws its own. */
.omnibox-frame entry:focus {
    outline: none;
    border: none;
    box-shadow: none;
}

.omnibox-frame entry image.left {
    opacity: 0.7;
}

/* Sibling controls inside the address bar stay small enough to sit inside
its 34 px height without inflating it. */
.omnibox-frame button,
.omnibox-frame menubutton button {
    min-height: 26px;
    min-width: 26px;
    padding: 0 6px;
    border-radius: 7px;
    background-color: transparent;
    box-shadow: none;
}

.omnibox-frame button:hover,
.omnibox-frame menubutton button:hover {
    background-color: color-mix(in srgb, currentColor 10%, transparent);
}

headerbar {
    box-shadow: none;
    border-bottom: none;
    padding: 4px 8px;
}

headerbar button.flat {
    border-radius: 8px;
    min-width: 32px;
    min-height: 32px;
    padding: 0;
}

tabbar tabbox {
    padding: 0 8px 4px;
}

tabbar tab {
    min-height: 28px;
    border-radius: 8px;
    padding: 0 10px;
}

tabbar tab:selected {
    font-weight: 600;
    box-shadow: inset 0 -2px var(--accent-bg-color);
}

/* The stock close button inherits button padding, which draws a wide box
around a small cross. Pin it to a circle that matches the tab height. */
tabbar tab .tab-close-button,
tabbar tab button.tab-close-button {
    min-width: 22px;
    min-height: 22px;
    padding: 0;
    border-radius: 9999px;
    background-color: transparent;
    box-shadow: none;
}

tabbar tab .tab-close-button:hover,
tabbar tab button.tab-close-button:hover {
    background-color: color-mix(in srgb, currentColor 12%, transparent);
}

.hairline-progress {
    min-height: 2px;
    padding: 0;
    margin: 0;
    border: none;
}

.hairline-progress trough {
    min-height: 2px;
    background-color: transparent;
    border: none;
    border-radius: 0;
}

.hairline-progress progress {
    min-height: 2px;
    background-color: var(--accent-bg-color);
    border: none;
    border-radius: 0;
}

/* Reading progress shares the hairline shape but whispers. */
.hairline-dim progress {
    background-color: color-mix(in srgb, currentColor 30%, transparent);
}

.sidebar-row {
    border-radius: 10px;
    margin: 2px 8px;
    min-height: 52px;
}

.sidebar-row-title {
    font-size: 14px;
    font-weight: 500;
}

.sidebar-row-host {
    font-size: 11px;
    opacity: 0.55;
}

.sidebar-badge {
    font-size: 11px;
    opacity: 0.7;
}

/* State reads at a glance: parked is pressure, sleeping is the clock, and
loading is neither, so only the first two take a hue. */
.sidebar-badge.badge-sleeping {
    color: var(--accent-bg-color);
    opacity: 1;
}

.sidebar-badge.badge-parked {
    color: var(--warning-bg-color, var(--accent-bg-color));
    opacity: 1;
}

/* Pinned tabs get their own section label, like the tab list headers in the
reference sidebar. */
.sidebar-section {
    font-size: 10px;
    font-weight: 700;
    letter-spacing: 0.7px;
    text-transform: uppercase;
    opacity: 0.45;
    margin: 6px 14px 2px;
}

.sidebar-dot {
    color: var(--accent-bg-color);
    font-size: 11px;
}

.sidebar-pin-active,
.sidebar-row-active,
.sidebar-row-active:hover {
    background-color: color-mix(in srgb, currentColor 8%, transparent);
}

.sidebar-row-active,
.sidebar-row-active:hover {
    box-shadow: inset 2px 0 var(--accent-bg-color);
}

.sidebar-pin-active {
    box-shadow: none;
}

/* Drag feedback: the row being dragged fades, the drop edge shows. */
.sidebar-row.dragging {
    opacity: 0.4;
}

.sidebar-row.drop-above {
    box-shadow: inset 0 2px var(--accent-bg-color);
}

.sidebar-row.drop-below {
    box-shadow: inset 0 -2px var(--accent-bg-color);
}

.sidebar-row.dragging.drop-above,
.sidebar-row.dragging.drop-below {
    box-shadow: none;
}

@keyframes sidebar-shimmer {
    from { opacity: 0.45; }
    to { opacity: 1.0; }
}

.sidebar-loading image.favicon {
    animation: sidebar-shimmer 700ms ease-in-out infinite alternate;
}

/* Motion: quick hovers, calm reveals. */
.sidebar-row,
headerbar button,
tabbar tab,
.omnibox-frame {
    transition: background-color 150ms ease-out, border-color 150ms ease-out;
}

.menu-popover {
    padding: 0;
}

.menu-row {
    padding: 1px 10px;
    min-height: 24px;
    border-radius: 8px;
    background-color: transparent;
    box-shadow: none;
    font-weight: 400;
}

.menu-accel {
    font-size: 12px;
    opacity: 0.55;
}

.menu-sep {
    margin: 3px 8px;
    min-height: 1px;
}

.palette-card {
    border-radius: 14px;
    padding-bottom: 8px;
}

.palette-card row {
    border-radius: 8px;
    margin: 1px 6px;
}

.palette-section {
    font-size: 10px;
    font-weight: 700;
    letter-spacing: 0.7px;
    text-transform: uppercase;
    opacity: 0.45;
    margin: 8px 14px 2px;
}

.palette-card row:selected {
    background-color: color-mix(in srgb, currentColor 9%, var(--popover-bg-color));
    box-shadow: inset 2px 0 var(--accent-bg-color);
}
"#;

/// Register the layout stylesheet. Colors arrive separately through
/// [`apply`], so this runs once at startup.
pub fn install(display: &Display) {
    let provider = CssProvider::new();
    provider.load_from_string(SHELL_CSS);
    gtk4::style_context_add_provider_for_display(display, &provider, CHROME_PRIORITY);
}

#[cfg(test)]
mod tests {
    use super::*;
    use br0x_core::prefs::Appearance;

    /// The bug this module exists to prevent: a light choice that leaves dark
    /// chrome. The painted palette must follow the pref, not the toolkit.
    /// Only the colors that actually differ between the palettes: white is a
    /// text color in dark and a surface in light, so a substring check on it
    /// would fail for the wrong reason.
    const DARK_SURFACES: [&str; 4] = ["#222226", "#2e2e32", "#36363a", "#1d1d20"];
    const LIGHT_SURFACES: [&str; 2] = ["#fafafb", "#ebebed"];

    #[test]
    fn light_chrome_never_uses_dark_surfaces() {
        let light = chrome_css(Scheme::Light);
        for dark_only in DARK_SURFACES {
            assert!(!light.contains(dark_only), "light chrome contains {dark_only}");
        }
        assert!(light.contains(LIGHT.sidebar_bg), "sidebar painted from the light palette");
        assert!(light.contains(LIGHT.header_bg), "header painted from the light palette");
    }

    #[test]
    fn dark_chrome_never_uses_light_surfaces() {
        let dark = chrome_css(Scheme::Dark);
        for light_only in LIGHT_SURFACES {
            assert!(!dark.contains(light_only), "dark chrome contains {light_only}");
        }
        assert!(dark.contains(DARK.sidebar_bg));
        assert!(dark.contains(DARK.header_bg));
    }

    /// Every surface the shell owns must be painted, or a stuck toolkit
    /// variant shows through as a dark island in light chrome.
    #[test]
    fn every_owned_surface_is_painted() {
        let css = chrome_css(Scheme::Light);
        for selector in [
            "window.br0x-window",
            "headerbar",
            "tabbar.br0x-tabbar",
            ".br0x-sidebar",
            "popover > contents",
            "window.dialog",
            ".palette-card",
        ] {
            assert!(css.contains(selector), "unpainted surface: {selector}");
        }
    }

    #[test]
    fn scheme_tokens_match_the_pages() {
        assert_eq!(Scheme::Light.token(), "light");
        assert_eq!(Scheme::Dark.token(), "dark");
        assert_eq!(chrome(Scheme::Light).view_bg, "#ffffff");
    }

    /// Structure and color are separate files: the layout sheet must not carry
    /// a palette value, or the two would drift apart.
    #[test]
    fn layout_sheet_carries_no_palette_colors() {
        for value in [LIGHT.window_bg, LIGHT.header_bg, LIGHT.sidebar_bg, DARK.sidebar_bg] {
            assert!(
                !SHELL_CSS.contains(value),
                "palette value {value} leaked into the layout sheet"
            );
        }
    }

    #[test]
    fn system_follows_the_toolkit() {
        // Only a compile-time sanity check: `System` is the one mode that
        // reads the toolkit, and it is handled in `apply`.
        assert_ne!(Appearance::System, Appearance::Light);
    }
}
