//! Transport abstraction over TCP and uTP (BEP 29).
//!
//! All peer I/O goes through this enum so MSE/handshake/codec stay transport-agnostic.
//! TCP-specific tuning (e.g. set_nodelay) is applied before wrapping into the enum.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use librqbit_utp::UtpStream;

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
