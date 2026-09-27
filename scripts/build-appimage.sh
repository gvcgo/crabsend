#!/usr/bin/env bash
#
# Builds Crabsend as a Linux AppImage.
#
#   scripts/build-appimage.sh                  build, then bundle
#   scripts/build-appimage.sh --install-deps   install missing build dependencies first
#   scripts/build-appimage.sh --extract        unpack the AppImage next to it, for inspection
#   scripts/build-appimage.sh --run            launch the AppImage once it is built
#   scripts/build-appimage.sh --mirror <base>  fetch the bundler's tools through a mirror
#
# The result is target/release/bundle/appimage/Crabsend_<version>_<arch>.AppImage, next to
# the Crabsend.AppDir it was built from. Nothing is installed: the frontend dependencies
# have to be in place or installable (`pnpm install` is run here), and `--install-deps`
# installs the missing system packages with pacman.
#
# Running the AppImage needs FUSE 2 (libfuse.so.2); where it is missing, the AppImage runs
# as `./Crabsend_<version>_<arch>.AppImage --appimage-extract-and-run`. Building needs no
# FUSE — the bundler runs linuxdeploy with --appimage-extract-and-run itself.
#
# The bundler downloads its own tools on the first run — linuxdeploy, a fork of the
# linuxdeploy GTK plugin, the AppImage output plugin and an AppRun — into
# $XDG_CACHE_HOME/tauri. `--mirror` (or TAURI_BUNDLER_TOOLS_GITHUB_MIRROR, which wins)
# fetches them from a GitHub mirror instead: the value is the mirror's base URL, and the
# path of the github.com URL is appended to it, so `https://ghproxy.net/` asks that host
# for https://ghproxy.net/https://github.com/…
#
# Two properties of this bundle are worth knowing, and the script handles both:
#
#   · The tray icon is the way back to a window that has been closed, and the library
#     behind it (libayatana-appindicator3) is loaded with dlopen at run time instead of
#     being linked, so nothing in the AppDir refers to it and linuxdeploy would not
#     deploy it. The load falls back to the older libappindicator3 and then gives up, and
#     giving up is a panic out of libappindicator-sys, which takes the whole application
#     down at startup — the AppImage would die on every machine that has neither library
#     installed. The script stages the library into the AppDir, where the bundler treats
#     it like any other file and deploys it together with the libraries it needs
#     (libdbusmenu-gtk3, libdbusmenu-glib).
#   · linuxdeploy ships its own binutils 2.35, whose strip cannot read the .relr.dyn
#     section that the libraries of a current distribution are built with; every strip
#     call fails and linuxdeploy treats that as fatal (exit 1), which fails the whole
#     bundling. The script runs it with NO_STRIP=1: what it would strip are the
#     distribution's own libraries, already stripped by the distribution.
set -euo pipefail

root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
readonly root
readonly bundle_dir="$root/target/release/bundle/appimage"
readonly config_file="$root/src-tauri/tauri.conf.json"

install_deps=false
keep_extract=false
run_appimage=false
mirror="${TAURI_BUNDLER_TOOLS_GITHUB_MIRROR:-}"

log() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

# The leading comment block is the help text.
usage() {
    awk 'NR > 2 && /^#/ { sub(/^# ?/, ""); print; next } NR > 2 { exit }' "${BASH_SOURCE[0]}"
    exit 0
}

while (($#)); do
    case "$1" in
        --install-deps) install_deps=true ;;
        --extract) keep_extract=true ;;
        --run) run_appimage=true ;;
        --mirror)
            shift
            mirror="${1:-}"
            [[ -n "$mirror" ]] || die "--mirror needs a base URL"
            ;;
        -h | --help) usage ;;
        *) die "unknown option: $1 (try --help)" ;;
    esac
    shift
done

require_command() {
    command -v "$1" >/dev/null 2>&1 || die "required command not found: $1${2:+ ($2)}"
}

[[ "$(uname -s)" == Linux ]] || die "this script builds Linux AppImages and needs to run on Linux"

# AppImage names its architectures after the Debian ones, and linuxdeploy uses the Rust
# triple's first element — which is the same word for both of these.
case "$(uname -m)" in
    x86_64) appimage_arch=amd64 ;;
    aarch64 | arm64) appimage_arch=aarch64 ;;
    *) die "unsupported architecture: $(uname -m) (the AppImage bundler takes x86_64 and aarch64)" ;;
esac
readonly appimage_arch

# --- build dependencies ------------------------------------------------------

# What the build links against; the GTK plugin needs pkg-config and the three tools below
# to write the caches a WebKitGTK application cannot start without.
readonly -a dep_modules=(webkit2gtk-4.1 javascriptcoregtk-4.1 gtk+-3.0 librsvg-2.0)
readonly -a dep_packages=(webkit2gtk-4.1 webkit2gtk-4.1 gtk3 librsvg)

check_build_dependencies() {
    local missing=() i
    for i in "${!dep_modules[@]}"; do
        pkg-config --exists "${dep_modules[$i]}" || missing+=("${dep_packages[$i]}")
    done
    # Deduplicate while keeping order (webkit2gtk-4.1 appears twice).
    local unique=()
    for i in "${missing[@]:-}"; do
        [[ " ${unique[*]:-} " == *" $i "* ]] || unique+=("$i")
    done
    ((${#unique[@]} == 0)) && return 0

    log "missing build dependencies: ${unique[*]}"
    if $install_deps; then
        require_command sudo
        sudo pacman -S --needed --noconfirm "${unique[@]}"
        return 0
    fi
    die "install them first (or rerun with --install-deps):
    sudo pacman -S --needed ${unique[*]}"
}

# --- the tray library --------------------------------------------------------

readonly tray_soname="libayatana-appindicator3.so.1"

# ldconfig answers with the absolute path of a soname. The symlink is what dlopen asks
# for and what the copy is named, so the symlink is what is staged. Its cache is read in
# one go and parsed in the shell: reading it through a pipeline that stops at the first
# match would leave ldconfig killed by SIGPIPE, and `set -o pipefail` counts that as a
# failure.
mapfile -t linker_cache < <(ldconfig -p)

find_library() {
    local entry name path
    for entry in "${linker_cache[@]}"; do
        # A line reads <soname> (<arch>) => <path>; `read` drops the leading blank that
        # a shell pattern would keep.
        read -r name _ _ path <<<"$entry" || continue
        [[ "$name" == "$1" ]] || continue
        printf '%s' "$path"
        return 0
    done
    return 1
}

has_library() { find_library "$1" >/dev/null; }

# --- build -------------------------------------------------------------------

require_command node "install nodejs"
require_command pnpm "install pnpm (corepack enable)"
require_command cargo "install rust"
require_command pkg-config "install pkgconf"
# linuxdeploy needs patchelf to make the libraries in the AppDir find each other, and dd
# on the copy of itself it blanks out so that its AppImage magic cannot be detected.
require_command patchelf "install patchelf"
require_command dd "install coreutils"
require_command gtk-update-icon-cache "install gtk-update-icon-cache"
require_command glib-compile-schemas "install glib2"
require_command gdk-pixbuf-query-loaders "install gdk-pixbuf2"
# readelf and ldd check the bundle below.
require_command readelf "install binutils"
require_command ldd "install binutils"

check_build_dependencies

tray_library="$(find_library "$tray_soname")" || die "$tray_soname is not installed, and it is
needed even though nothing links against it: the application loads it with dlopen, staging it
is what keeps the AppImage from dying with libappindicator-sys' panic on every machine that has
neither it nor the older libappindicator3. Install it with:
    sudo pacman -S libayatana-appindicator                (Arch)
    sudo apt install libayatana-appindicator3-1 libdbusmenu-gtk3-4  (Debian, Ubuntu)"
readonly tray_library

product="$(node -p "require('$config_file').productName")"
version="$(node -p "require('$config_file').version")"
readonly product version
readonly appimage="$bundle_dir/${product}_${version}_${appimage_arch}.AppImage"

# The files map is the AppDir's own tree: the key is the path to copy to, the value the
# library to copy. That the library is there before linuxdeploy runs is what makes it
# deploy the libraries that library needs in turn.
config="$(printf '{"bundle":{"linux":{"appimage":{"files":{"usr/lib/%s":"%s"}}}}}' \
    "$tray_soname" "$tray_library")"

cd "$root"
log "installing frontend dependencies"
pnpm install --frozen-lockfile

# NO_STRIP is linuxdeploy's own switch (see the header): its bundled strip cannot read the
# .relr.dyn sections of a current host's libraries, and a failed strip call fails the run.
export NO_STRIP=1
if [[ -n "$mirror" ]]; then
    log "downloading the bundler's tools through $mirror"
    export TAURI_BUNDLER_TOOLS_GITHUB_MIRROR="$mirror"
fi

log "building the release binary and bundling it ($appimage_arch)"
log "linuxdeploy runs with NO_STRIP=1: its bundled binutils 2.35 cannot read the .relr.dyn section of this host's libraries (see the header)"

# beforeBuildCommand (vite) runs as part of this, so dist/ is refreshed. tauri reports a
# linuxdeploy failure as the bare words above, without linuxdeploy's own message, and one
# attempt has failed that way here and then succeeded on an identical rerun — so the
# bundling gets a second attempt, and only its failure says where to look.
for attempt in 1 2; do
    if pnpm tauri build --bundles appimage --config "$config"; then
        break
    fi
    if ((attempt == 2)); then
        die "the bundling failed twice. Rerun it yourself to see what linuxdeploy says —
tauri only reports that it failed:
    NO_STRIP=1 pnpm tauri build --bundles appimage -v --config '$config'"
    fi
    warn "the bundling failed; trying once more (the cargo output and the tools are cached, so only the bundling is repeated)"
done

[[ -f "$appimage" ]] || die "the build reported success but $appimage is missing"
log "bundled $appimage ($(du -h "$appimage" | cut -f1))"

# --- verification ------------------------------------------------------------

# The AppDir is removed and rewritten on the next run, and the bundler removes the deb
# staging it fed it from, so what is checked is the AppImage itself: it unpacks without
# FUSE, which is what the machine that runs it may still have to do.
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
(cd "$work" && "$appimage" --appimage-extract >/dev/null) ||
    die "the AppImage does not unpack: $appimage --appimage-extract failed"
readonly squashfs="$work/squashfs-root"

for path in AppRun usr/bin/crabsend-app "usr/share/applications/$product.desktop" "$product.png"; do
    [[ -e "$squashfs/$path" ]] || die "the AppImage has no $path"
done

# The tray library and what it needs must be in the bundle, not merely on this machine:
# dlopen looks in the AppDir's usr/lib first (AppRun puts it in LD_LIBRARY_PATH), so these
# are the copies the application loads.
for soname in "$tray_soname" libdbusmenu-gtk3.so.4 libdbusmenu-glib.so.4; do
    [[ -e "$squashfs/usr/lib/$soname" ]] || die "the AppImage carries no $soname, so the tray
icon would take the application down on a machine without that library installed"
done

for binary in "$squashfs/usr/lib/$tray_soname" "$squashfs/usr/bin/crabsend-app"; do
    missing="$(LD_LIBRARY_PATH="$squashfs/usr/lib" ldd "$binary" | awk '/not found/')"
    [[ -z "$missing" ]] || die "$(basename "$binary") in the AppImage needs libraries that are
not in it:
$missing"
done
log "the bundle carries the GTK and WebKit libraries, the tray library and its own WebKit processes"

if $keep_extract; then
    rm -rf "$bundle_dir/${appimage##*/}.extracted"
    cp -a "$squashfs" "$bundle_dir/${appimage##*/}.extracted"
    log "extracted copy: $bundle_dir/${appimage##*/}.extracted"
fi

if ! has_library libfuse.so.2; then
    warn "libfuse.so.2 is missing, so this machine cannot run an AppImage directly:
    $appimage --appimage-extract-and-run"
    run_appimage=false
fi

if $run_appimage; then
    log "launching $appimage"
    exec "$appimage"
fi

log "AppImage: $appimage"
cat <<NOTES

The bundle is self-contained apart from a GL driver and the WebKitGTK libraries' own
dependencies, but the tray icon needs a panel that speaks StatusNotifier: a session with no
status-notifier host has nowhere to show the icon, and the window then closes as it always
did (the readme's Linux notes carry the details).
NOTES
