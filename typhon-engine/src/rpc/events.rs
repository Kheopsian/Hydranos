//! Push-based event bus for live stats streaming to clients.
//!
//! Replaces the 2-second polling loop Hydra-Go was doing via `list_torrents`
//! which serialized 13k torrent states each tick (~82 ms per call, ~4-8% CPU).
//! Now clients subscribe once and Typhon pushes:
//!   * `torrent_added` / `torrent_removed` — immediate on add/remove
//!   * `stats_snapshot` — batched every ~1 s, includes only torrents whose
//!     counters moved since last tick (delta-filtered).
//!
//! A `tokio::sync::broadcast::Sender` per engine, so hot-path sites
//! (add_torrent, peer uploads) can publish without holding a shared
//! async context.
//!
//! It used to be one global bus, which was right while one engine meant one
//! process. Events carry an info hash and no engine tag: merged, a subscriber
//! watching the race would receive the hoard's 240k torrents too, with no way
//! to tell them apart.
use tokio::sync::broadcast;
use serde::Serialize;

/// Capacity = 4096. Enough buffer for a slow consumer without OOM.
/// If a consumer lags > 4096 events, it gets `RecvError::Lagged(n)` and should
/// drop its cache + resubscribe (after requesting a full `list_torrents`
/// snapshot via the classic RPC).
const BUS_CAP: usize = 4096;

/// One engine's event stream.
pub struct EventBus {
    tx: broadcast::Sender<Event>,
}

impl Default for EventBus {
    fn default() -> Self {
        let (tx, _) = broadcast::channel(BUS_CAP);
        Self { tx }
    }
}

impl EventBus {
    /// Publish. Best-effort: dropped if nobody listens or the buffer is full.
    pub fn publish(&self, ev: Event) {
        let _ = self.tx.send(ev);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

/// Wire format matches Hydra-Go's existing `ltclient.Event` struct:
///   `{"event": "torrent_added", "data": {...}}`
/// Client detects events by the presence of the `event` field at top level.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum Event {
    TorrentAdded {
        info_hash: String,
        name: String,
        save_path: String,
        total_size: u64,
        num_pieces: u32,
        private: bool,
        seed_mode: bool,
    },
    TorrentRemoved {
        info_hash: String,
    },
    StatsSnapshot {
        /// Only torrents with delta since last snapshot tick.
        torrents: Vec<TorrentStatsMini>,
    },
}

#[derive(Clone, Debug, Serialize)]
pub struct TorrentStatsMini {
    pub info_hash: String,
    pub status: u8,              // TorrentStatus as u8
    pub total_uploaded: u64,
    pub total_downloaded: u64,
    pub upload_rate: u64,
    pub download_rate: u64,
    pub peers_connected: u32,
    pub peers_interested: u32,
    pub progress: f32,
}
