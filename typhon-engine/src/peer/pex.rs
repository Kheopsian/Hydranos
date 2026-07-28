//! BEP 10 Extension Protocol + BEP 11 Peer Exchange (PEX).
//!
//! Extended handshake (ext_id=0) negotiates extension ids. We advertise `ut_pex=1`.
//! Periodically (60s) we send a `ut_pex` message containing IPv4 peers added/dropped
//! since the last send, letting peers discover each other without the tracker.

use bytes::Bytes;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use crate::torrent::metainfo::bencode_decode;

/// Extension id we advertise for ut_pex. Remote side will use this id to send us PEX.
pub const OUR_UT_PEX_ID: u8 = 1;

/// How often each peer task emits a PEX message (spec recommends 60s minimum).
pub const PEX_INTERVAL_SECS: u64 = 60;

/// Cap on peers per PEX message. Most clients send ~50, clamp to this.
pub const PEX_MAX_PEERS: usize = 50;

fn bencode_str(out: &mut Vec<u8>, s: &[u8]) {
    out.extend_from_slice(s.len().to_string().as_bytes());
    out.push(b':');
    out.extend_from_slice(s);
}

fn bencode_int(out: &mut Vec<u8>, i: i64) {
    out.push(b'i');
    out.extend_from_slice(i.to_string().as_bytes());
    out.push(b'e');
}

/// Build the BEP 10 extended handshake payload sent as `Extended { ext_id: 0, payload }`.
/// Advertises `ut_pex` support. `listen_port = 0` omits the `p` field.
pub fn build_extended_handshake(listen_port: u16) -> Bytes {
    // Keys must be bencode-sorted (m < p < v).
    let mut out = Vec::with_capacity(96);
    out.push(b'd');
    // m = { ut_pex: OUR_UT_PEX_ID }
    bencode_str(&mut out, b"m");
    out.push(b'd');
    bencode_str(&mut out, b"ut_pex");
    bencode_int(&mut out, OUR_UT_PEX_ID as i64);
    out.push(b'e');
    // p = listen_port (optional, omitted when unknown)
    if listen_port != 0 {
        bencode_str(&mut out, b"p");
        bencode_int(&mut out, listen_port as i64);
    }
    // v = client version
    bencode_str(&mut out, b"v");
    bencode_str(&mut out, b"Typhon 0.1");
    out.push(b'e');
    Bytes::from(out)
}

/// Parse an incoming extended handshake (ext_id=0). Returns the peer's advertised
/// ut_pex extension id, or None if they don't support PEX.
pub fn parse_extended_handshake(payload: &[u8]) -> Option<u8> {
    let val = bencode_decode(payload).ok()?;
    let d = val.as_dict()?;
    let m = d.get("m")?.as_dict()?;
    let ut_pex = m.get("ut_pex")?.as_int()?;
    if ut_pex > 0 && ut_pex < 256 {
        Some(ut_pex as u8)
    } else {
        None
    }
}

fn encode_compact_v4(peers: &[SocketAddr]) -> Vec<u8> {
    let mut out = Vec::with_capacity(peers.len() * 6);
    for p in peers {
        if let SocketAddr::V4(a) = p {
            out.extend_from_slice(&a.ip().octets());
            out.extend_from_slice(&a.port().to_be_bytes());
        }
    }
    out
}

/// Build a `ut_pex` message payload. `added`/`dropped` are IPv4-only (we ignore v6 here).
pub fn build_pex_message(added: &[SocketAddr], dropped: &[SocketAddr]) -> Bytes {
    let added_v4: Vec<SocketAddr> = added.iter()
        .filter(|a| matches!(a, SocketAddr::V4(_)))
        .take(PEX_MAX_PEERS)
        .copied()
        .collect();
    let dropped_v4: Vec<SocketAddr> = dropped.iter()
        .filter(|a| matches!(a, SocketAddr::V4(_)))
        .take(PEX_MAX_PEERS)
        .copied()
        .collect();
    let added_bytes = encode_compact_v4(&added_v4);
    let dropped_bytes = encode_compact_v4(&dropped_v4);
    // flags: 0x02 = seeder, 0x01 = encrypted connection. We're always seeding.
    let added_flags: Vec<u8> = std::iter::repeat(0x02u8).take(added_v4.len()).collect();

    // Keys must be bencode-sorted: added < added.f < added6 < dropped
    let mut out = Vec::with_capacity(32 + added_bytes.len() + dropped_bytes.len());
    out.push(b'd');
    bencode_str(&mut out, b"added");
    bencode_str(&mut out, &added_bytes);
    bencode_str(&mut out, b"added.f");
    bencode_str(&mut out, &added_flags);
    bencode_str(&mut out, b"dropped");
    bencode_str(&mut out, &dropped_bytes);
    out.push(b'e');
    Bytes::from(out)
}

/// Parse an incoming `ut_pex` message. Returns `added` peer addrs (IPv4 only).
pub fn parse_pex_message(payload: &[u8]) -> Vec<SocketAddr> {
    let val = match bencode_decode(payload) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let d = match val.as_dict() {
        Some(d) => d,
        None => return Vec::new(),
    };
    let added = match d.get("added").and_then(|v| v.as_bytes()) {
        Some(b) => b,
        None => return Vec::new(),
    };
    let mut out = Vec::with_capacity(added.len() / 6);
    for chunk in added.chunks_exact(6) {
        let ip = Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]);
        let port = u16::from_be_bytes([chunk[4], chunk[5]]);
        if port == 0 || ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() {
            continue;
        }
        out.push(SocketAddr::V4(SocketAddrV4::new(ip, port)));
    }
    out
}
