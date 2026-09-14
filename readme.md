# Crabsend

Cross-platform file transfer over the local network, built with [Tauri 2](https://tauri.app)
and compatible with [LocalSend](https://github.com/localsend/localsend) on the wire
(protocol v2.2). Devices running Crabsend, LocalSend, or any other implementation of the
protocol find each other automatically and exchange files directly — no server, no account,
no cloud.

## What it does

- **Discovery** — multicast announcements on `224.0.0.167:53317` (IPv4) and `ff12::fd3a:e420`
  (IPv6), answered over HTTPS, plus a `/24` subnet scan and manual "add by IP" as fallbacks.
- **Pairing** — *Show QR* puts this device's address, port and certificate fingerprint into a
  code; *Scan QR* on the other device reads it and connects to exactly that device, without
  discovery. The fingerprint travels off the network, so the scanning side pins the connection
  to it and a device answering in its place is refused during the TLS handshake. A paired
  device is remembered in `peers.json`, its address refreshed whenever *Scan* reaches it again.
  This is Crabsend's own extension: LocalSend v2.2 defines no pairing or QR concept, and no
  LocalSend client scans codes today.
- **Sending** — pick files and folders or drop them on the window, choose a device, send.
  Files are uploaded in parallel (2 at a time), with per-file progress, cancellation, and one
  automatic retry per file after a checksum mismatch.
- **Receiving** — an accept/decline prompt lists the incoming files; a subset can be accepted
  and a per-device PIN can gate senders. Received files keep their folder structure and their
  original timestamps, and are verified against the SHA-256 the sender provides.
- **History** — the last 100 transfers with peer, size, status and "open folder".
- **Transport security** — HTTPS with a self-signed certificate; peers are identified by the
  certificate fingerprint (uppercase-hex SHA-256) and the peer's claim is only trusted when
  its own certificate proves it. Plain HTTP mode is available for compatibility.

Not implemented: the browser download/share API (`prepare-download`/`download`), which is why
this app announces `"download": false` and peers will not offer to send it a link. Also absent:
favourites, clipboard/text messages, and multi-recipient sends.

## Layout

```
crates/crabsend-core/   LocalSend v2 protocol: DTOs, identity, discovery, HTTP(S) server, client
  src/model.rs          wire types (JSON field names are the contract with peers)
  src/crypto.rs         self-signed identity, fingerprints, hashing
  src/tls.rs            server TLS: accepts any self-consistent peer certificate
  src/discovery.rs      multicast sockets, announce burst, probes, subnet scan
  src/server.rs         the v2 API (register/info/prepare-upload/upload/cancel)
  src/client.rs         pinned-fingerprint HTTP client (register, prepare-upload, upload)
  src/fs_util.rs        received-file naming: sanitizing, containment, collisions
  tests/end_to_end.rs   protocol tests: both sides of the wire, over loopback
src-tauri/              the application: settings, sessions, Tauri commands and events
  src/state.rs          transfer sessions, server/discovery lifecycle
  src/commands.rs       the IPC surface the UI calls
src/                    the UI: plain TypeScript, no framework
```

## Running it

```bash
pnpm install
pnpm tauri dev      # development window
pnpm tauri build    # bundles for the current platform
```

The icon set in `src-tauri/icons` (and the Android mipmaps under `src-tauri/gen/android`) comes
from `app-icon.png`:

```bash
pnpm tauri icon app-icon.png --ios-color '#6AC5B2'
```

The color is not cosmetic: the artwork carries its own teal background with transparent corners,
so the flag fills those corners — without it Android's adaptive icon and the iOS icon fall back
to white and show a teal square floating on it.

Linux needs WebKitGTK and the usual Tauri build dependencies
(`webkit2gtk-4.1`, `libayatana-appindicator`, `librsvg`, `patchelf`).

### Arch Linux

```bash
scripts/build-arch.sh              # build, then package as target/arch-pkg/crabsend-bin-<version>-<rel>-<arch>.pkg.tar.zst
scripts/build-arch.sh --install    # ... and install it with pacman
scripts/build-arch.sh --install-deps   # install missing build dependencies first
scripts/build-arch.sh --no-build   # package the release binary that is already built
```

The package installs `/usr/bin/crabsend`, a desktop entry and hicolor icons, and depends on
`webkit2gtk-4.1`, `gtk3`, `libayatana-appindicator` and `librsvg`.

### Android

```bash
scripts/build-android.sh                    # debug APK for aarch64
scripts/build-android.sh --release          # release APK (needs a signing key, see below)
scripts/build-android.sh --targets all      # aarch64, armv7, i686, x86_64
scripts/build-android.sh --split-per-abi    # one APK per ABI
scripts/build-android.sh --aab              # Android App Bundle
scripts/build-android.sh --install          # install on a connected device with adb
```

The script detects the SDK (including the default `~/Android/Sdk`) and the newest NDK below
it, exports `ANDROID_HOME`/`ANDROID_NDK_HOME`, installs the Rust targets it needs, generates
`src-tauri/gen/android` on first run, and prints the resulting artifacts.

`src-tauri/gen/android` is part of the repository, with three hand-made edits: the manifest
declares the camera as optional so the app installs on devices without one, and the app's Gradle
file bundles the ML Kit barcode model (see the note below) and signs the release build from
`keystore.properties`. Regenerating the project with `--reinit` drops all three, and the script
says so.

It also pins `JAVA_HOME` to a JDK between 17 and 24 (**JDK 21** is what it looks for first,
`sudo pacman -S jdk21-openjdk`). Gradle 8.14 refuses to load bytecode from a newer JDK, and
Android Studio ships its own JetBrains Runtime (`/opt/android-studio/jbr`) that is newer than
that — using it fails with `Unsupported class file major version 69`.

A debug APK carries unstripped debug symbols of every native library, so it is very large
(hundreds of MB). Use `--release` with a signing key for anything you install for real:

```bash
keytool -genkeypair -v -keystore ~/.android/crabsend.jks -alias crabsend \
    -keyalg RSA -keysize 2048 -validity 10000
```

The key has to be named in `src-tauri/gen/android/keystore.properties`, which is read by
`app/build.gradle.kts` and is not under version control:

```properties
storeFile=/home/you/.android/crabsend.jks
storePassword=<password>
keyAlias=crabsend
keyPassword=<password>
```

Without that file a release build is left unsigned, and `--release` refuses to run at all. Keep
the keystore *and* its password: an update for an installed app must be signed with the same key.

Two platform limits are worth knowing: Android suspends the application in the background, so
the transfer server only accepts connections while Crabsend is in the foreground; and some
devices drop multicast traffic with the Wi-Fi radio idle, which is why the *Scan* button also
probes every host of the local subnet.

Files received on a phone land in the shared directory that belongs to the application
(`/storage/emulated/0/Android/media/dev.crabsend.app/Crabsend/`), which needs no storage
permission and is visible to every file manager and gallery. Android 11 and later hide the
application's *files* directory (`Android/data/…`) from file managers and from other
applications, so a settings file that still pointed there is moved along with the files. Two
actions a phone cannot offer are hidden there: reviewing a received file in a file manager (a
phone has no such concept, and no way to open a file of ours from outside without a
`FileProvider` read grant) and choosing a directory (Android's dialogs cannot pick one). Files
picked through Android's picker come back as `content://` URIs, which the application reads into
its cache before hashing and sending, because only the provider that issued the URI can read the
bytes.

Only the mobile build can read a pairing code: the camera side of the barcode plugin has no
desktop implementation, so the desktop shows codes and the phone scans them. The plugin brings
CameraX and the *unbundled* ML Kit client, whose model Google Play services download on first
use — which never happens on a device without the Play Store. `app/build.gradle.kts` therefore
adds `com.google.mlkit:barcode-scanning`, which carries the same model and its native library
inside the app, at a cost of a few megabytes, so scanning works on a de-Googled phone and on a
`google_apis` emulator image as well. Two behaviours of the plugin are worth knowing: a device
that reports no camera at all leaves the request unanswered (the interface gives up after 45
seconds and closes the camera again), and a scan interrupted with the system back button closes
the app.

Tests:

```bash
cargo test                      # protocol unit + end-to-end tests
LOCALSEND_CLI=/path/to/localsend-cli cargo test -p crabsend-app --test official_peer
```

The last command checks interoperability against LocalSend's own Rust CLI: the app sends a
file to that peer and the received bytes are compared. Without the environment variable the
test is skipped.

## Settings and state

Both live in the platform configuration directory (`~/.config/dev.crabsend.app` on Linux):

- `settings.json` — alias, device model/type, port, encryption, download directory, PIN,
  auto-accept, checksum creation.
- `identity.pem` — the device certificate and private key (mode `0600`), created on first
  start. Its fingerprint *is* this device's identity; deleting it makes the device appear new
  to everyone.
- `peers.json` — the devices paired by QR code. Each entry is a name, an address and the
  fingerprint that was proven by the scan; the address is a hint that *Scan* refreshes, and
  *Forget* in the pairing dialog drops the entry.

Changing the port, the encryption mode, the alias, the device model/type or the download
directory restarts the transfer server and discovery.

## Protocol notes

The implementation follows the [LocalSend protocol v2.2](https://github.com/localsend/protocol)
with the following choices worth knowing:

- The fingerprint is always the certificate fingerprint, in both HTTPS and plain-HTTP mode, so
  a peer that has seen this device keeps recognising it when encryption is toggled.
- The server requires a client certificate (as LocalSend does when it is not serving browser
  pages), and accepts any time-valid self-signed certificate; identity is decided by
  fingerprint, not by a certificate authority.
- A `prepare-upload` request that the user does not answer within two minutes is declined, and
  an accepted session that receives no bytes for ten minutes is abandoned, so a sender that
  walks away cannot block the single session slot for good.
- A pairing code is plain JSON, so it stays readable when a camera cannot be used:
  `{"v":1,"protocol":"https","addresses":["192.168.1.42"],"port":53317,"fingerprint":"4BAD…",
  "alias":"Desk","deviceModel":"Arch Linux","deviceType":"desktop"}`. Every address is tried at
  once — a multi-homed host cannot know which of its addresses the scanner can reach — and the
  peer's name and model come from the answer rather than the code. A code written by a newer
  version is refused instead of being misread.
- The scanner only accepts a peer whose certificate proves the fingerprint in the code. Over
  plain HTTP there is no certificate to pin, so a code then rules out reaching the wrong device
  but not a peer that lies about its fingerprint; HTTPS is the mode with the guarantee.
- Received file names are sanitized component by component (`..`, absolute paths and platform
  metacharacters cannot escape the download directory) and collisions get a ` (1)` suffix.
