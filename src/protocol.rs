//! Wire protocol v5: signed envelopes, per-recipient payload keys, and the DNS
//! QNAME framing that carries them.
//!
//! ```text
//! Envelope { type, alg, ts, conv_seq, from, to, bundle?, body }
//!     │ body → best of {raw, SMAZ, DEFLATE} + 1-byte codec tag
//!     ▼
//! canonical bytes ──Ed25519 sign──► header ‖ sig(64) ‖ body
//!     │ payload key = group key (alg 0) or X25519(ours, theirs) (alg 1)
//!     ▼
//! ChaCha20-Poly1305(aad = msg_id) ─► nonce‖ciphertext ─► Base32
//!     ▼
//! m.<tag>.<msg-id>.<seq>.<total>.<data>.<zone>   (f.… for the last chunk)
//! ```
//!
//! The signature is what makes `from` meaningful: a shared passphrase lets any
//! key holder claim any id, so every envelope is signed with the sender's
//! Ed25519 key and checked against the key pinned for that id.

use crate::identity::{Identity, PublicKey, Signature64, verify as identity_verify};
use crate::{decrypt_with_key_aad, encrypt_with_key_aad, smaz};
use data_encoding::BASE32_NOPAD;
use std::io::{Read, Write};

/// Protocol version carried inside the encrypted envelope.
pub const VERSION: u8 = 5;

/// Practical maximum presentation length of a DNS name.
pub const MAX_QNAME: usize = 253;
/// Maximum length of a single DNS label.
pub const MAX_LABEL: usize = 63;
/// Longest accepted message id label (`random_id` produces 8).
pub const MAX_MSG_ID: usize = 16;
/// Hard cap on chunks per message; bounds reassembly memory.
pub const MAX_CHUNKS: u32 = 4096;
/// Hard cap on a decompressed body, so a hostile peer cannot zip-bomb us.
pub const MAX_BODY_BYTES: usize = 256 * 1024;
/// A routing tag is 4 bytes of keyed MAC rendered as 8 hex chars.
pub const TAG_LEN: usize = 8;
/// Length of the ids our encoder emits (`random_id`), used for budgeting.
pub const MSG_ID_LEN: usize = 8;
/// Length of `ed25519_pub || x25519_pub`.
pub const MAX_BUNDLE: usize = crate::identity::BUNDLE_LEN;

/// Tunnel verbs. QNAMEs are lowercased before parsing, so verbs must be
/// distinguishable in lowercase.
pub mod verb {
    /// A message data chunk.
    pub const MSG: char = 'm';
    /// The last chunk of a message. It also carries the sender's routing tag so
    /// the relay can hand that sender any queued mail in the ack reply (push).
    pub const FINAL: char = 'f';
    /// Poll the relay mailbox.
    pub const POLL: char = 'p';
    /// Acknowledge (drop from the mailbox) one or more message ids.
    pub const ACK: char = 'a';
}

/// Application-level message kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgType {
    Message = 1,
    Ack = 2,
    Read = 3,
    Heartbeat = 4,
    /// One part of a file transfer (see [`crate::file`]).
    File = 5,
}

impl MsgType {
    fn from_u8(v: u8) -> Option<MsgType> {
        match v {
            1 => Some(MsgType::Message),
            2 => Some(MsgType::Ack),
            3 => Some(MsgType::Read),
            4 => Some(MsgType::Heartbeat),
            5 => Some(MsgType::File),
            _ => None,
        }
    }

    /// Receipts, read markers and heartbeats are not themselves acknowledged.
    ///
    /// File parts are acknowledged once, by the receiver, when the whole
    /// transfer has been assembled and verified — not per part.
    pub fn expects_receipt(&self) -> bool {
        matches!(self, MsgType::Message)
    }
}

/// Which key protects the payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Alg {
    /// Derive the payload key from the shared passphrase (works with strangers).
    #[default]
    Group = 0,
    /// Derive the payload key from X25519 with the recipient's pinned key, so
    /// only that node can read it even if the passphrase leaks.
    Peer = 1,
}

impl Alg {
    fn from_u8(v: u8) -> Option<Alg> {
        match v {
            0 => Some(Alg::Group),
            1 => Some(Alg::Peer),
            _ => None,
        }
    }
}

const FLAG_BUNDLE: u8 = 0b0000_0001;
const FLAG_PEER_HAS_KEY: u8 = 0b0000_0010;

/// Body compression codec, stored as the first byte of the body field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Codec {
    Raw = 0,
    Smaz = 1,
    Deflate = 2,
}

impl Codec {
    fn from_u8(v: u8) -> Option<Codec> {
        match v {
            0 => Some(Codec::Raw),
            1 => Some(Codec::Smaz),
            2 => Some(Codec::Deflate),
            _ => None,
        }
    }
}

/// Deflate `input`, returning `None` if the encoder fails.
fn deflate(input: &[u8]) -> Option<Vec<u8>> {
    let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(6));
    enc.write_all(input).ok()?;
    enc.finish().ok()
}

/// Inflate at most [`MAX_BODY_BYTES`] of output.
fn inflate(input: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    flate2::read::DeflateDecoder::new(input)
        .take(MAX_BODY_BYTES as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|e| format!("deflate decode failed: {e}"))?;
    if out.len() > MAX_BODY_BYTES {
        return Err("body exceeds decompression limit".into());
    }
    Ok(out)
}

/// Compress a body with whichever codec wins, prefixed by its tag byte.
///
/// Bodies are byte strings, not text: a file part carries raw bytes. SMAZ is a
/// text dictionary, so it is only tried when the body happens to be UTF-8.
fn pack_body(body: &[u8]) -> Vec<u8> {
    let raw = body;
    let mut best: (Codec, Vec<u8>) = (Codec::Raw, raw.to_vec());

    if let Ok(text) = std::str::from_utf8(raw) {
        let smaz_bytes = smaz::compress(text);
        if smaz_bytes.len() < best.1.len() {
            best = (Codec::Smaz, smaz_bytes);
        }
    }
    // Below ~40 bytes DEFLATE's header alone tends to lose.
    if raw.len() >= 40
        && let Some(deflated) = deflate(raw)
        && deflated.len() < best.1.len()
    {
        best = (Codec::Deflate, deflated);
    }

    let mut out = Vec::with_capacity(best.1.len() + 1);
    out.push(best.0 as u8);
    out.extend_from_slice(&best.1);
    out
}

/// Inverse of [`pack_body`].
fn unpack_body(data: &[u8]) -> Result<Vec<u8>, String> {
    let (tag, payload) = data.split_first().ok_or("empty body")?;
    let codec = Codec::from_u8(*tag).ok_or_else(|| format!("unknown body codec {tag}"))?;
    match codec {
        Codec::Raw => Ok(payload.to_vec()),
        Codec::Smaz => smaz::decompress(payload)
            .map(|s| s.into_bytes())
            .map_err(|e| e.to_string()),
        Codec::Deflate => inflate(payload),
    }
}

/// A signed message envelope.
#[derive(Debug, Clone)]
pub struct Envelope {
    pub msg_type: MsgType,
    pub alg: Alg,
    pub ts: u64,
    /// Per-conversation sequence number (1-based); 0 for receipts.
    pub conv_seq: u64,
    pub from: String,
    pub to: String,
    /// Sender's `ed25519_pub || x25519_pub`, present until both sides have
    /// exchanged bundles at least once.
    pub bundle: Option<[u8; MAX_BUNDLE]>,
    /// True when the sender already holds *our* X25519 key. Once both sides can
    /// say that, payloads switch to [`Alg::Peer`].
    pub peer_has_key: bool,
    pub sig: Signature64,
    /// Raw body bytes: text for messages, opaque bytes for a file part.
    pub body: Vec<u8>,
    /// Exactly the body bytes that were signed, so verification never depends
    /// on re-compressing to the same result.
    body_packed: Vec<u8>,
}

impl Envelope {
    pub fn new(msg_type: MsgType, from: &str, to: &str, body: &str) -> Self {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Envelope {
            msg_type,
            alg: Alg::Group,
            ts,
            conv_seq: 0,
            from: from.to_string(),
            to: to.to_string(),
            bundle: None,
            peer_has_key: false,
            sig: [0u8; crate::identity::SIG_LEN],
            body: body.as_bytes().to_vec(),
            body_packed: pack_body(body.as_bytes()),
        }
    }

    /// A body that is not text (a file part).
    pub fn new_bytes(msg_type: MsgType, from: &str, to: &str, body: Vec<u8>) -> Self {
        let mut env = Envelope::new(msg_type, from, to, "");
        env.body_packed = pack_body(&body);
        env.body = body;
        env
    }

    /// The body as text, lossily. Use [`Envelope::body`] for binary payloads.
    pub fn text(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.body)
    }

    /// Set the body and re-pack it (used by tests and by rewritten envelopes).
    pub fn set_text(&mut self, body: &str) {
        self.body = body.as_bytes().to_vec();
        self.body_packed = pack_body(body.as_bytes());
    }

    pub fn with_seq(mut self, seq: u64) -> Self {
        self.conv_seq = seq;
        self
    }

    pub fn with_alg(mut self, alg: Alg) -> Self {
        self.alg = alg;
        self
    }

    pub fn with_bundle(mut self, bundle: [u8; MAX_BUNDLE]) -> Self {
        self.bundle = Some(bundle);
        self
    }

    pub fn with_peer_has_key(mut self, has: bool) -> Self {
        self.peer_has_key = has;
        self
    }

    /// Sign in place and return `self`.
    pub fn signed(mut self, signer: &Identity) -> Self {
        self.sign(signer);
        self
    }

    pub fn sign(&mut self, signer: &Identity) {
        self.sig = signer.sign(&self.signing_bytes());
    }

    pub fn is_signed(&self) -> bool {
        self.sig != [0u8; crate::identity::SIG_LEN]
    }

    /// Everything that is signed: header (without the signature) then body.
    fn signing_bytes(&self) -> Vec<u8> {
        let mut out = self.prefix();
        out.extend_from_slice(&self.body_packed);
        out
    }

    /// Header bytes up to (but excluding) the signature field.
    fn prefix(&self) -> Vec<u8> {
        let from = self.from.as_bytes();
        let to = self.to.as_bytes();
        let from = &from[..from.len().min(255)];
        let to = &to[..to.len().min(255)];

        let mut out = Vec::with_capacity(48 + from.len() + to.len());
        out.push(VERSION);
        out.push(self.msg_type as u8);
        out.push(self.alg as u8);
        let mut flags = 0u8;
        if self.bundle.is_some() {
            flags |= FLAG_BUNDLE;
        }
        if self.peer_has_key {
            flags |= FLAG_PEER_HAS_KEY;
        }
        out.push(flags);
        out.extend_from_slice(&self.ts.to_le_bytes());
        out.extend_from_slice(&self.conv_seq.to_le_bytes());
        out.push(from.len() as u8);
        out.extend_from_slice(from);
        out.push(to.len() as u8);
        out.extend_from_slice(to);
        match self.bundle {
            Some(bundle) => {
                out.push(MAX_BUNDLE as u8);
                out.extend_from_slice(&bundle);
            }
            None => out.push(0),
        }
        out
    }

    /// Full serialized envelope (header + signature + body).
    pub fn encode(&self) -> Vec<u8> {
        let mut out = self.prefix();
        out.extend_from_slice(&self.sig);
        out.extend_from_slice(&(self.body_packed.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.body_packed);
        out
    }

    /// Check the signature against a pinned Ed25519 key.
    pub fn verify(&self, ed_public: &PublicKey) -> bool {
        self.is_signed() && identity_verify(ed_public, &self.signing_bytes(), &self.sig)
    }

    /// Parse raw bytes back into an envelope (signature not checked here).
    pub fn decode(data: &[u8]) -> Result<Envelope, String> {
        if data.len() < 4 {
            return Err("envelope too short".into());
        }
        if data[0] != VERSION {
            return Err(format!("unsupported protocol version {}", data[0]));
        }
        let msg_type = MsgType::from_u8(data[1]).ok_or("invalid message type")?;
        let alg = Alg::from_u8(data[2]).ok_or("invalid payload algorithm")?;
        let flags = data[3];

        let mut pos = 4;
        let ts = read_u64(data, &mut pos)?;
        let conv_seq = read_u64(data, &mut pos)?;
        let from = read_str(data, &mut pos)?;
        let to = read_str(data, &mut pos)?;

        let bundle_len = *data.get(pos).ok_or("truncated bundle length")? as usize;
        pos += 1;
        let bundle = if bundle_len == 0 {
            None
        } else {
            if bundle_len != MAX_BUNDLE || flags & FLAG_BUNDLE == 0 {
                return Err("invalid bundle".into());
            }
            let bytes = data.get(pos..pos + bundle_len).ok_or("truncated bundle")?;
            let mut b = [0u8; MAX_BUNDLE];
            b.copy_from_slice(bytes);
            pos += bundle_len;
            Some(b)
        };

        let sig_bytes = data
            .get(pos..pos + crate::identity::SIG_LEN)
            .ok_or("truncated signature")?;
        let mut sig = [0u8; crate::identity::SIG_LEN];
        sig.copy_from_slice(sig_bytes);
        pos += crate::identity::SIG_LEN;

        let body_len = read_u32(data, &mut pos)? as usize;
        if body_len > MAX_BODY_BYTES {
            return Err("body length exceeds protocol limit".into());
        }
        let body_packed = data
            .get(pos..pos + body_len)
            .ok_or("envelope truncated at body")?
            .to_vec();
        let body = unpack_body(&body_packed)?;

        Ok(Envelope {
            msg_type,
            alg,
            ts,
            conv_seq,
            from,
            to,
            bundle,
            peer_has_key: flags & FLAG_PEER_HAS_KEY != 0,
            sig,
            body,
            body_packed,
        })
    }
}

fn read_u64(data: &[u8], pos: &mut usize) -> Result<u64, String> {
    let bytes = data.get(*pos..*pos + 8).ok_or("envelope truncated")?;
    *pos += 8;
    Ok(u64::from_le_bytes(bytes.try_into().unwrap()))
}

fn read_u32(data: &[u8], pos: &mut usize) -> Result<u32, String> {
    let bytes = data.get(*pos..*pos + 4).ok_or("envelope truncated")?;
    *pos += 4;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
}

fn read_str(data: &[u8], pos: &mut usize) -> Result<String, String> {
    let len = *data.get(*pos).ok_or("truncated string length")? as usize;
    *pos += 1;
    let bytes = data.get(*pos..*pos + len).ok_or("truncated string field")?;
    *pos += len;
    std::str::from_utf8(bytes)
        .map(|s| s.to_string())
        .map_err(|_| "invalid utf-8 in envelope".into())
}

/// Base32-encode bytes (uppercase, no padding).
pub fn base32_encode(data: &[u8]) -> String {
    BASE32_NOPAD.encode(data)
}

/// Base32-decode a (case-insensitive) string.
pub fn base32_decode(s: &str) -> Result<Vec<u8>, String> {
    let up = s.trim().replace(['.', ' ', '-'], "").to_ascii_uppercase();
    BASE32_NOPAD
        .decode(up.as_bytes())
        .map_err(|e| format!("base32 decode failed: {e}"))
}

/// Encrypt a signed envelope into a Base32 blob, authenticated against `msg_id`.
pub fn encode_blob(key: &[u8; 32], env: &Envelope, msg_id: &str) -> Result<String, String> {
    if !env.is_signed() {
        return Err("refusing to seal an unsigned envelope".into());
    }
    let raw = env.encode();
    let cipher = encrypt_with_key_aad(key, &raw, msg_id.as_bytes())?;
    Ok(base32_encode(&cipher))
}

/// Decode a Base32 blob back into an envelope. The signature is *not* checked
/// here: that needs the sender's pinned key, which lives in the database.
pub fn decode_blob(key: &[u8; 32], blob: &str, msg_id: &str) -> Result<Envelope, String> {
    let cipher = base32_decode(blob)?;
    let raw = decrypt_with_key_aad(key, &cipher, msg_id.as_bytes())?;
    Envelope::decode(&raw)
}

/// Maximum number of Base32 chars that fit in one message QNAME.
///
/// Budgeted for the *final* chunk (`f.<tag>.<sender-tag>.<msg-id>…`), which is
/// the longest form, so every chunk of a message can use the same size.
pub fn max_data_chars(zone: &str) -> usize {
    let zone_len = zone.trim_matches('.').len();
    let header = 1 + 1 + TAG_LEN + 1 + TAG_LEN + 1 + MSG_ID_LEN + 1 + 5 + 1 + 5 + 1;
    let budget = MAX_QNAME
        .saturating_sub(header + 1 + zone_len)
        .saturating_sub(1);
    let mut best = 0;
    for labels in 1..=8usize {
        let dots = labels - 1;
        if budget < dots {
            break;
        }
        let c = (MAX_LABEL * labels).min(budget - dots);
        best = best.max(c);
    }
    best.max(1)
}

/// Split a data field into <=63-char labels joined by dots.
fn to_labels(seg: &str) -> String {
    let mut out = String::new();
    for (i, c) in seg.as_bytes().chunks(MAX_LABEL).enumerate() {
        if i > 0 {
            out.push('.');
        }
        out.push_str(std::str::from_utf8(c).unwrap());
    }
    out
}

/// Split a Base32 payload into one data-field string per DNS query.
pub fn split_into_queries(blob: &str, max_chars: usize) -> Vec<String> {
    if blob.is_empty() {
        return vec![String::new()];
    }
    blob.as_bytes()
        .chunks(max_chars.max(1))
        .map(|c| to_labels(std::str::from_utf8(c).unwrap()))
        .collect()
}

/// Build a plain chunk or poll QNAME.
pub fn build_qname(
    verb: char,
    tag: &str,
    msg_id: &str,
    seq: u32,
    total: u32,
    data: &str,
    zone: &str,
) -> String {
    let zone = zone.trim_matches('.');
    match verb {
        verb::POLL => format!("{verb}.{tag}.0.0.{zone}"),
        _ => {
            if data.is_empty() {
                format!("{verb}.{tag}.{msg_id}.{seq}.{total}.{zone}")
            } else {
                format!("{verb}.{tag}.{msg_id}.{seq}.{total}.{data}.{zone}")
            }
        }
    }
}

/// Build the final chunk QNAME, which also carries the sender's routing tag:
/// `f.<tag>.<sender-tag>.<msg-id>.<seq>.<total>.<data>.<zone>`.
pub fn build_final_qname(
    tag: &str,
    sender_tag: &str,
    msg_id: &str,
    seq: u32,
    total: u32,
    data: &str,
    zone: &str,
) -> String {
    let zone = zone.trim_matches('.');
    if data.is_empty() {
        format!(
            "{}.{tag}.{sender_tag}.{msg_id}.{seq}.{total}.{zone}",
            verb::FINAL
        )
    } else {
        format!(
            "{}.{tag}.{sender_tag}.{msg_id}.{seq}.{total}.{data}.{zone}",
            verb::FINAL
        )
    }
}

/// Build a mailbox acknowledgement QNAME: `a.<tag>.<id>.<id>...0.0.<zone>`.
///
/// Each id is its own label: a comma is not legal in a DNS label, and a single
/// oversized label would be rejected by any resolver on the way.
pub fn build_ack_qname(tag: &str, ids: &[String], zone: &str) -> String {
    let zone = zone.trim_matches('.');
    let mut out = format!("{}.{tag}", verb::ACK);
    for id in ids {
        // Stop before the 253-char limit; anything left over is offered again
        // on the next poll, so dropping it here costs one extra round trip.
        if out.len() + 1 + id.len() + 1 + 1 + 1 + 1 + zone.len() > MAX_QNAME {
            break;
        }
        out.push('.');
        out.push_str(id);
    }
    format!("{out}.0.0.{zone}")
}

/// A parsed tunnel query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelQuery {
    pub verb: char,
    /// Routing tag of the recipient (the relay's mailbox key).
    pub tag: String,
    /// Sender's routing tag, present on the final chunk (for push).
    pub sender_tag: Option<String>,
    /// Message id (empty for polls and mailbox acks).
    pub msg_id: String,
    pub seq: u32,
    pub total: u32,
    /// Raw Base32 data exactly as carried in the QNAME (may be empty).
    pub data: String,
    /// Message ids acknowledged by an `a` query.
    pub acked: Vec<String>,
}

fn valid_tag(s: &str) -> bool {
    s.len() == TAG_LEN && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn valid_msg_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_MSG_ID && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Parse a QNAME into a tunnel query if it lives under `zone`.
///
/// Everything here is attacker-controlled, so the shape is validated strictly
/// per verb.
pub fn parse_qname(zone: &str, qname: &str) -> Option<TunnelQuery> {
    let q = qname.trim_end_matches('.').to_ascii_lowercase();
    let z = zone.trim_matches('.').to_ascii_lowercase();
    if z.is_empty() {
        return None;
    }
    let suffix = format!(".{z}");
    let core = q.strip_suffix(&suffix)?;
    if core.is_empty() {
        return None;
    }
    let parts: Vec<&str> = core.split('.').collect();
    if parts.first()?.len() != 1 {
        return None;
    }
    let verb = *parts[0].as_bytes().first()? as char;
    if !matches!(verb, verb::MSG | verb::FINAL | verb::POLL | verb::ACK) {
        return None;
    }
    let tag = parts.get(1)?;
    if !valid_tag(tag) {
        return None;
    }

    let mut query = TunnelQuery {
        verb,
        tag: tag.to_string(),
        sender_tag: None,
        msg_id: String::new(),
        seq: 0,
        total: 0,
        data: String::new(),
        acked: Vec::new(),
    };

    match verb {
        verb::POLL => {
            // p.<tag>.0.0.<zone>
            if parts.len() != 4 || parts[2] != "0" || parts[3] != "0" {
                return None;
            }
        }
        verb::ACK => {
            // a.<tag>.<id>.<id>...<id>.0.0.<zone>
            if parts.len() < 5 || parts[parts.len() - 2] != "0" || parts[parts.len() - 1] != "0" {
                return None;
            }
            let ids: Vec<String> = parts[2..parts.len() - 2]
                .iter()
                .map(|s| s.to_string())
                .collect();
            if ids.is_empty() || ids.len() > 32 || !ids.iter().all(|id| valid_msg_id(id)) {
                return None;
            }
            query.acked = ids;
        }
        verb::FINAL => {
            // f.<tag>.<sender-tag>.<msg-id>.<seq>.<total>.<data>.<zone>
            if parts.len() < 6 {
                return None;
            }
            let sender = parts[2];
            if !valid_tag(sender) || sender == *tag {
                return None;
            }
            query.sender_tag = Some(sender.to_string());
            fill_chunk(&mut query, &parts, 3)?;
        }
        _ => {
            // m.<tag>.<msg-id>.<seq>.<total>.<data>.<zone>
            if parts.len() < 5 {
                return None;
            }
            fill_chunk(&mut query, &parts, 2)?;
        }
    }
    Some(query)
}

/// Fill msg_id/seq/total/data for a chunk-shaped query, starting at `id_at`.
fn fill_chunk(q: &mut TunnelQuery, parts: &[&str], id_at: usize) -> Option<()> {
    let msg_id = parts.get(id_at)?;
    if !valid_msg_id(msg_id) {
        return None;
    }
    let seq = parts.get(id_at + 1)?.parse::<u32>().ok()?;
    let total = parts.get(id_at + 2)?.parse::<u32>().ok()?;
    if total == 0 || total > MAX_CHUNKS || seq >= total {
        return None;
    }
    let data = parts.get(id_at + 3..)?.join("");
    q.msg_id = msg_id.to_string();
    q.seq = seq;
    q.total = total;
    q.data = data;
    Some(())
}

/// The canonical apex name for a zone (no trailing dot).
pub fn zone_apex(zone: &str) -> String {
    zone.trim_matches('.').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key_from_str;

    fn test_key() -> [u8; 32] {
        key_from_str("unit-test-secret")
    }

    fn identity() -> Identity {
        Identity::generate()
    }

    #[test]
    fn envelope_roundtrip_with_signature() {
        let id = identity();
        let env = Envelope::new(MsgType::Message, "aaaa1111", "bbbb2222", "hello dns world")
            .with_seq(7)
            .with_bundle(id.bundle())
            .with_peer_has_key(true)
            .signed(&id);
        let raw = env.encode();
        let back = Envelope::decode(&raw).unwrap();
        assert_eq!(back.text(), "hello dns world");
        assert_eq!(back.from, "aaaa1111");
        assert_eq!(back.to, "bbbb2222");
        assert_eq!(back.conv_seq, 7);
        assert_eq!(back.alg, Alg::Group);
        assert!(back.verify(&id.ed_public));
        assert_eq!(back.bundle, Some(id.bundle()));
        assert!(back.peer_has_key);
        // Round-trips byte-for-byte, so signatures survive any re-encode.
        assert_eq!(back.encode(), raw);
    }

    #[test]
    fn tampering_with_any_field_breaks_the_signature() {
        let id = identity();
        let env = Envelope::new(MsgType::Message, "aaaa1111", "bbbb2222", "pay 10").signed(&id);
        assert!(env.verify(&id.ed_public));

        let mut forged = env.clone();
        forged.from = "cccc3333".to_string();
        assert!(!forged.verify(&id.ed_public), "sender id must be covered");

        let mut forged = env.clone();
        forged.set_text("pay 100");
        assert!(!forged.verify(&id.ed_public), "body must be covered");

        let mut forged = env.clone();
        forged.conv_seq = 99;
        assert!(!forged.verify(&id.ed_public), "seq must be covered");
    }

    #[test]
    fn another_identity_cannot_sign_for_us() {
        let alice = identity();
        let mallory = identity();
        let env = Envelope::new(MsgType::Message, "alice", "bob", "trust me").signed(&mallory);
        assert!(!env.verify(&alice.ed_public), "signature must not verify");
        assert!(env.verify(&mallory.ed_public));
    }

    #[test]
    fn unsigned_envelopes_are_refused() {
        let env = Envelope::new(MsgType::Message, "a", "b", "unsigned");
        assert!(encode_blob(&test_key(), &env, "deadbeef").is_err());
    }

    #[test]
    fn blob_roundtrip_and_aad_binding() {
        let key = test_key();
        let id = identity();
        let env = Envelope::new(
            MsgType::Message,
            "deadbeef",
            "cafebabe",
            "completely on dns",
        )
        .signed(&id);
        let blob = encode_blob(&key, &env, "deadbeef").unwrap();
        let back = decode_blob(&key, &blob, "deadbeef").unwrap();
        assert_eq!(back.text(), "completely on dns");
        assert!(back.verify(&id.ed_public));

        assert!(decode_blob(&key, &blob, "bbbbbbbb").is_err());
        assert!(decode_blob(&key_from_str("other"), &blob, "deadbeef").is_err());
    }

    #[test]
    fn deflate_beats_smaz_on_long_text() {
        let body = "alice: check the tunnel log at 09:15 -> ".repeat(40);
        let raw = body.len();
        let packed = pack_body(body.as_bytes());
        assert_eq!(packed[0], Codec::Deflate as u8, "deflate should win");
        assert!(packed.len() < raw / 2, "expected >2x compression");
        assert_eq!(unpack_body(&packed).unwrap(), body.as_bytes());
    }

    #[test]
    fn tiny_and_unicode_bodies_survive_the_codec() {
        for body in ["", "hi", "héllo wörld ✓", &"x".repeat(3000)] {
            let id = identity();
            let mut env = Envelope::new(MsgType::Message, "a1", "b2", body);
            env.sign(&id);
            let back = Envelope::decode(&env.encode()).unwrap();
            assert_eq!(back.text(), body);
            assert!(back.verify(&id.ed_public));
        }
    }

    #[test]
    fn rejects_wrong_version_and_unknown_algorithm() {
        let id = identity();
        let env = Envelope::new(MsgType::Message, "a1", "b2", "body").signed(&id);
        let mut raw = env.encode();
        raw[0] = VERSION.wrapping_add(1);
        assert!(Envelope::decode(&raw).is_err());

        let mut raw = env.encode();
        raw[2] = 9; // unknown payload algorithm
        assert!(Envelope::decode(&raw).is_err());
    }

    #[test]
    fn qname_build_parse() {
        let zone = "t.example.com";
        let q = build_qname('m', "0a1b2c3d", "deadbeef", 1, 3, "JBSWY3DP", zone);
        assert_eq!(q, "m.0a1b2c3d.deadbeef.1.3.JBSWY3DP.t.example.com");
        let parsed = parse_qname(zone, &q).unwrap();
        assert_eq!(parsed.verb, 'm');
        assert_eq!(parsed.tag, "0a1b2c3d");
        assert_eq!(parsed.msg_id, "deadbeef");
        assert_eq!(parsed.seq, 1);
        assert_eq!(parsed.total, 3);
        assert_eq!(parsed.data, "jbswy3dp");
        assert!(parsed.sender_tag.is_none());
    }

    #[test]
    fn final_qname_carries_the_sender_tag() {
        let zone = "t.example.com";
        let q = build_final_qname("0a1b2c3d", "11223344", "deadbeef", 2, 3, "AAAA", zone);
        assert_eq!(q, "f.0a1b2c3d.11223344.deadbeef.2.3.AAAA.t.example.com");
        let parsed = parse_qname(zone, &q).unwrap();
        assert_eq!(parsed.verb, 'f');
        assert_eq!(parsed.tag, "0a1b2c3d");
        assert_eq!(parsed.sender_tag.as_deref(), Some("11223344"));
        assert_eq!(parsed.msg_id, "deadbeef");
        assert_eq!(parsed.seq, 2);
    }

    #[test]
    fn poll_and_ack_qnames() {
        let zone = "t.example.com";
        let q = build_qname('p', "0a1b2c3d", "", 0, 0, "", zone);
        assert_eq!(q, "p.0a1b2c3d.0.0.t.example.com");
        let parsed = parse_qname(zone, &q).unwrap();
        assert_eq!(parsed.verb, 'p');
        assert!(parsed.msg_id.is_empty());

        let ids = vec!["deadbeef".to_string(), "cafebabe".to_string()];
        let a = build_ack_qname("0a1b2c3d", &ids, zone);
        assert_eq!(a, "a.0a1b2c3d.deadbeef.cafebabe.0.0.t.example.com");
        let parsed = parse_qname(zone, &a).unwrap();
        assert_eq!(parsed.verb, 'a');
        assert_eq!(parsed.acked, ids);
    }

    #[test]
    fn qname_parser_rejects_garbage() {
        let zone = "t.example.com";
        let q = |s: &str| parse_qname(zone, s);
        assert!(q("x.0a1b2c3d.deadbeef.0.1.AAAA.t.example.com").is_none());
        assert!(q("m.deadbee.m1.0.1.AAAA.t.example.com").is_none());
        assert!(q("m.zzzzzzzz.m1.0.1.AAAA.t.example.com").is_none());
        assert!(q("m.0a1b2c3d00.m1.0.1.AAAA.t.example.com").is_none());
        assert!(q("m.0a1b2c3d.deadbeef.5.3.AAAA.t.example.com").is_none());
        assert!(q("m.0a1b2c3d.deadbeef.0.0.AAAA.t.example.com").is_none());
        assert!(q("m.0a1b2c3d.deadbeef.0.999999.AAAA.t.example.com").is_none());
        assert!(
            q(&format!("m.0a1b2c3d.{}.0.1.AAAA.{zone}", "a".repeat(40))).is_none(),
            "message id must be bounded"
        );
        assert!(q("m.0a1b2c3d.dead-beef.0.1.AAAA.t.example.com").is_none());
        assert!(q("m.0a1b2c3d.deadbeef.0.1.AAAA.other.example").is_none());
        assert!(q("p.0a1b2c3d.0.0.AAAA.t.example.com").is_none());
        assert!(q("p.0a1b2c3d.1.0.t.example.com").is_none());
        // Mailbox acks: no ids, or bogus ids.
        assert!(q("a.0a1b2c3d.0.0.t.example.com").is_none());
        assert!(q("a.0a1b2c3d.not-an-id.0.0.t.example.com").is_none());
        // Final chunks need a sender tag, and it may not equal the recipient's.
        assert!(q("f.0a1b2c3d.deadbeef.0.1.AAAA.t.example.com").is_none());
        assert!(q("f.0a1b2c3d.0a1b2c3d.deadbeef.0.1.AAAA.t.example.com").is_none());
    }

    #[test]
    fn chunking_fits_budget() {
        let zone = "t.example.com";
        let max = max_data_chars(zone);
        let blob: String = (0..max * 2 + 7).map(|_| 'A').collect();
        let queries = split_into_queries(&blob, max);
        let total: usize = queries.iter().map(|q| q.replace('.', "").len()).sum();
        assert_eq!(total, blob.len());
        for (i, q) in queries.iter().enumerate() {
            let name = build_final_qname(
                "0a1b2c3d",
                "11223344",
                "ffffffff",
                i as u32,
                queries.len() as u32,
                q,
                zone,
            );
            assert!(name.len() <= MAX_QNAME, "qname too long: {}", name.len());
            assert!(q.split('.').all(|l| l.len() <= MAX_LABEL));
        }
    }

    #[test]
    fn budget_holds_for_long_zones() {
        // A long zone must shrink the payload, not overrun the name.
        for zone in ["a.b", "t.example.com", &"x".repeat(40), "deep.zone.example"] {
            let max = max_data_chars(zone);
            let q = "A".repeat(max);
            let name = build_final_qname("0a1b2c3d", "11223344", "deadbeef", 9, 99, &q, zone);
            assert!(
                name.len() <= MAX_QNAME,
                "zone {zone}: qname {} > {MAX_QNAME}",
                name.len()
            );
        }
    }

    #[test]
    fn worst_case_chunk_count_is_capped() {
        let zone = "t.example.com";
        let max = max_data_chars(zone);
        let blob = "A".repeat(max * MAX_CHUNKS as usize);
        let queries = split_into_queries(&blob, max);
        assert_eq!(queries.len(), MAX_CHUNKS as usize);
    }
}
