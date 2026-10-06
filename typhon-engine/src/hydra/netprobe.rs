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
const ECHO_V4: &str = "https://api.ipify.org/";
const ECHO_V6: &str = "https://api6.ipify.org/";

/// How often the exits are re-measured. Each pass costs one request per engine
/// per family, so this is a background timer and never a per-request probe.
const INTERVAL: Duration = Duration::from_secs(180);

/// Bounds one measurement, so a whole pass stays inside its budget however many
/// engines the node runs.
const ATTEMPT: Duration = Duration::from_secs(6);

/// One engine's network identity, in the shape the header reads.
pub type Snapshot = Arc<tokio::sync::Mutex<(Vec<serde_json::Value>, i64)>>;

pub fn spawn(engines: Arc<EngineHost>, snapshot: Snapshot, public_ip: crate::api::PublicIp) {
    tokio::spawn(async move {
        loop {
            measure(&engines, &snapshot, &public_ip).await;
            tokio::time::sleep(INTERVAL).await;
        }
    });
}

/// Take one measurement pass now.
pub async fn measure(engines: &Arc<EngineHost>, snapshot: &Snapshot, public_ip: &crate::api::PublicIp) {
    // The process's own exit, for the header's fallback line and /api/public-ip:
    // the DAEMON's way out (`[proxy]`, `[daemon] bind_interface`), which is
    // what 3.x measured too, and refused under a kill switch with none.
    let proc_v4 = echo_daemon(ECHO_V4).await;
    let proc_v6 = echo_daemon(ECHO_V6).await;
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
            None if daemon_route.is_direct() => (proc_v4.clone(), proc_v6.clone()),
            None if daemon_route.kill_switch => (None, None),
            None => (echo(None, ECHO_V4).await, echo(None, ECHO_V6).await),
            Some(_) => (echo(bound, ECHO_V4).await, echo(bound, ECHO_V6).await),
        };

        rows.push(row(&engine, bound.is_some(), v4, v6));
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mut slot = snapshot.lock().await;
    *slot = (rows, now);
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
    let port = engine.manager.announced_port(engine.listen_port);
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
async fn echo_daemon(url: &str) -> Option<String> {
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
async fn echo(device: Option<&str>, url: &str) -> Option<String> {
    let mut builder = reqwest::Client::builder().timeout(ATTEMPT);
    // ⚠ Binding a request to a named interface is SO_BINDTODEVICE, which only
    // exists on Linux -- reqwest does not expose `interface()` anywhere else.
    // Windows has no equivalent by device NAME (it binds by source ADDRESS),
    // so the probe answers None rather than silently reporting the address of
    // the default route as if it had gone out through the tunnel. A wrong
    // answer here reads as "the VPN is up" when it is not.
    #[cfg(target_os = "linux")]
    if let Some(dev) = device {
        builder = builder.interface(dev);
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
}
