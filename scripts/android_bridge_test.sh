#!/usr/bin/env bash
# Host smoke test for the Android JNI bridge: no phone, no emulator, no SDK UI.
#
# Builds the library with `jni-bridge`, compiles the tiny Java harness, starts a
# real peer node with the CLI and checks that a message sent through the bridge
# (the exact code path the Android app uses) is received and receipted.
#
#   ./scripts/android_bridge_test.sh
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
WORK=$(mktemp -d "${TMPDIR:-/tmp}/dl-bridge.XXXXXX")
PIDS=()
cleanup() {
  for p in "${PIDS[@]:-}"; do [ -n "$p" ] && kill "$p" 2>/dev/null || true; done
  rm -rf "$WORK"
}
trap cleanup EXIT

echo "[*] building libdns_lab.so with the JNI bridge"
cargo build --features jni-bridge --lib

BIN="$ROOT/target/debug/dns_lab"
[ -x "$BIN" ] || { echo "[!] need a debug binary: cargo build" >&2; exit 1; }

KEY="bridge-test-$(date +%s)"
mkdir -p "$WORK/java/dns_lab" "$WORK/peer"
cat > "$WORK/java/dns_lab/dns_lab.toml" <<EOF
display_name = "java"
node_id = "1111aaaa"
listen_addr = "127.0.0.1:17001"
zone = "lab.local"
egress = "direct"
shared_key = "$KEY"
db_path = "$WORK/java/dns_lab/java.db"
download_dir = "$WORK/java/dns_lab/downloads"
EOF
cat > "$WORK/peer/peer.toml" <<EOF
display_name = "peer"
node_id = "2222bbbb"
listen_addr = "127.0.0.1:17000"
zone = "lab.local"
egress = "direct"
shared_key = "$KEY"
db_path = "$WORK/peer/peer.db"
download_dir = "$WORK/peer/downloads"

[[peers]]
id = "1111aaaa"
name = "java"
addr = "127.0.0.1:17001"
via = "direct"
EOF

echo "[*] starting the peer node on 127.0.0.1:17000"
"$BIN" run --config "$WORK/peer/peer.toml" > "$WORK/peer.log" 2>&1 &
PIDS+=("$!")
sleep 1.5

echo "[*] compiling the harness against the real Native.java"
mkdir -p "$WORK/classes"
javac -d "$WORK/classes" android/src/com/dnslab/messenger/Native.java \
  android/test/NativeHarness.java

echo "[*] running the bridge"
java -Djava.library.path="$ROOT/target/debug" -cp "$WORK/classes" \
  com.dnslab.messenger.NativeHarness "$WORK/java" 2222bbbb 127.0.0.1:17000

echo "[*] peer inbox"
"$BIN" inbox --config "$WORK/peer/peer.toml" --limit 3
if ! "$BIN" inbox --config "$WORK/peer/peer.toml" --limit 3 | grep -q "received"; then
  echo "[!] the peer never stored the message" >&2
  tail -20 "$WORK/peer.log" >&2
  exit 1
fi
echo "[+] PASS: Java -> JNI -> DNS -> peer -> receipt"
