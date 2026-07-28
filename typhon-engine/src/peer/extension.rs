//! BEP 10 extension protocol handshake + BEP 11 PEX (Peer Exchange).
//!
//! On extension handshake: peer sends `extended_id=0` with bencoded dict
//! containing `m: {ut_pex: <their_id>, ...}` (their_id is the ID we MUST use to send them
//! ut_pex messages). We reply with our own m-dict (we use ut_pex=1).
//!
//! On PEX received (extended_id == 1, the id we advertised): bencoded dict
//! `{added: <compact 6-byte peers>, added.f: <flag bytes>, dropped: <compact peers>}`.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Instant;
use std::collections::BTreeMap;

use super::bencode::{Bencode, decode};

/// Reserved bits handshake byte for LTEP (BEP 10): reserved[5] |= 0x10.
pub const RESERVED_LTEP: u8 = 0x10;

/// Our advertised extended message id for ut_pex.
pub const OUR_UT_PEX_ID: u8 = 1;

/// Per-connection extension state.
pub struct PeerExt {
    /// Their extended message id for ut_pex (extracted from their handshake.m.ut_pex).
    pub ut_pex_id: Option<u8>,
    /// Last time we sent a PEX message to this peer.
    pub last_pex_sent: Option<Instant>,
    /// Peers we have already advertised to this peer (so we can compute added/dropped deltas).
    pub sent_peers: HashSet<SocketAddr>,
}

impl PeerExt {
    pub fn new() -> Self {
        Self { ut_pex_id: None, last_pex_sent: None, sent_peers: HashSet::new() }
    }
}

/// Build the BEP 10 extension handshake payload (without the leading extended_id=0 byte).
/// Caller prepends 0x00 (extended_id 0 = extension handshake) before sending.
pub fn build_extension_handshake(listen_port: u16) -> Vec<u8> {
    let mut m = BTreeMap::new();
    m.insert(b"ut_pex".to_vec(), Bencode::Int(OUR_UT_PEX_ID as i64));

    let mut root = BTreeMap::new();
    root.insert(b"m".to_vec(), Bencode::Dict(m));
    root.insert(b"p".to_vec(), Bencode::Int(listen_port as i64));
    root.insert(b"v".to_vec(), Bencode::Bytes(b"typhon 0.2".to_vec()));
    root.insert(b"reqq".to_vec(), Bencode::Int(250));

    Bencode::Dict(root).to_vec()
}

/// Parse a peer's extension handshake. Returns their ut_pex extended id if present.
pub fn parse_extension_handshake(payload: &[u8]) -> Option<u8> {
    let bv = decode(payload).ok()?;
    let m = bv.dict_get(b"m")?.as_dict()?;
    let id = m.get(&b"ut_pex"[..])?.as_int()?;
    if (1..=255).contains(&id) {
        Some(id as u8)
    } else {
        // 0 means peer disabled the extension after a previous handshake.
        None
    }
}

/// Parse a BEP 11 PEX message payload. Returns the IPv4 peers in `added`.
/// Ignores `added6` (IPv6) and `dropped` for now.
pub fn parse_pex(payload: &[u8]) -> Vec<SocketAddr> {
    let bv = match decode(payload) { Ok(v) => v, Err(_) => return Vec::new() };
    let added = match bv.dict_get(b"added").and_then(|v| v.as_bytes()) {
        Some(b) => b,
        None => return Vec::new(),
    };
    parse_compact_v4(added)
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
