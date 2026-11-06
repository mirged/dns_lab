//! Outgoing DNS queries: pipelined UDP with selective retransmit, TCP fallback.
//!
//! A message is split into chunks and sent with `window` queries in flight, so
//! the cost is `ceil(chunks / window)` round trips instead of one per chunk.
//! Replies are matched by DNS transaction id, so they may arrive out of order;
//! only the chunks that never came back are retransmitted.

use super::reply::{self, Reply};
use super::{Egress, Node};
use crate::config::Config;
use crate::protocol::{self, verb};
use simple_dns::rdata::OPT;
use simple_dns::{CLASS, Name, Packet, QCLASS, QTYPE, Question, TYPE};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, UdpSocket};
use std::time::{Duration, Instant};

/// Hard ceiling on how long one message may take, per chunk window.
fn send_budget(cfg: &Config, chunks: usize) -> Duration {
    let per_window = cfg.timeout_ms * (cfg.retries as u64 + 2);
    let windows = (chunks as u64).div_ceil(cfg.window.max(1) as u64);
    Duration::from_millis((per_window * windows).max(cfg.timeout_ms))
}

fn rand_u16() -> u16 {
    let b = crate::random_bytes(2);
    u16::from_le_bytes([b[0], b[1]])
}

/// Build a recursive TXT query packet with an EDNS0 OPT record.
pub fn build_query(cfg: &Config, qname: &str, txid: u16) -> Result<Vec<u8>, String> {
    let mut pkt = Packet::new_query(txid);
    let name = Name::new(qname).map_err(|e| format!("invalid qname ({qname}): {e}"))?;
    pkt.questions.push(Question::new(
        name,
        QTYPE::TYPE(TYPE::TXT),
        QCLASS::CLASS(CLASS::IN),
        false,
    ));
    *pkt.opt_mut() = Some(OPT {
        opt_codes: Vec::new(),
        udp_packet_size: cfg.edns_udp_size,
        version: 0,
    });
    pkt.build_bytes_vec_compressed().map_err(|e| e.to_string())
}

fn udp_socket(cfg: &Config) -> Result<UdpSocket, String> {
    let sock = UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
    let wait = Duration::from_millis(cfg.timeout_ms.clamp(50, 2000));
    sock.set_read_timeout(Some(wait))
        .map_err(|e| e.to_string())?;
    Ok(sock)
}

fn tcp_exchange(addr: SocketAddr, bytes: &[u8], timeout: Duration) -> Result<Vec<u8>, String> {
    let mut stream = TcpStream::connect_timeout(&addr, timeout).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    let len = (bytes.len() as u16).to_be_bytes();
    stream.write_all(&len).map_err(|e| e.to_string())?;
    stream.write_all(bytes).map_err(|e| e.to_string())?;
    let mut lb = [0u8; 2];
    stream.read_exact(&mut lb).map_err(|e| e.to_string())?;
    let n = u16::from_be_bytes(lb) as usize;
    let mut buf = vec![0u8; n];
    stream.read_exact(&mut buf).map_err(|e| e.to_string())?;
    Ok(buf)
}

/// One request/response exchange (polls, mailbox acks, probes).
pub fn exchange(node: &Node, egress: Egress, qname: &str) -> Result<Reply, String> {
    let addr = egress.addr();
    let timeout = Duration::from_millis(node.cfg.timeout_ms);
    let mut last = String::new();

    for _ in 0..=node.cfg.retries {
        let bytes = build_query(&node.cfg, qname, rand_u16())?;
        let sock = udp_socket(&node.cfg)?;
        if let Err(e) = sock.send_to(&bytes, addr) {
            last = e.to_string();
            continue;
        }
        let mut buf = vec![0u8; 65_535];
        match sock.recv_from(&mut buf) {
            Ok((n, _src)) => {
                let parsed = reply::parse(&buf[..n])?;
                if parsed.truncated {
                    node.log(format!(
                        "    [~] truncated over UDP ({addr}), retrying over TCP"
                    ));
                    let raw = tcp_exchange(addr, &bytes, timeout)
                        .map_err(|e| format!("tcp fallback failed: {e}"))?;
                    return reply::parse(&raw);
                }
                return Ok(parsed);
            }
            Err(e) => last = e.to_string(),
        }
    }
    Err(format!(
        "no response from {addr} after {} tries: {last}",
        node.cfg.retries + 1
    ))
}

/// What a completed message send produced.
#[derive(Debug, Default)]
pub struct Sent {
    pub chunks: usize,
    /// Mail the relay handed back in an ack reply (push).
    pub pushed: Vec<(String, String)>,
}

struct Pending {
    seq: u32,
    qname: String,
    bytes: Vec<u8>,
}

struct InFlight {
    idx: usize,
    sent_at: Instant,
}

/// Smoothed round-trip time, Jacobson/Karels style.
///
/// The retransmit timer used to be the fixed `timeout_ms`, so on a lossy link
/// every dropped datagram cost the full 800 ms even when the RTT was 30 ms.
/// Measuring the RTT turns that into `srtt + 4 * variance`, clamped.
#[derive(Debug, Default)]
struct Rtt {
    srtt: Option<Duration>,
    var: Duration,
    /// Peak sample, decayed slowly. A burst of pipelined queries queues up at
    /// the relay (one SQLite transaction each), so the *average* reply time is
    /// far below the worst one; using the average alone made the sender
    /// retransmit packets that were merely still in the relay's queue.
    peak: Option<Duration>,
}

impl Rtt {
    fn sample(&mut self, rtt: Duration) {
        match self.srtt {
            None => {
                self.srtt = Some(rtt);
                self.var = rtt / 2;
            }
            Some(avg) => {
                let diff = rtt.abs_diff(avg);
                self.var = (self.var * 3 + diff) / 4;
                self.srtt = Some((avg * 7 + rtt) / 8);
            }
        }
        // Forget old peaks in ~10 samples so a one-off stall does not pin the
        // timer high forever.
        let decayed = self.peak.map(|p| p * 9 / 10).unwrap_or_default();
        self.peak = Some(rtt.max(decayed));
    }

    /// Retransmit this long after a send. `cap` is the configured timeout.
    fn rto(&self, cap: Duration) -> Duration {
        let Some(srtt) = self.srtt else {
            return cap;
        };
        // `cap` comes from config and may be below the floor (tests use 120 ms).
        let hi = cap.max(MIN_RTO);
        let smooth = srtt + self.var * 4;
        let queued = self.peak.map(|p| p * 5 / 4).unwrap_or_default();
        smooth.max(queued).clamp(MIN_RTO, hi)
    }
}

/// Never retransmit faster than this, whatever the RTT says: a spurious resend
/// costs the relay real work, and the relay's own queueing is invisible to the
/// sampling (it happens after the datagram arrived).
const MIN_RTO: Duration = Duration::from_millis(250);

/// Chunk, transmit and confirm a Base32 blob.
///
/// The last chunk uses the `f` verb and carries `sender_tag`, which is how the
/// relay knows whose inbox it may attach to that ack.
pub fn send_blob(
    node: &Node,
    egress: Egress,
    blob: &str,
    msg_id: &str,
    tag: &str,
    sender_tag: &str,
) -> Result<Sent, String> {
    let max = protocol::max_data_chars(&node.cfg.zone);
    let parts = protocol::split_into_queries(blob, max);
    let total = parts.len() as u32;
    if total > protocol::MAX_CHUNKS {
        return Err(format!(
            "message needs {total} queries, over the {} chunk limit (body too large)",
            protocol::MAX_CHUNKS
        ));
    }

    let mut pending = Vec::with_capacity(parts.len());
    for (i, data) in parts.iter().enumerate() {
        let last = i as u32 + 1 == total;
        let qname = if last {
            protocol::build_final_qname(
                tag,
                sender_tag,
                msg_id,
                i as u32,
                total,
                data.as_str(),
                &node.cfg.zone,
            )
        } else {
            protocol::build_qname(
                verb::MSG,
                tag,
                msg_id,
                i as u32,
                total,
                data.as_str(),
                &node.cfg.zone,
            )
        };
        if qname.len() > protocol::MAX_QNAME {
            return Err(format!(
                "chunk qname is {} chars, over the {} limit: shorten `zone`",
                qname.len(),
                protocol::MAX_QNAME
            ));
        }
        let bytes = build_query(&node.cfg, &qname, 0)?;
        pending.push(Pending {
            seq: i as u32,
            qname,
            bytes,
        });
    }

    let mut out = Sent {
        chunks: pending.len(),
        pushed: Vec::new(),
    };
    let addr = egress.addr();
    let window = node.cfg.window.clamp(1, 64) as usize;
    let timeout = Duration::from_millis(node.cfg.timeout_ms);
    let budget = send_budget(&node.cfg, pending.len());
    let started = Instant::now();

    let sock = udp_socket(&node.cfg)?;
    if node.cfg.verbose && pending.len() > 1 {
        node.log(format!(
            "    [*] {total} chunk(s), window {window}, timeout {} ms",
            node.cfg.timeout_ms
        ));
    }
    let mut inflight: HashMap<u16, InFlight> = HashMap::new();
    let mut attempts: Vec<u32> = vec![0; pending.len()];
    let mut rtt = Rtt::default();
    let mut rto = timeout;
    // TCP-style exponential backoff: a timeout doubles the wait for the next
    // retransmit, any answered query resets it. Without this, a stall made the
    // sender resend at the same short interval until it ran out of attempts.
    let mut backoff: u32 = 1;
    let mut cursor = 0usize;
    let mut done = 0usize;
    let mut last_error = String::from("no response");

    fn transmit(
        sock: &UdpSocket,
        addr: SocketAddr,
        pending: &[Pending],
        idx: usize,
        txid: u16,
    ) -> Result<(), String> {
        let mut bytes = Vec::with_capacity(pending[idx].bytes.len());
        bytes.extend_from_slice(&txid.to_be_bytes());
        bytes.extend_from_slice(&pending[idx].bytes[2..]);
        sock.send_to(&bytes, addr)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    while done < pending.len() {
        if started.elapsed() > budget {
            return Err(format!(
                "{last_error} (gave up after {done}/{} chunks, {}s)",
                pending.len(),
                started.elapsed().as_secs()
            ));
        }

        while inflight.len() < window && cursor < pending.len() {
            let idx = cursor;
            cursor += 1;
            let txid = rand_u16();
            transmit(&sock, addr, &pending, idx, txid).map_err(|e| format!("send failed: {e}"))?;
            inflight.insert(
                txid,
                InFlight {
                    idx,
                    sent_at: Instant::now(),
                },
            );
        }

        if inflight.is_empty() {
            break;
        }

        // Wake up in time to retransmit the oldest in-flight chunk, instead of
        // always sleeping for the configured timeout.
        let _ = sock.set_read_timeout(Some(rto * backoff));
        let mut buf = vec![0u8; 65_535];
        match sock.recv_from(&mut buf) {
            Ok((n, _src)) => {
                let resp = &buf[..n];
                if resp.len() < 2 {
                    continue;
                }
                let txid = u16::from_be_bytes([resp[0], resp[1]]);
                let Some(entry) = inflight.remove(&txid) else {
                    continue; // duplicate or stale reply
                };
                let idx = entry.idx;
                backoff = 1;
                // Karn: only trust the timing of a datagram that was sent once.
                // Sampling a retransmit would fold the timeout into the RTT and
                // make the timer grow every time it had to save us.
                if attempts[idx] == 0 {
                    rtt.sample(entry.sent_at.elapsed());
                    rto = rtt.rto(timeout);
                }

                let parsed = match reply::parse(resp) {
                    Ok(p) => p,
                    Err(e) => {
                        last_error = e;
                        attempts[idx] += 1;
                        cursor = cursor.min(idx);
                        continue;
                    }
                };

                if parsed.truncated {
                    node.log(format!(
                        "    [~] truncated over UDP ({addr}), retrying chunk over TCP"
                    ));
                    match tcp_exchange(addr, &pending[idx].bytes, timeout) {
                        Ok(raw) => {
                            let parsed = reply::parse(&raw)?;
                            if !parsed.accepted(msg_id, pending[idx].seq) {
                                last_error = format!("chunk {} not acknowledged", idx + 1);
                                attempts[idx] += 1;
                                if attempts[idx] > node.cfg.retries {
                                    return Err(last_error);
                                }
                                cursor = cursor.min(idx);
                                continue;
                            }
                            collect_pushed(&mut out, parsed);
                            ack_log(node, egress, idx, pending.len(), &pending[idx].qname);
                            done += 1;
                        }
                        Err(e) => {
                            last_error = format!("tcp fallback failed: {e}");
                            attempts[idx] += 1;
                            if attempts[idx] > node.cfg.retries {
                                return Err(last_error);
                            }
                            cursor = cursor.min(idx);
                        }
                    }
                    continue;
                }

                if !parsed.accepted(msg_id, pending[idx].seq) {
                    last_error = format!(
                        "chunk {} not acknowledged (relay said {:?})",
                        idx + 1,
                        parsed.rcode
                    );
                    attempts[idx] += 1;
                    if attempts[idx] > node.cfg.retries {
                        return Err(last_error);
                    }
                    cursor = cursor.min(idx);
                    continue;
                }
                collect_pushed(&mut out, parsed);
                ack_log(node, egress, idx, pending.len(), &pending[idx].qname);
                done += 1;
            }
            Err(_) => {
                // Timeout: retransmit anything that has been out too long.
                let now = Instant::now();
                let stale: Vec<(u16, usize)> = inflight
                    .iter()
                    .filter(|(_, f)| now.duration_since(f.sent_at) >= rto)
                    .map(|(txid, f)| (*txid, f.idx))
                    .collect();
                if stale.is_empty() {
                    continue;
                }
                backoff = (backoff * 2).min(16);
                for (txid, idx) in stale {
                    inflight.remove(&txid);
                    attempts[idx] += 1;
                    if attempts[idx] > node.cfg.retries {
                        return Err(format!(
                            "{} (chunk {} dropped after {} attempts)",
                            last_error,
                            idx + 1,
                            attempts[idx]
                        ));
                    }
                    let new_txid = rand_u16();
                    transmit(&sock, addr, &pending, idx, new_txid)
                        .map_err(|e| format!("send failed: {e}"))?;
                    inflight.insert(
                        new_txid,
                        InFlight {
                            idx,
                            sent_at: Instant::now(),
                        },
                    );
                }
            }
        }
    }

    Ok(out)
}

fn collect_pushed(out: &mut Sent, reply: Reply) {
    for (mid, blob) in reply.complete_blobs() {
        out.pushed.push((mid, blob));
    }
}

fn ack_log(node: &Node, egress: Egress, idx: usize, total: usize, qname: &str) {
    if node.cfg.verbose {
        node.log(format!(
            "    -> [{}] chunk {}/{} ({}-char qname) ack",
            egress.label(),
            idx + 1,
            total,
            qname.len()
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use simple_dns::ResourceRecord;
    use simple_dns::rdata::RData;
    use simple_dns::rdata::TXT;
    use std::sync::Arc;

    fn test_node(cfg: &Config) -> Arc<Node> {
        let db = Arc::new(Db::open_in_memory().unwrap());
        let (tx, _rx) = std::sync::mpsc::channel();
        Node::new(cfg.clone(), db, tx).expect("node")
    }

    fn reply_for(pkt: &Packet, qname: &str, msg_id: &str, seq: u32) -> Vec<u8> {
        let mut reply = Packet::new_reply(pkt.id());
        let mut txt = TXT::new();
        let ack = format!("a:{msg_id}:{seq}");
        txt.add_string(&ack).unwrap();
        reply.answers.push(ResourceRecord::new(
            Name::new_unchecked(qname),
            CLASS::IN,
            0,
            RData::TXT(txt),
        ));
        reply.build_bytes_vec_compressed().unwrap()
    }

    #[test]
    fn the_retransmit_timer_tracks_the_measured_rtt() {
        let cap = Duration::from_millis(800);
        let mut rtt = Rtt::default();
        assert_eq!(rtt.rto(cap), cap, "no sample yet: fall back to the config");

        // A fast link: the old code would still wait the full 800 ms.
        rtt.sample(Duration::from_millis(20));
        rtt.sample(Duration::from_millis(22));
        let fast = rtt.rto(cap);
        assert_eq!(
            fast, MIN_RTO,
            "20 ms RTT sits on the floor, not on the old 800 ms timeout"
        );
        assert!(fast < cap);

        // A slow, jittery link: the timer has to grow with the variance.
        let mut slow = Rtt::default();
        for ms in [180u64, 260, 190, 250] {
            slow.sample(Duration::from_millis(ms));
        }
        let slow_rto = slow.rto(cap);
        assert!(
            slow_rto > Duration::from_millis(300),
            "240 ms RTT with jitter needs headroom, got {slow_rto:?}"
        );
        assert!(slow_rto <= cap, "{slow_rto:?} must not exceed the cap");

        // A single slow reply (a burst queueing at the relay) must raise the
        // timer above what the average alone would suggest: the next retransmit
        // would otherwise race a reply that is merely still queued.
        let mut burst = Rtt::default();
        for ms in [4u64, 5, 4, 90] {
            burst.sample(Duration::from_millis(ms));
        }
        assert!(
            burst.rto(cap) > Duration::from_millis(90),
            "the 90 ms sample must dominate the 4 ms average, got {:?}",
            burst.rto(cap)
        );

        // And the floor holds for tiny RTTs and tiny configs.
        let mut tiny = Rtt::default();
        tiny.sample(Duration::from_millis(1));
        assert_eq!(tiny.rto(Duration::from_millis(100)), MIN_RTO);
        assert_eq!(
            Rtt::default().rto(Duration::from_millis(50)),
            Duration::from_millis(50)
        );
    }

    #[test]
    fn pipelines_and_retransmits_only_what_was_dropped() {
        let relay = UdpSocket::bind("127.0.0.1:0").unwrap();
        relay
            .set_read_timeout(Some(Duration::from_millis(400)))
            .unwrap();
        let addr = relay.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut buf = vec![0u8; 4096];
            let mut served = 0usize;
            let mut requests = 0usize;
            let mut dropped = false;
            while let Ok((n, src)) = relay.recv_from(&mut buf) {
                requests += 1;
                let raw = buf[..n].to_vec();
                let Ok(pkt) = Packet::parse(&raw) else { break };
                let Some(q) = pkt.questions.first() else {
                    break;
                };
                let qname = q.qname.to_string();
                let Some(parsed) = protocol::parse_qname("lab.local", &qname) else {
                    continue;
                };
                if !dropped {
                    dropped = true;
                    continue; // pretend this datagram was lost
                }
                served += 1;
                let bytes = reply_for(&pkt, &qname, &parsed.msg_id, parsed.seq);
                let _ = relay.send_to(&bytes, src);
            }
            (requests, served)
        });

        let cfg = Config {
            zone: "lab.local".to_string(),
            timeout_ms: 120,
            retries: 4,
            window: 4,
            shared_key: "pipeline-test".to_string(),
            ..Config::default()
        };
        let node = test_node(&cfg);
        let blob = "A".repeat(protocol::max_data_chars("lab.local") * 3 + 5);
        let sent = send_blob(
            &node,
            Egress::Direct(addr),
            &blob,
            "deadbeef",
            "0a1b2c3d",
            "11223344",
        )
        .expect("send succeeds despite one dropped datagram");

        assert_eq!(sent.chunks, 4, "four chunks for this blob");
        let (requests, served) = server.join().unwrap();
        assert_eq!(served, 4, "every chunk got acked exactly once");
        assert!(
            requests >= 5,
            "the dropped datagram must be retransmitted (requests={requests})"
        );
    }

    #[test]
    fn final_chunk_is_sent_with_the_f_verb() {
        let relay = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = relay.local_addr().unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = seen.clone();
        let server = std::thread::spawn(move || {
            let mut buf = vec![0u8; 4096];
            while let Ok((n, src)) = relay.recv_from(&mut buf) {
                let raw = buf[..n].to_vec();
                let Ok(pkt) = Packet::parse(&raw) else { break };
                let Some(q) = pkt.questions.first() else {
                    break;
                };
                let qname = q.qname.to_string();
                sink.lock().unwrap().push(qname.clone());
                if let Some(parsed) = protocol::parse_qname("lab.local", &qname) {
                    let bytes = reply_for(&pkt, &qname, &parsed.msg_id, parsed.seq);
                    let _ = relay.send_to(&bytes, src);
                }
            }
        });

        let cfg = Config {
            zone: "lab.local".to_string(),
            timeout_ms: 150,
            retries: 2,
            window: 4,
            shared_key: "pipeline-test".to_string(),
            ..Config::default()
        };
        let node = test_node(&cfg);
        let blob = "A".repeat(protocol::max_data_chars("lab.local") + 5);
        send_blob(
            &node,
            Egress::Direct(addr),
            &blob,
            "cafebabe",
            "0a1b2c3d",
            "99aabbcc",
        )
        .expect("send");
        drop(server);

        let names = seen.lock().unwrap().clone();
        assert_eq!(names.len(), 2, "two chunks: {names:?}");
        assert!(
            names[0].starts_with("m."),
            "first chunk uses m: {}",
            names[0]
        );
        assert!(
            names[1].starts_with("f.0a1b2c3d.99aabbcc.cafebabe."),
            "final chunk carries sender tag: {}",
            names[1]
        );
    }
}
