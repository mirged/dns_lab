# dns_lab

<!--
  Replace OWNER below with your GitHub org/user once the repository exists.
  The workflow badge only renders after the first CI run on the default branch.
-->

[![CI](https://github.com/OWNER/dns_lab/actions/workflows/ci.yml/badge.svg)](https://github.com/OWNER/dns_lab/actions/workflows/ci.yml)
![Edition](https://img.shields.io/badge/rust-2024%20edition-blue)
![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-green)
![Stage](https://img.shields.io/badge/stage-early%20alpha-orange)
![Protocol](https://img.shields.io/badge/wire-RFC%201035%20DNS-blueviolet)

> **⚠️ Active Alpha / Experimental Lab: Not audited.**
> This is an early-stage research toy, not a product. The wire format and
> configuration **will break between commits**, and there are no stability
> guarantees. The ChaCha20-Poly1305 usage has **not** been reviewed by third
> party cryptographers, the key derivation is deliberately naive, and the
> Android client is a rough functional prototype. **Do not use this for
> anything you actually need to keep secret or keep working.**

`dns_lab` is an experimental messenger that carries small, compressed, encrypted
messages inside ordinary DNS queries (TXT records, RFC 1035) and reassembles
them on the other side. It exists to explore DNS tunnelling, packet framing,
and applied cryptography in a self-contained Rust codebase — not to replace
Signal.

---

## Table of contents

- [Status & maturity](#status--maturity)
- [Architecture](#architecture)
- [Features](#features)
- [Security & threat model](#security--threat-model)
- [Repository layout](#repository-layout)
- [Quickstart](#quickstart)
  - [1. Build](#1-build)
  - [2. Desktop client](#2-desktop-client)
  - [3. Headless relay on a VPS](#3-headless-relay-on-a-vps)
  - [4. Experimental Android client](#4-experimental-android-client)
- [Configuration](#configuration)
- [Wire format](#wire-format)
- [Testing](#testing)
- [Known quirks, limitations & roadmap](#known-quirks-limitations--roadmap)
- [Responsible & lawful use](#responsible--lawful-use)
- [Contributing](#contributing)
- [License](#license)

---

## Status & maturity

| Area | State |
|------|-------|
| Protocol / wire format | **Unstable**, versioned envelope (`VERSION = 2`), breaks freely |
| Desktop GUI (egui) | Works; bare-bones but usable |
| Headless relay / CLI | Most exercised target; runs under systemd |
| Android client | **Experimental proof-of-concept.** Rough UI, basic scaling, protocol demo only |
| Cryptography | ChaCha20-Poly1305 AEAD, **unofficial / unaudited** |
| Compression | Port of [SMAZ](LICENSES/SMAZ.txt); not tuned for chat traffic |
| Test suite | 6 unit + 2 end-to-end tests (`cargo test --all-targets`) |

This repository is being actively hacked on. Expect sharp edges, half-finished
features, and bugs. If you want a dependable secure messenger, please use one
that has been audited.

## Architecture

```
        SENDER (dns_lab gui | send)                     RECIPIENT (poller)
 ┌────────────────────────────────────┐        ┌──────────────────────────────┐
 │  chat text                         │        │  mailbox poll:               │
 │     │                              │        │  p.<node-id>.0.0.<zone>      │
 │     ▼                              │        │     │                        │
 │  SMAZ compress                     │        │     ▼                        │
 │     │                              │        │  fetch TXT blobs             │
 │     ▼                              │        │     │                        │
 │  envelope:                         │        │     ▼                        │
 │  ver|type|ts|from|to|len|body      │        │  ChaCha20-Poly1305 decrypt   │
 │     │                              │        │     │                        │
 │     ▼                              │        │     ▼                        │
 │  ChaCha20-Poly1305 (12B nonce)     │        │  SMAZ decompress → SQLite    │
 │     │                              │        └──────────────▲───────────────┘
 │     ▼                              │                       │
 │  Base32 (RFC 4648, no padding)     │                       │
 │     │                              │                       │
 │     ▼                              │                       │
 │  slice into ≤63-char DNS labels    │                       │
 │  qname ≤ 253 chars:                │                       │
 │  m.<msg-id>.<seq>.<total>.<data>   │                       │
 │     │                              │                       │
 └─────┼──────────────────────────────┘                       │
       │ UDP :53  (TXT query)                                 │
       ▼                                                      │
 ┌───────────────────────────┐                                │
 │ egress = "resolver"       │   or   direct ip:port          │
 │  public recursive DNS     │◄──────── egress = "direct" ────┤
 └─────────────┬─────────────┘                                │
               │ forwards under the delegated zone            │
               ▼                                              │
 ┌────────────────────────────────────────────────────────────┴─────────┐
 │         AUTHORITATIVE VPS RELAY  (`dns_lab serve`, port 53)          │
 │  parse qname → reassemble chunks → decrypt → SQLite mailbox → TXT ack │
 └───────────────────────────────────────────────────────────────────────┘
```

In **resolver** mode the client sends to a public recursive resolver
(e.g. `1.1.1.1:53`); the relay must be reachable as the authoritative NS for
`zone`. In **direct** mode the client sends straight to the relay / peer
`ip:port` with no recursion involved.

## Features

- **DNS transport** with automatic **TCP fallback** when a UDP reply sets the
  truncation (TC) bit. EDNS0 UDP payload size is advertised and respected.
- **Packet framing**: sequence numbers, multi-chunk reassembly, and a dynamic
  FQDN budget that fits the full name into ≤253 characters / ≤63-character
  labels.
- **Authenticated encryption** using **ChaCha20-Poly1305** with a fresh 12-byte
  nonce per message. The ciphertext is Base32-encoded for DNS-safe labels.
- **SMAZ compression** to shave bytes off short English-style chat text.
- **Multiple targets**:
  - headless **Linux server daemon** (`--no-default-features`, tiny binary),
  - **desktop GUI** (egui/eframe, default),
  - **experimental Android APK** (NativeActivity + OpenGL ES).
- **SQLite storage** (bundled, no system sqlite required) for messages,
  chunk reassembly, the relay mailbox, and contacts.
- **Ops-friendly**: systemd unit with `CAP_NET_BIND_SERVICE`, a one-shot
  headless installer/packager, and a config editor.

## Security & threat model

**Read this before trusting the project with anything.**

Implemented:

- **Confidentiality + integrity** of message bodies and metadata fields via
  **ChaCha20-Poly1305** (AEAD) with a random 96-bit nonce per message.
- Payloads are compressed before encryption, so body length on the wire is
  partially obscured.
- The default build uses a bundled, **lab-grade** shared key. **Always set your
  own `shared_key`.**

Explicitly **out of scope / not provided**:

- **No third-party audit.** The cryptographic assembly is unreviewed.
- **No real key management.** `shared_key` is *padded or truncated* to 32 bytes
  (“KDF” is a stretch); low-entropy or short secrets are weak. There is **no
  key exchange**, **no rotation**, **no forward secrecy**, and **no per-recipient
  keys**.
- **The relay can read everything.** It is a **group shared-key** system: the
  relay holds the same key and decrypts traffic in order to route/mailbox it.
  This is *not* end-to-end encryption between two clients behind a relay.
- **No replay protection.** A captured query can be replayed; the receiver has
  no anti-replay window.
- **Metadata leaks.** Query timing, sizes, counts, source/destination, and the
  recipient node id are all observable on the wire. DNS is not a covert channel
  against a determined network observer.
- **Availability is not defended.** Plain UDP/TCP listeners, per-connection
  threads, no rate limiting. Trivial DoS surface.
- **Public resolvers may rate-limit, block, or poison** tunnel traffic, and some
  networks prohibit it outright.

Treat `dns_lab` as an educational lab, not a privacy tool.

## Repository layout

```
dns_lab/
├── Cargo.toml                  # features: default = gui, --no-default-features = headless
├── config.example.toml         # client template
├── config.server.example.toml  # relay template
├── migrations/0001_init.sql    # embedded SQLite schema (include_str!)
├── src/
│   ├── lib.rs                  # crypto primitives, key/ID helpers, randomness
│   ├── main.rs                 # CLI entry: gui | serve | send | id
│   ├── config.rs               # TOML config model + loader
│   ├── db.rs                   # SQLite: messages, chunks, mailbox, peers
│   ├── protocol.rs             # envelope + DNS QNAME framing (chunking)
│   ├── smaz.rs                 # SMAZ compression port (BSD-3, see LICENSES/)
│   ├── android.rs              # Android native-activity entry point
│   ├── app/mod.rs              # egui/eframe desktop + Android UI
│   └── net/
│       ├── mod.rs              # Node orchestration + egress selection
│       ├── sender.rs           # UDP exchange + TCP fallback
│       ├── listener.rs         # authoritative UDP/TCP listener
│       └── poller.rs           # mailbox polling
├── tests/e2e.rs                # end-to-end UDP tests
└── deploy/                     # systemd unit, headless installer, Android build
```

## Quickstart

### Prerequisites

- Rust **1.88+** (edition 2024; let-chains are used).
- A domain (or subdomain) you control, for anything beyond `direct`/LAN mode.
- Desktop GUI: X11 or Wayland dev libraries. Headless builds need none.

### 1. Build

```sh
# Desktop GUI (default feature):
cargo build --release

# Headless server / CLI only (no graphics dependencies at all):
cargo build --release --no-default-features
```

The headless binary is at `target/release/dns_lab`.

### 2. Desktop client

```sh
cp config.example.toml dns_lab.toml
$EDITOR dns_lab.toml          # set zone, shared_key, resolver
cargo run --release           # or: ./target/release/dns_lab gui
```

Use the **Setup** tab to add contacts by node id (each peer prints theirs with
`dns_lab id`). Pick a conversation under **Chats** and send. The **DNS log** tab
shows the raw query framing.

CLI helpers (no GUI required):

```sh
dns_lab id                                   # print this node id
dns_lab send --to <recipient-node-id> --body "hello over DNS"
dns_lab send --to <id> --body "hi" --config /path/to/dns_lab.toml
```

### 3. Headless relay on a VPS

The relay is the authoritative nameserver for your tunnel zone. Delegate a
subdomain to it at your DNS provider:

| Type | Name              | Value                |
|------|-------------------|----------------------|
| NS   | `t.example.com`   | `ns1.example.com`    |
| A    | `ns1.example.com` | `<VPS_IP>`           |
| AAAA | `ns1.example.com` | `<VPS_IPv6>` (opt.)  |

Some providers require the NS name to live inside the delegated zone (glue
records); adjust accordingly and wait for propagation.

**Option A — prebuilt installer (recommended).** Build an archive on your dev
machine and run the installer on the VPS:

```sh
./deploy/package-headless.sh
# -> dist/dns_lab-headless-<version>-<arch>.tar.gz  (+ .sha256)

scp dist/dns_lab-headless-*.tar.gz root@<VPS>:/root/
ssh root@<VPS> 'tar xzf dns_lab-headless-*.tar.gz && \
  cd dns_lab-headless-* && \
  ./install.sh --zone t.example.com --shared-key "a-long-random-secret"'
```

The installer creates the `dns_lab` system user, `/etc/dns_lab` and
`/var/lib/dns_lab`, writes the config, and installs/starts the hardened
systemd unit (`CAP_NET_BIND_SERVICE`, `ProtectSystem=strict`). Edit the config
later with:

```sh
sudo dns-lab-config          # interactive menu
sudo systemctl restart dns_lab
journalctl -u dns_lab -f
```

See [`deploy/headless/README.md`](deploy/headless/README.md) for every flag.

**Option B — build on the VPS.**

```sh
cargo build --release --no-default-features
sudo install -m 0755 target/release/dns_lab /usr/local/bin/dns_lab
sudo mkdir -p /etc/dns_lab /var/lib/dns_lab
sudo cp config.server.example.toml /etc/dns_lab/dns_lab.toml
sudo cp deploy/dns_lab.service /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now dns_lab
```

Open the firewall (`ufw allow 53/tcp && ufw allow 53/udp`) and sanity-check:

```sh
dig @<VPS_IP> <zone> SOA          # expect a NOERROR SOA answer
dig @1.1.1.1 <zone> SOA           # expect it via recursion after delegation
```

Full delegation and verification notes live in [`deploy/README.md`](deploy/README.md).

### 4. Experimental Android client

> The Android app is a **barebones technical proof-of-concept**. The UI is
> rough, scaling is basic, and it is a protocol demo — not a consumer messenger.

The Android client uses resolver/relay mode (phones cannot bind port 53) and
reuses the same Rust protocol, crypto, and SQLite code via NativeActivity.

Prerequisites: `rustup target add aarch64-linux-android`, `cargo-apk2`, Android
SDK/NDK, JDK 17, and a signing keystore. Then:

```sh
export ANDROID_HOME=... ANDROID_NDK_HOME=... JAVA_HOME=...
export CARGO_APK_RELEASE_KEYSTORE="$PWD/android/dnslab-release.jks"
export CARGO_APK_RELEASE_KEYSTORE_PASSWORD=...
./deploy/android/build-apk.sh
# -> target/release/apk/dns-lab.apk
adb install -r target/release/apk/dns-lab.apk
```

See [`deploy/android/README.md`](deploy/android/README.md) for the full toolchain
and runtime details.

## Configuration

Configuration is TOML. Fields omitted fall back to defaults; the `node_id` is
generated on first load.

- **Client:** copy [`config.example.toml`](config.example.toml) to
  `dns_lab.toml`. Key fields: `zone`, `egress`, `resolver` / `direct_server` /
  `relay`, `shared_key`, `db_path`.
- **Relay:** use [`config.server.example.toml`](config.server.example.toml)
  (installed as `/etc/dns_lab/dns_lab.toml`). Key fields: `listen_addr`
  (`0.0.0.0:53`), `zone`, `authoritative = true`, `shared_key`, `db_path`.

The active config path is chosen by `--config FILE` or the `DNS_LAB_CONFIG`
environment variable (default: `dns_lab.toml`).

| Field | Default | Notes |
|-------|---------|-------|
| `display_name` | `anon` | Cosmetic label |
| `node_id` | auto | 8 hex chars, generated if empty |
| `listen_addr` | `0.0.0.0:1054` | Local DNS listener; relay uses `:53` |
| `zone` | `lab.local` | Tunnel suffix |
| `authoritative` | `false` | Relay sets `true` |
| `egress` | `direct` | `direct` or `resolver` |
| `resolver` | – | `ip:port`, for `egress = "resolver"` |
| `direct_server` / `relay` | – | Relay address; `relay` also enables polling |
| `poll_interval_secs` | `2` | Mailbox poll cadence |
| `edns_udp_size` | `1232` | Advertised EDNS0 UDP size |
| `timeout_ms` / `retries` | `800` / `3` | Per-query UDP timeout and retries |
| `db_path` | `dns_lab.db` | SQLite file (relative → next to config) |
| `shared_key` | built-in lab key | **Set your own**; identical on all nodes |
| `peers` | `[]` | `[[peers]]` with `id` / `name` / `addr` / `via` |

## Wire format

All of this is internal, unstable, and versioned by the `VERSION` byte inside
the encrypted envelope.

- **Query name:** `<verb>.<msg-id>.<seq>.<total>.<data>.<zone>`
  - `verb`: `m` = message chunk, `p` = mailbox poll.
  - `data`: uppercase Base32 (RFC 4648, no padding), split across labels of
    ≤63 chars; the whole name is kept ≤253 chars.
- **Envelope (plaintext, before encryption):**

  ```
  version(1) | msg_type(1) | ts(8, LE) | from_len(1) | from | to_len(1) | to
  | body_len(4, LE) | smaz(body)
  ```

- **On the wire:** `Base32( nonce[12] || ChaCha20-Poly1305(envelope) + tag )`,
  chunked into as many queries as needed.
- **Replies:** a TXT record. Message chunks are acknowledged with `a:<msg-id>:<seq>`;
  poll replies carry `cursor:<n>` and `m:<msg-id>:<seq>:<total>:<data>` entries.

## Testing

```sh
cargo test --all-targets                                   # 6 unit + 2 e2e
cargo test --no-default-features --all-targets             # headless build
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

The end-to-end tests spin up two real nodes on localhost UDP, exchange a
multi-chunk message, and exercise the relay mailbox path.

## Known quirks, limitations & roadmap

Known issues (contributions welcome):

- **Android insets / notches**: the UI reads the NativeActivity content rect to
  avoid the status/navigation bars, but falls back to a 24 dp top inset if the
  platform reports nothing. Behaviour varies by device.
- **Packet loss**: UDP retries and TCP fallback exist, but there is no
  end-to-end delivery guarantee. The relay marks mailbox entries delivered when
  they are *fetched*, so a lost poll response or a crash mid-decrypt loses the
  message. Chunks older than 5 minutes are garbage-collected.
- **Large messages**: chunk count grows quickly; there is no throughput control
  or congestion awareness.
- **Security**: no audit, no key exchange/rotation, no replay protection, shared
  group key (see [Security & threat model](#security--threat-model)).
- **Robustness**: unbounded per-connection threads; no rate limiting; aggressive
  resolvers may drop or poison traffic.

Roadmap ideas:

- **DoH / DoT transport** in addition to raw UDP/53.
- **Per-recipient keypairs** (X25519) instead of a single shared secret, plus
  replay protection.
- **Real key derivation** (HKDF/Argon2) and key rotation.
- **Better Android UI**, insets handling, and background polling.
- **Observability**: metrics, structured logging, per-peer status.
- **Packaging**: `.deb`/container images and reproducible release artifacts.

## Responsible & lawful use

DNS tunnelling can violate the terms of service of networks, resolvers, and DNS
providers, and may be illegal depending on jurisdiction and intent. Use this
software only on systems and networks you own or are explicitly authorized to
test, and comply with all applicable laws and policies. The authors provide
this for education and research and accept no liability for misuse.

## Contributing

Early-stage projects thrive on small, focused patches. Before opening a PR:

1. `cargo fmt --all`
2. `cargo clippy --all-targets -- -D warnings`
3. `cargo test --all-targets`

Keep changes scoped, explain the *why* in the PR description, and call out any
wire-format or security impact explicitly (they will break things).

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. Unless you explicitly state otherwise, any contribution
intentionally submitted for inclusion in this project shall be dual-licensed as
above, with no additional terms.

### Third-party notices

- `src/smaz.rs` is a Rust port of [SMAZ](https://github.com/antirez/smaz) by
  Salvatore Sanfilippo, licensed under the 3-clause BSD license. See
  [`LICENSES/SMAZ.txt`](LICENSES/SMAZ.txt).
