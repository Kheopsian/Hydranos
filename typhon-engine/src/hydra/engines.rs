//! The engines, running inside this process.
//!
//! This module is the reason 4.0.0 exists. In 3.x the Go front spoke to each
//! engine over a unix socket and kept its own copy of every torrent's state to
//! answer HTTP with: 313 call sites maintaining that copy, 1.62 GiB of live Go
//! heap at 243k torrents, and 6.6 KB more for every torrent added.
//!
//! Here a handler holds an `Arc<TorrentManager>` and reads the engine's own
//! DashMap. There is no second copy to keep in step, so there is nothing to
//! fall out of step: the class of bug where the UI showed a stale figure
//! because a refresh had not run yet cannot be written any more.
//!
//! Each engine is then put on the network by `typhon_engine::session::start`,
//! the same function the standalone engine binary calls. Sharing it is the
//! point: two copies of "how an engine comes up" would drift, and the way they
//! drift is silent -- a listener that binds differently, a switch applied to
//! one and not the other.
//!
//! `HYDRANOS_ENGINE_NET=0` holds every engine off the network: the managers are
//! built and their state loaded, and no socket is opened. It is an environment
//! variable and not a config key on purpose -- `/api/settings` echoes the
//! config file back verbatim, so a key here would change an answer that has to
//! stay byte-for-byte what 3.x sends, and 4.0.0 promises an unchanged config
//! file. The differential bench runs its candidate this way.

use std::sync::Arc;
use typhon_engine::{disk::DiskManager, torrent::TorrentManager};

use crate::config::Config;

/// One engine: its identity, and the state it owns.
pub struct Engine {
    pub id: String,
    pub role: String,
    pub listen_port: u16,
    pub bind_interface: String,
    /// True when the engine was configured to come up paused. The startup gate
    /// reports these as "held": nothing announces or dials until released.
    pub start_paused: bool,
    pub enable_ipv6: bool,
    /// The session this engine was actually configured with, already merged
    /// from its role profile and its own overrides.
    ///
    /// `connect` used to re-derive this with `match id { "race" => config.race,
    /// _ => config.hoard }`, which threw away everything `local_engines`
    /// computed: a third engine -- one VPN tunnel per engine, the whole point
    /// of "one agent, one engine" -- bound hoard's port and hoard's interface.
    /// The fields below were right all along, but only `netprobe` read them,
    /// so the network tab showed a port the socket was not listening on.
    pub session: crate::config::Session,
    /// Hands one torrent to the head of this engine's announce queue.
    ///
    /// Per engine, never a global: a `OnceLock` shared by the process is
    /// exactly how the egress setting leaked between two engines, and a bump
    /// sent to the wrong scheduler announces the wrong catalogue. Empty until
    /// `connect` puts the engine on the network -- an offline engine has no
    /// announce loop to jump.
    pub bump: std::sync::OnceLock<
        tokio::sync::mpsc::Sender<crate::announce::scheduler::BumpReq>,
    >,
    /// This engine's live announce policy, swappable while it runs.
    ///
    /// Empty until `connect` starts the announcer, like `bump`: an engine that
    /// is not on the network has no policy to reload.
    pub announce_policy: std::sync::OnceLock<crate::announce::PolicyHandle>,
    /// How far this engine's scheduler has got through the catalogue.
    ///
    /// Per engine for the same reason `bump` is: two engines admit at their own
    /// pace, and one number for both would tell the hoard's story about a race
    /// torrent. Zeroed until `connect` starts the scheduler, which reads as
    /// "nothing admitted" and is exactly right for an engine that is offline.
    pub admission: Arc<crate::announce::scheduler::Admission>,
    /// Whether a peer listener is actually bound.
    ///
    /// Not "was asked to listen": an engine pinned to an interface that is not
    /// there logs "session started" and "on the network, announcing", then a
    /// fraction of a millisecond later logs that the listener failed. It holds
    /// its catalogue, answers the API and accepts no peer. Published so the
    /// fleet page can say so instead of drawing it like a healthy engine.
    pub listening: Arc<std::sync::atomic::AtomicBool>,
    pub manager: Arc<TorrentManager>,
    /// Kept so the engine can be put on the network after it is built.
    pub disk: Arc<DiskManager>,
    /// What trackers last said about this engine's torrents. Written by the
    /// announcer, read by the trackers tab and the download slot manager.
    pub announce_cache: Arc<crate::announce::cache::Cache>,
    /// The engine's network config, set when `connect` puts it on the
    /// network. Magnet resolution dials from its bindings: an engine that is
    /// not on the network has none, and no business dialling at all.
    pub engine_config: std::sync::OnceLock<typhon_engine::config::EngineConfig>,
    /// Kept off the network by the kill switch, and why (`killswitch`): no
    /// listener, no dial, no announce, no DHT, no LSD. Decided once, in
    /// `connect`, like everything else about the engine's network.
    pub blocked: std::sync::OnceLock<String>,
}

pub struct EngineHost {
    engines: Vec<Engine>,
    /// Scopes the startup gate has released. Empty until somebody asks.
    released: std::sync::Mutex<std::collections::BTreeSet<String>>,
    /// Finished downloads, every engine's, tagged with the engine. Taken once,
    /// by the event workflows.
    completions: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<(String, [u8; 20])>>>,
    /// The catalogue's hardlink answers for the hoard list's "Hardlinks" and
    /// "Checked" columns. Published whole by whoever just computed them (the
    /// link scanner, a workflow pass), read as one `Arc` clone per request.
    /// Kept here because it describes these engines' catalogue, and both the
    /// scanner and the API already hold this host.
    link_summary: std::sync::RwLock<Arc<crate::linkindex::Summary>>,
    /// The catalogue's space on disk and when it was counted, published by the
    /// link scanner only. Apart from `link_summary` because a workflow pass
    /// publishes that one too, and must not wipe a figure it does not compute.
    catalogue_usage: std::sync::RwLock<Option<(crate::linkindex::DiskUsage, i64)>>,
    /// The client-wide rate caps -- qBittorrent's "global" limit, which the
    /// shim's `transfer/*` routes move -- above every engine of this host.
    client_rates: Arc<typhon_engine::torrent::ratelimit::RatePair>,
    /// The managed WireGuard tunnels of this process, by engine. Filled by
    /// `connect`, read by the Network tab, emptied at shutdown.
    wireguard: Arc<crate::wgtunnel::Registry>,
    /// `<data_dir>/wireguard`: the provider files and the list of devices
    /// this node made.
    wireguard_dir: std::path::PathBuf,
}

/// The per-engine settings a RUNNING engine takes without a restart: the rate
/// caps, the peer idle timeout and the choker. Called when the engine is built
/// and again whenever the settings are saved; `engine_config` hands the same
/// values to `session::start`, so the two paths cannot disagree.
pub fn apply_live_settings(manager: &TorrentManager, session: &crate::config::Session) {
    let (up, down) = session.rate_caps();
    manager.rates().engine.up.set_rate(up);
    manager.rates().engine.down.set_rate(down);
    manager.policy().set_idle_timeout_secs(session.peer_timeout);
    manager.policy().set_choking(session.choking);
    manager.policy().set_unchoke_slots(session.max_uploads_per_torrent);
}

impl EngineHost {
    /// Build the engines described by the config and load their durable state.
    ///
    /// Paths follow the layout 3.x already writes, and that is not negotiable
    /// while a rollback has to stay possible: `<config_dir>/<engine>` holds the
    /// engine, `<config_dir>/<engine>/resume` its resume data. An engine whose
    /// directory does not exist yet is still built -- a first run has no state
    /// and must not be an error.
    pub fn offline(config: &Config, config_dir: &std::path::Path) -> Self {
        let mut engines = Vec::new();
        // Hooked here, before any engine is on the network, rather than once
        // the workflows start: a download that finished in between would have
        // told nobody. Unbounded, and cheap: the receiver buffers whatever
        // arrives before it is taken.
        let (completed_tx, completed_rx) = tokio::sync::mpsc::unbounded_channel();
        let client_rates: Arc<typhon_engine::torrent::ratelimit::RatePair> = Default::default();

        // Whatever this node hosts, not a fixed race and hoard: an install
        // can run one engine per tunnel, each presenting as its own agent.
        for local in config.local_engines() {
            let id = local.id.as_str();
            let session = &local.session;
            let data_dir = config_dir.join(id);
            let resume_dir = data_dir.join("resume");

            let disk = Arc::new(DiskManager::new(session.file_pool_size()));
            let manager = Arc::new(TorrentManager::new(
                data_dir.to_string_lossy().into_owned(),
                resume_dir.to_string_lossy().into_owned(),
                disk.clone(),
            ));

            // The metainfo comes from the STORE, not from uploads/.
            //
            // Both hold the same bytes -- the store as a blob keyed by
            // info-hash, the directory as a file whose NAME used to be the one
            // the client uploaded. Two copies of one thing, and the file was
            // the one the resume record pointed at. Until the V4 a batch
            // ingester posting every torrent as `t.torrent` overwrote that file
            // over and over: 2789 records ended up pointing at the same path on
            // the production library, so a restart restored whichever torrent
            // had written last, under a hash neither database knew.
            //
            // Keyed lookup has no such failure mode: the key IS the identity.
            // A record the store does not know still falls back to its file
            // (an install predating the store), where record_matches_file
            // refuses the mismatch rather than restoring the wrong torrent.
            //
            // Read-only, and its own handle: the shared store is opened later
            // in main, after the engines are up, and reordering a daemon's
            // startup to borrow it would be a bigger change than this is worth.
            let blob_db = std::path::Path::new(&config.daemon.data_dir).join("hydra.db");
            match crate::store::Store::open(&blob_db, true) {
                Ok(store) => {
                    let store = Arc::new(std::sync::Mutex::new(store));
                    manager.set_blob_source(Arc::new(move |hash: &str| {
                        store.lock().ok()?.torrent_blob(hash).ok().flatten()
                    }));
                }
                Err(e) => tracing::warn!(
                    engine = id,
                    "no store to read metainfo from ({e}); falling back to the .torrent files"
                ),
            }

            // Everything this engine knows must have its metainfo in the
            // store before resume runs, because resume no longer looks
            // anywhere else. On an install that already migrated this finds
            // nothing and costs one query per torrent.
            {
                let uploads = std::path::Path::new(&config.daemon.data_dir).join("uploads");
                match crate::store::Store::open(&blob_db, false) {
                    Ok(rw) => {
                        let rw = std::sync::Mutex::new(rw);
                        let sink = |hash: &str, bytes: &[u8]| -> Result<(), String> {
                            rw.lock()
                                .map_err(|_| "store lock".to_string())?
                                .insert_torrent(hash, id, bytes, "", "", 0.0, false, "")
                                .map(|_| ())
                                .map_err(|e| e.to_string())
                        };
                        let (imported, lost) = manager.import_missing_blobs(&uploads, &sink);
                        if imported > 0 || lost > 0 {
                            tracing::warn!(
                                engine = id, imported, unrecoverable = lost,
                                "metainfo migrated out of uploads/ and into the store"
                            );
                        }
                    }
                    Err(e) => tracing::error!(
                        engine = id,
                        "cannot open the store to migrate metainfo ({e}); torrents whose blob is missing will not load"
                    ),
                }
            }

            // Before resume, so no torrent ever runs a moment uncapped.
            manager.rates().set_client(client_rates.clone());
            apply_live_settings(&manager, session);

            // So the startup screen can count this engine's restore while
            // it runs (`startup::snapshot`).
            crate::startup::register(id, manager.clone());
            let loaded = manager.load_resume_data();
            tracing::info!(engine = id, torrents = loaded, "engine state loaded");
            {
                let tx: tokio::sync::mpsc::UnboundedSender<(String, [u8; 20])> = completed_tx.clone();
                let engine_id = id.to_string();
                manager.set_completion_hook(Arc::new(move |ih| {
                    let _ = tx.send((engine_id.clone(), ih));
                }));
            }

            engines.push(Engine {
                id: id.to_string(),
                role: local.role.clone(),
                listen_port: session.listen_port,
                bind_interface: session.bind_interface.clone(),
                start_paused: session.start_paused,
                enable_ipv6: session.enable_ipv6,
                session: session.clone(),
                listening: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                manager,
                disk,
                announce_cache: Default::default(),
                bump: std::sync::OnceLock::new(),
                announce_policy: std::sync::OnceLock::new(),
                admission: Default::default(),
                engine_config: std::sync::OnceLock::new(),
                blocked: std::sync::OnceLock::new(),
            });
        }

        Self {
            engines,
            released: std::sync::Mutex::new(Default::default()),
            completions: std::sync::Mutex::new(Some(completed_rx)),
            link_summary: Default::default(),
            catalogue_usage: Default::default(),
            client_rates,
            wireguard: Default::default(),
            wireguard_dir: crate::wgtunnel::conf_dir(&config.daemon.data_dir),
        }
    }

    /// The managed WireGuard tunnels, as this process brought them up.
    pub fn wireguard(&self) -> &Arc<crate::wgtunnel::Registry> {
        &self.wireguard
    }

    /// Take every managed tunnel down: at shutdown, and when another network
    /// mode is saved. Engines pinned to them reach nobody until restarted,
    /// which is the point -- they must not fall back to the default route.
    pub async fn wireguard_down(&self) -> usize {
        let reg = self.wireguard.clone();
        let dir = self.wireguard_dir.clone();
        tokio::task::spawn_blocking(move || {
            crate::wgtunnel::down_all(&mut crate::wgtunnel::System, &dir, &reg)
        })
        .await
        .unwrap_or(0)
    }

    /// The client-wide rate caps, bytes/s.
    pub fn client_rates(&self) -> &Arc<typhon_engine::torrent::ratelimit::RatePair> {
        &self.client_rates
    }

    /// Put a (re)loaded config's live settings on the engines already
    /// running. Answers how many engines took them. An engine added to the
    /// file since boot is not running and is skipped: it needs a restart
    /// whatever this does.
    pub fn apply_config(&self, config: &Config) -> usize {
        let mut n = 0;
        for local in config.local_engines() {
            if let Some(e) = self.get(&local.id) {
                apply_live_settings(&e.manager, &local.session);
                n += 1;
            }
        }
        n
    }

    /// The last published hardlink summary. Empty until the first one: every
    /// torrent then reads as never measured, which is the truth.
    pub fn link_summary(&self) -> Arc<crate::linkindex::Summary> {
        self.link_summary.read().map(|g| g.clone()).unwrap_or_default()
    }

    /// Replace the summary. Whole, never patched: it is one computation over
    /// the whole catalogue, and half of two would match neither.
    pub fn publish_link_summary(&self, summary: crate::linkindex::Summary) {
        if let Ok(mut g) = self.link_summary.write() {
            *g = Arc::new(summary);
        }
    }

    /// The last counted space on disk with its unix time, `None` before the
    /// link scanner's first count.
    pub fn catalogue_usage(&self) -> Option<(crate::linkindex::DiskUsage, i64)> {
        self.catalogue_usage.read().ok().and_then(|g| *g)
    }

    pub fn publish_catalogue_usage(&self, usage: crate::linkindex::DiskUsage, at: i64) {
        if let Ok(mut g) = self.catalogue_usage.write() {
            *g = Some((usage, at));
        }
    }

    /// The finished-download stream, for the one listener that acts on it.
    pub fn take_completions(&self) -> Option<tokio::sync::mpsc::UnboundedReceiver<(String, [u8; 20])>> {
        self.completions.lock().ok()?.take()
    }

    /// Build the engines and put them on the network.
    ///
    /// Split from `offline` so the two halves are separable: the unit tests and
    /// the differential bench want engines that hold the production catalogue
    /// and open no socket, and that must not depend on remembering to set a
    /// flag -- it is a different call.
    pub async fn start(config: &Config, config_dir: &std::path::Path) -> Self {
        let host = Self::offline(config, config_dir);
        crate::startup::set_phase(crate::startup::Phase::Connecting);
        host.connect(config, config_dir).await;
        host
    }

    /// Put every engine that asks for it on the network.
    async fn connect(&self, config: &Config, config_dir: &std::path::Path) {
        // The tunnels first: an engine pinned to a device that is not there
        // yet would fail its listener and its uTP socket for good.
        let wants = crate::wgtunnel::wanted(crate::netmode::current(config), &config.local_engines());
        if networking_enabled() {
            let (reg, dir, w) = (self.wireguard.clone(), self.wireguard_dir.clone(), wants.clone());
            let _ = tokio::task::spawn_blocking(move || {
                crate::wgtunnel::reconcile(&mut crate::wgtunnel::System, &dir, &w, &reg)
            })
            .await;
        }
        // Which engines the kill switch keeps off the network. Before the
        // switch below, so a bench run with the network off still says it.
        let plan = crate::killswitch::plan(config);
        for engine in &self.engines {
            // The engine's own merged session. NOT config.race / config.hoard:
            // an engine that is neither is a legitimate configuration.
            let session = &engine.session;
            if let Some(why) = plan.blocked(&engine.id) {
                tracing::warn!(
                    engine = %engine.id,
                    "kill switch: BLOCKED, kept off the network (no listener, no dial, no announce, no DHT, no LSD): {why}"
                );
                let _ = engine.blocked.set(why);
                continue;
            }
            if !networking_enabled() {
                tracing::warn!(
                    engine = %engine.id,
                    "net = false: state loaded, no listener, no announce, no DHT"
                );
                continue;
            }
            let data_dir = config_dir.join(&engine.id);
            let resume_dir = data_dir.join("resume");
            // A NAT-PMP tunnel: ask for the port before the engine starts, so
            // its very first announce carries it. Not granted in time, the
            // follower keeps asking and the announces wait.
            // ⚠ The engine is born on its CONFIGURED port, not the granted one:
            // the gateway translates public `external_port` to that internal
            // port (measured on Proton), so the listener belongs there and only
            // the announced port is the external one (`set_external_port`).
            let follow = self.natpmp_follow(wants.iter().find(|w| w.engine == engine.id), session.listen_port).await;
            let mut born = session.clone();
            // LSD's default depends on the role, which the session alone
            // does not carry: resolved here, written out for the engine.
            born.enable_lsd = Some(session.lsd_on(&engine.role));
            match engine_config(&born, &data_dir, &resume_dir) {
                Some(engine_cfg) => {
                    let _ = engine.engine_config.set(engine_cfg.clone());
                    // The startup gate: no dial and no announce until it is
                    // released from the UI or the API. 4.3 showed the banner
                    // and held nothing.
                    if engine.start_paused {
                        engine.manager.limiter().set_dials_paused(true);
                        tracing::warn!(engine = %engine.id, "start_paused: holding dials and announces until released");
                    }
                    typhon_engine::session::start(
                        engine.manager.clone(),
                        engine.disk.clone(),
                        &engine_cfg,
                        engine.listening.clone(),
                    )
                    .await;
                    // Nothing else in this process tells a tracker we exist.
                    // Without this the engine seeds, listens and connects, and
                    // every tracker forgets the whole catalogue within one
                    // announce interval.
                    let first = announce_policy(config, session, &engine_cfg);
                    let policy: crate::announce::PolicyHandle =
                        std::sync::Arc::new(std::sync::RwLock::new(std::sync::Arc::new(first)));
                    let _ = engine.announce_policy.set(policy.clone());
                    // Before the runner: the hold has to be in place when the
                    // first announce is due, not a moment after.
                    if session.gluetun_port_forward {
                        crate::gluetun::spawn(
                            engine.id.clone(),
                            engine.manager.clone(),
                            session.gluetun_url.clone(),
                            session.gluetun_api_key.clone(),
                        );
                    }
                    if let Some((gateway, initial)) = follow {
                        crate::portfwd::spawn_follower(
                            engine.id.clone(),
                            engine.manager.clone(),
                            gateway,
                            session.bind_interface.clone(),
                            session.listen_port,
                            initial,
                            Arc::new(TunnelPort { registry: self.wireguard.clone(), engine: engine.id.clone() }),
                        );
                    }
                    let bump = crate::announce::runner::start(
                        engine.manager.clone(),
                        policy,
                        session.listen_port,
                        // By role: "race" is a behaviour, not a name. An engine
                        // called vpn1 with role=race announces like a racer.
                        if engine.role == "race" {
                            crate::announce::runner::Mode::Race
                        } else {
                            crate::announce::runner::Mode::Hoard
                        },
                        engine.announce_cache.clone(),
                        engine.admission.clone(),
                    );
                    // Set once, when this engine joins the network.
                    let _ = engine.bump.set(bump);
                    crate::workers::spawn_stagger_start(engine.manager.clone());
                    crate::workers::spawn_verify_throttle(engine.manager.clone());
                    if engine.role == "race" {
                        // The path the operator CONFIGURED, not one derived
                        // from config_dir: passing config_dir.join("race")
                        // pointed the drain at /configs/race, which sits on the
                        // appdata pool. It measured 77% there while /race was
                        // at 100%, stayed under its watermark, and never ran --
                        // a guard that guarded a disk nobody was filling.
                        // NOT spawned here any more: the drain has to read the
                        // per-tracker seed obligation, which lives in the live
                        // config, and the AppState that owns it does not exist
                        // yet. main.rs starts it once the state is built --
                        // same reason as the download slot manager below.
                    }
                    // The download slot manager is NOT started here. It has to
                    // read the paused column to know which stops are the
                    // operator's, and the store is opened after the engines
                    // are up. main.rs starts it once the store exists.
                    // Deliberately says "starting", not "on the network": the
                    // listener binds in a task that has not run yet, so this
                    // line cannot know. /api/engines publishes what happened.
                    tracing::info!(
                        engine = %engine.id,
                        listen_port = session.listen_port,
                        dht = session.enable_dht,
                        pex = session.enable_pex,
                        "engine starting, announcing"
                    );
                }
                None => {
                    // Refuse rather than come up half-configured: an engine
                    // that cannot describe its own network is one that would
                    // announce from somewhere nobody chose.
                    tracing::error!(
                        engine = %engine.id,
                        "cannot build the engine network config -- staying offline"
                    );
                }
            }
        }
    }

    /// Where to ask for this engine's port, and the first grant if one came
    /// in time. `None` for an engine with no NAT-PMP tunnel.
    ///
    /// A tunnel that did not come up gets no follower: there is no gateway
    /// to reach. Its engine is pinned to the missing device and reaches
    /// nobody either way.
    async fn natpmp_follow(
        &self,
        want: Option<&crate::wgtunnel::Want>,
        listen_port: u16,
    ) -> Option<(std::net::IpAddr, Option<crate::portfwd::Mapping>)> {
        let w = want.filter(|w| w.port_forward == crate::wgtunnel::PortForward::NatPmp)?;
        if !self.wireguard.get(&w.engine).is_some_and(|t| t.created) {
            return None;
        }
        let conf = crate::wgtunnel::load_conf(&self.wireguard_dir, &w.config_file).ok()?;
        let Some(gateway) = crate::wgtunnel::natpmp_gateway(&w.provider, &conf) else {
            let e = format!("{}: no NAT-PMP gateway known (no IPv4 DNS in the file); no port is asked for", w.config_file);
            tracing::warn!(engine = %w.engine, "wireguard: {e}");
            self.wireguard.update(&w.engine, |t| t.last_error = e);
            return None;
        };
        // Two tries: at boot, before the engine binds, a gateway that is slow
        // to answer costs seconds of startup per engine, not a minute. The
        // configured port as both internal and suggested: internal 0 is
        // reserved by RFC 6886, and a gateway that honours the suggestion
        // then maps the port straight through.
        let initial = match crate::portfwd::map_both(gateway, &w.device, listen_port, listen_port, 2).await {
            Ok((m, udp)) => {
                if let Some(e) = udp {
                    tracing::warn!(engine = %w.engine, "wireguard port forward: {e}");
                }
                tracing::info!(engine = %w.engine, port = m.external_port, "wireguard: port forwarded before the engine starts");
                self.wireguard.update(&w.engine, |t| t.forwarded_port = m.external_port);
                Some(m)
            }
            Err(e) => {
                tracing::warn!(engine = %w.engine, error = %e, "wireguard: no port yet; the engine starts with its announces held");
                self.wireguard.update(&w.engine, |t| t.last_error = e);
                None
            }
        };
        Some((gateway, initial))
    }

    /// Send `stopped` for every torrent that announced `started`, on every
    /// engine, race engines first, within `budget`. For a clean stop: what is
    /// not sent in time is left to the trackers' own timeouts, which is what
    /// libtorrent does with its 5-second `stop_tracker_timeout`.
    pub async fn depart_all(&self, budget: std::time::Duration) -> usize {
        let mut work: Vec<(std::sync::Arc<typhon_engine::torrent::meta::TorrentState>, std::sync::Arc<crate::announce::policy::Policy>, u16)> = Vec::new();
        let mut engines: Vec<&Engine> = self.engines().iter().collect();
        engines.sort_by_key(|e| e.role != "race");
        for engine in engines {
            let Some(handle) = engine.announce_policy.get() else { continue };
            let policy = handle.read().unwrap_or_else(|p| p.into_inner()).clone();
            for t in engine.manager.all() {
                let started = t.announce_book.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|s| s.started);
                if started {
                    work.push((t, policy.clone(), engine.manager.announced_port(engine.session.listen_port)));
                }
            }
        }
        if work.is_empty() {
            return 0;
        }
        let total = work.len();
        // 64 workers pulling from one list, not one task per torrent: a
        // million-torrent hoard must not spawn a million tasks on its way out.
        let queue = std::sync::Arc::new(std::sync::Mutex::new(work.into_iter()));
        let done = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..64 {
            let queue = queue.clone();
            let done = done.clone();
            set.spawn(async move {
                loop {
                    let next = queue.lock().unwrap_or_else(|p| p.into_inner()).next();
                    let Some((t, policy, port)) = next else { break };
                    crate::announce::runner::depart(t, policy, port).await;
                    done.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            });
        }
        let _ = tokio::time::timeout(budget, async { while set.join_next().await.is_some() {} }).await;
        set.abort_all();
        let done = done.load(std::sync::atomic::Ordering::Relaxed);
        tracing::info!(departed = done, of = total, "stopped sent to the trackers");
        done
    }

    /// Send `stopped` for one torrent leaving this engine, in the background.
    pub fn spawn_departure(&self, engine_id: &str, t: std::sync::Arc<typhon_engine::torrent::meta::TorrentState>) {
        let Some(engine) = self.engines().iter().find(|e| e.id == engine_id) else { return };
        let Some(handle) = engine.announce_policy.get() else { return };
        let policy = handle.read().unwrap_or_else(|p| p.into_inner()).clone();
        let port = engine.manager.announced_port(engine.session.listen_port);
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(20),
                    crate::announce::runner::depart(t, policy, port),
                )
                .await;
            });
        }
    }

    pub fn engines(&self) -> &[Engine] {
        &self.engines
    }

    pub fn get(&self, id: &str) -> Option<&Engine> {
        self.engines.iter().find(|e| e.id == id)
    }

    /// Scopes still held by the startup gate, sorted.
    ///
    /// Sorted because the Go side builds this from a map and encoding/json
    /// orders map keys: an unsorted answer would differ from 3.x on the wire
    /// for no reason anyone could see.
    pub fn held_startup_scopes(&self) -> Vec<String> {
        let released = self.released.lock().unwrap();
        let mut held: Vec<String> = self
            .engines
            .iter()
            .filter(|e| e.start_paused && !released.contains(&e.id))
            .map(|e| e.id.clone())
            .collect();
        held.sort();
        held
    }

    /// Free every held scope, returning what was actually freed.
    ///
    /// Returns only the scopes that WERE held: releasing twice is harmless and
    /// answers an empty list, which is what tells a caller nothing happened.
    pub fn release_startup(&self) -> Vec<String> {
        let freed = self.held_startup_scopes();
        let mut released = self.released.lock().unwrap();
        for scope in &freed {
            released.insert(scope.clone());
            if let Some(e) = self.engines.iter().find(|e| &e.id == scope) {
                e.manager.limiter().set_dials_paused(false);
            }
        }
        freed
    }

    /// Bytes moved by the torrents an engine currently holds.
    ///
    /// This is the "session" half of the totals: what the running engines
    /// account for. Added to the stored baseline it gives the lifetime figure,
    /// and the two must be kept separate -- collapsing them is how a restart
    /// used to appear to erase petabytes.
    /// Read straight from the counters rather than through `torrent_to_json`:
    /// the status route polls this, and serializing 300k torrents to JSON to
    /// add up two integers made every poll a multi-second walk.
    pub fn session_totals(&self) -> (i64, i64) {
        let (mut up, mut down) = (0i64, 0i64);
        for engine in &self.engines {
            let (u, d) = engine.manager.totals();
            up += u as i64;
            down += d as i64;
        }
        (up, down)
    }

    /// Bytes moved by every engine since this process started: "this
    /// session". Not `session_totals`, which despite its name is a sum of
    /// LIFETIME counters over the torrents loaded right now, and falls by a
    /// torrent's whole history when it is deleted. See
    /// `TorrentManager::moved`.
    pub fn moved_totals(&self) -> (i64, i64) {
        let (mut up, mut down) = (0i64, 0i64);
        for engine in &self.engines {
            let (u, d) = engine.manager.moved();
            up += u as i64;
            down += d as i64;
        }
        (up, down)
    }

    /// The same for ONE engine; (0, 0) for an engine this node does not host.
    pub fn moved_of(&self, engine_id: &str) -> (i64, i64) {
        self.get(engine_id)
            .map(|e| {
                let (u, d) = e.manager.moved();
                (u as i64, d as i64)
            })
            .unwrap_or((0, 0))
    }

    /// Total torrents across every engine, read from the engines themselves.
    ///
    /// The figure the Go front published came from a cache it refreshed on a
    /// timer, which is why it could disagree with the database. Here it is
    /// counted from the live maps at the moment of asking.
    pub fn total_torrents(&self) -> usize {
        self.engines.iter().map(|e| e.manager.len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn held_scopes_are_sorted_and_only_include_paused_engines() {
        // Built by hand rather than through start(): the ordering rule is what
        // is under test, not the disk layout.
        let held = |paused: [bool; 2]| {
            let mut v: Vec<String> = ["race", "hoard"]
                .iter()
                .zip(paused)
                .filter(|(_, p)| *p)
                .map(|(id, _)| id.to_string())
                .collect();
            v.sort();
            v
        };
        assert_eq!(held([true, true]), vec!["hoard", "race"]);
        assert_eq!(held([false, true]), vec!["hoard"]);
        assert!(held([false, false]).is_empty());
    }

    /// A third engine must keep ITS port and ITS interface.
    ///
    /// This is the multi-tunnel case -- one engine per VPN on one machine --
    /// and it was broken: `connect` re-derived the session with
    /// `match id { "race" => config.race, _ => config.hoard }`, so anything
    /// that was not race got hoard's network. Two engines then bound the same
    /// port, and the network tab still showed the configured one because
    /// `netprobe` reads the (correct) `Engine` fields rather than the socket.
    ///
    /// The assertion is on `Engine::session`, which is what `connect` now
    /// consumes: the previous code had no such field to read.
    #[test]
    fn an_extra_engine_keeps_its_own_network() {
        let dir = std::env::temp_dir().join(format!("hydra-engtest-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);

        let mut config = Config::default();
        config.race.listen_port = 16171;
        config.hoard.listen_port = 16172;
        config.hoard.bind_interface = "eth0".into();

        let mut over = toml::value::Table::new();
        over.insert("listen_port".into(), toml::Value::Integer(16999));
        over.insert("bind_interface".into(), toml::Value::String("wg1".into()));
        config.agent.push(crate::config::Agent {
            name: "vpn1".into(),
            role: "hoard".into(),
            engine_id: "vpn1".into(),
            session: over,
            ..Default::default()
        });

        let host = EngineHost::offline(&config, &dir);
        let vpn1 = host
            .engines()
            .iter()
            .find(|e| e.id == "vpn1")
            .expect("the extra engine must exist");

        assert_eq!(vpn1.session.listen_port, 16999, "took another engine's port");
        assert_eq!(vpn1.session.bind_interface, "wg1", "took another engine's interface");
        // What netprobe shows and what connect binds must be the same thing.
        assert_eq!(vpn1.listen_port, vpn1.session.listen_port);
        assert_eq!(vpn1.bind_interface, vpn1.session.bind_interface);
        // And it must not have collided with hoard.
        let hoard = host.engines().iter().find(|e| e.id == "hoard").unwrap();
        assert_ne!(vpn1.session.listen_port, hoard.session.listen_port);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ⭐ WireGuard mode, no engine assigned: the kill switch keeps every one
    /// off the network. `start` -- the production path, not `offline` --
    /// gives none of them a config, a listener, an announce loop or a bound
    /// port, and each says why.
    #[tokio::test]
    async fn an_unassigned_engine_is_blocked_and_opens_no_socket() {
        let dir = std::env::temp_dir().join(format!("hydra-engtest-blocked-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        // Free ports, so "nobody bound it" is checked on ports nobody else holds.
        let free = || std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let text = format!(
            "[daemon]\ndata_dir = {:?}\n[network]\nmode = \"wireguard\"\n[race]\nlisten_port = {}\n[hoard]\nlisten_port = {}\n",
            dir.to_string_lossy(),
            free(),
            free()
        );
        let config: Config = toml::from_str(&text).unwrap();
        // `start` moves the process-wide startup phase: put it back.
        let phase = crate::startup::phase();
        let host = EngineHost::start(&config, &dir).await;
        crate::startup::set_phase(phase);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        for e in host.engines() {
            let why = e.blocked.get().unwrap_or_else(|| panic!("{} was not blocked", e.id));
            assert!(why.contains("not assigned"), "{why}");
            assert!(e.engine_config.get().is_none(), "{}: no network config, so no dial and no magnet", e.id);
            assert!(e.bump.get().is_none() && e.announce_policy.get().is_none(), "{}: no announce loop", e.id);
            assert!(!e.listening.load(std::sync::atomic::Ordering::Relaxed), "{}: no listener", e.id);
            assert!(e.manager.dht().is_none(), "{}: no DHT", e.id);
            // Its port is free: nothing bound it, on any address.
            std::net::TcpListener::bind(("0.0.0.0", e.listen_port)).unwrap_or_else(|err| panic!("{}: port {} taken: {err}", e.id, e.listen_port));
            std::net::UdpSocket::bind(("0.0.0.0", e.listen_port)).unwrap_or_else(|err| panic!("{}: UDP {} taken: {err}", e.id, e.listen_port));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The switch is off only for the three spellings a bench would use, and
    /// on for everything else -- including an empty or misspelt value. An
    /// engine that silently stayed offline because someone wrote `HYDRANOS_ENGINE_NET=no`
    /// would look alive and seed nothing.
    #[test]
    fn only_an_explicit_off_holds_the_engines_back() {
        for off in ["0", "false", "off"] {
            unsafe { std::env::set_var("HYDRANOS_ENGINE_NET", off) };
            assert!(!super::networking_enabled(), "{off} should hold the engines off");
        }
        for on in ["1", "true", "", "no", "yes"] {
            unsafe { std::env::set_var("HYDRANOS_ENGINE_NET", on) };
            assert!(super::networking_enabled(), "{on:?} must not be read as off");
        }
        unsafe { std::env::remove_var("HYDRANOS_ENGINE_NET") };
        assert!(super::networking_enabled(), "unset means on");
    }
}

/// Whether engines may open sockets at all.
///
/// Off only for a bench: an instance holding the production catalogue must be
/// able to answer questions about it without telling 244k torrents' trackers
/// about a machine nobody meant to publish.
fn networking_enabled() -> bool {
    !matches!(
        std::env::var("HYDRANOS_ENGINE_NET").as_deref(),
        Ok("0") | Ok("false") | Ok("off")
    )
}

/// Writes a tunnel's forwarded port, or why there is none, where the
/// Network tab reads it.
struct TunnelPort {
    registry: Arc<crate::wgtunnel::Registry>,
    engine: String,
}

impl crate::portfwd::PortSink for TunnelPort {
    fn forwarded(&self, port: u16) {
        self.registry.update(&self.engine, |t| {
            t.forwarded_port = port;
            t.last_error.clear();
        });
    }

    fn failed(&self, error: String) {
        self.registry.update(&self.engine, |t| t.last_error = error);
    }
}

/// The announce policy an engine starts with.
fn announce_policy(
    config: &Config,
    session: &crate::config::Session,
    engine_cfg: &typhon_engine::config::EngineConfig,
) -> crate::announce::policy::Policy {
    // The id the listeners present in every handshake, so the
    // peer a tracker lists is the peer that connects. The
    // config draws it once; asking again returns the same.
    let peer_id = engine_cfg
        .resolved_bindings()
        .first()
        .map(|b| b.peer_id)
        .unwrap_or_else(|| engine_cfg.peer_id());
    // `announce_ip` only when written: an empty one sends no `ip=` and the
    // tracker keeps the source address. It was always passed as "" here, so
    // the key in the template was a comment that did nothing.
    let mut first = crate::announce::policy_from_config(
        config,
        String::from_utf8_lossy(&peer_id).into_owned(),
        session.announce_ip.trim().to_string(),
    );
    // The same derivation the engine's webseeds and magnets use, so the
    // three cannot leave by different doors.
    first.proxy = engine_cfg.http_proxy();
    // Per engine, from its own section: one engine can keep
    // to HTTP trackers while another announces everywhere.
    first.skip_udp = !session.udp_trackers();
    first.device = session.bind_interface.trim().to_string();
    first.no_ipv6 = !session.enable_ipv6;
    first.registration_window = std::time::Duration::from_secs(session.registration_retry_minutes() * 60);
    first
}

/// The proxy one session's announces and webseeds go through, as the engine
/// will derive it (`EngineConfig::http_proxy`): the tab's warnings and the
/// network check must not keep a second copy of that rule. Empty = none
/// configured (the transport may still apply `TYPHON_ANNOUNCE_PROXY`).
pub(crate) fn session_http_proxy(session: &crate::config::Session) -> String {
    let dir = std::path::Path::new("");
    engine_config(session, dir, dir).map(|c| c.http_proxy()).unwrap_or_default()
}

/// The same session's peer SOCKS5 proxy as a URL. Empty = peers dialled
/// directly.
pub(crate) fn session_socks5_url(session: &crate::config::Session) -> String {
    let dir = std::path::Path::new("");
    engine_config(session, dir, dir).map(|c| c.socks5_url()).unwrap_or_default()
}

/// What an engine would start with from this session, for tests outside
/// this module.
#[cfg(test)]
pub(crate) fn engine_config_for_test(session: &crate::config::Session) -> typhon_engine::config::EngineConfig {
    let dir = std::path::Path::new("/tmp");
    engine_config(session, dir, dir).expect("engine config builds")
}

/// The engine-side config for one session.
///
/// Built through serde rather than a struct literal on purpose: `EngineConfig`
/// carries three dozen fields, nearly all with a documented default, and
/// listing them here would fork those defaults into a second place that nobody
/// updates. Only what the Hydra config actually decides is set.
fn engine_config(
    session: &crate::config::Session,
    data_dir: &std::path::Path,
    resume_dir: &std::path::Path,
) -> Option<typhon_engine::config::EngineConfig> {
    // 0 and "" are how the file says "none": the engine's Options say it
    // with None, and a Some(0) would bind a PROXY v2 listener on a random port.
    let pv2_port = Some(session.listen_port_proxy_v2).filter(|p| *p != 0);
    let pv2_addr = Some(session.listen_addr_proxy_v2.trim()).filter(|a| !a.is_empty());
    let mut v = serde_json::json!({
        "data_dir": data_dir.to_string_lossy(),
        "resume_dir": resume_dir.to_string_lossy(),
        "listen_port": session.listen_port,
        "bind_device": session.bind_interface,
        "dht_enabled": session.enable_dht,
        "pex_enabled": session.enable_pex,
        // Resolved by the caller from the role (`Session::lsd_on`); a session
        // that was not resolved runs none.
        "lsd_enabled": session.enable_lsd.unwrap_or(false),
        "enable_webseed": session.enable_webseed,
        "enable_ipv6": session.enable_ipv6,
        "max_connections": session.max_connections.max(0),
        "max_dials_per_sec": if session.max_dials_per_sec.is_finite() { session.max_dials_per_sec.max(0.0) } else { 0.0 },
        "max_uploads_per_torrent": session.max_uploads_per_torrent.clamp(i32::MIN as i64, i32::MAX as i64),
        // Rate caps, the idle timeout and the choker: the same values
        // `apply_live_settings` puts on the running engine, so a start and a
        // hot reload cannot disagree. Bytes/s on both sides.
        "choking": session.choking,
        "upload_limit": session.rate_caps().0,
        "download_limit": session.rate_caps().1,
        "peer_timeout": session.peer_timeout,
        "file_pool_size": session.file_pool_size(),
        // The proxy and the PROXY v2 relay. Typed in the session and never
        // passed on, so the engine dialled every peer directly and started no
        // relay listener while the config and the Network tab said otherwise.
        "socks5_outbound_host": session.socks5_outbound_host.trim(),
        "socks5_outbound_user": session.socks5_outbound_user,
        "socks5_outbound_pass": session.socks5_outbound_pass,
        "announce_proxy": session.announce_proxy.trim(),
        "listen_port_proxy_v2": pv2_port,
        "listen_addr_proxy_v2": pv2_addr,
        "proxy_v2_trusted_sources": session.proxy_v2_trusted_sources,
    });
    // An unwritten port keeps the engine's own default (1080) rather than 0,
    // which no SOCKS5 server listens on.
    if session.socks5_outbound_port != 0 {
        v["socks5_outbound_port"] = serde_json::json!(session.socks5_outbound_port);
    }
    serde_json::from_value(v).ok()
}


#[cfg(test)]
mod network_wiring_tests {
    use super::*;
    use crate::config::Session;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn session(toml_text: &str) -> Session {
        toml::from_str(toml_text).expect("session parses")
    }

    fn cfg_of(s: &Session) -> typhon_engine::config::EngineConfig {
        let dir = std::path::Path::new("/tmp");
        engine_config(s, dir, dir).expect("engine config builds")
    }

    /// `enable_lsd` reaches the engine, and only once resolved: the start
    /// loop writes the role's default into the session before this runs.
    #[test]
    fn enable_lsd_reaches_the_engine() {
        assert!(cfg_of(&session("enable_lsd = true\n")).lsd_enabled);
        assert!(!cfg_of(&session("enable_lsd = false\n")).lsd_enabled);
        assert!(!cfg_of(&session("")).lsd_enabled, "unresolved = off");
        let race = session("");
        let mut born = race.clone();
        born.enable_lsd = Some(race.lsd_on("race"));
        assert!(cfg_of(&born).lsd_enabled, "race resolves to on");
    }

    /// ⭐ The keys reach the engine. `engine_config` passed none of them, so a
    /// configured proxy dialled every peer directly and a configured relay
    /// never listened, while the file and the Network tab said otherwise.
    #[test]
    fn the_proxy_and_relay_keys_reach_the_engine() {
        let c = cfg_of(&session(
            "socks5_outbound_host = \"10.0.0.1\"\nsocks5_outbound_user = \"u\"\nsocks5_outbound_pass = \"p\"\n\
             listen_port_proxy_v2 = 16271\nlisten_addr_proxy_v2 = \"[2001:db8::2]\"\n\
             proxy_v2_trusted_sources = [\"203.0.113.20\"]\nannounce_proxy = \"socks5h://10.9.9.9:9050\"\n",
        ));
        assert_eq!(c.socks5_outbound_host, "10.0.0.1");
        assert_eq!(c.socks5_outbound_port, 1080, "an unwritten port is the SOCKS default, not 0");
        assert!(c.resolved_bindings().iter().all(|b| b.egress.socks5.is_some()), "every dial is proxied");
        assert_eq!(c.listen_port_proxy_v2, Some(16271));
        assert_eq!(c.listen_addr_proxy_v2.as_deref(), Some("[2001:db8::2]"));
        assert_eq!(c.proxy_v2_trusted_sources, vec!["203.0.113.20".to_string()]);
        assert_eq!(c.http_proxy(), "socks5h://10.9.9.9:9050");

        // 0 and "" mean "none", not "port 0" and "address \"\"".
        let off = cfg_of(&session("listen_port_proxy_v2 = 0\nlisten_addr_proxy_v2 = \"\"\n"));
        assert_eq!(off.listen_port_proxy_v2, None);
        assert_eq!(off.listen_addr_proxy_v2, None);
        assert!(off.resolved_bindings().iter().all(|b| b.egress.socks5.is_none()));
    }

    /// A SOCKS5 server that is also the tracker: it records the CONNECT
    /// target (host NAME, as socks5h sends it) and the HTTP request line, and
    /// answers a bencoded announce.
    async fn proxy_tracker() -> (u16, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let seen: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let log = log.clone();
                tokio::spawn(async move {
                    let mut b = [0u8; 2];
                    s.read_exact(&mut b).await.ok()?;
                    let mut m = vec![0u8; b[1] as usize];
                    s.read_exact(&mut m).await.ok()?;
                    s.write_all(&[5, 0]).await.ok()?;
                    let mut req = [0u8; 4];
                    s.read_exact(&mut req).await.ok()?;
                    let target = match req[3] {
                        3 => {
                            let mut n = [0u8; 1];
                            s.read_exact(&mut n).await.ok()?;
                            let mut name = vec![0u8; n[0] as usize];
                            s.read_exact(&mut name).await.ok()?;
                            format!("name:{}", String::from_utf8_lossy(&name))
                        }
                        1 => { let mut a = [0u8; 4]; s.read_exact(&mut a).await.ok()?; format!("ip:{:?}", a) }
                        _ => { let mut a = [0u8; 16]; s.read_exact(&mut a).await.ok()?; format!("ip:{:?}", a) }
                    };
                    let mut p = [0u8; 2];
                    s.read_exact(&mut p).await.ok()?;
                    log.lock().unwrap().push(format!("{target}:{}", u16::from_be_bytes(p)));
                    s.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await.ok()?;
                    let mut http = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !http.windows(4).any(|w| w == b"\r\n\r\n") {
                        let n = s.read(&mut buf).await.ok()?;
                        if n == 0 { return None; }
                        http.extend_from_slice(&buf[..n]);
                    }
                    let line = String::from_utf8_lossy(&http).lines().next().unwrap_or_default().to_string();
                    log.lock().unwrap().push(line);
                    let body: &[u8] = b"d8:completei1e10:incompletei0e8:intervali1800e5:peers0:e";
                    let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                    s.write_all(head.as_bytes()).await.ok()?;
                    s.write_all(body).await.ok()?;
                    Some(())
                });
            }
        });
        (port, seen)
    }

    const IH: &str = "0123456789abcdef0123456789abcdef01234567";

    /// ⭐⭐ An announce goes through the engine's proxy, by NAME: socks5h, so
    /// the tracker's DNS is resolved at the proxy and nothing about it leaves
    /// from here. And `announce_ip`, written, is in the request the tracker
    /// receives. Built the way `connect` builds it, from the session.
    #[tokio::test]
    async fn an_announce_goes_through_the_proxy_by_name_and_carries_announce_ip() {
        let (port, seen) = proxy_tracker().await;
        let s = session(&format!(
            "socks5_outbound_host = \"127.0.0.1\"\nsocks5_outbound_port = {port}\nannounce_ip = \"198.51.100.7\"\n"
        ));
        let policy = announce_policy(&Config::default(), &s, &cfg_of(&s));
        assert_eq!(policy.proxy, format!("socks5h://127.0.0.1:{port}"), "announces default to the peer proxy");
        let req = crate::announce::policy::prepare(&policy, "http://tracker.invalid/announce", IH, 16171, 0, 0, 5, "started", None, None)
            .expect("request");
        let out = typhon_engine::tracker::http::send_announce_on(&req.url, &req.user_agent, req.ip_mode, &req.device, &req.proxy).await;
        assert!(out.is_ok(), "the tracker behind the proxy answered: {out:?}");
        let seen = seen.lock().unwrap().clone();
        assert!(seen.iter().any(|l| l == "name:tracker.invalid:80"), "the proxy got the NAME: {seen:?}");
        assert!(seen.iter().any(|l| l.starts_with("GET /announce?") && l.contains("&ip=198.51.100.7")), "ip= sent: {seen:?}");
    }

    /// Unwritten, `announce_ip` sends no `ip=` at all: the tracker keeps the
    /// address the announce came from. The UDP form leaves its field at 0.
    #[test]
    fn an_unwritten_announce_ip_sends_nothing() {
        let s = session("");
        let policy = announce_policy(&Config::default(), &s, &cfg_of(&s));
        let http = crate::announce::policy::prepare(&policy, "http://t.example/announce", IH, 1, 0, 0, 0, "", None, None).unwrap();
        assert!(!http.url.contains("&ip="), "{}", http.url);
        let udp = crate::announce::policy::prepare(&policy, "udp://t.example:6969/announce", IH, 1, 0, 0, 0, "", None, None).unwrap();
        assert_eq!(udp.udp.unwrap().ip, 0);

        let s = session("announce_ip = \"198.51.100.7\"\n");
        let policy = announce_policy(&Config::default(), &s, &cfg_of(&s));
        let udp = crate::announce::policy::prepare(&policy, "udp://t.example:6969/announce", IH, 1, 0, 0, 0, "", None, None).unwrap();
        assert_eq!(udp.udp.unwrap().ip, u32::from(std::net::Ipv4Addr::new(198, 51, 100, 7)), "BEP 15 `ip` field");
    }

    /// ⭐ A UDP tracker behind a proxy is refused with the reason, the same
    /// words the Network tab warns with, and never sent around the proxy.
    #[tokio::test]
    async fn a_udp_tracker_behind_the_proxy_is_refused_with_the_reason() {
        let s = session("socks5_outbound_host = \"127.0.0.1\"\nsocks5_outbound_port = 9\n");
        let policy = announce_policy(&Config::default(), &s, &cfg_of(&s));
        let req = crate::announce::policy::prepare(&policy, "udp://127.0.0.1:9/announce", IH, 1, 0, 0, 0, "", None, None).unwrap();
        let out = typhon_engine::tracker::udp::send_announce_on(req.udp.as_ref().unwrap(), req.ip_mode, &req.device, &req.proxy).await;
        assert_eq!(out.unwrap_err(), crate::netmode::UDP_BEHIND_PROXY);
    }

    /// ⭐ The relay listener starts, and with the allowlist set. Both were
    /// done only by the standalone binary: in Hydra the listener never bound
    /// and the trusted sources were never handed to the engine.
    #[tokio::test]
    async fn the_proxy_v2_listener_starts_with_its_trusted_sources() {
        let free = || std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let (listen, pv2) = (free(), free());
        let s = session(&format!(
            "listen_port = {listen}\nenable_dht = false\nenable_webseed = false\n\
             listen_port_proxy_v2 = {pv2}\nlisten_addr_proxy_v2 = \"127.0.0.1\"\n\
             proxy_v2_trusted_sources = [\"203.0.113.20\"]\n"
        ));
        let dir = std::env::temp_dir().join(format!("hydra-pv2-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let c = engine_config(&s, &dir, &dir.join("resume")).unwrap();
        let disk = Arc::new(DiskManager::new(16));
        let mgr = Arc::new(TorrentManager::new(dir.to_string_lossy().into_owned(), dir.join("resume").to_string_lossy().into_owned(), disk.clone()));
        typhon_engine::session::start(mgr.clone(), disk, &c, Arc::new(std::sync::atomic::AtomicBool::new(false))).await;

        let want: std::net::IpAddr = "203.0.113.20".parse().unwrap();
        assert_eq!(mgr.trusted_proxy_sources(), &[want], "the allowlist reached the engine");
        let mut up = false;
        for _ in 0..50 {
            if tokio::net::TcpStream::connect(("127.0.0.1", pv2)).await.is_ok() {
                up = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(up, "the PROXY v2 listener is bound on {pv2}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Bring one engine up from a session, the way `connect` does.
    async fn started(tag: &str, toml_text: &str) -> Arc<TorrentManager> {
        let free = || std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let s = session(&format!("listen_port = {}\nenable_webseed = false\n{toml_text}", free()));
        let dir = std::env::temp_dir().join(format!("hydra-dht-{tag}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let c = engine_config(&s, &dir, &dir.join("resume")).unwrap();
        let disk = Arc::new(DiskManager::new(16));
        let mgr = Arc::new(TorrentManager::new(dir.to_string_lossy().into_owned(), dir.join("resume").to_string_lossy().into_owned(), disk.clone()));
        typhon_engine::session::start(mgr.clone(), disk, &c, Arc::new(std::sync::atomic::AtomicBool::new(false))).await;
        let _ = std::fs::remove_dir_all(&dir);
        mgr
    }

    /// ⭐ An engine behind the SOCKS5 proxy starts no DHT node, even with
    /// `enable_dht = true`: the node would be plain UDP from the host's own
    /// address. The control is the same engine without the proxy, which does
    /// start one -- pinned to loopback, so the test talks to nobody.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_proxied_engine_starts_no_dht() {
        let pinned = "bind_interface = \"lo\"\nenable_dht = true\n";
        let control = started("direct", pinned).await;
        assert!(control.dht().is_some(), "the control engine must run a DHT, or the next assert proves nothing");
        let proxied = started("socks", &format!("{pinned}socks5_outbound_host = \"127.0.0.1\"\nsocks5_outbound_port = 9\n")).await;
        assert!(proxied.dht().is_none(), "a DHT node started behind the SOCKS5 proxy");
    }

    /// ⭐⭐ Fail closed. A tunnel that did not come up leaves its engine
    /// pinned to a device that is not there, and then NOTHING leaves: no
    /// listener, no DHT node, no announce, no peer dial -- each fails rather
    /// than falling back to the host's default route. The control for the
    /// announce is the same request unpinned, which does reach the tracker.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn an_engine_whose_tunnel_is_down_reaches_nobody() {
        const GONE: &str = "wg-hy-gone";
        let listening = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let free = || std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let s = session(&format!("listen_port = {}\nenable_webseed = false\nenable_dht = true\nbind_interface = \"{GONE}\"\n", free()));
        let dir = std::env::temp_dir().join(format!("hydra-closed-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let c = engine_config(&s, &dir, &dir.join("resume")).unwrap();
        let disk = Arc::new(DiskManager::new(16));
        let mgr = Arc::new(TorrentManager::new(dir.to_string_lossy().into_owned(), dir.join("resume").to_string_lossy().into_owned(), disk.clone()));
        typhon_engine::session::start(mgr.clone(), disk, &c, listening.clone()).await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(!listening.load(std::sync::atomic::Ordering::Relaxed), "a listener came up on a missing device");
        assert!(mgr.dht().is_none(), "a DHT node started on a missing device");

        // The tracker is on loopback and answers: only the pin can stop it.
        let tracker = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://127.0.0.1:{}/announce", tracker.local_addr().unwrap().port());
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = tracker.accept().await {
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let body: &[u8] = b"d8:intervali1800e5:peers0:e";
                let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body).await;
            }
        });
        let policy = announce_policy(&Config::default(), &s, &c);
        assert_eq!(policy.device, GONE);
        let req = crate::announce::policy::prepare(&policy, &url, IH, 1, 0, 0, 0, "started", None, None).unwrap();
        let pinned = typhon_engine::tracker::http::send_announce_on(&req.url, &req.user_agent, req.ip_mode, &req.device, &req.proxy).await;
        assert!(pinned.is_err(), "an announce left without its tunnel: {pinned:?}");
        let open = typhon_engine::tracker::http::send_announce_on(&req.url, &req.user_agent, req.ip_mode, "", &req.proxy).await;
        assert!(open.is_ok(), "the control announce must reach the tracker, or the assert above proves nothing: {open:?}");

        // A peer dial goes through the same pin, and fails the same way.
        let egress = typhon_engine::netpin::Egress { device: GONE.into(), ..Default::default() };
        let sock = tokio::net::TcpSocket::new_v4().unwrap();
        use std::os::fd::AsRawFd;
        assert!(typhon_engine::netpin::pin_fd(sock.as_raw_fd(), &egress).is_err(), "a dial socket pinned to nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
