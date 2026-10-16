use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::meta::FileEntry;

/// qBittorrent's file priorities, the only ones Hydranos accepts.
///
/// There is deliberately no 0 ("do not download"): Hydranos is a seeding
/// client and fetches every byte of a torrent. A priority changes the ORDER
/// pieces are fetched in, never the set.
pub const PRIO_NORMAL: u8 = 1;
pub const PRIO_HIGH: u8 = 6;
pub const PRIO_MAX: u8 = 7;
/// Rank of a file's first and last piece under "first/last piece first":
/// above every file priority, like qBittorrent, which gives them the top
/// priority whatever the file's own.
const RANK_FIRST_LAST: u8 = PRIO_MAX + 1;

pub fn is_valid_file_priority(p: u8) -> bool {
    matches!(p, PRIO_NORMAL | PRIO_HIGH | PRIO_MAX)
}

/// The operator's download-order settings for one torrent.
///
/// The default (nothing set) is what nearly every torrent carries, and it is
/// never stored: `TorrentState` keeps `None` for it, the resume record omits
/// it, and the picker keeps its plain rarest-first path.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadOrder {
    /// Pieces in index order instead of rarest first.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sequential: bool,
    /// Each file's first and last piece before anything else.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub first_last: bool,
    /// One priority per file, in `meta.files` order. Empty = all normal,
    /// which is how it is kept whenever every file is back to normal.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub file_prio: Vec<u8>,
}

impl DownloadOrder {
    pub fn is_default(&self) -> bool {
        !self.sequential && !self.first_last && self.file_prio.iter().all(|&p| p == PRIO_NORMAL)
    }

    pub fn file_priority(&self, index: usize) -> u8 {
        self.file_prio.get(index).copied().unwrap_or(PRIO_NORMAL)
    }

    /// Set one file's priority. Anything but 1/6/7 is ignored: the callers
    /// refuse it before getting here, and a 0 that slipped through must not
    /// become a file that is never fetched.
    pub fn set_file_priority(&mut self, index: usize, num_files: usize, prio: u8) {
        if index >= num_files || !is_valid_file_priority(prio) {
            return;
        }
        if self.file_prio.len() != num_files {
            self.file_prio.resize(num_files, PRIO_NORMAL);
        }
        self.file_prio[index] = prio;
        if self.file_prio.iter().all(|&p| p == PRIO_NORMAL) {
            self.file_prio = Vec::new();
        }
    }
}

/// What `pick_piece` consults when a torrent has a non-default order: the
/// settings turned into a rank per piece, computed once when they change
/// rather than walked from the file list on every pick.
pub struct PickOrder {
    sequential: bool,
    /// Higher is picked first. `None` when every piece ranks the same
    /// (sequential alone), so a sequential torrent pays no per-piece byte.
    rank: Option<Vec<u8>>,
}

impl PickOrder {
    /// `None` for the default order: the picker then takes the exact path it
    /// took before these settings existed.
    pub fn build(order: &DownloadOrder, files: &[FileEntry], piece_length: u32, num_pieces: u32) -> Option<Box<PickOrder>> {
        if order.is_default() {
            return None;
        }
        let ranked = order.first_last || order.file_prio.iter().any(|&p| p != PRIO_NORMAL);
        let rank = if ranked && num_pieces > 0 {
            let pl = piece_length.max(1) as u64;
            let last_piece = num_pieces as u64 - 1;
            let mut rank = vec![0u8; num_pieces as usize];
            for (i, f) in files.iter().enumerate() {
                if f.length == 0 {
                    continue;
                }
                let first = (f.offset / pl).min(last_piece) as usize;
                let last = ((f.offset + f.length - 1) / pl).min(last_piece) as usize;
                // A piece straddling two files takes the higher of the two:
                // putting it off would hold back the file that asked first.
                let p = order.file_priority(i);
                for r in &mut rank[first..=last] {
                    *r = (*r).max(p);
                }
                if order.first_last {
                    rank[first] = RANK_FIRST_LAST;
                    rank[last] = RANK_FIRST_LAST;
                }
            }
            Some(rank)
        } else {
            None
        };
        Some(Box::new(PickOrder { sequential: order.sequential, rank }))
    }
}

/// Who reserved a piece: one identity per peer session, one per webseed run.
pub type Owner = u64;

static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

/// A fresh owner identity. Never reused, so a release that comes late cannot
/// take away a reservation somebody else has made on the same piece since.
pub fn new_owner() -> Owner {
    NEXT_OWNER.fetch_add(1, Ordering::Relaxed)
}

/// One engine's pieces in flight and the bytes they really hold.
///
/// Kept by the pickers as they store and free blocks, so `/metrics` and the
/// five-minute allocator line read two atomics instead of walking a million
/// torrents. Measured on production before this existed: 17.3 GB of a 24 GB
/// heap sat under `start_piece`, and nothing short of a heap profile could see
/// it.
///
/// It is also the engine's download memory BUDGET. A piece costs its full size
/// from the moment it is started until its write is done: first as blocks
/// arriving, then as the assembled buffer `write_piece` hashes and writes.
/// Nothing bounded the sum. Every peer session holds one piece at a time, so
/// with a disk slower than the network the engine held one piece per
/// connection: measured 2026-10-14 on a 30 MB/s disk, 1 600 sessions of
/// 16 MiB pieces, RssAnon 6.7 GB at 16 s and 24.6 GB at 284 s, still climbing
/// -- the out-of-memory a Raspberry Pi install hit on a big download.
/// Over budget, no NEW piece is started; pieces already started go on, so the
/// budget drains by finishing and writing them and can never wedge.
#[derive(Debug, Default)]
pub struct PieceMem {
    pieces: AtomicU64,
    bytes: AtomicU64,
    /// Full size of every piece in flight: what they will hold when complete.
    reserved: AtomicU64,
    /// Assembled pieces handed to the disk and not yet written.
    writing: AtomicU64,
    /// Bytes `reserved + writing` may reach before no piece is started. 0 =
    /// no limit.
    budget: AtomicU64,
    /// Woken when a write returns: sessions held back by the budget try again
    /// at once instead of at their next choke tick.
    freed: tokio::sync::Notify,
}

impl PieceMem {
    pub fn pieces(&self) -> u64 {
        self.pieces.load(Ordering::Relaxed)
    }

    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// Bytes of assembled pieces waiting for their write.
    pub fn writing(&self) -> u64 {
        self.writing.load(Ordering::Relaxed)
    }

    /// Bytes the pieces in flight and the writes in progress may come to.
    pub fn committed(&self) -> u64 {
        self.reserved.load(Ordering::Relaxed) + self.writing()
    }

    pub fn budget(&self) -> u64 {
        self.budget.load(Ordering::Relaxed)
    }

    pub fn set_budget(&self, bytes: u64) {
        self.budget.store(bytes, Ordering::Relaxed);
    }

    /// May a new piece of `size` bytes be started? Always when nothing is
    /// committed, so a piece larger than the whole budget still downloads, one
    /// at a time.
    pub fn admits(&self, size: u64) -> bool {
        let budget = self.budget();
        let committed = self.committed();
        budget == 0 || committed == 0 || committed + size <= budget
    }

    /// Count `len` bytes as being written until the hold is dropped.
    pub fn hold_write(self: &Arc<Self>, len: u64) -> WriteHold {
        self.writing.fetch_add(len, Ordering::Relaxed);
        WriteHold { mem: self.clone(), len }
    }

    /// Resolves at the next write that gives budget back.
    pub async fn freed(&self) {
        self.freed.notified().await
    }

    fn add(&self, pieces: i64, bytes: i64) {
        apply(&self.pieces, pieces);
        apply(&self.bytes, bytes);
    }

    fn reserve(&self, bytes: i64) {
        apply(&self.reserved, bytes);
    }
}

/// An assembled piece on its way to the disk, counted in the budget until the
/// write returns, whichever way it returns.
#[derive(Debug)]
pub struct WriteHold {
    mem: Arc<PieceMem>,
    len: u64,
}

impl Drop for WriteHold {
    fn drop(&mut self) {
        self.mem.writing.fetch_sub(self.len, Ordering::Relaxed);
        self.mem.freed.notify_waiters();
    }
}

/// Default budget when the config leaves it at 0: an eighth of the memory this
/// process may use (the cgroup limit when there is one, else the machine's),
/// and never under 256 MiB. Per engine.
pub fn default_budget() -> u64 {
    const FLOOR: u64 = 256 << 20;
    memory_ceiling().map_or(0, |m| (m / 8).max(FLOOR))
}

fn memory_ceiling() -> Option<u64> {
    let total = std::fs::read_to_string("/proc/meminfo").ok().and_then(|s| {
        s.lines()
            .find_map(|l| l.strip_prefix("MemTotal:"))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
            .map(|kb| kb * 1024)
    });
    let cgroup = ["/sys/fs/cgroup/memory.max", "/sys/fs/cgroup/memory/memory.limit_in_bytes"]
        .iter()
        .find_map(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| s.trim().parse::<u64>().ok());
    match (total, cgroup) {
        (Some(t), Some(c)) => Some(t.min(c)),
        (t, c) => t.or(c),
    }
}

fn apply(c: &AtomicU64, d: i64) {
    if d >= 0 {
        c.fetch_add(d as u64, Ordering::Relaxed);
    } else {
        c.fetch_sub(d.unsigned_abs(), Ordering::Relaxed);
    }
}

/// What became of a block handed to `receive_block`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Block {
    /// Not stored: no such piece in flight, or not the block it claims to be.
    Refused,
    /// That block was already there (endgame, or a peer answering a request
    /// it had been asked twice).
    Duplicate,
    /// Stored; the piece still lacks blocks.
    Stored,
    /// Stored, and it was the last one: `take_piece_data` hands the piece over.
    Complete,
}

/// Rarest-first piece picker for download mode.
pub struct PiecePicker {
    num_pieces: u32,
    have: Vec<bool>,           // pieces we have
    availability: Vec<u16>,    // how many peers have each piece (u16: le swarm ne depasse jamais 65535 copies)
    pending: HashMap<u32, PendingPiece>,  // piece index -> in-flight reservation and the blocks received
    /// Bytes of block data held across `pending`: what this picker costs.
    held_bytes: u64,
    /// Full size of the pieces in `pending`: what they will cost complete.
    reserved_bytes: u64,
    /// The engine's totals, which this picker's changes are added to. `None`
    /// for a picker built outside a manager (a unit test, a magnet).
    mem: Option<Arc<PieceMem>>,
    /// The torrent's download order, when it is not the default. Boxed so the
    /// default costs one null pointer, and checked once per pick.
    order: Option<Box<PickOrder>>,
    /// Addresses cut off for sending pieces that failed their hash, refused
    /// for as long as this torrent downloads.
    banned: std::collections::HashSet<std::net::IpAddr>,
}

/// A piece somebody is fetching.
///
/// It costs the blocks that have ARRIVED and nothing else. It used to be a
/// `vec![0u8; piece_size]` made the moment a peer was asked for the piece:
/// 4 to 16 MiB reserved on a promise, kept for as long as that peer stayed
/// connected whether it sent anything or not, and made resident by the zero
/// fill on recycled allocator extents. On a production hoard (1.1 M torrents,
/// ~400 incomplete) that came to 17.3 GB under `start_piece`, growing by
/// ~0.5 GB/h.
struct PendingPiece {
    /// One slot per block, filled as the block arrives, at its exact length.
    blocks: Vec<Option<Box<[u8]>>>,
    received: u32,
    bytes: u64,
    block_size: u32,
    piece_size: u32,
    /// Everyone fetching it. Usually one; several in endgame, where the same
    /// piece is asked of several peers on purpose. The piece and its blocks go
    /// when the last of them lets go.
    owners: Vec<Owner>,
    /// Who delivered its blocks. One name means a failed hash is that peer's
    /// doing; several (endgame) prove nothing about any of them.
    sources: Vec<Owner>,
}

impl PendingPiece {
    fn block_len(&self, i: usize) -> u32 {
        let offset = i as u32 * self.block_size;
        std::cmp::min(self.block_size, self.piece_size.saturating_sub(offset))
    }

    /// The requests still owed for this piece: every block not yet received.
    fn missing(&self, piece: u32) -> Vec<(u32, u32, u32)> {
        (0..self.blocks.len())
            .filter(|&i| self.blocks[i].is_none())
            .map(|i| (piece, i as u32 * self.block_size, self.block_len(i)))
            .collect()
    }
}

impl Drop for PiecePicker {
    /// A torrent that goes takes its pieces in flight out of the engine's
    /// totals with it.
    fn drop(&mut self) {
        if let Some(m) = &self.mem {
            m.add(-(self.pending.len() as i64), -(self.held_bytes as i64));
            m.reserve(-(self.reserved_bytes as i64));
        }
    }
}

impl PiecePicker {
    pub fn new(num_pieces: u32) -> Self {
        Self {
            num_pieces,
            have: vec![false; num_pieces as usize],
            availability: vec![0; num_pieces as usize],
            pending: HashMap::new(),
            held_bytes: 0,
            reserved_bytes: 0,
            mem: None,
            order: None,
            banned: std::collections::HashSet::new(),
        }
    }

    /// Count this picker in an engine's totals. Whatever it already holds
    /// moves with it, so attaching twice, or to another engine, stays exact.
    pub fn attach_mem(&mut self, mem: Arc<PieceMem>) {
        if self.mem.as_ref().is_some_and(|m| Arc::ptr_eq(m, &mem)) {
            return;
        }
        let (n, b) = (self.pending.len() as i64, self.held_bytes as i64);
        let r = self.reserved_bytes as i64;
        if let Some(old) = self.mem.take() {
            old.add(-n, -b);
            old.reserve(-r);
        }
        mem.add(n, b);
        mem.reserve(r);
        self.mem = Some(mem);
    }

    /// The engine's totals this picker counts in, for a write to be held
    /// against them.
    pub fn mem(&self) -> Option<&Arc<PieceMem>> {
        self.mem.as_ref()
    }

    /// May a new piece of `size` bytes be started under the engine's budget?
    /// Always for a picker outside an engine.
    pub fn may_start(&self, size: u32) -> bool {
        self.mem.as_ref().is_none_or(|m| m.admits(size as u64))
    }

    /// Pieces in flight and the block bytes they hold, for this torrent.
    pub fn in_flight(&self) -> (usize, u64) {
        (self.pending.len(), self.held_bytes)
    }

    fn account(&mut self, pieces: i64, bytes: i64) {
        self.held_bytes = (self.held_bytes as i64 + bytes).max(0) as u64;
        if let Some(m) = &self.mem {
            m.add(pieces, bytes);
        }
    }

    /// Forget a piece in flight and free its blocks.
    fn drop_pending(&mut self, piece: u32) -> Option<PendingPiece> {
        let p = self.pending.remove(&piece)?;
        self.account(-1, -(p.bytes as i64));
        self.reserved_bytes = self.reserved_bytes.saturating_sub(p.piece_size as u64);
        if let Some(m) = &self.mem {
            m.reserve(-(p.piece_size as i64));
        }
        Some(p)
    }

    /// Install (or, with `None`, drop) the torrent's download order.
    pub fn set_order(&mut self, order: Option<Box<PickOrder>>) {
        self.order = order;
    }

    /// Is a non-default order installed?
    pub fn has_order(&self) -> bool {
        self.order.is_some()
    }

    /// Clear all have bits. Used before a recheck re-derives ownership
    /// from disk: without it set_have could only add pieces, never detect
    /// a piece that has since become corrupt/missing.
    pub fn reset_have(&mut self) {
        for h in self.have.iter_mut() {
            *h = false;
        }
    }

    /// Mark a piece as completed.
    pub fn set_have(&mut self, index: u32) {
        if (index as usize) < self.have.len() {
            self.have[index as usize] = true;
            self.drop_pending(index);
        }
    }

    /// Update availability from a peer's bitfield.
    pub fn add_bitfield(&mut self, bitfield: &[u8]) {
        for piece in 0..self.num_pieces {
            let byte_idx = piece as usize / 8;
            let bit_idx = 7 - (piece % 8);
            if byte_idx < bitfield.len() && (bitfield[byte_idx] >> bit_idx) & 1 == 1 {
                self.availability[piece as usize] = self.availability[piece as usize].saturating_add(1);
            }
        }
    }

    /// Update availability for a single HAVE message.
    pub fn add_have(&mut self, piece: u32) {
        if (piece as usize) < self.availability.len() {
            self.availability[piece as usize] = self.availability[piece as usize].saturating_add(1);
        }
    }

    /// Remove peer's contribution to availability.
    pub fn remove_bitfield(&mut self, bitfield: &[u8]) {
        for piece in 0..self.num_pieces {
            let byte_idx = piece as usize / 8;
            let bit_idx = 7 - (piece % 8);
            if byte_idx < bitfield.len() && (bitfield[byte_idx] >> bit_idx) & 1 == 1 {
                self.availability[piece as usize] = self.availability[piece as usize].saturating_sub(1);
            }
        }
    }

    /// Pick the next piece to request (rarest first).
    /// Returns None if all pieces are have or pending.
    ///
    /// Endgame: when ≤16 pieces remain non-have, allow picking pieces already
    /// in `pending` so multiple peers race for the same final pieces. Without
    /// this, stragglers (slow/dead peer holding the last piece) stall the DL.
    pub fn pick_piece(&self, peer_has: &[u8]) -> Option<u32> {
        // The one test the default pays. Everything below it is the picker
        // as it was before download orders existed, untouched.
        if let Some(order) = self.order.as_deref() {
            return self.pick_ordered(peer_has, order);
        }
        use rand::Rng;
        let remaining = self.num_pieces.saturating_sub(self.num_have());
        let endgame = remaining > 0 && remaining <= 16;

        // Rarest-first with reservoir sampling on ties: when multiple pieces
        // share the minimum availability, pick one uniformly at random.
        // Without this, peers converge on identical piece indices early
        // (all availability=1 at swarm start) → poor diversity → swarm can't
        // trade → our upload starves.
        let mut best_avail = u32::MAX;
        let mut best_count = 0u32;
        let mut best_pick: Option<u32> = None;
        let mut rng = rand::thread_rng();

        for piece in 0..self.num_pieces {
            if self.have[piece as usize] {
                continue;
            }
            if !endgame && self.pending.contains_key(&piece) {
                continue;
            }
            let byte_idx = piece as usize / 8;
            let bit_idx = 7 - (piece % 8);
            if byte_idx >= peer_has.len() || (peer_has[byte_idx] >> bit_idx) & 1 == 0 {
                continue;
            }
            let avail = self.availability[piece as usize] as u32;
            if avail < best_avail {
                best_avail = avail;
                best_count = 1;
                best_pick = Some(piece);
            } else if avail == best_avail {
                best_count += 1;
                if rng.gen_range(0..best_count) == 0 {
                    best_pick = Some(piece);
                }
            }
        }

        best_pick
    }

    /// `pick_piece` under a non-default order: the highest rank first; within
    /// a rank, the lowest index when sequential, else rarest first with the
    /// same random tie-break. The eligibility rules -- held, reserved,
    /// endgame, what the peer has -- are exactly the default's.
    ///
    /// Sequential still keeps several pieces in flight: every call skips the
    /// pieces already reserved, so a peer filling its pipeline takes the next
    /// ones in order, and so does the next peer.
    fn pick_ordered(&self, peer_has: &[u8], order: &PickOrder) -> Option<u32> {
        use rand::Rng;
        let remaining = self.num_pieces.saturating_sub(self.num_have());
        let endgame = remaining > 0 && remaining <= 16;
        let mut best_rank: i32 = -1;
        let mut best_avail = u32::MAX;
        let mut best_count = 0u32;
        let mut best_pick: Option<u32> = None;
        let mut rng = rand::thread_rng();

        for piece in 0..self.num_pieces {
            if self.have[piece as usize] {
                continue;
            }
            if !endgame && self.pending.contains_key(&piece) {
                continue;
            }
            let byte_idx = piece as usize / 8;
            let bit_idx = 7 - (piece % 8);
            if byte_idx >= peer_has.len() || (peer_has[byte_idx] >> bit_idx) & 1 == 0 {
                continue;
            }
            let rank = order.rank.as_ref().map_or(0, |r| r[piece as usize]) as i32;
            if rank < best_rank {
                continue;
            }
            if order.sequential {
                // Ascending walk: the first piece seen at a rank is the
                // lowest index of it, so only a strictly higher rank wins.
                if rank > best_rank {
                    best_rank = rank;
                    best_pick = Some(piece);
                }
                continue;
            }
            let avail = self.availability[piece as usize] as u32;
            if rank > best_rank || avail < best_avail {
                best_rank = rank;
                best_avail = avail;
                best_count = 1;
                best_pick = Some(piece);
            } else if avail == best_avail {
                best_count += 1;
                if rng.gen_range(0..best_count) == 0 {
                    best_pick = Some(piece);
                }
            }
        }

        best_pick
    }

    /// `owner` lets go of a piece. The last owner out frees it -- blocks and
    /// all -- so another source can pick it again from scratch.
    ///
    /// Called on peer disconnect, on a choke, on a Reject, when a peer has sent
    /// nothing for the piece in a while, and by the webseed on its failure
    /// paths. Releasing a piece the owner does not hold is a no-op: the
    /// reservation may have been completed, or remade by somebody else, since.
    /// The `have` bit is not consulted: an entry for a piece we hold is dead
    /// weight however it got there.
    pub fn release(&mut self, piece: u32, owner: Owner) {
        let last = match self.pending.get_mut(&piece) {
            Some(p) => {
                p.owners.retain(|&o| o != owner);
                p.owners.is_empty()
            }
            None => false,
        };
        if last {
            self.drop_pending(piece);
        }
    }

    /// Drop every piece in flight, whoever holds it. For a torrent that is
    /// being removed: its sessions may linger a moment, its memory must not.
    pub fn clear_pending(&mut self) {
        let pieces: Vec<u32> = self.pending.keys().copied().collect();
        for p in pieces {
            self.drop_pending(p);
        }
    }

    /// Export the `have` vector as a standard BT bitfield (MSB-first per byte).
    pub fn export_bitfield(&self) -> Vec<u8> {
        let num = self.num_pieces as usize;
        let byte_len = (num + 7) / 8;
        let mut bf = vec![0u8; byte_len];
        for (i, &h) in self.have.iter().enumerate() {
            if h {
                bf[i / 8] |= 1 << (7 - (i % 8));
            }
        }
        bf
    }

    /// Import a BT bitfield into `have`. Silently clamps to num_pieces.
    pub fn import_bitfield(&mut self, bitfield: &[u8]) {
        let num = self.num_pieces as usize;
        for i in 0..num {
            let byte_idx = i / 8;
            let bit_idx = 7 - (i % 8);
            if byte_idx < bitfield.len() && (bitfield[byte_idx] >> bit_idx) & 1 == 1 {
                self.have[i] = true;
            }
        }
    }

    /// Reserve a piece for `owner` and return the block requests it owes:
    /// (piece_index, offset, length).
    ///
    /// Nothing is allocated for the data here: a block costs memory when it
    /// arrives. A piece already in flight is JOINED, not reset -- the endgame
    /// asks several peers for the same piece on purpose, and a second start
    /// used to overwrite the first, throwing away every block received for it.
    /// The joiner is asked only for the blocks still missing.
    pub fn start_piece(&mut self, piece: u32, piece_size: u32, block_size: u32, owner: Owner) -> Vec<(u32, u32, u32)> {
        if let Some(p) = self.pending.get_mut(&piece) {
            if !p.owners.contains(&owner) {
                p.owners.push(owner);
            }
            return p.missing(piece);
        }
        if block_size == 0 {
            return Vec::new();
        }
        let num_blocks = piece_size.div_ceil(block_size) as usize;
        let p = PendingPiece {
            blocks: vec![None; num_blocks],
            received: 0,
            bytes: 0,
            block_size,
            piece_size,
            owners: vec![owner],
            sources: Vec::new(),
        };
        let requests = p.missing(piece);
        self.pending.insert(piece, p);
        self.account(1, 0);
        self.reserved_bytes += piece_size as u64;
        if let Some(m) = &self.mem {
            m.reserve(piece_size as i64);
        }
        requests
    }

    /// Store a block a peer sent, at its exact size.
    ///
    /// Only from a peer holding the piece: any other could fill a piece
    /// somebody else is fetching with bytes of its choosing, and the piece
    /// would fail its hash and be fetched again, as often as it pleased. The
    /// sender is remembered, so a piece that fails can be charged to it. A
    /// block for a piece no longer in flight -- released, completed -- is
    /// refused and costs nothing.
    pub fn receive_block_from(&mut self, piece: u32, offset: u32, data: &[u8], from: Owner) -> Block {
        if !self.holds(piece, from) {
            return Block::Refused;
        }
        let b = self.store_block(piece, offset, data);
        if matches!(b, Block::Stored | Block::Complete) {
            if let Some(p) = self.pending.get_mut(&piece) {
                if !p.sources.contains(&from) {
                    p.sources.push(from);
                }
            }
        }
        b
    }

    /// Store a block fetched by the caller itself (a webseed run), whose
    /// origin needs no checking.
    pub fn receive_block(&mut self, piece: u32, offset: u32, data: &[u8]) -> Block {
        self.store_block(piece, offset, data)
    }

    fn store_block(&mut self, piece: u32, offset: u32, data: &[u8]) -> Block {
        let Some(pending) = self.pending.get_mut(&piece) else {
            return Block::Refused;
        };
        // A block counts as received only once its bytes are actually
        // stored, and only if it is the block it claims to be.
        //
        // Marking the slot before the bounds check let a short, overlong or
        // unaligned block complete a piece with a hole of zeros in it. The
        // assembled buffer then failed SHA1 in write_piece, the piece was
        // re-requested, and with a large peer set that became a permanent
        // re-download loop: ~24% of completed pieces were being discarded.
        if pending.block_size == 0 || offset % pending.block_size != 0 {
            return Block::Refused;
        }
        let block_idx = (offset / pending.block_size) as usize;
        if block_idx >= pending.blocks.len() {
            return Block::Refused;
        }
        if data.len() != pending.block_len(block_idx) as usize {
            return Block::Refused;
        }
        if pending.blocks[block_idx].is_some() {
            return Block::Duplicate;
        }
        // Copied, not kept as the wire's `Bytes`: that one is a view into the
        // session's read buffer, and holding it would pin the whole buffer
        // (64 KiB and up) for every 16 KiB block waiting on its piece.
        pending.blocks[block_idx] = Some(Box::from(data));
        pending.received += 1;
        pending.bytes += data.len() as u64;
        let complete = pending.received as usize == pending.blocks.len();
        self.account(0, data.len() as i64);
        if complete {
            Block::Complete
        } else {
            Block::Stored
        }
    }

    /// The assembled piece, once every block is in; `None` before that, so a
    /// piece with a hole in it can never be handed to the disk.
    ///
    /// The one piece-sized buffer left on the download path: `write_piece`
    /// checks the hash and writes from a contiguous piece, and that path is
    /// shared with the webseed and deliberately left as it is. It lives from
    /// here to the end of the write, for the one piece being committed, and
    /// each block is freed as soon as it has been copied in.
    pub fn take_piece_data(&mut self, piece: u32) -> Option<Vec<u8>> {
        self.take_piece(piece).map(|(data, _)| data)
    }

    /// `take_piece_data`, with the one peer that sent every block of it, if
    /// a single one did.
    pub fn take_piece(&mut self, piece: u32) -> Option<(Vec<u8>, Option<Owner>)> {
        let ready = self
            .pending
            .get(&piece)
            .is_some_and(|p| p.received as usize == p.blocks.len());
        if !ready {
            return None;
        }
        let p = self.drop_pending(piece)?;
        let sole = match p.sources[..] {
            [one] => Some(one),
            _ => None,
        };
        let mut out = Vec::with_capacity(p.piece_size as usize);
        for b in p.blocks.into_iter().flatten() {
            out.extend_from_slice(&b);
        }
        Some((out, sole))
    }

    /// Refuse this address for as long as the torrent downloads.
    pub fn ban(&mut self, ip: std::net::IpAddr) {
        self.banned.insert(ip);
    }

    pub fn is_banned(&self, ip: std::net::IpAddr) -> bool {
        self.banned.contains(&ip)
    }

    /// Should `owner` still ask for this block? Not when the piece left its
    /// hands, and not when the block is already in -- from another peer in
    /// endgame, or from this one after a choke voided the request.
    pub fn wants_block(&self, piece: u32, offset: u32, owner: Owner) -> bool {
        match self.pending.get(&piece) {
            Some(p) if p.owners.contains(&owner) && p.block_size > 0 => {
                let i = (offset / p.block_size) as usize;
                i < p.blocks.len() && p.blocks[i].is_none()
            }
            _ => false,
        }
    }

    /// Does `owner` hold a reservation on this piece?
    pub fn holds(&self, piece: u32, owner: Owner) -> bool {
        self.pending.get(&piece).is_some_and(|p| p.owners.contains(&owner))
    }

    #[cfg(test)]
    pub fn begin_piece_for_test(&mut self, piece: u32, piece_size: u32, block_size: u32) {
        self.start_piece(piece, piece_size, block_size, 0);
    }

    /// How many pieces do we have?
    /// Swarm availability folded into (min, max, sum): how many copies of each
    /// piece are reachable right now. The minimum is the number qBittorrent
    /// shows — under 1.0 some piece is held by nobody we are connected to and
    /// the torrent cannot be completed until a seeder shows up. Pieces we
    /// already hold count as one copy: we are one of the holders.
    pub fn availability_stats(&self) -> (u32, u32, u64) {
        if self.availability.is_empty() {
            return (0, 0, 0);
        }
        let mut min = u32::MAX;
        let mut max = 0u32;
        let mut sum = 0u64;
        for (i, &peers) in self.availability.iter().enumerate() {
            let peers = peers as u32;
            let a = peers + if self.have[i] { 1 } else { 0 };
            min = min.min(a);
            max = max.max(a);
            sum += a as u64;
        }
        (min, max, sum)
    }

    pub fn num_have(&self) -> u32 {
        self.have.iter().filter(|&&h| h).count() as u32
    }

    /// Are we complete?
    pub fn is_complete(&self) -> bool {
        self.have.iter().all(|&h| h)
    }

    /// Lowest-indexed piece we neither hold nor have reserved.
    ///
    /// For a source that has everything, rarest-first is meaningless: every
    /// piece has availability 0, so `pick_piece` picks at random among all of
    /// them. That is right for a swarm and wrong for an HTTP mirror, where a
    /// batch of pieces is only worth batching if it is CONTIGUOUS -- a run can
    /// only be extended forwards, so starting in the middle truncates it.
    /// Walking from the front keeps every run as long as the torrent allows.
    pub fn first_missing(&self) -> Option<u32> {
        (0..self.num_pieces).find(|&i| {
            !self.have[i as usize] && !self.pending.contains_key(&i)
        })
    }

    /// Is this piece already reserved by someone?
    ///
    /// The webseed pool walks forward from a picked piece to batch a run of
    /// contiguous ones into a single HTTP request, and must stop at anything
    /// another source already holds: a run is one HTTP range, and a piece in it
    /// that a peer is already filling would be fetched twice.
    pub fn is_pending(&self, index: u32) -> bool {
        self.pending.contains_key(&index)
    }

    /// Do we have this specific piece?
    pub fn has_piece(&self, index: u32) -> bool {
        self.have.get(index as usize).copied().unwrap_or(false)
    }
}

#[cfg(test)]
mod receive_block_tests {
    use super::{Block, PieceMem, PiecePicker};
    use std::sync::Arc;

    fn picker_with_one_pending(piece_size: u32, block_size: u32) -> PiecePicker {
        let mut p = PiecePicker::new(1);
        p.begin_piece_for_test(0, piece_size, block_size);
        p
    }

    /// A block whose bytes do not fit must not be counted: counting it used to
    /// complete the piece with a hole and burn the whole piece on a SHA1 fail.
    #[test]
    fn short_block_does_not_complete_piece() {
        let mut p = picker_with_one_pending(32, 16);
        assert_eq!(p.receive_block(0, 0, &[1u8; 16]), Block::Stored);
        // second block arrives truncated -> refused, piece stays incomplete
        assert_eq!(p.receive_block(0, 16, &[2u8; 4]), Block::Refused);
        // the same block at its real length completes the piece
        assert_eq!(p.receive_block(0, 16, &[2u8; 16]), Block::Complete);
    }

    /// An unaligned offset addresses no block slot and must be refused.
    #[test]
    fn unaligned_offset_is_refused() {
        let mut p = picker_with_one_pending(32, 16);
        assert_eq!(p.receive_block(0, 5, &[1u8; 16]), Block::Refused);
        assert_eq!(p.receive_block(0, 0, &[1u8; 16]), Block::Stored);
        assert_eq!(p.receive_block(0, 16, &[2u8; 16]), Block::Complete);
    }

    /// The front-walking pick skips what we hold and what is reserved, and
    /// returns None only when there is nothing left to ask for.
    #[test]
    fn first_missing_walks_from_the_front() {
        let mut p = PiecePicker::new(4);
        assert_eq!(p.first_missing(), Some(0));
        p.set_have(0);
        assert_eq!(p.first_missing(), Some(1));
        p.begin_piece_for_test(1, 32, 16);
        assert_eq!(p.first_missing(), Some(2), "a reserved piece is skipped");
        p.set_have(2);
        p.set_have(3);
        assert_eq!(p.first_missing(), None, "1 is reserved, nothing else left");
    }

    /// A reserved piece must report as pending, so the webseed batcher stops
    /// its run there instead of resetting a piece a peer is mid-way through.
    #[test]
    fn reserved_piece_reports_pending() {
        let mut p = PiecePicker::new(4);
        assert!(!p.is_pending(2));
        p.begin_piece_for_test(2, 32, 16);
        assert!(p.is_pending(2));
        assert!(!p.is_pending(3));
        p.set_have(2);
        assert!(!p.is_pending(2), "set_have clears the reservation");
    }

    /// The tail block is shorter than block_size and must still be accepted.
    #[test]
    fn short_tail_block_is_accepted() {
        let mut p = picker_with_one_pending(20, 16);
        assert_eq!(p.receive_block(0, 0, &[1u8; 16]), Block::Stored);
        assert_eq!(p.receive_block(0, 16, &[2u8; 4]), Block::Complete);
    }

    /// ⭐ A piece in flight costs the blocks received, not the piece: starting
    /// a 4 MiB piece allocates no data at all, and each block adds exactly its
    /// length to the picker's and the engine's counters.
    #[test]
    fn a_piece_in_flight_holds_only_the_blocks_received() {
        let mem = Arc::new(PieceMem::default());
        let mut p = PiecePicker::new(4);
        p.attach_mem(mem.clone());
        let reqs = p.start_piece(1, 4 << 20, 16384, 7);
        assert_eq!(reqs.len(), 256);
        assert_eq!(p.in_flight(), (1, 0), "reserved, nothing held");
        assert_eq!((mem.pieces(), mem.bytes()), (1, 0));
        assert_eq!(p.receive_block(1, 0, &[1u8; 16384]), Block::Stored);
        assert_eq!(p.receive_block(1, 16384 * 9, &[2u8; 16384]), Block::Stored);
        assert_eq!(p.in_flight(), (1, 2 * 16384));
        assert_eq!(mem.bytes(), 2 * 16384);
        // A refused block costs nothing.
        assert_eq!(p.receive_block(1, 3, &[0u8; 16384]), Block::Refused);
        assert_eq!(p.receive_block(1, 16384, &[0u8; 100]), Block::Refused);
        assert_eq!(mem.bytes(), 2 * 16384);
        p.release(1, 7);
        assert_eq!(p.in_flight(), (0, 0));
        assert_eq!((mem.pieces(), mem.bytes()), (0, 0), "released, all of it");
    }

    /// Blocks arriving in any order make the same piece, byte for byte; and
    /// the piece is handed over only once it has no hole left.
    #[test]
    fn blocks_out_of_order_assemble_the_same_piece() {
        let piece: Vec<u8> = (0..40_000u32).map(|i| (i * 7 + 3) as u8).collect();
        let mut p = PiecePicker::new(1);
        let reqs = p.start_piece(0, piece.len() as u32, 16384, 1);
        assert_eq!(reqs, vec![(0, 0, 16384), (0, 16384, 16384), (0, 32768, 40_000 - 32768)]);
        assert!(p.take_piece_data(0).is_none(), "nothing to hand over yet");
        assert_eq!(p.receive_block(0, 32768, &piece[32768..]), Block::Stored);
        assert_eq!(p.receive_block(0, 0, &piece[..16384]), Block::Stored);
        assert!(p.take_piece_data(0).is_none(), "a hole is never handed to the disk");
        assert_eq!(p.receive_block(0, 0, &piece[..16384]), Block::Duplicate);
        assert_eq!(p.receive_block(0, 16384, &piece[16384..32768]), Block::Complete);
        assert_eq!(p.take_piece_data(0).as_deref(), Some(&piece[..]));
        assert_eq!(p.in_flight(), (0, 0));
        assert_eq!(p.receive_block(0, 0, &piece[..16384]), Block::Refused, "gone once taken");
    }

    /// ⭐ Endgame: a second peer joins the piece instead of resetting it, is
    /// asked only for what is missing, and the piece completes once.
    #[test]
    fn endgame_joins_a_piece_and_completes_it_once() {
        let mut p = PiecePicker::new(1);
        assert_eq!(p.start_piece(0, 32768, 16384, 1).len(), 2);
        assert_eq!(p.receive_block(0, 0, &[1u8; 16384]), Block::Stored);
        let second = p.start_piece(0, 32768, 16384, 2);
        assert_eq!(second, vec![(0, 16384, 16384)], "only the missing block");
        assert_eq!(p.in_flight(), (1, 16384), "the first block survived the join");
        assert!(!p.wants_block(0, 0, 2), "already in");
        assert!(p.wants_block(0, 16384, 2));
        assert_eq!(p.receive_block(0, 16384, &[2u8; 16384]), Block::Complete);
        assert_eq!(p.receive_block(0, 16384, &[2u8; 16384]), Block::Duplicate, "the other peer's copy");
        assert!(p.take_piece_data(0).is_some());
        assert!(p.take_piece_data(0).is_none(), "one piece, one hand-over");
        p.release(0, 1);
        p.release(0, 2);
        assert_eq!(p.in_flight(), (0, 0));
    }

    /// The piece stays while one owner still holds it, and goes with the last.
    /// A release that comes late cannot touch a reservation made since.
    #[test]
    fn the_last_owner_out_frees_the_piece() {
        let mut p = PiecePicker::new(2);
        p.start_piece(0, 32768, 16384, 1);
        p.start_piece(0, 32768, 16384, 2);
        p.receive_block(0, 0, &[1u8; 16384]);
        p.release(0, 1);
        assert_eq!(p.in_flight(), (1, 16384), "owner 2 is still on it");
        p.release(0, 2);
        assert_eq!(p.in_flight(), (0, 0));
        p.start_piece(0, 32768, 16384, 3);
        p.release(0, 1);
        assert!(p.holds(0, 3), "owner 1 let go long ago; 3's reservation stands");
    }

    /// A torrent dropped with pieces in flight takes them out of the engine's
    /// totals; so does a removal, before the last session is gone.
    #[test]
    fn a_dropped_or_cleared_picker_gives_its_bytes_back() {
        let mem = Arc::new(PieceMem::default());
        let mut p = PiecePicker::new(4);
        p.attach_mem(mem.clone());
        p.start_piece(0, 32768, 16384, 1);
        p.receive_block(0, 0, &[1u8; 16384]);
        p.start_piece(1, 32768, 16384, 2);
        assert_eq!((mem.pieces(), mem.bytes()), (2, 16384));
        p.clear_pending();
        assert_eq!((mem.pieces(), mem.bytes()), (0, 0));
        p.start_piece(2, 32768, 16384, 1);
        p.receive_block(2, 16384, &[1u8; 16384]);
        // Attaching again, or elsewhere, moves the totals rather than doubling them.
        p.attach_mem(mem.clone());
        assert_eq!((mem.pieces(), mem.bytes()), (1, 16384));
        let other = Arc::new(PieceMem::default());
        p.attach_mem(other.clone());
        assert_eq!((mem.pieces(), mem.bytes()), (0, 0));
        assert_eq!((other.pieces(), other.bytes()), (1, 16384));
        drop(p);
        assert_eq!((other.pieces(), other.bytes()), (0, 0));
    }

    /// `set_have` frees a piece in flight, whoever holds it.
    #[test]
    fn having_a_piece_frees_its_reservation() {
        let mem = Arc::new(PieceMem::default());
        let mut p = PiecePicker::new(2);
        p.attach_mem(mem.clone());
        p.start_piece(1, 32768, 16384, 1);
        p.receive_block(1, 0, &[1u8; 16384]);
        p.set_have(1);
        assert_eq!((mem.pieces(), mem.bytes()), (0, 0));
        assert_eq!(mem.committed(), 0, "and its place in the budget");
    }

    #[test]
    fn a_piece_counts_in_the_budget_from_its_start_to_the_end_of_its_write() {
        let mem = Arc::new(PieceMem::default());
        mem.set_budget(2 * 32768);
        let mut p = PiecePicker::new(4);
        p.attach_mem(mem.clone());
        p.start_piece(0, 32768, 16384, 1);
        assert_eq!(mem.committed(), 32768, "its full size, before a byte arrived");
        assert!(p.may_start(32768), "a second one fits");
        p.start_piece(1, 32768, 16384, 2);
        assert!(!p.may_start(32768), "a third one does not");

        p.receive_block(0, 0, &[1u8; 16384]);
        p.receive_block(0, 16384, &[1u8; 16384]);
        let data = p.take_piece_data(0).unwrap();
        let hold = p.mem().unwrap().hold_write(data.len() as u64);
        assert_eq!(mem.committed(), 2 * 32768, "taken for its write, still counted");
        assert!(!p.may_start(32768), "a slow disk keeps the budget spent");
        drop(hold);
        assert_eq!(mem.committed(), 32768);
        assert!(p.may_start(32768), "the write returned: room again");

        p.release(1, 2);
        assert_eq!(mem.committed(), 0, "a released piece gives its place back");
    }

    #[test]
    fn a_piece_larger_than_the_budget_still_downloads_alone() {
        let mem = Arc::new(PieceMem::default());
        mem.set_budget(16384);
        let mut p = PiecePicker::new(2);
        p.attach_mem(mem.clone());
        assert!(p.may_start(32768), "nothing committed: never wedged");
        p.start_piece(0, 32768, 16384, 1);
        assert!(!p.may_start(32768));
        mem.set_budget(0);
        assert!(p.may_start(32768), "0 is no limit");
    }

    #[test]
    fn a_dropped_or_moved_picker_takes_its_reservations_with_it() {
        let mem = Arc::new(PieceMem::default());
        let other = Arc::new(PieceMem::default());
        let mut p = PiecePicker::new(2);
        p.attach_mem(mem.clone());
        p.start_piece(0, 32768, 16384, 1);
        p.attach_mem(other.clone());
        assert_eq!((mem.committed(), other.committed()), (0, 32768));
        drop(p);
        assert_eq!(other.committed(), 0);
    }
}

#[cfg(test)]
mod order_tests {
    use super::*;
    use std::path::PathBuf;

    fn file(offset: u64, length: u64) -> FileEntry {
        FileEntry { path: PathBuf::from(format!("f{offset}")), offset, length }
    }

    /// A peer that has every piece.
    fn all(n: u32) -> Vec<u8> {
        vec![0xff; (n as usize).div_ceil(8)]
    }

    /// Availability per piece, as HAVE messages would build it.
    fn picker_with_avail(avail: &[u16]) -> PiecePicker {
        let mut p = PiecePicker::new(avail.len() as u32);
        for (i, &a) in avail.iter().enumerate() {
            for _ in 0..a {
                p.add_have(i as u32);
            }
        }
        p
    }

    fn ordered(p: &mut PiecePicker, o: &DownloadOrder, files: &[FileEntry], pl: u32) {
        let n = p.num_pieces;
        p.set_order(PickOrder::build(o, files, pl, n));
    }

    /// ⭐ The default order is not an order: building it allocates nothing,
    /// and the picker picks what it picked before these settings existed --
    /// on a fixed scenario with one rarest piece, that piece, every time.
    #[test]
    fn default_order_picks_exactly_as_before() {
        let files = [file(0, 8 * 16)];
        assert!(PickOrder::build(&DownloadOrder::default(), &files, 16, 8).is_none());
        let explicit_normal = DownloadOrder { file_prio: vec![PRIO_NORMAL], ..Default::default() };
        assert!(explicit_normal.is_default());
        assert!(PickOrder::build(&explicit_normal, &files, 16, 8).is_none());

        // Forty pieces, so the endgame (last sixteen) stays out of it.
        let mut avail = vec![5u16; 40];
        avail[4] = 1;
        avail[11] = 2;
        avail[27] = 3;
        let mut p = picker_with_avail(&avail);
        p.set_have(4);
        p.begin_piece_for_test(11, 16, 16);
        ordered(&mut p, &DownloadOrder::default(), &[file(0, 40 * 16)], 16);
        assert!(p.order.is_none());
        for _ in 0..50 {
            // 4 is held, 11 reserved: the rarest left is 27.
            assert_eq!(p.pick_piece(&all(40)), Some(27));
        }
    }

    #[test]
    fn sequential_takes_pieces_in_order_with_several_in_flight() {
        let files = [file(0, 40 * 16)];
        let mut p = picker_with_avail(&[5; 40]);
        p.add_have(30); // rarest-first would pick anything but 30
        let o = DownloadOrder { sequential: true, ..Default::default() };
        ordered(&mut p, &o, &files, 16);
        assert_eq!(p.pick_piece(&all(40)), Some(0));
        p.begin_piece_for_test(0, 16, 16);
        assert_eq!(p.pick_piece(&all(40)), Some(1), "the next one, while 0 is in flight");
        p.begin_piece_for_test(1, 16, 16);
        p.set_have(2);
        assert_eq!(p.pick_piece(&all(40)), Some(3));
        // A peer that lacks 3 is asked for the next piece it does have.
        let mut has = all(40);
        has[0] &= !(1 << 4);
        assert_eq!(p.pick_piece(&has), Some(4));
    }

    #[test]
    fn sequential_keeps_the_endgame() {
        let files = [file(0, 20 * 16)];
        let mut p = PiecePicker::new(20);
        for i in 0..18 {
            p.set_have(i);
        }
        ordered(&mut p, &DownloadOrder { sequential: true, ..Default::default() }, &files, 16);
        p.begin_piece_for_test(18, 16, 16);
        p.begin_piece_for_test(19, 16, 16);
        // Two left, both reserved: endgame lets another peer race for them.
        assert_eq!(p.pick_piece(&all(20)), Some(18));
    }

    #[test]
    fn high_priority_file_comes_before_normal_and_rarity_breaks_ties() {
        // Three files of four pieces each.
        let files = [file(0, 64), file(64, 64), file(128, 64)];
        // The rarest pieces are in file 0; file 2's rarest is piece 9.
        let mut p = picker_with_avail(&[1, 1, 1, 1, 4, 4, 4, 4, 3, 2, 3, 3]);
        let mut o = DownloadOrder::default();
        o.set_file_priority(2, 3, PRIO_HIGH);
        ordered(&mut p, &o, &files, 16);
        assert_eq!(p.pick_piece(&all(12)), Some(9));
        for i in 8..12 {
            p.set_have(i);
        }
        // File 2 done: back to rarest first among the normal ones.
        let pick = p.pick_piece(&all(12)).unwrap();
        assert!(pick < 4, "{pick}");
        // Maximal beats high.
        let mut q = picker_with_avail(&[1; 12]);
        o.set_file_priority(1, 3, PRIO_MAX);
        ordered(&mut q, &o, &files, 16);
        assert!((4..8).contains(&q.pick_piece(&all(12)).unwrap()));
    }

    #[test]
    fn a_piece_shared_by_two_files_takes_the_higher_priority() {
        // Piece 1 holds the end of file 0 and the start of file 1.
        let files = [file(0, 24), file(24, 40)];
        let mut p = picker_with_avail(&[1, 9, 1, 1]);
        let mut o = DownloadOrder::default();
        o.set_file_priority(0, 2, PRIO_MAX);
        ordered(&mut p, &o, &files, 16);
        // File 0 spans pieces 0 and 1: 0 is rarer, then 1 despite its 9 copies.
        assert_eq!(p.pick_piece(&all(4)), Some(0));
        p.set_have(0);
        assert_eq!(p.pick_piece(&all(4)), Some(1));
    }

    #[test]
    fn first_and_last_piece_of_each_file_come_first() {
        let files = [file(0, 10 * 16), file(160, 10 * 16)];
        // The middle pieces are the rarest.
        let mut avail = vec![1u16; 20];
        for (i, a) in avail.iter_mut().enumerate() {
            if [0, 9, 10, 19].contains(&i) {
                *a = 8;
            }
        }
        let mut p = picker_with_avail(&avail);
        let o = DownloadOrder { first_last: true, ..Default::default() };
        ordered(&mut p, &o, &files, 16);
        let mut got = Vec::new();
        for _ in 0..4 {
            let pick = p.pick_piece(&all(20)).unwrap();
            p.begin_piece_for_test(pick, 16, 16);
            got.push(pick);
        }
        got.sort();
        assert_eq!(got, vec![0, 9, 10, 19]);
        // With sequential, the ends first, then in order.
        let mut q = PiecePicker::new(20);
        ordered(&mut q, &DownloadOrder { sequential: true, first_last: true, ..Default::default() }, &files, 16);
        let mut seq = Vec::new();
        for _ in 0..6 {
            let pick = q.pick_piece(&all(20)).unwrap();
            q.begin_piece_for_test(pick, 16, 16);
            seq.push(pick);
        }
        assert_eq!(seq, vec![0, 9, 10, 19, 1, 2]);
    }

    /// Priority 0 does not exist here: nothing can make a file be skipped.
    #[test]
    fn priority_zero_is_never_stored() {
        let mut o = DownloadOrder::default();
        o.set_file_priority(0, 2, 0);
        assert!(o.is_default());
        o.set_file_priority(1, 2, PRIO_HIGH);
        o.set_file_priority(1, 2, PRIO_NORMAL);
        assert!(o.file_prio.is_empty(), "back to all-normal is back to nothing stored");
    }
}
