//! Name resolution through a tunnel's own DNS server.
//!
//! An engine in a managed WireGuard tunnel pins every socket to `wg-<engine>`,
//! but a tracker's NAME was still resolved by the host's resolver: the swarm
//! never saw the home address, the home ISP's DNS saw every tracker the engine
//! talked to. The provider's `.conf` names the server meant for that tunnel
//! (`DNS = 10.2.0.1`), reachable only through it; asking it from a socket
//! pinned to the tunnel keeps the question inside.
//!
//! There is no resolver crate in the tree (no hickory, no trust-dns), and the
//! need is narrow: A and AAAA, over UDP, to one or two known servers. So this
//! is a minimal stub resolver with a short cache, wired into reqwest through
//! its `dns_resolver` hook and called directly by the UDP announcer and the
//! DHT bootstrap.
//!
//! The registry is keyed by DEVICE, not by engine: every client in the engine
//! already carries the device it is pinned to, and two engines on one tunnel
//! share its server. A device the registry does not know resolves by the host,
//! as before -- `bind_interface = "wg7"` on a tunnel the operator built by hand
//! has no `.conf` here to read a server from.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};

/// One attempt at one server. A tunnel DNS server is one hop past the tunnel;
/// two seconds is generous, and an announce waits for it.
const ATTEMPT: Duration = Duration::from_secs(2);
/// Tries per server before the next one is asked.
const TRIES: usize = 2;
/// Bounds on how long an answer is reused, whatever TTL it carried. Short on
/// purpose: a tracker that moves must be followed within a minute, and the
/// floor stops a TTL of 0 from turning every announce into a query.
const CACHE_MIN: Duration = Duration::from_secs(5);
const CACHE_MAX: Duration = Duration::from_secs(60);

const TYPE_A: u16 = 1;
const TYPE_AAAA: u16 = 28;

/// What is known of one tunnel's DNS.
struct Entry {
    /// Empty: the `.conf` named no server, names go to the host's resolver.
    servers: Vec<SocketAddr>,
    /// The leak is logged once per tunnel, not once per announce.
    leak_logged: AtomicBool,
}

fn registry() -> &'static RwLock<HashMap<String, Arc<Entry>>> {
    static R: OnceLock<RwLock<HashMap<String, Arc<Entry>>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

type CacheKey = (String, String);

fn cache() -> &'static std::sync::Mutex<HashMap<CacheKey, (Vec<IpAddr>, Instant)>> {
    static C: OnceLock<std::sync::Mutex<HashMap<CacheKey, (Vec<IpAddr>, Instant)>>> = OnceLock::new();
    C.get_or_init(Default::default)
}

/// Record a tunnel's DNS servers, as its `.conf` lists them. An empty list is
/// recorded too: it is what makes the host-resolver fallback a logged leak
/// rather than a silent one.
pub fn set_servers(device: &str, servers: &[IpAddr]) {
    let device = device.trim();
    if device.is_empty() {
        return;
    }
    let entry = Arc::new(Entry {
        servers: servers.iter().map(|ip| SocketAddr::new(*ip, 53)).collect(),
        leak_logged: AtomicBool::new(false),
    });
    registry().write().unwrap_or_else(|p| p.into_inner()).insert(device.to_string(), entry);
    cache().lock().unwrap_or_else(|p| p.into_inner()).retain(|(d, _), _| d != device);
}

/// Forget a tunnel that was taken down.
pub fn forget(device: &str) {
    registry().write().unwrap_or_else(|p| p.into_inner()).remove(device.trim());
    cache().lock().unwrap_or_else(|p| p.into_inner()).retain(|(d, _), _| d != device.trim());
}

/// The servers a device resolves through: `None` when the device is not a
/// managed tunnel, `Some(empty)` when it is one whose `.conf` named none.
pub fn servers(device: &str) -> Option<Vec<SocketAddr>> {
    registry().read().unwrap_or_else(|p| p.into_inner()).get(device.trim()).map(|e| e.servers.clone())
}

/// Whether a request pinned to this device needs the tunnel resolver at all.
pub fn is_tunnel(device: &str) -> bool {
    !device.trim().is_empty() && registry().read().unwrap_or_else(|p| p.into_inner()).contains_key(device.trim())
}

#[cfg(test)]
fn host_lookups() -> &'static std::sync::Mutex<Vec<String>> {
    static H: OnceLock<std::sync::Mutex<Vec<String>>> = OnceLock::new();
    H.get_or_init(Default::default)
}

async fn host_lookup(host: &str) -> Result<Vec<IpAddr>, String> {
    #[cfg(test)]
    host_lookups().lock().unwrap().push(host.to_string());
    let addrs = tokio::net::lookup_host((host, 0))
        .await
        .map_err(|e| format!("dns lookup of {host} failed: {e}"))?;
    Ok(addrs.map(|a| a.ip()).collect())
}

/// Resolve `host` for a socket pinned to `device`.
///
/// A literal address is returned as is. A device with tunnel DNS asks that
/// server from a socket pinned to the device, and NEVER falls back to the
/// host: a tunnel server that does not answer is a failed lookup, the same
/// way a tunnel that is down is a failed connection. Anything else goes to
/// the host's resolver, which for a managed tunnel with no `DNS =` line is
/// logged once as the name leak it is.
pub async fn resolve(device: &str, host: &str) -> Result<Vec<IpAddr>, String> {
    let host = host.trim().trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![ip]);
    }
    let device = device.trim();
    let entry = if device.is_empty() {
        None
    } else {
        registry().read().unwrap_or_else(|p| p.into_inner()).get(device).cloned()
    };
    let Some(entry) = entry else {
        return host_lookup(host).await;
    };
    if entry.servers.is_empty() {
        if !entry.leak_logged.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                device,
                "tunnel DNS: the WireGuard file names no DNS server, so tracker names are resolved by the \
                 host's resolver -- the swarm sees the tunnel, the host's DNS provider sees every tracker name. \
                 Add a `DNS =` line to the file to keep them inside"
            );
        }
        return host_lookup(host).await;
    }

    let key = (device.to_string(), host.to_ascii_lowercase());
    if let Some((ips, until)) = cache().lock().unwrap_or_else(|p| p.into_inner()).get(&key) {
        if Instant::now() < *until {
            return Ok(ips.clone());
        }
    }
    let mut last = format!("tunnel DNS on {device}: no server answered for {host}");
    for server in &entry.servers {
        for _ in 0..TRIES {
            match query(device, *server, host).await {
                Ok((ips, ttl)) if !ips.is_empty() => {
                    let keep = ttl.clamp(CACHE_MIN, CACHE_MAX);
                    cache()
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .insert(key, (ips.clone(), Instant::now() + keep));
                    return Ok(ips);
                }
                // An authoritative "no such name" will not change on retry.
                Ok(_) => {
                    last = format!("tunnel DNS on {device}: {host} has no address");
                    break;
                }
                Err(e) => last = format!("tunnel DNS on {device}: {e}"),
            }
        }
    }
    Err(last)
}

/// A UDP socket pinned to the device, connected to the server.
fn pinned_socket(device: &str, server: SocketAddr) -> Result<tokio::net::UdpSocket, String> {
    let bind: SocketAddr = if server.is_ipv6() { "[::]:0".parse().unwrap() } else { "0.0.0.0:0".parse().unwrap() };
    let std = std::net::UdpSocket::bind(bind).map_err(|e| format!("bind: {e}"))?;
    // Pinned or not opened at all: an unpinned query would leave by the
    // default route, which is the leak this module exists to close.
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        let egress = crate::netpin::Egress { device: device.to_string(), ..Default::default() };
        crate::netpin::pin_fd(std.as_raw_fd(), &egress).map_err(|e| format!("pin to {device}: {e}"))?;
    }
    #[cfg(not(target_os = "linux"))]
    return Err(format!("a DNS query cannot be pinned to {device:?} on this platform"));
    #[allow(unreachable_code)]
    {
        std.connect(server).map_err(|e| format!("{server}: {e}"))?;
        std.set_nonblocking(true).map_err(|e| e.to_string())?;
        tokio::net::UdpSocket::from_std(std).map_err(|e| e.to_string())
    }
}

/// Ask one server for A and AAAA at once; the addresses and the smallest TTL.
async fn query(device: &str, server: SocketAddr, host: &str) -> Result<(Vec<IpAddr>, Duration), String> {
    let sock = pinned_socket(device, server)?;
    let base: u16 = rand::random();
    let ids = [base, base.wrapping_add(1)];
    for (id, qtype) in ids.iter().zip([TYPE_A, TYPE_AAAA]) {
        let q = encode_query(*id, host, qtype)?;
        sock.send(&q).await.map_err(|e| format!("{server}: {e}"))?;
    }
    let mut ips = Vec::new();
    let mut ttl = CACHE_MAX;
    let mut answered = [false, false];
    let deadline = tokio::time::Instant::now() + ATTEMPT;
    let mut buf = [0u8; 1500];
    while !(answered[0] && answered[1]) {
        let n = match tokio::time::timeout_at(deadline, sock.recv(&mut buf)).await {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(format!("{server}: {e}")),
            Err(_) if answered.iter().any(|a| *a) => break,
            Err(_) => return Err(format!("{server} did not answer")),
        };
        let Some(slot) = buf.get(..2).and_then(|b| ids.iter().position(|id| id.to_be_bytes() == b)) else {
            continue;
        };
        if answered[slot] {
            continue;
        }
        answered[slot] = true;
        if let Ok((mut found, t)) = decode_answer(&buf[..n]) {
            ips.append(&mut found);
            ttl = ttl.min(t);
        }
    }
    Ok((ips, ttl))
}

/// A standard query, recursion desired, one question.
fn encode_query(id: u16, host: &str, qtype: u16) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(18 + host.len());
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&[0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
    for label in host.trim_end_matches('.').split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(format!("{host:?} is not a host name"));
        }
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
    out.extend_from_slice(&qtype.to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes());
    Ok(out)
}

/// Step over a name, compressed or not; the offset just past it.
fn skip_name(msg: &[u8], mut at: usize) -> Option<usize> {
    loop {
        let len = *msg.get(at)? as usize;
        if len == 0 {
            return Some(at + 1);
        }
        if len & 0xC0 == 0xC0 {
            msg.get(at + 1)?;
            return Some(at + 2);
        }
        at += 1 + len;
    }
}

/// The A and AAAA records of an answer, whatever name they hang off (a CNAME
/// chain ends in them), with the smallest TTL among them.
fn decode_answer(msg: &[u8]) -> Result<(Vec<IpAddr>, Duration), String> {
    if msg.len() < 12 || msg[2] & 0x80 == 0 {
        return Err("not a DNS answer".into());
    }
    let rcode = msg[3] & 0x0F;
    if rcode != 0 {
        return Err(format!("DNS error {rcode}"));
    }
    let qd = u16::from_be_bytes([msg[4], msg[5]]) as usize;
    let an = u16::from_be_bytes([msg[6], msg[7]]) as usize;
    let mut at = 12;
    for _ in 0..qd {
        at = skip_name(msg, at).ok_or("truncated question")? + 4;
    }
    let mut ips = Vec::new();
    let mut ttl = u32::MAX;
    for _ in 0..an {
        at = skip_name(msg, at).ok_or("truncated answer")?;
        let rr = msg.get(at..at + 10).ok_or("truncated answer")?;
        let rtype = u16::from_be_bytes([rr[0], rr[1]]);
        let rttl = u32::from_be_bytes([rr[4], rr[5], rr[6], rr[7]]);
        let len = u16::from_be_bytes([rr[8], rr[9]]) as usize;
        let data = msg.get(at + 10..at + 10 + len).ok_or("truncated record")?;
        match (rtype, len) {
            (TYPE_A, 4) => {
                ips.push(IpAddr::from([data[0], data[1], data[2], data[3]]));
                ttl = ttl.min(rttl);
            }
            (TYPE_AAAA, 16) => {
                let mut b = [0u8; 16];
                b.copy_from_slice(data);
                ips.push(IpAddr::from(b));
                ttl = ttl.min(rttl);
            }
            _ => {}
        }
        at += 10 + len;
    }
    Ok((ips, Duration::from_secs(ttl as u64)))
}

/// reqwest's resolver for clients pinned to a device. Installed on every such
/// client whether or not the device is a tunnel today: the registry is read at
/// each lookup, so a client cached before the tunnel came up still asks the
/// tunnel's server once it has one.
pub struct Resolver {
    device: String,
}

impl Resolver {
    pub fn new(device: &str) -> Arc<Self> {
        Arc::new(Resolver { device: device.trim().to_string() })
    }
}

impl reqwest::dns::Resolve for Resolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let device = self.device.clone();
        let host = name.as_str().to_string();
        Box::pin(async move {
            let ips = resolve(&device, &host).await.map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
            let addrs: reqwest::dns::Addrs = Box::new(ips.into_iter().map(|ip| SocketAddr::new(ip, 0)));
            Ok(addrs)
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A DNS server on loopback that answers every A query with `answer` and
    /// counts what it was asked.
    pub(crate) async fn fake_server(answer: [u8; 4]) -> (SocketAddr, Arc<std::sync::Mutex<Vec<String>>>) {
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = sock.local_addr().unwrap();
        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = asked.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            loop {
                let Ok((n, from)) = sock.recv_from(&mut buf).await else { return };
                let q = &buf[..n];
                let end = skip_name(q, 12).unwrap();
                let qtype = u16::from_be_bytes([q[end], q[end + 1]]);
                let mut name = Vec::new();
                let mut at = 12;
                while q[at] != 0 {
                    let l = q[at] as usize;
                    name.push(String::from_utf8_lossy(&q[at + 1..at + 1 + l]).into_owned());
                    at += 1 + l;
                }
                seen.lock().unwrap().push(name.join("."));
                let mut r = q[..end + 4].to_vec();
                r[2] = 0x81;
                r[3] = 0x80;
                if qtype == TYPE_A {
                    r[7] = 1;
                    r.extend_from_slice(&[0xC0, 12, 0, 1, 0, 1, 0, 0, 0, 30, 0, 4]);
                    r.extend_from_slice(&answer);
                }
                let _ = sock.send_to(&r, from).await;
            }
        });
        (SocketAddr::new(addr.ip(), addr.port()), asked)
    }

    /// The server is registered with its real port: `set_servers` takes the
    /// `.conf`'s addresses and assumes 53, the test needs another one.
    pub(crate) fn register(device: &str, server: SocketAddr) {
        cache().lock().unwrap().retain(|(d, _), _| d != device);
        let entry = Arc::new(Entry { servers: vec![server], leak_logged: AtomicBool::new(false) });
        registry().write().unwrap().insert(device.to_string(), entry);
    }

    /// The tests that register `lo` take turns: the registry is keyed
    /// by device and they would otherwise overwrite each other's server.
    pub(crate) static LO: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn a_tunnel_name_is_asked_of_the_tunnel_server_and_never_of_the_host() {
        let _one = LO.lock().await;
        let (server, asked) = fake_server([192, 0, 2, 7]).await;
        // `lo` stands in for wg-<engine>: the query socket is pinned to it
        // exactly as it would be to the tunnel.
        register("lo", server);
        // A name the host resolver cannot know: answered at all, it was
        // answered by the tunnel server.
        let name = "tracker.tunnel-only.invalid";
        let ips = resolve("lo", name).await.expect("the tunnel server answers");
        assert_eq!(ips, vec![IpAddr::from([192, 0, 2, 7])]);
        assert!(asked.lock().unwrap().iter().any(|n| n == name), "the tunnel server was asked");
        assert!(
            !host_lookups().lock().unwrap().iter().any(|n| n == name),
            "the host resolver must never see a tunnel engine's tracker name"
        );
        // Cached: a second lookup does not ask again.
        let before = asked.lock().unwrap().len();
        resolve("lo", name).await.unwrap();
        assert_eq!(asked.lock().unwrap().len(), before, "a short cache absorbs repeated announces");
        forget("lo");
    }

    #[tokio::test]
    async fn a_silent_tunnel_server_fails_the_lookup_rather_than_asking_the_host() {
        let _one = LO.lock().await;
        // A bound socket that never answers.
        let mute = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        register("lo", mute.local_addr().unwrap());
        let name = "silent.tunnel-only.invalid";
        let err = resolve("lo", name).await.expect_err("no answer is a failed lookup");
        assert!(err.contains("did not answer"), "{err}");
        assert!(
            !host_lookups().lock().unwrap().iter().any(|n| n == name),
            "a tunnel server that does not answer must not hand the name to the host"
        );
        forget("lo");
    }

    #[tokio::test]
    async fn a_tunnel_without_dns_falls_back_to_the_host_and_an_unknown_device_too() {
        set_servers("wg-nodns", &[]);
        assert_eq!(servers("wg-nodns"), Some(vec![]));
        let _ = resolve("wg-nodns", "nodns.example.invalid").await;
        assert!(host_lookups().lock().unwrap().iter().any(|n| n == "nodns.example.invalid"));
        forget("wg-nodns");
        assert_eq!(servers("wg-nodns"), None);
        assert!(!is_tunnel("wg-nodns"));
        // Literals never reach any resolver.
        assert_eq!(resolve("wg-nodns", "[2001:db8::1]").await.unwrap(), vec!["2001:db8::1".parse::<IpAddr>().unwrap()]);
    }

    #[test]
    fn answers_are_decoded_through_compression_and_cname_chains() {
        // Question tracker.example, answer: CNAME then A via a pointer.
        let mut m = vec![0x12, 0x34, 0x81, 0x80, 0, 1, 0, 2, 0, 0, 0, 0];
        m.extend_from_slice(b"\x07tracker\x07example\x00\x00\x01\x00\x01");
        m.extend_from_slice(&[0xC0, 12, 0, 5, 0, 1, 0, 0, 1, 0, 0, 2, 0xC0, 20]);
        m.extend_from_slice(&[0xC0, 20, 0, 1, 0, 1, 0, 0, 0, 9, 0, 4, 198, 51, 100, 4]);
        let (ips, ttl) = decode_answer(&m).unwrap();
        assert_eq!(ips, vec![IpAddr::from([198, 51, 100, 4])]);
        assert_eq!(ttl, Duration::from_secs(9));
        // NXDOMAIN is an error, not an empty success.
        let mut nx = m.clone();
        nx[3] = 0x83;
        assert!(decode_answer(&nx).is_err());
        assert!(encode_query(1, "a..b", TYPE_A).is_err());
    }
}
