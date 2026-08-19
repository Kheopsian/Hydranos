use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use parking_lot::RwLock;
use bytes::Bytes;
use tokio::sync::broadcast;
use dashmap::{DashMap, DashSet};

use super::piece_picker::PiecePicker;
use super::rate::RateTracker;

pub type InfoHash = [u8; 20];

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TorrentStatus {
    Stopped = 0,
    Checking = 1,
    Downloading = 2,
    Seeding = 3,
    /// The data is gone: a read hit ENOENT, so we cannot honour a single
    /// request on this torrent. Set once, by the serve path, and never
    /// cleared on its own — nothing retries a torrent it refuses to serve.
    /// A recheck is what brings it back, same as qBittorrent's missing-files.
    Error = 4,
}

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: PathBuf,
    pub offset: u64,
    pub length: u64,
}

#[derive(Debug, Clone)]
pub struct TorrentMeta {
    pub info_hash: InfoHash,
    pub name: String,
    pub pieces: Vec<[u8; 20]>,
    pub piece_length: u32,
    pub total_size: u64,
    pub files: Vec<FileEntry>,
    pub trackers: Vec<Vec<String>>,
    pub private: bool,
    /// True when the torrent's info dict has a `files` key (even with a
    /// single entry). In that case, the on-disk path is `name/<file path>`.
    /// False for old-style single-file torrents (just the `length` key);
    /// there the on-disk path is simply `name` at save_path root.
    pub multi_file: bool,
    /// Size in bytes of the raw info dict this torrent was parsed from.
    /// Advertised as BEP 9 `metadata_size` so peers know they can fetch the
    /// dict from us. Only the length is kept: the bytes themselves are re-read
    /// from the .torrent on demand, because holding them for every torrent
    /// would cost far more RAM than serving them is worth.
    pub info_dict_len: u32,
}

impl TorrentMeta {
    pub fn num_pieces(&self) -> u32 {
        self.pieces.len() as u32
    }

    pub fn piece_size(&self, index: u32) -> u32 {
        let start = index as u64 * self.piece_length as u64;
        let remaining = self.total_size.saturating_sub(start);
        remaining.min(self.piece_length as u64) as u32
    }

    pub fn map_block(&self, piece: u32, offset: u32, length: u32) -> Vec<FileOp> {
        let abs_offset = piece as u64 * self.piece_length as u64 + offset as u64;
        let mut remaining = length as u64;
        let mut pos = abs_offset;
        let mut ops = Vec::new();

        for f in &self.files {
            if pos >= f.offset + f.length || remaining == 0 {
                continue;
            }
            if pos < f.offset {
                pos = f.offset;
            }
            let file_start = pos - f.offset;
            let available = f.length - file_start;
            let to_read = remaining.min(available);
            ops.push(FileOp {
                path: f.path.clone(),
                file_offset: file_start,
                length: to_read as u32,
            });
            pos += to_read;
            remaining -= to_read;
            if remaining == 0 { break; }
        }
        ops
    }
}

#[derive(Debug)]
pub struct FileOp {
    pub path: PathBuf,
    pub file_offset: u64,
    pub length: u32,
}

/// Per-peer stats. Each peer task holds an Arc<PeerStats> directly
/// (no lookup needed) and updates atomics. get_peers iterates all stats
/// on-demand to build a snapshot.
pub struct PeerStats {
    pub addr: std::net::SocketAddr,
    pub peer_id: [u8; 20],
    pub client: String,
    pub connected_at: std::time::SystemTime,
    pub is_encrypted: bool,
    pub fast_ext: bool,
    // Hot-path atomics — written by the owning peer task only
    pub total_uploaded: AtomicU64,
    pub total_downloaded: AtomicU64,
    pub num_pieces_have: AtomicU32,
    pub is_seed: AtomicBool,
    pub interested: AtomicBool,
    /// `true` = we choke this peer (am_choking). Initial BT default: true.
    /// Set by the choking engine tick; peer loop observes via `choking_gen` bumps.
    pub choked: AtomicBool,
    /// Monotonic counter bumped by the choking engine each time `choked` flips.
    /// Peer loop tracks a local copy and emits a Choke/Unchoke message whenever
    /// the engine's value overtakes its local copy.
    pub choking_gen: AtomicU32,
    /// Bytes uploaded to this peer since the choking engine last tick.
    /// Used to score peers by actual upload speed; reset to 0 by the engine.
    pub uploaded_last_tick: AtomicU64,
    /// Transfer rates for this peer. Nothing ticks these in the background:
    /// at a hundred thousand torrents a periodic sweep over every connected
    /// peer would cost O(all peers) forever to feed a panel that is almost
    /// never open. `get_peers` samples them instead, so the rate is a delta
    /// over whatever interval the caller polls at.
    pub dl_rate: RateTracker,
    pub ul_rate: RateTracker,
}

impl PeerStats {
    pub fn new(
        addr: std::net::SocketAddr,
        peer_id: [u8; 20],
        client: String,
        is_encrypted: bool,
        fast_ext: bool,
    ) -> Self {
        Self {
            addr,
            peer_id,
            client,
            connected_at: std::time::SystemTime::now(),
            is_encrypted,
            fast_ext,
            total_uploaded: AtomicU64::new(0),
            total_downloaded: AtomicU64::new(0),
            num_pieces_have: AtomicU32::new(0),
            is_seed: AtomicBool::new(false),
            interested: AtomicBool::new(false),
            choked: AtomicBool::new(true),
            choking_gen: AtomicU32::new(0),
            uploaded_last_tick: AtomicU64::new(0),
            dl_rate: RateTracker::new(),
            ul_rate: RateTracker::new(),
        }
    }
}

/// RAII guard: ensures peer is removed from registry and counter
/// decremented even on panic/early return.
pub struct PeerGuard {
    torrent: Arc<TorrentState>,
    addr: std::net::SocketAddr,
    was_interested: std::sync::atomic::AtomicBool,
}

impl PeerGuard {
    pub fn new(torrent: Arc<TorrentState>, stats: Arc<PeerStats>) -> Self {
        let addr = stats.addr;
        torrent.peer_stats.insert(addr, stats);
        torrent.peers_connected.fetch_add(1, Ordering::Relaxed);
        // Process-wide live connection count backing `max_connections`.
        // Maintained here rather than at the dial site so inbound sessions are
        // counted too, and so the decrement is RAII-guaranteed below.
        crate::tracker::dial_limiter::LIVE_CONNS.fetch_add(1, Ordering::Relaxed);
        // Connected-addr dedup: tracker/DHT both check `connected_addrs`
        // before dialing a peer to avoid spawning N parallel sockets to
        // the same address. This insert is the missing half — without it
        // the contains() check in tracker/mod.rs and dht.rs is always
        // false and every announce cycle re-dials known peers, fragmenting
        // throughput across redundant connections.
        torrent.connected_addrs.insert(addr);
        Self {
            torrent,
            addr,
            was_interested: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn mark_interested(&self, interested: bool) {
        let prev = self.was_interested.swap(interested, Ordering::Relaxed);
        if interested && !prev {
            self.torrent.peers_interested.fetch_add(1, Ordering::Relaxed);
        } else if !interested && prev {
            self.torrent.peers_interested.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

impl Drop for PeerGuard {
    fn drop(&mut self) {
        self.torrent.peer_stats.remove(&self.addr);
        self.torrent.peers_connected.fetch_sub(1, Ordering::Relaxed);
        crate::tracker::dial_limiter::LIVE_CONNS.fetch_sub(1, Ordering::Relaxed);
        self.torrent.connected_addrs.remove(&self.addr);
        if self.was_interested.load(Ordering::Relaxed) {
            self.torrent.peers_interested.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// Runtime state for an active torrent.
pub struct TorrentState {
    /// Parsed straight from the .torrent. `meta.trackers` is the SEED for
    /// `live_trackers` and nothing else reads it: the operator can edit the
    /// tracker list at runtime, so what the file said and what we announce
    /// to are two different facts.
    pub meta: TorrentMeta,
    /// The tracker list actually announced to, in tiers. Edited in place by
    /// the set_trackers command; the announce loop takes a snapshot each
    /// pass, so a change lands on the next announce without a restart.
    pub live_trackers: RwLock<Vec<Vec<String>>>,
    pub save_path: RwLock<PathBuf>,
    pub info_hash: InfoHash,
    pub torrent_file_path: String,
    pub added_time: i64,
    pub completed_time: AtomicI64,
    pub seed_mode: bool,

    pub status: AtomicU8,
    pub total_uploaded: AtomicU64,
    pub total_downloaded: AtomicU64,
    pub peers_connected: AtomicUsize,
    pub peers_interested: AtomicUsize,
    pub is_paused: AtomicBool,
    /// Anti-thrash: when true, this torrent serves no piece Requests
    /// (disk reads gated in peer::session), but stays connected and
    /// announced. Set by the per-disk seed-slot manager; cleared to resume.
    pub serving_suspended: AtomicBool,
    /// Set by TorrentManager::remove_torrent BEFORE the DashMap entry is
    /// dropped. Peer tasks keep an Arc<TorrentState> alive after the map
    /// entry disappears; they must observe this flag and exit their loop.
    /// Without it, zombie peer tasks keep servicing Piece messages and
    /// write_piece (create=true) re-creates the files we just deleted.
    pub is_removed: AtomicBool,

    // Download mode
    pub picker: OnceLock<Arc<Mutex<PiecePicker>>>,
    pub have_tx: Option<broadcast::Sender<u32>>,

    // Per-torrent rate tracking
    pub upload_rate: RateTracker,
    pub download_rate: RateTracker,

    // Tracker scrape data
    pub scrape_seeders: AtomicU32,
    pub scrape_leechers: AtomicU32,
    pub current_tracker: Mutex<String>,
    pub last_announce_ok: AtomicBool,
    pub last_announce_error: Mutex<String>,
    /// Unix seconds of the last SUCCESSFUL announce, 0 = never. Failures
    /// deliberately do not move it: "last announce 3h ago" next to an error
    /// is the useful reading, while stamping every failed attempt would show
    /// a fresh time for a tracker we have not reached in hours.
    pub last_announce_at: AtomicI64,
    /// Unix seconds when the next announce is due, 0 = unknown. The UI used
    /// to be handed a hardcoded 0 here, which it renders as "now", forever.
    pub next_announce_at: AtomicI64,

    /// Why this torrent is in `TorrentStatus::Error`, shown when the user
    /// opens it. Empty for every other status.
    pub error_msg: Mutex<String>,

    // Peer registry: insert at connect, remove at disconnect (via PeerGuard).
    // Each peer task holds its own Arc<PeerStats>, so hot-path updates do
    // NOT need lookups — zero contention during piece transfers.
    // This is only accessed during get_peers (user opens peer panel = rare).
    pub peer_stats: DashMap<std::net::SocketAddr, Arc<PeerStats>>,

    /// Addresses of peers currently connected on this torrent. Maintained by
    /// peer tasks (insert at handshake success, remove on disconnect). Used by
    /// PEX (BEP 11) to compute added/dropped between ticks and by the dial
    /// dedup path ("don't redial a peer we already serve").
    pub connected_addrs: DashSet<std::net::SocketAddr>,
}

impl TorrentState {
    pub fn new(meta: TorrentMeta, save_path: PathBuf, torrent_file_path: String, seed_mode: bool) -> Self {
        Self::new_with_times(meta, save_path, torrent_file_path, seed_mode, None, None)
    }

    /// Create a TorrentState, optionally restoring added_time / completed_time
    /// from fastresume. `None` falls back to `SystemTime::now()` (fresh add).
    pub fn new_with_times(
        meta: TorrentMeta,
        save_path: PathBuf,
        torrent_file_path: String,
        seed_mode: bool,
        added_time_override: Option<i64>,
        completed_time_override: Option<i64>,
    ) -> Self {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let added = added_time_override.filter(|&t| t > 0).unwrap_or(now_secs);
        let completed = completed_time_override
            .filter(|&t| t > 0)
            .unwrap_or(if seed_mode { now_secs } else { 0 });
        let ih = meta.info_hash;
        let status = if seed_mode {
            TorrentStatus::Seeding as u8
        } else {
            TorrentStatus::Stopped as u8
        };
        let picker: OnceLock<Arc<Mutex<PiecePicker>>> = OnceLock::new();
        if !seed_mode {
            let _ = picker.set(Arc::new(Mutex::new(PiecePicker::new(meta.num_pieces()))));
        }
        // Only downloaders use the have-broadcast; seeders never send/subscribe,
        // so skip the 256-slot ring allocation for every seeder.
        let have_tx = if seed_mode { None } else { Some(broadcast::channel(256).0) };
        Self {
            live_trackers: RwLock::new(meta.trackers.clone()),
            meta,
            save_path: RwLock::new(save_path),
            info_hash: ih,
            torrent_file_path,
            added_time: added,
            completed_time: AtomicI64::new(completed),
            seed_mode,
            status: AtomicU8::new(status),
            total_uploaded: AtomicU64::new(0),
            total_downloaded: AtomicU64::new(0),
            peers_connected: AtomicUsize::new(0),
            peers_interested: AtomicUsize::new(0),
            is_paused: AtomicBool::new(false),
            serving_suspended: AtomicBool::new(false),
            is_removed: AtomicBool::new(false),
            picker,
            have_tx,
            upload_rate: RateTracker::new(),
            download_rate: RateTracker::new(),
            scrape_seeders: AtomicU32::new(0),
            scrape_leechers: AtomicU32::new(0),
            current_tracker: Mutex::new(String::new()),
            last_announce_ok: AtomicBool::new(false),
            last_announce_error: Mutex::new(String::new()),
            last_announce_at: AtomicI64::new(0),
            next_announce_at: AtomicI64::new(0),
            error_msg: Mutex::new(String::new()),
            peer_stats: DashMap::new(),
            connected_addrs: DashSet::new(),
        }
    }

    /// The data is missing: park the torrent instead of promising pieces we
    /// cannot deliver. Suspending the serve path is what stops the bleeding —
    /// every request was allocating a formatted error only to reject the peer.
    /// Idempotent: the first caller wins, later ones are cheap loads.
    pub fn mark_error(&self, msg: &str) {
        if self.status.load(Ordering::Relaxed) == TorrentStatus::Error as u8 {
            return;
        }
        if let Ok(mut g) = self.error_msg.lock() {
            *g = msg.to_string();
        }
        self.status.store(TorrentStatus::Error as u8, Ordering::Relaxed);
        self.serving_suspended.store(true, Ordering::Relaxed);
        tracing::warn!("torrent {:?} parked in error: {}", self.meta.name, msg);
    }

    pub fn have_bitfield(&self) -> Bytes {
        let n = self.meta.num_pieces() as usize;
        let byte_len = (n + 7) / 8;
        if self.status.load(Ordering::Relaxed) == TorrentStatus::Seeding as u8 {
            let mut buf = vec![0xFFu8; byte_len];
            let trailing = n % 8;
            if trailing > 0 {
                buf[byte_len - 1] = 0xFF << (8 - trailing);
            }
            Bytes::from(buf)
        } else if let Some(p) = self.picker.get() {
            // Advertise partial progress during leeching — otherwise peers never
            // request from us and our upload stays at 0 until we fully complete.
            Bytes::from(p.lock().unwrap().export_bitfield())
        } else {
            Bytes::from(vec![0u8; byte_len])
        }
    }
}
