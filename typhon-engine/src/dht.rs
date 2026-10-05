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
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;
use futures::StreamExt;
use librqbit_dht::{Dht, DhtBuilder, Id20};
use tokio::task::AbortHandle;
use tracing::{info, warn};

use crate::torrent::meta::{InfoHash, TorrentState};

/// Whether an engine may run a DHT node, and why not when it may not.
///
/// Decided in one place for the two discovery mechanisms that speak plain UDP
/// to strangers -- the DHT, and Local Service Discovery once there is one --
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

/// The Local Service Discovery (BEP 14) decision, for when LSD exists.
///
/// There is no LSD in this engine yet. This is the hook it must go through
/// when it arrives: multicast on the LAN, never through a proxy, so it is off
/// behind one for the same reason as the DHT. Kept separate from
/// `dht_policy` so an LSD switch of its own slots in without touching the DHT.
pub fn lsd_policy(config: &crate::config::EngineConfig) -> Discovery {
    if !config.socks5_outbound_host.trim().is_empty() {
        Discovery::Off(OFF_BEHIND_PROXY)
    } else {
        Discovery::On
    }
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
                }))
            }
            Err(e) => {
                warn!("[dht] bootstrap failed: {:#}", e);
                None
            }
        }
    }

    /// Handle to the node. Magnet resolution needs peers for an info hash that
    /// has no TorrentState behind it yet.
    pub fn handle(&self) -> Dht {
        self.dht.clone()
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
        if !torrent.meta.allows_peer_discovery() {
            return;
        }
        let ih = torrent.info_hash;
        if self.tracked.contains_key(&ih) {
            return;
        }
        let info_hash = Id20::new(ih);
        let dht = self.dht.clone();
        let session = self.clone();
        let handle = tokio::spawn(async move {
            let mut stream = dht.get_peers(info_hash, None);
            while let Some(peer_addr) = stream.next().await {
                if torrent.is_removed.load(Ordering::Relaxed) {
                    break;
                }
                session.peers_discovered.fetch_add(1, Ordering::Relaxed);
                if torrent.connected_addrs.contains_key(&peer_addr) {
                    continue;
                }
                session.peers_dialed.fetch_add(1, Ordering::Relaxed);
                crate::tracker::enqueue_dial(peer_addr, torrent.clone());
            }
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
        let proxied = config(r#"{"socks5_outbound_host":"10.0.0.1","dht_enabled":true}"#);
        assert_eq!(dht_policy(&proxied), Discovery::Off(OFF_BEHIND_PROXY));
        assert_eq!(lsd_policy(&proxied), Discovery::Off(OFF_BEHIND_PROXY));

        let direct = config(r#"{"dht_enabled":true}"#);
        assert!(dht_policy(&direct).is_on());
        assert!(lsd_policy(&direct).is_on());
        let off = config(r#"{"dht_enabled":false}"#);
        assert_eq!(dht_policy(&off), Discovery::Off(OFF_BY_CONFIG));
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
}
