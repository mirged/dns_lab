//! Mailbox poller: fetches queued inbound blobs from every configured relay.
//!
//! Polling is non-destructive: the relay offers blobs, and the client
//! acknowledges (`a.<tag>.<ids>`) the ones it managed to open, so a lost reply
//! costs a retry instead of the message. A node with a `relays` fallback chain
//! asks each of them, so mail parked on a secondary relay is not missed.

use super::Node;
use super::sender;
use crate::net::Egress;
use crate::protocol::{self, verb};
use std::sync::Arc;
use std::time::Duration;

/// Longest the poll interval may be stretched to after repeated failures.
const MAX_BACKOFF_SECS: u64 = 60;
/// How many message ids to acknowledge in one query. `build_ack_qname` trims
/// the list further when the zone is long.
const ACK_BATCH: usize = 8;

pub fn spawn(node: Arc<Node>) {
    std::thread::spawn(move || {
        let mut failures: u64 = 0;
        loop {
            if node.stopped() {
                return;
            }
            let base = node.cfg.poll_interval_secs.max(1);
            // Exponential backoff (capped) so a dead relay is not hammered.
            let wait = if failures == 0 {
                base
            } else {
                (base * (1u64 << failures.min(4))).min(MAX_BACKOFF_SECS)
            };
            std::thread::sleep(Duration::from_secs(wait));
            match poll_once(&node) {
                Ok(()) => failures = 0,
                Err(e) => {
                    failures = failures.saturating_add(1);
                    node.log(format!("[!] poll failed ({failures}x): {e}"));
                }
            }
        }
    });
}

fn poll_once(node: &Arc<Node>) -> Result<(), String> {
    let tag = node
        .tag
        .clone()
        .ok_or("no `shared_key` configured: cannot derive a mailbox tag")?;
    let candidates = node.egress_candidates(None);
    if candidates.is_empty() {
        return Err("no relay configured to poll".to_string());
    }

    let mut reached = 0usize;
    let mut last_error = String::new();
    for egress in candidates {
        match poll_egress(node, egress, &tag) {
            Ok(()) => reached += 1,
            Err(e) => {
                node.log(format!("[!] poll via {egress} failed: {e}"));
                last_error = e;
            }
        }
    }
    if reached == 0 {
        return Err(last_error);
    }
    Ok(())
}

/// One poll against one relay: fetch, ingest and acknowledge.
fn poll_egress(node: &Arc<Node>, egress: Egress, tag: &str) -> Result<(), String> {
    let qname = protocol::build_qname(verb::POLL, tag, "", 0, 0, "", &node.cfg.zone);
    let reply = sender::exchange(node, egress, &qname)?;

    let blobs = reply.complete_blobs();
    if blobs.is_empty() {
        return Ok(());
    }

    let mut opened: Vec<String> = Vec::new();
    for (mid, blob) in blobs {
        match node.open_blob(&mid, &blob) {
            Ok(env) => match node.ingest(&mid, &env) {
                Ok(_) => opened.push(mid),
                Err(e) => node.log(format!("[!] polled ingest {mid} failed: {e}")),
            },
            Err(e) => node.log(format!("[!] polled blob {mid} could not be opened: {e}")),
        }
    }

    // Acknowledge everything we opened (stored, duplicate or receipt): the
    // relay can drop it, and we will not re-download it next time.
    for batch in opened.chunks(ACK_BATCH) {
        let ack = protocol::build_ack_qname(tag, batch, &node.cfg.zone);
        if let Err(e) = sender::exchange(node, egress, &ack) {
            node.log(format!("[~] mailbox ack via {egress} failed: {e}"));
            break;
        }
    }
    Ok(())
}
