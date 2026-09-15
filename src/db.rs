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
}

#[derive(Debug, Clone)]
pub struct Conversation {
    pub peer_id: String,
    pub peer_name: String,
    pub last_body: String,
    pub last_ts: i64,
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

    fn migrate(&self) -> Result<(), String> {
        let sql = include_str!("../migrations/0001_init.sql");
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(sql).map_err(|e| e.to_string())
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

    pub fn messages_for(&self, peer_id: &str, limit: i64) -> Vec<MessageRow> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare(
            "SELECT id, msg_id, direction, peer_id, peer_name, body, status, created_at
             FROM messages WHERE peer_id = ?1 OR peer_id = '' 
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

    // ---- reassembly chunks ---------------------------------------------

    pub fn add_chunk(&self, msg_id: &str, seq: u32, total: u32, data: &[u8]) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO chunks (msg_id, seq, total, data, received_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![msg_id, seq, total, data, now_secs()],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Returns `Some(ordered_chunk_data)` once every chunk has arrived.
    pub fn try_assemble(&self, msg_id: &str) -> Option<Vec<Vec<u8>>> {
        let conn = self.conn.lock().unwrap();
        let (count, total): (i64, Option<i64>) = conn
            .query_row(
                "SELECT COUNT(*), MAX(total) FROM chunks WHERE msg_id = ?1",
                params![msg_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok()?;
        let total = total?;
        if total <= 0 || count != total {
            return None;
        }
        let mut stmt = conn
            .prepare("SELECT data FROM chunks WHERE msg_id = ?1 ORDER BY seq ASC")
            .ok()?;
        let rows = stmt
            .query_map(params![msg_id], |r| r.get::<_, Vec<u8>>(0))
            .ok()?;
        Some(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn delete_chunks(&self, msg_id: &str) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute("DELETE FROM chunks WHERE msg_id = ?1", params![msg_id]);
    }

    pub fn gc_chunks(&self, older_than_secs: i64) {
        let conn = self.conn.lock().unwrap();
        let cutoff = now_secs() - older_than_secs;
        let _ = conn.execute("DELETE FROM chunks WHERE received_at < ?1", params![cutoff]);
    }

    // ---- relay mailbox --------------------------------------------------

    pub fn mailbox_add(&self, to_id: &str, msg_id: &str, blob: &str) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO mailbox (to_id, msg_id, blob, delivered, created_at)
             VALUES (?1, ?2, ?3, 0, ?4)",
            params![to_id, msg_id, blob, now_secs()],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Look at up to `limit` undelivered blobs for `to_id` without consuming them.
    ///
    /// Delivery must be confirmed separately with [`Db::mailbox_mark_delivered`]
    /// so that a reply which never reaches the client (e.g. truncated over UDP)
    /// does not silently drop the queued messages.
    pub fn mailbox_peek(&self, to_id: &str, limit: i64) -> Vec<(i64, String, String)> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare(
            "SELECT id, msg_id, blob FROM mailbox
             WHERE to_id = ?1 AND delivered = 0 ORDER BY id ASC LIMIT ?2",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map(params![to_id, limit], |r| {
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
                "UPDATE mailbox SET delivered = 1 WHERE id = ?1",
                params![id],
            );
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
