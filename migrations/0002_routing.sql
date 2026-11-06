-- v4: the relay routes on an opaque tag instead of a plaintext recipient id.
--
-- Chunks are scoped by tag so two senders that happen to pick the same random
-- message id cannot interleave. The old `chunks` table has a (msg_id, seq)
-- primary key that cannot be altered in place, and its framing predates v4, so
-- in-flight partial messages are dropped: they are at most 5-minute artefacts.
--
-- The old `mailbox` table is likewise replaced; queued v3 blobs cannot be
-- decoded by a v4 node, so nothing of value is lost.

CREATE TABLE IF NOT EXISTS message_chunks (
    tag         TEXT NOT NULL,
    msg_id      TEXT NOT NULL,
    seq         INTEGER NOT NULL,
    total       INTEGER NOT NULL,
    data        BLOB NOT NULL,
    received_at INTEGER NOT NULL,
    PRIMARY KEY (tag, msg_id, seq)
);
CREATE INDEX IF NOT EXISTS idx_message_chunks_received ON message_chunks(received_at);

DROP TABLE IF EXISTS chunks;

CREATE TABLE IF NOT EXISTS mailbox_routes (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    route      TEXT NOT NULL,
    msg_id     TEXT NOT NULL,
    blob       TEXT NOT NULL,
    delivered  INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    UNIQUE(route, msg_id)
);
CREATE INDEX IF NOT EXISTS idx_mailbox_route ON mailbox_routes(route, delivered);

DROP TABLE IF EXISTS mailbox;
