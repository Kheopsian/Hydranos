use serde::Deserialize;
use std::fs;

/// Binding describes one network identity through which Typhon listens
/// for inbound peer connections AND source-binds outbound peer dials.
/// Each binding presents its own peer_id in the BT handshake — a tracker
/// sees N apparent BitTorrent clients in the swarm, one per binding,
/// each on a different (IP, port). Used to spread the swarm across
/// multiple WireGuard tunnels (Proton multi-tunnel).
///
/// Empty bindings vec = legacy single-binding mode: peer_id is derived
/// from `peer_fingerprint` and listen socket from `listen_interfaces`/
/// `listen_port`. New deployments populate `bindings` directly.
#[derive(Debug, Clone, Deserialize)]
pub struct Binding {
    /// Stable index used for logs and round-robin dial selection.
    #[serde(default)]
    pub id: u32,
    /// 20-char ASCII peer_id (BEP-20). Empty = legacy random suffix on
    /// `peer_fingerprint` at startup.
    #[serde(default)]
    pub peer_id: String,
    /// Local IP this binding listens on. Multi-tunnel Proton setup shares
    /// "10.2.0.2" across all bindings; per-tunnel routing is by fwmark.
    pub listen_addr: String,
    /// Listen port on this binding (the *internal* port the WG tunnel
    /// forwards external NAT-PMP-mapped traffic to). Distinct per binding
    /// even when listen_addr is shared.
    pub listen_port: u16,
    /// Publicly-reachable port advertised in the BEP-10 extension handshake
    /// `p` field and used by the Go orchestrator for tracker announces.
    /// Differs from `listen_port` when behind NAT (Proton WG: this is the
    /// NAT-PMP-mapped external port). 0 = "fall back to listen_port"
    /// (legacy single-binding without NAT translation).
    #[serde(default)]
    pub announce_port: u16,
    /// Optional public IP we'd like trackers to advertise for us.
    #[serde(default)]
    pub public_ip: String,
    /// netfilter fwmark applied via SO_MARK on outbound peer dial sockets
    /// so the kernel routes them through the right WG tunnel
    /// (`ip rule fwmark X lookup tableX`). 0 = no fwmark (single-tunnel
    /// FOU/wstunnel path).
    #[serde(default)]
    pub fwmark: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EngineConfig {
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
    #[serde(default)]
    pub resume_dir: String,
    #[serde(default)]
    pub socket_path: String,
    #[serde(default = "default_listen_port")]
    pub listen_port: u16,
    #[serde(default)]
    pub listen_interfaces: String,
    /// Optional extra TCP listener that expects HAProxy PROXY protocol v2
    /// header at the start of each connection (real peer IP carried in header).
    /// Bind addr taken from `listen_addr_proxy_v2` (default [::]). None = disabled.
    #[serde(default)]
    pub listen_port_proxy_v2: Option<u16>,
    /// Explicit bind address for the PROXY v2 listener (e.g. "[d12::2]").
    /// Binding explicitly on the v6 global addr exposed to haproxy VPS avoids
    /// the Linux source-selection bug where replies on a wildcard listener
    /// go out with the kernel-preferred src (another addr on the same iface).
    #[serde(default)]
    pub listen_addr_proxy_v2: Option<String>,
    /// Extra IPs whose PROXY v2 headers are trusted (e.g. VPS haproxy v6).
    /// FW must restrict inbound 16271/16272 to these sources only.
    #[serde(default)]
    pub proxy_v2_trusted_sources: Vec<String>,
    /// SOCKS5 outbound proxy for v6 peer dials (VPS bypass). Empty = disabled.
    #[serde(default)]
    pub socks5_outbound_host: String,
    #[serde(default = "default_socks5_port")]
    pub socks5_outbound_port: u16,
    #[serde(default)]
    pub socks5_outbound_user: String,
    #[serde(default)]
    pub socks5_outbound_pass: String,
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,
    #[serde(default = "default_max_uploads_per_torrent")]
    pub max_uploads_per_torrent: i32,
    #[serde(default = "default_peer_timeout")]
    pub peer_timeout: u64,
    #[serde(default = "default_peer_timeout")]
    pub inactivity_timeout: u64,
    #[serde(default)]
    pub upload_limit: u64,
    #[serde(default)]
    pub download_limit: u64,
    #[serde(default = "default_file_pool_size")]
    pub file_pool_size: usize,
    #[serde(default = "default_peer_fingerprint")]
    pub peer_fingerprint: String,
    #[serde(default = "default_user_agent")]
    pub user_agent: String,
    /// When true, Typhon's internal HTTP tracker announce loop is skipped.
    /// The dial queue (PEX/DHT outbound) stays wired up. Used when the Go
    /// orchestrator owns announces (canonical post-multi-binding migration).
    #[serde(default)]
    pub disable_internal_announce: bool,
    /// Per-tunnel network bindings. Empty = legacy single-binding derived
    /// from `listen_port`/`listen_interfaces`/`peer_fingerprint`. Non-empty
    /// = each binding owns one TCP listener and one source-bound dial path
    /// with its own peer_id. See `Binding` doc above for design context.
    #[serde(default)]
    pub bindings: Vec<Binding>,
}

fn default_data_dir() -> String { "/configs".into() }
fn default_listen_port() -> u16 { 16172 }
fn default_max_connections() -> usize { 12000 }
fn default_max_uploads_per_torrent() -> i32 { -1 }
fn default_peer_timeout() -> u64 { 300 }
fn default_file_pool_size() -> usize { 5000 }
fn default_socks5_port() -> u16 { 1080 }
fn default_peer_fingerprint() -> String { "-HY2430-".into() }
fn default_user_agent() -> String { "Hydra/2.4.3-typhon".into() }

impl EngineConfig {
    pub fn load(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let data = fs::read_to_string(path)?;
        let mut config: Self = serde_json::from_str(&data)?;
        if config.resume_dir.is_empty() {
            config.resume_dir = format!("{}/resume", config.data_dir);
        }
        fs::create_dir_all(&config.resume_dir).ok();
        Ok(config)
    }

    pub fn peer_id(&self) -> [u8; 20] {
        let prefix = self.peer_fingerprint.as_bytes();
        let mut id = [0u8; 20];
        let copy_len = prefix.len().min(8);
        id[..copy_len].copy_from_slice(&prefix[..copy_len]);
        use rand::Rng;
        let mut rng = rand::thread_rng();
        for b in &mut id[copy_len..] {
            *b = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz"
                [rng.gen_range(0..62)];
        }
        id
    }

    pub fn listen_addr(&self) -> String {
        if self.listen_interfaces.is_empty() {
            format!("0.0.0.0:{}", self.listen_port)
        } else {
            self.listen_interfaces.clone()
        }
    }

    /// Resolve the configured bindings to a concrete list of (SocketAddr, peer_id)
    /// pairs ready for `peer::listen()`. Non-empty `self.bindings` is the source
    /// of truth; otherwise we synthesize one binding per `listen_interfaces` entry
    /// (legacy CSV path) sharing the global `peer_fingerprint`-derived peer_id.
    pub fn resolved_bindings(&self) -> Vec<ResolvedBinding> {
        // New path: explicit bindings.
        if !self.bindings.is_empty() {
            let mut out = Vec::with_capacity(self.bindings.len());
            for b in &self.bindings {
                let addr_str = format!("{}:{}", b.listen_addr, b.listen_port);
                let addr: std::net::SocketAddr = match addr_str.parse() {
                    Ok(a) => a,
                    Err(_) => {
                        // Skip malformed bindings — log handled by caller.
                        continue;
                    }
                };
                let pid = if b.peer_id.is_empty() {
                    self.peer_id()
                } else {
                    let mut p = [0u8; 20];
                    let bytes = b.peer_id.as_bytes();
                    let n = bytes.len().min(20);
                    p[..n].copy_from_slice(&bytes[..n]);
                    p
                };
                let advertised = if b.announce_port != 0 { b.announce_port } else { b.listen_port };
                out.push(ResolvedBinding {
                    id: b.id,
                    addr,
                    peer_id: pid,
                    fwmark: b.fwmark,
                    advertised_port: advertised,
                });
            }
            return out;
        }
        // Legacy path: derive bindings from listen_interfaces / listen_port.
        let global_pid = self.peer_id();
        let mut out = Vec::new();
        if self.listen_interfaces.is_empty() {
            if let Ok(addr) = format!("0.0.0.0:{}", self.listen_port).parse::<std::net::SocketAddr>() {
                let port = addr.port();
                out.push(ResolvedBinding { id: 0, addr, peer_id: global_pid, fwmark: 0, advertised_port: port });
            }
        } else {
            for (i, part) in self.listen_interfaces.split(',').enumerate() {
                let s = part.trim();
                if s.is_empty() { continue; }
                if let Ok(addr) = s.parse::<std::net::SocketAddr>() {
                    let port = addr.port();
                    out.push(ResolvedBinding { id: i as u32, addr, peer_id: global_pid, fwmark: 0, advertised_port: port });
                }
            }
        }
        out
    }
}

/// Resolved binding: ready-to-use config for one network tunnel. `addr` is
/// the listen socket address; `fwmark` is applied via SO_MARK on outbound
/// dial sockets to steer them through the matching WG interface.
/// `advertised_port` is the port we tell remote peers about in the BEP-10
/// extension handshake (NAT-PMP external port for Proton WG, equal to
/// addr.port() in the legacy direct-listen case).
#[derive(Debug, Clone)]
pub struct ResolvedBinding {
    pub id: u32,
    pub addr: std::net::SocketAddr,
    pub peer_id: [u8; 20],
    pub fwmark: u32,
    pub advertised_port: u16,
}
