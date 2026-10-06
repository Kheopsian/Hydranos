//! Asking a VPN gateway to forward a port (NAT-PMP, RFC 6886).
//!
//! A tunnel gives a private address, so nothing reaches us until the gateway
//! maps an external port to ours. The mapping expires on purpose: the gateway
//! forgets a client that stopped renewing, so a mapping has to be refreshed
//! for as long as it is wanted.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

pub(crate) const NATPMP_PORT: u16 = 5351;
const VERSION: u8 = 0;
pub(crate) const OP_MAP_UDP: u8 = 1;
pub(crate) const OP_MAP_TCP: u8 = 2;
const TIMEOUT: Duration = Duration::from_secs(3);
/// RFC 6886 asks for exponential backoff. Four tries over about twelve seconds
/// is enough to ride out a tunnel that has just come up and is not yet passing
/// traffic -- which is exactly when the first request is made.
pub(crate) const ATTEMPTS: usize = 4;

/// What the gateway granted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mapping {
    pub internal_port: u16,
    pub external_port: u16,
    pub lifetime: Duration,
}

/// How long to wait before renewing.
///
/// Half the granted lifetime, floored at five seconds. Renewing at the
/// deadline means a window where the mapping is already gone; halving it gives
/// a second chance before anything is lost.
pub fn renew_interval(granted: Duration) -> Duration {
    let half = granted / 2;
    if half < Duration::from_secs(5) {
        Duration::from_secs(5)
    } else {
        half
    }
}

/// Build one mapping request.
pub fn request(tcp: bool, internal: u16, suggested: u16, lifetime: Duration) -> [u8; 12] {
    let mut req = [0u8; 12];
    req[0] = VERSION;
    req[1] = if tcp { OP_MAP_TCP } else { OP_MAP_UDP };
    req[4..6].copy_from_slice(&internal.to_be_bytes());
    req[6..8].copy_from_slice(&suggested.to_be_bytes());
    req[8..12].copy_from_slice(&(lifetime.as_secs() as u32).to_be_bytes());
    req
}

/// Read a gateway reply.
pub fn parse_reply(buf: &[u8]) -> Result<Mapping, String> {
    if buf.len() < 16 {
        return Err(format!("short reply: {} bytes for 16 expected", buf.len()));
    }
    let result_code = u16::from_be_bytes([buf[2], buf[3]]);
    if result_code != 0 {
        return Err(result_text(result_code).to_string());
    }
    Ok(Mapping {
        internal_port: u16::from_be_bytes([buf[8], buf[9]]),
        external_port: u16::from_be_bytes([buf[10], buf[11]]),
        lifetime: Duration::from_secs(u32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]]) as u64),
    })
}

/// The gateway's own words for a refusal, so an operator is not left with a
/// number.
pub fn result_text(code: u16) -> &'static str {
    match code {
        0 => "success",
        1 => "the gateway speaks another version of NAT-PMP",
        2 => "the gateway refuses to map for us",
        3 => "the gateway has no external address yet",
        4 => "the gateway is out of resources",
        5 => "the gateway does not support this opcode",
        _ => "unknown result code",
    }
}

/// Ask a gateway for one mapping, retrying on silence.
///
/// The port is a constant in the protocol and the retry count is a constant in
/// the spec, which between them made this function unreachable from a test: a
/// gateway on 5351 is not something a test may assume. Both are parameters
/// here and constants at the call sites.
///
/// `device` pins the request to a tunnel. A tunnel's gateway (Proton's
/// 10.2.0.1) is only reachable through that tunnel: unpinned, the request
/// would follow the host's default route and ask the home router instead.
pub async fn map_to(
    target: SocketAddr,
    tcp: bool,
    internal: u16,
    suggested: u16,
    lifetime: Duration,
    attempts: usize,
    device: Option<&str>,
) -> Result<Mapping, String> {
    let socket = tokio::net::UdpSocket::bind(("0.0.0.0", 0))
        .await
        .map_err(|e| format!("cannot open a socket to ask: {e}"))?;
    if let Some(dev) = device {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        socket
            .bind_device(Some(dev.as_bytes()))
            .map_err(|e| format!("cannot pin the request to {dev}: {e}"))?;
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        return Err(format!("cannot pin the request to {dev}: not supported on this platform"));
    }
    let req = request(tcp, internal, suggested, lifetime);

    let mut last = String::from("no attempt made");
    for _ in 0..attempts {
        if let Err(e) = socket.send_to(&req, target).await {
            last = format!("cannot send: {e}");
            continue;
        }
        let mut buf = [0u8; 16];
        match tokio::time::timeout(TIMEOUT, socket.recv_from(&mut buf)).await {
            Ok(Ok((n, _))) => return parse_reply(&buf[..n]),
            Ok(Err(e)) => last = format!("cannot read: {e}"),
            Err(_) => last = format!("no answer from {target} in {}s", TIMEOUT.as_secs()),
        }
    }
    Err(last)
}

/// NAT-PMP leases asked of a VPN gateway. Proton grants sixty seconds and
/// nothing announces the expiry: the port simply stops answering while the
/// engine keeps advertising it. Renewed at half (`renew_interval`).
pub const VPN_LEASE: Duration = Duration::from_secs(60);
/// After a refusal or a silence. Short: until a port is known the engine's
/// announces are held.
const VPN_RETRY: Duration = Duration::from_secs(15);

/// Map one port, TCP and UDP, through a tunnel. The TCP grant decides the
/// port; UDP is asked for the same one and its failure only costs uTP.
///
/// Both, because forwarding only TCP works well enough to look correct and
/// quietly loses every uTP peer.
pub async fn map_both(
    gateway: IpAddr,
    device: &str,
    internal: u16,
    suggested: u16,
    attempts: usize,
) -> Result<(Mapping, Option<String>), String> {
    let target = SocketAddr::new(gateway, NATPMP_PORT);
    let tcp = map_to(target, true, internal, suggested, VPN_LEASE, attempts, Some(device)).await?;
    let udp = map_to(target, false, internal, tcp.external_port, VPN_LEASE, attempts, Some(device))
        .await
        .err()
        .map(|e| format!("UDP not forwarded, uTP peers cannot reach this engine: {e}"));
    Ok((tcp, udp))
}

/// The `(internal, suggested)` ports of a renewal.
///
/// ⚠ The internal port is the mapping's KEY, so it never moves: always the
/// configured port, the one the boot request used. It used to be the port the
/// listener held -- which, after the first grant, IS the external port. Proton
/// keys a mapping on its internal port, so every renewal (each 30 s) asked for
/// a new mapping, got a new port, moved the listener there and asked again:
/// measured on 2026-10-06, 45133 -> 37956 -> 50418 -> 46869, a tracker always
/// holding a dead port, and the orphaned mappings ran the gateway out of them
/// (result code 4). Only the suggestion follows what was granted.
pub fn renewal_ports(configured: u16, applied: u16) -> (u16, u16) {
    (configured, if applied != 0 { applied } else { configured })
}

/// What the follower reports, for the tunnel's status line.
pub trait PortSink: Send + Sync + 'static {
    fn forwarded(&self, port: u16);
    fn failed(&self, error: String);
}

/// Follow a tunnel's forwarded port for the life of the process.
///
/// The engine's announces are held (`set_port_pending`) until the first port
/// is known, as with gluetun: the configured port is a guess, and a tracker
/// told a guess hands it to every peer for a whole interval. The listener
/// stays on the configured (internal) port; trackers are told the external
/// port the gateway granted (`set_external_port`), and a new one at a renewal.
///
/// `initial` is a grant obtained before the engine started, so it was born on
/// the right port; it is applied first, then renewed.
pub fn spawn_follower(
    engine: String,
    manager: std::sync::Arc<typhon_engine::torrent::TorrentManager>,
    gateway: IpAddr,
    device: String,
    configured_port: u16,
    initial: Option<Mapping>,
    sink: std::sync::Arc<dyn PortSink>,
) {
    manager.set_port_pending(true);
    tracing::info!(engine = %engine, %gateway, device = %device, "wireguard port forward: announces held until the gateway gives a port");
    tokio::spawn(async move {
        let mut applied: u16 = 0;
        let mut pending = initial;
        let mut last_error = String::new();
        loop {
            let granted = match pending.take() {
                Some(m) => Ok((m, None)),
                None => {
                    let (internal, suggested) = renewal_ports(configured_port, applied);
                    map_both(gateway, &device, internal, suggested, ATTEMPTS).await
                }
            };
            let wait = match granted {
                Ok((m, udp_err)) => {
                    if let Some(e) = udp_err {
                        tracing::warn!(engine = %engine, "wireguard port forward: {e}");
                    }
                    if m.external_port != applied {
                        // ⚠ The listener STAYS on the internal port. NAT-PMP
                        // translates (RFC 6886): the gateway sends public
                        // `external_port` to our `internal` port, and Proton
                        // does exactly that -- measured on 2026-10-06, a SYN to
                        // public 45133 arrived on 10.2.0.2:16171. Moving the
                        // listener to the external number (what this did)
                        // left the forwarded port leading nowhere. Only the
                        // ANNOUNCED port changes.
                        tracing::info!(engine = %engine, from = applied, to = m.external_port,
                            listening_on = m.internal_port,
                            "wireguard port forward: announcing the forwarded port");
                        applied = m.external_port;
                        manager.set_external_port(applied);
                        manager.set_port_pending(false);
                        sink.forwarded(applied);
                    }
                    last_error.clear();
                    renew_interval(m.lifetime)
                }
                Err(e) => {
                    // Logged when it changes: a gateway down for an hour is
                    // one line, not 240.
                    if e != last_error {
                        tracing::warn!(engine = %engine, error = %e, holding = manager.port_pending(), "wireguard port forward: no port from the gateway");
                        sink.failed(e.clone());
                        last_error = e;
                    }
                    VPN_RETRY
                }
            };
            tokio::time::sleep(wait).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_says_which_ports_and_for_how_long() {
        let r = request(true, 16171, 16171, Duration::from_secs(3600));
        assert_eq!(r[0], VERSION);
        assert_eq!(r[1], OP_MAP_TCP, "TCP, not UDP");
        assert_eq!(u16::from_be_bytes([r[4], r[5]]), 16171);
        assert_eq!(u32::from_be_bytes([r[8], r[9], r[10], r[11]]), 3600);
        assert_eq!(request(false, 1, 1, Duration::ZERO)[1], OP_MAP_UDP);
    }

    #[test]
    fn a_refusal_is_read_as_a_refusal_not_a_mapping() {
        let mut buf = [0u8; 16];
        buf[2..4].copy_from_slice(&3u16.to_be_bytes());
        let err = parse_reply(&buf).unwrap_err();
        assert!(err.contains("external address"), "{err}");
        // A truncated reply is an error too: reading ports out of it would
        // invent an external port nobody granted.
        assert!(parse_reply(&[0u8; 8]).is_err());
    }

    #[test]
    fn a_grant_is_read_back_whole() {
        let mut buf = [0u8; 16];
        buf[8..10].copy_from_slice(&16171u16.to_be_bytes());
        buf[10..12].copy_from_slice(&50000u16.to_be_bytes());
        buf[12..16].copy_from_slice(&7200u32.to_be_bytes());
        let m = parse_reply(&buf).unwrap();
        assert_eq!(m.external_port, 50000);
        assert_eq!(m.lifetime, Duration::from_secs(7200));
    }

    /// Renewing at the deadline leaves a window where the mapping is already
    /// gone and we do not know it.
    #[test]
    fn renewal_happens_halfway_through() {
        assert_eq!(renew_interval(Duration::from_secs(3600)), Duration::from_secs(1800));
        assert_eq!(renew_interval(Duration::from_secs(4)), Duration::from_secs(5));
        assert_eq!(renew_interval(Duration::ZERO), Duration::from_secs(5));
    }
}

#[cfg(test)]
mod io_tests {
    use super::*;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
    }

    /// A gateway that answers one request with `reply`, then stops.
    fn gateway(reply: Vec<u8>) -> (SocketAddr, std::sync::mpsc::Receiver<Vec<u8>>) {
        let sock = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind");
        let addr = sock.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 64];
            if let Ok((n, from)) = sock.recv_from(&mut buf) {
                let _ = tx.send(buf[..n].to_vec());
                let _ = sock.send_to(&reply, from);
            }
        });
        (addr, rx)
    }

    /// The request really goes out and the grant really comes back. Both halves
    /// were covered as pure functions; nothing had ever put one on a socket.
    #[test]
    fn a_granted_mapping_is_asked_for_and_read_back() {
        let mut reply = [0u8; 16];
        reply[8..10].copy_from_slice(&16171u16.to_be_bytes());
        reply[10..12].copy_from_slice(&50000u16.to_be_bytes());
        reply[12..16].copy_from_slice(&7200u32.to_be_bytes());
        let (addr, rx) = gateway(reply.to_vec());

        let m = rt()
            .block_on(map_to(addr, true, 16171, 16171, Duration::from_secs(7200), 1, None))
            .expect("the gateway granted it");
        assert_eq!(m.external_port, 50000);
        assert_eq!(m.lifetime, Duration::from_secs(7200));

        let sent = rx.recv_timeout(std::time::Duration::from_secs(5)).expect("a request arrived");
        assert_eq!(sent.len(), 12, "NAT-PMP asks in twelve bytes");
        assert_eq!(sent[1], OP_MAP_TCP);
        assert_eq!(u16::from_be_bytes([sent[4], sent[5]]), 16171);
    }

    /// A refusal is the gateway's answer, not a timeout. Retrying through it
    /// would hammer a gateway that already said no.
    #[test]
    fn a_refusal_comes_back_as_the_gateways_reason() {
        let mut reply = [0u8; 16];
        reply[2..4].copy_from_slice(&2u16.to_be_bytes()); // "refuses to map for us"
        let (addr, _rx) = gateway(reply.to_vec());

        let err = rt()
            .block_on(map_to(addr, true, 16171, 16171, Duration::from_secs(3600), 1, None))
            .expect_err("code 2 is a refusal");
        assert!(err.contains("refuses to map"), "{err}");
    }

    /// Silence is the ordinary case -- most networks have no NAT-PMP at all --
    /// and it has to end rather than wait.
    #[test]
    fn a_gateway_that_never_answers_gives_up() {
        let sock = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind");
        let addr = sock.local_addr().unwrap();
        drop(sock);

        let err = rt()
            .block_on(map_to(addr, true, 16171, 16171, Duration::from_secs(3600), 1, None))
            .expect_err("nobody answered");
        assert!(!err.is_empty(), "the error says something");
    }
}

#[cfg(test)]
mod follower_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A gateway answering every request with the next port in `ports`.
    async fn gateway(ports: Vec<u16>) -> (SocketAddr, Arc<Mutex<Vec<u8>>>) {
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = sock.local_addr().unwrap();
        let ops: Arc<Mutex<Vec<u8>>> = Default::default();
        let seen = ops.clone();
        tokio::spawn(async move {
            let mut i = 0;
            let mut buf = [0u8; 64];
            while let Ok((n, from)) = sock.recv_from(&mut buf).await {
                if n < 12 {
                    continue;
                }
                seen.lock().unwrap().push(buf[1]);
                let mut reply = [0u8; 16];
                reply[1] = 128 + buf[1];
                reply[8..10].copy_from_slice(&buf[4..6]);
                let port = ports[i.min(ports.len() - 1)];
                // TCP and UDP of one round get the same port.
                if buf[1] == OP_MAP_UDP {
                    i += 1;
                }
                reply[10..12].copy_from_slice(&port.to_be_bytes());
                reply[12..16].copy_from_slice(&60u32.to_be_bytes());
                let _ = sock.send_to(&reply, from).await;
            }
        });
        (addr, ops)
    }

    /// ⭐ A renewal asks for the SAME mapping: the internal port is the one the
    /// boot request used, never the port the listener moved to.
    #[test]
    fn a_renewal_keeps_the_mapping_key_and_suggests_the_granted_port() {
        assert_eq!(renewal_ports(16171, 0), (16171, 16171), "first request");
        assert_eq!(renewal_ports(16171, 45133), (16171, 45133), "after the listener moved to 45133");
        assert_ne!(renewal_ports(16171, 45133).0, 45133, "the external port must not become the key");
    }

    /// ⭐ TCP and UDP are both asked for, the same port, and the grant is read.
    #[tokio::test]
    async fn both_protocols_are_mapped_to_one_port() {
        let (addr, ops) = gateway(vec![45243]).await;
        let tcp = map_to(addr, true, 16171, 16171, VPN_LEASE, 1, None).await.unwrap();
        let udp = map_to(addr, false, 16171, tcp.external_port, VPN_LEASE, 1, None).await.unwrap();
        assert_eq!((tcp.external_port, udp.external_port), (45243, 45243));
        assert_eq!(*ops.lock().unwrap(), vec![OP_MAP_TCP, OP_MAP_UDP]);
        assert_eq!(renew_interval(tcp.lifetime), Duration::from_secs(30), "renewed at half the lease");
    }

    /// A pin to a device that is not there fails the request: it is never
    /// sent by the default route to whatever gateway that leads to.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_request_pinned_to_a_missing_tunnel_is_not_sent() {
        let (addr, ops) = gateway(vec![1]).await;
        let err = map_to(addr, true, 1, 1, VPN_LEASE, 1, Some("hy-nodev0")).await.unwrap_err();
        assert!(err.contains("hy-nodev0"), "{err}");
        assert!(ops.lock().unwrap().is_empty());
    }
}
