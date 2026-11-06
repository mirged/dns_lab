#!/usr/bin/env bash
# Throughput bench: send 1/10/100 KB through a local relay, with and without
# chunk pipelining, under a simulated one-way delay.
#
#   ./scripts/bench.sh            # 25 ms simulated delay
#   DELAY_MS=0 ./scripts/bench.sh # loopback, no delay
#
# Measures wall-clock time for the send (which includes waiting for every ack)
# and reports the query count the sender had to emit.
set -euo pipefail

cd "$(dirname "$0")/.."

BIN="${1:-}"
if [ -z "$BIN" ]; then
  for candidate in target/release/dns_lab target/debug/dns_lab; do
    if [ -x "$candidate" ]; then BIN="$candidate"; break; fi
  done
fi
if [ -z "$BIN" ] || [ ! -x "$BIN" ]; then
  echo "[!] build first: cargo build --release" >&2
  exit 1
fi

DELAY_MS="${DELAY_MS:-25}"
KEY="bench-passphrase"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/dns_lab-bench.XXXXXX")"
BASE=15400

cleanup() {
  kill "${RELAY_PID:-}" "${RECV_PID:-}" 2>/dev/null || true
  wait 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

cat > "$WORK/relay.toml" <<EOF
node_id = "rrrr0001"
listen_addr = "127.0.0.1:$BASE"
zone = "lab.local"
authoritative = true
relay_mode = "authoritative"
shared_key = "$KEY"
db_path = "relay.db"
rate_limit_per_sec = 0   # the bench saturates on purpose
verbose = false
EOF

cat > "$WORK/recv.toml" <<EOF
node_id = "bbbb2222"
listen_addr = "127.0.0.1:$((BASE + 1))"
zone = "lab.local"
egress = "direct"
direct_server = "127.0.0.1:$BASE"
relay = "127.0.0.1:$BASE"
poll_interval_secs = 1
shared_key = "$KEY"
db_path = "recv.db"
verbose = false
EOF

DNS_LAB_DELAY_MS="$DELAY_MS" "$BIN" serve --config "$WORK/relay.toml" >/dev/null 2>&1 & RELAY_PID=$!
"$BIN" run   --config "$WORK/recv.toml"  >/dev/null 2>&1 & RECV_PID=$!
sleep 1.5

write_sender() {
  local window=$1
  cat > "$WORK/send.toml" <<EOF
node_id = "aaaa1111"
listen_addr = "127.0.0.1:$((BASE + 2))"
zone = "lab.local"
egress = "direct"
direct_server = "127.0.0.1:$BASE"
shared_key = "$KEY"
db_path = "send.db"
window = $window
verbose = false
EOF
}

now_ms() { date +%s%N | cut -c1-13; }

printf '%8s %6s %8s %9s %8s\n' "size" "window" "queries" "millisecs" "KB/s"
for size in 1024 10240 102400; do
  body="$(head -c "$size" /dev/urandom | base64 | tr -d '\n' | head -c "$size")"
  for window in 1 8; do
    write_sender "$window"
    start=$(now_ms)
    out=$("$BIN" send --config "$WORK/send.toml" \
          --to bbbb2222 --body "$body" 2>&1 || true)
    end=$(now_ms)
    queries=$(printf '%s' "$out" | sed -n 's/.*in \([0-9]*\) DNS quer.*/\1/p' | head -n1)
    queries=${queries:-0}
    elapsed=$((end - start))
    if [ "$elapsed" -le 0 ]; then elapsed=1; fi
    kbps=$(( size / elapsed ))
    printf '%8d %6d %8s %9d %8d\n' "$size" "$window" "$queries" "$elapsed" "$kbps"
  done
done

echo
echo "delay per reply: ${DELAY_MS} ms (simulated RTT on the relay)"
echo "window=1 is the old serial path; window=8 is pipelining."
