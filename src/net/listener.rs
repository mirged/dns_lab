//! Authoritative UDP + TCP tunnel listener: chunk reassembly, child relay and
//! mailbox.

use super::Node;
use crate::protocol::{self, verb};
use simple_dns::rdata::{OPT, RData, SOA, TXT};
use simple_dns::{CLASS, Name, Packet, PacketFlag, RCODE, ResourceRecord};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

/// Cap on simultaneously served TCP connections; each one gets a thread.
const MAX_TCP_CONNS: usize = 64;
/// Upper bound on the TXT payload of one reply (65 535 is the DNS/TCP ceiling).
const MAX_REPLY_TXT_BYTES: usize = 55_000;

static TCP_CONNS: AtomicUsize = AtomicUsize::new(0);

/// Per-source token bucket, so a single host cannot saturate the relay.
struct RateLimiter {
    /// Tokens per second (0 = disabled).
    rate: f64,
    burst: f64,
    buckets: Mutex<HashMap<IpAddr, (f64, Instant)>>,
}

impl RateLimiter {
    fn new(rate: u32) -> RateLimiter {
        RateLimiter {
            rate: rate as f64,
            burst: (rate as f64 * 2.0).max(1.0),
            buckets: Mutex::new(HashMap::new()),
        }
    }

    fn allow(&self, ip: IpAddr) -> bool {
        if self.rate <= 0.0 {
            return true;
        }
        let now = Instant::now();
        let mut buckets = match self.buckets.lock() {
            Ok(b) => b,
            Err(_) => return true,
        };
        // Keep the map from growing without bound under a spoofed-source flood.
        if buckets.len() > 4096 {
            buckets.retain(|_, (_, last)| now.duration_since(*last).as_secs() < 60);
        }
        let (tokens, last) = buckets.entry(ip).or_insert((self.burst, now));
        let elapsed = now.duration_since(*last).as_secs_f64();
        *tokens = (*tokens + elapsed * self.rate).min(self.burst);
        *last = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Lab hook (`DNS_LAB_DELAY_MS`): answer each query after a delay, modelled as
/// a separate thread so a pipelined client really does see RTT parallelism.
/// Used by `scripts/bench.sh`; unset in normal operation.
fn reply_delay_ms() -> Option<u64> {
    std::env::var("DNS_LAB_DELAY_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
}

/// Bind UDP + TCP and serve tunnel queries in background threads.
pub fn spawn(node: Arc<Node>) {
    let addr = node.cfg.listen_addr.clone();
    let limiter = Arc::new(RateLimiter::new(node.cfg.rate_limit_per_sec));

    let node_udp = node.clone();
    let addr_udp = addr.clone();
    let limiter_udp = limiter.clone();
    std::thread::spawn(move || match UdpSocket::bind(&addr_udp) {
        Ok(sock) => {
            node_udp.log(format!("[+] DNS/UDP listener bound on {addr_udp}"));
            serve_udp(node_udp, sock, limiter_udp);
        }
        Err(e) => node_udp.log(format!("[!] UDP bind {addr_udp} failed: {e}")),
    });

    std::thread::spawn(move || match TcpListener::bind(&addr) {
        Ok(listener) => {
            node.log(format!("[+] DNS/TCP listener bound on {addr}"));
            for stream in listener.incoming().flatten() {
                if TCP_CONNS.load(Ordering::SeqCst) >= MAX_TCP_CONNS {
                    node.log(format!(
                        "[!] dropping TCP connection: {MAX_TCP_CONNS} already in flight"
                    ));
                    continue;
                }
                TCP_CONNS.fetch_add(1, Ordering::SeqCst);
                let node = node.clone();
                let limiter = limiter.clone();
                std::thread::spawn(move || {
                    serve_tcp_conn(node, stream, limiter);
                    TCP_CONNS.fetch_sub(1, Ordering::SeqCst);
                });
            }
        }
        Err(e) => node.log(format!("[!] TCP bind {addr} failed: {e}")),
    });
}

fn serve_udp(node: Arc<Node>, sock: UdpSocket, limiter: Arc<RateLimiter>) {
    let mut buf = vec![0u8; 4096];
    loop {
        if node.stopped() {
            return;
        }
        let (n, src) = match sock.recv_from(&mut buf) {
            Ok(v) => v,
            Err(e) => {
                node.log(format!("[!] UDP recv error: {e}"));
                continue;
            }
        };
        if !limiter.allow(src.ip()) {
            continue; // silently drop: DNS has no backpressure channel
        }
        if let Some(resp) = handle_datagram(&node, &buf[..n], false) {
            match (reply_delay_ms(), sock.try_clone()) {
                (Some(ms), Ok(clone)) => {
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(ms));
                        let _ = clone.send_to(&resp, src);
                    });
                }
                _ => {
                    let _ = sock.send_to(&resp, src);
                }
            }
        }
    }
}

fn serve_tcp_conn(node: Arc<Node>, mut stream: TcpStream, limiter: Arc<RateLimiter>) {
    if let Ok(peer) = stream.peer_addr()
        && !limiter.allow(peer.ip())
    {
        return;
    }
    let mut lb = [0u8; 2];
    if stream.read_exact(&mut lb).is_err() {
        return;
    }
    let n = u16::from_be_bytes(lb) as usize;
    let mut buf = vec![0u8; n];
    if stream.read_exact(&mut buf).is_err() {
        return;
    }
    if let Some(resp) = handle_datagram(&node, &buf, true) {
        if resp.len() > u16::MAX as usize {
            node.log(format!(
                "[!] dropping oversized TCP reply ({} bytes)",
                resp.len()
            ));
            return;
        }
        if let Some(ms) = reply_delay_ms() {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
        let len = (resp.len() as u16).to_be_bytes();
        let _ = stream.write_all(&len);
        let _ = stream.write_all(&resp);
    }
}

fn handle_datagram(node: &Arc<Node>, bytes: &[u8], tcp: bool) -> Option<Vec<u8>> {
    let request = Packet::parse(bytes).ok()?;
    let question = request.questions.first()?;
    let qname = question.qname.to_string();

    let zone = node.cfg.zone.trim_matches('.').to_string();
    let qlow = qname.trim_end_matches('.').to_ascii_lowercase();

    // Zone apex: real SOA so it behaves like a proper authoritative zone.
    if qlow == zone.to_ascii_lowercase() {
        if node.cfg.authoritative {
            return Some(build_soa_reply(node, &request, tcp));
        }
        return None;
    }

    let parsed = match protocol::parse_qname(&node.cfg.zone, &qname) {
        Some(p) => p,
        None => {
            return if node.cfg.authoritative {
                Some(build_reply(node, &request, &[], RCODE::NameError, tcp).0)
            } else {
                None
            };
        }
    };

    match parsed.verb {
        verb::MSG | verb::FINAL => {
            // Relaying is decided on the QNAME alone: if this is a relay and the
            // tag is not ours, park the blob and never touch the key.
            let mine = node.tag.as_deref();
            if node.is_relay() && mine != Some(parsed.tag.as_str()) {
                relay_chunk(node, &parsed);
            } else {
                handle_message_chunk(node, &parsed, qname.len());
            }

            let mut txt = vec![format!("a:{}:{}", parsed.msg_id, parsed.seq)];
            // Push: the final chunk tells us who sent it, so we can hand that
            // sender anything queued for them in the same reply.
            if let Some(sender_tag) = parsed.sender_tag.as_deref()
                && node.is_relay()
            {
                let (entries, _ids) = mailbox_entries(node, sender_tag, 2, MAX_REPLY_TXT_BYTES / 4);
                if !entries.is_empty() {
                    txt.extend(entries);
                }
            }
            let (resp, _) = build_reply(node, &request, &txt, RCODE::NoError, tcp);
            Some(resp)
        }
        verb::POLL => {
            // Non-destructive: the client acknowledges what it stored with an
            // `a` query, so a lost reply costs a retry instead of the mail.
            let (txt, ids) = mailbox_entries(node, &parsed.tag, 8, MAX_REPLY_TXT_BYTES);
            let (resp, truncated) = build_reply(node, &request, &txt, RCODE::NoError, tcp);
            if !ids.is_empty() && !truncated {
                node.log(format!(
                    "[<] poll for tag {}: {} message(s) offered",
                    parsed.tag,
                    ids.len()
                ));
            }
            Some(resp)
        }
        verb::ACK => {
            node.db.mailbox_ack(&parsed.tag, &parsed.acked);
            if node.cfg.verbose {
                node.log(format!(
                    "[✓] tag {} acknowledged {} message(s)",
                    parsed.tag,
                    parsed.acked.len()
                ));
            }
            let (resp, _) = build_reply(node, &request, &[], RCODE::NoError, tcp);
            Some(resp)
        }
        _ => {
            let (resp, _) = build_reply(node, &request, &[], RCODE::NoError, tcp);
            Some(resp)
        }
    }
}

/// Buffer one chunk for someone else's inbox without decrypting anything.
///
/// The relay only ever concatenates the Base32 pieces in order: it has no key,
/// no envelope and no idea who is talking to whom.
fn relay_chunk(node: &Arc<Node>, parsed: &protocol::TunnelQuery) {
    if parsed.data.is_empty() {
        return;
    }
    match node.db.relay_store(
        crate::db::RelayChunk {
            tag: &parsed.tag,
            msg_id: &parsed.msg_id,
            seq: parsed.seq,
            total: parsed.total,
            data: parsed.data.as_bytes(),
        },
        crate::db::Quota {
            max_blobs: node.cfg.max_blobs_per_route,
            max_bytes: node.cfg.max_bytes_per_route,
        },
    ) {
        Ok(crate::db::RelayStore::Buffered) => {}
        Ok(crate::db::RelayStore::Stored { chunks, blob_chars }) => {
            if node.cfg.verbose {
                node.log(format!(
                    "[>] relayed {} for tag {} ({chunks} chunk(s), {blob_chars} blob chars)",
                    parsed.msg_id, parsed.tag
                ));
            }
        }
        Ok(crate::db::RelayStore::Quota { blobs, bytes }) => node.log(format!(
            "[!] quota hit for tag {} ({blobs} blobs, {bytes} bytes): dropped {}",
            parsed.tag, parsed.msg_id
        )),
        Err(e) => node.log(format!("[!] relay buffer failed: {e}")),
    }
}

fn handle_message_chunk(node: &Arc<Node>, parsed: &protocol::TunnelQuery, qname_len: usize) {
    if node.key.is_none() {
        node.log("[!] chunk dropped: no `shared_key` configured on this node");
        return;
    }
    if let Some(mine) = node.tag.as_deref()
        && mine != parsed.tag
    {
        node.log(format!(
            "[!] chunk dropped: tagged {} but we answer {}",
            parsed.tag, mine
        ));
        return;
    }
    let data = parsed.data.as_bytes();
    if data.is_empty() {
        node.log(format!(
            "[!] empty chunk {}/{}",
            parsed.seq + 1,
            parsed.total
        ));
        return;
    }
    if let Err(e) = node
        .db
        .add_chunk(&parsed.tag, &parsed.msg_id, parsed.seq, parsed.total, data)
    {
        node.log(format!("[!] chunk store failed: {e}"));
        return;
    }

    let parts = match node.db.try_assemble(&parsed.tag, &parsed.msg_id) {
        Some(p) => p,
        None => return,
    };

    let mut concat = String::new();
    for p in &parts {
        concat.push_str(&String::from_utf8_lossy(p));
    }

    if node.cfg.verbose {
        node.log(format!(
            "[*] reassembled {} ({} chunks, {}-char qname)",
            parsed.msg_id,
            parts.len(),
            qname_len
        ));
    }

    match node.open_blob(&parsed.msg_id, &concat) {
        Ok(env) => {
            if let Err(e) = node.ingest(&parsed.msg_id, &env) {
                node.log(format!("[!] ingest {} failed: {e}", parsed.msg_id));
            }
        }
        Err(e) => node.log(format!(
            "[!] decode failed for {} ({} chunks): {e}",
            parsed.msg_id,
            parts.len()
        )),
    }
    node.db.delete_chunks(&parsed.tag, &parsed.msg_id);
}

/// Build mailbox TXT entries for `route` within a byte budget.
///
/// Messages are added whole: once one no longer fits, it and everything after
/// it wait for the next poll, so a blob is never split across replies.
fn mailbox_entries(
    node: &Node,
    route: &str,
    max_entries: i64,
    budget: usize,
) -> (Vec<String>, Vec<i64>) {
    let max = protocol::max_data_chars(&node.cfg.zone).min(200);
    let rows = node.db.mailbox_peek(route, max_entries);

    let mut txt = Vec::new();
    let mut ids = Vec::new();
    let mut left = budget;
    for (id, mid, blob) in rows {
        let segs = protocol::split_into_queries(&blob, max);
        let total = segs.len();
        let entries: Vec<String> = segs
            .iter()
            .enumerate()
            .map(|(i, seg)| format!("m:{mid}:{i}:{total}:{seg}"))
            .collect();
        // +1 per entry for its TXT character-string length prefix.
        let cost: usize = entries.iter().map(|e| e.len() + 1).sum();
        if cost > left {
            break;
        }
        left -= cost;
        txt.extend(entries);
        ids.push(id);
    }
    (txt, ids)
}

fn apply_opt(node: &Node, request: &Packet, reply: &mut Packet) {
    if request.opt().is_some() {
        *reply.opt_mut() = Some(OPT {
            opt_codes: Vec::new(),
            udp_packet_size: node.cfg.edns_udp_size,
            version: 0,
        });
    }
}

fn udp_limit(request: &Packet) -> usize {
    request
        .opt()
        .map(|o| o.udp_packet_size as usize)
        .unwrap_or(512)
        .max(512)
}

/// Build a reply. The bool is `true` when the reply was truncated to fit the
/// client's UDP payload size (so the caller knows the payload was not delivered).
fn build_reply(
    node: &Node,
    request: &Packet,
    txt: &[String],
    rcode: RCODE,
    tcp: bool,
) -> (Vec<u8>, bool) {
    let mut reply = Packet::new_reply(request.id());
    reply.set_flags(PacketFlag::AUTHORITATIVE_ANSWER);
    if request.has_flags(PacketFlag::RECURSION_DESIRED) {
        reply.set_flags(PacketFlag::RECURSION_DESIRED);
    }
    if let Some(q) = request.questions.first() {
        reply.questions.push(q.clone());
    }
    *reply.rcode_mut() = rcode;

    if !txt.is_empty()
        && let Some(q) = request.questions.first()
    {
        let mut record = TXT::new();
        for s in txt {
            let _ = record.add_string(s);
        }
        reply.answers.push(ResourceRecord::new(
            q.qname.clone(),
            CLASS::IN,
            0,
            RData::TXT(record),
        ));
    }
    apply_opt(node, request, &mut reply);

    let bytes = reply.build_bytes_vec_compressed().unwrap_or_default();
    if !tcp && bytes.len() > udp_limit(request) {
        let mut t = Packet::new_reply(request.id());
        t.set_flags(PacketFlag::AUTHORITATIVE_ANSWER);
        t.set_flags(PacketFlag::TRUNCATION);
        if let Some(q) = request.questions.first() {
            t.questions.push(q.clone());
        }
        apply_opt(node, request, &mut t);
        return (t.build_bytes_vec_compressed().unwrap_or_default(), true);
    }
    (bytes, false)
}

fn build_soa_reply(node: &Node, request: &Packet, tcp: bool) -> Vec<u8> {
    let zone = node.cfg.zone.trim_matches('.').to_string();
    let mname_s = format!("ns1.{zone}");
    let rname_s = format!("hostmaster.{zone}");

    let mname = Name::new_unchecked(&mname_s);
    let rname = Name::new_unchecked(&rname_s);
    let apex = Name::new_unchecked(&zone);

    let soa = SOA {
        mname,
        rname,
        serial: 1,
        refresh: 3600,
        retry: 600,
        expire: 86400,
        minimum: 60,
    };

    let mut reply = Packet::new_reply(request.id());
    reply.set_flags(PacketFlag::AUTHORITATIVE_ANSWER);
    if request.has_flags(PacketFlag::RECURSION_DESIRED) {
        reply.set_flags(PacketFlag::RECURSION_DESIRED);
    }
    if let Some(q) = request.questions.first() {
        reply.questions.push(q.clone());
    }
    reply
        .name_servers
        .push(ResourceRecord::new(apex, CLASS::IN, 60, RData::SOA(soa)));
    apply_opt(node, request, &mut reply);

    let bytes = reply.build_bytes_vec_compressed().unwrap_or_default();
    if !tcp && bytes.len() > udp_limit(request) {
        let mut t = Packet::new_reply(request.id());
        t.set_flags(PacketFlag::AUTHORITATIVE_ANSWER);
        t.set_flags(PacketFlag::TRUNCATION);
        if let Some(q) = request.questions.first() {
            t.questions.push(q.clone());
        }
        apply_opt(node, request, &mut t);
        return t.build_bytes_vec_compressed().unwrap_or_default();
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, RelayMode};
    use crate::db::Db;

    fn node() -> Arc<Node> {
        let db = Arc::new(Db::open_in_memory().unwrap());
        let cfg = Config {
            node_id: "rrrr9999".to_string(),
            zone: "lab.local".to_string(),
            relay_mode: RelayMode::Authoritative,
            shared_key: String::new(),
            rate_limit_per_sec: 0,
            ..Config::default()
        };
        let (tx, _rx) = std::sync::mpsc::channel();
        Node::new(cfg, db, tx).unwrap()
    }

    #[test]
    fn rate_limiter_bursts_then_throttles() {
        let limiter = RateLimiter::new(2);
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        // Burst is 2x the rate.
        assert!(limiter.allow(ip));
        assert!(limiter.allow(ip));
        assert!(limiter.allow(ip));
        assert!(limiter.allow(ip));
        assert!(!limiter.allow(ip), "fifth query in the same instant");

        let off = RateLimiter::new(0);
        for _ in 0..1000 {
            assert!(off.allow(ip), "0 disables the limiter");
        }
    }

    #[test]
    fn mailbox_entries_respect_the_budget() {
        let n = node();
        n.db.mailbox_add("0a1b2c3d", "m1", &"A".repeat(60_000))
            .unwrap();
        let (entries, ids) = mailbox_entries(&n, "0a1b2c3d", 8, 1024);
        assert!(entries.is_empty(), "a 60 KB blob cannot fit 1 KB");
        assert!(ids.is_empty());
    }

    #[test]
    fn a_poll_does_not_consume_the_mailbox() {
        let n = node();
        n.db.mailbox_add("0a1b2c3d", "m1", "AAAA").unwrap();
        let (entries, ids) = mailbox_entries(&n, "0a1b2c3d", 8, MAX_REPLY_TXT_BYTES);
        assert_eq!(entries.len(), 1);
        assert_eq!(ids.len(), 1);
        // Peeking again returns it: only an explicit ack removes it.
        assert_eq!(n.db.mailbox_pending("0a1b2c3d"), 1);
        n.db.mailbox_ack("0a1b2c3d", &["m1".to_string()]);
        assert_eq!(n.db.mailbox_pending("0a1b2c3d"), 0);
    }
}
