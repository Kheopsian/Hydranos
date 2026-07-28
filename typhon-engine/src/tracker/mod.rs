pub mod http;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicI64, Ordering as AtomicOrdering};
use std::time::Duration;
use tokio::time;
use tracing::{info, warn, debug};
use librqbit_utp::UtpSocketUdp;

use crate::disk::DiskManager;
use crate::torrent::TorrentManager;
use crate::torrent::meta::{TorrentState, TorrentStatus};

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
static SELF_IPS: std::sync::OnceLock<Vec<std::net::IpAddr>> = std::sync::OnceLock::new();
fn self_ips() -> &'static [std::net::IpAddr] {
    SELF_IPS.get_or_init(|| {
        let raw = std::env::var("TYPHON_SELF_IPS").unwrap_or_default();
        let v: Vec<std::net::IpAddr> = raw.split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse::<std::net::IpAddr>().ok())
            .collect();
        if !v.is_empty() {
            eprintln!("[tracker] self-dial filter active on {:?}", v);
        }
        v
    })
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
    self_ips().iter().any(|ip| *ip == target)
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
    if let Some(tx) = DIAL_TX.get() {
        let _ = tx.send((addr, torrent));
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
/// When `disable_announce` is true, the announce loop is skipped (Go owns
/// announces) but the dial queue stays wired up so PEX/DHT outbound still works.
///
/// `bindings` drives the dial queue consumer's per-peer source-IP / peer_id
/// selection: each outbound dial (PEX/DHT/add_peers) is hashed onto one binding
/// (deterministic by peer addr), and the resulting binding's listen_addr is
/// used to source-bind the TCP socket. With multi-tunnel WG, this spreads
/// outbound dials across N tunnels while keeping each peer pinned to one
/// binding (so the peer always sees the same peer_id from us). Single-binding
/// (legacy) collapses to a single (peer_id, source_ip).
pub fn start_announce_loop(
    torrent_mgr: Arc<TorrentManager>,
    disk_mgr: Arc<DiskManager>,
    bindings: Vec<crate::config::ResolvedBinding>,
    utp_socket: Option<Arc<UtpSocketUdp>>,
    disable_announce: bool,
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
        tokio::spawn(async move {
            while let Some((addr, t)) = dial_rx.recv().await {
                let d = disk_c.clone();
                let u = utp_c.clone();
                let b = pick_binding_for_dial(&bindings_c, addr);
                tokio::spawn(async move {
                    dial_peer(addr, t, d, b.peer_id, u, b.addr.port(), b.fwmark).await;
                });
            }
        });
    }

    if disable_announce {
        info!("[tracker] internal announce loop disabled (Go owns announces)");
        return;
    }

    // Legacy internal announce loop uses bindings[0] — single peer_id+port.
    // Reaching this code requires disable_internal_announce=false in config,
    // which the Hydra Go orchestrator no longer sets.
    let legacy_pid = bindings.first().map(|b| b.peer_id).unwrap_or([0u8; 20]);
    let legacy_port = bindings.first().map(|b| b.addr.port()).unwrap_or(0);
    let peer_id = legacy_pid;
    let listen_port = legacy_port;
    let mgr = torrent_mgr.clone();
    let disk = disk_mgr.clone();
    tokio::spawn(async move {
        info!("[tracker] announce loop started, waiting 5s...");
        time::sleep(Duration::from_secs(5)).await;
        info!("[tracker] announce loop active");

        let mut announced: std::collections::HashSet<[u8; 20]> = std::collections::HashSet::new();
        // Stagger: max 100 new announces per 10s cycle (~10/sec = 13k torrents in ~22 min)
        // Stagger: 500/cycle = 50/sec. La Cale benched OK to 100/s.
        // 13k torrents in ~4.5 min.
        const MAX_NEW_PER_CYCLE: usize = 500;

        loop {
            let all = mgr.all();
            let mut new_this_cycle = 0;
            for t in &all {
                if announced.contains(&t.info_hash) {
                    continue;
                }
                let status = t.status.load(std::sync::atomic::Ordering::Relaxed);
                if status != TorrentStatus::Seeding as u8
                    && status != TorrentStatus::Downloading as u8
                {
                    continue;
                }
                if t.is_paused.load(std::sync::atomic::Ordering::Relaxed) {
                    continue;
                }
                if new_this_cycle >= MAX_NEW_PER_CYCLE {
                    break; // throttle, pick up remaining next cycle
                }

                let ih_hex = crate::torrent::hex_encode(&t.info_hash);
                if announced.is_empty() || new_this_cycle == 0 {
                    info!("[tracker] announcing {} new torrents this cycle (total: {}/{})",
                        all.iter().filter(|t| !announced.contains(&t.info_hash)).count().min(MAX_NEW_PER_CYCLE),
                        announced.len(), all.len());
                }
                announced.insert(t.info_hash);
                new_this_cycle += 1;
                let torrent = t.clone();
                let pid = peer_id;
                let port = listen_port;
                let d = disk.clone();
                let utp = utp_socket.clone();
                tokio::spawn(async move {
                    announce_torrent(torrent, d, pid, port, utp).await;
                });
            }
            time::sleep(Duration::from_secs(10)).await;
        }
    });
}

async fn announce_torrent(torrent: Arc<TorrentState>, disk: Arc<DiskManager>, peer_id: [u8; 20], port: u16, utp_socket: Option<Arc<UtpSocketUdp>>) {
    let ih_hex = crate::torrent::hex_encode(&torrent.info_hash);
    let short_hash = &ih_hex[..8];

    // Initial announce
    let mut interval = 1800u64; // default 30 min
    let mut first_announce = true;

    loop {
        let uploaded = torrent.total_uploaded.load(std::sync::atomic::Ordering::Relaxed);
        let downloaded = torrent.total_downloaded.load(std::sync::atomic::Ordering::Relaxed);
        let is_seeding = torrent.status.load(std::sync::atomic::Ordering::Relaxed)
            == TorrentStatus::Seeding as u8;
        // Seeders must announce left=0 or the tracker marks them as leechers and
        // distributes their IP only to other seeders (who ignore it). Imported
        // torrents have downloaded=0 so we can't derive left from that.
        let left = if is_seeding {
            0
        } else {
            torrent.meta.total_size.saturating_sub(downloaded)
        };
        // "started" only on the first announce. Later announces use "" (periodic).
        let event = if first_announce { "started" } else { "" };

        // Try each tracker tier
        let mut announced = false;
        for tier in &torrent.meta.trackers {
            for url in tier {
                if !url.starts_with("http://") && !url.starts_with("https://") {
                    continue; // Skip UDP trackers for now
                }
                match http::announce(
                    url,
                    &torrent.info_hash,
                    &peer_id,
                    port,
                    uploaded,
                    downloaded,
                    left,
                    event,
                ).await {
                    Ok(resp) => {
                        if resp.interval > 0 {
                            interval = resp.interval as u64;
                        }
                        // Store scrape data + current tracker + announce OK
                        torrent.scrape_seeders.store(resp.complete, std::sync::atomic::Ordering::Relaxed);
                        torrent.scrape_leechers.store(resp.incomplete, std::sync::atomic::Ordering::Relaxed);
                        torrent.last_announce_ok.store(true, std::sync::atomic::Ordering::Relaxed);
                        if let Ok(mut ct) = torrent.current_tracker.lock() {
                            *ct = url.clone();
                        }
                        if let Ok(mut err) = torrent.last_announce_error.lock() {
                            err.clear();
                        }
                        if !resp.peers.is_empty() {
                            info!("[tracker] {} got {} peers from {}", short_hash, resp.peers.len(), url);
                            // Connect to each peer
                            for addr in &resp.peers {
                                let t = torrent.clone();
                                let d = disk.clone();
                                let a = *addr;
                                let pid = peer_id;
                                let utp = utp_socket.clone();
                                let lp = port;
                                tokio::spawn(async move {
                                    // Legacy internal-announce dial path: no
                                    // fwmark (single-binding caller).
                                    dial_peer(a, t, d, pid, utp, lp, 0).await;
                                });
                            }
                        }
                        announced = true;
                        break; // Success on this tier, move on
                    }
                    Err(e) => {
                        warn!("[tracker] {} announce to {} failed: {}", short_hash, url, e);
                        if let Ok(mut err) = torrent.last_announce_error.lock() {
                            *err = e.clone();
                        }
                    }
                }
            }
            if announced { break; }
        }

        if !announced {
            warn!("[tracker] {} no tracker responded", short_hash);
        } else {
            first_announce = false;
        }

        // Peer deficit re-announce: if the tracker still reports leechers but we have
        // zero peers connected, the default 30-min interval leaves us invisible.
        // Shorten to 5 min (respecting BT client etiquette ~= qBittorrent behavior).
        let peers_now = torrent.peers_connected.load(std::sync::atomic::Ordering::Relaxed);
        let leechers_scrape = torrent.scrape_leechers.load(std::sync::atomic::Ordering::Relaxed);
        let effective_interval = if leechers_scrape > 0 && peers_now == 0 {
            interval.min(300)
        } else {
            interval
        };
        // Wait for next announce
        time::sleep(Duration::from_secs(effective_interval.max(60))).await;

        // Check if torrent is still active
        if torrent.is_paused.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
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

pub async fn dial_peer(
    addr: std::net::SocketAddr,
    torrent: Arc<TorrentState>,
    disk: Arc<DiskManager>,
    peer_id: [u8; 20],
    utp_socket: Option<Arc<UtpSocketUdp>>,
    listen_port: u16,
    source_fwmark: u32,
) {
    use tokio::net::TcpStream;
    use tokio::net::TcpSocket;
    use tokio_util::codec::Framed;
    use crate::wire::codec::BtCodec;
    use crate::peer::transport::PeerTransport;
    use crate::crypto::stream::CryptoStream;

    // Skip dials to our own public IPs on our own port — tracker can hand us
    // back our VPS egress IP as a "peer" and we'd handshake ourselves through
    // haproxy. Cross-engine (hoard <-> race) dials through the public IP stay
    // allowed because the port differs.
    if is_self_dial(addr, listen_port) {
        DIAL_SKIPPED_SELF.fetch_add(1, AtomicOrdering::Relaxed);
        return;
    }
    // Skip if we're already connected to this peer on this torrent.
    if torrent.connected_addrs.contains(&addr) {
        DIAL_SKIPPED_CONNECTED.fetch_add(1, AtomicOrdering::Relaxed);
        return;
    }
    // Skip if another dial_peer task is already in flight for (info_hash, addr).
    let inflight_key = (torrent.info_hash, addr);
    if !dial_inflight().insert(inflight_key) {
        DIAL_SKIPPED_INFLIGHT.fetch_add(1, AtomicOrdering::Relaxed);
        return;
    }
    let _dial_guard = DialInflightGuard(inflight_key);

    DIAL_ATTEMPTED.fetch_add(1, AtomicOrdering::Relaxed);

    // Try TCP first, fall back to uTP if it fails (NAT-bound peers).
    async fn try_tcp(
        addr: std::net::SocketAddr,
        source_fwmark: u32,
    ) -> Option<PeerTransport> {
        use crate::peer::SOCKS5_OUTBOUND;
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
            if source_fwmark != 0 {
                let socket = if addr.is_ipv4() {
                    TcpSocket::new_v4().ok()?
                } else {
                    TcpSocket::new_v6().ok()?
                };
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

    // Try the (transport, handshake-pair) combos in order: TCP-MSE, TCP-plain, uTP-MSE, uTP-plain.
    // Each combo returns the full handshake result so the session gets the remote peer_id.
    async fn open(
        addr: std::net::SocketAddr,
        utp_socket: &Option<Arc<UtpSocketUdp>>,
        info_hash: &[u8; 20],
        peer_id: &[u8; 20],
        source_fwmark: u32,
    ) -> Option<(CryptoStream, bool, bool, [u8; 20], bool)> {
        // 2.7.11: PLAINTEXT-FIRST outbound dial. Most peers only *prefer* MSE
        // (they accept plaintext too), so dialing plain first avoids the RC4
        // per-byte cost — measured single-conn: plain 619 MB/s vs MSE 280 MB/s.
        // MSE is kept as a FALLBACK for the minority that *require* encryption,
        // so connectivity is never lost. TYPHON_NO_MSE=1 also skips the MSE
        // fallback (pure-plaintext bench).
        let skip_mse = std::env::var("TYPHON_NO_MSE").map(|v| v == "1").unwrap_or(false);
        // TCP plaintext (preferred)
        if let Some(mut t) = try_tcp(addr, source_fwmark).await {
            if let Ok(hs) = crate::peer::handshake::outgoing(&mut t, info_hash, peer_id).await {
                return Some((CryptoStream::plain(t), hs.fast_extension, hs.extended_protocol, hs.peer_id, false));
            }
        }
        // TCP MSE (fallback for require-encryption peers)
        if !skip_mse {
            if let Some(mut t) = try_tcp(addr, source_fwmark).await {
                if let Ok((enc, dec, hs)) = crate::crypto::mse::handshake_outgoing(&mut t, info_hash, peer_id).await {
                    return Some((CryptoStream::new(t, Some(enc), Some(dec)), hs.fast_extension, hs.extended_protocol, hs.peer_id, true));
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

    let (cs, fast_ext, lt_ext, remote_peer_id, is_encrypted) = match open(addr, &utp_socket, &torrent.info_hash, &peer_id, source_fwmark).await {
        Some(v) => { DIAL_HANDSHAKE_OK.fetch_add(1, AtomicOrdering::Relaxed); v }
        None => { DIAL_HANDSHAKE_FAIL.fetch_add(1, AtomicOrdering::Relaxed); return; }
    };
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
