-- v6: file transfers. A file is split into parts; each part is a normal signed
-- message of type `File`, and these tables reassemble it on the receiving side
-- (and remember what the sending side already delivered, for resumes).

CREATE TABLE IF NOT EXISTS files (
    transfer_id TEXT PRIMARY KEY,
    direction   TEXT NOT NULL,               -- 'in' | 'out'
    peer_id     TEXT NOT NULL,
    name        TEXT NOT NULL,
    size        INTEGER NOT NULL,
    sha256      TEXT NOT NULL,
    parts       INTEGER NOT NULL,
    part_bytes  INTEGER NOT NULL,
    path        TEXT NOT NULL DEFAULT '',
    complete    INTEGER NOT NULL DEFAULT 0,
    created_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_files_peer ON files(peer_id, created_at);

-- Parts parked until the whole file is present. Deleted on completion.
CREATE TABLE IF NOT EXISTS file_parts (
    transfer_id TEXT NOT NULL,
    part        INTEGER NOT NULL,
    data        BLOB NOT NULL,
    received_at INTEGER NOT NULL,
    PRIMARY KEY (transfer_id, part)
);

-- Parts we have already pushed to the relay, so a resumed transfer skips them.
CREATE TABLE IF NOT EXISTS file_sent_parts (
    transfer_id TEXT NOT NULL,
    part        INTEGER NOT NULL,
    PRIMARY KEY (transfer_id, part)
);

-- Chat rows can point at a transfer instead of being plain text.
ALTER TABLE messages ADD COLUMN file_id TEXT;
