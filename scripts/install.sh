#!/bin/sh
# Install br0x from source or a release tarball.
# Usage: ./scripts/install.sh [--prefix ~/.local] [--uninstall]
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
ROOT="$(dirname "$0")/.."

if [ "$UNINSTALL" -eq 1 ]; then
  rm -f "$PREFIX/bin/br0x" \
    "$PREFIX/share/applications/org.br0x.Browser.desktop" \
    "$PREFIX/share/metainfo/org.br0x.Browser.metainfo.xml" \
    "$PREFIX/share/icons/hicolor/scalable/apps/org.br0x.Browser.svg"
  echo "removed br0x from $PREFIX"
  exit 0
fi

cargo build --release --locked -p br0x-shell-gtk
install -Dm755 "$ROOT/target/release/br0x" "$PREFIX/bin/br0x"
install -Dm644 "$ROOT/packaging/org.br0x.Browser.desktop" "$PREFIX/share/applications/org.br0x.Browser.desktop"
install -Dm644 "$ROOT/packaging/org.br0x.Browser.metainfo.xml" "$PREFIX/share/metainfo/org.br0x.Browser.metainfo.xml"
install -Dm644 "$ROOT/packaging/org.br0x.Browser.svg" "$PREFIX/share/icons/hicolor/scalable/apps/org.br0x.Browser.svg"
update-desktop-database "$PREFIX/share/applications" 2>/dev/null || true
gtk-update-icon-cache -f -t "$PREFIX/share/icons/hicolor" 2>/dev/null || true
echo "installed br0x to $PREFIX/bin/br0x"
