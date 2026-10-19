//! What each engine actually looks like from outside.
//!
//! The header shows an exit address per engine. Measuring it once for the
//! process would report the default route for every engine, so an engine
//! leaking outside its tunnel would look exactly like one inside it -- which is
//! the failure this measurement exists to catch. Each engine is therefore
//! probed through its own binding.
//!
//! Nothing here ever publishes a guess: an engine whose probe did not answer
//! reports no address at all, because a stale or borrowed address on this row
//! is how an operator concludes a tunnel is holding when it is not.

use std::sync::Arc;
use std::time::Duration;

use crate::engines::EngineHost;

/// The v4 and AAAA-only echoes 3.x used. The v6 one has no A record on
/// purpose: a v6 measurement that quietly answered over v4 would report an
/// address the engine never announces.
const ECHO_V6: &str = "https://api6.ipify.org/";

/// How often the exits are re-measured. Each pass costs one request per engine
/// per family, so this is a background timer and never a per-request probe.
const INTERVAL: Duration = Duration::from_secs(180);

/// Bounds one measurement, so a whole pass stays inside its budget however many
/// engines the node runs.
const ATTEMPT: Duration = Duration::from_secs(6);

/// One engine's network identity, in the shape the header reads.
pub type Snapshot = Arc<tokio::sync::Mutex<(Vec<serde_json::Value>, i64)>>;

/// What `[daemon] exit_probe` and `exit_probe_url` say, read from the live
/// config at each pass.
#[derive(Clone, Debug)]
pub struct Probe {
    /// `false`: no request is sent to any echo service.
    pub enabled: bool,
    v4: String,
    /// None for a custom URL: it cannot be known to answer over IPv6 only.
    v6: Option<String>,
}

impl Probe {
    pub fn from_config(cfg: &crate::config::Config) -> Probe {
        let v4 = cfg.daemon.exit_probe_url();
        let v6 = (v4 == crate::config::DEFAULT_EXIT_PROBE_URL).then(|| ECHO_V6.to_string());
        Probe { enabled: cfg.daemon.exit_probe_enabled(), v4, v6 }
    }
}

pub fn spawn(
    engines: Arc<EngineHost>,
    snapshot: Snapshot,
    public_ip: crate::api::PublicIp,
    config: Arc<std::sync::RwLock<Arc<crate::config::Config>>>,
) {
    tokio::spawn(async move {
        loop {
            let probe = Probe::from_config(&config.read().unwrap_or_else(|e| e.into_inner()).clone());
            measure(&engines, &snapshot, &public_ip, &probe).await;
            tokio::time::sleep(INTERVAL).await;
        }
    });
}

/// Take one measurement pass now.
pub async fn measure(
    engines: &Arc<EngineHost>,
    snapshot: &Snapshot,
    public_ip: &crate::api::PublicIp,
    probe: &Probe,
) {
    // The process's own exit, for the header's fallback line and /api/public-ip:
    // the DAEMON's way out (`[proxy]`, `[daemon] bind_interface`), which is
    // what 3.x measured too, and refused under a kill switch with none.
    let (proc_v4, proc_v6) = if probe.enabled {
        (
            echo_daemon(&probe.v4).await,
            match &probe.v6 {
                Some(u) => echo_daemon(u).await,
                None => None,
            },
        )
    } else {
        (None, None)
    };
    let daemon_route = typhon_engine::egress::route();
    {
        let mut cache = public_ip.lock().await;
        // Only overwrite on success: a failed lookup means "we did not find
        // out", not "the address is gone".
        if let Some(v4) = proc_v4.clone() {
            cache.0 = v4;
        }
        if let Some(v6) = proc_v6.clone() {
            cache.1 = v6;
        }
    }

    let mut rows = Vec::new();
    for engine in engines.engines() {
        let iface = engine.bind_interface.clone();
        let bound = if iface.is_empty() { None } else { Some(iface.as_str()) };
        let proxy = crate::engines::session_http_proxy(&engine.session);
        // An unbound engine leaves through the default route. When the daemon
        // does too, the process probe already measured it; when the daemon
        // goes another way, the engine is measured on its own -- unless the
        // kill switch is on, which sends nothing by the default route, not
        // even a probe (the engine shows no address, and the report says it
        // is not covered).
        let (v4, v6) = match bound {
            // Kept off the network by the kill switch: nothing to measure,
            // and no probe sent on its behalf.
            _ if engine.blocked.get().is_some() => (None, None),
            // An engine with a proxy of its own does not leave where the
            // daemon does: it is measured through that proxy, below.
            None if proxy.is_empty() && daemon_route.is_direct() => (proc_v4.clone(), proc_v6.clone()),
            None if proxy.is_empty() && daemon_route.kill_switch => (None, None),
            // The probe is off: nothing is asked for this engine either.
            _ if !probe.enabled => (None, None),
            None => (echo(None, &proxy, &probe.v4).await, echo_v6(None, &proxy, probe).await),
            Some(_) => (echo(bound, &proxy, &probe.v4).await, echo_v6(bound, &proxy, probe).await),
        };

        let mut r = row(&engine, bound.is_some(), v4, v6);
        if !probe.enabled && engine.blocked.get().is_none() {
            mark_unknown(&mut r);
        }
        rows.push(r);
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mut slot = snapshot.lock().await;
    *slot = (rows, now);
}

/// The probe is off: the row says its exit address is unknown, not that the
/// probe failed.
fn mark_unknown(row: &mut serde_json::Value) {
    row["state"] = "unknown".into();
    row["exit_probe"] = "off".into();
    row["detail"] = "exit address unknown: [daemon] exit_probe = false".into();
}

async fn echo_v6(device: Option<&str>, proxy: &str, probe: &Probe) -> Option<String> {
    match &probe.v6 {
        Some(u) => echo(device, proxy, u).await,
        None => None,
    }
}

/// One engine's row of the header panel.
fn row(engine: &crate::engines::Engine, bound: bool, v4: Option<String>, v6: Option<String>) -> serde_json::Value {
    // "ok" only when something answered. An engine bound to a device that
    // is not carrying traffic yet is "warn", not "bad": the tunnel may
    // still be coming up, and red here would cry wolf on every restart.
    // This is the OUTBOUND probe only -- "the echo service answered" --
    // and the panel says exactly that; inbound reachability is
    // `/api/port-forward`'s `reachable`.
    let state = match (&v4, &v6) {
        (None, None) if bound => "warn",
        (None, None) => "bad",
        _ => "ok",
    };

    let mut row = serde_json::Map::new();
    // The engine's id, which is what the header panel names each row by.
    // 4.3 wrote the literal "local" here, so every row of the panel read
    // "local" and the engines could only be told apart by port.
    row.insert("agent".into(), engine.id.clone().into());
    row.insert("engine".into(), engine.id.clone().into());
    row.insert("role".into(), engine.role.clone().into());
    row.insert("local".into(), true.into());
    row.insert("state".into(), state.into());
    if !engine.bind_interface.is_empty() {
        row.insert("bind_interface".into(), engine.bind_interface.clone().into());
    }
    // The port the listener holds now, a live change included.
    let port = engine.manager.listen_port_now(engine.listen_port);
    if port != 0 {
        row.insert("listen_port".into(), port.into());
    }
    if let Some(ip) = v4 {
        row.insert("exit_ip".into(), ip.into());
    }
    if let Some(ip) = v6 {
        row.insert("exit_ip_v6".into(), ip.into());
    }
    serde_json::Value::Object(row)
}

/// Ask one echo service what address the daemon's own requests leave with.
pub(crate) async fn echo_daemon(url: &str) -> Option<String> {
    let body = typhon_engine::egress::client()
        .ok()?
        .get(url)
        .timeout(ATTEMPT)
        .send()
        .await
        .ok()?
        .text()
        .await
        .ok()?;
    let ip = body.trim().to_string();
    ip.parse::<std::net::IpAddr>().ok()?;
    Some(ip)
}

/// Ask one echo service what address it saw, optionally out through `device`.
async fn echo(device: Option<&str>, proxy: &str, url: &str) -> Option<String> {
    echo_via(device, proxy, url, device.map(typhon_engine::tunneldns::Resolver::new)).await
}

/// `echo`, with the resolver of the bound request given.
async fn echo_via(
    device: Option<&str>,
    proxy: &str,
    url: &str,
    #[allow(unused_variables)] resolver: Option<std::sync::Arc<typhon_engine::tunneldns::Resolver>>,
) -> Option<String> {
    let mut builder = reqwest::Client::builder().timeout(ATTEMPT);
    // The engine's own proxy, as its announces use it (`egress::apply_route!`):
    // the probe must report the address the engine leaves with, and the
    // echo service's name is the proxy's to resolve.
    let proxied = !proxy.trim().is_empty();
    if proxied {
        // `no_proxy` first: an HTTP_PROXY in the environment is a second way out.
        builder = builder.no_proxy().proxy(reqwest::Proxy::all(proxy.trim()).ok()?);
    }
    // ⚠ Binding a request to a named interface is SO_BINDTODEVICE, which only
    // exists on Linux -- reqwest does not expose `interface()` anywhere else.
    // Windows has no equivalent by device NAME (it binds by source ADDRESS),
    // so the probe answers None rather than silently reporting the address of
    // the default route as if it had gone out through the tunnel. A wrong
    // answer here reads as "the VPN is up" when it is not.
    #[cfg(target_os = "linux")]
    if let Some(dev) = device {
        // The echo service's name is resolved the way the engine's own names
        // are: through the tunnel's DNS, or refused under
        // `dns_no_host_fallback`, never silently by the host.
        builder = builder.interface(dev);
        if let (Some(r), false) = (resolver, proxied) {
            builder = builder.dns_resolver(r);
        }
    }
    #[cfg(not(target_os = "linux"))]
    if device.is_some() {
        return None;
    }
    let client = builder.build().ok()?;
    let body = client.get(url).send().await.ok()?.text().await.ok()?;
    let ip = body.trim().to_string();
    // The echo answers a bare address; anything else means we reached a captive
    // portal or an error page, and parsing proves which.
    ip.parse::<std::net::IpAddr>().ok()?;
    Some(ip)
}

#[cfg(test)]
mod tests {
    /// ⭐ #8. Each row is named by its engine and shows the port it holds
    /// now; "ok" means the exit probe answered, nothing more.
    #[test]
    fn a_row_names_its_engine_and_its_live_port() {
        let s = crate::api::testing::state_from("netprobe-row", "[race]\nlisten_port = 16171\n");
        let engines = s.state.engines.engines();
        let race = engines.iter().find(|e| e.id == "race").unwrap();
        race.manager.set_live_port(40001);
        let r = super::row(race, false, Some("203.0.113.5".into()), None);
        assert_eq!(r["agent"], "race", "not the literal \"local\"");
        assert_eq!(r["engine"], "race");
        assert_eq!(r["listen_port"], 40001, "the port held now, not the startup one");
        assert_eq!(r["state"], "ok");
        assert_eq!(super::row(race, true, None, None)["state"], "warn");
        assert_eq!(super::row(race, false, None, None)["state"], "bad");
    }

    /// `dns_no_host_fallback`: the exit probe bound to an interface resolves
    /// the echo service's name like the engine does. With the switch on and
    /// no tunnel DNS for the device, the name is refused (no address); with
    /// it off, the host's resolver answers as before.
    #[tokio::test]
    async fn a_bound_echo_does_not_resolve_its_name_by_the_host_when_refused() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = listener.accept().await else { return };
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 1024];
                    let _ = s.read(&mut buf).await;
                    let _ = s
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\n203.0.113.5\n")
                        .await;
                });
            }
        });
        let url = format!("http://localhost:{port}/");
        // The refusal is given to the resolver, not set on the process-wide
        // switch other tests read.
        let via = |refuse| super::echo_via(Some("lo"), "", &url, Some(typhon_engine::tunneldns::Resolver::with_host_refusal("lo", refuse)));
        assert_eq!(via(false).await.as_deref(), Some("203.0.113.5"), "switch off: the host resolves");
        let refused = via(true).await;
        assert_eq!(refused, None, "switch on: the name is refused, not resolved by the host");
    }

    /// An engine that reaches the world through a proxy is measured through
    /// it: the echo service's name is the proxy's to resolve (it is not even
    /// resolvable here), and the address in its row is the proxy's exit.
    #[tokio::test]
    async fn an_engine_with_a_proxy_is_measured_through_that_proxy() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let asked = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let seen = asked.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = listener.accept().await else { return };
                let seen = seen.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 2048];
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    *seen.lock().unwrap() = String::from_utf8_lossy(&buf[..n]).into_owned();
                    let _ = s
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\n203.0.113.9\n")
                        .await;
                });
            }
        });
        let s = crate::api::testing::state_from(
            "netprobe-proxy",
            &format!("[race]\nlisten_port = 16172\nannounce_proxy = \"http://127.0.0.1:{port}\"\n"),
        );
        let probe = super::Probe { enabled: true, v4: "http://echo.proxy-only.invalid/".into(), v6: None };
        let snapshot: super::Snapshot = Default::default();
        let public_ip: crate::api::PublicIp = Default::default();
        super::measure(&s.state.engines, &snapshot, &public_ip, &probe).await;
        let rows = snapshot.lock().await.0.clone();
        let race = rows.iter().find(|r| r["engine"] == "race").expect("race has a row");
        assert_eq!(race["exit_ip"], "203.0.113.9", "the row holds the proxy's exit: {race}");
        assert!(
            asked.lock().unwrap().starts_with("GET http://echo.proxy-only.invalid/"),
            "the proxy was asked for the echo: {:?}",
            asked.lock().unwrap()
        );
    }
}
