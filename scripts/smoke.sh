#!/usr/bin/env bash
# Full-stack smoke test: relay + two clients, one message, one file, receipts.
#
# Exercises the real binary (not the library) end to end:
#   relay `serve`  <-  client A `send`  ->  client B `run` (poller) -> receipt
#   and a 40 KB file, checked byte for byte against its sha256.
#
# Usage: ./scripts/smoke.sh [path-to-binary]
# Defaults to target/release/dns_lab (falls back to target/debug/dns_lab).
set -euo pipefail

cd "$(dirname "$0")/.."

BIN="${1:-}"
if [ -z "$BIN" ]; then
  for candidate in target/release/dns_lab target/debug/dns_lab; do
    if [ -x "$candidate" ]; then BIN="$candidate"; break; fi
  done
fi
if [ -z "$BIN" ] || [ ! -x "$BIN" ]; then
  echo "[!] no dns_lab binary found; run 'cargo build --release' first" >&2
  exit 1
fi

WORK="$(mktemp -d "${TMPDIR:-/tmp}/dns_lab-smoke.XXXXXX")"
KEY="smoke-$(head -c 24 /dev/urandom | base64 | tr -dc 'A-Za-z0-9')"
BASE_PORT="${DNS_LAB_SMOKE_PORT:-15353}"
RELAY_ADDR="127.0.0.1:$BASE_PORT"
A_ADDR="127.0.0.1:$((BASE_PORT + 1))"
B_ADDR="127.0.0.1:$((BASE_PORT + 2))"

cleanup() {
  kill "${RELAY_PID:-}" "${A_PID:-}" "${B_PID:-}" 2>/dev/null || true
  wait 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

cat > "$WORK/relay.toml" <<EOF
display_name = "relay"
node_id = "rrrr0001"
listen_addr = "$RELAY_ADDR"
zone = "lab.local"
authoritative = true
egress = "direct"
db_path = "relay.db"
EOF

write_client() {
  local name=$1 id=$2 addr=$3
  cat > "$WORK/$name.toml" <<EOF
display_name = "$name"
node_id = "$id"
listen_addr = "$addr"
zone = "lab.local"
egress = "direct"
relay = "$RELAY_ADDR"
direct_server = "$RELAY_ADDR"
poll_interval_secs = 1
shared_key = "$KEY"
db_path = "$name.db"
download_dir = "$WORK/inbox-$name"
EOF
}
write_client a aaaa1111 "$A_ADDR"
write_client b bbbb2222 "$B_ADDR"

echo "[*] starting relay, A and B on $RELAY_ADDR"
"$BIN" serve --config "$WORK/relay.toml" > "$WORK/relay.log" 2>&1 & RELAY_PID=$!
"$BIN" run   --config "$WORK/a.toml"     > "$WORK/a.log"     2>&1 & A_PID=$!
"$BIN" run   --config "$WORK/b.toml"     > "$WORK/b.log"     2>&1 & B_PID=$!
sleep 1.5

echo "[*] sending A -> B"
"$BIN" send --config "$WORK/a.toml" --to bbbb2222 --body "smoke test: hello over DNS"

# Wait for the receipt to land on A (status flips to "delivered").
for _ in $(seq 1 40); do
  if "$BIN" inbox --config "$WORK/a.toml" --limit 1 | grep -q delivered; then
    break
  fi
  sleep 0.25
done

fail=0
check() {
  local label=$1 cfg=$2 pattern=$3
  if "$BIN" inbox --config "$WORK/$cfg" --limit 5 | grep -q "$pattern"; then
    echo "  ok   $label"
  else
    echo "  FAIL $label"
    fail=1
  fi
}

echo "[*] verifying"
check "A records the message"        a.toml delivered
check "B received the message"       b.toml received
if "$BIN" inbox --config "$WORK/relay.toml" --limit 5 | grep -q "no messages"; then
  echo "  ok   relay kept no plaintext"
else
  echo "  FAIL relay stored someone else's mail"
  fail=1
fi
# The relay has no shared_key at all, so it cannot have decrypted anything.
if grep -q "relayed" "$WORK/relay.log" && ! grep -q "\[<\] message" "$WORK/relay.log"; then
  echo "  ok   relay forwarded blind (no key, no plaintext in the log)"
else
  echo "  FAIL relay log shows decryption"
  fail=1
fi
if grep -q "blind forwarder" "$WORK/relay.log"; then
  echo "  ok   relay announced itself as a blind forwarder"
else
  echo "  note: relay did not log its key status"
fi

echo "[*] sending a 40 KB file A -> B"
head -c 40000 /dev/urandom | base64 | head -c 40000 > "$WORK/sample.bin"
want=$(sha256sum "$WORK/sample.bin" | cut -d' ' -f1)
"$BIN" send --config "$WORK/a.toml" --to bbbb2222 --file "$WORK/sample.bin" > /dev/null

for _ in $(seq 1 80); do
  if ls "$WORK"/inbox-b/*-sample.bin >/dev/null 2>&1; then break; fi
  sleep 0.25
done
got_file=$(ls "$WORK"/inbox-b/*-sample.bin 2>/dev/null | head -1)
if [ -n "$got_file" ] && [ "$(sha256sum "$got_file" | cut -d' ' -f1)" = "$want" ]; then
  echo "  ok   file arrived intact ($(stat -c%s "$got_file") bytes)"
else
  echo "  FAIL file transfer"
  fail=1
fi
if "$BIN" files --config "$WORK/b.toml" | grep -q "sample.bin"; then
  echo "  ok   transfer recorded on the receiving side"
else
  echo "  FAIL transfer not recorded"
  fail=1
fi

if [ "$fail" -ne 0 ]; then
  echo
  echo "--- relay log ---"; tail -n 15 "$WORK/relay.log"
  echo "--- A log ---";     tail -n 15 "$WORK/a.log"
  echo "--- B log ---";     tail -n 15 "$WORK/b.log"
  exit 1
fi

echo "[+] smoke test passed"
