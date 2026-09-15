use serde::{Deserialize, Serialize};
use std::path::Path;

fn default_display_name() -> String {
    "anon".to_string()
}
fn default_listen_addr() -> String {
    "0.0.0.0:1054".to_string()
}
fn default_zone() -> String {
    "lab.local".to_string()
}
fn default_key() -> String {
    String::from_utf8_lossy(crate::DEFAULT_KEY).to_string()
}
fn default_poll_interval() -> u64 {
    2
}
fn default_edns_size() -> u16 {
    1232
}
fn default_ui_scale() -> f32 {
    1.0
}
fn default_timeout_ms() -> u64 {
    800
}
fn default_retries() -> u32 {
    3
}
fn default_db_path() -> String {
    "dns_lab.db".to_string()
}
fn default_verb() -> String {
    "m".to_string()
}

/// How a client reaches its counterpart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EgressKind {
    /// Send DNS queries straight to a peer / relay address.
    #[default]
    Direct,
    /// Send DNS queries to a recursive resolver (public DNS tunneling).
    Resolver,
}

impl EgressKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EgressKind::Direct => "direct",
            EgressKind::Resolver => "resolver",
        }
    }
}

/// A known conversation partner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Peer {
    /// Remote node id (8 hex chars).
    pub id: String,
    pub name: String,
    /// `ip:port` used when `via = "direct"`.
    #[serde(default)]
    pub addr: String,
    /// `direct` or `relay`.
    #[serde(default = "default_verb")]
    pub via: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub display_name: String,
    /// This node's id. Filled in automatically on first load if empty.
    #[serde(default)]
    pub node_id: String,
    /// UDP/TCP bind address for the local DNS listener.
    pub listen_addr: String,
    /// Zone/suffix that tunnel queries live under (e.g. `lab.local` or `t.example.com`).
    pub zone: String,
    /// Answer SOA/NS for the apex and NXDOMAIN outside the zone (VPS mode).
    #[serde(default)]
    pub authoritative: bool,
    /// Egress mode.
    #[serde(default)]
    pub egress: EgressKind,
    /// Recursive resolver address for `egress = "resolver"`.
    #[serde(default)]
    pub resolver: Option<String>,
    /// Relay / authoritative server address for `egress = "direct"`.
    #[serde(default)]
    pub direct_server: Option<String>,
    /// Mailbox relay to poll for inbound messages (enables relay mode).
    #[serde(default)]
    pub relay: Option<String>,
    #[serde(default)]
    pub peers: Vec<Peer>,
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    /// EDNS0 advertised UDP payload size.
    #[serde(default = "default_edns_size")]
    pub edns_udp_size: u16,
    /// UI zoom multiplier (1.0 = platform default). Bump it on high-DPI screens.
    #[serde(default = "default_ui_scale")]
    pub ui_scale: f32,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_retries")]
    pub retries: u32,
    #[serde(default = "default_db_path")]
    pub db_path: String,
    #[serde(default = "default_key")]
    pub shared_key: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            display_name: default_display_name(),
            node_id: String::new(),
            listen_addr: default_listen_addr(),
            zone: default_zone(),
            authoritative: false,
            egress: EgressKind::Direct,
            resolver: None,
            direct_server: None,
            relay: None,
            peers: Vec::new(),
            poll_interval_secs: default_poll_interval(),
            edns_udp_size: default_edns_size(),
            ui_scale: default_ui_scale(),
            timeout_ms: default_timeout_ms(),
            retries: default_retries(),
            db_path: default_db_path(),
            shared_key: default_key(),
        }
    }
}

impl Config {
    /// Load from a TOML file, creating defaults (and a fresh node id) if missing.
    pub fn load(path: &str) -> Config {
        let mut cfg = if Path::new(path).exists() {
            match std::fs::read_to_string(path) {
                Ok(text) => toml::from_str(&text).unwrap_or_else(|e| {
                    eprintln!("[!] Failed to parse {path}: {e}; using defaults");
                    Config::default()
                }),
                Err(e) => {
                    eprintln!("[!] Failed to read {path}: {e}; using defaults");
                    Config::default()
                }
            }
        } else {
            Config::default()
        };

        if cfg.node_id.is_empty() {
            cfg.node_id = crate::random_id();
        }
        // Resolve a relative database path against the config file's directory so
        // it lands somewhere writable on all platforms (esp. Android, no cwd).
        if Path::new(&cfg.db_path).is_relative()
            && let Some(parent) = Path::new(path).parent()
            && !parent.as_os_str().is_empty()
        {
            cfg.db_path = parent.join(&cfg.db_path).to_string_lossy().to_string();
        }
        cfg
    }

    pub fn save(&self, path: &str) -> Result<(), String> {
        let text = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(path, text).map_err(|e| e.to_string())
    }

    /// 32-byte encryption key derived from `shared_key`.
    pub fn key(&self) -> [u8; 32] {
        crate::key_from_str(&self.shared_key)
    }
}
