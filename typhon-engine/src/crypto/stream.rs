//! Transparent RC4 encryption wrapper over a peer transport.
//! Implements AsyncRead + AsyncWrite so it works with tokio Framed codec.

use std::io;
use std::pin::Pin;
use std::sync::Mutex;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::peer::transport::PeerTransport;
use super::rc4::Rc4;

/// A peer transport (TCP or uTP) wrapped with optional RC4 encryption/decryption.
///
/// Write-side note: RC4 is a stream cipher — both peers' keystreams must
/// advance by exactly the same byte count or every subsequent decryption
/// produces garbage. The naïve "encrypt buf then poll_write" pattern
/// races with congestion: when the inner socket returns `Ok(n < buf.len())`
/// or `Pending`, the cipher has already advanced for the full buf but only
/// `n` (or zero) bytes hit the wire. The next poll_write call advances
/// the cipher again — silently desyncing both sides. We saw this manifest
/// as "frame too large: 1.2 GB" decoder errors after ~3-8 MB on
/// typhon-vs-typhon loopback transfers (rqbit and other peers fall back
/// to plaintext, so the bug only triggers when both sides actually
/// negotiate MSE).
///
/// Fix: encrypt incoming buf into an internal `pending_write` buffer and
/// drain it through inner.poll_write across calls. We always tell the
/// caller their full buf was accepted (it's queued internally, encrypted
/// once), and `poll_flush` blocks until the queue empties.
pub struct CryptoStream {
    inner: PeerTransport,
    encrypt: Option<Mutex<Rc4>>,
    decrypt: Option<Mutex<Rc4>>,
    pending_write: Vec<u8>,
}

impl CryptoStream {
    /// Wrap a transport with RC4 encryption. Pass None for plaintext.
    pub fn new(stream: PeerTransport, encrypt: Option<Rc4>, decrypt: Option<Rc4>) -> Self {
        Self {
            inner: stream,
            encrypt: encrypt.map(Mutex::new),
            decrypt: decrypt.map(Mutex::new),
            pending_write: Vec::new(),
        }
    }

    /// Plaintext wrapper (no encryption).
    pub fn plain(stream: PeerTransport) -> Self {
        Self { inner: stream, encrypt: None, decrypt: None, pending_write: Vec::new() }
    }
}

impl AsyncRead for CryptoStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let result = Pin::new(&mut this.inner).poll_read(cx, buf);

        if let Poll::Ready(Ok(())) = &result {
            let after = buf.filled().len();
            if after > before {
                if let Some(ref dec) = this.decrypt {
                    let mut dec = dec.lock().unwrap();
                    dec.process(&mut buf.filled_mut()[before..after]);
                }
            }
        }
        result
    }
}

impl AsyncWrite for CryptoStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();

        // Plaintext path: forward as-is — keystream desync isn't possible
        // when there's no keystream.
        if this.encrypt.is_none() {
            return Pin::new(&mut this.inner).poll_write(cx, buf);
        }

        // Encrypted path: append the just-encrypted bytes to our internal
        // queue (never re-encrypt the same bytes twice — that's exactly
        // the desync bug). Then drain the queue through the inner socket.
        {
            let mut enc = this.encrypt.as_ref().unwrap().lock().unwrap();
            let mut tmp = buf.to_vec();
            enc.process(&mut tmp);
            this.pending_write.extend_from_slice(&tmp);
        }

        // Try to push the queue out. Partial inner writes are fine — what's
        // not consumed stays in pending_write for the next poll, no
        // re-encryption.
        match Pin::new(&mut this.inner).poll_write(cx, &this.pending_write) {
            Poll::Ready(Ok(n)) => {
                this.pending_write.drain(..n);
                // Tell the caller we accepted the entire input. Their buf
                // is durably queued (encrypted). poll_flush will guarantee
                // delivery before reporting flushed.
                Poll::Ready(Ok(buf.len()))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => {
                // Same story — the bytes are queued, even if the inner
                // socket isn't ready right now.
                Poll::Ready(Ok(buf.len()))
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        // Drain the pending_write queue before flushing the inner socket.
        // Single non-blocking attempt per call — if the kernel buffer is
        // full we yield Pending and the runtime will wake us when the
        // underlying socket becomes writable again.
        if !this.pending_write.is_empty() {
            match Pin::new(&mut this.inner).poll_write(cx, &this.pending_write) {
                Poll::Ready(Ok(0)) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "crypto stream pending_write: inner returned 0",
                    )));
                }
                Poll::Ready(Ok(n)) => {
                    this.pending_write.drain(..n);
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
            if !this.pending_write.is_empty() {
                return Poll::Pending;
            }
        }
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}
