#!/usr/bin/env bash
#
# Builds Crabsend for Android.
#
#   scripts/build-android.sh                       debug APK for aarch64
#   scripts/build-android.sh --release             release APK (needs a signing key)
#   scripts/build-android.sh --targets all         every supported ABI
#   scripts/build-android.sh --split-per-abi       one APK per ABI
#   scripts/build-android.sh --aab                 Android App Bundle
#   scripts/build-android.sh --install             install the APK on a connected device
#   scripts/build-android.sh --reinit              regenerate the Android project
#
# GRADLE_DISTRIBUTION_URL overrides where the Gradle wrapper fetches Gradle
# from; without it the official URL is used unless it is unreachable, in which
# case a known mirror is substituted.
#
# Environment: ANDROID_HOME / ANDROID_SDK_ROOT and ANDROID_NDK_HOME / NDK_HOME
# are detected (the default Android Studio location included) and exported for
# the build. For `--release` the signing key is read from
# `src-tauri/gen/android/keystore.properties`, or created from
# CRABSEND_KEYSTORE, CRABSEND_KEYSTORE_PASSWORD, CRABSEND_KEY_ALIAS and
# CRABSEND_KEY_PASSWORD.
set -euo pipefail

root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
readonly root
readonly android_dir="$root/src-tauri/gen/android"

build_profile="debug"
gradle_url="${GRADLE_DISTRIBUTION_URL:-}"
build_aab=false
split_per_abi=false
install_apk=false
reinit=false
targets=()

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
        --release) build_profile="release" ;;
        --debug) build_profile="debug" ;;
        --aab) build_aab=true ;;
        --split-per-abi) split_per_abi=true ;;
        -t | --targets)
            shift
            while (($#)) && [[ "$1" != -* ]]; do
                targets+=("$1")
                shift
            done
            continue
            ;;
        --gradle-url)
            shift
            gradle_url="${1:-}"
            [[ -n "$gradle_url" ]] || die "--gradle-url needs a URL"
            ;;
        -i | --install) install_apk=true ;;
        --reinit) reinit=true ;;
        -h | --help) usage ;;
        *) die "unknown option: $1 (try --help)" ;;
    esac
    shift
done

# The ABIs Rust can target; the aliases match `tauri android build --target`.
declare -A abi_target=(
    [aarch64]=aarch64-linux-android
    [armv7]=armv7-linux-androideabi
    [i686]=i686-linux-android
    [x86_64]=x86_64-linux-android
)
if ((${#targets[@]} == 0)); then
    targets=(aarch64)
elif [[ " ${targets[*]} " == *" all "* ]]; then
    targets=(aarch64 armv7 i686 x86_64)
fi
for target in "${targets[@]}"; do
    [[ -n "${abi_target[$target]:-}" ]] || die "unknown ABI '$target' (use: aarch64 armv7 i686 x86_64 or all)"
done

log "targets: ${targets[*]} (${build_profile})"

# --- toolchain checks --------------------------------------------------------

for command in node pnpm cargo rustup java; do
    command -v "$command" >/dev/null 2>&1 || die "required command not found: $command"
done

# Gradle 8.x refuses to run on a JDK newer than its supported range, and
# Android Studio ships its own, much newer JetBrains Runtime that the Android
# build would otherwise pick up. Pin a JDK Gradle accepts.
jdk_major() {
    local release="$1/release"
    [[ -f "$release" ]] || return 1
    sed -n 's/^JAVA_VERSION="\([0-9]*\).*/\1/p' "$release"
}

pick_jdk() {
    local candidate major
    for candidate in "${JAVA_HOME:-}" /usr/lib/jvm/java-21-openjdk \
        /usr/lib/jvm/default-runtime /usr/lib/jvm/default; do
        [[ -n "$candidate" && -x "$candidate/bin/java" ]] || continue
        major="$(jdk_major "$candidate")" || continue
        if ((major >= 17 && major <= 24)); then
            printf '%s' "$candidate"
            return 0
        fi
    done
    return 1
}

jdk="$(pick_jdk)" || die "no JDK between 17 and 24 found, and Gradle cannot run on newer ones.
    Install one with:  sudo pacman -S jdk21-openjdk
    (Android Studio's bundled runtime at /opt/android-studio/jbr is too new for Gradle 8.x.)"

export JAVA_HOME="$jdk"
export PATH="$JAVA_HOME/bin:$PATH"
log "JDK: $JAVA_HOME ($(java -version 2>&1 | head -1))"

# --- Android SDK and NDK -----------------------------------------------------

find_sdk() {
    local candidate
    for candidate in "${ANDROID_HOME:-}" "${ANDROID_SDK_ROOT:-}" \
        "${HOME:-}/Android/Sdk" "${HOME:-}/Android/sdk" /opt/android-sdk; do
        [[ -n "$candidate" && -d "$candidate/platform-tools" ]] && {
            printf '%s' "$candidate"
            return
        }
    done
}

sdk="$(find_sdk)" || true
[[ -n "$sdk" ]] || die "no Android SDK found; install it with Android Studio or set ANDROID_HOME"
sdk="$(readlink -f "$sdk")"

find_ndk() {
    local candidate
    for candidate in "${ANDROID_NDK_HOME:-}" "${NDK_HOME:-}" "${ANDROID_NDK_ROOT:-}"; do
        [[ -n "$candidate" && -d "$candidate/toolchains/llvm" ]] && {
            printf '%s' "$candidate"
            return
        }
    done
    # Otherwise take the newest NDK the SDK ships.
    [[ -d "$sdk/ndk" ]] || return 0
    find "$sdk/ndk" -maxdepth 1 -mindepth 1 -type d -printf '%f\n' 2>/dev/null |
        sort -V | tail -1 |
        sed "s|^|$sdk/ndk/|"
}

ndk="$(find_ndk)" || true
[[ -n "$ndk" && -d "$ndk/toolchains/llvm" ]] ||
    die "no Android NDK found under $sdk/ndk; install one with:
    sdkmanager --install 'ndk;27.0.12077973'"
ndk="$(readlink -f "$ndk")"

# The build reads these; the Tauri CLI also writes them into the project.
export ANDROID_HOME="$sdk"
export ANDROID_SDK_ROOT="$sdk"
export ANDROID_NDK_HOME="$ndk"
export NDK_HOME="$ndk"

# adb ships inside the SDK and is often not on PATH.
if ! command -v adb >/dev/null 2>&1 && [[ -x "$sdk/platform-tools/adb" ]]; then
    export PATH="$sdk/platform-tools:$PATH"
fi

log "SDK: $sdk"
log "NDK: $ndk ($(sed -n 's/^Pkg.Revision = //p' "$ndk/source.properties" 2>/dev/null || echo 'unknown version'))"

# --- Rust targets ------------------------------------------------------------

for target in "${targets[@]}"; do
    triple="${abi_target[$target]}"
    if ! rustup target list --installed | grep -qx "$triple"; then
        log "installing the Rust target $triple"
        rustup target add "$triple"
    fi
done

# --- Android project ---------------------------------------------------------

cd "$root"
log "installing frontend dependencies"
pnpm install --frozen-lockfile

if $reinit && [[ -d "$android_dir" ]]; then
    log "removing the generated Android project"
    rm -rf "$android_dir"
fi
if [[ ! -d "$android_dir" ]]; then
    log "generating the Android project"
    pnpm tauri android init --ci
    warn "the project was generated fresh, so re-apply the three edits it does not carry:
      app/src/main/AndroidManifest.xml — android.hardware.camera.any as required=\"false\"
      app/build.gradle.kts          — the bundled com.google.mlkit:barcode-scanning dependency
      app/build.gradle.kts          — the signingConfigs block reading keystore.properties
    Without them the app demands a camera to install, scanning needs Google Play services,
    and --release produces an unsigned APK."
fi

if [[ "$build_profile" == "release" ]]; then
    keystore_properties="$android_dir/keystore.properties"
    if [[ ! -f "$keystore_properties" ]]; then
        if [[ -n "${CRABSEND_KEYSTORE:-}" ]]; then
            log "writing $keystore_properties from the environment"
            cat >"$keystore_properties" <<PROPERTIES
storeFile=$CRABSEND_KEYSTORE
storePassword=${CRABSEND_KEYSTORE_PASSWORD:?set CRABSEND_KEYSTORE_PASSWORD}
keyAlias=${CRABSEND_KEY_ALIAS:-crabsend}
keyPassword=${CRABSEND_KEY_PASSWORD:?set CRABSEND_KEY_PASSWORD}
PROPERTIES
        else
            die "a release build must be signed. Create a key and a properties file:
    keytool -genkeypair -v -keystore ~/.android/crabsend.jks -alias crabsend \\
        -keyalg RSA -keysize 2048 -validity 10000
    cat > $keystore_properties <<'EOF'
    storeFile=$HOME/.android/crabsend.jks
    storePassword=<password>
    keyAlias=crabsend
    keyPassword=<password>
    EOF
Or rerun without --release for a debug-signed APK."
        fi
    fi
fi

# --- Gradle wrapper ----------------------------------------------------------

# Gradle downloads its own distribution on first use. Where that host is not
# reachable, a mirror serving the same file is used instead.
readonly -a gradle_mirrors=(
    "https://mirrors.cloud.tencent.com/gradle"
    "https://mirror.nju.edu.cn/gradle"
)
gradle_properties="$android_dir/gradle/wrapper/gradle-wrapper.properties"
if [[ -f "$gradle_properties" ]]; then
    current_url="$(sed -n 's/^distributionUrl=//p' "$gradle_properties" | sed 's|\\:|:|g')"
    distribution="${current_url##*/}"
    distribution="${distribution%%[?#]*}"
    if [[ -z "$gradle_url" ]] && ! curl -sI --max-time 8 -o /dev/null "$current_url"; then
        for mirror in "${gradle_mirrors[@]}"; do
            if curl -sI --max-time 8 -o /dev/null "$mirror/$distribution"; then
                gradle_url="$mirror/$distribution"
                break
            fi
        done
        [[ -n "$gradle_url" ]] ||
            warn "$current_url is unreachable and no mirror answered; the build will likely fail"
    fi
    if [[ -n "$gradle_url" && "$gradle_url" != "$current_url" ]]; then
        log "fetching Gradle from $gradle_url"
        sed -i "s|^distributionUrl=.*|distributionUrl=$(printf '%s' "$gradle_url" | sed 's|:|\\:|g')|" "$gradle_properties"
    fi
fi

# --- build -------------------------------------------------------------------

build_args=(android build --ci -t "${targets[@]}")
$build_aab && build_args+=(--aab)
$split_per_abi && build_args+=(--split-per-abi)
if $build_aab; then
    :
else
    build_args+=(--apk)
fi
[[ "$build_profile" == "debug" ]] && build_args+=(--debug)

log "building: pnpm tauri ${build_args[*]}"
pnpm tauri "${build_args[@]}"

# --- artifacts ---------------------------------------------------------------

mapfile -t artifacts < <(
    find "$android_dir/app/build/outputs" \
        \( -name '*.apk' -o -name '*.aab' \) -newermt '-6 hours' 2>/dev/null | sort
)
((${#artifacts[@]})) || die "no APK or AAB was produced; see the build output above"

log "artifacts"
for artifact in "${artifacts[@]}"; do
    printf '    %s (%s)\n' "${artifact#"$root/"}" "$(du -h "$artifact" | cut -f1)"
done

if $install_apk; then
    apk="$(find "$android_dir/app/build/outputs/apk" -name '*.apk' -newermt '-6 hours' 2>/dev/null | sort | tail -1)"
    [[ -n "$apk" ]] || die "--install needs an APK; AABs are for the Play Store"
    command -v adb >/dev/null 2>&1 ||
        die "adb not found; install android-tools or add $sdk/platform-tools to PATH"
    log "installing $apk"
    adb install -r "$apk"
fi

cat <<'NOTES'

Note: Android suspends the app in the background, so the transfer server only
accepts connections while Crabsend is in the foreground. Some devices also drop
multicast traffic while the Wi-Fi radio is idle; use the "Scan" button, which
falls back to probing every host of the local subnet.
NOTES
