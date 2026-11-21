//! BEP 5 DHT integration via `librqbit-dht`.
//!
//! We bootstrap a DHT node at startup and, for every non-`private` torrent,
//! spawn a task that streams peers via `get_peers()` and funnels them into
//! the existing dial queue (`crate::tracker::enqueue_dial`).
//!
//! Each lookup carries the engine's announced port, so the nodes closest to
//! the info hash are sent `announce_peer` and the swarm learns our address.
//! Without a port `librqbit-dht` only searches, and nobody who relied on the
//! DHT could find us, seeds included.
//!
//! A finished torrent whose seeds are not followed (`dht_follow_seeds =
//! false`) drops the endless search but is still announced: one short lookup
//! every `ANNOUNCE_INTERVAL`, its peers left to dial us.
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

use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use dashmap::DashMap;
use futures::StreamExt;
use librqbit_dht::{Dht, DhtBuilder, Id20};
use tokio::sync::Notify;
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
        let Some((host, port)) = router.rsplit_once(':') else {
            continue;
        };
        match crate::tunneldns::resolve(device, host).await {
            Ok(ips) => {
                out.extend(ips.into_iter().map(|ip| {
                    std::net::SocketAddr::new(ip, port.parse().unwrap_or(6881)).to_string()
                }))
            }
            Err(e) => warn!(
                "[dht] bootstrap router {} not resolved through the tunnel: {}",
                host, e
            ),
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
    /// Live tasks, keyed by info hash. One entry per tracked torrent.
    tracked: DashMap<InfoHash, Tracked>,
    torrents_tracked: AtomicU64,
    peers_discovered: AtomicU64,
    peers_dialed: AtomicU64,
    /// `dht_follow_seeds`: keep `get_peers` running for finished torrents.
    /// Starts true, the behaviour so far; `session.rs` sets it from the config.
    follow_seeds: AtomicBool,
    /// The port to announce, asked again for every lookup. Unset, or None
    /// (the port is not known yet), a lookup only searches.
    announce_port: OnceLock<PortSource>,
}

/// Where the port to announce comes from (`set_announce_port`).
type PortSource = Box<dyn Fn() -> Option<u16> + Send + Sync>;

/// How often a finished torrent whose seeds are not followed is announced:
/// libtorrent's `dht_announce_interval`. Nodes keep a peer about 30 minutes.
const ANNOUNCE_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// How long one announce-only lookup runs: the recursion reaches the nodes
/// closest to the hash, and announces to them, well within it.
const ANNOUNCE_WINDOW: Duration = Duration::from_secs(30);

/// One tracked torrent: its task, and the bell `track_torrent` rings so a
/// waiting task looks again at whether it should search.
struct Tracked {
    abort: AbortHandle,
    wake: Arc<Notify>,
}

/// Whether a torrent has nothing left to download: seed mode, or no picker
/// (it seeds), or a complete one.
fn is_finished(t: &TorrentState) -> bool {
    t.seed_mode
        || t.picker
            .get()
            .is_none_or(|p| p.lock().unwrap_or_else(|e| e.into_inner()).is_complete())
}

/// Whether the DHT should keep searching peers for this torrent: never a
/// private one (BEP 27), and a finished one only when `follow_seeds` is on.
/// A public torrent that does not search is still announced.
pub fn searches_in_dht(t: &TorrentState, follow_seeds: bool) -> bool {
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
                    announce_port: OnceLock::new(),
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

    /// Where the port to announce comes from: the engine's announced port
    /// (the external one behind NAT-PMP), None while it is not known. Set
    /// once, before any torrent is tracked; asked again at every lookup.
    pub fn set_announce_port(&self, port: impl Fn() -> Option<u16> + Send + Sync + 'static) {
        let _ = self.announce_port.set(Box::new(port));
    }

    fn port_to_announce(&self) -> Option<u16> {
        self.announce_port
            .get()
            .and_then(|port| port())
            .filter(|p| *p != 0)
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
    /// Spawns a task that streams peers, enqueues them for dialing, and
    /// announces the torrent on every lookup.
    ///
    /// Idempotent: a torrent already tracked keeps its existing task rather
    /// than gaining a second one. `start_torrent` calls this on every resume,
    /// and the boot loop calls it for every loaded torrent. The existing task
    /// is woken, so a finished torrent that needs pieces again searches now
    /// rather than at its next announce.
    pub fn track_torrent(self: &Arc<Self>, torrent: Arc<TorrentState>) {
        if !torrent.meta.allows_peer_discovery() {
            return;
        }
        let ih = torrent.info_hash;
        if let Some(entry) = self.tracked.get(&ih) {
            entry.wake.notify_one();
            return;
        }
        let dht = self.dht.clone();
        self.track_with(torrent, move |port| dht.get_peers(Id20::new(ih), port));
    }

    /// The body of `track_torrent`, over any lookup: `lookup(port)` opens a
    /// stream of peers that announces `port` when it is Some.
    ///
    /// The task searches while `searches_in_dht` says so. Once it does not
    /// (finished, seeds not followed) it announces every `ANNOUNCE_INTERVAL`
    /// instead, and searches again when woken for a torrent that needs pieces
    /// again. When the task ends (the torrent was removed, or a search stream
    /// ended) the entry leaves `tracked` and `torrents_tracked` with it, so a
    /// later `track_torrent` tracks it afresh and the count stays true.
    fn track_with<L, S>(self: &Arc<Self>, torrent: Arc<TorrentState>, mut lookup: L)
    where
        L: FnMut(Option<u16>) -> S + Send + 'static,
        S: futures::Stream<Item = std::net::SocketAddr> + Send + 'static,
    {
        let ih = torrent.info_hash;
        let session = self.clone();
        let wake = Arc::new(Notify::new());
        let woken = wake.clone();
        let handle = tokio::spawn(async move {
            while !torrent.is_removed.load(Ordering::Relaxed) {
                if session.searches(&torrent) {
                    let stream = lookup(session.port_to_announce());
                    if session.search(&torrent, stream).await.is_break() {
                        break;
                    }
                    continue;
                }
                if let Some(port) = session.port_to_announce() {
                    announce_once(lookup(Some(port))).await;
                }
                tokio::select! {
                    _ = tokio::time::sleep(ANNOUNCE_INTERVAL) => {}
                    _ = woken.notified() => {}
                }
            }
            session.untrack_torrent(&ih);
        });
        // Race: two concurrent track_torrent calls for the same hash both pass
        // the `tracked.get` check. The loser's task is aborted so we never
        // leak one.
        let entry = Tracked {
            abort: handle.abort_handle(),
            wake,
        };
        if let Some(previous) = self.tracked.insert(ih, entry) {
            previous.abort.abort();
        } else {
            self.torrents_tracked.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn searches(&self, torrent: &TorrentState) -> bool {
        searches_in_dht(torrent, self.follow_seeds.load(Ordering::Relaxed))
    }

    /// Dial what a search stream finds. Continue: the torrent stopped
    /// searching (finished, seeds not followed). Break: it was removed or the
    /// stream ended, and the task is over.
    async fn search<S>(&self, torrent: &Arc<TorrentState>, stream: S) -> ControlFlow<()>
    where
        S: futures::Stream<Item = std::net::SocketAddr>,
    {
        let mut stream = std::pin::pin!(stream);
        while let Some(peer_addr) = stream.next().await {
            if torrent.is_removed.load(Ordering::Relaxed) {
                return ControlFlow::Break(());
            }
            // Finished since it was tracked, and seeds are not followed.
            if !self.searches(torrent) {
                return ControlFlow::Continue(());
            }
            self.peers_discovered.fetch_add(1, Ordering::Relaxed);
            if torrent.peers.contains_key(&peer_addr) {
                continue;
            }
            self.peers_dialed.fetch_add(1, Ordering::Relaxed);
            crate::tracker::enqueue_dial(peer_addr, torrent.clone());
        }
        ControlFlow::Break(())
    }

    /// Cancel a torrent's DHT task. Safe to call for a torrent that was
    /// never tracked (private, or added before the DHT bootstrapped).
    pub fn untrack_torrent(&self, info_hash: &InfoHash) {
        if let Some((_, entry)) = self.tracked.remove(info_hash) {
            entry.abort.abort();
            self.torrents_tracked.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// Run one lookup long enough to announce, then drop it. What it finds is
/// left alone: a seed that is not followed waits for peers to dial it.
async fn announce_once<S>(stream: S)
where
    S: futures::Stream<Item = std::net::SocketAddr>,
{
    let _ = tokio::time::timeout(ANNOUNCE_WINDOW, stream.for_each(|_| async {})).await;
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
        let proxied =
            config(r#"{"socks5_outbound_host":"10.0.0.1","dht_enabled":true,"lsd_enabled":true}"#);
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
        let mut info = format!(
            "d6:lengthi16384e4:name{}:{name}12:piece lengthi16384e6:pieces20:",
            name.len()
        )
        .into_bytes();
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
            let (ih, _) = mgr
                .add_torrent_bytes(&torrent_bytes(name, private), "/tmp", false, seed)
                .unwrap();
            mgr.get(&ih).unwrap()
        };
        assert!(add("down", false, false).meta.allows_peer_discovery());
        assert!(add("seed", false, true).meta.allows_peer_discovery());
        assert!(!add("priv", true, false).meta.allows_peer_discovery());
        let _ = std::fs::remove_dir_all(root);
    }

    /// `dht_follow_seeds = false` stops the search for finished torrents; a
    /// download searches either way, and a private torrent never does.
    #[test]
    fn finished_torrents_search_the_dht_only_when_the_option_says_so() {
        let (mgr, root) = manager("follow");
        let add = |name: &str, private: bool, seed: bool| {
            let (ih, _) = mgr
                .add_torrent_bytes(&torrent_bytes(name, private), "/tmp", false, seed)
                .unwrap();
            mgr.get(&ih).unwrap()
        };
        let down = add("down", false, false);
        let seed = add("seed", false, true);
        let private = add("priv", true, false);
        assert!(private.meta.private);
        for follow in [true, false] {
            assert!(
                searches_in_dht(&down, follow),
                "a download searches (follow={follow})"
            );
            assert!(
                !searches_in_dht(&private, follow),
                "a private torrent never does (follow={follow})"
            );
        }
        assert!(searches_in_dht(&seed, true), "default: a seed searches");
        assert!(
            !searches_in_dht(&seed, false),
            "option off: a seed does not"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// A node with no bootstrap peers: enough to own a `tracked` table.
    async fn offline_session(follow_seeds: bool, port: Option<u16>) -> Arc<DhtSession> {
        let config = librqbit_dht::DhtConfig {
            bootstrap_addrs: Some(Vec::new()),
            ..Default::default()
        };
        let dht = DhtBuilder::with_config(config)
            .await
            .expect("offline dht node");
        let session = Arc::new(DhtSession {
            dht,
            tracked: DashMap::new(),
            torrents_tracked: AtomicU64::new(0),
            peers_discovered: AtomicU64::new(0),
            peers_dialed: AtomicU64::new(0),
            follow_seeds: AtomicBool::new(follow_seeds),
            announce_port: OnceLock::new(),
        });
        session.set_announce_port(move || port);
        session
    }

    type SocketAddr = std::net::SocketAddr;
    /// One lookup the task opened: the port it announces, and the sender
    /// behind its stream of peers (dropped, the stream ends).
    type Lookup = (Option<u16>, tokio::sync::mpsc::UnboundedSender<SocketAddr>);

    /// A lookup that reports every stream it opens instead of asking the DHT.
    fn fake_lookups() -> (
        impl Fn(Option<u16>) -> futures::stream::BoxStream<'static, SocketAddr> + Send + 'static,
        tokio::sync::mpsc::UnboundedReceiver<Lookup>,
    ) {
        let (opened_tx, opened_rx) = tokio::sync::mpsc::unbounded_channel();
        let lookup = move |port| {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let _ = opened_tx.send((port, tx));
            futures::stream::poll_fn(move |cx| rx.poll_recv(cx)).boxed()
        };
        (lookup, opened_rx)
    }

    async fn next_lookup(opened: &mut tokio::sync::mpsc::UnboundedReceiver<Lookup>) -> Lookup {
        tokio::time::timeout(Duration::from_secs(5), opened.recv())
            .await
            .expect("no lookup was opened")
            .expect("the task is gone")
    }

    const PORT: u16 = 51413;

    /// ⭐ The lookup of a public torrent announces the engine's port, so the
    /// DHT nodes closest to the hash learn where to find us.
    #[tokio::test]
    async fn a_public_torrent_is_announced_with_the_engine_port() {
        let (mgr, root) = manager("announce");
        let (ih, _) = mgr
            .add_torrent_bytes(&torrent_bytes("down", false), "/tmp", false, false)
            .unwrap();
        let session = offline_session(true, Some(PORT)).await;
        let (lookup, mut opened) = fake_lookups();
        session.track_with(mgr.get(&ih).unwrap(), lookup);
        assert_eq!(next_lookup(&mut opened).await.0, Some(PORT));
        let _ = std::fs::remove_dir_all(root);
    }

    /// A private torrent never reaches the DHT, announce or search (BEP 27).
    #[tokio::test]
    async fn a_private_torrent_is_never_announced() {
        let (mgr, root) = manager("private");
        let (ih, _) = mgr
            .add_torrent_bytes(&torrent_bytes("priv", true), "/tmp", false, false)
            .unwrap();
        let session = offline_session(true, Some(PORT)).await;
        session.track_torrent(mgr.get(&ih).unwrap());
        assert_eq!(session.tracked_count(), 0);
        let _ = std::fs::remove_dir_all(root);
    }

    /// ⭐ `dht_follow_seeds = false`: a seed does not search, but is announced
    /// once per `ANNOUNCE_INTERVAL` by a lookup that lasts `ANNOUNCE_WINDOW`
    /// and dials nothing it finds.
    #[tokio::test(start_paused = true)]
    async fn a_seed_not_followed_is_announced_every_interval_and_dials_nothing() {
        let (mgr, root) = manager("seed-announce");
        let (ih, _) = mgr
            .add_torrent_bytes(&torrent_bytes("seed", false), "/tmp", false, true)
            .unwrap();
        let session = offline_session(false, Some(PORT)).await;
        let (lookup, mut opened) = fake_lookups();
        session.track_with(mgr.get(&ih).unwrap(), lookup);

        let started = tokio::time::Instant::now();
        let (port, peers) = next_lookup(&mut opened).await;
        assert_eq!(port, Some(PORT));
        peers.send("192.168.99.7:6881".parse().unwrap()).unwrap();
        tokio::time::sleep(ANNOUNCE_WINDOW + Duration::from_secs(1)).await;
        assert!(peers.is_closed(), "the announce lookup outlived its window");
        assert_eq!(
            (session.peers_discovered(), session.peers_dialed()),
            (0, 0),
            "an announce-only lookup dialed what it found"
        );
        assert_eq!(
            (session.tracked_count(), session.torrents_tracked()),
            (1, 1),
            "the seed left the DHT"
        );

        let (port, _peers) = next_lookup_after(&mut opened, ANNOUNCE_INTERVAL).await;
        assert_eq!(port, Some(PORT));
        assert!(started.elapsed() >= ANNOUNCE_INTERVAL);
        let _ = std::fs::remove_dir_all(root);
    }

    /// No port known yet (a forwarded port still pending): a seed that is not
    /// followed opens no lookup at all, rather than one that announces
    /// nothing.
    #[tokio::test(start_paused = true)]
    async fn a_seed_not_followed_is_not_looked_up_while_the_port_is_unknown() {
        let (mgr, root) = manager("seed-noport");
        let (ih, _) = mgr
            .add_torrent_bytes(&torrent_bytes("seed", false), "/tmp", false, true)
            .unwrap();
        let session = offline_session(false, None).await;
        let (lookup, mut opened) = fake_lookups();
        session.track_with(mgr.get(&ih).unwrap(), lookup);
        let none = tokio::time::timeout(ANNOUNCE_INTERVAL * 2, opened.recv()).await;
        assert!(
            none.is_err(),
            "a lookup was opened with no port to announce"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// `dht_follow_seeds = false`: a download that finishes drops its search
    /// at the next peer found and is announced instead, still tracked. Once it
    /// needs pieces again, `track_torrent` (a start) wakes it into a search at
    /// once rather than at its next announce.
    #[tokio::test(start_paused = true)]
    async fn a_finished_download_stops_searching_and_searches_again_when_started() {
        let (mgr, root) = manager("finish");
        let (ih, _) = mgr
            .add_torrent_bytes(&torrent_bytes("fin", false), "/tmp", false, false)
            .unwrap();
        let torrent = mgr.get(&ih).unwrap();
        let session = offline_session(false, Some(PORT)).await;
        let (lookup, mut opened) = fake_lookups();
        session.track_with(torrent.clone(), lookup);
        let (_, search) = next_lookup(&mut opened).await;

        let picker = || torrent.picker.get().expect("picker").lock().unwrap();
        picker().set_have(0);
        assert!(!searches_in_dht(&torrent, false));
        search.send("192.168.99.7:6881".parse().unwrap()).unwrap();
        let (port, announce) = next_lookup(&mut opened).await;
        assert_eq!(port, Some(PORT), "the finished torrent is not announced");
        assert!(search.is_closed(), "the search outlived the download");
        assert_eq!(
            (session.tracked_count(), session.torrents_tracked()),
            (1, 1)
        );
        tokio::time::sleep(ANNOUNCE_WINDOW + Duration::from_secs(1)).await;
        assert!(announce.is_closed());

        // Needs a piece again; started, it searches now.
        picker().clear_have(0);
        let woken = tokio::time::Instant::now();
        session.track_torrent(torrent.clone());
        let (port, search) = next_lookup(&mut opened).await;
        assert_eq!(port, Some(PORT));
        assert!(
            woken.elapsed() < ANNOUNCE_INTERVAL,
            "waited for the next announce"
        );
        tokio::time::sleep(ANNOUNCE_WINDOW * 2).await;
        assert!(
            !search.is_closed(),
            "a search is not an announce: it stays open"
        );
        assert_eq!(session.torrents_tracked(), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    /// A search stream that ends takes its entry with it, so a later
    /// `track_torrent` tracks the torrent afresh and the count stays true.
    #[tokio::test]
    async fn a_search_that_ends_is_forgotten() {
        let (mgr, root) = manager("forget");
        let (ih, _) = mgr
            .add_torrent_bytes(&torrent_bytes("fin", false), "/tmp", false, false)
            .unwrap();
        let session = offline_session(true, Some(PORT)).await;
        let (lookup, mut opened) = fake_lookups();
        session.track_with(mgr.get(&ih).unwrap(), lookup);
        drop(next_lookup(&mut opened).await);
        for _ in 0..100 {
            if session.tracked_count() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            (session.tracked_count(), session.torrents_tracked()),
            (0, 0),
            "the ended search left its entry behind"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    async fn next_lookup_after(
        opened: &mut tokio::sync::mpsc::UnboundedReceiver<Lookup>,
        wait: Duration,
    ) -> Lookup {
        tokio::time::timeout(wait * 2, opened.recv())
            .await
            .expect("no lookup was opened")
            .expect("the task is gone")
    }
}
