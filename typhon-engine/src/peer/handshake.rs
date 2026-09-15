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
    // Self-connection guard (dynamic, IP-agnostic): if the peer handshakes back
    // with OUR OWN peer_id, the tracker/DHT handed us our own listener. Abort so
    // we never loop back onto ourselves — works even when our public IP just
    // changed and the SELF_IPS pre-filter is momentarily stale. Cross-engine
    // dials (race<->hoard) use DISTINCT peer_ids so they pass.
    if &result.peer_id == our_peer_id {
        return Err("self-connection (own peer_id)".into());
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

/// The 68 bytes of a BEP 3 handshake.
///
/// `<pstrlen=19><"BitTorrent protocol"><8 reserved><info_hash><peer_id>`.
/// The length is fixed and the far side reads exactly that many bytes, so a
/// short peer id here makes a 65-byte handshake and the peer waits forever for
/// three bytes that never come. Hence `[u8; 20]` rather than a slice: the type
/// is what keeps the promise.
pub fn build_handshake(info_hash: &[u8; 20], peer_id: &[u8; 20]) -> [u8; 68] {
    let mut buf = [0u8; 68];
    buf[0] = 19; // pstrlen
    buf[1..20].copy_from_slice(PROTOCOL);
    // Reserved bits we claim. Everything else stays zero: advertising an
    // extension we do not implement makes a peer wait for messages we will
    // never send.
    buf[27] |= RESERVED_FAST; // BEP 6, reserved[7]
    buf[25] |= RESERVED_EXTENDED; // BEP 10, reserved[5]
    buf[28..48].copy_from_slice(info_hash);
    buf[48..68].copy_from_slice(peer_id);
    buf
}

/// Read the 68 bytes back, or say why they are not a handshake.
pub fn parse_handshake(buf: &[u8; 68]) -> Result<HandshakeResult, String> {
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

async fn send_handshake(
    stream: &mut PeerTransport,
    info_hash: &[u8; 20],
    peer_id: &[u8; 20],
) -> Result<(), String> {
    let buf = build_handshake(info_hash, peer_id);
    stream.write_all(&buf).await.map_err(|e| e.to_string())
}

async fn read_handshake(stream: &mut PeerTransport) -> Result<HandshakeResult, String> {
    let mut buf = [0u8; 68];
    stream.read_exact(&mut buf).await.map_err(|e| e.to_string())?;
    parse_handshake(&buf)
}
