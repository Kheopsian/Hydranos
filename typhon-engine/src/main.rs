use typhon_engine::{config, rpc, torrent, peer, disk, tracker, dht};

use std::sync::Arc;
use tracing::{info, error};

#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[tokio::main]
async fn main() {
    // SIGUSR1 => dump a jemalloc heap profile to $prof_prefix (set via
    // MALLOC_CONF). The Go watchdog raises this on a ballooning engine right
    // before killing it, so the 85GB heap leak (2026-07-09) gets an
    // allocation-site profile the next time it recurs.
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

    // Bootstrap DHT (BEP 5). Non-private torrents will get a get_peers stream
    // that funnels discovered peers into the dial queue.
    dht::start().await;
    for t in torrent_mgr.all().iter() {
        dht::track_torrent(t.clone());
    }

    // Bind shared uTP socket on the same UDP port as TCP listen_port (qBittorrent default).
    // Used both for outgoing (dial fallback) and incoming (separate accept loop).
    // max_live_vsocks default is 128 which saturates immediately on a seedbox with
    // thousands of peers — new uTP dials get rejected with TooManyActiveConnections.
    // Bumped to 4096 (2026-04-17 investigation: 70% of uTP fails were "error"=saturated).
    let listen_port = config.listen_port;
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
        match librqbit_utp::UtpSocketUdp::new_udp_with_opts(utp_bind, utp_opts, Default::default()).await {
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

    // Start tracker announce loops (with uTP socket for outgoing fallback)
    // Multi-binding: dial queue consumer hashes peer addr → picks one binding,
    // source-binds outbound TcpSocket on that binding's listen_addr. Single
    // binding collapses to the legacy single-source-IP behavior.
    let tm2 = torrent_mgr.clone();
    let dm2 = disk_mgr.clone();
    tracker::start_announce_loop(tm2, dm2, resolved_bindings.clone(), utp_socket.clone(), config.disable_internal_announce);

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
                    changed.push(rpc::events::TorrentStatsMini {
                        info_hash: torrent::hex_encode(&ih),
                        status: st,
                        total_uploaded: ul,
                        total_downloaded: dl,
                        upload_rate: t.upload_rate.get(),
                        download_rate: t.download_rate.get(),
                        peers_connected: peers as u32,
                        peers_interested: interested as u32,
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

    // Periodic resume save (every 5 min)
    let tm3 = torrent_mgr.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(300)).await;
            tm3.save_all_resume();
        }
    });

    // Start RPC server (blocks)
    let socket_path = config.socket_path.clone();
    rpc::serve(&socket_path, torrent_mgr.clone(), disk_mgr, config).await;

    // Save on shutdown
    torrent_mgr.save_all_resume();
    info!("[engine] resume data saved");
}
