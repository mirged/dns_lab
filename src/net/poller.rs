//! Mailbox poller: fetches queued inbound messages from a relay via DNS.

use super::Node;
use super::sender;
use crate::protocol::{self, verb};
use simple_dns::Packet;
use simple_dns::rdata::RData;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

pub fn spawn(node: Arc<Node>) {
    std::thread::spawn(move || {
        loop {
            if node.stopped() {
                return;
            }
            std::thread::sleep(Duration::from_secs(node.cfg.poll_interval_secs.max(1)));
            if let Err(e) = poll_once(&node) {
                node.log(format!("[!] poll failed: {e}"));
            }
            node.db.gc_chunks(300);
        }
    });
}

fn poll_once(node: &Node) -> Result<(), String> {
    let egress = node.egress_for(None)?;
    let qname = protocol::build_qname(verb::POLL, &node.cfg.node_id, 0, 0, "", &node.cfg.zone);
    let resp = sender::exchange(node, egress, &qname)?;
    let pkt = Packet::parse(&resp).map_err(|e| format!("poll response parse: {e}"))?;

    // msg_id -> (seq, total, chunk)
    let mut groups: HashMap<String, Vec<(u32, u32, String)>> = HashMap::new();
    for rr in &pkt.answers {
        if let RData::TXT(txt) = &rr.rdata {
            for (k, _v) in txt.iter_raw() {
                let s = String::from_utf8_lossy(k);
                if let Some(rest) = s.strip_prefix("m:") {
                    let parts: Vec<&str> = rest.splitn(4, ':').collect();
                    if parts.len() == 4 {
                        let seq = parts[1].parse().unwrap_or(0);
                        let total = parts[2].parse().unwrap_or(1);
                        groups.entry(parts[0].to_string()).or_default().push((
                            seq,
                            total,
                            parts[3].to_string(),
                        ));
                    }
                }
            }
        }
    }

    for (mid, mut chunks) in groups {
        chunks.sort_by_key(|c| c.0);
        let total = chunks.first().map(|c| c.1).unwrap_or(0);
        if total == 0 || chunks.len() != total as usize {
            continue;
        }
        let concat: String = chunks.iter().map(|c| c.2.clone()).collect();
        match protocol::decode_blob(&node.key, &concat) {
            Ok(env) => {
                let peer_name = node
                    .db
                    .peer(&env.from)
                    .map(|p| p.name)
                    .unwrap_or_else(|| env.from.clone());
                if let Ok(Some(_)) = node
                    .db
                    .insert_message(&mid, "in", &env.from, &peer_name, &env.body, "received")
                {
                    node.db.touch_peer(&env.from);
                    node.log(format!(
                        "[<] polled message {mid} from {}: {:?}",
                        env.from, env.body
                    ));
                    node.notify_message();
                }
            }
            Err(e) => node.log(format!("[!] polled blob decode failed for {mid}: {e}")),
        }
    }
    Ok(())
}
