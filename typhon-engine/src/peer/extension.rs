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
    /// ... and for ut_holepunch (BEP 55), if they carry that.
    pub ut_holepunch_id: Option<u8>,
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
            ut_holepunch_id: None,
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
pub fn build_extension_handshake(
    listen_port: u16,
    metadata_size: Option<usize>,
    policy: &PeerPolicy,
) -> Vec<u8> {
    let mut m = BTreeMap::new();
    // Left out entirely when PEX is off, rather than advertised and ignored.
    if policy.pex() {
        m.insert(b"ut_pex".to_vec(), Bencode::Int(OUR_UT_PEX_ID as i64));
    }
    m.insert(b"ut_metadata".to_vec(), Bencode::Int(OUR_UT_METADATA_ID as i64));
    // BEP 55. Advertised on the same condition as PEX: both are ways of
    // learning about peers outside the tracker, and a private torrent uses
    // neither.
    if policy.pex() {
        m.insert(
            b"ut_holepunch".to_vec(),
            Bencode::Int(crate::peer::holepunch::OUR_UT_HOLEPUNCH_ID as i64),
        );
    }

    let mut root = BTreeMap::new();
    root.insert(b"m".to_vec(), Bencode::Dict(m));
    if let Some(size) = metadata_size {
        root.insert(b"metadata_size".to_vec(), Bencode::Int(size as i64));
    }
    root.insert(b"p".to_vec(), Bencode::Int(listen_port as i64));
    // What every peer sees us call ourselves. Same string as the announce's
    // User-Agent: a peer and a tracker comparing notes must not be told two
    // different things.
    root.insert(
        b"v".to_vec(),
        Bencode::Bytes(crate::config::user_agent().into_bytes()),
    );
    root.insert(b"reqq".to_vec(), Bencode::Int(250));

    Bencode::Dict(root).to_vec()
}

/// Parse a peer's extension handshake. Returns their ut_pex extended id if present.
pub fn parse_extension_handshake(payload: &[u8]) -> Option<u8> {
    parse_extension_handshake_full(payload, &DEFAULT_POLICY)?.ut_pex_id
}

/// What a peer advertised in its BEP 10 handshake.
pub struct ExtHandshake {
    pub ut_pex_id: Option<u8>,
    pub ut_metadata_id: Option<u8>,
    /// Their id for `ut_holepunch` (BEP 55). An extended message must be sent
    /// with the id the RECEIVER advertised, never with ours.
    pub ut_holepunch_id: Option<u8>,
    /// Total size of their info dict, from the top-level `metadata_size` key.
    pub metadata_size: Option<usize>,
}

/// Parse a peer's extension handshake. Absent or zero ids mean "not offered":
/// 0 is how a peer disables an extension it advertised earlier.
pub fn parse_extension_handshake_full(payload: &[u8], policy: &PeerPolicy) -> Option<ExtHandshake> {
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
        ut_pex_id: if policy.pex() { id_of(b"ut_pex") } else { None },
        ut_metadata_id: id_of(b"ut_metadata"),
        // Same gate as PEX: both are ways of learning peers outside the
        // tracker, and a private torrent uses neither.
        ut_holepunch_id: if policy.pex() { id_of(b"ut_holepunch") } else { None },
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
///
/// Both switches belong to an engine, not to the process. They were statics,
/// which was accurate while one engine meant one process; with race and hoard
/// sharing an address space the last engine to start would have set them for
/// both, silently overriding a per-engine setting the operator had chosen.
pub struct PeerPolicy {
    pex: AtomicBool,
    ipv6: AtomicBool,
    block_mse: AtomicBool,
}

impl Default for PeerPolicy {
    /// PEX on, IPv6 off: what every install has run with.
    fn default() -> Self {
        Self {
            pex: AtomicBool::new(true),
            ipv6: AtomicBool::new(false),
            block_mse: AtomicBool::new(false),
        }
    }
}

impl PeerPolicy {
    pub fn set_pex(&self, on: bool) {
        self.pex.store(on, Ordering::Relaxed);
    }

    pub fn set_ipv6(&self, on: bool) {
        self.ipv6.store(on, Ordering::Relaxed);
    }

    /// Whether this engine takes part in BEP 11 peer exchange at all. Off is
    /// enforced on both sides at once: we stop advertising `ut_pex`, and we
    /// forget a peer's ut_pex id. Advertising the extension and then dropping
    /// what arrives would still tell the swarm we trade peer lists.
    pub fn pex(&self) -> bool {
        self.pex.load(Ordering::Relaxed)
    }

    pub fn ipv6(&self) -> bool {
        self.ipv6.load(Ordering::Relaxed)
    }

    /// Refuse MSE, to measure what encryption costs against the upload lost
    /// from peers that require it. Off by default: turning away real peers is
    /// a trade to measure, not a default to assume.
    ///
    /// Unlike most switches this reaches live sessions too -- the encrypted
    /// peers are precisely the long-lived ones, so gating only new handshakes
    /// would leave them running for hours and a measurement would never reach
    /// a clean state.
    pub fn block_mse(&self) -> bool {
        self.block_mse.load(Ordering::Relaxed)
    }

    pub fn set_block_mse(&self, on: bool) {
        self.block_mse.store(on, Ordering::Relaxed);
    }
}

/// The policy a torrent with no engine behind it runs under: a magnet being
/// resolved before it is added, and the unit tests. Never mutated -- it is a
/// constant, not a setting.
pub static DEFAULT_POLICY: std::sync::LazyLock<PeerPolicy> =
    std::sync::LazyLock::new(PeerPolicy::default);

/// Parse a BEP 11 PEX message payload. Returns the peers in `added`, plus
/// those in `added6` when IPv6 is enabled. `dropped` is still ignored.
pub fn parse_pex(payload: &[u8], policy: &PeerPolicy) -> Vec<SocketAddr> {
    let bv = match decode(payload) { Ok(v) => v, Err(_) => return Vec::new() };
    let mut out = match bv.dict_get(b"added").and_then(|v| v.as_bytes()) {
        Some(b) => parse_compact_v4(b),
        None => Vec::new(),
    };
    if policy.ipv6() {
        if let Some(b) = bv.dict_get(b"added6").and_then(|v| v.as_bytes()) {
            out.extend(parse_compact_v6(b));
        }
    }
    out
}

/// Whether an address a peer told us about could be a peer at all.
///
/// PEX arrives from strangers and feeds straight into the dial queue, so an
/// address nobody could be listening on is an instruction to open a connection
/// somewhere for somebody else's reasons. Only the port was checked before:
/// `127.0.0.1:631` was accepted and dialled.
///
/// Deliberately less strict than `holepunch::is_punchable`. PEX names a peer
/// for US to dial; a hole-punch rendezvous makes a THIRD PARTY dial an address
/// the asker chose, which is an amplifier and has to refuse anything private.
/// Two machines of one LAN in a swarm is an ordinary thing, so private
/// addresses stay allowed here.
fn is_plausible_peer(addr: &SocketAddr) -> bool {
    if addr.port() == 0 {
        return false;
    }
    match addr.ip() {
        IpAddr::V4(v4) => {
            !v4.is_loopback()
                && !v4.is_unspecified()
                && !v4.is_multicast()
                && !v4.is_broadcast()
                && !v4.is_link_local()
                && !v4.is_documentation()
        }
        IpAddr::V6(v6) => {
            !v6.is_loopback()
                && !v6.is_unspecified()
                && !v6.is_multicast()
                // fe80::/10, link local
                && (v6.segments()[0] & 0xffc0) != 0xfe80
        }
    }
}

/// Decode compact peer list (IPv6: 18 bytes per entry, BEP 7).
fn parse_compact_v6(buf: &[u8]) -> Vec<SocketAddr> {
    let mut out = Vec::with_capacity(buf.len() / 18);
    for chunk in buf.chunks_exact(18) {
        let mut octets = [0u8; 16];
        octets.copy_from_slice(&chunk[..16]);
        let port = u16::from_be_bytes([chunk[16], chunk[17]]);
        let ip = Ipv6Addr::from(octets);
        // A v4-mapped entry is a v4 peer wearing a v6 hat: unwrap it so it
        // matches everywhere else we compare addresses -- and so the check
        // below sees the v4 address it really is.
        let addr = match ip.to_ipv4_mapped() {
            Some(v4) => SocketAddr::new(IpAddr::V4(v4), port),
            None => SocketAddr::new(IpAddr::V6(ip), port),
        };
        if !is_plausible_peer(&addr) { continue; }
        out.push(addr)
    }
    out
}

/// Decode compact peer list (IPv4: 6 bytes per entry).
fn parse_compact_v4(buf: &[u8]) -> Vec<SocketAddr> {
    let mut out = Vec::with_capacity(buf.len() / 6);
    for chunk in buf.chunks_exact(6) {
        let ip = Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]);
        let port = u16::from_be_bytes([chunk[4], chunk[5]]);
        let addr = SocketAddr::new(IpAddr::V4(ip), port);
        if !is_plausible_peer(&addr) { continue; }
        out.push(addr);
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

    /// Both directions of the PEX switch. They used to have to share one test
    /// because ENABLE_PEX was a process-wide static and two tests toggling it
    /// would race; a policy is a value, so they no longer can.
    #[test]
    fn pex_off_is_silent_on_the_wire_and_deaf_to_what_arrives() {
        let on = PeerPolicy::default();
        let off = PeerPolicy::default();
        off.set_pex(false);

        // A handshake as a peer that does advertise ut_pex would send it.
        let peer_hs = build_extension_handshake(6881, None, &on);

        let ours = build_extension_handshake(6881, None, &on);
        assert!(
            ours.windows(6).any(|w| w == b"ut_pex"),
            "PEX on but ut_pex is missing from our handshake"
        );
        assert_eq!(
            parse_extension_handshake_full(&peer_hs, &on).unwrap().ut_pex_id,
            Some(OUR_UT_PEX_ID),
            "PEX on but we dropped the peer's ut_pex id"
        );

        let quiet = build_extension_handshake(6881, None, &off);
        assert!(
            !quiet.windows(6).any(|w| w == b"ut_pex"),
            "PEX off but we still advertise ut_pex, which tells the swarm we trade peers"
        );
        assert!(
            quiet.windows(11).any(|w| w == b"ut_metadata"),
            "PEX off must not take ut_metadata down with it"
        );
        assert_eq!(
            parse_extension_handshake_full(&peer_hs, &off).unwrap().ut_pex_id,
            None,
            "PEX off but we kept the peer's ut_pex id"
        );
    }

    /// The reason the switch stopped being a static. Race with PEX on and
    /// hoard with PEX off, in one process: under the old design the second
    /// engine to start decided for both.
    #[test]
    fn two_engines_hold_opposite_pex_settings_at_once() {
        let race = PeerPolicy::default();
        let hoard = PeerPolicy::default();
        hoard.set_pex(false);

        assert!(race.pex());
        assert!(!hoard.pex());
        assert!(build_extension_handshake(6881, None, &race).windows(6).any(|w| w == b"ut_pex"));
        assert!(!build_extension_handshake(6881, None, &hoard).windows(6).any(|w| w == b"ut_pex"));
    }
}

#[cfg(test)]
mod protocol_tests {
    use super::*;

    // -----------------------------------------------------------------------
    // BEP 9 -- metadata exchange
    // -----------------------------------------------------------------------

    /// BEP 9 splits the info dict into fixed 16 KiB blocks; only the last is
    /// short. Getting the count wrong asks for a block that does not exist, and
    /// the peer answers `reject` forever.
    #[test]
    fn bep9_the_dict_is_cut_into_sixteen_kib_blocks() {
        assert_eq!(metadata_block_count(0), 0);
        assert_eq!(metadata_block_count(1), 1);
        assert_eq!(metadata_block_count(METADATA_BLOCK), 1, "exactly one block");
        assert_eq!(metadata_block_count(METADATA_BLOCK + 1), 2, "one byte over");
        assert_eq!(metadata_block_count(METADATA_BLOCK * 4), 4);
    }

    #[test]
    fn bep9_a_request_round_trips() {
        let m = parse_metadata_message(&build_metadata_request(7)).expect("parses");
        assert_eq!(m.msg_type, METADATA_REQUEST);
        assert_eq!(m.piece, 7);
        assert_eq!(m.total_size, None, "a request states no size");
    }

    #[test]
    fn bep9_a_reject_round_trips() {
        let m = parse_metadata_message(&build_metadata_reject(3)).expect("parses");
        assert_eq!(m.msg_type, METADATA_REJECT);
        assert_eq!(m.piece, 3);
    }

    /// ⭐ The shape of BEP 9 that catches people out: the block is NOT inside
    /// the bencoded dictionary, it is appended after it. `data_offset` is where
    /// the dict ended and the bytes begin -- read it wrong and the info dict is
    /// assembled from the tail of its own header and never hashes.
    #[test]
    fn bep9_the_block_follows_the_dictionary_rather_than_sitting_in_it() {
        let block = b"the raw bytes of the info dict";
        let msg = build_metadata_data(2, 40000, block);

        let m = parse_metadata_message(&msg).expect("parses");
        assert_eq!(m.msg_type, METADATA_DATA);
        assert_eq!(m.piece, 2);
        assert_eq!(m.total_size, Some(40000), "data carries the total size");
        assert_eq!(
            &msg[m.data_offset..],
            block,
            "everything past the dictionary is the block, byte for byte"
        );
    }

    /// The peer is unauthenticated and we index on what it says. A piece index
    /// that is negative or past u32 is refused rather than wrapped.
    #[test]
    fn bep9_an_impossible_piece_index_is_refused() {
        let mut d = BTreeMap::new();
        d.insert(b"msg_type".to_vec(), Bencode::Int(METADATA_DATA));
        d.insert(b"piece".to_vec(), Bencode::Int(-1));
        assert!(parse_metadata_message(&Bencode::Dict(d).to_vec()).is_none());

        let mut d = BTreeMap::new();
        d.insert(b"msg_type".to_vec(), Bencode::Int(METADATA_DATA));
        d.insert(b"piece".to_vec(), Bencode::Int(i64::from(u32::MAX) + 1));
        assert!(parse_metadata_message(&Bencode::Dict(d).to_vec()).is_none());
    }

    #[test]
    fn bep9_a_message_that_is_not_one_is_refused() {
        assert!(parse_metadata_message(b"").is_none());
        assert!(parse_metadata_message(b"not bencode").is_none());
        // A dict with no msg_type is not a metadata message.
        assert!(parse_metadata_message(b"d5:piecei0ee").is_none());
    }

    // -----------------------------------------------------------------------
    // BEP 10 -- the extension handshake
    // -----------------------------------------------------------------------

    /// `ut_metadata` is offered whatever the PEX setting: serving our own info
    /// dict to a peer resolving a magnet is not peer discovery, and a private
    /// torrent has every reason to answer it.
    #[test]
    fn bep10_metadata_is_offered_even_with_pex_off() {
        let off = PeerPolicy::default();
        off.set_pex(false);
        let hs = build_extension_handshake(6881, Some(1234), &off);
        assert!(hs.windows(11).any(|w| w == b"ut_metadata"));
        assert!(
            !hs.windows(6).any(|w| w == b"ut_pex"),
            "PEX off means the key is absent, not advertised and ignored"
        );
    }

    /// The handshake states our listen port and the size of the dict we can
    /// serve, which is how a peer knows it may ask at all.
    #[test]
    fn bep10_the_handshake_states_the_port_and_the_dict_size() {
        let on = PeerPolicy::default();
        let hs = build_extension_handshake(16171, Some(40000), &on);
        let parsed = parse_extension_handshake_full(&hs, &on).expect("our own handshake parses");
        assert_eq!(parsed.metadata_size, Some(40000));
        assert_eq!(parsed.ut_metadata_id, Some(OUR_UT_METADATA_ID));

        // No dict to serve: the key is left out rather than sent as zero.
        let hs = build_extension_handshake(16171, None, &on);
        assert_eq!(parse_extension_handshake_full(&hs, &on).unwrap().metadata_size, None);
    }

    /// BEP 10: an id of zero means the peer is withdrawing an extension it
    /// offered before. Treating it as a real id would address messages to 0,
    /// which is the handshake itself.
    #[test]
    fn bep10_an_id_of_zero_is_a_withdrawal_not_an_id() {
        let on = PeerPolicy::default();
        let mut m = BTreeMap::new();
        m.insert(b"ut_pex".to_vec(), Bencode::Int(0));
        m.insert(b"ut_metadata".to_vec(), Bencode::Int(0));
        let mut root = BTreeMap::new();
        root.insert(b"m".to_vec(), Bencode::Dict(m));
        let payload = Bencode::Dict(root).to_vec();

        let parsed = parse_extension_handshake_full(&payload, &on).expect("parses");
        assert_eq!(parsed.ut_pex_id, None);
        assert_eq!(parsed.ut_metadata_id, None);
    }

    #[test]
    fn bep10_a_handshake_without_an_m_dict_is_not_one() {
        let on = PeerPolicy::default();
        assert!(parse_extension_handshake_full(b"de", &on).is_none());
        assert!(parse_extension_handshake_full(b"not bencode", &on).is_none());
    }

    // -----------------------------------------------------------------------
    // BEP 11 -- peer exchange
    // -----------------------------------------------------------------------

    /// BEP 11 carries peers in the compact form BEP 23 defines: six bytes each.
    #[test]
    fn bep11_peers_travel_in_the_compact_form() {
        let on = PeerPolicy::default();
        let peers: Vec<std::net::SocketAddr> = vec![
            "93.184.216.34:6881".parse().unwrap(),
            "45.33.32.156:51413".parse().unwrap(),
        ];
        let msg = build_pex_message(&peers, &[]);
        let back = parse_pex(&msg, &on);
        assert_eq!(back, peers, "what went out is what comes back");
    }

    /// ⭐ PEX arrives from strangers and feeds the dial queue. Only the port
    /// was checked before, so a peer naming `127.0.0.1:631` had us open a
    /// connection to a service on our own machine.
    #[test]
    fn bep11_an_address_that_cannot_be_a_peer_is_dropped() {
        let on = PeerPolicy::default();
        let junk: Vec<std::net::SocketAddr> = vec![
            "127.0.0.1:631".parse().unwrap(),
            "0.0.0.0:6881".parse().unwrap(),
            "169.254.1.1:6881".parse().unwrap(),
            "93.184.216.34:0".parse().unwrap(),
        ];
        let good: std::net::SocketAddr = "93.184.216.34:6881".parse().unwrap();
        let mut all = junk.clone();
        all.push(good);

        let back = parse_pex(&build_pex_message(&all, &[]), &on);
        assert_eq!(back, vec![good], "only the one that could be a peer");
    }

    /// And NOT stricter than that: two machines of one LAN in a swarm is an
    /// ordinary thing, so a private address is a peer. This is where the rule
    /// parts company with `holepunch::is_punchable`, which refuses them --
    /// because a rendezvous makes somebody ELSE dial what the asker named.
    #[test]
    fn bep11_a_private_address_is_still_a_peer() {
        let on = PeerPolicy::default();
        let lan: std::net::SocketAddr = "192.168.99.50:6881".parse().unwrap();
        assert_eq!(parse_pex(&build_pex_message(&[lan], &[]), &on), vec![lan]);
    }

    #[test]
    fn bep11_an_empty_or_malformed_message_yields_no_peers() {
        let on = PeerPolicy::default();
        assert!(parse_pex(b"", &on).is_empty());
        assert!(parse_pex(b"not bencode", &on).is_empty());
        assert!(parse_pex(&build_pex_message(&[], &[]), &on).is_empty());
    }
}
