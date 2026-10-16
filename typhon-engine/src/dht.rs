//! BEP 5 DHT integration via `librqbit-dht`.
//!
//! We bootstrap a DHT node at startup and, for every non-`private` torrent,
//! spawn a task that streams peers via `get_peers()` and funnels them into
//! the existing dial queue (`crate::tracker::enqueue_dial`).
//!
//! Private-tracker torrents (`TorrentMeta.private == true`) are skipped — BEP 27
//! forbids DHT for those and many trackers ban clients that announce them.
//!
//! Every spawned task is registered in `TRACKED` so it can be cancelled. Without
//! that, the only way out of the stream loop was the `is_removed` flag, and it is
//! only observed when the stream happens to yield a peer — so a *stopped* torrent
//! kept its `get_peers` recursion running forever, and a *removed* one could too
//! if its stream went quiet. Upstream's `request_peers_forever` pushes into an
//! unbounded `FuturesUnordered`, so an orphaned task is not merely idle: it grows
//! the heap for as long as the process lives.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use dashmap::DashMap;
use futures::StreamExt;
use librqbit_dht::{Dht, DhtBuilder, Id20};
use tokio::task::AbortHandle;
use tracing::{info, warn};

use crate::torrent::meta::{InfoHash, TorrentState};

/// Whether an engine may run a DHT node, and why not when it may not.
///
/// Decided in one place for the two discovery mechanisms that speak plain UDP
/// to strangers -- the DHT and Local Service Discovery --
/// because they share the reason to be off: the SOCKS5 proxy here has no UDP
/// ASSOCIATE, so neither can go through it. A DHT node behind the proxy would
/// leave by the host's route and hand its real address to every node in the
/// routing table, which is exactly what the proxy was set up to hide. qBittorrent
/// does the same: with a proxy for peers, DHT and LSD are off.
///
/// PEX is NOT covered: it rides the peer connections, which are proxied.
///
/// A tunnel is not a proxy. An engine pinned to a WireGuard device keeps its
/// DHT, pinned to the same device (`DhtSession::start`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Discovery {
    On,
    /// Off, with the sentence the log and the API say it with.
    Off(&'static str),
}

pub const OFF_BY_CONFIG: &str = "disabled by config";
pub const OFF_BEHIND_PROXY: &str =
    "off behind the SOCKS5 proxy: the DHT is plain UDP, the proxy carries no UDP, and DHT nodes would see this host's address";
pub const LSD_OFF_BEHIND_PROXY: &str =
    "off behind the SOCKS5 proxy: LSD is UDP multicast, which no proxy carries, and it would announce this host's own address";

impl Discovery {
    pub fn is_on(&self) -> bool {
        matches!(self, Discovery::On)
    }
}

/// The DHT decision for one engine.
pub fn dht_policy(config: &crate::config::EngineConfig) -> Discovery {
    if !config.dht_enabled {
        Discovery::Off(OFF_BY_CONFIG)
    } else if !config.socks5_outbound_host.trim().is_empty() {
        Discovery::Off(OFF_BEHIND_PROXY)
    } else {
        Discovery::On
    }
}

/// The Local Service Discovery (BEP 14) decision (`lsd::start`).
///
/// Multicast on the LAN, never through a proxy, so it is off behind one for
/// the same reason as the DHT. Its own switch, `lsd_enabled`, rather than the
/// DHT's: a hoard can want one without the other.
pub fn lsd_policy(config: &crate::config::EngineConfig) -> Discovery {
    if !config.lsd_enabled {
        Discovery::Off(OFF_BY_CONFIG)
    } else if !config.socks5_outbound_host.trim().is_empty() {
        Discovery::Off(LSD_OFF_BEHIND_PROXY)
    } else {
        Discovery::On
    }
}

/// The bootstrap routers as addresses, resolved by the tunnel's DNS server.
///
/// The DHT resolves its routers itself with the host's resolver, which for an
/// engine in a managed tunnel tells the host's DNS provider that this machine
/// runs a DHT node. Handing it literal addresses leaves it nothing to resolve.
/// A router the tunnel cannot resolve is dropped, never asked of the host: no
/// router at all is a DHT that does not bootstrap, the trackers carry on.
async fn tunnel_bootstrap(device: &str) -> Vec<String> {
    let mut out = Vec::new();
    for router in librqbit_dht::DHT_BOOTSTRAP {
        let Some((host, port)) = router.rsplit_once(':') else { continue };
        match crate::tunneldns::resolve(device, host).await {
            Ok(ips) => out.extend(ips.into_iter().map(|ip| std::net::SocketAddr::new(ip, port.parse().unwrap_or(6881)).to_string())),
            Err(e) => warn!("[dht] bootstrap router {} not resolved through the tunnel: {}", host, e),
        }
    }
    out
}

/// One DHT node, owned by one engine.
///
/// This used to be a set of `OnceLock` statics, which was sound while one
/// engine meant one process. Hydra 4 carries race and hoard in the same
/// process: a shared node would have given both engines one identity and one
/// UDP port, merged their tracked-torrent tables, and -- because the counters
/// were global too -- reported the sum of both under whichever engine was
/// asked. It would also have quietly undone `enable_dht = false` on hoard,
/// which is what keeps 240k idle torrents from paying for a DHT they never use.
pub struct DhtSession {
    dht: Dht,
    /// Live `get_peers` tasks, keyed by info hash. One entry per tracked torrent.
    tracked: DashMap<InfoHash, AbortHandle>,
    torrents_tracked: AtomicU64,
    peers_discovered: AtomicU64,
    peers_dialed: AtomicU64,
    /// `dht_follow_seeds`: keep `get_peers` running for finished torrents.
    /// Starts true, the behaviour so far; `session.rs` sets it from the config.
    follow_seeds: AtomicBool,
}

/// Whether a torrent has nothing left to download: seed mode, or no picker
/// (it seeds), or a complete one.
fn is_finished(t: &TorrentState) -> bool {
    t.seed_mode || t.picker.get().is_none_or(|p| p.lock().unwrap_or_else(|e| e.into_inner()).is_complete())
}

/// Whether the DHT should follow this torrent: never a private one (BEP 27),
/// and a finished one only when `follow_seeds` is on.
pub fn tracks_in_dht(t: &TorrentState, follow_seeds: bool) -> bool {
    t.meta.allows_peer_discovery() && (follow_seeds || !is_finished(t))
}

impl DhtSession {
    /// Bootstrap a node. None when bootstrap fails, which is not fatal: the
    /// engine keeps announcing to its trackers.
    ///
    /// `device` pins the node's socket like every other socket of the engine.
    /// It was built on the default route whatever `bind_interface` said, so an
    /// engine behind a tunnel ran its DHT from the host's own address. A pin
    /// that fails is a DHT that does not start, never one that runs unpinned.
    pub async fn start(device: Option<&str>) -> Option<Arc<Self>> {
        let config = librqbit_dht::DhtConfig {
            bind_device: device.map(str::to_string),
            bootstrap_addrs: match device {
                Some(d) if crate::tunneldns::is_tunnel(d) => Some(tunnel_bootstrap(d).await),
                _ => None,
            },
            ..Default::default()
        };
        match DhtBuilder::with_config(config).await {
            Ok(dht) => {
                match device {
                    Some(d) => info!("[dht] bootstrapped, pinned to device {}", d),
                    None => info!("[dht] bootstrapped"),
                }
                Some(Arc::new(Self {
                    dht,
                    tracked: DashMap::new(),
                    torrents_tracked: AtomicU64::new(0),
                    peers_discovered: AtomicU64::new(0),
                    peers_dialed: AtomicU64::new(0),
                    follow_seeds: AtomicBool::new(true),
                }))
            }
            Err(e) => {
                warn!("[dht] bootstrap failed: {:#}", e);
                None
            }
        }
    }

    /// Set `dht_follow_seeds` (default true) for the torrents tracked from now on.
    pub fn set_follow_seeds(&self, follow: bool) {
        self.follow_seeds.store(follow, Ordering::Relaxed);
    }

    /// Handle to the node. Magnet resolution needs peers for an info hash that
    /// has no TorrentState behind it yet.
    pub fn handle(&self) -> Dht {
        self.dht.clone()
    }

    /// Nodes in this engine's routing table: qBittorrent's `dht_nodes`.
    pub fn node_count(&self) -> usize {
        self.dht.stats().routing_table_size
    }

    /// Number of torrents currently streaming peers from the DHT.
    pub fn tracked_count(&self) -> usize {
        self.tracked.len()
    }

    pub fn torrents_tracked(&self) -> u64 {
        self.torrents_tracked.load(Ordering::Relaxed)
    }

    pub fn peers_discovered(&self) -> u64 {
        self.peers_discovered.load(Ordering::Relaxed)
    }

    pub fn peers_dialed(&self) -> u64 {
        self.peers_dialed.load(Ordering::Relaxed)
    }

    /// Register a torrent with the DHT. Skips `private` torrents (BEP 27).
    /// Spawns a task that streams peers and enqueues them for dialing.
    ///
    /// Idempotent: a torrent already tracked keeps its existing task rather
    /// than gaining a second one. `start_torrent` calls this on every resume,
    /// and the boot loop calls it for every loaded torrent.
    pub fn track_torrent(self: &Arc<Self>, torrent: Arc<TorrentState>) {
        if !tracks_in_dht(&torrent, self.follow_seeds.load(Ordering::Relaxed)) {
            return;
        }
        let ih = torrent.info_hash;
        if self.tracked.contains_key(&ih) {
            return;
        }
        let stream = self.dht.get_peers(Id20::new(ih), None);
        self.track_with(torrent, stream);
    }

    /// The body of `track_torrent`, over any stream of peers.
    ///
    /// When the stream stops being followed (the torrent finished while seeds
    /// are not followed, was removed, or the stream ended) the entry leaves
    /// `tracked` and `torrents_tracked` with it, so a torrent that needs peers
    /// again (a recheck) is tracked afresh and the count stays true.
    fn track_with<S>(self: &Arc<Self>, torrent: Arc<TorrentState>, stream: S)
    where
        S: futures::Stream<Item = std::net::SocketAddr> + Send + 'static,
    {
        let ih = torrent.info_hash;
        let session = self.clone();
        let handle = tokio::spawn(async move {
            let mut stream = Box::pin(stream);
            while let Some(peer_addr) = stream.next().await {
                if torrent.is_removed.load(Ordering::Relaxed) {
                    break;
                }
                // Finished since it was tracked, and seeds are not followed.
                if !tracks_in_dht(&torrent, session.follow_seeds.load(Ordering::Relaxed)) {
                    break;
                }
                session.peers_discovered.fetch_add(1, Ordering::Relaxed);
                if torrent.connected_addrs.contains_key(&peer_addr) {
                    continue;
                }
                session.peers_dialed.fetch_add(1, Ordering::Relaxed);
                crate::tracker::enqueue_dial(peer_addr, torrent.clone());
            }
            session.untrack_torrent(&ih);
        });
        // Race: two concurrent track_torrent calls for the same hash both pass
        // the contains_key check. The loser's task is aborted so we never leak
        // one.
        if let Some(previous) = self.tracked.insert(ih, handle.abort_handle()) {
            previous.abort();
        } else {
            self.torrents_tracked.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Cancel a torrent's `get_peers` task. Safe to call for a torrent that was
    /// never tracked (private, or added before the DHT bootstrapped).
    pub fn untrack_torrent(&self, info_hash: &InfoHash) {
        if let Some((_, handle)) = self.tracked.remove(info_hash) {
            handle.abort();
            self.torrents_tracked.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(json: &str) -> crate::config::EngineConfig {
        serde_json::from_str(json).expect("engine config")
    }

    /// ⭐ Behind the SOCKS5 proxy the DHT is off whatever `dht_enabled` says,
    /// and LSD with it; without a proxy the switch is the operator's.
    #[test]
    fn a_proxied_engine_runs_no_dht_and_no_lsd() {
        let proxied = config(r#"{"socks5_outbound_host":"10.0.0.1","dht_enabled":true,"lsd_enabled":true}"#);
        assert_eq!(dht_policy(&proxied), Discovery::Off(OFF_BEHIND_PROXY));
        assert_eq!(lsd_policy(&proxied), Discovery::Off(LSD_OFF_BEHIND_PROXY));

        let direct = config(r#"{"dht_enabled":true,"lsd_enabled":true}"#);
        assert!(dht_policy(&direct).is_on());
        assert!(lsd_policy(&direct).is_on());
        let off = config(r#"{"dht_enabled":false}"#);
        assert_eq!(dht_policy(&off), Discovery::Off(OFF_BY_CONFIG));
        // LSD is off unless asked for: the engine has no role to pick a
        // default from, Hydra decides it per role (`Session::lsd_on`).
        assert_eq!(lsd_policy(&off), Discovery::Off(OFF_BY_CONFIG));
    }

    /// A tunnel is not a proxy: an engine pinned to a device keeps its DHT.
    #[test]
    fn a_pinned_engine_keeps_its_dht() {
        let pinned = config(r#"{"bind_device":"wg-race","dht_enabled":true}"#);
        assert!(dht_policy(&pinned).is_on());
    }

    /// A device that does not exist is a DHT that does not start, never one
    /// that falls back to the default route.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_dht_pinned_to_a_missing_device_does_not_start() {
        assert!(DhtSession::start(Some("hy-nodev0")).await.is_none());
    }

    fn manager(tag: &str) -> (Arc<crate::torrent::TorrentManager>, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("typhon-dht-{tag}-{}", std::process::id()));
        let (data, resume) = (root.join("data"), root.join("resume"));
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&resume).unwrap();
        let mgr = Arc::new(crate::torrent::TorrentManager::new(
            data.to_string_lossy().into_owned(),
            resume.to_string_lossy().into_owned(),
            Arc::new(crate::disk::DiskManager::new(16)),
        ));
        (mgr, root)
    }

    fn torrent_bytes(name: &str, private: bool) -> Vec<u8> {
        let mut info = format!("d6:lengthi16384e4:name{}:{name}12:piece lengthi16384e6:pieces20:", name.len()).into_bytes();
        let mut piece = [0xCDu8; 20];
        piece[..name.len().min(20)].copy_from_slice(&name.as_bytes()[..name.len().min(20)]);
        info.extend_from_slice(&piece);
        if private {
            info.extend_from_slice(b"7:privatei1e");
        }
        info.push(b'e');
        let mut out = b"d4:info".to_vec();
        out.extend_from_slice(&info);
        out.push(b'e');
        out
    }

    /// Default unchanged, as far as it can be said without a DHT node (no new
    /// symbol, so it also compiles on the parent): a finished public torrent
    /// and a download both allow peer discovery; a private one does not.
    #[test]
    fn peer_discovery_is_allowed_for_public_torrents_finished_or_not() {
        let (mgr, root) = manager("allows");
        let add = |name: &str, private: bool, seed: bool| {
            let (ih, _) = mgr.add_torrent_bytes(&torrent_bytes(name, private), "/tmp", false, seed).unwrap();
            mgr.get(&ih).unwrap()
        };
        assert!(add("down", false, false).meta.allows_peer_discovery());
        assert!(add("seed", false, true).meta.allows_peer_discovery());
        assert!(!add("priv", true, false).meta.allows_peer_discovery());
        let _ = std::fs::remove_dir_all(root);
    }

    /// `dht_follow_seeds = false` keeps finished torrents off the DHT; a
    /// download is followed either way, and a private torrent never is.
    #[test]
    fn finished_torrents_follow_the_dht_only_when_the_option_says_so() {
        let (mgr, root) = manager("follow");
        let add = |name: &str, private: bool, seed: bool| {
            let (ih, _) = mgr.add_torrent_bytes(&torrent_bytes(name, private), "/tmp", false, seed).unwrap();
            mgr.get(&ih).unwrap()
        };
        let down = add("down", false, false);
        let seed = add("seed", false, true);
        let private = add("priv", true, false);
        assert!(private.meta.private);
        for follow in [true, false] {
            assert!(tracks_in_dht(&down, follow), "a download is followed (follow={follow})");
            assert!(!tracks_in_dht(&private, follow), "a private torrent never is (follow={follow})");
        }
        assert!(tracks_in_dht(&seed, true), "default: a seed is followed");
        assert!(!tracks_in_dht(&seed, false), "option off: a seed is not");
        let _ = std::fs::remove_dir_all(root);
    }

    /// A node with no bootstrap peers: enough to own a `tracked` table.
    async fn offline_session() -> Arc<DhtSession> {
        let config = librqbit_dht::DhtConfig { bootstrap_addrs: Some(Vec::new()), ..Default::default() };
        let dht = DhtBuilder::with_config(config).await.expect("offline dht node");
        Arc::new(DhtSession {
            dht,
            tracked: DashMap::new(),
            torrents_tracked: AtomicU64::new(0),
            peers_discovered: AtomicU64::new(0),
            peers_dialed: AtomicU64::new(0),
            follow_seeds: AtomicBool::new(false),
        })
    }

    /// `dht_follow_seeds = false`: when the lookup stops because the torrent
    /// finished, its entry leaves the table and the count, so a torrent that
    /// needs peers again is tracked afresh and `torrents_tracked` stays true.
    #[tokio::test]
    async fn a_lookup_stopped_for_a_finished_torrent_is_forgotten() {
        let (mgr, root) = manager("forget");
        let (ih, _) = mgr.add_torrent_bytes(&torrent_bytes("fin", false), "/tmp", false, false).unwrap();
        let torrent = mgr.get(&ih).unwrap();
        let session = offline_session().await;
        let peer: std::net::SocketAddr = "192.168.99.7:6881".parse().unwrap();

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        session.track_with(torrent.clone(), tokio_stream_from(rx));
        assert_eq!((session.tracked_count(), session.torrents_tracked()), (1, 1));

        // The torrent finishes; the next peer found stops the lookup.
        torrent.picker.get().expect("picker").lock().unwrap().set_have(0);
        assert!(!tracks_in_dht(&torrent, false));
        tx.send(peer).unwrap();
        for _ in 0..100 {
            if session.tracked_count() == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(session.tracked_count(), 0, "the stopped lookup left its entry behind");
        assert_eq!(session.torrents_tracked(), 0, "torrents_tracked counts a lookup that is gone");

        // An incomplete torrent can be followed again (the same hash, new task).
        let (_tx2, rx2) = tokio::sync::mpsc::unbounded_channel();
        session.track_with(torrent, tokio_stream_from(rx2));
        assert_eq!((session.tracked_count(), session.torrents_tracked()), (1, 1));
        let _ = std::fs::remove_dir_all(root);
    }

    fn tokio_stream_from(
        mut rx: tokio::sync::mpsc::UnboundedReceiver<std::net::SocketAddr>,
    ) -> impl futures::Stream<Item = std::net::SocketAddr> + Send + 'static {
        futures::stream::poll_fn(move |cx| rx.poll_recv(cx))
    }
}
