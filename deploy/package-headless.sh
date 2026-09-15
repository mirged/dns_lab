#!/usr/bin/env bash
# Build the headless relay binary and package it into a distributable archive.
#
#   ./deploy/package-headless.sh
#   # -> dist/dns_lab-headless-<version>-<arch>.tar.gz  (+ .sha256)
#
# The archive contains the self-contained binary, an installer, the systemd unit,
# an example config and the `dns-lab-config` editor.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

VERSION="$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -n1)"
ARCH="$(uname -m)"
NAME="dns_lab-headless-${VERSION}-${ARCH}"
OUT="$ROOT/dist"
STAGE="$(mktemp -d)"

cleanup() { rm -rf "$STAGE"; }
trap cleanup EXIT

echo "==> Building headless binary (--no-default-features)"
cargo build --release --no-default-features

echo "==> Staging $NAME"
DEST="$STAGE/$NAME"
install -d "$DEST"
install -m 0755 target/release/dns_lab "$DEST/dns_lab"
install -m 0755 deploy/headless/install.sh "$DEST/install.sh"
install -m 0755 deploy/headless/dns-lab-config "$DEST/dns-lab-config"
install -m 0644 deploy/headless/dns_lab.toml.example "$DEST/dns_lab.toml.example"
install -m 0644 deploy/headless/README.md "$DEST/README.md"
install -m 0644 deploy/dns_lab.service "$DEST/dns_lab.service"

mkdir -p "$OUT"
echo "==> Creating $OUT/$NAME.tar.gz"
tar -C "$STAGE" -czf "$OUT/$NAME.tar.gz" "$NAME"
( cd "$OUT" && sha256sum "$NAME.tar.gz" > "$NAME.tar.gz.sha256" )

echo
echo "Archive:  $OUT/$NAME.tar.gz"
echo "Checksum: $OUT/$NAME.tar.gz.sha256"
