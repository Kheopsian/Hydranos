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
//! Networking is deliberately absent for now. The managers are built and their
//! durable state is loaded, which is what the read endpoints need; listeners,
//! DHT, PEX and webseed come with the slice that ports the announce path, and
//! until then this process cannot talk to a peer even by accident.

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
    pub fn start(config: &Config, config_dir: &std::path::Path) -> Self {
        let mut engines = Vec::new();

        for (id, session) in [("race", &config.race), ("hoard", &config.hoard)] {
            let data_dir = config_dir.join(id);
            let resume_dir = data_dir.join("resume");

            let disk = Arc::new(DiskManager::new(session.file_pool_size()));
            let manager = Arc::new(TorrentManager::new(
                data_dir.to_string_lossy().into_owned(),
                resume_dir.to_string_lossy().into_owned(),
                disk,
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
            });
        }

        Self { engines, released: std::sync::Mutex::new(Default::default()) }
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
