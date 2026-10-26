# Maintainer: sakuyma <201369162+sakuyma@users.noreply.github.com>
#
# This PKGBUILD builds from the local git checkout. That is intentional: the
# upstream repository is private, so a public tarball URL would require
# credentials for every build. Commit before running makepkg; it packages HEAD,
# not the working tree.
#
# For a public release, replace the source below with, for example:
#   source=("$pkgname-$pkgver.tar.gz::https://github.com/sakuyma/sink/archive/refs/tags/v$pkgver.tar.gz")
# and replace sha256sums with the archive's checksum.

pkgname=sink
pkgver=0.1.0
pkgrel=1
pkgdesc="Minimal native PipeWire audio control panel for Wayland"
arch=('x86_64' 'aarch64')
url="https://github.com/sakuyma/sink"
license=('MIT')
depends=('gtk4' 'pipewire' 'wireplumber')
makedepends=('cargo' 'rust' 'pkgconf' 'clang' 'glib2' 'git')
source=("git+file://$startdir")
sha256sums=('SKIP')

build() {
  cd "$srcdir/$pkgname"
  cargo build --release --locked
}

check() {
  cd "$srcdir/$pkgname"
  cargo test --locked
}

package() {
  cd "$srcdir/$pkgname"

  install -Dm755 target/release/sink "$pkgdir/usr/bin/sink"
  install -Dm644 dev.saku.Sink.desktop \
    "$pkgdir/usr/share/applications/dev.saku.Sink.desktop"
  install -Dm644 icons/scalable/apps/sink.svg \
    "$pkgdir/usr/share/icons/hicolor/scalable/apps/sink.svg"
  install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
}
