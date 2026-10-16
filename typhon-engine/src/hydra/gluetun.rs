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

/// What `[daemon] gluetun_verify` asks of the exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verify {
    Off,
    Warn,
    Block,
}

impl Verify {
    /// Empty or "off" is no check; an unknown word is read as no check too,
    /// and said by `gate`.
    pub fn parse(s: &str) -> Option<Verify> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "off" => Some(Verify::Off),
            "warn" => Some(Verify::Warn),
            "block" => Some(Verify::Block),
            _ => None,
        }
    }
}

/// Starts every reason an engine is kept off by `gluetun_verify = "block"`.
pub const VERIFY_BLOCK: &str = "gluetun_verify = block: ";

/// The address gluetun's control server reports for its tunnel.
pub async fn control_public_ip(client: &reqwest::Client, url: &str, key: &str) -> Result<String, String> {
    let base = if url.trim().is_empty() { DEFAULT_URL } else { url.trim().trim_end_matches('/') };
    let path = "/v1/publicip/ip";
    let mut req = client.get(format!("{base}{path}"));
    if !key.is_empty() {
        req = req.header("X-API-Key", key);
    }
    let resp = req.send().await.map_err(|e| format!("{base}{path}: {e}"))?;
    let status = resp.status().as_u16();
    if status == 401 || status == 403 {
        return Err(format!("{base}{path}: http {status} -- set the gluetun API key, with GET {path} allowed"));
    }
    let body = resp.text().await.map_err(|e| format!("{base}{path}: {e}"))?;
    if !(200..300).contains(&status) {
        return Err(format!("{base}{path}: http {status}"));
    }
    let v: serde_json::Value = serde_json::from_str(&body).map_err(|_| format!("{base}{path}: not JSON"))?;
    v.get("public_ip")
        .and_then(|p| p.as_str())
        .filter(|p| p.parse::<std::net::IpAddr>().is_ok())
        .map(str::to_string)
        .ok_or_else(|| format!("{base}{path}: no public address"))
}

/// Check that the daemon's exit is gluetun's tunnel: the address an echo
/// service sees equals the one gluetun reports. Ok carries that address.
pub async fn verify_exit(cfg: &crate::config::Config) -> Result<String, String> {
    if !cfg.daemon.exit_probe_enabled() {
        return Err("the exit address is not measured ([daemon] exit_probe = false), so it cannot be compared with gluetun's".into());
    }
    let engines = cfg.local_engines();
    let s = engines.iter().map(|e| &e.session).find(|s| s.gluetun_port_forward);
    let (url, key) = s.map(|s| (s.gluetun_url.as_str(), s.gluetun_api_key.as_str())).unwrap_or(("", ""));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .no_proxy()
        .build()
        .map_err(|e| e.to_string())?;
    let tunnel = control_public_ip(&client, url, key).await?;
    let exit = crate::netprobe::echo_daemon(&cfg.daemon.exit_probe_url())
        .await
        .ok_or_else(|| "the echo service did not answer, so the exit cannot be compared with gluetun's".to_string())?;
    if exit == tunnel {
        Ok(exit)
    } else {
        Err(format!("the daemon leaves by {exit}, not by the address gluetun reports for its tunnel ({tunnel})"))
    }
}

/// `[daemon] gluetun_verify` at boot, in gluetun mode: the reason to keep
/// every engine off the network, if there is one. "warn" only says it.
pub async fn gate(cfg: &crate::config::Config) -> Option<String> {
    if crate::netmode::current(cfg) != "gluetun" {
        return None;
    }
    let mode = match Verify::parse(&cfg.daemon.gluetun_verify) {
        Some(m) => m,
        None => {
            tracing::warn!(value = %cfg.daemon.gluetun_verify, "[daemon] gluetun_verify is off, warn or block: reading it as off");
            return None;
        }
    };
    if mode == Verify::Off {
        return None;
    }
    match verify_exit(cfg).await {
        Ok(ip) => {
            tracing::info!(exit = %ip, "gluetun_verify: the exit is gluetun's tunnel");
            None
        }
        Err(why) if mode == Verify::Warn => {
            tracing::warn!("gluetun_verify: {why}");
            None
        }
        Err(why) => {
            tracing::warn!("gluetun_verify = block: {why}");
            Some(format!("{VERIFY_BLOCK}{why}"))
        }
    }
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

    async fn pair(echo_ip: &'static str, control_ip: &'static str) -> (String, String) {
        let echo = serve(axum::Router::new().route("/", axum::routing::get(move || async move { echo_ip }))).await;
        let control = serve(axum::Router::new().route(
            "/v1/publicip/ip",
            axum::routing::get(move || async move { format!("{{\"public_ip\":\"{control_ip}\"}}") }),
        ))
        .await;
        (echo, control)
    }

    fn gluetun_cfg(verify: &str, extra: &str, echo: &str, control: &str) -> crate::config::Config {
        toml::from_str(&format!(
            "[daemon]\ngluetun_verify = \"{verify}\"\nexit_probe_url = \"{echo}\"\n{extra}\n\
             [race]\ngluetun_port_forward = true\ngluetun_url = \"{control}\"\n"
        ))
        .unwrap()
    }

    #[tokio::test]
    async fn gluetun_verify_off_by_default_asks_nothing_and_warn_never_blocks() {
        let (echo, control) = pair("203.0.113.5", "198.51.100.9").await;
        // Unset: no check, so no request even to an unreachable server.
        let unset: crate::config::Config =
            toml::from_str("[race]\ngluetun_port_forward = true\ngluetun_url = \"http://127.0.0.1:9\"\n").unwrap();
        assert_eq!(gate(&unset).await, None);
        assert_eq!(gate(&gluetun_cfg("off", "", &echo, &control)).await, None);
        // A mismatch is only said under warn.
        assert_eq!(gate(&gluetun_cfg("warn", "", &echo, &control)).await, None);
        assert!(verify_exit(&gluetun_cfg("warn", "", &echo, &control)).await.is_err());
        // The same address on both sides passes, even under block.
        let (echo, control) = pair("203.0.113.5", "203.0.113.5").await;
        assert_eq!(gate(&gluetun_cfg("block", "", &echo, &control)).await, None);
    }

    #[tokio::test]
    async fn gluetun_verify_block_blocks_on_a_mismatch_and_on_an_unreachable_control_server() {
        let (echo, control) = pair("203.0.113.5", "198.51.100.9").await;
        let why = gate(&gluetun_cfg("block", "", &echo, &control)).await.unwrap();
        assert!(why.starts_with(VERIFY_BLOCK), "{why}");
        let why = gate(&gluetun_cfg("block", "", &echo, "http://127.0.0.1:9")).await.unwrap();
        assert!(why.starts_with(VERIFY_BLOCK), "{why}");
    }
}
