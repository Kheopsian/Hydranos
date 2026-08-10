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
use crate::wire::codec::BtCodec;

/// Build the BEP 9 reply for one requested block, or a reject if we cannot
/// serve it. Reading the .torrent per request is fine: requests are rare, and
/// the alternative is keeping every info dict resident forever.
fn serve_metadata_block(torrent: &Arc<TorrentState>, piece: u32) -> Vec<u8> {
    let total = torrent.meta.info_dict_len as usize;
    if total == 0 {
        return extension::build_metadata_reject(piece);
    }
    let dict = match crate::torrent::metainfo::info_dict_from_file(&torrent.torrent_file_path) {
        Ok(d) => d,
        Err(_) => return extension::build_metadata_reject(piece),
    };
    // The file on disk must still be the torrent we advertised; if it changed
    // underneath us, serving a mismatched slice would corrupt the peer's dict.
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
    let mut peer_ext = if lt_ext && !torrent.meta.private && listen_port != 0 {
        // Advertise the info dict size so peers know they can fetch it from us.
        let meta_size = if torrent.meta.info_dict_len > 0 {
            Some(torrent.meta.info_dict_len as usize)
        } else {
            None
        };
        let payload = Bytes::from(extension::build_extension_handshake(listen_port, meta_size));
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
        torrent.have_tx.as_ref().map(|tx| tx.subscribe())
    } else {
        None
    };

    // Zero-copy sendfile serve only off non-ZFS storage (NVMe/XFS race). On ZFS
    // (hoard), sendfile bypasses the ARC on ZoL -> the served hot-set would fill
    // dumb page-cache LRU and starve the ARC; the buffered read()+cache path
    // keeps blocks in the ARC (compressed, scan-resistant, prefetch).
    let serve_zerocopy = !is_encrypted && !crate::disk::path_is_zfs(torrent.save_path.read().as_path());
    let mut local_choking_gen: u32 = stats.choking_gen.load(Ordering::Relaxed);

    loop {
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

        if dl.is_downloading() && !dl.am_interested && dl.should_be_interested() {
            dl.am_interested = true;
            framed.send(Message::Interested).await.ok();
        }
        if dl.is_downloading() && !dl.peer_choking {
            for (idx, off, len) in dl.get_requests() {
                if framed
                    .send(Message::Request { index: idx, begin: off, length: len })
                    .await
                    .is_err()
                {
                    tracing::debug!("[peer-debug] {} BREAK send-request failed", addr);
                    break;
                }
            }
        }

        let deadline = tokio::time::sleep(Duration::from_secs(300));
        tokio::pin!(deadline);
        // Wake once per choking tick so the engine's decisions land on the wire
        // within the next tick cycle even when the peer is otherwise idle.
        let choke_poll = async move { if is_seeding { std::future::pending::<()>().await } else { tokio::time::sleep(Duration::from_secs(10)).await } };
        tokio::pin!(choke_poll);

        tokio::select! {
            msg = framed.next() => {
                match msg {
                    Some(Ok(message)) => {
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
                            }
                            Message::HaveAll => {
                                stats.num_pieces_have.store(num_pieces, Ordering::Relaxed);
                                stats.is_seed.store(true, Ordering::Relaxed);
                                dl.on_have_all();
                            }
                            Message::Have { piece } => {
                                // Only count pieces we did not already know from
                                // this peer -> num_pieces_have can never exceed
                                // num_pieces (fixes >100% / 200% progress).
                                if dl.on_have(piece) {
                                    let prev = stats.num_pieces_have.fetch_add(1, Ordering::Relaxed);
                                    if num_pieces > 0 && prev + 1 >= num_pieces {
                                        stats.is_seed.store(true, Ordering::Relaxed);
                                    }
                                }
                            }
                            Message::Request { index, begin, length } => {
                                let cur_status = torrent.status.load(Ordering::Relaxed);
                                let we_choke = stats.choked.load(Ordering::Relaxed);
                                // Serve a piece we actually have, even while still downloading
                                // (tit-for-tat). Seeding => we have all; downloading => check the
                                // picker (verified pieces only). Without this, leechers on a hot
                                // release we're still racing get every Request rejected until we
                                // hit 100% — huge lost upload/ratio on exactly the hottest swarms.
                                let have_requested = cur_status == TorrentStatus::Seeding as u8
                                    || torrent
                                        .picker
                                        .get()
                                        .map_or(false, |pk| pk.lock().unwrap().has_piece(index));
                                if !have_requested
                                    || length > 16384
                                    || we_choke
                                    || torrent.serving_suspended.load(Ordering::Relaxed)
                                {
                                    if fast_ext {
                                        framed
                                            .send(Message::Reject { index, begin, length })
                                            .await
                                            .ok();
                                    }
                                    continue;
                                }
                                // Zero-copy sendfile fast-path: plaintext TCP peer whose
                                // block lives in a single file. Splices from the page cache
                                // to the socket, skipping the userspace copies the buffered
                                // path pays. Any decline falls back to read_block below.
                                if serve_zerocopy {
                                    if let Some((file, foff)) = disk.block_file(&torrent, index, begin, length) {
                                        if crate::disk::is_block_resident(&file, foff) {
                                            if framed.flush().await.is_err() {
                                                tracing::debug!("[peer-debug] {} BREAK flush-before-sendfile failed", addr);
                                                break;
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
                                                    continue;
                                                }
                                                Ok(false) => {}
                                                Err(_) => {
                                                    tracing::debug!("[peer-debug] {} BREAK sendfile mid-stream", addr);
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                }
                                match disk.read_block(&torrent, index, begin, length).await {
                                    Ok(data) => {
                                        let len = data.len() as u64;
                                        if framed
                                            .send(Message::Piece { index, begin, data })
                                            .await
                                            .is_err()
                                        {
                                            tracing::debug!("[peer-debug] {} BREAK send-piece failed (idx={}, begin={}, len={})", addr, index, begin, len);
                                            break;
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
                                    if let Some(ext) = peer_ext.as_mut() {
                                        ext.ut_pex_id = extension::parse_extension_handshake(&payload);
                                    }
                                } else if ext_id == extension::OUR_UT_METADATA_ID {
                                    // BEP 9 request from a peer resolving a
                                    // magnet. Re-read the .torrent rather than
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
                                } else if ext_id == OUR_UT_PEX_ID {
                                    let new_peers = extension::parse_pex(&payload);
                                    if !new_peers.is_empty() {
                                        crate::tracker::PEX_PEERS_DISCOVERED.fetch_add(
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
                    Some(rx) => rx.recv().await.ok(),
                    None => std::future::pending().await,
                }
            } => {
                if let Some(piece) = piece {
                    framed.send(Message::Have { piece }).await.ok();
                }
            }
            _ = &mut choke_poll => {
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
