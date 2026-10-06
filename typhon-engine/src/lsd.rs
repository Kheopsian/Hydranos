//! Local Service Discovery (BEP 14): finding peers on the same LAN.
//!
//! An engine multicasts `BT-SEARCH` messages naming the info hashes it wants
//! peers for, and listens for the same messages from the other clients of the
//! LAN. A peer heard announcing a hash this engine holds is handed to the dial
//! queue like any tracker or DHT peer (`tracker::enqueue_dial`), so it goes
//! through the engine's dial limiter and its interface pin like every other
//! dial.
//!
//! What it is worth here: two clients on one LAN downloading the same public
//! torrent trade pieces at LAN speed instead of each pulling them from the
//! Internet. That is a download-time benefit, so what is announced is chosen
//! for it (see `Schedule`).
//!
//! Never for a `private` torrent (BEP 27), in either direction: not announced,
//! and an announce for one is not turned into a peer. Off behind the SOCKS5
//! proxy (`dht::lsd_policy`): multicast cannot go through a proxy and would
//! leave by the host's own interface.
//!
//! Inside a WireGuard tunnel it stays on, pinned to the tunnel's device like
//! every other socket of the engine: the LAN is then the tunnel's, which is the
//! only one such an engine is supposed to talk to. What it sends goes into the
//! tunnel and nowhere else, from the tunnel's address; a VPN server usually
//! drops multicast, so there LSD mostly finds nobody, but it never reaches the
//! host's own LAN. A socket that cannot join on that device leaves LSD off,
//! said once in the log -- it never falls back to the host's interfaces.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use librqbit_dualstack_sockets::{BindDevice, MulticastUdpSocket};
use tracing::{debug, info, warn};

use crate::torrent::TorrentManager;
use crate::torrent::meta::{InfoHash, TorrentState};

/// The port BEP 14 multicasts on.
pub const LSD_PORT: u16 = 6771;
/// BEP 14's IPv4 group, administratively scoped (never routed off the site).
pub const LSD_V4: Ipv4Addr = Ipv4Addr::new(239, 192, 152, 143);
/// BEP 14's IPv6 group, site-local scope (`ff15::`).
pub const LSD_V6: Ipv6Addr = Ipv6Addr::new(0xff15, 0, 0, 0, 0, 0, 0xefc0, 0x988f);

/// How often one torrent may be announced. BEP 14 asks for no more than one
/// announce per torrent per 5 minutes; libtorrent and qBittorrent use exactly
/// that.
pub const REANNOUNCE: Duration = Duration::from_secs(5 * 60);
/// One announce round per minute. A torrent that starts downloading waits at
/// most this long for its first LSD announce.
pub const ROUND: Duration = Duration::from_secs(60);
/// The cap: messages per round, so per minute, whatever the size of the
/// library. 60 messages of `HASHES_PER_MESSAGE` hashes is 1200 torrents a
/// minute, 6000 per 5-minute cycle -- far more than any engine downloads at
/// once -- for one ~1 KB multicast datagram a second on the LAN. Past that the
/// cycle stretches (oldest announce first) instead of the LAN getting louder.
pub const MAX_MESSAGES_PER_ROUND: usize = 60;
/// BEP 14 allows several `Infohash` headers in one message. 20 keeps a message
/// near 1.1 KB, under any LAN's MTU, so it is never fragmented.
pub const HASHES_PER_MESSAGE: usize = 20;
/// A message naming more than this is not a client announcing its torrents;
/// the rest is ignored rather than turned into dials.
const MAX_HASHES_PARSED: usize = 64;

/// One `BT-SEARCH` message, as read off the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Announce {
    /// The sender's peer port. Not the datagram's source port, which is the
    /// sender's LSD socket.
    pub port: u16,
    pub info_hashes: Vec<InfoHash>,
    pub cookie: Option<String>,
}

/// The `Host` header for a group: `239.192.152.143:6771` or
/// `[ff15::efc0:988f]:6771`, which is `SocketAddr`'s own form.
fn host_header(group: SocketAddr) -> String {
    match group {
        SocketAddr::V6(v6) => SocketAddr::V6(SocketAddrV6::new(*v6.ip(), v6.port(), 0, 0)).to_string(),
        v4 => v4.to_string(),
    }
}

/// Build one BEP 14 message. The format is BEP 14's to the byte, trailing
/// empty lines included: libtorrent parses it as HTTP and wants the blank
/// line.
pub fn build(group: SocketAddr, port: u16, info_hashes: &[InfoHash], cookie: &str) -> String {
    let mut msg = format!("BT-SEARCH * HTTP/1.1\r\nHost: {}\r\nPort: {}\r\n", host_header(group), port);
    for ih in info_hashes {
        msg.push_str("Infohash: ");
        msg.push_str(&crate::torrent::hex_encode(ih));
        msg.push_str("\r\n");
    }
    if !cookie.is_empty() {
        msg.push_str("cookie: ");
        msg.push_str(cookie);
        msg.push_str("\r\n");
    }
    msg.push_str("\r\n\r\n");
    msg
}

/// Parse one message. None for anything that is not a usable `BT-SEARCH`:
/// another protocol on the port, no port, no valid info hash.
///
/// Header names are case-insensitive (it is HTTP, and clients differ on
/// `cookie` / `Cookie`); a bare `\n` is accepted as a line end.
pub fn parse(buf: &[u8]) -> Option<Announce> {
    let text = std::str::from_utf8(buf).ok()?;
    let mut lines = text.split('\n').map(|l| l.trim_end_matches('\r'));
    if lines.next()?.trim() != "BT-SEARCH * HTTP/1.1" {
        return None;
    }
    let mut port = None;
    let mut info_hashes = Vec::new();
    let mut cookie = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "port" => port = value.parse::<u16>().ok().filter(|p| *p != 0),
            "infohash" if info_hashes.len() < MAX_HASHES_PARSED => {
                // v1 hashes only: a 64-character v2 hash names a torrent this
                // engine cannot hold.
                if value.len() == 40 {
                    if let Ok(ih) = crate::torrent::hex_decode(&value.to_ascii_lowercase()) {
                        if !info_hashes.contains(&ih) {
                            info_hashes.push(ih);
                        }
                    }
                }
            }
            "cookie" if !value.is_empty() => cookie = Some(value.to_string()),
            _ => {}
        }
    }
    if info_hashes.is_empty() {
        return None;
    }
    Some(Announce { port: port?, info_hashes, cookie })
}

/// Whether a torrent may take part in LSD at all: never a private one, and
/// not a stopped or removed one either -- a stopped torrent dials nobody, so
/// announcing it would only invite connections it refuses.
pub fn may_use_lsd(t: &TorrentState) -> bool {
    t.meta.allows_peer_discovery()
        && !t.is_paused.load(Ordering::Relaxed)
        && !t.is_removed.load(Ordering::Relaxed)
}

/// A torrent worth announcing this round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidate {
    pub info_hash: InfoHash,
    /// Still downloading. Announced before the ones that only have peers:
    /// they are what LSD actually speeds up.
    pub downloading: bool,
}

/// Which torrents go out in which message, under the cap.
///
/// Only ACTIVE torrents are candidates: the ones downloading, and the ones
/// with peers connected. A library of a million seeds sitting idle is a
/// million torrents nobody on the LAN is asking for; announcing them would be
/// a million datagrams for nothing. Of the candidates, a torrent is due once
/// `REANNOUNCE` has passed since its last announce, and at most
/// `MAX_MESSAGES_PER_ROUND` messages leave per round whatever is due --
/// downloads first, then the ones never announced, then the oldest.
#[derive(Debug, Default)]
pub struct Schedule {
    /// Last announce per torrent. Pruned every round to the candidates of
    /// that round, so it stays the size of the active set, not the library.
    last: HashMap<InfoHash, Instant>,
}

impl Schedule {
    pub fn plan(&mut self, candidates: &[Candidate], now: Instant) -> Vec<Vec<InfoHash>> {
        let active: HashSet<InfoHash> = candidates.iter().map(|c| c.info_hash).collect();
        self.last.retain(|ih, _| active.contains(ih));

        let mut due: Vec<(&Candidate, Option<Instant>)> = candidates
            .iter()
            .map(|c| (c, self.last.get(&c.info_hash).copied()))
            .filter(|(_, last)| last.is_none_or(|t| now.duration_since(t) >= REANNOUNCE))
            .collect();
        // Downloads first; then never announced (None sorts first); then the
        // longest wait.
        due.sort_by_key(|(c, last)| (!c.downloading, *last));

        let room = MAX_MESSAGES_PER_ROUND * HASHES_PER_MESSAGE;
        let chosen: Vec<InfoHash> = due.into_iter().take(room).map(|(c, _)| c.info_hash).collect();
        for ih in &chosen {
            self.last.insert(*ih, now);
        }
        chosen.chunks(HASHES_PER_MESSAGE).map(|c| c.to_vec()).collect()
    }
}

/// The active torrents of one engine, as LSD candidates.
///
/// Downloads come from the `incomplete` index, O(downloading). Torrents with
/// peers need a walk of the catalogue; it runs once per `ROUND` and reads one
/// map length per torrent, the same order of cost as the DHT's boot loop.
pub fn candidates(mgr: &TorrentManager) -> Vec<Candidate> {
    let downloading = mgr.collect_incomplete(usize::MAX, |t| may_use_lsd(t));
    let seen: HashSet<InfoHash> = downloading.iter().copied().collect();
    let mut out: Vec<Candidate> = downloading
        .into_iter()
        .map(|info_hash| Candidate { info_hash, downloading: true })
        .collect();
    let with_peers = mgr.collect_torrents(usize::MAX, |t| {
        !t.connected_addrs.is_empty() && may_use_lsd(t) && !seen.contains(&t.info_hash)
    });
    out.extend(with_peers.into_iter().map(|info_hash| Candidate { info_hash, downloading: false }));
    out
}

/// One engine's LSD socket and what it has done.
pub struct LsdSession {
    sock: MulticastUdpSocket,
    /// Sent in every message and compared on every message received: our own
    /// announces come back to us (multicast loopback, and every interface we
    /// send on), and must not become dials to ourselves. Per session, so two
    /// engines in one process -- or on one host -- still find each other.
    cookie: String,
    messages_sent: AtomicU64,
    hashes_announced: AtomicU64,
    peers_found: AtomicU64,
    own_ignored: AtomicU64,
    /// Peers found per torrent, for the `[LSD]` line of a torrent's trackers.
    /// Only torrents LSD found someone for have an entry.
    found: DashMap<InfoHash, u32>,
}

impl LsdSession {
    /// Join the BEP 14 groups on `port` (6771 outside tests).
    ///
    /// `device` pins the socket (`SO_BINDTODEVICE`, as `netpin::pin_fd` does)
    /// and restricts the groups to that interface. A device that does not
    /// exist, or no interface able to join, is an Err: LSD then stays off,
    /// never runs on the default route instead.
    pub async fn bind(device: Option<&str>, port: u16) -> Result<Arc<Self>, String> {
        let device = match device {
            Some(d) => Some(BindDevice::new_from_name(d).map_err(|e| format!("device {d}: {e}"))?),
            None => None,
        };
        let v4 = SocketAddrV4::new(LSD_V4, port);
        let v6 = SocketAddrV6::new(LSD_V6, port, 0, 0);
        // Dual-stack first, so one socket serves both groups; IPv4 alone on a
        // host with IPv6 switched off.
        let mut last_err = String::new();
        for bind in [
            SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port),
        ] {
            match MulticastUdpSocket::new(bind, v4, v6, None, device.as_ref()).await {
                Ok(sock) => {
                    return Ok(Arc::new(Self {
                        sock,
                        cookie: format!("{:08x}", rand::random::<u32>()),
                        messages_sent: AtomicU64::new(0),
                        hashes_announced: AtomicU64::new(0),
                        peers_found: AtomicU64::new(0),
                        own_ignored: AtomicU64::new(0),
                        found: DashMap::new(),
                    }));
                }
                Err(e) => last_err = format!("{e:#}"),
            }
        }
        Err(last_err)
    }

    pub fn cookie(&self) -> &str {
        &self.cookie
    }

    /// Send ONE message naming `info_hashes`, on every interface able to carry
    /// it (each with the `Host` of the group it goes to).
    pub async fn announce(&self, listen_port: u16, info_hashes: &[InfoHash]) {
        if info_hashes.is_empty() {
            return;
        }
        let cookie = self.cookie.clone();
        self.sock
            .try_send_mcast_everywhere(&|opts| Some(build(opts.mcast_addr(), listen_port, info_hashes, &cookie)))
            .await;
        self.messages_sent.fetch_add(1, Ordering::Relaxed);
        self.hashes_announced.fetch_add(info_hashes.len() as u64, Ordering::Relaxed);
    }

    /// The next message from someone else. Our own (same cookie) and anything
    /// that does not parse are skipped here.
    pub async fn recv(&self) -> std::io::Result<(SocketAddr, Announce)> {
        let mut buf = [0u8; 2048];
        loop {
            let (n, from) = self.sock.recv_from(&mut buf).await?;
            let Some(msg) = parse(&buf[..n]) else {
                continue;
            };
            if msg.cookie.as_deref() == Some(self.cookie.as_str()) {
                self.own_ignored.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            return Ok((from, msg));
        }
    }

    /// Hand the peers of one message to the torrents of `mgr` that may use
    /// them. Returns how many peers were offered to the dial queue.
    pub fn offer(&self, mgr: &TorrentManager, from: SocketAddr, msg: &Announce) -> usize {
        let ip = match from.ip() {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v6)),
            v4 => v4,
        };
        let peer = SocketAddr::new(ip, msg.port);
        let mut offered = 0;
        for ih in &msg.info_hashes {
            let Some(t) = mgr.get(ih) else {
                continue;
            };
            if !may_use_lsd(&t) {
                continue;
            }
            self.peers_found.fetch_add(1, Ordering::Relaxed);
            *self.found.entry(*ih).or_insert(0) += 1;
            // Same filters as a tracker's peer list (`rpc::add_peers`).
            if t.connected_addrs.contains_key(&peer) || crate::tracker::is_self_ip(peer.ip()) {
                continue;
            }
            crate::tracker::enqueue_dial(peer, t);
            offered += 1;
        }
        offered
    }

    /// Peers LSD found for one torrent since the engine started.
    pub fn found_for(&self, info_hash: &InfoHash) -> u32 {
        self.found.get(info_hash).map(|v| *v).unwrap_or(0)
    }

    pub fn stats(&self) -> serde_json::Value {
        serde_json::json!({
            "messages_sent": self.messages_sent.load(Ordering::Relaxed),
            "hashes_announced": self.hashes_announced.load(Ordering::Relaxed),
            "peers_found": self.peers_found.load(Ordering::Relaxed),
            "own_ignored": self.own_ignored.load(Ordering::Relaxed),
        })
    }
}

/// Start LSD for one engine, if `dht::lsd_policy` allows it. Returns the
/// session it attached to the manager.
pub async fn start(mgr: Arc<TorrentManager>, config: &crate::config::EngineConfig) -> Option<Arc<LsdSession>> {
    match crate::dht::lsd_policy(config) {
        crate::dht::Discovery::On => {}
        crate::dht::Discovery::Off(why) if why == crate::dht::OFF_BY_CONFIG => {
            info!("[lsd] Local Service Discovery disabled by config");
            return None;
        }
        crate::dht::Discovery::Off(why) => {
            warn!("[lsd] Local Service Discovery {}", why);
            return None;
        }
    }
    let device = Some(config.bind_device.trim()).filter(|d| !d.is_empty());
    let session = match LsdSession::bind(device, LSD_PORT).await {
        Ok(s) => s,
        Err(e) => {
            // Not fatal: trackers, DHT and PEX are untouched.
            warn!("[lsd] not started ({}): no multicast on {}", e, device.unwrap_or("any interface"));
            return None;
        }
    };
    match device {
        Some(d) => info!("[lsd] listening on {}:{} and [{}]:{}, pinned to device {}", LSD_V4, LSD_PORT, LSD_V6, LSD_PORT, d),
        None => info!("[lsd] listening on {}:{} and [{}]:{}", LSD_V4, LSD_PORT, LSD_V6, LSD_PORT),
    }
    mgr.set_lsd(session.clone());

    let (s, m) = (session.clone(), mgr.clone());
    tokio::spawn(async move {
        loop {
            match s.recv().await {
                Ok((from, msg)) => {
                    let n = s.offer(&m, from, &msg);
                    if n > 0 {
                        debug!("[lsd] {} offered {} peer(s)", from, n);
                    }
                }
                Err(e) => {
                    // A socket error is not a reason to spin.
                    debug!("[lsd] recv: {}", e);
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    });

    let configured_port = config.listen_port;
    let (s, m) = (session.clone(), mgr);
    tokio::spawn(async move {
        // Give the listeners a moment: an announce names a port that must
        // already accept.
        tokio::time::sleep(Duration::from_secs(5)).await;
        let mut schedule = Schedule::default();
        let mut tick = tokio::time::interval(ROUND);
        loop {
            tick.tick().await;
            let plan = schedule.plan(&candidates(&m), Instant::now());
            // The port the listener holds: a LAN peer connects to it directly,
            // never through the VPN's translated port.
            let port = m.listen_port_now(configured_port);
            for message in plan {
                s.announce(port, &message).await;
                // Spread over the round rather than a burst of 60 datagrams.
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    });
    Some(session)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ih(n: u8) -> InfoHash {
        [n; 20]
    }

    fn v4_group() -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(LSD_V4, LSD_PORT))
    }

    /// ⭐ BEP 14 to the byte: request line, Host, Port, one Infohash header per
    /// torrent, cookie, and the two trailing CRLFs.
    #[test]
    fn the_message_is_bep14_to_the_byte() {
        let msg = build(v4_group(), 6881, &[ih(0xab), ih(0x01)], "c00k1e");
        assert_eq!(
            msg,
            "BT-SEARCH * HTTP/1.1\r\n\
             Host: 239.192.152.143:6771\r\n\
             Port: 6881\r\n\
             Infohash: abababababababababababababababababababab\r\n\
             Infohash: 0101010101010101010101010101010101010101\r\n\
             cookie: c00k1e\r\n\
             \r\n\r\n"
        );
        let v6 = build(SocketAddr::V6(SocketAddrV6::new(LSD_V6, LSD_PORT, 0, 3)), 1, &[ih(1)], "");
        assert!(v6.starts_with("BT-SEARCH * HTTP/1.1\r\nHost: [ff15::efc0:988f]:6771\r\n"), "{v6}");
        assert!(!v6.contains("cookie"), "no cookie header when there is none");
    }

    #[test]
    fn a_message_parses_back_to_what_was_built() {
        let msg = build(v4_group(), 51413, &[ih(7), ih(9)], "abc");
        assert_eq!(
            parse(msg.as_bytes()),
            Some(Announce { port: 51413, info_hashes: vec![ih(7), ih(9)], cookie: Some("abc".into()) })
        );
    }

    /// What other clients send: upper-case hex, `Cookie` capitalised, bare
    /// `\n` line ends. And what must be refused: another protocol, no port, a
    /// v2 hash only, port 0.
    #[test]
    fn parse_takes_other_clients_and_refuses_junk() {
        let other = "BT-SEARCH * HTTP/1.1\nHost: 239.192.152.143:6771\nPort: 6881\nInfohash: ABABABABABABABABABABABABABABABABABABABAB\nCookie: x\n\n";
        let a = parse(other.as_bytes()).expect("parses");
        assert_eq!(a.info_hashes, vec![ih(0xab)]);
        assert_eq!(a.cookie.as_deref(), Some("x"));

        assert_eq!(parse(b"M-SEARCH * HTTP/1.1\r\nPort: 1\r\nInfohash: abababababababababababababababababababab\r\n\r\n"), None);
        assert_eq!(parse(b"BT-SEARCH * HTTP/1.1\r\nInfohash: abababababababababababababababababababab\r\n\r\n"), None);
        assert_eq!(parse(b"BT-SEARCH * HTTP/1.1\r\nPort: 0\r\nInfohash: abababababababababababababababababababab\r\n\r\n"), None);
        let v2 = format!("BT-SEARCH * HTTP/1.1\r\nPort: 1\r\nInfohash: {}\r\n\r\n", "ab".repeat(32));
        assert_eq!(parse(v2.as_bytes()), None);
    }

    /// ⭐ The cap: a million active torrents are at most
    /// `MAX_MESSAGES_PER_ROUND` messages a round, downloads first; the rest
    /// wait their turn, and nobody is announced twice within `REANNOUNCE`.
    #[test]
    fn a_million_torrents_are_sixty_messages_a_minute() {
        let mut s = Schedule::default();
        let mut cands: Vec<Candidate> = (0..1_000_000u32)
            .map(|i| {
                let mut h = [0u8; 20];
                h[..4].copy_from_slice(&i.to_be_bytes());
                Candidate { info_hash: h, downloading: false }
            })
            .collect();
        cands[999_999].downloading = true;
        let t0 = Instant::now();
        let plan = s.plan(&cands, t0);
        assert_eq!(plan.len(), MAX_MESSAGES_PER_ROUND);
        assert!(plan.iter().all(|m| m.len() <= HASHES_PER_MESSAGE));
        assert_eq!(plan[0][0], cands[999_999].info_hash, "the download goes first");

        // The next round announces others, not the same ones again.
        let first: HashSet<InfoHash> = plan.iter().flatten().copied().collect();
        let next = s.plan(&cands, t0 + ROUND);
        assert_eq!(next.len(), MAX_MESSAGES_PER_ROUND);
        assert!(next.iter().flatten().all(|h| !first.contains(h)));

        // A small active set: each torrent once per REANNOUNCE, no more.
        let mut s = Schedule::default();
        let few = &cands[..3];
        assert_eq!(s.plan(few, t0).concat().len(), 3);
        assert!(s.plan(few, t0 + ROUND).is_empty(), "not due before 5 minutes");
        assert_eq!(s.plan(few, t0 + REANNOUNCE).concat().len(), 3);
    }

    fn manager(tag: &str) -> (Arc<TorrentManager>, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("typhon-lsd-{tag}-{}", std::process::id()));
        let (data, resume) = (root.join("data"), root.join("resume"));
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&resume).unwrap();
        let mgr = Arc::new(TorrentManager::new(
            data.to_string_lossy().into_owned(),
            resume.to_string_lossy().into_owned(),
            Arc::new(crate::disk::DiskManager::new(16)),
        ));
        (mgr, root)
    }

    fn torrent_bytes(name: &str, private: bool) -> Vec<u8> {
        let mut info = format!("d6:lengthi16384e4:name{}:{name}12:piece lengthi16384e6:pieces20:", name.len()).into_bytes();
        let mut piece = [0xCDu8; 20];
        piece[..name.len().min(20)].copy_from_slice(&name.as_bytes()[..name.len().min(20)]);
        info.extend_from_slice(&piece);
        if private {
            info.extend_from_slice(b"7:privatei1e");
        }
        info.push(b'e');
        let mut out = b"d4:info".to_vec();
        out.extend_from_slice(&info);
        out.push(b'e');
        out
    }

    /// ⭐ A private torrent is never a candidate, downloading or not, and an
    /// announce naming it from the LAN is not turned into a peer (BEP 27).
    #[test]
    fn a_private_torrent_is_never_announced_nor_fed() {
        let (mgr, root) = manager("private");
        let (public, _) = mgr.add_torrent_bytes(&torrent_bytes("public", false), "/tmp", false, false).unwrap();
        let (private, _) = mgr.add_torrent_bytes(&torrent_bytes("private", true), "/tmp", false, false).unwrap();
        let (stopped, _) = mgr.add_torrent_bytes(&torrent_bytes("stopped", false), "/tmp", true, false).unwrap();
        assert!(mgr.get(&private).unwrap().meta.private, "the fixture must really be private");
        // Downloading and with a peer: everything that would make it a
        // candidate if it were public.
        let p = mgr.get(&private).unwrap();
        p.connected_addrs.insert("192.0.2.1:1".parse().unwrap(), ());

        let got: Vec<InfoHash> = candidates(&mgr).iter().map(|c| c.info_hash).collect();
        assert_eq!(got, vec![public], "only the public, running torrent");
        assert!(!got.contains(&stopped));

        let plan = Schedule::default().plan(&candidates(&mgr), Instant::now());
        assert!(plan.iter().flatten().all(|h| *h != private));

        // `offer` needs a session; the predicate it applies is this one.
        assert!(!may_use_lsd(&p));
        assert!(may_use_lsd(&mgr.get(&public).unwrap()));
        let _ = std::fs::remove_dir_all(root);
    }

    /// ⭐ Behind the SOCKS5 proxy no LSD socket is opened, whatever
    /// `lsd_enabled` says: `start` returns before binding.
    #[tokio::test]
    async fn a_proxied_engine_opens_no_lsd_socket() {
        let (mgr, root) = manager("socks");
        let cfg: crate::config::EngineConfig =
            serde_json::from_str(r#"{"lsd_enabled":true,"socks5_outbound_host":"127.0.0.1"}"#).unwrap();
        assert!(start(mgr.clone(), &cfg).await.is_none());
        assert!(mgr.lsd().is_none());
        let off: crate::config::EngineConfig = serde_json::from_str(r#"{"lsd_enabled":false}"#).unwrap();
        assert!(start(mgr.clone(), &off).await.is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    /// A device that does not exist is an LSD that does not start, never one
    /// on the default route.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn an_lsd_pinned_to_a_missing_device_does_not_start() {
        assert!(LsdSession::bind(Some("hy-nodev0"), free_udp_port()).await.is_err());
    }

    fn free_udp_port() -> u16 {
        std::net::UdpSocket::bind("0.0.0.0:0").unwrap().local_addr().unwrap().port()
    }

    /// ⭐ Two engines on one host find each other, and neither takes its own
    /// announce for a peer. Real multicast, on a test port rather than 6771 so
    /// a client running on the test host is not disturbed. Skipped (not
    /// failed) where the platform has no multicast-capable interface.
    #[tokio::test]
    async fn two_engines_discover_each_other_and_ignore_themselves() {
        let port = free_udp_port();
        let (a, b) = match (LsdSession::bind(None, port).await, LsdSession::bind(None, port).await) {
            (Ok(a), Ok(b)) => (a, b),
            (Err(e), _) | (_, Err(e)) => {
                eprintln!("no multicast here ({e}): skipped");
                return;
            }
        };
        assert_ne!(a.cookie(), b.cookie());
        let wait = Duration::from_secs(3);

        a.announce(16171, &[ih(0x42)]).await;
        let (from, msg) = tokio::time::timeout(wait, b.recv()).await.expect("b hears a").unwrap();
        assert_eq!(msg.port, 16171);
        assert_eq!(msg.info_hashes, vec![ih(0x42)]);
        assert_eq!(msg.cookie.as_deref(), Some(a.cookie()));
        assert!(from.port() == port, "from a's LSD socket: {from}");

        // a got its own message back and dropped it: nothing for a to read.
        assert!(tokio::time::timeout(Duration::from_millis(500), a.recv()).await.is_err());
        assert!(a.stats()["own_ignored"].as_u64().unwrap() >= 1, "a saw its own announce and skipped it");

        b.announce(16172, &[ih(0x43)]).await;
        let (_, msg) = tokio::time::timeout(wait, a.recv()).await.expect("a hears b").unwrap();
        assert_eq!((msg.port, msg.info_hashes), (16172, vec![ih(0x43)]));
    }
}
