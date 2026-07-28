//! BEP 5 DHT integration via `librqbit-dht`.
//!
//! We bootstrap a DHT node at startup and, for every non-`private` torrent,
//! spawn a task that streams peers via `get_peers()` and funnels them into
//! the existing dial queue (`crate::tracker::enqueue_dial`).
//!
//! Private-tracker torrents (`TorrentMeta.private == true`) are skipped — BEP 27
//! forbids DHT for those and many trackers ban clients that announce them.

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::StreamExt;
use librqbit_dht::{Dht, DhtBuilder, Id20};
use tracing::{info, warn};

use crate::torrent::meta::TorrentState;

pub static DHT_TORRENTS_TRACKED: AtomicU64 = AtomicU64::new(0);
pub static DHT_PEERS_DISCOVERED: AtomicU64 = AtomicU64::new(0);
pub static DHT_PEERS_DIALED: AtomicU64 = AtomicU64::new(0);

static DHT: OnceLock<Dht> = OnceLock::new();

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

/// Register a torrent with the DHT. Skips `private` torrents (BEP 27).
/// Spawns a task that streams peers and enqueues them for dialing.
pub fn track_torrent(torrent: Arc<TorrentState>) {
    if torrent.meta.private {
        return;
    }
    let Some(dht) = DHT.get() else { return };
    let info_hash = Id20::new(torrent.info_hash);
    let dht = dht.clone();
    DHT_TORRENTS_TRACKED.fetch_add(1, Ordering::Relaxed);
    tokio::spawn(async move {
        let mut stream = dht.get_peers(info_hash, None);
        while let Some(peer_addr) = stream.next().await {
            if torrent.is_removed.load(Ordering::Relaxed) {
                break;
            }
            DHT_PEERS_DISCOVERED.fetch_add(1, Ordering::Relaxed);
            if torrent.connected_addrs.contains(&peer_addr) {
                continue;
            }
            DHT_PEERS_DIALED.fetch_add(1, Ordering::Relaxed);
            crate::tracker::enqueue_dial(peer_addr, torrent.clone());
        }
    });
}
