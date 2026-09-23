# Maintainer: br0x contributors <https://github.com/Sachind6/br0x>
pkgname=br0x
pkgver=0.1.0
pkgrel=1
pkgdesc='A Linux browser that parks idle tabs (Rust, GTK4, WebKitGTK)'
arch=('x86_64')
url='https://github.com/Sachind6/br0x'
license=('MIT')
depends=('gtk4' 'libadwaita' 'webkitgtk-6.0' 'hicolor-icon-theme')
makedepends=('rust' 'cargo')
source=("$pkgname-$pkgver.tar.gz::https://github.com/Sachind6/br0x/archive/refs/tags/v$pkgver.tar.gz")
# Fill with `updpkgsums` after tagging v0.1.0.
sha256sums=('SKIP')

build() {
  cd "$pkgname-$pkgver"
  cargo build --release --locked -p br0x-shell-gtk
}

package() {
  cd "$pkgname-$pkgver"
  install -Dm755 target/release/br0x "$pkgdir/usr/bin/br0x"
  install -Dm644 packaging/org.br0x.Browser.desktop "$pkgdir/usr/share/applications/org.br0x.Browser.desktop"
  install -Dm644 packaging/org.br0x.Browser.metainfo.xml "$pkgdir/usr/share/metainfo/org.br0x.Browser.metainfo.xml"
  install -Dm644 packaging/org.br0x.Browser.svg "$pkgdir/usr/share/icons/hicolor/scalable/apps/org.br0x.Browser.svg"
  install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
}
