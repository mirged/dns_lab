use dns_lab::config::Config;
use dns_lab::db::Db;
use dns_lab::net::{AppEvent, Node};
use std::sync::Arc;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("gui");
    match cmd {
        "serve" => serve(&args),
        "send" => send(&args),
        "id" => id(&args),
        "gui" => gui(&args),
        _ => usage(),
    }
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
        "dns_lab 0.2.0 - DNS-tunnel messenger\n\n\
         USAGE:\n\
           dns_lab gui   [--config FILE]        Launch the desktop app (default)\n\
           dns_lab serve [--config FILE]        Run the headless authoritative relay (VPS)\n\
           dns_lab send  --to ID --body TEXT    Send one message from the CLI\n\
           dns_lab id    [--config FILE]        Print this node id\n"
    );
}

/// Console drain for headless runs (node logs are already printed to stderr).
fn drain_logs(rx: std::sync::mpsc::Receiver<AppEvent>) {
    std::thread::spawn(move || while rx.recv().is_ok() {});
}

fn serve(args: &[String]) {
    let path = config_path(args);
    let mut cfg = Config::load(&path);
    cfg.authoritative = true;

    let db = Arc::new(Db::open(&cfg.db_path).expect("open database"));
    let (tx, rx) = std::sync::mpsc::channel();
    drain_logs(rx);

    let node = Node::new(cfg.clone(), db, tx);
    node.start();

    println!("==================================================");
    println!("  DNS Lab authoritative relay online");
    println!("  Zone:      {}", cfg.zone);
    println!("  Listen:    {} (UDP + TCP)", cfg.listen_addr);
    println!("  Database:  {}", cfg.db_path);
    println!("  Node id:   {}", cfg.node_id);
    println!("==================================================");

    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

fn send(args: &[String]) {
    let path = config_path(args);
    let cfg = Config::load(&path);
    let to = match arg_value(args, "--to") {
        Some(v) => v,
        None => {
            eprintln!("[!] --to ID is required (see `dns_lab id` on the peer)");
            std::process::exit(1);
        }
    };
    let body = arg_value(args, "--body").unwrap_or_else(|| "hello over dns".to_string());

    let db = Arc::new(Db::open(&cfg.db_path).expect("open database"));
    let (tx, rx) = std::sync::mpsc::channel();
    drain_logs(rx);
    let node = Node::new(cfg, db, tx);

    let msg_id = dns_lab::random_id();
    println!("[*] sending {msg_id} to {to}: {body:?}");
    match node.do_send(&to, &body, &msg_id) {
        Ok(chunks) => println!("[+] delivered in {chunks} DNS queries"),
        Err(e) => {
            eprintln!("[!] send failed: {e}");
            std::process::exit(1);
        }
    }
}

fn id(args: &[String]) {
    let cfg = Config::load(&config_path(args));
    println!("{}", cfg.node_id);
}

#[cfg(feature = "gui")]
fn gui(args: &[String]) {
    dns_lab::app::run(&config_path(args));
}

#[cfg(not(feature = "gui"))]
fn gui(_args: &[String]) {
    eprintln!("[!] This binary was built without the `gui` feature. Use `dns_lab serve`.");
    std::process::exit(1);
}
