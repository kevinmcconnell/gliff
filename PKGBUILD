# Maintainer: Kevin McConnell
pkgname=gliff
pkgver=0.1.0
pkgrel=1
pkgdesc="Hyprland remote desktop over SSH (Vulkan Video, 4:4:4)"
arch=('x86_64')
url="https://github.com/kevinmcconnell/gliff"
license=('MIT')
depends=('wayland' 'libxkbcommon' 'libdrm' 'mesa' 'gtk4' 'libadwaita' 'vulkan-icd-loader')
makedepends=('rust' 'cargo' 'pkgconf')
optdepends=('vulkan-radeon: Vulkan on AMD'
            'vulkan-intel: Vulkan on Intel'
            'vulkan-tools: vulkaninfo for debugging'
            'vulkan-validation-layers: driver call validation for development')
source=()

build() {
    cd "$startdir"
    cargo build --release --locked
}

package() {
    cd "$startdir"
    install -Dm755 target/release/gliff-server "$pkgdir/usr/bin/gliff-server"
    install -Dm755 target/release/gliff-client "$pkgdir/usr/bin/gliff-client"
    install -Dm755 target/release/gliff-probe  "$pkgdir/usr/bin/gliff-probe"
    install -Dm644 README.md "$pkgdir/usr/share/doc/$pkgname/README.md"
    install -Dm644 docs/hardware-quirks.md "$pkgdir/usr/share/doc/$pkgname/hardware-quirks.md"
}
