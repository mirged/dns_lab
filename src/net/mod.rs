//! DNS transport: node orchestration, sender, authoritative listener and poller.

pub mod listener;
pub mod poller;
pub mod sender;

use crate::config::{Config, EgressKind};
use crate::db::Db;
use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

/// Events pushed from transport threads to the UI (or a console drain).
#[derive(Debug, Clone)]
pub enum AppEvent {
    /// A message was stored; refresh the conversation.
    Message,
    /// A human-readable transport log line.
    Log(String),
}

/// Where outgoing DNS queries are sent.
#[derive(Debug, Clone, Copy)]
pub enum Egress {
    /// Straight to a peer/relay `ip:port`.
    Direct(SocketAddr),
    /// To a recursive resolver that forwards to our authoritative zone.
    Resolver(SocketAddr),
}

impl Egress {
    pub fn addr(&self) -> SocketAddr {
        match self {
            Egress::Direct(a) | Egress::Resolver(a) => *a,
        }
    }
    pub fn label(&self) -> &'static str {
        match self {
            Egress::Direct(_) => "direct",
            Egress::Resolver(_) => "resolver",
        }
    }
}

/// Shared node state used by every transport thread.
pub struct Node {
    pub cfg: Config,
    pub db: Arc<Db>,
    pub key: [u8; 32],
    pub events: Mutex<Sender<AppEvent>>,
    pub stop: Arc<AtomicBool>,
}

impl Node {
    pub fn new(cfg: Config, db: Arc<Db>, events: Sender<AppEvent>) -> Arc<Node> {
        let key = cfg.key();
        Arc::new(Node {
            cfg,
            db,
            key,
            events: Mutex::new(events),
            stop: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn log(&self, msg: impl Into<String>) {
        let msg = msg.into();
        eprintln!("{msg}");
        if let Ok(tx) = self.events.lock() {
            let _ = tx.send(AppEvent::Log(msg));
        }
    }

    pub fn notify_message(&self) {
        if let Ok(tx) = self.events.lock() {
            let _ = tx.send(AppEvent::Message);
        }
    }

    pub fn stop(&self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn stopped(&self) -> bool {
        self.stop.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Resolve the egress for a recipient id (or `None` for polls / broadcasts).
    pub fn egress_for(&self, to: Option<&str>) -> Result<Egress, String> {
        match self.cfg.egress {
            EgressKind::Resolver => {
                let addr = self
                    .cfg
                    .resolver
                    .clone()
                    .ok_or("egress=resolver but no `resolver` configured")?;
                let sa: SocketAddr = addr
                    .parse()
                    .map_err(|_| format!("invalid resolver address: {addr}"))?;
                Ok(Egress::Resolver(sa))
            }
            EgressKind::Direct => {
                if let Some(to) = to
                    && let Some(peer) = self.db.peer(to)
                    && peer.via == "direct"
                    && !peer.addr.is_empty()
                    && let Ok(sa) = peer.addr.parse()
                {
                    return Ok(Egress::Direct(sa));
                }
                let addr = self
                    .cfg
                    .relay
                    .clone()
                    .or_else(|| self.cfg.direct_server.clone())
                    .ok_or(
                        "egress=direct but no peer addr, `relay` or `direct_server` configured",
                    )?;
                let sa: SocketAddr = addr
                    .parse()
                    .map_err(|_| format!("invalid direct address: {addr}"))?;
                Ok(Egress::Direct(sa))
            }
        }
    }

    /// Encrypt, chunk and transmit a chat message. Blocking; call from a worker thread.
    pub fn do_send(&self, to: &str, body: &str, msg_id: &str) -> Result<usize, String> {
        use crate::protocol::{Envelope, MsgType, encode_blob};
        let env = Envelope::new(MsgType::Message, &self.cfg.node_id, to, body);
        let blob = encode_blob(&self.key, &env)?;
        let egress = self.egress_for(Some(to))?;
        sender::send_blob(self, egress, &blob, msg_id)
    }

    /// Spawn a worker to send a message and update its status.
    pub fn send_message(self: &Arc<Self>, to: &str, body: &str) {
        let msg_id = crate::random_id();
        let peer_name = self
            .db
            .peer(to)
            .map(|p| p.name)
            .unwrap_or_else(|| to.to_string());
        if let Err(e) = self
            .db
            .insert_message(&msg_id, "out", to, &peer_name, body, "sending")
        {
            self.log(format!("[!] db error: {e}"));
            return;
        }
        self.notify_message();

        let node = self.clone();
        let to = to.to_string();
        let body = body.to_string();
        std::thread::spawn(move || match node.do_send(&to, &body, &msg_id) {
            Ok(chunks) => {
                node.db.set_status(&msg_id, "sent");
                node.log(format!(
                    "[+] sent message {msg_id} to {to} in {chunks} DNS queries"
                ));
                node.notify_message();
            }
            Err(e) => {
                node.db.set_status(&msg_id, "failed");
                node.log(format!("[!] send {msg_id} failed: {e}"));
                node.notify_message();
            }
        });
    }

    /// Start the UDP + TCP tunnel listener and (optionally) the mailbox poller.
    pub fn start(self: &Arc<Self>) {
        listener::spawn(self.clone());
        if self.cfg.relay.is_some() || self.cfg.egress == EgressKind::Resolver {
            poller::spawn(self.clone());
        }
    }
}
