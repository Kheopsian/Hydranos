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
pub struct Agent {
    #[serde(default)]
    pub name: String,
    /// Empty means "started here". Present means "reached over the network",
    /// and then everything below is ignored: the engine's settings live on the
    /// far side.
    #[serde(default)]
    pub addr: String,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub tls_ca: String,
    /// "race" or "hoard". Required for a local entry, and deliberately so: an
    /// entry that merely forgot its `addr` would otherwise be started here,
    /// turning a remote node into a local one without a word.
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub engine_id: String,
    /// A SPARSE override of the role profile, not a whole configuration. The
    /// entry holds what is true of this engine -- its port, its interface --
    /// and nothing else; everything shared comes from [race] or [hoard], where
    /// a change is made once.
    #[serde(default)]
    pub session: toml::value::Table,
}

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

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Dedup {
    /// "auto" links without asking, "ask" only records the match for the UI,
    /// anything else is off.
    ///
    /// Defaults to "ask" rather than "auto": linking is safe, but it writes to
    /// the filesystem, and a feature that starts doing that on upgrade without
    /// anyone asking is the kind of surprise that costs trust.
    #[serde(default = "dedup_default_mode")]
    pub mode: String,
    /// Skip matches whose payload is below this, in MiB. Linking a 3 KB .nfo
    /// pack saves nothing and still creates inodes.
    #[serde(default = "dedup_default_min_mib")]
    pub min_mib: u64,
}

fn dedup_default_mode() -> String {
    "ask".to_string()
}

fn dedup_default_min_mib() -> u64 {
    16
}

impl Default for Dedup {
    fn default() -> Self {
        Self { mode: dedup_default_mode(), min_mib: dedup_default_min_mib() }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Config {
    #[serde(default)]
    pub daemon: Daemon,

    /// Every node of the fleet, local and remote alike.
    ///
    /// An entry with an `addr` is reached over the network and its engine is
    /// configured on the far side. An entry without one runs HERE: this
    /// process starts that engine itself. That is what "one agent, one engine"
    /// means -- a node hosts an arbitrary set of engines, each able to sit on
    /// its own tunnel, rather than exactly a race and a hoard.
    #[serde(default)]
    pub agent: Vec<Agent>,

    /// The engines this node hosts, under the name that says what they are.
    ///
    /// `[[agent]]` meant two different things at once -- an engine started here
    /// and a machine reached over the network -- which is why nobody could say
    /// what either word meant. A node is now a whole Hydra, held in the store,
    /// and an engine is a session inside one; this block is only ever the
    /// latter.
    ///
    /// `[[agent]]` is still read, and read FIRST, so an existing file keeps
    /// working and a rollback finds what it left behind. Same id in both means
    /// `[[engine]]` wins: it is the newer spelling, so it is the deliberate one.
    #[serde(default)]
    pub engine: Vec<Agent>,

    #[serde(default)]
    pub auth: Auth,

    /// Recognising payload we already hold when a torrent is added.
    #[serde(default)]
    pub dedup: Dedup,

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
    /// Trackers whose faults the operator has seen and accepted.
    ///
    /// A badge that is always lit is not a badge. This node points 107k torrents
    /// at an archive.org it cannot reach and 6k at a gemini that refuses it:
    /// without a way to say "I know", the Trackers tab would be red forever and
    /// the day a working tracker breaks it would say nothing new.
    #[serde(default)]
    pub announce_muted: BTreeMap<String, String>,

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

/// The placeholder shipped in configs/default.toml.
///
/// Published in the repository, so an install that kept it has a key that
/// every reader of the source already knows.
pub const PLACEHOLDER_API_KEY: &str = "change-me-in-production";

/// A key no one else can guess: 24 bytes of system entropy, hex encoded.
///
/// Same shape and same source as `install.sh` uses when it enrols a node
/// (`head -c 24 /dev/urandom`), so a key looks the same whichever path made it.
fn fresh_api_key() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 24];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Give this instance an API key of its own before it serves anything.
///
/// 3.x generated one on first boot and persisted it; the port dropped that
/// step while keeping a template that ships `api_key = ""`. The result was an
/// install whose expected key was the empty string -- and `authorised` matched
/// it against a caller who sent no key at all, so the whole API answered
/// unauthenticated. `authorised` now fails closed on an empty key, which makes
/// generating one here the thing that keeps a fresh install usable.
///
/// The placeholder is treated as absent for the same reason: it is a published
/// constant, not a secret.
///
/// Written back to the TOML so it survives a restart and so a headless
/// operator can read it out of the file. If the file cannot be written the old
/// value is kept rather than replaced by a key that would change at every
/// boot -- an instance whose key rotates behind the operator's back is worse
/// than one still carrying the placeholder, and the warning says so.
///
/// Returns whether a key was generated, so the caller can log it once.
pub fn ensure_api_key(config: &mut Config, path: &Path) -> bool {
    let current = config.daemon.api_key.as_str();
    let placeholder = current == PLACEHOLDER_API_KEY;
    if !current.is_empty() && !placeholder {
        return false;
    }

    let key = fresh_api_key();
    let Ok(doc) = std::fs::read_to_string(path) else {
        tracing::error!(
            path = %path.display(),
            "cannot read the config to store a generated API key; the API stays closed"
        );
        return false;
    };
    let quoted = crate::tomledit::quote_toml_key(&key);
    let edited = match crate::tomledit::set_toml_value(&doc, "daemon", "api_key", &quoted) {
        Ok(edited) => edited,
        Err(e) => {
            tracing::error!(error = %e, "cannot set daemon.api_key; the API stays closed");
            return false;
        }
    };
    if let Err(e) = std::fs::write(path, &edited) {
        tracing::error!(
            error = %e,
            path = %path.display(),
            "cannot persist a generated API key; the API stays closed"
        );
        return false;
    }

    config.daemon.api_key = key;
    if placeholder {
        tracing::warn!(
            path = %path.display(),
            "the configured API key was the published placeholder and has been replaced by a \
             generated one -- clients configured with the old value must be updated; the new \
             key is in the config file, or from POST /api/login with the admin account"
        );
    } else {
        tracing::warn!(
            path = %path.display(),
            "no API key was configured; a new one has been generated and written to the config"
        );
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_tmp(name: &str, body: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("hydra-cfgtest-{name}-{}.toml", std::process::id()));
        std::fs::write(&path, body).unwrap();
        path
    }

    /// The shipped template has `api_key = ""`. Booting on it used to leave the
    /// expected key empty, which `authorised` matched against a caller sending
    /// no key at all -- the whole API, unauthenticated, on a fresh install.
    #[test]
    fn a_fresh_install_generates_and_persists_a_key() {
        let path = write_tmp("fresh", "[daemon]\napi_key = \"\"\ndata_dir = \"/config\"\n");
        let mut cfg = Config::load(&path).unwrap();
        assert!(ensure_api_key(&mut cfg, &path), "an empty key must be filled");
        assert_eq!(cfg.daemon.api_key.len(), 48, "24 random bytes, hex encoded");

        // Persisted, not just in memory: a key that lived only in this process
        // would change at every restart and lock every client out.
        let reloaded = Config::load(&path).unwrap();
        assert_eq!(reloaded.daemon.api_key, cfg.daemon.api_key);
        std::fs::remove_file(&path).ok();
    }

    /// The placeholder is published in this repository, so it is not a secret.
    #[test]
    fn the_published_placeholder_is_replaced() {
        let path = write_tmp("placeholder", &format!("[daemon]\napi_key = \"{PLACEHOLDER_API_KEY}\"\n"));
        let mut cfg = Config::load(&path).unwrap();
        assert!(ensure_api_key(&mut cfg, &path));
        assert_ne!(cfg.daemon.api_key, PLACEHOLDER_API_KEY);
        std::fs::remove_file(&path).ok();
    }

    /// An operator's own key is never touched: rotating it behind their back
    /// would break every client on a restart they did not ask for.
    #[test]
    fn a_configured_key_is_left_alone() {
        let path = write_tmp("configured", "[daemon]\napi_key = \"mine\"\n");
        let mut cfg = Config::load(&path).unwrap();
        assert!(!ensure_api_key(&mut cfg, &path), "nothing to generate");
        assert_eq!(cfg.daemon.api_key, "mine");
        std::fs::remove_file(&path).ok();
    }

    /// Two instances must not come up with the same key.
    #[test]
    fn generated_keys_differ() {
        assert_ne!(fresh_api_key(), fresh_api_key());
    }

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

/// One engine this node runs.
#[derive(Debug, Clone)]
pub struct LocalEngine {
    pub id: String,
    pub role: String,
    pub session: Session,
}

impl Config {
    /// The fleet profile for a role.
    pub fn profile_for_role(&self, role: &str) -> Option<&Session> {
        match role {
            "race" => Some(&self.race),
            "hoard" => Some(&self.hoard),
            _ => None,
        }
    }

    /// Every engine this process starts.
    ///
    /// `[race]` and `[hoard]` first, because that is what every install has,
    /// then the `[[agent]]` entries that run here. Additive on purpose: the
    /// older `[[engine]]` blocks REPLACED the two sections the moment one
    /// existed, a rule nothing stated and which silently disabled half a
    /// config. An entry whose id matches one already resolved replaces that
    /// one rather than colliding with it, so a node can override its own race
    /// engine without restating the rest.
    pub fn local_engines(&self) -> Vec<LocalEngine> {
        let mut out = vec![
            LocalEngine { id: "race".into(), role: "race".into(), session: self.race.clone() },
            LocalEngine { id: "hoard".into(), role: "hoard".into(), session: self.hoard.clone() },
        ];

        for agent in self.agent.iter().chain(self.engine.iter()) {
            // An addr means the engine lives elsewhere. A missing role means we
            // do not know what it is, and guessing would start a remote node's
            // engine here.
            if !agent.addr.trim().is_empty() || agent.role.trim().is_empty() {
                continue;
            }
            let Some(profile) = self.profile_for_role(agent.role.trim()) else {
                tracing::warn!(
                    agent = %agent.name,
                    role = %agent.role,
                    "unknown role: this agent starts no engine"
                );
                continue;
            };
            let id = if !agent.engine_id.trim().is_empty() {
                agent.engine_id.trim().to_string()
            } else {
                agent.name.trim().to_string()
            };
            if id.is_empty() {
                tracing::warn!("a local agent has neither engine_id nor name: skipped");
                continue;
            }
            let session = merge_session(profile, &agent.session);
            let engine = LocalEngine { id: id.clone(), role: agent.role.trim().to_string(), session };
            match out.iter().position(|e| e.id == id) {
                Some(i) => out[i] = engine,
                None => out.push(engine),
            }
        }
        out
    }
}

/// The role profile with an entry's own keys laid over it.
///
/// Through TOML rather than field by field: a sparse override that had to name
/// every key would stop being sparse, and a new session key would silently not
/// be overridable until someone remembered to add it here.
fn merge_session(profile: &Session, over: &toml::value::Table) -> Session {
    if over.is_empty() {
        return profile.clone();
    }
    let Ok(toml::Value::Table(mut base)) = toml::Value::try_from(profile) else {
        return profile.clone();
    };
    for (k, v) in over {
        base.insert(k.clone(), v.clone());
    }
    match toml::Value::Table(base).try_into() {
        Ok(s) => s,
        Err(e) => {
            // A bad override is refused rather than half-applied: half a
            // configuration is one nobody wrote.
            tracing::warn!(error = %e, "agent session override refused, using the role profile");
            profile.clone()
        }
    }
}

#[cfg(test)]
mod agent_tests {
    use super::*;

    fn cfg(toml_text: &str) -> Config {
        toml::from_str(toml_text).expect("config parses")
    }

    #[test]
    fn a_node_with_no_agents_still_runs_race_and_hoard() {
        let c = cfg("[race]\nlisten_port = 1\n\n[hoard]\nlisten_port = 2\n");
        let ids: Vec<String> = c.local_engines().iter().map(|e| e.id.clone()).collect();
        assert_eq!(ids, ["race", "hoard"]);
    }

    /// ⭐ Additive, unlike the [[engine]] blocks it replaces: those took over
    /// the moment one existed, silently disabling the rest of a config.
    #[test]
    fn a_local_agent_adds_an_engine_without_removing_the_others() {
        let c = cfg(
            "[race]\nlisten_port = 1\n\n[hoard]\nlisten_port = 2\n\n\
             [[agent]]\nname = \"vpn7\"\nrole = \"race\"\n[agent.session]\nlisten_port = 26991\n",
        );
        let engines = c.local_engines();
        let ids: Vec<String> = engines.iter().map(|e| e.id.clone()).collect();
        assert_eq!(ids, ["race", "hoard", "vpn7"]);
        assert_eq!(engines[2].session.listen_port, 26991);
    }

    /// The override is sparse: everything it does not say comes from the role
    /// profile. Taking it verbatim would run an engine with every other field
    /// at its zero value -- a configuration nobody wrote.
    #[test]
    fn an_override_keeps_everything_it_does_not_mention() {
        let c = cfg(
            "[race]\nlisten_port = 1\nmax_connections = 500\nenable_dht = true\n\n\
             [hoard]\nlisten_port = 2\n\n\
             [[agent]]\nname = \"vpn7\"\nrole = \"race\"\n[agent.session]\nlisten_port = 26991\n",
        );
        let vpn7 = c.local_engines().into_iter().find(|e| e.id == "vpn7").unwrap();
        assert_eq!(vpn7.session.listen_port, 26991, "what the entry says");
        assert_eq!(vpn7.session.max_connections, 500, "what it does not, from the profile");
        assert!(vpn7.session.enable_dht);
    }

    /// An entry that merely forgot its addr must not be started here: that
    /// turns a remote node into a local one, silently.
    #[test]
    fn a_remote_agent_and_a_roleless_one_start_nothing_here() {
        let c = cfg(
            "[race]\nlisten_port = 1\n\n[hoard]\nlisten_port = 2\n\n\
             [[agent]]\nname = \"far\"\naddr = \"10.0.0.9:7000\"\nrole = \"race\"\n\n\
             [[agent]]\nname = \"nameless\"\n",
        );
        let ids: Vec<String> = c.local_engines().iter().map(|e| e.id.clone()).collect();
        assert_eq!(ids, ["race", "hoard"]);
    }

    /// A node overriding its own race engine replaces it rather than ending up
    /// with two engines called race.
    #[test]
    fn an_id_that_already_exists_replaces_it() {
        let c = cfg(
            "[race]\nlisten_port = 1\n\n[hoard]\nlisten_port = 2\n\n\
             [[agent]]\nengine_id = \"race\"\nrole = \"race\"\n[agent.session]\nlisten_port = 9999\n",
        );
        let engines = c.local_engines();
        assert_eq!(engines.len(), 2, "replaced, not added");
        assert_eq!(engines[0].session.listen_port, 9999);
    }
}
