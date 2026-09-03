//! BEP 10 extension protocol handshake + BEP 11 PEX (Peer Exchange)
//! + BEP 9 ut_metadata (fetching an info dict from the swarm).
//!
//! On extension handshake: peer sends `extended_id=0` with bencoded dict
//! containing `m: {ut_pex: <their_id>, ...}` (their_id is the ID we MUST use to send them
//! ut_pex messages). We reply with our own m-dict (we use ut_pex=1).
//!
//! On PEX received (extended_id == 1, the id we advertised): bencoded dict
//! `{added: <compact 6-byte peers>, added.f: <flag bytes>, dropped: <compact peers>}`.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use std::collections::BTreeMap;

use super::bencode::{Bencode, decode, decode_prefix};

/// Reserved bits handshake byte for LTEP (BEP 10): reserved[5] |= 0x10.
pub const RESERVED_LTEP: u8 = 0x10;

/// Our advertised extended message id for ut_pex.
pub const OUR_UT_PEX_ID: u8 = 1;

/// Our advertised extended message id for ut_metadata (BEP 9).
pub const OUR_UT_METADATA_ID: u8 = 2;

/// BEP 9 splits the info dict into fixed 16 KiB blocks; only the last is short.
pub const METADATA_BLOCK: usize = 16384;

/// BEP 9 msg_type values.
pub const METADATA_REQUEST: i64 = 0;
pub const METADATA_DATA: i64 = 1;
pub const METADATA_REJECT: i64 = 2;

/// How many 16 KiB blocks an info dict of `size` bytes is split into.
pub fn metadata_block_count(size: usize) -> u32 {
    ((size + METADATA_BLOCK - 1) / METADATA_BLOCK) as u32
}

/// Per-connection extension state.
pub struct PeerExt {
    /// Their extended message id for ut_pex (extracted from their handshake.m.ut_pex).
    pub ut_pex_id: Option<u8>,
    /// Their extended message id for ut_metadata, if they carry BEP 9.
    pub ut_metadata_id: Option<u8>,
    /// Info dict size they advertised, if any.
    pub metadata_size: Option<usize>,
    /// Last time we sent a PEX message to this peer.
    pub last_pex_sent: Option<Instant>,
    /// Peers we have already advertised to this peer (so we can compute added/dropped deltas).
    pub sent_peers: HashSet<SocketAddr>,
}

impl PeerExt {
    pub fn new() -> Self {
        Self {
            ut_pex_id: None,
            ut_metadata_id: None,
            metadata_size: None,
            last_pex_sent: None,
            sent_peers: HashSet::new(),
        }
    }
}

/// Build the BEP 10 extension handshake payload (without the leading extended_id=0 byte).
/// Caller prepends 0x00 (extended_id 0 = extension handshake) before sending.
/// `metadata_size` is the size of our info dict when we hold it (so peers know
/// they can fetch it from us). Pass `None` while resolving a magnet: we still
/// advertise ut_metadata, because the id we publish here is the one peers must
/// use to send data *back* to us.
pub fn build_extension_handshake(listen_port: u16, metadata_size: Option<usize>) -> Vec<u8> {
    let mut m = BTreeMap::new();
    // Left out entirely when PEX is off, rather than advertised and ignored.
    if pex_enabled() {
        m.insert(b"ut_pex".to_vec(), Bencode::Int(OUR_UT_PEX_ID as i64));
    }
    m.insert(b"ut_metadata".to_vec(), Bencode::Int(OUR_UT_METADATA_ID as i64));

    let mut root = BTreeMap::new();
    root.insert(b"m".to_vec(), Bencode::Dict(m));
    if let Some(size) = metadata_size {
        root.insert(b"metadata_size".to_vec(), Bencode::Int(size as i64));
    }
    root.insert(b"p".to_vec(), Bencode::Int(listen_port as i64));
    root.insert(b"v".to_vec(), Bencode::Bytes(b"typhon 0.2".to_vec()));
    root.insert(b"reqq".to_vec(), Bencode::Int(250));

    Bencode::Dict(root).to_vec()
}

/// Parse a peer's extension handshake. Returns their ut_pex extended id if present.
pub fn parse_extension_handshake(payload: &[u8]) -> Option<u8> {
    parse_extension_handshake_full(payload)?.ut_pex_id
}

/// What a peer advertised in its BEP 10 handshake.
pub struct ExtHandshake {
    pub ut_pex_id: Option<u8>,
    pub ut_metadata_id: Option<u8>,
    /// Total size of their info dict, from the top-level `metadata_size` key.
    pub metadata_size: Option<usize>,
}

/// Parse a peer's extension handshake. Absent or zero ids mean "not offered":
/// 0 is how a peer disables an extension it advertised earlier.
pub fn parse_extension_handshake_full(payload: &[u8]) -> Option<ExtHandshake> {
    let bv = decode(payload).ok()?;
    let m = bv.dict_get(b"m")?.as_dict()?;
    let id_of = |key: &[u8]| -> Option<u8> {
        let id = m.get(key)?.as_int()?;
        if (1..=255).contains(&id) { Some(id as u8) } else { None }
    };
    let metadata_size = bv
        .dict_get(b"metadata_size")
        .and_then(|v| v.as_int())
        .filter(|n| *n > 0)
        .map(|n| n as usize);
    Some(ExtHandshake {
        // With PEX off the peer's id is dropped here, so no caller can reach
        // for it later and start a conversation the config forbade.
        ut_pex_id: if pex_enabled() { id_of(b"ut_pex") } else { None },
        ut_metadata_id: id_of(b"ut_metadata"),
        metadata_size,
    })
}

/// A decoded BEP 9 ut_metadata message.
pub struct MetadataMsg {
    pub msg_type: i64,
    pub piece: u32,
    /// Info dict size, carried on `data` messages.
    pub total_size: Option<usize>,
    /// Offset in the payload where the raw block starts (`data` messages only).
    pub data_offset: usize,
}

/// Parse a ut_metadata message: a bencoded dict, then (for `data`) raw bytes.
pub fn parse_metadata_message(payload: &[u8]) -> Option<MetadataMsg> {
    let (bv, consumed) = decode_prefix(payload).ok()?;
    let msg_type = bv.dict_get(b"msg_type")?.as_int()?;
    let piece = bv.dict_get(b"piece")?.as_int()?;
    if piece < 0 || piece > u32::MAX as i64 {
        return None;
    }
    let total_size = bv
        .dict_get(b"total_size")
        .and_then(|v| v.as_int())
        .filter(|n| *n > 0)
        .map(|n| n as usize);
    Some(MetadataMsg { msg_type, piece: piece as u32, total_size, data_offset: consumed })
}

/// Build a BEP 9 request for one block of the info dict.
pub fn build_metadata_request(piece: u32) -> Vec<u8> {
    let mut d = BTreeMap::new();
    d.insert(b"msg_type".to_vec(), Bencode::Int(METADATA_REQUEST));
    d.insert(b"piece".to_vec(), Bencode::Int(piece as i64));
    Bencode::Dict(d).to_vec()
}

/// Build a BEP 9 reject ("I can't serve that block").
pub fn build_metadata_reject(piece: u32) -> Vec<u8> {
    let mut d = BTreeMap::new();
    d.insert(b"msg_type".to_vec(), Bencode::Int(METADATA_REJECT));
    d.insert(b"piece".to_vec(), Bencode::Int(piece as i64));
    Bencode::Dict(d).to_vec()
}

/// Build a BEP 9 data message: the dict, then the raw block appended after it.
pub fn build_metadata_data(piece: u32, total_size: usize, block: &[u8]) -> Vec<u8> {
    let mut d = BTreeMap::new();
    d.insert(b"msg_type".to_vec(), Bencode::Int(METADATA_DATA));
    d.insert(b"piece".to_vec(), Bencode::Int(piece as i64));
    d.insert(b"total_size".to_vec(), Bencode::Int(total_size as i64));
    let mut out = Bencode::Dict(d).to_vec();
    out.extend_from_slice(block);
    out
}

/// Whether to take the IPv6 peers a PEX message offers. Off unless the engine
/// was started with `enable_ipv6`: without a v6 listener, dialling them would
/// mean advertising a return path we cannot serve.
static ENABLE_IPV6: AtomicBool = AtomicBool::new(false);

pub fn set_enable_ipv6(on: bool) {
    ENABLE_IPV6.store(on, Ordering::Relaxed);
}

/// Whether this engine takes part in BEP 11 peer exchange at all. On unless
/// the config says otherwise, which is the behaviour every install has had.
/// Off is enforced on both sides at once: we stop advertising `ut_pex`, and we
/// forget a peer's ut_pex id. Advertising the extension and then dropping what
/// arrives would still tell the swarm we trade peer lists.
static ENABLE_PEX: AtomicBool = AtomicBool::new(true);

pub fn set_enable_pex(on: bool) {
    ENABLE_PEX.store(on, Ordering::Relaxed);
}

pub fn pex_enabled() -> bool {
    ENABLE_PEX.load(Ordering::Relaxed)
}

/// Parse a BEP 11 PEX message payload. Returns the peers in `added`, plus
/// those in `added6` when IPv6 is enabled. `dropped` is still ignored.
pub fn parse_pex(payload: &[u8]) -> Vec<SocketAddr> {
    let bv = match decode(payload) { Ok(v) => v, Err(_) => return Vec::new() };
    let mut out = match bv.dict_get(b"added").and_then(|v| v.as_bytes()) {
        Some(b) => parse_compact_v4(b),
        None => Vec::new(),
    };
    if ENABLE_IPV6.load(Ordering::Relaxed) {
        if let Some(b) = bv.dict_get(b"added6").and_then(|v| v.as_bytes()) {
            out.extend(parse_compact_v6(b));
        }
    }
    out
}

/// Decode compact peer list (IPv6: 18 bytes per entry, BEP 7).
fn parse_compact_v6(buf: &[u8]) -> Vec<SocketAddr> {
    let mut out = Vec::with_capacity(buf.len() / 18);
    for chunk in buf.chunks_exact(18) {
        let mut octets = [0u8; 16];
        octets.copy_from_slice(&chunk[..16]);
        let port = u16::from_be_bytes([chunk[16], chunk[17]]);
        if port == 0 { continue; }
        let ip = Ipv6Addr::from(octets);
        // A v4-mapped entry is a v4 peer wearing a v6 hat: unwrap it so it
        // matches everywhere else we compare addresses.
        match ip.to_ipv4_mapped() {
            Some(v4) => out.push(SocketAddr::new(IpAddr::V4(v4), port)),
            None => out.push(SocketAddr::new(IpAddr::V6(ip), port)),
        }
    }
    out
}

/// Decode compact peer list (IPv4: 6 bytes per entry).
fn parse_compact_v4(buf: &[u8]) -> Vec<SocketAddr> {
    let mut out = Vec::with_capacity(buf.len() / 6);
    for chunk in buf.chunks_exact(6) {
        let ip = Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]);
        let port = u16::from_be_bytes([chunk[4], chunk[5]]);
        if port == 0 { continue; }
        out.push(SocketAddr::new(IpAddr::V4(ip), port));
    }
    out
}

/// Build a BEP 11 PEX message payload (without the leading extended_id byte).
/// Caller prepends `peer.ut_pex_id` byte before sending.
pub fn build_pex_message(added: &[SocketAddr], dropped: &[SocketAddr]) -> Vec<u8> {
    let mut root = BTreeMap::new();
    root.insert(b"added".to_vec(), Bencode::Bytes(encode_compact_v4(added)));
    root.insert(b"added.f".to_vec(), Bencode::Bytes(vec![0u8; added.len()]));
    root.insert(b"dropped".to_vec(), Bencode::Bytes(encode_compact_v4(dropped)));
    Bencode::Dict(root).to_vec()
}

fn encode_compact_v4(addrs: &[SocketAddr]) -> Vec<u8> {
    let mut out = Vec::with_capacity(addrs.len() * 6);
    for a in addrs {
        if let SocketAddr::V4(v4) = a {
            out.extend_from_slice(&v4.ip().octets());
            out.extend_from_slice(&v4.port().to_be_bytes());
        }
    }
    out
}


#[cfg(test)]
mod tests {
    use super::*;

    /// Both directions of the PEX switch, in one test on purpose: ENABLE_PEX is
    /// a process-wide static, so two tests toggling it would race each other.
    #[test]
    fn pex_off_is_silent_on_the_wire_and_deaf_to_what_arrives() {
        // A handshake as a peer that does advertise ut_pex would send it.
        let peer_hs = build_extension_handshake(6881, None);

        set_enable_pex(true);
        let on = build_extension_handshake(6881, None);
        assert!(
            on.windows(6).any(|w| w == b"ut_pex"),
            "PEX on but ut_pex is missing from our handshake"
        );
        assert_eq!(
            parse_extension_handshake_full(&peer_hs).unwrap().ut_pex_id,
            Some(OUR_UT_PEX_ID),
            "PEX on but we dropped the peer's ut_pex id"
        );

        set_enable_pex(false);
        let off = build_extension_handshake(6881, None);
        assert!(
            !off.windows(6).any(|w| w == b"ut_pex"),
            "PEX off but we still advertise ut_pex, which tells the swarm we trade peers"
        );
        assert!(
            off.windows(11).any(|w| w == b"ut_metadata"),
            "PEX off must not take ut_metadata down with it"
        );
        assert_eq!(
            parse_extension_handshake_full(&peer_hs).unwrap().ut_pex_id,
            None,
            "PEX off but we kept the peer's ut_pex id"
        );

        set_enable_pex(true);
    }
}
