//! Bringing one engine up on the network.
//!
//! Extracted from the engine binary so the two callers cannot drift: the
//! standalone `typhon-engine` process and Hydra 4, which runs race and hoard as
//! two of these inside one process. Everything this touches is per-engine
//! state reached through the manager -- there is no global left here to let one
//! engine decide for the other.

use std::sync::Arc;
use tracing::{error, info, warn};

use crate::config::EngineConfig;
use crate::disk::DiskManager;
use crate::torrent::TorrentManager;

/// Start listening, announcing and discovering for one engine.
///
/// Returns once everything is spawned; the tasks outlive the call.
pub async fn start(
    mgr: Arc<TorrentManager>,
    disk: Arc<DiskManager>,
    config: &EngineConfig,
    listening: Arc<std::sync::atomic::AtomicBool>,
) {
    // DHT (BEP 5) and PEX (BEP 11) are the two ways this engine finds peers
    // without a tracker. Both default to on and both already skip `private`
    // torrents, but an operator who wants to talk to nothing but their
    // trackers can switch either off per engine.
    // Per engine, not per process: the manager hands this to every torrent it
    // owns, so two engines in one process keep opposite settings.
    mgr.policy().set_pex(config.pex_enabled);
    mgr.policy().set_ipv6(config.enable_ipv6);
    mgr.policy().set_idle_timeout_secs(config.peer_timeout);
    // Engine-wide byte-rate caps, bytes/s (0 = none). Read by nobody until
    // 4.4; the RPC and the binary move them live afterwards.
    mgr.rates().engine.up.set_rate(config.upload_limit);
    mgr.rates().engine.down.set_rate(config.download_limit);
    if !config.pex_enabled {
        info!("[engine] PEX disabled by config: ut_pex is not advertised, and an incoming PEX message is ignored");
    }

    // Bootstrap DHT (BEP 5). Non-private torrents will get a get_peers stream
    // that funnels discovered peers into the dial queue.
    //
    // Skipping start() is the whole switch: DHT.get() then stays None, so the
    // track_torrent calls that fire later on add/start/magnet return early by
    // themselves. Nothing else in the engine has to test this flag.
    //
    // Off behind a SOCKS5 proxy whatever `dht_enabled` says (see
    // `dht::dht_policy`); pinned to the engine's device otherwise, so an
    // engine in a tunnel runs its DHT through that tunnel.
    match crate::dht::dht_policy(config) {
        crate::dht::Discovery::On => {
            let device = Some(config.bind_device.trim()).filter(|d| !d.is_empty());
            if let Some(session) = crate::dht::DhtSession::start(device).await {
                mgr.set_dht(session);
            }
            for t in mgr.all().iter() {
                // Stopped torrents stay off the DHT until they are started again;
                // tracking them here would resurrect the very tasks stop_torrent kills.
                if t.is_paused.load(std::sync::atomic::Ordering::Relaxed) {
                    continue;
                }
                mgr.track_in_dht(t.clone());
            }
        }
        crate::dht::Discovery::Off(why) if why == crate::dht::OFF_BY_CONFIG => {
            info!("[engine] DHT disabled by config: no bootstrap, no get_peers, no peer discovery outside the trackers");
        }
        crate::dht::Discovery::Off(why) => {
            warn!("[engine] DHT {}; PEX stays on (it rides the proxied peer connections)", why);
        }
    }
    // Local Service Discovery (BEP 14), only if `dht::lsd_policy` says so: it
    // is UDP multicast, which no SOCKS5 proxy carries. Pinned to the engine's
    // device like the DHT; a socket that cannot join leaves LSD off, nothing
    // else.
    crate::lsd::start(mgr.clone(), config).await;

    // BEP 19 webseed. Started after the resume load so the very first scan
    // already sees the whole catalogue.
    crate::webseed::start(mgr.clone(), &config);

    // Bind shared uTP socket on the same UDP port as TCP listen_port (qBittorrent default).
    // Used both for outgoing (dial fallback) and incoming (separate accept loop).
    // max_live_vsocks default is 128 which saturates immediately on a seedbox with
    // thousands of peers — new uTP dials get rejected with TooManyActiveConnections.
    // Bumped to 4096 (2026-04-17 investigation: 70% of uTP fails were "error"=saturated).
    let listen_port = config.listen_port;
    // Before ANY socket is opened: every one of them is pinned to this device.
    // The device travels inside each binding's Egress rather than a global, so
    // a process carrying two engines pins each one to its own tunnel.
    let egress = crate::netpin::Egress {
        fwmark: 0,
        device: config.bind_device.clone(),
        socks5: None,
    };
    if let Some(dev) = egress.device() {
        info!("[engine] every socket is pinned to device {}", dev);
    }
    // Said once, at startup, because the consequences are not visible
    // anywhere else: a peer the proxy cannot reach is simply never dialled.
    if !config.socks5_outbound_host.trim().is_empty() {
        info!(
            "[engine] every outbound peer connection goes through SOCKS5 {}:{}; a dial the proxy refuses is dropped, never retried directly",
            config.socks5_outbound_host, config.socks5_outbound_port
        );
        info!("[engine] outbound uTP is OFF for this engine: SOCKS5 without UDP ASSOCIATE cannot carry it (incoming uTP still accepted)");
    }
    let http_proxy = config.http_proxy();
    if !http_proxy.is_empty() {
        info!("[engine] tracker announces and webseed fetches go through {}; udp:// trackers are skipped (the proxy carries TCP only)", crate::tracker::http::redact_proxy(&http_proxy));
    }

    // The PROXY v2 allowlist belongs to the engine: set here, where both the
    // standalone binary and Hydra bring an engine up, so neither can forget
    // it. Hydra did: its listener never started, and had it started it would
    // have trusted nothing but the built-in private ranges.
    let trusted: Vec<std::net::IpAddr> = config
        .proxy_v2_trusted_sources
        .iter()
        .filter_map(|s| match s.trim().trim_start_matches('[').trim_end_matches(']').parse() {
            Ok(ip) => Some(ip),
            Err(_) => {
                warn!("[engine] proxy_v2_trusted_sources: {:?} is not an IP address, ignored", s);
                None
            }
        })
        .collect();
    if !trusted.is_empty() {
        info!("[engine] trusting {} extra PROXY v2 source(s): {:?}", trusted.len(), trusted);
        mgr.set_trusted_proxy_sources(trusted);
    }
    // TYPHON_DISABLE_UTP=1 skips uTP entirely, incoming included. An engine
    // with a SOCKS5 proxy no longer needs it to stay hidden: its OUTBOUND uTP
    // is refused in `tracker::open_peer` whatever this says. What is left is
    // incoming uTP, which only answers a peer that already found us.
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
        let utp_dev = egress
            .device()
            .map(|d| d.parse::<librqbit_utp::BindDevice>())
            .transpose();
        match utp_dev {
            // The message always said "refusing to open it"; 4.3 then opened
            // it anyway with no device, so an interface missing at startup (a
            // tunnel not up yet) sent every uTP dial out by the default route.
            // Refused for real now: TCP-only until a restart finds the device.
            Err(e) => {
                error!("[engine] bind_device is not usable for the uTP socket: {} — uTP disabled rather than leak the default route; restart once the interface is up", e);
                None
            }
            Ok(utp_dev) => {
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
    let tm = mgr.clone();
    let dm = disk.clone();
    let utp_for_listen = utp_socket.clone();
    let bindings_for_listen = resolved_bindings.clone();
    let listening_for_listen = listening.clone();
    tokio::spawn(async move {
        let flag = listening_for_listen.clone();
        if let Err(e) =
            crate::peer::listen(bindings_for_listen, listen_port, tm, dm, utp_for_listen, flag).await
        {
            // Lower it again: the engine is up, holds its catalogue and answers
            // the API, and accepts no peer at all. That state has a name now
            // instead of being a line of ERROR under two lines of INFO saying
            // the opposite.
            listening_for_listen.store(false, std::sync::atomic::Ordering::Relaxed);
            error!("[engine] peer listener failed: {}", e);
        }
    });

    // Optional PROXY v2 listener: a relay (haproxy on a VPS, say) forwards
    // incoming peers here with their real address in a PROXY v2 header.
    // It presents the FIRST binding's peer id and leaves by its egress: the
    // peer reached us through the same engine the tracker lists, so it must
    // see the same identity, and the replies must take the same tunnel.
    // `config.peer_id()` was right only for an engine without bindings.
    if let (Some(pv2_port), Some(first)) = (config.listen_port_proxy_v2.filter(|p| *p != 0), resolved_bindings.first()) {
        let tm = mgr.clone();
        let dm = disk.clone();
        let pid = first.peer_id;
        let pv2_egress = first.egress.clone();
        let u = utp_socket.clone();
        let bind_addr = config.listen_addr_proxy_v2.clone().unwrap_or_default();
        tokio::spawn(async move {
            if let Err(e) = crate::peer::listen_proxy_v2(bind_addr, pv2_port, tm, dm, pid, pv2_egress, u).await {
                error!("[engine] proxy-v2 listener failed: {}", e);
            }
        });
    }

    // Start the outbound dial queue consumer (with uTP socket for outgoing
    // fallback). Announces themselves belong to the Go control plane.
    // Multi-binding: dial queue consumer hashes peer addr → picks one binding,
    // source-binds outbound TcpSocket on that binding's listen_addr. Single
    // binding collapses to the legacy single-source-IP behavior.
    let dm2 = disk.clone();
    // Connection ceiling: honoured from here on. This key has been in the
    // config (and echoed by get_config) since long before anything read it,
    // so an existing install may already carry a value that has never taken
    // effect -- it starts biting at this upgrade.
    mgr.limiter().set_max_connections(config.max_connections);
    mgr.limiter().set_max_dials_per_sec(config.max_dials_per_sec);
    crate::tracker::start_announce_loop(dm2, resolved_bindings.clone(), utp_socket.clone(), config.max_dials_per_sec, mgr.limiter().clone());

    // The choker: spawned always, OFF unless `choking = true`. It was removed
    // outright in 2.4.13-typhon -- ticking every 10 s and choking all but the
    // top four peers of every seeding torrent (~13k on the hoard) churned the
    // peers into a ~300 ceiling instead of ~11k sustained -- and is back as
    // an opt-in, read live each tick (see `choking::choking_loop`).
    mgr.policy().set_choking(config.choking);
    mgr.policy().set_unchoke_slots(config.max_uploads_per_torrent as i64);
    tokio::spawn(crate::peer::choking::choking_loop(mgr.clone()));

    info!(
        "[engine] session started, listen={}, upload_limit={} B/s, download_limit={} B/s, peer_timeout={}s, resume_dir={}",
        config.listen_addr(),
        config.upload_limit,
        config.download_limit,
        mgr.policy().idle_timeout().as_secs(),
        config.resume_dir,
    );

    // Rate tracking tick, once a second: the page pushes a status frame every
    // second, and a 2 s tick left every other frame repeating the last figure,
    // so the speeds on screen moved at half the rate of everything around them.
    let tm_rate = mgr.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            tm_rate.update_rates();
        }
    });

    // Push-based stats snapshot emitter (every 1s, delta-filtered).
    // Replaces Go's `list_torrents` polling every 2s — cuts ~4-8% CPU spent
    // on 13k-torrent JSON serialization (see crate::rpc::events docs).
    let tm_stats = mgr.clone();
    tokio::spawn(async move {
        use std::collections::HashMap;
        use std::sync::atomic::Ordering;
        let mut last: HashMap<[u8; 20], (u64, u64, u8, usize, usize)> = HashMap::new();
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            // Skip the whole scan when nobody listens — saves 13k atomic loads
            // per second on a system with no subscribers.
            if tm_stats.bus().receiver_count() == 0 {
                continue;
            }
            let mut changed: Vec<crate::rpc::events::TorrentStatsMini> = Vec::new();
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
                    changed.push(crate::rpc::events::TorrentStatsMini {
                        info_hash: crate::torrent::hex_encode(&ih),
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
                tm_stats.bus().publish(crate::rpc::events::Event::StatsSnapshot { torrents: changed });
            }
        }
    });

    // Unseeded peers count (every 30s — O(N) scan too expensive for tight loop)
    let tm_unseeded = mgr.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            tm_unseeded.update_unseeded_count();
        }
    });

    // Persist completions as they happen, not on the next sweep.
    // Taken before the spawn, not inside it: the receiver exists once per
    // engine, and a task spawned only to find it already taken would sit there
    // for the life of the process doing nothing.
    if let Some(mut rx) = mgr.take_completion_receiver() {
        let tm_done = mgr.clone();
        tokio::spawn(async move {
            while let Some(ih) = rx.recv().await {
                tm_done.on_completed(&ih);
            }
        });
    }

    // Periodic resume save (every 5 min)
    let tm3 = mgr.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(300)).await;
            tm3.save_all_resume();
        }
    });
}
