#!/usr/bin/env bash
# Build a signed release APK for aarch64 Android devices.
#
# Everything can be overridden from the environment; with nothing set it uses
# the user-local install the docs describe:
#   ANDROID_HOME      ~/android-sdk (or ANDROID_SDK_ROOT)
#   ANDROID_NDK_HOME  newest ~/android-sdk/ndk/*
#   JAVA_HOME         the JDK that owns `java` on PATH
#   keystore          ~/.android-certs/dnslab-release.jks
#   keystore password ~/.android-certs/dnslab.pass (chmod 600, never committed)
#
# Required: an SDK with platforms;android-34 and build-tools;34.0.0, an NDK
# (r27 verified), `rustup target add aarch64-linux-android`, and
# `cargo install cargo-apk2`.
set -euo pipefail
cd "$(dirname "$0")/../.."

ANDROID_HOME="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/android-sdk}}"
if [ -z "${ANDROID_NDK_HOME:-}" ]; then
  ANDROID_NDK_HOME="$(ls -d "$ANDROID_HOME"/ndk/* 2>/dev/null | sort -V | tail -1 || true)"
fi
if [ -z "${JAVA_HOME:-}" ]; then
  # R8 from build-tools 34 is Java 17-era and refuses newer class files
  # ("Unsupported class file major version 65/69"), so a JDK 21/25 on PATH is
  # not a usable default. Prefer a JDK 17 beside the SDK, then the packaged one.
  for candidate in "$ANDROID_HOME/jdk" /usr/lib/jvm/java-17-openjdk-amd64; do
    if [ -x "$candidate/bin/javac" ]; then JAVA_HOME="$candidate"; break; fi
  done
fi
if [ -z "${JAVA_HOME:-}" ]; then
  JAVA_BIN="$(command -v java || true)"
  if [ -n "$JAVA_BIN" ]; then
    JAVA_HOME="$(dirname "$(dirname "$(readlink -f "$JAVA_BIN")")")"
  fi
fi
# Fail before a 10-minute build does it for us.
if [ -n "${JAVA_HOME:-}" ] && [ -x "$JAVA_HOME/bin/javac" ]; then
  jdk_major="$("$JAVA_HOME/bin/javac" -version 2>&1 | sed -n 's/^javac \([0-9]*\).*/\1/p')"
  if [ -n "$jdk_major" ] && [ "$jdk_major" -gt 17 ]; then
    echo "[!] JDK $jdk_major at $JAVA_HOME: R8 in build-tools 34 only reads class" >&2
    echo "    files up to Java 17. Install Temurin 17 at \$ANDROID_HOME/jdk or" >&2
    echo "    set JAVA_HOME to a 17 (see deploy/android/README.md)." >&2
    exit 1
  fi
fi

KEYSTORE="${CARGO_APK_RELEASE_KEYSTORE:-$HOME/.android-certs/dnslab-release.jks}"
if [ -z "${CARGO_APK_RELEASE_KEYSTORE_PASSWORD:-}" ] \
  && [ -f "$HOME/.android-certs/dnslab.pass" ]; then
  CARGO_APK_RELEASE_KEYSTORE_PASSWORD="$(cat "$HOME/.android-certs/dnslab.pass")"
fi

problem=""
[ -d "$ANDROID_HOME" ] || problem="no SDK at $ANDROID_HOME (set ANDROID_HOME)"
if [ -z "${ANDROID_NDK_HOME:-}" ] || [ ! -d "${ANDROID_NDK_HOME:-/nonexistent}" ]; then
  problem="no NDK under $ANDROID_HOME/ndk (sdkmanager \"ndk;27.2.12479018\")"
fi
[ -n "${JAVA_HOME:-}" ] || problem="no JDK (set JAVA_HOME)"
[ -f "$KEYSTORE" ] || problem="no keystore at $KEYSTORE (see deploy/android/README.md)"
[ -n "${CARGO_APK_RELEASE_KEYSTORE_PASSWORD:-}" ] \
  || problem="no keystore password (set CARGO_APK_RELEASE_KEYSTORE_PASSWORD or create ~/.android-certs/dnslab.pass)"
command -v cargo-apk2 >/dev/null || problem="cargo-apk2 not installed (cargo install cargo-apk2)"
rustup target list --installed 2>/dev/null | grep -qx aarch64-linux-android \
  || problem="rust target missing (rustup target add aarch64-linux-android)"
if [ -n "$problem" ]; then
  echo "[!] $problem" >&2
  exit 1
fi

export ANDROID_HOME ANDROID_NDK_HOME JAVA_HOME
export CARGO_APK_RELEASE_KEYSTORE="$KEYSTORE"
export CARGO_APK_RELEASE_KEYSTORE_PASSWORD

# API level 26 matches [package.metadata.android] min_sdk_version: cpal's
# AAudio backend (push-to-talk audio) does not exist before Android 8.0.
NDK_BIN="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin"
CLANG="$NDK_BIN/aarch64-linux-android26-clang"
if [ ! -x "$CLANG" ]; then
  echo "[!] $CLANG is missing: this NDK does not provide an API 26 linker" >&2
  exit 1
fi
export PATH="$JAVA_HOME/bin:$ANDROID_HOME/platform-tools:$ANDROID_HOME/cmdline-tools/latest/bin:$ANDROID_HOME/build-tools/34.0.0:$NDK_BIN:$PATH"

export CC_aarch64_linux_android="$CLANG"
export AR_aarch64_linux_android="$NDK_BIN/llvm-ar"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$CLANG"

echo "[*] SDK $ANDROID_HOME"
echo "[*] NDK $ANDROID_NDK_HOME"
echo "[*] JDK $JAVA_HOME"
echo "[*] key $KEYSTORE"
cargo apk2 build --release --lib --target aarch64-linux-android
echo
echo "APK: target/release/apk/dns-lab.apk"
echo "Install: adb install -r target/release/apk/dns-lab.apk"
