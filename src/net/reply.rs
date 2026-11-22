//! Parsing of tunnel TXT replies (shared by the sender and the poller).

use crate::protocol::MAX_CHUNKS;
use simple_dns::rdata::RData;
use simple_dns::{Packet, PacketFlag, RCODE};

/// One queued blob, exactly as the relay stored it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobChunk {
    pub msg_id: String,
    pub seq: u32,
    pub total: u32,
    pub data: String,
}

#[derive(Debug, Clone, Default)]
pub struct Reply {
    /// `a:<msg-id>:<seq>` chunk acknowledgements.
    pub acks: Vec<(String, u32)>,
    /// `m:<msg-id>:<seq>:<total>:<data>` mailbox entries (poll or push).
    pub blobs: Vec<BlobChunk>,
    /// Relayed count for this reply, if present.
    pub cursor: Option<i64>,
    pub truncated: bool,
    pub rcode: Option<RCODE>,
}

impl Reply {
    pub fn accepted(&self, msg_id: &str, seq: u32) -> bool {
        self.acks.iter().any(|(id, s)| id == msg_id && *s == seq)
    }

    /// Group mailbox chunks by message id, keeping only complete messages.
    ///
    /// The result is in the order the relay offered them (first appearance in
    /// the reply), **not** hash order: the relay writes mailbox rows oldest
    /// first, and a caller replaying them out of order saw voice packets arrive
    /// shuffled and text pushed late.
    pub fn complete_blobs(&self) -> Vec<(String, String)> {
        use std::collections::HashMap;
        let mut order: Vec<&str> = Vec::new();
        let mut groups: HashMap<&str, Vec<&BlobChunk>> = HashMap::new();
        for b in &self.blobs {
            let entry = groups.entry(b.msg_id.as_str()).or_default();
            if entry.is_empty() {
                order.push(b.msg_id.as_str());
            }
            entry.push(b);
        }
        let mut out = Vec::with_capacity(order.len());
        for mid in order {
            let Some(chunks) = groups.get_mut(mid) else {
                continue;
            };
            chunks.sort_by_key(|c| c.seq);
            chunks.dedup_by_key(|c| c.seq);
            let total = chunks.first().map(|c| c.total).unwrap_or(0);
            if total == 0 || total as usize != chunks.len() {
                continue;
            }
            let joined: String = chunks.iter().map(|c| c.data.as_str()).collect();
            out.push((mid.to_string(), joined));
        }
        out
    }
}

/// Parse a tunnel reply packet.
pub fn parse(response: &[u8]) -> Result<Reply, String> {
    let packet = Packet::parse(response).map_err(|e| format!("reply parse: {e}"))?;
    let mut reply = Reply {
        truncated: packet.has_flags(PacketFlag::TRUNCATION),
        rcode: Some(packet.rcode()),
        ..Default::default()
    };
    for rr in &packet.answers {
        let RData::TXT(txt) = &rr.rdata else {
            continue;
        };
        for (raw, _) in txt.iter_raw() {
            let Ok(text) = std::str::from_utf8(raw) else {
                continue;
            };
            if let Some(rest) = text.strip_prefix("a:") {
                let mut it = rest.splitn(2, ':');
                if let (Some(id), Some(seq)) = (it.next(), it.next())
                    && let Ok(seq) = seq.parse::<u32>()
                {
                    reply.acks.push((id.to_string(), seq));
                }
            } else if let Some(rest) = text.strip_prefix("m:") {
                let parts: Vec<&str> = rest.splitn(4, ':').collect();
                if parts.len() != 4 {
                    continue;
                }
                let (Ok(seq), Ok(total)) = (parts[1].parse::<u32>(), parts[2].parse::<u32>())
                else {
                    continue;
                };
                if total == 0 || total > MAX_CHUNKS || seq >= total {
                    continue;
                }
                reply.blobs.push(BlobChunk {
                    msg_id: parts[0].to_string(),
                    seq,
                    total,
                    data: parts[3].to_string(),
                });
            } else if let Some(rest) = text.strip_prefix("cursor:") {
                reply.cursor = rest.parse::<i64>().ok();
            }
        }
    }
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;
    use simple_dns::rdata::TXT;
    use simple_dns::{CLASS, Name, Packet, ResourceRecord};

    fn packet_with(entries: &[&str]) -> Vec<u8> {
        let mut pkt = Packet::new_reply(7);
        let mut txt = TXT::new();
        for e in entries {
            txt.add_string(e).unwrap();
        }
        pkt.answers.push(ResourceRecord::new(
            Name::new_unchecked("m.example.com"),
            CLASS::IN,
            0,
            RData::TXT(txt),
        ));
        pkt.build_bytes_vec_compressed().unwrap()
    }

    #[test]
    fn parses_acks_blobs_and_cursor() {
        let raw = packet_with(&[
            "cursor:1",
            "a:deadbeef:0",
            "m:cafebabe:1:2:JBSW",
            "m:cafebabe:0:2:Y3DP",
        ]);
        let reply = parse(&raw).unwrap();
        assert!(reply.accepted("deadbeef", 0));
        assert!(!reply.accepted("deadbeef", 1));
        assert_eq!(reply.cursor, Some(1));
        let complete = reply.complete_blobs();
        assert_eq!(complete.len(), 1);
        assert_eq!(
            complete[0],
            ("cafebabe".to_string(), "Y3DPJBSW".to_string())
        );
    }

    #[test]
    fn complete_blobs_keep_the_order_the_relay_offered() {
        // The relay writes mailbox rows oldest first, and a client that applied
        // them in hash order saw everything shuffled: voice packets arrived out
        // of sequence and pushed text landed after later mail.
        let raw = packet_with(&[
            "m:aaaa0000:0:1:AAAA",
            "m:aaaa0001:0:1:BBBB",
            "m:aaaa0002:0:1:CCCC",
            "m:aaaa0003:0:1:DDDD",
        ]);
        let reply = parse(&raw).unwrap();
        let ids: Vec<String> = reply.complete_blobs().into_iter().map(|(m, _)| m).collect();
        assert_eq!(ids, ["aaaa0000", "aaaa0001", "aaaa0002", "aaaa0003"]);

        // And a multi-chunk blob does not reorder its neighbours.
        let raw = packet_with(&[
            "m:bbbb0001:0:1:ZZZZ",
            "m:cccc0002:1:2:IIII",
            "m:bbbb0000:0:1:YYYY",
            "m:cccc0002:0:2:HHHH",
        ]);
        let ids: Vec<String> = parse(&raw)
            .unwrap()
            .complete_blobs()
            .into_iter()
            .map(|(m, _)| m)
            .collect();
        assert_eq!(ids, ["bbbb0001", "cccc0002", "bbbb0000"]);
    }

    #[test]
    fn incomplete_or_bogus_entries_are_ignored() {
        let raw = packet_with(&[
            "m:cafebabe:0:2:AAAA",   // missing seq 1
            "m:deadbeef:5:2:AAAA",   // seq >= total
            "m:bad",                 // malformed
            "a:deadbeef:notanumber", // malformed
        ]);
        let reply = parse(&raw).unwrap();
        assert!(reply.blobs.len() == 1, "only the parseable chunk is kept");
        assert!(reply.acks.is_empty());
        assert!(reply.complete_blobs().is_empty());
    }
}
