#!/usr/bin/env bash
# Install the DNS Lab headless relay from a release archive.
#
# Run as root from the unpacked archive:
#   sudo ./install.sh --zone t.example.com
#
# Options:
#   --zone ZONE          authoritative zone (default: keep example value)
#   --shared-key KEY     give the relay a key too (default: none, blind relay)
#   --listen ADDR        UDP/TCP bind address (default: 0.0.0.0:53)
#   --no-start           install but do not enable/start the service
#   --force-config       overwrite an existing /etc/dns_lab/dns_lab.toml
#   -h, --help           show this help
#
# Environment: PREFIX, DNS_LAB_ETC, DNS_LAB_VAR, DNS_LAB_SERVICE, DNS_LAB_USER,
#              DNS_LAB_SYSTEMD_DIR
set -euo pipefail

PREFIX="${PREFIX:-/usr/local}"
ETC_DIR="${DNS_LAB_ETC:-/etc/dns_lab}"
VAR_DIR="${DNS_LAB_VAR:-/var/lib/dns_lab}"
SERVICE_NAME="${DNS_LAB_SERVICE:-dns_lab}"
SERVICE_USER="${DNS_LAB_USER:-dns_lab}"
UNIT_DIR="${DNS_LAB_SYSTEMD_DIR:-/etc/systemd/system}"
CONFIG="$ETC_DIR/dns_lab.toml"
export DNS_LAB_CONFIG="$CONFIG"
export DNS_LAB_SERVICE="$SERVICE_NAME"

ZONE="${DNS_LAB_ZONE:-t.example.com}"
SHARED_KEY="${DNS_LAB_SHARED_KEY:-}"
LISTEN="${DNS_LAB_LISTEN:-0.0.0.0:53}"
START=1
FORCE_CONFIG=0

usage() { awk 'NR>1 && /^#/ {sub(/^# ?/,""); print; next} NR>1 {exit}' "$0"; }

while [ $# -gt 0 ]; do
    case "$1" in
        --zone) ZONE="${2:?}"; shift 2 ;;
        --shared-key) SHARED_KEY="${2:?}"; shift 2 ;;
        --listen) LISTEN="${2:?}"; shift 2 ;;
        --no-start) START=0; shift ;;
        --force-config) FORCE_CONFIG=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown option: $1" >&2; usage; exit 1 ;;
    esac
done

if [ "$(id -u)" -ne 0 ]; then
    echo "error: run as root (sudo $0 ...)" >&2
    exit 1
fi

SRC_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
for f in dns_lab dns-lab-config dns_lab.toml.example dns_lab.service; do
    if [ ! -f "$SRC_DIR/$f" ]; then
        echo "error: $f not found next to install.sh; unpack the whole archive" >&2
        exit 1
    fi
done

echo "==> Installing binaries to $PREFIX/bin"
install -d -m 0755 "$PREFIX/bin"
install -m 0755 "$SRC_DIR/dns_lab" "$PREFIX/bin/dns_lab"
install -m 0755 "$SRC_DIR/dns-lab-config" "$PREFIX/bin/dns-lab-config"

echo "==> Creating service account '$SERVICE_USER'"
if ! id -u "$SERVICE_USER" >/dev/null 2>&1; then
    useradd --system --no-create-home --home-dir "$VAR_DIR" \
        --shell /usr/sbin/nologin "$SERVICE_USER" 2>/dev/null \
        || useradd --system --no-create-home --home-dir "$VAR_DIR" "$SERVICE_USER"
fi

echo "==> Creating $ETC_DIR and $VAR_DIR"
install -d -m 0755 "$ETC_DIR"
install -d -m 0750 -o "$SERVICE_USER" -g "$SERVICE_USER" "$VAR_DIR"

if [ -f "$CONFIG" ] && [ "$FORCE_CONFIG" -eq 0 ]; then
    echo "==> Keeping existing $CONFIG (use --force-config to overwrite)"
else
    if [ -f "$CONFIG" ]; then
        backup="$CONFIG.bak.$(date +%Y%m%d%H%M%S)"
        cp -a "$CONFIG" "$backup"
        echo "==> Backed up existing config to $backup"
    fi
    echo "==> Writing $CONFIG"
    install -m 0640 "$SRC_DIR/dns_lab.toml.example" "$CONFIG"
    # A relay needs no key: it forwards ciphertext and routes on the QNAME tag.
    # Only set one when the operator asked for it explicitly.
    if [ -n "$SHARED_KEY" ]; then
        "$PREFIX/bin/dns-lab-config" --no-restart \
            --set zone "$ZONE" \
            --set listen_addr "$LISTEN" \
            --set shared_key "$SHARED_KEY"
    else
        "$PREFIX/bin/dns-lab-config" --no-restart \
            --set zone "$ZONE" \
            --set listen_addr "$LISTEN"
    fi
fi
chown -R root:"$SERVICE_USER" "$ETC_DIR"
chmod 0640 "$CONFIG"
chown "$SERVICE_USER":"$SERVICE_USER" "$VAR_DIR"

install -m 0644 "$SRC_DIR/README.md" "$ETC_DIR/README.md" 2>/dev/null || true

echo "==> Installing systemd unit"
if [ -d /run/systemd/system ] || command -v systemctl >/dev/null 2>&1; then
    install -d -m 0755 "$UNIT_DIR"
    install -m 0644 "$SRC_DIR/dns_lab.service" "$UNIT_DIR/${SERVICE_NAME}.service"
    systemctl daemon-reload
    systemctl enable "$SERVICE_NAME" >/dev/null 2>&1 || true
    if [ "$START" -eq 1 ]; then
        systemctl restart "$SERVICE_NAME"
        echo "==> $SERVICE_NAME started"
    else
        echo "==> $SERVICE_NAME enabled but not started (--no-start)"
    fi
else
    echo "warn: systemd not detected; run manually:" >&2
    echo "      $PREFIX/bin/dns_lab serve --config $CONFIG" >&2
fi

echo
echo "=================================================="
echo "  DNS Lab relay installed"
echo "  Zone:        $ZONE"
echo "  Shared key:  ${SHARED_KEY:-none (blind relay - forwards what it cannot read)}"
echo "  Config:      $CONFIG"
echo "  Database:    $VAR_DIR/dns_lab.db"
echo "  Edit config: sudo dns-lab-config"
echo "  Logs:        journalctl -u $SERVICE_NAME -f"
echo "=================================================="
echo
echo "Firewall (if used): sudo ufw allow 53/tcp && sudo ufw allow 53/udp"
echo "Delegation: point an NS record for '$ZONE' at this host, then verify:"
echo "  dig @<this-host-ip> test.$ZONE SOA"
