pub mod dial_limiter;
pub mod http;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicI64, Ordering as AtomicOrdering};
use std::time::Duration;
use tracing::{info, warn};
use librqbit_utp::UtpSocketUdp;

use crate::disk::DiskManager;
use crate::torrent::meta::TorrentState;

pub static DIAL_ATTEMPTED: AtomicU64 = AtomicU64::new(0);
pub static DIAL_TCP_OK: AtomicU64 = AtomicU64::new(0);
pub static DIAL_TCP_FAIL: AtomicU64 = AtomicU64::new(0);
pub static DIAL_UTP_OK: AtomicU64 = AtomicU64::new(0);
pub static DIAL_UTP_FAIL: AtomicU64 = AtomicU64::new(0);
pub static DIAL_UTP_FAIL_TIMEOUT: AtomicU64 = AtomicU64::new(0);
pub static DIAL_UTP_FAIL_ERROR: AtomicU64 = AtomicU64::new(0);
// Split error by cause (2026-04-17 investigation)
pub static DIAL_UTP_ERR_TOO_MANY: AtomicU64 = AtomicU64::new(0);
pub static DIAL_UTP_ERR_SEND_SYN: AtomicU64 = AtomicU64::new(0);
pub static DIAL_UTP_ERR_DISPATCHER: AtomicU64 = AtomicU64::new(0);
pub static DIAL_UTP_ERR_OTHER: AtomicU64 = AtomicU64::new(0);
// Dedup in-flight uTP dials per addr. librqbit-utp 0.7 has MAX_CONNECTING_PER_ADDR=4
// and silently drops the requester tx when exceeded → DispatcherDead on receiver.
// With 13k torrents, popular peers get 10+ concurrent dials across torrents → pileup.
pub static DIAL_UTP_SKIPPED_INFLIGHT: AtomicU64 = AtomicU64::new(0);
static UTP_INFLIGHT: std::sync::OnceLock<dashmap::DashSet<std::net::SocketAddr>> = std::sync::OnceLock::new();
fn utp_inflight() -> &'static dashmap::DashSet<std::net::SocketAddr> {
    UTP_INFLIGHT.get_or_init(dashmap::DashSet::new)
}
struct UtpInflightGuard(std::net::SocketAddr);
impl Drop for UtpInflightGuard {
    fn drop(&mut self) { utp_inflight().remove(&self.0); }
}

// Dedup dial_peer tasks per (info_hash, addr). Tracker/DHT/PEX can all queue
// the same peer addr for the same torrent concurrently → multiple dial_peer
// tasks race, wasting tokio tasks + CPU. Also skip if addr already connected.
pub static DIAL_SKIPPED_INFLIGHT: AtomicU64 = AtomicU64::new(0);
pub static DIAL_SKIPPED_CONNECTED: AtomicU64 = AtomicU64::new(0);
pub static DIAL_SKIPPED_SELF: AtomicU64 = AtomicU64::new(0);

/// Self-dial filter. Tracker can hand us back our own public IP (VPS egress)
/// as a "peer" — we'd loop back through haproxy and handshake ourselves. Set
/// `TYPHON_SELF_IPS=ip1,ip2,...` (comma-separated v4 or v6) to skip those.
/// Only skips when addr.port() == listen_port — cross-engine dials between
/// hoard (16172) and race (16171) via public IP are still allowed.
// Self-IP set for the pre-dial fast-path. RwLock (not OnceLock) so Go can push
// the CURRENT public IP at runtime (set_self_ips RPC) instead of relying on a
// hard-coded TYPHON_SELF_IPS that goes stale when the ISP lease changes. This is
// only an optimisation to skip the wasted connect; correctness is guaranteed by
// the peer_id self-check in handshake::outgoing regardless of this list.
static SELF_IPS: std::sync::RwLock<Vec<std::net::IpAddr>> = std::sync::RwLock::new(Vec::new());

/// Replace the self-IP set (called at startup from env seed, then at runtime by
/// Go via the set_self_ips RPC with the observed public IP).
pub fn set_self_ips(ips: Vec<std::net::IpAddr>) {
    eprintln!("[tracker] self-dial filter set to {:?}", ips);
    *SELF_IPS.write().unwrap() = ips;
}

/// Seed the self-IP set from the TYPHON_SELF_IPS env (comma-separated).
pub fn seed_self_ips_from_env() {
    let raw = std::env::var("TYPHON_SELF_IPS").unwrap_or_default();
    let v: Vec<std::net::IpAddr> = raw.split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse::<std::net::IpAddr>().ok())
        .collect();
    if !v.is_empty() { set_self_ips(v); }
}
fn is_self_dial(addr: std::net::SocketAddr, listen_port: u16) -> bool {
    if addr.port() != listen_port { return false; }
    is_self_ip(addr.ip())
}

/// Port-agnostic self-IP check. Used by peer/mod.rs to reject incoming
/// connections whose source is ourselves (SOCKS5 loopback, styx netns
/// outbound SNAT, etc.). Source port is ephemeral on inbound so we can't
/// reuse is_self_dial.
pub fn is_self_ip(ip: std::net::IpAddr) -> bool {
    // Normalize IPv4-mapped v6 (::ffff:a.b.c.d) so we compare against raw v4.
    let target = match ip {
        std::net::IpAddr::V6(v6) => v6.to_ipv4_mapped().map(std::net::IpAddr::V4).unwrap_or(std::net::IpAddr::V6(v6)),
        v4 => v4,
    };
    SELF_IPS.read().unwrap().iter().any(|ip| *ip == target)
}
static DIAL_INFLIGHT: std::sync::OnceLock<dashmap::DashSet<([u8; 20], std::net::SocketAddr)>> = std::sync::OnceLock::new();
fn dial_inflight() -> &'static dashmap::DashSet<([u8; 20], std::net::SocketAddr)> {
    DIAL_INFLIGHT.get_or_init(dashmap::DashSet::new)
}
struct DialInflightGuard(([u8; 20], std::net::SocketAddr));
impl Drop for DialInflightGuard {
    fn drop(&mut self) { dial_inflight().remove(&self.0); }
}
pub static DIAL_HANDSHAKE_OK: AtomicU64 = AtomicU64::new(0);
pub static DIAL_HANDSHAKE_FAIL: AtomicU64 = AtomicU64::new(0);

// Outcome breakdown for the two legs of `open()`. It dials plaintext first and
// only then falls back to MSE, so a peer that *requires* encryption always
// burns one failed plaintext handshake before the MSE attempt. Without the
// split, `dial_handshake_fail` lumps "swarm is encryption-only" together with
// "MSE fallback is broken" — the pair below is what tells them apart.
pub static DIAL_PLAIN_OK: AtomicU64 = AtomicU64::new(0);
pub static DIAL_PLAIN_FAIL: AtomicU64 = AtomicU64::new(0);
pub static DIAL_MSE_ATTEMPTED: AtomicU64 = AtomicU64::new(0);
pub static DIAL_MSE_OK: AtomicU64 = AtomicU64::new(0);
pub static DIAL_MSE_FAIL: AtomicU64 = AtomicU64::new(0);
/// block_mse instrumentation: inbound MSE handshakes refused, outbound MSE
/// fallbacks skipped, and live encrypted sessions closed by a flag flip.
pub static MSE_INBOUND_REFUSED: AtomicU64 = AtomicU64::new(0);
pub static MSE_OUTBOUND_SKIPPED: AtomicU64 = AtomicU64::new(0);
pub static MSE_SESSIONS_DROPPED: AtomicU64 = AtomicU64::new(0);
// Queue accounting: `enqueue_dial` is fire-and-forget into an unbounded
// channel, so a peer that never reaches a dial worker leaves no trace at all.
pub static DIAL_ENQUEUED: AtomicU64 = AtomicU64::new(0);
pub static DIAL_ENQUEUE_DROPPED: AtomicU64 = AtomicU64::new(0);

/// Single-torrent dial trace, armed with `TYPHON_DIAL_TRACE_IH=<40 hex chars>`.
/// The global counters run at several hundred dials/s, which drowns the ~40
/// peers of one torrent completely; this narrows the log to one info_hash so a
/// stuck torrent can be followed decision by decision. Unset = zero overhead
/// beyond one pointer comparison per dial.
static DIAL_TRACE_IH: std::sync::OnceLock<Option<[u8; 20]>> = std::sync::OnceLock::new();

pub fn dial_trace_ih() -> &'static Option<[u8; 20]> {
    DIAL_TRACE_IH.get_or_init(|| {
        let raw = std::env::var("TYPHON_DIAL_TRACE_IH").ok()?;
        let hex = raw.trim();
        if hex.len() != 40 {
            return None;
        }
        let mut out = [0u8; 20];
        for i in 0..20 {
            out[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
        }
        info!("[dialtrace] armed for info_hash {}", hex);
        Some(out)
    })
}

#[inline]
fn is_traced(info_hash: &[u8; 20]) -> bool {
    dial_trace_ih().as_ref() == Some(info_hash)
}

// BT protocol message counters (diagnostic for download/upload flow)
pub static BT_SENT_INTERESTED: AtomicU64 = AtomicU64::new(0);
pub static BT_GOT_UNCHOKE: AtomicU64 = AtomicU64::new(0);
pub static BT_GOT_CHOKE: AtomicU64 = AtomicU64::new(0);
pub static BT_GOT_BITFIELD: AtomicU64 = AtomicU64::new(0);
pub static BT_GOT_HAVE_ALL: AtomicU64 = AtomicU64::new(0);
pub static BT_GOT_HAVE_NONE: AtomicU64 = AtomicU64::new(0);
pub static BT_GOT_HAVE: AtomicU64 = AtomicU64::new(0);
pub static BT_GOT_INTERESTED: AtomicU64 = AtomicU64::new(0);
pub static BT_GOT_REQUEST: AtomicU64 = AtomicU64::new(0);
pub static BT_SENT_PIECE: AtomicU64 = AtomicU64::new(0);
pub static BT_SENT_REQUEST: AtomicU64 = AtomicU64::new(0);
pub static BT_GOT_PIECE: AtomicU64 = AtomicU64::new(0);
pub static BT_DL_ENTRIES_LOOP: AtomicU64 = AtomicU64::new(0);        // how many peer sessions entered the dl loop
pub static BT_DL_SHOULD_INTERESTED_FALSE: AtomicU64 = AtomicU64::new(0); // how many times should_be_interested returned false (at least once per peer)

// Instant gauges: signed so we can spot classification/accounting bugs (negative means over-decrement).
// Peer is classified when it sends HaveAll (seed), a full bitfield (seed), or a partial bitfield (leech).
// Peers that disconnect before sending either message never appear here.
pub static PEERS_SEEDERS_CONNECTED: AtomicI64 = AtomicI64::new(0);
pub static PEERS_LEECHERS_CONNECTED: AtomicI64 = AtomicI64::new(0);

// Leecher lifetime histogram + outcome counters — captures what happens between
// "peer sent us a partial bitfield" and "peer disconnected".
pub static LEECH_LIFETIME_LT1S: AtomicU64 = AtomicU64::new(0);
pub static LEECH_LIFETIME_1_5S: AtomicU64 = AtomicU64::new(0);
pub static LEECH_LIFETIME_5_30S: AtomicU64 = AtomicU64::new(0);
pub static LEECH_LIFETIME_30_300S: AtomicU64 = AtomicU64::new(0);
pub static LEECH_LIFETIME_GT300S: AtomicU64 = AtomicU64::new(0);
pub static LEECH_NEVER_INTERESTED: AtomicU64 = AtomicU64::new(0);
pub static LEECH_GOT_INTERESTED: AtomicU64 = AtomicU64::new(0);
pub static LEECH_GOT_REQUEST: AtomicU64 = AtomicU64::new(0);
pub static LEECH_WE_SERVED_PIECE: AtomicU64 = AtomicU64::new(0);

// Direction-split cumulative classification counters.
// IN = peer dialed us, OUT = we dialed the peer.
// If LEECHERS_IN_TOTAL >> LEECHERS_OUT_TOTAL we can confirm NAT-bound leechers.
pub static SEEDERS_IN_TOTAL: AtomicU64 = AtomicU64::new(0);
pub static SEEDERS_OUT_TOTAL: AtomicU64 = AtomicU64::new(0);
pub static LEECHERS_IN_TOTAL: AtomicU64 = AtomicU64::new(0);
pub static LEECHERS_OUT_TOTAL: AtomicU64 = AtomicU64::new(0);

// BEP 10 / BEP 11 PEX counters.
pub static PEX_EXT_HANDSHAKES_SENT: AtomicU64 = AtomicU64::new(0);
pub static PEX_EXT_HANDSHAKES_RECV: AtomicU64 = AtomicU64::new(0);
pub static PEX_MSGS_SENT: AtomicU64 = AtomicU64::new(0);
pub static PEX_MSGS_RECV: AtomicU64 = AtomicU64::new(0);
pub static PEX_PEERS_DISCOVERED: AtomicU64 = AtomicU64::new(0);
pub static PEX_PEERS_DIALED: AtomicU64 = AtomicU64::new(0);

/// Pending-dial channel. PEX-discovered peer addrs go through here so that
/// `dial_peer` never recurses directly on itself (which the compiler cannot
/// prove Send, since recursion-through-async-fn has unresolved Send inference).
/// Initialized by `start_announce_loop`. A consumer task reads the queue and
/// spawns `dial_peer` for each entry.
static DIAL_TX: std::sync::OnceLock<tokio::sync::mpsc::UnboundedSender<(std::net::SocketAddr, Arc<TorrentState>)>> = std::sync::OnceLock::new();

pub fn enqueue_dial(addr: std::net::SocketAddr, torrent: Arc<TorrentState>) {
    let traced = is_traced(&torrent.info_hash);
    match DIAL_TX.get() {
        Some(tx) => match tx.send((addr, torrent)) {
            Ok(()) => {
                DIAL_ENQUEUED.fetch_add(1, AtomicOrdering::Relaxed);
                if traced {
                    info!("[dialtrace] {} enqueued", addr);
                }
            }
            Err(_) => {
                // Consumer task is gone: every future dial is a silent no-op.
                DIAL_ENQUEUE_DROPPED.fetch_add(1, AtomicOrdering::Relaxed);
                if traced {
                    warn!("[dialtrace] {} DROPPED (dial consumer dead)", addr);
                }
            }
        },
        None => {
            DIAL_ENQUEUE_DROPPED.fetch_add(1, AtomicOrdering::Relaxed);
            if traced {
                warn!("[dialtrace] {} DROPPED (dial queue not initialised)", addr);
            }
        }
    }
}

/// Pick the binding to dial a given peer from. Hash on the peer's IP so the
/// same peer always dialed from the same binding (consistent peer_id from the
/// peer's perspective, sticks under the same tunnel for return path), and
/// different peers spread across bindings (load balancing). Falls back to a
/// zero binding when the input slice is empty (caller logs the warning).
fn pick_binding_for_dial(
    bindings: &[crate::config::ResolvedBinding],
    addr: std::net::SocketAddr,
) -> crate::config::ResolvedBinding {
    if bindings.is_empty() {
        return crate::config::ResolvedBinding {
            id: 0,
            addr: "0.0.0.0:0".parse().unwrap(),
            peer_id: [0u8; 20],
            fwmark: 0,
            advertised_port: 0,
            only_v6: false,
        };
    }
    if bindings.len() == 1 {
        return bindings[0].clone();
    }
    // FNV-1a 32-bit hash on IP octets — cheap, deterministic, well-spread.
    let mut h: u32 = 0x811c9dc5;
    let octets: Vec<u8> = match addr.ip() {
        std::net::IpAddr::V4(v4) => v4.octets().to_vec(),
        std::net::IpAddr::V6(v6) => v6.octets().to_vec(),
    };
    for b in &octets {
        h ^= *b as u32;
        h = h.wrapping_mul(0x01000193);
    }
    bindings[(h as usize) % bindings.len()].clone()
}

/// Start tracker announce loops for all torrents.
/// Spawns a task per torrent that periodically announces to its trackers.
/// The Go control plane owns every announce; this only wires up the dial queue
/// so PEX/DHT outbound works.
///
/// `bindings` drives the dial queue consumer's per-peer source-IP / peer_id
/// selection: each outbound dial (PEX/DHT/add_peers) is hashed onto one binding
/// (deterministic by peer addr), and the resulting binding's listen_addr is
/// used to source-bind the TCP socket. With multi-tunnel WG, this spreads
/// outbound dials across N tunnels while keeping each peer pinned to one
/// binding (so the peer always sees the same peer_id from us). Single-binding
/// (legacy) collapses to a single (peer_id, source_ip).
pub fn start_announce_loop(
    disk_mgr: Arc<DiskManager>,
    bindings: Vec<crate::config::ResolvedBinding>,
    utp_socket: Option<Arc<UtpSocketUdp>>,
    max_dials_per_sec: f64,
) {
    if bindings.is_empty() {
        warn!("[tracker] start_announce_loop called with no bindings — dials will use kernel default");
    }
    // Set up the PEX/DHT dial queue consumer. Peer tasks push addrs through
    // `enqueue_dial`; this consumer owns disk_mgr/utp_socket and actually
    // spawns dial_peer (breaking the recursive-async-fn Send cycle). Per-dial
    // it picks one binding by hashing the peer addr → consistent peer_id+src
    // per peer across reconnects.
    let (dial_tx, mut dial_rx) = tokio::sync::mpsc::unbounded_channel::<(std::net::SocketAddr, Arc<TorrentState>)>();
    if DIAL_TX.set(dial_tx).is_ok() {
        let disk_c = disk_mgr.clone();
        let utp_c = utp_socket.clone();
        let bindings_c = bindings.clone();
        // This loop is the single chokepoint every outbound dial goes through
        // -- tracker peers, PEX, DHT and the orchestrator's `add_peers` all
        // arrive here -- which is why the pacing, the connection ceiling and
        // the startup pause all live at this one spot rather than at each
        // discovery source. The pacer is owned by the task: no other caller,
        // so no locking.
        let mut pacer = dial_limiter::DialPacer::new(max_dials_per_sec);
        if pacer.is_some() {
            info!("[peer] outbound dial rate limit active: {}/s", max_dials_per_sec);
        }
        tokio::spawn(async move {
            while let Some((addr, t)) = dial_rx.recv().await {
                // Startup pause: drop rather than queue. The peer is not lost
                // -- the next announce hands it back -- whereas queueing a
                // paused hoard's worth of peers would grow without bound and
                // then release the very burst the pause exists to prevent.
                if dial_limiter::dials_paused() {
                    dial_limiter::DIAL_SKIPPED_PAUSED.fetch_add(1, AtomicOrdering::Relaxed);
                    continue;
                }
                // Ceiling on live connections. Checked before pacing so a
                // saturated engine sheds work instead of accumulating delay.
                if dial_limiter::conn_cap_reached() {
                    dial_limiter::DIAL_SKIPPED_CONN_CAP.fetch_add(1, AtomicOrdering::Relaxed);
                    continue;
                }
                if let Some(p) = pacer.as_mut() {
                    p.acquire().await;
                }
                let d = disk_c.clone();
                let u = utp_c.clone();
                let b = pick_binding_for_dial(&bindings_c, addr);
                tokio::spawn(async move {
                    dial_peer(addr, t, d, b.peer_id, u, b.addr.port(), b.fwmark).await;
                });
            }
        });
    }

}



/// Best-effort peer-client identification from the 20-byte peer_id (Azureus style).
pub fn client_from_peer_id(pid: &[u8; 20]) -> String {
    if pid.len() >= 8 && pid[0] == b'-' && pid[7] == b'-' {
        let code = std::str::from_utf8(&pid[1..3]).unwrap_or("??");
        let ver = std::str::from_utf8(&pid[3..7]).unwrap_or("????");
        let name = match code {
            "qB" => "qBittorrent",
            "UT" => "uTorrent",
            "TR" => "Transmission",
            "DE" => "Deluge",
            "LT" => "libtorrent",
            "AZ" => "Azureus",
            _ => code,
        };
        format!("{} {}", name, ver)
    } else {
        String::new()
    }
}

use tokio::net::{TcpSocket, TcpStream};
use crate::peer::transport::PeerTransport;
use crate::crypto::stream::CryptoStream;

// Try TCP first, fall back to uTP if it fails (NAT-bound peers).
async fn try_tcp(
    addr: std::net::SocketAddr,
    source_fwmark: u32,
) -> Option<PeerTransport> {
    use crate::peer::SOCKS5_OUTBOUND;
    #[cfg(unix)]
    use std::os::unix::io::AsRawFd;
    // 3s — on a reachable LAN/WAN peer, TCP connect succeeds in ≤300ms.
    let connect_fut = async move {
        // Route v6 outbound via SOCKS5 if configured (avoids leaking Free IP).
        // SOCKS5 client doesn't expose fwmark here; the multi-binding case
        // (Proton WG) doesn't use SOCKS5 and falls through to direct.
        {
            if let Some((host, port, auth)) = SOCKS5_OUTBOUND.get() {
                let target = (addr.ip().to_string(), addr.port());
                let stream_res = match auth {
                    Some((u, pw)) => tokio_socks::tcp::Socks5Stream::connect_with_password(
                        (host.as_str(), *port), target, u, pw,
                    ).await,
                    None => tokio_socks::tcp::Socks5Stream::connect(
                        (host.as_str(), *port), target,
                    ).await,
                };
                return stream_res.ok().map(|s| s.into_inner());
            }
        }
        // Multi-tunnel path: set SO_MARK on the socket so the kernel's
        // `ip rule fwmark X lookup tableX` policy steers outbound through
        // the right WG interface. Source IP becomes 10.2.0.2 automatically
        // (the only address bound on every wg-hy* iface, per Proton's
        // shared-Address scheme).
        // A device pin and/or a fwmark both need the socket before connect().
        // The device is what steers a Proton-style setup, where every tunnel
        // shares 10.2.0.2 and a source address decides nothing.
        #[cfg(unix)]
        if source_fwmark != 0 || crate::netpin::bind_device().is_some() {
            let socket = if addr.is_ipv4() {
                TcpSocket::new_v4().ok()?
            } else {
                TcpSocket::new_v6().ok()?
            };
            if crate::netpin::pin_fd(socket.as_raw_fd()).is_err() {
                // Fail the dial rather than let it leave by the default route.
                return None;
            }
            if source_fwmark != 0 {
                let mark_val: libc::c_int = source_fwmark as libc::c_int;
                let rc = unsafe {
                    libc::setsockopt(
                        socket.as_raw_fd(),
                        libc::SOL_SOCKET,
                        libc::SO_MARK,
                        &mark_val as *const _ as *const libc::c_void,
                        std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                    )
                };
                if rc != 0 {
                    return None;
                }
            }
            return socket.connect(addr).await.ok();
        }
        TcpStream::connect(addr).await.ok()
    };
    match tokio::time::timeout(Duration::from_secs(3), connect_fut).await {
        Ok(Some(s)) => {
            s.set_nodelay(true).ok();
            DIAL_TCP_OK.fetch_add(1, AtomicOrdering::Relaxed);
            Some(PeerTransport::Tcp(s))
        }
        _ => {
            DIAL_TCP_FAIL.fetch_add(1, AtomicOrdering::Relaxed);
            None
        }
    }
}
async fn try_utp(addr: std::net::SocketAddr, sock: &Arc<UtpSocketUdp>) -> Option<PeerTransport> {
    if !utp_inflight().insert(addr) {
        DIAL_UTP_SKIPPED_INFLIGHT.fetch_add(1, AtomicOrdering::Relaxed);
        return None;
    }
    let _guard = UtpInflightGuard(addr);
    match tokio::time::timeout(Duration::from_secs(15), sock.connect(addr)).await {
        Ok(Ok(s)) => {
            DIAL_UTP_OK.fetch_add(1, AtomicOrdering::Relaxed);
            Some(PeerTransport::Utp(s))
        }
        Ok(Err(e)) => {
            DIAL_UTP_FAIL.fetch_add(1, AtomicOrdering::Relaxed);
            DIAL_UTP_FAIL_ERROR.fetch_add(1, AtomicOrdering::Relaxed);
            let es = format!("{}", e);
            if es.contains("too many active connections") {
                DIAL_UTP_ERR_TOO_MANY.fetch_add(1, AtomicOrdering::Relaxed);
            } else if es.contains("error sending SYN") {
                DIAL_UTP_ERR_SEND_SYN.fetch_add(1, AtomicOrdering::Relaxed);
                // Sample: warn first 20 unique errors to see actual io error
                static COUNT: AtomicU64 = AtomicU64::new(0);
                if COUNT.fetch_add(1, AtomicOrdering::Relaxed) < 20 {
                    warn!("[utp] ErrorSendingSyn to {}: {}", addr, e);
                }
            } else if es.contains("dispatcher dead") {
                DIAL_UTP_ERR_DISPATCHER.fetch_add(1, AtomicOrdering::Relaxed);
            } else {
                DIAL_UTP_ERR_OTHER.fetch_add(1, AtomicOrdering::Relaxed);
                static COUNT: AtomicU64 = AtomicU64::new(0);
                if COUNT.fetch_add(1, AtomicOrdering::Relaxed) < 20 {
                    warn!("[utp] other error to {}: {}", addr, e);
                }
            }
            None
        }
        Err(_) => {
            DIAL_UTP_FAIL.fetch_add(1, AtomicOrdering::Relaxed);
            DIAL_UTP_FAIL_TIMEOUT.fetch_add(1, AtomicOrdering::Relaxed);
            None
        }
    }
}

// Try the (transport, handshake-pair) combos in order: TCP-plain, TCP-MSE, uTP-plain, uTP-MSE.
/// Open a peer connection and complete the BitTorrent handshake, returning the
/// framed-ready stream plus what the handshake told us.
///
/// Shared by `dial_peer` (which then runs a full session) and magnet metadata
/// resolution (which only wants the extension handshake). It deliberately takes
/// an info hash rather than a TorrentState: resolving a magnet has no torrent
/// and no disk yet.
// Each combo returns the full handshake result so the session gets the remote peer_id.
pub(crate) async fn open_peer(
    addr: std::net::SocketAddr,
    utp_socket: &Option<Arc<UtpSocketUdp>>,
    info_hash: &[u8; 20],
    peer_id: &[u8; 20],
    source_fwmark: u32,
    traced: bool,
) -> Option<(CryptoStream, bool, bool, [u8; 20], bool)> {
    // 2.7.11: PLAINTEXT-FIRST outbound dial. Most peers only *prefer* MSE
    // (they accept plaintext too), so dialing plain first avoids the RC4
    // per-byte cost — measured single-conn: plain 619 MB/s vs MSE 280 MB/s.
    // MSE is kept as a FALLBACK for the minority that *require* encryption,
    // so connectivity is never lost. TYPHON_NO_MSE=1 also skips the MSE
    // fallback (pure-plaintext bench).
    let skip_mse = std::env::var("TYPHON_NO_MSE").map(|v| v == "1").unwrap_or(false)
        || crate::peer::block_mse();
    // TCP plaintext (preferred)
    if let Some(mut t) = try_tcp(addr, source_fwmark).await {
        match crate::peer::handshake::outgoing(&mut t, info_hash, peer_id).await {
            Ok(hs) => {
                DIAL_PLAIN_OK.fetch_add(1, AtomicOrdering::Relaxed);
                if traced {
                    info!("[dialtrace] {} tcp/plain handshake OK", addr);
                }
                return Some((CryptoStream::plain(t), hs.fast_extension, hs.extended_protocol, hs.peer_id, false));
            }
            Err(e) => {
                DIAL_PLAIN_FAIL.fetch_add(1, AtomicOrdering::Relaxed);
                if traced {
                    info!("[dialtrace] {} tcp/plain handshake FAIL: {}", addr, e);
                }
            }
        }
    } else if traced {
        info!("[dialtrace] {} tcp connect FAIL (plain leg)", addr);
    }
    // TCP MSE (fallback for require-encryption peers)
    if skip_mse {
        MSE_OUTBOUND_SKIPPED.fetch_add(1, AtomicOrdering::Relaxed);
    }
    if !skip_mse {
        if let Some(mut t) = try_tcp(addr, source_fwmark).await {
            DIAL_MSE_ATTEMPTED.fetch_add(1, AtomicOrdering::Relaxed);
            match crate::crypto::mse::handshake_outgoing(&mut t, info_hash, peer_id).await {
                Ok((enc, dec, hs)) => {
                    DIAL_MSE_OK.fetch_add(1, AtomicOrdering::Relaxed);
                    if traced {
                        info!("[dialtrace] {} tcp/MSE handshake OK", addr);
                    }
                    return Some((CryptoStream::new(t, Some(enc), Some(dec)), hs.fast_extension, hs.extended_protocol, hs.peer_id, true));
                }
                Err(e) => {
                    DIAL_MSE_FAIL.fetch_add(1, AtomicOrdering::Relaxed);
                    if traced {
                        info!("[dialtrace] {} tcp/MSE handshake FAIL: {}", addr, e);
                    }
                }
            }
        }
    }
    if let Some(sock) = utp_socket.as_ref() {
        // uTP plaintext (preferred)
        if let Some(mut t) = try_utp(addr, sock).await {
            if let Ok(hs) = crate::peer::handshake::outgoing(&mut t, info_hash, peer_id).await {
                return Some((CryptoStream::plain(t), hs.fast_extension, hs.extended_protocol, hs.peer_id, false));
            }
        }
        // uTP MSE (fallback)
        if !skip_mse {
            if let Some(mut t) = try_utp(addr, sock).await {
                if let Ok((enc, dec, hs)) = crate::crypto::mse::handshake_outgoing(&mut t, info_hash, peer_id).await {
                    return Some((CryptoStream::new(t, Some(enc), Some(dec)), hs.fast_extension, hs.extended_protocol, hs.peer_id, true));
                }
            }
        }
    }
    None
}


pub async fn dial_peer(
    addr: std::net::SocketAddr,
    torrent: Arc<TorrentState>,
    disk: Arc<DiskManager>,
    peer_id: [u8; 20],
    utp_socket: Option<Arc<UtpSocketUdp>>,
    listen_port: u16,
    source_fwmark: u32,
) {
    use tokio_util::codec::Framed;
    use crate::wire::codec::BtCodec;

    let traced = is_traced(&torrent.info_hash);
    if traced {
        info!("[dialtrace] {} dial_peer entered", addr);
    }

    // Skip dials to our own public IPs on our own port — tracker can hand us
    // back our VPS egress IP as a "peer" and we'd handshake ourselves through
    // haproxy. Cross-engine (hoard <-> race) dials through the public IP stay
    // allowed because the port differs.
    if is_self_dial(addr, listen_port) {
        DIAL_SKIPPED_SELF.fetch_add(1, AtomicOrdering::Relaxed);
        if traced {
            info!("[dialtrace] {} SKIPPED self-dial", addr);
        }
        return;
    }
    // Skip if we're already connected to this peer on this torrent.
    if torrent.connected_addrs.contains_key(&addr) {
        DIAL_SKIPPED_CONNECTED.fetch_add(1, AtomicOrdering::Relaxed);
        if traced {
            info!("[dialtrace] {} SKIPPED already connected", addr);
        }
        return;
    }
    // Skip if another dial_peer task is already in flight for (info_hash, addr).
    let inflight_key = (torrent.info_hash, addr);
    if !dial_inflight().insert(inflight_key) {
        DIAL_SKIPPED_INFLIGHT.fetch_add(1, AtomicOrdering::Relaxed);
        if traced {
            info!("[dialtrace] {} SKIPPED inflight (stale entry never cleared?)", addr);
        }
        return;
    }
    let _dial_guard = DialInflightGuard(inflight_key);

    DIAL_ATTEMPTED.fetch_add(1, AtomicOrdering::Relaxed);

    let (cs, fast_ext, lt_ext, remote_peer_id, is_encrypted) = match open_peer(addr, &utp_socket, &torrent.info_hash, &peer_id, source_fwmark, traced).await {
        Some(v) => { DIAL_HANDSHAKE_OK.fetch_add(1, AtomicOrdering::Relaxed); v }
        None => {
            DIAL_HANDSHAKE_FAIL.fetch_add(1, AtomicOrdering::Relaxed);
            if traced {
                info!("[dialtrace] {} all legs exhausted, no session", addr);
            }
            return;
        }
    };
    if traced {
        info!("[dialtrace] {} session starting (encrypted={})", addr, is_encrypted);
    }
    let framed = Framed::new(cs, BtCodec::new());
    crate::peer::session::run(
        framed,
        addr,
        torrent,
        disk,
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
