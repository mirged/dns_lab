#!/usr/bin/env bash
# Test dns_lab between a phone (Android APK) and this laptop on one LAN.
#
# Direct mode: no relay, no domain, no DNS delegation. Both sides bind the same
# UDP/TCP port and each has the other in its contacts. Everything is set from
# the phone's Setup tab - the app writes its own config, so there is nothing to
# push over adb.
#
#   scripts/lan_test.sh setup                  # write ~/dns-lab-lan/laptop.toml, print the phone values
#   scripts/lan_test.sh setup bbbb2222 192.168.1.42   # ... and add the phone as a contact
#   scripts/lan_test.sh up                     # start the laptop GUI
#   scripts/lan_test.sh status                 # listeners + recent messages
#   scripts/lan_test.sh down                   # stop the laptop GUI
#
# Environment overrides:
#   DNS_LAB_BIN        binary            (default: target/release/dns_lab)
#   DNS_LAB_LAN_DIR    state directory   (default: ~/dns-lab-lan)
#   DNS_LAB_LAN_PORT   port on both ends (default: 5354)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${DNS_LAB_BIN:-$ROOT/target/release/dns_lab}"
DIR="${DNS_LAB_LAN_DIR:-$HOME/dns-lab-lan}"
PORT="${DNS_LAB_LAN_PORT:-5354}"
LAPTOP_ID="1111aaaa"
ZONE="lab.local"
CFG="$DIR/laptop.toml"
PID="$DIR/laptop.pid"

say() { printf '[*] %s\n' "$*"; }
die() { printf '[!] %s\n' "$*" >&2; exit 1; }

lan_ip() {
  # The address of the interface that carries the default route - the one the
  # phone has to be able to reach. Falls back to the first global IPv4.
  local ip
  ip="$(ip route get 1.1.1.1 2>/dev/null | awk '{for(i=1;i<=NF;i++) if($i=="src"){print $(i+1); exit}}')"
  [ -n "$ip" ] || ip="$(ip -4 -o addr show scope global | awk 'NR==1{split($4,a,"/"); print a[1]}')"
  printf '%s' "$ip"
}

key() {
  mkdir -p "$DIR"
  if [ ! -s "$DIR/shared.key" ]; then
    openssl rand -base64 32 | tr -d '\n' > "$DIR/shared.key"
    chmod 600 "$DIR/shared.key"
  fi
  cat "$DIR/shared.key"
}

cmd_setup() {
  local phone_id="${1:-}" phone_ip="${2:-}"
  local shared ip; shared="$(key)"; ip="$(lan_ip)"
  [ -n "$ip" ] || die "no LAN IPv4 address found (is Wi-Fi up?)"
  mkdir -p "$DIR"

  if [ -f "$CFG" ] && [ -z "$phone_id" ]; then
    say "$CFG already exists (kept; delete it to regenerate)"
  else
    cat > "$CFG" <<EOF
# dns_lab laptop side of a phone <-> laptop LAN test.
display_name = "laptop"
node_id      = "$LAPTOP_ID"
listen_addr  = "0.0.0.0:$PORT"
zone         = "$ZONE"
egress       = "direct"
poll_interval_secs = 1
edns_udp_size = 4096
window = 8
shared_key   = "$shared"
db_path      = "laptop.db"
download_dir = "downloads-laptop"
EOF
    if [ -n "$phone_id" ]; then
      [ -n "$phone_ip" ] || die "usage: $0 setup <phone-node-id> <phone-lan-ip>"
      cat >> "$CFG" <<EOF

[[peers]]
id   = "$phone_id"
name = "phone"
addr = "$phone_ip:$PORT"
via  = "direct"
EOF
    fi
    chmod 600 "$CFG"
    say "wrote $CFG"
  fi

  cat <<EOF

  Phone: DNS Lab -> Setup tab, then Save and "Restart node" last.

    Shared key        $shared
    Zone / suffix     $ZONE
    Listen address    0.0.0.0:$PORT
    Egress            direct
    Resolver          (leave empty)
    Direct/relay addr (leave empty)
    Poll relay        (leave empty)

  Then, still in Setup -> Contacts: add the laptop as a contact
    id   $LAPTOP_ID
    name laptop
    addr $ip:$PORT
    via  direct

  On the laptop, add the phone the same way (id = the phone's Node id from its
  Setup tab, addr = the phone's LAN IP:$PORT, via direct), or re-run:
    $0 setup <phone-node-id> <phone-lan-ip>

  Start the laptop side with:  $0 up
EOF
}

cmd_up() {
  [ -x "$BIN" ] || die "no binary at $BIN - run 'cargo build --release' first"
  [ -f "$CFG" ] || die "no $CFG - run '$0 setup' first"
  [ -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ] || die "no display; run '$BIN run --config $CFG' for headless"
  if [ -f "$PID" ] && kill -0 "$(cat "$PID" 2>/dev/null)" 2>/dev/null; then
    say "already running (pid $(cat "$PID"))"; return 0
  fi
  : > "$DIR/laptop.log"
  nohup "$BIN" gui --config "$CFG" >> "$DIR/laptop.log" 2>&1 &
  echo $! > "$PID"
  for _ in $(seq 1 40); do
    ss -lunH "sport = :$PORT" 2>/dev/null | grep -q . && break
    sleep 0.25
  done
  kill -0 "$(cat "$PID")" 2>/dev/null || { tail -n 20 "$DIR/laptop.log" >&2; die "GUI exited"; }
  say "laptop up on 0.0.0.0:$PORT (pid $(cat "$PID"), node $LAPTOP_ID)"
  say "phone should use its own Node id and this address: $(lan_ip):$PORT"
}

cmd_down() {
  if [ -f "$PID" ]; then
    local p; p="$(cat "$PID" 2>/dev/null || true)"
    [ -n "$p" ] && kill "$p" 2>/dev/null || true
    rm -f "$PID"
    say "stopped"
  else
    say "not running"
  fi
}

cmd_status() {
  if [ -f "$PID" ] && kill -0 "$(cat "$PID" 2>/dev/null)" 2>/dev/null; then
    echo "laptop gui   up (pid $(cat "$PID"))"
  else
    echo "laptop gui   down"
  fi
  ss -lunH "sport = :$PORT" 2>/dev/null | grep -q . \
    && echo "port $PORT    bound (udp)" || echo "port $PORT    free"
  [ -f "$CFG" ] || return 0
  echo
  "$BIN" id --config "$CFG" 2>&1 | sed 's/^/  /'
  "$BIN" inbox --config "$CFG" --limit 5 2>&1 | sed 's/^/  /'
}

case "${1:-setup}" in
  setup)  shift || true; cmd_setup "${1:-}" "${2:-}" ;;
  up)     cmd_up ;;
  down)   cmd_down ;;
  status) cmd_status ;;
  -h|--help|help) sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//' ;;
  *) die "unknown command: $1 (setup|up|down|status)" ;;
esac
