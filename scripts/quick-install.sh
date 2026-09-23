#!/bin/sh
# One-step br0x installer. Detects your distro, installs build tools,
# fetches the source, builds, and registers the app in your launcher.
# Usage: curl -fsSL https://raw.githubusercontent.com/SachinD6/br0x/main/scripts/quick-install.sh | sh
#    or: ./scripts/quick-install.sh [--prefix ~/.local]
set -eu
PREFIX="${HOME:-/tmp}/.local"
while [ $# -gt 0 ]; do
  case "$1" in
    --prefix=*) PREFIX="${1#--prefix=}"; shift;;
    --prefix) PREFIX="${2:-$PREFIX}"; shift 2;;
    *) shift;;
  esac
done

say() { printf 'br0x: %s\n' "$1"; }
have() { command -v "$1" >/dev/null 2>&1; }

SUDO=""
if [ "$(id -u)" -ne 0 ]; then
  if ! have sudo; then
    echo "br0x: need root or sudo to install system packages" >&2
    exit 1
  fi
  SUDO="sudo"
fi

# Find or fetch the source.
if [ -f ./Cargo.toml ] && [ -f ./scripts/install.sh ]; then
  SRC="."
else
  SRC="${BR0X_SRC:-$HOME/.local/src/br0x}"
  if [ -f "$SRC/Cargo.toml" ]; then
    say "updating $SRC"
    if have git; then
      git -C "$SRC" pull --ff-only 2>/dev/null || true
    fi
  else
    say "fetching source into $SRC"
    ensure_git() {
      if have git; then return 0; fi
      if have pacman; then $SUDO pacman -Sy --noconfirm --needed git
      elif have dnf; then $SUDO dnf install -y git
      elif have apt-get; then $SUDO apt-get update && $SUDO apt-get install -y git
      else echo "br0x: install git, then rerun" >&2; exit 1; fi
    }
    ensure_git
    git clone https://github.com/SachinD6/br0x.git "$SRC"
  fi
fi

# Ubuntu and Debian ship a GTK older than this codebase needs,
# so they install the Flatpak build instead of compiling.
if have apt-get && ! have pacman && ! have dnf; then
  say "Ubuntu/Debian detected: installing the Flatpak build"
  export DEBIAN_FRONTEND=noninteractive
  $SUDO apt-get update && $SUDO apt-get install -y flatpak flatpak-builder git
  flatpak remote-add --user --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo
  (cd "$SRC" && flatpak-builder --user --install --force-clean build-dir .flatpak/org.br0x.Browser.yml)
  say "done: find br0x in your app grid"
  exit 0
fi

# Native build: system GTK stack plus Rust.
if have pacman; then
  say "Arch detected: installing build tools"
  $SUDO pacman -Sy --noconfirm --needed base-devel pkgconf gtk4 libadwaita webkitgtk-6.0 \
    gst-plugins-good gst-libav gst-plugins-bad git curl
elif have dnf; then
  say "Fedora detected: installing build tools"
  $SUDO dnf install -y gcc pkgconf gtk4-devel libadwaita-devel webkitgtk6.0-devel git curl
else
  echo "br0x: no supported package manager found (need pacman, dnf, or apt-get)." >&2
  echo "br0x: install gtk4, libadwaita, webkitgtk-6.0 and Rust, then run $SRC/scripts/install.sh" >&2
  exit 1
fi

if ! have cargo; then
  say "installing Rust"
  curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal
  export PATH="$HOME/.cargo/bin:$PATH"
fi

say "building and installing to $PREFIX"
sh "$SRC/scripts/install.sh" --prefix "$PREFIX"
say "done: run br0x or find it in your app grid"
