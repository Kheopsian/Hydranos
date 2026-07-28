//! PROXY protocol v2 parser.
//!
//! Format (ref: https://www.haproxy.org/download/1.8/doc/proxy-protocol.txt):
//!   bytes 0..12  : signature   0D 0A 0D 0A 00 0D 0A 51 55 49 54 0A
//!   byte  12     : ver/cmd     (ver=2 upper nibble, cmd=PROXY=1 lower nibble; 0=LOCAL)
//!   byte  13     : fam/proto   (fam: INET=1, INET6=2 ; proto: STREAM=1, DGRAM=2)
//!   bytes 14..16 : len (BE)    remaining header bytes
//!   bytes 16..   : payload (INET: 12B, INET6: 36B)

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use tokio::io::{AsyncRead, AsyncReadExt};

const SIG: [u8; 12] = [
    0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A,
];

/// Parse a PROXY v2 header from the start of an async stream.
/// Returns the real peer SocketAddr on success. Consumes exactly the header bytes
/// from the stream; subsequent reads start at the BT handshake.
pub async fn parse_v2<R: AsyncRead + Unpin>(mut reader: R) -> io::Result<SocketAddr> {
    let mut hdr = [0u8; 16];
    reader.read_exact(&mut hdr).await?;
    if hdr[0..12] != SIG {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "PROXY v2: bad signature",
        ));
    }
    let version = (hdr[12] & 0xF0) >> 4;
    let cmd = hdr[12] & 0x0F;
    if version != 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("PROXY v2: wrong version {}", version),
        ));
    }
    let fam = (hdr[13] & 0xF0) >> 4;
    let proto = hdr[13] & 0x0F;
    let len = u16::from_be_bytes([hdr[14], hdr[15]]) as usize;

    // Upper bound : INET6+STREAM = 36 bytes, plus TLVs can extend.
    // Guard against malicious huge len.
    if len > 536 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("PROXY v2: header too large ({})", len),
        ));
    }

    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).await?;

    if cmd == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "PROXY v2: LOCAL command not supported",
        ));
    }

    match (fam, proto) {
        (1, 1) if len >= 12 => {
            let src = Ipv4Addr::new(payload[0], payload[1], payload[2], payload[3]);
            let src_port = u16::from_be_bytes([payload[8], payload[9]]);
            Ok(SocketAddr::new(IpAddr::V4(src), src_port))
        }
        (2, 1) if len >= 36 => {
            let mut src6 = [0u8; 16];
            src6.copy_from_slice(&payload[0..16]);
            let src_port = u16::from_be_bytes([payload[32], payload[33]]);
            Ok(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(src6)), src_port))
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("PROXY v2: unsupported fam={} proto={} len={}", fam, proto, len),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[tokio::test]
    async fn parses_inet_stream() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&SIG);
        buf.push(0x21); // ver=2 cmd=PROXY
        buf.push(0x11); // fam=INET proto=STREAM
        buf.extend_from_slice(&12u16.to_be_bytes());
        buf.extend_from_slice(&[1, 2, 3, 4]); // src
        buf.extend_from_slice(&[5, 6, 7, 8]); // dst
        buf.extend_from_slice(&50000u16.to_be_bytes()); // src port
        buf.extend_from_slice(&16172u16.to_be_bytes()); // dst port
        let addr = parse_v2(Cursor::new(buf)).await.unwrap();
        assert_eq!(addr.to_string(), "1.2.3.4:50000");
    }

    #[tokio::test]
    async fn parses_inet6_stream() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&SIG);
        buf.push(0x21);
        buf.push(0x21); // fam=INET6 proto=STREAM
        buf.extend_from_slice(&36u16.to_be_bytes());
        let src = [
            0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01,
        ];
        let dst = [0u8; 16];
        buf.extend_from_slice(&src);
        buf.extend_from_slice(&dst);
        buf.extend_from_slice(&40000u16.to_be_bytes());
        buf.extend_from_slice(&16172u16.to_be_bytes());
        let addr = parse_v2(Cursor::new(buf)).await.unwrap();
        assert_eq!(addr.to_string(), "[2001:db8::1]:40000");
    }

    #[tokio::test]
    async fn rejects_bad_sig() {
        let buf = vec![0u8; 16];
        assert!(parse_v2(Cursor::new(buf)).await.is_err());
    }
}
