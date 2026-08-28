//! Download logic for a peer connection.
//! Manages piece requesting, block assembly, and completion.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use bytes::Bytes;
use tracing::info;

use crate::disk::DiskManager;
use crate::torrent::meta::{TorrentState, TorrentStatus};
use crate::torrent::piece_picker::PiecePicker;

const BLOCK_SIZE: u32 = 16384;
const MAX_PIPELINE: usize = 32; // max in-flight requests (was 16; bumped — MAX_PIPELINE=32 doubles the in-flight window without overwhelming the picker mutex on multi-peer scenarios)

pub struct DownloadState {
    torrent: Arc<TorrentState>,
    disk: Arc<DiskManager>,
    peer_bitfield: Vec<u8>,
    pub am_interested: bool,
    pub peer_choking: bool,
    pending_requests: usize,
    /// FIFO of blocks already picked from the picker (via start_piece) but not
    /// yet emitted on the wire. Emptied by get_requests as pipeline budget
    /// allows. Required because start_piece returns ALL blocks of a piece at
    /// once (typically ~128 for 2MB pieces) but MAX_PIPELINE caps how many we
    /// can have in flight. Previously the remainder was silently discarded —
    /// pieces never completed on large torrents (only tiny ones with <16
    /// blocks fit in one pipeline).
    pending_block_queue: VecDeque<(u32, u32, u32)>,
    /// Pieces this peer has `start_piece`'d on the shared picker. Released
    /// via `cancel_piece` on disconnect so abandoned in-flight pieces don't
    /// stay stuck in `pending` forever (was the DL-stall-at-N% bug).
    started_pieces: HashSet<u32>,
}

impl DownloadState {
    pub fn new(torrent: Arc<TorrentState>, disk: Arc<DiskManager>) -> Self {
        Self {
            torrent,
            disk,
            peer_bitfield: Vec::new(),
            am_interested: false,
            peer_choking: true,
            pending_requests: 0,
            pending_block_queue: VecDeque::new(),
            started_pieces: HashSet::new(),
        }
    }

    pub fn is_downloading(&self) -> bool {
        self.torrent.picker.get().is_some() &&
        self.torrent.status.load(Ordering::Relaxed) == TorrentStatus::Downloading as u8
    }

    /// Process incoming Bitfield from peer.
    pub fn on_bitfield(&mut self, data: &[u8]) {
        self.peer_bitfield = data.to_vec();
        if let Some(picker) = self.torrent.picker.get() {
            picker.lock().unwrap().add_bitfield(data);
        }
    }

    /// Process incoming HaveAll from peer.
    pub fn on_have_all(&mut self) {
        let num_pieces = self.torrent.meta.num_pieces() as usize;
        let byte_len = (num_pieces + 7) / 8;
        self.peer_bitfield = vec![0xFF; byte_len];
        // Fix trailing bits
        let trailing = num_pieces % 8;
        if trailing > 0 {
            self.peer_bitfield[byte_len - 1] = 0xFF << (8 - trailing);
        }
        if let Some(picker) = self.torrent.picker.get() {
            picker.lock().unwrap().add_bitfield(&self.peer_bitfield);
        }
    }

    /// Process incoming Have from peer. Returns true iff the piece bit was
    /// NEWLY set. The caller gates num_pieces_have on this so a duplicate /
    /// redundant Have (peer re-announcing a piece already in its bitfield)
    /// can no longer inflate progress past 100% (the 200%-in-peerlist bug).
    pub fn on_have(&mut self, piece: u32) -> bool {
        // Ignore Have for an out-of-range piece index (buggy/malicious peer).
        if (piece as usize) >= self.torrent.meta.num_pieces() as usize {
            return false;
        }
        let byte_idx = piece as usize / 8;
        let bit_idx = 7 - (piece % 8);
        if byte_idx >= self.peer_bitfield.len() {
            self.peer_bitfield.resize(byte_idx + 1, 0);
        }
        let mask = 1u8 << bit_idx;
        if self.peer_bitfield[byte_idx] & mask != 0 {
            return false; // already known -> do not double-count
        }
        self.peer_bitfield[byte_idx] |= mask;
        if let Some(picker) = self.torrent.picker.get() {
            picker.lock().unwrap().add_have(piece);
        }
        true
    }

    pub fn on_unchoke(&mut self) {
        self.peer_choking = false;
    }

    pub fn on_choke(&mut self) {
        self.peer_choking = true;
        self.pending_requests = 0;
    }

    /// Check if we should send Interested to this peer.
    pub fn should_be_interested(&self) -> bool {
        if !self.is_downloading() { return false; }
        if let Some(picker) = self.torrent.picker.get() {
            let p = picker.lock().unwrap();
            p.pick_piece(&self.peer_bitfield).is_some()
        } else {
            false
        }
    }

    /// Generate Request messages to send. Returns vec of (piece, offset, length).
    ///
    /// Drains `pending_block_queue` first (blocks already picked but not yet
    /// on the wire), then picks a new piece when the queue runs dry. A piece's
    /// blocks all go into the queue in order, so we always finish requesting
    /// one piece before starting another. This matters for torrents where
    /// num_blocks_per_piece > MAX_PIPELINE (typical: 128 blocks for 2MB
    /// piece) — otherwise only the first 16 blocks were ever requested and
    /// pieces never completed.
    pub fn get_requests(&mut self) -> Vec<(u32, u32, u32)> {
        if !self.is_downloading() || self.peer_choking {
            return Vec::new();
        }

        let picker = match self.torrent.picker.get() {
            Some(p) => p,
            None => return Vec::new(),
        };

        let mut requests = Vec::new();
        let mut picker = picker.lock().unwrap();

        while self.pending_requests < MAX_PIPELINE {
            // Drain queue first.
            if let Some(req) = self.pending_block_queue.pop_front() {
                requests.push(req);
                self.pending_requests += 1;
                continue;
            }
            // Queue empty — pick the next piece.
            let piece = match picker.pick_piece(&self.peer_bitfield) {
                Some(p) => p,
                None => break,
            };
            let piece_size = self.torrent.meta.piece_size(piece);
            let piece_requests = picker.start_piece(piece, piece_size, BLOCK_SIZE);
            self.pending_block_queue.extend(piece_requests);
            self.started_pieces.insert(piece);
        }

        requests
    }

    /// Process received Piece data. Returns Some(piece_index) if a piece completed.
    pub async fn on_piece(&mut self, index: u32, begin: u32, data: &[u8]) -> Option<u32> {
        self.pending_requests = self.pending_requests.saturating_sub(1);

        // Skip if torrent was removed mid-flight — otherwise write_piece
        // (create=true) would recreate the files we just deleted.
        if self.torrent.is_removed.load(Ordering::Relaxed) {
            return None;
        }

        let picker = match self.torrent.picker.get() {
            Some(p) => p,
            None => return None,
        };

        let complete = {
            let mut p = picker.lock().unwrap();
            p.receive_block(index, begin, data)
        };

        if complete {
            let piece_data = {
                let mut p = picker.lock().unwrap();
                p.take_piece_data(index)
            };

            if let Some(piece_data) = piece_data {
                let len = piece_data.len() as u64;
                // Write to disk with SHA1 verification
                match self.disk.write_piece(&self.torrent, index, piece_data).await {
                    Ok(true) => {
                        // SHA1 verified, piece complete
                        self.torrent.total_downloaded.fetch_add(len, Ordering::Relaxed);
                        self.started_pieces.remove(&index);
                        {
                            let mut p = picker.lock().unwrap();
                            p.set_have(index);
                        }
                        // Broadcast Have to all peers
                        if let Some(tx) = &self.torrent.have_tx { tx.send(index).ok(); }

                        // Check if download is complete
                        let is_complete = {
                            let p = picker.lock().unwrap();
                            p.is_complete()
                        };
                        if is_complete {
                            info!("[download] {} complete!", crate::torrent::hex_encode(&self.torrent.info_hash)[..8].to_string());
                            self.torrent.status.store(TorrentStatus::Seeding as u8, Ordering::Relaxed);
                            // Nothing will verify this torrent again unless the
                            // user asks for a recheck, so give the hash table back.
                            self.torrent.release_piece_hashes();
                            self.torrent.completed_time.store(
                                std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_secs() as i64,
                                Ordering::Relaxed,
                            );
                        }

                        return Some(index);
                    }
                    Ok(false) => {
                        // SHA1 mismatch — release so another peer can retry.
                        tracing::warn!("[download] piece {} SHA1 mismatch, re-requesting", index);
                        self.started_pieces.remove(&index);
                        picker.lock().unwrap().cancel_piece(index);
                    }
                    Err(e) => {
                        tracing::warn!("[download] piece {} write failed: {}", index, e);
                        self.started_pieces.remove(&index);
                        picker.lock().unwrap().cancel_piece(index);
                    }
                }
            }
        }
        None
    }

    /// Cleanup when peer disconnects. Releases pieces this peer had started
    /// but not finished, so other peers can pick them up. Without this, any
    /// piece in `started_pieces` leaks into the picker's `pending` map and
    /// is never re-picked — the torrent stalls near completion.
    pub fn on_disconnect(&mut self) {
        if let Some(picker) = self.torrent.picker.get() {
            let mut p = picker.lock().unwrap();
            if !self.peer_bitfield.is_empty() {
                p.remove_bitfield(&self.peer_bitfield);
            }
            for piece in self.started_pieces.drain() {
                p.cancel_piece(piece);
            }
        }
    }
}
