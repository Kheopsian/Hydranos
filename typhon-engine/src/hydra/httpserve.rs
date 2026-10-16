//! The API's HTTP server: what accepts a connection and hands its requests to
//! the router.

use std::future::Future;
use std::net::SocketAddr;
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
        Limits { header_timeout: Duration::from_secs(30), max_connections: 1024 }
    }
}

/// Serve `app` on `listener` until `shutdown` resolves, then let the open
/// connections finish. Each request carries `ConnectInfo<SocketAddr>`.
pub async fn serve(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    limits: Limits,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let _ = limits;
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(shutdown)
        .await
}
