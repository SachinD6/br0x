# Verifying UI changes

The shell is GTK4 + WebKitGTK, so a change to the chrome is not proven by a
compiler. This is the loop that shows real pixels on a machine with no display
server, and it is how every chrome change in this repository was checked.

## The loop

```sh
# 1. Build the debug binary (the scripts use target/debug/br0x).
cargo build -p br0x-shell-gtk

# 2. A fresh X server, no root needed. Fetch the packages once and extract
#    them into a prefix; the scripts read $XVFB_ROOT (default /tmp/xvfb/root).
apt-get download xvfb xserver-common libxfont2 xkb-data x11-apps xdotool libxdo3
for d in *.deb; do dpkg -x "$d" "$XVFB_ROOT"; done

# 3. Capture the chrome in either scheme.
scripts/ui-shot.sh /tmp/light.png Light
scripts/ui-shot.sh /tmp/dark.png Dark

# 4. Capture an open surface. A popover draws on its own X window, so a
#    window snapshot cannot see it; these open it first and paint its content.
BR0X_SHOT_OPEN=menu scripts/ui-shot.sh /tmp/menu.png Light
BR0X_SHOT_OPEN=palette scripts/ui-shot.sh /tmp/palette.png Dark

# 5. Drive the real app and capture the screen, menus included.
scripts/interact.sh start Light
scripts/interact.sh click 1045 28 /tmp/menu-open.png   # x y from BR0X_SHOT_DUMP
```

## Levers inside the binary

- `BR0X_SHOT=<path>` renders the window to a PNG after first paint and exits.
  `BR0X_SHOT_DELAY_MS` tunes how long the window settles first.
- `BR0X_SHOT_OPEN=menu|palette` opens that surface and captures it.
- `BR0X_SHOT_HOLD=<seconds>` opens the surface and stays alive so an external
  capture can grab the popup's own X window.
- `BR0X_SHOT_DUMP=1` prints the bounds of the header controls, so interaction
  scripts click real coordinates instead of guessing them.

## Reading the result

- `scripts/px.py <png> --box x0 y0 x1 y1` lists the most common colors in a
  region, which turns "looks wrong" into a hex value to compare against a theme
  token.
- `scripts/px.py <png> --row y x0 x1` prints a horizontal scan, which is how
  row heights, paddings, and hit-target widths get measured.
- `scripts/check-css.py` fails if the shell stylesheet uses a raw color instead
  of a libadwaita token, or a property GTK does not support.

## Rules the code enforces

- `theme.rs` paints two sheets. `chrome_css(scheme)` carries the palette and is
  loaded at `CHROME_PRIORITY` (user + 1) so a hand-written `gtk.css` cannot leave
  the chrome dark under a light setting; `SHELL_CSS` carries layout only, and a
  test fails if a palette value leaks into it.
- One scheme decision drives the toolkit variant, the painted chrome, and the
  internal pages. `theme::apply` records the answer, and `theme::applied` is
  what the page builders read.
- The palette values are libadwaita's own light and dark token values, so the
  painted chrome matches the platform greys. Read them from the shipped
  stylesheet with the `@define-color` search in this file's history.
- A popover taller than the window is silently not shown at all, so menus that
  can grow go inside a `ScrolledWindow```. Adjust the row height before the cap:
  the full menu is 26 rows at 26 px plus separators.
- The `background` shorthand does nothing in GTK CSS. Use `background-color`,
  which is how the address field kept a user theme's dark fill.
