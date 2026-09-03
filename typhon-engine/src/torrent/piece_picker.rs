use std::collections::HashMap;

/// Rarest-first piece picker for download mode.
pub struct PiecePicker {
    num_pieces: u32,
    have: Vec<bool>,           // pieces we have
    availability: Vec<u16>,    // how many peers have each piece (u16: le swarm ne depasse jamais 65535 copies)
    pending: HashMap<u32, PendingPiece>,  // piece index -> in-flight requests
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
        }
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
