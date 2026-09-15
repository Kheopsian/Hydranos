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
    pub async fn start() -> Option<Arc<Self>> {
        match DhtBuilder::new().await {
            Ok(dht) => {
                info!("[dht] bootstrapped");
                Some(Arc::new(Self {
                    dht,
                    tracked: DashMap::new(),
                    torrents_tracked: AtomicU64::new(0),
                    peers_discovered: AtomicU64::new(0),
                    peers_dialed: AtomicU64::new(0),
                }))
            }
            Err(e) => {
                warn!("[dht] bootstrap failed: {}", e);
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
