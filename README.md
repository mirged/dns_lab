# dns_lab

[![CI](https://github.com/mirged/dns_lab/actions/workflows/ci.yml/badge.svg)](https://github.com/mirged/dns_lab/actions/workflows/ci.yml)
![Edition](https://img.shields.io/badge/rust-2024%20edition-blue)
![License](https://img.shields.io/badge/license-MIT-green)
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

**New in 0.6** (see [CHANGELOG.md](CHANGELOG.md)): **file transfers**. A file up
to 32 MiB is split into 8 KiB parts, each one an ordinary signed, encrypted
envelope, and reassembled on the other side against a SHA-256 the sender
promised — with resume, a verified-on-disk write, and a single delivery receipt
per transfer. `dns_lab send --file` and a "Send file" box in the GUI composer.
Also fixed: batched mailbox acknowledgements were being rejected as illegal DNS
names, so multi-message acks silently retried forever.

The previous release added identities: Ed25519-signed envelopes, X25519
per-recipient keys, push delivery, a retrying outbox and a pipelined sender.
The wire format is `VERSION = 5` and is **not** compatible with 0.4 nodes.

---

## Status & maturity

| Area | State |
|------|-------|
| Protocol / wire format | **Unstable**, versioned envelope (`VERSION = 5`), breaks freely |
| Desktop GUI (egui) | Works; small but usable: conversation list, delivery states, DNS inspector |
| Headless relay / CLI | Most exercised target; runs under systemd |
| Android client | **Experimental proof-of-concept.** Rough UI, basic scaling, protocol demo only |
| Cryptography | ChaCha20-Poly1305 AEAD, Ed25519 signatures, X25519 + HKDF-SHA256 key agreement, **unofficial / unaudited** |
| Compression | SMAZ **or** DEFLATE per message, whichever is smaller |
| Test suite | 72 unit + 9 end-to-end tests (`cargo test --all-targets`) |

This repository is being actively hacked on. Expect sharp edges, half-finished
features, and bugs. If you want a dependable secure messenger, please use one
that has been audited.

## Architecture

```
   SENDER (dns_lab gui | send)                    RECIPIENT (listener | poller)
 ┌─────────────────────────────────┐      ┌──────────────────────────────────┐
 │  chat text                      │      │  poll: p.<tag>.0.0.<zone>        │
 │     │                           │      │     │                            │
 │     ▼                           │      │     ▼                            │
 │  envelope (ver 4)               │      │  fetch TXT blobs                 │
 │  ver|type|ts|from|to|len|body   │      │     │                            │
 │     │                           │      │     ▼                            │
 │     ▼                           │      │  AEAD decrypt (AAD = msg id)     │
 │  best of raw / SMAZ / DEFLATE   │      │     │                            │
 │     │                           │      │     ▼                            │
 │     ▼                           │      │  decompress → SQLite             │
 │  ChaCha20-Poly1305 (12B nonce)  │      │     │                            │
 │     │                           │      │     ▼                            │
 │     ▼                           │      │  queue delivery receipt ─────────┼──►
 │  Base32 (RFC 4648, no padding)  │      └───────────────▲──────────────────┘
 │     │                           │                      │
 │     ▼                           │                      │
 │  ≤63-char labels, ≤253 total    │                      │
 │  m.<tag>.<msg-id>.<seq>.<total> │                      │
 └─────┼───────────────────────────┘                      │
       │ UDP :53  (TXT query)                             │
       ▼                                                  │
 ┌───────────────────────────┐                            │
 │ egress = "resolver"       │   or   direct ip:port      │
 │  public recursive DNS     │◄──────── egress = "direct" ┤
 └─────────────┬─────────────┘                            │
               │ forwards under the delegated zone        │
               ▼                                          │
 ┌────────────────────────────────────────────────────────┴──────────────┐
 │        AUTHORITATIVE VPS RELAY  (`dns_lab serve`, port 53)            │
 │  parse qname → route by <tag> → park opaque blob → TXT ack            │
 │  (no shared_key needed: it cannot decrypt what it forwards)           │
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
  nonce per message. The message id is bound in as AEAD associated data, so
  chunks cannot be moved between messages. The ciphertext is Base32-encoded for
  DNS-safe, case-insensitive labels.
- **Real key derivation**: the configured passphrase goes through
  **HKDF-SHA256** (versioned salt) instead of being padded/truncated.
- **Per-message compression**: SMAZ and DEFLATE are both tried and the smaller
  result wins (1-byte codec tag). Repetitive chat text shrinks ~3×, which
  directly reduces the number of DNS queries.
- **Signed envelopes**: every message carries an Ed25519 signature over its
  canonical header and body, so a peer cannot claim to be someone else even if
  it holds the shared passphrase. Peer keys are pinned on first contact.
- **Per-recipient keys**: after both sides have exchanged public bundles the
  conversation switches to `HKDF(X25519(ours, theirs))`. A third node holding
  the group passphrase can no longer read it — the relay never could.
- **Delivery receipts**: the recipient sends an end-to-end `Ack` envelope, so
  the sender's UI can show `sent` → `delivered`.
- **Push, not just poll**: the relay attaches anything queued for you to the ack
  of your final chunk, so a node that has just started (or cannot poll) still
  receives mail. Polling is non-destructive and acknowledged, so a lost reply
  costs a retry instead of the message.
- **Pipelined transfers**: a configurable window (`window`, default 8) of chunk
  queries in flight, replies matched by DNS transaction id, selective
  retransmit, TCP fallback on truncation.
- **Retrying outbox**: undeliverable messages are queued with a conversation
  sequence number and retried with backoff (`dns_lab run` / the GUI), instead of
  being lost.
- **File transfer**: up to 32 MiB, split into 8 KiB parts that travel as normal
  messages, verified against a SHA-256 end to end, resumable after a failure and
  written to `download_dir` only once every part has arrived and hashed.
- **Relay failover**: `relays = [...]` adds fallback relays after the primary.
  Sending stops at the first one that accepts the message; polling asks all of
  them, so mail parked on a secondary relay is still collected.
- **Blind relay**: messages are addressed by `HKDF(key, recipient-id)[..4]`
  rendered as 8 hex chars in the QNAME. The relay partitions mailboxes by that
  tag and concatenates Base32 chunks without ever holding a key — a relay config
  can leave `shared_key` empty.
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
  **ChaCha20-Poly1305** (AEAD) with a random 96-bit nonce per message, with the
  message id as associated data so ciphertexts are bound to their QNAME.
- The configured passphrase is expanded with **HKDF-SHA256** (fixed, versioned
  salt) rather than zero-padded, so short secrets are no longer trivially
  equivalent to their padded forms.
- **Senders are authenticated.** Each envelope is signed with the node's Ed25519
  key (generated locally, stored in the same SQLite file) over a canonical
  header plus body. The first time a peer is seen its signing and agreement keys
  are pinned; a later message signed by a different key is refused with a loud
  error instead of being accepted.
- **Per-recipient payload keys** (`alg = 1`): once both sides hold each other's
  X25519 bundle, bodies are encrypted with `HKDF(X25519(ours, theirs))`. The
  group passphrase alone stops being enough to read a conversation — there is a
  unit test that a third node with the same passphrase cannot open such a blob.
- **Replay cache**: a message id already ingested is rejected for the TTL of the
  cache, so a re-delivered or replayed blob cannot appear twice.
- **The relay does not hold a key.** Recipients are addressed by
  `HKDF(key, recipient-id)[..4]` as 8 hex chars; the relay routes on that tag,
  concatenates Base32 chunks and stores the result. A relay configured without
  `shared_key` has nothing that can decrypt anything, and no code path that
  tries to.
- Payloads are compressed before encryption, so body length on the wire is
  partially obscured.

Explicitly **out of scope / not provided**:

- **No third-party audit.** The cryptographic assembly is unreviewed.
- **No forward secrecy and no rotation.** Per-recipient keys are static X25519
  pairs, established once and never ratcheted. A node whose database leaks
  exposes every conversation it ever carried, past and future. There is also no
  way to rotate the group passphrase without touching every node.
- **Tags are bearer capabilities.** Anyone holding the key can derive another
  recipient's tag and poll their mailbox. There is no proof-of-possession check
  on a poll.
- **Replay protection is a cache, not a window.** A repeated message id is
  dropped for an hour; after that, a very old captured query could be accepted
  again if the recipient has no record of it.
- **Metadata leaks.** Query timing, sizes, counts, and source/destination are
  observable, and a tag that recurs is linkable even though it is not a
  plaintext identity.
- **Availability is only partly defended.** Per-source-IP token buckets and
  per-tag quotas bound the damage a single host can do, but there is no global
  budget, no connection accounting beyond 64 TCP sockets, and no protection
  against a distributed flood.
- **Public resolvers may rate-limit, block, or poison** tunnel traffic, and some
  networks prohibit it outright.

Treat `dns_lab` as an educational lab, not a privacy tool.

## Repository layout

```
dns_lab/
├── Cargo.toml                  # features: default = gui, --no-default-features = headless
├── config.example.toml         # client template
├── config.server.example.toml  # relay template
├── migrations/                 # numbered SQL migrations (0001 schema, 0002 tags, 0003 identity)
├── src/
│   ├── lib.rs                  # HKDF key derivation, AEAD helpers, randomness
│   ├── identity.rs             # Ed25519 signing key + X25519 agreement key
│   ├── file.rs                 # file parts: header framing, hashing, names
│   ├── main.rs                 # CLI: gui | serve | run | send | files | inbox | peers | id
│   ├── config.rs               # TOML config model + loader
│   ├── db.rs                   # SQLite: messages, chunks, mailbox, peers, outbox, keys
│   ├── protocol.rs             # signed envelope, codecs, AEAD blob, QNAME framing
│   ├── smaz.rs                 # SMAZ compression port (BSD-3, see LICENSES/)
│   ├── android.rs              # Android native-activity entry point
│   ├── app/mod.rs              # egui/eframe desktop + Android UI
│   └── net/
│       ├── mod.rs              # Node: sealing/opening, ingest, push intake, outbox
│       ├── sender.rs           # pipelined UDP + selective retransmit + TCP fallback
│       ├── reply.rs            # TXT reply parser (acks, pushed blobs, cursor)
│       ├── listener.rs         # authoritative UDP/TCP listener, relay, rate limits
│       └── poller.rs           # mailbox polling with backoff, across all relays
├── tests/e2e.rs                # end-to-end UDP tests (direct + relay + push + keys)
├── scripts/smoke.sh            # full-stack check against the built binary
├── scripts/bench.sh            # throughput, pipelining window 1 vs 8
├── scripts/bench_net.sh        # the same over an emulated network
├── scripts/netem.py            # userspace UDP/TCP delay/jitter/loss/rate emulator
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
dns_lab run                                  # headless client daemon (listener + poller)
dns_lab send --to <recipient-node-id> --body "hello over DNS"
dns_lab send --to <id> --body -  < message.txt   # body from stdin
dns_lab inbox --limit 20                     # recent messages + delivery state
dns_lab peers                                # known contacts
dns_lab version
```

`run` is the same node the GUI starts, minus the window: it serves the local
listener, polls its mailbox, sends delivery receipts, and keeps running until
killed. Pair it with `send`/`inbox` from another shell — they share one SQLite
file, so messages sent either way appear in one history.

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
  ./install.sh --zone t.example.com'
```

The installer creates the `dns_lab` system user, `/etc/dns_lab` and
`/var/lib/dns_lab`, writes the config, and installs/starts the hardened
systemd unit (`CAP_NET_BIND_SERVICE`, `ProtectSystem=strict`).

The relay is installed **without a key**: it routes on routing tags and never
decrypts, so there is nothing on that box worth stealing but queued ciphertext.
Pass `--shared-key ...` only if the VPS also chats. Edit the config later with:

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
| `shared_key` | *empty* | Passphrase for clients (identical on every node that sends/reads). **Empty = no key**: the node cannot encrypt, and a relay becomes a blind forwarder |
| `peers` | `[]` | `[[peers]]` with `id` / `name` / `addr` / `via` |

## Wire format

All of this is internal, unstable, and versioned by the `VERSION` byte inside
the encrypted envelope. Bumping it (currently `5`) breaks compatibility on
purpose.

- **Routing tag.** `tag = lower_hex(HKDF-SHA256(key, salt="dns_lab/v4-route",
  info=recipient_id)[..4])` — 8 hex chars. It is the only recipient handle that
  travels in the clear; it is stable per (key, id) and useless without the key.
- **Query names:**
  - chunk: `m.<tag>.<msg-id>.<seq>.<total>.<data>.<zone>`
  - final chunk: `f.<tag>.<msg-id>.<seq>.<total>.<data>.<zone>` (names the
    sender, so the relay can push in the ack)
  - poll:  `p.<tag>.0.0.<zone>`
  - ack:   `a.<tag>.<id>.<id>...<zone>` (mailbox blobs the client stored)
  - `data`: uppercase Base32 (RFC 4648, no padding), split across labels of
    ≤63 chars; the whole name is kept ≤253 chars.
  - Incoming names are validated strictly: known verbs only, a well-formed
    8-hex tag, `msg-id` ≤16 alphanumeric chars, `0 < total ≤ 4096`,
    `seq < total`, and polls must be exactly `p.<tag>.0.0.<zone>`.
- **Envelope (plaintext, before encryption):**

  ```
  version(1) | msg_type(1) | alg(1) | flags(1) | ts(8, LE) | conv_seq(8, LE)
  | from_len(1) | from | to_len(1) | to | [bundle(64)]
  | body_len(4, LE) | codec(1) | body
  ```

  `msg_type`: 1 = message, 2 = delivery receipt, 3 = read marker, 4 = heartbeat,
  5 = file part.
  `alg`: 0 = shared group key, 1 = X25519 per-recipient key. `flags` carries
  `bundle` (a 64-byte Ed25519‖X25519 bundle follows) and `peer_has_key` (I hold
  *your* bundle, so you can switch to `alg = 1`). `codec`: 0 = raw, 1 = SMAZ,
  2 = DEFLATE — the encoder picks the smallest result.
- **Signature.** `Ed25519(node_key, canonical_header ‖ body)` is appended to the
  envelope before encryption, and covers the codec tag and body length too, so
  a chunk cannot be re-framed.
- **On the wire:** `Base32( nonce[12] || ChaCha20-Poly1305(envelope, aad = msg-id) + tag )`,
  chunked into as many queries as needed.
- **Replies:** a single TXT record carrying, in order, chunk acks
  `a:<msg-id>:<seq>`, pushed blobs `m:<msg-id>:<seq>:<total>:<data>` and
  `cursor:<n>`. The sender verifies the ack fields, so a cached or forged answer
  can never fake a delivery. A reply that would exceed the client's UDP payload
  size is truncated (TC bit) and the client re-asks over TCP.
- **Receipts:** when a message is stored for the local node, an `Ack` envelope
  whose body is the received message id is sent back through the same egress.
  Receipts are never acknowledged themselves and never appear as chat rows; they
  flip the sender's status to `delivered`.
- **File parts** carry an ASCII header inside the encrypted body, then raw bytes:

  ```
  DLF1\n<transfer-id>\n<name>\n<size>\n<sha256>\n<part>/<parts>\n<part-bytes>\n\n<bytes>
  ```

  The name is a base name with control characters stripped, the size is capped
  at 32 MiB, and `part-bytes` lets the receiver sanity-check the frame before it
  touches the database. Per-part message ids are `<transfer-id><part-in-hex>`,
  so a retried part cannot become a second copy.
- **Relay behaviour:** with a tag that is not its own, a relay buffers chunks by
  `(tag, msg-id)`, concatenates them once `total` are present, stores the blob
  under the tag and deletes the chunks — one transaction per chunk. It never
  base32-decodes, never decrypts and never parses an envelope.

## Testing

```sh
cargo test --all-targets                                   # 72 unit + 9 e2e
cargo test --no-default-features --all-targets             # headless build
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
./scripts/smoke.sh                                         # real binary, relay + 2 clients
./scripts/bench.sh                                         # loopback throughput, window 1 vs 8
./scripts/bench_net.sh                                     # the same, through an emulated network
```

The end-to-end tests spin up real nodes on localhost UDP: a pipelined
multi-chunk transfer (including the compression win and the incompressible
case), a relay mailbox round trip big enough to force TCP fallback, an
end-to-end delivery receipt, a **keyless relay** that forwards mail it provably
cannot read, **push delivery to a node that never polls**, a conversation that
upgrades itself to per-recipient keys, and two file transfers (byte-for-byte
verification with a delivery receipt, and a resume that does not duplicate). `scripts/smoke.sh` goes one level further and drives the shipped
binary: it starts a relay plus two client daemons, sends a message, and asserts
the sender sees `delivered`, the recipient sees `received`, and the relay kept
no plaintext.

## Benchmarking

Two harnesses, both driving the shipped binary rather than the library:

- `scripts/bench.sh` — sends through a local relay with a simulated reply delay.
  It answers "does pipelining work": 100 KB at 25 ms one-way went from 16.3 s
  (window 1) to 2.1 s (window 8).
- `scripts/bench_net.sh` — puts `scripts/netem.py` in the path: a userspace
  UDP/TCP emulator that applies one-way delay, jitter, loss and a bandwidth cap
  to the real packets **in both directions**, and needs no root (unlike
  `tc netem`). It reports the DNS queries the message needed, the datagrams the
  link actually carried, how many were dropped, and the resulting transfer time.

A 2 KB and a 16 KB message, and a 32 KiB file, on four link profiles. Every row
is one real run; `wire` above `chunks` is the cost of loss and jitter.

```
profile   payload window  chunks    wire dropped millisecs     KB/s  retrans%  state
lan        2048B      1      15      15       0       102       20         0%     ok
lan        2048B      8      15      15       0        48       42         0%     ok
lan       16384B      8     103     103       0        71      230         0%     ok
lan       32768B      8     212     212       0       148      221         0%     ok
wifi       2048B      1      15      15       0       498        4         0%     ok
wifi       2048B      8      15      15       0       100       20         0%     ok
wifi      16384B      8     103     103       0       457       35         0%     ok
wifi      32768B      8     212     214       4      1390       23         0%     ok
lte        2048B      1      15      15       0      1369        1         0%     ok
lte        2048B      8      15      15       0       234        8         0%     ok
lte       16384B      8     103     103       2      1889        8         0%     ok
lte       32768B      8     212     214       6      3829        8         0%     ok
bad        2048B      1      15      16       3      4909        0         6%     ok
bad        2048B      8      15      16       3      1992        1         6%     ok
bad       16384B      8     103     112      13      7271        2         8%     ok
bad       32768B      8     212     223      24     14523        2         5%     ok
```

Profiles: `lan` 2 ms RTT; `wifi` 30 ms ±8 ms, 0.5 % loss, 4 Mbps; `lte` 90 ms
±20 ms, 2 % loss, 1.2 Mbps; `bad` 240 ms ±40 ms, 5 % loss, 400 kbps. The
`32768B` rows are 32 KiB file transfers (4 parts); the rest are single messages.
Run-to-run variance at these sizes is roughly ±10 % for the timed columns.

What the numbers say:

- **Pipelining is the whole story on a real link.** At 90 ms RTT the window
  turns a 1.4 s message into 0.23 s — the transfer stops being round-trip bound
  and becomes bandwidth bound.
- **Loss was costing a full timeout per drop.** The retransmit timer used to be
  the fixed `timeout_ms`, so every dropped datagram cost 800 ms even at 30 ms
  RTT. It now tracks the measured RTT — Jacobson/Karels, plus the recent *peak*
  (a burst queues at the relay, so the average reply time badly understates the
  worst one), with Karn's rule and exponential backoff. On the `bad` profile a
  2 KB message went from 6.1 s to 4.9 s and a 16 KB one from 8.1 s to 7.3 s at
  the same 5 % loss; on a clean link the saving is larger in relative terms
  because there is no latency to hide the timeout behind.
- **Getting that wrong is easy, and the emulator caught it.** The first version
  floored the timer at 150 ms, which was below the relay's own queueing under a
  burst of eight; the sender resent packets that were merely queued, Karn's rule
  then refused to learn from those resends, and the retry budget ran out. The
  `smoke.sh` file transfer failed on loopback because of it. The fix was a
  higher floor *and* TCP-style backoff, which is why the numbers above are worth
  more than the ones a hand-tuned constant would produce.
- **The relay was the bottleneck before the wire ever was.** A relayed chunk
  used to cost four SQLite transactions, each with its own fsync (~30 ms), which
  is why pipelining showed no benefit at first. One transaction per chunk, with
  WAL and `synchronous = NORMAL`, removed it.
- **Percent-level loss is survivable, and visible.** At 5 % loss per direction,
  the link drops 13 of 112 datagrams and the send still completes with an
  integrity-checked result.

Limits worth knowing: the emulator is userspace Python, so it adds scheduling
jitter of its own (fine at these rates, not for line-rate tests), and the
bandwidth bucket is per direction rather than per connection.

## Known quirks, limitations & roadmap

Known issues (contributions welcome):

- **Android insets / notches**: the UI reads the NativeActivity content rect to
  avoid the status/navigation bars, but falls back to a 24 dp top inset if the
  platform reports nothing. Behaviour varies by device.
- **At-most-once delivery per message.** Chunk acks, selective retransmit, TCP
  fallback, an outbox with 20 retries and end-to-end receipts are all in place,
  but a relay that accepts every chunk and then dies still loses the message:
  the relay's mailbox is the only copy until the recipient acks it. Chunks older
  than 5 minutes and mail older than 7 days are purged.
- **No forward secrecy.** Per-recipient keys are static: a stolen X25519 secret
  decrypts every message ever sent to that node.
- **Files are capped at 32 MiB** and there is no bandwidth sharing: a large
  transfer takes the tunnel for as long as it runs, one part at a time.
- **Security**: no third-party audit. Signatures, pinning and per-recipient keys
  are implemented but reviewed only by the test suite (see
  [Security & threat model](#security--threat-model)).
- **Robustness**: the relay rate-limits per source IP and per tag, and caps 64
  TCP connections, but there is no global connection or memory budget;
  aggressive resolvers may still drop or poison traffic.
- **Relay trust**: the relay sees which routing tag gets how many bytes. It
  keeps no plaintext and never learns an id, but a malicious one can log
  metadata, and it can sit on a message for up to 7 days.
- **Single-writer SQLite**: one `Mutex<Connection>` serves every thread, so the
  send and poll paths serialise on it. Fine at chat rates, not a bulk transport.

Roadmap, in the order that pays off most:

**Near term (small, contained)**

- **Metrics endpoint.** Queries, chunks, decodes, mailbox depth and rejects on a
  loopback `prometheus`-style listener; the counters already exist in `Stats`.
- **`thiserror` pass** over the `Result<_, String>` signatures, then let the GUI
  and CLI branch on error *kind* instead of message text.
- **Resumable sends.** The outbox remembers which chunks were acked, so a
  restart resumes a 200 KB message instead of re-sending it from the top.

**Deeper changes (each is a protocol revision)**

- **Tag ownership proofs.** A poll currently only needs the tag, which makes a
  tag a bearer capability for any key holder. Have the client prove possession
  (HMAC over the relay's nonce) so a leaked tag alone fetches nothing.
- **Ratcheting.** Per-recipient keys are already in place; a Double-Ratchet-style
  chain would add forward secrecy, so a stolen long-term key cannot decrypt old
  mail. Today a compromised X25519 secret exposes every conversation it ever
  carried.
- **DoH / DoT transport.** Send tunnel queries as HTTPS POSTs to a resolver, so
  traffic looks like ordinary web traffic and survives UDP blocking.
- **Label-space upgrade.** Base32 costs 60% overhead. A case-insensitive
  "base-n" that packs 5→6 bits, or binary labels with escaping, plus `NS`
  delegation chaining, would roughly halve the query count per message.
- **Relay federation.** `relays` already gives a client a fallback chain, but
  relays do not talk to each other: mail parked on a relay the client stops
  reaching stays there. Gossip or a shared pull would close that gap.

**Structural cleanup (no wire change)**

- **Typed errors.** ~40 signatures return `Result<_, String>`; a `thiserror`
  enum would let the GUI and CLI distinguish "relay down" from "bad key"
  instead of matching on message text.
- **Split `net/mod.rs`.** It is the largest file and mixes node lifecycle,
  sealing/opening, ingestion, push intake and the outbox worker.
- **Database actor.** SQLite is behind a `Mutex<Connection>`; a single owning
  thread with a command channel would remove the lock from the send path and
  make batching natural.
- **`Runtime` split.** `egress_for` re-parses `SocketAddr` strings on every
  query; parsing once at `Node::new` would remove that from the hot path.

**Product surface**

- **Better Android UI** (list/detail, background polling via WorkManager),
  insets handling, and notification on new mail.
- **Packaging**: `.deb`, container image, and reproducible release artifacts.
- **GUI**: message search, per-peer mute/block, and a real packet view
  (expand a message into the exact QNAMEs it was sent as).

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

Licensed under the MIT license ([LICENSE-MIT](LICENSE-MIT)).

### Third-party notices

- `src/smaz.rs` is a Rust port of [SMAZ](https://github.com/antirez/smaz) by
  Salvatore Sanfilippo, licensed under the 3-clause BSD license. See
  [`LICENSES/SMAZ.txt`](LICENSES/SMAZ.txt). That notice must be retained when
  redistributing this source or a build containing it.
