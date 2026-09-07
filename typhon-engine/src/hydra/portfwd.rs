//! Asking a VPN gateway to forward a port (NAT-PMP, RFC 6886).
//!
//! A tunnel gives a private address, so nothing reaches us until the gateway
//! maps an external port to ours. The mapping expires on purpose: the gateway
//! forgets a client that stopped renewing, so a mapping has to be refreshed
//! for as long as it is wanted.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

const NATPMP_PORT: u16 = 5351;
const VERSION: u8 = 0;
const OP_MAP_UDP: u8 = 1;
const OP_MAP_TCP: u8 = 2;
const TIMEOUT: Duration = Duration::from_secs(3);
/// RFC 6886 asks for exponential backoff. Four tries over about twelve seconds
/// is enough to ride out a tunnel that has just come up and is not yet passing
/// traffic -- which is exactly when the first request is made.
const ATTEMPTS: usize = 4;

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

/// Ask once, retrying on silence.
pub async fn map(
    gateway: IpAddr,
    tcp: bool,
    internal: u16,
    suggested: u16,
    lifetime: Duration,
) -> Result<Mapping, String> {
    let socket = tokio::net::UdpSocket::bind(("0.0.0.0", 0))
        .await
        .map_err(|e| format!("cannot open a socket to ask: {e}"))?;
    let target = SocketAddr::new(gateway, NATPMP_PORT);
    let req = request(tcp, internal, suggested, lifetime);

    let mut last = String::from("no attempt made");
    for _ in 0..ATTEMPTS {
        if let Err(e) = socket.send_to(&req, target).await {
            last = format!("cannot send: {e}");
            continue;
        }
        let mut buf = [0u8; 16];
        match tokio::time::timeout(TIMEOUT, socket.recv_from(&mut buf)).await {
            Ok(Ok((n, _))) => return parse_reply(&buf[..n]),
            Ok(Err(e)) => last = format!("cannot read: {e}"),
            Err(_) => last = format!("no answer from {gateway} in {}s", TIMEOUT.as_secs()),
        }
    }
    Err(last)
}

/// Keep a mapping alive for as long as the process runs.
pub fn spawn_renewal(gateway: IpAddr, internal: u16, lifetime: Duration) {
    tokio::spawn(async move {
        let mut suggested = internal;
        loop {
            match map(gateway, true, internal, suggested, lifetime).await {
                Ok(m) => {
                    tracing::info!(
                        external = m.external_port,
                        internal = m.internal_port,
                        lifetime_s = m.lifetime.as_secs(),
                        "port forwarded"
                    );
                    // Ask for the same one next time: a gateway usually grants
                    // it, and a stable external port is what the tracker was
                    // told about.
                    suggested = m.external_port;
                    tokio::time::sleep(renew_interval(m.lifetime)).await;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "port forwarding refused, retrying");
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
            }
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
