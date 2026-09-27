#!/usr/bin/env bash
#
# Builds Crabsend as a universal (x86_64 + arm64) macOS bundle and packages it as a dmg.
#
#   scripts/build-macos.sh
#
# Compile only: nothing is installed and the frontend dependencies are expected to be in
# place already (`pnpm install` is not run here; the build fails if node_modules is missing).
#
# The app in the dmg is signed ad-hoc unless CRABSEND_SIGNING_IDENTITY (or
# APPLE_SIGNING_IDENTITY) names an Apple-issued identity. macOS 15 and later grant the local
# network by code signature, and a bundle the system cannot track is refused discovery and
# every connection to a peer without a word, so an unsigned build does not work there at all
# (the readme's macOS section has the details). The dmg is mounted once to check that the app
# inside it is signed.
#
# Prerequisites:
#   · node + pnpm, with node_modules installed
#   · a rustup-managed toolchain new enough for edition 2024 and able to target
#     aarch64-apple-darwin. A Homebrew/MacPorts rust builds the host target only and has no
#     rustup, so a universal build needs a rustup one; when `rustup` is not on PATH the
#     script falls back to the private sandbox at $HOME/.cache/crabsend-universal-rust
#     (override with CRABSEND_RUST_SANDBOX), which is also where the missing std targets are
#     added when it is used. To create that sandbox (it leaves the global environment alone,
#     and can be deleted when it is no longer wanted):
#       SB=~/.cache/crabsend-universal-rust
#       mkdir -p "$SB"
#       curl -sSf https://static.rust-lang.org/rustup/dist/x86_64-apple-darwin/rustup-init \
#           -o "$SB/rustup-init" && chmod +x "$SB/rustup-init"
#       RUSTUP_HOME="$SB/rustup" CARGO_HOME="$SB/cargo" "$SB/rustup-init" -y \
#           --no-modify-path --profile minimal --default-toolchain stable \
#           -t aarch64-apple-darwin
#   · Xcode command line tools (lipo, codesign, hdiutil)
#
# Result: target/universal-apple-darwin/release/bundle/dmg/*.dmg, printed as absolute paths
# together with the directory holding them.
set -euo pipefail

root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
readonly root
readonly dmg_dir="$root/target/universal-apple-darwin/release/bundle/dmg"

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
        -h | --help) usage ;;
        *) die "unknown option: $1 (try --help)" ;;
    esac
    shift
done

require_command() {
    command -v "$1" >/dev/null 2>&1 || die "required command not found: $1${2:+ ($2)}"
}

[[ "$(uname -s)" == Darwin ]] || die "this script builds macOS bundles and needs to run on macOS"

# The private sandbox, used whenever it is there and rustup is not on PATH: the target
# toolchain, its sysroot and the cargo registry all stay inside it.
sandbox="${CRABSEND_RUST_SANDBOX:-$HOME/.cache/crabsend-universal-rust}"
if [[ -x "$sandbox/cargo/bin/rustup" ]] && ! command -v rustup >/dev/null 2>&1; then
    log "using the Rust toolchain in $sandbox"
    export RUSTUP_HOME="$sandbox/rustup" CARGO_HOME="$sandbox/cargo"
    export PATH="$sandbox/cargo/bin:$PATH"
fi

require_command node "install nodejs"
require_command pnpm "install pnpm (corepack enable)"
require_command cargo "install rust"
require_command lipo "install the Xcode command line tools"
require_command codesign "install the Xcode command line tools"
require_command hdiutil "install the Xcode command line tools"

[[ -d "$root/node_modules" ]] ||
    die "frontend dependencies are missing; run 'pnpm install' first (this script compiles only)"

# tauri builds the universal binary by compiling both slices, each of which needs its own std.
need=()
sysroot="$(rustc --print sysroot)"
for triple in x86_64-apple-darwin aarch64-apple-darwin; do
    [[ -d "$sysroot/lib/rustlib/$triple" ]] || need+=("$triple")
done
if ((${#need[@]})); then
    if command -v rustup >/dev/null 2>&1; then
        log "adding the missing Rust std targets: ${need[*]}"
        rustup target add "${need[@]}"
    else
        die "the active toolchain ($(rustc --version)) at $(command -v rustc) has no std for
    ${need[*]}
and no rustup to add it. Install a rustup toolchain, or create the private sandbox
$sandbox as described in the header (scripts/build-macos.sh --help) and rerun this script"
    fi
fi

cd "$root"
log "building the universal bundle (x86_64 + arm64)"

# macOS 15 and later grant the local network by code signature: a program whose signature
# the system cannot track is refused the multicast announcements and every connection it
# starts towards a peer, silently, while what a peer starts with it keeps working. An
# unsigned bundle can be granted nothing at all, so one is signed ad-hoc here — enough to
# be granted, but the grant has to be given again after every rebuild, because an ad-hoc
# signature changes with the code. A Developer ID is what makes one grant stick; name one
# in CRABSEND_SIGNING_IDENTITY (or APPLE_SIGNING_IDENTITY) to use it.
signing="${CRABSEND_SIGNING_IDENTITY:-${APPLE_SIGNING_IDENTITY:-}}"
if [[ -n "$signing" ]]; then
    log "signing with $signing"
    signing_config="{\"bundle\":{\"macOS\":{\"signingIdentity\":\"$signing\"}}}"
elif [[ -n "${APPLE_CERTIFICATE:-}" ]]; then
    # The certificate is the identity here, and the bundler imports it itself: naming one as
    # well, this script's ad-hoc default included, makes it refuse the pair as mismatched.
    log "signing with the certificate in APPLE_CERTIFICATE"
    signing_config='{}'
else
    log "signing ad-hoc (set CRABSEND_SIGNING_IDENTITY to sign with an Apple-issued identity)"
    signing_config='{"bundle":{"macOS":{"signingIdentity":"-"}}}'
fi

# CI=true keeps the dmg bundler from mounting a writable image and driving Finder through
# AppleScript to lay out icons (tauri-bundler only passes --skip-jenkins on its own when it
# sees CI); the Applications drop link and the volume icon are unaffected. The .app that
# feeds the dmg is removed by the bundler once the dmg is written.
CI=true pnpm tauri build --ci --target universal-apple-darwin --bundles dmg --config "$signing_config"

shopt -s nullglob
dgms=("$dmg_dir"/*.dmg)
((${#dgms[@]})) || die "the build reported success but there is no dmg under $dmg_dir"

# The dmg is what gets installed, so it is the app inside it that has to carry the
# signature: that app is the one the system is asked to grant the local network to.
for dmg in "${dgms[@]}"; do
    mount_point="$(mktemp -d)"
    hdiutil attach "$dmg" -nobrowse -quiet -mountpoint "$mount_point" ||
        die "cannot mount $dmg to check the signature of the app inside it"
    app="$(find "$mount_point" -maxdepth 1 -name '*.app' -print -quit)"
    if [[ -z "$app" ]] || ! codesign --verify --strict "$app" 2>/dev/null; then
        hdiutil detach -quiet "$mount_point" || true
        die "the app in $dmg is not signed, and macOS 15 and later grant the local network by
code signature: unsigned, it would find no device and reach none. Rerun this script (it
signs ad-hoc), or set CRABSEND_SIGNING_IDENTITY to an Apple-issued identity."
    fi
    signature="$(codesign -dv "$app" 2>&1 | sed -n 's/^Signature=//p')"
    hdiutil detach -quiet "$mount_point" || die "cannot unmount the app checked in $dmg"
    rmdir "$mount_point"
    log "the app in $(basename "$dmg") is signed (signature: ${signature:-unknown})"
done

log "dmg directory: $dmg_dir"
for dmg in "${dgms[@]}"; do
    printf '    %s\n' "$dmg"
done
