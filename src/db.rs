//! Local SQLite storage: messages, reassembly chunks, relay mailbox and peers.

use crate::config::Peer;
use rusqlite::{Connection, params};
use std::sync::Mutex;

#[derive(Debug, Clone)]
pub struct MessageRow {
    pub id: i64,
    pub msg_id: String,
    pub direction: String,
    pub peer_id: String,
    pub peer_name: String,
    pub body: String,
    pub status: String,
    pub created_at: i64,
    /// Set when this row announces a file transfer.
    pub file_id: Option<String>,
}

/// One file transfer, in either direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRow {
    pub transfer_id: String,
    /// `"in"` or `"out"`.
    pub direction: String,
    pub peer_id: String,
    pub name: String,
    pub size: u64,
    pub sha256: String,
    pub parts: u32,
    pub part_bytes: usize,
    /// Where the assembled file lives (empty until complete).
    pub path: String,
    pub complete: bool,
    pub created_at: i64,
}

#[derive(Debug, Clone)]
pub struct Conversation {
    pub peer_id: String,
    pub peer_name: String,
    pub last_body: String,
    pub last_ts: i64,
}

/// Aggregate counters shown in the UI status bar.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub sent: i64,
    pub received: i64,
    pub delivered: i64,
    pub failed: i64,
    pub pending_chunks: i64,
    pub mailbox: i64,
    pub outbox: i64,
}

/// Stored `(ed25519 seed, x25519 secret)` for this node.
pub type StoredSecrets = ([u8; 32], [u8; 32]);

/// Pinned `(ed25519, x25519)` public keys, still in storage form.
pub type PinnedKeys = (Option<Vec<u8>>, Option<Vec<u8>>);

/// One chunk on its way through the relay.
#[derive(Debug, Clone, Copy)]
pub struct RelayChunk<'a> {
    pub tag: &'a str,
    pub msg_id: &'a str,
    pub seq: u32,
    pub total: u32,
    pub data: &'a [u8],
}

/// Per-recipient limits on what the relay will park.
#[derive(Debug, Clone, Copy)]
pub struct Quota {
    pub max_blobs: i64,
    pub max_bytes: i64,
}

/// What happened to a relayed chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayStore {
    /// Still waiting for the other chunks.
    Buffered,
    /// Joined and parked for the recipient.
    Stored { chunks: usize, blob_chars: usize },
    /// The recipient's mailbox is full.
    Quota { blobs: i64, bytes: i64 },
}

/// Result of pinning a peer's public keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinOutcome {
    /// First sight: keys are now pinned.
    Pinned,
    /// Matches the existing pin.
    Same,
    /// A different key showed up for the same id. Not accepted.
    Changed,
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    pub fn open(path: &str) -> Result<Db, String> {
        let conn = Connection::open(path).map_err(|e| e.to_string())?;
        // WAL + NORMAL: still crash-safe for the process, but one fsync per
        // transaction instead of one per statement. Without this the relay is
        // disk-bound at ~30 ms per DNS query.
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")
            .map_err(|e| e.to_string())?;
        let db = Db {
            conn: Mutex::new(conn),
        };
        db.migrate()?;
        Ok(db)
    }

    pub fn open_in_memory() -> Result<Db, String> {
        let conn = Connection::open_in_memory().map_err(|e| e.to_string())?;
        let db = Db {
            conn: Mutex::new(conn),
        };
        db.migrate()?;
        Ok(db)
    }

    /// Apply every migration the database has not seen yet.
    ///
    /// Each file is applied once and recorded in `schema_version`. A database
    /// created before this table existed reports version 0; migration 0001 is
    /// all `CREATE ... IF NOT EXISTS`, so re-running it is a no-op.
    fn migrate(&self) -> Result<(), String> {
        const MIGRATIONS: &[(i64, &str)] = &[
            (1, include_str!("../migrations/0001_init.sql")),
            (2, include_str!("../migrations/0002_routing.sql")),
            (3, include_str!("../migrations/0003_identity.sql")),
            (4, include_str!("../migrations/0004_files.sql")),
        ];
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY)",
        )
        .map_err(|e| e.to_string())?;
        let current: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_version",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        for (version, sql) in MIGRATIONS {
            if *version <= current {
                continue;
            }
            conn.execute_batch(sql)
                .map_err(|e| format!("migration {version} failed: {e}"))?;
            conn.execute(
                "INSERT OR IGNORE INTO schema_version (version) VALUES (?1)",
                params![version],
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Highest applied migration (0 for a fresh in-memory database).
    /// Highest migration version this build knows about.
    pub fn latest_version() -> i64 {
        const MIGRATIONS: &[(i64, &str)] = &[
            (1, include_str!("../migrations/0001_init.sql")),
            (2, include_str!("../migrations/0002_routing.sql")),
            (3, include_str!("../migrations/0003_identity.sql")),
            (4, include_str!("../migrations/0004_files.sql")),
        ];
        MIGRATIONS.last().map(|(v, _)| *v).unwrap_or(0)
    }

    pub fn schema_version(&self) -> i64 {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0)
    }

    // ---- messages -------------------------------------------------------

    pub fn insert_message(
        &self,
        msg_id: &str,
        direction: &str,
        peer_id: &str,
        peer_name: &str,
        body: &str,
        status: &str,
    ) -> Result<Option<i64>, String> {
        let conn = self.conn.lock().unwrap();
        let changed = conn
            .execute(
                "INSERT OR IGNORE INTO messages
                 (msg_id, direction, peer_id, peer_name, body, status, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    msg_id,
                    direction,
                    peer_id,
                    peer_name,
                    body,
                    status,
                    now_secs()
                ],
            )
            .map_err(|e| e.to_string())?;
        if changed == 0 {
            Ok(None)
        } else {
            Ok(Some(conn.last_insert_rowid()))
        }
    }

    pub fn set_status(&self, msg_id: &str, status: &str) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "UPDATE messages SET status = ?2 WHERE msg_id = ?1",
            params![msg_id, status],
        );
    }

    /// Current status of one message (`None` if we never stored it).
    pub fn status_of(&self, msg_id: &str) -> Option<String> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT status FROM messages WHERE msg_id = ?1 LIMIT 1",
            params![msg_id],
            |r| r.get(0),
        )
        .ok()
    }

    /// Insert a chat row that announces a file transfer.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_file_message(
        &self,
        msg_id: &str,
        direction: &str,
        peer_id: &str,
        peer_name: &str,
        body: &str,
        status: &str,
        file_id: &str,
    ) -> Result<Option<i64>, String> {
        let conn = self.conn.lock().unwrap();
        let changed = conn
            .execute(
                "INSERT OR IGNORE INTO messages
                 (msg_id, direction, peer_id, peer_name, body, status, created_at, file_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    msg_id,
                    direction,
                    peer_id,
                    peer_name,
                    body,
                    status,
                    now_secs(),
                    file_id
                ],
            )
            .map_err(|e| e.to_string())?;
        if changed == 0 {
            Ok(None)
        } else {
            Ok(Some(conn.last_insert_rowid()))
        }
    }

    /// Record a transfer (idempotent: every part carries the same header).
    #[allow(clippy::too_many_arguments)]
    pub fn file_add(
        &self,
        transfer_id: &str,
        direction: &str,
        peer_id: &str,
        name: &str,
        size: u64,
        sha256: &str,
        parts: u32,
        part_bytes: usize,
        path: &str,
    ) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO files
             (transfer_id, direction, peer_id, name, size, sha256, parts, part_bytes,
              path, complete, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, ?10)",
            params![
                transfer_id,
                direction,
                peer_id,
                name,
                size as i64,
                sha256,
                parts as i64,
                part_bytes as i64,
                path,
                now_secs()
            ],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn file_get(&self, transfer_id: &str) -> Option<FileRow> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT transfer_id, direction, peer_id, name, size, sha256, parts, part_bytes,
                    path, complete, created_at
             FROM files WHERE transfer_id = ?1",
            params![transfer_id],
            |r| {
                Ok(FileRow {
                    transfer_id: r.get(0)?,
                    direction: r.get(1)?,
                    peer_id: r.get(2)?,
                    name: r.get(3)?,
                    size: r.get::<_, i64>(4)? as u64,
                    sha256: r.get(5)?,
                    parts: r.get::<_, i64>(6)? as u32,
                    part_bytes: r.get::<_, i64>(7)? as usize,
                    path: r.get(8)?,
                    complete: r.get::<_, i64>(9)? != 0,
                    created_at: r.get(10)?,
                })
            },
        )
        .ok()
    }

    /// Park one part. Returns `true` when it was not already present.
    pub fn file_part_add(&self, transfer_id: &str, part: u32, data: &[u8]) -> Result<bool, String> {
        let conn = self.conn.lock().unwrap();
        let changed = conn
            .execute(
                "INSERT OR IGNORE INTO file_parts (transfer_id, part, data, received_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![transfer_id, part as i64, data, now_secs()],
            )
            .map_err(|e| e.to_string())?;
        Ok(changed > 0)
    }

    /// All parts in order, but only when every one of them has arrived.
    pub fn file_parts_complete(&self, transfer_id: &str, expect: u32) -> Option<Vec<u8>> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM file_parts WHERE transfer_id = ?1",
                params![transfer_id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if count as u32 != expect {
            return None;
        }
        let mut stmt = conn
            .prepare("SELECT data FROM file_parts WHERE transfer_id = ?1 ORDER BY part ASC")
            .ok()?;
        let rows = stmt
            .query_map(params![transfer_id], |r| r.get::<_, Vec<u8>>(0))
            .ok()?;
        let mut out = Vec::new();
        for row in rows.flatten() {
            out.extend_from_slice(&row);
        }
        Some(out)
    }

    pub fn file_mark_complete(&self, transfer_id: &str, path: &str) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE files SET complete = 1, path = ?2 WHERE transfer_id = ?1",
            params![transfer_id, path],
        )
        .map_err(|e| e.to_string())?;
        // The assembled file lives on disk now; the parked parts are dead weight.
        conn.execute(
            "DELETE FROM file_parts WHERE transfer_id = ?1",
            params![transfer_id],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Parts already pushed for this transfer (used to resume a send).
    pub fn file_sent_mark(&self, transfer_id: &str, part: u32) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO file_sent_parts (transfer_id, part) VALUES (?1, ?2)",
            params![transfer_id, part as i64],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn file_sent_parts(&self, transfer_id: &str) -> Vec<u32> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn
            .prepare("SELECT part FROM file_sent_parts WHERE transfer_id = ?1 ORDER BY part ASC")
        {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map(params![transfer_id], |r| r.get::<_, i64>(0));
        match rows {
            Ok(iter) => iter.filter_map(|r| r.ok()).map(|v| v as u32).collect(),
            Err(_) => Vec::new(),
        }
    }

    pub fn recent_files(&self, limit: i64) -> Vec<FileRow> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare(
            "SELECT transfer_id, direction, peer_id, name, size, sha256, parts, part_bytes,
                    path, complete, created_at
             FROM files ORDER BY created_at DESC LIMIT ?1",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map(params![limit], |r| {
            Ok(FileRow {
                transfer_id: r.get(0)?,
                direction: r.get(1)?,
                peer_id: r.get(2)?,
                name: r.get(3)?,
                size: r.get::<_, i64>(4)? as u64,
                sha256: r.get(5)?,
                parts: r.get::<_, i64>(6)? as u32,
                part_bytes: r.get::<_, i64>(7)? as usize,
                path: r.get(8)?,
                complete: r.get::<_, i64>(9)? != 0,
                created_at: r.get(10)?,
            })
        });
        match rows {
            Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    pub fn messages_for(&self, peer_id: &str, limit: i64) -> Vec<MessageRow> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare(
            "SELECT id, msg_id, direction, peer_id, peer_name, body, status, created_at, file_id
             FROM messages WHERE peer_id = ?1
             ORDER BY created_at ASC, id ASC LIMIT ?2",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map(params![peer_id, limit], |r| {
            Ok(MessageRow {
                id: r.get(0)?,
                msg_id: r.get(1)?,
                direction: r.get(2)?,
                peer_id: r.get(3)?,
                peer_name: r.get(4)?,
                body: r.get(5)?,
                status: r.get(6)?,
                created_at: r.get(7)?,
                file_id: r.get(8)?,
            })
        });
        match rows {
            Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Most recent messages across every peer, newest first.
    pub fn recent_messages(&self, limit: i64) -> Vec<MessageRow> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare(
            "SELECT id, msg_id, direction, peer_id, peer_name, body, status, created_at, file_id
             FROM messages ORDER BY id DESC LIMIT ?1",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map(params![limit], |r| {
            Ok(MessageRow {
                id: r.get(0)?,
                msg_id: r.get(1)?,
                direction: r.get(2)?,
                peer_id: r.get(3)?,
                peer_name: r.get(4)?,
                body: r.get(5)?,
                status: r.get(6)?,
                created_at: r.get(7)?,
                file_id: r.get(8)?,
            })
        });
        match rows {
            Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    pub fn conversations(&self) -> Vec<Conversation> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare(
            "SELECT m.peer_id, m.peer_name, m.body, m.created_at
             FROM messages m
             JOIN (SELECT peer_id, MAX(id) AS mid FROM messages GROUP BY peer_id) x
               ON m.id = x.mid
             ORDER BY m.created_at DESC",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map([], |r| {
            Ok(Conversation {
                peer_id: r.get(0)?,
                peer_name: r.get(1)?,
                last_body: r.get(2)?,
                last_ts: r.get(3)?,
            })
        });
        match rows {
            Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    // ---- reassembly chunks (scoped by routing tag) ----------------------

    /// Buffer one chunk. `tag` scopes the message to its recipient so two
    /// senders that pick the same random message id cannot interleave.
    pub fn add_chunk(
        &self,
        tag: &str,
        msg_id: &str,
        seq: u32,
        total: u32,
        data: &[u8],
    ) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO message_chunks (tag, msg_id, seq, total, data, received_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![tag, msg_id, seq, total, data, now_secs()],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Returns `Some(ordered_chunk_data)` once every chunk has arrived.
    pub fn try_assemble(&self, tag: &str, msg_id: &str) -> Option<Vec<Vec<u8>>> {
        let conn = self.conn.lock().unwrap();
        let (count, total): (i64, Option<i64>) = conn
            .query_row(
                "SELECT COUNT(*), MAX(total) FROM message_chunks
                 WHERE tag = ?1 AND msg_id = ?2",
                params![tag, msg_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok()?;
        let total = total?;
        if total <= 0 || count != total {
            return None;
        }
        let mut stmt = conn
            .prepare(
                "SELECT data FROM message_chunks
                 WHERE tag = ?1 AND msg_id = ?2 ORDER BY seq ASC",
            )
            .ok()?;
        let rows = stmt
            .query_map(params![tag, msg_id], |r| r.get::<_, Vec<u8>>(0))
            .ok()?;
        Some(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn delete_chunks(&self, tag: &str, msg_id: &str) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "DELETE FROM message_chunks WHERE tag = ?1 AND msg_id = ?2",
            params![tag, msg_id],
        );
    }

    pub fn gc_chunks(&self, older_than_secs: i64) {
        let conn = self.conn.lock().unwrap();
        let cutoff = now_secs() - older_than_secs;
        let _ = conn.execute(
            "DELETE FROM message_chunks WHERE received_at < ?1",
            params![cutoff],
        );
    }

    // ---- relay mailbox (keyed by routing tag) ---------------------------

    pub fn mailbox_add(&self, route: &str, msg_id: &str, blob: &str) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO mailbox_routes (route, msg_id, blob, delivered, created_at)
             VALUES (?1, ?2, ?3, 0, ?4)",
            params![route, msg_id, blob, now_secs()],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Look at up to `limit` undelivered blobs for `route` without consuming them.
    ///
    /// Delivery must be confirmed separately with [`Db::mailbox_mark_delivered`]
    /// so that a reply which never reaches the client (e.g. truncated over UDP)
    /// does not silently drop the queued messages.
    pub fn mailbox_peek(&self, route: &str, limit: i64) -> Vec<(i64, String, String)> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare(
            "SELECT id, msg_id, blob FROM mailbox_routes
             WHERE route = ?1 AND delivered = 0 ORDER BY id ASC LIMIT ?2",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map(params![route, limit], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        });
        match rows {
            Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Mark specific mailbox rows as delivered (after a reply is actually sent).
    pub fn mailbox_mark_delivered(&self, ids: &[i64]) {
        if ids.is_empty() {
            return;
        }
        let conn = self.conn.lock().unwrap();
        for id in ids {
            let _ = conn.execute(
                "UPDATE mailbox_routes SET delivered = 1 WHERE id = ?1",
                params![id],
            );
        }
    }

    /// Drop delivered rows and anything older than `max_age_secs`.
    ///
    /// Without this the relay mailbox grows without bound: entries are only ever
    /// marked delivered, never removed.
    pub fn mailbox_gc(&self, max_age_secs: i64) -> usize {
        let conn = self.conn.lock().unwrap();
        let cutoff = now_secs() - max_age_secs.max(0);
        conn.execute(
            "DELETE FROM mailbox_routes WHERE delivered = 1 OR created_at < ?1",
            params![cutoff],
        )
        .unwrap_or(0)
    }

    /// Mark mailbox rows delivered by `(route, msg_id)` — the client's ack.
    pub fn mailbox_ack(&self, route: &str, ids: &[String]) -> usize {
        if ids.is_empty() {
            return 0;
        }
        let conn = self.conn.lock().unwrap();
        let mut n = 0;
        for id in ids {
            n += conn
                .execute(
                    "UPDATE mailbox_routes SET delivered = 1 WHERE route = ?1 AND msg_id = ?2",
                    params![route, id],
                )
                .unwrap_or(0);
        }
        n
    }

    /// Buffer a chunk and, once complete, park the joined blob — all in one
    /// transaction, so a relayed chunk costs one fsync instead of four.
    pub fn relay_store(&self, chunk: RelayChunk<'_>, quota: Quota) -> Result<RelayStore, String> {
        let RelayChunk {
            tag,
            msg_id,
            seq,
            total,
            data,
        } = chunk;
        let Quota {
            max_blobs,
            max_bytes,
        } = quota;
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT OR IGNORE INTO message_chunks (tag, msg_id, seq, total, data, received_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![tag, msg_id, seq, total, data, now_secs()],
        )
        .map_err(|e| e.to_string())?;

        let (count, max_total): (i64, Option<i64>) = tx
            .query_row(
                "SELECT COUNT(*), MAX(total) FROM message_chunks
                 WHERE tag = ?1 AND msg_id = ?2",
                params![tag, msg_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(|e| e.to_string())?;
        let Some(max_total) = max_total else {
            tx.commit().map_err(|e| e.to_string())?;
            return Ok(RelayStore::Buffered);
        };
        if max_total <= 0 || count != max_total {
            tx.commit().map_err(|e| e.to_string())?;
            return Ok(RelayStore::Buffered);
        }

        let mut stmt = tx
            .prepare(
                "SELECT data FROM message_chunks
                 WHERE tag = ?1 AND msg_id = ?2 ORDER BY seq ASC",
            )
            .map_err(|e| e.to_string())?;
        let parts: Vec<Vec<u8>> = stmt
            .query_map(params![tag, msg_id], |r| r.get::<_, Vec<u8>>(0))
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);
        let blob: String = parts.iter().map(|p| String::from_utf8_lossy(p)).collect();

        let (blobs, bytes): (i64, i64) = tx
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(LENGTH(blob)), 0) FROM mailbox_routes
                 WHERE route = ?1 AND delivered = 0",
                params![tag],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(|e| e.to_string())?;
        if blobs >= max_blobs || bytes + blob.len() as i64 > max_bytes {
            tx.execute(
                "DELETE FROM message_chunks WHERE tag = ?1 AND msg_id = ?2",
                params![tag, msg_id],
            )
            .map_err(|e| e.to_string())?;
            tx.commit().map_err(|e| e.to_string())?;
            return Ok(RelayStore::Quota { blobs, bytes });
        }

        tx.execute(
            "INSERT OR IGNORE INTO mailbox_routes (route, msg_id, blob, delivered, created_at)
             VALUES (?1, ?2, ?3, 0, ?4)",
            params![tag, msg_id, blob, now_secs()],
        )
        .map_err(|e| e.to_string())?;
        tx.execute(
            "DELETE FROM message_chunks WHERE tag = ?1 AND msg_id = ?2",
            params![tag, msg_id],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(RelayStore::Stored {
            chunks: parts.len(),
            blob_chars: blob.len(),
        })
    }

    /// Chunks buffered in the relay for a tag but not yet joined.
    pub fn buffered_chunks(&self, tag: &str) -> i64 {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM message_chunks WHERE tag = ?1",
            params![tag],
            |r| r.get(0),
        )
        .unwrap_or(0)
    }

    /// `(blobs, bytes)` currently queued for a route, for quota checks.
    pub fn mailbox_usage(&self, route: &str) -> (i64, i64) {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(LENGTH(blob)), 0) FROM mailbox_routes
             WHERE route = ?1 AND delivered = 0",
            params![route],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap_or((0, 0))
    }

    /// Number of blobs still waiting for a routing tag.
    pub fn mailbox_pending(&self, route: &str) -> i64 {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM mailbox_routes WHERE route = ?1 AND delivered = 0",
            params![route],
            |r| r.get(0),
        )
        .unwrap_or(0)
    }

    /// Cheap counters for the UI status bar.
    ///
    /// `own_route` is this node's routing tag; pass `""` when it has no key.
    pub fn stats(&self, own_route: &str) -> Stats {
        let conn = self.conn.lock().unwrap();
        let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap_or(0) };
        let with_arg = |sql: &str, arg: &str| -> i64 {
            conn.query_row(sql, params![arg], |r| r.get(0)).unwrap_or(0)
        };
        Stats {
            sent: count("SELECT COUNT(*) FROM messages WHERE direction = 'out'"),
            received: count("SELECT COUNT(*) FROM messages WHERE direction = 'in'"),
            delivered: count(
                "SELECT COUNT(*) FROM messages WHERE direction = 'out' AND status = 'delivered'",
            ),
            failed: count(
                "SELECT COUNT(*) FROM messages WHERE direction = 'out' AND status = 'failed'",
            ),
            pending_chunks: count(
                "SELECT COUNT(*) FROM (SELECT DISTINCT tag, msg_id FROM message_chunks)",
            ),
            mailbox: with_arg(
                "SELECT COUNT(*) FROM mailbox_routes WHERE route = ?1 AND delivered = 0",
                own_route,
            ),
            outbox: count("SELECT COUNT(*) FROM outbox"),
        }
    }

    // ---- identity -------------------------------------------------------

    /// Stored Ed25519 seed and X25519 secret, if this node has minted them.
    pub fn identity_secrets(&self) -> Result<Option<StoredSecrets>, String> {
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT ed_secret, x_secret FROM node_identity WHERE id = 1",
                [],
                |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?)),
            )
            .ok();
        let Some((ed, x)) = row else {
            return Ok(None);
        };
        let ed: [u8; 32] = ed
            .try_into()
            .map_err(|_| "stored ed25519 secret has the wrong length".to_string())?;
        let x: [u8; 32] = x
            .try_into()
            .map_err(|_| "stored x25519 secret has the wrong length".to_string())?;
        Ok(Some((ed, x)))
    }

    pub fn store_identity(&self, ed: &[u8; 32], x: &[u8; 32]) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO node_identity (id, ed_secret, x_secret, created_at)
             VALUES (1, ?1, ?2, ?3)
             ON CONFLICT(id) DO NOTHING",
            params![ed.as_slice(), x.as_slice(), now_secs()],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    // ---- pinned peer keys (trust on first use) --------------------------

    /// Both public keys pinned for `id`, if any.
    pub fn peer_keys(&self, id: &str) -> Option<PinnedKeys> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT ed_public, x_public FROM peers WHERE id = ?1",
            params![id],
            |r| {
                Ok((
                    r.get::<_, Option<Vec<u8>>>(0)?,
                    r.get::<_, Option<Vec<u8>>>(1)?,
                ))
            },
        )
        .ok()
    }

    /// Pin a peer's keys on first sight.
    ///
    /// Never overwrites an existing pin: a changed key is reported as
    /// [`PinOutcome::Changed`] and the caller decides (today: refuse the
    /// message and log loudly).
    pub fn pin_peer_keys(
        &self,
        id: &str,
        ed: &[u8; 32],
        x: &[u8; 32],
    ) -> Result<PinOutcome, String> {
        let conn = self.conn.lock().unwrap();
        let existing: Option<PinnedKeys> = conn
            .query_row(
                "SELECT ed_public, x_public FROM peers WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        match existing {
            None => {
                conn.execute(
                    "INSERT INTO peers (id, name, addr, via, last_seen, ed_public, x_public, pinned_at)
                     VALUES (?1, ?1, '', 'relay', ?4, ?2, ?3, ?4)
                     ON CONFLICT(id) DO UPDATE SET
                        ed_public = excluded.ed_public,
                        x_public  = excluded.x_public,
                        pinned_at = excluded.pinned_at",
                    params![id, ed.as_slice(), x.as_slice(), now_secs()],
                )
                .map_err(|e| e.to_string())?;
                Ok(PinOutcome::Pinned)
            }
            Some((Some(stored_ed), same_x)) => {
                if stored_ed == ed && same_x.as_deref() == Some(x.as_slice()) {
                    Ok(PinOutcome::Same)
                } else {
                    Ok(PinOutcome::Changed)
                }
            }
            Some((None, _)) => {
                // Row exists (a contact was added before first contact) but no
                // keys were pinned yet.
                conn.execute(
                    "UPDATE peers SET ed_public = ?2, x_public = ?3, pinned_at = ?4 WHERE id = ?1",
                    params![id, ed.as_slice(), x.as_slice(), now_secs()],
                )
                .map_err(|e| e.to_string())?;
                Ok(PinOutcome::Pinned)
            }
        }
    }

    // ---- outbox (retry queue) -------------------------------------------

    pub fn outbox_add(
        &self,
        msg_id: &str,
        to: &str,
        body: &str,
        conv_seq: u64,
    ) -> Result<(), String> {
        let now = now_secs();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO outbox
                (msg_id, to_id, body, conv_seq, attempts, next_try, created_at)
             VALUES (?1, ?2, ?3, ?4, 0, ?5, ?5)",
            params![msg_id, to, body, conv_seq as i64, now],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Entries whose backoff has elapsed, oldest first.
    pub fn outbox_due(&self, limit: i64) -> Vec<(String, String, String, i64, u64)> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare(
            "SELECT msg_id, to_id, body, attempts, conv_seq FROM outbox
             WHERE next_try <= ?1 ORDER BY created_at ASC LIMIT ?2",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map(params![now_secs(), limit], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get::<_, i64>(4)? as u64,
            ))
        });
        match rows {
            Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    pub fn outbox_bump(&self, msg_id: &str, next_try: i64) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "UPDATE outbox SET attempts = attempts + 1, next_try = ?2 WHERE msg_id = ?1",
            params![msg_id, next_try],
        );
    }

    pub fn outbox_remove(&self, msg_id: &str) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute("DELETE FROM outbox WHERE msg_id = ?1", params![msg_id]);
    }

    pub fn outbox_len(&self) -> i64 {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT COUNT(*) FROM outbox", [], |r| r.get(0))
            .unwrap_or(0)
    }

    // ---- replay cache ---------------------------------------------------

    /// Record `msg_id`; returns `false` when it was already seen.
    pub fn seen_mark(&self, msg_id: &str) -> bool {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO seen_msgs (msg_id, seen_at) VALUES (?1, ?2)",
            params![msg_id, now_secs()],
        )
        .map(|n| n == 1)
        .unwrap_or(true)
    }

    pub fn seen_gc(&self, older_than_secs: i64) {
        let conn = self.conn.lock().unwrap();
        let cutoff = now_secs() - older_than_secs;
        let _ = conn.execute("DELETE FROM seen_msgs WHERE seen_at < ?1", params![cutoff]);
    }

    // ---- conversation sequence numbers ----------------------------------

    /// Next outgoing sequence number for a peer (1-based).
    pub fn conv_next_out(&self, peer: &str) -> i64 {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT OR IGNORE INTO conv_state (peer_id, out_seq, in_seq) VALUES (?1, 0, 0)",
            params![peer],
        );
        let _ = conn.execute(
            "UPDATE conv_state SET out_seq = out_seq + 1 WHERE peer_id = ?1",
            params![peer],
        );
        conn.query_row(
            "SELECT out_seq FROM conv_state WHERE peer_id = ?1",
            params![peer],
            |r| r.get(0),
        )
        .unwrap_or(1)
    }

    /// True when the peer has told us it holds our X25519 key.
    pub fn peer_knows_us(&self, peer: &str) -> bool {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT peer_knows_us FROM conv_state WHERE peer_id = ?1",
            params![peer],
            |r| r.get::<_, i64>(0),
        )
        .map(|v| v != 0)
        .unwrap_or(false)
    }

    pub fn set_peer_knows_us(&self, peer: &str, knows: bool) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO conv_state (peer_id, out_seq, in_seq, peer_knows_us)
             VALUES (?1, 0, 0, ?2)
             ON CONFLICT(peer_id) DO UPDATE SET peer_knows_us = excluded.peer_knows_us",
            params![peer, i64::from(knows)],
        );
    }

    /// Every pinned X25519 key, for trying candidate payload keys.
    pub fn peers_with_x_keys(&self) -> Vec<Vec<u8>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare("SELECT x_public FROM peers WHERE x_public IS NOT NULL") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map([], |r| r.get::<_, Vec<u8>>(0));
        match rows {
            Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Observe an inbound sequence; returns the gap (first missing) if any.
    pub fn conv_observe_in(&self, peer: &str, seq: i64) -> Option<i64> {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT OR IGNORE INTO conv_state (peer_id, out_seq, in_seq) VALUES (?1, 0, 0)",
            params![peer],
        );
        let highest: i64 = conn
            .query_row(
                "SELECT in_seq FROM conv_state WHERE peer_id = ?1",
                params![peer],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if seq > highest {
            let _ = conn.execute(
                "UPDATE conv_state SET in_seq = ?2 WHERE peer_id = ?1",
                params![peer, seq],
            );
        }
        if seq > highest + 1 {
            Some(highest + 1)
        } else {
            None
        }
    }

    // ---- peers ----------------------------------------------------------

    pub fn upsert_peer(&self, id: &str, name: &str, addr: &str, via: &str) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO peers (id, name, addr, via, last_seen) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                addr = CASE WHEN excluded.addr <> '' THEN excluded.addr ELSE peers.addr END,
                via  = excluded.via,
                last_seen = excluded.last_seen",
            params![id, name, addr, via, now_secs()],
        );
    }

    pub fn list_peers(&self) -> Vec<Peer> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare("SELECT id, name, addr, via FROM peers ORDER BY name") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map([], |r| {
            Ok(Peer {
                id: r.get(0)?,
                name: r.get(1)?,
                addr: r.get(2)?,
                via: r.get(3)?,
            })
        });
        match rows {
            Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    pub fn peer(&self, id: &str) -> Option<Peer> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, name, addr, via FROM peers WHERE id = ?1",
            params![id],
            |r| {
                Ok(Peer {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    addr: r.get(2)?,
                    via: r.get(3)?,
                })
            },
        )
        .ok()
    }

    pub fn touch_peer(&self, id: &str) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "UPDATE peers SET last_seen = ?2 WHERE id = ?1",
            params![id, now_secs()],
        );
    }

    // ---- settings -------------------------------------------------------

    pub fn set_setting(&self, key: &str, value: &str) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        );
    }

    pub fn get_setting(&self, key: &str) -> Option<String> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT value FROM settings WHERE key = ?1",
            params![key],
            |r| r.get(0),
        )
        .ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_dedupes_and_scopes_by_peer() {
        let db = Db::open_in_memory().unwrap();
        assert!(
            db.insert_message("m1", "in", "p1", "peer", "hi", "received")
                .unwrap()
                .is_some()
        );
        // A retransmitted message must not appear twice.
        assert!(
            db.insert_message("m1", "in", "p1", "peer", "hi", "received")
                .unwrap()
                .is_none()
        );
        assert_eq!(db.messages_for("p1", 10).len(), 1);
        // Conversations are scoped: no leakage between peers.
        assert!(db.messages_for("p2", 10).is_empty());
    }

    #[test]
    fn chunks_assemble_only_when_complete() {
        let db = Db::open_in_memory().unwrap();
        db.add_chunk("tg", "m1", 0, 2, b"JBSW").unwrap();
        assert!(
            db.try_assemble("tg", "m1").is_none(),
            "incomplete set must wait"
        );
        db.add_chunk("tg", "m1", 1, 2, b"Y3DP").unwrap();
        let parts = db.try_assemble("tg", "m1").expect("all chunks present");
        assert_eq!(parts, vec![b"JBSW".to_vec(), b"Y3DP".to_vec()]);
    }

    #[test]
    fn chunks_from_different_tags_do_not_mix() {
        // Two senders may pick the same random message id; the routing tag keeps
        // their chunk sets apart.
        let db = Db::open_in_memory().unwrap();
        db.add_chunk("tag-a", "same-id", 0, 2, b"AAAA").unwrap();
        db.add_chunk("tag-b", "same-id", 0, 2, b"BBBB").unwrap();
        assert!(db.try_assemble("tag-a", "same-id").is_none());
        db.add_chunk("tag-a", "same-id", 1, 2, b"aaaa").unwrap();
        let parts = db.try_assemble("tag-a", "same-id").unwrap();
        assert_eq!(parts, vec![b"AAAA".to_vec(), b"aaaa".to_vec()]);
        assert!(
            db.try_assemble("tag-b", "same-id").is_none(),
            "tag-b must not inherit tag-a's chunks"
        );
        // Deleting is scoped too.
        db.delete_chunks("tag-a", "same-id");
        assert!(db.try_assemble("tag-a", "same-id").is_none());
        assert_eq!(db.mailbox_pending("tag-b"), 0);
    }

    #[test]
    fn migrations_are_recorded_and_idempotent() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.schema_version(), Db::latest_version());
        // Re-opening the same file must not re-run migrations.
        let path = std::env::temp_dir().join(format!("dns_lab_mig_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let p = path.to_string_lossy().to_string();
        {
            let first = Db::open(&p).unwrap();
            assert_eq!(first.schema_version(), Db::latest_version());
        }
        {
            let second = Db::open(&p).unwrap();
            assert_eq!(second.schema_version(), Db::latest_version());
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn upgrading_a_pre_migration_database_keeps_history() {
        // Simulate a 0.3 database: schema 0001 applied, no schema_version table.
        let path = std::env::temp_dir().join(format!("dns_lab_up_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let p = path.to_string_lossy().to_string();
        {
            let conn = Connection::open(&p).unwrap();
            conn.execute_batch(include_str!("../migrations/0001_init.sql"))
                .unwrap();
            conn.execute(
                "INSERT INTO messages
                 (msg_id, direction, peer_id, peer_name, body, status, created_at)
                 VALUES ('old1', 'in', 'p1', 'peer', 'kept across the upgrade', 'received', 1)",
                [],
            )
            .unwrap();
        }

        let db = Db::open(&p).unwrap();
        assert_eq!(db.schema_version(), Db::latest_version());
        assert_eq!(
            db.messages_for("p1", 10)[0].body,
            "kept across the upgrade",
            "message history must survive the migration"
        );
        let _ = std::fs::remove_file(&path);
    }
    fn chunk<'a>(
        tag: &'a str,
        mid: &'a str,
        seq: u32,
        total: u32,
        data: &'a [u8],
    ) -> RelayChunk<'a> {
        RelayChunk {
            tag,
            msg_id: mid,
            seq,
            total,
            data,
        }
    }

    fn roomy() -> Quota {
        Quota {
            max_blobs: 10,
            max_bytes: 1 << 20,
        }
    }

    #[test]
    fn relay_store_joins_chunks_in_order_and_frees_them() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(
            db.relay_store(chunk("ab12cd34", "m1", 1, 2, b"world"), roomy())
                .unwrap(),
            RelayStore::Buffered
        );
        assert_eq!(
            db.relay_store(chunk("ab12cd34", "m1", 0, 2, b"hello"), roomy())
                .unwrap(),
            RelayStore::Stored {
                chunks: 2,
                blob_chars: 10
            }
        );
        // Chunks are gone, the blob is queued exactly once.
        let rows = db.mailbox_peek("ab12cd34", 8);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].2, "helloworld");
        assert_eq!(db.buffered_chunks("ab12cd34"), 0);
    }

    #[test]
    fn relay_store_refuses_to_exceed_the_quota() {
        let db = Db::open_in_memory().unwrap();
        let tiny = Quota {
            max_blobs: 1,
            max_bytes: 4,
        };
        assert!(matches!(
            db.relay_store(chunk("ab12cd34", "m1", 0, 1, b"1234"), tiny)
                .unwrap(),
            RelayStore::Stored { .. }
        ));
        // Second blob would be over both limits, and is dropped rather than queued.
        assert_eq!(
            db.relay_store(chunk("ab12cd34", "m2", 0, 1, b"5"), tiny)
                .unwrap(),
            RelayStore::Quota { blobs: 1, bytes: 4 }
        );
        assert_eq!(db.mailbox_peek("ab12cd34", 8).len(), 1);
        // The dropped message leaves no chunks behind either.
        assert_eq!(db.buffered_chunks("ab12cd34"), 0);
        // A different tag has its own budget.
        assert!(matches!(
            db.relay_store(chunk("ff00ff00", "m3", 0, 1, b"1"), tiny)
                .unwrap(),
            RelayStore::Stored { .. }
        ));
    }

    #[test]
    fn mailbox_peek_does_not_consume() {
        let db = Db::open_in_memory().unwrap();
        db.mailbox_add("bb", "m1", "BLOB").unwrap();
        let first = db.mailbox_peek("bb", 8);
        assert_eq!(first.len(), 1);
        // Peeking twice must return the same row: delivery is confirmed later.
        assert_eq!(db.mailbox_peek("bb", 8).len(), 1);
        assert_eq!(db.mailbox_pending("bb"), 1);
        db.mailbox_mark_delivered(&[first[0].0]);
        assert!(db.mailbox_peek("bb", 8).is_empty());
        assert_eq!(db.mailbox_pending("bb"), 0);
    }

    #[test]
    fn mailbox_gc_drops_delivered_rows_but_keeps_pending() {
        let db = Db::open_in_memory().unwrap();
        db.mailbox_add("bb", "m1", "ONE").unwrap();
        db.mailbox_add("bb", "m2", "TWO").unwrap();
        let rows = db.mailbox_peek("bb", 8);
        db.mailbox_mark_delivered(&[rows[0].0]);

        assert_eq!(db.mailbox_gc(7 * 86_400), 1, "only the delivered row goes");
        let left = db.mailbox_peek("bb", 8);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].1, "m2");
    }

    #[test]
    fn stats_count_statuses() {
        let db = Db::open_in_memory().unwrap();
        db.insert_message("a", "out", "p1", "peer", "hi", "sent")
            .unwrap();
        db.insert_message("b", "out", "p1", "peer", "yo", "delivered")
            .unwrap();
        db.insert_message("c", "in", "p1", "peer", "hey", "received")
            .unwrap();
        let s = db.stats("me");
        assert_eq!((s.sent, s.received, s.delivered, s.failed), (2, 1, 1, 0));
    }

    #[test]
    fn peers_upsert_keeps_address_when_blank() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_peer("p1", "bob", "1.2.3.4:53", "direct");
        db.upsert_peer("p1", "bob", "", "direct");
        assert_eq!(db.peer("p1").unwrap().addr, "1.2.3.4:53");
        assert_eq!(db.list_peers().len(), 1);
    }
}
