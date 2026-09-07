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

mod api;
mod engines;
mod logbuf;
mod qbitrow;
mod row;
mod speedtest;
mod store;
mod tomledit;
mod trackeredit;
mod walrepair;
mod web;
mod benchdb;
mod bootstrap;
mod announce;
mod health;
mod importer;
mod jobs;
mod wgtun;
mod workers;
mod portfwd;
mod raceevents;
mod reconnect;
mod config;

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
    let config = Config::load(&config_path)?;

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

    let state = api::AppState {
        imports: Default::default(),
        config: Arc::new(std::sync::RwLock::new(Arc::new(config))),
        engines: engine_host,
        store: shared_store.clone(),
        public_ip: Arc::new(tokio::sync::Mutex::new((String::new(), String::new()))),
        started_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
        logs,
        reconnect: Default::default(),
        config_path: config_path.clone(),
        update_check: Arc::new(tokio::sync::Mutex::new(None)),
        bench,
    };
    let app = api::router(state);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(%addr, "hydra API listening");
    axum::serve(listener, app).await?;
    Ok(())
}
