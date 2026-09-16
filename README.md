# br0x

br0x is a fast browser for Linux. It uses Rust, GTK4, and WebKitGTK. It targets low memory use and quick startup.

## Goals

- Use 900 MB to 1.2 GB RAM with 9 tabs open. Firefox uses about 2.63 GB in the same load.
- Start in under 300 ms on a cold cache.
- Keep the UI native to GNOME.
- Stay offline first. No account. No telemetry.

## Stack

- Rust 2024 edition
- GTK4 plus libadwaita for the shell
- WebKitGTK 6.0 for page render
- Flatpak for release builds, AUR for Arch installs

## Linux v1 scope

- Tabs, history, and bookmarks stored locally in SQLite
- Ad and tracker block using a local filter list
- Keyboard first navigation and command palette
- GNOME keyring for passwords
- No sync, no extensions, and no Windows or macOS port in v1

## Quick start

Install deps on Arch:

```sh
sudo pacman -S base-devel pkg-config gtk4 libadwaita webkitgtk-6.0 rustup
rustup default stable
```

Run the shell:

```sh
cargo run -p br0x-shell-gtk
```

## Project layout

- `crates/br0x-core`: profile, history, bookmarks, block list. No GTK code here.
- `crates/br0x-shell-gtk`: GTK4 window, tabs, address bar. Calls `br0x-core`.
- `.flatpak/`: Flatpak manifest for release builds.

## Contributing

Read `CONTRIBUTING.md`, then open a small PR. Maintainers review within 7 days.
