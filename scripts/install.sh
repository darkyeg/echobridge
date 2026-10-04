#!/bin/sh
# Installs EchoBridge for the current user on any Linux distribution; no administrator
# rights needed. Use it where the .deb does not apply (Fedora, Arch, openSUSE, ...).
#
#   curl -fsSL https://github.com/darkyeg/echobridge/releases/latest/download/install.sh | sh
#   ./install.sh                  from an extracted EchoBridge-*-linux-*.tar.gz
#   ./install.sh --uninstall      remove it again
#
# Options: --prefix DIR   install under DIR instead of ~/.local
#          --version X    install release X (default: the latest)
set -eu

REPO=${ECHOBRIDGE_REPO:-darkyeg/echobridge}
prefix=$HOME/.local
version=latest
action=install

while [ $# -gt 0 ]; do
    case $1 in
        --uninstall) action=uninstall ;;
        --prefix) prefix=${2:?--prefix needs a folder}; shift ;;
        --version) version=${2:?--version needs a number}; shift ;;
        -h|--help) sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
    shift
done

bin_dir=$prefix/bin
apps_dir=$prefix/share/applications
icon_dir=$prefix/share/icons/hicolor/256x256/apps
config_dir=${XDG_CONFIG_HOME:-$HOME/.config}

say() { printf '%s\n' "$*"; }
fail() { printf 'error: %s\n' "$*" >&2; exit 1; }

refresh_caches() {
    command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$apps_dir" 2>/dev/null || true
    command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -q -t "$prefix/share/icons/hicolor" 2>/dev/null || true
}

if [ "$action" = uninstall ]; then
    rm -f "$bin_dir/EchoBridge" "$apps_dir/echobridge.desktop" "$icon_dir/echobridge.png" \
        "$config_dir/autostart/echobridge.desktop"
    refresh_caches
    say "EchoBridge removed. Your settings stay in ${XDG_DATA_HOME:-$HOME/.local/share}/EchoBridge."
    exit 0
fi

case $(uname -m) in
    x86_64) arch=x86_64 ;;
    aarch64|arm64) arch=aarch64 ;;
    *) fail "EchoBridge has no build for the $(uname -m) CPU." ;;
esac
[ "$(uname -s)" = Linux ] || fail "this installer is for Linux."

here=$(cd "$(dirname "$0")" 2>/dev/null && pwd || echo .)
work=
cleanup() { [ -z "$work" ] || rm -rf "$work"; }
trap cleanup EXIT INT TERM

if [ -x "$here/EchoBridge" ] && [ -f "$here/echobridge.desktop" ]; then
    source_dir=$here
else
    command -v tar >/dev/null 2>&1 || fail "tar is needed."
    if command -v curl >/dev/null 2>&1; then
        fetch() { curl -fsSL "$1" -o "$2"; }
    elif command -v wget >/dev/null 2>&1; then
        fetch() { wget -q "$1" -O "$2"; }
    else
        fail "curl or wget is needed to download EchoBridge."
    fi
    if [ "$version" = latest ]; then
        base=https://github.com/$REPO/releases/latest/download
    else
        base=https://github.com/$REPO/releases/download/v${version#v}
    fi
    work=$(mktemp -d)
    say "Downloading EchoBridge for $arch..."
    fetch "$base/SHA256SUMS-linux.txt" "$work/SHA256SUMS-linux.txt" || fail "could not download the release list from $base."
    tarball=$(sed -n "s/^[0-9a-f]*  \(EchoBridge-.*-linux-$arch\.tar\.gz\)\$/\1/p" "$work/SHA256SUMS-linux.txt" | head -n 1)
    [ -n "$tarball" ] || fail "this release has no Linux $arch build."
    fetch "$base/$tarball" "$work/$tarball" || fail "could not download $tarball."
    expected=$(sed -n "s/^\([0-9a-f]*\)  $tarball\$/\1/p" "$work/SHA256SUMS-linux.txt")
    command -v sha256sum >/dev/null 2>&1 || fail "sha256sum is needed to check the download."
    actual=$(sha256sum "$work/$tarball" | cut -d' ' -f1)
    [ "$expected" = "$actual" ] || fail "the download does not match its checksum; not installing."
    tar -xzf "$work/$tarball" -C "$work"
    source_dir=$work/${tarball%.tar.gz}
fi

mkdir -p "$bin_dir" "$apps_dir" "$icon_dir"
install -m 755 "$source_dir/EchoBridge" "$bin_dir/EchoBridge"
install -m 644 "$source_dir/echobridge.png" "$icon_dir/echobridge.png"
# The menu entry names the program by its full path, so it works when ~/.local/bin is not
# on the PATH of the desktop session.
sed "s|^Exec=.*|Exec=\"$bin_dir/EchoBridge\"|" "$source_dir/echobridge.desktop" > "$apps_dir/echobridge.desktop"
chmod 644 "$apps_dir/echobridge.desktop"
refresh_caches
say "Installed EchoBridge in $bin_dir."

# What EchoBridge needs from the system.
if command -v ldd >/dev/null 2>&1; then
    missing=$(ldd "$bin_dir/EchoBridge" 2>/dev/null | sed -n 's/^[[:space:]]*\(.*\) => not found.*/\1/p' | tr '\n' ' ')
    if [ -n "$missing" ]; then
        say "Missing libraries: $missing"
    fi
fi
has_pipewire=no
if command -v pw-cli >/dev/null 2>&1 || { command -v ldconfig >/dev/null 2>&1 && ldconfig -p 2>/dev/null | grep -q libpipewire-0.3; }; then
    has_pipewire=yes
fi
if [ "$has_pipewire" = no ] || [ -n "${missing:-}" ]; then
    . /etc/os-release 2>/dev/null || true
    case " ${ID:-} ${ID_LIKE:-} " in
        *" debian "*|*" ubuntu "*) hint="sudo apt install pipewire pipewire-pulse wireplumber libfontconfig1 libxkbcommon0" ;;
        *" fedora "*|*" rhel "*) hint="sudo dnf install pipewire pipewire-pulseaudio wireplumber fontconfig libxkbcommon" ;;
        *" arch "*) hint="sudo pacman -S pipewire pipewire-pulse wireplumber fontconfig libxkbcommon" ;;
        *" suse "*|*" opensuse "*) hint="sudo zypper install pipewire pipewire-pulseaudio wireplumber fontconfig libxkbcommon0" ;;
        *) hint="install PipeWire (with its PulseAudio support and WirePlumber), fontconfig and libxkbcommon with your package manager" ;;
    esac
    say "EchoBridge needs PipeWire and a few libraries. Install them with:"
    say "  $hint"
fi
case ":$PATH:" in
    *":$bin_dir:"*) ;;
    *) say "Add $bin_dir to your PATH to start it with the command EchoBridge; the menu entry works already." ;;
esac
say "Open EchoBridge from your applications menu. In Discord, choose \"EchoBridge Microphone\" while it runs."
