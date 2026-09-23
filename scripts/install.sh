#!/bin/sh
# Install br0x from a source checkout or a release tarball.
# From source: ./scripts/install.sh [--prefix ~/.local] [--uninstall]
# From a tarball: ./install.sh [--prefix ~/.local] [--uninstall]
set -eu
PREFIX="${HOME:-/tmp}/.local"
UNINSTALL=0
while [ $# -gt 0 ]; do
  case "$1" in
    --prefix=*) PREFIX="${1#--prefix=}"; shift;;
    --prefix) PREFIX="${2:-$PREFIX}"; shift 2;;
    --uninstall) UNINSTALL=1; shift;;
    *) shift;;
  esac
done
HERE="$(dirname "$0")"
# Source checkout lays out as scripts/install.sh above the root;
# a release tarball lays out as install.sh beside the payload.
if [ -f "$HERE/../Cargo.toml" ]; then
  ROOT="$HERE/.."
else
  ROOT="$HERE"
fi

if [ "$UNINSTALL" -eq 1 ]; then
  rm -f "$PREFIX/bin/br0x" \
    "$PREFIX/share/applications/org.br0x.Browser.desktop" \
    "$PREFIX/share/metainfo/org.br0x.Browser.metainfo.xml" \
    "$PREFIX/share/icons/hicolor/scalable/apps/org.br0x.Browser.svg"
  echo "removed br0x from $PREFIX"
  exit 0
fi

if [ -f "$ROOT/Cargo.toml" ]; then
  # Source checkout: build, so the binary matches this machine.
  cargo build --release --locked -p br0x-shell-gtk
  BIN="$ROOT/target/release/br0x"
  META="$ROOT/packaging"
elif [ -f "$ROOT/br0x" ]; then
  # Release tarball: install the bundled binary (built for Arch).
  BIN="$ROOT/br0x"
  META="$ROOT"
else
  echo "error: no source checkout (Cargo.toml) and no bundled binary (br0x) found" >&2
  exit 1
fi
install -Dm755 "$BIN" "$PREFIX/bin/br0x"
install -Dm644 "$META/org.br0x.Browser.desktop" "$PREFIX/share/applications/org.br0x.Browser.desktop"
install -Dm644 "$META/org.br0x.Browser.metainfo.xml" "$PREFIX/share/metainfo/org.br0x.Browser.metainfo.xml"
install -Dm644 "$META/org.br0x.Browser.svg" "$PREFIX/share/icons/hicolor/scalable/apps/org.br0x.Browser.svg"
update-desktop-database "$PREFIX/share/applications" 2>/dev/null || true
gtk-update-icon-cache -f -t "$PREFIX/share/icons/hicolor" 2>/dev/null || true
echo "installed br0x to $PREFIX/bin/br0x"
