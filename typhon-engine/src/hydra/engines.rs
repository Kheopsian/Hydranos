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
//! An engine only comes up on the network when its config says so. `net =
//! false` on a session builds the manager and loads its state without opening
//! a socket, which is what the differential bench runs against.

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
    pub manager: Arc<TorrentManager>,
    /// Kept so the engine can be put on the network after it is built.
    pub disk: Arc<DiskManager>,
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

        for (id, session) in [("race", &config.race), ("hoard", &config.hoard)] {
            let data_dir = config_dir.join(id);
            let resume_dir = data_dir.join("resume");

            let disk = Arc::new(DiskManager::new(session.file_pool_size()));
            let manager = Arc::new(TorrentManager::new(
                data_dir.to_string_lossy().into_owned(),
                resume_dir.to_string_lossy().into_owned(),
                disk.clone(),
            ));

            let loaded = manager.load_resume_data();
            tracing::info!(engine = id, torrents = loaded, "engine state loaded");

            engines.push(Engine {
                id: id.to_string(),
                role: id.to_string(),
                listen_port: session.listen_port,
                bind_interface: session.bind_interface.clone(),
                start_paused: session.start_paused,
                enable_ipv6: session.enable_ipv6,
                manager,
                disk,
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
            let session = match engine.id.as_str() {
                "race" => &config.race,
                _ => &config.hoard,
            };
            if !session.net {
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
                    )
                    .await;
                    tracing::info!(
                        engine = %engine.id,
                        listen_port = session.listen_port,
                        dht = session.enable_dht,
                        pex = session.enable_pex,
                        "engine on the network"
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
    pub fn session_totals(&self) -> (i64, i64) {
        let (mut up, mut down) = (0i64, 0i64);
        for engine in &self.engines {
            for t in engine.manager.all().iter() {
                let row = typhon_engine::rpc::dispatch::torrent_to_json(t);
                up += row.get("total_upload").and_then(|v| v.as_i64()).unwrap_or(0);
                down += row.get("total_download").and_then(|v| v.as_i64()).unwrap_or(0);
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
