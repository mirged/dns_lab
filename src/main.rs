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
        "voice" => voice(&args),
        "inbox" => inbox(&args),
        "files" => files(&args),
        "peers" => peers(&args),
        "id" => id(&args),
        "tag" => tag(&args),
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
  dns_lab run    [--config FILE] [--voice-save DIR]
                                              Headless client daemon (listener + poller)
  dns_lab send   --to ID [--body TEXT|-]      Send one message from the CLI
  dns_lab send   --to ID --file PATH          Send a file (resumes if re-run)
  dns_lab inbox  [--limit N]                  Show recent messages
  dns_lab files  [--limit N]                  Show file transfers
  dns_lab voice listen [--save DIR]           Play (or save) incoming push-to-talk voice
  dns_lab voice send   --to ID --wav FILE     Send a WAV file as a voice transmission
  dns_lab voice test   (--wav FILE|--tone S) [--out F]
                                              Encode+decode offline, no network
  dns_lab peers  [--config FILE]              List known contacts
  dns_lab id     [--config FILE]              Print this node id
  dns_lab tag    --for ID [--config FILE]     Print the routing tag ID must answer
  dns_lab version                             Show the version

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

    // `--voice-save DIR` turns the daemon into a voice recorder: incoming
    // push-to-talk is decoded and written as `<dir>/voice-<stream>-<ts>.wav`.
    #[allow(unused_variables)]
    let voice = arg_value(args, "--voice-save");
    if let Some(dir) = &voice {
        println!("  Voice:     decoding to {dir}");
    }
    #[cfg(feature = "audio")]
    let _voice = voice.clone().map(|dir| {
        dns_lab::voice::audio::listen(&node, &cfg, Some(dir))
            .map_err(|e| eprintln!("[!] voice receiver: {e}"))
            .ok()
    });
    #[cfg(not(feature = "audio"))]
    let _voice = voice.clone().map(|dir| {
        println!("  Voice:     audio not compiled in, saving only");
        dns_lab::voice::start_receiver(&node, None, Some(dir))
    });
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

// ---------------------------------------------------------------------------
// voice
// ---------------------------------------------------------------------------

fn voice(args: &[String]) -> i32 {
    match args.get(1).map(|s| s.as_str()).unwrap_or("listen") {
        "listen" | "rx" | "recv" => voice_listen(args),
        "send" | "tx" => voice_send(args),
        "test" => voice_test(args),
        other => {
            eprintln!("[!] unknown `voice` subcommand: {other}");
            eprintln!("    voice listen | voice send --to ID --wav FILE | voice test --wav FILE");
            2
        }
    }
}

/// Read a WAV, downmix to 8 kHz mono, and report what it contains.
fn load_wav(path: &str) -> Result<(Vec<i16>, dns_lab::voice::wav::Wav), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read {path}: {e}"))?;
    let wav = dns_lab::voice::wav::read(&bytes).map_err(|e| format!("{path}: {e}"))?;
    let pcm = dns_lab::voice::wav::to_mono_8k(&wav);
    if pcm.is_empty() {
        return Err(format!("{path} contains no audio"));
    }
    Ok((pcm, wav))
}

fn voice_mode(cfg: &Config) -> dns_lab::voice::Mode {
    dns_lab::voice::Mode::from_bitrate(cfg.voice_mode)
}

/// Voiced-sounding test signal: a 140 Hz buzz with harmonics and a syllable
/// envelope. Codec 2 is a speech vocoder, so a pure sine is a poor test.
fn synth_voice(seconds: f32) -> Vec<i16> {
    use std::f32::consts::TAU;
    let n = (dns_lab::voice::SAMPLE_RATE as f32 * seconds) as usize;
    (0..n)
        .map(|i| {
            let t = i as f32 / dns_lab::voice::SAMPLE_RATE as f32;
            let syllabic = 0.55 + 0.45 * (TAU * 3.2 * t).sin();
            let mut v = 0.0f32;
            for (freq, amp) in [(140.0, 0.5), (280.0, 0.3), (560.0, 0.2), (1120.0, 0.12)] {
                v += amp * (TAU * freq * t).sin();
            }
            (v * syllabic * 12_000.0).clamp(-32_768.0, 32_767.0) as i16
        })
        .collect()
}

/// Encode a WAV into packets, in memory. Shared by `send` and `test`.
fn encode_wav(cfg: &Config, pcm: &[i16]) -> Vec<dns_lab::voice::Packet> {
    let mut enc = dns_lab::voice::Encoder::new(voice_mode(cfg), cfg.voice_frames_per_packet);
    let mut packets = enc.push(pcm);
    if let Some(last) = enc.flush() {
        packets.push(last);
    }
    packets
}

fn voice_send(args: &[String]) -> i32 {
    let Some(to) = arg_value(args, "--to") else {
        eprintln!("[!] --to ID is required");
        return 2;
    };
    let Some(path) = arg_value(args, "--wav") else {
        eprintln!("[!] --wav FILE is required (use `voice test` to check a clip offline)");
        return 2;
    };
    let cfg = Config::load(&config_path(args));
    let (pcm, wav) = match load_wav(&path) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[!] {e}");
            return 1;
        }
    };
    let packets = encode_wav(&cfg, &pcm);
    let mode = voice_mode(&cfg);
    let speech_ms = pcm.len() as u64 * 1000 / dns_lab::voice::SAMPLE_RATE as u64;
    println!(
        "[*] {} ({}, {} Hz, {} ch) -> {:.1} s at codec2 {} bps",
        path,
        dns_lab::file::human_size(pcm.len() as u64 * 2),
        wav.rate,
        wav.channels,
        speech_ms as f32 / 1000.0,
        mode.bitrate()
    );
    println!(
        "[*] {} packet(s) of {} frames ({} ms) to {to}",
        packets.len(),
        cfg.voice_frames_per_packet,
        cfg.voice_frames_per_packet.max(1) as u32 * dns_lab::voice::FRAME_MS
    );

    let (node, _db) = open_node(cfg);
    let mut queries = 0usize;
    let mut failed = 0usize;
    for packet in &packets {
        match node.send_voice(&to, packet) {
            Ok(q) => {
                queries += q;
                print!(
                    "\r    packet {}/{} ({} queries)",
                    packet.seq + 1,
                    packets.len(),
                    queries
                );
                use std::io::Write;
                let _ = std::io::stdout().flush();
            }
            Err(e) => {
                failed += 1;
                eprintln!("\n[!] packet {} failed: {e}", packet.seq);
            }
        }
    }
    println!();
    if failed == packets.len() {
        eprintln!("[!] every packet failed");
        return 1;
    }
    println!(
        "[+] voice sent: {} queries, {} packet(s) failed",
        queries, failed
    );
    0
}

/// Encode and immediately decode a file: proves the codec path works without
/// any network or audio device.
fn voice_test(args: &[String]) -> i32 {
    let cfg = Config::load(&config_path(args));
    let (pcm, wav) = match (arg_value(args, "--wav"), arg_value(args, "--tone")) {
        (Some(path), _) => match load_wav(&path) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[!] {e}");
                return 1;
            }
        },
        (None, Some(secs)) => {
            let seconds: f32 = secs.parse::<f32>().unwrap_or(1.0).clamp(0.05, 30.0);
            let samples = synth_voice(seconds);
            (
                samples.clone(),
                dns_lab::voice::wav::Wav {
                    rate: dns_lab::voice::SAMPLE_RATE,
                    channels: 1,
                    samples,
                },
            )
        }
        (None, None) => {
            eprintln!("[!] pass --wav FILE or --tone SECONDS");
            return 2;
        }
    };
    let path = arg_value(args, "--wav")
        .unwrap_or_else(|| format!("a {:.1} s tone", wav.duration_ms() as f32 / 1000.0));
    let packets = encode_wav(&cfg, &pcm);
    let coded: usize = packets.iter().map(|p| p.data.len()).sum();
    let speech_ms = pcm.len() as u64 * 1000 / dns_lab::voice::SAMPLE_RATE as u64;

    let mut stream = dns_lab::voice::Stream::new();
    let mut decoded = Vec::new();
    for packet in &packets {
        // Through the wire format and back, exactly as the receiver sees it.
        let body = packet.encode();
        let parsed = match dns_lab::voice::Packet::decode(&body) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[!] packet {} did not round trip: {e}", packet.seq);
                return 1;
            }
        };
        decoded.extend(stream.push(&parsed, std::time::Instant::now()).pcm);
        // A hole that never fills would hold the tail back; the test has none.
        decoded.extend(stream.tick(std::time::Instant::now()));
    }

    println!("[+] {} -> codec2 {} bps", path, voice_mode(&cfg).bitrate());
    println!(
        "    input   {:.2} s @ {} Hz, {} channel(s)",
        wav.duration_ms() as f32 / 1000.0,
        wav.rate,
        wav.channels
    );
    println!(
        "    coded   {coded} bytes in {} packet(s), {:.1}% of 8 kHz 16-bit PCM",
        packets.len(),
        coded as f32 * 100.0 / (pcm.len() as f32 * 2.0).max(1.0)
    );
    println!(
        "    decoded {:.2} s ({} frames){}",
        decoded.len() as f32 / dns_lab::voice::SAMPLE_RATE as f32,
        stream.played_frames,
        if stream.filled_frames > 0 {
            format!(", {} frame(s) filled", stream.filled_frames)
        } else {
            String::new()
        }
    );
    println!(
        "    airtime ~{} DNS quer{} at ~125 bytes per query",
        packets.len() * 2,
        if packets.len() * 2 == 1 { "y" } else { "ies" }
    );
    if let Some(out) = arg_value(args, "--out") {
        let bytes = dns_lab::voice::wav::write(&decoded, dns_lab::voice::SAMPLE_RATE);
        match std::fs::write(&out, bytes) {
            Ok(()) => println!("[+] wrote {out}"),
            Err(e) => {
                eprintln!("[!] cannot write {out}: {e}");
                return 1;
            }
        }
    }
    let expected = packets.iter().map(|p| p.frames as u64).sum::<u64>() * 20;
    if speech_ms.abs_diff(expected) > 40 {
        eprintln!("[!] coded duration {expected} ms does not match the input {speech_ms} ms");
        return 1;
    }
    0
}

fn voice_listen(args: &[String]) -> i32 {
    let cfg = Config::load(&config_path(args));
    let save = arg_value(args, "--save");
    let (node, _db) = open_node(cfg.clone());
    node.start();

    println!("DNS Lab voice listener {VERSION}");
    println!("  Node id:   {}", cfg.node_id);
    println!(
        "  Codec:     codec2 {} bps, {} frames per packet",
        voice_mode(&cfg).bitrate(),
        cfg.voice_frames_per_packet
    );
    println!(
        "  Saving:    {}",
        save.clone()
            .unwrap_or_else(|| "(no --save directory)".to_string())
    );

    #[cfg(feature = "audio")]
    let _listen = match dns_lab::voice::audio::listen(&node, &cfg, save) {
        Ok(l) => Some(l),
        Err(e) => {
            eprintln!("[!] cannot start the audio path: {e}");
            eprintln!("    hint: pass --save DIR to record transmissions instead of playing them");
            return 1;
        }
    };
    #[cfg(not(feature = "audio"))]
    {
        if save.is_none() {
            eprintln!(
                "[!] built without the `audio` feature: pass --save DIR to record voice to disk"
            );
            return 2;
        }
        let _keep = dns_lab::voice::start_receiver(&node, None, save);
        println!("  Audio:     not compiled in (heads-up: --save only)");
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        }
    }
    #[cfg(feature = "audio")]
    {
        println!("[+] listening - hold to talk on the other device");
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
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

/// The tag the *other* node must answer with. Each node's tag is
/// `HKDF(shared_key, its own node_id)`, so two peers legitimately show
/// different tags on their own screens; the pair only matches when the key and
/// the ids agree. Cheap enough to compute without opening the node (no listener
/// bind, no database), so it is safe to run while the GUI holds the port.
fn tag(args: &[String]) -> i32 {
    let cfg = Config::load(&config_path(args));
    let Some(to) = arg_value(args, "--for") else {
        eprintln!("[!] --for ID is required (the peer's node id)");
        return 2;
    };
    let Some(key) = cfg.key_opt() else {
        eprintln!("[!] no shared_key in this config: nothing to derive a tag from");
        return 1;
    };
    println!("{}", dns_lab::routing_tag(&key, &to));
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
