-- v5: identities, pinned peer keys, an outbox for retries, a replay cache and
-- per-conversation sequence state.

-- Ed25519 signing seed + X25519 agreement secret for this node (single row).
CREATE TABLE IF NOT EXISTS node_identity (
    id         INTEGER PRIMARY KEY CHECK (id = 1),
    ed_secret  BLOB NOT NULL,
    x_secret   BLOB NOT NULL,
    created_at INTEGER NOT NULL
);

-- Public keys pinned on first contact (TOFU). A changed key is a red flag and
-- is reported instead of silently accepted.
ALTER TABLE peers ADD COLUMN ed_public BLOB;
ALTER TABLE peers ADD COLUMN x_public BLOB;
ALTER TABLE peers ADD COLUMN pinned_at INTEGER NOT NULL DEFAULT 0;

-- Messages that could not be delivered yet; a worker retries with backoff.
CREATE TABLE IF NOT EXISTS outbox (
    msg_id     TEXT PRIMARY KEY,
    to_id      TEXT NOT NULL,
    body       TEXT NOT NULL,
    conv_seq   INTEGER NOT NULL DEFAULT 0,
    attempts   INTEGER NOT NULL DEFAULT 0,
    next_try   INTEGER NOT NULL,
    created_at INTEGER NOT NULL
);

-- Recently accepted message ids, so replayed queries die before touching disk.
CREATE TABLE IF NOT EXISTS seen_msgs (
    msg_id  TEXT PRIMARY KEY,
    seen_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_seen_msgs_age ON seen_msgs(seen_at);

-- Per-conversation counters: our outgoing sequence and the highest inbound
-- sequence seen, so a missing message can be noticed instead of just absent.
CREATE TABLE IF NOT EXISTS conv_state (
    peer_id       TEXT PRIMARY KEY,
    out_seq       INTEGER NOT NULL DEFAULT 0,
    in_seq        INTEGER NOT NULL DEFAULT 0,
    -- 1 once the peer has told us it holds our X25519 key, which is what lets
    -- us switch that conversation to per-recipient payload keys.
    peer_knows_us INTEGER NOT NULL DEFAULT 0
);
