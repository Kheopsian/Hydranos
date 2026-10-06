//! Following gluetun's forwarded port.
//!
//! A VPN provider that forwards a port hands one out per lease, and gluetun
//! renews the lease with a different port whenever it likes. The engine has to
//! listen there AND tell trackers that port, or it is unreachable from the
//! swarm while looking healthy. Gluetun's control server says which port it
//! holds; this reads it and moves the engine when it changes.
//!
//! Until the first answer, the engine's announces are held
//! (`TorrentManager::set_port_pending`): the configured port is a guess, and a
//! tracker told a guess hands it to every peer for a whole interval.

use std::sync::Arc;
use std::time::Duration;

use typhon_engine::torrent::TorrentManager;

const DEFAULT_URL: &str = "http://127.0.0.1:8000";
/// Polled faster while nothing is known: the engine is silent until then.
const POLL_PENDING: Duration = Duration::from_secs(10);
const POLL_SETTLED: Duration = Duration::from_secs(60);

/// The port in a control-server answer. `{"port":0}` is gluetun's "no forward
/// yet", not a port.
pub fn port_from(body: &str) -> Option<u16> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let port = v.get("port")?.as_u64()?;
    u16::try_from(port).ok().filter(|p| *p != 0)
}

/// Ask gluetun for its forwarded port.
///
/// `/v1/portforward` since gluetun 3.40; `/v1/openvpn/portforwarded` before,
/// tried when the new route is a 404.
pub async fn fetch(client: &reqwest::Client, url: &str, key: &str) -> Result<u16, String> {
    let base = if url.trim().is_empty() { DEFAULT_URL } else { url.trim().trim_end_matches('/') };
    let mut last = String::new();
    for path in ["/v1/portforward", "/v1/openvpn/portforwarded"] {
        let mut req = client.get(format!("{base}{path}"));
        if !key.is_empty() {
            req = req.header("X-API-Key", key);
        }
        let resp = req.send().await.map_err(|e| format!("{base}{path}: {e}"))?;
        let status = resp.status().as_u16();
        if status == 404 {
            last = format!("{base}{path}: 404");
            continue;
        }
        if status == 401 || status == 403 {
            return Err(format!("{base}{path}: http {status} -- set the gluetun API key, with GET {path} allowed"));
        }
        let body = resp.text().await.map_err(|e| format!("{base}{path}: {e}"))?;
        if !(200..300).contains(&status) {
            return Err(format!("{base}{path}: http {status}"));
        }
        return port_from(&body).ok_or_else(|| format!("{base}{path}: no port forwarded yet"));
    }
    Err(last)
}

/// Follow gluetun's port for one engine, for the life of the process.
pub fn spawn(engine: String, manager: Arc<TorrentManager>, url: String, key: String) {
    manager.set_port_pending(true);
    tracing::info!(engine = %engine, "gluetun port forward: announces held until gluetun gives the port");
    tokio::spawn(async move {
        let client = match reqwest::Client::builder().timeout(Duration::from_secs(10)).build() {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(engine = %engine, error = %e, "gluetun port forward: no http client, announces stay held");
                return;
            }
        };
        let mut applied: u16 = 0;
        // Logged when it changes, not every poll: a gluetun that is down for
        // an hour is one line, not 360.
        let mut last_error = String::new();
        loop {
            match fetch(&client, &url, &key).await {
                Ok(port) if port == applied => {}
                Ok(port) => {
                    // Awaited: announces are released only once the listener
                    // really holds the port. A port that cannot be bound
                    // keeps them held rather than hand trackers a dead port.
                    match manager.rebind_listener(port).await {
                        Ok(_) => {
                            tracing::info!(engine = %engine, from = applied, to = port, "gluetun port forward: listening on the forwarded port");
                            applied = port;
                            last_error.clear();
                            manager.set_port_pending(false);
                        }
                        Err(e) => {
                            // The listener is not up yet, or the port is
                            // taken; the next poll retries.
                            tracing::debug!(engine = %engine, port, error = %e, "gluetun port forward: listener not moved");
                        }
                    }
                }
                Err(e) => {
                    if e != last_error {
                        tracing::warn!(
                            engine = %engine,
                            error = %e,
                            holding = manager.port_pending(),
                            "gluetun port forward: cannot read the port"
                        );
                        last_error = e;
                    }
                }
            }
            tokio::time::sleep(if applied == 0 { POLL_PENDING } else { POLL_SETTLED }).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;

    #[test]
    fn a_zero_port_is_no_forward_yet() {
        assert_eq!(port_from(r#"{"port":51413}"#), Some(51413));
        assert_eq!(port_from(r#"{"port":0}"#), None);
        assert_eq!(port_from(r#"{"port":70000}"#), None);
        assert_eq!(port_from("not json"), None);
    }

    async fn serve(app: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn the_current_route_is_read_with_the_key() {
        let app = axum::Router::new().route(
            "/v1/portforward",
            axum::routing::get(|h: HeaderMap| async move {
                if h.get("x-api-key").and_then(|v| v.to_str().ok()) == Some("k") {
                    (StatusCode::OK, r#"{"port":40123}"#).into_response()
                } else {
                    StatusCode::UNAUTHORIZED.into_response()
                }
            }),
        );
        let url = serve(app).await;
        let client = reqwest::Client::new();
        assert_eq!(fetch(&client, &url, "k").await, Ok(40123));
        let err = fetch(&client, &url, "").await.unwrap_err();
        assert!(err.contains("API key"), "{err}");
    }

    #[tokio::test]
    async fn an_older_gluetun_is_read_on_its_old_route() {
        let app = axum::Router::new().route(
            "/v1/openvpn/portforwarded",
            axum::routing::get(|| async { r#"{"port":39000}"# }),
        );
        let url = serve(app).await;
        assert_eq!(fetch(&reqwest::Client::new(), &url, "").await, Ok(39000));
    }
}
