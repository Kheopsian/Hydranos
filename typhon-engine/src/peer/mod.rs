pub mod bencode;
pub mod extension;
pub mod choking;
pub mod download;
pub mod handshake;
pub mod message;
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
        tokio::spawn(async move {
            handle_incoming(PeerTransport::Tcp(stream), addr, tm, dm, peer_id, u, listen_port).await;
        });
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
