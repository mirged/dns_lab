# Android build

The phone app is a **plain Java UI** (`android/src/com/dnslab/messenger/`) over a
small JNI bridge (`src/bridge.rs`) into the same Rust core the desktop GUI uses:
same protocol, crypto, SQLite, chunking, receipts and Codec 2 voice. No AndroidX,
no Gradle, no egui — cargo-apk2 runs `javac` against `android.jar`, dexes the
result with `d8`, and packages it next to `libdns_lab.so`. One toolchain, one
script, and an APK that is ~1.4 MB instead of ~3.5 MB because the graphics stack
is gone.

The app is a **client**: it sends queries directly to a peer (LAN mode) or through
a recursive resolver to a relay mailbox (resolver mode). Android cannot bind port
53, but it never needs to — only the relay does.

## Architecture

| layer | file | job |
|-------|------|-----|
| Java UI | `android/src/com/dnslab/messenger/MainActivity.java` | list, chat, setup, log; 700 ms poll |
| JNI surface | `android/src/com/dnslab/messenger/Native.java` | 14 native methods, records as `\u001f`-joined fields |
| Bridge | `src/bridge.rs` | sessions, config, contacts, messages, send, voice |
| Core | `src/net/`, `src/db.rs`, `src/voice/` | unchanged |

Two things to know before editing the bridge:

- `Native.init(Activity)` hands the JVM and application context to
  `ndk_context`, which `cpal`'s AAudio backend needs. `android-activity` used to
  do that implicitly; with a plain `Activity` the app must call it, or the first
  audio call panics.
- The bridge is compiled on the host too (`--features jni-bridge`), which is how
  it is tested without a phone — see *Testing the bridge* below.

## Prerequisites

- Rust target: `rustup target add aarch64-linux-android`
- `cargo-apk2` (APK packager): `cargo install cargo-apk2`
- Android SDK with `platforms;android-34` and `build-tools;34.0.0`
- Android NDK r27 (or similar)
- **A JDK 17.** Not 21, not 25: R8 in build-tools 34 refuses class files newer
  than Java 17 (`Unsupported class file major version 65/69`) and only fails
  after the native link has already succeeded. `build-apk.sh` prefers a JDK 17
  at `$ANDROID_HOME/jdk` and rejects a newer one up front.
- Environment, e.g.:

```sh
export ANDROID_HOME=$HOME/android-sdk
export ANDROID_NDK_HOME=$HOME/android-sdk/ndk/android-ndk-r27c
export JAVA_HOME=$HOME/android-sdk/jdk
```

With nothing exported, `build-apk.sh` looks for the SDK at `~/android-sdk`, the
newest NDK under `~/android-sdk/ndk`, and a JDK 17 at `~/android-sdk/jdk`.
`./deploy/android/install-sdk.sh` puts all of that in place without root
(cmdline-tools + platform-tools + build-tools + NDK ~700 MB + Temurin 17
~190 MB; every download resumes if interrupted).

Kits that are already on the box: `sdkmanager` from the distro package works
for the SDK parts, but it is old — `install-sdk.sh` fetches the current
command-line tools itself.

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
# -> target/release/apk/dns-lab.apk  (signed, arm64-v8a, ~1.4 MB)
```

Or manually:

```sh
cargo apk2 build --release --lib --target aarch64-linux-android
```

## Install

```sh
adb install -r target/release/apk/dns-lab.apk
```

## Testing the bridge without a phone

The JNI layer is the part that can actually be wrong, and it does not need
Android to run: build the library with the `jni-bridge` feature and call it from
a desktop JVM.

```sh
./scripts/android_bridge_test.sh
```

That script starts a real peer node with the CLI, then runs
`android/test/NativeHarness.java`, which opens a node through `Native.start`,
adds a contact, sends a message and waits for the receipt. It prints
`Java -> JNI -> DNS -> peer -> receipt` and fails loudly otherwise. What it
cannot cover is the Java UI itself and AAudio — those need a device.

## Runtime behaviour

- Config + SQLite live in the app's private data dir:
  `/data/data/com.dnslab.messenger/files/dns_lab/dns_lab.toml` and `.db`.
- On first launch the app writes a default config: `display_name = "android"`,
  `listen_addr = 0.0.0.0:5354`, `zone = lab.local`, direct egress and **no
  shared key**. Fill the key in the Setup tab, add a contact, then *Save and
  restart node*.
- Every screen is one column: a contact list, a conversation with large touch
  targets, Setup, and the DNS log. Back returns from a conversation to the list.
- There is no UI-scale control any more: sizes are in dp and the platform
  density does the work. Status- and navigation-bar insets are handled by the
  normal Activity window (not edge-to-edge), so nothing sits under them.
- Permissions declared: `INTERNET`, `ACCESS_NETWORK_STATE`, `RECORD_AUDIO`.
  The microphone is requested at runtime, the first time you tap the talk
  button.

## Voice on the phone

Push-to-talk is Codec 2 in pure Rust, so the APK needs nothing extra from the
NDK — but audio I/O is `cpal`, which on Android is **AAudio**. That is why
`min_sdk_version` is 26 (Android 8.0): chat worked on 24, voice cannot.

**The talk button is a toggle on Android**: tap to start, tap again to send
(it turns red and reads "Talking - tap to send"). Press-and-hold is a desktop
affordance — one thumb cannot hold a button and read the transcript — and egui's
pointer-down model is not what touch delivers anyway. That is the whole reason
the phone UI exists separately.

`RECORD_AUDIO` is a *dangerous* permission, so it starts out denied. The first
tap shows the system prompt; if you deny it, the app says "Microphone
unavailable" and you can grant it under

    Settings → Apps → DNS Lab → Permissions → Microphone → Allow

Playback needs no permission.

Through a mailbox relay, set these (Setup tab, then *Save and restart node*) or a
press arrives chopped across several polls:

    poll_interval_secs = 1
    edns_udp_size = 4096

`dns_lab run --voice-save DIR` on a PC gives you something to listen to while
testing the phone, and `dns_lab voice send --to <phone-id> --wav clip.wav`
gives you something to send *to* it without a second phone.

## Notes / limitations

- Only `aarch64-linux-android` is configured in `[package.metadata.android]`.
  Add `armv7-linux-androideabi` / `x86_64-linux-android` to `build_targets` for
  older or emulated devices (and set the matching `CC_*`/`AR_*` env vars).
- The Android relayer would require binding port 53, which is not possible on a
  non-rooted device; run the authoritative relay on the VPS instead.
