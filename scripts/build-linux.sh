#!/usr/bin/env bash
# Builds EchoBridge for Linux into dist/linux/: a tarball (for scripts/install.sh and any
# distribution), a .deb for Debian and Ubuntu, and SHA256SUMS-linux.txt for both.
#
#   scripts/build-linux.sh            build, then package
#   scripts/build-linux.sh --no-build package the existing release build as it is
#
# Build it on an old distribution (CI uses Debian 12) so the program runs on newer ones too.
# Needs a Rust toolchain, libpipewire-0.3-dev, libclang and libfontconfig1-dev. The .deb needs dpkg-deb, which is skipped when it is missing.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

if [ "${1:-}" != "--no-build" ]; then
    # Load fontconfig when the program starts instead of linking it, so the only library a
    # system must provide is PipeWire.
    RUST_FONTCONFIG_DLOPEN=on cargo build --release --locked -p echobridge
fi

# ECHOBRIDGE_VERSION names a nightly build, such as 1.0.0-nightly.20261004.abc1234.
version=${ECHOBRIDGE_VERSION:-$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)}
# In Debian ordering a tilde sorts before the release, so a nightly never outranks it.
deb_version=$(printf '%s' "$version" | sed 's/-/~/')
case "$(uname -m)" in
    x86_64) arch=x86_64 deb_arch=amd64 ;;
    aarch64) arch=aarch64 deb_arch=arm64 ;;
    *) echo "unsupported CPU: $(uname -m)" >&2; exit 1 ;;
esac

binary=${CARGO_TARGET_DIR:-target}/release/EchoBridge
[ -x "$binary" ] || { echo "$binary not found; build first" >&2; exit 1; }

dist=dist/linux
rm -rf "$dist"
mkdir -p "$dist"

if command -v pwsh >/dev/null 2>&1; then
    pwsh -NoProfile -File scripts/notices.ps1 -Output "$dist/THIRD-PARTY-NOTICES.txt" -Target "$arch-unknown-linux-gnu"
else
    echo "pwsh not found: THIRD-PARTY-NOTICES.txt skipped" >&2
fi

# Tarball: the program, its desktop entry and icon, and the installer that places them.
name="EchoBridge-$version-linux-$arch"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/$name"
install -m 755 "$binary" "$stage/$name/EchoBridge"
install -m 755 scripts/install.sh "$stage/$name/install.sh"
install -m 644 packaging/linux/echobridge.desktop "$stage/$name/echobridge.desktop"
install -m 644 crates/app/assets/icon.png "$stage/$name/echobridge.png"
install -m 644 LICENSE "$stage/$name/LICENSE"
install -m 644 START-HERE-LINUX.txt "$stage/$name/START-HERE.txt"
[ -f "$dist/THIRD-PARTY-NOTICES.txt" ] && install -m 644 "$dist/THIRD-PARTY-NOTICES.txt" "$stage/$name/"
tar -C "$stage" --owner=0 --group=0 -czf "$dist/$name.tar.gz" "$name"

# Debian package.
if command -v dpkg-deb >/dev/null 2>&1; then
    deb="$stage/deb"
    install -D -m 755 "$binary" "$deb/usr/bin/EchoBridge"
    install -D -m 644 packaging/linux/echobridge.desktop "$deb/usr/share/applications/echobridge.desktop"
    install -D -m 644 crates/app/assets/icon.png "$deb/usr/share/icons/hicolor/256x256/apps/echobridge.png"
    install -D -m 644 LICENSE "$deb/usr/share/doc/echobridge/copyright"
    [ -f "$dist/THIRD-PARTY-NOTICES.txt" ] && install -D -m 644 "$dist/THIRD-PARTY-NOTICES.txt" "$deb/usr/share/doc/echobridge/THIRD-PARTY-NOTICES.txt"
    size=$(du -sk "$deb" | cut -f1)
    mkdir -p "$deb/DEBIAN"
    cat > "$deb/DEBIAN/control" <<EOF
Package: echobridge
Version: $deb_version
Section: sound
Priority: optional
Architecture: $deb_arch
Installed-Size: $size
Depends: libpipewire-0.3-0t64 | libpipewire-0.3-0 (>= 0.3.50), libfontconfig1, libc6 (>= 2.36)
Recommends: pipewire-pulse, wireplumber
Maintainer: EchoBridge <noreply@users.noreply.github.com>
Homepage: https://github.com/darkyeg/echobridge
Description: Removes headphone sound that leaks into your microphone
 EchoBridge listens to what your headphones play, subtracts it from the
 microphone, and offers the clean microphone to Discord or any call app as the
 virtual microphone "EchoBridge Microphone". It needs PipeWire (the default on
 current Ubuntu, Debian, Fedora and Arch).
EOF
    dpkg-deb --root-owner-group --build "$deb" "$dist/echobridge_${deb_version}_$deb_arch.deb" >/dev/null
else
    echo "dpkg-deb not found: .deb skipped" >&2
fi

(cd "$dist" && sha256sum -- $(ls -- *.tar.gz *.deb 2>/dev/null) > SHA256SUMS-linux.txt)
ls -l "$dist"
