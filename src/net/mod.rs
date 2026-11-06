//! DNS transport: node orchestration, sender, listener, poller, relay.

pub mod listener;
pub mod poller;
pub mod reply;
pub mod sender;

use crate::config::{Config, EgressKind, RelayMode};
use crate::db::Db;
use crate::identity::{self, Identity};
use crate::protocol::{self, Alg, Envelope, MsgType};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// TTL for partially received messages before their chunks are dropped.
pub const CHUNK_TTL_SECS: i64 = 300;
/// TTL for relay mailbox entries (read or unread) before they are purged.
pub const MAILBOX_TTL_SECS: i64 = 7 * 86_400;
/// TTL for the replay cache.
pub const SEEN_TTL_SECS: i64 = 3600;
/// How often housekeeping runs.
pub const HOUSEKEEPING_SECS: u64 = 60;
/// How often the outbox retries undelivered messages.
pub const OUTBOX_SECS: u64 = 20;
/// Give up on an outbox entry after this many attempts.
pub const OUTBOX_MAX_ATTEMPTS: i64 = 20;

/// X25519 payload keys are derived with this fixed context: the pair identity is
/// already carried by the Diffie-Hellman itself, and a fixed context lets the
/// receiver try candidate keys without first decrypting the envelope.
const PEER_KEY_CONTEXT: &[u8] = b"dns_lab/v5/peer-payload";

/// Result of [`Node::send_file`].
#[derive(Debug, Clone)]
pub struct FileSend {
    pub transfer_id: String,
    pub parts: u32,
    pub bytes: u64,
    /// Parts that were already delivered before this call (a resumed send).
    pub skipped: u32,
}

/// Events pushed from transport threads to the UI (or a console drain).
#[derive(Debug, Clone)]
pub enum AppEvent {
    /// A message was stored; refresh the conversation.
    Message,
    /// A human-readable transport log line.
    Log(String),
}

/// Outcome of handing a decrypted envelope to the node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ingested {
    /// New inbound message, stored and receipted.
    Stored,
    /// Already had this message (retransmission).
    Duplicate,
    /// Delivery receipt for one of our outbound messages.
    Receipt,
    /// Valid but not a chat message (heartbeat / read marker).
    Ignored,
    /// Dropped: signature missing/invalid, or the key changed.
    Rejected(String),
}

/// Where outgoing DNS queries are sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Egress {
    /// Straight to a peer/relay `ip:port`.
    Direct(SocketAddr),
    /// To a recursive resolver that forwards to our authoritative zone.
    Resolver(SocketAddr),
}

impl std::fmt::Display for Egress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.label(), self.addr())
    }
}

impl Egress {
    pub fn addr(&self) -> SocketAddr {
        match self {
            Egress::Direct(a) | Egress::Resolver(a) => *a,
        }
    }
    pub fn label(&self) -> &'static str {
        match self {
            Egress::Direct(_) => "direct",
            Egress::Resolver(_) => "resolver",
        }
    }
}

/// Shared node state used by every transport thread.
pub struct Node {
    pub cfg: Config,
    pub db: Arc<Db>,
    /// Group/payload key from the shared passphrase; `None` means this node
    /// cannot encrypt or route anything (a blind relay).
    pub key: Option<[u8; 32]>,
    /// This node's own routing tag (`None` without a group key).
    pub tag: Option<String>,
    /// Ed25519 + X25519 identity, minted on first run.
    pub identity: Identity,
    pub events: Mutex<Sender<AppEvent>>,
    pub stop: Arc<AtomicBool>,
}

impl Node {
    pub fn new(cfg: Config, db: Arc<Db>, events: Sender<AppEvent>) -> Result<Arc<Node>, String> {
        let key = cfg.key_opt();
        let tag = key.map(|k| crate::routing_tag(&k, &cfg.node_id));
        let identity = Identity::load_or_create(&db)?;
        // Contacts hand-written into the config file are authoritative for
        // identity/name/route, so the GUI and CLI see them without re-adding.
        for p in &cfg.peers {
            if !p.id.trim().is_empty() {
                db.upsert_peer(&p.id, &p.name, &p.addr, &p.via);
            }
        }
        Ok(Arc::new(Node {
            cfg,
            db,
            key,
            tag,
            identity,
            events: Mutex::new(events),
            stop: Arc::new(AtomicBool::new(false)),
        }))
    }

    /// Routing tag for a peer, or an error when this node has no group key.
    pub fn tag_for(&self, recipient: &str) -> Result<String, String> {
        let key = self
            .key
            .ok_or("no `shared_key` configured: this node cannot encrypt or route mail")?;
        Ok(crate::routing_tag(&key, recipient))
    }

    /// Fingerprint of our own signing key, for logs and the UI.
    pub fn fingerprint(&self) -> String {
        identity::fingerprint(&self.identity.ed_public)
    }

    pub fn log(&self, msg: impl Into<String>) {
        let msg = msg.into();
        eprintln!("{msg}");
        if let Ok(tx) = self.events.lock() {
            let _ = tx.send(AppEvent::Log(msg));
        }
    }

    pub fn notify_message(&self) {
        if let Ok(tx) = self.events.lock() {
            let _ = tx.send(AppEvent::Message);
        }
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// True when this node may store-and-forward for other ids.
    pub fn is_relay(&self) -> bool {
        matches!(
            self.cfg.relay_mode,
            RelayMode::Peer | RelayMode::Authoritative
        )
    }

    /// Resolve the egress for a recipient id (or `None` for polls / broadcasts).
    /// Every place this node can send a query, primary first.
    ///
    /// `relays` is a fallback chain: the first entry that answers wins, and a
    /// poll asks all of them so mail parked on a secondary is not missed.
    pub fn egress_candidates(&self, to: Option<&str>) -> Vec<Egress> {
        let mut out: Vec<Egress> = Vec::new();
        if let Ok(primary) = self.egress_for(to) {
            out.push(primary);
        }
        // Resolver egress cannot be extended by relay addresses: they are
        // different transports, not fallbacks.
        if self.cfg.egress == EgressKind::Direct {
            for addr in &self.cfg.relays {
                if let Ok(sa) = addr.parse::<SocketAddr>() {
                    let e = Egress::Direct(sa);
                    if !out.contains(&e) {
                        out.push(e);
                    }
                }
            }
        }
        out
    }

    pub fn egress_for(&self, to: Option<&str>) -> Result<Egress, String> {
        match self.cfg.egress {
            EgressKind::Resolver => {
                let addr = self
                    .cfg
                    .resolver
                    .clone()
                    .ok_or("egress=resolver but no `resolver` configured")?;
                let sa: SocketAddr = addr
                    .parse()
                    .map_err(|_| format!("invalid resolver address: {addr}"))?;
                Ok(Egress::Resolver(sa))
            }
            EgressKind::Direct => {
                if let Some(to) = to
                    && let Some(peer) = self.db.peer(to)
                    && peer.via == "direct"
                    && !peer.addr.is_empty()
                    && let Ok(sa) = peer.addr.parse()
                {
                    return Ok(Egress::Direct(sa));
                }
                let addr = self
                    .cfg
                    .relay
                    .clone()
                    .or_else(|| self.cfg.direct_server.clone())
                    .ok_or(
                        "egress=direct but no peer addr, `relay` or `direct_server` configured",
                    )?;
                let sa: SocketAddr = addr
                    .parse()
                    .map_err(|_| format!("invalid direct address: {addr}"))?;
                Ok(Egress::Direct(sa))
            }
        }
    }

    // ---- sealing / opening ---------------------------------------------

    /// Pinned X25519 key of a peer, if we have seen their bundle.
    fn peer_x_key(&self, id: &str) -> Option<[u8; 32]> {
        self.db
            .peer_keys(id)
            .and_then(|(_, x)| x)
            .and_then(|v| <[u8; 32]>::try_from(v.as_slice()).ok())
    }

    /// Payload key + the flags an envelope needs for this recipient.
    fn seal_params(&self, to: &str) -> Result<([u8; 32], Alg, bool, bool), String> {
        let group = self
            .key
            .ok_or("no `shared_key` configured: nothing to encrypt with")?;
        let theirs = self.peer_x_key(to);
        let they_know_ours = self.db.peer_knows_us(to);
        match theirs {
            Some(x) if they_know_ours => {
                let shared = self.identity.agree(&x)?;
                Ok((
                    identity::kdf(&shared, PEER_KEY_CONTEXT),
                    Alg::Peer,
                    false,
                    true,
                ))
            }
            // We have theirs but they may not have ours: group key, and carry
            // our bundle so they can switch next time. `peer_has_key` tells them
            // we are ready.
            Some(_) => Ok((group, Alg::Group, true, true)),
            // Stranger: group key plus our bundle.
            None => Ok((group, Alg::Group, true, false)),
        }
    }

    /// Decrypt a blob, trying the group key and then each pinned peer's key.
    ///
    /// Push delivery hands us blobs without saying who sent them, and an
    /// `Alg::Peer` payload key needs the sender's X25519 key, so we try the
    /// candidates we have. The AEAD tag rejects the wrong keys immediately, and
    /// the contact list is small; the signature check after decryption is what
    /// actually authenticates the result.
    pub fn open_blob(&self, msg_id: &str, blob: &str) -> Result<Envelope, String> {
        let mut tried_group = false;
        let mut first_error = String::new();
        if let Some(group) = self.key {
            tried_group = true;
            match protocol::decode_blob(&group, blob, msg_id) {
                Ok(env) => return Ok(env),
                Err(e) => first_error = e,
            }
        }
        for peer in self.db.peers_with_x_keys() {
            let Ok(x) = <[u8; 32]>::try_from(peer.as_slice()) else {
                continue;
            };
            let Ok(shared) = self.identity.agree(&x) else {
                continue;
            };
            let key = identity::kdf(&shared, PEER_KEY_CONTEXT);
            if let Ok(env) = protocol::decode_blob(&key, blob, msg_id) {
                return Ok(env);
            }
        }
        Err(if tried_group {
            format!("could not decrypt {msg_id}: {first_error}")
        } else {
            "no `shared_key` and no pinned peer key can decrypt this blob".to_string()
        })
    }

    /// Encrypt, chunk and transmit a message envelope. Blocking.
    pub fn do_send(
        self: &Arc<Self>,
        to: &str,
        body: &str,
        msg_id: &str,
        conv_seq: u64,
    ) -> Result<usize, String> {
        let (payload_key, alg, include_bundle, peer_has_key) = self.seal_params(to)?;
        let mut env = Envelope::new(MsgType::Message, &self.cfg.node_id, to, body)
            .with_seq(conv_seq)
            .with_alg(alg)
            .with_peer_has_key(peer_has_key);
        if include_bundle {
            env = env.with_bundle(self.identity.bundle());
        }
        env.sign(&self.identity);
        let blob = protocol::encode_blob(&payload_key, &env, msg_id)?;

        let sender_tag = self
            .tag
            .clone()
            .ok_or("no `shared_key` configured: cannot route")?;
        let tag = self.tag_for(to)?;

        let mut last_error = String::from("no relay configured");
        for egress in self.egress_candidates(Some(to)) {
            match sender::send_blob(self, egress, &blob, msg_id, &tag, &sender_tag) {
                Ok(sent) => {
                    self.accept_pushed(&sent.pushed);
                    return Ok(sent.chunks);
                }
                Err(e) => {
                    self.log(format!("[!] {egress} refused the message: {e}"));
                    last_error = e;
                }
            }
        }
        Err(last_error)
    }

    /// Send `data` as a named file transfer.
    ///
    /// Each part is a normal signed envelope, so encryption, integrity and
    /// routing are exactly the same as for a message; only the body framing
    /// differs. Parts already recorded as sent are skipped, which makes a re-run
    /// after a failure a resume rather than a restart.
    pub fn send_file<F>(
        self: &Arc<Self>,
        to: &str,
        name: &str,
        data: &[u8],
        transfer_id: &str,
        mut progress: F,
    ) -> Result<FileSend, String>
    where
        F: FnMut(u32, u32),
    {
        if transfer_id.len() != 8 || !transfer_id.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err("transfer id must be 8 alphanumeric characters".to_string());
        }
        if data.len() as u64 > crate::file::MAX_FILE_BYTES {
            return Err(format!(
                "{} exceeds the {} transfer limit",
                crate::file::human_size(data.len() as u64),
                crate::file::human_size(crate::file::MAX_FILE_BYTES)
            ));
        }
        let part_bytes = crate::file::DEFAULT_PART_BYTES;
        let parts = crate::file::part_count(data.len() as u64, part_bytes);
        let sha256 = crate::file::digest(data);
        let name = crate::file::sanitize_name(name);

        self.db.file_add(
            transfer_id,
            "out",
            to,
            &name,
            data.len() as u64,
            &sha256,
            parts,
            part_bytes,
            "",
        )?;

        // The chat row is what a completion receipt flips to `delivered`.
        let peer_name = self
            .db
            .peer(to)
            .map(|p| p.name)
            .unwrap_or_else(|| to.to_string());
        let line = format!(
            "[file] {name} ({})",
            crate::file::human_size(data.len() as u64)
        );
        let _ = self.db.insert_file_message(
            transfer_id,
            "out",
            to,
            &peer_name,
            &line,
            "sending",
            transfer_id,
        );

        let already: std::collections::HashSet<u32> =
            self.db.file_sent_parts(transfer_id).into_iter().collect();
        let mut skipped = 0u32;
        let mut sent_chunks = 0usize;

        for (part, payload) in crate::file::split_parts(data, part_bytes) {
            if already.contains(&part) {
                skipped += 1;
                progress(part + 1, parts);
                continue;
            }
            let header = crate::file::Header {
                transfer_id: transfer_id.to_string(),
                name: name.clone(),
                total_bytes: data.len() as u64,
                sha256: sha256.clone(),
                part,
                parts,
                part_bytes: part_bytes as u32,
            };
            let body = crate::file::encode_body(&header, payload);

            // Deterministic per-part id: retrying a part cannot create a second
            // copy at the receiver, and the seen-cache dedupes it for free.
            let part_id = format!("{transfer_id}{part:04x}");
            let (payload_key, alg, include_bundle, peer_has_key) = self.seal_params(to)?;
            let mut env = Envelope::new_bytes(MsgType::File, &self.cfg.node_id, to, body)
                .with_seq(self.db.conv_next_out(to) as u64)
                .with_alg(alg)
                .with_peer_has_key(peer_has_key);
            if include_bundle {
                env = env.with_bundle(self.identity.bundle());
            }
            env.sign(&self.identity);
            let blob = protocol::encode_blob(&payload_key, &env, &part_id)?;

            let sender_tag = self
                .tag
                .clone()
                .ok_or("no `shared_key` configured: cannot route")?;
            let tag = self.tag_for(to)?;

            let mut last_error = String::from("no relay configured");
            let mut ok = false;
            for egress in self.egress_candidates(Some(to)) {
                match sender::send_blob(self, egress, &blob, &part_id, &tag, &sender_tag) {
                    Ok(out) => {
                        self.accept_pushed(&out.pushed);
                        sent_chunks += out.chunks;
                        ok = true;
                        break;
                    }
                    Err(e) => last_error = e,
                }
            }
            if !ok {
                return Err(format!(
                    "part {}/{} failed ({last_error}); re-run to resume",
                    part + 1,
                    parts
                ));
            }
            self.db.file_sent_mark(transfer_id, part)?;
            progress(part + 1, parts);
        }

        self.db.file_mark_complete(transfer_id, "")?;
        self.log(format!(
            "[>] sent {name} ({}) in {parts} part(s), {sent_chunks} queries{}",
            crate::file::human_size(data.len() as u64),
            if skipped > 0 {
                format!(", {skipped} resumed")
            } else {
                String::new()
            }
        ));
        self.notify_message();
        Ok(FileSend {
            transfer_id: transfer_id.to_string(),
            parts,
            bytes: data.len() as u64,
            skipped,
        })
    }

    /// Store one incoming file part, assembling and verifying on the last one.
    fn ingest_file_part(
        self: &Arc<Self>,
        msg_id: &str,
        env: &Envelope,
    ) -> Result<Ingested, String> {
        let (header, payload) = crate::file::decode_body(&env.body)?;
        let peer_name = self
            .db
            .peer(&env.from)
            .map(|p| p.name)
            .unwrap_or_else(|| env.from.clone());

        let dir = std::path::PathBuf::from(&self.cfg.download_dir);
        let path = dir.join(format!("{}-{}", header.transfer_id, header.name));
        self.db.file_add(
            &header.transfer_id,
            "in",
            &env.from,
            &header.name,
            header.total_bytes,
            &header.sha256,
            header.parts,
            header.part_bytes as usize,
            &path.to_string_lossy(),
        )?;
        let fresh = self
            .db
            .file_part_add(&header.transfer_id, header.part, &payload)?;

        let Some(data) = self
            .db
            .file_parts_complete(&header.transfer_id, header.parts)
        else {
            if fresh && self.cfg.verbose {
                self.log(format!(
                    "[<] file {} part {}/{} from {}",
                    header.transfer_id,
                    header.part + 1,
                    header.parts,
                    env.from
                ));
            }
            return Ok(Ingested::Stored);
        };

        // Every part is in: size and hash must match what was promised.
        if data.len() as u64 != header.total_bytes {
            self.db.file_mark_complete(&header.transfer_id, "")?;
            return Err(format!(
                "file {}: assembled {} bytes, expected {}",
                header.transfer_id,
                data.len(),
                header.total_bytes
            ));
        }
        if !crate::file::verify(&data, &header.sha256) {
            self.db.file_mark_complete(&header.transfer_id, "")?;
            return Err(format!(
                "file {}: sha256 mismatch, discarding {} bytes",
                header.transfer_id,
                data.len()
            ));
        }
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return Err(format!("cannot create {}: {e}", dir.display()));
        }
        if let Err(e) = std::fs::write(&path, &data) {
            return Err(format!("cannot write {}: {e}", path.display()));
        }
        self.db
            .file_mark_complete(&header.transfer_id, &path.to_string_lossy())?;

        let line = format!(
            "[file] {} ({})",
            header.name,
            crate::file::human_size(header.total_bytes)
        );
        let _ = self.db.insert_file_message(
            &header.transfer_id,
            "in",
            &env.from,
            &peer_name,
            &line,
            "received",
            &header.transfer_id,
        );
        self.log(format!(
            "[<] file {} from {} saved to {} ({} verified, {msg_id})",
            header.name,
            env.from,
            path.display(),
            crate::file::human_size(header.total_bytes)
        ));
        self.notify_message();
        // One receipt for the whole transfer, keyed by the transfer id.
        self.send_receipt(&env.from, &header.transfer_id);
        Ok(Ingested::Stored)
    }

    /// Store a message locally and deliver it in the background.
    pub fn send_message(self: &Arc<Self>, to: &str, body: &str) {
        let msg_id = crate::random_id();
        let peer_name = self
            .db
            .peer(to)
            .map(|p| p.name)
            .unwrap_or_else(|| to.to_string());
        let seq = self.db.conv_next_out(to) as u64;
        if let Err(e) = self
            .db
            .insert_message(&msg_id, "out", to, &peer_name, body, "sending")
        {
            self.log(format!("[!] db error: {e}"));
            return;
        }
        let _ = self.db.outbox_add(&msg_id, to, body, seq);
        self.notify_message();
        self.spawn_delivery(msg_id, to.to_string(), body.to_string(), seq);
    }

    fn spawn_delivery(self: &Arc<Self>, msg_id: String, to: String, body: String, seq: u64) {
        let node = self.clone();
        std::thread::spawn(move || match node.do_send(&to, &body, &msg_id, seq) {
            Ok(chunks) => {
                node.db.outbox_remove(&msg_id);
                // A receipt may have raced past us while we were still sending.
                if node.db.status_of(&msg_id).as_deref() != Some("delivered") {
                    node.db.set_status(&msg_id, "sent");
                }
                node.log(format!("[+] sent {msg_id} to {to} in {chunks} DNS queries"));
                node.notify_message();
            }
            Err(e) => {
                // Keep it queued: the outbox worker retries with backoff.
                node.db.set_status(&msg_id, "queued");
                node.log(format!("[~] send {msg_id} to {to} failed: {e} (queued)"));
                node.notify_message();
            }
        });
    }

    /// Retry anything the outbox is still holding.
    fn spawn_outbox(self: &Arc<Self>) {
        let node = self.clone();
        std::thread::spawn(move || {
            while !node.stopped() {
                std::thread::sleep(Duration::from_secs(OUTBOX_SECS));
                if node.stopped() {
                    return;
                }
                for (msg_id, to, body, attempts, seq) in node.db.outbox_due(4) {
                    if attempts >= OUTBOX_MAX_ATTEMPTS {
                        node.db.outbox_remove(&msg_id);
                        node.db.set_status(&msg_id, "failed");
                        node.log(format!(
                            "[!] giving up on {msg_id} after {attempts} attempts"
                        ));
                        node.notify_message();
                        continue;
                    }
                    match node.do_send(&to, &body, &msg_id, seq) {
                        Ok(_) => {
                            node.db.outbox_remove(&msg_id);
                            if node.db.status_of(&msg_id).as_deref() != Some("delivered") {
                                node.db.set_status(&msg_id, "sent");
                            }
                            node.log(format!("[+] outbox: delivered {msg_id} to {to}"));
                            node.notify_message();
                        }
                        Err(e) => {
                            let backoff = 5i64 << attempts.min(6);
                            node.db
                                .outbox_bump(&msg_id, crate::db::now_secs() + backoff);
                            node.log(format!("[~] outbox retry {msg_id} in {backoff}s: {e}"));
                        }
                    }
                }
            }
        });
    }

    // ---- inbound --------------------------------------------------------

    /// Verify the sender's signature against the key pinned for `env.from`.
    ///
    /// A first contact is pinned (TOFU) and accepted; a *changed* key is
    /// refused and logged loudly, because that is what impersonation looks like.
    fn verify_sender(&self, env: &Envelope) -> Result<(), String> {
        if let Some(bundle) = env.bundle {
            let (ed, x) =
                identity::split_bundle(&bundle).ok_or("malformed public bundle in envelope")?;
            if self.db.pin_peer_keys(&env.from, &ed, &x)? == crate::db::PinOutcome::Changed {
                return Err(format!(
                    "KEY CHANGED for {}: refusing the message (pinned fingerprint {}, offered {})",
                    env.from,
                    self.db
                        .peer_keys(&env.from)
                        .and_then(|(ed, _)| ed)
                        .and_then(|v| <[u8; 32]>::try_from(v.as_slice()).ok())
                        .map(|k| identity::fingerprint(&k))
                        .unwrap_or_else(|| "unknown".into()),
                    identity::fingerprint(&ed)
                ));
            }
        }
        let pinned = self
            .db
            .peer_keys(&env.from)
            .and_then(|(ed, _)| ed)
            .and_then(|v| <[u8; 32]>::try_from(v.as_slice()).ok())
            .ok_or_else(|| format!("no pinned key for {} (unsigned first contact)", env.from))?;
        if !env.verify(&pinned) {
            return Err(format!(
                "bad signature from {} (signed by a different key?)",
                env.from
            ));
        }
        // The envelope tells us whether they already hold our X25519 key.
        self.db.set_peer_knows_us(&env.from, env.peer_has_key);
        Ok(())
    }

    /// Handle one fully reassembled, decrypted envelope.
    pub fn ingest(self: &Arc<Self>, msg_id: &str, env: &Envelope) -> Result<Ingested, String> {
        let me = self.cfg.node_id.as_str();
        if env.to != me {
            self.log(format!(
                "[!] dropping {msg_id}: addressed to {}, we are {me}",
                env.to
            ));
            return Ok(Ingested::Ignored);
        }
        if let Err(e) = self.verify_sender(env) {
            self.log(format!("[!] rejected {msg_id}: {e}"));
            return Ok(Ingested::Rejected(e));
        }
        if !self.db.seen_mark(msg_id) {
            return Ok(Ingested::Duplicate);
        }

        match env.msg_type {
            MsgType::Ack => {
                let acked = env.text();
                self.db.set_status(&acked, "delivered");
                self.log(format!(
                    "[✓] delivered {} (receipt from {})",
                    acked, env.from
                ));
                self.notify_message();
                Ok(Ingested::Receipt)
            }
            MsgType::Message => {
                let text = std::str::from_utf8(&env.body)
                    .map_err(|_| format!("message {msg_id} body is not valid utf-8"))?;
                let peer_name = self
                    .db
                    .peer(&env.from)
                    .map(|p| p.name)
                    .unwrap_or_else(|| env.from.clone());
                if let Some(missing) = self.db.conv_observe_in(&env.from, env.conv_seq as i64) {
                    self.log(format!(
                        "[!] gap from {}: expected #{missing}, got #{}",
                        env.from, env.conv_seq
                    ));
                }
                let stored = self
                    .db
                    .insert_message(msg_id, "in", &env.from, &peer_name, text, "received")?;
                if stored.is_none() {
                    return Ok(Ingested::Duplicate);
                }
                self.db.touch_peer(&env.from);
                self.log(format!(
                    "[<] message {msg_id} from {} #{}{} ({} bytes)",
                    env.from,
                    env.conv_seq,
                    match env.alg {
                        Alg::Peer => " peer-key",
                        Alg::Group => "",
                    },
                    env.body.len()
                ));
                self.notify_message();
                self.send_receipt(&env.from, msg_id);
                Ok(Ingested::Stored)
            }
            MsgType::File => self.ingest_file_part(msg_id, env),
            other => {
                self.log(format!("[~] ignoring {other:?} from {}", env.from));
                Ok(Ingested::Ignored)
            }
        }
    }

    /// Open and ingest blobs the relay pushed back in an ack reply.
    pub fn accept_pushed(self: &Arc<Self>, pushed: &[(String, String)]) {
        for (mid, blob) in pushed {
            match self.open_blob(mid, blob) {
                Ok(env) => {
                    if let Err(e) = self.ingest(mid, &env) {
                        self.log(format!("[!] pushed {mid} ingest failed: {e}"));
                    }
                }
                Err(e) => self.log(format!("[~] pushed {mid} could not be opened: {e}")),
            }
        }
    }

    /// Best-effort end-to-end delivery receipt, sent off-thread.
    fn send_receipt(self: &Arc<Self>, to: &str, msg_id: &str) {
        if to == self.cfg.node_id || self.stopped() || self.key.is_none() {
            return;
        }
        let node = self.clone();
        let to = to.to_string();
        let msg_id = msg_id.to_string();
        std::thread::spawn(move || {
            let (payload_key, alg, include_bundle, peer_has_key) = match node.seal_params(&to) {
                Ok(p) => p,
                Err(e) => {
                    node.log(format!("[~] no receipt for {msg_id}: {e}"));
                    return;
                }
            };
            let mut receipt = Envelope::new(MsgType::Ack, &node.cfg.node_id, &to, &msg_id)
                .with_alg(alg)
                .with_peer_has_key(peer_has_key);
            if include_bundle {
                receipt = receipt.with_bundle(node.identity.bundle());
            }
            receipt.sign(&node.identity);
            let rid = crate::random_id();
            let blob = match protocol::encode_blob(&payload_key, &receipt, &rid) {
                Ok(b) => b,
                Err(e) => {
                    node.log(format!("[~] receipt encode failed: {e}"));
                    return;
                }
            };
            let tag = match node.tag_for(&to) {
                Ok(t) => t,
                Err(e) => {
                    node.log(format!("[~] no receipt route: {e}"));
                    return;
                }
            };
            let sender_tag = match node.tag.clone() {
                Some(t) => t,
                None => return,
            };
            let egress = match node.egress_for(Some(&to)) {
                Ok(e) => e,
                Err(e) => {
                    node.log(format!("[~] no route for receipt {msg_id}: {e}"));
                    return;
                }
            };
            match sender::send_blob(&node, egress, &blob, &rid, &tag, &sender_tag) {
                Ok(sent) => {
                    node.log(format!("[>] receipt {msg_id} -> {to}"));
                    node.accept_pushed(&sent.pushed);
                }
                Err(e) => node.log(format!("[~] receipt {msg_id} -> {to} failed: {e}")),
            }
        });
    }

    // ---- lifecycle ------------------------------------------------------

    /// Start the UDP + TCP listener, the poller, housekeeping and the outbox.
    pub fn start(self: &Arc<Self>) {
        listener::spawn(self.clone());
        if self.cfg.relay.is_some() || self.cfg.egress == EgressKind::Resolver {
            poller::spawn(self.clone());
        }
        self.spawn_housekeeping();
        self.spawn_outbox();
    }

    /// Periodically drop stale chunks, expired mail and stale replay entries.
    fn spawn_housekeeping(self: &Arc<Self>) {
        let node = self.clone();
        std::thread::spawn(move || {
            while !node.stopped() {
                std::thread::sleep(Duration::from_secs(HOUSEKEEPING_SECS));
                if node.stopped() {
                    return;
                }
                node.db.gc_chunks(CHUNK_TTL_SECS);
                node.db.seen_gc(SEEN_TTL_SECS);
                let purged = node.db.mailbox_gc(MAILBOX_TTL_SECS);
                if purged > 0 && node.cfg.verbose {
                    node.log(format!("[*] mailbox housekeeping purged {purged} rows"));
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::encode_blob;

    const TEST_KEY: &str = "unit-test-passphrase";

    fn node_with(id: &str, relay_mode: RelayMode, key: Option<&str>) -> (Arc<Node>, Arc<Db>) {
        let db = Arc::new(Db::open_in_memory().unwrap());
        let cfg = Config {
            node_id: id.to_string(),
            relay_mode,
            shared_key: key.unwrap_or("").to_string(),
            listen_addr: "127.0.0.1:0".to_string(),
            direct_server: Some("127.0.0.1:9".to_string()),
            ..Config::default()
        };
        let (tx, _rx) = std::sync::mpsc::channel();
        (Node::new(cfg, db.clone(), tx).unwrap(), db)
    }

    fn node(id: &str) -> (Arc<Node>, Arc<Db>) {
        node_with(id, RelayMode::Client, Some(TEST_KEY))
    }

    /// A signed envelope from an identity that the receiver has never met.
    fn from_stranger(me: &str, body: &str) -> (Envelope, Identity) {
        let id = Identity::generate();
        let env = Envelope::new(MsgType::Message, "aaaa1111", me, body)
            .with_seq(1)
            .with_bundle(id.bundle())
            .signed(&id);
        (env, id)
    }

    #[test]
    fn first_contact_is_pinned_and_stored() {
        let (n, db) = node("bbbb2222");
        let (env, id) = from_stranger("bbbb2222", "hello");
        assert_eq!(n.ingest("m1", &env).unwrap(), Ingested::Stored);
        assert_eq!(db.messages_for("aaaa1111", 10).len(), 1);
        let (pinned, _) = db.peer_keys("aaaa1111").expect("pinned");
        assert_eq!(pinned.unwrap(), id.ed_public.to_vec());
    }

    #[test]
    fn unsigned_or_forged_messages_are_rejected() {
        let (n, db) = node("bbbb2222");

        // Never met this sender and there is no bundle to pin.
        let unsigned = Envelope::new(MsgType::Message, "aaaa1111", "bbbb2222", "no sig");
        assert!(matches!(
            n.ingest("u1", &unsigned).unwrap(),
            Ingested::Rejected(_)
        ));

        // Bundle present but the signature belongs to a different key.
        let mallory = Identity::generate();
        let mut env = Envelope::new(MsgType::Message, "aaaa1111", "bbbb2222", "spoof")
            .with_bundle(mallory.bundle());
        env.sig = mallory.sign(&env.encode()[..0]); // garbage signature
        assert!(matches!(
            n.ingest("u2", &env).unwrap(),
            Ingested::Rejected(_)
        ));
        assert!(db.recent_messages(10).is_empty());
    }

    #[test]
    fn a_changed_key_is_refused() {
        let (n, db) = node("bbbb2222");
        let (first, id) = from_stranger("bbbb2222", "hello");
        assert_eq!(n.ingest("m1", &first).unwrap(), Ingested::Stored);

        // Same id, different key: that is what impersonation looks like.
        let impostor = Identity::generate();
        let env = Envelope::new(MsgType::Message, "aaaa1111", "bbbb2222", "i am bob now")
            .with_seq(2)
            .with_bundle(impostor.bundle())
            .signed(&impostor);
        match n.ingest("m2", &env).unwrap() {
            Ingested::Rejected(why) => assert!(why.contains("KEY CHANGED"), "{why}"),
            other => panic!("expected rejection, got {other:?}"),
        }
        // The pin is unchanged and the message was not stored.
        let (pinned, _) = db.peer_keys("aaaa1111").unwrap();
        assert_eq!(pinned.unwrap(), id.ed_public.to_vec());
        assert_eq!(db.messages_for("aaaa1111", 10).len(), 1);
    }

    #[test]
    fn replays_are_dropped() {
        let (n, _db) = node("bbbb2222");
        let (env, _id) = from_stranger("bbbb2222", "once");
        assert_eq!(n.ingest("m1", &env).unwrap(), Ingested::Stored);
        assert_eq!(n.ingest("m1", &env).unwrap(), Ingested::Duplicate);
    }

    #[test]
    fn delivery_receipts_flip_the_status() {
        let (n, db) = node("aaaa1111");
        db.insert_message("out1", "out", "bbbb2222", "bob", "hi", "sent")
            .unwrap();
        let bob = Identity::generate();
        db.pin_peer_keys("bbbb2222", &bob.ed_public, &bob.x_public)
            .unwrap();
        let receipt = Envelope::new(MsgType::Ack, "bbbb2222", "aaaa1111", "out1").signed(&bob);
        assert_eq!(n.ingest("ack1", &receipt).unwrap(), Ingested::Receipt);
        assert_eq!(db.messages_for("bbbb2222", 10)[0].status, "delivered");
    }

    #[test]
    fn mail_addressed_elsewhere_is_ignored() {
        let (n, db) = node("bbbb2222");
        let id = Identity::generate();
        let env = Envelope::new(MsgType::Message, "aaaa1111", "cccc3333", "not for me")
            .with_bundle(id.bundle())
            .signed(&id);
        assert_eq!(n.ingest("m1", &env).unwrap(), Ingested::Ignored);
        assert!(db.recent_messages(10).is_empty());
    }

    #[test]
    fn peer_payload_keys_need_both_sides_to_know_the_bundle() {
        // A: no idea who B is -> group key + bundle.
        let (a, _) = node("aaaa1111");
        let (_key, alg, include_bundle, peer_has_key) = a.seal_params("bbbb2222").unwrap();
        assert_eq!(alg, Alg::Group);
        assert!(include_bundle);
        assert!(!peer_has_key);

        // Once B knows A's key (they told us), we still send Group+bundle until
        // we see a message from them that says they hold ours.
        let db_a = a.db.clone();
        db_a.pin_peer_keys("bbbb2222", &[7u8; 32], &[9u8; 32])
            .unwrap();
        let (_, alg, include_bundle, peer_has_key) = a.seal_params("bbbb2222").unwrap();
        assert_eq!(alg, Alg::Group, "they may not have our key yet");
        assert!(include_bundle, "so we still have to carry our bundle");
        assert!(peer_has_key);

        // Now they have told us they hold our key: switch to per-recipient.
        db_a.set_peer_knows_us("bbbb2222", true);
        let (_, alg, include_bundle, peer_has_key) = a.seal_params("bbbb2222").unwrap();
        assert_eq!(alg, Alg::Peer);
        assert!(!include_bundle);
        assert!(peer_has_key);
    }

    #[test]
    fn open_blob_finds_peer_keyed_payloads() {
        let (b, db_b) = node("bbbb2222");
        let a = Identity::generate();
        db_b.pin_peer_keys("aaaa1111", &a.ed_public, &a.x_public)
            .unwrap();

        // A encrypts to B with the X25519 pair key.
        let shared = a.agree(&b.identity.x_public).unwrap();
        let key = identity::kdf(&shared, PEER_KEY_CONTEXT);
        let env = Envelope::new(MsgType::Message, "aaaa1111", "bbbb2222", "peer-keyed")
            .with_alg(Alg::Peer)
            .signed(&a);
        let blob = encode_blob(&key, &env, "p1").unwrap();

        let opened = b.open_blob("p1", &blob).expect("peer key tried");
        assert_eq!(opened.text(), "peer-keyed");
        assert_eq!(b.ingest("p1", &opened).unwrap(), Ingested::Stored);
    }

    #[test]
    fn the_group_key_cannot_open_a_peer_keyed_blob() {
        // The security claim behind Alg::Peer: someone holding only the shared
        // passphrase (an eavesdropper, or the relay operator) cannot read it.
        let b = Identity::generate();
        let a = Identity::generate();
        let shared = a.agree(&b.x_public).unwrap();
        let peer_key = identity::kdf(&shared, PEER_KEY_CONTEXT);
        let env = Envelope::new(MsgType::Message, "aaaa1111", "bbbb2222", "for b only")
            .with_alg(Alg::Peer)
            .signed(&a);
        let blob = encode_blob(&peer_key, &env, "pk1").unwrap();

        let group_key = crate::key_from_str(TEST_KEY);
        assert!(
            crate::protocol::decode_blob(&group_key, &blob, "pk1").is_err(),
            "the group key must not open a peer-keyed payload"
        );
    }

    #[test]
    fn routing_tags_are_keyed_and_stable() {
        let (a, _) = node("aaaa1111");
        let tag = a.tag.clone().expect("key set");
        assert_eq!(tag.len(), crate::protocol::TAG_LEN);
        assert_eq!(
            a.tag_for("bbbb2222").unwrap(),
            a.tag_for("bbbb2222").unwrap()
        );
        assert_ne!(
            a.tag_for("bbbb2222").unwrap(),
            a.tag_for("cccc3333").unwrap()
        );
        let (b, _) = node_with(
            "aaaa1111",
            RelayMode::Client,
            Some("a-different-passphrase"),
        );
        assert_ne!(
            b.tag_for("bbbb2222").unwrap(),
            a.tag_for("bbbb2222").unwrap()
        );
    }

    #[test]
    fn keyless_node_cannot_send_or_route() {
        let (n, _) = node_with("rrrr9999", RelayMode::Authoritative, None);
        assert!(n.key.is_none());
        assert!(n.tag.is_none());
        let err = n.do_send("bbbb2222", "hi", "m1", 1).unwrap_err();
        assert!(err.contains("shared_key"), "unexpected error: {err}");
        assert!(n.tag_for("bbbb2222").is_err());
    }

    #[test]
    fn only_peer_or_authoritative_modes_relay() {
        let (client, _) = node_with("cccc3333", RelayMode::Client, Some(TEST_KEY));
        assert!(!client.is_relay(), "a client must not park strangers' mail");
        let (peer, _) = node_with("dddd4444", RelayMode::Peer, Some(TEST_KEY));
        assert!(peer.is_relay());
        let (auth, _) = node_with("eeee5555", RelayMode::Authoritative, Some(TEST_KEY));
        assert!(auth.is_relay());
    }

    #[test]
    fn identities_are_persistent() {
        let path = std::env::temp_dir().join(format!("dns_lab_id_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let p = path.to_string_lossy().to_string();
        let (first, second) = {
            let db1 = Arc::new(Db::open(&p).unwrap());
            let cfg = Config {
                node_id: "aaaa1111".to_string(),
                shared_key: TEST_KEY.to_string(),
                ..Config::default()
            };
            let (tx, _rx) = std::sync::mpsc::channel();
            let n1 = Node::new(cfg.clone(), db1, tx).unwrap();
            let fp1 = n1.identity.ed_public;
            drop(n1);
            let db2 = Arc::new(Db::open(&p).unwrap());
            let (tx2, _rx2) = std::sync::mpsc::channel();
            let n2 = Node::new(cfg, db2, tx2).unwrap();
            (fp1, n2.identity.ed_public)
        };
        assert_eq!(first, second, "identity must survive a restart");
        let _ = std::fs::remove_file(&path);
    }
}
