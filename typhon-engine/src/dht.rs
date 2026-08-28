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
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;
use futures::StreamExt;
use librqbit_dht::{Dht, DhtBuilder, Id20};
use tokio::task::AbortHandle;
use tracing::{info, warn};

use crate::torrent::meta::{InfoHash, TorrentState};

pub static DHT_TORRENTS_TRACKED: AtomicU64 = AtomicU64::new(0);
pub static DHT_PEERS_DISCOVERED: AtomicU64 = AtomicU64::new(0);
pub static DHT_PEERS_DIALED: AtomicU64 = AtomicU64::new(0);

static DHT: OnceLock<Dht> = OnceLock::new();

/// Live `get_peers` tasks, keyed by info hash. One entry per tracked torrent.
static TRACKED: OnceLock<DashMap<InfoHash, AbortHandle>> = OnceLock::new();

fn tracked() -> &'static DashMap<InfoHash, AbortHandle> {
    TRACKED.get_or_init(DashMap::new)
}

/// Number of torrents currently streaming peers from the DHT.
pub fn tracked_count() -> usize {
    tracked().len()
}

/// Bootstrap the DHT node. Call once at startup.
pub async fn start() {
    match DhtBuilder::new().await {
        Ok(dht) => {
            info!("[dht] bootstrapped");
            if DHT.set(dht).is_err() {
                warn!("[dht] already initialized");
            }
        }
        Err(e) => {
            warn!("[dht] bootstrap failed: {}", e);
        }
    }
}

/// Handle to the running DHT node, if it bootstrapped. Magnet resolution needs
/// peers for an info hash that has no TorrentState behind it yet.
pub fn handle() -> Option<Dht> {
    DHT.get().cloned()
}

/// Register a torrent with the DHT. Skips `private` torrents (BEP 27).
/// Spawns a task that streams peers and enqueues them for dialing.
///
/// Idempotent: a torrent already tracked keeps its existing task rather than
/// gaining a second one. `start_torrent` calls this on every resume, and the
/// boot loop calls it for every loaded torrent.
pub fn track_torrent(torrent: Arc<TorrentState>) {
    if torrent.meta.private {
        return;
    }
    let Some(dht) = DHT.get() else { return };
    let ih = torrent.info_hash;
    if tracked().contains_key(&ih) {
        return;
    }
    let info_hash = Id20::new(ih);
    let dht = dht.clone();
    let handle = tokio::spawn(async move {
        let mut stream = dht.get_peers(info_hash, None);
        while let Some(peer_addr) = stream.next().await {
            if torrent.is_removed.load(Ordering::Relaxed) {
                break;
            }
            DHT_PEERS_DISCOVERED.fetch_add(1, Ordering::Relaxed);
            if torrent.connected_addrs.contains_key(&peer_addr) {
                continue;
            }
            DHT_PEERS_DIALED.fetch_add(1, Ordering::Relaxed);
            crate::tracker::enqueue_dial(peer_addr, torrent.clone());
        }
    });
    // Race: two concurrent track_torrent calls for the same hash both pass the
    // contains_key check. The loser's task is aborted so we never leak one.
    if let Some(previous) = tracked().insert(ih, handle.abort_handle()) {
        previous.abort();
    } else {
        DHT_TORRENTS_TRACKED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Cancel a torrent's `get_peers` task. Safe to call for a torrent that was
/// never tracked (private, or added before the DHT bootstrapped).
pub fn untrack_torrent(info_hash: &InfoHash) {
    if let Some((_, handle)) = tracked().remove(info_hash) {
        handle.abort();
        DHT_TORRENTS_TRACKED.fetch_sub(1, Ordering::Relaxed);
    }
}
