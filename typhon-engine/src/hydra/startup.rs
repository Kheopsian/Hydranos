//! The daemon's startup, as something a caller can watch.
//!
//! Until 4.4 the HTTP port opened only once every engine had loaded its
//! catalogue: minutes at a million torrents, during which a browser got
//! "connection refused" and the page's startup screen -- a progress bar fed
//! by `/api/startup` -- could never be shown. The route existed and always
//! answered `ready: true`, because nothing could ask it anything earlier.
//!
//! Now the port opens first, in front of a gate. Until the real router is
//! handed over, the gate serves the page, its assets, `/health`,
//! `/api/startup` and `/api/setup`, and answers everything else with a 503
//! that says how far the load has got. The phase and the per-engine restore
//! counts below are what those answers are made of.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use typhon_engine::torrent::TorrentManager;

/// Where the startup is. Ordered: each phase comes after the previous one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Phase {
    /// The engines read their resume records and rebuild the catalogue.
    Loading = 0,
    /// Listeners, tunnels and the announcers come up.
    Connecting = 1,
    /// `hydra.db` is opened and checked.
    OpeningStore = 2,
    /// The background workers start; the router is about to be served.
    StartingWorkers = 3,
    Ready = 4,
    /// The store could not be opened; only the rescue surface is served.
    Rescue = 5,
}

impl Phase {
    pub fn name(self) -> &'static str {
        match self {
            Phase::Loading => "loading",
            Phase::Connecting => "connecting",
            Phase::OpeningStore => "opening_store",
            Phase::StartingWorkers => "starting_workers",
            Phase::Ready => "ready",
            Phase::Rescue => "rescue",
        }
    }

    fn from_u8(v: u8) -> Phase {
        match v {
            1 => Phase::Connecting,
            2 => Phase::OpeningStore,
            3 => Phase::StartingWorkers,
            4 => Phase::Ready,
            5 => Phase::Rescue,
            _ => Phase::Loading,
        }
    }
}

static PHASE: AtomicU8 = AtomicU8::new(Phase::Loading as u8);
static STARTED: OnceLock<i64> = OnceLock::new();
/// The engines being restored, in load order. Registered before each one
/// reads its records, so a reader sees the engine as soon as it starts.
static LOADING: Mutex<Vec<(String, Arc<TorrentManager>)>> = Mutex::new(Vec::new());

pub fn set_phase(p: Phase) {
    PHASE.store(p as u8, Ordering::SeqCst);
}

pub fn phase() -> Phase {
    Phase::from_u8(PHASE.load(Ordering::SeqCst))
}

/// Unix second the process started at. The first call fixes it; `main`
/// makes that call before anything slow.
pub fn started_at() -> i64 {
    *STARTED.get_or_init(now_secs)
}

/// Whole seconds since the process started, load included.
pub fn uptime_secs() -> i64 {
    (now_secs() - started_at()).max(0)
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// An engine about to restore its catalogue. Replaces an earlier entry of
/// the same id (a test building engines twice).
pub fn register(id: &str, manager: Arc<TorrentManager>) {
    let mut g = LOADING.lock().unwrap_or_else(|e| e.into_inner());
    g.retain(|(i, _)| i != id);
    g.push((id.to_string(), manager));
}

/// One engine's restore, as published.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct EngineRestore {
    pub id: String,
    pub restored: usize,
    pub total: usize,
}

/// Every registered engine's restore so far.
pub fn restores() -> Vec<EngineRestore> {
    LOADING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|(id, m)| {
            let (done, total) = m.restore_progress();
            EngineRestore { id: id.clone(), restored: done.min(total), total }
        })
        .collect()
}

/// The `/api/startup` answer before the router is served.
///
/// `total` only counts engines that have started reading: the next engine's
/// record count is not known until it opens its records, so the bar can
/// step back once per engine. Said here rather than hidden behind a guess.
pub fn snapshot() -> serde_json::Value {
    let engines = restores();
    let total: usize = engines.iter().map(|e| e.total).sum();
    let restored: usize = engines.iter().map(|e| e.restored).sum();
    let phase = phase();
    serde_json::json!({
        "ready": phase == Phase::Ready,
        "phase": phase.name(),
        "total": total,
        "restored": restored,
        "engines": engines,
        "uptime": uptime_secs(),
    })
}

/// What the gate needs to know to draw the page before the state exists.
#[derive(Clone)]
pub struct Early {
    /// No local engine: the page drops its engine panels.
    pub front_only: bool,
    pub needs_setup: bool,
    pub network_storage: &'static str,
}

/// The router the listener serves from the first second.
///
/// Until `slot` is filled, requests go to the startup surface; afterwards,
/// each one goes to the router in the slot. A `Router` clone is a reference
/// count, so the hand-over costs one atomic load per request.
pub fn gate(slot: Arc<OnceLock<Router>>, early: Early) -> Router {
    let starting = starting_router(early);
    Router::new().fallback(move |req: Request<Body>| {
        let slot = slot.clone();
        let starting = starting.clone();
        async move {
            use tower::ServiceExt;
            let router = slot.get().cloned().unwrap_or(starting);
            match router.oneshot(req).await {
                Ok(r) => r,
                Err(never) => match never {},
            }
        }
    })
}

/// What is served while the daemon starts.
fn starting_router(early: Early) -> Router {
    let page = early.front_only;
    let setup = serde_json::json!({
        "needs_setup": early.needs_setup,
        "network_storage": early.network_storage,
        "store_repair": false,
    });
    Router::new()
        .route("/", get(move || async move { crate::web::page(page) }))
        .route("/static/*path", get(crate::web::static_file))
        .route("/health", get(|| async { starting_health() }))
        .route("/metrics", get(|| async { starting_metrics() }))
        .route("/api/startup", get(|| async { Json(snapshot()) }))
        .route("/api/setup", get(move || {
            let setup = setup.clone();
            async move { Json(setup) }
        }))
        .fallback(|| async { not_yet() })
}

/// `/health` while starting: 200, with `"status": "starting"`.
///
/// 200 and not 503, deliberately. A restore that is making progress is the
/// process doing its job, and a 1.1M-torrent catalogue takes minutes: an
/// orchestrator that restarts "unhealthy" containers (autoheal, a Swarm
/// service, a Kubernetes liveness probe) would kill every start before it
/// finished and loop forever. A caller that wants "ready" reads the `status`
/// word or `/api/startup`'s `ready`; a 503 is kept for a daemon that is up
/// and broken.
fn starting_health() -> Response {
    Json(serde_json::json!({
        "status": "starting",
        "version": crate::api::HYDRANOS_VERSION,
        "uptime": uptime_secs() as f64,
        "startup": snapshot(),
    }))
    .into_response()
}

/// `/metrics` while starting: up, and how far each engine's restore is. A
/// scrape that got "connection refused" for minutes read as a dead target.
fn starting_metrics() -> Response {
    let mut x = crate::obs::Exposition::new();
    x.family("hydra_up", "gauge", "1 while the process answers.", &[(vec![], 1.0)]);
    x.family(
        "hydra_uptime_seconds",
        "gauge",
        "Seconds since the process started, the catalogue load included.",
        &[(vec![], uptime_secs() as f64)],
    );
    x.family("hydra_starting", "gauge", "1 while the catalogue loads.", &[(vec![], 1.0)]);
    let rs = restores();
    let restored: Vec<_> = rs.iter().map(|r| (vec![("engine", r.id.clone())], r.restored as f64)).collect();
    let total: Vec<_> = rs.iter().map(|r| (vec![("engine", r.id.clone())], r.total as f64)).collect();
    x.family("hydra_startup_restored", "gauge", "Resume records gone through so far.", &restored);
    x.family("hydra_startup_records", "gauge", "Resume records to go through.", &total);
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], x.finish()).into_response()
}

/// Everything else, until the router is in place.
fn not_yet() -> Response {
    let s = snapshot();
    let msg = format!(
        "Hydranos is starting ({}): {} of {} torrents restored",
        s["phase"].as_str().unwrap_or(""),
        s["restored"],
        s["total"],
    );
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::RETRY_AFTER, "5")],
        Json(serde_json::json!({"error": msg, "startup": s})),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;

    fn early() -> Early {
        Early { front_only: false, needs_setup: false, network_storage: "" }
    }

    async fn call(r: &Router, path: &str) -> (StatusCode, serde_json::Value) {
        let resp = r
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
    }

    /// The gate answers from the first second, with the real figures, and
    /// hands every request to the real router once it is in the slot.
    #[tokio::test]
    async fn the_gate_reports_the_load_then_hands_over() {
        let manager = Arc::new(TorrentManager::new(
            "/nonexistent-startup-test".into(),
            "/nonexistent-startup-test/resume".into(),
            Arc::new(typhon_engine::disk::DiskManager::new(4)),
        ));
        manager.restore_total.store(1000, Ordering::Relaxed);
        manager.restore_done.store(250, Ordering::Relaxed);
        register("startup-test-engine", manager);

        let slot: Arc<OnceLock<Router>> = Arc::new(OnceLock::new());
        let gate = gate(slot.clone(), early());

        let (st, body) = call(&gate, "/api/startup").await;
        assert_eq!(st, StatusCode::OK);
        let mine = body["engines"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["id"] == "startup-test-engine")
            .cloned()
            .unwrap();
        assert_eq!(mine["restored"], 250);
        assert_eq!(mine["total"], 1000);

        // Starting is alive, not unhealthy: see `starting_health`.
        let (st, body) = call(&gate, "/health").await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(body["status"], "starting");

        // Anything else says it is not there yet, and how far it got.
        let (st, body) = call(&gate, "/api/status").await;
        assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body["error"].as_str().unwrap().contains("starting"), "{body}");
        assert!(body.get("version").is_none(), "a deploy script waits for /api/status to carry a version");

        let (st, body) = call(&gate, "/api/setup").await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(body["store_repair"], false);

        let _ = slot.set(Router::new().route("/api/status", get(|| async { Json(serde_json::json!({"version": "x"})) })));
        let (st, body) = call(&gate, "/api/status").await;
        assert_eq!(st, StatusCode::OK, "the real router answers once it is in place");
        assert_eq!(body["version"], "x");
    }

    #[test]
    fn phases_round_trip() {
        for p in [Phase::Loading, Phase::Connecting, Phase::OpeningStore, Phase::StartingWorkers, Phase::Ready, Phase::Rescue] {
            assert_eq!(Phase::from_u8(p as u8), p);
        }
    }
}
