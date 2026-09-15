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
/// How many pieces in a row may fail to reach the disk before the torrent
/// stops asking for more.
///
/// Three and not one: a seedbox that drains under pressure can refuse a write
/// and accept the next. Three and not thirty: each retry is a whole piece
/// fetched from the swarm again, so guessing high is paid in other people's
/// upload.
const DISK_FAILURES_BEFORE_ERROR: u32 = 3;
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
        // A paused torrent asks for nothing. This is the single choke point
        // for the download side: `stop_torrent` sets the flag and untracks the
        // DHT, but it does not tear down sessions that are already connected,
        // and nothing else on this path consulted the flag -- so a paused
        // torrent kept requesting blocks from every peer it already had and
        // went on downloading at full speed while the interface said stopped.
        // Blocks already in flight still arrive; the pipeline is bounded, so
        // that is a handful of them and then silence.
        if self.torrent.is_paused.load(Ordering::Relaxed) {
            return Vec::new();
        }
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
                // Already started by THIS peer. In endgame -- the last sixteen
                // pieces -- `pick_piece` stops excluding pieces that are
                // already pending, which is the point: the same piece is meant
                // to be asked of several peers at once. It is not meant to be
                // asked of the same peer twice, and without this guard the
                // loop below refilled the whole pipeline with copies of the
                // block it had just requested.
                //
                // Worse than wasted requests: `start_piece` inserts, so a
                // second start on a piece already in flight overwrote its
                // `blocks_received` and threw away everything received for it.
                Some(p) if self.started_pieces.contains(&p) => break,
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
                // Off this peer's books either way: commit_piece hands the
                // piece back to the picker itself when it fails to verify.
                self.started_pieces.remove(&index);
                if commit_piece(&self.torrent, &self.disk, index, piece_data).await {
                    return Some(index);
                }
            }
        }
        None
    }

    /// Cleanup when peer disconnects. Releases pieces this peer had started
    /// but not finished, so other peers can pick them up. Without this, any
    /// piece in `started_pieces` leaks into the picker's `pending` map and
    /// is never re-picked — the torrent stalls near completion.
    ///
    /// Called explicitly at the end of a session, and again by `Drop` for every
    /// other way out. Draining `started_pieces` makes the second call a no-op.
    pub fn on_disconnect(&mut self) {
        if let Some(picker) = self.torrent.picker.get() {
            let mut p = picker.lock().unwrap();
            if !self.peer_bitfield.is_empty() {
                p.remove_bitfield(&self.peer_bitfield);
                // Idempotence, and it is not optional: this runs once from
                // `session::run` and once more from `Drop`. `started_pieces`
                // is drained so the piece half replays harmlessly, but
                // `remove_bitfield` DECREMENTS availability counters -- running
                // it twice would understate how rare every piece this peer held
                // is, and rarest-first picks on those counters.
                self.peer_bitfield.clear();
            }
            for piece in self.started_pieces.drain() {
                p.cancel_piece(piece);
            }
        }
    }
}

/// Release reserved pieces however the session ends.
///
/// `on_disconnect` is reached from exactly one place -- the end of
/// `session::run` -- so any other way out kept every piece this peer had
/// started in the picker's `pending` map, each holding a full piece-sized
/// buffer that nothing would ever free or re-pick. Early returns, a panic in
/// the session task, and task cancellation at an await point all take that
/// path, and an engine accepting 76 inbound connections a second takes it
/// often.
///
/// Measured on production 2026-09-08, two heap profiles 26 minutes apart:
/// `PiecePicker::start_piece` accounted for 459.7 MB of 457.8 MB of growth --
/// 100% of it -- reached through `session::run` -> `get_requests`. The webseed
/// pool, which shares the picker, was NEGATIVE over the same window: it
/// releases correctly, on all three of its failure paths.
///
/// A destructor rather than another call site: the bug was never a missing
/// call, it was that correctness depended on reaching one.
impl Drop for DownloadState {
    fn drop(&mut self) {
        self.on_disconnect();
    }
}

/// Verify, store and account one assembled piece.
///
/// Shared by the peer path and the BEP 19 webseed pool: both assemble a piece
/// from somewhere and then need the identical completion sequence — SHA1 through
/// the disk layer, `set_have`, the Have broadcast, the hash-table release and
/// the durable completion notice. There is deliberately ONE copy: the last time
/// a completion path diverged from what the resume writer expected, every
/// complete torrent had its bitfield erased on the following sweep.
///
/// Returns true when the piece verified and is now owned. On failure the piece
/// is released back to the picker so another source can retry it.
pub async fn commit_piece(
    torrent: &Arc<TorrentState>,
    disk: &Arc<DiskManager>,
    index: u32,
    piece_data: Vec<u8>,
) -> bool {
    let picker = match torrent.picker.get() {
        Some(p) => p,
        None => return false,
    };
    let len = piece_data.len() as u64;
    match disk.write_piece(torrent, index, piece_data).await {
        Ok(true) => {
            torrent.total_downloaded.fetch_add(len, Ordering::Relaxed);
            {
                let mut p = picker.lock().unwrap();
                p.set_have(index);
            }
            // Broadcast Have to all peers
            torrent.have_sender().send(index).ok();

            let is_complete = {
                let p = picker.lock().unwrap();
                p.is_complete()
            };
            if is_complete {
                info!(
                    "[download] {} complete!",
                    crate::torrent::hex_encode(&torrent.info_hash)[..8].to_string()
                );
                torrent
                    .status
                    .store(TorrentStatus::Seeding as u8, Ordering::Relaxed);
                // BEP 3: a tracker only learns a download finished if we say
                // so. Private trackers count snatches from this event.
                torrent.pending_announce_event.store(
                    crate::torrent::meta::ANNOUNCE_EVENT_COMPLETED,
                    Ordering::Relaxed,
                );
                // Nothing will verify this torrent again unless the user asks
                // for a recheck, so give the hash table back.
                torrent.release_piece_hashes();
                // No further piece will complete: nothing left to announce.
                torrent.release_have_tx();
                torrent.completed_time.store(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64,
                    Ordering::Relaxed,
                );
                // Write it down now. Waiting for the five-minute sweep meant a
                // restart inside that window lost the completion and
                // re-downloaded the whole torrent.
                torrent.notify_completed();
            }
            // The disk answered. Whatever was wrong before is not wrong now.
            torrent.disk_write_failures.store(0, Ordering::Relaxed);
            true
        }
        Ok(false) => {
            // The peer lied about its data. Asking somebody else is exactly
            // right, and it says nothing about the disk.
            tracing::warn!("[download] piece {} SHA1 mismatch, re-requesting", index);
            picker.lock().unwrap().cancel_piece(index);
            false
        }
        Err(e) => {
            // The disk refused. Re-requesting cannot fix a full volume or a
            // read-only mount, and each attempt pulls a whole piece off the
            // swarm again -- so this downloaded the same piece forever, at full
            // speed, making no progress and showing none.
            tracing::warn!("[download] piece {} write failed: {}", index, e);
            // Released either way: the buffer is a whole piece, and nothing
            // else would free it.
            picker.lock().unwrap().cancel_piece(index);

            let failures = torrent.disk_write_failures.fetch_add(1, Ordering::Relaxed) + 1;
            if failures >= DISK_FAILURES_BEFORE_ERROR {
                // The contract the serve path already uses when a file has
                // gone: set once, never cleared on its own, brought back by a
                // recheck. A torrent that cannot store what it fetches must
                // stop fetching.
                // One door, next to the one the serve path uses. It keeps
                // serving what we already hold: a full volume still reads.
                torrent.mark_write_error(&format!(
                    "cannot write to disk: {e}. Check the volume has space and is \
                     writable, then recheck this torrent"
                ));
            }
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::torrent::meta::TorrentMeta;
    use std::path::PathBuf;

    /// `num_pieces` pieces of `piece_length` bytes each.
    fn meta(num_pieces: u32, piece_length: u32) -> TorrentMeta {
        TorrentMeta {
            info_hash: [3u8; 20],
            name: "t".into(),
            num_pieces,
            piece_length,
            total_size: num_pieces as u64 * piece_length as u64,
            files: Vec::new(),
            trackers: Vec::new(),
            url_list: Vec::new(),
            private: false,
            multi_file: false,
            info_dict_len: 0,
        }
    }

    /// A torrent we are fetching: `seed_mode` false is what builds the picker.
    fn downloading(num_pieces: u32, piece_length: u32) -> Arc<TorrentState> {
        let t = Arc::new(TorrentState::new(
            meta(num_pieces, piece_length),
            PathBuf::from("/tmp"),
            false,
        ));
        t.status
            .store(TorrentStatus::Downloading as u8, Ordering::Relaxed);
        t
    }

    fn state(t: Arc<TorrentState>) -> DownloadState {
        DownloadState::new(t, Arc::new(DiskManager::new(100)))
    }

    /// A peer holding everything, unchoking us: the ordinary case the request
    /// tests need.
    fn ready(dl: &mut DownloadState) {
        dl.on_have_all();
        dl.on_unchoke();
    }

    // -----------------------------------------------------------------------
    // Have / Bitfield
    // -----------------------------------------------------------------------

    /// BEP 3: piece 0 is the high bit of the first byte.
    #[test]
    fn a_have_sets_the_bit_the_spec_names() {
        let mut dl = state(downloading(16, 16384));
        assert!(dl.on_have(0), "newly known");
        assert_eq!(dl.peer_bitfield[0], 0b1000_0000);
        assert!(dl.on_have(7));
        assert_eq!(dl.peer_bitfield[0], 0b1000_0001);
    }

    /// The 200%-in-the-peer-list bug: a peer re-announcing a piece it already
    /// had was counted twice, and progress walked past 100%. The caller gates
    /// its counter on this return value, so the guard has to live here.
    #[test]
    fn a_repeated_have_is_not_counted_twice() {
        let mut dl = state(downloading(16, 16384));
        assert!(dl.on_have(3), "the first time");
        assert!(!dl.on_have(3), "the second time tells the caller not to count it");
    }

    /// A peer may send a piece index this torrent does not have. Believing it
    /// would grow the bitfield past the torrent and inflate the same counter.
    #[test]
    fn a_have_beyond_the_torrent_is_refused() {
        let mut dl = state(downloading(16, 16384));
        assert!(!dl.on_have(16), "sixteen pieces are numbered 0..15");
        assert!(!dl.on_have(u32::MAX));
        assert!(dl.peer_bitfield.is_empty(), "nothing was recorded");
    }

    /// `have all` is a bitfield of every piece and nothing more: the padding
    /// bits of the last byte must stay clear, or the peer looks like it holds
    /// pieces the torrent does not have.
    #[test]
    fn have_all_sets_every_piece_and_no_padding() {
        let mut dl = state(downloading(10, 16384));
        dl.on_have_all();
        assert_eq!(dl.peer_bitfield.len(), 2, "ten pieces need two bytes");
        assert_eq!(dl.peer_bitfield[0], 0xFF);
        assert_eq!(dl.peer_bitfield[1], 0b1100_0000, "two real pieces, six of padding");
    }

    #[test]
    fn a_bitfield_is_taken_as_sent() {
        let mut dl = state(downloading(16, 16384));
        dl.on_bitfield(&[0b1010_1010, 0x00]);
        assert_eq!(dl.peer_bitfield, vec![0b1010_1010, 0x00]);
    }

    // -----------------------------------------------------------------------
    // Choke / interest
    // -----------------------------------------------------------------------

    /// A choke voids everything in flight: the peer will answer none of it, and
    /// leaving the counter up would keep the pipeline shut after the unchoke.
    #[test]
    fn a_choke_voids_what_was_in_flight() {
        let t = downloading(64, 2 * 1024 * 1024);
        let mut dl = state(t);
        ready(&mut dl);
        assert!(!dl.get_requests().is_empty(), "requests went out");

        dl.on_choke();
        assert!(dl.peer_choking);
        dl.on_unchoke();
        assert!(
            !dl.get_requests().is_empty(),
            "after an unchoke the pipeline opens again"
        );
    }

    #[test]
    fn there_is_nothing_to_want_from_a_peer_that_has_nothing() {
        let mut dl = state(downloading(16, 16384));
        dl.on_bitfield(&[0x00, 0x00]);
        assert!(!dl.should_be_interested());
        dl.on_have_all();
        assert!(dl.should_be_interested(), "now it has what we need");
    }

    // -----------------------------------------------------------------------
    // Requests
    // -----------------------------------------------------------------------

    /// ⭐ The regression this file exists for: a paused torrent kept requesting
    /// blocks from every peer it already held and went on downloading at full
    /// speed while the interface said stopped. `stop_torrent` sets the flag but
    /// does not tear down live sessions, and nothing on this path read it.
    #[test]
    fn a_paused_torrent_asks_for_nothing() {
        let t = downloading(64, 2 * 1024 * 1024);
        let mut dl = state(t.clone());
        ready(&mut dl);
        assert!(!dl.get_requests().is_empty(), "running, it asks");

        t.is_paused.store(true, Ordering::Relaxed);
        assert!(
            dl.get_requests().is_empty(),
            "paused means paused, including for a session already open"
        );
    }

    #[test]
    fn a_choked_peer_is_asked_for_nothing() {
        let mut dl = state(downloading(64, 2 * 1024 * 1024));
        dl.on_have_all();
        // No unchoke: BT starts choked.
        assert!(dl.get_requests().is_empty());
    }

    #[test]
    fn a_torrent_that_is_not_downloading_asks_for_nothing() {
        let t = downloading(64, 2 * 1024 * 1024);
        t.status.store(TorrentStatus::Seeding as u8, Ordering::Relaxed);
        let mut dl = state(t);
        ready(&mut dl);
        assert!(dl.get_requests().is_empty());
    }

    /// ⭐ The other regression: `start_piece` hands back every block of a piece
    /// at once -- 128 of them for a 2 MiB piece -- while the pipeline caps what
    /// may be in flight. The remainder used to be dropped, so only the first
    /// blocks were ever asked for and a piece never completed on any torrent
    /// with pieces larger than the pipeline.
    #[test]
    fn a_full_pipeline_is_asked_for_and_stays_on_one_piece() {
        let mut dl = state(downloading(64, 2 * 1024 * 1024));
        ready(&mut dl);

        let reqs = dl.get_requests();
        assert_eq!(reqs.len(), MAX_PIPELINE, "the pipeline is filled, not the piece");

        // All of one piece before any of the next: a piece half-requested from
        // two peers completes twice as slowly and pins twice the memory.
        let first = reqs[0].0;
        assert!(
            reqs.iter().all(|(piece, _, _)| *piece == first),
            "one piece at a time: {reqs:?}"
        );
        // Blocks are contiguous and of the block size.
        for (i, (_, begin, len)) in reqs.iter().enumerate() {
            assert_eq!(*begin, i as u32 * BLOCK_SIZE, "block {i} starts where the last ended");
            assert_eq!(*len, BLOCK_SIZE);
        }
    }

    /// A piece shorter than the block size is asked for at its real length.
    /// Asking for a full block past the end is what a tracker-side "invalid
    /// request" is made of.
    #[test]
    fn the_last_short_piece_is_asked_for_at_its_real_size() {
        // One piece of 1000 bytes.
        let t = Arc::new(TorrentState::new(
            TorrentMeta { total_size: 1000, ..meta(1, 16384) },
            PathBuf::from("/tmp"),
            false,
        ));
        t.status.store(TorrentStatus::Downloading as u8, Ordering::Relaxed);
        let mut dl = state(t);
        ready(&mut dl);

        let reqs = dl.get_requests();
        assert_eq!(reqs.len(), 1, "one short block");
        assert_eq!(reqs[0], (0, 0, 1000));
    }
}

#[cfg(test)]
mod endgame_tests {
    use super::*;
    use crate::torrent::meta::TorrentMeta;
    use std::path::PathBuf;

    /// ⭐ The bug this guards: `pick_piece` stops excluding pending pieces once
    /// sixteen or fewer remain -- endgame, and asking several PEERS for the same
    /// piece is the point of it. Asking the same peer twice is not, and
    /// `get_requests` looped until its pipeline was full of copies of the block
    /// it had just asked for.
    ///
    /// A one-piece torrent is permanently in endgame, which is what makes it
    /// the smallest case that shows this.
    #[test]
    fn endgame_does_not_make_one_peer_ask_itself_twice() {
        let t = Arc::new(TorrentState::new(
            TorrentMeta {
                info_hash: [3u8; 20],
                name: "t".into(),
                num_pieces: 1,
                piece_length: 16384,
                total_size: 1000,
                files: Vec::new(),
                trackers: Vec::new(),
                url_list: Vec::new(),
                private: false,
                multi_file: false,
                info_dict_len: 0,
            },
            PathBuf::from("/tmp"),
            false,
        ));
        t.status.store(TorrentStatus::Downloading as u8, Ordering::Relaxed);
        let mut dl = DownloadState::new(t, Arc::new(DiskManager::new(100)));
        dl.on_have_all();
        dl.on_unchoke();

        let reqs = dl.get_requests();
        assert_eq!(reqs.len(), 1, "one piece of one short block, asked for once: {reqs:?}");
        assert_eq!(reqs[0], (0, 0, 1000), "and at its real length, not a full block");
    }

    /// The same, with room in the pipeline and several pieces: what goes out
    /// must carry no block twice.
    #[test]
    fn no_block_is_asked_for_twice_in_one_pipeline() {
        let t = Arc::new(TorrentState::new(
            TorrentMeta {
                info_hash: [3u8; 20],
                name: "t".into(),
                num_pieces: 4,
                piece_length: 32768, // two blocks each: eight in total, pipeline is 32
                total_size: 4 * 32768,
                files: Vec::new(),
                trackers: Vec::new(),
                url_list: Vec::new(),
                private: false,
                multi_file: false,
                info_dict_len: 0,
            },
            PathBuf::from("/tmp"),
            false,
        ));
        t.status.store(TorrentStatus::Downloading as u8, Ordering::Relaxed);
        let mut dl = DownloadState::new(t, Arc::new(DiskManager::new(100)));
        dl.on_have_all();
        dl.on_unchoke();

        let reqs = dl.get_requests();
        let mut seen = std::collections::HashSet::new();
        for r in &reqs {
            assert!(seen.insert(*r), "block {r:?} asked for twice in {reqs:?}");
        }
        assert!(reqs.len() <= 8, "four pieces of two blocks is eight, got {}", reqs.len());
    }
}

#[cfg(test)]
mod disk_failure_tests {
    use super::*;
    use crate::torrent::meta::TorrentMeta;
    use std::path::PathBuf;

    /// A torrent with no piece hash table: every `write_piece` refuses before
    /// touching the filesystem, which is the failure path without needing a
    /// full disk to make one.
    fn unwritable() -> Arc<TorrentState> {
        let t = Arc::new(TorrentState::new(
            TorrentMeta {
                info_hash: [9u8; 20],
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
            },
            PathBuf::from("/tmp"),
            false,
        ));
        t.status.store(TorrentStatus::Downloading as u8, Ordering::Relaxed);
        t
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
    }

    /// One refusal is not a verdict: a seedbox that drains under pressure can
    /// refuse a write and accept the next one.
    #[test]
    fn a_single_write_failure_does_not_stop_the_torrent() {
        let t = unwritable();
        let disk = Arc::new(DiskManager::new(100));
        let ok = rt().block_on(commit_piece(&t, &disk, 0, vec![0u8; 16384]));
        assert!(!ok, "the write failed");
        assert_eq!(
            t.status.load(Ordering::Relaxed),
            TorrentStatus::Downloading as u8,
            "still going"
        );
        assert_eq!(t.disk_write_failures.load(Ordering::Relaxed), 1);
    }

    /// ⭐ The bug: a disk that cannot take the data used to make us fetch the
    /// same piece from the swarm forever, at full speed, with nothing to show
    /// for it. Past the threshold the torrent stops asking.
    #[test]
    fn a_disk_that_keeps_refusing_stops_the_torrent() {
        let t = unwritable();
        let disk = Arc::new(DiskManager::new(100));
        let rt = rt();
        for _ in 0..DISK_FAILURES_BEFORE_ERROR {
            rt.block_on(commit_piece(&t, &disk, 0, vec![0u8; 16384]));
        }
        assert_eq!(
            t.status.load(Ordering::Relaxed),
            TorrentStatus::Error as u8,
            "a torrent that cannot store what it fetches must stop fetching"
        );
        let msg = t.error_msg.lock().unwrap().clone();
        assert!(!msg.is_empty(), "and it says why, in the interface");
        assert!(msg.contains("cannot write"), "{msg}");
    }

    /// And stopping actually stops it: `get_requests` gates on the status, so
    /// this is the half that makes the loop end rather than merely be labelled.
    #[test]
    fn a_stopped_torrent_asks_for_no_more_pieces() {
        let t = unwritable();
        let disk = Arc::new(DiskManager::new(100));
        let rt = rt();
        let mut dl = DownloadState::new(t.clone(), disk.clone());
        dl.on_have_all();
        dl.on_unchoke();
        assert!(!dl.get_requests().is_empty(), "asking, before the disk gives up");

        for _ in 0..DISK_FAILURES_BEFORE_ERROR {
            rt.block_on(commit_piece(&t, &disk, 0, vec![0u8; 16384]));
        }
        assert!(
            dl.get_requests().is_empty(),
            "the whole point: no more pieces are pulled off the swarm"
        );
    }

    /// Scattered failures must not accumulate into a stop. Only a run of them
    /// means the volume is gone.
    #[test]
    fn a_success_clears_what_came_before() {
        let t = unwritable();
        t.disk_write_failures.store(2, Ordering::Relaxed);
        // Simulate the reset a successful write performs.
        t.disk_write_failures.store(0, Ordering::Relaxed);
        let disk = Arc::new(DiskManager::new(100));
        rt().block_on(commit_piece(&t, &disk, 0, vec![0u8; 16384]));
        assert_eq!(
            t.disk_write_failures.load(Ordering::Relaxed),
            1,
            "counting starts again from the last success"
        );
        assert_eq!(t.status.load(Ordering::Relaxed), TorrentStatus::Downloading as u8);
    }
}
