//! Getting the listen port forwarded, by whatever the network speaks.
//!
//! Two protocols exist and equipment answers one or the other, almost never
//! both: a VPN gateway answers NAT-PMP (`portfwd`), a home router answers UPnP
//! IGD (`igd`). Until now the first was implemented and never called -- `mod
//! portfwd;` with no call site anywhere -- and the second did not exist. So no
//! installation ever obtained a mapping automatically, while the interface told
//! Proton users their port was "obtained by NAT-PMP and renewed continuously".
//!
//! This is what makes that true. It only ever ADDS a mapping; the listen port
//! is never changed underneath the engines, because the tracker has already
//! been told which port we are on.

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

/// Which way the port was obtained, for the log and for the interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mapped {
    NatPmp { external_port: u16 },
    Upnp { external_port: u16 },
}

/// Try NAT-PMP, then UPnP, and say which worked.
///
/// NAT-PMP first because it is cheap -- one datagram to a known address -- and
/// because the equipment that speaks it, a VPN gateway, is also the case where
/// UPnP cannot work at all: there is no router on the tunnel to discover.
pub async fn map_once(internal_port: u16) -> Result<Mapped, String> {
    let mut why = Vec::new();

    if let Some(gw) = default_gateway() {
        match portfwd::map(IpAddr::V4(gw), true, internal_port, internal_port, LEASE).await {
            Ok(m) => return Ok(Mapped::NatPmp { external_port: m.external_port }),
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
        Ok(gateway) => {
            match igd::add_mapping(&gateway, &local.to_string(), internal_port, internal_port, LEASE)
                .await
            {
                // IGD has no way to grant a different external port: we asked
                // for ours and it either agreed or refused.
                Ok(()) => return Ok(Mapped::Upnp { external_port: internal_port }),
                Err(e) => why.push(format!("UPnP: {e}")),
            }
        }
        Err(e) => why.push(format!("UPnP: {e}")),
    }

    Err(why.join(" | "))
}

/// Keep the port mapped for as long as the process runs.
pub fn spawn(internal_port: u16) {
    tokio::spawn(async move {
        loop {
            match map_once(internal_port).await {
                Ok(Mapped::NatPmp { external_port }) => {
                    tracing::info!(external_port, internal_port, "port forwarded by NAT-PMP");
                    tokio::time::sleep(portfwd::renew_interval(LEASE)).await;
                }
                Ok(Mapped::Upnp { external_port }) => {
                    tracing::info!(external_port, internal_port, "port forwarded by UPnP");
                    tokio::time::sleep(portfwd::renew_interval(LEASE)).await;
                }
                Err(e) => {
                    // Not an error the operator has to act on: plenty of
                    // networks have neither protocol, and a manual forward is
                    // perfectly normal. Said once every quarter hour, not
                    // every few seconds.
                    tracing::info!(
                        reason = %e,
                        "no automatic port forward; if you are not reachable, forward {} by hand",
                        internal_port
                    );
                    tokio::time::sleep(RETRY).await;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn an_empty_table_yields_nothing() {
        assert_eq!(parse_default_gateway("Iface\tDestination\tGateway\n"), None);
        assert_eq!(parse_default_gateway(""), None);
    }
}
