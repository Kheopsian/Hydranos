//! Getting the listen port forwarded, by whatever the network speaks.
//!
//! Two protocols exist and equipment answers one or the other, almost never
//! both: a VPN gateway answers NAT-PMP (`portfwd`), a home router answers UPnP
//! IGD (`igd`). Until now the first was implemented and never called -- `mod
//! portfwd;` with no call site anywhere -- and the second did not exist. So no
//! installation ever obtained a mapping automatically, while the interface told
//! Proton users their port was "obtained by NAT-PMP and renewed continuously".
//!
//! This is what makes that true. It only ever ADDS a mapping, TCP and UDP;
//! the listen port is never changed underneath the engines, because the
//! tracker has already been told which port we are on -- it is the mapping
//! that follows the engine's port, not the other way round.

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use crate::igd;
use crate::portfwd;

/// How long a lease is asked for. Both protocols expire on purpose, so that
/// equipment forgets a client that stopped renewing instead of accumulating
/// entries nobody can account for.
const LEASE: Duration = Duration::from_secs(3600);
/// How long to wait before trying again after a total failure. Long, because a
/// network with neither protocol will never succeed and retrying hard achieves
/// nothing but noise in someone's router log.
const RETRY: Duration = Duration::from_secs(15 * 60);

/// Our address on the local network.
///
/// Found by opening a UDP socket towards a public address and asking the
/// kernel which interface it would use. Nothing is sent -- UDP `connect` only
/// sets the peer -- so this works with no network at all beyond a route
/// existing, and needs no interface enumeration.
pub fn local_ip() -> Option<IpAddr> {
    let sock = std::net::UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    sock.connect(("192.0.2.1", 9)).ok()?; // TEST-NET-1: routed nowhere
    sock.local_addr().ok().map(|a| a.ip())
}

/// The default gateway, read from the kernel's routing table.
///
/// `/proc/net/route` gives the address little-endian in hex, which is the one
/// detail worth a test: reading it big-endian yields a plausible-looking
/// address on another network entirely, and the NAT-PMP request then goes to
/// a machine that never answers.
pub fn parse_default_gateway(proc_net_route: &str) -> Option<Ipv4Addr> {
    for line in proc_net_route.lines().skip(1) {
        let mut f = line.split_whitespace();
        let _iface = f.next()?;
        let dest = f.next()?;
        let gateway = f.next()?;
        if dest != "00000000" {
            continue;
        }
        let raw = u32::from_str_radix(gateway, 16).ok()?;
        let [a, b, c, d] = raw.to_le_bytes();
        let ip = Ipv4Addr::new(a, b, c, d);
        if !ip.is_unspecified() {
            return Some(ip);
        }
    }
    None
}

pub fn default_gateway() -> Option<Ipv4Addr> {
    let text = std::fs::read_to_string("/proc/net/route").ok()?;
    parse_default_gateway(&text)
}

/// The interface the IPv4 default route leaves by: the home router's side.
pub fn parse_default_interface(proc_net_route: &str) -> Option<String> {
    proc_net_route.lines().skip(1).find_map(|line| {
        let mut f = line.split_whitespace();
        let iface = f.next()?;
        (f.next()? == "00000000").then(|| iface.to_string())
    })
}

pub fn default_interface() -> Option<String> {
    parse_default_interface(&std::fs::read_to_string("/proc/net/route").ok()?)
}

/// Which way the port was obtained, for the log and for the interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mapped {
    NatPmp { external_port: u16 },
    Upnp { external_port: u16 },
}

/// One mapping: TCP decides the port, UDP is asked for the same one. A UDP
/// refusal does not undo TCP -- it costs uTP and the DHT, not the engine --
/// and is reported on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Granted {
    pub how: Mapped,
    /// None when UDP was mapped too; else the router's reason.
    pub udp_error: Option<String>,
}

impl Granted {
    pub fn external_port(&self) -> u16 {
        match self.how {
            Mapped::NatPmp { external_port } | Mapped::Upnp { external_port } => external_port,
        }
    }
}

/// What the home-router mapper of one engine last did, for
/// `GET /api/port-forward`. Until 4.4 the log was the only place that said
/// whether a port had been forwarded, and the route answered constants.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Status {
    /// `natpmp`, `upnp`, or empty when nothing is mapped (see `error`).
    pub method: String,
    pub internal_port: u16,
    pub external_port: u16,
    pub tcp: bool,
    pub udp: bool,
    /// Why UDP is not mapped while TCP is.
    pub udp_error: String,
    /// Why nothing is mapped, after both protocols failed.
    pub error: String,
    /// Why no mapping was even asked for this engine (tunnel, proxy, pinned
    /// to another interface): the home router's port would publish the
    /// host's address.
    pub refused: String,
    /// Unix seconds of the last attempt.
    pub at: i64,
}

fn registry() -> &'static std::sync::Mutex<std::collections::BTreeMap<String, Status>> {
    static R: std::sync::OnceLock<std::sync::Mutex<std::collections::BTreeMap<String, Status>>> =
        std::sync::OnceLock::new();
    R.get_or_init(Default::default)
}

/// The last outcome for one engine, if its mapper ever ran.
pub fn status(engine: &str) -> Option<Status> {
    registry().lock().unwrap_or_else(|p| p.into_inner()).get(engine).cloned()
}

fn record(engine: &str, internal_port: u16, outcome: &Result<Granted, String>) {
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let st = match outcome {
        Ok(g) => Status {
            method: match g.how {
                Mapped::NatPmp { .. } => "natpmp".into(),
                Mapped::Upnp { .. } => "upnp".into(),
            },
            internal_port,
            external_port: g.external_port(),
            tcp: true,
            udp: g.udp_error.is_none(),
            udp_error: g.udp_error.clone().unwrap_or_default(),
            at,
            ..Default::default()
        },
        Err(e) => Status { internal_port, error: e.clone(), at, ..Default::default() },
    };
    registry().lock().unwrap_or_else(|p| p.into_inner()).insert(engine.to_string(), st);
}

/// No mapping is asked for this engine, and why.
pub fn record_refusal(engine: &str, internal_port: u16, why: &str) {
    let st = Status { internal_port, refused: why.to_string(), ..Default::default() };
    registry().lock().unwrap_or_else(|p| p.into_inner()).insert(engine.to_string(), st);
}

/// NAT-PMP, TCP then UDP on the port TCP was granted.
pub async fn natpmp_both(target: std::net::SocketAddr, internal_port: u16, attempts: usize) -> Result<Granted, String> {
    let tcp = portfwd::map_to(target, true, internal_port, internal_port, LEASE, attempts, None).await?;
    let udp = portfwd::map_to(target, false, internal_port, tcp.external_port, LEASE, attempts, None)
        .await
        .err()
        .map(|e| format!("UDP not forwarded, uTP and DHT peers cannot reach this engine: {e}"));
    Ok(Granted { how: Mapped::NatPmp { external_port: tcp.external_port }, udp_error: udp })
}

/// UPnP, TCP then UDP. IGD cannot grant another external port: we ask for
/// ours and it agrees or refuses.
pub async fn upnp_both(gateway: &igd::Gateway, local: &str, internal_port: u16) -> Result<Granted, String> {
    igd::add_mapping(gateway, local, internal_port, internal_port, "TCP", LEASE).await?;
    let udp = igd::add_mapping(gateway, local, internal_port, internal_port, "UDP", LEASE)
        .await
        .err()
        .map(|e| format!("UDP not forwarded, uTP and DHT peers cannot reach this engine: {e}"));
    Ok(Granted { how: Mapped::Upnp { external_port: internal_port }, udp_error: udp })
}

/// Try NAT-PMP, then UPnP, and say which worked.
///
/// NAT-PMP first because it is cheap -- one datagram to a known address -- and
/// because the equipment that speaks it, a VPN gateway, is also the case where
/// UPnP cannot work at all: there is no router on the tunnel to discover.
///
/// Both protocols map TCP AND UDP: until 4.4 only TCP was asked for, so uTP
/// and DHT peers could never reach an engine behind a home router.
pub async fn map_once(internal_port: u16) -> Result<Granted, String> {
    let mut why = Vec::new();

    if let Some(gw) = default_gateway() {
        let target = std::net::SocketAddr::new(IpAddr::V4(gw), portfwd::NATPMP_PORT);
        match natpmp_both(target, internal_port, portfwd::ATTEMPTS).await {
            Ok(g) => return Ok(g),
            Err(e) => why.push(format!("NAT-PMP: {e}")),
        }
    } else {
        why.push("NAT-PMP: no default gateway".to_string());
    }

    let Some(local) = local_ip() else {
        why.push("UPnP: cannot tell our own address on this network".to_string());
        return Err(why.join(" | "));
    };

    match igd::discover().await {
        Ok(gateway) => match upnp_both(&gateway, &local.to_string(), internal_port).await {
            Ok(g) => return Ok(g),
            Err(e) => why.push(format!("UPnP: {e}")),
        },
        Err(e) => why.push(format!("UPnP: {e}")),
    }

    Err(why.join(" | "))
}

/// Keep one engine's port mapped for as long as the process runs.
///
/// The port is read from the engine at each round (`announced_port`), so a
/// listen-port change made live is mapped within seconds, not at the next
/// start: until 4.4 the mapper kept renewing the port the engine had left.
/// The old port's lease is simply not renewed and expires on the router.
pub fn spawn(engine: String, manager: std::sync::Arc<typhon_engine::torrent::TorrentManager>, configured_port: u16) {
    tokio::spawn(async move {
        loop {
            let internal_port = manager.announced_port(configured_port);
            let outcome = map_once(internal_port).await;
            record(&engine, internal_port, &outcome);
            let wait = match &outcome {
                Ok(g) => {
                    let via = match g.how {
                        Mapped::NatPmp { .. } => "NAT-PMP",
                        Mapped::Upnp { .. } => "UPnP",
                    };
                    tracing::info!(engine = %engine, external_port = g.external_port(), internal_port, udp = g.udp_error.is_none(), "port forwarded by {via}");
                    if let Some(e) = &g.udp_error {
                        tracing::warn!(engine = %engine, internal_port, "{e}; forward UDP {internal_port} by hand");
                    }
                    portfwd::renew_interval(LEASE)
                }
                Err(e) => {
                    // Not an error the operator has to act on: plenty of
                    // networks have neither protocol, and a manual forward is
                    // perfectly normal. Said once every quarter hour, not
                    // every few seconds.
                    tracing::info!(
                        engine = %engine,
                        reason = %e,
                        "no automatic port forward; if you are not reachable, forward {} (TCP and UDP) by hand",
                        internal_port
                    );
                    RETRY
                }
            };
            // Sleep, but wake as soon as the engine moves to another port.
            let until = tokio::time::Instant::now() + wait;
            while tokio::time::Instant::now() < until {
                tokio::time::sleep(PORT_WATCH.min(until - tokio::time::Instant::now())).await;
                if manager.announced_port(configured_port) != internal_port {
                    break;
                }
            }
        }
    });
}

/// How often a sleeping mapper looks at the engine's port.
const PORT_WATCH: Duration = Duration::from_secs(5);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_route_names_its_interface() {
        let t = "Iface\tDestination\tGateway\tFlags\n\
                 wg7\t0000000A\t00000000\t0001\n\
                 eth0\t00000000\t0101A8C0\t0003\n";
        assert_eq!(parse_default_interface(t).as_deref(), Some("eth0"));
        assert_eq!(parse_default_interface("Iface\tDestination\n"), None);
    }

    const ROUTE: &str = "\
Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT
eth0\t00000000\t0101A8C0\t0003\t0\t0\t0\t00000000\t0\t0\t0
eth0\t0001A8C0\t00000000\t0001\t0\t0\t0\t00FFFFFF\t0\t0\t0
";

    /// The kernel writes the address little-endian. Reading it the other way
    /// round gives 192.168.99.1 as 1.99.168.192 -- a plausible address on a // leak-ok: byte order
    /// network that does not exist, so the NAT-PMP request goes nowhere and
    /// times out instead of failing.
    #[test]
    fn the_gateway_is_read_little_endian() {
        assert_eq!(
            parse_default_gateway(ROUTE),
            Some(Ipv4Addr::new(192, 168, 1, 1))
        );
    }

    /// Only the default route has a gateway worth asking. The second line here
    /// is the on-link route for the subnet, whose gateway is 0.0.0.0.
    #[test]
    fn an_on_link_route_is_not_a_gateway() {
        let only_onlink = "Iface\tDestination\tGateway\n\
                           eth0\t0001A8C0\t00000000\t0001\t0\t0\t0\t00FFFFFF\t0\t0\t0\n";
        assert_eq!(parse_default_gateway(only_onlink), None);
    }

    #[test]
    fn a_default_route_with_no_gateway_is_skipped() {
        let no_gw = "Iface\tDestination\tGateway\n\
                     eth0\t00000000\t00000000\t0003\t0\t0\t0\t00000000\t0\t0\t0\n";
        assert_eq!(parse_default_gateway(no_gw), None);
    }

    /// A NAT-PMP gateway granting `port` to every request; records the ops.
    async fn natpmp_gateway(port: u16, refuse_udp: bool) -> (std::net::SocketAddr, std::sync::Arc<std::sync::Mutex<Vec<u8>>>) {
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = sock.local_addr().unwrap();
        let ops: std::sync::Arc<std::sync::Mutex<Vec<u8>>> = Default::default();
        let seen = ops.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 64];
            while let Ok((n, from)) = sock.recv_from(&mut buf).await {
                if n < 12 {
                    continue;
                }
                seen.lock().unwrap().push(buf[1]);
                let mut reply = [0u8; 16];
                reply[1] = 128 + buf[1];
                if refuse_udp && buf[1] == portfwd::OP_MAP_UDP {
                    reply[3] = 2; // not authorized
                }
                reply[8..10].copy_from_slice(&buf[4..6]);
                reply[10..12].copy_from_slice(&port.to_be_bytes());
                reply[12..16].copy_from_slice(&3600u32.to_be_bytes());
                let _ = sock.send_to(&reply, from).await;
            }
        });
        (addr, ops)
    }

    /// ⭐ #55. The home router is asked for UDP too: 4.3 mapped TCP only, so
    /// no uTP or DHT peer could ever reach an engine behind it.
    #[tokio::test]
    async fn natpmp_maps_tcp_and_udp_on_one_port() {
        let (gw, ops) = natpmp_gateway(16171, false).await;
        let g = natpmp_both(gw, 16171, 1).await.unwrap();
        assert_eq!(g.how, Mapped::NatPmp { external_port: 16171 });
        assert_eq!(g.udp_error, None);
        assert_eq!(*ops.lock().unwrap(), vec![portfwd::OP_MAP_TCP, portfwd::OP_MAP_UDP]);
    }

    /// A UDP refusal keeps the TCP mapping and says what it costs.
    #[tokio::test]
    async fn a_udp_refusal_is_reported_without_losing_tcp() {
        let (gw, _) = natpmp_gateway(16171, true).await;
        let g = natpmp_both(gw, 16171, 1).await.unwrap();
        assert_eq!(g.external_port(), 16171);
        assert!(g.udp_error.as_deref().unwrap_or_default().contains("uTP"), "{g:?}");
        record("udp-refused", 16171, &Ok(g));
        let st = status("udp-refused").unwrap();
        assert!(st.tcp && !st.udp, "{st:?}");
        assert!(!st.udp_error.is_empty());
    }

    /// A router answering every SOAP request with 200; returns the bodies.
    fn upnp_router(n: usize) -> (String, std::sync::mpsc::Receiver<String>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for _ in 0..n {
                let Ok((mut sock, _)) = listener.accept() else { return };
                sock.set_read_timeout(Some(std::time::Duration::from_millis(300))).ok();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                while let Ok(k) = sock.read(&mut chunk) {
                    if k == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..k]);
                    if String::from_utf8_lossy(&buf).contains("</s:Envelope>") {
                        break;
                    }
                }
                let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
                let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            }
        });
        (format!("http://127.0.0.1:{port}/ctl"), rx)
    }

    /// ⭐ #55. UPnP asks for TCP, then UDP, same port both times.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn upnp_maps_tcp_and_udp() {
        let (url, rx) = upnp_router(2);
        let gw = igd::Gateway {
            control_url: url,
            service_type: "urn:schemas-upnp-org:service:WANIPConnection:1".into(),
        };
        let g = upnp_both(&gw, "192.168.99.50", 16172).await.unwrap();
        assert_eq!(g, Granted { how: Mapped::Upnp { external_port: 16172 }, udp_error: None });
        let first = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        let second = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(first.contains("<NewProtocol>TCP</NewProtocol>"), "{first}");
        assert!(second.contains("<NewProtocol>UDP</NewProtocol>"), "{second}");
        assert!(second.contains("<NewExternalPort>16172</NewExternalPort>"), "{second}");
    }

    /// What `/api/port-forward` reads: the last outcome, failure or refusal
    /// included.
    #[test]
    fn the_status_says_what_happened() {
        record("st-fail", 1234, &Err("NAT-PMP: no answer | UPnP: no router".into()));
        let st = status("st-fail").unwrap();
        assert_eq!((st.tcp, st.udp, st.internal_port), (false, false, 1234));
        assert!(st.error.contains("UPnP"));
        record_refusal("st-refused", 4321, "behind a SOCKS5 proxy");
        assert_eq!(status("st-refused").unwrap().refused, "behind a SOCKS5 proxy");
        assert!(status("never-ran").is_none());
    }

    #[test]
    fn an_empty_table_yields_nothing() {
        assert_eq!(parse_default_gateway("Iface\tDestination\tGateway\n"), None);
        assert_eq!(parse_default_gateway(""), None);
    }
}
