PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS identity (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS peers (
    id        TEXT PRIMARY KEY,
    name      TEXT NOT NULL DEFAULT '',
    addr      TEXT NOT NULL DEFAULT '',
    via       TEXT NOT NULL DEFAULT 'relay',
    last_seen INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS messages (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    msg_id     TEXT NOT NULL UNIQUE,
    direction  TEXT NOT NULL,
    peer_id    TEXT NOT NULL,
    peer_name  TEXT NOT NULL DEFAULT '',
    body       TEXT NOT NULL,
    status     TEXT NOT NULL DEFAULT 'sent',
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_messages_peer ON messages(peer_id, created_at);

CREATE TABLE IF NOT EXISTS chunks (
    msg_id      TEXT NOT NULL,
    seq         INTEGER NOT NULL,
    total       INTEGER NOT NULL,
    data        BLOB NOT NULL,
    received_at INTEGER NOT NULL,
    PRIMARY KEY (msg_id, seq)
);
CREATE INDEX IF NOT EXISTS idx_chunks_received ON chunks(received_at);

CREATE TABLE IF NOT EXISTS mailbox (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    to_id      TEXT NOT NULL,
    msg_id     TEXT NOT NULL,
    blob       TEXT NOT NULL,
    delivered  INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    UNIQUE(to_id, msg_id)
);
CREATE INDEX IF NOT EXISTS idx_mailbox_recipient ON mailbox(to_id, delivered);

CREATE TABLE IF NOT EXISTS settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
