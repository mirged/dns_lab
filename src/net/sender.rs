//! Outgoing DNS queries: packet building, UDP/TCP exchange with retry + TCP fallback.

use super::{Egress, Node};
use crate::config::Config;
use crate::protocol::{self, verb};
use simple_dns::rdata::{OPT, RData};
use simple_dns::{CLASS, Name, Packet, PacketFlag, QCLASS, QTYPE, Question, TYPE};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, UdpSocket};
use std::time::Duration;

fn rand_u16() -> u16 {
    let b = crate::random_bytes(2);
    u16::from_le_bytes([b[0], b[1]])
}

/// Build a recursive TXT query packet with an EDNS0 OPT record.
pub fn build_query(cfg: &Config, qname: &str) -> Result<Vec<u8>, String> {
    let mut pkt = Packet::new_query(rand_u16());
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

fn udp_exchange(addr: SocketAddr, bytes: &[u8], timeout: Duration) -> Result<Vec<u8>, String> {
    let sock = UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
    sock.set_read_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    sock.send_to(bytes, addr).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 4096];
    let (n, _src) = sock.recv_from(&mut buf).map_err(|e| e.to_string())?;
    buf.truncate(n);
    Ok(buf)
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

/// Send one query and return the raw response, retrying UDP and falling back to TCP
/// when the response is truncated.
pub fn exchange(node: &Node, egress: Egress, qname: &str) -> Result<Vec<u8>, String> {
    let bytes = build_query(&node.cfg, qname)?;
    let addr = egress.addr();
    let timeout = Duration::from_millis(node.cfg.timeout_ms);

    let mut last = String::new();
    for _ in 0..=node.cfg.retries {
        match udp_exchange(addr, &bytes, timeout) {
            Ok(resp) => {
                if let Ok(pkt) = Packet::parse(&resp)
                    && pkt.has_flags(PacketFlag::TRUNCATION)
                {
                    node.log(format!(
                        "    [~] truncated over UDP ({addr}), retrying over TCP"
                    ));
                    return tcp_exchange(addr, &bytes, timeout)
                        .map_err(|e| format!("tcp fallback failed: {e}"));
                }
                return Ok(resp);
            }
            Err(e) => last = e,
        }
    }
    Err(format!(
        "no response from {addr} after {} tries: {last}",
        node.cfg.retries + 1
    ))
}

fn is_ack(resp: &[u8]) -> bool {
    let pkt = match Packet::parse(resp) {
        Ok(p) => p,
        Err(_) => return false,
    };
    for rr in &pkt.answers {
        if let RData::TXT(txt) = &rr.rdata {
            for (k, _v) in txt.iter_raw() {
                if let Ok(s) = std::str::from_utf8(k)
                    && s.starts_with("a:")
                {
                    return true;
                }
            }
        }
    }
    // No explicit ack token: accept a valid NOERROR answer.
    pkt.rcode() == simple_dns::RCODE::NoError && !pkt.answers.is_empty()
}

/// Chunk, transmit and confirm a Base32 blob. Returns the number of DNS queries sent.
pub fn send_blob(node: &Node, egress: Egress, blob: &str, msg_id: &str) -> Result<usize, String> {
    let max = protocol::max_data_chars(&node.cfg.zone);
    let queries = protocol::split_into_queries(blob, max);
    let total = queries.len() as u32;

    for (i, data) in queries.iter().enumerate() {
        let qname = protocol::build_qname(
            verb::MSG,
            msg_id,
            i as u32,
            total,
            data.as_str(),
            &node.cfg.zone,
        );
        let resp = exchange(node, egress, &qname)?;
        if !is_ack(&resp) {
            return Err(format!("chunk {}/{} not acknowledged", i + 1, total));
        }
        node.log(format!(
            "    -> [{}] chunk {}/{} ({}-char qname) ack",
            egress.label(),
            i + 1,
            total,
            qname.len()
        ));
    }
    Ok(queries.len())
}
