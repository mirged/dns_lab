//! Wire protocol: message envelope, encryption pipeline, and DNS QNAME framing.

use crate::{decrypt_with_key, encrypt_with_key, smaz};
use data_encoding::BASE32_NOPAD;

/// Protocol version carried inside the encrypted envelope.
pub const VERSION: u8 = 2;

/// Practical maximum presentation length of a DNS name.
pub const MAX_QNAME: usize = 253;
/// Maximum length of a single DNS label.
pub const MAX_LABEL: usize = 63;

/// Tunnel verbs.
pub mod verb {
    /// A message data chunk.
    pub const MSG: char = 'm';
    /// Poll the relay mailbox for inbound messages.
    pub const POLL: char = 'p';
    /// Acknowledge a chunk (used in response text).
    pub const ACK: char = 'a';
}

/// Application-level message kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgType {
    Message = 1,
    Ack = 2,
    Read = 3,
    Heartbeat = 4,
}

impl MsgType {
    fn from_u8(v: u8) -> Option<MsgType> {
        match v {
            1 => Some(MsgType::Message),
            2 => Some(MsgType::Ack),
            3 => Some(MsgType::Read),
            4 => Some(MsgType::Heartbeat),
            _ => None,
        }
    }
}

/// Decrypted application envelope.
#[derive(Debug, Clone)]
pub struct Envelope {
    pub msg_type: MsgType,
    pub from: String,
    pub to: String,
    pub body: String,
    pub ts: u64,
}

impl Envelope {
    pub fn new(msg_type: MsgType, from: &str, to: &str, body: &str) -> Self {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Envelope {
            msg_type,
            from: from.to_string(),
            to: to.to_string(),
            body: body.to_string(),
            ts,
        }
    }

    /// Serialize (with the body smaz-compressed) into raw bytes.
    pub fn encode(&self) -> Vec<u8> {
        let body_comp = smaz::compress(&self.body);
        let from = self.from.as_bytes();
        let to = self.to.as_bytes();

        let mut out = Vec::with_capacity(24 + from.len() + to.len() + body_comp.len());
        out.push(VERSION);
        out.push(self.msg_type as u8);
        out.extend_from_slice(&self.ts.to_le_bytes());
        out.push(from.len().min(255) as u8);
        out.extend_from_slice(&from[..from.len().min(255)]);
        out.push(to.len().min(255) as u8);
        out.extend_from_slice(&to[..to.len().min(255)]);
        out.extend_from_slice(&(body_comp.len() as u32).to_le_bytes());
        out.extend_from_slice(&body_comp);
        out
    }

    /// Parse raw bytes back into an envelope.
    pub fn decode(data: &[u8]) -> Result<Envelope, String> {
        if data.len() < 2 {
            return Err("envelope too short".into());
        }
        let version = data[0];
        if version != VERSION {
            return Err(format!("unsupported protocol version {version}"));
        }
        let msg_type = MsgType::from_u8(data[1]).ok_or("invalid message type")?;
        let mut pos = 2;
        if data.len() < pos + 8 {
            return Err("envelope truncated at timestamp".into());
        }
        let ts = u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap());
        pos += 8;

        let from_len = *data.get(pos).ok_or("truncated from_len")? as usize;
        pos += 1;
        let from = read_str(data, &mut pos, from_len)?;

        let to_len = *data.get(pos).ok_or("truncated to_len")? as usize;
        pos += 1;
        let to = read_str(data, &mut pos, to_len)?;

        if data.len() < pos + 4 {
            return Err("envelope truncated at body length".into());
        }
        let body_len = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;
        if data.len() < pos + body_len {
            return Err("envelope truncated at body".into());
        }
        let body = smaz::decompress(&data[pos..pos + body_len])?;

        Ok(Envelope {
            msg_type,
            from,
            to,
            body,
            ts,
        })
    }
}

fn read_str(data: &[u8], pos: &mut usize, len: usize) -> Result<String, String> {
    if data.len() < *pos + len {
        return Err("truncated string field".into());
    }
    let s = std::str::from_utf8(&data[*pos..*pos + len])
        .map_err(|_| "invalid utf-8 in envelope")?
        .to_string();
    *pos += len;
    Ok(s)
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

/// Encode an envelope into a Base32 blob ready for chunking.
pub fn encode_blob(key: &[u8; 32], env: &Envelope) -> Result<String, String> {
    let raw = env.encode();
    let cipher = encrypt_with_key(key, &raw)?;
    Ok(base32_encode(&cipher))
}

/// Decode a Base32 blob back into an envelope.
pub fn decode_blob(key: &[u8; 32], blob: &str) -> Result<Envelope, String> {
    let cipher = base32_decode(blob)?;
    let raw = decrypt_with_key(key, &cipher)?;
    Envelope::decode(&raw)
}

/// Maximum number of Base32 chars that fit in one tunnel QNAME for this zone.
pub fn max_data_chars(zone: &str) -> usize {
    let zone_len = zone.trim_matches('.').len();
    // data_chars + num_labels <= 234 - zone_len  (derived from MAX_QNAME)
    let budget = 234usize.saturating_sub(zone_len);
    let mut best = 0;
    for labels in 1..=8usize {
        if labels > budget {
            break;
        }
        let c = (MAX_LABEL * labels).min(budget - labels);
        if c > best {
            best = c;
        }
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

/// Build a tunnel QNAME.
pub fn build_qname(
    verb: char,
    msg_id: &str,
    seq: u32,
    total: u32,
    data: &str,
    zone: &str,
) -> String {
    let zone = zone.trim_matches('.');
    if data.is_empty() {
        format!("{verb}.{msg_id}.{seq}.{total}.{zone}")
    } else {
        format!("{verb}.{msg_id}.{seq}.{total}.{data}.{zone}")
    }
}

/// A parsed tunnel query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelQuery {
    pub verb: char,
    pub msg_id: String,
    pub seq: u32,
    pub total: u32,
    /// Raw Base32 data exactly as carried in the QNAME (may be empty).
    pub data: String,
}

/// Parse a QNAME into a tunnel query if it lives under `zone`.
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
    if parts.len() < 4 {
        return None;
    }
    let verb = parts[0].chars().next()?;
    let msg_id = parts[1].to_string();
    let seq = parts[2].parse::<u32>().ok()?;
    let total = parts[3].parse::<u32>().ok()?;
    let data = parts[4..].join("");

    Some(TunnelQuery {
        verb,
        msg_id,
        seq,
        total,
        data,
    })
}

/// The canonical apex name for a zone (no trailing dot).
pub fn zone_apex(zone: &str) -> String {
    zone.trim_matches('.').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DEFAULT_KEY;

    #[test]
    fn envelope_roundtrip() {
        let env = Envelope::new(MsgType::Message, "aaaa1111", "bbbb2222", "hello dns world");
        let raw = env.encode();
        let back = Envelope::decode(&raw).unwrap();
        assert_eq!(back.body, "hello dns world");
        assert_eq!(back.from, "aaaa1111");
        assert_eq!(back.to, "bbbb2222");
    }

    #[test]
    fn blob_roundtrip() {
        let env = Envelope::new(
            MsgType::Message,
            "deadbeef",
            "cafebabe",
            "completely on dns",
        );
        let blob = encode_blob(DEFAULT_KEY, &env).unwrap();
        let back = decode_blob(DEFAULT_KEY, &blob).unwrap();
        assert_eq!(back.body, "completely on dns");
    }

    #[test]
    fn qname_build_parse() {
        let zone = "t.example.com";
        let q = build_qname('m', "deadbeef", 1, 3, "JBSWY3DP", zone);
        let parsed = parse_qname(zone, &q).unwrap();
        assert_eq!(parsed.verb, 'm');
        assert_eq!(parsed.msg_id, "deadbeef");
        assert_eq!(parsed.seq, 1);
        assert_eq!(parsed.total, 3);
        assert_eq!(parsed.data, "jbswy3dp");
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
            let name = build_qname('m', "deadbeef", i as u32, queries.len() as u32, q, zone);
            assert!(name.len() <= MAX_QNAME, "qname too long: {}", name.len());
            assert!(q.split('.').all(|l| l.len() <= MAX_LABEL));
        }
    }
}
