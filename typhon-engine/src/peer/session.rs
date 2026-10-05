//! Peer session loop — runs after the BT handshake has succeeded.
//!
//! Shared by both the incoming (`peer::handle_incoming`) and outgoing
//! (`tracker::dial_peer`) paths. The caller is responsible for:
//!   * negotiating the handshake (MSE or plaintext)
//!   * extracting the remote peer_id and the fast/lt_ext bits
//!
//! This function then:
//!   * wires a `PeerStats` + `PeerGuard` (RAII registration in the torrent)
//!   * sends the initial Bitfield/HaveAll then Unchoke (the choking engine
//!     may re-choke later via `choking_gen`)
//!   * runs the bidirectional message loop until the peer disconnects

/// A peer that sends us nothing for this long is dropped.
///
/// Note this now actually fires on downloading sessions. It could not before:
/// the deadline was rebuilt on every turn of the loop, and the 10s choke tick
/// Do we hold every piece of this torrent right now?
///
/// Read live rather than reusing the `is_seeding` snapshot taken when the
/// session opened: a torrent that completed since then is a seeder too, and
/// keeping its useless connections is exactly what we are trying to stop.
/// Whether a peer may hand us other peers on this torrent -- PEX, or a BEP 55
/// `connect` naming somebody to dial.
///
/// ⭐ BEP 27: a private torrent takes peers from its tracker and from nowhere
/// else. Not advertising `ut_pex` / `ut_holepunch` in our handshake is not
/// enough, because nothing stops a peer from SENDING the message anyway, and
/// a `connect` was acted on -- we dialled an address a peer chose, on a
/// torrent whose tracker is the only permitted source. The guard sits on what
/// we RECEIVE, where the leak actually is.
pub fn peer_sources_allowed(torrent: &crate::torrent::meta::TorrentState) -> bool {
    torrent.meta.allows_peer_discovery() && torrent.policy().pex()
}

fn we_are_complete(t: &std::sync::Arc<crate::torrent::meta::TorrentState>) -> bool {
    t.status.load(Ordering::Relaxed) == TorrentStatus::Seeding as u8
}

/// Does this frame count as the peer being alive, for the idle timeout?
///
/// Everything except a keep-alive. BEP 3 has a client send one every ~2 min to
/// hold a connection open, which is well inside the idle timeout; treating
/// it as activity made the deadline unreachable and every peer immortal. The
/// timeout exists to drop peers that do nothing, and a peer whose entire
/// contribution is "still here" is doing nothing.
///
/// Split out of the loop so the rule can be tested without standing up a
/// session, and so restoring the bug means deleting a test.
fn pushes_idle_deadline(m: &Message) -> bool {
    !matches!(m, Message::KeepAlive)
}

/// turned the loop, so a non-seeding session reset its own 300s timeout every
/// 10 seconds and never hit it. Seeding sessions are unaffected -- their choke
/// arm is disabled, so only peer traffic ever turned their loop.
///
/// The length is the engine's `peer_timeout`. It was a constant 300 s while
/// the config key was read by nobody; it is now read on every reset, so a new
/// value reaches the connections already open.
fn peer_idle_timeout(torrent: &TorrentState) -> Duration {
    torrent.policy().idle_timeout()
}
/// Granularity for pushing out the idle deadline.
///
/// Resetting a `tokio::time::Sleep` removes and re-inserts an entry in the
/// timer wheel behind a sharded mutex. Doing it per inbound message meant
/// ~39k wheel operations per second at 640 MB/s of download -- visible as
/// `Sleep::poll` plus `parking_lot::lock_slow` in a profile. The deadline
/// guards a 300 s idle timeout, so moving it at most once per second is
/// indistinguishable in behaviour and drops the wheel traffic by ~4 orders
/// of magnitude on a busy peer.
const DEADLINE_GRANULARITY: Duration = Duration::from_secs(1);
/// Cadence at which a downloading session wakes to flush choking decisions.
const CHOKE_TICK: Duration = Duration::from_secs(10);
/// Most block requests a session holds back while an upload cap makes them
/// wait. Clients pipeline a few hundred at most (libtorrent's default ceiling
/// is 500); beyond this one is refused rather than queued, so a peer that
/// floods requests cannot make us buffer an unbounded list on its behalf.
const MAX_DEFERRED_REQUESTS: usize = 512;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use librqbit_utp::UtpSocketUdp;
use tokio_util::codec::Framed;

use crate::crypto::stream::CryptoStream;
use crate::disk::DiskManager;
use crate::peer::choking;
use crate::peer::download::DownloadState;
use crate::peer::extension::{self, PeerExt, OUR_UT_PEX_ID};
use crate::peer::message::Message;
use crate::torrent::meta::{PeerGuard, PeerStats, TorrentState, TorrentStatus};
use crate::torrent::ratelimit::{Admit, Dir, Gate};
use crate::wire::codec::BtCodec;

/// Build the BEP 9 reply for one requested block, or a reject if we cannot
/// serve it. Re-reading the metainfo per request is fine: requests are rare,
/// and the alternative is keeping every info dict resident forever.
fn serve_metadata_block(torrent: &Arc<TorrentState>, piece: u32) -> Vec<u8> {
    let total = torrent.meta.info_dict_len as usize;
    if total == 0 {
        return extension::build_metadata_reject(piece);
    }
    let bytes = match torrent.metainfo_bytes() {
        Some(b) => b,
        None => return extension::build_metadata_reject(piece),
    };
    let dict = match crate::torrent::metainfo::info_dict_from_bytes(&bytes) {
        Ok(d) => d,
        Err(_) => return extension::build_metadata_reject(piece),
    };
    // What the store holds must still be the torrent we advertised; serving a
    // mismatched slice would corrupt the peer's dict.
    if dict.len() != total {
        return extension::build_metadata_reject(piece);
    }
    let offset = piece as usize * extension::METADATA_BLOCK;
    if offset >= total {
        return extension::build_metadata_reject(piece);
    }
    let end = (offset + extension::METADATA_BLOCK).min(total);
    extension::build_metadata_data(piece, total, &dict[offset..end])
}

/// Whether a block request may be served now.
///
/// Checked when the request arrives AND again when a deferred one finally goes
/// out: a torrent paused, or a peer choked, while a request waited behind the
/// upload cap must not be served it afterwards.
fn may_serve(torrent: &TorrentState, stats: &PeerStats, index: u32, length: u32) -> bool {
    // Serve a piece we actually have, even while still downloading
    // (tit-for-tat). Seeding => we have all; downloading => check the
    // picker (verified pieces only). Without this, leechers on a hot
    // release we're still racing get every Request rejected until we
    // hit 100% — huge lost upload/ratio on exactly the hottest swarms.
    let have_requested = torrent.status.load(Ordering::Relaxed) == TorrentStatus::Seeding as u8
        || torrent
            .picker
            .get()
            .map_or(false, |pk| pk.lock().unwrap().has_piece(index));
    have_requested
        && length <= 16384
        && !stats.choked.load(Ordering::Relaxed)
        && !torrent.serving_suspended.load(Ordering::Relaxed)
        // A paused torrent serves nothing either. Same gap as the request
        // side: the flag was set and never read here, so a paused seed kept
        // uploading to the peers it was already connected to.
        && !torrent.is_paused.load(Ordering::Relaxed)
}

/// Read one block and put it on the wire, by sendfile when it can and the
/// buffered path otherwise. Answers false when the connection is broken and
/// the session must end.
#[allow(clippy::too_many_arguments)]
async fn serve_block(
    framed: &mut Framed<CryptoStream, BtCodec>,
    torrent: &Arc<TorrentState>,
    disk: &DiskManager,
    stats: &PeerStats,
    addr: SocketAddr,
    serve_zerocopy: bool,
    fast_ext: bool,
    index: u32,
    begin: u32,
    length: u32,
) -> bool {
    // Zero-copy sendfile fast-path: plaintext TCP peer whose
    // block lives in a single file. Splices from the page cache
    // to the socket, skipping the userspace copies the buffered
    // path pays. Any decline falls back to read_block below.
    if serve_zerocopy {
        if let Some((file, foff)) = disk.block_file(torrent, index, begin, length) {
            if crate::disk::is_block_resident(&file, foff) {
                if framed.flush().await.is_err() {
                    tracing::debug!("[peer-debug] {} BREAK flush-before-sendfile failed", addr);
                    return false;
                }
                let served = match framed.get_ref().plain_tcp() {
                    Some(sock) => crate::disk::serve_block_sendfile(
                        sock, &file, foff, index, begin, length as usize,
                    ).await,
                    None => Ok(false),
                };
                match served {
                    Ok(true) => {
                        let len = length as u64;
                        torrent.total_uploaded.fetch_add(len, Ordering::Relaxed);
                        stats.total_uploaded.fetch_add(len, Ordering::Relaxed);
                        stats.uploaded_last_tick.fetch_add(len, Ordering::Relaxed);
                        return true;
                    }
                    Ok(false) => {}
                    Err(_) => {
                        tracing::debug!("[peer-debug] {} BREAK sendfile mid-stream", addr);
                        return false;
                    }
                }
            }
        }
    }
    match disk.read_block(torrent, index, begin, length).await {
        Ok(data) => {
            let len = data.len() as u64;
            if framed
                .send(Message::Piece { index, begin, data })
                .await
                .is_err()
            {
                tracing::debug!("[peer-debug] {} BREAK send-piece failed (idx={}, begin={}, len={})", addr, index, begin, len);
                return false;
            }
            torrent.total_uploaded.fetch_add(len, Ordering::Relaxed);
            stats.total_uploaded.fetch_add(len, Ordering::Relaxed);
            stats
                .uploaded_last_tick
                .fetch_add(len, Ordering::Relaxed);
        }
        Err(_) => {
            if fast_ext {
                framed
                    .send(Message::Reject { index, begin, length })
                    .await
                    .ok();
            }
        }
    }
    true
}

pub async fn run(
    mut framed: Framed<CryptoStream, BtCodec>,
    addr: SocketAddr,
    torrent: Arc<TorrentState>,
    disk: Arc<DiskManager>,
    peer_id: [u8; 20],
    remote_peer_id: [u8; 20],
    is_encrypted: bool,
    fast_ext: bool,
    lt_ext: bool,
    utp_socket: Option<Arc<UtpSocketUdp>>,
    listen_port: u16,
) {
    let client = choking::client_from_peer_id(&remote_peer_id);
    tracing::debug!("[peer-debug] {} SESSION-START remote_peer_id={:02x?} client={:?} encrypted={} fast_ext={} lt_ext={}", addr, remote_peer_id, client, is_encrypted, fast_ext, lt_ext);
    let stats = Arc::new(PeerStats::new(addr, remote_peer_id, client, is_encrypted, fast_ext));
    // RAII: inserts into torrent.peer_stats and bumps peers_connected;
    // removes + decrements on drop (panic-safe).
    let guard = PeerGuard::new(torrent.clone(), stats.clone());

    let is_seeding = torrent.status.load(Ordering::Relaxed) == TorrentStatus::Seeding as u8;
    let num_pieces = torrent.meta.num_pieces();

    // Send bitfield / have_all.
    if is_seeding && fast_ext {
        if framed.send(Message::HaveAll).await.is_err() {
            tracing::debug!("[peer-debug] {} EARLY-RETURN send-haveall failed", addr);
            return;
        }
    } else {
        let bf = torrent.have_bitfield();
        if framed.send(Message::Bitfield { data: bf }).await.is_err() {
            tracing::debug!("[peer-debug] {} EARLY-RETURN send-bitfield failed", addr);
            return;
        }
    }
    // Immediate Unchoke so peers can Request right away. The choking engine
    // may re-choke via `choking_gen` if the peer underperforms.
    stats.choked.store(false, Ordering::Relaxed);
    if framed.send(Message::Unchoke).await.is_err() {
        tracing::debug!("[peer-debug] {} EARLY-RETURN send-unchoke failed", addr);
        return;
    }

    // BEP 10 extension handshake (skip on private trackers — BEP 27).
    let mut peer_ext = if lt_ext && torrent.meta.allows_peer_discovery() && listen_port != 0 {
        // Advertise the info dict size so peers know they can fetch it from us.
        let meta_size = if torrent.meta.info_dict_len > 0 {
            Some(torrent.meta.info_dict_len as usize)
        } else {
            None
        };
        let payload = Bytes::from(extension::build_extension_handshake(listen_port, meta_size, torrent.policy()));
        framed.send(Message::Extended { ext_id: 0, payload }).await.ok();
        Some(PeerExt::new())
    } else {
        None
    };

    let mut dl = DownloadState::new(torrent.clone(), disk.clone());

    // Have broadcasts only matter while we're downloading. Subscribing on
    // seeding torrents pointlessly wakes every task on every piece we
    // complete (we don't complete any) — see feedback_tokio_broadcast_cap.
    let mut have_rx = if torrent.picker.get().is_some() && !is_seeding {
        torrent.subscribe_have()
    } else {
        None
    };

    // Zero-copy sendfile serve only off non-ZFS storage (NVMe/XFS race). On ZFS
    // (hoard), sendfile bypasses the ARC on ZoL -> the served hot-set would fill
    // dumb page-cache LRU and starve the ARC; the buffered read()+cache path
    // keeps blocks in the ARC (compressed, scan-resistant, prefetch).
    let serve_zerocopy = !is_encrypted && !crate::disk::path_is_zfs(torrent.save_path.read().as_path());
    let mut local_choking_gen: u32 = stats.choking_gen.load(Ordering::Relaxed);

    // Both timers are built once and reset in place. Building them inside the
    // loop meant every turn allocated a `Sleep`, took tokio's timer-wheel lock
    // to register an entry, and took it again on drop to cancel it. That lock
    // (`tokio::runtime::time::Inner`) is one mutex for the whole process, and
    // with 67k sessions on 12 worker threads the contention measured ~30% of
    // the engine's CPU -- more than any BitTorrent work it was doing.
    let deadline = tokio::time::sleep(peer_idle_timeout(&torrent));
    tokio::pin!(deadline);
    // Last time the idle deadline was actually pushed out; see DEADLINE_GRANULARITY.
    let mut deadline_set_at = tokio::time::Instant::now();
    // Wake once per choking tick so the engine's decisions land on the wire
    // within the next tick cycle even when the peer is otherwise idle. A
    // seeding session has no choking decisions to flush, so its arm stays
    // disabled and never registers a timer at all.
    let choke_poll = tokio::time::sleep(CHOKE_TICK);
    tokio::pin!(choke_poll);
    // The filter generation this session last checked its peer against.
    let mut filter_gen = crate::ipfilter::generation();

    // ── Rate caps ──────────────────────────────────────────────────────────
    // Waiting for upload tokens INSIDE the request arm would hold the whole
    // loop: this task is also the only reader of the socket, so a capped seed
    // would stop reading the peer's Cancels, Haves and -- when we download
    // from the same peer -- its Pieces, throttling our download by our upload
    // cap and leaving a fast-extension peer without the answer it is owed.
    // Rejecting what cannot go now would be wrong the other way: the peer
    // asks again at once, and a Reject storm is all a cap would produce.
    //
    // So a request the cap holds back is queued here, in order, and served
    // from the top of the loop when its slot comes, while messages keep being
    // read. The slot is BOOKED (see `ratelimit`), so a session sleeps once per
    // block, never in a retry loop. Nothing here is allocated or armed for an
    // uncapped session: the queue stays empty and the timer stays `None`.
    let mut ul_gate = Gate::default();
    let mut ul_queue: std::collections::VecDeque<(u32, u32, u32)> = std::collections::VecDeque::new();
    let mut ul_wait: Option<tokio::time::Instant> = None;
    // The download side gates the REQUESTS we send: a block is booked when it
    // is asked for, which is when its bytes are decided, and the pipeline is
    // simply not refilled while the cap says wait.
    let mut dl_gate = Gate::default();
    let mut dl_wait: Option<tokio::time::Instant> = None;
    let mut rate_timer: Option<std::pin::Pin<Box<tokio::time::Sleep>>> = None;
    let mut rate_timer_at: Option<tokio::time::Instant> = None;

    loop {
        // A ban has to reach the peers already connected, not only the next
        // ones. Re-checked once per change of the list, never per message.
        let g = crate::ipfilter::generation();
        if g != filter_gen {
            filter_gen = g;
            if crate::ipfilter::blocked(addr.ip()) {
                crate::ipfilter::DROPPED.fetch_add(1, Ordering::Relaxed);
                break;
            }
        }
        // A flag that gated only NEW handshakes would leave the long-lived
        // encrypted sessions in place for hours — they are exactly the
        // persistent peers — so a measurement block would never reach a clean
        // state. Drop them here instead, on the next turn of their own loop.
        if is_encrypted && torrent.policy().block_mse() {
            crate::tracker::MSE_SESSIONS_DROPPED.fetch_add(1, Ordering::Relaxed);
            break;
        }
        // Flush any pending Choke/Unchoke produced by the choking engine.
        let cur_gen = stats.choking_gen.load(Ordering::Relaxed);
        if cur_gen != local_choking_gen {
            local_choking_gen = cur_gen;
            let we_choke = stats.choked.load(Ordering::Relaxed);
            let msg = if we_choke { Message::Choke } else { Message::Unchoke };
            if framed.send(msg).await.is_err() {
                tracing::debug!("[peer-debug] {} BREAK send-choke-msg failed", addr);
                break;
            }
        }

        // Deferred uploads whose slot has come.
        if !ul_queue.is_empty() && ul_wait.map_or(true, |t| tokio::time::Instant::now() >= t) {
            ul_wait = None;
            let mut broken = false;
            while let Some(&(index, begin, length)) = ul_queue.front() {
                if !may_serve(&torrent, &stats, index, length) {
                    ul_queue.pop_front();
                    if fast_ext && framed.send(Message::Reject { index, begin, length }).await.is_err() {
                        broken = true;
                        break;
                    }
                    continue;
                }
                match ul_gate.admit(length as u64, &torrent.rate_chain(Dir::Up)) {
                    Admit::Now => {
                        ul_queue.pop_front();
                        if !serve_block(&mut framed, &torrent, &disk, &stats, addr, serve_zerocopy, fast_ext, index, begin, length).await {
                            broken = true;
                            break;
                        }
                    }
                    Admit::Wait(t) => {
                        ul_wait = Some(t);
                        break;
                    }
                }
            }
            if broken {
                break;
            }
        }

        if dl.is_downloading() && !dl.am_interested && dl.should_be_interested() {
            dl.am_interested = true;
            framed.send(Message::Interested).await.ok();
        }
        if dl.is_downloading()
            && !dl.peer_choking
            && dl_wait.map_or(true, |t| tokio::time::Instant::now() >= t)
        {
            dl_wait = None;
            let chain = torrent.rate_chain(Dir::Down);
            let requests = dl.get_requests_gated(|len| match dl_gate.admit(len as u64, &chain) {
                Admit::Now => true,
                Admit::Wait(t) => {
                    dl_wait = Some(t);
                    false
                }
            });
            // SinkExt::send flushes after every message, so a pipelined peer
            // cost one write() syscall and a full task wake-up per 17-byte
            // Request. Feed the whole batch, then flush once.
            let mut queued = false;
            for (idx, off, len) in requests {
                if framed
                    .feed(Message::Request { index: idx, begin: off, length: len })
                    .await
                    .is_err()
                {
                    tracing::debug!("[peer-debug] {} BREAK send-request failed", addr);
                    break;
                }
                queued = true;
            }
            if queued && framed.flush().await.is_err() {
                tracing::debug!("[peer-debug] {} BREAK flush-requests failed", addr);
            }
        }

        // One timer for both caps, armed only while one of them makes us wait,
        // and moved only when the instant changes: resetting a Sleep is a
        // timer-wheel operation (see DEADLINE_GRANULARITY). A wait already in
        // the past is not re-armed -- it would fire at once, every turn.
        let now = tokio::time::Instant::now();
        let next = [if ul_queue.is_empty() { None } else { ul_wait }, dl_wait]
            .into_iter()
            .flatten()
            .filter(|t| *t > now)
            .min();
        if next != rate_timer_at {
            rate_timer_at = next;
            if let Some(at) = next {
                match rate_timer.as_mut() {
                    Some(t) => t.as_mut().reset(at),
                    None => rate_timer = Some(Box::pin(tokio::time::sleep_until(at))),
                }
            }
        }

        tokio::select! {
            // A capped block's slot has come; the top of the loop sends it.
            _ = async {
                match rate_timer.as_mut() {
                    Some(t) => t.as_mut().await,
                    None => std::future::pending().await,
                }
            }, if rate_timer_at.is_some() => {
                rate_timer_at = None;
                continue;
            }
            // Another peer's task asked us to introduce this one. Woken rather
            // than polled: an idle seeding session registers no timer, and a
            // hole left unmentioned for a minute has closed long before.
            _ = stats.punch_wake.notified() => {
                let waiting: Vec<std::net::SocketAddr> = match stats.punch_outbox.lock() {
                    Ok(mut q) => std::mem::take(&mut *q),
                    Err(_) => Vec::new(),
                };
                if let Some(id) = peer_ext.as_ref().and_then(|e| e.ut_holepunch_id) {
                    let mut broken = false;
                    for who in waiting {
                        let payload = Bytes::from(
                            crate::peer::holepunch::Punch::Connect(who).encode(),
                        );
                        if framed
                            .send(Message::Extended { ext_id: id, payload })
                            .await
                            .is_err()
                        {
                            broken = true;
                            break;
                        }
                    }
                    if broken {
                        tracing::debug!("[peer-debug] {} BREAK send-holepunch failed", addr);
                        break;
                    }
                }
            }
            msg = framed.next() => {
                match msg {
                    Some(Ok(message)) => {
                        // USEFUL traffic from the peer is what "not idle"
                        // means. A keep-alive is not traffic: BEP 3 has every
                        // client emit one every ~2 min, which is well inside
                        // the 300 s timeout, so counting it made the deadline
                        // unreachable and every connection immortal. Measured
                        // 15/09/2026 on the prod hoard: 16 k standing peers,
                        // p50 lastrcv 33 s (breathing) next to p50 bytes_sent
                        // 305 B (a handshake and nothing since), climbing
                        // ~350/h with no plateau. The two numbers are not a
                        // contradiction -- they are the portrait of a peer
                        // kept alive by its own keep-alives.
                        if pushes_idle_deadline(&message) {
                            let now = tokio::time::Instant::now();
                            if now.duration_since(deadline_set_at) >= DEADLINE_GRANULARITY {
                                deadline_set_at = now;
                                deadline.as_mut().reset(now + peer_idle_timeout(&torrent));
                            }
                        }
                        match message {
                            Message::Interested => {
                                stats.interested.store(true, Ordering::Relaxed);
                                guard.mark_interested(true);
                            }
                            Message::NotInterested => {
                                stats.interested.store(false, Ordering::Relaxed);
                                guard.mark_interested(false);
                            }
                            Message::Choke => dl.on_choke(),
                            Message::Unchoke => dl.on_unchoke(),
                            Message::Bitfield { data } => {
                                let count = choking::count_bitfield_pieces(&data, num_pieces);
                                stats.num_pieces_have.store(count, Ordering::Relaxed);
                                let is_seed = num_pieces > 0 && count >= num_pieces;
                                stats.is_seed.store(is_seed, Ordering::Relaxed);
                                dl.on_bitfield(&data);
                                if is_seed && we_are_complete(&torrent) {
                                    crate::peer::SEED_SEED_DROPPED
                                        .fetch_add(1, Ordering::Relaxed);
                                    break;
                                }
                            }
                            Message::HaveAll => {
                                stats.num_pieces_have.store(num_pieces, Ordering::Relaxed);
                                stats.is_seed.store(true, Ordering::Relaxed);
                                dl.on_have_all();
                                if we_are_complete(&torrent) {
                                    crate::peer::SEED_SEED_DROPPED
                                        .fetch_add(1, Ordering::Relaxed);
                                    break;
                                }
                            }
                            Message::Have { piece } => {
                                // Only count pieces we did not already know from
                                // this peer -> num_pieces_have can never exceed
                                // num_pieces (fixes >100% / 200% progress).
                                if dl.on_have(piece) {
                                    let prev = stats.num_pieces_have.fetch_add(1, Ordering::Relaxed);
                                    if num_pieces > 0 && prev + 1 >= num_pieces {
                                        stats.is_seed.store(true, Ordering::Relaxed);
                                        // Same rule as Bitfield/HaveAll, which
                                        // is where a peer that arrives complete
                                        // gets dropped. A peer that finishes
                                        // piece by piece while connected to us
                                        // reaches the same state by a different
                                        // road, and used to keep its socket for
                                        // ever: neither side can give the other
                                        // anything any more.
                                        if we_are_complete(&torrent) {
                                            crate::peer::SEED_SEED_DROPPED
                                                .fetch_add(1, Ordering::Relaxed);
                                            break;
                                        }
                                    }
                                }
                            }
                            Message::Request { index, begin, length } => {
                                if !may_serve(&torrent, &stats, index, length) {
                                    if fast_ext {
                                        framed
                                            .send(Message::Reject { index, begin, length })
                                            .await
                                            .ok();
                                    }
                                    continue;
                                }
                                // Behind requests already waiting on the cap:
                                // queued after them, so blocks go out in the
                                // order they were asked for.
                                let admit = if ul_queue.is_empty() {
                                    ul_gate.admit(length as u64, &torrent.rate_chain(Dir::Up))
                                } else {
                                    Admit::Wait(ul_wait.unwrap_or_else(tokio::time::Instant::now))
                                };
                                match admit {
                                    Admit::Now => {
                                        if !serve_block(&mut framed, &torrent, &disk, &stats, addr, serve_zerocopy, fast_ext, index, begin, length).await {
                                            break;
                                        }
                                    }
                                    Admit::Wait(t) => {
                                        if ul_queue.len() >= MAX_DEFERRED_REQUESTS {
                                            // Over the ceiling: refused, which
                                            // a fast peer is told and a plain
                                            // one finds out by its own timeout.
                                            if fast_ext {
                                                framed
                                                    .send(Message::Reject { index, begin, length })
                                                    .await
                                                    .ok();
                                            }
                                        } else {
                                            ul_queue.push_back((index, begin, length));
                                            if ul_wait.is_none() {
                                                ul_wait = Some(t);
                                            }
                                        }
                                    }
                                }
                            }
                            // BEP 3 cancel. Only a request still waiting on the
                            // upload cap can be withdrawn -- anything else was
                            // served the moment it arrived. BEP 6 owes a fast
                            // peer exactly one answer per request, so the
                            // withdrawn one is answered with a Reject.
                            Message::Cancel { index, begin, length } => {
                                if let Some(i) = ul_queue.iter().position(|r| *r == (index, begin, length)) {
                                    ul_queue.remove(i);
                                    if fast_ext {
                                        framed
                                            .send(Message::Reject { index, begin, length })
                                            .await
                                            .ok();
                                    }
                                }
                            }
                            Message::Piece { index, begin, data } => {
                                let len = data.len() as u64;
                                if let Some(completed) = dl.on_piece(index, begin, &data).await {
                                    framed.send(Message::Have { piece: completed }).await.ok();
                                }
                                stats.total_downloaded.fetch_add(len, Ordering::Relaxed);
                            }
                            Message::Extended { ext_id, payload } => {
                                if ext_id == 0 {
                                    if let Some(parsed) = extension::parse_extension_handshake_full(
                                        &payload,
                                        torrent.policy(),
                                    ) {
                                        // Published on the stats rather than kept
                                        // in this task alone: another peer's task
                                        // has to know whether this one can be
                                        // introduced to anybody.
                                        stats.supports_holepunch.store(
                                            parsed.ut_holepunch_id.is_some(),
                                            Ordering::Relaxed,
                                        );
                                        if let Some(ext) = peer_ext.as_mut() {
                                            ext.ut_pex_id = parsed.ut_pex_id;
                                            ext.ut_metadata_id = parsed.ut_metadata_id;
                                            ext.ut_holepunch_id = parsed.ut_holepunch_id;
                                            ext.metadata_size = parsed.metadata_size;
                                        }
                                    }
                                } else if ext_id == extension::OUR_UT_METADATA_ID {
                                    // BEP 9 request from a peer resolving a
                                    // magnet. Re-read the metainfo rather than
                                    // hold the dict in memory; on any problem
                                    // reply reject, which is a valid answer and
                                    // lets the peer move on to someone else.
                                    if let Some(m) = extension::parse_metadata_message(&payload) {
                                        if m.msg_type == extension::METADATA_REQUEST {
                                            let reply = serve_metadata_block(&torrent, m.piece);
                                            framed
                                                .send(Message::Extended {
                                                    ext_id: extension::OUR_UT_METADATA_ID,
                                                    payload: Bytes::from(reply),
                                                })
                                                .await
                                                .ok();
                                        }
                                    }
                                } else if ext_id == crate::peer::holepunch::OUR_UT_HOLEPUNCH_ID
                                    && peer_sources_allowed(&torrent)
                                {
                                    use crate::peer::holepunch::{Error as PunchError, Punch};
                                    match Punch::decode(&payload) {
                                        // Somebody we are connected to says a
                                        // peer is expecting us right now. The
                                        // whole value of the message is in
                                        // dialling immediately: the other side
                                        // is opening its hole at this instant
                                        // and it closes in seconds.
                                        Some(Punch::Connect(addr)) => {
                                            if crate::peer::holepunch::is_punchable(&addr) {
                                                crate::tracker::enqueue_dial(addr, torrent.clone());
                                            }
                                        }
                                        // Somebody asks to be introduced. The
                                        // register is `torrent.peer_stats`, which
                                        // already holds every peer this torrent
                                        // has and is emptied by the RAII guard
                                        // when a session ends.
                                        Some(Punch::Rendezvous(target)) => {
                                            let (connected, supports) =
                                                match torrent.peer_stats.get(&target) {
                                                    Some(e) => (
                                                        true,
                                                        e.supports_holepunch
                                                            .load(Ordering::Relaxed),
                                                    ),
                                                    None => (false, false),
                                                };
                                            let reply = crate::peer::holepunch::answer_rendezvous(
                                                addr, target, connected, supports,
                                            );
                                            // BOTH sides, or neither: the asker
                                            // dials into a closed NAT unless the
                                            // other end punches at the same moment.
                                            // Queued here and sent by that peer's
                                            // own task, which owns its socket.
                                            if matches!(reply, Punch::Connect(_)) {
                                                if let Some(e) = torrent.peer_stats.get(&target) {
                                                    e.queue_punch(addr);
                                                }
                                            }
                                            // Their id, not ours: an extended
                                            // message is addressed with the number
                                            // the RECEIVER advertised.
                                            if let Some(id) =
                                                peer_ext.as_ref().and_then(|e| e.ut_holepunch_id)
                                            {
                                                framed
                                                    .send(Message::Extended {
                                                        ext_id: id,
                                                        payload: Bytes::from(reply.encode()),
                                                    })
                                                    .await
                                                    .ok();
                                            }
                                        }
                                        Some(Punch::Error(addr, why)) => {
                                            tracing::debug!(%addr, reason = why.reason(), "hole punch refused");
                                        }
                                        None => {}
                                    }
                                } else if ext_id == OUR_UT_PEX_ID && peer_sources_allowed(&torrent) {
                                    let new_peers = extension::parse_pex(&payload, torrent.policy());
                                    if !new_peers.is_empty() {
                                        torrent.pex_peers_discovered.fetch_add(
                                            new_peers.len() as u64,
                                            Ordering::Relaxed,
                                        );
                                        for _a in new_peers { /* PEX-triggered dial disabled: session::run future non-Send. Peers reachable via tracker/DHT. */ }
                                    }
                                }
                            }
                            Message::KeepAlive => {}
                            _ => {}
                        }
                    }
                    Some(Err(e)) => {
                        tracing::debug!("[peer-debug] {} BREAK framed-error: {:?}", addr, e);
                        break;
                    }
                    None => {
                        tracing::debug!("[peer-debug] {} BREAK framed-none (peer closed)", addr);
                        break;
                    }
                }
            }
            piece = async {
                match have_rx.as_mut() {
                    // Hand the Result out instead of `.ok()`. An error here is
                    // not "nothing to send": once the torrent reaches Seeding,
                    // `release_have_tx` drops the sender, so every surviving
                    // Receiver returns Closed *immediately, forever*. With
                    // `.ok()` that became a select! arm ready on every single
                    // turn, and the body's `if let Some` silently swallowed it
                    // without disarming -- a busy loop for the rest of the
                    // session's life (up to the idle timeout, 300 s by default).
                    // Measured in prod 2026-09-13: `broadcast::recv_ref` was
                    // 5.3% of all CPU, ~11M recv/s against a legitimate Have
                    // rate of ~10^3/s.
                    Some(rx) => Some(rx.recv().await),
                    None => std::future::pending().await,
                }
            } => {
                match piece {
                    Some(Ok(piece)) => {
                        framed.send(Message::Have { piece }).await.ok();
                    }
                    // The 256-slot ring overflowed and we missed some Have's.
                    // Transient, and the peer can still ask for those pieces --
                    // keep listening. (`.ok()` used to hide this too.)
                    Some(Err(tokio::sync::broadcast::error::RecvError::Lagged(n))) => {
                        crate::peer::HAVE_RX_LAGGED.fetch_add(n, Ordering::Relaxed);
                    }
                    // The sender is gone for good: the torrent is seeding and
                    // will never announce another piece on this channel.
                    // Disarm the arm so `pending()` parks it instead.
                    Some(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                        have_rx = None;
                        crate::peer::HAVE_RX_DISARMED.fetch_add(1, Ordering::Relaxed);
                    }
                    // Unreachable: `pending()` never resolves.
                    None => {}
                }
            }
            _ = &mut choke_poll, if !is_seeding => {
                choke_poll.as_mut().reset(tokio::time::Instant::now() + CHOKE_TICK);
                // fall through — top of loop flushes pending Choke/Unchoke
                continue;
            }
            _ = &mut deadline => {
                tracing::debug!("[peer-debug] {} BREAK deadline-300s elapsed", addr);
                break;
            }
        }
    }

    let dl_total = stats.total_downloaded.load(Ordering::Relaxed);
    let ul_total = stats.total_uploaded.load(Ordering::Relaxed);
    tracing::debug!("[peer-debug] {} SESSION-END dl={} ul={}", addr, dl_total, ul_total);
    dl.on_disconnect();
    // guard drops here -> peer_stats.remove + peers_connected/interested decrement
    drop(guard);
}

#[cfg(test)]
mod idle_deadline_tests {
    use super::*;
    use bytes::Bytes;

    #[test]
    fn a_keep_alive_does_not_push_the_deadline() {
        assert!(!pushes_idle_deadline(&Message::KeepAlive));
    }

    #[test]
    fn a_peer_that_asks_for_data_is_alive() {
        assert!(pushes_idle_deadline(&Message::Request {
            index: 0,
            begin: 0,
            length: 16384
        }));
        assert!(pushes_idle_deadline(&Message::Interested));
        assert!(pushes_idle_deadline(&Message::Have { piece: 3 }));
    }

    #[test]
    fn every_other_frame_counts_as_activity() {
        for m in [
            Message::Choke,
            Message::Unchoke,
            Message::NotInterested,
            Message::Bitfield { data: Bytes::new() },
            Message::HaveAll,
            Message::HaveNone,
            Message::Extended { ext_id: 0, payload: Bytes::new() },
            Message::Unknown { id: 99, payload: Bytes::new() },
        ] {
            assert!(pushes_idle_deadline(&m), "{m:?} should push the deadline");
        }
    }
}
