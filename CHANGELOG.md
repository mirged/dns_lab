# Changelog

All notable changes to this project. The wire format is unstable: any `VERSION`
bump below is a deliberate compatibility break, and every node must be updated
together.

## 0.6.1 — measured on a real emulated network

### Performance

- **The retransmit timer now tracks the RTT.** It used to be the fixed
  `timeout_ms`, so on a lossy link every dropped datagram cost the full 800 ms
  even when the round trip was 30 ms. It is now Jacobson/Karels
  (`srtt + 4 * variance`), floored at 250 ms, capped by the configured timeout,
  and raised to cover the recent peak so a burst queueing at the relay is not
  mistaken for loss. Karn's rule keeps a retransmit from inflating its own
  estimate, and every timeout doubles the next wait (TCP-style backoff) so a
  stall cannot burn the attempt budget. At 5 % loss per direction and 240 ms
  RTT: a 2 KB message went 6.1 s -> 4.9 s, a 16 KB message 8.1 s -> 7.3 s.
- **The relay was measured too.** Its per-chunk cost was four SQLite
  transactions with an fsync each (~30 ms), which is why pipelining showed no
  benefit until that became one transaction with `synchronous = NORMAL`.
- Measured, not asserted: the table in the README
  (2 KB / 16 KB / 32 KiB file across lan, wifi, lte and bad profiles).

### Tooling

- `scripts/netem.py` — a userspace UDP/TCP network emulator: one-way delay,
  jitter, loss and a bandwidth cap applied to the real packets in both
  directions, with per-direction counters. No root required, unlike `tc netem`,
  and it is UDP-aware so the TCP fallback path is emulated too.
- `scripts/bench_net.sh` — 4 link profiles against 2 message sizes and a file
  transfer, at window 1 and 8, reporting queries needed, datagrams carried,
  drops and the resulting retransmit ratio.

### Tests

- 72 unit tests, including one that pins the retransmit timer's behaviour: fast
  links retransmit quickly, jittery links get headroom, the cap and the floor
  both hold.

## 0.6.0 — file transfers

### Files

- **Attachments.** `dns_lab send --to ID --file PATH` (and a "Send file" box in
  the GUI composer) transfers a file up to 32 MiB. The file is split into 8 KiB
  parts; each part is a normal signed, encrypted envelope of the new message
  type `File`, so it gets the same routing, retransmission and delivery
  guarantees as a message — no second transport to maintain.
- **Integrity is enforced, not assumed.** Every part carries the transfer id,
  name, total size and the SHA-256 of the whole file. The receiver assembles the
  parts, checks the length *and* the hash, and only then writes the file; a
  mismatch discards the parts and logs an error instead of leaving a corrupt
  file on disk.
- **Resumable.** Per-part ids are deterministic (`<transfer-id><part>`), and the
  sender records which parts the relay accepted. Re-running the same command —
  or calling `send_file` again with the same transfer id — skips what already
  went out and reports how much it resumed.
- **Names cannot escape the download directory.** The receiver keeps only the
  base name, strips control characters and caps the length; the file is written
  as `<transfer-id>-<name>` under `download_dir`.
- **One receipt per transfer**, sent when the assembled file has been verified,
  so the sender's chat row flips to `delivered` once — not once per part.
- New `dns_lab files [--limit N]` lists transfers with direction, size, part
  count and either the saved path or the peer.

### Fixed

- **Batched mailbox acknowledgements were rejected.** Several acknowledged
  message ids were joined with commas into a single DNS label, which is not a
  legal name, so any acknowledgement covering more than one message failed
  ("invalid label") and the mail was re-offered forever. Ids now get one label
  each, and `build_ack_qname` trims the list to stay inside 253 characters. A
  unit test covers the multi-id and long-zone cases.

### Internals

- Envelope bodies are byte strings instead of `String`, because a file part is
  binary. SMAZ is only tried when the body happens to be UTF-8; DEFLATE and raw
  always apply. Text messages still validate as UTF-8 on arrival.
- Migration `0004_files` adds `files`, `file_parts`, `file_sent_parts` and a
  `messages.file_id` column; `Db::latest_version()` keeps tests from hard-coding
  the schema number.

### Tests

- 71 unit tests (header round trip, path-traversal-safe names, hash vectors,
  part splitting, bad-header rejection, ack QNAME legality) and 9 end-to-end
  tests, two of them for files: a 21 KB transfer verified byte for byte with a
  delivery receipt, and a re-send that resumes instead of duplicating.

## 0.5.0 — protocol v5: identities, per-recipient keys, push

### Wire format (breaking)

- `VERSION` is now `5`. The envelope is
  `type | alg | flags | ts | conv_seq | from | to | [bundle] | body | sig`, and
  the signature is **Ed25519 over the canonical header and body**, so `from` is
  authenticated. In v4 anyone holding the shared passphrase could claim any
  sender id; now they cannot sign as someone else.
- `alg` selects the payload key: `0` = shared group key, `1` =
  `HKDF(X25519(ours, theirs))`. A 64-byte `bundle` (Ed25519 ‖ X25519 public
  keys) is carried on first contact, and a `peer_has_key` flag tells a peer that
  we hold their bundle, so a conversation upgrades from the group key to
  per-recipient keys by itself.
- `conv_seq` numbers each direction of a conversation, so ordering and gaps are
  explicit instead of inferred from receipt times.
- New verbs: `f` marks the **final** chunk of a message (it names the sender, so
  the relay can push queued mail in the same reply), `a` acknowledges stored
  mailbox blobs (`a.<tag>.<id>.<id>...`), and `m`/`p` keep their v4 meaning.
- Reply TXT records now carry three things in one answer: chunk acks
  (`a:<msg-id>:<seq>`), pushed blobs (`m:<msg-id>:<seq>:<total>:<data>`) and a
  `cursor:<n>` hint. Truncated replies are flagged, so the client re-asks over
  TCP instead of silently losing the payload.

### Delivery

- **Push.** A node that cannot poll (or has just started) still receives mail:
  the relay attaches anything queued for the sender to the ack of the final
  chunk. Polling drops from "the only way to get mail" to a fallback.
- **Non-destructive polling.** A poll offers blobs; the client stores them and
  then acknowledges by message id. Losing a reply costs a retry instead of the
  message, and a crash between receiving and storing no longer loses mail.
- **Outbox.** Messages that cannot be delivered (relay down, TCP-only path
  failing) are queued with a sequence number and retried with backoff for 20
  attempts. `dns_lab send` leaves a failed message in the queue and says so.
- **Replay cache.** `seen_msgs` rejects a message id that was already ingested
  (re-delivery after a lost ack, or a replayed blob).
- **TOFU key pinning.** A peer's signing and agreement keys are pinned on first
  contact. A later message signed by a different key is refused with a loud
  error rather than silently accepted.

### Transport

- **Pipelining.** The sender keeps a window of chunk queries in flight
  (`window`, default 8), matches replies by DNS transaction id, retransmits only
  the chunks that were not acked, and falls back to TCP when a reply is
  truncated. Cost is `ceil(chunks / window)` round trips rather than one per
  chunk.
- **The relay is no longer disk-bound.** A relayed chunk used to cost four
  separate SQLite transactions (insert, read back, park, delete), each with its
  own fsync, which made the relay — not the network — the bottleneck at ~30 ms
  per query. It is now one transaction, and the database runs WAL with
  `synchronous = NORMAL`.
- **Abuse limits.** Per-source-IP token bucket (`rate_limit_per_sec`, default
  40/s) and per-tag quotas (`max_blobs_per_route` 200, `max_bytes_per_route`
  5 MiB); a relay drops rather than grows without bound.
- **Relay failover.** `relays = [...]` is a fallback chain: sending tries each
  relay in order until one accepts the message, and polling asks all of them, so
  a dead primary relay costs a retry rather than the mail.
- **Relay roles.** `relay_mode = client | peer | authoritative` makes the
  intent explicit: a client never forwards, a peer forwards with quotas, and an
  authoritative relay also answers SOA/NXDOMAIN for the zone.

### Benchmarks

`scripts/bench.sh` measures a real send through a local relay with a simulated
25 ms reply delay (window 1 is the old serial path):

```
    size window  queries millisecs
    1024      1        9       322
    1024      8        9        83
   10240      1       65      1717
   10240      8       64       253
  102400      1      630     16315
  102400      8      630      2146
```

### CLI & GUI

- `dns_lab id` prints the node id **and** its key fingerprint; `dns_lab send`
  reports the retry queue on failure.
- The GUI shows the key fingerprint (copyable, for out-of-band comparison), the
  pipelining window, the relay role, quotas, and the retry-queue counter.

### Tests

- 64 unit tests and 6 end-to-end tests. New end-to-end coverage: pipelined
  multi-chunk transfer, compressed single-query send, relayed long message over
  TCP fallback, delivery receipt, keyless relay, **push delivery without a
  poller**, and a conversation that upgrades to per-recipient keys.
- A unit test pins the security claim behind `alg = 1`: a third node holding the
  shared passphrase cannot open a peer-keyed blob.
- `scripts/bench.sh` with a delay knob; `scripts/smoke.sh` still asserts the
  relay keeps no plaintext.

## 0.4.0 — protocol v4: blind relay

### Wire format (breaking)

- `VERSION` is now `4`.
- Mail is addressed by a **routing tag**: `lower_hex(HKDF-SHA256(key,
  salt="dns_lab/v4-route", info=recipient_id)[..4])`, 8 hex chars. The QNAMEs are
  now `m.<tag>.<msg-id>.<seq>.<total>.<data>.<zone>` and
  `p.<tag>.0.0.<zone>`; the plaintext recipient id no longer appears anywhere on
  the wire.
- Reassembly chunks are keyed by `(tag, msg-id)`, so two senders that pick the
  same random message id cannot interleave.
- `max_data_chars` is now derived from the real header instead of a magic
  constant, with a test that long zones shrink the payload rather than overrun
  the 253-char limit.

### The relay is now blind

- Relaying happens on the QNAME alone: a relay buffers chunks by tag, joins the
  Base32 pieces when the declared count is reached, parks the blob and deletes
  the chunks. It never decodes Base32, never decrypts and never parses an
  envelope.
- A relay configured with an empty `shared_key` holds **nothing** that can read
  traffic; there is no code path that tries.
- Relay log lines name only the tag, message id and size — no sender, no
  recipient.
- Mail whose authenticated envelope names a different recipient is dropped by
  the receiving node instead of being stored.

### Configuration & storage

- `shared_key` now defaults to **empty**, meaning "no key". A node without one
  fails loudly (`no shared_key configured`) instead of silently using the
  bundled lab secret.
- The example relay configs ship without a key, and `deploy/headless/install.sh`
  no longer generates one (pass `--shared-key` if the VPS also chats).
- SQLite migrations are numbered and recorded in `schema_version`; `0002_routing`
  replaces the `chunks` and `mailbox` tables. In-flight v3 chunks and queued v3
  blobs are dropped on upgrade — they are ≤5-minute artefacts in an
  incompatible framing.

### CLI & GUI

- `dns_lab serve` reports its key state (`none - blind forwarder`,
  `EXAMPLE PLACEHOLDER`, or `configured`), and `dns_lab run` warns when a client
  has no key.
- The GUI shows the derived **routing tag** (with a copy button) next to the node
  id, and distinguishes "no key" from the example placeholder.

### Tests

- 43 unit tests (tag derivation, tag-scoped reassembly, keyless nodes, migration
  bookkeeping, config defaults) and 5 end-to-end tests, including a keyless
  relay that forwards mail and provably stores no plaintext.
- `scripts/smoke.sh` now runs the relay with no key and asserts the relay log
  contains no decrypted message.

## 0.3.0 — protocol v3

### Wire format (breaking)

- `VERSION` is now `3`. Envelopes carry a **codec tag** byte, and the AEAD
  **associated data is the tunnel message id**, so a ciphertext is bound to the
  QNAME that carries it.
- Message bodies are compressed with **whichever of raw / SMAZ / DEFLATE is
  smallest** instead of SMAZ-only. Repetitive chat text shrinks ~2–3×, which
  reduces the number of DNS queries per message.
- Incoming QNAMEs are validated strictly: known verbs only, `msg-id` limited to
  16 alphanumeric chars, `0 < total ≤ MAX_CHUNKS`, `seq < total`, and poll
  queries may not carry a payload.
- Chunk acknowledgements are verified against `a:<msg-id>:<seq>` instead of
  accepting any NOERROR answer — a cached or forged response can no longer fake
  a delivery.

### Crypto

- The `shared_key` passphrase is expanded with **HKDF-SHA256** (fixed, versioned
  salt) rather than zero-padded/truncated to 32 bytes.
- New `encrypt_with_key_aad` / `decrypt_with_key_aad` helpers; the no-AAD
  variants remain for tests and interop.

### Delivery

- **End-to-end receipts.** A receiving node answers a stored message with an
  `Ack` envelope whose body is the received message id. The sender's message
  then shows `sent` → `delivered`. Receipts are never acknowledged and never
  appear as chat rows.
- All inbound handling now goes through one `Node::ingest` funnel shared by the
  listener and the poller, so storage, relaying and receipts cannot drift apart.
- **Relays no longer keep other people's plaintext.** A message addressed to
  someone else is parked as an opaque blob in the mailbox and not written to the
  relay's own message table.
- Mailbox entries are garbage-collected after 7 days and once delivered
  (previously the table grew forever). Housekeeping also drops stale chunks
  every 60 s.
- The poller backs off exponentially (capped at 60 s) instead of retrying a dead
  relay on a fixed cadence.

### Robustness

- `MAX_CHUNKS` (4096) caps reassembly memory; oversized bodies fail fast with a
  clear error instead of filling the disk.
- `MAX_BODY_BYTES` (256 KiB) caps decompression, so a hostile peer cannot
  zip-bomb the receiver.
- At most 64 TCP connections are served at once; extra ones are dropped with a
  log line.
- Sends check the final QNAME length and report a helpful error when the
  configured `zone` is too long.

### CLI

- New `dns_lab run` (headless client daemon: listener + poller + receipts) and
  `dns_lab inbox` / `dns_lab peers` commands.
- `dns_lab send --body -` reads the message from stdin.
- `dns_lab send` records the message locally, so it appears in history and can
  receive a delivery receipt.
- `dns_lab version` and `--help`; unknown commands exit with status 2 and print
  usage.
- GUI startup failures are reported instead of being silently swallowed.

### GUI

- Conversation list and chat pane are separate again (the conversation used to
  be rendered twice), with a filter box and click targets the full width of the
  pane.
- Per-message timestamps (UTC), direction, and a word-based delivery state
  (`sending`, `sent`, `delivered`, `failed`) plus an inline retry on failure.
  Status words are used instead of ✓/✕ because egui's bundled font has no
  glyphs for them.
- Setup is grouped into Identity / Transport / Contacts, with **generate key**,
  **new node id**, copy buttons, and warnings for the built-in lab key or a
  missing resolver.
- DNS inspector shows counters (sent / received / delivered / failed /
  assembling / queued), a log filter, and copy-all.

### Tests

- 33 unit tests (crypto AAD + KDF, codecs, QNAME validation, DB, ingest/relay
  logic, UI helpers) and 4 end-to-end tests (direct multi-chunk and compressed
  single-chunk, long relayed message forcing TCP fallback, opaque relay
  forwarding, end-to-end receipt).

## 0.2.0

- Relay no longer drops long messages when the poll reply is truncated over UDP;
  the mailbox entry is consumed only after the reply is actually sent.

## 0.1.0 — initial public release

- DNS-tunnel messenger: encrypted, compressed messages in TXT queries, relay
  mailbox, desktop GUI, headless CLI, experimental Android client.
