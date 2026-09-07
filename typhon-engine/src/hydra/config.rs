//! The daemon configuration, read from the same default.toml the Go binary reads.
//!
//! The file format is frozen for the port. An existing install must be able to
//! run 4.0.0 against the config it already has, and roll back to 3.x against
//! that same file: a config the new binary rewrote in its own dialect would
//! make the rollback a restore-from-backup instead of an image change.
//!
//! Only the sections the ported surface needs are typed. Everything else is
//! kept verbatim in `rest` so that a round trip never drops a key the rest of
//! the daemon still relies on.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Daemon {
    #[serde(default)]
    pub api_host: String,
    #[serde(default)]
    pub api_port: u16,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub data_dir: String,
    #[serde(default)]
    pub agent_token: String,
    #[serde(default)]
    pub create_torrent_folder: bool,
    #[serde(default)]
    pub update_check_disabled: bool,
}

/// One engine's section of the config ([race] or [hoard]).
///
/// Only the keys the ported surface reads are typed. The rest stays in the file
/// and is served verbatim by /api/settings, which re-reads it.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Session {
    #[serde(default)]
    pub listen_port: u16,
    #[serde(default)]
    pub bind_interface: String,
    #[serde(default)]
    pub start_paused: bool,
    #[serde(default)]
    pub max_connections: i64,
    #[serde(default)]
    pub enable_dht: bool,
    #[serde(default)]
    pub enable_pex: bool,
    #[serde(default)]
    pub enable_webseed: bool,
    /// Put this engine on the network at all. True everywhere in production;
    /// false builds the manager and loads its durable state without opening a
    /// socket, which is what the differential bench runs against -- a bench
    /// instance holding the production catalogue must not announce it.
    #[serde(default = "yes")]
    pub net: bool,
    #[serde(default)]
    pub aio_threads: Option<usize>,
    #[serde(default)]
    pub active_downloads: i64,
    #[serde(default)]
    pub max_uploads_per_torrent: i64,
    #[serde(default)]
    pub enable_ipv6: bool,
    #[serde(default)]
    pub gluetun_port_forward: bool,
    #[serde(default)]
    pub gluetun_url: String,
    #[serde(default)]
    pub gluetun_api_key: String,
    #[serde(default)]
    pub listen_port_proxy_v2: u16,
    #[serde(default)]
    pub listen_addr_proxy_v2: String,
    #[serde(default)]
    pub proxy_v2_trusted_sources: Vec<String>,
    #[serde(default)]
    pub socks5_outbound_host: String,
    #[serde(default)]
    pub socks5_outbound_port: u16,
    #[serde(default)]
    pub socks5_outbound_user: String,
    #[serde(default)]
    pub socks5_outbound_pass: String,
}

impl Session {
    /// How many file descriptors the engine's disk pool keeps open.
    ///
    /// 3.x derives it from aio_threads when set; the fallback matches the
    /// engine's own default so an unset config behaves identically.
    pub fn file_pool_size(&self) -> usize {
        self.aio_threads.unwrap_or(256)
    }
}

/// The race drain: it deletes payload when the disk fills, so every field here
/// is read rather than assumed.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct RaceDrain {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub add_block_enabled: bool,
    #[serde(default)]
    pub age_ratio_action: String,
    #[serde(default)]
    pub age_ratio_enabled: bool,
    #[serde(default)]
    pub age_ratio_mode: String,
    #[serde(default)]
    pub check_interval_seconds: i64,
    #[serde(default)]
    pub high_watermark_pct: i64,
    #[serde(default)]
    pub low_watermark_pct: i64,
    #[serde(default)]
    pub max_age_hours: i64,
    #[serde(default)]
    pub min_age_minutes: i64,
    #[serde(default)]
    pub min_ratio: f64,
    #[serde(default)]
    pub reserve_free_gb: i64,
    #[serde(default)]
    pub race_path: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct VpnSpeedtest {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub iperf3_server: String,
}

/// Per-tracker client identity override.
///
/// The field names are capitalised because that is what the Go structure
/// serialises to: it has no json tags, so encoding/json used the exported Go
/// field names as-is. Renaming them here to something more idiomatic would be
/// an invisible break for every client that already parses this endpoint.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct AnnounceClient {
    #[serde(rename = "PeerIDPrefix", alias = "peer_id_prefix", default)]
    pub peer_id_prefix: String,
    #[serde(rename = "UserAgent", alias = "user_agent", default)]
    pub user_agent: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Auth {
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password_hash: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Config {
    #[serde(default)]
    pub daemon: Daemon,

    #[serde(default)]
    pub auth: Auth,

    /// tracker host -> client identity. BTreeMap, not HashMap: the Go side
    /// serialises a map and encoding/json sorts map keys, so the ordering is
    /// part of the observable output.
    #[serde(default)]
    pub announce_clients: BTreeMap<String, AnnounceClient>,

    /// tracker host -> "zero" | ...
    #[serde(default)]
    pub announce_secondary_stats: BTreeMap<String, String>,

    /// tracker host -> "v4" | "v6" | ...
    #[serde(default)]
    pub announce_ip_modes: BTreeMap<String, String>,

    /// tracker host -> passkey substituted into the announce URL.
    #[serde(default)]
    pub announce_passkeys: BTreeMap<String, String>,

    #[serde(default)]
    pub vpn_speedtest: VpnSpeedtest,

    #[serde(default)]
    pub race: Session,

    #[serde(default)]
    pub hoard: Session,

    #[serde(default)]
    pub race_drain: RaceDrain,

    /// Every section not yet typed, preserved so nothing is lost on rewrite.
    #[serde(flatten)]
    pub rest: BTreeMap<String, toml::Value>,
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("reading {}: {}", path.display(), e))?;
        let cfg: Config = toml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("parsing {}: {}", path.display(), e))?;
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The capitalised field names are the contract, so a test pins them: this
    // is the kind of detail that breaks the *arr stack silently rather than
    // loudly, and a rename would otherwise pass every other check.
    #[test]
    fn announce_client_serialises_with_go_field_names() {
        let c = AnnounceClient {
            peer_id_prefix: "-qB5220-".into(),
            user_agent: "qBittorrent/5.2.2".into(),
        };
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(
            json,
            r#"{"PeerIDPrefix":"-qB5220-","UserAgent":"qBittorrent/5.2.2"}"#
        );
    }

    #[test]
    fn reads_the_announce_sections_of_a_production_config() {
        let toml_text = r#"
[daemon]
api_host = "0.0.0.0"
api_port = 8199
api_key = "secret"

[announce_clients."t.myanonamouse.net"]
peer_id_prefix = "-qB5220-"
user_agent = "qBittorrent/5.2.2"

[announce_secondary_stats]
"seedpool.org" = "zero"

[announce_ip_modes]
"gemini-tracker.org" = "v4"
"#;
        let cfg: Config = toml::from_str(toml_text).unwrap();
        assert_eq!(cfg.daemon.api_port, 8199);
        assert_eq!(
            cfg.announce_clients["t.myanonamouse.net"].peer_id_prefix,
            "-qB5220-"
        );
        assert_eq!(cfg.announce_secondary_stats["seedpool.org"], "zero");
        assert_eq!(cfg.announce_ip_modes["gemini-tracker.org"], "v4");
    }
}

/// Serde default for a switch that is on unless the operator says otherwise.
fn yes() -> bool {
    true
}
