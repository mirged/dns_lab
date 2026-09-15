#!/usr/bin/env bash
# Build a signed release APK for aarch64 Android devices.
#
# Required env:
#   ANDROID_HOME, ANDROID_NDK_HOME, JAVA_HOME
#   CARGO_APK_RELEASE_KEYSTORE            (absolute path to a .jks)
#   CARGO_APK_RELEASE_KEYSTORE_PASSWORD   (keystore/key password)
set -euo pipefail

: "${ANDROID_HOME:?set ANDROID_HOME to your Android SDK root}"
: "${ANDROID_NDK_HOME:?set ANDROID_NDK_HOME to your NDK}"
: "${JAVA_HOME:?set JAVA_HOME to a JDK 17}"
: "${CARGO_APK_RELEASE_KEYSTORE:?set CARGO_APK_RELEASE_KEYSTORE (absolute path)}"
: "${CARGO_APK_RELEASE_KEYSTORE_PASSWORD:?set CARGO_APK_RELEASE_KEYSTORE_PASSWORD}"

NDK_BIN="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin"
export PATH="$JAVA_HOME/bin:$ANDROID_HOME/platform-tools:$ANDROID_HOME/cmdline-tools/latest/bin:$ANDROID_HOME/build-tools/34.0.0:$NDK_BIN:$PATH"

export CC_aarch64_linux_android="$NDK_BIN/aarch64-linux-android24-clang"
export AR_aarch64_linux_android="$NDK_BIN/llvm-ar"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$NDK_BIN/aarch64-linux-android24-clang"

cd "$(dirname "$0")/../.."
cargo apk2 build --release --lib --target aarch64-linux-android
echo
echo "APK: target/release/apk/dns-lab.apk"
