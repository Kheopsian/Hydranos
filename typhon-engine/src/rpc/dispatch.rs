//! How a torrent reads out as JSON, and the host a tracker URL names: the
//! helpers the daemon shares across its routes, its rules and its stats.
//!
//! The name is a leftover. These used to back the verbs of a JSON-RPC socket
//! served by a standalone engine binary; that binary and its socket are gone,
//! and what the daemon still calls stayed here.

use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::torrent::hex_encode;
use crate::torrent::meta::TorrentStatus;

/// The subset the scheduling loops read. Field names and values match
/// `torrent_to_json` exactly -- both derive them from `torrent_core`, so a
/// change to how state or progress is computed cannot make the two disagree.
pub fn torrent_to_json_slim(t: &Arc<crate::torrent::meta::TorrentState>) -> Value {
    let c = torrent_core(t);
    json!({
        "info_hash": hex_encode(&t.info_hash),
        "state": c.state,
        "progress": c.progress,
        "total_size": t.meta.total_size,
        "total_done": c.total_done,
        "download_rate": t.download_rate.get(),
        "is_paused": c.is_paused,
        "is_finished": c.progress >= 1.0,
    })
}

/// State, progress and bytes-done, shared by the full and slim projections so
/// they cannot drift apart.
pub struct TorrentCore {
    pub state: &'static str,
    pub progress: f64,
    pub total_done: u64,
    pub is_paused: bool,
    pub status_u8: u8,
}

/// The state string, from the two flags that decide it.
///
/// Split out of `torrent_core` so a caller that wants only the state does not
/// pay for the progress it also computes -- `torrent_core` takes the picker
/// lock, and a workflow pass asking 300 000 torrents what state they are in
/// would take it 300 000 times. One definition either way: two places deciding
/// separately what "paused" means is how a view starts contradicting another.
pub fn state_str(status_u8: u8, is_paused: bool) -> &'static str {
    if status_u8 == TorrentStatus::Error as u8 {
        "error"
    } else if is_paused {
        "paused"
    } else {
        match status_u8 {
            0 => "stopped",
            1 => "checking_files",
            2 => "downloading",
            3 => "seeding",
            4 => "error",
            _ => "unknown",
        }
    }
}

pub fn torrent_core(t: &Arc<crate::torrent::meta::TorrentState>) -> TorrentCore {
    let status_u8 = t.status.load(Ordering::Relaxed);
    let is_paused = t.is_paused.load(Ordering::Relaxed);
    let state = state_str(status_u8, is_paused);
    // Real progress from picker's have count (was a 0.0/1.0 placeholder).
    let (num_have, progress) = if status_u8 == TorrentStatus::Seeding as u8 {
        (t.meta.num_pieces(), 1.0_f64)
    } else if let Some(picker) = t.picker.get() {
        let n = picker.lock().unwrap().num_have();
        let total = t.meta.num_pieces();
        let p = if total > 0 {
            n as f64 / total as f64
        } else {
            0.0
        };
        (n, p)
    } else {
        // No picker = added in seed_mode = its data is trusted whole (that is
        // what makes 100k seeders cheap). Stopped before it ever ran it still
        // holds the files, so report it complete instead of a bogus 0 % --
        // start_torrent draws the same conclusion from the same fact.
        (t.meta.num_pieces(), 1.0)
    };
    // Approximate -- over-reports by <=1 piece_length when the last (short)
    // piece is among the have set but we can't tell cheaply. Good enough
    // for a progress display.
    let total_done: u64 = if progress >= 1.0 {
        t.meta.total_size
    } else {
        (num_have as u64 * t.meta.piece_length as u64).min(t.meta.total_size)
    };
    TorrentCore {
        state,
        progress,
        total_done,
        is_paused,
        status_u8,
    }
}

/// The live peer table for one torrent: one row per connected peer, carrying
/// the flags that say what each connection is actually doing.
///
/// Split out of `get_peers` so the HTTP layer can serve it too. 4.0.0 left the
/// detail panel's `"peers"` hard-coded to `[]` and wired no route to this code,
/// so the one instrument that separates "the peer is interested and we are
/// choking it" from "the peer is a seed with nothing to ask for" did not exist
/// on a running node. The upload collapse of 2026-09-08 was diagnosed blind for
/// want of it: 87% of inbound peers received under 10 KB and never requested a
/// block, and nothing on the node could say why.
pub fn peers_json(t: &Arc<crate::torrent::meta::TorrentState>) -> Value {
    let now = std::time::SystemTime::now();
    let total_pieces = t.meta.num_pieces();
    // Iterate DashMap snapshot — called on-demand when user opens peer panel
    let peers: Vec<Value> = t
        .peer_stats
        .iter()
        .map(|entry| {
            let p = entry.value();
            let dur = now
                .duration_since(p.connected_at)
                .unwrap_or_default()
                .as_secs();
            let interested = p.interested.load(Ordering::Relaxed);
            let choked = p.choked.load(Ordering::Relaxed);
            let is_seed = p.is_seed.load(Ordering::Relaxed);
            let num_pieces = p.num_pieces_have.load(Ordering::Relaxed);
            let mut flags = String::new();
            if interested {
                flags.push('i');
            }
            if !choked {
                flags.push('U');
            }
            if p.is_encrypted {
                flags.push('E');
            }
            if p.fast_ext {
                flags.push('F');
            }
            if is_seed {
                flags.push('S');
            }
            let progress = if total_pieces > 0 {
                num_pieces as f64 / total_pieces as f64
            } else {
                0.0
            };
            // Sample the rates here rather than from a background tick: this call
            // only happens while someone is looking at the peer panel, so the delta
            // lands over the caller's own polling interval and costs nothing the
            // rest of the time. The first sample for a freshly connected peer
            // reports 0 by design — RateTracker seeds its reference point before it
            // will emit anything, which is what stops a peer that arrives with a
            // resumed byte count from reporting an absurd one-off spike.
            let dl_total = p.total_downloaded.load(Ordering::Relaxed);
            let ul_total = p.total_uploaded.load(Ordering::Relaxed);
            p.dl_rate.update(dl_total);
            p.ul_rate.update(ul_total);
            json!({
                "ip": p.addr.ip().to_string(),
                "port": p.addr.port(),
                "client": p.client,
                "dl_rate": p.dl_rate.get(),
                "ul_rate": p.ul_rate.get(),
                "total_download": dl_total,
                "total_upload": ul_total,
                "progress": progress,
                "flags": flags,
                "num_pieces": num_pieces,
                "connection_duration": dur,
            })
        })
        .collect();
    Value::Array(peers)
}

/// tracker_host_of extracts the bare host from a tracker announce URL, e.g.
/// "https://tracker.example/announce/KEY" -> "tracker.example". Lets the list
/// view label each torrent with its (static) tracker without a per-torrent call.
pub fn tracker_host_of(url: &str) -> String {
    tracker_host_str(url).to_string()
}

/// `tracker_host_of` without the copy, for loops over the whole catalogue that
/// only look the host up.
pub fn tracker_host_str(url: &str) -> &str {
    let s = url.split("://").nth(1).unwrap_or(url);
    s.split(|c| c == '/' || c == ':').next().unwrap_or("")
}

pub fn torrent_to_json(t: &Arc<crate::torrent::meta::TorrentState>) -> Value {
    // Shared with the slim projection so the two cannot disagree on what a
    // torrent's state or progress is.
    let core = torrent_core(t);
    let status_u8 = core.status_u8;
    let is_paused = core.is_paused;
    let state = core.state;
    let progress = core.progress;
    let total_done = core.total_done;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let active_time = (now - t.added_time).max(0);
    let completed = t.completed_time.load(Ordering::Relaxed);
    // The ACCUMULATED seed time, not the age of the completion. Those two used
    // to be the same expression here, which meant a torrent completed 50 hours
    // ago and stopped for 40 of them reported 50 hours of seeding. A minimum
    // seed obligation measured against that is satisfied early, and early is
    // the direction that costs a hit-and-run.
    let seeding_time = t.seed_time_now(now);
    let current_tracker = t
        .current_tracker
        .lock()
        .map(|s| s.clone())
        .unwrap_or_default();
    let scrape_seeders = t.scrape_seeders.load(Ordering::Relaxed);
    let scrape_leechers = t.scrape_leechers.load(Ordering::Relaxed);
    let announce_ok = t.last_announce_ok.load(Ordering::Relaxed);
    let tracker_error = t
        .last_announce_error
        .lock()
        .map(|s| s.clone())
        .unwrap_or_default();
    let tracker_host = t
        .live_trackers
        .read()
        .iter()
        .flatten()
        .next()
        .map(|u| tracker_host_of(u))
        .unwrap_or_default();
    let error_msg = t.error_msg.lock().map(|g| g.clone()).unwrap_or_default();

    json!({
        "info_hash": hex_encode(&t.info_hash),
        "name": t.meta.content_name(),
        "state": state,
        "progress": progress,
        "total_size": t.meta.total_size,
        "multi_file": t.meta.multi_file,
        "total_done": total_done,
        "total_upload": t.total_uploaded.load(Ordering::Relaxed),
        "total_download": t.total_downloaded.load(Ordering::Relaxed),
        "upload_rate": t.upload_rate.get(),
        "download_rate": t.download_rate.get(),
        "num_peers": t.peers_connected.load(Ordering::Relaxed),
        "num_seeds": scrape_seeders,
        "list_seeds": scrape_seeders,
        "list_peers": scrape_leechers,
        "save_path": t.save_path.read().to_string_lossy(),
        "added_time": t.added_time,
        "completed_time": completed,
        "num_pieces": t.meta.num_pieces(),
        "piece_length": t.meta.piece_length,
        "seeding_time": seeding_time,
        "active_time": active_time,
        "current_tracker": current_tracker,
        "tracker_host": tracker_host,
        "is_paused": is_paused,
        "is_finished": progress >= 1.0,
        "is_seeding": status_u8 == TorrentStatus::Seeding as u8,
        "is_announced": announce_ok,
        "tracker_error": !tracker_error.is_empty(),
        "tracker_error_msg": tracker_error,
        "error_msg": error_msg,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `tracker_host_of` is what keys every per-tracker counter and the
    /// breaker; getting it wrong splits one tracker into several.
    #[test]
    fn the_tracker_host_is_the_host_and_nothing_else() {
        assert_eq!(
            tracker_host_of("https://tracker.example/announce"),
            "tracker.example"
        );
        assert_eq!(
            tracker_host_of("http://tracker.example:8080/x"),
            "tracker.example"
        );
        assert_eq!(
            tracker_host_of("udp://tracker.example:1337"),
            "tracker.example"
        );
        assert_eq!(
            tracker_host_of("tracker.example/announce"),
            "tracker.example"
        );
        assert_eq!(tracker_host_of(""), "");
    }
}
