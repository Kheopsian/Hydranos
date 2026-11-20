//! Transport abstraction over TCP and uTP (BEP 29).
//!
//! All peer I/O goes through this enum so MSE/handshake/codec stay transport-agnostic.
//! TCP-specific tuning (e.g. set_nodelay) is applied before wrapping into the enum.

use librqbit_utp::UtpStream;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

pub enum PeerTransport {
    Tcp(TcpStream),
    Utp(UtpStream),
}

impl PeerTransport {
    pub fn kind(&self) -> &'static str {
        match self {
            PeerTransport::Tcp(_) => "tcp",
            PeerTransport::Utp(_) => "utp",
        }
    }
}

impl AsyncRead for PeerTransport {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            PeerTransport::Tcp(s) => Pin::new(s).poll_read(cx, buf),
            PeerTransport::Utp(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for PeerTransport {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            PeerTransport::Tcp(s) => Pin::new(s).poll_write(cx, buf),
            PeerTransport::Utp(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            PeerTransport::Tcp(s) => Pin::new(s).poll_flush(cx),
            PeerTransport::Utp(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            PeerTransport::Tcp(s) => Pin::new(s).poll_shutdown(cx),
            PeerTransport::Utp(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Two connected TCP transports over loopback.
    async fn tcp_pair() -> (PeerTransport, PeerTransport) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let (c, s) = tokio::join!(TcpStream::connect(addr), l.accept());
        (
            PeerTransport::Tcp(c.unwrap()),
            PeerTransport::Tcp(s.unwrap().0),
        )
    }

    /// Two connected uTP transports over loopback.
    async fn utp_pair() -> (PeerTransport, PeerTransport) {
        let a = librqbit_utp::UtpSocketUdp::new_udp("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let b = librqbit_utp::UtpSocketUdp::new_udp("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let (c, s) = tokio::join!(a.connect(b.bind_addr()), b.accept());
        (
            PeerTransport::Utp(c.unwrap()),
            PeerTransport::Utp(s.unwrap()),
        )
    }

    /// The label the engine logs and counts by comes from the variant, not
    /// from anything else: swapping them would put every uTP peer under "tcp".
    #[tokio::test]
    async fn each_transport_names_its_own_kind() {
        let (t, _t2) = tcp_pair().await;
        assert_eq!(t.kind(), "tcp");
        let (u, _u2) = utp_pair().await;
        assert_eq!(u.kind(), "utp");
    }

    /// Bytes written on one side arrive intact on the other, both ways, and a
    /// shutdown on one side reaches the other as end-of-stream -- through
    /// whichever transport carries the peer. A variant that forwarded a write
    /// to the wrong direction, dropped a flush or turned `shutdown` into a
    /// no-op would leave the peer waiting forever.
    async fn round_trip(mut a: PeerTransport, mut b: PeerTransport) {
        let payload: Vec<u8> = (0..4000u32).map(|i| (i % 251) as u8).collect();
        a.write_all(&payload).await.unwrap();
        a.flush().await.unwrap();
        let mut got = vec![0u8; payload.len()];
        tokio::time::timeout(Duration::from_secs(5), b.read_exact(&mut got))
            .await
            .expect("the payload arrives")
            .unwrap();
        assert_eq!(got, payload);

        b.write_all(b"pong").await.unwrap();
        b.flush().await.unwrap();
        let mut back = [0u8; 4];
        tokio::time::timeout(Duration::from_secs(5), a.read_exact(&mut back))
            .await
            .expect("the reply arrives")
            .unwrap();
        assert_eq!(&back, b"pong");

        a.shutdown().await.unwrap();
        let mut rest = Vec::new();
        let n = tokio::time::timeout(Duration::from_secs(5), b.read_to_end(&mut rest))
            .await
            .expect("a shutdown reaches the peer as end-of-stream")
            .unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn a_tcp_transport_carries_bytes_both_ways_and_shuts_down() {
        let (a, b) = tcp_pair().await;
        round_trip(a, b).await;
    }

    #[tokio::test]
    async fn a_utp_transport_carries_bytes_both_ways_and_shuts_down() {
        let (a, b) = utp_pair().await;
        round_trip(a, b).await;
    }
}
