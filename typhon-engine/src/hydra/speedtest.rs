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

    #[test]
    fn the_plan_bounds_the_length_and_trims_what_the_file_holds() {
        let long = plan(&cfg("[vpn_speedtest]\nenabled = true\niperf3_server = \"  s.example \"\nduration_secs = 600\n")).unwrap();
        assert_eq!((long.server.as_str(), long.secs), ("s.example", 60), "an hour-long saturation is cut to a minute");
        let zero = plan(&cfg("[vpn_speedtest]\nenabled = true\niperf3_server = \"s.example\"\nduration_secs = 0\n")).unwrap();
        assert_eq!(zero.secs, 1, "never zero seconds");
        let blank = plan(&cfg("[vpn_speedtest]\nenabled = true\niperf3_server = \"   \"\n")).unwrap_err();
        assert!(blank.contains("iperf3_server"), "{blank}");
        let spaced = plan(&cfg("[race]\nbind_interface = \"  wg0 \"\n[vpn_speedtest]\nenabled = true\niperf3_server = \"s\"\n")).unwrap();
        assert_eq!(spaced.device, "wg0");
    }

    /// A proxy plus an interface of its own is fine: the interface is what
    /// iperf3 will bind to, and the proxy is not on its path.
    #[test]
    fn an_interface_wins_over_a_proxy_host() {
        let ok = plan(&cfg(
            "[race]\nbind_interface = \"wg0\"\nsocks5_outbound_host = \"10.0.0.1\"\n[vpn_speedtest]\nenabled = true\niperf3_server = \"s\"\n",
        ))
        .unwrap();
        assert_eq!(ok.device, "wg0");
    }

    /// The measured engine is the race one, and not the hoard's way out.
    #[test]
    fn the_race_engine_is_the_one_measured() {
        let p = plan(&cfg(
            "[race]\nbind_interface = \"wg-race\"\n[hoard]\nbind_interface = \"wg-hoard\"\n\
             [vpn_speedtest]\nenabled = true\niperf3_server = \"s\"\n",
        ))
        .unwrap();
        assert_eq!(p.device, "wg-race");
    }

    // ---- the measurement itself, against a stand-in iperf3 on PATH ----
    //
    // The binary is looked up by name, so a script called `iperf3` in a
    // directory put first on PATH is what runs. It answers by the server name
    // it is given, which keeps tests running in parallel apart, and logs the
    // arguments it was called with.

    #[cfg(unix)]
    mod with_iperf3 {
        use super::*;

        const SCRIPT: &str = r#"#!/bin/sh
dir=$(dirname "$0")
echo "$@" >> "$dir/args.log"
server=$2
reverse=no
for a in "$@"; do [ "$a" = "-R" ] && reverse=yes; done
rate() { if [ $reverse = yes ]; then echo 800000000.0; else echo 400000000.0; fi; }
good() { echo "{\"end\":{\"sum_sent\":{\"bits_per_second\":9000000000.0},\"sum_received\":{\"bits_per_second\":$(rate)}}}"; }
case "$server" in
  *-ok.example) good ;;
  *-upfail.example) if [ $reverse = no ]; then echo '{"error":"the up leg is shut"}'; exit 1; else good; fi ;;
  *-downfail.example) if [ $reverse = yes ]; then echo '{"error":"the down leg is shut"}'; exit 1; else good; fi ;;
  *-json-error.example) echo '{"error":"unable to connect to server"}'; exit 1 ;;
  *-stderr-error.example) echo "  boom on stderr  " >&2; exit 2 ;;
  *-long-error.example) printf '{"error":"%s"}' "$(printf 'x%.0s' $(seq 1 300))"; exit 1 ;;
  *-garbage.example) echo "not json" ;;
  *-no-rate.example) echo '{"end":{}}' ;;
  *-hang.example) exec sleep 300 ;;
esac
"#;

        fn install() -> std::path::PathBuf {
            static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
            DIR.get_or_init(|| {
                use std::os::unix::fs::PermissionsExt;
                let dir = std::env::temp_dir().join(format!("hydra-fake-iperf3-{}", std::process::id()));
                std::fs::create_dir_all(&dir).unwrap();
                let bin = dir.join("iperf3");
                std::fs::write(&bin, SCRIPT).unwrap();
                std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
                let old = std::env::var("PATH").unwrap_or_default();
                std::env::set_var("PATH", format!("{}:{old}", dir.display()));
                dir
            })
            .clone()
        }

        fn plan_for(server: &str) -> Plan {
            install();
            Plan { server: server.into(), port: 5202, secs: 1, device: String::new() }
        }

        #[tokio::test]
        async fn each_direction_is_its_own_call_and_reports_the_received_rate() {
            let dir = install();
            let mut p = plan_for("args-ok.example");
            p.secs = 3;
            p.device = "wg0".into();
            let up = run_iperf3(&p, false).await.expect("upload leg");
            let down = run_iperf3(&p, true).await.expect("download leg");
            assert_eq!((up, down), (400.0, 800.0), "megabits per second, from sum_received, not the 9000 sent");
            let log = std::fs::read_to_string(dir.join("args.log")).unwrap();
            let calls: Vec<&str> = log.lines().filter(|l| l.contains("args-ok.example")).collect();
            assert!(calls.contains(&"-c args-ok.example -p 5202 -J -t 3 --bind-dev wg0"), "{calls:?}");
            assert!(calls.contains(&"-c args-ok.example -p 5202 -J -t 3 --bind-dev wg0 -R"), "{calls:?}");
        }

        #[tokio::test]
        async fn no_interface_means_no_bind_dev_argument() {
            let dir = install();
            let p = plan_for("nodev-ok.example");
            run_iperf3(&p, false).await.unwrap();
            let log = std::fs::read_to_string(dir.join("args.log")).unwrap();
            let call = log.lines().find(|l| l.contains("nodev-ok.example")).unwrap();
            assert_eq!(call, "-c nodev-ok.example -p 5202 -J -t 1", "the default route: nothing to bind");
        }

        /// iperf3 puts its own error in the JSON and leaves stderr empty; when
        /// it does not, stderr is the message, and neither is unbounded.
        #[tokio::test]
        async fn a_failed_run_says_why_in_iperf3s_own_words() {
            let e = run_iperf3(&plan_for("a-json-error.example"), false).await.unwrap_err();
            assert_eq!(e, "unable to connect to server");
            let e = run_iperf3(&plan_for("a-stderr-error.example"), false).await.unwrap_err();
            assert_eq!(e, "boom on stderr", "trimmed");
            let e = run_iperf3(&plan_for("a-long-error.example"), false).await.unwrap_err();
            assert_eq!(e.chars().count(), 200, "cut to 200 characters");
        }

        #[tokio::test]
        async fn an_answer_that_is_not_a_measurement_is_an_error() {
            let e = run_iperf3(&plan_for("a-garbage.example"), false).await.unwrap_err();
            assert!(e.starts_with("iperf3 json"), "{e}");
            let e = run_iperf3(&plan_for("a-no-rate.example"), false).await.unwrap_err();
            assert!(e.contains("sum_received"), "{e}");
        }

        /// Upload then download, the engines' rates read in the middle of each
        /// leg: the upload figure from the first, the download from the second.
        #[tokio::test]
        async fn a_measurement_pairs_each_leg_with_the_engines_rate_of_its_direction() {
            let m = measure(&plan_for("m-ok.example"), || (8_000_000, 16_000_000)).await.expect("measured");
            assert_eq!((m.ul_mbps, m.dl_mbps), (400.0, 800.0));
            assert_eq!(m.ul_torrent_mbps, 64.0, "8 MB/s of torrent upload is 64 Mbps");
            assert_eq!(m.dl_torrent_mbps, 128.0, "16 MB/s of torrent download is 128 Mbps");
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
            assert!((now - m.ts).abs() < 30, "stamped when the run began: {}", m.ts);
        }

        /// "iperf3 failed" would send an operator to the whole tunnel when only
        /// one leg is shut.
        #[tokio::test]
        async fn a_failure_names_the_leg_that_failed() {
            let e = measure(&plan_for("m-upfail.example"), || (0, 0)).await.unwrap_err();
            assert_eq!(e, "upload: the up leg is shut");
            let e = measure(&plan_for("m-downfail.example"), || (0, 0)).await.unwrap_err();
            assert_eq!(e, "download: the down leg is shut");
            let e = measure(&plan_for("m-json-error.example"), || (0, 0)).await.unwrap_err();
            assert_eq!(e, "upload: unable to connect to server, download: unable to connect to server");
        }

        /// The whole run: planned from the settings, measured, recorded in the
        /// bench database and returned. The slot is process-wide and another
        /// test takes it for a moment, so a refusal for that is retried.
        #[tokio::test]
        async fn a_run_is_recorded_for_the_history_routes() {
            let mut s = crate::api::testing::state_from(
                "st-run",
                "[vpn_speedtest]\nenabled = true\niperf3_server = \"r-ok.example\"\nduration_secs = 1\n",
            );
            install();
            let bench: crate::benchdb::Shared =
                Arc::new(std::sync::Mutex::new(crate::benchdb::BenchDb::open_in_memory().unwrap()));
            s.state.bench = Some(bench.clone());
            let mut m = run(&s.state).await;
            for _ in 0..40 {
                if m.as_ref().err().is_some_and(|e| e.contains("already running")) {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    m = run(&s.state).await;
                }
            }
            let m = m.expect("measured");
            assert_eq!((m.ul_mbps, m.dl_mbps), (400.0, 800.0));
            let stored = bench.lock().unwrap().latest_speedtest().expect("recorded");
            assert_eq!(stored, m, "what the latest route will read is what the run returned");
        }

        /// The torrents' share is read in the MIDDLE of each leg -- half the
        /// test length after it began -- and not at its start or its end.
        #[tokio::test]
        async fn the_engines_rates_are_read_half_way_through_each_leg() {
            let mut p = plan_for("half-ok.example");
            p.secs = 2;
            let start = std::time::Instant::now();
            let reads = std::sync::Mutex::new(Vec::new());
            measure(&p, || {
                reads.lock().unwrap().push(start.elapsed());
                (0, 0)
            })
            .await
            .expect("measured");
            let reads = reads.into_inner().unwrap();
            assert_eq!(reads.len(), 2, "once per leg");
            let (first, second) = (reads[0].as_secs_f64(), reads[1].as_secs_f64());
            assert!((1.0..5.0).contains(&first), "half of 2 s into the upload leg: {first}");
            assert!(second - first >= 0.95 && second < 9.0, "and half of 2 s into the download leg that follows: {first} {second}");
        }

        /// An iperf3 that does not come back is cut off after twice the test
        /// length plus twenty seconds, and the error says which limit it was.
        /// Run on a paused clock: the stand-in sleeps for minutes.
        #[tokio::test(start_paused = true)]
        async fn an_iperf3_that_never_answers_is_cut_off_at_twice_the_length_plus_twenty() {
            for (secs, limit) in [(1, 22), (30, 80)] {
                let mut p = plan_for("t-hang.example");
                p.secs = secs;
                let e = run_iperf3(&p, false).await.unwrap_err();
                assert_eq!(e, format!("iperf3 did not finish in {limit}s"), "a test of {secs} s");
            }
        }

        /// The torrents' own share of the line is the sum of every engine's
        /// rate, each direction apart.
        #[tokio::test]
        async fn a_run_reports_the_rates_of_all_the_engines_added_up() {
            let mut s = crate::api::testing::state_from(
                "st-sum",
                "[vpn_speedtest]\nenabled = true\niperf3_server = \"sum-ok.example\"\nduration_secs = 1\n",
            );
            install();
            s.state.bench = None;
            let engines = s.state.engines.engines();
            assert!(engines.len() >= 2, "a sum needs more than one engine");
            // A rate is measured between two updates: the first only seeds it.
            for e in engines.iter() {
                e.manager.upload_rate.update(0);
                e.manager.download_rate.update(0);
            }
            tokio::time::sleep(Duration::from_millis(700)).await;
            for (i, e) in engines.iter().enumerate() {
                e.manager.upload_rate.update(4_000_000 * (i as u64 + 1));
                e.manager.download_rate.update(2_000_000 * (i as u64 + 3));
            }
            let (up, down) = engines.iter().fold((0u64, 0u64), |a, e| (a.0 + e.manager.upload_rate.get(), a.1 + e.manager.download_rate.get()));
            assert!(engines.iter().all(|e| e.manager.upload_rate.get() > 0 && e.manager.download_rate.get() > 0));
            let mut m = run(&s.state).await;
            for _ in 0..40 {
                if m.as_ref().err().is_some_and(|e| e.contains("already running")) {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    m = run(&s.state).await;
                }
            }
            let m = m.expect("measured");
            assert_eq!(m.ul_torrent_mbps, up as f64 * 8.0 / 1_000_000.0, "every engine's upload, added");
            assert_eq!(m.dl_torrent_mbps, down as f64 * 8.0 / 1_000_000.0, "every engine's download, added");
        }

        #[tokio::test]
        async fn a_run_with_nothing_configured_is_refused_before_it_takes_the_slot() {
            let s = crate::api::testing::state_from("st-off", "");
            let e = run(&s.state).await.unwrap_err();
            assert!(e.contains("iperf3_server"), "{e}");
        }
    }
}
