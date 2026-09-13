//! hydra -- the unified daemon.
//!
//! 4.0.0 replaces two processes with one. The Go front and the Rust engine used
//! to exchange every torrent's state over a local socket, which meant the front
//! held a full second copy of it: measured at 1.62 GiB of live Go heap for
//! 243k torrents, 3.88 GiB of RSS once the collector's headroom is counted, and
//! growing at 6.6 KB per torrent. On the way to a million torrents that copy is
//! the wall, not the hardware.
//!
//! The port proceeds one slice of routes at a time. Every slice is compared to
//! the Go binary with tools/paritydiff, running both against the same frozen
//! store, before the next one begins.

use std::path::PathBuf;
use std::sync::Arc;

// Per BINARY, not per crate. `typhon-engine`'s main.rs carries this attribute;
// this binary was written beside it in 4.0.0 without it, so every 4.x release
// up to 4.4.1 ran on glibc malloc while MALLOC_CONF sat inert in the
// environment. See allocdiag for what that cost.
#[cfg(not(windows))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

mod allocdiag;
mod api;
mod engines;
mod errclass;
mod logbuf;
mod qbitrow;
mod row;
mod speedtest;
mod dedup;
mod store;
mod tomledit;
mod trackeredit;
mod walrepair;
mod web;
mod benchdb;
mod benchsampler;
mod netprobe;
mod nodes;
mod bootstrap;
mod announce;
mod health;
mod importer;
mod jobs;
mod jobsrun;
mod wgtun;
mod volumes;
mod workers;
mod portfwd;
mod raceevents;
mod reconnect;
mod config;
mod rules;
mod rulesrun;
mod rulesapi;
mod session;

use config::Config;

fn parse_args() -> PathBuf {
    let mut args = std::env::args().skip(1);
    let mut path = PathBuf::from("/config/default.toml");
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                if let Some(value) = args.next() {
                    path = PathBuf::from(value);
                }
            }
            "--version" => {
                println!("hydra {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            _ => {}
        }
    }
    path
}

/// Serve the rescue surface and nothing else.
async fn rescue(
    store_path: &std::path::Path,
    config_path: &std::path::Path,
    why: &str,
) -> anyhow::Result<()> {
    // A diagnosis that itself fails must still carry the PATH: that is the one
    // thing the operator needs, and defaulting it away leaves the rescue screen
    // saying "something is wrong with ''". The reason is logged separately.
    let diagnosis = walrepair::diagnose(store_path).unwrap_or_else(|e| {
        tracing::warn!("could not diagnose the store: {e}");
        crate::walrepair::Diagnosis {
            path: store_path.display().to_string(),
            ..Default::default()
        }
    });
    tracing::error!(
        store = %store_path.display(),
        needs_repair = diagnosis.needs_repair(),
        on_network = diagnosis.on_network,
        hot_wal = diagnosis.hot_wal,
        "the store could not be opened: {why} -- starting in rescue mode"
    );

    let state = api::RescueState {
        diagnosis,
        config_path: config_path.to_path_buf(),
    };
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8199").await?;
    axum::serve(listener, api::rescue_router(state)).await?;
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Before anything allocates in anger: the signal handlers and the
    // five-minute stats line are the only instruments that can tell a real
    // leak from pages the allocator is holding.
    allocdiag::spawn();

    // Every event goes both to stderr and to the in-memory ring the Logs tab
    // reads. Registering the ring as a layer rather than scraping stderr keeps
    // the level and the message as fields instead of a line to re-parse.
    let logs = logbuf::LogBuffer::new();
    {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        tracing_subscriber::registry()
            .with(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "info".into()),
            )
            .with(tracing_subscriber::fmt::layer())
            .with(logbuf::LogLayer { buffer: logs.clone() })
            .init();
    }

    let config_path = parse_args();
    let mut config = Config::load(&config_path)?;
    // Before anything is served: an install with no key of its own would
    // otherwise answer every caller who sends no key. See config::ensure_api_key.
    config::ensure_api_key(&mut config, &config_path);
    let config = config;

    let host = if config.daemon.api_host.is_empty() {
        "0.0.0.0".to_string()
    } else {
        config.daemon.api_host.clone()
    };
    let port = if config.daemon.api_port == 0 { 8199 } else { config.daemon.api_port };
    let addr = format!("{host}:{port}");

    // The engines live here now, not in a child process behind a socket.
    let config_dir = config_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("/config"))
        .to_path_buf();
    // Before any engine builds its peer id: the fingerprint's four characters
    // ARE the version to every client that decodes them, and ours said 2.4.3.0
    // on a 4.x daemon for the whole life of the project.
    typhon_engine::config::set_version(api::HYDRA_VERSION);
    let engine_host = Arc::new(engines::EngineHost::start(&config, &config_dir).await);

    // Same file 3.x writes: the store is what makes the switch reversible.
    let cfg_data_dir = config.daemon.data_dir.clone();
    let store_path = std::path::Path::new(&cfg_data_dir).join("hydra.db");
    // A store that will not open is not a reason to die silently: the daemon
    // comes up in rescue mode instead, serving just enough to explain the
    // problem and offer the fix. See api::rescue_router.
    let store = match store::Store::open(&store_path, false) {
        Ok(store) => match store.check_schema() {
            Ok(()) => store,
            Err(e) => return rescue(&store_path, &config_path, &e.to_string()).await,
        },
        Err(e) => return rescue(&store_path, &config_path, &e.to_string()).await,
    };
    tracing::info!(torrents = engine_host.total_torrents(), "engines up");

    // Telemetry, alongside the store in data_dir. Its absence is survivable:
    // every route that reads it answers empty, exactly as 3.x does when the
    // file cannot be created.
    let bench_path = std::path::Path::new(&cfg_data_dir).join("bench.db");
    let bench = match benchdb::BenchDb::open(&bench_path) {
        Ok(db) => {
            let shared = Arc::new(std::sync::Mutex::new(db));
            raceevents::spawn(engine_host.clone(), shared.clone());
            Some(shared)
        }
        Err(e) => {
            tracing::warn!(path = %bench_path.display(), "no bench database: {e}");
            None
        }
    };

    // Shared before the state is built: the reconcile task needs the same
    // handle the handlers use, not a second connection to the same file.
    let shared_store = Arc::new(std::sync::Mutex::new(store));
    workers::spawn_store_reconcile(engine_host.clone(), shared_store.clone());

    // Index anything added by a build that did not know about the content
    // index. In batches, off the startup path: the pass takes ~45 s on a
    // 300k-torrent catalogue and the store mutex is what every API handler
    // waits on, so doing it in one call would freeze the UI for the duration.
    {
        let store = shared_store.clone();
        std::thread::spawn(move || {
            let mut total = 0usize;
            loop {
                let done = match store.lock() {
                    Ok(s) => s.backfill_content_index(2000),
                    Err(_) => break,
                };
                match done {
                    Ok(0) => break,
                    Ok(n) => {
                        total += n;
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    Err(e) => {
                        tracing::warn!("content index backfill: {e}");
                        break;
                    }
                }
            }
            if total > 0 {
                tracing::info!(indexed = total, "content index backfilled");
            }
        });
    }

    // Put the operator's pauses back into the engines, then start the manager
    // that must not undo them.
    //
    // Both need the store, which is why neither happens where the engines are
    // built: the catalogue comes up from each engine's resume file, which does
    // not carry the intent. Without this a restart silently resumed everything
    // the operator had stopped.
    //
    // `stop_torrent` rather than a flag, because the stagger start may already
    // have started some of them -- it runs from a task spawned moments ago. It
    // is idempotent and self-correcting either way.
    for engine in engine_host.engines() {
        let hashes = match shared_store.lock() {
            Ok(store) => store.paused_hashes(&engine.id).unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        let mut restored = 0usize;
        for hash in &hashes {
            if let Some(info_hash) = store::hex20(hash) {
                if engine.manager.stop_torrent(&info_hash).is_ok() {
                    restored += 1;
                }
            }
        }
        if restored > 0 {
            tracing::info!(engine = %engine.id, restored, "pause: restored user intent");
        }
        workers::spawn_download_slots(
            engine.manager.clone(),
            engine.announce_cache.clone(),
            engine.session.active_downloads,
            shared_store.clone(),
            engine.id.clone(),
        );
        // Here and not in engines.rs for the same reason as the slot manager:
        // the store does not exist yet when the engines are built.
        workers::spawn_seed_time_sync(
            engine.manager.clone(),
            shared_store.clone(),
            engine.id.clone(),
        );
    }

    // The benchmark graphs read what this writes and nothing else does: with no
    // sampler the whole tab is empty while the node is at full throughput.
    if let Some(shared) = bench.clone() {
        benchsampler::spawn(engine_host.clone(), shared, shared_store.clone());
    }

    // Exit addresses for the header. Nothing filled these before, so every IP
    // the interface showed was blank however the node was routed.
    let public_ip: api::PublicIp =
        Arc::new(tokio::sync::Mutex::new((String::new(), String::new())));
    let net_engines: netprobe::Snapshot =
        Arc::new(tokio::sync::Mutex::new((Vec::new(), 0)));
    netprobe::spawn(engine_host.clone(), net_engines.clone(), public_ip.clone());

    // Warm the Records card before anyone asks. The scan takes seconds and the
    // overview header waits on its request, so computing it lazily meant the
    // first page load of every process paid for it.
    let records: api::Records = Default::default();
    if bench.is_some() {
        api::refresh_records(bench_path.clone(), records.clone());
    }

    // The mark that separates "this session" and "today" from "ever". Taken
    // here, once the engines have loaded their resume data: their per-torrent
    // counters are lifetime totals, so without this mark `day_uploaded`
    // publishes the entire history of the library as one day's work.
    let odometer: api::Odo = {
        let (up, down) = engine_host.session_totals();
        Arc::new(std::sync::Mutex::new(api::Odometer {
            session_offset: (up, down),
            day_baseline: (0, 0),
            day_date: String::new(),
        }))
    };

    let state = api::AppState {
        imports: Default::default(),
        config: Arc::new(std::sync::RwLock::new(Arc::new(config))),
        engines: engine_host,
        store: shared_store.clone(),
        public_ip: public_ip.clone(),
        net_engines: net_engines.clone(),
        odometer: odometer.clone(),
        records: records.clone(),
        bench_path: bench_path.clone(),
        started_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
        logs,
        reconnect: Default::default(),
        config_path: config_path.clone(),
        update_check: Arc::new(tokio::sync::Mutex::new(None)),
        bench,
        sessions: Default::default(),
    };
    // Roll the day counters on a timer, not only when somebody asks. 3.x reset
    // on the first request of the new day, so a dashboard opened in the
    // afternoon had been showing yesterday's baseline until that moment.
    {
        let state = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                tick.tick().await;
                let _ = api::session_and_day(&state);
            }
        });
    }

    // The race drain, here rather than in engines.rs: it reads the per-tracker
    // seed obligation out of the live config, and the state that holds it is
    // only built above.
    for engine in state.engines.engines().iter() {
        if engine.role != "race" {
            continue;
        }
        workers::spawn_race_drain(
            state.clone(),
            engine.manager.clone(),
            state.config_handle(),
            engine.id.clone(),
        );
    }

    // The transit sweep runs on EVERY engine, not only the race ones: a
    // graduation target lives in the hoard by definition, so scoping this the
    // way the drain is scoped would mean nothing ever leaves the transit area.
    // What keeps it safe is the category scope, not the engine scope.
    for engine in state.engines.engines().iter() {
        workers::spawn_transit_sweep(
            state.clone(),
            engine.manager.clone(),
            state.config_handle(),
            engine.id.clone(),
        );
    }

    // The job runner. One task, one job at a time -- see the module header for
    // why concurrency buys nothing here.
    jobsrun::spawn(state.clone());

    // The workflow timer, before the router takes ownership of the state.
    // It waits two minutes of its own so it never fires against a catalogue
    // that is still loading.
    rulesapi::spawn(state.clone());

    // Taken before the router consumes the state: `flush_on_shutdown` needs the
    // engines, and by then `state` has been moved.
    let engines_for_shutdown = state.engines.clone();
    let app = api::router(state);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(%addr, "hydra API listening");
    // with_connect_info so a handler can see who dialled it. One thing needs it:
    // a node being handed a torrent has to be told where to fetch it from, and
    // the sender cannot know which of ITS addresses the receiver can reach --
    // tunnels, NAT, several interfaces. The receiver can: it is the address the
    // request arrived from.
    // ⭐⭐ With a shutdown, because for the whole of V4 there was none.
    //
    // `axum::serve(..).await` alone never returns, and this process is PID 1
    // in its container. PID 1 does not get the default disposition of
    // SIGTERM: with no handler installed the signal is simply DISCARDED. So
    // `docker stop -t 300` sent a SIGTERM that nothing received, waited the
    // full five minutes while the daemon kept accepting peers, and then
    // SIGKILLed a 300k-torrent instance. Every V4 deploy went that way, and
    // the log said "arrete" as though it had been graceful.
    //
    // What that cost: resume state is written by a five-minute sweep
    // (`session::start`), so a kill throws away up to five minutes of piece
    // progress and byte counters for every engine, and the next start
    // re-checks what it lost. 3.x saved on the way out; the port dropped it.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;

    flush_on_shutdown(&engines_for_shutdown);
    Ok(())
}

/// Resolve once a termination signal arrives, naming the one that did.
///
/// A handler that cannot be installed leaves its branch pending forever
/// rather than resolving: a failed SIGINT registration must not fake a
/// shutdown, and must not stop SIGTERM from being heard.
#[cfg(unix)]
async fn shutdown_signal() -> () {
    use tokio::signal::unix::{signal, SignalKind};

    let mut term = signal(SignalKind::terminate())
        .map_err(|e| tracing::error!("SIGTERM handler setup failed: {e}"))
        .ok();
    let mut int = signal(SignalKind::interrupt())
        .map_err(|e| tracing::error!("SIGINT handler setup failed: {e}"))
        .ok();

    async fn recv(s: &mut Option<tokio::signal::unix::Signal>) {
        match s {
            Some(sig) => {
                sig.recv().await;
            }
            None => std::future::pending().await,
        }
    }

    let which = tokio::select! {
        _ = recv(&mut term) => "SIGTERM",
        _ = recv(&mut int) => "SIGINT",
    };
    tracing::warn!("{which} received, draining the API and flushing resume data");
}

#[cfg(not(unix))]
async fn shutdown_signal() -> () {
    let _ = tokio::signal::ctrl_c().await;
}

/// Write every engine's resume state before the process ends.
///
/// Bounded, because the alternative to a partial sweep is not a complete one
/// -- it is the SIGKILL that arrives when `docker stop -t N` runs out of
/// patience. Every torrent written before the budget expires is one the next
/// start does not have to re-check, so a sweep that is cut short is still
/// strictly better than no sweep.
///
/// Engines are flushed on threads of their own: one slow disk must not spend
/// another engine's share of the budget.
fn flush_on_shutdown(engines: &std::sync::Arc<engines::EngineHost>) {
    let budget = std::env::var("HYDRA_STOP_TIMEOUT")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(120);
    let budget = std::time::Duration::from_secs(budget);
    let started = std::time::Instant::now();

    let handles: Vec<_> = engines
        .engines()
        .iter()
        .map(|e| {
            let id = e.id.clone();
            let manager = e.manager.clone();
            std::thread::spawn(move || {
                let t = std::time::Instant::now();
                manager.flush_all_resume();
                tracing::info!(engine = %id, took_ms = t.elapsed().as_millis() as u64,
                               "resume data saved");
            })
        })
        .collect();

    for h in handles {
        // No per-thread timeout exists for a std thread, so the budget is
        // enforced by the caller of `docker stop`: this logs how close it came.
        let _ = h.join();
    }
    let took = started.elapsed();
    if took > budget {
        tracing::warn!(took_s = took.as_secs(), budget_s = budget.as_secs(),
                       "shutdown flush overran its budget; raise HYDRA_STOP_TIMEOUT and docker stop -t");
    } else {
        tracing::info!(took_s = took.as_secs(), "shutdown flush complete");
    }
}
