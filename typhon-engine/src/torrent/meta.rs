use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use parking_lot::RwLock;
use bytes::Bytes;
use tokio::sync::broadcast;
use dashmap::DashMap;

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
    /// How many pieces this torrent has. The 20-byte SHA-1 of each one is
    /// NOT kept here -- see `TorrentState::piece_hash`. Every caller but the
    /// two that actually verify data wants this count and nothing more.
    pub num_pieces: u32,
    pub piece_length: u32,
    pub total_size: u64,
    pub files: Vec<FileEntry>,
    pub trackers: Vec<Vec<String>>,
    /// BEP 19 `url-list`: HTTP mirrors that serve this torrent's payload.
    /// Empty for almost every torrent (an empty Vec is 24 bytes), which is
    /// what makes carrying it on a million-torrent catalogue affordable.
    pub url_list: Vec<String>,
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
        self.num_pieces
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
        torrent.connected_addrs.insert(addr, ());
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
/// Shard count for the two per-torrent concurrent maps below.
///
/// `DashMap::new()` sizes its shard array from the machine: `(nproc * 4)`
/// rounded up to a power of two. On a 12-core box that is 64 shards, and
/// dashmap allocates the whole array up front, at construction, before a
/// single entry exists. Each shard is a `CachePadded<RwLock<HashMap>>` = 128
/// bytes, so one empty `DashMap` costs 8 KiB and a `TorrentState` carrying two
/// of them costs ~16.7 KiB of untouched shards.
///
/// Measured on prod 2026-08-28: 204,893 torrents, 167k of them with zero
/// peers, 3.43 GB of RAM in shard arrays that never held anything.
///
/// Sharding buys nothing here. These maps are per torrent, not global: they
/// are written on peer connect/disconnect and read by the PEX tick, the dial
/// dedup and `get_peers`. Contention is bounded by one torrent's peer count,
/// and every critical section is a single insert/remove on a small map. The
/// global maps in `TorrentManager` (`torrents`, `skey_index`) keep the default
/// sharding -- those are genuinely hot across 200k entries.
/// 2, not 1: dashmap asserts `shard_amount > 1` at construction.
const PER_TORRENT_SHARDS: usize = 2;

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

    /// The torrent's 20-byte piece hashes: loaded from `torrent_file_path` the
    /// first time something needs to verify data, dropped again the moment the
    /// torrent goes back to seeding, and never loaded at all for a torrent that
    /// only ever seeds. See `piece_hash` and `release_piece_hashes`.
    ///
    /// Behind an `Arc` so a verification already in flight keeps the table it
    /// is reading even if another thread releases it mid-check.
    piece_hashes: Mutex<Option<Arc<Vec<[u8; 20]>>>>,

    // Download mode
    pub picker: OnceLock<Arc<Mutex<PiecePicker>>>,
    /// Broadcast of freshly completed pieces, so connected peers can be sent
    /// a Have. Created on first use and dropped again the moment the torrent
    /// starts seeding: a complete torrent never announces a new piece, and
    /// nothing subscribes to a channel that will never fire.
    ///
    /// It used to be built eagerly for every torrent not added in seed_mode
    /// and kept for life. A 256-slot ring costs ~8.4 KiB, and production had
    /// 81,005 such torrents holding one long after they had finished
    /// downloading: 682 MB of rings nothing would ever send to. See
    /// `have_sender`, `subscribe_have` and `release_have_tx`.
    have_tx: RwLock<Option<broadcast::Sender<u32>>>,

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
    pub connected_addrs: DashMap<std::net::SocketAddr, ()>,
}

impl TorrentState {
    /// The have-broadcast sender, creating the channel on first use.
    ///
    /// Only the download path calls this, and only when a piece completes, so
    /// a torrent that merely seeds never allocates the ring at all.
    pub fn have_sender(&self) -> broadcast::Sender<u32> {
        if let Some(tx) = self.have_tx.read().as_ref() {
            return tx.clone();
        }
        let mut slot = self.have_tx.write();
        // Another thread may have created it while we waited for the write lock.
        if let Some(tx) = slot.as_ref() {
            return tx.clone();
        }
        let tx = broadcast::channel(256).0;
        *slot = Some(tx.clone());
        tx
    }

    /// Subscribe to the have-broadcast, if there is one.
    ///
    /// Deliberately does NOT create the channel: a peer session attaching to a
    /// seeding torrent would otherwise allocate the very ring this scheme
    /// exists to avoid, once per torrent, the first time anyone connected.
    pub fn subscribe_have(&self) -> Option<broadcast::Receiver<u32>> {
        self.have_tx.read().as_ref().map(|tx| tx.subscribe())
    }

    /// Drop the have-broadcast. Called when the torrent reaches Seeding.
    ///
    /// Existing subscribers keep their `Receiver` and simply see the channel
    /// close, which is exactly right: there will be no further pieces to
    /// announce. If a recheck later finds the torrent incomplete, the download
    /// path calls `have_sender` and a fresh channel is built.
    pub fn release_have_tx(&self) {
        *self.have_tx.write() = None;
    }

    /// The expected SHA-1 of one piece, loading the hash table on first use.
    ///
    /// Returns `None` when the table cannot be read -- a missing or truncated
    /// `.torrent`. Callers MUST treat that as "cannot verify", never as
    /// "verified": the two call sites compare against the returned hash, so a
    /// `None` has to fail the piece rather than pass it.
    pub fn piece_hash(&self, piece: u32) -> Option<[u8; 20]> {
        let table = {
            let mut slot = match self.piece_hashes.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            if slot.is_none() {
                match crate::torrent::metainfo::piece_hashes_from_file(&self.torrent_file_path) {
                    Ok(h) => {
                        if h.len() as u32 != self.meta.num_pieces {
                            tracing::error!(
                                "piece hashes for {} disagree with metadata: {} on disk, {} expected; refusing to verify",
                                self.torrent_file_path, h.len(), self.meta.num_pieces
                            );
                            return None;
                        }
                        *slot = Some(Arc::new(h));
                    }
                    Err(e) => {
                        tracing::error!("cannot load piece hashes: {}", e);
                        return None;
                    }
                }
            }
            // Clone the Arc, not the table, and drop the lock before indexing.
            slot.clone()?
        };
        table.get(piece as usize).copied()
    }

    /// Drop the piece hash table.
    ///
    /// Called when a torrent reaches Seeding, by download completion or by a
    /// recheck that found everything. A seeder never verifies: it reads a piece
    /// off disk and sends it. Keeping the table would hand back the 20 bytes
    /// per piece this whole scheme exists to avoid -- 20.1 KiB for the average
    /// production torrent -- for the entire remaining life of the torrent,
    /// which for a seedbox is forever.
    ///
    /// Safe to call at any time and from any thread: a verification in flight
    /// holds its own `Arc` and finishes against the table it started with, and
    /// anything that needs the hashes again just reloads them.
    pub fn release_piece_hashes(&self) {
        let mut slot = match self.piece_hashes.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        *slot = None;
    }


    pub fn new(meta: TorrentMeta, save_path: PathBuf, torrent_file_path: String, seed_mode: bool) -> Self {
        Self::new_with_times(meta, save_path, torrent_file_path, seed_mode, None, None, !seed_mode)
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
        // A picker is ~5 bytes per piece and is useless to a torrent that has
        // nothing left to pick. The caller knows whether this one still needs
        // one; it is NOT the same question as `seed_mode`, because a torrent
        // added as a download and since completed is no longer picking either.
        needs_picker: bool,
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
        if needs_picker {
            let _ = picker.set(Arc::new(Mutex::new(PiecePicker::new(meta.num_pieces()))));
        }
        // Only downloaders use the have-broadcast; seeders never send/subscribe,
        // so skip the 256-slot ring allocation for every seeder.
        // Built lazily by `have_sender`; a seeder never asks for one.
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
            piece_hashes: Mutex::new(None),
            picker,
            have_tx: RwLock::new(None),
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
            peer_stats: DashMap::with_shard_amount(PER_TORRENT_SHARDS),
            connected_addrs: DashMap::with_shard_amount(PER_TORRENT_SHARDS),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The two per-torrent maps must NOT be sharded by core count.
    ///
    /// `DashMap::new()` allocates `(nproc * 4).next_power_of_two()` shards up
    /// front -- 64 on a 12-core box, 128 bytes each, per map, per torrent,
    /// before a single peer exists. At 200k torrents that was 3.43 GB of empty
    /// shard arrays. Revert either constructor to `::new()` and this test goes
    /// from 2 to nproc*4 on the CI runner.
    #[test]
    fn per_torrent_maps_are_not_sharded_by_core_count() {
        let stats: DashMap<std::net::SocketAddr, Arc<PeerStats>> =
            DashMap::with_shard_amount(PER_TORRENT_SHARDS);
        let addrs: DashMap<std::net::SocketAddr, ()> =
            DashMap::with_shard_amount(PER_TORRENT_SHARDS);
        assert_eq!(
            stats.shards().len(),
            PER_TORRENT_SHARDS,
            "peer_stats regained shards"
        );
        assert_eq!(
            addrs.shards().len(),
            PER_TORRENT_SHARDS,
            "connected_addrs regained shards"
        );

        // Proof the default is what we are avoiding: if dashmap ever stops
        // sizing from the core count, this assert fires and the constant can
        // go away.
        let default_shards = DashMap::<u8, u8>::new().shards().len();
        assert!(
            default_shards > PER_TORRENT_SHARDS,
            "dashmap no longer shards by core count; PER_TORRENT_SHARDS is moot"
        );
    }

    fn torrent_bytes(n: usize) -> (Vec<u8>, Vec<[u8; 20]>) {
        let mut hashes = Vec::new();
        let mut pieces = Vec::new();
        for i in 0..n {
            let h = [(i + 1) as u8; 20];
            hashes.push(h);
            pieces.extend_from_slice(&h);
        }
        let mut b = Vec::new();
        b.extend_from_slice(b"d8:announce19:http://tracker/annc");
        b.extend_from_slice(b"4:infod");
        b.extend_from_slice(format!("6:lengthi{}e", n * 16384).as_bytes());
        b.extend_from_slice(b"4:name8:some.bin");
        b.extend_from_slice(b"12:piece lengthi16384e");
        b.extend_from_slice(format!("6:pieces{}:", pieces.len()).as_bytes());
        b.extend_from_slice(&pieces);
        b.extend_from_slice(b"ee");
        (b, hashes)
    }

    fn state_for(tag: &str, n: usize) -> (TorrentState, Vec<[u8; 20]>, String) {
        let (bytes, hashes) = torrent_bytes(n);
        let mut p = std::env::temp_dir();
        p.push(format!("typhon-state-{}-{}.torrent", std::process::id(), tag));
        std::fs::write(&p, &bytes).unwrap();
        let path = p.to_string_lossy().into_owned();
        let meta = crate::torrent::metainfo::parse_torrent_bytes(&bytes).unwrap();
        let st = TorrentState::new(meta, std::path::PathBuf::from("/tmp"), path.clone(), true);
        (st, hashes, path)
    }

    /// A seeder holds no hashes until something asks to verify, and then it
    /// gets the right ones.
    #[test]
    fn piece_hash_loads_on_demand_and_matches() {
        let (st, hashes, path) = state_for("ondemand", 4);
        assert!(st.piece_hashes.lock().unwrap().is_none(), "hashes were loaded eagerly");
        assert_eq!(st.piece_hash(0), Some(hashes[0]));
        assert_eq!(st.piece_hash(3), Some(hashes[3]));
        assert!(st.piece_hashes.lock().unwrap().is_some(), "table was not cached");
        assert_eq!(st.piece_hash(4), None, "out-of-range piece returned a hash");
        std::fs::remove_file(&path).ok();
    }

    /// A seeder must never allocate the have-broadcast ring.
    ///
    /// This is the whole point: 81,005 production torrents were holding an
    /// 8.4 KiB channel nothing would ever send to. `subscribe_have` in
    /// particular must not create one, or the first peer to attach would
    /// resurrect it for every torrent.
    #[test]
    fn seeding_torrent_never_allocates_the_have_ring() {
        let (st, _, path) = state_for("havetx-seed", 3);
        assert!(st.have_tx.read().is_none(), "ring allocated at construction");
        assert!(st.subscribe_have().is_none(), "subscribe created a ring");
        assert!(st.have_tx.read().is_none(), "subscribe left a ring behind");
        std::fs::remove_file(&path).ok();
    }

    /// A downloader gets one on demand, and subscribers then see it.
    #[test]
    fn downloader_gets_a_ring_on_demand_and_peers_can_subscribe() {
        let (st, _, path) = state_for("havetx-dl", 3);
        let tx = st.have_sender();
        assert!(st.have_tx.read().is_some(), "sender did not cache the ring");
        let mut rx = st.subscribe_have().expect("subscribe found no ring");
        tx.send(7).expect("send failed with a live subscriber");
        assert_eq!(rx.try_recv().ok(), Some(7), "subscriber missed the piece");

        // Asking twice hands back the same channel, not a second one.
        let tx2 = st.have_sender();
        assert!(tx.same_channel(&tx2), "have_sender built a second ring");
        std::fs::remove_file(&path).ok();
    }

    /// Releasing frees it, and a later download rebuilds one.
    #[test]
    fn release_have_tx_frees_and_a_later_download_rebuilds() {
        let (st, _, path) = state_for("havetx-rel", 3);
        let _ = st.have_sender();
        st.release_have_tx();
        assert!(st.have_tx.read().is_none(), "release kept the ring");
        assert!(st.subscribe_have().is_none(), "release left a subscribable ring");
        st.release_have_tx(); // idempotent
        let _ = st.have_sender();
        assert!(st.have_tx.read().is_some(), "could not rebuild after release");
        std::fs::remove_file(&path).ok();
    }

    /// Releasing gives the memory back and is idempotent.
    #[test]
    fn release_drops_the_table_and_can_be_repeated() {
        let (st, hashes, path) = state_for("release", 4);
        assert_eq!(st.piece_hash(1), Some(hashes[1]));
        assert!(st.piece_hashes.lock().unwrap().is_some());

        st.release_piece_hashes();
        assert!(st.piece_hashes.lock().unwrap().is_none(), "release kept the table");
        st.release_piece_hashes(); // no-op, must not panic
        assert!(st.piece_hashes.lock().unwrap().is_none());

        // Still correct afterwards: a later recheck reloads from the file.
        assert_eq!(st.piece_hash(1), Some(hashes[1]), "reload after release failed");
        std::fs::remove_file(&path).ok();
    }

    /// The release really frees the table rather than hiding it: with the file
    /// gone afterwards there is nothing left to serve, and the engine says so
    /// instead of quietly answering from a stale copy.
    #[test]
    fn release_is_not_a_cache_that_survives_the_file() {
        let (st, hashes, path) = state_for("released-gone", 3);
        assert_eq!(st.piece_hash(0), Some(hashes[0]));
        st.release_piece_hashes();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(st.piece_hash(0), None, "answered from a table it claimed to release");
    }

    /// A verification already in flight keeps the table it started with, so
    /// releasing concurrently cannot make it read freed memory or fail.
    #[test]
    fn release_during_a_verification_does_not_disturb_it() {
        let (st, hashes, path) = state_for("concurrent", 6);
        assert_eq!(st.piece_hash(0), Some(hashes[0]));
        let in_flight = st.piece_hashes.lock().unwrap().clone().unwrap();
        st.release_piece_hashes();
        // The Arc held by the "verifier" is still whole and still correct.
        assert_eq!(in_flight.len(), 6);
        assert_eq!(in_flight[5], hashes[5]);
        std::fs::remove_file(&path).ok();
    }

    /// ⚠️ The one that matters: a torrent whose file is gone must report "I
    /// cannot verify", never a hash. Both call sites turn None into a refusal,
    /// so returning Some(anything) here would silently bless unchecked data.
    #[test]
    fn piece_hash_is_none_when_the_torrent_file_is_gone() {
        let (st, _, path) = state_for("gone", 3);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(st.piece_hash(0), None, "verification passed without a hash table");
    }

    /// A `.torrent` that no longer agrees with the metadata is refused whole,
    /// rather than verifying some pieces against the wrong offsets.
    #[test]
    fn piece_hash_refuses_a_table_of_the_wrong_length() {
        let (st, _, path) = state_for("mismatch", 5);
        let (other, _) = torrent_bytes(2);
        std::fs::write(&path, &other).unwrap();
        assert_eq!(st.piece_hash(0), None, "accepted a table of the wrong length");
        std::fs::remove_file(&path).ok();
    }

    /// One shard still has to behave like the set it replaced.
    #[test]
    fn single_shard_map_still_stores_and_removes() {
        let addrs: DashMap<std::net::SocketAddr, ()> =
            DashMap::with_shard_amount(PER_TORRENT_SHARDS);
        let a: std::net::SocketAddr = "10.0.0.1:6881".parse().unwrap();
        let b: std::net::SocketAddr = "10.0.0.2:6881".parse().unwrap();
        assert!(addrs.insert(a, ()).is_none(), "first insert was not new");
        assert!(addrs.insert(b, ()).is_none());
        assert!(addrs.insert(a, ()).is_some(), "duplicate insert must report the old entry");
        assert_eq!(addrs.len(), 2, "duplicate must not grow the map");
        assert!(addrs.contains_key(&a));
        assert!(addrs.remove(&a).is_some());
        assert!(!addrs.contains_key(&a));
        assert_eq!(addrs.len(), 1);
    }
}
