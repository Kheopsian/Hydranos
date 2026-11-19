use serde::{Deserialize, Serialize};
use tracing::warn;

use super::meta::InfoHash;

/// What the state store puts in a record's `bitfield` when the one it holds
/// no longer matches its checksum. Not hex, so it reads as "no usable
/// bitfield"; the loader then asks the disk rather than the record.
pub const BITFIELD_DISCARDED: &str = "!discarded";

#[derive(Clone, Serialize, Deserialize)]
pub struct ResumeData {
    pub info_hash: String,
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
    /// necessarily what the stored metainfo parses to: the operator can edit it.
    /// This record is what restores a torrent at startup, so without the
    /// list here every edit is undone by the next restart. Serde default
    /// keeps older resume files loadable -- empty means "whatever the
    /// .torrent says".
    #[serde(default)]
    pub trackers: Vec<Vec<String>>,
    /// Seconds this torrent has actually spent seeding, accumulated across
    /// restarts AND across engine moves.
    ///
    /// Not the same thing as the age of the completion: a torrent finished 50
    /// hours ago and stopped for 40 of them has seeded 10. The tracker counts
    /// the second number, and it is the one a minimum-seed obligation is
    /// measured against. Serde default keeps older records loadable -- they
    /// restart the count from zero, which under-reports rather than over.
    #[serde(default)]
    pub seed_secs: i64,
    /// This torrent's own upload cap, bytes/s. 0 = none. Kept in the record
    /// so it survives a restart AND an engine move, exactly like the edited
    /// tracker list: a cap that silently fell off at the next boot would be a
    /// setting that works until nobody is watching.
    #[serde(default)]
    pub up_limit: u64,
    /// This torrent's own download cap, bytes/s. 0 = none.
    #[serde(default)]
    pub down_limit: u64,
    /// File priorities, sequential, first/last piece. `None` = the default,
    /// which is what nearly every record carries. Kept here for the same
    /// reason as the caps: it must survive a restart and an engine move.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_order: Option<super::piece_picker::DownloadOrder>,
    /// Files renamed after the torrent was added: index -> path below the
    /// torrent's folder (`/`-joined). Empty for nearly every torrent. Kept for
    /// the same reason as the order: a rename undone by the next restart, or
    /// lost in an engine move, would point the torrent at files it never had.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub file_renames: std::collections::BTreeMap<usize, String>,
    /// Why the torrent is parked for pieces found missing from the disk, ''
    /// when it is not. The error status itself is not saved, and the record
    /// of such a torrent says "nothing held": without this, the next start
    /// would take it for a download and fetch everything again, which only
    /// a recheck the operator asks for may start.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub data_lost: String,
}

/// Save resume data for a torrent.
///
/// Writes to a sibling `.tmp` and renames into place. A plain write to the
/// final path is not atomic: a crash, an OOM kill or a full disk part-way
/// through leaves a truncated -- often zero-byte -- record, and `load_all`
/// can only warn and skip it, so the torrent silently loses its resume state
/// at the next start. Two records were found in exactly that state in
/// production, months after the fact, because one warn line in a busy log is
/// invisible. rename(2) inside a directory is atomic: a reader sees either
/// the old record or the new one, never a half-written one.
pub fn save(resume_dir: &str, info_hash: &InfoHash, data: &ResumeData) {
    if let Err(e) = try_save(resume_dir, info_hash, data) {
        warn!("[resume] {}", e);
    }
}

/// `save`, saying whether the record is on disk now.
///
/// The temp file is flushed to disk (`sync_all`) before the rename: without
/// it a crash can persist the rename but not the data, leaving the zero-byte
/// record this function exists to prevent. The temp name is unique per call,
/// so two saves of one torrent never interleave their writes in one file; the
/// last rename wins, and each rename moves a complete record.
pub fn try_save(resume_dir: &str, info_hash: &InfoHash, data: &ResumeData) -> Result<(), String> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);

    let hex = super::hex_encode(info_hash);
    let path = format!("{}/{}.json", resume_dir, hex);
    // Ends in `.tmp` so `load_all` sweeps one a crash left behind.
    let tmp = format!(
        "{}/{}.json.{}.{}.tmp",
        resume_dir,
        hex,
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    );
    let json = serde_json::to_string(data).map_err(|e| format!("failed to serialize: {}", e))?;
    let written = std::fs::File::create(&tmp).and_then(|mut f| {
        f.write_all(json.as_bytes())?;
        f.sync_all()
    });
    if let Err(e) = written {
        std::fs::remove_file(&tmp).ok();
        return Err(format!("failed to write {}: {}", tmp, e));
    }
    std::fs::rename(&tmp, &path).map_err(|e| {
        std::fs::remove_file(&tmp).ok();
        format!("failed to rename {} -> {}: {}", tmp, path, e)
    })?;
    #[cfg(unix)]
    if let Ok(d) = std::fs::File::open(resume_dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// Load all resume data from a directory.
pub fn load_all(resume_dir: &str) -> Vec<ResumeData> {
    load_all_reporting(resume_dir).0
}

/// `load_all`, plus the name (lower case, without `.json`) of every record
/// that could not be read. Such a torrent is not loaded, and is not gone
/// either: the caller says so, as `StateDb::load_all_reporting` lets it for a
/// damaged row, rather than leave one warn line in a busy log.
pub fn load_all_reporting(resume_dir: &str) -> (Vec<ResumeData>, Vec<String>) {
    let dir = match std::fs::read_dir(resume_dir) {
        Ok(d) => d,
        Err(_) => return (Vec::new(), Vec::new()),
    };

    let mut results = Vec::new();
    let mut unreadable = Vec::new();
    for entry in dir {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            // A `.tmp` here is a save that died between write and rename.
            // Nothing will ever read it, so sweep it rather than let one
            // leak per crash forever.
            if path.extension().and_then(|e| e.to_str()) == Some("tmp") {
                std::fs::remove_file(&path).ok();
            }
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_lowercase();
        match std::fs::read_to_string(&path) {
            Ok(json) => match serde_json::from_str::<ResumeData>(&json) {
                Ok(data) => results.push(data),
                Err(e) => {
                    warn!("[resume] bad JSON in {:?}: {}", path, e);
                    unreadable.push(name);
                }
            },
            Err(e) => {
                warn!("[resume] read {:?}: {}", path, e);
                unreadable.push(name);
            }
        }
    }
    (results, unreadable)
}

/// Remove resume data for a torrent.
pub fn remove(resume_dir: &str, info_hash: &InfoHash) {
    let path = format!("{}/{}.json", resume_dir, super::hex_encode(info_hash));
    std::fs::remove_file(&path).ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(ih: &InfoHash, up: u64) -> ResumeData {
        serde_json::from_value(serde_json::json!({
            "info_hash": super::super::hex_encode(ih),
            "save_path": "/data/x",
            "seed_mode": false,
            "paused": false,
            "total_uploaded": up,
            "total_downloaded": 0,
            "added_time": 1,
            "completed_time": 0,
            "seed_secs": 0,
            "up_limit": 0,
            "down_limit": 0,
        }))
        .expect("record")
    }

    fn dir(tag: &str) -> String {
        let d = std::env::temp_dir().join(format!("hydra-fastresume-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.to_string_lossy().into_owned()
    }

    #[test]
    fn a_saved_record_round_trips_and_leaves_no_temp() {
        let d = dir("roundtrip");
        let ih = [7u8; 20];
        try_save(&d, &ih, &rec(&ih, 42)).unwrap();
        let (records, bad) = load_all_reporting(&d);
        assert!(bad.is_empty());
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].total_uploaded, 42);
        let tmps = std::fs::read_dir(&d)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .and_then(|x| x.to_str())
                    == Some("tmp")
            })
            .count();
        assert_eq!(tmps, 0);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_truncated_record_is_reported_not_loaded_and_the_next_save_repairs_it() {
        let d = dir("truncated");
        let ih = [9u8; 20];
        let hex = super::super::hex_encode(&ih);
        let path = format!("{d}/{hex}.json");
        std::fs::write(&path, b"").unwrap();
        let (records, bad) = load_all_reporting(&d);
        assert!(records.is_empty());
        assert_eq!(bad, vec![hex.clone()]);
        std::fs::write(&path, b"{\"info_hash\":\"ab").unwrap();
        assert_eq!(load_all_reporting(&d).1, vec![hex.clone()]);
        try_save(&d, &ih, &rec(&ih, 5)).unwrap();
        let (records, bad) = load_all_reporting(&d);
        assert!(bad.is_empty());
        assert_eq!(records.len(), 1);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_leftover_temp_is_swept_and_never_read() {
        let d = dir("sweep");
        let ih = [3u8; 20];
        let hex = super::super::hex_encode(&ih);
        let tmp = format!("{d}/{hex}.json.1.2.tmp");
        std::fs::write(&tmp, b"{").unwrap();
        let (records, bad) = load_all_reporting(&d);
        assert!(records.is_empty() && bad.is_empty());
        assert!(!std::path::Path::new(&tmp).exists());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn concurrent_saves_of_one_torrent_leave_one_whole_record() {
        let d = dir("concurrent");
        let ih = [5u8; 20];
        let threads: Vec<_> = (0..8u64)
            .map(|t| {
                let d = d.clone();
                std::thread::spawn(move || {
                    for i in 0..25u64 {
                        try_save(&d, &ih, &rec(&ih, t * 1000 + i)).expect("save");
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let (records, bad) = load_all_reporting(&d);
        assert!(bad.is_empty(), "a save was torn: {bad:?}");
        assert_eq!(records.len(), 1);
        let leftovers = std::fs::read_dir(&d).unwrap().count();
        assert_eq!(leftovers, 1, "temp files leaked");
        let _ = std::fs::remove_dir_all(d);
    }
}
