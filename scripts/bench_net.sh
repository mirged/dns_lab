#!/usr/bin/env bash
# Benchmark through an emulated network: delay + jitter + loss + bandwidth cap
# applied to the real DNS packets in both directions, by scripts/netem.py.
#
#   ./scripts/bench_net.sh              # full matrix (~2 min)
#   ./scripts/bench_net.sh --quick      # 4 KB only
#   PROFILES=wifi,bad ./scripts/bench_net.sh
#
# Topology:  client A --\
#                        netem (delay/jitter/loss/rate) --- relay --- B
#            client B --/
#
# A sends; B only polls occasionally so it does not distort the measurement.
# Every number below is a real run of the shipped binary.
set -uo pipefail

cd "$(dirname "$0")/.."
BIN="${BIN:-}"
if [ -z "$BIN" ]; then
  for candidate in target/release/dns_lab target/debug/dns_lab; do
    if [ -x "$candidate" ]; then BIN="$candidate"; break; fi
  done
fi
if [ -z "$BIN" ] || [ ! -x "$BIN" ]; then
  echo "[!] build first: cargo build --release" >&2
  exit 1
fi

WORK="$(mktemp -d "${TMPDIR:-/tmp}/dns_lab-net.XXXXXX")"
BASE=15500
NETEM="$BASE"          # clients talk to this
RELAY=$((BASE + 1))
A_PORT=$((BASE + 2))
B_PORT=$((BASE + 3))
KEY="net-bench-passphrase"

PROFILES="${PROFILES:-lan,wifi,lte,bad}"
QUICK=0
[ "${1:-}" = "--quick" ] && QUICK=1

cleanup() {
  kill "${NETEM_PID:-}" "${RELAY_PID:-}" "${B_PID:-}" 2>/dev/null || true
  wait 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

cat > "$WORK/relay.toml" <<EOF
node_id = "rrrr0001"
listen_addr = "127.0.0.1:$RELAY"
zone = "lab.local"
authoritative = true
relay_mode = "authoritative"
shared_key = "$KEY"
db_path = "relay.db"
rate_limit_per_sec = 0
verbose = false
EOF

# A sends through the emulator; B lives on the far side with the relay.
cat > "$WORK/a.toml" <<EOF
node_id = "aaaa1111"
listen_addr = "127.0.0.1:$A_PORT"
zone = "lab.local"
egress = "direct"
direct_server = "127.0.0.1:$NETEM"
shared_key = "$KEY"
db_path = "a.db"
verbose = false
EOF

cat > "$WORK/b.toml" <<EOF
node_id = "bbbb2222"
listen_addr = "127.0.0.1:$B_PORT"
zone = "lab.local"
egress = "direct"
direct_server = "127.0.0.1:$RELAY"
shared_key = "$KEY"
db_path = "b.db"
verbose = false
EOF

"$BIN" serve --config "$WORK/relay.toml" > "$WORK/relay.log" 2>&1 & RELAY_PID=$!
sleep 1

stats_field() { # $1 = json key
  python3 - "$WORK/netem.out" "$1" <<'PY'
import json, sys
path, key = sys.argv[1], sys.argv[2]
last = None
for line in open(path, encoding="utf-8", errors="replace"):
    line = line.strip()
    if line.startswith("{"):
        last = line
if not last:
    print(0); raise SystemExit
try:
    print(json.loads(last).get(key, 0))
except Exception:
    print(0)
PY
}

snapshot() { kill -USR1 "$NETEM_PID" 2>/dev/null; sleep 0.25; }

header() {
  printf '%-6s %9s %6s %7s %7s %7s %9s %8s %9s %6s\n' \
    "profile" "payload" "window" "chunks" "wire" "dropped" "millisecs" "KB/s" "retrans%" "state"
}

run_case() { # $1 profile $2 bytes $3 window $4 delay $5 jitter $6 loss $7 rate
  local profile=$1 bytes=$2 window=$3 delay=$4 jitter=$5 loss=$6 rate=$7

  # Fresh emulator per profile setting, so counters start clean.
  if [ -n "${NETEM_PID:-}" ]; then kill "$NETEM_PID" 2>/dev/null; wait "$NETEM_PID" 2>/dev/null; fi
  : > "$WORK/netem.out"
  python3 scripts/netem.py --listen "127.0.0.1:$NETEM" --upstream "127.0.0.1:$RELAY" \
    --delay-ms "$delay" --jitter-ms "$jitter" --loss "$loss" --rate-kbps "$rate" \
    > "$WORK/netem.out" 2>> "$WORK/netem.err" & NETEM_PID=$!
  sleep 0.8

  cat > "$WORK/send.toml" <<EOF
node_id = "aaaa1111"
listen_addr = "127.0.0.1:$A_PORT"
zone = "lab.local"
egress = "direct"
direct_server = "127.0.0.1:$NETEM"
shared_key = "$KEY"
db_path = "a.db"
window = $window
timeout_ms = 800
retries = 4
verbose = false
EOF

  local body
  body=$(head -c "$bytes" /dev/urandom | base64 | tr -d '\n' | head -c "$bytes")

  snapshot
  local before_up before_drop t0 t1 out rc chunks wire dropped elapsed kbps retrans state
  before_up=$(stats_field udp_up)
  before_drop=$(( $(stats_field udp_dropped_up) + $(stats_field udp_dropped_down) ))
  t0=$(date +%s%N)
  out=$("$BIN" send --config "$WORK/send.toml" --to bbbb2222 --body "$body" 2>&1)
  rc=$?
  t1=$(date +%s%N)
  snapshot

  wire=$(( $(stats_field udp_up) - before_up ))
  dropped=$(( $(stats_field udp_dropped_up) + $(stats_field udp_dropped_down) - before_drop ))
  chunks=$(printf '%s' "$out" | sed -n 's/.*in \([0-9]*\) DNS quer.*/\1/p' | head -n1)
  chunks=${chunks:-0}
  elapsed=$(( (t1 - t0) / 1000000 ))
  [ "$elapsed" -le 0 ] && elapsed=1
  kbps=$(( bytes / elapsed ))
  if [ "$chunks" -gt 0 ]; then retrans=$(( (wire - chunks) * 100 / chunks )); else retrans=0; fi
  [ "$rc" -eq 0 ] && state=ok || state=FAILED

  printf '%-6s %8sB %6s %7s %7s %7s %9s %8s %9s%% %6s\n' \
    "$profile" "$bytes" "$window" "$chunks" "$wire" "$dropped" "$elapsed" "$kbps" "$retrans" "$state"
}

# A file goes through the same link, split into parts like any other send.
run_file_case() { # $1 profile $2 bytes $3 window $4..$7 link params
  local profile=$1 bytes=$2 window=$3 delay=$4 jitter=$5 loss=$6 rate=$7
  if [ -n "${NETEM_PID:-}" ]; then kill "$NETEM_PID" 2>/dev/null; wait "$NETEM_PID" 2>/dev/null; fi
  : > "$WORK/netem.out"
  python3 scripts/netem.py --listen "127.0.0.1:$NETEM" --upstream "127.0.0.1:$RELAY" \
    --delay-ms "$delay" --jitter-ms "$jitter" --loss "$loss" --rate-kbps "$rate" \
    > "$WORK/netem.out" 2>> "$WORK/netem.err" & NETEM_PID=$!
  sleep 0.8

  cat > "$WORK/send.toml" <<EOF
node_id = "aaaa1111"
listen_addr = "127.0.0.1:$A_PORT"
zone = "lab.local"
egress = "direct"
direct_server = "127.0.0.1:$NETEM"
shared_key = "$KEY"
db_path = "a.db"
window = $window
timeout_ms = 800
retries = 4
verbose = false
EOF

  head -c "$bytes" /dev/urandom | base64 | head -c "$bytes" > "$WORK/payload.bin"

  snapshot
  local before_up before_drop t0 t1 out rc chunks wire dropped elapsed kbps retrans state
  before_up=$(stats_field udp_up)
  before_drop=$(( $(stats_field udp_dropped_up) + $(stats_field udp_dropped_down) ))
  t0=$(date +%s%N)
  out=$("$BIN" send --config "$WORK/send.toml" --to bbbb2222 --file "$WORK/payload.bin" 2>&1)
  rc=$?
  t1=$(date +%s%N)
  snapshot

  wire=$(( $(stats_field udp_up) - before_up ))
  dropped=$(( $(stats_field udp_dropped_up) + $(stats_field udp_dropped_down) - before_drop ))
  chunks=$(printf '%s' "$out" | sed -n 's/.*in [0-9]* part(s), \([0-9]*\) queries.*/\1/p' | head -n1)
  chunks=${chunks:-0}
  elapsed=$(( (t1 - t0) / 1000000 ))
  [ "$elapsed" -le 0 ] && elapsed=1
  kbps=$(( bytes / elapsed ))
  if [ "$chunks" -gt 0 ]; then retrans=$(( (wire - chunks) * 100 / chunks )); else retrans=0; fi
  [ "$rc" -eq 0 ] && state=ok || state=FAILED
  printf '%-6s %8sB %6s %7s %7s %7s %9s %8s %9s%% %6s\n' \
    "$profile" "$bytes" "$window" "$chunks" "$wire" "$dropped" "$elapsed" "$kbps" "$retrans" "$state"
}

echo "[*] relay on 127.0.0.1:$RELAY, emulator on 127.0.0.1:$NETEM"
echo

header
for profile in ${PROFILES//,/ }; do
  case $profile in
    lan)  delay=1;   jitter=0;  loss=0;     rate=0 ;;
    wifi) delay=15;  jitter=8;  loss=0.005; rate=4000 ;;
    lte)  delay=45;  jitter=20; loss=0.02;  rate=1200 ;;
    bad)  delay=120; jitter=40; loss=0.05;  rate=400 ;;
    *) echo "[!] unknown profile $profile"; continue ;;
  esac
  echo "== $profile: one-way ${delay}ms +/-${jitter}ms, ${loss} loss/dir, ${rate:-unlimited}kbps  (RTT $((delay * 2))ms)"
  # Both windows on the small size, window 8 only on the big one: window 1 at
  # 240 ms RTT is slow enough to make the full matrix a coffee break.
  run_case "$profile" 2048 1 "$delay" "$jitter" "$loss" "$rate"
  run_case "$profile" 2048 8 "$delay" "$jitter" "$loss" "$rate"
  if [ "$QUICK" -eq 0 ]; then
    run_case "$profile" 16384 8 "$delay" "$jitter" "$loss" "$rate"
    # A 32 KiB file on the same link (4 parts, ~280 queries end to end).
    run_file_case "$profile" 32768 8 "$delay" "$jitter" "$loss" "$rate"
  fi
  echo
done

echo "chunks   = DNS queries the message needed (ceil(bytes/118) plus header)"
echo "wire     = datagrams the emulator actually carried for that send"
echo "retrans% = wire above chunks, i.e. what loss and jitter cost"
