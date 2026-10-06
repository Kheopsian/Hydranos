pub mod bencode;
pub mod extension;
pub mod choking;
pub mod download;
pub mod handshake;
pub mod holepunch;
pub mod message;
pub mod metadata;
pub mod peerclient;
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

// The trusted PROXY v2 sources live on the TorrentManager: the proxy-v2
// listener is per engine, so its allowlist is too.

/// (host, port, optional (user, pass)) for outbound SOCKS5 on v6 dials.
pub type Socks5Config = (String, u16, Option<(String, String)>);
// The configured proxy travels inside each binding's Egress (see
// `Config::socks5_outbound`), not in a global: it decides which address a peer
// sees, and one process can carry two engines sent out different ways.

// Runtime listen-port rebind: `TorrentManager::rebind_listener` sends the new
// port; the supervisor in `listen()` moves the TCP accept socket(s) and the
// uTP socket without restarting the engine (torrents and live TCP peers
// untouched), and keeps the old port when the new one cannot be bound.
// The rebind channel lives on the TorrentManager: one listener per engine, so
// one channel per engine. See `TorrentManager::request_listen_rebind`.
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
/// `listening` is raised once a round of binds has actually succeeded, and is
/// the only honest answer to "is this engine reachable".
///
/// Every early return below happens BEFORE it is raised, which is the point:
/// the caller logs "session started" and "on the network, announcing" from a
/// spawned task's point of view, so those lines were printed before the bind
/// was even attempted and a failure arrived a fraction of a millisecond later,
/// contradicting them. The flag is what `/api/engines` publishes instead.
pub async fn listen(
    bindings: Vec<crate::config::ResolvedBinding>,
    default_port: u16,
    torrent_mgr: Arc<TorrentManager>,
    disk_mgr: Arc<DiskManager>,
    utp: UtpHandle,
    listening: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<(), Box<dyn std::error::Error>> {
    if bindings.is_empty() {
        return Err("no bindings to listen on (config error)".into());
    }
    // Refused before any socket: outside Linux a pinned listener would be an
    // unpinned one (`netpin::DEVICE_PIN_SUPPORTED`).
    if !crate::netpin::DEVICE_PIN_SUPPORTED && bindings.iter().any(|b| b.egress.device().is_some()) {
        return Err(crate::netpin::UNSUPPORTED.into());
    }

    // Hot-rebind requests: the API, gluetun and the tunnel's port follower
    // push a new port here, and the loop below moves the TCP accept socket(s)
    // and the uTP socket without dropping torrents or live TCP peers.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Rebind>();
    torrent_mgr.set_rebind_tx(tx);

    // `cur` is the live binding set; a rebind sets addr.port and the BEP-10
    // advertised_port to the new value (single-binding direct/gluetun case).
    let mut cur = bindings.clone();
    // The first round is the startup bind: a failure there is the engine's
    // listener failing, and the caller lowers `listening` and says so.
    let first = bind_round(&cur).map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    let mut handles = spawn_tcp(first, &torrent_mgr, &disk_mgr, &utp);
    let mut utp_task = spawn_utp(&utp, &cur, default_port, &torrent_mgr, &disk_mgr);
    // The port actually held, from the first bind on, so whoever asks
    // (announces, the API, the port mapper) reads the listener and not a
    // config value that may say something else.
    if cur[0].addr.port() != 0 {
        torrent_mgr.set_live_port(cur[0].addr.port());
    }
    // Every socket of this round is bound and accepting. Raised here and
    // not before, so the flag never claims a port the engine does not hold.
    listening.store(!handles.is_empty(), std::sync::atomic::Ordering::Relaxed);

    loop {
        tokio::select! {
            req = rx.recv() => {
                let Some(Rebind { port, reply }) = req else {
                    break; // every sender dropped
                };
                let outcome = rebind(port, &mut cur, &mut handles, &mut utp_task, &utp, &torrent_mgr, &disk_mgr).await;
                match &outcome {
                    Ok(p) => {
                        torrent_mgr.set_live_port(*p);
                        listening.store(!handles.is_empty(), std::sync::atomic::Ordering::Relaxed);
                    }
                    Err(e) => warn!("[peer] rebind to port {} refused: {}", port, e),
                }
                if let Some(reply) = reply {
                    let _ = reply.send(outcome);
                }
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

/// One request to move an engine's listeners to another port. `reply`, when
/// present, receives the port now listened on, or why the old one was kept.
pub struct Rebind {
    pub port: u16,
    pub reply: Option<tokio::sync::oneshot::Sender<Result<u16, String>>>,
}

/// Move every listener to `port`, or leave every one where it was.
///
/// The new sockets are bound BEFORE the old ones are let go. 4.3 aborted the
/// accept tasks first and bound afterwards, so a port already in use killed
/// the engine's listener for good: no inbound peer until a restart, behind a
/// route that had already answered 200.
async fn rebind(
    port: u16,
    cur: &mut Vec<crate::config::ResolvedBinding>,
    handles: &mut Vec<tokio::task::JoinHandle<()>>,
    utp_task: &mut Option<tokio::task::JoinHandle<()>>,
    utp: &UtpHandle,
    torrent_mgr: &Arc<TorrentManager>,
    disk_mgr: &Arc<DiskManager>,
) -> Result<u16, String> {
    if port == 0 {
        return Err("port 0 is not a port".into());
    }
    let was = cur[0].addr.port();
    if port == was {
        return Ok(port);
    }
    info!("[peer] hot rebind requested -> port {}", port);
    let mut next = cur.clone();
    for b in next.iter_mut() {
        b.addr.set_port(port);
        b.advertised_port = port;
    }
    let listeners = bind_round(&next).map_err(|e| format!("{e}; still listening on {was}"))?;
    let fresh_utp = utp
        .bind_next(port)
        .await
        .map_err(|e| format!("{e}; still listening on {was}"))?;
    // Both bound: only now does the old port go.
    for h in handles.iter() {
        h.abort();
    }
    for h in handles.drain(..) {
        let _ = h.await;
    }
    if let Some(t) = utp_task.take() {
        t.abort();
    }
    if let Some(live) = fresh_utp {
        utp.install(live);
    }
    *cur = next;
    *handles = spawn_tcp(listeners, torrent_mgr, disk_mgr, utp);
    *utp_task = spawn_utp(utp, cur, port, torrent_mgr, disk_mgr);
    Ok(port)
}

/// Bind and listen on every binding of one round; nothing is spawned.
/// Any socket that cannot be bound fails the whole round, so a rebind is
/// all-or-nothing.
fn bind_round(
    cur: &[crate::config::ResolvedBinding],
) -> Result<Vec<(TcpListener, crate::config::ResolvedBinding)>, String> {
    let mut out = Vec::new();
    for b in cur {
        // TcpSocket to set SO_REUSEADDR + a generous backlog. Default
        // TcpListener::bind() uses backlog=128 which drops SYNs under load.
        let socket = if b.addr.is_ipv4() {
            TcpSocket::new_v4()
        } else {
            TcpSocket::new_v6()
        }
        .map_err(|e| format!("socket for {}: {e}", b.addr))?;
        // Pin the LISTENER, not just the dials: a socket accepted on it
        // inherits the device, so the reply to a peer that arrived on the
        // second tunnel leaves by that tunnel too. Without it the reply
        // follows the default route, reaches the peer from an address it
        // never dialled, and the connection dies silently.
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            if let Err(e) = crate::netpin::pin_fd(socket.as_raw_fd(), &b.egress) {
                return Err(format!(
                    "cannot pin the peer listener to bind_device: {} — refusing to listen on the default route",
                    e
                ));
            }
        }
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
        socket.set_reuseaddr(true).map_err(|e| format!("{}: {e}", b.addr))?;
        socket.bind(b.addr).map_err(|e| format!("cannot bind {}: {e}", b.addr))?;
        let listener = socket.listen(4096).map_err(|e| format!("cannot listen on {}: {e}", b.addr))?;
        out.push((listener, b.clone()));
    }
    Ok(out)
}

fn spawn_tcp(
    listeners: Vec<(TcpListener, crate::config::ResolvedBinding)>,
    torrent_mgr: &Arc<TorrentManager>,
    disk_mgr: &Arc<DiskManager>,
    utp: &UtpHandle,
) -> Vec<tokio::task::JoinHandle<()>> {
    listeners
        .into_iter()
        .map(|(listener, b)| {
            info!(
                "[peer] TCP listening on {} (binding id={}, peer_id_prefix={:?}, advertised_port={}, backlog=4096)",
                b.addr,
                b.id,
                std::str::from_utf8(&b.peer_id[..8]).unwrap_or("?"),
                b.advertised_port,
            );
            let tm = torrent_mgr.clone();
            let dm = disk_mgr.clone();
            let u = utp.get();
            tokio::spawn(async move {
                tcp_accept_loop(listener, tm, dm, b.peer_id, u, b.advertised_port).await;
            })
        })
        .collect()
}

/// The uTP accept loop on whatever socket the handle holds now.
fn spawn_utp(
    utp: &UtpHandle,
    cur: &[crate::config::ResolvedBinding],
    default_port: u16,
    torrent_mgr: &Arc<TorrentManager>,
    disk_mgr: &Arc<DiskManager>,
) -> Option<tokio::task::JoinHandle<()>> {
    let sock = utp.get()?;
    let utp_peer_id = cur[0].peer_id;
    let utp_advertised_port = if cur[0].advertised_port != 0 {
        cur[0].advertised_port
    } else {
        default_port
    };
    info!("[peer] uTP listening on {} (peer_id from binding[0], advertised_port={})",
        sock.bind_addr(), utp_advertised_port);
    let tm = torrent_mgr.clone();
    let dm = disk_mgr.clone();
    let u = Some(sock.clone());
    Some(tokio::spawn(async move {
        utp_accept_loop(sock, tm, dm, utp_peer_id, u, utp_advertised_port).await;
    }))
}

/// This engine's uTP socket, which a rebind replaces.
///
/// One UDP socket carries both directions -- the accept loop and every
/// outbound uTP dial -- so moving the port means a new socket that both
/// sides pick up. Dials read it per dial (`get`), so the next one after a
/// rebind leaves from the new port.
///
/// The old socket is cancelled, not just dropped: librqbit-utp's dispatcher
/// task holds its own reference and runs until its cancellation token fires,
/// so dropping our handles would have kept the old UDP port bound and
/// answering forever. Cancelling it ends the uTP connections that were on
/// it; those peers come back on the new port (TCP peers are kept).
#[derive(Clone, Default)]
pub struct UtpHandle {
    inner: Arc<std::sync::RwLock<Option<UtpLive>>>,
    /// The device the socket is pinned to; a replacement gets the same pin.
    device: Option<Arc<str>>,
}

struct UtpLive {
    sock: Arc<UtpSocketUdp>,
    cancel: tokio_util::sync::CancellationToken,
}

impl UtpHandle {
    /// No uTP at all (TYPHON_DISABLE_UTP, or a socket that could not be
    /// opened): a rebind then moves TCP only, as before.
    pub fn off() -> Self {
        Self::default()
    }

    /// Open the engine's uTP socket on `port`, pinned to `device`.
    pub async fn bind(port: u16, device: Option<&str>) -> Result<Self, String> {
        let live = bind_utp(port, device).await?;
        Ok(Self {
            inner: Arc::new(std::sync::RwLock::new(Some(live))),
            device: device.map(Arc::from),
        })
    }

    /// The socket in use now.
    pub fn get(&self) -> Option<Arc<UtpSocketUdp>> {
        self.inner
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .map(|l| l.sock.clone())
    }

    /// A socket on `port` for a rebind, not installed yet. None when this
    /// engine runs no uTP.
    async fn bind_next(&self, port: u16) -> Result<Option<UtpLive>, String> {
        if self.get().is_none() {
            return Ok(None);
        }
        bind_utp(port, self.device.as_deref()).await.map(Some)
    }

    /// Put `live` in place and shut the old socket down.
    fn install(&self, live: UtpLive) {
        let old = self.inner.write().unwrap_or_else(|p| p.into_inner()).replace(live);
        if let Some(old) = old {
            old.cancel.cancel();
        }
    }
}

async fn bind_utp(port: u16, device: Option<&str>) -> Result<UtpLive, String> {
    let bind = SocketAddr::from(([0, 0, 0, 0], port));
    // max_live_vsocks default is 128 which saturates immediately on a seedbox
    // with thousands of peers — new uTP dials get rejected with
    // TooManyActiveConnections. Bumped to 4096 (2026-04-17 investigation: 70%
    // of uTP fails were "error"=saturated).
    let mut opts = librqbit_utp::SocketOpts::default();
    opts.max_live_vsocks = std::num::NonZeroUsize::new(4096);
    let cancel = opts.cancellation_token.clone();
    // uTP is raw UDP and gets the same device pin as everything else.
    // Without it the tunnel steering would hold for TCP and leak for uTP,
    // which is the shape of leak nobody notices: it is the same swarm.
    let dev = device
        .map(|d| d.parse::<librqbit_utp::BindDevice>())
        .transpose()
        .map_err(|e| format!("bind_device is not usable for the uTP socket: {e}"))?;
    let udp_opts = librqbit_utp::UtpSocketUdpOpts { bind_device: dev.as_ref() };
    let sock = UtpSocketUdp::new_udp_with_opts(bind, opts, udp_opts)
        .await
        .map_err(|e| format!("cannot bind the uTP socket on {bind}: {e}"))?;
    Ok(UtpLive { sock, cancel })
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
///
/// `peer_id` and `egress` are the engine's first binding's, like the main
/// listener's: one engine, one identity, one way out.
pub async fn listen_proxy_v2(
    bind_addr: String,
    port: u16,
    torrent_mgr: Arc<TorrentManager>,
    disk_mgr: Arc<DiskManager>,
    peer_id: [u8; 20],
    egress: crate::netpin::Egress,
    utp_socket: Option<Arc<UtpSocketUdp>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let addr_str = if bind_addr.is_empty() { "[::]".to_string() } else { bind_addr };
    let sockaddr: std::net::SocketAddr = format!("{}:{}", addr_str, port)
        .parse()
        .map_err(|e| format!("invalid proxy-v2 listen addr {}:{}: {}", addr_str, port, e))?;
    let socket = if sockaddr.is_ipv4() { TcpSocket::new_v4()? } else { TcpSocket::new_v6()? };
    // Pinned like the main listener, for the same reason: an accepted socket
    // inherits the device, so the reply to the relay leaves by the tunnel the
    // relay reached us through, not by the default route.
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        if let Err(e) = crate::netpin::pin_fd(socket.as_raw_fd(), &egress) {
            return Err(format!(
                "cannot pin the PROXY v2 listener to bind_device: {} — refusing to listen on the default route",
                e
            )
            .into());
        }
    }
    #[cfg(not(unix))]
    let _ = &egress;
    socket.set_reuseaddr(true)?;
    socket.bind(sockaddr)?;
    let listener = socket.listen(4096)?;
    info!("[peer] PROXY v2 TCP listening on {} (backlog=4096)", sockaddr);

    loop {
        let (stream, wire_addr) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                warn!("[peer] proxy-v2 accept error: {}", e);
                continue;
            }
        };
        let tm = torrent_mgr.clone();
        let dm = disk_mgr.clone();
        let u = utp_socket.clone();
        tokio::spawn(async move {
            let extras = tm.trusted_proxy_sources().to_vec();
            match accept_proxied(stream, wire_addr, &extras).await {
                Ok((stream, real_addr)) => {
                    // Same rationale as the plain TCP listener: BT handshake
                    // catches real self-loops via peer_id, and the IP filter
                    // blocked cross-engine peers behind the same public IP.
                    stream.set_nodelay(true).ok();
                    handle_incoming(PeerTransport::Tcp(stream), real_addr, tm, dm, peer_id, u, port).await;
                }
                Err(e) => warn!("[peer] proxy-v2 {}", e),
            }
        });
    }
}

/// The trust check and the header read for one connection to the PROXY v2
/// listener: the peer address to hand to `handle_incoming`, or why not.
///
/// The source is checked BEFORE a byte is read. The header carries an address
/// the sender chose, so from anyone but our own relay it is an attacker
/// claiming to be any peer -- past the IP filter, into PeerStats, around a
/// per-IP limit. Apart from the listener so the decision can be tested
/// against a real socket without an engine behind it.
async fn accept_proxied(
    mut stream: tokio::net::TcpStream,
    wire_addr: SocketAddr,
    trusted: &[std::net::IpAddr],
) -> Result<(tokio::net::TcpStream, SocketAddr), String> {
    if !is_trusted_proxy_source(&wire_addr, trusted) {
        return Err(format!("reject untrusted src {}", wire_addr));
    }
    match tokio::time::timeout(Duration::from_secs(5), proxy_protocol::parse_v2(&mut stream)).await {
        Ok(Ok(real_addr)) => Ok((stream, real_addr)),
        Ok(Err(e)) => Err(format!("parse err from {}: {}", wire_addr, e)),
        Err(_) => Err(format!("read timeout from {}", wire_addr)),
    }
}

/// Accept only local-trust sources for the PROXY v2 header : loopback, RFC1918
/// v4, ULA v6 (fd00::/8), IPv4-mapped-IPv6 of these. Anything else (including
/// public IPs or peer-space LAN) is rejected since the PROXY v2 header carries
/// an attacker-chosen peer IP.
fn is_trusted_proxy_source(addr: &SocketAddr, extras: &[std::net::IpAddr]) -> bool {
    use std::net::{IpAddr, Ipv4Addr};
    // Config-driven allowlist (e.g. VPS haproxy public v6). FW restricts source.
    if extras.iter().any(|ip| *ip == addr.ip()) {
        return true;
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
/// Inbound connections dropped because their handshake never finished.
/// Exposed so the fd curve has a named cause instead of a mystery.
pub static HS_TIMED_OUT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Connections closed because both ends already hold every piece.
/// Two seeders have nothing to exchange; keeping the socket open costs a file
/// descriptor per shared swarm, and a big seedbox shares thousands with us.
pub static SEED_SEED_DROPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub static INBOUND_ACCEPTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Sessions whose have-broadcast arm was disarmed after the sender went away.
///
/// The torrent reached Seeding, `release_have_tx` dropped the sender, and the
/// session's `Receiver` started returning Closed on every poll. Each increment
/// is one session that would otherwise have spun until its idle deadline.
/// This is the measurement, not decoration: it is the only way to tell how
/// much of the engine's CPU that bug was worth on a live swarm.
pub static HAVE_RX_DISARMED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Piece announcements lost because the 256-slot have-ring overflowed.
/// Previously swallowed by `.ok()`, so an overflowing ring was invisible.
pub static HAVE_RX_LAGGED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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

    // Before anything costs: no handshake, no MSE, no log line per attempt
    // from a range somebody filtered precisely because it knocks a lot.
    if crate::ipfilter::blocked(addr.ip()) {
        crate::ipfilter::BLOCKED_IN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return;
    }
    info!("[peer] incoming {} from {}", stream.kind(), addr);
    if !crate::tracker::is_self_ip(addr.ip()) {
        INBOUND_ACCEPTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        torrent_mgr.note_inbound_peer();
    }

    // A peer that connects and then says nothing used to park a task in
    // read_exact forever: the socket stayed ESTABLISHED, no PeerGuard was ever
    // built, so `active_peers` never counted it and nothing ever reaped it.
    // Measured on prod: 20758 established sockets for 10874 peers.
    // Every read and write of the handshake is bounded from here on.
    const HS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

    // Read first byte to detect MSE vs plaintext
    let mut first = [0u8; 1];
    match tokio::time::timeout(HS_TIMEOUT, stream.read_exact(&mut first)).await {
        Ok(Ok(_)) => {}
        Ok(Err(_)) => return,
        Err(_) => {
            HS_TIMED_OUT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::debug!("[peer] {} sent nothing within {:?}, dropping", addr, HS_TIMEOUT);
            return;
        }
    }

    let (crypto_stream, torrent, fast_ext, lt_ext, remote_peer_id, is_encrypted) = if first[0] == 19 {
        // Plaintext BT handshake — read remaining 67 bytes
        let mut rest = [0u8; 67];
        match tokio::time::timeout(HS_TIMEOUT, stream.read_exact(&mut rest)).await {
            Ok(Ok(_)) => {}
            Ok(Err(_)) => return,
            Err(_) => { HS_TIMED_OUT.fetch_add(1, std::sync::atomic::Ordering::Relaxed); return; }
        }
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
        // The torrent's identity, not the binding's: on a private tracker the
        // announce may have been spoofed, and this is what its peers compare.
        reply.extend_from_slice(&peer_id);
        match tokio::time::timeout(HS_TIMEOUT, stream.write_all(&reply)).await {
            Ok(Ok(_)) => {}
            Ok(Err(_)) => return,
            Err(_) => { HS_TIMED_OUT.fetch_add(1, std::sync::atomic::Ordering::Relaxed); return; }
        }

        (CryptoStream::plain(stream), torrent, fast_ext, ext_proto, remote_pid, false)
    } else {
        // MSE handshake — first byte is part of DH public key
        if torrent_mgr.policy().block_mse() {
            crate::tracker::MSE_INBOUND_REFUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return;
        }
        let mut ya_rest = [0u8; 95];
        match tokio::time::timeout(HS_TIMEOUT, stream.read_exact(&mut ya_rest)).await {
            Ok(Ok(_)) => {}
            Ok(Err(_)) => {
                warn!("[peer] {} MSE: failed to read Ya", addr);
                return;
            }
            Err(_) => { HS_TIMED_OUT.fetch_add(1, std::sync::atomic::Ordering::Relaxed); return; }
        }

        let tm_clone = torrent_mgr.clone();
        let mse_res = tokio::time::timeout(
            HS_TIMEOUT,
            crate::crypto::mse::handshake_incoming(
                &mut stream,
                first[0],
                &ya_rest,
                &peer_id,
                // resolved per torrent below, same rule as the plaintext path
                |req2_hash| tm_clone.lookup_skey(req2_hash),
            ),
        ).await;
        let mse_res = match mse_res {
            Ok(r) => r,
            Err(_) => {
                HS_TIMED_OUT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return;
            }
        };
        match mse_res {
            Ok((enc, dec, hs)) => {
                let torrent = match torrent_mgr.get(&hs.info_hash) {
                    Some(t) => t,
                    None => return,
                };
                (CryptoStream::new(stream, Some(enc), Some(dec)), torrent, hs.fast_extension, hs.extended_protocol, hs.peer_id, true)
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

#[cfg(test)]
mod proxy_trust_tests {
    use super::is_trusted_proxy_source;
    use std::net::{IpAddr, SocketAddr};

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    /// ⭐ What this decides: whether to believe a PROXY v2 header, which
    /// carries the peer address the sender CLAIMS to be. Trust the wrong
    /// source and a stranger picks its own identity -- including one already in
    /// a swarm, or one that reads as ours.
    #[test]
    fn a_stranger_on_the_public_internet_is_never_trusted() {
        for a in [
            "93.184.216.34:16271",
            "45.33.32.156:16271",
            "172.15.0.1:16271", // leak-ok: just outside RFC1918, the boundary is the test
            "172.32.0.1:16271", // leak-ok: just outside RFC1918, the boundary is the test
            "[2606:2800:220:1:248:1893:25c8:1946]:16271",
        ] {
            assert!(
                !is_trusted_proxy_source(&addr(a), &[]),
                "{a} must not be able to declare its own address"
            );
        }
    }

    /// The private ranges the header may legitimately come from: a reverse
    /// proxy on the same host or the same network.
    #[test]
    fn the_local_and_private_sources_are_trusted() {
        for a in [
            "127.0.0.1:16271",
            "10.0.0.5:16271",
            "172.16.0.1:16271",
            "172.31.255.254:16271",
            "192.168.99.50:16271",
            "172.17.0.1:16271", // the Docker default bridge
            "[::1]:16271",
            "[fc00::1]:16271", // ULA
            "[fd00::1]:16271",
        ] {
            assert!(is_trusted_proxy_source(&addr(a), &[]), "{a} is local");
        }
    }

    /// The 172 range is 172.16 through 172.31 and nothing either side of it.
    /// One off at either end either locks out a legitimate proxy or hands the
    /// right to 172.32.0.0/11, which is public.  // leak-ok: prose about the range
    #[test]
    fn the_172_range_stops_exactly_where_rfc1918_does() {
        assert!(!is_trusted_proxy_source(&addr("172.15.255.255:1"), &[])); // leak-ok: RFC1918 boundary
        assert!(is_trusted_proxy_source(&addr("172.16.0.0:1"), &[]));
        assert!(is_trusted_proxy_source(&addr("172.31.255.255:1"), &[]));
        assert!(!is_trusted_proxy_source(&addr("172.32.0.0:1"), &[])); // leak-ok: RFC1918 boundary
    }

    /// A v4-mapped v6 address is the v4 address it wraps. Reading it as an
    /// opaque v6 would refuse a proxy on 10.0.0.1 reaching a dual-stack
    /// listener -- or, read the other way round, trust one that is public.
    #[test]
    fn a_v4_mapped_address_is_judged_as_the_v4_it_is() {
        assert!(is_trusted_proxy_source(&addr("[::ffff:10.0.0.1]:16271"), &[]));
        assert!(is_trusted_proxy_source(&addr("[::ffff:127.0.0.1]:16271"), &[]));
        assert!(
            !is_trusted_proxy_source(&addr("[::ffff:93.184.216.34]:16271"), &[]),
            "public is public, in either notation"
        );
    }

    /// The configured allowlist is how a proxy on a public address is trusted
    /// -- a VPS in front of the node -- and it has to match exactly.
    #[test]
    fn the_allowlist_trusts_exactly_what_it_names() {
        let named: IpAddr = "93.184.216.34".parse().unwrap();
        let extras = vec![named];

        assert!(is_trusted_proxy_source(&addr("93.184.216.34:16271"), &extras));
        assert!(
            is_trusted_proxy_source(&addr("93.184.216.34:9999"), &extras),
            "the port is not part of the identity"
        );
        assert!(
            !is_trusted_proxy_source(&addr("93.184.216.35:16271"), &extras),
            "the neighbouring address is a different machine"
        );
        assert!(
            !is_trusted_proxy_source(&addr("[2606:2800:220::1]:16271"), &extras),
            "naming a v4 address does not trust a v6 one"
        );
    }

    /// An empty allowlist is the default, and it must not read as "everyone".
    #[test]
    fn an_empty_allowlist_grants_nothing_extra() {
        assert!(!is_trusted_proxy_source(&addr("93.184.216.34:16271"), &[]));
    }
}

#[cfg(test)]
mod proxy_v2_accept_tests {
    use super::accept_proxied;
    use tokio::io::AsyncWriteExt;

    /// A PROXY v2 header for a TCP/IPv4 peer at `src`, as haproxy sends it.
    fn header(src: std::net::SocketAddrV4) -> Vec<u8> {
        let mut h = vec![0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A, 0x21, 0x11, 0, 12];
        h.extend_from_slice(&src.ip().octets());
        h.extend_from_slice(&[192, 0, 2, 1]);
        h.extend_from_slice(&src.port().to_be_bytes());
        h.extend_from_slice(&16271u16.to_be_bytes());
        h
    }

    /// One real connection: the client writes `bytes`, the server side is
    /// handed to `accept_proxied` as if it came from `wire`.
    async fn accept_as(wire: &str, trusted: &[std::net::IpAddr], bytes: Vec<u8>) -> Result<std::net::SocketAddr, String> {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let at = l.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let mut c = tokio::net::TcpStream::connect(at).await.unwrap();
            c.write_all(&bytes).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        });
        let (s, _) = l.accept().await.unwrap();
        let out = accept_proxied(s, wire.parse().unwrap(), trusted).await.map(|(_, real)| real);
        client.abort();
        out
    }

    /// ⭐ From our relay, the peer is who the header says: that address is
    /// what `handle_incoming` and the IP filter see, not the relay's.
    #[tokio::test]
    async fn a_trusted_relay_hands_over_the_real_peer_address() {
        let real = "198.51.100.7:51413".parse().unwrap();
        let got = accept_as("127.0.0.1:40000", &[], header(real)).await.unwrap();
        assert_eq!(got, std::net::SocketAddr::V4(real));
        // A public relay is trusted only once listed.
        let relay: std::net::IpAddr = "203.0.113.20".parse().unwrap();
        let got = accept_as("203.0.113.20:40000", &[relay], header(real)).await.unwrap();
        assert_eq!(got, std::net::SocketAddr::V4(real));
    }

    /// ⭐ From anyone else the header is a forged identity and the connection
    /// is refused before it is read.
    #[tokio::test]
    async fn an_untrusted_source_is_refused() {
        let real = "198.51.100.7:51413".parse().unwrap();
        let err = accept_as("203.0.113.66:40000", &[], header(real)).await.unwrap_err();
        assert!(err.contains("untrusted"), "{err}");
        let listed: std::net::IpAddr = "203.0.113.20".parse().unwrap();
        assert!(accept_as("203.0.113.66:40000", &[listed], header(real)).await.is_err(), "another listed IP is not this one");
    }
}
