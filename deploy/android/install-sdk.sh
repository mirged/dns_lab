#!/usr/bin/env bash
# Install a user-local Android SDK + NDK for building the dns_lab APK.
# No root required: everything lands in $ANDROID_HOME (default ~/android-sdk).
#
#   ./deploy/android/install-sdk.sh          # full install (SDK + build-tools + NDK)
#   ./deploy/android/install-sdk.sh --minimal  # SDK + build-tools, no NDK (~800 MB less)
#
# The NDK is the big one (~700 MB). The download resumes, so re-running after a
# dropped connection continues where it stopped.
set -euo pipefail

SDK="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/android-sdk}}"
MINIMAL=0
[ "${1:-}" = "--minimal" ] && MINIMAL=1

TOOLS_ZIP=commandlinetools-linux-11076708_latest.zip
TOOLS_URL="https://dl.google.com/android/repository/$TOOLS_ZIP"

mkdir -p "$SDK/cmdline-tools"
cd "$SDK"

if [ ! -x "$SDK/cmdline-tools/latest/bin/sdkmanager" ]; then
  if [ ! -s "$TOOLS_ZIP" ] || ! unzip -tq "$TOOLS_ZIP" >/dev/null 2>&1; then
    echo "[*] downloading command-line tools (~150 MB) — resumes if interrupted"
    curl -fL -C - --retry 5 --retry-delay 3 -o "$TOOLS_ZIP" "$TOOLS_URL"
  fi
  rm -rf cmdline-tools/tmp
  mkdir -p cmdline-tools/tmp
  unzip -q "$TOOLS_ZIP" -d cmdline-tools/tmp
  rm -rf cmdline-tools/latest
  mv cmdline-tools/tmp/cmdline-tools cmdline-tools/latest
  rmdir cmdline-tools/tmp
  rm -f "$TOOLS_ZIP"
fi

export ANDROID_HOME="$SDK"
export ANDROID_SDK_ROOT="$SDK"
SM="$SDK/cmdline-tools/latest/bin/sdkmanager"

echo "[*] accepting licences"
yes | "$SM" --sdk_root="$SDK" --licenses >/dev/null 2>&1 || true

PACKAGES=("platform-tools" "platforms;android-34" "build-tools;34.0.0")
if [ "$MINIMAL" -eq 0 ]; then
  PACKAGES+=("ndk;27.2.12479018")
fi

echo "[*] installing: ${PACKAGES[*]}"
"$SM" --sdk_root="$SDK" "${PACKAGES[@]}"

# JDK 17 beside the SDK. R8 in build-tools 34 cannot read class files newer than
# Java 17, and the distro JDK is often 21/25; build-apk.sh prefers this one.
if [ ! -x "$SDK/jdk/bin/javac" ]; then
  echo "[*] downloading Temurin JDK 17 (~190 MB)"
  curl -fL --retry 5 --retry-delay 3 -o "$SDK/jdk17.tar.gz" \
    "https://api.adoptium.net/v3/binary/latest/17/ga/linux/x64/jdk/hotspot/normal/eclipse"
  tar xzf "$SDK/jdk17.tar.gz" -C "$SDK"
  rm -rf "$SDK/jdk"
  mv "$SDK"/jdk-17* "$SDK/jdk"
  rm -f "$SDK/jdk17.tar.gz"
fi
echo "[+] JDK $("$SDK/jdk/bin/javac" -version 2>&1) at $SDK/jdk"

echo "[+] SDK ready at $SDK"
ls -d "$SDK"/ndk/* 2>/dev/null || echo "    (no NDK: --minimal install)"
echo "Next: rustup target add aarch64-linux-android && cargo install cargo-apk2"
echo "Then: ./deploy/android/build-apk.sh"
