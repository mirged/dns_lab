//! End-to-end: two nodes exchange a multi-chunk message over local UDP DNS.

use dns_lab::config::{Config, EgressKind};
use dns_lab::db::Db;
use dns_lab::net::Node;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn node_cfg(id: &str, listen: &str, target: &str) -> Config {
    Config {
        display_name: id.to_string(),
        node_id: id.to_string(),
        listen_addr: listen.to_string(),
        zone: "lab.local".to_string(),
        egress: EgressKind::Direct,
        direct_server: Some(target.to_string()),
        timeout_ms: 500,
        retries: 4,
        ..Config::default()
    }
}

#[test]
fn two_nodes_exchange_multichunk_message() {
    let db_b = Arc::new(Db::open_in_memory().unwrap());
    let db_a = Arc::new(Db::open_in_memory().unwrap());

    let (tx_b, _rx_b) = std::sync::mpsc::channel();
    let (tx_a, _rx_a) = std::sync::mpsc::channel();

    let node_b = Node::new(
        node_cfg("bbbb2222", "127.0.0.1:11555", ""),
        db_b.clone(),
        tx_b,
    );
    node_b.start();

    let node_a = Node::new(
        node_cfg("aaaa1111", "127.0.0.1:11554", "127.0.0.1:11555"),
        db_a.clone(),
        tx_a,
    );

    std::thread::sleep(Duration::from_millis(200));

    let body: String = "The quick brown fox jumps over the lazy dog. ".repeat(40);
    let msg_id = dns_lab::random_id();
    let chunks = node_a
        .do_send("bbbb2222", &body, &msg_id)
        .expect("send failed");
    assert!(chunks >= 2, "expected a multi-chunk transfer, got {chunks}");

    // Wait for the listener to reassemble and store.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let msgs = db_b.messages_for("aaaa1111", 10);
        if let Some(m) = msgs.iter().find(|m| m.msg_id == msg_id) {
            assert_eq!(m.body, body, "reassembled body mismatch");
            break;
        }
        if Instant::now() > deadline {
            panic!("message was not received by node B");
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    node_a.stop();
    node_b.stop();
}

/// Low-compressibility text so the message really spans many chunks and forces
/// the poll reply past the UDP payload limit (regression for dropped packets).
fn noisy_text(n: usize) -> String {
    let mut s = String::with_capacity(n);
    let mut x: u64 = 0x1234_5678_9abc_def0;
    while s.len() < n {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.push((b'a' + (x % 26) as u8) as char);
    }
    s
}

#[test]
fn relay_mailbox_long_message() {
    let relay_addr = "127.0.0.1:11559";

    let db_relay = Arc::new(Db::open_in_memory().unwrap());
    let db_b = Arc::new(Db::open_in_memory().unwrap());
    let db_a = Arc::new(Db::open_in_memory().unwrap());

    let relay_cfg = Config {
        node_id: "rrrr9999".to_string(),
        listen_addr: relay_addr.to_string(),
        zone: "lab.local".to_string(),
        authoritative: true,
        ..Config::default()
    };
    let relay = Node::new(relay_cfg, db_relay.clone(), std::sync::mpsc::channel().0);
    relay.start();

    let mut b_cfg = node_cfg("bbbb2222", "127.0.0.1:11560", relay_addr);
    b_cfg.relay = Some(relay_addr.to_string());
    b_cfg.poll_interval_secs = 1;
    let node_b = Node::new(b_cfg, db_b.clone(), std::sync::mpsc::channel().0);
    node_b.start();

    let node_a = Node::new(
        node_cfg("aaaa1111", "127.0.0.1:11561", relay_addr),
        db_a.clone(),
        std::sync::mpsc::channel().0,
    );

    std::thread::sleep(Duration::from_millis(200));

    // ~12 KB of noisy text: the poll reply is far too big for one UDP datagram,
    // so the client must fall back to TCP without the relay dropping the message.
    let body = noisy_text(12_000);
    let msg_id = dns_lab::random_id();
    node_a
        .do_send("bbbb2222", &body, &msg_id)
        .expect("send via relay");

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let msgs = db_b.messages_for("aaaa1111", 10);
        if let Some(m) = msgs.iter().find(|m| m.msg_id == msg_id) {
            assert_eq!(m.body, body, "reassembled long body mismatch");
            break;
        }
        if Instant::now() > deadline {
            panic!("long relayed message never reached B's inbox");
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    relay.stop();
    node_a.stop();
    node_b.stop();
}

#[test]
fn relay_mailbox_delivery() {
    let relay_addr = "127.0.0.1:11556";

    let db_relay = Arc::new(Db::open_in_memory().unwrap());
    let db_b = Arc::new(Db::open_in_memory().unwrap());
    let db_a = Arc::new(Db::open_in_memory().unwrap());

    let relay_cfg = Config {
        node_id: "rrrr9999".to_string(),
        listen_addr: relay_addr.to_string(),
        zone: "lab.local".to_string(),
        authoritative: true,
        ..Config::default()
    };
    let relay = Node::new(relay_cfg, db_relay.clone(), std::sync::mpsc::channel().0);
    relay.start();

    // B: a client that polls the relay for its inbox.
    let mut b_cfg = node_cfg("bbbb2222", "127.0.0.1:11557", relay_addr);
    b_cfg.relay = Some(relay_addr.to_string());
    b_cfg.poll_interval_secs = 1;
    let node_b = Node::new(b_cfg, db_b.clone(), std::sync::mpsc::channel().0);
    node_b.start();

    // A: a client that sends through the relay.
    let node_a = Node::new(
        node_cfg("aaaa1111", "127.0.0.1:11558", relay_addr),
        db_a.clone(),
        std::sync::mpsc::channel().0,
    );

    std::thread::sleep(Duration::from_millis(200));

    let body = "relayed secretly over dns";
    let msg_id = dns_lab::random_id();
    node_a
        .do_send("bbbb2222", body, &msg_id)
        .expect("send via relay");

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let msgs = db_b.messages_for("aaaa1111", 10);
        if let Some(m) = msgs.iter().find(|m| m.msg_id == msg_id) {
            assert_eq!(m.body, body);
            break;
        }
        if Instant::now() > deadline {
            panic!("relayed message never reached B's inbox");
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    relay.stop();
    node_a.stop();
    node_b.stop();
}
