//! BEP 40: canonical peer priority.
//!
//! A number both ends of a pair compute the same, so two peers that each
//! have to choose between many candidates tend to choose each other. Here it
//! only orders things: which candidate to dial first, and which idle session
//! to give up first. It never picks the survivor of a duplicate connection;
//! that is decided by the peer IDs (`PeerRegistry::insert_unless_duplicate`).
//!
//! A LOWER number is the better peer: dial it first, evict it last.

use std::net::{IpAddr, SocketAddr};

/// CRC32-C (Castagnoli), bitwise. A priority is computed for a handful of
/// candidates per announce, not per packet, so a table buys nothing.
fn crc32c(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0x82F6_3B78 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

/// The BEP 40 priority of the pair `a`, `b`. Symmetric.
pub fn peer_priority(a: SocketAddr, b: SocketAddr) -> u32 {
    let mut buf = Vec::with_capacity(36);
    match (a.ip(), b.ip()) {
        (IpAddr::V4(x), IpAddr::V4(y)) => {
            let (x, y) = (x.octets(), y.octets());
            // Same /24: unmasked. Same /16: the last octet is masked to 0x55.
            // Otherwise the last two are.
            let mask: [u8; 4] = if x[..3] == y[..3] {
                [0xff; 4]
            } else if x[..2] == y[..2] {
                [0xff, 0xff, 0xff, 0x55]
            } else {
                [0xff, 0xff, 0x55, 0x55]
            };
            let mut mx = [0u8; 4];
            let mut my = [0u8; 4];
            for i in 0..4 {
                mx[i] = x[i] & mask[i];
                my[i] = y[i] & mask[i];
            }
            let (lo, hi) = if mx <= my { (mx, my) } else { (my, mx) };
            buf.extend_from_slice(&lo);
            buf.extend_from_slice(&hi);
        }
        (IpAddr::V6(x), IpAddr::V6(y)) => {
            // Only the routing prefix (first 8 bytes) is used. The more of
            // it the two share, the less of it is masked, in the manner of
            // libtorrent's v6 masks (0x55 over the trailing bytes).
            let (x, y) = (x.octets(), y.octets());
            let shared = x[..6]
                .iter()
                .zip(&y[..6])
                .take_while(|(p, q)| p == q)
                .count();
            let keep = (shared + 2).clamp(4, 8);
            let mut mx = [0u8; 8];
            let mut my = [0u8; 8];
            for i in 0..8 {
                let m = if i < keep { 0xff } else { 0x55 };
                mx[i] = x[i] & m;
                my[i] = y[i] & m;
            }
            let (lo, hi) = if mx <= my { (mx, my) } else { (my, mx) };
            buf.extend_from_slice(&lo);
            buf.extend_from_slice(&hi);
        }
        (x, y) => {
            // Mixed families never meet in one swarm view; keep it symmetric.
            let (x, y) = (raw(x), raw(y));
            let (lo, hi) = if x <= y { (x, y) } else { (y, x) };
            buf.extend_from_slice(&lo);
            buf.extend_from_slice(&hi);
        }
    }
    if a.ip() == b.ip() {
        // Same host: the ports order the pair instead.
        buf.clear();
        let (lo, hi) = if a.port() <= b.port() {
            (a.port(), b.port())
        } else {
            (b.port(), a.port())
        };
        buf.extend_from_slice(&lo.to_be_bytes());
        buf.extend_from_slice(&hi.to_be_bytes());
    }
    crc32c(&buf)
}

fn raw(ip: IpAddr) -> Vec<u8> {
    match ip {
        IpAddr::V4(v) => v.octets().to_vec(),
        IpAddr::V6(v) => v.octets().to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn crc32c_check_value() {
        assert_eq!(crc32c(b"123456789"), 0xe306_9283);
    }

    #[test]
    fn bep40_vectors() {
        assert_eq!(
            peer_priority(sa("123.213.32.10:1"), sa("98.76.54.32:2")), // leak-ok: BEP 40 spec vector
            0xec2d_7224
        );
        assert_eq!(
            peer_priority(sa("123.213.32.10:1"), sa("123.213.32.234:2")), // leak-ok: BEP 40 spec vector
            0x9956_8189
        );
    }

    #[test]
    fn priority_is_symmetric() {
        for (a, b) in [
            ("203.0.113.10:6881", "198.51.100.32:51413"),
            ("203.0.113.10:6881", "203.0.113.234:6881"),
            ("203.0.113.10:6881", "203.0.113.99:6881"),
            ("10.0.0.1:6881", "10.0.0.1:6882"),
            ("[2001:db8::1]:6881", "[2001:db8:0:1::5]:6881"),
            ("[2001:db8::1]:6881", "[2a00::1]:6881"),
            ("1.2.3.4:6881", "[2001:db8::1]:6881"),
        ] {
            assert_eq!(
                peer_priority(sa(a), sa(b)),
                peer_priority(sa(b), sa(a)),
                "{a} {b}"
            );
        }
    }

    #[test]
    fn same_host_is_ordered_by_port() {
        let p = peer_priority(sa("10.0.0.1:1000"), sa("10.0.0.1:2000"));
        assert_eq!(p, crc32c(&[0x03, 0xe8, 0x07, 0xd0]));
    }
}
