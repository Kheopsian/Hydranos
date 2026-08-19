use std::path::Path;
use serde::{Serialize, Deserialize};
use tracing::{info, warn};

use super::meta::InfoHash;

#[derive(Serialize, Deserialize)]
pub struct ResumeData {
    pub info_hash: String,
    pub torrent_path: String,
    pub save_path: String,
    pub seed_mode: bool,
    pub paused: bool,
    pub total_uploaded: u64,
    pub total_downloaded: u64,
    pub added_time: i64,
    pub completed_time: i64,
    /// Hex-encoded BT bitfield of verified pieces. Empty string when the
    /// torrent is in seed_mode (no picker) or freshly added. Serde default
    /// keeps older resume files compatible.
    #[serde(default)]
    pub bitfield: String,
    /// The tracker list actually announced to, in tiers, which is NOT
    /// necessarily what `torrent_path` parses to: the operator can edit it.
    /// This record is what restores a torrent at startup, so without the
    /// list here every edit is undone by the next restart. Serde default
    /// keeps older resume files loadable -- empty means "whatever the
    /// .torrent says".
    #[serde(default)]
    pub trackers: Vec<Vec<String>>,
}

/// Save resume data for a torrent.
pub fn save(resume_dir: &str, info_hash: &InfoHash, data: &ResumeData) {
    let path = format!("{}/{}.json", resume_dir, super::hex_encode(info_hash));
    match serde_json::to_string(data) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, json) {
                warn!("[resume] failed to write {}: {}", path, e);
            }
        }
        Err(e) => warn!("[resume] failed to serialize: {}", e),
    }
}

/// Load all resume data from a directory.
pub fn load_all(resume_dir: &str) -> Vec<ResumeData> {
    let dir = match std::fs::read_dir(resume_dir) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };

    let mut results = Vec::new();
    for entry in dir {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match std::fs::read_to_string(&path) {
            Ok(json) => {
                match serde_json::from_str::<ResumeData>(&json) {
                    Ok(data) => results.push(data),
                    Err(e) => warn!("[resume] bad JSON in {:?}: {}", path, e),
                }
            }
            Err(e) => warn!("[resume] read {:?}: {}", path, e),
        }
    }
    results
}

/// Remove resume data for a torrent.
pub fn remove(resume_dir: &str, info_hash: &InfoHash) {
    let path = format!("{}/{}.json", resume_dir, super::hex_encode(info_hash));
    std::fs::remove_file(&path).ok();
}
