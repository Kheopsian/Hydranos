use typhon_engine::{config, rpc, torrent, peer, disk, tracker, dht};

use std::sync::Arc;
use tracing::{info, error};

#[cfg(not(windows))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[tokio::main]
async fn main() {
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

    // Apply extra trusted PROXY v2 sources from config (VPS v6 etc).
    {
        let extras: Vec<std::net::IpAddr> = config
            .proxy_v2_trusted_sources
            .iter()
            .filter_map(|s| s.parse().ok())
            .collect();
        if !extras.is_empty() {
            info!("[engine] trusting {} extra PROXY v2 source(s): {:?}", extras.len(), extras);
            peer::set_trusted_proxy_sources(extras);
        }
    }

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

    // Configure outbound SOCKS5 proxy (used for v6 peer dials to avoid Free leak).
    if !config.socks5_outbound_host.is_empty() {
        info!(
            "[engine] v6 outbound dials via SOCKS5 {}:{}",
            config.socks5_outbound_host, config.socks5_outbound_port
        );
        peer::set_socks5_outbound(
            config.socks5_outbound_host.clone(),
            config.socks5_outbound_port,
            config.socks5_outbound_user.clone(),
            config.socks5_outbound_pass.clone(),
        );
    }

    // IPv6: opt-in. Gates the extra [::] listener (added in resolved_bindings)
    // and the v6 peer sources, so that off is byte-for-byte the old behaviour.
    if config.enable_ipv6 {
        info!("[engine] IPv6 enabled: listening on [::]:{} and accepting PEX added6", config.listen_port);
    }
    peer::extension::set_enable_ipv6(config.enable_ipv6);

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
    info!("[engine] config: {}", config_path);
    info!("[engine] socket: {}", config.socket_path);

    let disk_mgr = Arc::new(disk::DiskManager::new(config.file_pool_size));
    let torrent_mgr = Arc::new(torrent::TorrentManager::new(
        config.data_dir.clone(),
        config.resume_dir.clone(),
        disk_mgr.clone(),
    ));

    // Load resume data
    let loaded = torrent_mgr.load_resume_data();
    info!("[engine] loaded {} torrents from resume data", loaded);

    // DHT (BEP 5) and PEX (BEP 11) are the two ways this engine finds peers
    // without a tracker. Both default to on and both already skip `private`
    // torrents, but an operator who wants to talk to nothing but their
    // trackers can switch either off per engine.
    peer::extension::set_enable_pex(config.pex_enabled);
    if !config.pex_enabled {
        info!("[engine] PEX disabled by config: ut_pex is not advertised, and an incoming PEX message is ignored");
    }

    // Bootstrap DHT (BEP 5). Non-private torrents will get a get_peers stream
    // that funnels discovered peers into the dial queue.
    //
    // Skipping start() is the whole switch: DHT.get() then stays None, so the
    // track_torrent calls that fire later on add/start/magnet return early by
    // themselves. Nothing else in the engine has to test this flag.
    if config.dht_enabled {
        dht::start().await;
        for t in torrent_mgr.all().iter() {
            // Stopped torrents stay off the DHT until they are started again;
            // tracking them here would resurrect the very tasks stop_torrent kills.
            if t.is_paused.load(std::sync::atomic::Ordering::Relaxed) {
                continue;
            }
            dht::track_torrent(t.clone());
        }
    } else {
        info!("[engine] DHT disabled by config: no bootstrap, no get_peers, no peer discovery outside the trackers");
    }

    // BEP 19 webseed. Started after the resume load so the very first scan
    // already sees the whole catalogue.
    typhon_engine::webseed::start(torrent_mgr.clone(), &config);

    // Bind shared uTP socket on the same UDP port as TCP listen_port (qBittorrent default).
    // Used both for outgoing (dial fallback) and incoming (separate accept loop).
    // max_live_vsocks default is 128 which saturates immediately on a seedbox with
    // thousands of peers — new uTP dials get rejected with TooManyActiveConnections.
    // Bumped to 4096 (2026-04-17 investigation: 70% of uTP fails were "error"=saturated).
    let listen_port = config.listen_port;
    // Before ANY socket is opened: every one of them is pinned to this device.
    typhon_engine::netpin::set_bind_device(&config.bind_device);
    if let Some(dev) = typhon_engine::netpin::bind_device() {
        info!("[engine] every socket is pinned to device {}", dev);
    }
    // TYPHON_DISABLE_UTP=1 skips uTP entirely. uTP is raw UDP, cannot route via
    // SOCKS5_OUTBOUND, and so leaks the netns default-route source IP to peers.
    // Set this when SOCKS5_OUTBOUND is the only sanctioned egress (no FOU/WG L3 tunnel).
    let utp_socket = if std::env::var("TYPHON_DISABLE_UTP").map(|v| v == "1" || v.eq_ignore_ascii_case("true")).unwrap_or(false) {
        info!("[engine] uTP disabled via TYPHON_DISABLE_UTP — TCP-only dial+listen");
        None
    } else {
        let utp_bind: std::net::SocketAddr = format!("0.0.0.0:{}", listen_port).parse().unwrap();
        let mut utp_opts = librqbit_utp::SocketOpts::default();
        utp_opts.max_live_vsocks = std::num::NonZeroUsize::new(4096);
        // uTP is raw UDP and gets the same device pin as everything else.
        // Without it the tunnel steering would hold for TCP and leak for uTP,
        // which is the shape of leak nobody notices: it is the same swarm.
        let utp_dev = typhon_engine::netpin::bind_device()
            .map(|d| d.parse::<librqbit_utp::BindDevice>())
            .transpose();
        let utp_dev = match utp_dev {
            Ok(d) => d,
            Err(e) => {
                error!("[engine] bind_device is not usable for the uTP socket: {} — refusing to open it rather than leak the default route", e);
                None
            }
        };
        let udp_opts = librqbit_utp::UtpSocketUdpOpts { bind_device: utp_dev.as_ref() };
        match librqbit_utp::UtpSocketUdp::new_udp_with_opts(utp_bind, utp_opts, udp_opts).await {
            Ok(s) => {
                info!("[engine] uTP socket bound on {}", utp_bind);
                Some(s)
            }
            Err(e) => {
                error!("[engine] failed to bind uTP socket on {}: {} — uTP disabled", utp_bind, e);
                None
            }
        }
    };

    // Start TCP listener for incoming peers (+ uTP accept loop if socket bound)
    // Multi-binding: each binding has its own peer_id and (addr, port). Empty
    // `bindings` in config falls back to legacy single-binding from
    // listen_interfaces / listen_port / peer_fingerprint.
    let resolved_bindings = config.resolved_bindings();
    if resolved_bindings.is_empty() {
        error!("[engine] no resolvable bindings — engine cannot accept peers");
    } else {
        info!(
            "[engine] resolved {} binding(s) for peer listeners",
            resolved_bindings.len()
        );
    }
    let tm = torrent_mgr.clone();
    let dm = disk_mgr.clone();
    let utp_for_listen = utp_socket.clone();
    let bindings_for_listen = resolved_bindings.clone();
    tokio::spawn(async move {
        if let Err(e) = peer::listen(bindings_for_listen, listen_port, tm, dm, utp_for_listen).await {
            error!("[engine] peer listener failed: {}", e);
        }
    });

    // Optional PROXY v2 listener (for v6 bypass via VPS haproxy)
    if let Some(pv2_port) = config.listen_port_proxy_v2 {
        let tm = torrent_mgr.clone();
        let dm = disk_mgr.clone();
        let pid = config.peer_id();
        let u = utp_socket.clone();
        let bind_addr = config.listen_addr_proxy_v2.clone().unwrap_or_default();
        tokio::spawn(async move {
            if let Err(e) = peer::listen_proxy_v2(bind_addr, pv2_port, tm, dm, pid, u).await {
                error!("[engine] proxy-v2 listener failed: {}", e);
            }
        });
    }

    // Start the outbound dial queue consumer (with uTP socket for outgoing
    // fallback). Announces themselves belong to the Go control plane.
    // Multi-binding: dial queue consumer hashes peer addr → picks one binding,
    // source-binds outbound TcpSocket on that binding's listen_addr. Single
    // binding collapses to the legacy single-source-IP behavior.
    let dm2 = disk_mgr.clone();
    // Connection ceiling: honoured from here on. This key has been in the
    // config (and echoed by get_config) since long before anything read it,
    // so an existing install may already carry a value that has never taken
    // effect -- it starts biting at this upgrade.
    tracker::dial_limiter::set_max_connections(config.max_connections);
    tracker::dial_limiter::set_max_dials_per_sec(config.max_dials_per_sec);
    tracker::start_announce_loop(dm2, resolved_bindings.clone(), utp_socket.clone(), config.max_dials_per_sec);

    // Choking engine DISABLED (2.4.13-typhon).
    // Le loop tickait toutes les 10s et chokait tous les peers sauf top-4 par
    // torrent sur tous les torrents seeding (~13k hoard). Resultat: churn
    // massif des peers, plafond ~300 tw/p au lieu de ~11k sustained.
    // Pour re-activer: bump max_unchoked_per_torrent et tick_interval dans
    // peer::choking::ChokingConfig::default().

    info!(
        "[engine] session started, listen={}, max_uploads/torrent={}, resume_dir={}",
        config.listen_addr(),
        config.max_uploads_per_torrent,
        config.resume_dir,
    );

    // Rate tracking tick (every 2s)
    let tm_rate = torrent_mgr.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            tm_rate.update_rates();
        }
    });

    // Push-based stats snapshot emitter (every 1s, delta-filtered).
    // Replaces Go's `list_torrents` polling every 2s — cuts ~4-8% CPU spent
    // on 13k-torrent JSON serialization (see rpc::events docs).
    let tm_stats = torrent_mgr.clone();
    tokio::spawn(async move {
        use std::collections::HashMap;
        use std::sync::atomic::Ordering;
        let mut last: HashMap<[u8; 20], (u64, u64, u8, usize, usize)> = HashMap::new();
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            // Skip the whole scan when nobody listens — saves 13k atomic loads
            // per second on a system with no subscribers.
            if rpc::events::bus().receiver_count() == 0 {
                continue;
            }
            let mut changed: Vec<rpc::events::TorrentStatsMini> = Vec::new();
            for t in tm_stats.all().iter() {
                let ih = t.info_hash;
                let ul = t.total_uploaded.load(Ordering::Relaxed);
                let dl = t.total_downloaded.load(Ordering::Relaxed);
                let st = t.status.load(Ordering::Relaxed);
                let peers = t.peers_connected.load(Ordering::Relaxed);
                let interested = t.peers_interested.load(Ordering::Relaxed);
                let prev = last.get(&ih);
                let moved = match prev {
                    Some(&(pul, pdl, pst, pp, pi)) => {
                        ul != pul || dl != pdl || st != pst || peers != pp || interested != pi
                    }
                    None => true, // first-time entry always sent
                };
                if moved {
                    // Live progress so the UI's progress column streams (0..1);
                    // seeders / seed-mode (no picker) are complete by definition.
                    let progress: f32 = if st == crate::torrent::meta::TorrentStatus::Seeding as u8 {
                        1.0
                    } else if let Some(pk) = t.picker.get() {
                        let np = t.meta.num_pieces();
                        if np > 0 { pk.lock().unwrap().num_have() as f32 / np as f32 } else { 0.0 }
                    } else {
                        1.0
                    };
                    changed.push(rpc::events::TorrentStatsMini {
                        info_hash: torrent::hex_encode(&ih),
                        status: st,
                        total_uploaded: ul,
                        total_downloaded: dl,
                        upload_rate: t.upload_rate.get(),
                        download_rate: t.download_rate.get(),
                        peers_connected: peers as u32,
                        peers_interested: interested as u32,
                        progress,
                    });
                    last.insert(ih, (ul, dl, st, peers, interested));
                }
            }
            if !changed.is_empty() {
                rpc::events::publish(rpc::events::Event::StatsSnapshot { torrents: changed });
            }
        }
    });

    // Unseeded peers count (every 30s — O(N) scan too expensive for tight loop)
    let tm_unseeded = torrent_mgr.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            tm_unseeded.update_unseeded_count();
        }
    });

    // Persist completions as they happen, not on the next sweep.
    let tm_done = torrent_mgr.clone();
    tokio::spawn(async move {
        if let Some(mut rx) = torrent::take_completion_receiver() {
            while let Some(ih) = rx.recv().await {
                tm_done.persist_completed(&ih);
            }
        }
    });

    // Periodic resume save (every 5 min)
    let tm3 = torrent_mgr.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(300)).await;
            tm3.save_all_resume();
        }
    });

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
    // by default, HYDRA_STOP_TIMEOUT) and kills the process when it runs out,
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
