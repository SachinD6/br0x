//! Shell appearance: one scheme decision, one stylesheet.
//!
//! Two rules keep the chrome coherent, and both are enforceable by tests:
//!
//! 1. **Every chrome color comes from a libadwaita token** (`@window_bg_color`,
//!    `var(--sidebar-bg-color)`, `@borders`, ...). Tokens are defined per theme
//!    variant by the same stylesheet that paints the toolkit's own widgets, so
//!    the chrome cannot disagree with the variant. Hardcoded hex values here
//!    would reintroduce exactly the mismatch this module exists to prevent.
//! 2. **The appearance pref decides the variant, and the variant decides
//!    everything else.** `scheme` is the single read of that decision; both the
//!    toolkit switch and the internal pages consume its result, so a page can
//!    never render light inside a dark shell.

/// Light or dark, as actually rendered right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    Light,
    Dark,
}

impl Scheme {
    /// CSS `color-scheme` token for internal pages, so their `light-dark()`
    /// resolves the same way the chrome did.
    pub fn token(self) -> &'static str {
        match self {
            Scheme::Light => "light",
            Scheme::Dark => "dark",
        }
    }
}

/// Ask for a variant and report what the toolkit actually did.
///
/// libadwaita ignores the request when `GTK_THEME` is set or a user
/// `gtk.css` forces a variant, so the answer comes from the StyleManager,
/// not the request. Callers surface a mismatch instead of failing quietly.
pub fn apply(appearance: br0x_core::prefs::Appearance) -> Scheme {
    use br0x_core::prefs::Appearance;
    let manager = adw::StyleManager::default();
    manager.set_color_scheme(match appearance {
        Appearance::System => adw::ColorScheme::Default,
        Appearance::Light => adw::ColorScheme::ForceLight,
        Appearance::Dark => adw::ColorScheme::ForceDark,
    });
    current()
}

fn scheme_from(is_dark: bool) -> Scheme {
    if is_dark { Scheme::Dark } else { Scheme::Light }
}

/// What the chrome is rendering right now.
pub fn current() -> Scheme {
    scheme_from(adw::StyleManager::default().is_dark())
}

/// What the pref asked for, with `System` resolved against the OS. Compared
/// against [`current`] to tell a real switch from a swallowed one.
pub fn wanted(appearance: br0x_core::prefs::Appearance) -> Scheme {
    use br0x_core::prefs::Appearance;
    match appearance {
        Appearance::Light => Scheme::Light,
        Appearance::Dark => Scheme::Dark,
        Appearance::System => current(),
    }
}

/// The shell stylesheet.
///
/// Layout note: the chrome is two rows, one 46 px header bar and one 34 px
/// tab strip, both painted with the headerbar surface so they read as a
/// single bar. Radii follow libadwaita's scale (9 px for inputs and buttons).
pub const SHELL_CSS: &str = r#"
/* ---- window and sidebars ---------------------------------------------- */

window.br0x-window {
    background-color: var(--window-bg-color);
    color: var(--window-fg-color);
}

/* The sidebar is chrome, not content: give it the toolkit's sidebar surface
and a hairline against the page so the split is visible in both variants. */
.br0x-sidebar {
    background-color: var(--sidebar-bg-color);
    color: var(--sidebar-fg-color);
    border-right: 1px solid var(--sidebar-border-color, @borders);
}

.br0x-sidebar-head {
    border-bottom: 1px solid var(--sidebar-border-color, @borders);
}

.br0x-sidebar-title {
    font-size: 11px;
    font-weight: 700;
    letter-spacing: 0.6px;
    text-transform: uppercase;
    opacity: 0.55;
}

/* ---- address bar ------------------------------------------------------ */

.omnibox-frame {
    border-radius: 9px;
    padding: 0 4px;
    min-height: 34px;
    /* A tint of the bar it sits on, so the field reads as an input in both
    variants instead of vanishing into an equally white header. */
    background-color: color-mix(in srgb, currentColor 5%, var(--headerbar-bg-color));
    border: 1px solid var(--border-color, @borders);
}

.omnibox-frame:hover {
    background-color: color-mix(in srgb, currentColor 8%, var(--headerbar-bg-color));
}

.omnibox-frame:focus-within {
    border-color: color-mix(in srgb, var(--accent-bg-color) 55%, transparent);
    box-shadow: 0 0 0 2px color-mix(in srgb, var(--accent-bg-color) 22%, transparent);
}

.omnibox-frame entry {
    background: transparent;
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
    background: transparent;
    box-shadow: none;
}

.omnibox-frame button:hover,
.omnibox-frame menubutton button:hover {
    background: color-mix(in srgb, currentColor 10%, transparent);
}

/* ---- header bar and tabs --------------------------------------------- */

headerbar {
    background-color: var(--headerbar-bg-color);
    color: var(--headerbar-fg-color);
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

/* The tab strip shares the headerbar surface: one bar, two rows. */
tabbar.br0x-tabbar {
    background-color: var(--headerbar-bg-color);
    color: var(--headerbar-fg-color);
    border-bottom: 1px solid var(--border-color, @borders);
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

/* ---- progress hairlines ---------------------------------------------- */

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
    background-color: var(--accent-bg-color);
    border: none;
    border-radius: 0;
}

/* Reading progress shares the hairline shape but whispers. */
.hairline-dim progress {
    background-color: color-mix(in srgb, currentColor 30%, transparent);
}

/* ---- sidebar rows ----------------------------------------------------- */

.sidebar-row {
    border-radius: 8px;
    margin: 1px 6px;
    min-height: 34px;
}

.sidebar-row:hover {
    background-color: color-mix(in srgb, currentColor 7%, transparent);
}

/* Selection is a neutral fill plus an accent edge. Tinting the whole row
with the accent turns muddy the moment the system accent is warm, and the
edge reads as a selected state in every accent. */
.sidebar-row-active,
.sidebar-row-active:hover {
    background-color: color-mix(in srgb, currentColor 9%, var(--sidebar-bg-color));
    box-shadow: inset 2px 0 var(--accent-bg-color);
}

.sidebar-pin-active {
    background-color: color-mix(in srgb, currentColor 9%, var(--sidebar-bg-color));
    border-radius: 8px;
}

.sidebar-badge {
    font-size: 11px;
    opacity: 0.7;
}

.sidebar-dot {
    color: var(--accent-bg-color);
    font-size: 11px;
}

@keyframes sidebar-shimmer {
    from { opacity: 0.45; }
    to { opacity: 1.0; }
}

.sidebar-loading image.favicon {
    animation: sidebar-shimmer 700ms ease-in-out infinite alternate;
}

/* ---- motion ----------------------------------------------------------- */

.sidebar-row,
headerbar button,
tabbar tab,
.omnibox-frame {
    transition: background-color 150ms ease-out, border-color 150ms ease-out;
}

/* ---- menus ------------------------------------------------------------ */

.menu-popover {
    padding: 0;
}

.menu-row {
    padding: 7px 10px;
    border-radius: 8px;
    background: transparent;
    box-shadow: none;
    font-weight: 400;
}

.menu-row:hover {
    background-color: color-mix(in srgb, currentColor 9%, transparent);
}

.menu-accel {
    font-size: 12px;
    opacity: 0.55;
}

.menu-sep {
    margin: 5px 8px;
    background-color: var(--border-color, @borders);
    min-height: 1px;
}

/* ---- palette ---------------------------------------------------------- */

.palette-card {
    background-color: var(--popover-bg-color);
    color: var(--popover-fg-color);
    border-radius: 14px;
    border: 1px solid var(--border-color, @borders);
    box-shadow: 0 12px 40px var(--shade-color);
    padding-bottom: 8px;
}

.palette-card row {
    border-radius: 8px;
    margin: 1px 6px;
}

.palette-card row:selected {
    background-color: color-mix(in srgb, currentColor 9%, var(--popover-bg-color));
    box-shadow: inset 2px 0 var(--accent-bg-color);
}
"#;

/// Register the shell stylesheet for `display`.
pub fn install(display: &gtk4::gdk::Display) {
    let provider = gtk4::CssProvider::new();
    provider.load_from_string(SHELL_CSS);
    gtk4::style_context_add_provider_for_display(
        display,
        &provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rule 1: no raw colors. A hex or `rgb()` in the shell stylesheet means a
    /// surface that cannot follow the theme variant.
    #[test]
    fn shell_css_uses_only_theme_tokens() {
        let mut violations = Vec::new();
        for line in SHELL_CSS.lines() {
            let code = line.split("/*").next().unwrap_or("");
            let lower = code.to_ascii_lowercase();
            if lower.contains("background-color:")
                || lower.contains("background:")
                || lower.contains("color:")
                || lower.contains("border-color:")
            {
                let has_hex = code.contains('#')
                    && !code.trim_start().starts_with("/*")
                    && code.split(':').nth(1).is_some_and(|v| v.contains('#'));
                let has_rgb = code.contains("rgb(") || code.contains("rgba(");
                if has_hex || has_rgb {
                    violations.push(line.trim().to_string());
                }
            }
        }
        assert!(violations.is_empty(), "raw colors in the shell stylesheet: {violations:#?}");
    }

    /// Rule 2: the scheme answer follows the variant, never the request.
    #[test]
    fn scheme_reflects_the_variant_not_the_request() {
        assert_eq!(scheme_from(true), Scheme::Dark);
        assert_eq!(scheme_from(false), Scheme::Light);
        assert_eq!(Scheme::Light.token(), "light");
        assert_eq!(Scheme::Dark.token(), "dark");
    }

    /// The sidebar surface must be styled unconditionally, so System mode
    /// cannot leave it transparent over a dark window.
    #[test]
    fn sidebar_is_painted_in_every_scheme() {
        let css = SHELL_CSS;
        let block = css.split(".br0x-sidebar {").nth(1).expect("sidebar rule present");
        assert!(block.contains("background-color: var(--sidebar-bg-color)"));
        assert!(block.contains("border-right"));
    }
}
