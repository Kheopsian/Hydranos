use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::peer::transport::PeerTransport;

const PROTOCOL: &[u8] = b"BitTorrent protocol";
const RESERVED_FAST: u8 = 0x04;      // BEP 6 Fast Extension (reserved[7])
const RESERVED_EXTENDED: u8 = 0x10;  // BEP 10 Extension Protocol (reserved[5])

pub struct HandshakeResult {
    pub peer_id: [u8; 20],
    pub info_hash: [u8; 20],
    pub fast_extension: bool,
    pub extended_protocol: bool,
}

/// Perform outgoing handshake: we send first, then read theirs.
pub async fn outgoing(
    stream: &mut PeerTransport,
    our_info_hash: &[u8; 20],
    our_peer_id: &[u8; 20],
) -> Result<HandshakeResult, String> {
    send_handshake(stream, our_info_hash, our_peer_id).await?;
    let result = read_handshake(stream).await?;
    if &result.info_hash != our_info_hash {
        return Err("info_hash mismatch".into());
    }
    Ok(result)
}

/// Perform incoming handshake: read theirs first, then respond.
pub async fn incoming(
    stream: &mut PeerTransport,
    our_peer_id: &[u8; 20],
    info_hash_lookup: impl Fn(&[u8; 20]) -> bool,
) -> Result<HandshakeResult, String> {
    let result = read_handshake(stream).await?;
    if !info_hash_lookup(&result.info_hash) {
        return Err("unknown info_hash".into());
    }
    send_handshake(stream, &result.info_hash, our_peer_id).await?;
    Ok(result)
}

async fn send_handshake(
    stream: &mut PeerTransport,
    info_hash: &[u8; 20],
    peer_id: &[u8; 20],
) -> Result<(), String> {
    let mut buf = Vec::with_capacity(68);
    buf.push(19u8); // pstrlen
    buf.extend_from_slice(PROTOCOL);
    let mut reserved = [0u8; 8];
    reserved[7] |= RESERVED_FAST;     // BEP 6
    reserved[5] |= RESERVED_EXTENDED; // BEP 10
    buf.extend_from_slice(&reserved);
    buf.extend_from_slice(info_hash);
    buf.extend_from_slice(peer_id);
    stream.write_all(&buf).await.map_err(|e| e.to_string())
}

async fn read_handshake(stream: &mut PeerTransport) -> Result<HandshakeResult, String> {
    let mut buf = [0u8; 68];
    stream.read_exact(&mut buf).await.map_err(|e| e.to_string())?;
    if buf[0] != 19 || &buf[1..20] != PROTOCOL {
        return Err("invalid protocol string".into());
    }
    let reserved = &buf[20..28];
    let fast_extension = (reserved[7] & RESERVED_FAST) != 0;
    let extended_protocol = (reserved[5] & RESERVED_EXTENDED) != 0;
    let mut info_hash = [0u8; 20];
    let mut peer_id = [0u8; 20];
    info_hash.copy_from_slice(&buf[28..48]);
    peer_id.copy_from_slice(&buf[48..68]);
    Ok(HandshakeResult { peer_id, info_hash, fast_extension, extended_protocol })
}
