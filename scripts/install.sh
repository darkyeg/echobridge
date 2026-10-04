#!/bin/sh
# Installs EchoBridge for the current user on any Linux distribution (Debian, Ubuntu,
# Fedora, Arch, openSUSE, ...). No administrator rights are needed, except to add a missing
# system library, which it asks about first.
#
#   curl -fsSL https://github.com/darkyeg/echobridge/releases/latest/download/install.sh | sh
#   ./install.sh                  from an extracted EchoBridge-*-linux-*.tar.gz
#   ./install.sh --uninstall      remove it again
#
# Options: --yes          install missing libraries without asking
#          --nightly      install the nightly build, which may be unfinished
#          --version X    install release X (default: the latest stable release)
#          --prefix DIR   install under DIR instead of ~/.local
set -eu

REPO=${ECHOBRIDGE_REPO:-darkyeg/echobridge}
prefix=$HOME/.local
version=latest
action=install
assume_yes=

while [ $# -gt 0 ]; do
    case $1 in
        --uninstall) action=uninstall ;;
        --yes|-y) assume_yes=1 ;;
        --nightly) version=nightly ;;
        --version) version=${2:?--version needs a number}; shift ;;
        --prefix) prefix=${2:?--prefix needs a folder}; shift ;;
        -h|--help) sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
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

[ "$(uname -s)" = Linux ] || fail "this installer is for Linux."
case $(uname -m) in
    x86_64) arch=x86_64 ;;
    aarch64|arm64) arch=aarch64 ;;
    *) fail "EchoBridge has no build for the $(uname -m) CPU." ;;
esac

# Where the files come from: this folder when it is an extracted release, else a download.
here=$(cd "$(dirname "$0")" 2>/dev/null && pwd || echo .)
work=
cleanup() { [ -z "$work" ] || rm -rf "$work"; }
trap cleanup EXIT INT TERM

if [ -x "$here/EchoBridge" ] && [ -f "$here/echobridge.desktop" ]; then
    source_dir=$here
else
    command -v tar >/dev/null 2>&1 || fail "tar is needed."
    command -v sha256sum >/dev/null 2>&1 || fail "sha256sum is needed to check the download."
    if command -v curl >/dev/null 2>&1; then
        fetch() { curl -fsSL "$1" -o "$2"; }
    elif command -v wget >/dev/null 2>&1; then
        fetch() { wget -q "$1" -O "$2"; }
    else
        fail "curl or wget is needed to download EchoBridge."
    fi
    case $version in
        # GitHub's "latest" never points at a pre-release, so this is the stable channel.
        latest) base=https://github.com/$REPO/releases/latest/download ;;
        nightly) base=https://github.com/$REPO/releases/download/nightly ;;
        *) base=https://github.com/$REPO/releases/download/v${version#v} ;;
    esac
    work=$(mktemp -d)
    sums=$work/SHA256SUMS-linux.txt
    say "Downloading EchoBridge..."
    fetch "$base/SHA256SUMS-linux.txt" "$sums" || fail "could not download the release list from $base."
    tarball=$(sed -n "s/^[0-9a-f]*  \(EchoBridge-.*-linux-$arch\.tar\.gz\)\$/\1/p" "$sums" | head -n 1)
    [ -n "$tarball" ] || fail "this release has no Linux $arch build."
    fetch "$base/$tarball" "$work/$tarball" || fail "could not download $tarball."
    expected=$(sed -n "s/^\([0-9a-f]*\)  $tarball\$/\1/p" "$sums")
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

# EchoBridge needs the PipeWire library, and fontconfig for text. Add what is missing.
missing_library() { ! ldconfig -p 2>/dev/null | grep -q "$1"; }
need_libraries=
if command -v ldd >/dev/null 2>&1 && ldd "$bin_dir/EchoBridge" 2>/dev/null | grep -q "not found"; then
    need_libraries=1
fi
if command -v ldconfig >/dev/null 2>&1 && { missing_library libpipewire-0.3.so.0 || missing_library libfontconfig.so.1; }; then
    need_libraries=1
fi

if [ -n "$need_libraries" ]; then
    if command -v apt-get >/dev/null 2>&1; then
        pipewire_package=libpipewire-0.3-0
        # Ubuntu 24.04 and Debian 13 renamed it.
        apt-cache show libpipewire-0.3-0t64 >/dev/null 2>&1 && pipewire_package=libpipewire-0.3-0t64
        install_command="apt-get install -y $pipewire_package libfontconfig1"
    elif command -v dnf >/dev/null 2>&1; then
        install_command="dnf install -y pipewire-libs fontconfig"
    elif command -v pacman >/dev/null 2>&1; then
        install_command="pacman -S --needed --noconfirm pipewire fontconfig"
    elif command -v zypper >/dev/null 2>&1; then
        install_command="zypper --non-interactive install libpipewire-0_3-0 fontconfig"
    else
        install_command=
    fi
    if [ -z "$install_command" ]; then
        say "EchoBridge needs the PipeWire and fontconfig libraries. Install them with your package manager."
    else
        privileged=
        [ "$(id -u)" = 0 ] || privileged=sudo
        say "EchoBridge needs a few system libraries. Command: $privileged $install_command"
        answer=y
        if [ -z "$assume_yes" ]; then
            if [ -r /dev/tty ] && [ -w /dev/tty ]; then
                printf 'Run it now? [Y/n] ' > /dev/tty
                read -r answer < /dev/tty || answer=n
            else
                answer=n
            fi
        fi
        case $answer in
            ""|y|Y|yes)
                if [ -n "$privileged" ] && ! command -v sudo >/dev/null 2>&1; then
                    say "sudo is not available; run the command above as an administrator."
                else
                    # shellcheck disable=SC2086
                    $privileged $install_command || say "Could not install them; run the command above yourself."
                fi
                ;;
            *) say "Run the command above before starting EchoBridge." ;;
        esac
    fi
fi

# Apps record from the virtual microphone through PipeWire's PulseAudio support. A system
# that still runs plain PulseAudio needs to switch.
if command -v pactl >/dev/null 2>&1; then
    server=$(pactl info 2>/dev/null | sed -n 's/^Server Name: //p')
    case $server in
        "") ;;
        *PipeWire*) ;;
        *) say "Your sound server is $server, not PipeWire. EchoBridge needs PipeWire (package pipewire-pulse)." ;;
    esac
fi

case ":$PATH:" in
    *":$bin_dir:"*) ;;
    *) say "Add $bin_dir to your PATH to start it with the command EchoBridge; the menu entry works already." ;;
esac
say "Done. Open EchoBridge from your applications menu, then choose \"EchoBridge Microphone\" in your call app."
