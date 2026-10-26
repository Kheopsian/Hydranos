//! Download logic for a peer connection.
//! Manages piece requesting, block assembly, and completion.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::info;

use crate::disk::DiskManager;
use crate::torrent::meta::{PeerStats, TorrentState, TorrentStatus};
use crate::torrent::piece_picker::{self, Block, Owner};

const BLOCK_SIZE: u32 = 16384;
/// How many pieces in a row may fail to reach the disk before the torrent
/// stops asking for more.
///
/// Three and not one: a seedbox that drains under pressure can refuse a write
/// and accept the next. Three and not thirty: each retry is a whole piece
/// fetched from the swarm again, so guessing high is paid in other people's
/// upload.
const DISK_FAILURES_BEFORE_ERROR: u32 = 3;
/// Pieces sent entirely by one peer that failed their hash before that peer is
/// cut off. One can be bad luck -- a flipped bit on its disk -- a second is not.
const BAD_PIECES_BEFORE_CUT: u32 = 2;
const MAX_PIPELINE: usize = 32; // max in-flight requests (was 16; bumped — MAX_PIPELINE=32 doubles the in-flight window without overwhelming the picker mutex on multi-peer scenarios)
/// How long a peer may hold a piece without sending a block of it before the
/// piece is given back, blocks and all.
///
/// The idle timeout cannot do this job: any message keeps a session alive, so
/// a peer that chokes us, or simply stops answering, while it goes on sending
/// Haves kept its pieces for the life of the connection -- the accumulation
/// measured in production (+192 MB in 22 minutes, nothing else moving).
///
/// 120 s, not less: a piece's first block can wait behind a full pipeline of
/// the previous one (32 blocks, 512 KiB), so a peer delivering 4.3 KiB/s still
/// gets each piece's first block in time. Not more: it is well inside the
/// 300 s idle timeout, so it bites before a silent peer would be dropped
/// anyway, and it bounds what a stalled torrent can hold to two minutes of
/// whatever its peers sent.
pub const PIECE_STALL_TIMEOUT: Duration = Duration::from_secs(120);

/// A piece this peer is fetching.
struct Started {
    /// When the piece was started or last advanced by this peer.
    since: Instant,
    /// Blocks of it this peer has delivered.
    got: u32,
}

pub struct DownloadState {
    torrent: Arc<TorrentState>,
    disk: Arc<DiskManager>,
    peer_bitfield: Vec<u8>,
    pub am_interested: bool,
    pub peer_choking: bool,
    /// This session's identity on the shared picker. Releases are by owner, so
    /// letting go of a piece never takes it away from another peer racing for
    /// it in endgame, or from whoever reserved it again since.
    owner: Owner,
    /// Requests on the wire, not yet answered. A choke voids them (BEP 3), so
    /// they are kept rather than counted: after the unchoke they are the first
    /// ones owed. Merely counting them lost them -- the piece then waited
    /// forever for blocks nobody would ask for again, and its reservation
    /// stayed for the life of the connection.
    inflight: VecDeque<(u32, u32, u32)>,
    /// FIFO of blocks already picked from the picker (via start_piece) but not
    /// yet emitted on the wire. Emptied by get_requests as pipeline budget
    /// allows. Required because start_piece returns ALL blocks of a piece at
    /// once (typically ~128 for 2MB pieces) but MAX_PIPELINE caps how many we
    /// can have in flight. Previously the remainder was silently discarded —
    /// pieces never completed on large torrents (only tiny ones with <16
    /// blocks fit in one pipeline).
    pending_block_queue: VecDeque<(u32, u32, u32)>,
    /// Pieces this peer has reserved on the shared picker. Each is given back
    /// when the peer chokes us before sending any of it, rejects a request for
    /// it, sends none of it for `PIECE_STALL_TIMEOUT`, or disconnects -- a
    /// reservation nobody fills is a piece nobody else may fetch, and the
    /// blocks it already holds are memory nothing else will free.
    started: HashMap<u32, Started>,
    /// The last refill stopped on the engine's download budget: wake on the
    /// next write that frees some (see `PieceMem`).
    held_back: bool,
    /// Pieces this peer alone sent that failed their hash.
    bad_pieces: u32,
    /// Too many of them: asked for nothing more, and nothing it sends is
    /// kept. The session ends the connection (see `cut_off`).
    cut_off: bool,
}

impl DownloadState {
    pub fn new(torrent: Arc<TorrentState>, disk: Arc<DiskManager>) -> Self {
        Self {
            torrent,
            disk,
            peer_bitfield: Vec::new(),
            am_interested: false,
            peer_choking: true,
            owner: piece_picker::new_owner(),
            inflight: VecDeque::new(),
            pending_block_queue: VecDeque::new(),
            started: HashMap::new(),
            held_back: false,
            bad_pieces: 0,
            cut_off: false,
        }
    }

    /// This peer sent too many pieces that failed their hash: the session
    /// should hang up, and the address should not be let back in.
    pub fn cut_off(&self) -> bool {
        self.cut_off
    }

    /// The engine's totals, when the last refill was held back by its budget.
    pub fn held_back_by(&self) -> Option<Arc<piece_picker::PieceMem>> {
        if !self.held_back {
            return None;
        }
        self.torrent.picker.get()?.lock().unwrap().mem().cloned()
    }

    pub fn is_downloading(&self) -> bool {
        self.torrent.picker.get().is_some()
            && self.torrent.status.load(Ordering::Relaxed) == TorrentStatus::Downloading as u8
    }

    /// Process incoming Bitfield from peer.
    pub fn on_bitfield(&mut self, data: &[u8]) {
        self.replace_bitfield(data.to_vec());
    }

    /// The peer's view replaces what it said before, in the picker as here:
    /// counting a second Bitfield on top of the first left availability up by
    /// one for good, since the disconnect only takes back the last one.
    fn replace_bitfield(&mut self, bitfield: Vec<u8>) {
        if let Some(picker) = self.torrent.picker.get() {
            let mut p = picker.lock().unwrap();
            if !self.peer_bitfield.is_empty() {
                p.remove_bitfield(&self.peer_bitfield);
            }
            p.add_bitfield(&bitfield);
        }
        self.peer_bitfield = bitfield;
    }

    /// Process incoming HaveAll from peer.
    pub fn on_have_all(&mut self) {
        let num_pieces = self.torrent.meta.num_pieces() as usize;
        let byte_len = num_pieces.div_ceil(8);
        let mut all = vec![0xFF; byte_len];
        // Fix trailing bits
        let trailing = num_pieces % 8;
        if trailing > 0 {
            all[byte_len - 1] = 0xFF << (8 - trailing);
        }
        self.replace_bitfield(all);
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

    /// Copy what this session knows about the peer's side of the conversation
    /// onto the stats the peer table reads: does it choke us, do we want its
    /// pieces, how many of our requests wait for an answer.
    ///
    /// Plain stores of three values this session already holds: nothing is
    /// computed, and the table is read only when someone opens it.
    pub fn publish_to(&self, stats: &PeerStats) {
        stats
            .peer_choking
            .store(self.peer_choking, Ordering::Relaxed);
        stats
            .am_interested
            .store(self.am_interested, Ordering::Relaxed);
        stats
            .requests_in_flight
            .store(self.inflight.len() as u32, Ordering::Relaxed);
    }

    pub fn on_unchoke(&mut self) {
        self.peer_choking = false;
    }

    /// The peer stops serving us.
    ///
    /// Every request on the wire is void (BEP 3): they go back to the front of
    /// the queue, to be asked again first after an unchoke. A fast-extension
    /// peer may still answer some, and the block is then simply not asked
    /// again. Pieces this peer has sent nothing of go back to the picker now,
    /// for any other peer to take: a choke can last for the rest of the
    /// connection. Pieces it has started delivering stay ours for
    /// `PIECE_STALL_TIMEOUT`, so an unchoke soon after loses nothing.
    pub fn on_choke(&mut self) {
        self.peer_choking = true;
        while let Some(r) = self.inflight.pop_back() {
            self.pending_block_queue.push_front(r);
        }
        let untouched: Vec<u32> = self
            .started
            .iter()
            .filter(|(_, s)| s.got == 0)
            .map(|(&p, _)| p)
            .collect();
        self.release(&untouched);
    }

    /// BEP 6: the peer will not send this block. The piece goes back to the
    /// picker -- asking the same peer again could only be refused again.
    /// A Reject for a request no longer on the wire (the ones a choke already
    /// voided) changes nothing.
    pub fn on_reject(&mut self, index: u32, begin: u32, length: u32) {
        let Some(i) = self
            .inflight
            .iter()
            .position(|r| *r == (index, begin, length))
        else {
            return;
        };
        self.inflight.remove(i);
        self.release(&[index]);
    }

    /// Is this peer holding any piece? Lets the session skip the clock read
    /// for the stall check on the many connections that fetch nothing.
    pub fn has_started(&self) -> bool {
        !self.started.is_empty()
    }

    /// Give back every piece this peer has not advanced for
    /// `PIECE_STALL_TIMEOUT`, received blocks included. Returns the requests
    /// still on the wire for them, for the caller to Cancel; a block that
    /// arrives anyway finds no piece and is dropped.
    ///
    /// Runs whatever the torrent's state: a paused, errored or rechecking
    /// torrent requests nothing new, and this is what lets its pieces go.
    pub fn release_stalled(&mut self, now: Instant) -> Vec<(u32, u32, u32)> {
        let stalled: Vec<u32> = self
            .started
            .iter()
            .filter(|(_, s)| now.saturating_duration_since(s.since) >= PIECE_STALL_TIMEOUT)
            .map(|(&p, _)| p)
            .collect();
        if stalled.is_empty() {
            return Vec::new();
        }
        let cancels = self
            .inflight
            .iter()
            .filter(|r| stalled.contains(&r.0))
            .copied()
            .collect();
        self.release(&stalled);
        cancels
    }

    /// Let go of these pieces: off our books, out of the queue and the
    /// pipeline, and released on the picker under our owner.
    fn release(&mut self, pieces: &[u32]) {
        if pieces.is_empty() {
            return;
        }
        for p in pieces {
            self.started.remove(p);
        }
        self.pending_block_queue.retain(|r| !pieces.contains(&r.0));
        self.inflight.retain(|r| !pieces.contains(&r.0));
        if let Some(picker) = self.torrent.picker.get() {
            let mut pk = picker.lock().unwrap();
            for &p in pieces {
                pk.release(p, self.owner);
            }
        }
    }

    /// Check if we should send Interested to this peer.
    pub fn should_be_interested(&self) -> bool {
        if !self.is_downloading() {
            return false;
        }
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
        if self.cut_off {
            return Vec::new();
        }
        self.get_requests_gated(|_| true)
    }

    /// `get_requests`, asking `admit` before each block goes into the batch.
    ///
    /// This is where a download cap bites: the first block `admit` refuses
    /// goes back to the FRONT of the queue -- it is the next one owed, and its
    /// piece must still be finished before another is started -- and the batch
    /// stops there. The pipeline is then refilled when the cap allows, instead
    /// of being sized by a number the cap knows nothing about.
    pub fn get_requests_gated(
        &mut self,
        mut admit: impl FnMut(u32) -> bool,
    ) -> Vec<(u32, u32, u32)> {
        self.held_back = false;
        // A paused torrent asks for nothing. This is the single choke point
        // for the download side: `stop_torrent` sets the flag and untracks the
        // DHT, but it does not tear down sessions that are already connected,
        // and nothing else on this path consulted the flag -- so a paused
        // torrent kept requesting blocks from every peer it already had and
        // went on downloading at full speed while the interface said stopped.
        // Blocks already in flight still arrive; the pipeline is bounded, so
        // that is a handful of them and then silence.
        //
        // A removed torrent neither: its blocks are dropped on arrival, so
        // every piece started for it was a reservation that could never
        // complete, and the next one was started as soon as the pipeline
        // drained.
        if self.torrent.is_paused.load(Ordering::Relaxed)
            || self.torrent.is_removed.load(Ordering::Relaxed)
        {
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
        let now = Instant::now();

        while self.inflight.len() < MAX_PIPELINE {
            // Drain queue first.
            if let Some(req) = self.pending_block_queue.pop_front() {
                // Not ours to ask for any more: the block came in meanwhile
                // (from another peer in endgame, or from this one after a
                // choke voided the request), or the piece left our hands.
                if !picker.wants_block(req.0, req.1, self.owner) {
                    continue;
                }
                if !admit(req.2) {
                    self.pending_block_queue.push_front(req);
                    break;
                }
                requests.push(req);
                self.inflight.push_back(req);
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
                Some(p) if self.started.contains_key(&p) => {
                    if picker.holds(p, self.owner) {
                        break;
                    }
                    // Our record outlived the reservation: another peer
                    // completed the piece and it failed its hash, say. Start
                    // it afresh rather than refuse it for good.
                    self.started.remove(&p);
                    p
                }
                Some(p) => p,
                None => break,
            };
            let piece_size = self.torrent.meta.piece_size(piece);
            // The engine's download memory is spent: start nothing new until
            // pieces in flight finish and their writes return. Joining one
            // already in flight (endgame) costs nothing more.
            // The check and the reservation are one step: other torrents start
            // pieces on the same budget under their own locks.
            let Some(piece_requests) =
                picker.start_piece_within_budget(piece, piece_size, BLOCK_SIZE, self.owner)
            else {
                self.held_back = true;
                break;
            };
            self.pending_block_queue.extend(piece_requests);
            self.started.insert(piece, Started { since: now, got: 0 });
        }

        requests
    }

    /// Process received Piece data. Returns Some(piece_index) if a piece completed.
    pub async fn on_piece(&mut self, index: u32, begin: u32, data: &[u8]) -> Option<u32> {
        if self.cut_off {
            return None;
        }
        if let Some(i) = self
            .inflight
            .iter()
            .position(|r| r.0 == index && r.1 == begin)
        {
            self.inflight.remove(i);
        }

        // Skip if torrent was removed mid-flight — otherwise write_piece
        // (create=true) would recreate the files we just deleted.
        if self.torrent.is_removed.load(Ordering::Relaxed) {
            return None;
        }

        let picker = match self.torrent.picker.get() {
            Some(p) => p,
            None => return None,
        };

        // Stored and, if it was the last block, taken in the same lock: two
        // peers finishing the same endgame piece cannot both hand it over.
        let (outcome, piece_data) = {
            let mut p = picker.lock().unwrap();
            let outcome = p.receive_block_from(index, begin, data, self.owner);
            let piece_data = if outcome == Block::Complete {
                p.take_piece(index)
            } else {
                None
            };
            (outcome, piece_data)
        };
        if matches!(outcome, Block::Stored | Block::Complete | Block::Duplicate) {
            if let Some(s) = self.started.get_mut(&index) {
                s.since = Instant::now();
                s.got += 1;
            }
        }

        if let Some((piece_data, sole)) = piece_data {
            // Off this peer's books either way: the picker no longer holds it,
            // and blocks of it still queued are skipped by `wants_block`.
            self.started.remove(&index);
            match commit(&self.torrent, &self.disk, index, piece_data).await {
                Commit::Owned => return Some(index),
                // Every block of it came from this peer: the bad bytes are its.
                // The last block is ours, so a sole sender can only be us.
                Commit::BadHash if sole == Some(self.owner) => {
                    self.bad_pieces += 1;
                    if self.bad_pieces >= BAD_PIECES_BEFORE_CUT {
                        tracing::warn!(
                            "[download] {}: cutting off a peer after {} pieces that failed their hash",
                            &crate::torrent::hex_encode(&self.torrent.info_hash)[..8],
                            self.bad_pieces
                        );
                        self.cut_off = true;
                        self.on_disconnect();
                    }
                }
                Commit::BadHash | Commit::Failed => {}
            }
        }
        None
    }

    /// Cleanup when peer disconnects. Releases pieces this peer had started
    /// but not finished, so other peers can pick them up. Without this, any
    /// piece in `started` leaks into the picker's `pending` map and is never
    /// re-picked — the torrent stalls near completion.
    ///
    /// Called explicitly at the end of a session, and again by `Drop` for every
    /// other way out. Draining `started` makes the second call a no-op.
    pub fn on_disconnect(&mut self) {
        let pieces: Vec<u32> = self.started.keys().copied().collect();
        self.release(&pieces);
        self.pending_block_queue.clear();
        self.inflight.clear();
        if let Some(picker) = self.torrent.picker.get() {
            let mut p = picker.lock().unwrap();
            if !self.peer_bitfield.is_empty() {
                p.remove_bitfield(&self.peer_bitfield);
                // Idempotence, and it is not optional: this runs once from
                // `session::run` and once more from `Drop`. `started` is
                // drained so the piece half replays harmlessly, but
                // `remove_bitfield` DECREMENTS availability counters -- running
                // it twice would understate how rare every piece this peer held
                // is, and rarest-first picks on those counters.
                self.peer_bitfield.clear();
            }
        }
    }
}

/// Release reserved pieces however the session ends.
///
/// `on_disconnect` is reached from exactly one place -- the end of
/// `session::run` -- so any other way out kept every piece this peer had
/// started in the picker's `pending` map, each holding whatever had been
/// received for it, that nothing would ever free or re-pick. Early returns, a
/// panic in the session task, and task cancellation at an await point all take
/// that path, and an engine accepting 76 inbound connections a second takes it
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
/// is simply missing again -- `take_piece_data` already took it out of the
/// picker -- so any source can fetch it anew.
pub async fn commit_piece(
    torrent: &Arc<TorrentState>,
    disk: &Arc<DiskManager>,
    index: u32,
    piece_data: Vec<u8>,
) -> bool {
    commit(torrent, disk, index, piece_data).await == Commit::Owned
}

/// How `commit` ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Commit {
    /// Verified, written, ours.
    Owned,
    /// The bytes do not match the piece hash.
    BadHash,
    /// Not written: no picker, no hash table, or the disk refused.
    Failed,
}

async fn commit(
    torrent: &Arc<TorrentState>,
    disk: &Arc<DiskManager>,
    index: u32,
    piece_data: Vec<u8>,
) -> Commit {
    let picker = match torrent.picker.get() {
        Some(p) => p,
        None => return Commit::Failed,
    };
    let len = piece_data.len() as u64;
    // Counted in the engine's download budget until the write returns.
    let _writing = picker.lock().unwrap().mem().map(|m| m.hold_write(len));
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
                torrent.pending_announce_event.fetch_or(
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
            Commit::Owned
        }
        Ok(false) => {
            // The peer lied about its data. Asking somebody else is exactly
            // right, and it says nothing about the disk.
            // Nothing to release: the piece left the picker when it was taken
            // for this write, and is picked again like any missing piece.
            tracing::warn!("[download] piece {} SHA1 mismatch, re-requesting", index);
            Commit::BadHash
        }
        Err(e) => {
            // The disk refused. Re-requesting cannot fix a full volume or a
            // read-only mount, and each attempt pulls a whole piece off the
            // swarm again -- so this downloaded the same piece forever, at full
            // speed, making no progress and showing none.
            tracing::warn!("[download] piece {} write failed: {}", index, e);
            // The piece left the picker when it was taken for this write: the
            // buffer goes with this call, and the piece is missing again.

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
            Commit::Failed
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
            v2: false,
            renames: Default::default(),
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
        assert!(
            !dl.on_have(3),
            "the second time tells the caller not to count it"
        );
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
        assert_eq!(
            dl.peer_bitfield[1], 0b1100_0000,
            "two real pieces, six of padding"
        );
    }

    #[test]
    fn a_bitfield_is_taken_as_sent() {
        let mut dl = state(downloading(16, 16384));
        dl.on_bitfield(&[0b1010_1010, 0x00]);
        assert_eq!(dl.peer_bitfield, vec![0b1010_1010, 0x00]);
    }

    fn copies_seen(dl: &DownloadState) -> u64 {
        dl.torrent
            .picker
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .availability_stats()
            .2
    }

    /// ⭐ One peer is one copy of what it holds, however many times it says
    /// so. Each repeat used to add another copy that the disconnect never took
    /// back, and rarest-first went on trusting those counters for good.
    #[test]
    fn a_peer_that_repeats_its_bitfield_still_counts_once() {
        let mut dl = state(downloading(16, 16384));
        dl.on_bitfield(&[0b1010_1010, 0x00]);
        dl.on_bitfield(&[0b1010_1010, 0x00]);
        dl.on_have_all();
        dl.on_have_all();
        assert_eq!(
            copies_seen(&dl),
            16,
            "one copy of each of the sixteen pieces"
        );
        dl.on_disconnect();
        assert_eq!(
            copies_seen(&dl),
            0,
            "gone with the peer, nothing left behind"
        );
    }

    /// A Bitfield that drops a piece the peer announced by Have takes that copy
    /// back: what the peer last said is what it holds.
    #[test]
    fn a_later_bitfield_replaces_what_the_haves_said() {
        let mut dl = state(downloading(16, 16384));
        assert!(dl.on_have(0));
        assert!(dl.on_have(9));
        dl.on_bitfield(&[0b1000_0000, 0x00]);
        assert_eq!(copies_seen(&dl), 1);
        dl.on_disconnect();
        assert_eq!(copies_seen(&dl), 0);
    }

    // -----------------------------------------------------------------------
    // Choke / interest
    // -----------------------------------------------------------------------

    /// A session whose peer chokes us says so on its stats, and says the
    /// opposite after the unchoke; a choke voids what was on the wire. This is
    /// what tells a seeder that holds everything and sends nothing (it chokes
    /// us) from one that is slow.
    #[test]
    fn a_session_publishes_who_is_choking_whom() {
        let t = downloading(64, 2 * 1024 * 1024);
        let s = registered(&t);
        let mut dl = state(t.clone());

        dl.publish_to(&s);
        assert!(
            s.peer_choking.load(Ordering::Relaxed),
            "BT starts with the peer choking us"
        );

        dl.on_have_all();
        dl.on_unchoke();
        dl.publish_to(&s);
        assert!(
            !s.peer_choking.load(Ordering::Relaxed),
            "false after the unchoke"
        );

        dl.am_interested = true;
        let asked = dl.get_requests().len();
        assert!(asked > 0, "the unchoked peer is asked for blocks");
        dl.publish_to(&s);
        assert!(s.am_interested.load(Ordering::Relaxed));
        assert_eq!(s.requests_in_flight.load(Ordering::Relaxed), asked as u32);
        assert_eq!(
            crate::rpc::dispatch::peers_json(&t)[0]["flags"],
            "D",
            "the table reads what the session published"
        );

        dl.on_choke();
        dl.publish_to(&s);
        assert!(
            s.peer_choking.load(Ordering::Relaxed),
            "true again after a choke"
        );
        assert_eq!(
            s.requests_in_flight.load(Ordering::Relaxed),
            0,
            "a choke voids what was on the wire"
        );
    }

    /// A peer registered on the torrent, as a session's guard does.
    fn registered(t: &TorrentState) -> Arc<PeerStats> {
        let addr: std::net::SocketAddr = "192.0.2.7:6881".parse().unwrap();
        let s = Arc::new(PeerStats::new(addr, [7; 20], "test".into(), false, false));
        t.peer_stats.insert(addr, s.clone());
        s
    }

    /// The peer table says, per peer, who chokes whom: `flags` only ever said
    /// whether WE choke it. A fresh connection is choked by the peer and
    /// wanted by nobody (BT's defaults), and the qBittorrent download letters
    /// follow the two fields.
    #[test]
    fn the_peer_table_names_both_directions() {
        let t = downloading(8, 16384);
        let s = registered(&t);
        let row = |t: &Arc<TorrentState>| crate::rpc::dispatch::peers_json(t)[0].clone();

        let r = row(&t);
        assert_eq!(r["peer_choking"], true, "BT starts choked");
        assert_eq!(r["am_interested"], false);
        assert_eq!(r["requests_in_flight"], 0);
        assert_eq!(r["flags"], "", "nothing to say yet");

        s.am_interested.store(true, Ordering::Relaxed);
        s.requests_in_flight.store(5, Ordering::Relaxed);
        let r = row(&t);
        assert_eq!(r["flags"], "d", "we want data, the peer will not send");
        assert_eq!(r["requests_in_flight"], 5);

        s.peer_choking.store(false, Ordering::Relaxed);
        let r = row(&t);
        assert_eq!(r["peer_choking"], false);
        assert_eq!(r["flags"], "D", "we want data and it flows");
    }

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
        t.status
            .store(TorrentStatus::Seeding as u8, Ordering::Relaxed);
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
        assert_eq!(
            reqs.len(),
            MAX_PIPELINE,
            "the pipeline is filled, not the piece"
        );

        // All of one piece before any of the next: a piece half-requested from
        // two peers completes twice as slowly and pins twice the memory.
        let first = reqs[0].0;
        assert!(
            reqs.iter().all(|(piece, _, _)| *piece == first),
            "one piece at a time: {reqs:?}"
        );
        // Blocks are contiguous and of the block size.
        for (i, (_, begin, len)) in reqs.iter().enumerate() {
            assert_eq!(
                *begin,
                i as u32 * BLOCK_SIZE,
                "block {i} starts where the last ended"
            );
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
            TorrentMeta {
                total_size: 1000,
                ..meta(1, 16384)
            },
            PathBuf::from("/tmp"),
            false,
        ));
        t.status
            .store(TorrentStatus::Downloading as u8, Ordering::Relaxed);
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
                v2: false,
                renames: Default::default(),
            },
            PathBuf::from("/tmp"),
            false,
        ));
        t.status
            .store(TorrentStatus::Downloading as u8, Ordering::Relaxed);
        let mut dl = DownloadState::new(t, Arc::new(DiskManager::new(100)));
        dl.on_have_all();
        dl.on_unchoke();

        let reqs = dl.get_requests();
        assert_eq!(
            reqs.len(),
            1,
            "one piece of one short block, asked for once: {reqs:?}"
        );
        assert_eq!(
            reqs[0],
            (0, 0, 1000),
            "and at its real length, not a full block"
        );
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
                v2: false,
                renames: Default::default(),
            },
            PathBuf::from("/tmp"),
            false,
        ));
        t.status
            .store(TorrentStatus::Downloading as u8, Ordering::Relaxed);
        let mut dl = DownloadState::new(t, Arc::new(DiskManager::new(100)));
        dl.on_have_all();
        dl.on_unchoke();

        let reqs = dl.get_requests();
        let mut seen = std::collections::HashSet::new();
        for r in &reqs {
            assert!(seen.insert(*r), "block {r:?} asked for twice in {reqs:?}");
        }
        assert!(
            reqs.len() <= 8,
            "four pieces of two blocks is eight, got {}",
            reqs.len()
        );
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
                v2: false,
                renames: Default::default(),
            },
            PathBuf::from("/tmp"),
            false,
        ));
        t.status
            .store(TorrentStatus::Downloading as u8, Ordering::Relaxed);
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
        assert!(
            !dl.get_requests().is_empty(),
            "asking, before the disk gives up"
        );

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
        assert_eq!(
            t.status.load(Ordering::Relaxed),
            TorrentStatus::Downloading as u8
        );
    }
}

#[cfg(test)]
mod release_tests {
    use super::*;
    use crate::torrent::meta::TorrentMeta;
    use std::collections::HashSet;
    use std::path::PathBuf;

    const PIECE: u32 = 4 * BLOCK_SIZE; // four blocks a piece

    fn torrent(num_pieces: u32, piece_length: u32) -> Arc<TorrentState> {
        let t = Arc::new(TorrentState::new(
            TorrentMeta {
                info_hash: [7u8; 20],
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
                v2: false,
                renames: Default::default(),
            },
            PathBuf::from("/tmp"),
            false,
        ));
        t.status
            .store(TorrentStatus::Downloading as u8, Ordering::Relaxed);
        t
    }

    fn peer(t: &Arc<TorrentState>) -> DownloadState {
        let mut dl = DownloadState::new(t.clone(), Arc::new(DiskManager::new(4)));
        dl.on_have_all();
        dl.on_unchoke();
        dl
    }

    fn in_flight(t: &TorrentState) -> (usize, u64) {
        t.picker.get().unwrap().lock().unwrap().in_flight()
    }

    fn holds(t: &TorrentState, piece: u32, dl: &DownloadState) -> bool {
        t.picker
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .holds(piece, dl.owner)
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn block() -> Vec<u8> {
        vec![0xA5; BLOCK_SIZE as usize]
    }

    /// ⭐ A choke gives back the pieces this peer sent nothing of, keeps the
    /// one it was delivering, and does not touch another peer's.
    #[test]
    fn a_choke_frees_the_untouched_pieces_and_only_this_peers() {
        let t = torrent(64, PIECE);
        let mut a = peer(&t);
        let mut b = peer(&t);
        let ra = a.get_requests();
        assert_eq!(ra.len(), MAX_PIPELINE);
        let a_pieces: HashSet<u32> = ra.iter().map(|r| r.0).collect();
        assert_eq!(a_pieces.len(), 8, "32 blocks of four-block pieces");
        let rb = b.get_requests();
        let b_pieces: HashSet<u32> = rb.iter().map(|r| r.0).collect();
        assert!(a_pieces.is_disjoint(&b_pieces));
        assert_eq!(in_flight(&t), (16, 0), "sixteen reserved, not a byte held");

        let first = ra[0].0;
        rt().block_on(a.on_piece(first, 0, &block()));
        assert_eq!(in_flight(&t), (16, BLOCK_SIZE as u64), "one block held");

        a.on_choke();
        assert_eq!(
            in_flight(&t),
            (9, BLOCK_SIZE as u64),
            "A's seven untouched pieces went back"
        );
        assert!(holds(&t, first, &a), "the piece A was delivering stays A's");
        for p in &b_pieces {
            assert!(holds(&t, *p, &b), "B's piece {p} is B's business");
        }

        // After the unchoke the voided requests of the kept piece go first,
        // and the block already in is not asked again.
        a.on_unchoke();
        let again = a.get_requests();
        assert_eq!(
            &again[..3],
            &[
                (first, BLOCK_SIZE, BLOCK_SIZE),
                (first, 2 * BLOCK_SIZE, BLOCK_SIZE),
                (first, 3 * BLOCK_SIZE, BLOCK_SIZE)
            ],
            "the rest of the piece, first: {again:?}"
        );
        assert!(!again.contains(&(first, 0, BLOCK_SIZE)));
    }

    /// ⭐ The production leak: a peer that stays connected and stops sending
    /// kept its pieces for the life of the connection. Past the stall delay
    /// they go back, received blocks included; a block that turns up later
    /// is dropped and costs nothing.
    #[test]
    fn a_piece_with_no_new_block_for_the_delay_is_given_back() {
        let t = torrent(64, PIECE);
        let mut a = peer(&t);
        let reqs = a.get_requests();
        let p = reqs[0].0;
        rt().block_on(a.on_piece(p, 0, &block()));
        let now = Instant::now();
        assert!(
            a.release_stalled(now + PIECE_STALL_TIMEOUT / 2).is_empty(),
            "not yet"
        );
        assert_eq!(in_flight(&t).0, 8);

        let cancels = a.release_stalled(now + PIECE_STALL_TIMEOUT + Duration::from_secs(1));
        assert_eq!(
            cancels.len(),
            MAX_PIPELINE - 1,
            "every request still on the wire is withdrawn"
        );
        assert_eq!(
            in_flight(&t),
            (0, 0),
            "the partial piece went too, blocks and all"
        );
        assert!(!a.has_started());

        // The late block: no panic, no piece, no byte held.
        assert_eq!(rt().block_on(a.on_piece(p, BLOCK_SIZE, &block())), None);
        assert_eq!(in_flight(&t), (0, 0));
        // And the peer can start afresh.
        assert!(!a.get_requests().is_empty());
    }

    /// A paused torrent asks for nothing new; the stall delay is what lets
    /// the pieces it was fetching go.
    #[test]
    fn a_paused_torrents_pieces_still_go_after_the_delay() {
        let t = torrent(64, PIECE);
        let mut a = peer(&t);
        a.get_requests();
        t.is_paused.store(true, Ordering::Relaxed);
        a.release_stalled(Instant::now() + PIECE_STALL_TIMEOUT + Duration::from_secs(1));
        assert_eq!(in_flight(&t), (0, 0));
    }

    /// BEP 6: a rejected request gives the piece back; a Reject for a request
    /// that is not on the wire changes nothing.
    #[test]
    fn a_reject_gives_the_piece_back() {
        let t = torrent(64, PIECE);
        let mut a = peer(&t);
        let reqs = a.get_requests();
        a.on_reject(999, 0, BLOCK_SIZE);
        assert_eq!(in_flight(&t).0, 8);
        let (p, begin, len) = reqs[1];
        a.on_reject(p, begin, len);
        assert_eq!(in_flight(&t).0, 7);
        assert!(!holds(&t, p, &a));
        // The rejects a fast peer sends after a choke are for requests the
        // choke already voided: they do not cost the piece being delivered.
        let q = reqs[8].0;
        rt().block_on(a.on_piece(q, 0, &block()));
        a.on_choke();
        a.on_reject(q, BLOCK_SIZE, BLOCK_SIZE);
        assert!(holds(&t, q, &a));
    }

    /// ⭐ Endgame: two peers on the same last piece. Each block is stored once,
    /// the piece is handed to the disk once, and nothing is left behind.
    #[test]
    fn endgame_two_peers_one_piece_one_write_nothing_left() {
        // A one-piece torrent is always in endgame. No piece hashes: the write
        // fails, which counts it -- the count IS the number of writes.
        let t = torrent(1, 2 * BLOCK_SIZE);
        let mut a = peer(&t);
        let mut b = peer(&t);
        assert_eq!(a.get_requests().len(), 2);
        assert_eq!(b.get_requests().len(), 2, "B joins the same piece");
        assert_eq!(in_flight(&t).0, 1, "one piece, two owners");
        let rt = rt();
        rt.block_on(a.on_piece(0, 0, &block()));
        rt.block_on(b.on_piece(0, 0, &block())); // duplicate
        assert_eq!(
            in_flight(&t),
            (1, BLOCK_SIZE as u64),
            "a duplicate is not stored twice"
        );
        rt.block_on(b.on_piece(0, BLOCK_SIZE, &block()));
        rt.block_on(a.on_piece(0, BLOCK_SIZE, &block()));
        assert_eq!(
            t.disk_write_failures.load(Ordering::Relaxed),
            1,
            "one write, not two"
        );
        assert_eq!(in_flight(&t), (0, 0));
        drop(a);
        drop(b);
        assert_eq!(in_flight(&t), (0, 0));
    }

    /// Disconnect and drop free what a peer held, whatever it had received.
    #[test]
    fn disconnect_and_drop_always_free() {
        let t = torrent(64, PIECE);
        let mut a = peer(&t);
        let mut b = peer(&t);
        let ra = a.get_requests();
        b.get_requests();
        rt().block_on(a.on_piece(ra[0].0, 0, &block()));
        a.on_disconnect();
        a.on_disconnect();
        assert_eq!(in_flight(&t), (8, 0), "only B's eight left");
        drop(b);
        assert_eq!(in_flight(&t), (0, 0));
    }

    /// The memory a piece costs follows the blocks received, on the engine's
    /// counters too.
    #[test]
    fn the_engine_counters_follow_the_blocks() {
        let t = torrent(64, PIECE);
        let mem = Arc::new(crate::torrent::piece_picker::PieceMem::default());
        t.picker
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .attach_mem(mem.clone());
        let mut a = peer(&t);
        let reqs = a.get_requests();
        assert_eq!((mem.pieces(), mem.bytes()), (8, 0));
        let rt = rt();
        for r in reqs.iter().take(3) {
            rt.block_on(a.on_piece(r.0, r.1, &block()));
        }
        assert_eq!(mem.bytes(), 3 * BLOCK_SIZE as u64);
        drop(a);
        assert_eq!((mem.pieces(), mem.bytes()), (0, 0));
    }

    /// Sessions of DIFFERENT torrents start pieces at the same moment on one
    /// engine's budget, each under its own picker lock. The budget is checked
    /// and spent in one step, so what they hold together never passes it.
    #[test]
    fn sessions_of_different_torrents_starting_pieces_together_stay_within_the_budget() {
        use std::sync::Barrier;
        const SESSIONS: usize = 8;
        let budget = 2 * PIECE as u64;
        let mut worst = 0;
        for _ in 0..200 {
            let mem = Arc::new(crate::torrent::piece_picker::PieceMem::default());
            mem.set_budget(budget);
            let ready = Arc::new(Barrier::new(SESSIONS));
            let fired = Arc::new(Barrier::new(SESSIONS));
            let handles: Vec<_> = (0..SESSIONS)
                .map(|_| {
                    let (mem, ready, fired) = (mem.clone(), ready.clone(), fired.clone());
                    std::thread::spawn(move || {
                        let t = torrent(64, PIECE);
                        t.picker
                            .get()
                            .unwrap()
                            .lock()
                            .unwrap()
                            .attach_mem(mem.clone());
                        let mut dl = peer(&t);
                        ready.wait();
                        let _ = dl.get_requests();
                        fired.wait();
                        mem.committed()
                    })
                })
                .collect();
            for h in handles {
                worst = worst.max(h.join().unwrap());
            }
        }
        assert!(
            worst <= budget,
            "the sessions committed {worst} bytes on a budget of {budget}"
        );
    }
}

#[cfg(test)]
mod poison_tests {
    use super::*;
    use crate::torrent::meta::TorrentMeta;
    use sha1::{Digest, Sha1};
    use std::path::PathBuf;

    const PIECE: u32 = 2 * BLOCK_SIZE;

    fn good() -> Vec<u8> {
        vec![0x5A; PIECE as usize]
    }

    /// A torrent whose every piece is `good()`, its hash table in the store,
    /// so a wrong piece fails its hash rather than the write.
    fn torrent(num_pieces: u32) -> Arc<TorrentState> {
        let t = Arc::new(TorrentState::new(
            TorrentMeta {
                info_hash: [5u8; 20],
                name: "t".into(),
                num_pieces,
                piece_length: PIECE,
                total_size: num_pieces as u64 * PIECE as u64,
                files: Vec::new(),
                trackers: Vec::new(),
                url_list: Vec::new(),
                private: false,
                multi_file: false,
                info_dict_len: 0,
                v2: false,
                renames: Default::default(),
            },
            PathBuf::from("/nonexistent"),
            false,
        ));
        t.status
            .store(TorrentStatus::Downloading as u8, Ordering::Relaxed);
        let h = Sha1::digest(good());
        let mut blob = format!("d4:infod6:pieces{}:", num_pieces * 20).into_bytes();
        for _ in 0..num_pieces {
            blob.extend_from_slice(&h);
        }
        blob.extend_from_slice(b"ee");
        t.blob_source
            .set(Arc::new(move |_: &str| Some(blob.clone())))
            .ok();
        t
    }

    fn peer(t: &Arc<TorrentState>) -> DownloadState {
        let mut dl = DownloadState::new(t.clone(), Arc::new(DiskManager::new(4)));
        dl.on_have_all();
        dl.on_unchoke();
        dl
    }

    fn in_flight(t: &TorrentState) -> (usize, u64) {
        t.picker.get().unwrap().lock().unwrap().in_flight()
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    /// ⭐ A peer cannot fill a piece it was never asked for: its blocks are
    /// dropped and the piece stays with the peer fetching it.
    #[test]
    fn a_block_this_peer_was_not_asked_for_is_dropped() {
        let t = torrent(1);
        let mut a = peer(&t);
        let mut b = peer(&t);
        let ra = a.get_requests();
        assert_eq!(ra.len(), 2);
        let rt = rt();
        for r in &ra {
            rt.block_on(b.on_piece(r.0, r.1, &vec![0u8; r.2 as usize]));
        }
        assert_eq!(in_flight(&t), (1, 0), "a block nobody asked B for was kept");
    }

    /// ⭐ Two pieces from this peer alone that fail their hash, and it is cut
    /// off: asked for nothing more.
    #[test]
    fn a_peer_whose_pieces_fail_twice_is_asked_for_nothing_more() {
        // Past the endgame: each piece is this peer's alone.
        let t = torrent(64);
        let mut a = peer(&t);
        let ra = a.get_requests();
        let rt = rt();
        let bad: Vec<_> = ra
            .iter()
            .filter(|r| r.0 == ra[0].0 || r.0 == ra[2].0)
            .copied()
            .collect();
        assert_eq!(bad.len(), 4, "two pieces of two blocks");
        for r in &bad {
            rt.block_on(a.on_piece(r.0, r.1, &vec![0u8; r.2 as usize]));
        }
        let again = a.get_requests();
        assert!(
            again.is_empty(),
            "still asked for {again:?} after two bad pieces"
        );
        assert_eq!(
            in_flight(&t),
            (0, 0),
            "what it held went back for others to fetch"
        );
    }

    /// An endgame piece filled by two peers that fails proves nothing about
    /// either: neither is charged.
    #[test]
    fn a_bad_piece_from_two_peers_charges_neither() {
        let t = torrent(1);
        let rt = rt();
        let mut a = peer(&t);
        let mut b = peer(&t);
        for _ in 0..3 {
            assert_eq!(a.get_requests().len(), 2);
            assert_eq!(b.get_requests().len(), 2, "B joins the endgame piece");
            rt.block_on(a.on_piece(0, 0, &vec![0u8; BLOCK_SIZE as usize]));
            rt.block_on(b.on_piece(0, BLOCK_SIZE, &good()[BLOCK_SIZE as usize..]));
            assert_eq!(in_flight(&t), (0, 0), "handed over and failed");
        }
    }
}
