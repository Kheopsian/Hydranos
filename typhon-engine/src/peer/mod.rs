pub mod bencode;
pub mod extension;
pub mod choking;
pub mod download;
pub mod handshake;
pub mod message;
pub mod metadata;
pub mod pex;
pub mod session;
pub mod transport;
pub mod proxy_protocol;

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

/// Incoming TCP/PROXY-v2 connections rejected because the source IP matches
/// TYPHON_SELF_IPS (own VPS / styx netns / tunnel egress). Exposed for
/// diagnostics via the Hydra /api/hoard/stats endpoint.
pub static INCOMING_REJECTED_SELF: AtomicU64 = AtomicU64::new(0);

static EXTRA_TRUSTED_PROXY_SOURCES: OnceLock<Vec<std::net::IpAddr>> = OnceLock::new();

/// Register additional IPs whose PROXY v2 headers are trusted.
/// FW must guarantee that only these sources can reach the PROXY v2 port.
pub fn set_trusted_proxy_sources(ips: Vec<std::net::IpAddr>) {
    let _ = EXTRA_TRUSTED_PROXY_SOURCES.set(ips);
}

/// (host, port, optional (user, pass)) for outbound SOCKS5 on v6 dials.
pub type Socks5Config = (String, u16, Option<(String, String)>);
pub static SOCKS5_OUTBOUND: OnceLock<Socks5Config> = OnceLock::new();

pub fn set_socks5_outbound(host: String, port: u16, user: String, pass: String) {
    if host.is_empty() {
        return;
    }
    let auth = if !user.is_empty() { Some((user, pass)) } else { None };
    let _ = SOCKS5_OUTBOUND.set((host, port, auth));
}

/// Runtime listen-port rebind signal. The RPC `set_listen_port` sends the new
/// port here; the supervisor in `listen()` rebinds the TCP accept socket(s)
/// without restarting the engine (torrents + live peer connections untouched).
pub static REBIND_TX: OnceLock<tokio::sync::watch::Sender<u16>> = OnceLock::new();

/// Request a hot rebind of the TCP peer listener to `port`. Returns false when
/// the listener supervisor is not up yet. uTP (if enabled) keeps its port.
pub fn request_listen_rebind(port: u16) -> bool {
    match REBIND_TX.get() {
        Some(tx) => tx.send(port).is_ok(),
        None => false,
    }
}
use std::time::Duration;
use tokio::net::{TcpListener, TcpSocket};
use librqbit_utp::UtpSocketUdp;
use tracing::{info, warn};

use crate::disk::DiskManager;
use crate::torrent::TorrentManager;
use crate::peer::transport::PeerTransport;

/// Listen for incoming peer connections on TCP (one listener per binding,
/// each with its own peer_id used in the BT handshake) and uTP (one shared
/// UDP socket since uTP/UDP cannot share a port across multiple bound IPs
/// in a useful way for our case — uTP keeps the legacy single peer_id).
///
/// `bindings` is the resolved list (see config::resolved_bindings()). Empty
/// is treated as a startup error: we never want to silently come up with
/// no listener.
pub async fn listen(
    bindings: Vec<crate::config::ResolvedBinding>,
    default_port: u16,
    torrent_mgr: Arc<TorrentManager>,
    disk_mgr: Arc<DiskManager>,
    utp_socket: Option<Arc<UtpSocketUdp>>,
) -> Result<(), Box<dyn std::error::Error>> {
    if bindings.is_empty() {
        return Err("no bindings to listen on (config error)".into());
    }

    // Hot-rebind channel: `set_listen_port` RPC pushes a new port here and the
    // supervisor loop below re-binds the TCP accept socket(s) without dropping
    // torrents or live peer connections. Seeded with the current port so the
    // first `.changed()` only fires on a real request.
    let (tx, mut rx) = tokio::sync::watch::channel(bindings[0].addr.port());
    let _ = REBIND_TX.set(tx);

    // uTP shares one UDP socket bound at startup (main.rs). It is NOT rebound
    // on a hot port change — raw UDP dial+listen share the socket, and TCP is
    // the path that matters for gluetun / NAT-PMP port rotation.
    if let Some(sock) = utp_socket.clone() {
        let utp_peer_id = bindings[0].peer_id;
        let utp_advertised_port = if bindings[0].advertised_port != 0 {
            bindings[0].advertised_port
        } else {
            default_port
        };
        info!("[peer] uTP listening on {} (peer_id from binding[0], advertised_port={})",
            sock.bind_addr(), utp_advertised_port);
        let tm = torrent_mgr.clone();
        let dm = disk_mgr.clone();
        let u = utp_socket.clone();
        tokio::spawn(async move {
            utp_accept_loop(sock, tm, dm, utp_peer_id, u, utp_advertised_port).await;
        });
    }

    // `cur` is the live binding set; on a rebind we set addr.port + the BEP-10
    // advertised_port to the new value (single-binding direct/gluetun case).
    let mut cur = bindings.clone();
    loop {
        let mut handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();
        for b in &cur {
            // TcpSocket to set SO_REUSEADDR + a generous backlog. Default
            // TcpListener::bind() uses backlog=128 which drops SYNs under load.
            let socket = if b.addr.is_ipv4() {
                TcpSocket::new_v4()?
            } else {
                TcpSocket::new_v6()?
            };
            // IPV6_V6ONLY for the `enable_ipv6` listener: it sits beside the v4
            // one, so it must not also swallow v4. A dual-stack wildcard would
            // hand us v4 peers as `::ffff:a.b.c.d` and every address compared
            // downstream (dedup, allowlists, stats) would stop matching. Must
            // be set before bind(). Explicitly configured bindings are left
            // alone, their behaviour does not change.
            //
            // Unix only: Linux decides this from net.ipv6.bindv6only, which is
            // 0 (dual-stack) on every mainstream distro. Windows already
            // defaults the option on, so there is nothing to set there.
            #[cfg(unix)]
            if b.only_v6 {
                use std::os::fd::AsRawFd;
                let on: libc::c_int = 1;
                let rc = unsafe {
                    libc::setsockopt(
                        socket.as_raw_fd(),
                        libc::IPPROTO_IPV6,
                        libc::IPV6_V6ONLY,
                        &on as *const _ as *const libc::c_void,
                        std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                    )
                };
                if rc != 0 {
                    // Refuse to bind rather than quietly take over v4 too.
                    warn!(
                        "[peer] IPV6_V6ONLY failed on {} ({}), skipping the IPv6 listener",
                        b.addr,
                        std::io::Error::last_os_error()
                    );
                    continue;
                }
            }
            socket.set_reuseaddr(true)?;
            socket.bind(b.addr)?;
            let listener = socket.listen(4096)?;
            info!(
                "[peer] TCP listening on {} (binding id={}, peer_id_prefix={:?}, advertised_port={}, backlog=4096)",
                b.addr,
                b.id,
                std::str::from_utf8(&b.peer_id[..8]).unwrap_or("?"),
                b.advertised_port,
            );
            let tm = torrent_mgr.clone();
            let dm = disk_mgr.clone();
            let pid = b.peer_id;
            let u = utp_socket.clone();
            let advertised_port = b.advertised_port;
            handles.push(tokio::spawn(async move {
                tcp_accept_loop(listener, tm, dm, pid, u, advertised_port).await;
            }));
        }

        tokio::select! {
            changed = rx.changed() => {
                if changed.is_err() {
                    break; // sender dropped
                }
                let new_port = *rx.borrow();
                info!("[peer] hot rebind requested -> port {}", new_port);
                // Abort + await so each accept task drops its TcpListener and
                // frees the socket before we re-bind (SO_REUSEADDR makes an
                // overlap harmless, but awaiting is deterministic).
                for h in &handles {
                    h.abort();
                }
                for h in handles {
                    let _ = h.await;
                }
                for b in cur.iter_mut() {
                    b.addr.set_port(new_port);
                    b.advertised_port = new_port;
                }
                // loop -> re-bind on the new port
            }
            _ = tokio::signal::ctrl_c() => {
                for h in &handles {
                    h.abort();
                }
                break;
            }
        }
    }
    Ok(())
}

/// Thread-per-core for peer sessions, switchable at runtime.
///
/// A session pinned to one single-threaded runtime has its socket touched by
/// exactly one thread, so `lock_sock` is never contended — a perf profile of the
/// shared multi-threaded runtime showed `__pv_queued_spin_lock_slowpath` at 20%.
/// A bench measured contention falling from 15.7% to 3% and throughput rising
/// 6-9%, but that was a bench: whether it holds on a production swarm is exactly
/// what the flag exists to answer.
///
/// It is a hot flag rather than the start-up env var it began as, because
/// restarting to A/B it is not free: a restart resets the per-torrent upload
/// counters, and trackers credit upload by MAX per torrent, so every flip would
/// be paid for in credit. `TYPHON_SESSION_RUNTIMES` still sets the pool size.
///
/// Only NEW sessions follow the switch. Sessions already running stay on the
/// runtime that accepted them — moving a live socket between reactors is not
/// something a measurement knob should do — so a flip takes effect as peers
/// churn, over minutes rather than instantly. Blocks must be long enough to
/// outlast that.
static SESSION_PINNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static SESSION_RT_N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static SESSION_RTS: std::sync::OnceLock<Vec<tokio::runtime::Handle>> = std::sync::OnceLock::new();
static SESSION_RR: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Refuse MSE outright, in both directions, and drop the encrypted sessions
/// already running. An encrypted peer cannot use the zero-copy sendfile serve
/// path (`serve_zerocopy` requires plaintext), so it pays RC4 per byte plus a
/// heap copy on every write. This gates that cost so an A/B ladder can price
/// it against the upload we lose from peers that require encryption.
///
/// Off by default: refusing MSE turns away real peers, which is a trade to
/// measure, not a default to assume.
static BLOCK_MSE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Answer an inbound MSE handshake with `crypto_select = plaintext` whenever
/// the peer allows it. Outbound has dialed plaintext-first since v2.7.11 for
/// the same reason; the accepting side kept forcing RC4, which is where a
/// seedbox's bytes actually are -- most of our traffic runs on connections
/// somebody else opened, so we are the one choosing.
///
/// Costs no peer: "require encryption" clients do not offer plaintext, and
/// they keep getting RC4. Off by default all the same, see mse.rs.
static MSE_PREFER_PLAINTEXT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn session_pinning() -> bool {
    SESSION_PINNING.load(std::sync::atomic::Ordering::Relaxed)
}

/// Turn pinning on or off. The first `true` builds the runtime pool; later
/// flips only change which spawn path new sessions take.
pub fn set_session_pinning(on: bool) {
    SESSION_PINNING.store(on, std::sync::atomic::Ordering::Relaxed);
    if on {
        let n = session_runtimes().len();
        info!("[peer] session pinning ON over {} runtimes", n);
    } else {
        info!("[peer] session pinning OFF (new sessions on the shared runtime)");
    }
}

pub fn block_mse() -> bool {
    BLOCK_MSE.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn mse_prefer_plaintext() -> bool {
    MSE_PREFER_PLAINTEXT.load(std::sync::atomic::Ordering::Relaxed)
}

/// Flip the inbound plaintext preference. Only new handshakes see it: an
/// established RC4 session cannot drop its cipher mid-stream, so unlike the
/// MSE block this one cannot reach live sessions and an A/B needs to wait for
/// the connection pool to turn over.
pub fn set_mse_prefer_plaintext(on: bool) {
    MSE_PREFER_PLAINTEXT.store(on, std::sync::atomic::Ordering::Relaxed);
    if on {
        info!("[peer] inbound MSE will select plaintext when the peer offers it");
    } else {
        info!("[peer] inbound MSE back to selecting RC4");
    }
}

/// Flip the MSE block. Unlike session pinning this reaches live sessions too:
/// the encrypted peers are precisely the long-lived ones, so gating only new
/// handshakes would leave them running for hours and a measurement block would
/// never reach a clean state. Live sessions notice on their next loop turn.
pub fn set_block_mse(on: bool) {
    BLOCK_MSE.store(on, std::sync::atomic::Ordering::Relaxed);
    if on {
        info!("[peer] MSE BLOCKED (inbound refused, outbound fallback skipped, encrypted sessions draining)");
    } else {
        info!("[peer] MSE allowed again");
    }
}

/// Pool size for the next build. Refused once the pool exists: tearing down
/// runtimes that carry live sessions is not worth it, and silently ignoring the
/// request would make a measurement lie about what it measured.
pub fn set_session_runtimes(n: usize) -> bool {
    if SESSION_RTS.get().is_some() {
        return false;
    }
    SESSION_RT_N.store(n, std::sync::atomic::Ordering::Relaxed);
    true
}

/// How many runtimes the pool has, or would have. Explicit setting wins, then
/// TYPHON_SESSION_RUNTIMES, then one per core.
pub fn session_runtimes_n() -> usize {
    let n = SESSION_RT_N.load(std::sync::atomic::Ordering::Relaxed);
    if n > 0 {
        return n;
    }
    if let Some(n) = std::env::var("TYPHON_SESSION_RUNTIMES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
    {
        return n;
    }
    std::thread::available_parallelism().map(|v| v.get()).unwrap_or(1)
}

fn session_runtimes() -> &'static [tokio::runtime::Handle] {
    SESSION_RTS.get_or_init(|| {
        let n = session_runtimes_n();
        let mut handles = Vec::with_capacity(n);
        for i in 0..n {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(r) => r,
                Err(e) => {
                    warn!("[peer] session runtime {} failed: {}", i, e);
                    continue;
                }
            };
            handles.push(rt.handle().clone());
            let _ = std::thread::Builder::new()
                .name(format!("session-rt-{}", i))
                .spawn(move || {
                    rt.block_on(std::future::pending::<()>());
                });
        }
        info!("[peer] thread-per-core: {} dedicated session runtimes", handles.len());
        handles
    })
}

async fn tcp_accept_loop(
    listener: TcpListener,
    torrent_mgr: Arc<TorrentManager>,
    disk_mgr: Arc<DiskManager>,
    peer_id: [u8; 20],
    utp_socket: Option<Arc<UtpSocketUdp>>,
    listen_port: u16,
) {
    loop {
        let (stream, addr) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                warn!("[peer] tcp accept error: {}", e);
                continue;
            }
        };
        // is_self_ip filter removed: it blocked legitimate cross-engine
        // peers (race dialing hoard via public IP and vice-versa). The BT
        // handshake already rejects same-peer_id self-loops, and the dial
        // side filter (is_self_dial) keeps the port-aware outbound block.
        stream.set_nodelay(true).ok();
        let tm = torrent_mgr.clone();
        let dm = disk_mgr.clone();
        let u = utp_socket.clone();
        // OPT thread-per-core: pin the session to ONE runtime so its socket is only
        // ever touched by a single thread -> lock_sock is never contended (perf showed
        // __pv_queued_spin_lock_slowpath at 20% with the shared multi-thread runtime).
        // The tokio TcpStream is bound to the reactor that created it, so it must go
        // through into_std/from_std to move to another runtime.
        let rts: &[tokio::runtime::Handle] = if session_pinning() {
            session_runtimes()
        } else {
            &[]
        };
        if rts.is_empty() {
            tokio::spawn(async move {
                handle_incoming(PeerTransport::Tcp(stream), addr, tm, dm, peer_id, u, listen_port).await;
            });
        } else {
            let idx = SESSION_RR.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % rts.len();
            match stream.into_std() {
                Ok(std_s) => {
                    rts[idx].spawn(async move {
                        match tokio::net::TcpStream::from_std(std_s) {
                            Ok(s) => {
                                handle_incoming(PeerTransport::Tcp(s), addr, tm, dm, peer_id, u, listen_port).await;
                            }
                            Err(e) => warn!("[peer] from_std failed: {}", e),
                        }
                    });
                }
                Err(e) => warn!("[peer] into_std failed: {}", e),
            }
        }
    }
}

/// Listen for incoming peer connections wrapped in PROXY protocol v2.
/// Used behind an haproxy TCP frontend that prepends a PROXY v2 header carrying
/// the real peer IP (bypass path v6: peer -> VPS haproxy -> the router rdr v6 -> the seedbox host).
pub async fn listen_proxy_v2(
    bind_addr: String,
    port: u16,
    torrent_mgr: Arc<TorrentManager>,
    disk_mgr: Arc<DiskManager>,
    peer_id: [u8; 20],
    utp_socket: Option<Arc<UtpSocketUdp>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let addr_str = if bind_addr.is_empty() { "[::]".to_string() } else { bind_addr };
    let sockaddr: std::net::SocketAddr = format!("{}:{}", addr_str, port)
        .parse()
        .map_err(|e| format!("invalid proxy-v2 listen addr {}:{}: {}", addr_str, port, e))?;
    let socket = if sockaddr.is_ipv4() { TcpSocket::new_v4()? } else { TcpSocket::new_v6()? };
    socket.set_reuseaddr(true)?;
    socket.bind(sockaddr)?;
    let listener = socket.listen(4096)?;
    info!("[peer] PROXY v2 TCP listening on {} (backlog=4096)", sockaddr);

    loop {
        let (mut stream, wire_addr) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                warn!("[peer] proxy-v2 accept error: {}", e);
                continue;
            }
        };
        // Defense-in-depth : only trust PROXY v2 source from loopback or
        // Docker private network (where the seedbox host socat relay sits).
        // An attacker forging a PROXY v2 header could otherwise claim any
        // peer IP and bypass per-IP rate limits or pollute PeerStats.
        if !is_trusted_proxy_source(&wire_addr) {
            warn!("[peer] proxy-v2 reject untrusted src {}", wire_addr);
            continue;
        }
        let tm = torrent_mgr.clone();
        let dm = disk_mgr.clone();
        let u = utp_socket.clone();
        tokio::spawn(async move {
            match tokio::time::timeout(
                Duration::from_secs(5),
                proxy_protocol::parse_v2(&mut stream),
            )
            .await
            {
                Ok(Ok(real_addr)) => {
                    // Same rationale as the plain TCP listener: BT handshake
                    // catches real self-loops via peer_id, and the IP filter
                    // blocked cross-engine peers behind the same public IP.
                    stream.set_nodelay(true).ok();
                    handle_incoming(PeerTransport::Tcp(stream), real_addr, tm, dm, peer_id, u, port).await;
                }
                Ok(Err(e)) => warn!("[peer] proxy-v2 parse err from {}: {}", wire_addr, e),
                Err(_) => warn!("[peer] proxy-v2 read timeout from {}", wire_addr),
            }
        });
    }
}

/// Accept only local-trust sources for the PROXY v2 header : loopback, RFC1918
/// v4, ULA v6 (fd00::/8), IPv4-mapped-IPv6 of these. Anything else (including
/// public IPs or peer-space LAN) is rejected since the PROXY v2 header carries
/// an attacker-chosen peer IP.
fn is_trusted_proxy_source(addr: &SocketAddr) -> bool {
    use std::net::{IpAddr, Ipv4Addr};
    // Config-driven allowlist (e.g. VPS haproxy public v6). FW restricts source.
    if let Some(extras) = EXTRA_TRUSTED_PROXY_SOURCES.get() {
        if extras.iter().any(|ip| *ip == addr.ip()) {
            return true;
        }
    }
    let v4 = match addr.ip() {
        IpAddr::V4(v) => v,
        IpAddr::V6(v6) => {
            let segs = v6.segments();
            // ::1 loopback or fc00::/7 ULA is trusted directly
            if v6.is_loopback() || (segs[0] & 0xfe00) == 0xfc00 {
                return true;
            }
            // Unwrap IPv4-mapped ::ffff:a.b.c.d
            if segs[0..5] == [0; 5] && segs[5] == 0xffff {
                Ipv4Addr::new(
                    (segs[6] >> 8) as u8,
                    (segs[6] & 0xff) as u8,
                    (segs[7] >> 8) as u8,
                    (segs[7] & 0xff) as u8,
                )
            } else {
                return false;
            }
        }
    };
    if v4.is_loopback() { return true; }
    let o = v4.octets();
    // RFC1918 including Docker default bridge 172.17.0.0/16
    o[0] == 10
        || (o[0] == 172 && (16..=31).contains(&o[1]))
        || (o[0] == 192 && o[1] == 168)
}

async fn utp_accept_loop(
    socket: Arc<UtpSocketUdp>,
    torrent_mgr: Arc<TorrentManager>,
    disk_mgr: Arc<DiskManager>,
    peer_id: [u8; 20],
    utp_socket: Option<Arc<UtpSocketUdp>>,
    listen_port: u16,
) {
    loop {
        let stream = match socket.accept().await {
            Ok(s) => s,
            Err(e) => {
                warn!("[peer] utp accept error: {}", e);
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let addr = stream.remote_addr();
        let tm = torrent_mgr.clone();
        let dm = disk_mgr.clone();
        let u = utp_socket.clone();
        tokio::spawn(async move {
            handle_incoming(PeerTransport::Utp(stream), addr, tm, dm, peer_id, u, listen_port).await;
        });
    }
}

/// Peers that opened a connection to us, excluding our own addresses.
///
/// This is the only proof of reachability that costs nothing and cannot be
/// faked: a probe we send ourselves turns around at our own router or VPN
/// provider and proves nothing either way, while a stranger arriving here has,
/// by definition, got through. Self-addresses are excluded so our own
/// reachability probe cannot validate itself.
pub static INBOUND_ACCEPTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

async fn handle_incoming(
    mut stream: PeerTransport,
    addr: SocketAddr,
    torrent_mgr: Arc<TorrentManager>,
    disk_mgr: Arc<DiskManager>,
    peer_id: [u8; 20],
    utp_socket: Option<Arc<UtpSocketUdp>>,
    listen_port: u16,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_util::codec::Framed;
    use crate::wire::codec::BtCodec;
    use crate::crypto::stream::CryptoStream;

    info!("[peer] incoming {} from {}", stream.kind(), addr);
    if !crate::tracker::is_self_ip(addr.ip()) {
        INBOUND_ACCEPTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    // Read first byte to detect MSE vs plaintext
    let mut first = [0u8; 1];
    if stream.read_exact(&mut first).await.is_err() { return; }

    let (crypto_stream, torrent, fast_ext, lt_ext, remote_peer_id, is_encrypted) = if first[0] == 19 {
        // Plaintext BT handshake — read remaining 67 bytes
        let mut rest = [0u8; 67];
        if stream.read_exact(&mut rest).await.is_err() { return; }
        if &rest[0..19] != b"BitTorrent protocol" { return; }

        let mut info_hash = [0u8; 20];
        info_hash.copy_from_slice(&rest[27..47]);
        let mut remote_pid = [0u8; 20];
        remote_pid.copy_from_slice(&rest[47..67]);
        let fast_ext = (rest[26] & 0x04) != 0;
        let ext_proto = (rest[24] & 0x10) != 0; // reserved[5] in the full 68-byte HS = rest[24]

        let torrent = match torrent_mgr.get(&info_hash) {
            Some(t) => t,
            None => return,
        };

        // Send our handshake
        let mut reply = Vec::with_capacity(68);
        reply.push(19u8);
        reply.extend_from_slice(b"BitTorrent protocol");
        let mut res = [0u8; 8];
        res[7] |= 0x04; // BEP 6
        res[5] |= 0x10; // BEP 10
        reply.extend_from_slice(&res);
        reply.extend_from_slice(&info_hash);
        reply.extend_from_slice(&peer_id);
        if stream.write_all(&reply).await.is_err() { return; }

        (CryptoStream::plain(stream), torrent, fast_ext, ext_proto, remote_pid, false)
    } else {
        // MSE handshake — first byte is part of DH public key
        if block_mse() {
            crate::tracker::MSE_INBOUND_REFUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return;
        }
        let mut ya_rest = [0u8; 95];
        if stream.read_exact(&mut ya_rest).await.is_err() {
            warn!("[peer] {} MSE: failed to read Ya", addr);
            return;
        }

        let tm_clone = torrent_mgr.clone();
        match crate::crypto::mse::handshake_incoming(
            &mut stream,
            first[0],
            &ya_rest,
            &peer_id,
            |req2_hash| tm_clone.lookup_skey(req2_hash),
        ).await {
            Ok((enc, dec, hs)) => {
                let torrent = match torrent_mgr.get(&hs.info_hash) {
                    Some(t) => t,
                    None => return,
                };
                // Only wrap the transport when RC4 was actually selected. A
                // plaintext-selected session reports is_encrypted = false on
                // purpose: it is plaintext on the wire, so it belongs on the
                // zero-copy serve path and has nothing for the MSE block to
                // drain.
                let (enc, dec) = if hs.rc4 { (Some(enc), Some(dec)) } else { (None, None) };
                (CryptoStream::new(stream, enc, dec), torrent, hs.fast_extension, hs.extended_protocol, hs.peer_id, hs.rc4)
            }
            Err(e) => {
                warn!("[peer] {} MSE handshake failed: {}", addr, e);
                return;
            }
        }
    };

    let framed = Framed::new(crypto_stream, BtCodec::new());
    session::run(
        framed,
        addr,
        torrent,
        disk_mgr,
        peer_id,
        remote_peer_id,
        is_encrypted,
        fast_ext,
        lt_ext,
        utp_socket,
        listen_port,
    )
    .await;
}
