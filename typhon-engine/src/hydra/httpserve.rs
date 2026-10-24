//! The API's HTTP server: what accepts a connection and hands its requests to
//! the router.

use std::future::Future;
use std::time::Duration;

/// How long a connection may take to send a request's headers, and how many
/// connections the API serves at once.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub header_timeout: Duration,
    pub max_connections: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            header_timeout: Duration::from_secs(30),
            max_connections: 1024,
        }
    }
}

/// Serve `app` on `listener` until `shutdown` resolves, then let the open
/// connections finish. Each request carries `ConnectInfo<SocketAddr>`.
///
/// Not `axum::serve`, which sets neither of `limits`: a connection that sent
/// nothing, or headers that never end, held its task and its file descriptor
/// for good, and the descriptors are the same pool the peer connections draw
/// from. Here a request has `header_timeout` to finish its headers (an idle
/// keep-alive connection included), and past `max_connections` the next
/// connection waits in the kernel's backlog until one closes. HTTP/1 only, as
/// axum::serve was built here.
pub async fn serve(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    limits: Limits,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    use hyper_util::rt::{TokioIo, TokioTimer};
    use std::sync::Arc;
    use tower::ServiceExt;

    let slots = Arc::new(tokio::sync::Semaphore::new(limits.max_connections.max(1)));
    let graceful = hyper_util::server::graceful::GracefulShutdown::new();
    let mut builder = hyper::server::conn::http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(limits.header_timeout);
    tokio::pin!(shutdown);

    loop {
        let permit = tokio::select! {
            p = slots.clone().acquire_owned() => p.expect("the semaphore is never closed"),
            _ = &mut shutdown => break,
        };
        let (stream, peer) = tokio::select! {
            r = listener.accept() => match r {
                Ok(v) => v,
                // A connection reset before it was accepted: nothing to do.
                Err(e) if matches!(
                    e.kind(),
                    std::io::ErrorKind::ConnectionRefused
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::ConnectionReset
                ) => continue,
                // Most likely a full descriptor table. Waiting a second, as
                // axum::serve did, gives the process a chance to close some.
                Err(e) => {
                    tracing::error!(error = %e, "api: accept failed");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            },
            _ = &mut shutdown => break,
        };
        let svc = app
            .clone()
            .layer(axum::Extension(axum::extract::ConnectInfo(peer)));
        let svc = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
            svc.clone().oneshot(req.map(axum::body::Body::new))
        });
        // Only the headers are timed: the event stream and the log tail run
        // for as long as a tab is open.
        let conn = graceful.watch(builder.serve_connection(TokioIo::new(stream), svc));
        tokio::spawn(async move {
            let _ = conn.await;
            drop(permit);
        });
    }
    drop(listener);
    graceful.shutdown().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    /// A server with a 1 s header timeout and `max` connections, answering
    /// "ok" with the caller's address.
    async fn server(max: usize) -> (SocketAddr, tokio::sync::oneshot::Sender<()>) {
        use axum::extract::ConnectInfo;
        let app = axum::Router::new().route(
            "/",
            axum::routing::get(|ConnectInfo(peer): ConnectInfo<SocketAddr>| async move {
                format!("ok {}", peer.ip())
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let limits = Limits {
            header_timeout: Duration::from_secs(1),
            max_connections: max,
        };
        tokio::spawn(serve(listener, app, limits, async move {
            let _ = rx.await;
        }));
        (addr, tx)
    }

    /// Reads until the server closes the connection, or `within` runs out.
    async fn closed_within(s: &mut TcpStream, within: Duration) -> bool {
        let mut buf = [0u8; 1024];
        tokio::time::timeout(within, async {
            loop {
                match s.read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(_) => continue,
                }
            }
        })
        .await
        .is_ok()
    }

    async fn get(addr: SocketAddr) -> String {
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut out = String::new();
        tokio::time::timeout(Duration::from_secs(5), s.read_to_string(&mut out))
            .await
            .unwrap()
            .unwrap();
        out
    }

    #[tokio::test]
    async fn a_request_is_served_with_its_caller_address() {
        let (addr, _stop) = server(4).await;
        let out = get(addr).await;
        assert!(out.starts_with("HTTP/1.1 200"), "{out}");
        assert!(out.ends_with("ok 127.0.0.1"), "{out}");
    }

    /// The timeout is on the headers only: a response that streams for longer,
    /// like the event stream, runs to its end.
    #[tokio::test]
    async fn a_long_response_is_not_cut_by_the_header_timeout() {
        let app = axum::Router::new().route(
            "/slow",
            axum::routing::get(|| async {
                let chunks = futures::stream::unfold(0u8, |n| async move {
                    if n == 5 {
                        return None;
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    Some((
                        Ok::<_, std::convert::Infallible>(format!("chunk{n};")),
                        n + 1,
                    ))
                });
                axum::body::Body::from_stream(chunks)
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let limits = Limits {
            header_timeout: Duration::from_secs(1),
            max_connections: 4,
        };
        tokio::spawn(serve(listener, app, limits, std::future::pending()));
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(b"GET /slow HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut out = Vec::new();
        tokio::time::timeout(Duration::from_secs(6), s.read_to_end(&mut out))
            .await
            .unwrap()
            .unwrap();
        let out = String::from_utf8_lossy(&out);
        assert!(out.contains("chunk4;"), "the 2.5 s response was cut: {out}");
    }

    #[tokio::test]
    async fn a_connection_that_sends_nothing_is_closed() {
        let (addr, _stop) = server(4).await;
        let mut s = TcpStream::connect(addr).await.unwrap();
        assert!(
            closed_within(&mut s, Duration::from_secs(4)).await,
            "an idle connection stayed open"
        );
    }

    #[tokio::test]
    async fn headers_that_never_end_are_cut_off() {
        let (addr, _stop) = server(4).await;
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n").await.unwrap();
        for _ in 0..3 {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let _ = s.write_all(b"X-Slow: 1\r\n").await;
        }
        assert!(
            closed_within(&mut s, Duration::from_secs(4)).await,
            "a request that never finished its headers stayed open"
        );
    }

    #[tokio::test]
    async fn connections_beyond_the_limit_wait_for_a_free_slot() {
        let (addr, _stop) = server(2).await;
        // Two connections that hold their slot (headers started, not finished).
        let mut held = Vec::new();
        for _ in 0..2 {
            let mut s = TcpStream::connect(addr).await.unwrap();
            s.write_all(b"GET / HTTP/1.1\r\n").await.unwrap();
            held.push(s);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        // A third is accepted by the kernel but not served while both are held.
        let mut third = TcpStream::connect(addr).await.unwrap();
        third
            .write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut buf = [0u8; 64];
        let early = tokio::time::timeout(Duration::from_millis(300), third.read(&mut buf)).await;
        assert!(
            early.is_err(),
            "the third connection was served while the limit was reached"
        );
        // The held ones time out (1 s), which frees the slots.
        let mut out = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), third.read_to_end(&mut out))
            .await
            .unwrap()
            .unwrap();
        assert!(String::from_utf8_lossy(&out).starts_with("HTTP/1.1 200"));
    }
}
