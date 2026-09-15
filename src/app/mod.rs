//! egui/eframe desktop client: setup, chats, conversation and DNS inspector.

use crate::config::{Config, EgressKind, Peer};
use crate::db::{Db, MessageRow};
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
    input: String,
    logs: Vec<String>,
    status: String,

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
            input: String::new(),
            logs: Vec::new(),
            status: "not started".to_string(),
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

    fn start(&mut self) {
        let _ = self.cfg.save(&self.config_path);
        match Db::open(&self.cfg.db_path) {
            Ok(db) => {
                let db = Arc::new(db);
                let (tx, rx) = std::sync::mpsc::channel();
                self.events = Some(rx);
                let node = Node::new(self.cfg.clone(), db.clone(), tx);
                node.start();
                self.logs.push(format!(
                    "[+] started node {} on {} (zone {}, egress {})",
                    self.cfg.node_id,
                    self.cfg.listen_addr,
                    self.cfg.zone,
                    self.cfg.egress.as_str()
                ));
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

    fn pump_events(&mut self) {
        let mut got = false;
        if let Some(rx) = &self.events {
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    AppEvent::Message => got = true,
                    AppEvent::Log(line) => {
                        self.logs.push(line);
                        if self.logs.len() > 800 {
                            self.logs.drain(0..200);
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
        let mut map: Vec<Chat> = Vec::new();
        for p in db.list_peers() {
            map.push(Chat {
                id: p.id.clone(),
                name: if p.name.is_empty() {
                    p.id.clone()
                } else {
                    p.name
                },
                last: String::new(),
            });
        }
        for c in db.conversations() {
            if let Some(existing) = map.iter_mut().find(|x| x.id == c.peer_id) {
                existing.last = c.last_body;
                if existing.name.is_empty() {
                    existing.name = c.peer_name;
                }
            } else {
                map.push(Chat {
                    id: c.peer_id.clone(),
                    name: if c.peer_name.is_empty() {
                        c.peer_id
                    } else {
                        c.peer_name
                    },
                    last: c.last_body,
                });
            }
        }
        self.chats = map;
        if let Some(sel) = self.selected.clone() {
            self.messages = db.messages_for(&sel, 500);
        }
        self.dirty = false;
    }

    fn add_peer(&mut self) {
        if self.new_id.trim().is_empty() {
            return;
        }
        let peer = Peer {
            id: self.new_id.trim().to_string(),
            name: if self.new_name.trim().is_empty() {
                self.new_id.trim().to_string()
            } else {
                self.new_name.trim().to_string()
            },
            addr: self.new_addr.trim().to_string(),
            via: self.new_via.clone(),
        };
        self.cfg.peers.push(peer.clone());
        let _ = self.cfg.save(&self.config_path);
        if let Some(db) = &self.db {
            db.upsert_peer(&peer.id, &peer.name, &peer.addr, &peer.via);
        }
        self.new_id.clear();
        self.new_name.clear();
        self.new_addr.clear();
        self.dirty = true;
    }

    fn send_current(&mut self) {
        let peer = match &self.selected {
            Some(p) => p.clone(),
            None => return,
        };
        if self.input.trim().is_empty() {
            return;
        }
        if let Some(node) = &self.node {
            node.send_message(&peer, &self.input);
        }
        self.logs.push(format!("[>] queued message to {peer}"));
        self.input.clear();
        self.dirty = true;
    }

    fn chats_ui(&mut self, ui: &mut egui::Ui) {
        ui.heading("Chats");
        ui.separator();
        let chats = self.chats.clone();
        egui::ScrollArea::vertical()
            .id_salt("chat_list")
            .max_height(220.0)
            .show(ui, |ui| {
                for c in &chats {
                    let selected = self.selected.as_deref() == Some(c.id.as_str());
                    let label = if c.last.is_empty() {
                        c.name.clone()
                    } else {
                        format!("{}\n   {}", c.name, truncate(&c.last, 28))
                    };
                    if ui.selectable_label(selected, label).clicked() {
                        self.selected = Some(c.id.clone());
                        self.dirty = true;
                    }
                }
            });

        ui.separator();
        ui.label("Conversation");
        egui::ScrollArea::vertical()
            .id_salt("messages")
            .max_height(260.0)
            .show(ui, |ui| {
                for m in &self.messages {
                    let who = if m.direction == "out" { "me" } else { "them" };
                    ui.label(format!("[{who}] {}", m.body));
                    ui.label(
                        egui::RichText::new(format!("     {}", m.status))
                            .small()
                            .weak(),
                    );
                }
            });

        ui.separator();
        ui.horizontal(|ui| {
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.input)
                    .hint_text("type a message and press Enter")
                    .desired_width(260.0),
            );
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if ui.button("Send").clicked() || enter {
                self.send_current();
            }
        });
        if self.selected.is_none() {
            ui.weak("select or add a contact on the right");
        }
    }

    fn setup_ui(&mut self, ui: &mut egui::Ui) {
        ui.heading("Setup");
        ui.label(format!("Config: {}", self.config_path));
        ui.separator();

        egui::Grid::new("cfg").num_columns(2).show(ui, |ui| {
            ui.label("Display name");
            ui.text_edit_singleline(&mut self.cfg.display_name);
            ui.end_row();
            ui.label("Node id");
            ui.text_edit_singleline(&mut self.cfg.node_id);
            ui.end_row();
            ui.label("Listen address");
            ui.text_edit_singleline(&mut self.cfg.listen_addr);
            ui.end_row();
            ui.label("Zone / suffix");
            ui.text_edit_singleline(&mut self.cfg.zone);
            ui.end_row();
            ui.label("Shared key");
            ui.text_edit_singleline(&mut self.cfg.shared_key);
            ui.end_row();
            ui.label("Egress");
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.cfg.egress, EgressKind::Direct, "direct");
                ui.selectable_value(&mut self.cfg.egress, EgressKind::Resolver, "resolver");
            });
            ui.end_row();
            ui.label("Resolver");
            ui.text_edit_singleline(opt_string(&mut self.cfg.resolver));
            ui.end_row();
            ui.label("Direct / relay addr");
            ui.text_edit_singleline(opt_string(&mut self.cfg.direct_server));
            ui.end_row();
            ui.label("Poll relay (enables inbox)");
            ui.text_edit_singleline(opt_string(&mut self.cfg.relay));
            ui.end_row();
            ui.label("Poll interval (s)");
            ui.add(egui::DragValue::new(&mut self.cfg.poll_interval_secs).range(1..=3600));
            ui.end_row();
            ui.label("EDNS0 UDP size");
            ui.add(egui::DragValue::new(&mut self.cfg.edns_udp_size).range(512..=4096));
            ui.end_row();
            ui.label("Authoritative");
            ui.checkbox(
                &mut self.cfg.authoritative,
                "answer SOA/NXDOMAIN (VPS relay)",
            );
            ui.end_row();
            ui.label("Database");
            ui.text_edit_singleline(&mut self.cfg.db_path);
            ui.end_row();
            ui.label("UI scale");
            ui.horizontal(|ui| {
                ui.add(
                    egui::DragValue::new(&mut self.cfg.ui_scale)
                        .range(0.5..=4.0)
                        .speed(0.05),
                );
                if ui.small_button("A-").clicked() {
                    self.bump_scale(-0.1);
                }
                if ui.small_button("A+").clicked() {
                    self.bump_scale(0.1);
                }
            });
            ui.end_row();
        });

        ui.separator();
        ui.horizontal(|ui| {
            if ui.button("Save").clicked() {
                if let Err(e) = self.cfg.save(&self.config_path) {
                    self.logs.push(format!("[!] save failed: {e}"));
                } else {
                    self.logs.push("[+] config saved".to_string());
                }
            }
            if ui.button("Restart node").clicked() {
                self.stop();
                self.start();
            }
        });

        ui.separator();
        ui.heading("Contacts");
        egui::Grid::new("peers").num_columns(2).show(ui, |ui| {
            ui.label("id (8 hex)");
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
        if ui.button("Add contact").clicked() {
            self.add_peer();
        }
    }

    fn log_ui(&mut self, ui: &mut egui::Ui) {
        ui.heading("DNS inspector");
        ui.label(format!(
            "node {} | zone {} | {}",
            self.cfg.node_id, self.cfg.zone, self.status
        ));
        ui.separator();
        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for line in &self.logs {
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

        egui::Panel::top("tabs").show(ui, |ui| {
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
                ui.label(&self.status);
            });
        });

        if self.tab == Tab::Chats {
            egui::Panel::left("left")
                .resizable(true)
                .default_size(300.0)
                .show(ui, |ui| {
                    self.chats_ui(ui);
                });
            egui::CentralPanel::default().show(ui, |ui| match &self.selected {
                Some(sel) => {
                    ui.heading(
                        self.chats
                            .iter()
                            .find(|c| &c.id == sel)
                            .map(|c| c.name.clone())
                            .unwrap_or_else(|| sel.clone()),
                    );
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            for m in &self.messages {
                                let who = if m.direction == "out" { "me" } else { "them" };
                                ui.label(format!("[{who}] {}", m.body));
                                ui.label(
                                    egui::RichText::new(format!("     {}", m.status))
                                        .small()
                                        .weak(),
                                );
                            }
                        });
                    ui.separator();
                    ui.horizontal(|ui| {
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut self.input)
                                .hint_text("message")
                                .desired_width(420.0),
                        );
                        let enter = resp.lost_focus()
                            && ui.ctx().input(|i| i.key_pressed(egui::Key::Enter));
                        if ui.button("Send").clicked() || enter {
                            self.send_current();
                        }
                    });
                }
                None => {
                    ui.heading("No conversation selected");
                    ui.weak("Use Setup to add a contact id, then pick it on the left.");
                }
            });
        } else {
            egui::CentralPanel::default().show(ui, |ui| match self.tab {
                Tab::Setup => self.setup_ui(ui),
                Tab::Log => self.log_ui(ui),
                Tab::Chats => {}
            });
        }

        ui.ctx().request_repaint_after(Duration::from_millis(500));
    }
}

fn opt_string(v: &mut Option<String>) -> &mut String {
    v.get_or_insert_with(String::new)
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n).collect();
        format!("{t}…")
    }
}

/// Launch the desktop app.
pub fn run(config_path: &str) {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1000.0, 660.0]),
        ..Default::default()
    };
    let path = config_path.to_string();
    let _ = eframe::run_native(
        "DNS Messenger",
        options,
        Box::new(move |_cc| Ok(Box::new(App::new(path)))),
    );
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
