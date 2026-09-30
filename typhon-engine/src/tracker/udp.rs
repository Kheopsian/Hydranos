//! BEP 15: announcing to a `udp://` tracker.
//!
//! Two round trips where HTTP has one: `connect` hands out a connection id
//! that proves we can receive at our source address, then `announce` uses it.
//! The id is good for a minute, so it is kept per tracker address and the
//! common case is a single round trip.
//!
//! ## One socket per family, not one per announce
//!
//! A catalogue of a million torrents announces hundreds of times a second. A
//! socket per request would be as many ephemeral ports, file descriptors and
//! bind calls. Instead each address family has ONE socket, and one task reads
//! it and hands every datagram to whoever is waiting on its transaction id --
//! the same shape libtorrent uses. A reply is only delivered if it comes from
//! the address the request went to: a transaction id is 32 random bits, not a
//! credential, and anybody can send us a datagram.
//!
//! ## What goes on the wire is decided elsewhere
//!
//! Counters, event, key, numwant and `ip` arrive in `UdpAnnounce`, filled by
//! the announcer from the very same inputs as the HTTP URL. This module only
//! encodes and transports; a tracker cannot be told something over UDP that
//! it would not have been told over HTTP.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use tokio::net::UdpSocket;
use tokio::sync::oneshot;

use super::http::{AnnounceResponse, IpMode};

/// BEP 15's magic constant, sent in every `connect`.
const PROTOCOL_ID: u64 = 0x0417_2710_1980;
const ACTION_CONNECT: u32 = 0;
const ACTION_ANNOUNCE: u32 = 1;
const ACTION_ERROR: u32 = 3;

/// How long a connection id is used. BEP 15 lets a client keep one for a
/// minute; a little less leaves room for the tracker's clock and ours.
const CONNECTION_TTL: Duration = Duration::from_secs(50);

/// How long a resolved tracker address is reused. Every announce resolving
/// its tracker again would be hundreds of lookups a second for a handful of
/// names.
const DNS_TTL: Duration = Duration::from_secs(300);

/// The waits of one exchange, one per attempt.
///
/// ⚠️ Not BEP 15's `15 * 2^n` seconds. That schedule reaches an hour after
/// eight tries and was written for a client with a handful of torrents; the
/// announcer here has its own retry, its own breaker and a pass to finish.
/// Two short attempts per step bound a whole announce -- connect plus
/// announce -- to about the fifteen seconds an HTTP one is allowed, which is
/// what libtorrent does too.
const WAITS: [Duration; 2] = [Duration::from_secs(3), Duration::from_secs(5)];

/// BEP 41 caps one URLData option at 255 bytes; longer data is split over
/// several options, which the tracker concatenates.
const URL_DATA_CHUNK: usize = 255;

/// Everything one UDP announce says, already decided by the announcer.
#[derive(Debug, Clone, PartialEq)]
pub struct UdpAnnounce {
    /// The tracker's URL, passkey already applied: host and port come from
    /// it, and its path and query travel as BEP 41 URLData.
    pub tracker: String,
    pub info_hash: [u8; 20],
    pub peer_id: [u8; 20],
    pub downloaded: u64,
    pub left: u64,
    pub uploaded: u64,
    /// 0 none, 1 completed, 2 started, 3 stopped.
    pub event: u32,
    /// The BEP 7 `ip=` of HTTP, IPv4 only; 0 lets the tracker use the source.
    pub ip: u32,
    pub key: u32,
    pub num_want: i32,
    pub port: u16,
}

/// The event names the announcer uses, as BEP 15 numbers them.
pub fn event_code(event: &str) -> u32 {
    match event {
        "completed" => 1,
        "started" => 2,
        "stopped" => 3,
        _ => 0,
    }
}

pub fn is_udp(url: &str) -> bool {
    url.len() > 6 && url[..6].eq_ignore_ascii_case("udp://")
}

/// Host, port and BEP 41 URL data (path and query) of a `udp://` URL.
///
/// A port is required: there is no default port for a UDP tracker, and
/// guessing one would send announces to whatever listens there.
pub fn split_url(url: &str) -> Option<(String, u16, String)> {
    if !is_udp(url) {
        return None;
    }
    let rest = &url[6..];
    let (authority, tail) = match rest.find(['/', '?']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let (host, port) = if let Some(inner) = authority.strip_prefix('[') {
        let (h, after) = inner.split_once(']')?;
        (h, after.strip_prefix(':')?)
    } else {
        authority.rsplit_once(':')?
    };
    let port: u16 = port.parse().ok().filter(|p| *p != 0)?;
    if host.is_empty() {
        return None;
    }
    Some((host.to_string(), port, tail.to_string()))
}

pub(crate) fn connect_packet(tx: u32) -> [u8; 16] {
    let mut p = [0u8; 16];
    p[..8].copy_from_slice(&PROTOCOL_ID.to_be_bytes());
    p[8..12].copy_from_slice(&ACTION_CONNECT.to_be_bytes());
    p[12..16].copy_from_slice(&tx.to_be_bytes());
    p
}

/// The 98-byte announce, then the URL data as BEP 41 options.
///
/// `ip` is taken from the request on the IPv4 leg only: the field is four
/// bytes, and on an IPv6 announce the tracker must use the source address.
pub(crate) fn announce_packet(conn: u64, tx: u32, a: &UdpAnnounce, url_data: &str, v6: bool) -> Vec<u8> {
    let mut p = Vec::with_capacity(98 + url_data.len() + 8);
    p.extend_from_slice(&conn.to_be_bytes());
    p.extend_from_slice(&ACTION_ANNOUNCE.to_be_bytes());
    p.extend_from_slice(&tx.to_be_bytes());
    p.extend_from_slice(&a.info_hash);
    p.extend_from_slice(&a.peer_id);
    p.extend_from_slice(&a.downloaded.to_be_bytes());
    p.extend_from_slice(&a.left.to_be_bytes());
    p.extend_from_slice(&a.uploaded.to_be_bytes());
    p.extend_from_slice(&a.event.to_be_bytes());
    p.extend_from_slice(&(if v6 { 0 } else { a.ip }).to_be_bytes());
    p.extend_from_slice(&a.key.to_be_bytes());
    p.extend_from_slice(&a.num_want.to_be_bytes());
    p.extend_from_slice(&a.port.to_be_bytes());
    // The path and query are how a tracker that keys users by URL -- a
    // passkey in `/announce/<key>` -- knows who is asking. Without them a
    // private UDP tracker would see an anonymous announce.
    for chunk in url_data.as_bytes().chunks(URL_DATA_CHUNK) {
        p.push(0x2);
        p.push(chunk.len() as u8);
        p.extend_from_slice(chunk);
    }
    p
}

/// Offset of the transaction id in each request, rewritten on every attempt.
const CONNECT_TX_AT: usize = 12;
const ANNOUNCE_TX_AT: usize = 12;

/// Check a reply's header and return what follows it.
///
/// An error reply is the tracker refusing us, in its own words: it is
/// reported the way an HTTP `failure reason` is, `tracker: <message>`, so the
/// announcer treats the two identically.
pub(crate) fn reply_body(buf: &[u8], tx: u32, want: u32) -> Result<&[u8], String> {
    if buf.len() < 8 {
        return Err(format!("udp: short reply ({} bytes)", buf.len()));
    }
    let action = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let got_tx = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
    if got_tx != tx {
        return Err("udp: reply for another transaction".into());
    }
    if action == ACTION_ERROR {
        let msg = String::from_utf8_lossy(&buf[8..]);
        let msg = msg.trim_end_matches('\0').trim();
        return Err(format!("tracker: {}", if msg.is_empty() { "(no reason given)" } else { msg }));
    }
    if action != want {
        return Err(format!("udp: tracker answered action {action}, expected {want}"));
    }
    Ok(&buf[8..])
}

pub(crate) fn parse_connect(buf: &[u8], tx: u32) -> Result<u64, String> {
    let body = reply_body(buf, tx, ACTION_CONNECT)?;
    if body.len() < 8 {
        return Err("udp: short connect reply".into());
    }
    Ok(u64::from_be_bytes(body[..8].try_into().unwrap()))
}

/// An announce reply. The peers are 6-byte entries on an IPv4 exchange and
/// 18-byte ones on IPv6: BEP 15 ties the format to the family the request
/// used, there is no marker in the packet.
pub(crate) fn parse_announce(buf: &[u8], tx: u32, v6: bool) -> Result<AnnounceResponse, String> {
    let body = reply_body(buf, tx, ACTION_ANNOUNCE)?;
    if body.len() < 12 {
        return Err("udp: short announce reply".into());
    }
    let word = |i: usize| u32::from_be_bytes(body[i..i + 4].try_into().unwrap());
    let interval = match word(0) {
        0 => super::http::DEFAULT_INTERVAL,
        n => n.min(super::http::MAX_INTERVAL as u32),
    };
    let incomplete = word(4);
    let complete = word(8);
    let stride = if v6 { 18 } else { 6 };
    let mut peers = Vec::new();
    for c in body[12..].chunks_exact(stride) {
        let port = u16::from_be_bytes([c[stride - 2], c[stride - 1]]);
        // Port 0 is not a listening peer.
        if port == 0 {
            continue;
        }
        let ip = if v6 {
            let mut b = [0u8; 16];
            b.copy_from_slice(&c[..16]);
            IpAddr::V6(Ipv6Addr::from(b))
        } else {
            IpAddr::V4(Ipv4Addr::new(c[0], c[1], c[2], c[3]))
        };
        peers.push(SocketAddr::new(ip, port));
    }
    Ok(AnnounceResponse {
        interval,
        // BEP 15 has no `min interval`, `tracker id` or warning.
        min_interval: 0,
        peers,
        complete,
        incomplete,
        failure: None,
        tracker_id: None,
        warning: None,
    })
}

/// One family's socket, and who is waiting for what on it.
struct Family {
    sock: Arc<UdpSocket>,
    pending: Arc<DashMap<u32, (SocketAddr, oneshot::Sender<Vec<u8>>)>>,
    /// False once the reading task is gone -- its runtime shut down. The
    /// socket is then registered with a reactor that no longer runs, and the
    /// family has to be built again on the current one.
    alive: Arc<AtomicBool>,
}

/// Flips `alive` when the reading task ends, however it ends.
struct AliveGuard(Arc<AtomicBool>);
impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

impl Family {
    fn open(v6: bool) -> Result<Family, String> {
        let bind = if v6 {
            SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0)
        } else {
            SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)
        };
        let std = std::net::UdpSocket::bind(bind).map_err(|e| format!("udp: bind {bind}: {e}"))?;
        std.set_nonblocking(true).map_err(|e| format!("udp: {e}"))?;
        let sock = Arc::new(UdpSocket::from_std(std).map_err(|e| format!("udp: {e}"))?);
        let pending: Arc<DashMap<u32, (SocketAddr, oneshot::Sender<Vec<u8>>)>> = Arc::new(DashMap::new());
        let alive = Arc::new(AtomicBool::new(true));
        {
            let sock = sock.clone();
            let pending = pending.clone();
            let guard = AliveGuard(alive.clone());
            tokio::spawn(async move {
                let _guard = guard;
                let mut buf = vec![0u8; 4096];
                loop {
                    match sock.recv_from(&mut buf).await {
                        Ok((n, from)) => {
                            if n < 8 {
                                continue;
                            }
                            let tx = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
                            if let Some((_, (_, reply))) = pending.remove_if(&tx, |_, (to, _)| *to == from) {
                                let _ = reply.send(buf[..n].to_vec());
                            }
                        }
                        // An ICMP "port unreachable" surfaces on some
                        // platforms as an error on the NEXT receive. It says
                        // nothing about the other exchanges in flight; the
                        // one it concerns times out on its own.
                        Err(_) => tokio::time::sleep(Duration::from_millis(5)).await,
                    }
                }
            });
        }
        Ok(Family { sock, pending, alive })
    }

    /// Send, wait, send again with a new transaction id, as WAITS says.
    async fn exchange(&self, to: SocketAddr, mut packet: Vec<u8>, tx_at: usize) -> Result<(Vec<u8>, u32), String> {
        for wait in WAITS {
            let tx = loop {
                let t = rand::random::<u32>();
                if !self.pending.contains_key(&t) {
                    break t;
                }
            };
            packet[tx_at..tx_at + 4].copy_from_slice(&tx.to_be_bytes());
            let (reply_tx, reply_rx) = oneshot::channel();
            self.pending.insert(tx, (to, reply_tx));
            if let Err(e) = self.sock.send_to(&packet, to).await {
                self.pending.remove(&tx);
                return Err(format!("udp: send to {to}: {e}"));
            }
            match tokio::time::timeout(wait, reply_rx).await {
                Ok(Ok(reply)) => return Ok((reply, tx)),
                _ => {
                    self.pending.remove(&tx);
                }
            }
        }
        Err(format!("udp: {to} timed out"))
    }
}

/// The UDP announce client: sockets, connection ids and resolved addresses.
pub struct Client {
    v4: Mutex<Option<Arc<Family>>>,
    v6: Mutex<Option<Arc<Family>>>,
    conns: DashMap<SocketAddr, (u64, Instant)>,
    dns: DashMap<(String, u16, bool), (SocketAddr, Instant)>,
}

impl Default for Client {
    fn default() -> Self {
        Client {
            v4: Mutex::new(None),
            v6: Mutex::new(None),
            conns: DashMap::new(),
            dns: DashMap::new(),
        }
    }
}

impl Client {
    fn family(&self, v6: bool) -> Result<Arc<Family>, String> {
        let cell = if v6 { &self.v6 } else { &self.v4 };
        let mut slot = cell.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(f) = slot.as_ref().filter(|f| f.alive.load(Ordering::Relaxed)) {
            return Ok(f.clone());
        }
        let f = Arc::new(Family::open(v6)?);
        *slot = Some(f.clone());
        Ok(f)
    }

    async fn resolve(&self, host: &str, port: u16, v6: bool) -> Result<SocketAddr, String> {
        let key = (host.to_ascii_lowercase(), port, v6);
        if let Some(hit) = self.dns.get(&key).filter(|e| e.1.elapsed() < DNS_TTL) {
            return Ok(hit.0);
        }
        let found: Vec<SocketAddr> = match host.parse::<IpAddr>() {
            Ok(ip) => vec![SocketAddr::new(ip, port)],
            Err(_) => tokio::net::lookup_host((host, port))
                .await
                .map_err(|e| format!("udp: dns lookup of {host} failed: {e}"))?
                .collect(),
        };
        let addr = found
            .into_iter()
            .find(|a| a.is_ipv6() == v6)
            .ok_or_else(|| {
                format!("udp: {host} has no {} address (unreachable)", if v6 { "IPv6" } else { "IPv4" })
            })?;
        self.dns.insert(key, (addr, Instant::now()));
        Ok(addr)
    }

    async fn announce_family(&self, a: &UdpAnnounce, host: &str, port: u16, url_data: &str, v6: bool) -> Result<AnnounceResponse, String> {
        let fam = self.family(v6)?;
        let to = self.resolve(host, port, v6).await?;
        let conn = match self.conns.get(&to).filter(|e| e.1.elapsed() < CONNECTION_TTL).map(|e| e.0) {
            Some(id) => id,
            None => {
                let (reply, tx) = fam.exchange(to, connect_packet(0).to_vec(), CONNECT_TX_AT).await?;
                let id = parse_connect(&reply, tx)?;
                self.conns.insert(to, (id, Instant::now()));
                id
            }
        };
        let packet = announce_packet(conn, 0, a, url_data, v6);
        let answer = match fam.exchange(to, packet, ANNOUNCE_TX_AT).await {
            Ok((reply, tx)) => parse_announce(&reply, tx, v6),
            Err(e) => Err(e),
        };
        if answer.is_err() {
            // Whatever went wrong, the id is the first suspect: a tracker
            // that restarted forgets every id it issued and ignores, or
            // refuses, announces that carry one.
            self.conns.remove(&to);
        }
        answer
    }

    pub async fn announce(&self, a: &UdpAnnounce, mode: IpMode) -> Result<AnnounceResponse, String> {
        let (host, port, url_data) =
            split_url(&a.tracker).ok_or_else(|| "udp: not a udp://host:port tracker URL".to_string())?;
        match mode {
            IpMode::V4 => self.announce_family(a, &host, port, &url_data, false).await,
            IpMode::V6 => self.announce_family(a, &host, port, &url_data, true).await,
            // Both families with the same peer id, as over HTTP: one peer,
            // two addresses.
            IpMode::Auto => {
                let (four, six) = tokio::join!(
                    self.announce_family(a, &host, port, &url_data, false),
                    self.announce_family(a, &host, port, &url_data, true),
                );
                super::http::merge_announce(four, six)
            }
        }
    }
}

static CLIENT: OnceLock<Client> = OnceLock::new();

/// Announce to a `udp://` tracker.
///
/// ⚠️ Refused while announces are proxied. `TYPHON_ANNOUNCE_PROXY` exists so
/// no tracker sees this host's own address, and a SOCKS proxy carries TCP:
/// sending the UDP announce directly would publish exactly what the proxy is
/// there to hide.
pub async fn send_announce(a: &UdpAnnounce, mode: IpMode) -> Result<AnnounceResponse, String> {
    if super::http::announces_proxied() {
        return Err("udp tracker skipped: announces are proxied and UDP cannot follow the proxy".into());
    }
    CLIENT.get_or_init(Client::default).announce(a, mode).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(tracker: &str) -> UdpAnnounce {
        UdpAnnounce {
            tracker: tracker.into(),
            info_hash: [0xAB; 20],
            peer_id: *b"-HY4240-abcdefghijkl",
            downloaded: 1,
            left: 2,
            uploaded: 3,
            event: event_code("started"),
            ip: u32::from(Ipv4Addr::new(203, 0, 113, 7)),
            key: 0xDEADBEEF,
            num_want: 200,
            port: 16172,
        }
    }

    #[test]
    fn a_udp_url_gives_its_host_port_and_path() {
        assert_eq!(
            split_url("udp://tracker.opentrackr.org:1337/announce"),
            Some(("tracker.opentrackr.org".into(), 1337, "/announce".into()))
        );
        assert_eq!(split_url("UDP://t.example:80"), Some(("t.example".into(), 80, "".into())));
        assert_eq!(
            split_url("udp://[2001:db8::1]:6969/announce/KEY?x=1"),
            Some(("2001:db8::1".into(), 6969, "/announce/KEY?x=1".into()))
        );
        assert_eq!(split_url("udp://t.example/announce"), None, "no port, no guess");
        assert_eq!(split_url("udp://t.example:0/announce"), None);
        assert_eq!(split_url("http://t.example:80/announce"), None);
    }

    /// Byte for byte against BEP 15's layout: a field one byte off is a
    /// tracker answering about a torrent nobody has.
    #[test]
    fn the_announce_packet_is_laid_out_as_bep_15_says() {
        let a = req("udp://t.example:6969/announce");
        let p = announce_packet(0x0102030405060708, 0x11223344, &a, "", false);
        assert_eq!(p.len(), 98);
        assert_eq!(&p[0..8], &0x0102030405060708u64.to_be_bytes());
        assert_eq!(&p[8..12], &1u32.to_be_bytes(), "action announce");
        assert_eq!(&p[12..16], &0x11223344u32.to_be_bytes());
        assert_eq!(&p[16..36], &[0xAB; 20]);
        assert_eq!(&p[36..56], b"-HY4240-abcdefghijkl");
        assert_eq!(&p[56..64], &1u64.to_be_bytes(), "downloaded");
        assert_eq!(&p[64..72], &2u64.to_be_bytes(), "left");
        assert_eq!(&p[72..80], &3u64.to_be_bytes(), "uploaded");
        assert_eq!(&p[80..84], &2u32.to_be_bytes(), "started is 2");
        assert_eq!(&p[84..88], &[203, 0, 113, 7]);
        assert_eq!(&p[88..92], &0xDEADBEEFu32.to_be_bytes());
        assert_eq!(&p[92..96], &200i32.to_be_bytes());
        assert_eq!(&p[96..98], &16172u16.to_be_bytes());

        let six = announce_packet(1, 2, &a, "", true);
        assert_eq!(&six[84..88], &[0, 0, 0, 0], "no IPv4 ip= on an IPv6 announce");
    }

    /// BEP 41: the path and query after the 98 bytes, in options of at most
    /// 255 bytes -- the passkey of `/announce/<key>` is in there.
    #[test]
    fn the_path_travels_as_url_data_in_255_byte_options() {
        let a = req("udp://t.example:6969/announce");
        let p = announce_packet(1, 2, &a, "/announce/KEY", false);
        assert_eq!(&p[98..], &[&[0x2u8, 13][..], b"/announce/KEY"].concat()[..]);

        let long = format!("/{}", "k".repeat(300));
        let p = announce_packet(1, 2, &a, &long, false);
        assert_eq!(p[98], 0x2);
        assert_eq!(p[99], 255);
        assert_eq!(p[98 + 2 + 255], 0x2);
        assert_eq!(p[98 + 2 + 255 + 1] as usize, 301 - 255);
        assert_eq!(p.len(), 98 + 2 + 255 + 2 + 46);
    }

    #[test]
    fn an_announce_reply_reads_counts_and_peers_of_its_family() {
        let mut r = Vec::new();
        r.extend_from_slice(&1u32.to_be_bytes());
        r.extend_from_slice(&7u32.to_be_bytes());
        r.extend_from_slice(&1800u32.to_be_bytes());
        r.extend_from_slice(&4u32.to_be_bytes()); // leechers
        r.extend_from_slice(&9u32.to_be_bytes()); // seeders
        r.extend_from_slice(&[10, 0, 0, 1, 0x1A, 0xE1]);
        r.extend_from_slice(&[10, 0, 0, 2, 0, 0]); // port 0: dropped
        let got = parse_announce(&r, 7, false).unwrap();
        assert_eq!((got.interval, got.incomplete, got.complete), (1800, 4, 9));
        assert_eq!(got.peers, vec!["10.0.0.1:6881".parse().unwrap()]);

        let mut r6 = r[..20].to_vec();
        r6.extend_from_slice(&Ipv6Addr::LOCALHOST.octets());
        r6.extend_from_slice(&6881u16.to_be_bytes());
        let got = parse_announce(&r6, 7, true).unwrap();
        assert_eq!(got.peers, vec!["[::1]:6881".parse().unwrap()]);
    }

    /// A refusal reads like an HTTP `failure reason`, so the announcer files
    /// it the same way -- "unregistered" included.
    #[test]
    fn an_error_reply_is_the_trackers_refusal_in_its_words() {
        let mut r = Vec::new();
        r.extend_from_slice(&3u32.to_be_bytes());
        r.extend_from_slice(&5u32.to_be_bytes());
        r.extend_from_slice(b"Unregistered torrent\0");
        assert_eq!(parse_announce(&r, 5, false).unwrap_err(), "tracker: Unregistered torrent");
        assert!(reply_body(&r, 6, 1).unwrap_err().contains("another transaction"));
        assert!(reply_body(&[0, 0, 0], 5, 1).is_err());
    }

    #[test]
    fn a_zero_interval_takes_the_default_and_a_huge_one_is_capped() {
        let mk = |iv: u32| {
            let mut r = Vec::new();
            r.extend_from_slice(&1u32.to_be_bytes());
            r.extend_from_slice(&1u32.to_be_bytes());
            r.extend_from_slice(&iv.to_be_bytes());
            r.extend_from_slice(&[0; 8]);
            parse_announce(&r, 1, false).unwrap().interval
        };
        assert_eq!(mk(0), super::super::http::DEFAULT_INTERVAL);
        assert_eq!(mk(u32::MAX), super::super::http::MAX_INTERVAL as u32);
    }

    /// A tracker in a test: answers connect with an id, checks the announce
    /// carries it, and replies with one peer. Everything goes over a real
    /// socket through the real client.
    async fn fake_tracker(conn_id: u64, answer_announce: bool) -> (SocketAddr, tokio::task::JoinHandle<Vec<Vec<u8>>>) {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = sock.local_addr().unwrap();
        let h = tokio::spawn(async move {
            let mut seen = Vec::new();
            let mut buf = [0u8; 2048];
            while let Ok(Ok((n, from))) =
                tokio::time::timeout(Duration::from_millis(1500), sock.recv_from(&mut buf)).await
            {
                let p = buf[..n].to_vec();
                seen.push(p.clone());
                let tx = &p[12..16];
                if p.len() == 16 && p[..8] == PROTOCOL_ID.to_be_bytes() {
                    let mut r = 0u32.to_be_bytes().to_vec();
                    r.extend_from_slice(tx);
                    r.extend_from_slice(&conn_id.to_be_bytes());
                    sock.send_to(&r, from).await.unwrap();
                } else if answer_announce {
                    assert_eq!(&p[..8], &conn_id.to_be_bytes(), "the announce carries the id");
                    let mut r = 1u32.to_be_bytes().to_vec();
                    r.extend_from_slice(tx);
                    r.extend_from_slice(&900u32.to_be_bytes());
                    r.extend_from_slice(&1u32.to_be_bytes());
                    r.extend_from_slice(&2u32.to_be_bytes());
                    r.extend_from_slice(&[127, 0, 0, 1, 0x1A, 0xE1]);
                    sock.send_to(&r, from).await.unwrap();
                }
            }
            seen
        });
        (addr, h)
    }

    /// ⭐ The whole exchange over real sockets, twice: the second announce
    /// reuses the connection id instead of connecting again.
    #[tokio::test]
    async fn an_announce_connects_once_and_reuses_the_id() {
        let (addr, tracker) = fake_tracker(0x5555_6666_7777_8888, true).await;
        let client = Client::default();
        let a = req(&format!("udp://127.0.0.1:{}/announce", addr.port()));
        let got = client.announce(&a, IpMode::V4).await.expect("answered");
        assert_eq!((got.interval, got.incomplete, got.complete), (900, 1, 2));
        assert_eq!(got.peers, vec!["127.0.0.1:6881".parse().unwrap()]);
        client.announce(&a, IpMode::V4).await.expect("answered again");

        let seen = tracker.await.unwrap();
        let connects = seen.iter().filter(|p| p.len() == 16).count();
        assert_eq!(connects, 1, "one connect for two announces");
        assert_eq!(seen.len(), 3);
        assert_eq!(&seen[1][98..], &[&[0x2u8, 9][..], b"/announce"].concat()[..], "URL data sent");
    }

    /// A tracker that never answers the announce is a timeout, classified as
    /// one, and it drops the connection id rather than trusting it again.
    #[tokio::test]
    async fn a_silent_tracker_times_out_and_forgets_the_id() {
        let (addr, _tracker) = fake_tracker(42, false).await;
        let client = Client::default();
        let a = req(&format!("udp://127.0.0.1:{}", addr.port()));
        let err = client.announce(&a, IpMode::V4).await.unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        assert!(client.conns.is_empty(), "the id is not reused after a failure");
    }

    /// A datagram with the right transaction id from the WRONG address is
    /// not a reply: 32 bits are guessable, the source address is the check.
    #[tokio::test]
    async fn a_reply_from_another_address_is_not_delivered() {
        let client = Client::default();
        let fam = client.family(false).unwrap();
        let (tx, rx) = oneshot::channel();
        let expected: SocketAddr = "127.0.0.1:9".parse().unwrap();
        fam.pending.insert(77, (expected, tx));
        let spoofer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut forged = 0u32.to_be_bytes().to_vec();
        forged.extend_from_slice(&77u32.to_be_bytes());
        forged.extend_from_slice(&[0; 8]);
        let local = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), fam.sock.local_addr().unwrap().port());
        spoofer.send_to(&forged, local).await.unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(300), rx).await.is_err(), "not delivered");
        assert!(fam.pending.contains_key(&77), "still waiting for the real one");
    }
}
