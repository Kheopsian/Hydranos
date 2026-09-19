use typhon_engine::{config, rpc, torrent, peer, disk, tracker, dht};

use std::sync::Arc;
use tracing::{info, error};

#[cfg(not(windows))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn main() {
    let workers = typhon_engine::runtime::worker_threads();
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .expect("build tokio runtime")
        .block_on(async_main(workers))
}

async fn async_main(workers: usize) {
    // SIGUSR1 => dump a jemalloc heap profile to $prof_prefix (set via
    // MALLOC_CONF). The Go watchdog raises this on a ballooning engine right
    // before killing it, so the 85GB heap leak (2026-07-09) gets an
    // allocation-site profile the next time it recurs.
    #[cfg(unix)]
    tokio::spawn(async {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::user_defined1()) {
            Ok(mut sig) => loop {
                sig.recv().await;
                let r = unsafe {
                    tikv_jemalloc_ctl::raw::write(
                        b"prof.dump\0",
                        std::ptr::null::<std::os::raw::c_char>(),
                    )
                };
                match r {
                    Ok(_) => tracing::warn!("jemalloc heap profile dumped (SIGUSR1)"),
                    Err(e) => tracing::error!("jemalloc prof.dump failed: {}", e),
                }
            },
            Err(e) => tracing::error!("SIGUSR1 handler setup failed: {}", e),
        }
    });
    // SIGUSR2 => hand the allocator's dirty pages back to the kernel, now.
    //
    // `dirty_decay_ms`/`muzzy_decay_ms` are the RAM/CPU dial, and they are set
    // through MALLOC_CONF -- an environment variable, so moving them normally
    // means recreating the container. That is the one operation on this engine
    // with a real blast radius, which is why the dial never got retuned after
    // the 2026-09-01 measurement showed `decay_ms:0` taking RSS from 6.81 GiB
    // to 2.86 GiB at equal age, for 0.03% of CPU in jemalloc and 0.00% in
    // madvise. Both arena settings are writable at runtime, so expose them on
    // a signal instead: setting the decay to 0 purges what is already dirty
    // and keeps it that way, and it is reversible by restoring the old value.
    //
    // Logs allocated/resident either side so the effect is measured, not
    // assumed -- `resident` is the number that moves, `allocated` should not.
    #[cfg(unix)]
    tokio::spawn(async {
        use tokio::signal::unix::{signal, SignalKind};
        use tikv_jemalloc_ctl::{epoch, stats};
        let mb = |v: usize| v as f64 / (1024.0 * 1024.0);
        let snapshot = || {
            let _ = epoch::advance();
            (stats::allocated::read().unwrap_or(0), stats::resident::read().unwrap_or(0))
        };
        match signal(SignalKind::user_defined2()) {
            Ok(mut sig) => loop {
                sig.recv().await;
                let (al0, re0) = snapshot();
                // NOT MALLCTL_ARENAS_ALL (4096). That sentinel is only accepted by
                // arena.<i>.{purge,decay,reset,destroy}; arena.<i>.dirty_decay_ms
                // resolves the index through arena_get(), which indexes the arena
                // array unchecked in a release build. Passing 4096 against
                // narenas:8 reads out of bounds -- verified on an isolated engine,
                // where it segfaulted the process and the Go watchdog then
                // restarted the whole stack. Walk the real indices instead.
                let narenas: u32 = unsafe {
                    tikv_jemalloc_ctl::raw::read(b"arenas.narenas\0").unwrap_or(0)
                };
                // Arenas are created lazily, so most of 0..narenas do not exist yet
                // and answer EFAULT. That is not a failure: an arena that was never
                // initialised holds no dirty pages. Skip it and keep going -- only
                // a run where *nothing* took is worth reporting as an error.
                // Also move the template every future arena is created from.
                let mut moved = 0u32;
                let mut err = None;
                for i in 0..narenas {
                    let d = format!("arena.{}.dirty_decay_ms\0", i);
                    let m = format!("arena.{}.muzzy_decay_ms\0", i);
                    let r = unsafe {
                        tikv_jemalloc_ctl::raw::write(d.as_bytes(), 0isize)
                            .and_then(|_| tikv_jemalloc_ctl::raw::write(m.as_bytes(), 0isize))
                    };
                    match r {
                        Ok(_) => moved += 1,
                        Err(e) => err = Some(e),
                    }
                }
                unsafe {
                    let _ = tikv_jemalloc_ctl::raw::write(b"arenas.dirty_decay_ms\0", 0isize);
                    let _ = tikv_jemalloc_ctl::raw::write(b"arenas.muzzy_decay_ms\0", 0isize);
                }
                if narenas == 0 {
                    tracing::error!("jemalloc: arenas.narenas unreadable, decay left alone");
                } else if moved == 0 {
                    tracing::error!(
                        "jemalloc decay moved on no arena out of {}: {}",
                        narenas,
                        err.map(|e| e.to_string()).unwrap_or_default()
                    );
                } else {
                    let (al1, re1) = snapshot();
                    tracing::warn!(
                        "jemalloc decay forced to 0 on {}/{} arenas (SIGUSR2): resident {:.0}MiB -> {:.0}MiB (freed {:.0}MiB), allocated {:.0}MiB -> {:.0}MiB",
                        moved, narenas, mb(re0), mb(re1), mb(re0.saturating_sub(re1)), mb(al0), mb(al1)
                    );
                }
            },
            Err(e) => tracing::error!("SIGUSR2 handler setup failed: {}", e),
        }
    });

    // Exact allocator accounting, every five minutes.
    //
    // The sampled heap profile is not enough to say where the memory is: at
    // lg_prof_sample:12 it accounted for well under half of RSS on the 200k
    // torrent instance, which left "live objects the sampler misses" and
    // "pages jemalloc is holding" indistinguishable. These five numbers are
    // not sampled, so they separate the two for good:
    //
    //   allocated          bytes the application actually holds
    //   active - allocated allocator slop inside live pages
    //   resident - active  dirty pages jemalloc kept instead of returning
    //   retained           address space unmapped, costs no RSS
    //
    // If resident tracks allocated, the memory is real and the fix is in the
    // code. If resident dwarfs it, the fix is in the decay settings.
    #[cfg(unix)]
    tokio::spawn(async {
        use tikv_jemalloc_ctl::{epoch, stats};
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(300));
        loop {
            tick.tick().await;
            // Stats are cached; advancing the epoch is what refreshes them.
            if epoch::advance().is_err() {
                continue;
            }
            let mb = |v: usize| v as f64 / (1024.0 * 1024.0);
            match (
                stats::allocated::read(),
                stats::active::read(),
                stats::resident::read(),
                stats::mapped::read(),
                stats::retained::read(),
            ) {
                (Ok(al), Ok(ac), Ok(re), Ok(ma), Ok(rt)) => tracing::info!(
                    "jemalloc allocated={:.0}MiB active={:.0}MiB resident={:.0}MiB mapped={:.0}MiB retained={:.0}MiB metadata={:.0}MiB slop={:.0}MiB dirty={:.0}MiB",
                    mb(al), mb(ac), mb(re), mb(ma), mb(rt),
                    mb(stats::metadata::read().unwrap_or(0)),
                    mb(ac.saturating_sub(al)), mb(re.saturating_sub(ac))
                ),
                _ => tracing::warn!("jemalloc stats unavailable"),
            }
        }
    });

    // Parse CLI args: --config <path> --socket <path>
    // Compatible with hydra-engine C++ interface
    let args: Vec<String> = std::env::args().collect();
    let mut config_path = String::new();
    let mut socket_override = String::new();

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--config" if i + 1 < args.len() => {
                config_path = args[i + 1].clone();
                i += 2;
            }
            "--socket" if i + 1 < args.len() => {
                socket_override = args[i + 1].clone();
                i += 2;
            }
            other => {
                // Fallback: positional arg = config path
                if config_path.is_empty() {
                    config_path = other.to_string();
                }
                i += 1;
            }
        }
    }

    if config_path.is_empty() {
        eprintln!("usage: typhon-engine --config <config.json> --socket <socket.path>");
        std::process::exit(1);
    }

    let mut config = match config::EngineConfig::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            error!("failed to load config {}: {}", config_path, e);
            std::process::exit(1);
        }
    };

    // CLI --socket overrides config
    if !socket_override.is_empty() {
        config.socket_path = socket_override;
    }

    // Parsed here, applied once the manager exists: the allowlist belongs to
    // the engine, not to the process.
    let proxy_v2_extras: Vec<std::net::IpAddr> = config
        .proxy_v2_trusted_sources
        .iter()
        .filter_map(|s| s.parse().ok())
        .collect();

    // Seed the self-dial IP filter from env; Go refreshes it at runtime with the
    // observed public IP via the set_self_ips RPC (no more hard-coded staleness).
    tracker::seed_self_ips_from_env();
    // Discovered before the first dial, then kept fresh: interfaces come and go
    // (a tunnel raised after boot brings a new address that must not be dialled).
    tracker::refresh_own_ips();
    tokio::spawn(async {
        let mut tk = tokio::time::interval(std::time::Duration::from_secs(120));
        loop {
            tk.tick().await;
            tracker::refresh_own_ips();
        }
    });

    // The outbound SOCKS5 for v6 dials is carried by each binding's Egress
    // (see Config::socks5_outbound); nothing to install here.
    if !config.socks5_outbound_host.is_empty() {
        info!(
            "[engine] v6 outbound dials via SOCKS5 {}:{}",
            config.socks5_outbound_host, config.socks5_outbound_port
        );
    }

    // IPv6: opt-in. Gates the extra [::] listener (added in resolved_bindings)
    // and the v6 peer sources, so that off is byte-for-byte the old behaviour.
    if config.enable_ipv6 {
        info!("[engine] IPv6 enabled: listening on [::]:{} and accepting PEX added6", config.listen_port);
    }


    // Flamegraph 2026-04-19 showed console-subscriber's task/resource stats
    // scanning (HashMap::retain + DroppedAt) dominates CPU (~50%+ of samples).
    // Gate it behind a feature so prod builds skip the overhead entirely.
    #[cfg(feature = "tokio-console")]
    {
        use tracing_subscriber::prelude::*;
        let default_console_port = config.listen_port.wrapping_add(1000);
        let console_bind: std::net::SocketAddr = std::env::var("TOKIO_CONSOLE_BIND")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| ([0, 0, 0, 0], default_console_port).into());
        let console_layer = console_subscriber::ConsoleLayer::builder()
            .server_addr(console_bind)
            .spawn();
        let fmt_layer = tracing_subscriber::fmt::layer()
            .with_target(false)
            .with_writer(std::io::stderr);
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
        tracing_subscriber::registry()
            .with(console_layer)
            .with(fmt_layer.with_filter(filter))
            .init();
        info!("[engine] tokio-console server bound at {}", console_bind);
    }
    #[cfg(not(feature = "tokio-console"))]
    {
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .with_writer(std::io::stderr)
            .init();
    }

    info!("[engine] typhon-engine starting");
    // After the subscriber is installed, not before: the first version of
    // this line was emitted from the top of async_main and vanished, because
    // nothing was collecting spans yet.
    info!("[engine] tokio runtime: {} worker threads", workers);
    info!("[engine] config: {}", config_path);
    info!("[engine] socket: {}", config.socket_path);

    let disk_mgr = Arc::new(disk::DiskManager::new(config.file_pool_size));
    let torrent_mgr = Arc::new(torrent::TorrentManager::new(
        config.data_dir.clone(),
        config.resume_dir.clone(),
        disk_mgr.clone(),
    ));

    if !proxy_v2_extras.is_empty() {
        info!(
            "[engine] trusting {} extra PROXY v2 source(s): {:?}",
            proxy_v2_extras.len(),
            proxy_v2_extras
        );
        torrent_mgr.set_trusted_proxy_sources(proxy_v2_extras);
    }

    // Load resume data
    let loaded = torrent_mgr.load_resume_data();
    info!("[engine] loaded {} torrents from resume data", loaded);

    // Everything that puts this engine on the network. Shared with Hydra 4,
    // which brings up two of these in one process.
    typhon_engine::session::start(
        torrent_mgr.clone(),
        disk_mgr.clone(),
        &config,
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
    .await;

    // Start RPC server (blocks until it dies or a shutdown signal arrives).
    //
    // rpc::serve is an endless accept loop, so before this select the line
    // below it was unreachable: the default disposition of SIGTERM/SIGINT
    // killed the process where it stood and the resume data on disk was
    // whatever the five-minute periodic save had last written. A restart then
    // re-hashed pieces that were already complete, and a torrent that had just
    // finished could come back at 0%.
    let socket_path = config.socket_path.clone();
    let serve = rpc::serve(&socket_path, torrent_mgr.clone(), disk_mgr, config);
    tokio::pin!(serve);
    tokio::select! {
        _ = &mut serve => info!("[engine] RPC server exited"),
        sig = shutdown_signal() => info!("[engine] {} received, flushing resume data", sig),
    }

    // Save on shutdown. Hydra allows a bounded budget for this (10s per engine
    // by default, HYDRANOS_STOP_TIMEOUT) and kills the process when it runs out,
    // so a partial sweep is still better than none: every torrent written
    // before the kill is one the next start does not have to re-check.
    torrent_mgr.save_all_resume();
    info!("[engine] resume data saved");
}

/// Resolve once either termination signal is received, naming the one that
/// arrived. Hydra stops its engines with SIGINT; a bare-metal or systemd setup
/// sends SIGTERM, and a terminal sends SIGINT, so both have to be honoured.
#[cfg(unix)]
async fn shutdown_signal() -> &'static str {
    use tokio::signal::unix::{signal, SignalKind};

    // A handler that cannot be installed must not swallow the shutdown: leave
    // that branch pending forever so the other one still fires.
    let mut term = signal(SignalKind::terminate())
        .map_err(|e| error!("SIGTERM handler setup failed: {}", e))
        .ok();
    let mut int = signal(SignalKind::interrupt())
        .map_err(|e| error!("SIGINT handler setup failed: {}", e))
        .ok();

    async fn recv(s: &mut Option<tokio::signal::unix::Signal>) {
        match s {
            Some(sig) => {
                sig.recv().await;
            }
            None => std::future::pending().await,
        }
    }

    tokio::select! {
        _ = recv(&mut term) => "SIGTERM",
        _ = recv(&mut int) => "SIGINT",
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() -> &'static str {
    if let Err(e) = tokio::signal::ctrl_c().await {
        error!("ctrl-c handler setup failed: {}", e);
        std::future::pending::<()>().await;
    }
    "ctrl-c"
}
