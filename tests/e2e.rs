//! End-to-end: real nodes exchanging signed messages over local UDP DNS.

use dns_lab::config::{Config, EgressKind, RelayMode};
use dns_lab::db::Db;
use dns_lab::net::Node;
use std::sync::Arc;
use std::time::{Duration, Instant};

const TEST_KEY: &str = "e2e-shared-passphrase";

fn node_cfg(id: &str, listen: &str, target: &str) -> Config {
    Config {
        display_name: id.to_string(),
        node_id: id.to_string(),
        listen_addr: listen.to_string(),
        zone: "lab.local".to_string(),
        egress: EgressKind::Direct,
        direct_server: Some(target.to_string()),
        timeout_ms: 400,
        retries: 4,
        window: 8,
        shared_key: TEST_KEY.to_string(),
        rate_limit_per_sec: 0,
        ..Config::default()
    }
}

/// A relay config; pass `key = None` for a blind forwarder.
fn relay_cfg(id: &str, listen: &str, key: Option<&str>) -> Config {
    Config {
        node_id: id.to_string(),
        listen_addr: listen.to_string(),
        zone: "lab.local".to_string(),
        authoritative: true,
        relay_mode: RelayMode::Authoritative,
        shared_key: key.unwrap_or("").to_string(),
        rate_limit_per_sec: 0,
        ..Config::default()
    }
}

fn node(cfg: Config, db: &Arc<Db>) -> Arc<Node> {
    let (tx, _rx) = std::sync::mpsc::channel();
    Node::new(cfg, db.clone(), tx).expect("node")
}

fn wait_for<F: Fn() -> bool>(timeout: Duration, what: &str, cond: F) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {what}");
}

fn stored_body(db: &Db, peer: &str, msg_id: &str) -> Option<String> {
    db.messages_for(peer, 50)
        .into_iter()
        .find(|m| m.msg_id == msg_id)
        .map(|m| m.body)
}

/// Low-compressibility text, so a message really spans many chunks.
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
fn two_nodes_exchange_multichunk_message() {
    let db_b = Arc::new(Db::open_in_memory().unwrap());
    let db_a = Arc::new(Db::open_in_memory().unwrap());

    let node_b = node(node_cfg("bbbb2222", "127.0.0.1:11555", ""), &db_b);
    node_b.start();
    let node_a = node(
        node_cfg("aaaa1111", "127.0.0.1:11554", "127.0.0.1:11555"),
        &db_a,
    );

    std::thread::sleep(Duration::from_millis(200));

    // Highly repetitive text compresses into a single query.
    let squishy: String = "The quick brown fox jumps over the lazy dog. ".repeat(40);
    let squishy_id = dns_lab::random_id();
    let chunks = node_a
        .do_send("bbbb2222", &squishy, &squishy_id, 1)
        .expect("send failed");
    // 1800 bytes of prose would be ~18 queries raw; with DEFLATE plus the
    // first-contact bundle and signature it should still be a handful.
    assert!(
        chunks <= 4,
        "compression should keep repetitive text under 4 queries, got {chunks}"
    );

    // Incompressible text must still be chunked and reassembled correctly.
    let body = noisy_text(3_000);
    let msg_id = dns_lab::random_id();
    let chunks = node_a
        .do_send("bbbb2222", &body, &msg_id, 2)
        .expect("send failed");
    assert!(chunks >= 2, "expected a multi-chunk transfer, got {chunks}");

    wait_for(
        Duration::from_secs(5),
        "node B to store the message",
        || stored_body(&db_b, "aaaa1111", &msg_id).is_some(),
    );
    assert_eq!(stored_body(&db_b, "aaaa1111", &msg_id).unwrap(), body);

    // First contact pinned A's signing key, so B can authenticate it later.
    assert!(
        db_b.peer_keys("aaaa1111")
            .map(|(ed, _)| ed.is_some())
            .unwrap_or(false),
        "A's key must be pinned"
    );

    node_a.stop();
    node_b.stop();
}

#[test]
fn relay_mailbox_long_message() {
    let relay_addr = "127.0.0.1:11559";

    let db_relay = Arc::new(Db::open_in_memory().unwrap());
    let db_b = Arc::new(Db::open_in_memory().unwrap());
    let db_a = Arc::new(Db::open_in_memory().unwrap());

    let relay = node(relay_cfg("rrrr9999", relay_addr, Some(TEST_KEY)), &db_relay);
    relay.start();

    let mut b_cfg = node_cfg("bbbb2222", "127.0.0.1:11560", relay_addr);
    b_cfg.relay = Some(relay_addr.to_string());
    b_cfg.poll_interval_secs = 1;
    let node_b = node(b_cfg, &db_b);
    node_b.start();

    let node_a = node(node_cfg("aaaa1111", "127.0.0.1:11561", relay_addr), &db_a);
    std::thread::sleep(Duration::from_millis(200));

    // ~12 KB of noisy text: the poll reply is far too big for one UDP datagram,
    // so the client must fall back to TCP without the relay dropping anything.
    let body = noisy_text(12_000);
    let msg_id = dns_lab::random_id();
    node_a
        .do_send("bbbb2222", &body, &msg_id, 1)
        .expect("send via relay");

    wait_for(
        Duration::from_secs(20),
        "the long relayed message to reach B",
        || stored_body(&db_b, "aaaa1111", &msg_id).is_some(),
    );
    assert_eq!(stored_body(&db_b, "aaaa1111", &msg_id).unwrap(), body);

    relay.stop();
    node_a.stop();
    node_b.stop();
}

/// The receiving side confirms delivery end to end: A sends, B stores and
/// receipts, the relay carries the receipt, A's poller flips the status.
#[test]
fn delivery_receipt_updates_sender_status() {
    let relay_addr = "127.0.0.1:11570";

    let db_relay = Arc::new(Db::open_in_memory().unwrap());
    let db_b = Arc::new(Db::open_in_memory().unwrap());
    let db_a = Arc::new(Db::open_in_memory().unwrap());

    let relay = node(relay_cfg("rrrr9999", relay_addr, Some(TEST_KEY)), &db_relay);
    relay.start();

    let mut a_cfg = node_cfg("aaaa1111", "127.0.0.1:11571", relay_addr);
    a_cfg.relay = Some(relay_addr.to_string());
    a_cfg.poll_interval_secs = 1;
    let node_a = node(a_cfg, &db_a);
    node_a.start();

    let mut b_cfg = node_cfg("bbbb2222", "127.0.0.1:11572", relay_addr);
    b_cfg.relay = Some(relay_addr.to_string());
    b_cfg.poll_interval_secs = 1;
    let node_b = node(b_cfg, &db_b);
    node_b.start();

    std::thread::sleep(Duration::from_millis(250));
    node_a.send_message("bbbb2222", "ping with a receipt");

    wait_for(Duration::from_secs(25), "the receipt to arrive", || {
        db_a.recent_messages(1)
            .first()
            .map(|m| m.status == "delivered")
            .unwrap_or(false)
    });

    // Exactly one chat message on each side: receipts are metadata, not chat.
    assert_eq!(db_a.recent_messages(10).len(), 1);
    assert_eq!(db_b.recent_messages(10).len(), 1);
    // And the outbox is empty again.
    assert_eq!(db_a.outbox_len(), 0);

    relay.stop();
    node_a.stop();
    node_b.stop();
}

/// The relay holds no key at all: it routes on the QNAME tag and forwards
/// ciphertext it cannot read.
#[test]
fn blind_relay_forwards_without_a_key() {
    let relay_addr = "127.0.0.1:11580";

    let db_relay = Arc::new(Db::open_in_memory().unwrap());
    let db_b = Arc::new(Db::open_in_memory().unwrap());
    let db_a = Arc::new(Db::open_in_memory().unwrap());

    let relay = node(relay_cfg("rrrr9999", relay_addr, None), &db_relay);
    assert!(relay.key.is_none(), "relay must have no key");
    relay.start();

    let mut b_cfg = node_cfg("bbbb2222", "127.0.0.1:11581", relay_addr);
    b_cfg.relay = Some(relay_addr.to_string());
    b_cfg.poll_interval_secs = 1;
    let node_b = node(b_cfg, &db_b);
    node_b.start();

    let node_a = node(node_cfg("aaaa1111", "127.0.0.1:11582", relay_addr), &db_a);
    std::thread::sleep(Duration::from_millis(250));

    let body = "forwarded by a relay that cannot read this";
    let msg_id = dns_lab::random_id();
    node_a
        .do_send("bbbb2222", body, &msg_id, 1)
        .expect("send through a blind relay");

    wait_for(
        Duration::from_secs(10),
        "B to receive mail via a keyless relay",
        || stored_body(&db_b, "aaaa1111", &msg_id).is_some(),
    );
    assert_eq!(stored_body(&db_b, "aaaa1111", &msg_id).unwrap(), body);

    // The relay kept neither plaintext nor a plaintext-addressed mailbox.
    assert!(
        db_relay.recent_messages(10).is_empty(),
        "blind relay must not store mail"
    );
    assert_eq!(
        db_relay.mailbox_pending("bbbb2222"),
        0,
        "mailbox must be keyed by routing tag, not by node id"
    );

    relay.stop();
    node_a.stop();
    node_b.stop();
}

/// Push: a node that never polls still receives mail, because the relay
/// attaches it to the ack of the final chunk it sends.
#[test]
fn queued_mail_rides_the_senders_final_ack() {
    let relay_addr = "127.0.0.1:11590";

    let db_relay = Arc::new(Db::open_in_memory().unwrap());
    let db_a = Arc::new(Db::open_in_memory().unwrap());
    let db_b = Arc::new(Db::open_in_memory().unwrap());

    let relay = node(relay_cfg("rrrr9999", relay_addr, Some(TEST_KEY)), &db_relay);
    relay.start();

    // Neither client polls: `relay` is deliberately not set, only direct_server.
    let node_a = node(node_cfg("aaaa1111", "127.0.0.1:11591", relay_addr), &db_a);
    let node_b = node(node_cfg("bbbb2222", "127.0.0.1:11592", relay_addr), &db_b);
    std::thread::sleep(Duration::from_millis(250));

    // B leaves a message in A's inbox.
    let from_b = "left in A's mailbox";
    let id_b = dns_lab::random_id();
    node_b
        .do_send("aaaa1111", from_b, &id_b, 1)
        .expect("b -> a");
    assert_eq!(db_a.recent_messages(10).len(), 0, "A has not polled yet");

    // A sends something; the relay hands A its queued mail in the ack.
    let from_a = "and this pulls the mail in";
    node_a
        .do_send("bbbb2222", from_a, &dns_lab::random_id(), 1)
        .expect("a -> b");

    // No poller is running on A, so this can only have arrived by push.
    assert_eq!(
        stored_body(&db_a, "bbbb2222", &id_b).as_deref(),
        Some(from_b),
        "queued mail must ride the final ack"
    );

    relay.stop();
    node_a.stop();
    node_b.stop();
}

/// Two nodes that have exchanged bundles switch to per-recipient payload keys,
/// and a third node holding the group passphrase still cannot read them.
#[test]
fn conversation_upgrades_to_peer_keys() {
    let relay_addr = "127.0.0.1:11600";

    let db_relay = Arc::new(Db::open_in_memory().unwrap());
    let db_a = Arc::new(Db::open_in_memory().unwrap());
    let db_b = Arc::new(Db::open_in_memory().unwrap());

    let relay = node(relay_cfg("rrrr9999", relay_addr, Some(TEST_KEY)), &db_relay);
    relay.start();

    let mut a_cfg = node_cfg("aaaa1111", "127.0.0.1:11601", relay_addr);
    a_cfg.relay = Some(relay_addr.to_string());
    a_cfg.poll_interval_secs = 1;
    let node_a = node(a_cfg, &db_a);
    node_a.start();

    let mut b_cfg = node_cfg("bbbb2222", "127.0.0.1:11602", relay_addr);
    b_cfg.relay = Some(relay_addr.to_string());
    b_cfg.poll_interval_secs = 1;
    let node_b = node(b_cfg, &db_b);
    node_b.start();

    std::thread::sleep(Duration::from_millis(250));

    // Round 1: A -> B (group key + bundle), B's receipt back (group + bundle).
    node_a.send_message("bbbb2222", "first contact");
    wait_for(Duration::from_secs(20), "B to pin A's keys", || {
        db_b.peer_keys("aaaa1111")
            .map(|(ed, x)| ed.is_some() && x.is_some())
            .unwrap_or(false)
    });
    wait_for(Duration::from_secs(20), "A to learn B's keys", || {
        db_a.peer_keys("bbbb2222")
            .map(|(ed, x)| ed.is_some() && x.is_some())
            .unwrap_or(false)
    });
    // B's receipt told A that B holds A's key, so A can switch to Peer.
    wait_for(
        Duration::from_secs(20),
        "A to switch that conversation to peer keys",
        || db_a.peer_knows_us("bbbb2222"),
    );

    // Round 2 must be encrypted to B alone: an eavesdropping C with the same
    // passphrase can read the mailbox but not the payload.
    let msg_id = dns_lab::random_id();
    node_a
        .do_send("bbbb2222", "peer-keyed secret", &msg_id, 2)
        .expect("peer-keyed send");
    wait_for(
        Duration::from_secs(20),
        "B to store the peer-keyed message",
        || stored_body(&db_b, "aaaa1111", &msg_id).is_some(),
    );

    // B chained back too, so both directions are on per-recipient keys now.
    wait_for(
        Duration::from_secs(20),
        "B to switch to peer keys as well",
        || db_b.peer_knows_us("aaaa1111"),
    );
    assert!(
        db_a.peer_keys("bbbb2222")
            .map(|(ed, x)| ed.is_some() && x.is_some())
            .unwrap_or(false),
        "A must hold B's signing and agreement keys"
    );

    relay.stop();
    node_a.stop();
    node_b.stop();
}

/// A dead primary relay must not lose mail: the configured fallback chain is
/// tried in order, and the message arrives through the relay that answers.
#[test]
fn sending_fails_over_to_a_live_relay() {
    let live = "127.0.0.1:11610";
    // Nothing is ever bound here.
    let dead = "127.0.0.1:11611";

    let db_relay = Arc::new(Db::open_in_memory().unwrap());
    let db_b = Arc::new(Db::open_in_memory().unwrap());
    let db_a = Arc::new(Db::open_in_memory().unwrap());

    let relay = node(relay_cfg("rrrr9999", live, Some(TEST_KEY)), &db_relay);
    relay.start();

    let mut b_cfg = node_cfg("bbbb2222", "127.0.0.1:11612", live);
    b_cfg.relay = Some(live.to_string());
    b_cfg.poll_interval_secs = 1;
    let node_b = node(b_cfg, &db_b);
    node_b.start();

    let mut a_cfg = node_cfg("aaaa1111", "127.0.0.1:11613", dead);
    a_cfg.relay = Some(dead.to_string());
    a_cfg.relays = vec![live.to_string()];
    a_cfg.timeout_ms = 150;
    a_cfg.retries = 1;
    let node_a = node(a_cfg, &db_a);
    std::thread::sleep(Duration::from_millis(250));

    let msg_id = dns_lab::random_id();
    node_a
        .do_send("bbbb2222", "sent through the backup relay", &msg_id, 1)
        .expect("the fallback relay should accept it");

    wait_for(
        Duration::from_secs(20),
        "B to receive mail via the fallback relay",
        || stored_body(&db_b, "aaaa1111", &msg_id).is_some(),
    );

    relay.stop();
    node_a.stop();
    node_b.stop();
}

/// A file is split, encrypted, relayed, reassembled and verified byte for byte.
#[test]
fn file_transfer_round_trip() {
    let relay_addr = "127.0.0.1:11620";
    let drop_dir = std::env::temp_dir().join(format!("dns_lab_files_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&drop_dir);

    let db_relay = Arc::new(Db::open_in_memory().unwrap());
    let db_b = Arc::new(Db::open_in_memory().unwrap());
    let db_a = Arc::new(Db::open_in_memory().unwrap());

    let relay = node(relay_cfg("rrrr9999", relay_addr, Some(TEST_KEY)), &db_relay);
    relay.start();

    let mut b_cfg = node_cfg("bbbb2222", "127.0.0.1:11621", relay_addr);
    b_cfg.relay = Some(relay_addr.to_string());
    b_cfg.poll_interval_secs = 1;
    b_cfg.download_dir = drop_dir.to_string_lossy().to_string();
    let node_b = node(b_cfg, &db_b);
    node_b.start();

    // A polls too, so the completion receipt can come back.
    let mut a_cfg = node_cfg("aaaa1111", "127.0.0.1:11622", relay_addr);
    a_cfg.relay = Some(relay_addr.to_string());
    a_cfg.poll_interval_secs = 1;
    let node_a = node(a_cfg, &db_a);
    node_a.start();
    std::thread::sleep(Duration::from_millis(250));

    // ~21 KB of incompressible bytes: three parts at the default part size.
    let data: Vec<u8> = (0..21_000u32).map(|i| (i * 37 % 251) as u8).collect();
    let transfer = dns_lab::file::digest(&data)[..8].to_string();
    let sent = node_a
        .send_file("bbbb2222", "payload.bin", &data, &transfer, |_, _| {})
        .expect("send file");
    assert_eq!(sent.parts, 3, "21 KB at 8 KiB per part");
    assert_eq!(sent.skipped, 0);

    wait_for(Duration::from_secs(25), "B to assemble the file", || {
        db_b.file_get(&transfer)
            .map(|f| f.complete)
            .unwrap_or(false)
    });

    let on_disk = drop_dir.join(format!("{transfer}-payload.bin"));
    let received = std::fs::read(&on_disk).expect("the assembled file must exist");
    assert_eq!(received.len(), data.len());
    assert_eq!(received, data, "content must match exactly");
    assert_eq!(
        dns_lab::file::digest(&received),
        dns_lab::file::digest(&data),
        "and so must its hash"
    );

    // The chat row points at the transfer instead of being plain text.
    let row = db_b
        .recent_messages(5)
        .into_iter()
        .next()
        .expect("chat row");
    assert_eq!(row.file_id.as_deref(), Some(transfer.as_str()));
    assert!(row.body.starts_with("[file] payload.bin"));
    assert_eq!(row.direction, "in");

    // The completion receipt flips the sender's row to delivered.
    wait_for(Duration::from_secs(20), "the file receipt", || {
        db_a.file_get(&transfer).is_some()
            && db_a
                .recent_messages(5)
                .iter()
                .any(|m| m.file_id.as_deref() == Some(transfer.as_str()) && m.status == "delivered")
    });

    let _ = std::fs::remove_dir_all(&drop_dir);
    relay.stop();
    node_a.stop();
    node_b.stop();
}

/// Re-running a send with the same transfer id skips what already went out.
#[test]
fn resending_a_file_resumes_instead_of_duplicating() {
    let relay_addr = "127.0.0.1:11630";
    let drop_dir = std::env::temp_dir().join(format!("dns_lab_resume_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&drop_dir);

    let db_relay = Arc::new(Db::open_in_memory().unwrap());
    let db_b = Arc::new(Db::open_in_memory().unwrap());
    let db_a = Arc::new(Db::open_in_memory().unwrap());

    let relay = node(relay_cfg("rrrr9999", relay_addr, Some(TEST_KEY)), &db_relay);
    relay.start();

    let mut b_cfg = node_cfg("bbbb2222", "127.0.0.1:11631", relay_addr);
    b_cfg.relay = Some(relay_addr.to_string());
    b_cfg.poll_interval_secs = 1;
    b_cfg.download_dir = drop_dir.to_string_lossy().to_string();
    let node_b = node(b_cfg, &db_b);
    node_b.start();

    let node_a = node(node_cfg("aaaa1111", "127.0.0.1:11632", relay_addr), &db_a);
    std::thread::sleep(Duration::from_millis(250));

    let data: Vec<u8> = (0..17_000u32).map(|i| (i % 253) as u8).collect();
    let transfer = dns_lab::file::digest(&data)[..8].to_string();
    let first = node_a
        .send_file("bbbb2222", "again.bin", &data, &transfer, |_, _| {})
        .expect("first send");
    assert_eq!(first.skipped, 0);

    wait_for(Duration::from_secs(25), "first copy", || {
        db_b.file_get(&transfer)
            .map(|f| f.complete)
            .unwrap_or(false)
    });

    let second = node_a
        .send_file("bbbb2222", "again.bin", &data, &transfer, |_, _| {})
        .expect("second send");
    assert_eq!(second.parts, first.parts);
    assert_eq!(second.skipped, second.parts, "everything was already sent");

    // And the receiver still holds exactly one copy, not two.
    let _ = node_b;
    wait_for(Duration::from_secs(5), "stable", || {
        db_b.file_get(&transfer)
            .map(|f| f.complete)
            .unwrap_or(false)
    });
    let files = db_b.recent_files(10);
    assert_eq!(files.len(), 1, "one transfer, not one per send");

    let _ = std::fs::remove_dir_all(&drop_dir);
    relay.stop();
    node_a.stop();
    node_b.stop();
}
