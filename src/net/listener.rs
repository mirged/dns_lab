//! Authoritative UDP + TCP tunnel listener with chunk reassembly and relay mailbox.

use super::Node;
use crate::protocol::{self, verb};
use simple_dns::rdata::{OPT, RData, SOA, TXT};
use simple_dns::{CLASS, Name, Packet, PacketFlag, RCODE, ResourceRecord};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::sync::Arc;

/// Bind UDP + TCP and serve tunnel queries in background threads.
pub fn spawn(node: Arc<Node>) {
    let addr = node.cfg.listen_addr.clone();

    let node_udp = node.clone();
    let addr_udp = addr.clone();
    std::thread::spawn(move || match UdpSocket::bind(&addr_udp) {
        Ok(sock) => {
            node_udp.log(format!("[+] DNS/UDP listener bound on {addr_udp}"));
            serve_udp(node_udp, sock);
        }
        Err(e) => node_udp.log(format!("[!] UDP bind {addr_udp} failed: {e}")),
    });

    std::thread::spawn(move || match TcpListener::bind(&addr) {
        Ok(listener) => {
            node.log(format!("[+] DNS/TCP listener bound on {addr}"));
            for stream in listener.incoming().flatten() {
                let node = node.clone();
                std::thread::spawn(move || serve_tcp_conn(node, stream));
            }
        }
        Err(e) => node.log(format!("[!] TCP bind {addr} failed: {e}")),
    });
}

fn serve_udp(node: Arc<Node>, sock: UdpSocket) {
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
        if let Some(resp) = handle_datagram(&node, &buf[..n], false) {
            let _ = sock.send_to(&resp, src);
        }
    }
}

fn serve_tcp_conn(node: Arc<Node>, mut stream: TcpStream) {
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
        let len = (resp.len() as u16).to_be_bytes();
        let _ = stream.write_all(&len);
        let _ = stream.write_all(&resp);
    }
}

fn handle_datagram(node: &Node, bytes: &[u8], tcp: bool) -> Option<Vec<u8>> {
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
        verb::MSG => {
            node.log(format!(
                "[*] chunk {}/{} msg {} from query ({}-char qname)",
                parsed.seq + 1,
                parsed.total,
                parsed.msg_id,
                qname.len()
            ));
            handle_chunk(node, &parsed);
            let (resp, _) = build_reply(
                node,
                &request,
                &[format!("a:{}:{}", parsed.msg_id, parsed.seq)],
                RCODE::NoError,
                tcp,
            );
            Some(resp)
        }
        verb::POLL => {
            let (txt, ids) = handle_poll(node, &parsed);
            let (resp, truncated) = build_reply(node, &request, &txt, RCODE::NoError, tcp);
            // Only consume the mailbox once the reply actually goes out. A reply
            // truncated over UDP is retried over TCP, which re-reads the same
            // still-undelivered entries instead of losing them.
            if !ids.is_empty() {
                if truncated {
                    node.log(format!(
                        "[<] poll from {}: reply truncated, will retry over TCP",
                        parsed.msg_id
                    ));
                } else {
                    node.log(format!(
                        "[<] poll from {}: {} message(s) delivered",
                        parsed.msg_id,
                        ids.len()
                    ));
                }
            }
            if !truncated {
                node.db.mailbox_mark_delivered(&ids);
            }
            Some(resp)
        }
        _ => {
            let (resp, _) = build_reply(node, &request, &[], RCODE::NoError, tcp);
            Some(resp)
        }
    }
}

fn handle_chunk(node: &Node, parsed: &protocol::TunnelQuery) {
    let data = parsed.data.as_bytes();
    if let Err(e) = node
        .db
        .add_chunk(&parsed.msg_id, parsed.seq, parsed.total, data)
    {
        node.log(format!("[!] chunk store failed: {e}"));
        return;
    }

    let parts = match node.db.try_assemble(&parsed.msg_id) {
        Some(p) => p,
        None => return,
    };

    let mut concat = String::new();
    for p in &parts {
        concat.push_str(&String::from_utf8_lossy(p));
    }

    match protocol::decode_blob(&node.key, &concat) {
        Ok(env) => {
            let peer_name = node
                .db
                .peer(&env.from)
                .map(|p| p.name)
                .unwrap_or_else(|| env.from.clone());
            match node.db.insert_message(
                &parsed.msg_id,
                "in",
                &env.from,
                &peer_name,
                &env.body,
                "received",
            ) {
                Ok(Some(_)) => {
                    node.db.touch_peer(&env.from);
                    if node.cfg.authoritative || node.cfg.relay.is_some() {
                        let _ = node.db.mailbox_add(&env.to, &parsed.msg_id, &concat);
                    }
                    node.log(format!(
                        "[<] message {} from {} -> {} ({} bytes): {:?}",
                        parsed.msg_id,
                        env.from,
                        env.to,
                        env.body.len(),
                        env.body
                    ));
                    node.notify_message();
                }
                Ok(None) => {}
                Err(e) => node.log(format!("[!] store message failed: {e}")),
            }
        }
        Err(e) => node.log(format!(
            "[!] reassembly decode failed for {}: {e}",
            parsed.msg_id
        )),
    }
    node.db.delete_chunks(&parsed.msg_id);
}

/// Upper bound on the TXT payload of one poll reply. Keeps the message below the
/// 65535-byte DNS/TCP limit even before name compression and record overhead.
const MAX_POLL_TXT_BYTES: usize = 55_000;

/// Build mailbox TXT entries, returning them with the row ids they came from.
///
/// Messages are added whole; once one no longer fits in the reply budget it and
/// everything after it are left for a later poll so no blob is split across
/// replies.
fn handle_poll(node: &Node, parsed: &protocol::TunnelQuery) -> (Vec<String>, Vec<i64>) {
    let requester = parsed.msg_id.clone();
    let max = protocol::max_data_chars(&node.cfg.zone).min(200);
    let rows = node.db.mailbox_peek(&requester, 8);

    let mut txt = Vec::new();
    let mut ids = Vec::new();
    let mut budget = MAX_POLL_TXT_BYTES;
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
        if cost > budget {
            break;
        }
        budget -= cost;
        txt.extend(entries);
        ids.push(id);
    }

    txt.insert(0, format!("cursor:{}", ids.len()));
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
