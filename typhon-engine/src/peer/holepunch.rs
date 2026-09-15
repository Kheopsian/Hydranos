//! BEP 55 -- `ut_holepunch`, connecting two peers that neither can be dialled.
//!
//! Two peers behind NAT cannot reach each other: each has only a private
//! address, and their router discards anything arriving unasked. The way
//! through is a third peer, C, already connected to both. C tells each of them
//! the other's public address, both send at the same moment, and each outgoing
//! packet opens a hole in its own router that the other's packet arrives
//! through just after -- the router takes it for the reply it was waiting for.
//!
//! Three messages, carried over BEP 10:
//!   rendezvous (0) -- A asks C to put it in touch with B
//!   connect    (1) -- C tells A and B to dial each other, now
//!   error      (2) -- C cannot, and says why
//!
//! ⚠️ What this does NOT do is make an unreachable client reachable on its own.
//! It needs a rendezvous peer, which means being connected to somebody already,
//! which means asking the tracker for peers. A seeding torrent that asks for
//! `numwant=0` has nobody to ask and this never fires. That ordering is not an
//! accident of the implementation, it is the shape of the protocol.
//!
//! ⚠️ And it only works where the NAT assigns a port independently of the
//! destination. Commercial VPNs share one exit address between many customers
//! and generally do not, so the hole is punched at a port the other side is not
//! told about. It works for some providers and not for most.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// Our advertised extended message id for `ut_holepunch` (BEP 10 `m` dict).
pub const OUR_UT_HOLEPUNCH_ID: u8 = 3;

pub const MSG_RENDEZVOUS: u8 = 0;
pub const MSG_CONNECT: u8 = 1;
pub const MSG_ERROR: u8 = 2;

const ADDR_V4: u8 = 0;
const ADDR_V6: u8 = 1;

/// Why a rendezvous was refused. The numbers are BEP 55's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// C is not connected to the peer A asked about.
    NoSuchPeer = 1,
    /// C knows the peer but is not connected to it right now.
    NotConnected = 2,
    /// The peer does not speak this extension, so telling it to dial is futile.
    NoSupport = 3,
    /// A asked to be introduced to itself.
    NoSelf = 4,
}

impl Error {
    pub fn from_code(code: u32) -> Option<Error> {
        match code {
            1 => Some(Error::NoSuchPeer),
            2 => Some(Error::NotConnected),
            3 => Some(Error::NoSupport),
            4 => Some(Error::NoSelf),
            _ => None,
        }
    }

    /// Words rather than a number, for whoever reads the log.
    pub fn reason(self) -> &'static str {
        match self {
            Error::NoSuchPeer => "the rendezvous peer does not know that address",
            Error::NotConnected => "the rendezvous peer is not connected to it",
            Error::NoSupport => "that peer does not speak hole punching",
            Error::NoSelf => "a peer cannot be introduced to itself",
        }
    }
}

/// One `ut_holepunch` message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Punch {
    /// Put me in touch with this peer.
    Rendezvous(SocketAddr),
    /// Dial this peer now.
    Connect(SocketAddr),
    /// I cannot, because of this.
    Error(SocketAddr, Error),
}

impl Punch {
    pub fn encode(&self) -> Vec<u8> {
        let (msg_type, addr, err) = match self {
            Punch::Rendezvous(a) => (MSG_RENDEZVOUS, a, None),
            Punch::Connect(a) => (MSG_CONNECT, a, None),
            Punch::Error(a, e) => (MSG_ERROR, a, Some(*e)),
        };
        let mut out = Vec::with_capacity(24);
        out.push(msg_type);
        match addr.ip() {
            IpAddr::V4(v4) => {
                out.push(ADDR_V4);
                out.extend_from_slice(&v4.octets());
            }
            IpAddr::V6(v6) => {
                out.push(ADDR_V6);
                out.extend_from_slice(&v6.octets());
            }
        }
        out.extend_from_slice(&addr.port().to_be_bytes());
        if let Some(e) = err {
            out.extend_from_slice(&(e as u32).to_be_bytes());
        }
        out
    }

    /// Read one, or say nothing rather than guess.
    ///
    /// Every length is checked before it is used: this arrives from a stranger,
    /// and an address read past the end of the buffer is how a peer makes us
    /// dial whatever happened to be in memory.
    pub fn decode(buf: &[u8]) -> Option<Punch> {
        if buf.len() < 2 {
            return None;
        }
        let msg_type = buf[0];
        let addr_len = match buf[1] {
            ADDR_V4 => 4,
            ADDR_V6 => 16,
            _ => return None,
        };
        if buf.len() < 2 + addr_len + 2 {
            return None;
        }
        let ip = if addr_len == 4 {
            let mut o = [0u8; 4];
            o.copy_from_slice(&buf[2..6]);
            IpAddr::V4(Ipv4Addr::from(o))
        } else {
            let mut o = [0u8; 16];
            o.copy_from_slice(&buf[2..18]);
            IpAddr::V6(Ipv6Addr::from(o))
        };
        let p = 2 + addr_len;
        let port = u16::from_be_bytes([buf[p], buf[p + 1]]);
        let addr = SocketAddr::new(ip, port);

        match msg_type {
            MSG_RENDEZVOUS => Some(Punch::Rendezvous(addr)),
            MSG_CONNECT => Some(Punch::Connect(addr)),
            MSG_ERROR => {
                let e = p + 2;
                if buf.len() < e + 4 {
                    return None;
                }
                let code = u32::from_be_bytes([buf[e], buf[e + 1], buf[e + 2], buf[e + 3]]);
                Error::from_code(code).map(|err| Punch::Error(addr, err))
            }
            _ => None,
        }
    }
}

/// An address worth being introduced to.
///
/// A rendezvous is a request to make a third party dial an address of our
/// choosing, so it is an amplifier pointed at whoever we name. Refusing the
/// addresses that are not a peer on the public internet is what stops it being
/// aimed at a machine on the rendezvous peer's own network.
pub fn is_punchable(addr: &SocketAddr) -> bool {
    if addr.port() == 0 {
        return false;
    }
    match addr.ip() {
        IpAddr::V4(v4) => {
            !v4.is_loopback()
                && !v4.is_private()
                && !v4.is_link_local()
                && !v4.is_broadcast()
                && !v4.is_multicast()
                && !v4.is_unspecified()
                && !v4.is_documentation()
        }
        IpAddr::V6(v6) => {
            !v6.is_loopback()
                && !v6.is_multicast()
                && !v6.is_unspecified()
                // fc00::/7, unique local
                && (v6.segments()[0] & 0xfe00) != 0xfc00
                // fe80::/10, link local
                && (v6.segments()[0] & 0xffc0) != 0xfe80
        }
    }
}

/// What C answers when A asks to be introduced to `target`.
///
/// `connected` says whether C holds a live connection to that address, and
/// `supports` whether that peer advertised the extension. Both have to be true
/// for the introduction to lead anywhere: telling a peer that cannot read the
/// message to dial achieves nothing but a connection attempt A will wait for.
pub fn answer_rendezvous(
    asker: SocketAddr,
    target: SocketAddr,
    connected: bool,
    supports: bool,
) -> Punch {
    if target == asker || !is_punchable(&target) {
        return Punch::Error(target, Error::NoSelf);
    }
    if !connected {
        return Punch::Error(target, Error::NotConnected);
    }
    if !supports {
        return Punch::Error(target, Error::NoSupport);
    }
    Punch::Connect(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    /// BEP 55: type, address type, address, port. An IPv4 message is eight
    /// bytes and a peer that reads a ninth is reading the next message.
    #[test]
    fn a_rendezvous_is_type_addrtype_address_port() {
        let m = Punch::Rendezvous(v4("93.184.216.34:6881")).encode();
        assert_eq!(m[0], MSG_RENDEZVOUS);
        assert_eq!(m[1], ADDR_V4);
        assert_eq!(&m[2..6], &[93, 184, 216, 34]);
        assert_eq!(&m[6..8], &6881u16.to_be_bytes());
        assert_eq!(m.len(), 8);
    }

    #[test]
    fn an_ipv6_message_carries_sixteen_bytes_of_address() {
        let m = Punch::Connect("[2606:2800:220:1:248:1893:25c8:1946]:6881".parse().unwrap()).encode();
        assert_eq!(m[0], MSG_CONNECT);
        assert_eq!(m[1], ADDR_V6);
        assert_eq!(m.len(), 20);
    }

    /// An error carries the code after the address, so the asker learns which
    /// of its requests failed and why.
    #[test]
    fn an_error_carries_its_code_after_the_address() {
        let m = Punch::Error(v4("93.184.216.34:6881"), Error::NotConnected).encode();
        assert_eq!(m[0], MSG_ERROR);
        assert_eq!(&m[8..12], &2u32.to_be_bytes());
        assert_eq!(m.len(), 12);
    }

    #[test]
    fn every_message_round_trips() {
        for p in [
            Punch::Rendezvous(v4("93.184.216.34:6881")),
            Punch::Connect(v4("45.33.32.156:51413")),
            Punch::Connect("[2606:2800:220:1:248:1893:25c8:1946]:6881".parse().unwrap()),
            Punch::Error(v4("93.184.216.34:6881"), Error::NoSuchPeer),
            Punch::Error(v4("93.184.216.34:6881"), Error::NoSelf),
        ] {
            assert_eq!(Punch::decode(&p.encode()), Some(p));
        }
    }

    /// This arrives from a stranger. Reading an address past the end of the
    /// buffer is how a peer makes us dial whatever was next in memory.
    #[test]
    fn a_truncated_message_is_refused_rather_than_guessed() {
        assert_eq!(Punch::decode(&[]), None);
        assert_eq!(Punch::decode(&[MSG_CONNECT]), None);
        // Claims IPv6, delivers four bytes.
        assert_eq!(Punch::decode(&[MSG_CONNECT, ADDR_V6, 1, 2, 3, 4]), None);
        // An error with no code.
        assert_eq!(
            Punch::decode(&[MSG_ERROR, ADDR_V4, 203, 0, 113, 7, 0x1a, 0xe1]),
            None
        );
    }

    #[test]
    fn an_unknown_type_or_family_is_refused() {
        assert_eq!(Punch::decode(&[99, ADDR_V4, 1, 2, 3, 4, 0, 80]), None);
        assert_eq!(Punch::decode(&[MSG_CONNECT, 9, 1, 2, 3, 4, 0, 80]), None);
        // An error code BEP 55 does not define.
        let mut bad = Punch::Error(v4("93.184.216.34:6881"), Error::NoSelf).encode();
        bad[8..12].copy_from_slice(&99u32.to_be_bytes());
        assert_eq!(Punch::decode(&bad), None);
    }

    /// ⭐ A rendezvous asks somebody else to dial an address we choose. Without
    /// this, a peer could aim our connections at a machine on the rendezvous
    /// peer's own network -- their router, their NAS -- and use the swarm as
    /// the amplifier.
    #[test]
    fn a_private_address_is_never_punchable() {
        for a in [
            "127.0.0.1:6881",
            "192.168.99.1:80",
            "10.0.0.5:6881",
            "172.16.0.1:6881",
            "169.254.1.1:6881",
            "0.0.0.0:6881",
            "[::1]:6881",
            "[fe80::1]:6881",
            "[fc00::1]:6881",
        ] {
            assert!(!is_punchable(&a.parse().unwrap()), "{a} must be refused");
        }
        assert!(is_punchable(&v4("93.184.216.34:6881")));
        assert!(is_punchable(&"[2606:2800:220:1:248:1893:25c8:1946]:6881".parse().unwrap()));
    }

    /// Port zero is not a peer.
    #[test]
    fn port_zero_is_not_punchable() {
        assert!(!is_punchable(&v4("93.184.216.34:0")));
    }

    #[test]
    fn an_introduction_needs_a_live_peer_that_speaks_the_extension() {
        let asker = v4("45.33.32.156:51413");
        let target = v4("93.184.216.34:6881");

        assert_eq!(
            answer_rendezvous(asker, target, true, true),
            Punch::Connect(target)
        );
        assert_eq!(
            answer_rendezvous(asker, target, false, true),
            Punch::Error(target, Error::NotConnected)
        );
        // Telling a peer that cannot read the message to dial leaves the asker
        // waiting for a connection that was never requested of anyone.
        assert_eq!(
            answer_rendezvous(asker, target, true, false),
            Punch::Error(target, Error::NoSupport)
        );
    }

    #[test]
    fn nobody_is_introduced_to_themselves_or_to_a_private_address() {
        let asker = v4("45.33.32.156:51413");
        assert_eq!(
            answer_rendezvous(asker, asker, true, true),
            Punch::Error(asker, Error::NoSelf)
        );
        let private = v4("192.168.99.1:80");
        assert_eq!(
            answer_rendezvous(asker, private, true, true),
            Punch::Error(private, Error::NoSelf)
        );
    }
}
