//! BEP 9 metadata resolution: turn a bare info hash into the raw info dict.
//!
//! Kept apart from `peer::session` on purpose. A session assumes the metadata
//! already exists -- it opens with a bitfield, drives a piece picker and writes
//! through the disk manager. While resolving a magnet none of that exists yet,
//! so this path does the bare minimum (handshake, BEP 10, BEP 9) and never
//! touches the disk.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::stream::FuturesUnordered;
use futures::{SinkExt, StreamExt};
use librqbit_utp::UtpSocketUdp;
use sha1::{Digest, Sha1};
use tokio_util::codec::Framed;
use tracing::debug;

use crate::peer::extension::{self, METADATA_BLOCK, METADATA_DATA, METADATA_REJECT};
use crate::peer::message::Message;
use crate::wire::codec::BtCodec;

/// Cap on the info dict size a peer may claim. The peer is unauthenticated and
/// we allocate on its word alone; real dicts, even for huge multi-file
/// torrents, stay far below this.
pub const MAX_METADATA_SIZE: usize = 8 * 1024 * 1024;

/// How long one peer gets before we write it off. Callers race several peers,
/// so a slow one costs nothing but its own slot.
pub const PEER_TIMEOUT: Duration = Duration::from_secs(30);

/// How many peers we dial at once while racing.
pub const DEFAULT_CONCURRENCY: usize = 8;

/// Ask one peer for the info dict and return it only if it hashes to
/// `info_hash`.
pub async fn fetch_from_peer(
    addr: SocketAddr,
    info_hash: [u8; 20],
    peer_id: [u8; 20],
    utp_socket: Option<Arc<UtpSocketUdp>>,
    listen_port: u16,
    source_fwmark: u32,
) -> Result<Vec<u8>, String> {
    let (cs, _fast_ext, lt_ext, _remote_peer_id, _encrypted) =
        crate::tracker::open_peer(addr, &utp_socket, &info_hash, &peer_id, source_fwmark, false)
            .await
            .ok_or_else(|| "no connection".to_string())?;
    if !lt_ext {
        return Err("peer does not speak BEP 10".into());
    }
    let mut framed = Framed::new(cs, BtCodec::new());

    // Unconditional extension handshake, unlike a normal session: BEP 27 turns
    // off DHT and PEX for private torrents, but whether this torrent is private
    // is itself a key inside the dict we are trying to fetch. We cannot know it
    // yet, so we handshake regardless and simply never send PEX on this
    // connection. BEP 27 does not restrict ut_metadata.
    //
    // No metadata_size: we are the one asking. The id we advertise is what the
    // peer must use to send blocks back to us.
    let hs = extension::build_extension_handshake(listen_port, None);
    framed
        .send(Message::Extended { ext_id: 0, payload: Bytes::from(hs) })
        .await
        .map_err(|e| e.to_string())?;

    let mut total: Option<usize> = None;
    let mut dict: Vec<u8> = Vec::new();
    let mut have: Vec<bool> = Vec::new();
    let mut filled = 0usize;

    while let Some(frame) = framed.next().await {
        let msg = frame.map_err(|e| e.to_string())?;
        match msg {
            Message::Extended { ext_id: 0, payload } => {
                if total.is_some() {
                    // A second handshake tells us nothing new; re-requesting on
                    // it would just duplicate every block in flight.
                    continue;
                }
                let peer_hs = extension::parse_extension_handshake_full(&payload)
                    .ok_or_else(|| "unreadable extension handshake".to_string())?;
                let their_id = peer_hs
                    .ut_metadata_id
                    .ok_or_else(|| "peer does not offer ut_metadata".to_string())?;
                let size = peer_hs
                    .metadata_size
                    .ok_or_else(|| "peer offers ut_metadata but no metadata_size".to_string())?;
                if size > MAX_METADATA_SIZE {
                    return Err(format!("metadata_size {} over the {} cap", size, MAX_METADATA_SIZE));
                }
                let blocks = extension::metadata_block_count(size) as usize;
                dict = vec![0u8; size];
                have = vec![false; blocks];
                total = Some(size);
                // Request every block up front: a dict is a handful of 16 KiB
                // pieces and peers pipeline them without complaint.
                for piece in 0..blocks as u32 {
                    let req = extension::build_metadata_request(piece);
                    framed
                        .send(Message::Extended { ext_id: their_id, payload: Bytes::from(req) })
                        .await
                        .map_err(|e| e.to_string())?;
                }
            }
            // Peers address us with the id *we* advertised.
            Message::Extended { ext_id, payload } if ext_id == extension::OUR_UT_METADATA_ID => {
                let size = match total {
                    Some(s) => s,
                    // Data before we knew the size: nothing to place it in.
                    None => continue,
                };
                let m = extension::parse_metadata_message(&payload)
                    .ok_or_else(|| "malformed ut_metadata message".to_string())?;
                if m.msg_type == METADATA_REJECT {
                    return Err(format!("peer rejected block {}", m.piece));
                }
                if m.msg_type != METADATA_DATA {
                    continue;
                }
                let idx = m.piece as usize;
                if idx >= have.len() || have[idx] {
                    continue;
                }
                let block = payload.get(m.data_offset..).unwrap_or(&[]);
                let offset = idx * METADATA_BLOCK;
                let expected = METADATA_BLOCK.min(size - offset);
                if block.len() != expected {
                    return Err(format!(
                        "block {} is {} bytes, expected {}",
                        m.piece,
                        block.len(),
                        expected
                    ));
                }
                dict[offset..offset + expected].copy_from_slice(block);
                have[idx] = true;
                filled += 1;
                if filled == have.len() {
                    break;
                }
            }
            _ => {}
        }
    }

    if total.is_none() || filled != have.len() {
        return Err("connection ended before the dict was complete".into());
    }

    // The whole point of the exercise. A peer that hands us bytes hashing to
    // something else is broken or hostile; either way they are worthless, and
    // accepting them would poison the torrent we are about to create.
    let mut hasher = Sha1::new();
    hasher.update(&dict);
    let digest: [u8; 20] = hasher.finalize().into();
    if digest != info_hash {
        return Err("info dict does not hash to the requested info hash".into());
    }

    Ok(dict)
}

/// Race peers for the info dict; the first verified answer wins.
pub async fn fetch(
    peers: &[SocketAddr],
    info_hash: [u8; 20],
    peer_id: [u8; 20],
    utp_socket: Option<Arc<UtpSocketUdp>>,
    listen_port: u16,
    source_fwmark: u32,
    concurrency: usize,
) -> Result<Vec<u8>, String> {
    if peers.is_empty() {
        return Err("no peers to ask".into());
    }
    let width = concurrency.max(1);
    for wave in peers.chunks(width) {
        let mut inflight = FuturesUnordered::new();
        for addr in wave {
            let addr = *addr;
            let utp = utp_socket.clone();
            inflight.push(async move {
                let attempt =
                    fetch_from_peer(addr, info_hash, peer_id, utp, listen_port, source_fwmark);
                match tokio::time::timeout(PEER_TIMEOUT, attempt).await {
                    Ok(r) => (addr, r),
                    Err(_) => (addr, Err("timed out".to_string())),
                }
            });
        }
        while let Some((addr, result)) = inflight.next().await {
            match result {
                Ok(dict) => return Ok(dict),
                Err(e) => debug!("[metadata] {} failed: {}", addr, e),
            }
        }
    }
    Err("no peer served the metadata".into())
}
