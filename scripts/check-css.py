#!/usr/bin/env python3
"""Syntax check for GTK CSS and HTML style blocks embedded in main.rs.

Extracts every raw string literal, splits HTML <style> blocks from the bare
GTK stylesheet, and verifies balanced braces/parens plus a few GTK specific
rules that silently fail at runtime. Exits nonzero on any finding, so it can
gate a commit.
"""

import re
import sys

SRC = sys.argv[1:] or [
    "crates/br0x-shell-gtk/src/main.rs",
    "crates/br0x-shell-gtk/src/pages.rs",
]

# Properties GTK4 CSS does not support. Using them fails silently: the widget
# keeps looking wrong and the stylesheet reports no error.
UNSUPPORTED = ["visibility:", "float:", "z-index:", "position:", "overflow:", "cursor:"]

# libadwaita/GTK named colors available in GTK4 stylesheets.
KNOWN_COLORS = {
    "accent_bg_color", "accent_color", "accent_fg_color", "window_bg_color",
    "window_fg_color", "view_bg_color", "view_fg_color", "headerbar_bg_color",
    "headerbar_fg_color", "card_bg_color", "card_fg_color", "sidebar_bg_color",
    "sidebar_fg_color", "popover_bg_color", "popover_fg_color", "shade_color",
    "scrollbar_outline_color", "destructive_bg_color", "destructive_color",
    "success_bg_color", "success_color", "warning_bg_color", "warning_color",
    "error_bg_color", "error_color", "dialog_bg_color", "dialog_fg_color",
    "thumbnail_bg_color", "thumbnail_fg_color", "borders", "insensitive_fg_color",
}


def extract(src):
    """Yield (label, css) for every stylesheet the shell ships."""
    raw = re.findall(r'r#"(.*?)"#,', src, re.S) + re.findall(r'r##"(.*?)"##,', src, re.S)
    out = []
    for block in raw:
        for style in re.findall(r"<style[^>]*>(.*?)</style>", block, re.S):
            out.append(("html-style", style))
    for block in raw:
        if "<" not in block and ("{" in block) and re.search(r"^[a-z]|\.[a-z-]+ \{", block, re.M):
            out.append(("gtk-css", block))
    return out


def check_balance(label, css, problems):
    depth = 0
    paren = 0
    line = 1
    for ch in css:
        if ch == "\n":
            line += 1
        elif ch == "{":
            depth += 1
        elif ch == "}":
            depth -= 1
            if depth < 0:
                problems.append(f"{label}:{line}: closing brace with no opener")
                depth = 0
        elif ch == "(":
            paren += 1
        elif ch == ")":
            paren -= 1
            if paren < 0:
                problems.append(f"{label}:{line}: closing paren with no opener")
                paren = 0
    if depth != 0:
        problems.append(f"{label}: unbalanced braces (depth {depth})")
    if paren != 0:
        problems.append(f"{label}: unbalanced parens (depth {paren})")


def check_gtk(label, css, problems):
    for name in UNSUPPORTED:
        for m in re.finditer(re.escape(name), css):
            line = css[: m.start()].count("\n") + 1
            problems.append(f"{label}:{line}: GTK CSS has no {name.strip(':')} property")
    for m in re.finditer(r"@([a-z_0-9]+)", css):
        name = m.group(1)
        if name in ("media", "keyframes", "import", "define-color", "theme"):
            continue
        if name not in KNOWN_COLORS and not name.endswith("_color"):
            line = css[: m.start()].count("\n") + 1
            problems.append(f"{label}:{line}: unknown @{name} color")


def theme_css():
    """The shell stylesheet now lives in the theme module."""
    path = "crates/br0x-shell-gtk/src/theme.rs"
    src = open(path).read()
    block = re.search(r"SHELL_CSS: &str = r#\"(.*?)\"#;", src, re.S)
    return block.group(1) if block else None


def main():
    problems = []
    seen = 0
    for path in SRC:
        src = open(path).read()
        seen += len(extract(src))
        for label, css in extract(src):
            check_balance(f"{path}:{label}", css, problems)
            # Only GTK stylesheets are parsed by GTK. HTML blocks are rendered
            # by WebKit, where position/overflow/cursor are all legal.
            if label == "gtk-css":
                check_gtk(f"{path}:{label}", css, problems)

    css = theme_css()
    if css is None:
        problems.append("theme: SHELL_CSS not found in theme.rs")
    else:
        check_balance("theme", css, problems)
        check_gtk("theme", css, problems)
        # Rule: chrome colors come from theme tokens, never raw values, or the
        # surface cannot follow the light/dark variant.
        for line in css.splitlines():
            code = line.split("/*")[0]
            stripped = code.strip()
            if not stripped or ":" not in stripped:
                continue
            prop, _, value = stripped.partition(":")
            if prop.strip() not in ("background-color", "background", "color", "border-color", "border"):
                continue
            if "#" in value or "rgb(" in value or "rgba(" in value:
                problems.append(f"theme: raw color in '{stripped}'")
        # The sidebar must be painted by whichever sheet owns the palette, or
        # a stuck toolkit variant shows through as a dark column.
        src = open("crates/br0x-shell-gtk/src/theme.rs").read()
        if ".br0x-sidebar {" not in src or "background-color: {sidebar_bg}" not in src:
            problems.append("theme: sidebar surface is not painted")

    for p in problems:
        print(p)
    print(f"checked {seen} embedded stylesheets + theme, {len(problems)} problems")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
