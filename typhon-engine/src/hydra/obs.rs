//! What every engine is doing, from counters already kept.
//!
//! One reader for `/metrics`, `/health`, `/api/engines` and the anomaly
//! report, so the four cannot disagree. Nothing here walks the catalogue:
//! the per-state counts and the swarm gauges come from the once-a-second
//! `update_rates` walk, the announce figures from the scheduler's and the
//! cache's atomics, the seed size from the last tracker pass. A scrape costs
//! the same at a hundred torrents and at a million.

use std::sync::atomic::Ordering::Relaxed;

use crate::api::AppState;
use crate::engines::{Engine, EngineHost};
use typhon_engine::torrent::{STATUS_PAUSED, STATUS_SLOTS};

/// The names of the per-state slots, in `TorrentManager::status_counts`
/// order. `error` is a torrent whose data is gone (a read hit ENOENT).
pub const STATE_NAMES: [&str; STATUS_SLOTS] = ["stopped", "checking", "downloading", "seeding", "error", "paused"];

/// One engine, as observed now.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct EngineStats {
    pub id: String,
    pub role: String,
    pub torrents: usize,
    /// Keyed by `STATE_NAMES`. `paused` overlaps the others: a paused torrent
    /// is also counted under its status.
    pub states: std::collections::BTreeMap<&'static str, usize>,
    pub upload_rate: u64,
    pub download_rate: u64,
    pub peers: usize,
    pub with_peers: usize,
    pub uploading: usize,
    /// Lifetime bytes of the torrents loaded now. Falls when a torrent is
    /// removed: a gauge, not a counter.
    pub lifetime_uploaded: u64,
    pub lifetime_downloaded: u64,
    /// Bytes moved since the process started. Only grows.
    pub session_uploaded: u64,
    pub session_downloaded: u64,
    /// Sum of the seeding torrents' sizes per tracker, from the last tracker
    /// pass (every 30 s); `None` before the first one.
    pub seed_size: Option<i64>,
    pub announces_ok: u64,
    pub announces_failed: u64,
    pub announce_in_flight: u64,
    pub announce_limit: u64,
    pub announce_late: u64,
    pub announce_needed_per_s: f64,
    /// Bytes/s, 0 = no cap.
    pub upload_limit: u64,
    pub download_limit: u64,
    pub choking: bool,
    pub unchoke_slots: Option<usize>,
    pub listening: bool,
    /// Dials and announces held by `start_paused` until released.
    pub held: bool,
}

pub fn engine_stats(host: &EngineHost) -> Vec<EngineStats> {
    let trackers = crate::benchsampler::latest_trackers();
    host.engines().iter().map(|e| one(e, trackers.as_deref())).collect()
}

fn one(e: &Engine, trackers: Option<&[crate::benchsampler::TrackerRow]>) -> EngineStats {
    let m = &e.manager;
    let counts = m.status_counts();
    let states = STATE_NAMES.iter().copied().zip(counts).collect();
    let (lu, ld) = m.totals();
    let (su, sd) = m.moved();
    let (ok, failed) = e.announce_cache.outcomes();
    let a = &e.admission;
    let rates = m.rates();
    let policy = m.policy();
    EngineStats {
        id: e.id.clone(),
        role: e.role.clone(),
        torrents: m.len(),
        states,
        upload_rate: m.upload_rate.get(),
        download_rate: m.download_rate.get(),
        peers: m.cached_active_peers.load(Relaxed),
        with_peers: m.cached_torrents_with_peers.load(Relaxed),
        uploading: m.cached_torrents_uploading.load(Relaxed),
        lifetime_uploaded: lu,
        lifetime_downloaded: ld,
        session_uploaded: su,
        session_downloaded: sd,
        seed_size: trackers.map(|rows| rows.iter().filter(|r| r.engine == e.id).map(|r| r.seed_size).sum()),
        announces_ok: ok,
        announces_failed: failed,
        announce_in_flight: a.in_flight.load(Relaxed),
        announce_limit: a.concurrency.load(Relaxed),
        announce_late: a.late.load(Relaxed),
        announce_needed_per_s: a.needed_milli.load(Relaxed) as f64 / 1000.0,
        upload_limit: rates.engine.up.rate(),
        download_limit: rates.engine.down.rate(),
        choking: policy.choking(),
        unchoke_slots: policy.unchoke_slots(),
        listening: e.listening.load(Relaxed),
        held: m.limiter().dials_paused(),
    }
}

/// How the store answers right now.
#[derive(Debug, Clone, PartialEq)]
pub enum StoreState {
    Ok,
    /// Someone holds the read connection; not a failure, and not waited for.
    Busy,
    Error(String),
}

pub fn store_state(state: &AppState) -> StoreState {
    match state.store.try_read() {
        None => StoreState::Busy,
        Some(Err(())) => StoreState::Error("the store's lock is poisoned: a thread panicked holding it".into()),
        Some(Ok(s)) => match s.ping() {
            Ok(()) => StoreState::Ok,
            Err(e) => StoreState::Error(e),
        },
    }
}

/// `/health` once the router is served: 200 `healthy` or `degraded`, 503
/// `unhealthy`.
///
/// 503 is kept for a daemon that cannot do its job at all -- the store does
/// not answer, or no engine is on the network. One engine without a
/// listener among several is `degraded`: the others still seed, and a
/// restart would cost them their swarms for nothing it can fix.
pub fn health(state: &AppState) -> (bool, serde_json::Value) {
    let mut problems: Vec<serde_json::Value> = Vec::new();
    let mut fatal = false;
    let store = store_state(state);
    let store_word = match &store {
        StoreState::Ok => "ok".to_string(),
        StoreState::Busy => "busy".to_string(),
        StoreState::Error(e) => {
            fatal = true;
            problems.push(serde_json::json!({"check": "store", "severity": "fail", "detail": e}));
            "error".to_string()
        }
    };
    // Only an engine `connect` put on the network is expected to listen: one
    // left offline (`HYDRANOS_ENGINE_NET=0`, a test host) has no listener by
    // design, and calling that a fault would make the probe lie the other way.
    let online = |e: &Engine| e.engine_config.get().is_some();
    let engines: Vec<serde_json::Value> = state
        .engines
        .engines()
        .iter()
        .map(|e| {
            let listening = e.listening.load(Relaxed);
            let held = e.manager.limiter().dials_paused();
            if online(e) && !listening {
                problems.push(serde_json::json!({
                    "check": "engine", "engine": e.id, "severity": "warn",
                    "detail": format!("engine {} is not listening for peers (port {} not bound, or its tunnel is down)", e.id, e.listen_port),
                }));
            }
            // Kept off the network by the kill switch: the others still
            // seed, and a restart changes nothing until the config does, so
            // `degraded` and never `unhealthy` -- not even when it is every
            // engine (none of them counts as "online" below).
            let blocked = e.blocked.get();
            if let Some(why) = blocked {
                problems.push(serde_json::json!({
                    "check": "engine", "engine": e.id, "severity": "warn",
                    "detail": format!("engine {} is blocked by the kill switch: {why}", e.id),
                }));
            }
            if held {
                problems.push(serde_json::json!({
                    "check": "engine", "engine": e.id, "severity": "warn",
                    "detail": format!("engine {} is held by start_paused: no announce, no dial until released", e.id),
                }));
            }
            serde_json::json!({
                "id": e.id, "torrents": e.manager.len(), "online": online(e),
                "listening": listening, "held": held, "blocked": blocked,
            })
        })
        .collect();
    let on: Vec<&Engine> = state.engines.engines().iter().filter(|e| online(e)).collect();
    if !on.is_empty() && on.iter().all(|e| !e.listening.load(Relaxed)) {
        fatal = true;
        problems.push(serde_json::json!({
            "check": "engines", "severity": "fail",
            "detail": "no engine is listening for peers",
        }));
    }
    let status = if fatal {
        "unhealthy"
    } else if problems.is_empty() {
        "healthy"
    } else {
        "degraded"
    };
    (
        !fatal,
        serde_json::json!({
            "status": status,
            "version": crate::api::HYDRANOS_VERSION,
            "uptime": crate::startup::uptime_secs() as f64,
            "checks": {"store": store_word, "engines": engines},
            "problems": problems,
        }),
    )
}

/// Free space below this on a volume holding torrent data is reported.
const DISK_LOW_BYTES: u64 = 5 << 30;
/// ... or below this share of the volume, whichever is larger.
const DISK_LOW_PCT: f64 = 2.0;

/// `/api/health/anomalies`, and the MCP `health` tool.
///
/// Until 4.4 only `efficiency` and the historical re-download figures were
/// real; every other counter was a constant 0 and `anomalies` was always
/// null, while the invariant scanner that should have filled them sat in
/// `health.rs`, never spawned. The torrent checks come from its last pass
/// (every 5 minutes, `generated_at` says when); the tracker, announce,
/// engine and disk figures are read live from counters already kept.
pub fn anomalies(state: &AppState) -> serde_json::Value {
    let cfg = state.cfg();
    let last = crate::health::latest();

    // Torrent invariants, from the last pass.
    let kinds = [
        crate::health::FILES_MISSING,
        crate::health::GHOST,
        crate::health::FAKE_SEED,
        crate::health::STARVED,
        crate::health::REDL,
        crate::health::DUAL_SEED,
    ];
    let mut counts = serde_json::Map::new();
    for k in kinds {
        let n = last.as_ref().and_then(|r| r.2.counts.get(k).copied()).unwrap_or(0);
        counts.insert(k.to_string(), n.into());
    }

    // Trackers failing now: distinct hosts by the Trackers tab's own rule.
    let mut failing: std::collections::BTreeMap<String, (&'static str, Vec<String>)> = Default::default();
    for e in state.engines.engines() {
        let vers = e.announce_cache.verifications();
        for (host, classes) in e.announce_cache.error_breakdown() {
            let names: Vec<&str> = classes.iter().map(|(c, _)| c.as_str()).collect();
            let verdict = vers.get(&host).map(|v| v.verdict()).unwrap_or("");
            let sev = crate::api::tracker_severity(
                cfg.announce_hidden.contains_key(&host),
                cfg.announce_muted.contains_key(&host),
                &names,
                verdict,
            );
            if sev != "red" && sev != "amber" {
                continue;
            }
            let entry = failing.entry(host).or_insert((sev, Vec::new()));
            if sev == "red" {
                entry.0 = "red";
            }
            entry.1.push(e.id.clone());
        }
    }
    let red = failing.values().filter(|(s, _)| *s == "red").count();
    counts.insert("trackers_failing".into(), failing.len().into());

    // Announces behind schedule, and engines holding theirs.
    let stats = engine_stats(&state.engines);
    let late: u64 = stats.iter().map(|s| s.announce_late).sum();
    let held: Vec<&str> = stats.iter().filter(|s| s.held).map(|s| s.id.as_str()).collect();
    counts.insert("announces_late".into(), late.into());
    counts.insert("engines_held".into(), held.len().into());

    // Free space where the data is: the active torrents' save paths from the
    // last pass, plus data_dir, one line per filesystem.
    let mut paths: Vec<std::path::PathBuf> = vec![std::path::PathBuf::from(&cfg.daemon.data_dir)];
    if let Some(r) = &last {
        paths.extend(r.2.save_paths.iter().cloned());
    }
    let mut seen_dev = std::collections::HashSet::new();
    let mut disks = Vec::new();
    let mut low = 0usize;
    for p in paths {
        let Some(dev) = crate::volumes::device_of_nearest(&p) else { continue };
        if !seen_dev.insert(dev) {
            continue;
        }
        let Some((_used, total, free)) = crate::platform::usage(&p) else { continue };
        let pct = if total > 0 { free as f64 * 100.0 / total as f64 } else { 0.0 };
        let is_low = total > 0 && (free < DISK_LOW_BYTES || pct < DISK_LOW_PCT);
        low += is_low as usize;
        disks.push(serde_json::json!({
            "path": crate::volumes::mount_point_of(&p).display().to_string(),
            "free_bytes": free,
            "total_bytes": total,
            "free_pct": (pct * 10.0).round() / 10.0,
            "low": is_low,
        }));
    }
    counts.insert("disks_low".into(), low.into());

    let engines: serde_json::Map<String, serde_json::Value> = stats
        .iter()
        .map(|s| (s.id.clone(), serde_json::json!({"torrents": s.torrents, "states": s.states})))
        .collect();
    let (generated_at, took, listed, truncated, wasted, eff, redl_n, redl_b, scanned) = match &last {
        Some(r) => {
            let (at, ms, rep) = (&r.0, &r.1, &r.2);
            (
                serde_json::json!(at),
                serde_json::json!(ms),
                serde_json::to_value(&rep.anomalies).unwrap_or_default(),
                rep.truncated(),
                rep.wasted_bytes,
                rep.efficiency(),
                rep.redl_historical,
                rep.redl_historical_bytes,
                serde_json::to_value(&rep.scanned).unwrap_or_default(),
            )
        }
        None => (
            serde_json::Value::Null,
            serde_json::Value::Null,
            serde_json::json!([]),
            false,
            0,
            1.0,
            0,
            0,
            serde_json::json!({}),
        ),
    };
    serde_json::json!({
        // "pending" until the first pass, two minutes after the start: an
        // empty list then means "not looked yet", which `generated_at: null`
        // also says.
        "scan": if last.is_some() { "done" } else { "pending" },
        "generated_at": generated_at,
        "scan_duration_ms": took,
        "scanned": scanned,
        "scanned_race": scanned.get("race").cloned().unwrap_or(0.into()),
        "scanned_hoard": scanned.get("hoard").cloned().unwrap_or(0.into()),
        "counts": counts,
        "anomalies": listed,
        "anomalies_truncated": truncated,
        "wasted_bytes": wasted,
        "efficiency": eff,
        "redl_historical": redl_n,
        "redl_historical_bytes": redl_b,
        "trackers": failing
            .iter()
            .map(|(h, (sev, engines))| serde_json::json!({"host": h, "severity": sev, "engines": engines}))
            .collect::<Vec<_>>(),
        "trackers_red": red,
        "announces": {"late": late, "held_engines": held},
        "disks": disks,
        "engines": engines,
    })
}

/// A Prometheus label value: backslash, quote and newline escaped.
fn label(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

/// Prometheus text, exposition format 0.0.4.
pub struct Exposition {
    out: String,
}

impl Exposition {
    pub fn new() -> Self {
        Exposition { out: String::with_capacity(8 << 10) }
    }

    /// `# HELP` and `# TYPE` for a family, then its samples.
    pub fn family(&mut self, name: &str, kind: &str, help: &str, samples: &[(Vec<(&str, String)>, f64)]) {
        if samples.is_empty() {
            return;
        }
        self.out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} {kind}\n"));
        for (labels, v) in samples {
            self.out.push_str(name);
            if !labels.is_empty() {
                let l: Vec<String> = labels.iter().map(|(k, v)| format!("{k}=\"{}\"", label(v))).collect();
                self.out.push('{');
                self.out.push_str(&l.join(","));
                self.out.push('}');
            }
            if v.fract() == 0.0 && v.abs() < 1e15 {
                self.out.push_str(&format!(" {}\n", *v as i64));
            } else {
                self.out.push_str(&format!(" {v}\n"));
            }
        }
    }

    pub fn finish(self) -> String {
        self.out
    }
}

/// The whole `/metrics` page.
pub fn metrics(state: &AppState) -> String {
    let stats = engine_stats(&state.engines);
    let mut x = Exposition::new();
    x.family("hydra_up", "gauge", "1 while the process answers.", &[(vec![], 1.0)]);
    x.family(
        "hydra_build_info",
        "gauge",
        "The running version, as a label.",
        &[(vec![("version", crate::api::HYDRANOS_VERSION.to_string())], 1.0)],
    );
    x.family(
        "hydra_uptime_seconds",
        "gauge",
        "Seconds since the process started, the catalogue load included.",
        &[(vec![], crate::startup::uptime_secs() as f64)],
    );

    // One family per figure, one sample per engine.
    let per = |f: &dyn Fn(&EngineStats) -> f64| -> Vec<(Vec<(&str, String)>, f64)> {
        stats.iter().map(|s| (vec![("engine", s.id.clone())], f(s))).collect()
    };
    x.family("hydra_torrents", "gauge", "Torrents loaded in the engine.", &per(&|s| s.torrents as f64));
    let mut by_state = Vec::new();
    for s in &stats {
        for (name, n) in &s.states {
            if *name == STATE_NAMES[STATUS_PAUSED] {
                continue;
            }
            by_state.push((vec![("engine", s.id.clone()), ("state", name.to_string())], *n as f64));
        }
    }
    x.family(
        "hydra_torrents_by_state",
        "gauge",
        "Torrents per state; error means the data is missing.",
        &by_state,
    );
    x.family(
        "hydra_torrents_paused",
        "gauge",
        "Torrents stopped by the operator (also counted under their state).",
        &per(&|s| s.states.get("paused").copied().unwrap_or(0) as f64),
    );
    x.family("hydra_upload_rate_bytes", "gauge", "Upload rate, bytes per second.", &per(&|s| s.upload_rate as f64));
    x.family("hydra_download_rate_bytes", "gauge", "Download rate, bytes per second.", &per(&|s| s.download_rate as f64));
    x.family(
        "hydra_session_uploaded_bytes_total",
        "counter",
        "Bytes uploaded since the process started.",
        &per(&|s| s.session_uploaded as f64),
    );
    x.family(
        "hydra_session_downloaded_bytes_total",
        "counter",
        "Bytes downloaded since the process started.",
        &per(&|s| s.session_downloaded as f64),
    );
    x.family(
        "hydra_lifetime_uploaded_bytes",
        "gauge",
        "Lifetime upload of the torrents loaded now; falls when one is removed.",
        &per(&|s| s.lifetime_uploaded as f64),
    );
    x.family(
        "hydra_lifetime_downloaded_bytes",
        "gauge",
        "Lifetime download of the torrents loaded now; falls when one is removed.",
        &per(&|s| s.lifetime_downloaded as f64),
    );
    x.family("hydra_peers", "gauge", "Connected peers.", &per(&|s| s.peers as f64));
    x.family("hydra_torrents_with_peers", "gauge", "Torrents with at least one peer.", &per(&|s| s.with_peers as f64));
    x.family("hydra_torrents_uploading", "gauge", "Torrents uploading now.", &per(&|s| s.uploading as f64));
    let with_seed: Vec<_> = stats
        .iter()
        .filter_map(|s| s.seed_size.map(|v| (vec![("engine", s.id.clone())], v as f64)))
        .collect();
    x.family(
        "hydra_seed_size_bytes",
        "gauge",
        "Size of the seeding torrents, as trackers credit it (a cross-seed counts per tracker).",
        &with_seed,
    );
    let mut announces = Vec::new();
    for s in &stats {
        announces.push((vec![("engine", s.id.clone()), ("result", "ok".into())], s.announces_ok as f64));
        announces.push((vec![("engine", s.id.clone()), ("result", "failed".into())], s.announces_failed as f64));
    }
    x.family("hydra_announces_total", "counter", "Announces since the process started.", &announces);
    let mut per_tracker = Vec::new();
    let mut errors = Vec::new();
    for e in state.engines.engines() {
        for (host, ok, failed) in e.announce_cache.host_outcomes() {
            per_tracker.push((vec![("engine", e.id.clone()), ("tracker", host.clone()), ("result", "ok".into())], ok as f64));
            per_tracker.push((vec![("engine", e.id.clone()), ("tracker", host), ("result", "failed".into())], failed as f64));
        }
        let mut breakdown: Vec<_> = e.announce_cache.error_breakdown().into_iter().collect();
        breakdown.sort();
        for (host, classes) in breakdown {
            for (class, n) in classes {
                errors.push((vec![("engine", e.id.clone()), ("tracker", host.clone()), ("class", class)], n as f64));
            }
        }
    }
    x.family(
        "hydra_tracker_announces_total",
        "counter",
        "Announces per tracker since the process started.",
        &per_tracker,
    );
    x.family(
        "hydra_tracker_errors_last_hour",
        "gauge",
        "Failed announces per tracker and error class over the last 60 minutes.",
        &errors,
    );
    x.family(
        "hydra_announce_in_flight",
        "gauge",
        "Announces out now.",
        &per(&|s| s.announce_in_flight as f64),
    );
    x.family(
        "hydra_announce_slots",
        "gauge",
        "Announces allowed out at once (the scheduler's concurrency).",
        &per(&|s| s.announce_limit as f64),
    );
    x.family(
        "hydra_announce_late",
        "gauge",
        "Torrents whose announce deadline passed more than 5 s ago.",
        &per(&|s| s.announce_late as f64),
    );
    x.family(
        "hydra_announce_needed_per_second",
        "gauge",
        "Announces per second the catalogue's intervals require.",
        &per(&|s| s.announce_needed_per_s),
    );
    let mut limits = Vec::new();
    for s in &stats {
        limits.push((vec![("engine", s.id.clone()), ("direction", "up".into())], s.upload_limit as f64));
        limits.push((vec![("engine", s.id.clone()), ("direction", "down".into())], s.download_limit as f64));
    }
    let client = state.engines.client_rates();
    limits.push((vec![("engine", "client".into()), ("direction", "up".into())], client.up.rate() as f64));
    limits.push((vec![("engine", "client".into()), ("direction", "down".into())], client.down.rate() as f64));
    x.family(
        "hydra_rate_limit_bytes",
        "gauge",
        "Active speed cap in bytes per second, 0 = none; engine=\"client\" is the cap above every engine.",
        &limits,
    );
    x.family(
        "hydra_choking",
        "gauge",
        "1 when the choker is on (choking = true).",
        &per(&|s| s.choking as u8 as f64),
    );
    let slots: Vec<_> = stats
        .iter()
        .filter_map(|s| s.unchoke_slots.map(|n| (vec![("engine", s.id.clone())], n as f64)))
        .collect();
    x.family("hydra_unchoke_slots", "gauge", "Upload slots per torrent while the choker is on.", &slots);
    x.family("hydra_listening", "gauge", "1 when the engine's peer listener is bound.", &per(&|s| s.listening as u8 as f64));
    x.family(
        "hydra_held",
        "gauge",
        "1 while start_paused holds the engine's dials and announces.",
        &per(&|s| s.held as u8 as f64),
    );
    if let Some((allocated, resident)) = crate::allocdiag::memory() {
        x.family(
            "hydra_memory_allocated_bytes",
            "gauge",
            "Bytes the process holds, as jemalloc counts them.",
            &[(vec![], allocated as f64)],
        );
        x.family(
            "hydra_memory_resident_bytes",
            "gauge",
            "Resident bytes jemalloc maps; much above allocated means pages SIGUSR2 can return.",
            &[(vec![], resident as f64)],
        );
    }
    x.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::testing::{state_from, TestState};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    const KEY: &str = "obs-test-key";

    fn torrent_bytes(name: &str) -> Vec<u8> {
        let mut info = Vec::new();
        info.extend_from_slice(format!("d6:lengthi16384e4:name{}:{name}", name.len()).as_bytes());
        info.extend_from_slice(b"12:piece lengthi16384e6:pieces20:");
        let mut piece = [0xCDu8; 20];
        piece[0] = name.as_bytes()[0];
        piece[1] = name.len() as u8;
        info.extend_from_slice(&piece);
        info.push(b'e');
        let announce = "https://tracker.example/announce";
        let mut out = Vec::new();
        out.extend_from_slice(format!("d8:announce{}:{announce}4:info", announce.len()).as_bytes());
        out.extend_from_slice(&info);
        out.push(b'e');
        out
    }

    /// race, hoard and an `[[engine]]` block, one torrent in each.
    fn three_engines(tag: &str) -> (TestState, Vec<String>) {
        let s = state_from(
            tag,
            &format!(
                "[daemon]\napi_key = \"{KEY}\"\n\n[[engine]]\nname = \"vpn7\"\nrole = \"race\"\n[engine.session]\nlisten_port = 26991\n"
            ),
        );
        let mut hashes = Vec::new();
        for (engine, name) in [("race", "alpha"), ("hoard", "bravo"), ("vpn7", "charlie")] {
            let (h, _) = crate::api::add_torrent_bytes(&s.state, &torrent_bytes(name), "", "/tmp", "", false, true, engine)
                .unwrap_or_else(|e| panic!("add to {engine}: {e}"));
            hashes.push(h);
        }
        // The per-state counts come from the once-a-second walk.
        for e in s.state.engines.engines() {
            e.manager.update_rates();
        }
        (s, hashes)
    }

    async fn get(s: &TestState, path: &str) -> (StatusCode, String) {
        let resp = crate::api::router(s.state.clone())
            .oneshot(Request::builder().uri(path).header("X-Api-Key", KEY).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// #88: one series per engine, the extra one included, with HELP and
    /// TYPE, from counters -- not the three lines 4.3 printed.
    #[tokio::test]
    async fn metrics_have_per_engine_series_for_every_engine() {
        let (s, _) = three_engines("obs-metrics");
        s.state.engines.get("vpn7").unwrap().announce_cache.count_ok("tracker.example");
        let (st, text) = get(&s, "/metrics").await;
        assert_eq!(st, StatusCode::OK);
        for line in [
            "# TYPE hydra_torrents gauge",
            "hydra_torrents{engine=\"vpn7\"} 1",
            "hydra_torrents_by_state{engine=\"vpn7\",state=\"seeding\"} 1",
            "# TYPE hydra_announces_total counter",
            "hydra_announces_total{engine=\"vpn7\",result=\"ok\"} 1",
            "hydra_tracker_announces_total{engine=\"vpn7\",tracker=\"tracker.example\",result=\"ok\"} 1",
            "hydra_upload_rate_bytes{engine=\"hoard\"} 0",
            "hydra_rate_limit_bytes{engine=\"client\",direction=\"up\"} 0",
            "hydra_announce_slots{engine=\"race\"}",
            "hydra_session_uploaded_bytes_total{engine=\"race\"} 0",
            "hydra_build_info{version=",
        ] {
            assert!(text.contains(line), "missing {line:?} in:\n{text}");
        }
        #[cfg(unix)]
        assert!(text.contains("hydra_memory_resident_bytes "), "jemalloc figures on Unix");
    }

    /// #87: /health looks. An offline test host is healthy (nothing was
    /// meant to listen); the store answering is part of it.
    #[tokio::test]
    async fn health_reports_its_checks() {
        let (s, _) = three_engines("obs-health");
        let (st, body) = get(&s, "/health").await;
        assert_eq!(st, StatusCode::OK, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["status"], "healthy");
        assert_eq!(v["checks"]["store"], "ok");
        assert_eq!(v["checks"]["engines"].as_array().unwrap().len(), 3);
        assert!(v["version"].is_string(), "the page reads its version labels here");
    }

    /// An engine the kill switch keeps off the network is `degraded`, still
    /// 200: a restart would not bring it back, and an orchestrator restarting
    /// "unhealthy" containers would only cost the other engines their swarms.
    #[tokio::test]
    async fn an_engine_blocked_by_the_kill_switch_is_degraded_not_unhealthy() {
        let (s, _) = three_engines("obs-health-blocked");
        let vpn = s.state.engines.get("vpn7").unwrap();
        vpn.blocked.set("not assigned to a tunnel".into()).unwrap();
        let (st, body) = get(&s, "/health").await;
        assert_eq!(st, StatusCode::OK, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["status"], "degraded", "{v:#}");
        let p = &v["problems"][0];
        assert_eq!(p["engine"], "vpn7");
        assert!(p["detail"].as_str().unwrap().contains("kill switch"), "{v:#}");
        let e = v["checks"]["engines"].as_array().unwrap().iter().find(|e| e["id"] == "vpn7").unwrap().clone();
        assert_eq!(e["blocked"], "not assigned to a tunnel");
        assert_eq!(e["online"], false, "never put on the network");
    }

    /// #89: a torrent whose data is gone is counted and listed, from the
    /// background pass; nothing is a constant 0 any more.
    #[tokio::test]
    async fn anomalies_count_what_the_pass_found() {
        let (s, hashes) = three_engines("obs-anomalies");
        let vpn = s.state.engines.get("vpn7").unwrap();
        let ih = typhon_engine::torrent::hex_decode(&hashes[2]).unwrap();
        let t = vpn.manager.get(&ih).unwrap();
        t.status.store(typhon_engine::torrent::meta::TorrentStatus::Error as u8, Relaxed);
        crate::workers::health_pass(&s.state.engines);

        let (st, body) = get(&s, "/api/health/anomalies").await;
        assert_eq!(st, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["scan"], "done");
        assert!(v["counts"]["files_missing"].as_i64().unwrap() >= 1, "{v}");
        assert!(
            v["anomalies"].as_array().unwrap().iter().any(|a| a["info_hash"] == hashes[2] && a["type"] == "files_missing"),
            "{v}"
        );
        for gone in ["persistent_counters", "goroutines", "gc_cpu_pct", "orphan_files", "ghost_files", "errors"] {
            assert!(v.get(gone).is_none(), "{gone} was a constant and is removed");
        }
        assert!(v["engines"]["vpn7"]["states"].is_object(), "every engine, the extra one included");
        assert!(v["disks"].as_array().is_some_and(|d| !d.is_empty()), "data_dir's filesystem at least");
    }

    /// #92: tracker health covers every engine, and a tracker failing on two
    /// engines is one tracker in the badge.
    #[tokio::test]
    async fn announce_health_covers_extra_engines_and_counts_trackers_once() {
        let (s, _) = three_engines("obs-trackers");
        for id in ["race", "vpn7"] {
            s.state.engines.get(id).unwrap().announce_cache.count_failed_kind("tracker.example", "timeout");
        }
        let (st, body) = get(&s, "/api/announce/health").await;
        assert_eq!(st, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(v["vpn7"]["hosts"]["tracker.example"].is_object(), "the extra engine is in: {v}");
        assert_eq!(v["badges"]["trackers_amber"], 1, "one tracker, not one per engine");

        let (_, body) = get(&s, "/api/announce/errors?host=tracker.example").await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let engines: Vec<&str> = v["engines"].as_array().unwrap().iter().filter_map(|e| e["engine"].as_str()).collect();
        assert!(engines.contains(&"vpn7"), "{v}");
    }

    /// #92: the Overview's per-engine cards and the Benchmark's live cards
    /// have every engine to draw.
    #[tokio::test]
    async fn engines_and_bench_current_list_every_engine() {
        let (s, _) = three_engines("obs-engines");
        let (_, body) = get(&s, "/api/engines").await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let vpn = v.as_array().unwrap().iter().find(|e| e["id"] == "vpn7").expect("vpn7 listed").clone();
        assert_eq!(vpn["stats"]["torrents"], 1);
        assert_eq!(vpn["stats"]["states"]["seeding"], 1);

        let (_, body) = get(&s, "/api/benchmark/current").await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let ids: Vec<&str> = v["engines"].as_array().unwrap().iter().filter_map(|e| e["id"].as_str()).collect();
        assert_eq!(ids, ["race", "hoard", "vpn7"]);
    }

    /// #85: the logs route filters on the server, and refuses a filter it
    /// cannot read rather than ignoring it.
    #[tokio::test]
    async fn the_logs_route_filters() {
        let (s, _) = three_engines("obs-logs");
        for (level, module, msg) in [("INFO", "hydranos::store", "store open"), ("ERROR", "hydranos::announce::runner", "boom")] {
            s.state.logs.push(crate::logbuf::Entry {
                ts: String::new(),
                source: "rust".into(),
                level: level.into(),
                module: module.into(),
                msg: msg.into(),
                seq: 0,
                unix: 2_000_000_000,
            });
        }
        let (_, body) = get(&s, "/api/logs?level=ERROR").await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let msgs: Vec<&str> = v["entries"].as_array().unwrap().iter().filter_map(|e| e["msg"].as_str()).collect();
        assert_eq!(msgs, ["boom"]);
        assert!(v["modules"].as_array().unwrap().iter().any(|m| m == "hydranos::store"));
        assert_eq!(v["last_seq"], 2);

        let (_, body) = get(&s, "/api/logs?module=store&q=OPEN").await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["entries"].as_array().unwrap().len(), 1);

        let (st, _) = get(&s, "/api/logs?since=forever").await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn label_values_are_escaped() {
        assert_eq!(label(r#"a"b\c"#), r#"a\"b\\c"#);
        assert_eq!(label("x\ny"), "x\\ny");
    }

    #[test]
    fn a_family_has_help_type_and_samples() {
        let mut x = Exposition::new();
        x.family("m", "gauge", "Help.", &[(vec![("engine", "race".into())], 3.0), (vec![], 0.5)]);
        x.family("empty", "gauge", "Never printed.", &[]);
        let out = x.finish();
        assert_eq!(out, "# HELP m Help.\n# TYPE m gauge\nm{engine=\"race\"} 3\nm 0.5\n");
    }
}
