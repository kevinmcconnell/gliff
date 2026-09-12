# Maintainer: Kevin McConnell
pkgname=haver
pkgver=0.1.0
pkgrel=1
pkgdesc="Hyprland remote desktop over SSH (VA-API, 4:4:4)"
arch=('x86_64')
url="https://github.com/kevin/haver"
license=('MIT')
depends=('wayland' 'libxkbcommon' 'libdrm' 'mesa' 'gtk4' 'libadwaita' 'libva')
makedepends=('rust' 'cargo' 'clang' 'pkgconf')
optdepends=('libva-utils: vainfo for debugging'
            'intel-media-driver: VA-API on Intel'
            'libva-mesa-driver: VA-API on AMD/older Intel')
source=()

build() {
    cd "$startdir"
    cargo build --release --locked
}

package() {
    cd "$startdir"
    install -Dm755 target/release/haver-server "$pkgdir/usr/bin/haver-server"
    install -Dm755 target/release/haver-client "$pkgdir/usr/bin/haver-client"
    install -Dm755 target/release/haver-probe  "$pkgdir/usr/bin/haver-probe"
    install -Dm644 README.md "$pkgdir/usr/share/doc/$pkgname/README.md"
    install -Dm644 docs/hardware-quirks.md "$pkgdir/usr/share/doc/$pkgname/hardware-quirks.md"
}
