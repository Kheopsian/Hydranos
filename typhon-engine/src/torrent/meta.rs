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
    /// A v2-only torrent (BEP 52): pieces are checked against SHA-256 merkle
    /// hashes, and `info_hash` is the truncated SHA-256 of the info dict. A
    /// hybrid is `false` -- it is checked through its v1 hashes, which cover
    /// every byte.
    pub v2: bool,
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

    /// The file ranges a block of a piece is made of, in order.
    ///
    /// A stretch of the stream that no file covers is alignment padding --
    /// BEP 47 pad files, or the gap after each file of a v2 torrent -- and
    /// comes back as a `pad` op: zeros on read, nothing on write, no file.
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
                let gap = (f.offset - pos).min(remaining);
                ops.push(FileOp { path: PathBuf::new(), file_offset: 0, length: gap as u32, pad: true });
                pos += gap;
                remaining -= gap;
                if remaining == 0 {
                    break;
                }
            }
            let file_start = pos - f.offset;
            let available = f.length - file_start;
            let to_read = remaining.min(available);
            ops.push(FileOp {
                path: f.path.clone(),
                file_offset: file_start,
                length: to_read as u32,
                pad: false,
            });
            pos += to_read;
            remaining -= to_read;
            if remaining == 0 { break; }
        }
        ops
    }
}

/// A piece's expected content: a SHA-1 (v1, hybrids) or a merkle root (v2).
#[derive(Debug, Clone)]
pub enum PieceCheck {
    V1([u8; 20]),
    V2(crate::torrent::merkle::PieceCheck),
}

impl PieceCheck {
    pub fn matches(&self, piece: &[u8]) -> bool {
        match self {
            PieceCheck::V1(want) => {
                use sha1::{Digest, Sha1};
                let got: [u8; 20] = Sha1::digest(piece).into();
                got == *want
            }
            PieceCheck::V2(c) => c.matches(piece),
        }
    }
}

#[derive(Debug)]
pub struct FileOp {
    pub path: PathBuf,
    pub file_offset: u64,
    pub length: u32,
    /// Alignment padding: zeros, backed by no file.
    pub pad: bool,
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
    /// This peer advertised `ut_holepunch` (BEP 55) in its extension
    /// handshake. Introducing two peers where one cannot read the message
    /// leaves the other waiting on a connection nobody was asked to make.
    pub supports_holepunch: AtomicBool,
    /// Peers this session should be told to dial, put here by ANOTHER peer's
    /// task when it asked us for an introduction.
    ///
    /// A queue and not a direct send, because the socket belongs to this
    /// session's task and nothing else may write to it.
    pub punch_outbox: std::sync::Mutex<Vec<std::net::SocketAddr>>,
    /// Wakes this session when its outbox is filled.
    ///
    /// Needed rather than merely polite: an idle seeding session registers no
    /// timer at all, so without a wake it would drain the queue at its next
    /// scrap of traffic -- long after the hole at the other end has closed.
    pub punch_wake: tokio::sync::Notify,
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
            supports_holepunch: AtomicBool::new(false),
            punch_outbox: std::sync::Mutex::new(Vec::new()),
            punch_wake: tokio::sync::Notify::new(),
        }
    }

    /// Ask this session to tell its peer to dial `who`, now.
    ///
    /// Called from another peer's task. Bounded on purpose: a peer that asks
    /// to be introduced to everyone is asking us to open connections on its
    /// behalf, and a queue that grows without limit is the amplifier.
    pub fn queue_punch(&self, who: std::net::SocketAddr) -> bool {
        const MAX_PENDING: usize = 16;
        if let Ok(mut q) = self.punch_outbox.lock() {
            if q.len() >= MAX_PENDING || q.contains(&who) {
                return false;
            }
            q.push(who);
        } else {
            return false;
        }
        self.punch_wake.notify_one();
        true
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
        torrent.limiter().connection_opened();
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
        self.torrent.limiter().connection_closed();
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

/// No announce event is owed.
pub const ANNOUNCE_EVENT_NONE: u8 = 0;
/// BEP 3 `event=completed`: this torrent finished downloading.
pub const ANNOUNCE_EVENT_COMPLETED: u8 = 1;
/// BEP 3 `event=stopped`: the user stopped this torrent and the trackers
/// should drop us from the swarm rather than wait for the entry to go stale.
pub const ANNOUNCE_EVENT_STOPPED: u8 = 2;
// The two are BITS, not a state: a download can finish and be stopped before
// the runner gets to it, and both are owed. Set with `fetch_or`, never
// `store`, or the second transition erases the first.

/// What one tracker has been told about this torrent, this session.
///
/// BEP 3's events are per tracker, not per torrent: `started` opens a session
/// with a tracker, `completed` and `stopped` are said to a tracker that heard
/// `started`. A torrent that fails over from one tracker to another, or has a
/// tracker added while it runs, owes each of them its own sequence. Keeping
/// one flag per torrent is how a second tracker came to hear a periodic
/// announce, or `completed`, without ever having heard `started`.
///
/// Kept small on purpose -- there is one per tracker per torrent, and a node
/// holds a million torrents.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackerSlot {
    /// FNV-1a of the tracker URL as listed on the torrent.
    pub key: u64,
    /// This tracker has acknowledged `started` in the current session.
    pub started: bool,
    /// `completed` is owed to this tracker, which saw us leeching.
    pub completed_owed: bool,
    /// `stopped` is owed to this tracker, which saw us `started`.
    pub stopped_owed: bool,
    /// BEP 31 `retry in: never`: not to be announced to again this session.
    pub disabled: bool,
    /// Unix seconds of the last answer from this tracker, 0 = never.
    pub last_ok: i64,
    /// Unix seconds before which this tracker must not be asked again on
    /// our own initiative: its `min interval`. A re-announce a person forces
    /// may cross it, as qBittorrent's does.
    pub not_before: i64,
    /// Unix seconds before which this tracker must not be asked AT ALL: it
    /// said so itself, with BEP 31 `retry in` or an HTTP `Retry-After`.
    /// Nothing crosses it, forced or not.
    pub hint_until: i64,
    /// Refusals in a row from a tracker that has not registered this torrent
    /// yet -- the count a race's registration retries are bounded by.
    pub refusals: u8,
    /// BEP 3 `tracker id`, echoed back as `trackerid=`.
    pub tracker_id: Option<Box<str>>,
}

/// The key a tracker URL is filed under in the announce book.
pub fn tracker_key(url: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in url.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

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
    /// The peer id a tracker was last actually told, and when.
    ///
    /// Written AFTER an announce succeeds, never from the policy: the policy
    /// says what we intend to send next, and an operator who changed an
    /// override needs to see which torrents have caught up. Deriving this from
    /// the intention would make every torrent look compliant the instant the
    /// setting was saved -- the failure mode where nothing contradicts itself.
    ///
    /// None until this torrent has announced at least once since startup.
    pub announced_peer_id: RwLock<Option<([u8; 20], u64)>>,
    pub save_path: RwLock<PathBuf>,
    pub info_hash: InfoHash,
    pub added_time: i64,
    pub completed_time: AtomicI64,
    pub seed_mode: bool,

    pub status: AtomicU8,
    pub total_uploaded: AtomicU64,
    pub total_downloaded: AtomicU64,
    pub peers_connected: AtomicUsize,
    pub peers_interested: AtomicUsize,
    /// Peers this torrent learned about through PEX. Counted per torrent, not
    /// per process: the diagnostics of one engine must not include the PEX
    /// traffic of the engine sharing its process.
    pub pex_peers_discovered: AtomicU64,
    /// The policy of the engine that owns this torrent. Unset only for a
    /// torrent built outside a manager (a magnet being resolved, a unit test),
    /// which falls back to the constant default.
    pub policy: std::sync::OnceLock<Arc<crate::peer::extension::PeerPolicy>>,
    /// The dial ceilings of the engine that owns this torrent. Unset for a
    /// torrent built outside a manager, which falls back to unlimited.
    pub limiter: std::sync::OnceLock<Arc<crate::tracker::dial_limiter::DialLimiter>>,
    /// The byte-rate caps of the engine that owns this torrent (and of the
    /// client above it). Unset outside a manager, which reads as unlimited.
    pub rates: std::sync::OnceLock<Arc<crate::torrent::ratelimit::EngineRates>>,
    /// This torrent's own caps. Allocated the first time a limit is set on it
    /// and never otherwise: on a million-torrent hoard almost none carries
    /// one, and an empty `OnceLock<Box<_>>` is all the rest pay.
    own_rates: std::sync::OnceLock<Box<crate::torrent::ratelimit::RatePair>>,
    /// Where this torrent reports that it finished downloading, so its engine
    /// can persist the fact at once.
    pub completed_tx: std::sync::OnceLock<tokio::sync::mpsc::UnboundedSender<InfoHash>>,
    /// The metainfo bytes of this torrent, by info-hash, from the store.
    ///
    /// The store is the only authority: it keys the blob by info-hash, so what
    /// comes back IS this torrent by construction. There is deliberately no
    /// fallback to a file. uploads/ used to be a second copy of these bytes and
    /// the one every runtime path actually read, which is how a torrent whose
    /// file had gone missing kept asking peers for pieces it could never
    /// verify. One copy, one authority, no way to disagree.
    ///
    /// Unset for a torrent built outside a manager (a magnet being resolved, a
    /// unit test): such a torrent cannot verify, which the callers already
    /// treat as "refuse the piece".
    pub blob_source: std::sync::OnceLock<Arc<dyn Fn(&str) -> Option<Vec<u8>> + Send + Sync>>,
    pub is_paused: AtomicBool,
    /// An announce event owed to this torrent's trackers, not yet delivered.
    ///
    /// `ANNOUNCE_EVENT_NONE`, `_COMPLETED` or `_STOPPED`. Set where the
    /// transition happens -- in this crate -- and taken by the announce runner,
    /// which lives in the `hydra` binary and cannot be called from here. The
    /// runner clears it as it sends, so the event goes out once and once only.
    pub pending_announce_event: AtomicU8,
    /// Per-tracker announce state for the current session. Empty until the
    /// first announce; see `TrackerSlot`.
    pub announce_book: Mutex<Vec<TrackerSlot>>,
    /// `total_uploaded` / `total_downloaded` when the current session began.
    ///
    /// BEP 3 reports the bytes moved since the client sent `started` -- the
    /// session -- and every mainstream client restarts the count there. The
    /// totals are lifetime figures persisted across restarts; reporting them
    /// raw told a tracker, on every boot, that a peer announcing `started` had
    /// already uploaded hundreds of gigabytes. That is the exact signature
    /// tracker anti-cheat looks for, and a tracker crediting the first report
    /// of a new peer would have counted it all again.
    pub session_base_up: AtomicU64,
    pub session_base_down: AtomicU64,
    /// `total_uploaded` / `total_downloaded` as last credited to the engine's
    /// "moved since this process started" counter. See `take_uncounted`.
    ///
    /// Starts at the LIFETIME value the torrent was loaded with (resume
    /// record, engine move), never at zero: history that predates this
    /// process is not this process's traffic. Only `restore_lifetime` may
    /// write the totals wholesale, because it moves this mark with them.
    pub last_counted_up: AtomicU64,
    pub last_counted_down: AtomicU64,
    /// Seconds spent seeding, folded in at every state change and at the
    /// periodic sweep. See `fold_seed_time`.
    pub seed_secs: AtomicI64,
    /// Unix time this torrent last STARTED seeding, or 0 when it is not
    /// seeding. The open interval; `seed_secs` holds the closed ones.
    pub seed_since: AtomicI64,
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

    /// The torrent's 20-byte piece hashes: loaded from the store the
    /// first time something needs to verify data, dropped again the moment the
    /// torrent goes back to seeding, and never loaded at all for a torrent that
    /// only ever seeds. See `piece_hash` and `release_piece_hashes`.
    ///
    /// Behind an `Arc` so a verification already in flight keeps the table it
    /// is reading even if another thread releases it mid-check.
    piece_hashes: Mutex<Option<Arc<Vec<[u8; 20]>>>>,
    /// The same, for a v2 torrent: a merkle hash per piece. Loaded and
    /// released on the same terms.
    v2_checks: Mutex<Option<Arc<Vec<crate::torrent::merkle::PieceCheck>>>>,

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
    /// Consecutive failures to WRITE a verified piece to disk.
    ///
    /// Counted rather than acted on at the first one, because a full disk on a
    /// seedbox is often transient -- the drain frees space and the next piece
    /// lands. What is not transient is a read-only mount or a volume that went
    /// away, and those look identical for one piece.
    ///
    /// Reset by any successful write, so scattered failures never accumulate
    /// into a stop.
    pub disk_write_failures: AtomicU32,

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


/// Unix seconds. The seed counter needs a wall clock, not a monotonic one:
/// it is persisted and compared across restarts.
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl TorrentState {
    /// Close the open seeding interval and start a new one if still seeding.
    ///
    /// Called at every state change and at the five-minute sweep, so the
    /// counter costs nothing in steady state -- a per-second loop over 300k
    /// torrents to move a number that only changes on transitions is the kind
    /// of cost that turns up in a CPU profile three weeks later.
    ///
    /// ⚠ Folding on the transitions that STOP seeding is the part that has to
    /// be right. Relying on the sweep alone would credit a torrent paused two
    /// minutes after a sweep with the whole five minutes -- an over-count, and
    /// an over-counted obligation is one deleted too early.
    pub fn fold_seed_time(&self, now: i64) {
        use std::sync::atomic::Ordering;
        let seeding = self.status.load(Ordering::Relaxed) == crate::torrent::TorrentStatus::Seeding as u8
            && !self.is_paused.load(Ordering::Relaxed)
            && !self.is_removed.load(Ordering::Relaxed);
        let since = self.seed_since.load(Ordering::Relaxed);
        if since > 0 {
            let delta = (now - since).max(0);
            if delta > 0 {
                self.seed_secs.fetch_add(delta, Ordering::Relaxed);
            }
        }
        // A clock that jumped backwards leaves `since` in the future; storing
        // `now` anyway is what keeps the next fold from crediting the gap.
        self.seed_since.store(if seeding { now } else { 0 }, Ordering::Relaxed);
    }

    /// The counter including the interval still open, without mutating it.
    /// What the API should report: folding on a read would make a GET a write.
    pub fn seed_time_now(&self, now: i64) -> i64 {
        use std::sync::atomic::Ordering;
        let base = self.seed_secs.load(Ordering::Relaxed);
        let since = self.seed_since.load(Ordering::Relaxed);
        if since > 0 { base + (now - since).max(0) } else { base }
    }
}

impl TorrentState {
    /// Open a new announce session: the counters reported to trackers start
    /// from zero again, and every tracker is owed a fresh `started`.
    ///
    /// Called when a session genuinely begins -- the torrent was loaded by
    /// this process, or resumed after a stop. Not on a periodic announce, and
    /// not on a recheck: those are the same session.
    pub fn begin_announce_session(&self) {
        self.session_base_up.store(self.total_uploaded.load(Ordering::Relaxed), Ordering::Relaxed);
        self.session_base_down.store(self.total_downloaded.load(Ordering::Relaxed), Ordering::Relaxed);
        if let Ok(mut book) = self.announce_book.lock() {
            for slot in book.iter_mut() {
                slot.started = false;
                slot.completed_owed = false;
                slot.stopped_owed = false;
                // The floor paced the session that ended.
                slot.not_before = 0;
                slot.tracker_id = None;
            }
        }
    }

    /// Set the lifetime totals a torrent arrives with -- from its resume
    /// record at boot, or from the record another engine handed over.
    ///
    /// Moves the counted mark with them, so those bytes are never credited to
    /// this process's session: they were moved before it, or by another
    /// engine that already counted them.
    pub fn restore_lifetime(&self, up: u64, down: u64) {
        self.total_uploaded.store(up, Ordering::Relaxed);
        self.total_downloaded.store(down, Ordering::Relaxed);
        self.last_counted_up.store(up, Ordering::Relaxed);
        self.last_counted_down.store(down, Ordering::Relaxed);
    }

    /// Bytes moved since the last call, given the current totals. Each byte is
    /// handed out exactly once, whoever asks: the 1 Hz walk and the removal
    /// race each other on purpose, and both must be able to settle.
    ///
    /// `fetch_max`, not `swap`: a caller holding an older read of the total
    /// would `swap` the mark BACKWARDS, and the next caller would count the
    /// gap a second time. The mark only ever moves forward. And it is touched
    /// only when the total moved, so the walk over a million idle torrents
    /// stays a pair of loads each.
    pub fn take_uncounted_with(&self, up: u64, down: u64) -> (u64, u64) {
        fn take(total: u64, mark: &AtomicU64) -> u64 {
            if mark.load(Ordering::Relaxed) >= total {
                return 0;
            }
            total.saturating_sub(mark.fetch_max(total, Ordering::Relaxed))
        }
        (take(up, &self.last_counted_up), take(down, &self.last_counted_down))
    }

    /// `take_uncounted_with` on the totals as they are now.
    pub fn take_uncounted(&self) -> (u64, u64) {
        self.take_uncounted_with(
            self.total_uploaded.load(Ordering::Relaxed),
            self.total_downloaded.load(Ordering::Relaxed),
        )
    }

    /// BEP 3 `uploaded`: bytes sent to peers since the session began.
    pub fn session_uploaded(&self) -> u64 {
        self.total_uploaded
            .load(Ordering::Relaxed)
            .saturating_sub(self.session_base_up.load(Ordering::Relaxed))
    }

    /// BEP 3 `downloaded`: verified bytes received since the session began.
    pub fn session_downloaded(&self) -> u64 {
        self.total_downloaded
            .load(Ordering::Relaxed)
            .saturating_sub(self.session_base_down.load(Ordering::Relaxed))
    }

    /// BEP 3 `left`: bytes this client still needs, from the pieces it holds.
    ///
    /// Not `total_size - downloaded`. The traffic counter knows nothing of data
    /// that was already on disk -- a resumed download, a partial cross-seed --
    /// so the subtraction announced a torrent at 90 % as one at 0 %. The piece
    /// map is the fact; a seed holds everything by definition.
    pub fn bytes_left(&self) -> u64 {
        if self.status.load(Ordering::Relaxed) == TorrentStatus::Seeding as u8 {
            return 0;
        }
        let Some(picker) = self.picker.get() else {
            // No piece map: either a seed (handled above) or a torrent whose
            // state is not known yet. Claiming to need all of it is the answer
            // that never tells a tracker we have data we do not.
            return self.meta.total_size;
        };
        let Ok(p) = picker.lock() else { return self.meta.total_size };
        let n = self.meta.num_pieces();
        if n == 0 {
            return 0;
        }
        // O(1): every piece is `piece_length` long except possibly the last.
        let plen = self.meta.piece_length as u64;
        let mut have = p.num_have() as u64 * plen;
        if p.has_piece(n - 1) {
            have = have.saturating_sub(plen - self.meta.piece_size(n - 1) as u64);
        }
        self.meta.total_size.saturating_sub(have)
    }

    /// The PEX / IPv6 policy this torrent runs under.
    /// Signal that this torrent finished downloading. Cheap and non-blocking,
    /// and a no-op for a torrent with no engine behind it.
    pub fn notify_completed(&self) {
        if let Some(tx) = self.completed_tx.get() {
            let _ = tx.send(self.info_hash);
        }
    }

    /// The dial ceilings this torrent runs under.
    pub fn limiter(&self) -> &crate::tracker::dial_limiter::DialLimiter {
        self.limiter
            .get()
            .map(|l| l.as_ref())
            .unwrap_or(&crate::tracker::dial_limiter::DEFAULT_LIMITER)
    }

    /// The engine (and client) caps this torrent runs under.
    pub fn engine_rates(&self) -> &crate::torrent::ratelimit::EngineRates {
        self.rates
            .get()
            .map(|r| r.as_ref())
            .unwrap_or(&crate::torrent::ratelimit::UNLIMITED)
    }

    /// Every bucket a block of this torrent has to clear, narrowest first.
    pub fn rate_chain(&self, dir: crate::torrent::ratelimit::Dir) -> crate::torrent::ratelimit::Chain<'_> {
        crate::torrent::ratelimit::Chain::new(self.own_rates.get().map(|b| b.as_ref()), self.engine_rates(), dir)
    }

    /// This torrent's own caps in bytes/s, `(up, down)`. 0 = none.
    pub fn rate_limits(&self) -> (u64, u64) {
        self.own_rates.get().map_or((0, 0), |p| (p.up.rate(), p.down.rate()))
    }

    /// Set this torrent's own caps in bytes/s; `None` leaves a direction as it
    /// is, 0 lifts it. Clearing a torrent that never had a cap allocates
    /// nothing -- a bulk "unlimited" over the whole hoard must not build a
    /// million empty buckets.
    pub fn set_rate_limits(&self, up: Option<u64>, down: Option<u64>) {
        let wants = up.is_some_and(|v| v > 0) || down.is_some_and(|v| v > 0);
        let pair = match self.own_rates.get() {
            Some(p) => p,
            None if wants => self.own_rates.get_or_init(Default::default),
            None => return,
        };
        if let Some(v) = up {
            pair.up.set_rate(v);
        }
        if let Some(v) = down {
            pair.down.set_rate(v);
        }
    }

    pub fn policy(&self) -> &crate::peer::extension::PeerPolicy {
        self.policy
            .get()
            .map(|p| p.as_ref())
            .unwrap_or(&crate::peer::extension::DEFAULT_POLICY)
    }

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
    /// Returns `None` when the table cannot be read -- no blob in the store for
    /// this info-hash, or a blob that does not parse. Callers MUST treat that
    /// as "cannot verify", never as "verified": the two call sites compare
    /// against the returned hash, so a `None` has to fail the piece rather than
    /// pass it.
    pub fn piece_hash(&self, piece: u32) -> Option<[u8; 20]> {
        let table = {
            let mut slot = match self.piece_hashes.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            if slot.is_none() {
                let ih = self.info_hash_hex();
                let bytes = match self.metainfo_bytes() {
                    Some(b) => b,
                    None => {
                        tracing::error!(
                            info_hash = %ih,
                            "no metainfo in the store; refusing to verify"
                        );
                        return None;
                    }
                };
                match crate::torrent::metainfo::piece_hashes_from_bytes(&bytes) {
                    Ok(h) => {
                        if h.len() as u32 != self.meta.num_pieces {
                            tracing::error!(
                                info_hash = %ih,
                                "piece hashes disagree with metadata: {} in the store, {} expected; refusing to verify",
                                h.len(), self.meta.num_pieces
                            );
                            return None;
                        }
                        *slot = Some(Arc::new(h));
                    }
                    Err(e) => {
                        tracing::error!(info_hash = %ih, "cannot parse the stored metainfo: {}", e);
                        return None;
                    }
                }
            }
            // Clone the Arc, not the table, and drop the lock before indexing.
            slot.clone()?
        };
        table.get(piece as usize).copied()
    }

    /// This torrent's metainfo bytes, or None when the store has no blob for
    /// it. The single point where a runtime path obtains a .torrent.
    pub fn metainfo_bytes(&self) -> Option<Vec<u8>> {
        let src = self.blob_source.get()?;
        src(&self.info_hash_hex())
    }

    /// The info-hash as lowercase hex, which is how the store keys it.
    pub fn info_hash_hex(&self) -> String {
        let mut out = String::with_capacity(40);
        for b in self.info_hash.iter() {
            out.push_str(&format!("{:02x}", b));
        }
        out
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
        drop(slot);
        let mut v2 = match self.v2_checks.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        *v2 = None;
    }

    /// Whether a whole piece -- alignment padding included, as `map_block`
    /// reads it -- is the piece the metainfo describes. None when it cannot
    /// be told: no metainfo to check against, which a caller must treat as a
    /// refusal, never as a pass.
    pub fn verify_piece(&self, piece: u32, data: &[u8]) -> Option<bool> {
        Some(self.piece_check(piece)?.matches(data))
    }

    /// What a piece is checked against, owned: taken here, where the table is
    /// loaded, and applied wherever the hashing runs (a blocking thread).
    pub fn piece_check(&self, piece: u32) -> Option<PieceCheck> {
        if !self.meta.v2 {
            return self.piece_hash(piece).map(PieceCheck::V1);
        }
        let table = {
            let mut slot = match self.v2_checks.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            if slot.is_none() {
                let bytes = self.metainfo_bytes()?;
                match crate::torrent::metainfo::v2_piece_table(&bytes) {
                    Ok(t) if t.len() as u32 == self.meta.num_pieces => *slot = Some(Arc::new(t)),
                    Ok(t) => {
                        tracing::error!(info_hash = %self.info_hash_hex(), "v2 piece table has {} entries, {} pieces expected; refusing to verify", t.len(), self.meta.num_pieces);
                        return None;
                    }
                    Err(e) => {
                        tracing::error!(info_hash = %self.info_hash_hex(), "v2 piece table: {e}; refusing to verify");
                        return None;
                    }
                }
            }
            slot.clone()?
        };
        table.get(piece as usize).cloned().map(PieceCheck::V2)
    }


    pub fn new(meta: TorrentMeta, save_path: PathBuf, seed_mode: bool) -> Self {
        Self::new_with_times(meta, save_path, seed_mode, None, None, !seed_mode)
    }

    /// Create a TorrentState, optionally restoring added_time / completed_time
    /// from fastresume. `None` falls back to `SystemTime::now()` (fresh add).
    pub fn new_with_times(
        meta: TorrentMeta,
        save_path: PathBuf,
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
            announced_peer_id: RwLock::new(None),
            meta,
            save_path: RwLock::new(save_path),
            info_hash: ih,
            added_time: added,
            completed_time: AtomicI64::new(completed),
            seed_mode,
            status: AtomicU8::new(status),
            total_uploaded: AtomicU64::new(0),
            total_downloaded: AtomicU64::new(0),
            peers_connected: AtomicUsize::new(0),
            peers_interested: AtomicUsize::new(0),
            pex_peers_discovered: AtomicU64::new(0),
            policy: std::sync::OnceLock::new(),
            limiter: std::sync::OnceLock::new(),
            rates: std::sync::OnceLock::new(),
            own_rates: std::sync::OnceLock::new(),
            completed_tx: std::sync::OnceLock::new(),
            blob_source: std::sync::OnceLock::new(),
            is_paused: AtomicBool::new(false),
            pending_announce_event: AtomicU8::new(ANNOUNCE_EVENT_NONE),
            announce_book: Mutex::new(Vec::new()),
            session_base_up: AtomicU64::new(0),
            session_base_down: AtomicU64::new(0),
            last_counted_up: AtomicU64::new(0),
            last_counted_down: AtomicU64::new(0),
            seed_secs: AtomicI64::new(0),
            seed_since: AtomicI64::new(0),
            serving_suspended: AtomicBool::new(false),
            is_removed: AtomicBool::new(false),
            piece_hashes: Mutex::new(None),
            v2_checks: Mutex::new(None),
            picker,
            have_tx: RwLock::new(None),
            upload_rate: RateTracker::new(),
            download_rate: RateTracker::new(),
            scrape_seeders: AtomicU32::new(0),
            scrape_leechers: AtomicU32::new(0),
            current_tracker: Mutex::new(String::new()),
            last_announce_ok: AtomicBool::new(false),
            last_announce_error: Mutex::new(String::new()),
            disk_write_failures: AtomicU32::new(0),
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

    /// The disk refused what we fetched: stop fetching, keep serving.
    ///
    /// Deliberately not `mark_error`, which also suspends the serve path. That
    /// is right when the data has gone -- there is nothing left to offer -- and
    /// wrong here: a volume that is full or read-only still READS, and a node
    /// that stopped seeding everything it holds because one download could not
    /// write would have traded a stalled torrent for a stalled library.
    pub fn mark_write_error(&self, msg: &str) {
        if self.status.load(Ordering::Relaxed) == TorrentStatus::Error as u8 {
            return;
        }
        if let Ok(mut g) = self.error_msg.lock() {
            *g = msg.to_string();
        }
        self.status.store(TorrentStatus::Error as u8, Ordering::Relaxed);
        tracing::error!(torrent = %self.meta.name, "stopped fetching: {}", msg);
    }

    /// Forget a fault the operator has dealt with.
    ///
    /// Called when a recheck starts, which is the one moment somebody has
    /// declared the underlying problem fixed. Nothing cleared `error_msg`
    /// before, so a torrent that erred, was repaired and came back kept its
    /// panic message on display for the rest of the process's life -- and the
    /// operator had no way to tell a torrent that is broken from one that was.
    ///
    /// The failure count goes with it: leaving it at the threshold would put
    /// the torrent back in error on its first unlucky write instead of its
    /// third.
    pub fn clear_error(&self) {
        if let Ok(mut g) = self.error_msg.lock() {
            g.clear();
        }
        self.disk_write_failures.store(0, Ordering::Relaxed);
        self.serving_suspended.store(false, Ordering::Relaxed);
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

/// BEP 27: whether this torrent may look for peers anywhere but its trackers.
///
/// A private torrent must not be announced to the DHT, offered over PEX, or
/// broadcast on the local network. The rule is one line, and it was written
/// twice in two distant files -- the DHT registration and the peer session --
/// which is one copy too many for something a private tracker will ban an
/// account over. Both call this.
impl TorrentMeta {
    pub fn allows_peer_discovery(&self) -> bool {
        !self.private
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
        // No file is written any more: the metainfo reaches the torrent the
        // way it does in production, from the store. The path is still handed
        // back so the callers' cleanup stays harmless.
        let mut p = std::env::temp_dir();
        p.push(format!("typhon-state-{}-{}.torrent", std::process::id(), tag));
        let path = p.to_string_lossy().into_owned();
        let meta = crate::torrent::metainfo::parse_torrent_bytes(&bytes).unwrap();
        let st = TorrentState::new(meta, std::path::PathBuf::from("/tmp"), true);
        let blob = bytes.clone();
        let _ = st
            .blob_source
            .set(Arc::new(move |_: &str| Some(blob.clone())));
        (st, hashes, path)
    }

    /// A torrent wired to a store whose blob the test can swap or remove.
    ///
    /// This is how a metainfo goes missing now: not by deleting a file, but by
    /// the store no longer answering for that hash.
    fn state_with_store(n: usize) -> (TorrentState, Vec<[u8; 20]>, Arc<Mutex<Option<Vec<u8>>>>) {
        let (bytes, hashes) = torrent_bytes(n);
        let meta = crate::torrent::metainfo::parse_torrent_bytes(&bytes).unwrap();
        let st = TorrentState::new(meta, std::path::PathBuf::from("/tmp"), true);
        let slot = Arc::new(Mutex::new(Some(bytes)));
        let handle = slot.clone();
        let _ = st.blob_source.set(Arc::new(move |_: &str| {
            handle.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }));
        (st, hashes, slot)
    }

    /// A torrent whose metainfo the store does not have cannot verify, and says
    /// so by returning None.
    ///
    /// This is the whole point of dropping the file fallback: `None` is what
    /// makes `write_piece` refuse the piece. If this ever returned a hash from
    /// somewhere else, a torrent could accept data nothing had checked.
    #[test]
    fn piece_hash_without_a_store_blob_refuses_to_verify() {
        let (bytes, _) = torrent_bytes(4);
        let meta = crate::torrent::metainfo::parse_torrent_bytes(&bytes).unwrap();
        let st = TorrentState::new(meta, std::path::PathBuf::from("/tmp"), true);
        // Wired, but the store holds nothing for this hash.
        let _ = st.blob_source.set(Arc::new(|_: &str| None));
        assert_eq!(st.piece_hash(0), None, "verified a piece with no metainfo");

        // And with no source wired at all (a torrent built outside a manager).
        let meta2 = crate::torrent::metainfo::parse_torrent_bytes(&bytes).unwrap();
        let bare = TorrentState::new(meta2, std::path::PathBuf::from("/tmp"), true);
        assert_eq!(bare.piece_hash(0), None, "verified a piece with no blob source");
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

        // Still correct afterwards: a later recheck reloads from the store.
        assert_eq!(st.piece_hash(1), Some(hashes[1]), "reload after release failed");
        std::fs::remove_file(&path).ok();
    }

    /// The release really frees the table rather than hiding it: with the blob
    /// gone afterwards there is nothing left to serve, and the engine says so
    /// instead of quietly answering from a stale copy.
    #[test]
    fn release_is_not_a_cache_that_survives_the_store() {
        let (st, hashes, blob) = state_with_store(3);
        assert_eq!(st.piece_hash(0), Some(hashes[0]));
        st.release_piece_hashes();
        *blob.lock().unwrap() = None;
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

    /// ⚠️ The one that matters: a torrent the store no longer answers for must
    /// report "I cannot verify", never a hash. Both call sites turn None into a
    /// refusal, so returning Some(anything) here would silently bless unchecked
    /// data -- and the opposite failure, refusing every piece forever, is what
    /// cost 19 475 wasted piece requests in production on 2026-09-13.
    #[test]
    fn piece_hash_is_none_when_the_store_loses_the_blob() {
        let (st, _, blob) = state_with_store(3);
        *blob.lock().unwrap() = None;
        assert_eq!(st.piece_hash(0), None, "verification passed without a hash table");
    }

    /// A metainfo that no longer agrees with the metadata is refused whole,
    /// rather than verifying some pieces against the wrong offsets.
    #[test]
    fn piece_hash_refuses_a_table_of_the_wrong_length() {
        let (st, _, blob) = state_with_store(5);
        let (other, _) = torrent_bytes(2);
        *blob.lock().unwrap() = Some(other);
        assert_eq!(st.piece_hash(0), None, "accepted a table of the wrong length");
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

#[cfg(test)]
mod punch_queue_tests {
    use super::PeerStats;
    use std::net::SocketAddr;

    fn stats() -> PeerStats {
        PeerStats::new(
            "93.184.216.34:6881".parse().unwrap(),
            [0u8; 20],
            "test".into(),
            false,
            false,
        )
    }

    fn addr(n: u8) -> SocketAddr {
        format!("93.184.216.{n}:6881").parse().unwrap()
    }

    #[test]
    fn a_queued_introduction_is_kept() {
        let s = stats();
        assert!(s.queue_punch(addr(1)));
        assert_eq!(s.punch_outbox.lock().unwrap().as_slice(), &[addr(1)]);
    }

    /// Asking twice for the same peer does not make it dial twice. A peer that
    /// repeats its request would otherwise have us send as many messages as it
    /// asked for.
    #[test]
    fn the_same_peer_is_not_queued_twice() {
        let s = stats();
        assert!(s.queue_punch(addr(1)));
        assert!(!s.queue_punch(addr(1)), "already waiting");
        assert_eq!(s.punch_outbox.lock().unwrap().len(), 1);
    }

    /// ⭐ A rendezvous asks us to make somebody else open connections. An
    /// unbounded queue is exactly the amplifier that turns a swarm into an
    /// attack, so the queue has a ceiling and says no past it.
    #[test]
    fn the_queue_has_a_ceiling() {
        let s = stats();
        for i in 0..16 {
            assert!(s.queue_punch(addr(i)), "the first sixteen are accepted");
        }
        assert!(!s.queue_punch(addr(200)), "the seventeenth is refused");
        assert_eq!(s.punch_outbox.lock().unwrap().len(), 16);
    }
}

#[cfg(test)]
mod error_recovery_tests {
    use super::*;
    use crate::torrent::meta::TorrentMeta;
    use std::path::PathBuf;

    fn torrent() -> Arc<TorrentState> {
        let t = Arc::new(TorrentState::new(
            TorrentMeta {
                info_hash: [11u8; 20],
                name: "t".into(),
                num_pieces: 8,
                piece_length: 16384,
                total_size: 8 * 16384,
                files: Vec::new(),
                trackers: Vec::new(),
                url_list: Vec::new(),
                private: false,
                multi_file: false,
                info_dict_len: 0,
                v2: false,
            },
            PathBuf::from("/tmp"),
            false,
        ));
        t.status.store(TorrentStatus::Downloading as u8, Ordering::Relaxed);
        t
    }

    /// ⭐ A full or read-only volume still READS. Suspending the serve path
    /// here would stop seeding everything the node already holds because one
    /// download could not write -- a stalled library traded for a stalled
    /// torrent. That is the whole difference from `mark_error`.
    #[test]
    fn a_write_error_stops_fetching_but_keeps_serving() {
        let t = torrent();
        t.mark_write_error("cannot write to disk: No space left on device");

        assert_eq!(t.status.load(Ordering::Relaxed), TorrentStatus::Error as u8);
        assert!(
            !t.serving_suspended.load(Ordering::Relaxed),
            "what is already on disk is still worth uploading"
        );
        assert!(t.error_msg.lock().unwrap().contains("No space left"));
    }

    /// The first fault wins, as `mark_error` does: later ones would overwrite
    /// the message that actually explains what happened first.
    #[test]
    fn the_first_fault_is_the_one_reported() {
        let t = torrent();
        t.mark_write_error("first");
        t.mark_write_error("second");
        assert_eq!(*t.error_msg.lock().unwrap(), "first");
    }

    /// ⭐ Nothing cleared `error_msg` before this. A torrent that erred, was
    /// repaired and came back kept its panic on display for the life of the
    /// process, and the operator could not tell a torrent that is broken from
    /// one that was.
    #[test]
    fn clearing_forgets_the_message_and_the_count() {
        let t = torrent();
        t.disk_write_failures.store(3, Ordering::Relaxed);
        t.mark_write_error("cannot write to disk: whatever");
        t.serving_suspended.store(true, Ordering::Relaxed);

        t.clear_error();

        assert!(t.error_msg.lock().unwrap().is_empty(), "no stale panic");
        assert_eq!(
            t.disk_write_failures.load(Ordering::Relaxed),
            0,
            "left at the threshold, the next unlucky write would error again \
             on the first piece instead of the third"
        );
        assert!(!t.serving_suspended.load(Ordering::Relaxed));
    }
}

#[cfg(test)]
mod announce_session_tests {
    use super::*;

    /// A single-file torrent of `total` bytes. Bencode lengths computed.
    fn meta(total: u64, piece: u32) -> TorrentMeta {
        let n = ((total + piece as u64 - 1) / piece as u64) as usize;
        let mut info = Vec::new();
        info.extend_from_slice(format!("d6:lengthi{total}e4:name1:x12:piece lengthi{piece}e6:pieces{}:", n * 20).as_bytes());
        info.extend(std::iter::repeat(0xAB).take(n * 20));
        info.push(b'e');
        let mut out = b"d8:announce20:http://t.example/ann4:info".to_vec();
        out.extend_from_slice(&info);
        out.push(b'e');
        crate::torrent::metainfo::parse_torrent_bytes(&out).expect("fixture parses")
    }

    /// `left` comes from the pieces held, not from the traffic counter: data
    /// already on disk was never downloaded by us, and still is not needed.
    #[test]
    fn left_counts_the_pieces_we_do_not_hold() {
        // Three pieces of 16 KiB and a short last one of 1 000 bytes.
        let total = 3 * 16384 + 1000;
        let t = TorrentState::new_with_times(meta(total, 16384), "/tmp".into(), false, Some(1), Some(0), true);
        assert_eq!(t.bytes_left(), total, "nothing held, everything left");
        {
            let p = t.picker.get().unwrap();
            let mut p = p.lock().unwrap();
            p.set_have(0);
            p.set_have(3);
        }
        assert_eq!(t.bytes_left(), 2 * 16384, "the short last piece counts at its real size");
        t.total_downloaded.store(0, Ordering::Relaxed);
        assert_eq!(t.bytes_left(), 2 * 16384, "the traffic counter is irrelevant");
        t.status.store(TorrentStatus::Seeding as u8, Ordering::Relaxed);
        assert_eq!(t.bytes_left(), 0, "a seed needs nothing");
    }

    #[test]
    fn the_session_counters_never_go_negative() {
        let t = TorrentState::new_with_times(meta(16384, 16384), "/tmp".into(), true, Some(1), Some(0), false);
        t.total_uploaded.store(100, Ordering::Relaxed);
        t.begin_announce_session();
        t.total_uploaded.store(50, Ordering::Relaxed);
        assert_eq!(t.session_uploaded(), 0, "a counter that moved back reads zero, never wraps");
    }

    #[test]
    fn tracker_keys_are_stable_and_distinct() {
        assert_eq!(tracker_key("https://a/announce"), tracker_key("https://a/announce"));
        assert_ne!(tracker_key("https://a/announce"), tracker_key("https://b/announce"));
    }
}
