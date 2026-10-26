pub mod bencode;
pub mod choking;
pub mod download;
pub mod extension;
pub mod handshake;
pub mod holepunch;
pub mod message;
pub mod metadata;
pub mod peerclient;
pub mod proxy_protocol;
#[cfg(test)]
mod rebind_tests;
pub mod session;
pub mod transport;

use std::net::SocketAddr;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

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
use librqbit_utp::UtpSocketUdp;
use std::time::Duration;
use tokio::net::{TcpListener, TcpSocket};
use tracing::{info, warn};

use crate::disk::DiskManager;
use crate::peer::transport::PeerTransport;
use crate::torrent::TorrentManager;

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
    listen_until(
        bindings,
        default_port,
        torrent_mgr,
        disk_mgr,
        utp,
        listening,
        crate::stop::global().clone(),
    )
    .await
}

/// `listen` with the stop signal given, so a test can close the listeners
/// without raising the process-wide one.
async fn listen_until(
    bindings: Vec<crate::config::ResolvedBinding>,
    default_port: u16,
    torrent_mgr: Arc<TorrentManager>,
    disk_mgr: Arc<DiskManager>,
    utp: UtpHandle,
    listening: std::sync::Arc<std::sync::atomic::AtomicBool>,
    stop: crate::stop::Stop,
) -> Result<(), Box<dyn std::error::Error>> {
    if bindings.is_empty() {
        return Err("no bindings to listen on (config error)".into());
    }
    // Refused before any socket: outside Linux a pinned listener would be an
    // unpinned one (`netpin::DEVICE_PIN_SUPPORTED`).
    if !crate::netpin::DEVICE_PIN_SUPPORTED && bindings.iter().any(|b| b.egress.device().is_some())
    {
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
            // The stop closes the listeners, TCP and uTP, at its start: the
            // drain and the flush that follow take no new peer in. The peers
            // already connected are not touched; they end with the process.
            _ = stop.wait() => {
                info!("[peer] stop: closing the listeners");
                close_listeners(&mut handles, &mut utp_task, &utp).await;
                listening.store(false, std::sync::atomic::Ordering::Relaxed);
                break;
            }
        }
    }
    Ok(())
}

/// Let go of every listening socket and wait until the accept tasks are gone,
/// so the ports are free when this returns.
async fn close_listeners(
    handles: &mut Vec<tokio::task::JoinHandle<()>>,
    utp_task: &mut Option<tokio::task::JoinHandle<()>>,
    utp: &UtpHandle,
) {
    for h in handles.iter() {
        h.abort();
    }
    for h in handles.drain(..) {
        let _ = h.await;
    }
    if let Some(t) = utp_task.take() {
        t.abort();
        let _ = t.await;
    }
    utp.close();
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
        socket
            .set_reuseaddr(true)
            .map_err(|e| format!("{}: {e}", b.addr))?;
        socket
            .bind(b.addr)
            .map_err(|e| format!("cannot bind {}: {e}", b.addr))?;
        let listener = socket
            .listen(4096)
            .map_err(|e| format!("cannot listen on {}: {e}", b.addr))?;
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
    info!(
        "[peer] uTP listening on {} (peer_id from binding[0], advertised_port={})",
        sock.bind_addr(),
        utp_advertised_port
    );
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

    /// Shut the socket down for good: the handle lets go of it, its
    /// dispatcher ends and the UDP port is released. Dials after this find no
    /// uTP socket, as on an engine that never had one.
    pub fn close(&self) {
        let old = self.inner.write().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(old) = old {
            old.cancel.cancel();
        }
    }

    /// Put `live` in place and shut the old socket down.
    fn install(&self, live: UtpLive) {
        let old = self
            .inner
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .replace(live);
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
    let opts = librqbit_utp::SocketOpts {
        max_live_vsocks: std::num::NonZeroUsize::new(4096),
        ..Default::default()
    };
    let cancel = opts.cancellation_token.clone();
    // uTP is raw UDP and gets the same device pin as everything else.
    // Without it the tunnel steering would hold for TCP and leak for uTP,
    // which is the shape of leak nobody notices: it is the same swarm.
    let dev = device
        .map(|d| d.parse::<librqbit_utp::BindDevice>())
        .transpose()
        .map_err(|e| format!("bind_device is not usable for the uTP socket: {e}"))?;
    let udp_opts = librqbit_utp::UtpSocketUdpOpts {
        bind_device: dev.as_ref(),
    };
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
    std::thread::available_parallelism()
        .map(|v| v.get())
        .unwrap_or(1)
}

fn session_runtimes() -> &'static [tokio::runtime::Handle] {
    SESSION_RTS.get_or_init(|| {
        let n = session_runtimes_n();
        let mut handles = Vec::with_capacity(n);
        for i in 0..n {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
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
        info!(
            "[peer] thread-per-core: {} dedicated session runtimes",
            handles.len()
        );
        handles
    })
}

/// After a failed accept, wait before the next one.
///
/// Out of descriptors (EMFILE, ENFILE) or buffers, `accept` fails again at
/// once for as long as the condition lasts: retried immediately, the loop
/// spins a core and writes a warning per turn. The wait doubles from 10 ms to
/// a second, and the warning is written on the 1st, 2nd, 4th, 8th... failure.
async fn accept_backoff(kind: &str, e: &std::io::Error, failures: &mut u32) {
    *failures = failures.saturating_add(1);
    if failures.is_power_of_two() {
        warn!(
            "[peer] {} accept error ({} in a row): {}",
            kind, failures, e
        );
    }
    let ms = 10u64
        .saturating_mul(1u64 << (*failures - 1).min(7))
        .min(1000);
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

async fn tcp_accept_loop(
    listener: TcpListener,
    torrent_mgr: Arc<TorrentManager>,
    disk_mgr: Arc<DiskManager>,
    peer_id: [u8; 20],
    utp_socket: Option<Arc<UtpSocketUdp>>,
    listen_port: u16,
) {
    let mut failures = 0u32;
    loop {
        let (stream, addr) = match listener.accept().await {
            Ok(v) => {
                failures = 0;
                v
            }
            Err(e) => {
                accept_backoff("tcp", &e, &mut failures).await;
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
                handle_incoming(
                    PeerTransport::Tcp(stream),
                    addr,
                    tm,
                    dm,
                    peer_id,
                    u,
                    listen_port,
                )
                .await;
            });
        } else {
            let idx = SESSION_RR.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % rts.len();
            match stream.into_std() {
                Ok(std_s) => {
                    rts[idx].spawn(async move {
                        match tokio::net::TcpStream::from_std(std_s) {
                            Ok(s) => {
                                handle_incoming(
                                    PeerTransport::Tcp(s),
                                    addr,
                                    tm,
                                    dm,
                                    peer_id,
                                    u,
                                    listen_port,
                                )
                                .await;
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
    let addr_str = if bind_addr.is_empty() {
        "[::]".to_string()
    } else {
        bind_addr
    };
    let sockaddr: std::net::SocketAddr = format!("{}:{}", addr_str, port)
        .parse()
        .map_err(|e| format!("invalid proxy-v2 listen addr {}:{}: {}", addr_str, port, e))?;
    let socket = if sockaddr.is_ipv4() {
        TcpSocket::new_v4()?
    } else {
        TcpSocket::new_v6()?
    };
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
    info!(
        "[peer] PROXY v2 TCP listening on {} (backlog=4096)",
        sockaddr
    );

    let mut failures = 0u32;
    loop {
        let (stream, wire_addr) = match listener.accept().await {
            Ok(v) => {
                failures = 0;
                v
            }
            Err(e) => {
                accept_backoff("proxy-v2", &e, &mut failures).await;
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
                    handle_incoming(
                        PeerTransport::Tcp(stream),
                        real_addr,
                        tm,
                        dm,
                        peer_id,
                        u,
                        port,
                    )
                    .await;
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
    match tokio::time::timeout(
        Duration::from_secs(5),
        proxy_protocol::parse_v2(&mut stream),
    )
    .await
    {
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
    if v4.is_loopback() {
        return true;
    }
    let o = v4.octets();
    // RFC1918 including Docker default bridge 172.17.0.0/16
    o[0] == 10 || (o[0] == 172 && (16..=31).contains(&o[1])) || (o[0] == 192 && o[1] == 168)
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
            handle_incoming(
                PeerTransport::Utp(stream),
                addr,
                tm,
                dm,
                peer_id,
                u,
                listen_port,
            )
            .await;
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

/// Inbound handshakes refused because the torrent asked for is stopped.
pub static INBOUND_REFUSED_PAUSED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Inbound handshakes refused because the engine already holds
/// `max_connections` live sessions.
pub static INBOUND_REFUSED_CAP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Whether the engine is at its `max_connections` ceiling. The ceiling used to
/// bound only the dials we made: a peer reaching us added a session past it.
/// Checked once the torrent is named, before our handshake goes back.
fn at_connection_cap(t: &crate::torrent::meta::TorrentState) -> bool {
    if t.limiter().conn_cap_reached() {
        INBOUND_REFUSED_CAP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return true;
    }
    false
}

/// Whether an inbound peer may open a session on this torrent.
///
/// A stopped torrent -- by the operator, the queue, or held by a path claim
/// until its files are proven -- takes no session at all. The session itself
/// neither requests nor serves while the flag is set (`download.rs`,
/// `session.rs`), but accepting it kept a connection open on a torrent the
/// user stopped, and any moment the flag was lifted it became a live transfer
/// with a peer the user never let in. Refused at the handshake, the peer
/// learns nothing: same answer as for a torrent we do not have.
pub fn accepts_inbound(t: &crate::torrent::meta::TorrentState) -> bool {
    use std::sync::atomic::Ordering;
    !t.is_paused.load(Ordering::Relaxed) && !t.is_removed.load(Ordering::Relaxed)
}

/// Inbound connections still in their handshake, all listeners together.
/// A handshake takes a round trip or two; these bound what silent sockets can
/// hold for the 30 s the handshake is given, before any torrent is named.
const MAX_HANDSHAKING: usize = 4096;
/// The same, per source: an IPv4 address, or an IPv6 /64 (one host's usual
/// allocation, which it can spread connections over freely).
const MAX_HANDSHAKING_PER_SOURCE: u32 = 32;
static HANDSHAKING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static HANDSHAKING_BY_SOURCE: std::sync::OnceLock<dashmap::DashMap<std::net::IpAddr, u32>> =
    std::sync::OnceLock::new();

/// Inbound connections closed unread because too many were already in their
/// handshake, from that source or overall.
pub static INBOUND_REFUSED_BUSY: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Inbound plaintext handshakes closed because `peer_encryption = require`.
pub static PLAINTEXT_INBOUND_REFUSED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Inbound handshakes dropped because their source is being ignored
/// (`max_unknown_infohash_per_ip`).
pub static INBOUND_REFUSED_IGNORED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Inbound encrypted handshakes that named an info_hash we do not hold.
/// Ordinary traffic on a public port: counted, not logged one by one.
pub static UNKNOWN_SKEY_HANDSHAKES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Longest an unreported run of unknown-info_hash handshakes waits before the
/// one summary line that stands for it.
const UNKNOWN_SKEY_LOG_WINDOW: Duration = Duration::from_secs(60);

/// The log's side of `UNKNOWN_SKEY_HANDSHAKES`: one summary line per window,
/// instead of one warning per connection. On a public port the encrypted
/// handshakes naming a torrent we do not hold arrive by the thousand a minute
/// and drowned the rest of the log. No timer: the line for a window is written
/// by the first event after it, and the counter is exact in between.
#[derive(Default)]
pub struct UnknownSkeyLog {
    /// When the open window started, and how many it has seen.
    window: std::sync::Mutex<Option<(std::time::Instant, u64)>>,
}

impl UnknownSkeyLog {
    /// Record one handshake. Returns `(count, seconds)` for the window that
    /// just closed when it is time to write its line, else `None`.
    pub fn note(&self, now: std::time::Instant) -> Option<(u64, u64)> {
        let mut w = self.window.lock().unwrap_or_else(|e| e.into_inner());
        match *w {
            Some((start, n)) if now.duration_since(start) >= UNKNOWN_SKEY_LOG_WINDOW => {
                *w = Some((now, 1));
                Some((n, now.duration_since(start).as_secs()))
            }
            Some((_, ref mut n)) => {
                *n += 1;
                None
            }
            None => {
                *w = Some((now, 1));
                None
            }
        }
    }
}

/// Window over which a source's unknown-info_hash handshakes are counted.
const UNKNOWN_HASH_WINDOW: Duration = Duration::from_secs(60);
/// How long a source over the limit is ignored.
const UNKNOWN_HASH_IGNORE: Duration = Duration::from_secs(600);
/// Sources tracked at once. Full of live entries, a new source is simply not
/// tracked: the table never grows past this, whatever the senders do.
const UNKNOWN_HASH_MAX_TRACKED: usize = 4096;

struct UnknownHashEntry {
    window_start: std::time::Instant,
    count: u32,
    ignored_until: Option<std::time::Instant>,
}

impl UnknownHashEntry {
    /// Nothing left to remember about this source.
    fn expired(&self, now: std::time::Instant) -> bool {
        match self.ignored_until {
            Some(t) => now >= t,
            None => now.duration_since(self.window_start) >= UNKNOWN_HASH_WINDOW,
        }
    }
}

/// Per-source count of inbound handshakes that named an info_hash we do not
/// have (`max_unknown_infohash_per_ip`). Over the limit within a window, the
/// source is ignored for a while. Bounded: at most `UNKNOWN_HASH_MAX_TRACKED`
/// entries, expired ones swept when it fills.
#[derive(Default)]
pub struct UnknownHashLimiter {
    sources: dashmap::DashMap<std::net::IpAddr, UnknownHashEntry>,
}

impl UnknownHashLimiter {
    /// Whether this source is currently being ignored.
    pub fn is_ignored(&self, ip: std::net::IpAddr, now: std::time::Instant) -> bool {
        let key = handshake_source(ip);
        match self.sources.get(&key) {
            Some(e) => e.ignored_until.is_some_and(|t| now < t),
            None => false,
        }
    }

    /// Record one unknown-info_hash handshake. `limit` 0 = off. Returns true
    /// when this one puts the source over the limit.
    pub fn note_unknown(&self, ip: std::net::IpAddr, now: std::time::Instant, limit: u32) -> bool {
        if limit == 0 {
            return false;
        }
        let key = handshake_source(ip);
        if !self.sources.contains_key(&key) && self.sources.len() >= UNKNOWN_HASH_MAX_TRACKED {
            self.sources.retain(|_, e| !e.expired(now));
            if self.sources.len() >= UNKNOWN_HASH_MAX_TRACKED {
                return false;
            }
        }
        let mut e = self.sources.entry(key).or_insert(UnknownHashEntry {
            window_start: now,
            count: 0,
            ignored_until: None,
        });
        if e.expired(now) {
            *e = UnknownHashEntry {
                window_start: now,
                count: 0,
                ignored_until: None,
            };
        }
        if e.ignored_until.is_some() {
            return true;
        }
        e.count = e.count.saturating_add(1);
        if e.count > limit {
            e.ignored_until = Some(now + UNKNOWN_HASH_IGNORE);
            return true;
        }
        false
    }

    pub fn tracked(&self) -> usize {
        self.sources.len()
    }
}

fn handshake_source(ip: std::net::IpAddr) -> std::net::IpAddr {
    use std::net::IpAddr;
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => {
                let s = v6.segments();
                IpAddr::V6(std::net::Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
            }
        },
        v4 => v4,
    }
}

/// One inbound connection's place among those in their handshake, given back
/// when dropped.
struct HandshakeSlot(std::net::IpAddr);

impl HandshakeSlot {
    fn take(ip: std::net::IpAddr) -> Option<HandshakeSlot> {
        use std::sync::atomic::Ordering;
        if HANDSHAKING.fetch_add(1, Ordering::Relaxed) >= MAX_HANDSHAKING {
            HANDSHAKING.fetch_sub(1, Ordering::Relaxed);
            return None;
        }
        let key = handshake_source(ip);
        let mut n = HANDSHAKING_BY_SOURCE
            .get_or_init(Default::default)
            .entry(key)
            .or_insert(0);
        if *n >= MAX_HANDSHAKING_PER_SOURCE {
            drop(n);
            HANDSHAKING.fetch_sub(1, Ordering::Relaxed);
            return None;
        }
        *n += 1;
        Some(HandshakeSlot(key))
    }
}

impl Drop for HandshakeSlot {
    fn drop(&mut self) {
        HANDSHAKING.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        let map = HANDSHAKING_BY_SOURCE.get_or_init(Default::default);
        if let Some(mut n) = map.get_mut(&self.0) {
            *n = n.saturating_sub(1);
        }
        map.remove_if(&self.0, |_, n| *n == 0);
    }
}

/// How a plaintext BitTorrent handshake opens: the length byte, then the protocol string.
const PLAIN_HANDSHAKE_HEAD: [u8; 20] = *b"\x13BitTorrent protocol";

async fn handle_incoming(
    mut stream: PeerTransport,
    addr: SocketAddr,
    torrent_mgr: Arc<TorrentManager>,
    disk_mgr: Arc<DiskManager>,
    peer_id: [u8; 20],
    utp_socket: Option<Arc<UtpSocketUdp>>,
    listen_port: u16,
) {
    use crate::crypto::stream::CryptoStream;
    use crate::wire::codec::BtCodec;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_util::codec::Framed;

    // Before anything costs: no handshake, no MSE, no log line per attempt
    // from a range somebody filtered precisely because it knocks a lot.
    if crate::ipfilter::blocked(addr.ip()) {
        crate::ipfilter::BLOCKED_IN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return;
    }
    let unknown_limit = torrent_mgr.policy().unknown_hash_limit();
    if unknown_limit > 0
        && torrent_mgr
            .unknown_hashes()
            .is_ignored(addr.ip(), std::time::Instant::now())
    {
        INBOUND_REFUSED_IGNORED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return;
    }
    let Some(slot) = HandshakeSlot::take(addr.ip()) else {
        INBOUND_REFUSED_BUSY.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return;
    };
    // Debug, not info: anyone who can reach the port decides how often it is
    // written. `INBOUND_ACCEPTED` counts them.
    tracing::debug!("[peer] incoming {} from {}", stream.kind(), addr);
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

    // Tell MSE from plaintext on the 20 bytes a plaintext handshake opens with
    // (length byte and protocol string), not on the first byte alone: the first
    // byte of an MSE public key is 19 one time in 256. An MSE initiator sends at
    // least 96 bytes, so reading 20 never waits for more than it will get.
    let mut head = [0u8; 20];
    match tokio::time::timeout(HS_TIMEOUT, stream.read_exact(&mut head)).await {
        Ok(Ok(_)) => {}
        Ok(Err(_)) => return,
        Err(_) => {
            HS_TIMED_OUT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::debug!(
                "[peer] {} sent nothing within {:?}, dropping",
                addr,
                HS_TIMEOUT
            );
            return;
        }
    }

    let (crypto_stream, torrent, fast_ext, lt_ext, remote_peer_id, is_encrypted) = if head
        == PLAIN_HANDSHAKE_HEAD
    {
        // Plaintext BT handshake — 20 bytes read, 48 to go. `rest` is what
        // follows the length byte, as before.
        let mut rest = [0u8; 67];
        rest[..19].copy_from_slice(&head[1..]);
        match tokio::time::timeout(HS_TIMEOUT, stream.read_exact(&mut rest[19..])).await {
            Ok(Ok(_)) => {}
            Ok(Err(_)) => return,
            Err(_) => {
                HS_TIMED_OUT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return;
            }
        }
        // `peer_encryption = require`: a plaintext handshake is closed before
        // the torrent is even looked up, so it learns nothing.
        if torrent_mgr.policy().encryption() == crate::peer::extension::Encryption::Require {
            PLAINTEXT_INBOUND_REFUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return;
        }

        let mut info_hash = [0u8; 20];
        info_hash.copy_from_slice(&rest[27..47]);
        let mut remote_pid = [0u8; 20];
        remote_pid.copy_from_slice(&rest[47..67]);
        let fast_ext = (rest[26] & 0x04) != 0;
        let ext_proto = (rest[24] & 0x10) != 0; // reserved[5] in the full 68-byte HS = rest[24]

        let torrent = match torrent_mgr.get(&info_hash) {
            Some(t) if accepts_inbound(&t) => t,
            Some(_) => {
                INBOUND_REFUSED_PAUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return;
            }
            None => {
                torrent_mgr.unknown_hashes().note_unknown(
                    addr.ip(),
                    std::time::Instant::now(),
                    unknown_limit,
                );
                return;
            }
        };
        if at_connection_cap(&torrent) {
            return;
        }

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
            Err(_) => {
                HS_TIMED_OUT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return;
            }
        }

        (
            CryptoStream::plain(stream),
            torrent,
            fast_ext,
            ext_proto,
            remote_pid,
            false,
        )
    } else {
        // MSE handshake — the 20 bytes already read are the start of the DH
        // public key, handed on intact.
        if torrent_mgr.policy().block_mse() {
            crate::tracker::MSE_INBOUND_REFUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return;
        }
        let mut ya_rest = [0u8; 95];
        ya_rest[..19].copy_from_slice(&head[1..]);
        match tokio::time::timeout(HS_TIMEOUT, stream.read_exact(&mut ya_rest[19..])).await {
            Ok(Ok(_)) => {}
            Ok(Err(_)) => {
                warn!("[peer] {} MSE: failed to read Ya", addr);
                return;
            }
            Err(_) => {
                HS_TIMED_OUT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return;
            }
        }

        let tm_clone = torrent_mgr.clone();
        let unknown_skey = std::sync::atomic::AtomicBool::new(false);
        let mse_res = tokio::time::timeout(
            HS_TIMEOUT,
            crate::crypto::mse::handshake_incoming_with(
                &mut stream,
                head[0],
                &ya_rest,
                &peer_id,
                // resolved per torrent below, same rule as the plaintext path
                |req2_hash| {
                    let found = tm_clone.lookup_skey(req2_hash);
                    if found.is_none() {
                        unknown_skey.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    found
                },
                torrent_mgr.policy().encryption() == crate::peer::extension::Encryption::Require,
            ),
        )
        .await;
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
                    Some(t) if accepts_inbound(&t) => t,
                    Some(_) => {
                        INBOUND_REFUSED_PAUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        return;
                    }
                    None => return,
                };
                if at_connection_cap(&torrent) {
                    return;
                }
                (
                    CryptoStream::new(stream, Some(enc), Some(dec)),
                    torrent,
                    hs.fast_extension,
                    hs.extended_protocol,
                    hs.peer_id,
                    true,
                )
            }
            Err(e) => {
                if unknown_skey.load(std::sync::atomic::Ordering::Relaxed) {
                    torrent_mgr.unknown_hashes().note_unknown(
                        addr.ip(),
                        std::time::Instant::now(),
                        unknown_limit,
                    );
                }
                if unknown_skey.load(std::sync::atomic::Ordering::Relaxed) {
                    // Ordinary traffic: counted, and summed up once a window.
                    // Every other failure keeps its own line.
                    UNKNOWN_SKEY_HANDSHAKES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if let Some((n, secs)) =
                        torrent_mgr.unknown_skeys().note(std::time::Instant::now())
                    {
                        info!(
                            "[peer] {n} incoming encrypted handshakes for torrents we do not hold in the last {secs} s"
                        );
                    }
                } else {
                    warn!("[peer] {} MSE handshake failed: {}", addr, e);
                }
                return;
            }
        }
    };

    drop(slot);
    let framed = Framed::new(crypto_stream, BtCodec::new());
    // What the peer is told to call back on (BEP 10 `p`): read now, so it is
    // the public port behind a translating NAT and the new one after a rebind.
    let advertised = torrent_mgr.announced_port(listen_port);
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
        advertised,
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
        assert!(is_trusted_proxy_source(
            &addr("[::ffff:10.0.0.1]:16271"),
            &[]
        ));
        assert!(is_trusted_proxy_source(
            &addr("[::ffff:127.0.0.1]:16271"),
            &[]
        ));
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

        assert!(is_trusted_proxy_source(
            &addr("93.184.216.34:16271"),
            &extras
        ));
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
        let mut h = vec![
            0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A, 0x21, 0x11, 0,
            12,
        ];
        h.extend_from_slice(&src.ip().octets());
        h.extend_from_slice(&[192, 0, 2, 1]);
        h.extend_from_slice(&src.port().to_be_bytes());
        h.extend_from_slice(&16271u16.to_be_bytes());
        h
    }

    /// One real connection: the client writes `bytes`, the server side is
    /// handed to `accept_proxied` as if it came from `wire`.
    async fn accept_as(
        wire: &str,
        trusted: &[std::net::IpAddr],
        bytes: Vec<u8>,
    ) -> Result<std::net::SocketAddr, String> {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let at = l.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let mut c = tokio::net::TcpStream::connect(at).await.unwrap();
            c.write_all(&bytes).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        });
        let (s, _) = l.accept().await.unwrap();
        let out = accept_proxied(s, wire.parse().unwrap(), trusted)
            .await
            .map(|(_, real)| real);
        client.abort();
        out
    }

    /// ⭐ From our relay, the peer is who the header says: that address is
    /// what `handle_incoming` and the IP filter see, not the relay's.
    #[tokio::test]
    async fn a_trusted_relay_hands_over_the_real_peer_address() {
        let real = "198.51.100.7:51413".parse().unwrap();
        let got = accept_as("127.0.0.1:40000", &[], header(real))
            .await
            .unwrap();
        assert_eq!(got, std::net::SocketAddr::V4(real));
        // A public relay is trusted only once listed.
        let relay: std::net::IpAddr = "203.0.113.20".parse().unwrap();
        let got = accept_as("203.0.113.20:40000", &[relay], header(real))
            .await
            .unwrap();
        assert_eq!(got, std::net::SocketAddr::V4(real));
    }

    /// ⭐ From anyone else the header is a forged identity and the connection
    /// is refused before it is read.
    #[tokio::test]
    async fn an_untrusted_source_is_refused() {
        let real = "198.51.100.7:51413".parse().unwrap();
        let err = accept_as("203.0.113.66:40000", &[], header(real))
            .await
            .unwrap_err();
        assert!(err.contains("untrusted"), "{err}");
        let listed: std::net::IpAddr = "203.0.113.20".parse().unwrap();
        assert!(
            accept_as("203.0.113.66:40000", &[listed], header(real))
                .await
                .is_err(),
            "another listed IP is not this one"
        );
    }
}

#[cfg(test)]
mod unknown_hash_limiter_tests {
    use super::*;
    use std::time::Instant;

    fn ip(s: &str) -> std::net::IpAddr {
        s.parse().unwrap()
    }

    /// Within one window the limit is allowed; the next one tips the source
    /// into being ignored, which lasts, then ends.
    #[test]
    fn a_source_over_the_limit_is_ignored_for_a_while() {
        let l = UnknownHashLimiter::default();
        let t0 = Instant::now();
        let a = ip("192.168.99.1");
        for _ in 0..3 {
            assert!(!l.note_unknown(a, t0, 3));
        }
        assert!(!l.is_ignored(a, t0));
        assert!(l.note_unknown(a, t0, 3), "the fourth is over");
        assert!(l.is_ignored(a, t0 + Duration::from_secs(599)));
        assert!(!l.is_ignored(a, t0 + UNKNOWN_HASH_IGNORE), "the ban ends");
        assert!(
            !l.is_ignored(ip("192.168.99.2"), t0),
            "another source is untouched"
        );
        assert!(!l.note_unknown(a, t0, 0), "limit 0 counts nothing");
    }

    /// Slow guessing under the limit per window never trips it.
    #[test]
    fn the_count_restarts_with_each_window() {
        let l = UnknownHashLimiter::default();
        let t0 = Instant::now();
        let a = ip("192.168.99.1");
        for w in 0..10u64 {
            let t = t0 + UNKNOWN_HASH_WINDOW * w as u32;
            assert!(!l.note_unknown(a, t, 2));
            assert!(!l.note_unknown(a, t, 2));
        }
        assert!(!l.is_ignored(a, t0 + UNKNOWN_HASH_WINDOW * 10));
    }

    /// A flood of distinct sources cannot grow the table past its cap, and a
    /// full table sheds only what has expired.
    #[test]
    fn the_table_is_bounded() {
        let l = UnknownHashLimiter::default();
        let t0 = Instant::now();
        for i in 0..(UNKNOWN_HASH_MAX_TRACKED as u32 + 500) {
            let o = i.to_be_bytes();
            l.note_unknown(ip(&format!("10.{}.{}.{}", o[1], o[2], o[3])), t0, 5);
        }
        assert_eq!(l.tracked(), UNKNOWN_HASH_MAX_TRACKED);
        // A window later every entry is stale: a newcomer sweeps them out.
        l.note_unknown(ip("203.0.113.9"), t0 + UNKNOWN_HASH_WINDOW, 5);
        assert_eq!(l.tracked(), 1);
    }

    /// One IPv6 host spreading over its /64 is one source.
    #[test]
    fn an_ipv6_64_is_one_source() {
        let l = UnknownHashLimiter::default();
        let t0 = Instant::now();
        assert!(!l.note_unknown(ip("2001:db8::1"), t0, 1));
        assert!(l.note_unknown(ip("2001:db8::2"), t0, 1));
        assert!(l.is_ignored(ip("2001:db8::3"), t0));
    }
}

#[cfg(test)]
mod paused_inbound_tests {
    use super::handle_incoming;
    use crate::torrent::TorrentManager;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn manager(
        tag: &str,
    ) -> (
        Arc<TorrentManager>,
        Arc<crate::disk::DiskManager>,
        std::path::PathBuf,
    ) {
        let root = std::env::temp_dir().join(format!("typhon-inb-{tag}-{}", std::process::id()));
        let (data, resume) = (root.join("data"), root.join("resume"));
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&resume).unwrap();
        let disk = Arc::new(crate::disk::DiskManager::new(16));
        let mgr = Arc::new(TorrentManager::new(
            data.to_string_lossy().into_owned(),
            resume.to_string_lossy().into_owned(),
            disk.clone(),
        ));
        (mgr, disk, root)
    }

    /// A private one-piece torrent; the bencode lengths are computed.
    fn torrent_bytes() -> Vec<u8> {
        let mut info = Vec::new();
        info.extend_from_slice(b"d6:lengthi16384e4:name5:inbnd12:piece lengthi16384e6:pieces20:");
        info.extend_from_slice(&[0x5Au8; 20]);
        info.extend_from_slice(b"7:privatei1ee");
        let announce = "https://tracker.example/announce";
        let mut out = format!("d8:announce{}:{announce}4:info", announce.len()).into_bytes();
        out.extend_from_slice(&info);
        out.push(b'e');
        out
    }

    /// One plaintext handshake from a real TCP peer; what came back, if anything,
    /// before the server side closed or 5 s passed.
    async fn knock(
        mgr: Arc<TorrentManager>,
        disk: Arc<crate::disk::DiskManager>,
        ih: [u8; 20],
    ) -> Vec<u8> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let at = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (s, addr) = listener.accept().await.unwrap();
            handle_incoming(
                super::PeerTransport::Tcp(s),
                addr,
                mgr,
                disk,
                [7u8; 20],
                None,
                at.port(),
            )
            .await;
        });
        let mut c = tokio::net::TcpStream::connect(at).await.unwrap();
        let mut hs = vec![19u8];
        hs.extend_from_slice(b"BitTorrent protocol");
        hs.extend_from_slice(&[0u8; 8]);
        hs.extend_from_slice(&ih);
        hs.extend_from_slice(&[9u8; 20]);
        c.write_all(&hs).await.unwrap();
        let mut got = vec![0u8; 68];
        let n =
            match tokio::time::timeout(std::time::Duration::from_secs(5), c.read(&mut got)).await {
                Ok(Ok(n)) => n,
                _ => 0,
            };
        drop(c);
        server.abort();
        got.truncate(n);
        got
    }

    /// Parent-compatible control: by default a plaintext handshake for a
    /// known torrent is answered. Uses no `peer_encryption` symbol.
    #[tokio::test]
    async fn by_default_a_plaintext_handshake_for_a_known_torrent_is_answered() {
        let (mgr, disk, root) = manager("plain-default");
        let (ih, _) = mgr
            .add_torrent_bytes(
                &torrent_bytes(),
                root.join("data").to_str().unwrap(),
                false,
                false,
            )
            .unwrap();
        assert_eq!(knock(mgr, disk, ih).await.len(), 68);
        let _ = std::fs::remove_dir_all(root);
    }

    /// `peer_encryption = require`: a plaintext handshake for a torrent we do
    /// have is closed unanswered. At the default (`allow`) the same handshake
    /// is answered, so the refusal is the option, not the fixture.
    #[tokio::test]
    async fn require_encryption_refuses_an_inbound_plaintext_handshake() {
        let (mgr, disk, root) = manager("require");
        let (ih, _) = mgr
            .add_torrent_bytes(
                &torrent_bytes(),
                root.join("data").to_str().unwrap(),
                false,
                false,
            )
            .unwrap();

        let answered = knock(mgr.clone(), disk.clone(), ih).await;
        assert_eq!(
            answered.len(),
            68,
            "allow (default) answers a plaintext handshake"
        );

        mgr.policy().set_encryption("prefer");
        let answered = knock(mgr.clone(), disk.clone(), ih).await;
        assert_eq!(answered.len(), 68, "prefer still accepts plaintext inbound");

        mgr.policy().set_encryption("require");
        let refused = knock(mgr.clone(), disk.clone(), ih).await;
        assert!(
            refused.is_empty(),
            "require: plaintext closed unanswered, got {} bytes",
            refused.len()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// ⭐ An MSE initiator whose public key happens to start with byte 19 --
    /// the length byte of a plaintext handshake, one dial in 256 -- is still an
    /// MSE initiator. Taken for plaintext, its connection is closed (the
    /// protocol string does not follow). Here the key is forced to start with
    /// 19 and the dial goes through the real acceptance path: both ends must
    /// derive the same keys, which the initiator proves by being heard, over the
    /// encrypted stream, by the session on the other side.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_mse_dial_whose_public_key_starts_with_19_is_accepted() {
        use futures::{SinkExt, StreamExt};
        use std::sync::atomic::Ordering;
        use tokio_util::codec::Framed;

        let (mgr, disk, root) = manager("mse-19");
        let (ih, _) = mgr
            .add_torrent_bytes(
                &torrent_bytes(),
                root.join("data").to_str().unwrap(),
                false,
                false,
            )
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let at = listener.local_addr().unwrap();
        let server = tokio::spawn({
            let mgr = mgr.clone();
            async move {
                let (s, addr) = listener.accept().await.unwrap();
                handle_incoming(
                    super::PeerTransport::Tcp(s),
                    addr,
                    mgr,
                    disk,
                    [7u8; 20],
                    None,
                    at.port(),
                )
                .await;
            }
        });

        let (xa, ya) = crate::crypto::mse::gen_keypair_starting_with(19);
        assert_eq!(
            ya[0], 19,
            "the fixture: a public key that begins with the plaintext length byte"
        );
        let mut t = super::PeerTransport::Tcp(tokio::net::TcpStream::connect(at).await.unwrap());
        let done = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            crate::crypto::mse::handshake_outgoing_keyed(
                &mut t,
                &ih,
                b"-TY0001-initiator000",
                false,
                xa,
                ya,
            ),
        )
        .await
        .expect("the handshake finished in time");
        let (enc, dec, hs) = done.expect(
            "an MSE initiator whose public key starts with byte 19 was taken for a plaintext handshake and dropped",
        );
        assert_eq!(
            hs.peer_id, [7u8; 20],
            "the reply is ours: the handshake went through the MSE path"
        );

        // Same keys on both sides: the session's first message decrypts into
        // something the codec reads, and what we send is understood.
        let mut peer = Framed::new(
            crate::crypto::stream::CryptoStream::new(t, Some(enc), Some(dec)),
            crate::wire::codec::BtCodec::new(),
        );
        match tokio::time::timeout(std::time::Duration::from_secs(5), peer.next()).await {
            Ok(Some(Ok(_))) => {}
            other => {
                panic!("the session's first message did not decrypt into a message: {other:?}")
            }
        }
        peer.send(crate::peer::message::Message::Interested)
            .await
            .unwrap();
        let torrent = mgr.get(&ih).unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while !torrent
            .peer_stats
            .iter()
            .any(|e| e.value().interested.load(Ordering::Relaxed))
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "what we encrypted was not decrypted by the session"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        server.abort();
        let _ = std::fs::remove_dir_all(root);
    }

    /// Parent-compatible control: with no setting, a source that sends fifty
    /// handshakes for unknown info_hashes is still answered for a real one.
    /// Uses no `max_unknown_infohash_per_ip` symbol.
    #[tokio::test]
    async fn by_default_unknown_infohashes_never_get_a_source_ignored() {
        let (mgr, disk, root) = manager("unknown-default");
        let (ih, _) = mgr
            .add_torrent_bytes(
                &torrent_bytes(),
                root.join("data").to_str().unwrap(),
                false,
                false,
            )
            .unwrap();
        for i in 0..50u8 {
            assert!(knock(mgr.clone(), disk.clone(), [i.wrapping_add(100); 20])
                .await
                .is_empty());
        }
        assert_eq!(knock(mgr, disk, ih).await.len(), 68);
        let _ = std::fs::remove_dir_all(root);
    }

    /// `max_unknown_infohash_per_ip`: at 0 (default) a source may name any
    /// number of unknown info_hashes. Set to 3, the fourth is the last it is
    /// heard on: the source is then ignored, even for a torrent we have. The
    /// limit applies per source, so a different IP is untouched.
    #[tokio::test]
    async fn unknown_infohash_limit_ignores_a_source_that_keeps_guessing() {
        let (mgr, disk, root) = manager("unknown");
        let (ih, _) = mgr
            .add_torrent_bytes(
                &torrent_bytes(),
                root.join("data").to_str().unwrap(),
                false,
                false,
            )
            .unwrap();

        for i in 0..6u8 {
            let r = knock(mgr.clone(), disk.clone(), [i; 20]).await;
            assert!(r.is_empty());
        }
        assert_eq!(
            knock(mgr.clone(), disk.clone(), ih).await.len(),
            68,
            "limit 0: never ignored"
        );

        mgr.policy().set_unknown_hash_limit(3);
        for i in 0..3u8 {
            assert!(knock(mgr.clone(), disk.clone(), [i; 20]).await.is_empty());
        }
        assert_eq!(
            knock(mgr.clone(), disk.clone(), ih).await.len(),
            68,
            "three guesses in: still heard"
        );
        assert!(knock(mgr.clone(), disk.clone(), [9u8; 20]).await.is_empty());
        assert!(
            knock(mgr.clone(), disk.clone(), ih).await.is_empty(),
            "past the limit the source is ignored, a real info_hash included"
        );
        mgr.policy().set_unknown_hash_limit(0);
        assert_eq!(
            knock(mgr.clone(), disk.clone(), ih).await.len(),
            68,
            "limit off: heard again"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// The limit is an engine's own: guesses made at one engine are not
    /// counted at another, and a source banned by one is heard by the other.
    /// Two engines in one process, both at 2, each guessed at twice from the
    /// same address: neither is over its limit.
    #[tokio::test]
    async fn the_unknown_infohash_count_belongs_to_each_engine() {
        let (race, race_disk, root_a) = manager("unknown-race");
        let (hoard, hoard_disk, root_b) = manager("unknown-hoard");
        let mut known = [0u8; 20];
        for (mgr, root) in [(&race, &root_a), (&hoard, &root_b)] {
            let (ih, _) = mgr
                .add_torrent_bytes(
                    &torrent_bytes(),
                    root.join("data").to_str().unwrap(),
                    false,
                    false,
                )
                .unwrap();
            known = ih;
            mgr.policy().set_unknown_hash_limit(2);
        }
        for i in 0..2u8 {
            assert!(knock(race.clone(), race_disk.clone(), [i + 40; 20])
                .await
                .is_empty());
        }
        for i in 0..2u8 {
            assert!(knock(hoard.clone(), hoard_disk.clone(), [i + 60; 20])
                .await
                .is_empty());
        }
        assert_eq!(
            knock(hoard.clone(), hoard_disk.clone(), known).await.len(),
            68,
            "the hoard counted only its own two"
        );
        assert_eq!(
            knock(race.clone(), race_disk.clone(), known).await.len(),
            68,
            "the race counted only its own two"
        );
        // The race goes over its own limit: only the race ignores the source.
        assert!(knock(race.clone(), race_disk.clone(), [99u8; 20])
            .await
            .is_empty());
        assert!(
            knock(race.clone(), race_disk.clone(), known)
                .await
                .is_empty(),
            "the race ignores the source"
        );
        assert_eq!(
            knock(hoard.clone(), hoard_disk.clone(), known).await.len(),
            68,
            "the hoard still hears it"
        );
        let _ = std::fs::remove_dir_all(root_a);
        let _ = std::fs::remove_dir_all(root_b);
    }

    /// ⭐ A stopped torrent takes no inbound session: the handshake is closed
    /// unanswered, as for a torrent we do not have. Started, the same torrent
    /// answers -- so the refusal is the pause, not the fixture.
    #[tokio::test]
    async fn a_stopped_torrent_refuses_an_inbound_handshake() {
        let (mgr, disk, root) = manager("stopped");
        let (ih, _) = mgr
            .add_torrent_bytes(
                &torrent_bytes(),
                root.join("data").to_str().unwrap(),
                true,
                false,
            )
            .unwrap();

        let refused = knock(mgr.clone(), disk.clone(), ih).await;
        assert!(
            refused.is_empty(),
            "a stopped torrent answered a handshake: {} bytes",
            refused.len()
        );

        mgr.start_torrent(&ih).unwrap();
        let answered = knock(mgr.clone(), disk.clone(), ih).await;
        assert!(
            answered.len() >= 48 && answered[0] == 19,
            "a started torrent must answer"
        );
        assert_eq!(&answered[28..48], &ih, "with its own info hash");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// ⭐ An engine at its `max_connections` ceiling takes no inbound session:
    /// the handshake is closed unanswered. Under the ceiling, the same torrent
    /// answers -- so the refusal is the ceiling, not the fixture.
    #[tokio::test]
    async fn an_engine_at_its_connection_ceiling_refuses_an_inbound_handshake() {
        let (mgr, disk, root) = manager("ceiling");
        let (ih, _) = mgr
            .add_torrent_bytes(
                &torrent_bytes(),
                root.join("data").to_str().unwrap(),
                false,
                false,
            )
            .unwrap();
        let limiter = mgr.limiter().clone();
        limiter.set_max_connections(1);
        limiter.connection_opened();

        let refused = knock(mgr.clone(), disk.clone(), ih).await;
        assert!(
            refused.is_empty(),
            "an engine at its ceiling answered a handshake: {} bytes",
            refused.len()
        );

        limiter.connection_closed();
        let answered = knock(mgr.clone(), disk.clone(), ih).await;
        assert!(
            answered.len() >= 48 && answered[0] == 19,
            "under the ceiling it answers"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod accept_error_tests {
    use std::sync::Arc;

    const NAME: &str = "peer::accept_error_tests::a_failing_accept_does_not_spin_a_core";

    fn cpu_ms() -> u64 {
        let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
        unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
        let ms = |t: libc::timeval| t.tv_sec as u64 * 1000 + t.tv_usec as u64 / 1000;
        ms(ru.ru_utime) + ms(ru.ru_stime)
    }

    /// ⭐ An accept that keeps failing -- here EMFILE, the process out of
    /// descriptors with connections waiting -- must not spin a core. Run in a
    /// child process: the descriptor limit is process-wide and would starve
    /// every other test.
    #[test]
    fn a_failing_accept_does_not_spin_a_core() {
        if std::env::var_os("TYPHON_ACCEPT_CHILD").is_none() {
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
                .env("TYPHON_ACCEPT_CHILD", "1")
                .output()
                .unwrap();
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(out.status.success() && text.contains("1 passed"), "{text}");
            return;
        }
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let root = std::env::temp_dir().join(format!("typhon-emfile-{}", std::process::id()));
            std::fs::create_dir_all(root.join("r")).unwrap();
            let disk = Arc::new(crate::disk::DiskManager::new(1));
            let mgr = Arc::new(crate::torrent::TorrentManager::new(
                root.to_string_lossy().into_owned(),
                root.join("r").to_string_lossy().into_owned(),
                disk.clone(),
            ));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let at = listener.local_addr().unwrap();
            let _clients: Vec<_> = (0..4)
                .map(|_| std::net::TcpStream::connect(at).unwrap())
                .collect();

            // No descriptor left to hand out: the limit just above the highest
            // open one, and every hole below it filled.
            let highest = std::fs::read_dir("/proc/self/fd")
                .unwrap()
                .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<u64>().ok())
                .max()
                .unwrap();
            let mut lim: libc::rlimit = unsafe { std::mem::zeroed() };
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) };
            lim.rlim_cur = highest + 1;
            assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lim) }, 0);
            let mut _fill = Vec::new();
            while let Ok(f) = std::fs::File::open("/dev/null") {
                _fill.push(f);
            }

            let before = cpu_ms();
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                super::tcp_accept_loop(listener, mgr, disk, [7u8; 20], None, at.port()),
            )
            .await;
            let spent = cpu_ms() - before;
            assert!(
                spent < 300,
                "the accept loop burnt {spent} ms of CPU in one second"
            );
        });
    }
}

#[cfg(test)]
mod handshake_cap_tests {
    use std::sync::{Arc, Mutex};
    use tokio::io::AsyncReadExt;

    fn manager(
        tag: &str,
    ) -> (
        Arc<crate::torrent::TorrentManager>,
        Arc<crate::disk::DiskManager>,
    ) {
        let root = std::env::temp_dir().join(format!("typhon-hscap-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(root.join("r")).unwrap();
        let disk = Arc::new(crate::disk::DiskManager::new(1));
        let mgr = Arc::new(crate::torrent::TorrentManager::new(
            root.to_string_lossy().into_owned(),
            root.join("r").to_string_lossy().into_owned(),
            disk.clone(),
        ));
        (mgr, disk)
    }

    /// ⭐ One source cannot park more than 32 silent connections in the
    /// handshake: past that, the listener closes them at once instead of
    /// holding each for the 30 s handshake timeout. The source is 127.0.0.77
    /// so no other test's connections share its count.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn one_source_cannot_park_more_than_32_silent_handshakes() {
        let (mgr, disk) = manager("src");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let at = listener.local_addr().unwrap();
        let server = tokio::spawn(super::tcp_accept_loop(
            listener,
            mgr,
            disk,
            [7u8; 20],
            None,
            at.port(),
        ));
        let mut socks = Vec::new();
        for _ in 0..64 {
            let s = tokio::net::TcpSocket::new_v4().unwrap();
            s.bind("127.0.0.77:0".parse().unwrap()).unwrap();
            socks.push(s.connect(at).await.unwrap());
        }
        let closed = futures::future::join_all(socks.iter_mut().map(|s| async move {
            let mut b = [0u8; 1];
            matches!(
                tokio::time::timeout(std::time::Duration::from_secs(2), s.read(&mut b)).await,
                Ok(Ok(0)) | Ok(Err(_))
            )
        }))
        .await
        .into_iter()
        .filter(|c| *c)
        .count();
        server.abort();
        assert_eq!(
            closed, 32,
            "the 32 connections past the cap are closed, the first 32 are not"
        );
    }

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Captured {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Captured;
        fn make_writer(&'a self) -> Captured {
            self.clone()
        }
    }

    /// An inbound connection writes nothing at the default level (info):
    /// whoever reaches the port would otherwise choose how fast the log grows.
    /// Run alone in a child process: `tracing` caches per call site whether
    /// anyone listens, and a test on another thread can leave it at "no".
    #[test]
    fn an_inbound_connection_is_not_logged_at_info() {
        const NAME: &str = "peer::handshake_cap_tests::an_inbound_connection_is_not_logged_at_info";
        if std::env::var_os("TYPHON_LOG_CHILD").is_none() {
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
                .env("TYPHON_LOG_CHILD", "1")
                .output()
                .unwrap();
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(out.status.success() && text.contains("1 passed"), "{text}");
            return;
        }
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(inbound_log_child());
    }

    async fn inbound_log_child() {
        let out = Captured::default();
        let sub = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_writer(out.clone())
            .finish();
        let _g = tracing::subscriber::set_default(sub);
        let (mgr, disk) = manager("log");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let at = listener.local_addr().unwrap();
        let c = tokio::net::TcpStream::connect(at).await.unwrap();
        let (s, addr) = listener.accept().await.unwrap();
        drop(c);
        super::handle_incoming(
            super::PeerTransport::Tcp(s),
            addr,
            mgr,
            disk,
            [7u8; 20],
            None,
            at.port(),
        )
        .await;
        tracing::info!("[peer] control line");
        let text = String::from_utf8_lossy(&out.0.lock().unwrap()).into_owned();
        assert!(text.contains("control line"), "the capture works: {text:?}");
        assert!(!text.contains("incoming"), "logged at info: {text}");
    }
}

#[cfg(test)]
mod unknown_skey_log_tests {
    use super::handle_incoming;
    use crate::torrent::TorrentManager;
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex};
    use tokio::io::AsyncWriteExt;

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Captured {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Captured;
        fn make_writer(&'a self) -> Captured {
            self.clone()
        }
    }

    /// True when this is the child process. Otherwise runs `name` alone in a
    /// child and checks it passed: `tracing` caches per call site whether
    /// anyone listens, and the counters are process-wide, so a test on another
    /// thread would feed both.
    fn is_child(name: &str) -> bool {
        if std::env::var_os("TYPHON_SKEY_CHILD").is_some() {
            return true;
        }
        let full = format!("peer::unknown_skey_log_tests::{name}");
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &full, "--nocapture", "--test-threads=1"])
            .env("TYPHON_SKEY_CHILD", "1")
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.status.success() && text.contains("1 passed"), "{text}");
        false
    }

    fn manager(tag: &str) -> (Arc<TorrentManager>, Arc<crate::disk::DiskManager>) {
        let root = std::env::temp_dir().join(format!("typhon-skey-{tag}-{}", std::process::id()));
        let (data, resume) = (root.join("data"), root.join("resume"));
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&resume).unwrap();
        let disk = Arc::new(crate::disk::DiskManager::new(16));
        let mgr = Arc::new(TorrentManager::new(
            data.to_string_lossy().into_owned(),
            resume.to_string_lossy().into_owned(),
            disk.clone(),
        ));
        (mgr, disk)
    }

    /// Serve one inbound connection on a fresh listener with `handle_incoming`,
    /// while `client` plays the peer on the other end.
    async fn serve_one<F, Fut>(
        mgr: &Arc<TorrentManager>,
        disk: &Arc<crate::disk::DiskManager>,
        client: F,
    ) where
        F: FnOnce(tokio::net::TcpStream) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let at = listener.local_addr().unwrap();
        let c = tokio::net::TcpStream::connect(at).await.unwrap();
        let (s, addr) = listener.accept().await.unwrap();
        let server = handle_incoming(
            super::PeerTransport::Tcp(s),
            addr,
            mgr.clone(),
            disk.clone(),
            [7u8; 20],
            None,
            at.port(),
        );
        tokio::join!(server, client(c));
    }

    fn captured_runtime(
        out: &Captured,
    ) -> (tokio::runtime::Runtime, tracing::subscriber::DefaultGuard) {
        let sub = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_writer(out.clone())
            .finish();
        let g = tracing::subscriber::set_default(sub);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        (rt, g)
    }

    /// An MSE initiator asking for `ih`: fails, since nobody holds it.
    async fn ask_for(c: tokio::net::TcpStream, ih: [u8; 20]) {
        let mut t = super::PeerTransport::Tcp(c);
        let r = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            crate::crypto::mse::handshake_outgoing(&mut t, &ih, b"-TY0001-initiator000"),
        )
        .await
        .expect("the handshake ended in time");
        assert!(r.is_err(), "nobody holds this torrent");
    }

    /// A window yields one line, carrying its count, when the first handshake
    /// after it arrives; nothing before.
    #[test]
    fn the_summary_comes_once_per_window_with_the_count() {
        use std::time::{Duration, Instant};
        let l = super::UnknownSkeyLog::default();
        let t0 = Instant::now();
        for i in 0..40 {
            assert_eq!(l.note(t0 + Duration::from_millis(i)), None);
        }
        assert_eq!(l.note(t0 + Duration::from_secs(59)), None);
        // 41 in the first window, then this one opens the second.
        assert_eq!(l.note(t0 + Duration::from_secs(61)), Some((41, 61)));
        assert_eq!(l.note(t0 + Duration::from_secs(62)), None);
        assert_eq!(l.note(t0 + Duration::from_secs(130)), Some((2, 69)));
    }

    /// ⭐ Fifty encrypted handshakes for torrents we do not hold write no
    /// warning each, and are all counted. On a public port that is ordinary
    /// traffic, and a warning per connection once filled the log file in a
    /// minute.
    #[test]
    fn handshakes_for_torrents_we_do_not_hold_are_counted_not_logged_each() {
        if !is_child("handshakes_for_torrents_we_do_not_hold_are_counted_not_logged_each") {
            return;
        }
        let out = Captured::default();
        let (rt, _g) = captured_runtime(&out);
        rt.block_on(async {
            let (mgr, disk) = manager("flood");
            for i in 0..50u8 {
                serve_one(&mgr, &disk, |c| ask_for(c, [i; 20])).await;
            }
        });
        let text = String::from_utf8_lossy(&out.0.lock().unwrap()).into_owned();
        let lines = text.lines().filter(|l| l.contains("[peer]")).count();
        assert!(
            lines <= 1,
            "{lines} log lines for 50 handshakes in one window: {text}"
        );
        assert_eq!(
            super::UNKNOWN_SKEY_HANDSHAKES.load(Ordering::Relaxed),
            50,
            "{text}"
        );
    }

    /// Any other way an encrypted handshake fails still has its warning, with
    /// the peer's address, and is not counted as an unknown torrent.
    #[test]
    fn another_handshake_failure_still_writes_its_own_line() {
        if !is_child("another_handshake_failure_still_writes_its_own_line") {
            return;
        }
        let out = Captured::default();
        let (rt, _g) = captured_runtime(&out);
        rt.block_on(async {
            let (mgr, disk) = manager("other");
            for _ in 0..3 {
                // A public key's worth of bytes, then the peer hangs up.
                serve_one(&mgr, &disk, |mut c| async move {
                    c.write_all(&[1u8; 96]).await.unwrap();
                    c.shutdown().await.unwrap();
                })
                .await;
            }
        });
        let text = String::from_utf8_lossy(&out.0.lock().unwrap()).into_owned();
        assert_eq!(
            text.matches("MSE handshake failed").count(),
            3,
            "one warning per failure: {text}"
        );
        assert_eq!(
            super::UNKNOWN_SKEY_HANDSHAKES.load(Ordering::Relaxed),
            0,
            "{text}"
        );
    }
}

#[cfg(test)]
mod holepunch_flood_tests {
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const NAME: &str =
        "peer::holepunch_flood_tests::a_peer_cannot_have_us_dial_more_than_ten_connects_a_minute";

    /// ⭐ A connected peer sending fifty hole-punch `Connect` in a row has us
    /// act on ten. Each would otherwise queue a dial to the address it names.
    /// Counted through `enqueue_dial`'s counters, so run alone in a child
    /// process where no other test feeds them.
    #[test]
    fn a_peer_cannot_have_us_dial_more_than_ten_connects_a_minute() {
        if std::env::var_os("TYPHON_PUNCH_CHILD").is_none() {
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
                .env("TYPHON_PUNCH_CHILD", "1")
                .output()
                .unwrap();
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(out.status.success() && text.contains("1 passed"), "{text}");
            return;
        }
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let root = std::env::temp_dir().join(format!("typhon-punch-{}", std::process::id()));
            std::fs::create_dir_all(root.join("r")).unwrap();
            let disk = Arc::new(crate::disk::DiskManager::new(1));
            let mgr = Arc::new(crate::torrent::TorrentManager::new(
                root.to_string_lossy().into_owned(),
                root.join("r").to_string_lossy().into_owned(),
                disk.clone(),
            ));
            let mut info =
                b"d6:lengthi16384e4:name5:punch12:piece lengthi16384e6:pieces20:".to_vec();
            info.extend_from_slice(&[0x5Au8; 20]);
            info.push(b'e');
            let mut torrent = b"d4:info".to_vec();
            torrent.extend_from_slice(&info);
            torrent.push(b'e');
            let (ih, _) = mgr
                .add_torrent_bytes(&torrent, root.to_str().unwrap(), false, false)
                .unwrap();

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let at = listener.local_addr().unwrap();
            let (m, d) = (mgr.clone(), disk.clone());
            tokio::spawn(async move {
                let (s, addr) = listener.accept().await.unwrap();
                super::handle_incoming(
                    super::PeerTransport::Tcp(s),
                    addr,
                    m,
                    d,
                    [7u8; 20],
                    None,
                    at.port(),
                )
                .await;
            });
            let mut c = tokio::net::TcpStream::connect(at).await.unwrap();
            let mut hs = vec![19u8];
            hs.extend_from_slice(b"BitTorrent protocol");
            hs.extend_from_slice(&[0, 0, 0, 0, 0, 0x10, 0, 0x04]);
            hs.extend_from_slice(&ih);
            hs.extend_from_slice(&[9u8; 20]);
            c.write_all(&hs).await.unwrap();
            let mut reply = [0u8; 68];
            c.read_exact(&mut reply).await.unwrap();
            let before = crate::tracker::DIAL_ENQUEUED.load(Ordering::Relaxed)
                + crate::tracker::DIAL_ENQUEUE_DROPPED.load(Ordering::Relaxed);
            for i in 1..=50u8 {
                let target: std::net::SocketAddr = format!("93.184.216.{i}:6881").parse().unwrap();
                let payload = crate::peer::holepunch::Punch::Connect(target).encode();
                let mut frame = ((payload.len() + 2) as u32).to_be_bytes().to_vec();
                frame.extend_from_slice(&[20, crate::peer::holepunch::OUR_UT_HOLEPUNCH_ID]);
                frame.extend_from_slice(&payload);
                c.write_all(&frame).await.unwrap();
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let dials = crate::tracker::DIAL_ENQUEUED.load(Ordering::Relaxed)
                + crate::tracker::DIAL_ENQUEUE_DROPPED.load(Ordering::Relaxed)
                - before;
            assert!(
                dials >= 1,
                "the fixture reaches the Connect handling at all"
            );
            assert_eq!(dials, 10, "fifty Connect, ten dials");
        });
    }
}
