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
    // The process's own exit, for the header's fallback line and /api/public-ip.
    let proc_v4 = echo(None, ECHO_V4).await;
    let proc_v6 = echo(None, ECHO_V6).await;
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
        // An unbound engine leaves through the default route, which the process
        // probe already measured -- no reason to ask the echo a second time.
        let (v4, v6) = match bound {
            None => (proc_v4.clone(), proc_v6.clone()),
            Some(_) => (echo(bound, ECHO_V4).await, echo(bound, ECHO_V6).await),
        };

        // "ok" only when something answered. An engine bound to a device that
        // is not carrying traffic yet is "warn", not "bad": the tunnel may
        // still be coming up, and red here would cry wolf on every restart.
        let state = match (&v4, &v6) {
            (None, None) if bound.is_some() => "warn",
            (None, None) => "bad",
            _ => "ok",
        };

        let mut row = serde_json::Map::new();
        row.insert("agent".into(), "local".into());
        row.insert("engine".into(), engine.id.clone().into());
        row.insert("role".into(), engine.role.clone().into());
        row.insert("local".into(), true.into());
        row.insert("state".into(), state.into());
        if !engine.bind_interface.is_empty() {
            row.insert("bind_interface".into(), engine.bind_interface.clone().into());
        }
        if engine.listen_port != 0 {
            row.insert("listen_port".into(), engine.listen_port.into());
        }
        if let Some(ip) = v4 {
            row.insert("exit_ip".into(), ip.into());
        }
        if let Some(ip) = v6 {
            row.insert("exit_ip_v6".into(), ip.into());
        }
        rows.push(serde_json::Value::Object(row));
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mut slot = snapshot.lock().await;
    *slot = (rows, now);
}

/// Ask one echo service what address it saw, optionally out through `device`.
async fn echo(device: Option<&str>, url: &str) -> Option<String> {
    let mut builder = reqwest::Client::builder().timeout(ATTEMPT);
    if let Some(dev) = device {
        builder = builder.interface(dev);
    }
    let client = builder.build().ok()?;
    let body = client.get(url).send().await.ok()?.text().await.ok()?;
    let ip = body.trim().to_string();
    // The echo answers a bare address; anything else means we reached a captive
    // portal or an error page, and parsing proves which.
    ip.parse::<std::net::IpAddr>().ok()?;
    Some(ip)
}
