//! Headless CLI: relay, one-shot send, id, inbox and peers.

use dns_lab::config::Config;
use dns_lab::db::Db;
use dns_lab::net::{AppEvent, Node};
use std::io::Read;
use std::sync::Arc;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("gui");
    let code = match cmd {
        "serve" | "relay" => serve(&args),
        "run" => run(&args),
        "send" => send(&args),
        "inbox" => inbox(&args),
        "files" => files(&args),
        "peers" => peers(&args),
        "id" => id(&args),
        "gui" => gui(&args),
        "-h" | "--help" | "help" => {
            usage();
            0
        }
        "-V" | "--version" | "version" => {
            println!("dns_lab {VERSION}");
            0
        }
        _ => {
            eprintln!("[!] unknown command: {cmd}\n");
            usage();
            2
        }
    };
    std::process::exit(code);
}

fn config_path(args: &[String]) -> String {
    if let Some(i) = args.iter().position(|a| a == "--config")
        && let Some(p) = args.get(i + 1)
    {
        return p.clone();
    }
    std::env::var("DNS_LAB_CONFIG").unwrap_or_else(|_| "dns_lab.toml".to_string())
}

fn arg_value(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

fn usage() {
    eprintln!(
        "dns_lab {VERSION} - DNS-tunnel messenger

USAGE:
  dns_lab gui    [--config FILE]              Desktop app (default command)
  dns_lab serve  [--config FILE]              Headless authoritative relay (VPS)
  dns_lab run    [--config FILE]              Headless client daemon (listener + poller)
  dns_lab send   --to ID [--body TEXT|-]      Send one message from the CLI
  dns_lab send   --to ID --file PATH          Send a file (resumes if re-run)
  dns_lab inbox  [--limit N]                  Show recent messages
  dns_lab files  [--limit N]                  Show file transfers
  dns_lab peers  [--config FILE]              List known contacts
  dns_lab id     [--config FILE]              Print this node id
  dns_lab version                             Print the version

`--body -` reads the message from stdin. The config file defaults to
$DNS_LAB_CONFIG, then ./dns_lab.toml."
    );
}

/// Console drain for headless runs (node logs are already printed to stderr).
fn drain_logs(rx: std::sync::mpsc::Receiver<AppEvent>) {
    std::thread::spawn(move || while rx.recv().is_ok() {});
}

fn open_node(cfg: Config) -> (Arc<Node>, Arc<Db>) {
    let db = Arc::new(Db::open(&cfg.db_path).expect("open database"));
    let (tx, rx) = std::sync::mpsc::channel();
    drain_logs(rx);
    let node = Node::new(cfg, db.clone(), tx).expect("initialise node identity");
    (node, db)
}

fn serve(args: &[String]) -> i32 {
    let path = config_path(args);
    let mut cfg = Config::load(&path);
    cfg.authoritative = true;
    cfg.relay_mode = dns_lab::config::RelayMode::Authoritative;

    let (node, _db) = open_node(cfg.clone());
    node.start();

    println!("==================================================");
    println!("  DNS Lab authoritative relay {VERSION}");
    println!("  Zone:      {}", cfg.zone);
    println!("  Listen:    {} (UDP + TCP)", cfg.listen_addr);
    println!("  Database:  {}", cfg.db_path);
    println!("  Node id:   {}", cfg.node_id);
    println!("  Identity:  {}", node.fingerprint());
    println!(
        "  Key:       {}",
        match cfg.key_opt() {
            None => "none - blind forwarder, cannot decrypt anything",
            Some(_) if cfg.key_is_placeholder() => "EXAMPLE PLACEHOLDER (insecure)",
            Some(_) => "configured",
        }
    );
    println!("==================================================");

    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

/// Headless client daemon: same listener + poller the GUI runs, no window.
fn run(args: &[String]) -> i32 {
    let cfg = Config::load(&config_path(args));
    let polling = cfg.relay.is_some() || cfg.egress == dns_lab::config::EgressKind::Resolver;
    let (node, _db) = open_node(cfg.clone());
    node.start();

    println!("DNS Lab client {VERSION}");
    println!("  Node id:   {}", cfg.node_id);
    println!("  Identity:  {}", node.fingerprint());
    println!("  Listen:    {} (UDP + TCP)", cfg.listen_addr);
    println!(
        "  Egress:    {} {}",
        cfg.egress.as_str(),
        cfg.relay
            .clone()
            .or_else(|| cfg.direct_server.clone())
            .or_else(|| cfg.resolver.clone())
            .unwrap_or_else(|| "(unset)".to_string())
    );
    println!(
        "  Inbox:     {}",
        if polling {
            format!("polling every {}s", cfg.poll_interval_secs)
        } else {
            "disabled (set `relay` or egress = resolver)".to_string()
        }
    );
    println!("  Database:  {}", cfg.db_path);
    println!("Sending is done from another shell: `dns_lab send --to <id> --body ...`");
    if cfg.key_opt().is_none() {
        eprintln!("[!] no shared_key configured: this node cannot send, receive or route mail");
    }

    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

fn read_body(args: &[String]) -> Option<String> {
    match arg_value(args, "--body").as_deref() {
        Some("-") => {
            let mut s = String::new();
            std::io::stdin().read_to_string(&mut s).ok()?;
            Some(s.trim_end().to_string())
        }
        Some(v) => Some(v.to_string()),
        None => None,
    }
}

fn send(args: &[String]) -> i32 {
    let cfg = Config::load(&config_path(args));
    let Some(to) = arg_value(args, "--to") else {
        eprintln!("[!] --to ID is required (see `dns_lab id` on the peer)");
        return 2;
    };
    if let Some(path) = arg_value(args, "--file") {
        return send_file(&path, &to, args);
    }
    let body = read_body(args).unwrap_or_else(|| "hello over dns".to_string());
    if body.is_empty() {
        eprintln!("[!] refusing to send an empty message");
        return 2;
    }

    let (node, db) = open_node(cfg);
    let msg_id = dns_lab::random_id();
    let peer_name = db.peer(&to).map(|p| p.name).unwrap_or_else(|| to.clone());
    // Record it locally so the message shows up in history and a delivery
    // receipt has something to update.
    let _ = db.insert_message(&msg_id, "out", &to, &peer_name, &body, "sending");
    let seq = db.conv_next_out(&to) as u64;
    let _ = db.outbox_add(&msg_id, &to, &body, seq);

    println!("[*] sending {msg_id} to {to} ({} bytes)", body.len());
    match node.do_send(&to, &body, &msg_id, seq) {
        Ok(chunks) => {
            db.outbox_remove(&msg_id);
            db.set_status(&msg_id, "sent");
            println!(
                "[+] relay accepted it in {chunks} DNS quer{}",
                if chunks == 1 { "y" } else { "ies" }
            );
            0
        }
        Err(e) => {
            // Left in the outbox: `dns_lab run` will retry it with backoff.
            db.set_status(&msg_id, "queued");
            eprintln!("[!] send failed: {e}");
            1
        }
    }
}

fn send_file(path: &str, to: &str, args: &[String]) -> i32 {
    let cfg = Config::load(&config_path(args));
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[!] cannot read {path}: {e}");
            return 1;
        }
    };
    let name = std::path::Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    // A stable id per (name, size, content) so re-running the same command
    // resumes the transfer instead of starting a second copy.
    let transfer_id = &dns_lab::file::digest(&data)[..8];

    let (node, db) = open_node(cfg);
    println!(
        "[*] sending {name} ({}, {} part(s)) to {to} as {transfer_id}",
        dns_lab::file::human_size(data.len() as u64),
        dns_lab::file::part_count(data.len() as u64, dns_lab::file::DEFAULT_PART_BYTES)
    );
    let mut last_pct = u32::MAX;
    let result = node.send_file(to, &name, &data, transfer_id, |done, total| {
        let pct = (done * 100).checked_div(total).unwrap_or(100);
        if pct / 10 != last_pct / 10 || done == total {
            last_pct = pct;
            // End the line on the last part so the node's own log line that
            // follows does not land in the middle of the progress bar.
            if done == total {
                eprintln!("    {done}/{total} parts ({pct}%)");
            } else {
                eprint!("\r    {done}/{total} parts ({pct}%)");
            }
        }
    });
    match result {
        Ok(sent) => {
            println!(
                "[+] {name} accepted by the relay ({} parts, {} resumed)",
                sent.parts, sent.skipped
            );
            let _ = db.file_get(&sent.transfer_id);
            0
        }
        Err(e) => {
            eprintln!("[!] file send failed: {e}");
            1
        }
    }
}

fn files(args: &[String]) -> i32 {
    let cfg = Config::load(&config_path(args));
    let db = Db::open(&cfg.db_path).expect("open database");
    let limit = arg_value(args, "--limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let rows = db.recent_files(limit);
    if rows.is_empty() {
        println!("(no file transfers in {})", cfg.db_path);
        return 0;
    }
    for f in rows {
        println!(
            "{}  {}  {:<24} {:>10}  {}/{}  {}",
            f.transfer_id,
            f.direction,
            f.name,
            dns_lab::file::human_size(f.size),
            if f.complete { "done" } else { "partial" },
            f.parts,
            if f.path.is_empty() {
                f.peer_id.clone()
            } else {
                f.path.clone()
            }
        );
    }
    0
}

fn inbox(args: &[String]) -> i32 {
    let cfg = Config::load(&config_path(args));
    let limit: i64 = arg_value(args, "--limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let db = match Db::open(&cfg.db_path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("[!] cannot open {}: {e}", cfg.db_path);
            return 1;
        }
    };
    let mut rows = db.recent_messages(limit);
    rows.reverse();
    if rows.is_empty() {
        println!("(no messages in {})", cfg.db_path);
        return 0;
    }
    for m in rows {
        let (from, to) = if m.direction == "out" {
            ("me".to_string(), m.peer_id.clone())
        } else {
            (m.peer_id.clone(), "me".to_string())
        };
        println!(
            "{:>10} {:<9} {} -> {}: {}",
            m.created_at,
            m.status,
            from,
            to,
            m.body.replace('\n', " ")
        );
    }
    0
}

fn peers(args: &[String]) -> i32 {
    let cfg = Config::load(&config_path(args));
    let db = match Db::open(&cfg.db_path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("[!] cannot open {}: {e}", cfg.db_path);
            return 1;
        }
    };
    let peers = db.list_peers();
    if peers.is_empty() {
        println!("(no contacts)");
        return 0;
    }
    for p in peers {
        println!("{:<10} {:<20} via {:<7} {}", p.id, p.name, p.via, p.addr);
    }
    0
}

fn id(args: &[String]) -> i32 {
    let cfg = Config::load(&config_path(args));
    let (node, _db) = open_node(cfg.clone());
    println!("node id     {}", cfg.node_id);
    println!(
        "routing tag {}",
        node.tag.as_deref().unwrap_or("(needs a shared_key)")
    );
    println!("fingerprint {}", node.fingerprint());
    0
}

#[cfg(feature = "gui")]
fn gui(args: &[String]) -> i32 {
    dns_lab::app::run(&config_path(args));
    0
}

#[cfg(not(feature = "gui"))]
fn gui(_args: &[String]) -> i32 {
    eprintln!("[!] This binary was built without the `gui` feature. Use `dns_lab serve`.");
    1
}
