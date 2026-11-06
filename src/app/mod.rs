//! egui/eframe client: conversation list + chat, setup, and a DNS inspector.

use crate::config::{Config, EgressKind, Peer, RelayMode};
use crate::db::{Db, MessageRow, Stats};
use crate::net::{AppEvent, Node};
use eframe::egui;
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

#[cfg(target_os = "android")]
type PlatformHandle = android_activity::AndroidApp;
#[cfg(not(target_os = "android"))]
type PlatformHandle = ();

#[derive(PartialEq, Eq, Clone, Copy)]
enum Tab {
    Chats,
    Setup,
    Log,
}

#[derive(Clone)]
struct Chat {
    id: String,
    name: String,
    last: String,
    ts: i64,
}

pub struct App {
    config_path: String,
    cfg: Config,
    db: Option<Arc<Db>>,
    node: Option<Arc<Node>>,
    events: Option<Receiver<AppEvent>>,
    tab: Tab,

    chats: Vec<Chat>,
    selected: Option<String>,
    messages: Vec<MessageRow>,
    stats: Stats,
    input: String,
    /// Path typed into the attach box (no native file dialog, so a text field).
    attach_path: String,
    logs: Vec<String>,
    status: String,
    /// Set when the message box should grab keyboard focus next frame.
    focus_input: bool,

    /// Filter box for the conversation list.
    chat_filter: String,
    /// Filter box for the DNS inspector.
    log_filter: String,

    new_id: String,
    new_name: String,
    new_addr: String,
    new_via: String,

    last_refresh: Instant,
    dirty: bool,
    /// Platform density scale on Android, used to detect a missing scale factor.
    android_scale: Option<f32>,
    /// Native Android handle, used for window insets (status/navigation bars).
    #[allow(dead_code)]
    platform: Option<PlatformHandle>,
    zoom_applied: bool,
}

impl App {
    pub(crate) fn new(config_path: String) -> Self {
        let cfg = Config::load(&config_path);
        let mut app = App {
            config_path,
            cfg,
            db: None,
            node: None,
            events: None,
            tab: Tab::Chats,
            chats: Vec::new(),
            selected: None,
            messages: Vec::new(),
            stats: Stats::default(),
            input: String::new(),
            attach_path: String::new(),
            logs: Vec::new(),
            status: "not started".to_string(),
            focus_input: false,
            chat_filter: String::new(),
            log_filter: String::new(),
            new_id: String::new(),
            new_name: String::new(),
            new_addr: String::new(),
            new_via: "relay".to_string(),
            last_refresh: Instant::now(),
            dirty: true,
            android_scale: None,
            platform: None,
            zoom_applied: false,
        };
        app.start();
        app
    }

    /// Hand the NativeActivity handle to the app (Android) for window insets.
    #[cfg(target_os = "android")]
    pub(crate) fn set_android_app(&mut self, app: android_activity::AndroidApp) {
        self.android_scale = Some(
            app.config()
                .density()
                .map(|d| d as f32 / 160.0)
                .unwrap_or(2.0),
        );
        self.platform = Some(app);
    }

    /// Safe-area insets in points as `(left, right, top, bottom)`.
    #[cfg(target_os = "android")]
    fn insets(&self, ctx: &egui::Context) -> (f32, f32, f32, f32) {
        let Some(app) = &self.platform else {
            return (0.0, 0.0, 24.0, 0.0);
        };
        let rect = app.content_rect();
        let ppp = ctx.pixels_per_point().max(0.1);
        let screen = ctx.viewport_rect();
        let left = (rect.left as f32 / ppp).max(0.0);
        let top = (rect.top as f32 / ppp).max(0.0);
        let right = (screen.width() - rect.right as f32 / ppp).max(0.0);
        let bottom = (screen.height() - rect.bottom as f32 / ppp).max(0.0);
        if left + right + top + bottom < 0.5 {
            // The platform reported no insets; keep clear of the status bar.
            (0.0, 0.0, 24.0, 0.0)
        } else {
            (left, right, top, bottom)
        }
    }

    fn bump_scale(&mut self, delta: f32) {
        self.cfg.ui_scale = (self.cfg.ui_scale + delta).clamp(0.5, 4.0);
        let _ = self.cfg.save(&self.config_path);
    }

    fn log(&mut self, line: String) {
        self.logs.push(line);
        if self.logs.len() > 2000 {
            self.logs.drain(0..500);
        }
        self.dirty = true;
    }

    fn start(&mut self) {
        let _ = self.cfg.save(&self.config_path);
        match Db::open(&self.cfg.db_path) {
            Ok(db) => {
                let db = Arc::new(db);
                let (tx, rx) = std::sync::mpsc::channel();
                self.events = Some(rx);
                let node = match Node::new(self.cfg.clone(), db.clone(), tx) {
                    Ok(n) => n,
                    Err(e) => {
                        self.status = format!("identity error: {e}");
                        self.log(format!("[!] could not initialise the node identity: {e}"));
                        return;
                    }
                };
                node.start();
                self.log(format!(
                    "[+] node {} on {} | zone {} | egress {} | {}",
                    self.cfg.node_id,
                    self.cfg.listen_addr,
                    self.cfg.zone,
                    self.cfg.egress.as_str(),
                    if self.cfg.authoritative {
                        "relay (authoritative)"
                    } else {
                        "client"
                    }
                ));
                if !self.cfg.authoritative
                    && self.cfg.relay.is_none()
                    && self.cfg.direct_server.is_none()
                {
                    self.log(
                        "[!] no relay / direct_server configured: outbound mail has nowhere to go"
                            .to_string(),
                    );
                }
                self.status = format!("online as {}", self.cfg.node_id);
                self.db = Some(db);
                self.node = Some(node);
                self.dirty = true;
            }
            Err(e) => {
                self.status = format!("db error: {e}");
            }
        }
    }

    fn stop(&mut self) {
        if let Some(node) = &self.node {
            node.stop();
        }
        self.node = None;
        self.db = None;
        self.events = None;
        self.status = "stopped".to_string();
    }

    fn restart(&mut self) {
        self.stop();
        // Give listeners a moment to release the port before rebinding.
        std::thread::sleep(Duration::from_millis(150));
        self.start();
        self.log("[*] node restarted".to_string());
    }

    fn pump_events(&mut self) {
        let mut got = false;
        if let Some(rx) = &self.events {
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    AppEvent::Message => got = true,
                    AppEvent::Log(line) => {
                        self.logs.push(line);
                        if self.logs.len() > 2000 {
                            self.logs.drain(0..500);
                        }
                    }
                }
            }
        }
        if got {
            self.dirty = true;
        }
    }

    fn refresh(&mut self) {
        let db = match &self.db {
            Some(db) => db.clone(),
            None => return,
        };
        let mut map: Vec<Chat> = db
            .list_peers()
            .into_iter()
            .map(|p| Chat {
                name: if p.name.is_empty() {
                    p.id.clone()
                } else {
                    p.name
                },
                id: p.id,
                last: String::new(),
                ts: 0,
            })
            .collect();
        for c in db.conversations() {
            match map.iter_mut().find(|x| x.id == c.peer_id) {
                Some(existing) => {
                    existing.last = c.last_body;
                    existing.ts = c.last_ts;
                }
                None => map.push(Chat {
                    name: if c.peer_name.is_empty() {
                        c.peer_id.clone()
                    } else {
                        c.peer_name
                    },
                    id: c.peer_id,
                    last: c.last_body,
                    ts: c.last_ts,
                }),
            }
        }
        // Busiest conversation first; contacts without traffic after that.
        map.sort_by(|a, b| b.ts.cmp(&a.ts).then_with(|| a.name.cmp(&b.name)));
        self.chats = map;
        // Open the most recent conversation when nothing is selected: landing on
        // "pick a contact" every launch is a dead end.
        if self.selected.is_none()
            && let Some(first) = self.chats.first()
        {
            self.selected = Some(first.id.clone());
        }
        if let Some(sel) = self.selected.clone() {
            self.messages = db.messages_for(&sel, 500);
        }
        let own_route = self
            .node
            .as_ref()
            .and_then(|n| n.tag.clone())
            .unwrap_or_default();
        self.stats = db.stats(&own_route);
        self.dirty = false;
    }

    fn add_peer(&mut self) {
        let id = self.new_id.trim().to_string();
        if id.is_empty() {
            self.log("[!] contact id is required".to_string());
            return;
        }
        if self.chats.iter().any(|c| c.id == id) {
            self.log(format!("[!] contact {id} already exists"));
            return;
        }
        let peer = Peer {
            name: if self.new_name.trim().is_empty() {
                id.clone()
            } else {
                self.new_name.trim().to_string()
            },
            addr: self.new_addr.trim().to_string(),
            via: self.new_via.clone(),
            id: id.clone(),
        };
        self.cfg.peers.push(peer.clone());
        let _ = self.cfg.save(&self.config_path);
        if let Some(db) = &self.db {
            db.upsert_peer(&peer.id, &peer.name, &peer.addr, &peer.via);
        }
        self.log(format!("[+] contact {} ({})", peer.name, peer.id));
        self.new_id.clear();
        self.new_name.clear();
        self.new_addr.clear();
        self.dirty = true;
    }

    /// Send the file named in the attach box. The transfer runs on its own
    /// thread: a large file is hundreds of queries and must not freeze the UI.
    fn send_attached(&mut self) {
        let Some(peer) = self.selected.clone() else {
            return;
        };
        let path = self.attach_path.trim().to_string();
        let Some(node) = self.node.clone() else {
            self.log("[!] cannot send: node is stopped".to_string());
            return;
        };
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(e) => {
                self.log(format!("[!] cannot read {path}: {e}"));
                return;
            }
        };
        let name = std::path::Path::new(&path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "file".to_string());
        let transfer_id = crate::file::digest(&data)[..8].to_string();
        let parts = crate::file::part_count(data.len() as u64, crate::file::DEFAULT_PART_BYTES);
        self.log(format!(
            "[*] sending {name} ({}) to {peer}: {parts} part(s)",
            crate::file::human_size(data.len() as u64)
        ));
        self.attach_path.clear();
        self.dirty = true;

        std::thread::spawn(move || {
            let mut last = 0u32;
            let result = node.send_file(&peer, &name, &data, &transfer_id, |done, total| {
                // One log line per 10%, not one per part.
                let pct = (done * 100).checked_div(total).unwrap_or(100);
                if pct >= last + 10 || done == total {
                    last = pct;
                    node.log(format!("    {name}: {done}/{total} parts ({pct}%)"));
                }
            });
            match result {
                Ok(sent) => node.log(format!(
                    "[+] {name} sent ({} parts, {} resumed)",
                    sent.parts, sent.skipped
                )),
                Err(e) => node.log(format!("[!] {name} failed: {e}")),
            }
        });
    }

    fn send_current(&mut self) {
        let peer = match &self.selected {
            Some(p) => p.clone(),
            None => return,
        };
        let body = self.input.trim().to_string();
        if body.is_empty() {
            return;
        }
        match &self.node {
            Some(node) => node.send_message(&peer, &body),
            None => {
                self.status = "node is stopped".to_string();
                self.log("[!] cannot send: node is stopped".to_string());
                return;
            }
        }
        self.input.clear();
        self.focus_input = true;
        self.dirty = true;
    }

    // ---- UI ------------------------------------------------------------

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(&mut self.tab, Tab::Chats, "Chats");
            ui.selectable_value(&mut self.tab, Tab::Setup, "Setup");
            ui.selectable_value(&mut self.tab, Tab::Log, "DNS log");
            ui.separator();
            if ui
                .small_button("A-")
                .on_hover_text("Smaller text")
                .clicked()
            {
                self.bump_scale(-0.1);
            }
            ui.label(format!("{:.0}%", self.cfg.ui_scale * 100.0));
            if ui.small_button("A+").on_hover_text("Larger text").clicked() {
                self.bump_scale(0.1);
            }
            ui.separator();
            ui.label(format!("me: {}", self.cfg.node_id));
            ui.separator();
            // Painted dot: the bundled default font has no "●" glyph, so a
            // RichText marker would render as a tofu box.
            let online = self.node.is_some();
            let color = if online {
                egui::Color32::from_rgb(90, 180, 110)
            } else {
                egui::Color32::from_rgb(200, 90, 90)
            };
            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(10.0, ui.spacing().interact_size.y),
                egui::Sense::hover(),
            );
            ui.painter().circle_filled(rect.center(), 4.0, color);
            ui.label(egui::RichText::new(if online { "online" } else { "stopped" }).color(color));
        });
    }

    fn chat_list(&mut self, ui: &mut egui::Ui) {
        ui.heading("Conversations");
        ui.add(
            egui::TextEdit::singleline(&mut self.chat_filter)
                .hint_text("filter…")
                .desired_width(f32::INFINITY),
        );
        ui.separator();

        if self.chats.is_empty() {
            ui.weak("No contacts yet.");
            ui.weak("Add one under Setup > Contacts.");
            return;
        }

        let filter = self.chat_filter.to_lowercase();
        let chats = self.chats.clone();
        let mut pick: Option<String> = None;
        egui::ScrollArea::vertical()
            .id_salt("chat_list")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for c in &chats {
                    if !filter.is_empty()
                        && !c.name.to_lowercase().contains(&filter)
                        && !c.id.contains(&filter)
                    {
                        continue;
                    }
                    let selected = self.selected.as_deref() == Some(c.id.as_str());
                    let label = if c.last.is_empty() {
                        format!("{}\n   {}", c.name, c.id)
                    } else {
                        format!("{}\n   {}", c.name, truncate(&c.last, 34))
                    };
                    // Full-width rows are easier to hit than a bare label.
                    let row = ui.allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), 36.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| ui.selectable_label(selected, label),
                    );
                    if row.inner.clicked() {
                        pick = Some(c.id.clone());
                    }
                }
            });
        if let Some(id) = pick {
            self.selected = Some(id);
            self.focus_input = true;
            self.dirty = true;
        }
    }

    fn conversation(&mut self, ui: &mut egui::Ui) {
        let Some(sel) = self.selected.clone() else {
            ui.heading("No conversation selected");
            ui.weak("Pick a contact on the left, or add one under Setup > Contacts.");
            return;
        };

        let (name, id) = self
            .chats
            .iter()
            .find(|c| c.id == sel)
            .map(|c| (c.name.clone(), c.id.clone()))
            .unwrap_or_else(|| (sel.clone(), sel.clone()));

        ui.horizontal(|ui| {
            ui.heading(&name);
            if name != id {
                ui.weak(format!("({id})"));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .small_button("copy id")
                    .on_hover_text("Copy this contact's node id")
                    .clicked()
                {
                    ui.ctx().copy_text(id.clone());
                }
            });
        });
        ui.separator();

        // Composer first, pinned to the bottom: if the transcript is laid out
        // first it eats all remaining height and pushes the input off-screen.
        egui::Panel::bottom("composer")
            .show_separator_line(true)
            .show(ui, |ui| {
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    let width = (ui.available_width() - 70.0).max(80.0);
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.input)
                            .hint_text("message - Enter sends")
                            .desired_width(width),
                    );
                    if self.focus_input {
                        resp.request_focus();
                        self.focus_input = false;
                    }
                    // egui's single-line TextEdit surrenders focus the moment
                    // Enter arrives, so `has_focus()` is already false here; the
                    // idiomatic check is `lost_focus() && Enter`.
                    let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if ui.button("Send").clicked() || enter {
                        self.send_current();
                    }
                });
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.attach_path)
                            .hint_text("path to a file - sends as parts")
                            .desired_width((ui.available_width() - 90.0).max(80.0)),
                    );
                    if ui
                        .button("Send file")
                        .on_hover_text("Chunked, signed and verified end to end")
                        .clicked()
                    {
                        self.send_attached();
                    }
                });
                ui.add_space(2.0);
            });

        let now = crate::db::now_secs();
        let mut retry: Option<String> = None;
        egui::ScrollArea::vertical()
            .id_salt("messages")
            .stick_to_bottom(true)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if self.messages.is_empty() {
                    ui.weak("No messages yet. Each line you send rides one or more DNS queries.");
                    return;
                }
                for m in &self.messages {
                    let out = m.direction == "out";
                    ui.horizontal(|ui| {
                        ui.monospace(egui::RichText::new(stamp(m.created_at, now)).weak());
                        let who = if out { "me" } else { m.peer_name.as_str() };
                        ui.label(egui::RichText::new(who).strong());
                        if out {
                            // Words, not glyphs: the bundled font has no ✓/✕.
                            let (label, color, tip) = match m.status.as_str() {
                                "delivered" => (
                                    "delivered",
                                    egui::Color32::from_rgb(90, 180, 110),
                                    "peer confirmed receipt",
                                ),
                                "sent" => (
                                    "sent",
                                    egui::Color32::from_rgb(150, 160, 180),
                                    "relay accepted every chunk",
                                ),
                                "failed" => (
                                    "failed",
                                    egui::Color32::from_rgb(200, 90, 90),
                                    "send failed",
                                ),
                                _ => (
                                    "sending",
                                    egui::Color32::from_rgb(150, 160, 180),
                                    "queued for sending",
                                ),
                            };
                            ui.label(egui::RichText::new(label).small().color(color))
                                .on_hover_text(tip);
                        }
                        if out && m.status == "failed" && ui.small_button("retry").clicked() {
                            retry = Some(m.body.clone());
                        }
                    });
                    ui.label(&m.body);
                    ui.add_space(6.0);
                }
            });

        if let Some(body) = retry
            && let Some(node) = &self.node
        {
            node.send_message(&sel, &body);
            self.log(format!("[*] retrying message to {sel}"));
        }
    }

    fn setup_ui(&mut self, ui: &mut egui::Ui) {
        ui.heading("Setup");
        ui.weak(format!("Config file: {}", self.config_path));
        if !self.cfg.authoritative
            && self.cfg.egress == EgressKind::Resolver
            && self.cfg.resolver.is_none()
        {
            ui.colored_label(
                egui::Color32::from_rgb(200, 160, 60),
                "egress = resolver but no resolver address is set",
            );
        }
        if self.cfg.key_opt().is_none() {
            ui.colored_label(
                egui::Color32::from_rgb(200, 90, 90),
                "no shared_key: this node cannot send, receive or route mail",
            );
        } else if self.cfg.key_is_placeholder() {
            ui.colored_label(
                egui::Color32::from_rgb(200, 160, 60),
                "shared_key is the example placeholder — everyone with this repo can read your mail",
            );
        }

        egui::ScrollArea::vertical()
            .id_salt("setup")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(4.0);
                ui.label(egui::RichText::new("Identity").strong());
                egui::Grid::new("cfg_identity")
                    .num_columns(2)
                    .spacing([12.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("Display name");
                        ui.text_edit_singleline(&mut self.cfg.display_name);
                        ui.end_row();

                        ui.label("Node id");
                        ui.horizontal(|ui| {
                            ui.text_edit_singleline(&mut self.cfg.node_id);
                            if ui
                                .small_button("new")
                                .on_hover_text("Generate a fresh id")
                                .clicked()
                            {
                                self.cfg.node_id = crate::random_id();
                            }
                            if ui.small_button("copy").clicked() {
                                ui.ctx().copy_text(self.cfg.node_id.clone());
                            }
                        });
                        ui.end_row();

                        ui.label("Routing tag");
                        ui.horizontal(|ui| {
                            // What the relay actually sees: a keyed 8-hex tag,
                            // not the node id above.
                            match self.node.as_ref().and_then(|n| n.tag.clone()) {
                                Some(tag) => {
                                    ui.monospace(&tag);
                                    if ui.small_button("copy").clicked() {
                                        ui.ctx().copy_text(tag.clone());
                                    }
                                }
                                None => {
                                    ui.weak("(needs a shared_key)");
                                }
                            }
                        });
                        ui.end_row();

                        ui.label("Key fingerprint");
                        ui.horizontal(|ui| {
                            match self.node.as_ref() {
                                Some(n) => {
                                    let fp = n.fingerprint();
                                    ui.monospace(&fp)
                                        .on_hover_text("Ed25519 key of this node; compare out of band");
                                    if ui.small_button("copy").clicked() {
                                        ui.ctx().copy_text(fp);
                                    }
                                }
                                None => {
                                    ui.weak("(node not running)");
                                }
                            }
                        });
                        ui.end_row();

                        ui.label("Shared key");
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.cfg.shared_key)
                                    .desired_width(240.0)
                                    .password(true),
                            );
                            if ui
                                .small_button("generate")
                                .on_hover_text(
                                    "Random 32-byte base32 secret — every node needs the same one",
                                )
                                .clicked()
                            {
                                self.cfg.shared_key = crate::random_secret();
                            }
                        });
                        ui.end_row();
                    });

                ui.separator();
                ui.label(egui::RichText::new("Transport").strong());
                egui::Grid::new("cfg_transport")
                    .num_columns(2)
                    .spacing([12.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("Zone / suffix");
                        ui.text_edit_singleline(&mut self.cfg.zone);
                        ui.end_row();

                        ui.label("Listen address");
                        ui.text_edit_singleline(&mut self.cfg.listen_addr);
                        ui.end_row();

                        ui.label("Egress");
                        ui.horizontal(|ui| {
                            ui.selectable_value(&mut self.cfg.egress, EgressKind::Direct, "direct");
                            ui.selectable_value(
                                &mut self.cfg.egress,
                                EgressKind::Resolver,
                                "resolver",
                            );
                        });
                        ui.end_row();

                        ui.label("Resolver");
                        ui.text_edit_singleline(opt_string(&mut self.cfg.resolver));
                        ui.end_row();

                        ui.label("Direct / relay addr");
                        ui.text_edit_singleline(opt_string(&mut self.cfg.direct_server));
                        ui.end_row();

                        ui.label("Poll relay");
                        ui.text_edit_singleline(opt_string(&mut self.cfg.relay));
                        ui.end_row();

                        ui.label("Poll interval (s)");
                        ui.add(
                            egui::DragValue::new(&mut self.cfg.poll_interval_secs).range(1..=3600),
                        );
                        ui.end_row();

                        ui.label("Pipelining window");
                        ui.add(egui::DragValue::new(&mut self.cfg.window).range(1..=64))
                            .on_hover_text(
                                "Chunk queries in flight: one window of DNS round trips instead of one per chunk",
                            );
                        ui.end_row();

                        ui.label("EDNS0 UDP size");
                        ui.add(egui::DragValue::new(&mut self.cfg.edns_udp_size).range(512..=4096));
                        ui.end_row();

                        ui.label("Timeout / retries");
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::DragValue::new(&mut self.cfg.timeout_ms).range(100..=10_000),
                            );
                            ui.label("ms");
                            ui.add(egui::DragValue::new(&mut self.cfg.retries).range(0..=10));
                        });
                        ui.end_row();

                        ui.label("Role");
                        ui.horizontal(|ui| {
                            let mut mode = self.cfg.relay_mode;
                            for (m, label, hint) in [
                                (RelayMode::Client, "client", "send and poll only, never forward"),
                                (RelayMode::Peer, "peer", "forward mail for others, with quotas"),
                                (
                                    RelayMode::Authoritative,
                                    "authoritative",
                                    "forward, and answer SOA / NXDOMAIN for the zone",
                                ),
                            ] {
                                ui.selectable_value(&mut mode, m, label)
                                    .on_hover_text(hint);
                            }
                            if mode != self.cfg.relay_mode {
                                // `authoritative` is the derived flag the
                                // listener uses; the role is the one knob.
                                self.cfg.relay_mode = mode;
                                self.cfg.authoritative = mode == RelayMode::Authoritative;
                            }
                        });
                        ui.end_row();

                        ui.label("Quotas per tag");
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::DragValue::new(&mut self.cfg.max_blobs_per_route)
                                    .range(1..=100_000),
                            );
                            ui.weak("blobs");
                            ui.add(
                                egui::DragValue::new(&mut self.cfg.max_bytes_per_route)
                                    .range(1..=100_000_000),
                            );
                            ui.weak("bytes");
                            ui.add(
                                egui::DragValue::new(&mut self.cfg.rate_limit_per_sec)
                                    .range(0..=10_000),
                            );
                            ui.weak("queries/s per IP");
                        });
                        ui.end_row();

                        ui.label("Database");
                        ui.text_edit_singleline(&mut self.cfg.db_path);
                        ui.end_row();

                        ui.label("UI scale");
                        ui.add(
                            egui::DragValue::new(&mut self.cfg.ui_scale)
                                .range(0.5..=4.0)
                                .speed(0.05),
                        );
                        ui.end_row();
                    });

                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        match self.cfg.save(&self.config_path) {
                            Ok(()) => self.log("[+] config saved".to_string()),
                            Err(e) => self.log(format!("[!] save failed: {e}")),
                        }
                    }
                    if ui
                        .button("Restart node")
                        .on_hover_text("Apply key / transport changes")
                        .clicked()
                    {
                        self.restart();
                    }
                });

                ui.separator();
                ui.label(egui::RichText::new("Contacts").strong());
                egui::Grid::new("cfg_contacts")
                    .num_columns(2)
                    .spacing([12.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("id");
                        ui.text_edit_singleline(&mut self.new_id);
                        ui.end_row();

                        ui.label("name");
                        ui.text_edit_singleline(&mut self.new_name);
                        ui.end_row();

                        ui.label("addr (direct only)");
                        ui.text_edit_singleline(&mut self.new_addr);
                        ui.end_row();

                        ui.label("via");
                        ui.horizontal(|ui| {
                            ui.selectable_value(&mut self.new_via, "relay".to_string(), "relay");
                            ui.selectable_value(&mut self.new_via, "direct".to_string(), "direct");
                        });
                        ui.end_row();
                    });
                ui.horizontal(|ui| {
                    if ui.button("Add contact").clicked() {
                        self.add_peer();
                    }
                    ui.weak("relay = a mailbox server moves the mail, direct = peer ip:port");
                });
                ui.add_space(8.0);
            });
    }

    fn log_ui(&mut self, ui: &mut egui::Ui) {
        ui.heading("DNS inspector");
        let s = self.stats;
        ui.label(format!(
            "sent {} | received {} | delivered {} | failed {} | assembling {} | inbox queued {} | retry queue {}",
            s.sent, s.received, s.delivered, s.failed, s.pending_chunks, s.mailbox, s.outbox
        ));
        ui.weak(format!(
            "node {} | zone {} | {}{}",
            self.cfg.node_id,
            self.cfg.zone,
            self.status,
            if self.cfg.authoritative {
                " | authoritative"
            } else {
                ""
            }
        ));
        ui.separator();

        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.log_filter)
                    .hint_text("filter log…")
                    .desired_width(240.0),
            );
            if ui.small_button("clear").clicked() {
                self.logs.clear();
            }
            if ui.small_button("copy all").clicked() {
                ui.ctx().copy_text(self.logs.join("\n"));
            }
        });

        let filter = self.log_filter.to_lowercase();
        egui::ScrollArea::vertical()
            .id_salt("dns_log")
            .stick_to_bottom(true)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for line in &self.logs {
                    if !filter.is_empty() && !line.to_lowercase().contains(&filter) {
                        continue;
                    }
                    ui.monospace(line);
                }
            });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.pump_events();
        if self.dirty || self.last_refresh.elapsed() > Duration::from_secs(1) {
            self.refresh();
            self.last_refresh = Instant::now();
        }

        // On the first frame, if the platform failed to apply its density scale
        // (common on Android), compensate so the UI is touch-friendly. Otherwise
        // use a comfortable minimum zoom on phones.
        if !self.zoom_applied {
            if let Some(desired) = self.android_scale {
                let ppp = ui.ctx().pixels_per_point();
                if ppp > 0.0 && ppp < desired * 0.9 {
                    self.cfg.ui_scale = (desired / ppp).clamp(1.0, 4.0);
                } else if self.cfg.ui_scale <= 1.0 {
                    self.cfg.ui_scale = 1.4;
                }
            }
            self.zoom_applied = true;
        }
        let zoom = self.cfg.ui_scale.clamp(0.5, 4.0);
        if (ui.ctx().zoom_factor() - zoom).abs() > 0.001 {
            ui.ctx().set_zoom_factor(zoom);
        }

        // Keep content clear of the Android status/navigation bars so the top
        // controls stay tappable.
        #[cfg(target_os = "android")]
        {
            let (left, right, top, bottom) = self.insets(ui.ctx());
            if top > 0.5 {
                egui::Panel::top("safe_top")
                    .frame(egui::Frame::NONE)
                    .show_separator_line(false)
                    .exact_size(top)
                    .show(ui, |_| {});
            }
            if bottom > 0.5 {
                egui::Panel::bottom("safe_bottom")
                    .frame(egui::Frame::NONE)
                    .show_separator_line(false)
                    .exact_size(bottom)
                    .show(ui, |_| {});
            }
            if left > 0.5 {
                egui::Panel::left("safe_left")
                    .frame(egui::Frame::NONE)
                    .show_separator_line(false)
                    .exact_size(left)
                    .show(ui, |_| {});
            }
            if right > 0.5 {
                egui::Panel::right("safe_right")
                    .frame(egui::Frame::NONE)
                    .show_separator_line(false)
                    .exact_size(right)
                    .show(ui, |_| {});
            }
        }

        egui::Panel::top("tabs").show(ui, |ui| self.top_bar(ui));

        match self.tab {
            Tab::Chats => {
                egui::Panel::left("left")
                    .resizable(true)
                    .default_size(280.0)
                    .show(ui, |ui| self.chat_list(ui));
                egui::CentralPanel::default().show(ui, |ui| self.conversation(ui));
            }
            Tab::Setup => {
                egui::CentralPanel::default().show(ui, |ui| self.setup_ui(ui));
            }
            Tab::Log => {
                egui::CentralPanel::default().show(ui, |ui| self.log_ui(ui));
            }
        }

        ui.ctx().request_repaint_after(Duration::from_millis(500));
    }
}

fn opt_string(v: &mut Option<String>) -> &mut String {
    v.get_or_insert_with(String::new)
}

fn truncate(s: &str, n: usize) -> String {
    let flat = s.replace('\n', " ");
    if flat.chars().count() <= n {
        flat
    } else {
        let t: String = flat.chars().take(n).collect();
        format!("{t}…")
    }
}

/// `HH:MM` (UTC) for timestamps from today, `YYYY-MM-DD` before that.
fn stamp(ts: i64, now: i64) -> String {
    let day = ts.div_euclid(86_400);
    let secs = ts.rem_euclid(86_400);
    let hm = format!("{:02}:{:02}", secs / 3600, (secs % 3600) / 60);
    if day == now.div_euclid(86_400) {
        hm
    } else {
        let (y, m, d) = civil_from_days(day);
        format!("{y:04}-{m:02}-{d:02}")
    }
}

/// Days since the Unix epoch to a civil (year, month, day), UTC.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Launch the desktop app.
pub fn run(config_path: &str) {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1040.0, 680.0]),
        ..Default::default()
    };
    let path = config_path.to_string();
    let result = eframe::run_native(
        "DNS Messenger",
        options,
        Box::new(move |_cc| Ok(Box::new(App::new(path)))),
    );
    if let Err(e) = result {
        eprintln!(
            "[!] could not start the GUI: {e}\n    \
             hint: headless box? use `dns_lab run` for the same node without a window,\n    \
             or try WGPU_BACKEND=gl / LIBGL_ALWAYS_SOFTWARE=1"
        );
        std::process::exit(1);
    }
}

/// Native options for the Android backend (wires in the `AndroidApp` handle).
#[cfg(target_os = "android")]
pub fn android_options(app: android_activity::AndroidApp) -> eframe::NativeOptions {
    // Let Android size the surface to the screen; safe-area insets are applied
    // separately in `App::ui`.
    let mut options = eframe::NativeOptions::default();
    options.android_app = Some(app);
    options
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates_match_known_values() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_000), (2024, 10, 4));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
    }

    #[test]
    fn stamp_shows_time_today_and_date_otherwise() {
        let now = 1_700_000_000; // 2023-11-14T22:13:20Z
        assert_eq!(stamp(now, now), "22:13");
        assert_eq!(stamp(now - 86_400, now), "2023-11-13");
        assert_eq!(stamp(0, now), "1970-01-01");
    }

    #[test]
    fn truncate_flattens_newlines_and_ellipsizes() {
        assert_eq!(truncate("a\nb", 10), "a b");
        assert_eq!(truncate("abcdef", 3), "abc…");
        assert_eq!(truncate("abc", 3), "abc");
    }
}
