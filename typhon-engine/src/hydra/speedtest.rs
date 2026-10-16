//! Measuring the line with iperf3: `[vpn_speedtest]`.
//!
//! From the way out the engines use -- the race engine's `bind_interface`,
//! with SO_BINDTODEVICE like every engine socket -- so a tunnelled seedbox
//! measures its tunnel and not the host's own route. The torrents keep
//! running meanwhile, and what they move during the test is reported beside
//! it: iperf3 alone measures what was LEFT of the line.
//!
//! The result lands in the bench database (`vpn_speedtest`), which is what
//! `/api/vpn-speedtest/latest` and `/history` read.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::api::AppState;

/// One measurement, both directions, in megabits per second.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct Measurement {
    pub ts: i64,
    pub ul_mbps: f64,
    pub dl_mbps: f64,
    /// What the engines uploaded / downloaded meanwhile.
    pub ul_torrent_mbps: f64,
    pub dl_torrent_mbps: f64,
}

/// Where and how long, read from the config at each run.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub server: String,
    pub port: u16,
    pub secs: u64,
    /// SO_BINDTODEVICE, empty = the default route.
    pub device: String,
}

/// The plan for this config, or why there is none.
pub fn plan(cfg: &crate::config::Config) -> Result<Plan, String> {
    let v = &cfg.vpn_speedtest;
    if !v.enabled || v.iperf3_server.trim().is_empty() {
        return Err("iperf3_server not configured in [vpn_speedtest]".into());
    }
    let engines = cfg.local_engines();
    let engine = engines.iter().find(|l| l.role == "race").or(engines.first());
    let session = engine.map(|l| l.session.clone()).unwrap_or_else(|| cfg.race.clone());
    let device = session.bind_interface.trim().to_string();
    // iperf3 has no SOCKS5: past a proxy it would measure the host's route
    // and call it the tunnel.
    if device.is_empty() && !session.socks5_outbound_host.trim().is_empty() {
        return Err("the engines go out through a SOCKS5 proxy, which iperf3 cannot use".into());
    }
    // iperf3 leaves the way the measured engine would, so it may only run where
    // the kill switch lets that engine out: blocked engine, no run. Inside
    // gluetun's network, or with allow_direct, the default route is allowed.
    if let Some(e) = engine {
        use super::killswitch::{armed, engine_verdict, Coverage};
        let mode = crate::netmode::current(cfg);
        let v = engine_verdict(mode, armed(cfg), e, None, crate::portmap::default_interface().as_deref());
        if v.state == Coverage::Blocked {
            return Err(format!("the kill switch keeps this engine off the network ({}): iperf3 would leave by the host's route", v.how));
        }
    }
    Ok(Plan {
        server: v.iperf3_server.trim().to_string(),
        port: v.iperf3_port.unwrap_or(5201),
        secs: v.duration_secs.unwrap_or(10).clamp(1, 60),
        device,
    })
}

/// A run in flight, so two cannot overlap.
///
/// iperf3 saturates the link by design. Two at once measure each other, and
/// the number they produce is meaningless -- worse than no number, because it
/// looks like one.
static RUNNING: AtomicBool = AtomicBool::new(false);

/// Frees the slot however the run ends, a dropped request included.
struct Slot;
impl Drop for Slot {
    fn drop(&mut self) {
        RUNNING.store(false, Ordering::Release);
    }
}

fn take_slot() -> Option<Slot> {
    RUNNING.compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed).ok().map(|_| Slot)
}

/// Run one test now, record it, return it.
pub async fn run(state: &AppState) -> Result<Measurement, String> {
    let plan = plan(&state.cfg())?;
    let _slot = take_slot().ok_or("a speedtest is already running")?;
    let engines = state.engines.clone();
    let m = measure(&plan, move || {
        engines.engines().iter().fold((0u64, 0u64), |acc, e| {
            (acc.0 + e.manager.upload_rate.get(), acc.1 + e.manager.download_rate.get())
        })
    })
    .await?;
    if let Some(bench) = state.bench.as_ref() {
        let db = bench.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(e) = db.record_speedtest(&m) {
            tracing::warn!("speedtest not recorded: {e}");
        }
    }
    tracing::info!(ul = m.ul_mbps, dl = m.dl_mbps, device = %plan.device, "speedtest");
    Ok(m)
}

/// The periodic test, when `interval_secs` > 0. Read live: a change in the
/// settings applies at the next wake.
pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        loop {
            let every = state.cfg().vpn_speedtest.interval_secs.unwrap_or(0);
            if every == 0 || plan(&state.cfg()).is_err() {
                tokio::time::sleep(Duration::from_secs(60)).await;
                continue;
            }
            tokio::time::sleep(Duration::from_secs(every.max(300))).await;
            if let Err(e) = run(&state).await {
                tracing::warn!("periodic speedtest: {e}");
            }
        }
    });
}

/// Upload then download, one after the other, the engines' own rates read
/// in the middle of each leg.
///
/// Never together: iperf3 in both directions at once splits the link between
/// them and reports half of each.
async fn measure(plan: &Plan, rates: impl Fn() -> (u64, u64)) -> Result<Measurement, String> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mbps = |bytes: u64| bytes as f64 * 8.0 / 1_000_000.0;
    let half = Duration::from_millis(plan.secs * 500);

    let (up, mid_up) = tokio::join!(run_iperf3(plan, false), async {
        tokio::time::sleep(half).await;
        rates()
    });
    let (down, mid_down) = tokio::join!(run_iperf3(plan, true), async {
        tokio::time::sleep(half).await;
        rates()
    });
    match (up, down) {
        (Ok(u), Ok(d)) => Ok(Measurement {
            ts,
            ul_mbps: u,
            dl_mbps: d,
            ul_torrent_mbps: mbps(mid_up.0),
            dl_torrent_mbps: mbps(mid_down.1),
        }),
        // Which direction failed: "iperf3 failed" alone sends an operator to
        // look at the whole tunnel when only one leg is shut.
        (u, d) => Err([u.err().map(|e| format!("upload: {e}")), d.err().map(|e| format!("download: {e}"))]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(", ")),
    }
}

/// One direction. Returns megabits per second.
async fn run_iperf3(plan: &Plan, reverse: bool) -> Result<f64, String> {
    let mut cmd = tokio::process::Command::new("iperf3");
    cmd.arg("-c").arg(&plan.server).arg("-p").arg(plan.port.to_string()).arg("-J").arg("-t").arg(plan.secs.to_string());
    if !plan.device.is_empty() {
        cmd.arg("--bind-dev").arg(&plan.device);
    }
    if reverse {
        cmd.arg("-R");
    }
    // Killed with the request that started it: a closed tab must not leave
    // the link saturated.
    cmd.kill_on_drop(true);
    let limit = plan.secs * 2 + 20;
    let out = tokio::time::timeout(Duration::from_secs(limit), cmd.output())
        .await
        .map_err(|_| format!("iperf3 did not finish in {limit}s"))?
        .map_err(|e| format!("cannot run iperf3: {e}"))?;
    if !out.status.success() {
        // -J puts iperf3's own error in the JSON, stderr is often empty.
        let json_err = serde_json::from_slice::<serde_json::Value>(&out.stdout)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string));
        let err = json_err.unwrap_or_else(|| String::from_utf8_lossy(&out.stderr).trim().to_string());
        return Err(err.chars().take(200).collect());
    }
    parse_bits_per_second(&out.stdout).map(|bps| bps / 1_000_000.0)
}

/// Pull the summary throughput out of iperf3's JSON.
///
/// `sum_received` and not `sum_sent`: what arrived is the throughput, and the
/// two differ by exactly the packets that were lost.
pub fn parse_bits_per_second(stdout: &[u8]) -> Result<f64, String> {
    let v: serde_json::Value =
        serde_json::from_slice(stdout).map_err(|e| format!("iperf3 json: {e}"))?;
    v.get("end")
        .and_then(|e| e.get("sum_received"))
        .and_then(|s| s.get("bits_per_second"))
        .and_then(|b| b.as_f64())
        .ok_or_else(|| "iperf3 json has no end.sum_received.bits_per_second".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_received_rate_is_the_one_that_counts() {
        let json = br#"{"end":{"sum_sent":{"bits_per_second":900000000.0},
                                "sum_received":{"bits_per_second":850000000.0}}}"#;
        // Sent is what we pushed, received is what arrived. The gap is loss,
        // and reporting the larger number would hide it.
        assert_eq!(parse_bits_per_second(json).unwrap(), 850_000_000.0);
    }

    #[test]
    fn a_reply_without_a_rate_is_an_error_not_a_zero() {
        // Zero would show as "the tunnel is dead" on a panel; an error says
        // "the measurement did not happen", which is a different thing.
        assert!(parse_bits_per_second(b"{}").is_err());
        assert!(parse_bits_per_second(b"not json").is_err());
    }

    #[test]
    fn two_runs_cannot_overlap() {
        let a = take_slot().expect("free");
        assert!(take_slot().is_none(), "two iperf3 runs at once measure each other, not the link");
        drop(a);
        assert!(take_slot().is_some(), "the slot is freed however the run ends");
    }

    fn cfg(src: &str) -> crate::config::Config {
        toml::from_str(src).unwrap()
    }

    /// The race engine's way out, the port and the length the file says.
    #[test]
    fn the_plan_follows_the_config() {
        assert!(plan(&cfg("[vpn_speedtest]\nenabled = false\niperf3_server = \"x\"\n")).is_err(), "off");
        assert!(plan(&cfg("[vpn_speedtest]\nenabled = true\n")).is_err(), "no server");
        let p = plan(&cfg("[race]\nbind_interface = \"wg0\"\n[vpn_speedtest]\nenabled = true\niperf3_server = \"ping.example\"\niperf3_port = 5202\nduration_secs = 5\n")).unwrap();
        assert_eq!((p.server.as_str(), p.port, p.secs, p.device.as_str()), ("ping.example", 5202, 5, "wg0"));
        let d = plan(&cfg("[vpn_speedtest]\nenabled = true\niperf3_server = \"s\"\n")).unwrap();
        assert_eq!((d.port, d.secs, d.device.as_str()), (5201, 10, ""), "defaults, default route");
    }

    /// Behind a SOCKS5 proxy iperf3 would measure the host's own route.
    #[test]
    fn a_socks5_way_out_is_refused_by_name() {
        let e = plan(&cfg("[race]\nsocks5_outbound_host = \"10.0.0.1\"\n[vpn_speedtest]\nenabled = true\niperf3_server = \"s\"\n")).unwrap_err();
        assert!(e.contains("SOCKS5"), "{e}");
    }

    /// With the kill switch armed, a run with no interface of its own would
    /// leave by the host's route, which is exactly what the switch forbids.
    #[test]
    fn an_armed_kill_switch_refuses_a_run_with_no_interface() {
        let on = "[daemon]\nkill_switch = true\n[vpn_speedtest]\nenabled = true\niperf3_server = \"s\"\n";
        let e = plan(&cfg(on)).expect_err("armed, no interface");
        assert!(e.contains("kill switch"), "{e}");
        let bound = format!("[race]\nbind_interface = \"wg0\"\n{on}");
        assert_eq!(plan(&cfg(&bound)).unwrap().device, "wg0", "an interface of its own is fine");
        let off = "[daemon]\nkill_switch = false\n[vpn_speedtest]\nenabled = true\niperf3_server = \"s\"\n";
        assert!(plan(&cfg(off)).is_ok(), "disarmed: the default route is allowed");
        let gluetun = "[network]\nmode = \"gluetun\"\n[vpn_speedtest]\nenabled = true\niperf3_server = \"s\"\n";
        assert!(plan(&cfg(gluetun)).is_ok(), "inside gluetun the default route is the tunnel");
        let direct = format!("[race]\nallow_direct = true\n{on}");
        assert!(plan(&cfg(&direct)).is_ok(), "allow_direct: the kill switch lets the engine out, so the run too");
    }
}
