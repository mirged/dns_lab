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
    // Empty means "no key": the node is a blind forwarder (relay) or simply not
    // configured yet. Silently falling back to a bundled key was a footgun.
    String::new()
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
fn default_window() -> u32 {
    8
}
fn default_verbose() -> bool {
    true
}
fn default_max_blobs_per_route() -> i64 {
    200
}
fn default_max_bytes_per_route() -> i64 {
    5 * 1024 * 1024
}
fn default_rate_limit() -> u32 {
    40
}
fn default_download_dir() -> String {
    "downloads".to_string()
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

/// What this node is allowed to do for other people's mail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RelayMode {
    /// Never relay (default). A `relay` address here only means "poll there".
    #[default]
    Client,
    /// Forward other people's mail, subject to quotas.
    Peer,
    /// Forward and answer SOA/NXDOMAIN as the authoritative nameserver.
    Authoritative,
}

impl RelayMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            RelayMode::Client => "client",
            RelayMode::Peer => "peer",
            RelayMode::Authoritative => "authoritative",
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
    /// Whether this node forwards other people's mail for them.
    #[serde(default)]
    pub relay_mode: RelayMode,
    /// Egress mode.
    #[serde(default)]
    pub egress: EgressKind,
    /// Recursive resolver address for `egress = "resolver"`.
    #[serde(default)]
    pub resolver: Option<String>,
    /// Relay / authoritative server address for `egress = "direct"`.
    #[serde(default)]
    pub direct_server: Option<String>,
    /// Extra relay addresses to fall back on, in order, after the primary
    /// (`relay`, or the peer address for direct contacts). Sending stops at the
    /// first relay that accepts the message, and polling asks every one of them,
    /// so a dead relay costs a retry instead of the mail.
    #[serde(default)]
    pub relays: Vec<String>,

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
    /// Chunks in flight per message (1 = old serial behaviour).
    #[serde(default = "default_window")]
    pub window: u32,
    /// Log every chunk; turn off for quieter relays.
    #[serde(default = "default_verbose")]
    pub verbose: bool,
    /// Mailbox quotas, enforced per routing tag on this relay.
    #[serde(default = "default_max_blobs_per_route")]
    pub max_blobs_per_route: i64,
    #[serde(default = "default_max_bytes_per_route")]
    pub max_bytes_per_route: i64,
    /// Sustained queries per second allowed per source address (0 = unlimited).
    #[serde(default = "default_rate_limit")]
    pub rate_limit_per_sec: u32,
    #[serde(default = "default_db_path")]
    pub db_path: String,

    /// Where received files are written. Relative paths resolve next to the
    /// config file, like `db_path`.
    #[serde(default = "default_download_dir")]
    pub download_dir: String,
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
            relay_mode: RelayMode::default(),
            egress: EgressKind::Direct,
            resolver: None,
            direct_server: None,
            relay: None,
            relays: Vec::new(),
            peers: Vec::new(),
            poll_interval_secs: default_poll_interval(),
            edns_udp_size: default_edns_size(),
            ui_scale: default_ui_scale(),
            timeout_ms: default_timeout_ms(),
            retries: default_retries(),
            window: default_window(),
            verbose: default_verbose(),
            max_blobs_per_route: default_max_blobs_per_route(),
            max_bytes_per_route: default_max_bytes_per_route(),
            rate_limit_per_sec: default_rate_limit(),
            db_path: default_db_path(),
            download_dir: default_download_dir(),
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
            if Path::new(&cfg.download_dir).is_relative() {
                cfg.download_dir = parent.join(&cfg.download_dir).to_string_lossy().to_string();
            }
        }
        cfg
    }

    pub fn save(&self, path: &str) -> Result<(), String> {
        let text = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(path, text).map_err(|e| e.to_string())
    }

    /// 32-byte encryption key derived from `shared_key`.
    ///
    /// `None` when no passphrase is configured: such a node cannot encrypt,
    /// decrypt or derive routing tags. That is deliberate — a relay with no
    /// `shared_key` is a blind forwarder, and a client with no passphrase fails
    /// loudly instead of quietly using the built-in lab key.
    pub fn key_opt(&self) -> Option<[u8; 32]> {
        if self.shared_key.trim().is_empty() {
            None
        } else {
            Some(crate::key_from_str(&self.shared_key))
        }
    }

    /// True when the configured passphrase is the shipped example placeholder.
    pub fn key_is_placeholder(&self) -> bool {
        self.shared_key.starts_with("CHANGE-ME")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!("dns_lab_cfg_{}_{}", std::process::id(), name));
        let _ = std::fs::create_dir_all(&dir);
        dir.join("dns_lab.toml").to_string_lossy().to_string()
    }

    #[test]
    fn empty_passphrase_means_no_key() {
        let cfg = Config::default();
        assert!(
            cfg.key_opt().is_none(),
            "default config must not imply a key"
        );
        let blank = Config {
            shared_key: "  ".to_string(),
            ..Config::default()
        };
        assert!(blank.key_opt().is_none(), "whitespace is not a passphrase");
        let cfg = Config {
            shared_key: "hunter2".to_string(),
            ..Config::default()
        };
        assert_eq!(cfg.key_opt(), Some(crate::key_from_str("hunter2")));
    }

    #[test]
    fn load_creates_id_and_resolves_relative_db() {
        let path = scratch("load");
        let _ = std::fs::remove_file(&path);
        let cfg = Config::load(&path);
        assert_eq!(cfg.node_id.len(), 8);
        // Relative db paths resolve next to the config file.
        assert!(cfg.db_path.starts_with(&path[..path.rfind('/').unwrap()]));
        assert!(cfg.db_path.ends_with("dns_lab.db"));
    }

    #[test]
    fn save_then_load_roundtrips() {
        let path = scratch("round");
        let cfg = Config {
            node_id: "abcd1234".to_string(),
            shared_key: "round-trip-secret".to_string(),
            zone: "t.example.com".to_string(),
            db_path: "/tmp/round.db".to_string(),
            ..Config::default()
        };
        cfg.save(&path).unwrap();

        let back = Config::load(&path);
        assert_eq!(back.node_id, "abcd1234");
        assert_eq!(back.zone, "t.example.com");
        assert_eq!(back.db_path, "/tmp/round.db");
        assert_eq!(back.key_opt(), cfg.key_opt());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn placeholder_key_is_flagged() {
        let placeholder = Config {
            shared_key: "CHANGE-ME-to-a-long-random-secret".to_string(),
            ..Config::default()
        };
        assert!(placeholder.key_is_placeholder());
        let generated = Config {
            shared_key: crate::random_secret(),
            ..Config::default()
        };
        assert!(!generated.key_is_placeholder());
    }
}
