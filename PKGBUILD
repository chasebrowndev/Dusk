# Maintainer: Chase
pkgname=dusk
pkgver=1.0
pkgrel=1
pkgdesc="P2P voice, text, and video communicator over Tailscale"
arch=('x86_64' 'aarch64')
license=('MIT')
depends=(
    'python'
    'python-pywebview'
    'portaudio'
    'python-pyaudio'
    'opus'
    'tailscale'
    'gtk3'
    'webkit2gtk'
    'ttf-nerd-fonts-symbols'
    'python-opencv'
    'python-numpy'
    'python-mss'
)
optdepends=(
    'pipewire: PipeWire audio backend'
    'pulseaudio: PulseAudio backend'
    'python-opuslib: Opus audio codec (AUR)'
)

# Local install — run makepkg from the dusk directory
source=()
sha256sums=()

package() {
    local src="$(dirname "$(realpath "${BASH_SOURCE[0]}")")"

    # Main application → /opt/dusk
    install -dm755 "$pkgdir/opt/dusk/nexus/ui"
    install -dm755 "$pkgdir/opt/dusk/nexus/network"
    install -dm755 "$pkgdir/opt/dusk/nexus/audio"

    install -Dm644 "$src/main.py"                     "$pkgdir/opt/dusk/main.py"
    install -Dm644 "$src/config.toml"                 "$pkgdir/opt/dusk/config.toml"
    install -Dm644 "$src/nexus/__init__.py"           "$pkgdir/opt/dusk/nexus/__init__.py"
    install -Dm644 "$src/nexus/api.py"                "$pkgdir/opt/dusk/nexus/api.py"
    install -Dm644 "$src/nexus/network/__init__.py"   "$pkgdir/opt/dusk/nexus/network/__init__.py"
    install -Dm644 "$src/nexus/audio/__init__.py"     "$pkgdir/opt/dusk/nexus/audio/__init__.py"
    install -Dm644 "$src/nexus/ui/index.html"         "$pkgdir/opt/dusk/nexus/ui/index.html"

    # /usr/bin launchers
    install -Dm755 /dev/stdin "$pkgdir/usr/bin/dusk-host" <<'EOF'
#!/usr/bin/env bash
export GDK_BACKEND=x11
export DISPLAY="${DISPLAY:-:0}"
export WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS=1
exec python /opt/dusk/main.py --host "$@"
EOF

    install -Dm755 /dev/stdin "$pkgdir/usr/bin/dusk-client" <<'EOF'
#!/usr/bin/env bash
export GDK_BACKEND=x11
export DISPLAY="${DISPLAY:-:0}"
export WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS=1
exec python /opt/dusk/main.py --connect "$@"
EOF

    install -Dm755 /dev/stdin "$pkgdir/usr/bin/dusk" <<'EOF'
#!/usr/bin/env bash
export GDK_BACKEND=x11
export DISPLAY="${DISPLAY:-:0}"
export WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS=1
exec python /opt/dusk/main.py "$@"
EOF

    # Desktop entry
    install -Dm644 /dev/stdin "$pkgdir/usr/share/applications/dusk.desktop" <<'EOF'
[Desktop Entry]
Name=Dusk
Comment=P2P voice, text, and video communicator
Exec=dusk
Icon=network-wireless
Terminal=false
Type=Application
Categories=Network;Chat;
Keywords=p2p;voice;video;tailscale;chat;
EOF
}
