// The bench sample is one `json!` object of ~60 keys; the macro recurses once
// per key and the default limit of 128 no longer covers it.
#![recursion_limit = "256"]
// ⚠ Windows only: without this the daemon is a console program, and
// double-clicking it opens a black window that closes when it does. The 3.x
// package had no such window and this build regressed it. `windows` means "no
// console is created for me"; the startup code below ATTACHES to the terminal
// that launched us when there is one, so running it from PowerShell still
// prints, and --console makes one on purpose.
#![cfg_attr(windows, windows_subsystem = "windows")]
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

use std::path::{Path, PathBuf};
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
mod platform;
mod tray;
mod trayicon;
mod engines;
mod errclass;
mod logbuf;
mod qbitrow;
mod row;
mod speedtest;
mod dedup;
mod deadkeys;
mod export;
mod selection;
mod sharelimits;
mod store;
mod tomledit;
mod trackeredit;
mod trackerlists;
mod spoofmigration;
mod walrepair;
mod web;
mod benchdb;
mod benchsampler;
mod netprobe;
mod netmode;
mod nodes;
mod bootstrap;
mod announce;
mod health;
mod importer;
mod jobs;
mod jobsrun;
mod wgtun;
mod wgtunnel;
mod volumes;
mod workers;
mod portfwd;
mod gluetun;
mod igd;
mod portmap;
mod raceevents;
mod reconnect;
mod config;
mod linkindex;
mod linkscan;
mod rules;
mod rulesrun;
mod rulesapi;
mod magnets;
mod ipfilter;
mod watch;
mod mcp;
mod session;
mod shutdown;

use config::Config;

/// What the command line asked for.
#[derive(Debug, PartialEq)]
struct Args {
    config: PathBuf,
    /// Make a console window even when launched without one.
    console: bool,
    /// One line per argument that was not understood. Printed once the
    /// console is attached: before that, on Windows, stderr goes nowhere.
    warnings: Vec<String>,
}

/// The outcome of reading the command line, kept apart from acting on it so
/// it can be tested without the process exiting under the test.
#[derive(Debug, PartialEq)]
enum Cli {
    Run(Args),
    Version,
    Help,
}

const USAGE: &str = "usage: hydranos [--config <path/to/default.toml>] [--console]
       hydranos --version
       hydranos reset-password <password> [path/to/default.toml]
       hydranos hash-password <password>";

/// The 3.x split-process flags. 4.x is one process: there is no agent or front
/// to start any more, and machines join a fleet as nodes instead.
fn is_legacy_split_flag(name: &str) -> bool {
    name.starts_with("agent-") || name.starts_with("front-")
}

/// The 3.x flags that took a value. Their value is swallowed with them, or a
/// 3.x command line would print a second, more confusing warning about it.
fn legacy_flag_takes_value(name: &str) -> bool {
    matches!(name, "agent-addr" | "agent-token" | "agent-tls-cert" | "agent-tls-key" | "front-addr")
}

/// Read the command line (without the program name).
///
/// ⚠ An argument that is not understood is WARNED about, never fatal. 4.3
/// ignored them in silence, so a 3.x script still passing `--agent-only`
/// started a full daemon and nobody was told; refusing to start now would
/// instead break every such script at its next upgrade. Saying so on stderr
/// and starting anyway is the one choice that both informs and keeps running.
fn parse_argv<I: IntoIterator<Item = String>>(argv: I, default_config: PathBuf) -> Cli {
    let mut args = argv.into_iter().peekable();
    let mut path = default_config;
    let mut console = false;
    let mut warnings = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--console" => console = true,
            "--config" => match args.next() {
                Some(value) => path = PathBuf::from(value),
                None => warnings.push(format!(
                    "--config needs a path; using {}", path.display()
                )),
            },
            a if a.starts_with("--config=") => path = PathBuf::from(&a["--config=".len()..]),
            // CARGO_PKG_VERSION is 0.1.0 and has never been bumped: the
            // release number lives in HYDRANOS_VERSION. See parse_args.
            "--version" | "-V" => return Cli::Version,
            "--help" | "-h" => return Cli::Help,
            other => {
                // Go's flag package, which 3.x used, took -flag and --flag
                // alike and allowed --flag=value: old scripts carry all three.
                let name = other.trim_start_matches('-');
                let (name, inline_value) = match name.split_once('=') {
                    Some((n, _)) => (n, true),
                    None => (name, false),
                };
                if other.starts_with('-') && is_legacy_split_flag(name) {
                    if !inline_value && legacy_flag_takes_value(name) {
                        if let Some(next) = args.peek() {
                            if !next.starts_with('-') {
                                args.next();
                            }
                        }
                    }
                    warnings.push(format!(
                        "ignoring {other}: the agent/front split was removed in 4.x and Hydranos \
                         now runs as one process. Starting a normal instance. To spread torrents \
                         over several machines, enrol each one as a node instead \
                         (install.sh --register-to <url> --token <token>; the Nodes page \
                         gives the full command)."
                    ));
                } else {
                    warnings.push(format!(
                        "ignoring unknown argument {other:?}; starting anyway (see hydranos --help)"
                    ));
                }
            }
        }
    }
    Cli::Run(Args { config: path, console, warnings })
}

/// `hydranos reset-password <password> [config]` and `hydranos hash-password
/// <password>`: the recovery the first-run screen and the packaging point to.
/// 4.3 had neither, and an unknown argument just started the daemon.
fn password_command() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = argv.first().map(String::as_str) else { return };
    if cmd != "reset-password" && cmd != "hash-password" {
        return;
    }
    let Some(password) = argv.get(1) else {
        eprintln!("usage: hydranos {cmd} <password>{}", if cmd == "reset-password" { " [path/to/default.toml]" } else { "" });
        std::process::exit(2);
    };
    if password.chars().count() < 8 {
        eprintln!("hydranos: the password must be at least 8 characters");
        std::process::exit(2);
    }
    let hash = match bcrypt::hash(password, bcrypt::DEFAULT_COST) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("hydranos: cannot hash the password: {e}");
            std::process::exit(1);
        }
    };
    if cmd == "hash-password" {
        println!("{hash}");
        std::process::exit(0);
    }
    let path = argv
        .iter()
        .skip(2)
        .find(|a| a.as_str() != "--config")
        .map(PathBuf::from)
        .unwrap_or_else(default_config_path);
    let doc = match std::fs::read_to_string(&path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("hydranos: cannot read {}: {e}", path.display());
            std::process::exit(1);
        }
    };
    let pairs = vec![("password_hash".to_string(), tomledit::quote_toml_key(&hash))];
    match tomledit::set_toml_table(&doc, "auth", &pairs) {
        Ok(out) if toml::from_str::<Config>(&out).is_ok() => {
            if let Err(e) = std::fs::write(&path, out) {
                eprintln!("hydranos: cannot write {}: {e}", path.display());
                std::process::exit(1);
            }
            println!("hydranos: password set in {}. Restart Hydranos for it to apply.", path.display());
            std::process::exit(0);
        }
        _ => {
            eprintln!("hydranos: {} would not parse after the edit; nothing written", path.display());
            std::process::exit(1);
        }
    }
}

fn parse_args() -> Args {
    password_command();
    match parse_argv(std::env::args().skip(1), default_config_path()) {
        Cli::Run(args) => args,
        Cli::Version => {
            // CARGO_PKG_VERSION is 0.1.0 and has never been bumped: the
            // release number lives in HYDRANOS_VERSION, which is what the
            // API, the changelog and the CI guard all agree on. This
            // printed "hydra 0.1.0" -- the old name and a version no
            // release has ever carried.
            println!("hydranos {}", api::HYDRANOS_VERSION);
            std::process::exit(0);
        }
        Cli::Help => {
            println!("{USAGE}");
            std::process::exit(0);
        }
    }
}

/// Where the config lives when `--config` is not given.
///
/// On Windows, beside the executable, as the Windows README says ("writes
/// `default.toml` ... beside itself"). 4.3 used `/config/default.toml` on
/// every platform, which Windows resolves to `\config` on the CURRENT drive:
/// a double-click from Explorer and a launch from a shell on another drive
/// read two different configs.
fn default_config_path() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
            let beside = dir.join("default.toml");
            // An install that 4.3 seeded at \config keeps it: starting on a
            // fresh config would look like every torrent and setting vanished.
            let legacy = PathBuf::from("/config/default.toml");
            if !beside.exists() && legacy.exists() {
                eprintln!(
                    "hydranos: using {} (the 4.3 location); move it beside hydranos.exe or pass --config",
                    legacy.display()
                );
                return legacy;
            }
            return beside;
        }
    }
    PathBuf::from("/config/default.toml")
}

/// Attach to the terminal that launched us, or make one when asked.
///
/// With `windows_subsystem = "windows"` the process starts with NO standard
/// handles at all. `AttachConsole(ATTACH_PARENT_PROCESS)` gives them back when
/// a terminal launched us -- so `hydranos.exe` in PowerShell prints its log as
/// any console program would -- and fails harmlessly when nothing launched us
/// from a console, which is the double-click case the 3.x package handled by
/// having no window either.
///
/// ⚠ Attaching is not enough on its own: the C runtime opened stdout before
/// main ran and still points at nothing. The handles are reopened onto CONOUT$
/// so `println!` reaches the window.
#[cfg(windows)]
fn attach_console(force: bool) {
    use windows_sys::Win32::System::Console::{AllocConsole, AttachConsole, ATTACH_PARENT_PROCESS};
    let attached = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) } != 0;
    if !attached && force {
        unsafe { AllocConsole() };
    } else if !attached {
        return;
    }
    // Point the standard streams at the console we now have.
    //
    // ⚠ The handle is deliberately NEVER closed. Wrapping it in a File "so it
    // does not leak" closes it when that File drops -- and the freed handle
    // NUMBER is then reused by the next CreateFile, which is hydranos.log.
    // stdout silently became the log file, so the console layer and the file
    // layer both wrote there and every line appeared twice. The process owns
    // this handle for its whole life; that is not a leak, it is stdout.
    unsafe {
        use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileA, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        };
        use windows_sys::Win32::System::Console::{
            SetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
        };
        let h = CreateFileA(
            c"CONOUT$".as_ptr() as *const u8,
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        );
        if h != INVALID_HANDLE_VALUE {
            SetStdHandle(STD_OUTPUT_HANDLE, h);
            SetStdHandle(STD_ERROR_HANDLE, h);
        }
    }
}

#[cfg(not(windows))]
fn attach_console(_force: bool) {}

/// Serve the rescue surface and nothing else.
async fn rescue(
    store_path: &std::path::Path,
    config_path: &std::path::Path,
    addr: &str,
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
    // The address the operator configured, as in normal mode: 4.3 bound
    // 0.0.0.0:8199 here whatever api_host said, so an instance kept on
    // loopback became reachable from the network exactly when it was broken.
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, api::rescue_router(state)).await?;
    Ok(())
}

/// Write a starting config when there is none.
///
/// ⚠ The daemon used to exit with "reading <path>: No such file or directory"
/// on a fresh install. Only the container ever worked, because entrypoint.sh
/// seeds the file itself -- so the documented Windows route ("unzip and run")
/// and every bare-metal Linux install hit a wall the maintainers never saw.
/// The template is the one shipped in configs/, compiled in so the binary
/// needs nothing beside it.
fn seed_config(path: &Path) {
    if path.exists() {
        return;
    }
    let Some(dir) = path.parent() else { return };
    if !dir.as_os_str().is_empty() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("hydranos: cannot create {}: {e}", dir.display());
            return;
        }
    }
    // data_dir follows the config rather than staying at the Linux default:
    // "/config" is not writable on Windows and does not exist on a bare-metal
    // Linux box either.
    let data_dir = if dir.as_os_str().is_empty() {
        PathBuf::from("data")
    } else {
        dir.join("data")
    };
    let template = include_str!("../../../configs/default.toml");
    let seeded = template.replace(
        "data_dir = \"/config\"",
        &format!("data_dir = \"{}\"", data_dir.display().to_string().replace('\\', "/")),
    );
    match std::fs::write(path, seeded) {
        Ok(()) => eprintln!("hydranos: wrote a starting config at {}", path.display()),
        Err(e) => eprintln!("hydranos: cannot write {}: {e}", path.display()),
    }
}

/// The on-disk log, beside the config. None when the operator asked for stdout
/// only, or when the file cannot be opened -- a daemon that will not start
/// because its log file is read-only would be a poor trade.
fn log_file(config_path: &Path) -> Option<RotatingLog> {
    if std::env::var_os("HYDRANOS_LOG_STDOUT").is_some() {
        return None;
    }
    let dir = config_path.parent().filter(|d| !d.as_os_str().is_empty());
    let path = match dir {
        Some(d) => d.join("hydranos.log"),
        None => PathBuf::from("hydranos.log"),
    };
    match RotatingLog::open(path.clone()) {
        Ok(f) => Some(f),
        Err(e) => {
            eprintln!("hydranos: no log file at {} ({e}); console only", path.display());
            None
        }
    }
}

fn main() -> anyhow::Result<()> {
    // Explicit runtime instead of #[tokio::main]: the macro's default is one
    // worker per core, which on a 128-core host is ~4x more workers than this
    // load needs and costs 13% of all CPU in work-stealing. See
    // typhon_engine::runtime.
    let workers = typhon_engine::runtime::worker_threads();
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()?
        .block_on(async_main(workers))
}

async fn async_main(workers: usize) -> anyhow::Result<()> {
    // Before anything allocates in anger: the signal handlers and the
    // five-minute stats line are the only instruments that can tell a real
    // leak from pages the allocator is holding.
    allocdiag::spawn();

    // Every event goes both to stderr and to the in-memory ring the Logs tab
    // reads. Registering the ring as a layer rather than scraping stderr keeps
    // the level and the message as fields instead of a line to re-parse.
    // ⚠ The command line is read BEFORE logging starts, not after: the log
    // file lives next to the config, so there is no file to open until the
    // config path is known. Initialising the subscriber first is what used to
    // send every startup line to a console that may not exist.
    let args = parse_args();
    attach_console(args.console);
    for w in &args.warnings {
        eprintln!("hydranos: {w}");
    }
    let config_path = args.config;
    seed_config(&config_path);

    let logs = logbuf::LogBuffer::new();
    {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "info".into());
        let registry = tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer())
            .with(logbuf::LogLayer { buffer: logs.clone() });

        // A second copy on disk, beside the config. Without it a daemon
        // started with no console -- a service, a shortcut, the Windows
        // double-click -- keeps no record of why it would not start.
        // HYDRANOS_LOG_STDOUT means "the console copy is enough", and no file
        // is written at all.
        match log_file(&config_path) {
            Some(file) => registry
                .with(
                    tracing_subscriber::fmt::layer()
                        // No colour: this is read in Notepad, and escape
                        // codes there are line noise.
                        .with_ansi(false)
                        .with_writer(move || file.clone()),
                )
                .init(),
            None => registry.init(),
        }
    }

    tracing::info!("tokio runtime: {} worker threads", workers);
    // Read now, not only at the stop: a typo here would otherwise surface
    // for the first time in the last log lines of a shutdown nobody watches.
    let stop_budget = stop_budget();

    let mut config = Config::load(&config_path)?;
    // Before anything is served: an install with no key of its own would
    // otherwise answer every caller who sends no key. See config::ensure_api_key.
    config::ensure_api_key(&mut config, &config_path);
    let config = config;
    // Keys the file holds and nothing reads: said once, never fatal -- an old
    // config must still start, but its owner must not believe it acts.
    deadkeys::warn_config(&config_path);

    // ⚠ Create data_dir HERE, before anything opens a database in it. SQLite
    // makes the FILE but never the DIRECTORY: a data_dir that does not exist
    // fails with "unable to open database file", and the engines -- which
    // start below and read the store themselves -- fall back to the .torrent
    // files while the daemon drops into rescue mode.
    //
    // ⚠ Placing this next to store_path (some 25 lines down) is too late:
    // EngineHost::start runs first and has already failed by then. That is
    // exactly the mistake this comment exists to stop someone repeating.
    //
    // The container never hit any of it, because entrypoint.sh does its own
    // mkdir -p. Every bare-metal install did, on Linux as much as on Windows.
    if let Err(e) = std::fs::create_dir_all(&config.daemon.data_dir) {
        tracing::warn!("could not create data_dir {}: {}", config.daemon.data_dir, e);
    }

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
    // ARE the version to every client that decodes them, and ours said 2.4.3.0 // leak-ok: a version
    // on a 4.x daemon for the whole life of the project.
    typhon_engine::config::set_version(api::HYDRANOS_VERSION);
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
            Err(e) => return rescue(&store_path, &config_path, &addr, &e.to_string()).await,
        },
        Err(e) => return rescue(&store_path, &config_path, &addr, &e.to_string()).await,
    };
    tracing::info!(torrents = engine_host.total_torrents(), "engines up");
    // Categories live in the store, not the TOML, so their 3.x routing fields
    // are checked here. Same two sources as api::categories_map, same order.
    {
        let doc = store
            .meta_doc("categories")
            .filter(|d| !d.is_empty())
            .or_else(|| std::fs::read_to_string(std::path::Path::new(&cfg_data_dir).join("categories.json")).ok());
        deadkeys::warn_categories(doc.as_deref());
    }

    // Get the listen port forwarded. `portfwd` has been able to do this since
    // it was written and was never once called -- `mod portfwd;` and no call
    // site -- while the interface told Proton users their port was obtained by
    // NAT-PMP and renewed continuously. One port per engine, deduplicated:
    // race and hoard listen on different ones.
    if config.auto_port_forward {
        let mut asked = std::collections::BTreeSet::new();
        for engine in engine_host.engines() {
            // A tunnelled engine's port is asked of its tunnel's gateway
            // (`portfwd::spawn_follower`); a home-router mapping for it
            // would open a port the engine does not even listen on.
            if engine_host.wireguard().get(&engine.id).is_some() {
                continue;
            }
            if engine.listen_port != 0 && asked.insert(engine.listen_port) {
                portmap::spawn(engine.listen_port);
            }
        }
    }

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
    // In WAL, a read-only connection beside the shared one lets the long
    // reads through without holding any write up; cf `StoreLock`.
    let wal = store.journal_mode() == "wal";
    let reader = if wal {
        match store::Store::open(&store_path, true) {
            Ok(r) => Some(r),
            Err(e) => {
                tracing::warn!("no read connection, reads share the writer's: {e}");
                None
            }
        }
    } else {
        None
    };
    if wal {
        store::spawn_checkpointer(&store_path);
    }
    tracing::info!(wal, read_connection = reader.is_some(), "store open");
    let shared_store = Arc::new(store::StoreLock::with_reader(store, reader));
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
    // A stop rather than a flag, because the stagger start may already have
    // started some of them -- it runs from a task spawned moments ago. It is
    // idempotent and self-correcting either way.
    for engine in engine_host.engines() {
        let hashes = match shared_store.lock() {
            Ok(store) => store.paused_hashes(&engine.id).unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        let mut restored = 0usize;
        for hash in &hashes {
            if let Some(info_hash) = store::hex20(hash) {
                // `restore_stopped`, not `stop_torrent`: the latter owes the
                // trackers a departure, and re-sending one for every stopped
                // torrent on every boot is how a tracker that refuses this
                // client kept seeing it.
                if engine.manager.restore_stopped(&info_hash).is_ok() {
                    restored += 1;
                }
            }
        }
        if restored > 0 {
            tracing::info!(engine = %engine.id, restored, "pause: restored user intent");
        }
        // Here and not in engines.rs for the same reason as the queue (spawned
        // with the live config below): the store does not exist yet when the
        // engines are built.
        workers::spawn_seed_time_sync(
            engine.manager.clone(),
            shared_store.clone(),
            engine.id.clone(),
        );
    }

    // The release that removed client spoofing. Torrents that were announcing
    // under a borrowed identity are paused once, so the operator learns about
    // it from a stopped torrent rather than from a tracker's ban message.
    //
    // Only those torrents: a blanket pause would cost seeding time to everyone
    // who never configured an override, and on a private tracker that is its
    // own way of getting an account in trouble.
    {
        let config_text = std::fs::read_to_string(&config_path).unwrap_or_default();
        let spoofed_hosts = spoofmigration::legacy_spoofed_hosts(&config_text);
        if !spoofed_hosts.is_empty() {
            let mut paused_total = 0usize;
            for engine in engine_host.engines() {
                for torrent in engine.manager.all() {
                    if !spoofmigration::is_affected(&torrent.meta.trackers, &spoofed_hosts) {
                        continue;
                    }
                    if engine.manager.stop_torrent(&torrent.info_hash).is_err() {
                        continue;
                    }
                    paused_total += 1;
                    let hex = typhon_engine::torrent::hex_encode(&torrent.info_hash);
                    if let Ok(store) = shared_store.lock() {
                        let _ = store.set_paused(&hex, &engine.id, true);
                    }
                }
            }
            tracing::warn!(
                hosts = ?spoofed_hosts,
                paused = paused_total,
                "client spoofing has been removed from Hydranos. These torrents were \
                 announcing as another client to the trackers listed and are now PAUSED. \
                 They will announce under their real peer id when you resume them -- check \
                 each tracker allows this client before you do."
            );
            // Dropping the tables is the marker: next boot finds no hosts and
            // does nothing. Written only after the pauses are in the store, so
            // an interruption re-runs rather than skips.
            let cleaned = spoofmigration::without_legacy_tables(&config_text);
            if let Err(e) = std::fs::write(&config_path, cleaned) {
                tracing::error!(error = %e, "could not strip [announce_clients] from the config; \
                                             the migration will run again next boot");
            }
        }
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

    // "Today" is the session minus what it was at the last local midnight;
    // the day this process starts on begins at zero. The session itself needs
    // no mark: the engines count what they move from zero (see
    // `TorrentManager::moved`), so the lifetime history their torrents load
    // with is never mistaken for this boot's traffic.
    let odometer: api::Odo = Arc::new(std::sync::Mutex::new(api::Odometer::at_boot()));

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
    // The client-wide rate caps (qBittorrent's global limit) live in the
    // store, not the TOML: a qBit client sets them over the shim, and the
    // TOML editor refuses keys a file does not already have.
    api::restore_client_rate_limits(&state);
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

    // The queue (active_downloads, and active_seeds / active_limit under
    // `queueing`) and the share limits, per engine, on the LIVE config: a
    // limit saved in the settings applies on the next pass, no restart.
    for engine in state.engines.engines().iter() {
        workers::spawn_download_slots(
            engine.manager.clone(),
            engine.announce_cache.clone(),
            state.config_handle(),
            state.store.clone(),
            engine.id.clone(),
        );
        workers::spawn_share_limits(
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
    // Event workflows: run the moment a download finishes, not on the clock.
    if let Some(rx) = state.engines.take_completions() {
        rulesapi::spawn_events(state.clone(), rx);
    }
    // Magnets waiting for their metadata, including the ones a restart
    // interrupted: their requests are in the store.
    magnets::spawn(state.clone());
    // Block lists and bans, before the engines take their first peers.
    ipfilter::spawn(state.clone());
    // Watched folders: a .torrent or .magnet dropped in is added.
    watch::spawn(state.clone());
    // Keeps the hardlink index the workflows read, instead of each pass
    // stat-ing the whole catalogue itself.
    linkscan::spawn(state.engines.clone(), state.store.clone());

    // Taken before the router consumes the state: `flush_on_shutdown` needs the
    // engines, and by then `state` has been moved.
    let engines_for_shutdown = state.engines.clone();

    // The notification-area icon, as the 3.x Windows package had. A no-op on
    // Unix. The closure is what it shows on hover, rebuilt every couple of
    // seconds by the tray thread -- it holds engine handles, not a snapshot,
    // so a tooltip cannot go stale the way a copied value would.
    {
        let engines = state.engines.clone();
        shutdown::spawn_stop_event_listener();
        tray::spawn(
            port,
            std::sync::Arc::new(move || {
                let (up, down) = engines.session_totals();
                let torrents = engines.total_torrents();
                format!(
                    "Hydranos {}\n{} torrents\nup {:.1} MB  down {:.1} MB",
                    api::HYDRANOS_VERSION,
                    torrents,
                    up as f64 / 1e6,
                    down as f64 / 1e6,
                )
            }),
        );
    }
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
    //
    // ⚠ The drain is bounded. A graceful shutdown waits for every open
    // connection, and the UI keeps two that never end on their own (the event
    // stream and the live log tail): with a tab open, 4.3 never reached the
    // flush and the supervisor's SIGKILL arrived first. Five seconds is
    // enough for any real request to finish.
    let draining = std::sync::Arc::new(tokio::sync::Notify::new());
    let drain_started = draining.clone();
    let serve = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        shutdown_signal().await;
        drain_started.notify_one();
    });
    tokio::select! {
        r = std::future::IntoFuture::into_future(serve) => r?,
        _ = async {
            draining.notified().await;
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        } => {
            tracing::warn!("connections still open 5 s into the drain (event stream, log tail); flushing without them");
        }
    }

    // Departures before the flush, bounded like libtorrent's
    // stop_tracker_timeout: trackers stop sending leechers to a client that
    // is going away. 4.3 sent none.
    engines_for_shutdown.depart_all(std::time::Duration::from_secs(5)).await;
    flush_on_shutdown(&engines_for_shutdown, stop_budget);
    // After the flush, not before: the departures above leave through the
    // tunnels. Under --network host the devices would outlive the process.
    let tunnels = engines_for_shutdown.wireguard_down().await;
    if tunnels > 0 {
        tracing::info!(tunnels, "wireguard: tunnels taken down");
    }
    if shutdown::restart_requested() {
        shutdown::exit_for_restart();
    }
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
        // No tray on Unix; this is also where a restart from the UI lands.
        _ = tray::quit_notify().notified() => {
            if shutdown::restart_requested() { "restart request" } else { "tray quit" }
        }
        _ = recv(&mut int) => "SIGINT",
    };
    tracing::warn!("{which} received, draining the API and flushing resume data");
}

#[cfg(not(unix))]
async fn shutdown_signal() -> () {
    // The tray's Quit ends here too, so it flushes resume data exactly like
    // Ctrl+C does. A tray that terminated the process would be the Task
    // Manager kill the 3.x README told people not to use.
    //
    // 4.3 heard only Ctrl+C and the tray: closing the console window, a
    // Windows shutdown, a sign-out and Ctrl+Break all ended the process
    // without a flush. Windows allows a few seconds after these events, so
    // the flush is a race there, but a partial flush beats none.
    use tokio::signal::windows;
    // One arm per console event. A handler that cannot be installed waits
    // forever instead of faking a shutdown.
    macro_rules! console_event {
        ($make:path) => {
            async {
                match $make() {
                    Ok(mut s) => {
                        s.recv().await;
                    }
                    Err(e) => {
                        tracing::error!("console handler setup failed: {e}");
                        std::future::pending::<()>().await
                    }
                }
            }
        };
    }
    let which = tokio::select! {
        _ = tokio::signal::ctrl_c() => "Ctrl+C",
        _ = console_event!(windows::ctrl_break) => "Ctrl+Break",
        _ = console_event!(windows::ctrl_close) => "console closed",
        _ = console_event!(windows::ctrl_logoff) => "sign-out",
        _ = console_event!(windows::ctrl_shutdown) => "system shutdown",
        _ = tray::quit_notify().notified() => {
            if shutdown::restart_requested() { "restart request" } else { "tray quit" }
        }
    };
    tracing::warn!("{which}: draining the API and flushing resume data");
}


const STOP_TIMEOUT_DEFAULT_S: u64 = 120;

/// The shutdown flush budget from `HYDRANOS_STOP_TIMEOUT`.
///
/// An unreadable value falls back to the default WITH a warning. 4.x parsed a
/// bare integer only, so the `45s` / `2m` forms the 3.x changelog documented
/// were dropped in silence and a budget raised for a large instance quietly
/// became 120 s again.
fn stop_budget() -> std::time::Duration {
    let secs = match std::env::var("HYDRANOS_STOP_TIMEOUT") {
        Err(_) => STOP_TIMEOUT_DEFAULT_S,
        Ok(raw) => parse_duration_secs(&raw).unwrap_or_else(|| {
            tracing::warn!(
                "HYDRANOS_STOP_TIMEOUT={raw:?} is not a duration (use 120, 60s, 2m or 1m30s); \
                 using {STOP_TIMEOUT_DEFAULT_S} s"
            );
            STOP_TIMEOUT_DEFAULT_S
        }),
    };
    std::time::Duration::from_secs(secs)
}

/// Seconds in `120`, `60s`, `2m`, `1m30s` or `1h`. Units run largest first and
/// each appears at most once, so `30s1m` and `1m1m` are refused rather than
/// guessed at; an empty string, a sign or a fraction is refused too.
fn parse_duration_secs(raw: &str) -> Option<u64> {
    let s = raw.trim().to_ascii_lowercase();
    if s.is_empty() {
        return None;
    }
    if s.bytes().all(|b| b.is_ascii_digit()) {
        return s.parse().ok();
    }
    let mut total: u64 = 0;
    let mut last_unit = u64::MAX;
    let mut rest = s.as_str();
    while !rest.is_empty() {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        let n: u64 = rest[..digits].parse().ok()?;
        let unit = match rest.as_bytes().get(digits)? {
            b'h' => 3600,
            b'm' => 60,
            b's' => 1,
            _ => return None,
        };
        if unit >= last_unit {
            return None;
        }
        last_unit = unit;
        total = total.checked_add(n.checked_mul(unit)?)?;
        rest = &rest[digits + 1..];
    }
    Some(total)
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    fn run(argv: &[&str]) -> Args {
        match parse_argv(argv.iter().map(|s| s.to_string()), PathBuf::from("/d.toml")) {
            Cli::Run(a) => a,
            other => panic!("expected a run, got {other:?}"),
        }
    }

    #[test]
    fn known_flags_are_read_without_warnings() {
        let a = run(&["--config", "/x.toml", "--console"]);
        assert_eq!((a.config, a.console, a.warnings.len()), (PathBuf::from("/x.toml"), true, 0));
        assert_eq!(run(&["--config=/y.toml"]).config, PathBuf::from("/y.toml"));
        assert_eq!(run(&[]).config, PathBuf::from("/d.toml"));
    }

    #[test]
    fn version_and_help_stop_the_parse() {
        let p = |v: &[&str]| parse_argv(v.iter().map(|s| s.to_string()), PathBuf::new());
        assert_eq!(p(&["--bogus", "--version"]), Cli::Version);
        assert_eq!(p(&["-h"]), Cli::Help);
    }

    #[test]
    fn an_unknown_argument_warns_and_still_starts() {
        let a = run(&["--bogus", "--config", "/x.toml"]);
        assert_eq!(a.config, PathBuf::from("/x.toml"));
        assert_eq!(a.warnings.len(), 1);
        assert!(a.warnings[0].contains("--bogus"), "{:?}", a.warnings);
    }

    #[test]
    fn the_3x_split_flags_point_to_nodes() {
        for flag in ["--agent-only", "--front-only", "-agent-only"] {
            let a = run(&[flag]);
            assert_eq!(a.warnings.len(), 1, "{flag}");
            assert!(a.warnings[0].contains("removed in 4.x"), "{flag}");
            assert!(a.warnings[0].contains("--register-to"), "{flag}");
        }
        // The value of a 3.x flag goes with it, not into a second warning.
        let a = run(&["--agent-only", "--agent-addr", ":9090", "--config", "/x.toml"]);
        assert_eq!(a.warnings.len(), 2, "{:?}", a.warnings);
        assert_eq!(a.config, PathBuf::from("/x.toml"));
    }

    #[test]
    fn a_config_flag_without_a_path_keeps_the_default_and_says_so() {
        let a = run(&["--config"]);
        assert_eq!(a.config, PathBuf::from("/d.toml"));
        assert_eq!(a.warnings.len(), 1);
    }
}

#[cfg(test)]
mod stop_timeout_tests {
    use super::parse_duration_secs as p;

    #[test]
    fn a_bare_number_is_seconds() {
        assert_eq!(p("120"), Some(120));
        assert_eq!(p(" 45 "), Some(45));
    }

    #[test]
    fn suffixed_and_compound_forms_are_read() {
        assert_eq!(p("60s"), Some(60));
        assert_eq!(p("2m"), Some(120));
        assert_eq!(p("1m30s"), Some(90));
        assert_eq!(p("1h"), Some(3600));
        assert_eq!(p("2M"), Some(120));
    }

    #[test]
    fn anything_else_is_refused_not_guessed() {
        for bad in ["", "abc", "2 m", "1.5m", "-5", "m", "30s1m", "1m1m", "5x", "10ms", "99999999999999999999"] {
            assert_eq!(p(bad), None, "{bad:?}");
        }
    }
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
fn flush_on_shutdown(engines: &std::sync::Arc<engines::EngineHost>, budget: std::time::Duration) {
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

    // The budget is enforced here: past it, the process leaves with what is
    // written. 4.3 joined every thread unconditionally, so the variable only
    // chose which log line to print. Each engine's resume store commits per
    // transaction, so leaving mid-sweep loses the unwritten rest, not the
    // written part.
    for h in handles {
        while !h.is_finished() && started.elapsed() < budget {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if !h.is_finished() {
            tracing::warn!(budget_s = budget.as_secs(),
                           "shutdown flush hit HYDRANOS_STOP_TIMEOUT; leaving with what is saved");
            return;
        }
        let _ = h.join();
    }
    tracing::info!(took_s = started.elapsed().as_secs(), "shutdown flush complete");
}

/// The log file, rotated by size: `hydranos.log` up to `LOG_MAX` bytes, then
/// `.1` to `.4` behind it. 4.3 appended forever; one line per inbound peer
/// connection fills a disk in weeks on a busy instance.
#[derive(Clone)]
struct RotatingLog(std::sync::Arc<std::sync::Mutex<RotatingInner>>);

struct RotatingInner {
    path: PathBuf,
    file: std::fs::File,
    size: u64,
}

const LOG_MAX: u64 = 128 * 1024 * 1024;
const LOG_KEEP: u32 = 4;

impl RotatingLog {
    fn open(path: PathBuf) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(RotatingLog(std::sync::Arc::new(std::sync::Mutex::new(RotatingInner { path, file, size }))))
    }
}

impl RotatingInner {
    fn rotate(&mut self) -> std::io::Result<()> {
        let name = |i: u32| {
            let mut p = self.path.clone().into_os_string();
            p.push(format!(".{i}"));
            PathBuf::from(p)
        };
        // Close before renaming: Windows refuses to rename an open file.
        self.file = std::fs::OpenOptions::new().append(true).open(if cfg!(windows) { "NUL" } else { "/dev/null" })?;
        let _ = std::fs::remove_file(name(LOG_KEEP));
        for i in (1..LOG_KEEP).rev() {
            let _ = std::fs::rename(name(i), name(i + 1));
        }
        let _ = std::fs::rename(&self.path, name(1));
        self.file = std::fs::OpenOptions::new().create(true).append(true).open(&self.path)?;
        self.size = 0;
        Ok(())
    }
}

impl std::io::Write for RotatingLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut inner = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if inner.size + buf.len() as u64 > LOG_MAX {
            let _ = inner.rotate();
        }
        let n = inner.file.write(buf)?;
        inner.size += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).file.flush()
    }
}

#[cfg(test)]
mod rotating_log_tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn the_log_rotates_and_keeps_four_behind_it() {
        let dir = std::env::temp_dir().join(format!("hydranos-logrot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hydranos.log");
        let mut log = RotatingLog::open(path.clone()).unwrap();
        for _ in 0..6 {
            log.0.lock().unwrap().size = LOG_MAX; // as if full
            log.write_all(b"line\n").unwrap();
        }
        assert!(path.exists());
        for i in 1..=LOG_KEEP {
            assert!(dir.join(format!("hydranos.log.{i}")).exists(), "{i}");
        }
        assert!(!dir.join(format!("hydranos.log.{}", LOG_KEEP + 1)).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
