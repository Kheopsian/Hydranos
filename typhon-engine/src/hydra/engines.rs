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
//! `HYDRA_ENGINE_NET=0` holds every engine off the network: the managers are
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
    pub bump: std::sync::OnceLock<tokio::sync::mpsc::Sender<String>>,
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
}

pub struct EngineHost {
    engines: Vec<Engine>,
    /// Scopes the startup gate has released. Empty until somebody asks.
    released: std::sync::Mutex<std::collections::BTreeSet<String>>,
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

            let loaded = manager.load_resume_data();
            tracing::info!(engine = id, torrents = loaded, "engine state loaded");

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
            });
        }

        Self { engines, released: std::sync::Mutex::new(Default::default()) }
    }

    /// Build the engines and put them on the network.
    ///
    /// Split from `offline` so the two halves are separable: the unit tests and
    /// the differential bench want engines that hold the production catalogue
    /// and open no socket, and that must not depend on remembering to set a
    /// flag -- it is a different call.
    pub async fn start(config: &Config, config_dir: &std::path::Path) -> Self {
        let host = Self::offline(config, config_dir);
        host.connect(config, config_dir).await;
        host
    }

    /// Put every engine that asks for it on the network.
    async fn connect(&self, config: &Config, config_dir: &std::path::Path) {
        for engine in &self.engines {
            // The engine's own merged session. NOT config.race / config.hoard:
            // an engine that is neither is a legitimate configuration.
            let session = &engine.session;
            if !networking_enabled() {
                tracing::warn!(
                    engine = %engine.id,
                    "net = false: state loaded, no listener, no announce, no DHT"
                );
                continue;
            }
            let data_dir = config_dir.join(&engine.id);
            let resume_dir = data_dir.join("resume");
            match engine_config(session, &data_dir, &resume_dir) {
                Some(engine_cfg) => {
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
                    let peer_id = engine_cfg.peer_id();
                    let policy: crate::announce::PolicyHandle =
                        std::sync::Arc::new(std::sync::RwLock::new(std::sync::Arc::new(
                            crate::announce::policy_from_config(
                                config,
                                String::from_utf8_lossy(&peer_id).into_owned(),
                                String::new(),
                            ),
                        )));
                    let _ = engine.announce_policy.set(policy.clone());
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
                        crate::workers::spawn_race_drain(
                            engine.manager.clone(),
                            config.race_drain.clone(),
                            config_dir.join("race"),
                        );
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
        use std::sync::atomic::Ordering;
        let (mut up, mut down) = (0i64, 0i64);
        for engine in &self.engines {
            for t in engine.manager.all().iter() {
                up += t.total_uploaded.load(Ordering::Relaxed) as i64;
                down += t.total_downloaded.load(Ordering::Relaxed) as i64;
            }
        }
        (up, down)
    }

    /// Total torrents across every engine, read from the engines themselves.
    ///
    /// The figure the Go front published came from a cache it refreshed on a
    /// timer, which is why it could disagree with the database. Here it is
    /// counted from the live maps at the moment of asking.
    pub fn total_torrents(&self) -> usize {
        self.engines.iter().map(|e| e.manager.all().len()).sum()
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

    /// The switch is off only for the three spellings a bench would use, and
    /// on for everything else -- including an empty or misspelt value. An
    /// engine that silently stayed offline because someone wrote `HYDRA_ENGINE_NET=no`
    /// would look alive and seed nothing.
    #[test]
    fn only_an_explicit_off_holds_the_engines_back() {
        for off in ["0", "false", "off"] {
            unsafe { std::env::set_var("HYDRA_ENGINE_NET", off) };
            assert!(!super::networking_enabled(), "{off} should hold the engines off");
        }
        for on in ["1", "true", "", "no", "yes"] {
            unsafe { std::env::set_var("HYDRA_ENGINE_NET", on) };
            assert!(super::networking_enabled(), "{on:?} must not be read as off");
        }
        unsafe { std::env::remove_var("HYDRA_ENGINE_NET") };
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
        std::env::var("HYDRA_ENGINE_NET").as_deref(),
        Ok("0") | Ok("false") | Ok("off")
    )
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
    serde_json::from_value(serde_json::json!({
        "data_dir": data_dir.to_string_lossy(),
        "resume_dir": resume_dir.to_string_lossy(),
        "listen_port": session.listen_port,
        "bind_device": session.bind_interface,
        "dht_enabled": session.enable_dht,
        "pex_enabled": session.enable_pex,
        "enable_webseed": session.enable_webseed,
        "enable_ipv6": session.enable_ipv6,
        "max_connections": session.max_connections.max(0),
        "max_uploads_per_torrent": session.max_uploads_per_torrent,
        "file_pool_size": session.file_pool_size(),
    }))
    .ok()
}

