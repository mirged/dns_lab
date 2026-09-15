# Android build

The desktop UI is egui/eframe, which runs natively on Android through
`android-activity` (NativeActivity + OpenGL ES). The same Rust protocol, crypto,
SQLite and DNS transport are reused; only the entry point and storage paths differ.

The Android app is a **client**: it sends queries via a recursive resolver and
polls the relay mailbox for replies (resolver/relay mode). Android cannot bind
port 53, but it never needs to — only the VPS relay does.

## Prerequisites

- Rust target: `rustup target add aarch64-linux-android`
- `cargo-apk2` (APK packager): `cargo install cargo-apk2`
- Android SDK with `platforms;android-34` and `build-tools;34.0.0`
- Android NDK r27 (or similar)
- A JDK 17
- Environment, e.g.:

```sh
export ANDROID_HOME=$HOME/android-sdk
export ANDROID_NDK_HOME=$HOME/android-sdk/android-ndk-r27c
export JAVA_HOME=$HOME/android-sdk/jdk
```

## Signing

Release APKs must be signed. `cargo-apk2` reads the keystore from environment
variables, so no secrets are committed. Create a keystore once:

```sh
keytool -genkeypair -v \
  -keystore android/dnslab-release.jks \
  -alias dnslab -keyalg RSA -keysize 2048 -validity 10000 \
  -storepass "$STORE_PASS" -keypass "$STORE_PASS" \
  -dname "CN=DNS Lab, O=DNS Lab, C=US"
```

Then export **absolute** paths (the signer runs from the APK output dir):

```sh
export CARGO_APK_RELEASE_KEYSTORE="$PWD/android/dnslab-release.jks"
export CARGO_APK_RELEASE_KEYSTORE_PASSWORD="$STORE_PASS"
```

## Build

```sh
./deploy/android/build-apk.sh
# -> target/release/apk/dns-lab.apk  (signed, arm64-v8a, ~3 MB)
```

Or manually:

```sh
cargo apk2 build --release --lib --target aarch64-linux-android
```

## Install

```sh
adb install -r target/release/apk/dns-lab.apk
```

## Runtime behaviour

- Config + SQLite live in the app's private data dir:
  `/data/data/com.dnslab.messenger/files/dns_lab/dns_lab.toml` and `.db`.
- On first launch the app writes a default config using **resolver** egress
  (`1.1.1.1:53`). Set your zone (`t.example.com`) and shared key in the
  **Setup** tab, then **Restart node**.
- Add contacts by node id (from `dns_lab id` on the peer) in the Setup tab.
- **UI scale**: the top bar has `A-` / `A+` controls and the Setup tab has a
  "UI scale" field (persisted). Android defaults to 1.4x; if the platform ever
  reports a missing density scale the app auto-compensates on first frame.
- **Safe area**: the app reads the NativeActivity content rect each frame and
  insets the UI away from the status/navigation bars (falling back to a 24dp top
  inset when none is reported), so the top controls stay tappable on
  edge-to-edge Android.
- Permissions declared: `INTERNET`, `ACCESS_NETWORK_STATE`.

## Notes / limitations

- Only `aarch64-linux-android` is configured in `[package.metadata.android]`.
  Add `armv7-linux-androideabi` / `x86_64-linux-android` to `build_targets` for
  older or emulated devices (and set the matching `CC_*`/`AR_*` env vars).
- The Android relayer would require binding port 53, which is not possible on a
  non-rooted device; run the authoritative relay on the VPS instead.
