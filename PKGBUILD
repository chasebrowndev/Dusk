# Maintainer: Chase Brown <brown10.chase@gmail.com>
pkgname=dusk
pkgver=0.1.0
pkgrel=1
pkgdesc="Lightweight TUI chat over Tailscale"
arch=('x86_64')
url="https://github.com/chasebrowndev/dusk"
license=('MIT')
depends=('tailscale' 'ffmpeg' 'wl-screenrec' 'opus' 'alsa-lib')
makedepends=('rust' 'cargo' 'pkg-config')
source=("$pkgname-$pkgver.tar.gz::$url/archive/refs/tags/v${pkgver}.tar.gz")
sha256sums=('dac2ee635a8384f509f8da1e79472d9eedbb30069b3dd5c89a0a9fa0fb37c8a6')

prepare() {
    cd "$pkgname-$pkgver"
    export RUSTUP_TOOLCHAIN=stable
    cargo fetch --locked --target "$(rustc -vV | sed -n 's/host: //p')"
}

build() {
    cd "$pkgname-$pkgver"
    export RUSTUP_TOOLCHAIN=stable
    export CARGO_TARGET_DIR=target
    cargo build --frozen --release
}

package() {
    cd "$pkgname-$pkgver"
    install -Dm0755 target/release/dusk "$pkgdir/usr/bin/dusk"
    install -Dm0644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
}
