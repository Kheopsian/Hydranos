use std::collections::HashMap;

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

/// Rarest-first piece picker for download mode.
pub struct PiecePicker {
    num_pieces: u32,
    have: Vec<bool>,           // pieces we have
    availability: Vec<u16>,    // how many peers have each piece (u16: le swarm ne depasse jamais 65535 copies)
    pending: HashMap<u32, PendingPiece>,  // piece index -> in-flight requests
    /// The torrent's download order, when it is not the default. Boxed so the
    /// default costs one null pointer, and checked once per pick.
    order: Option<Box<PickOrder>>,
}

struct PendingPiece {
    blocks_requested: Vec<bool>,
    blocks_received: Vec<bool>,
    data: Vec<u8>,
    block_size: u32,
    piece_size: u32,
}

impl PiecePicker {
    pub fn new(num_pieces: u32) -> Self {
        Self {
            num_pieces,
            have: vec![false; num_pieces as usize],
            availability: vec![0; num_pieces as usize],
            pending: HashMap::new(),
            order: None,
        }
    }

    /// Install (or, with `None`, drop) the torrent's download order.
    pub fn set_order(&mut self, order: Option<Box<PickOrder>>) {
        self.order = order;
    }

    /// Is a non-default order installed?
    pub fn has_order(&self) -> bool {
        self.order.is_some()
    }

    /// Mark a piece as completed.
    /// Clear all have bits. Used before a recheck re-derives ownership
    /// from disk: without it set_have could only add pieces, never detect
    /// a piece that has since become corrupt/missing.
    pub fn reset_have(&mut self) {
        for h in self.have.iter_mut() {
            *h = false;
        }
    }

    pub fn set_have(&mut self, index: u32) {
        if (index as usize) < self.have.len() {
            self.have[index as usize] = true;
            self.pending.remove(&index);
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

    /// Release a piece from `pending` so another peer can pick it.
    /// Called on peer disconnect and on hash-mismatch/write-error, so
    /// abandoned in-flight pieces don't leak and stall the DL.
    pub fn cancel_piece(&mut self, piece: u32) {
        if !self.have.get(piece as usize).copied().unwrap_or(false) {
            self.pending.remove(&piece);
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

    /// Generate block requests for a piece.
    /// Returns (piece_index, offset, length) tuples.
    pub fn start_piece(&mut self, piece: u32, piece_size: u32, block_size: u32) -> Vec<(u32, u32, u32)> {
        let num_blocks = (piece_size + block_size - 1) / block_size;
        let mut requests = Vec::new();

        for i in 0..num_blocks {
            let offset = i * block_size;
            let len = std::cmp::min(block_size, piece_size - offset);
            requests.push((piece, offset, len));
        }

        self.pending.insert(piece, PendingPiece {
            blocks_requested: vec![true; num_blocks as usize],
            blocks_received: vec![false; num_blocks as usize],
            data: vec![0u8; piece_size as usize],
            block_size,
            piece_size,
        });

        requests
    }

    /// Record a received block. Returns true if the piece is now complete.
    pub fn receive_block(&mut self, piece: u32, offset: u32, data: &[u8]) -> bool {
        if let Some(pending) = self.pending.get_mut(&piece) {
            // A block counts as received only once its bytes are actually in
            // the buffer, and only if it is the block it claims to be.
            //
            // Marking the slot before the bounds check let a short, overlong or
            // unaligned block complete a piece with a hole of zeros in it. The
            // assembled buffer then failed SHA1 in write_piece, the piece was
            // re-requested, and with a large peer set that became a permanent
            // re-download loop: ~24% of completed pieces were being discarded.
            if pending.block_size == 0 || offset % pending.block_size != 0 {
                return false;
            }
            let block_idx = (offset / pending.block_size) as usize;
            if block_idx >= pending.blocks_received.len() {
                return false;
            }
            let expected_len =
                std::cmp::min(pending.block_size, pending.piece_size.saturating_sub(offset))
                    as usize;
            if data.len() != expected_len {
                return false;
            }
            let start = offset as usize;
            let end = start + data.len();
            if end > pending.data.len() {
                return false;
            }
            pending.data[start..end].copy_from_slice(data);
            pending.blocks_received[block_idx] = true;
            // Check if all blocks received
            pending.blocks_received.iter().all(|&b| b)
        } else {
            false
        }
    }

    /// Get the assembled piece data (after receive_block returned true).
    pub fn take_piece_data(&mut self, piece: u32) -> Option<Vec<u8>> {
        self.pending.remove(&piece).map(|p| p.data)
    }

    #[cfg(test)]
    pub fn begin_piece_for_test(&mut self, piece: u32, piece_size: u32, block_size: u32) {
        let num_blocks = piece_size.div_ceil(block_size);
        self.pending.insert(piece, PendingPiece {
            blocks_requested: vec![true; num_blocks as usize],
            blocks_received: vec![false; num_blocks as usize],
            data: vec![0u8; piece_size as usize],
            block_size,
            piece_size,
        });
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
    /// another source already holds: calling `start_piece` on a pending piece
    /// resets its `blocks_received`, throwing away whatever a peer had already
    /// delivered for it.
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
    use super::PiecePicker;

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
        assert!(!p.receive_block(0, 0, &[1u8; 16]));
        // second block arrives truncated -> refused, piece stays incomplete
        assert!(!p.receive_block(0, 16, &[2u8; 4]));
        // the same block at its real length completes the piece
        assert!(p.receive_block(0, 16, &[2u8; 16]));
    }

    /// An unaligned offset addresses no block slot and must be refused.
    #[test]
    fn unaligned_offset_is_refused() {
        let mut p = picker_with_one_pending(32, 16);
        assert!(!p.receive_block(0, 5, &[1u8; 16]));
        assert!(!p.receive_block(0, 0, &[1u8; 16]));
        assert!(p.receive_block(0, 16, &[2u8; 16]));
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
        assert!(!p.receive_block(0, 0, &[1u8; 16]));
        assert!(p.receive_block(0, 16, &[2u8; 4]));
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
