use std::sync::Arc;
use std::sync::atomic::Ordering;
use serde_json::{json, Value};
use tracing::info;

use crate::config::EngineConfig;
use crate::disk::DiskManager;
use crate::torrent::{self, TorrentManager, hex_encode, hex_decode};
use crate::torrent::meta::TorrentStatus;

pub fn dispatch(
    method: &str,
    params: &Value,
    torrent_mgr: &Arc<TorrentManager>,
    _disk_mgr: &Arc<DiskManager>,
    config: &EngineConfig,
) -> Value {
    match method {
        "ping" => json!({"pong": true}),
        "add_torrent" => add_torrent(params, torrent_mgr),
        "fetch_metadata" => fetch_metadata(params, config, torrent_mgr),
        "get_metadata" => get_metadata(params, torrent_mgr),
        "remove_torrent" => remove_torrent(params, torrent_mgr),
        "start_torrent" => start_torrent(params, torrent_mgr),
        "stop_torrent" => stop_torrent(params, torrent_mgr),
        "set_serving_suspended" => set_serving_suspended(params, torrent_mgr),
        "set_save_path" => set_save_path(params, torrent_mgr),
        "export_state" => export_state(params, torrent_mgr),
        "import_state" => import_state(params, torrent_mgr),
        "verify_torrent" => verify_torrent(params, torrent_mgr),
        "recheck_torrent" => verify_torrent(params, torrent_mgr),
        "get_status" => get_status(params, torrent_mgr),
        "list_torrents" => list_torrents(params, torrent_mgr),
        "get_peers" => get_peers(params, torrent_mgr),
        "add_peers" => add_peers(params, torrent_mgr),
        "set_upload_limit" => json!({"ok": true}), // TODO
        "set_download_limit" => json!({"ok": true}), // TODO
        "get_session_stats" => get_session_stats(torrent_mgr),
        "get_trackers" => get_trackers(params, torrent_mgr),
        "set_trackers" => set_trackers(params, torrent_mgr),
        "get_files" => get_files(params, torrent_mgr),
        "get_availability" => get_availability(params, torrent_mgr),
        "set_opt_flag" => set_opt_flag(params, torrent_mgr),
        "get_opt_flags" => get_opt_flags(torrent_mgr),
        "get_diagnostics" => get_diagnostics(torrent_mgr, config),
        "set_listen_port" => set_listen_port(params, torrent_mgr),
        "set_self_ips" => set_self_ips(params),
        "set_dials_paused" => set_dials_paused(params, torrent_mgr),
        "set_dial_limits" => set_dial_limits(params, torrent_mgr),
        _ => json!({"error": format!("unknown method: {}", method)}),
    }
}

fn add_torrent(params: &Value, mgr: &Arc<TorrentManager>) -> Value {
    let torrent_path = match params.get("torrent_path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return json!({"error": "missing torrent_path"}),
    };
    let save_path = match params.get("save_path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return json!({"error": "missing save_path"}),
    };
    let stopped = params.get("stopped").and_then(|v| v.as_bool()).unwrap_or(false);
    let seed_mode = params.get("seed_mode").and_then(|v| v.as_bool()).unwrap_or(false);

    match mgr.add_torrent(torrent_path, save_path, stopped, seed_mode) {
        Ok((ih, name)) => {
            // Data already on disk at save_path (re-add / cross-seed / a
            // download resumed elsewhere)? Hash-check it instead of blindly
            // re-downloading over it. seed_mode (skip_checking) stays trust-fast.
            if !seed_mode && !stopped && mgr.any_file_exists(&ih) {
                let _ = mgr.recheck(&ih);
            }
            json!({"info_hash": hex_encode(&ih), "name": name})
        }
        Err(e) => json!({"error": e}),
    }
}

/// Kick off magnet resolution. Returns immediately: resolution is a background
/// job, polled through `get_metadata`.
fn fetch_metadata(params: &Value, config: &EngineConfig, mgr: &Arc<TorrentManager>) -> Value {
    let ih = match get_info_hash(params) {
        Ok(ih) => ih,
        Err(e) => return e,
    };
    let trackers: Vec<String> = params
        .get("trackers")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
    let peers: Vec<std::net::SocketAddr> = params
        .get("peers")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).filter_map(|s| s.parse().ok()).collect())
        .unwrap_or_default();
    let binding_id = params.get("binding_id").and_then(|v| v.as_u64()).map(|n| n as u32);

    let started = mgr.magnet().start(ih, trackers, peers, config, binding_id, mgr.dht().map(|d| d.handle()));
    json!({"info_hash": hex_encode(&ih), "started": started})
}

/// Poll a resolution. `state` is one of resolving / done / failed; on done the
/// raw info dict comes back hex-encoded (there is no base64 in the tree).
fn get_metadata(params: &Value, torrent_mgr: &Arc<TorrentManager>) -> Value {
    let ih = match get_info_hash(params) {
        Ok(ih) => ih,
        Err(e) => return e,
    };
    match torrent_mgr.magnet().state_of(&ih) {
        None => json!({"state": "unknown"}),
        Some(crate::magnet::JobState::Resolving) => json!({"state": "resolving"}),
        Some(crate::magnet::JobState::Failed(e)) => json!({"state": "failed", "error": e}),
        Some(crate::magnet::JobState::Done(dict)) => {
            let encoded = crate::magnet::hex(&dict);
            // The caller has it now; holding megabytes per resolved magnet
            // would be a slow leak at our torrent counts.
            torrent_mgr.magnet().forget(&ih);
            json!({"state": "done", "info": encoded})
        }
    }
}

fn remove_torrent(params: &Value, mgr: &Arc<TorrentManager>) -> Value {
    let ih = match get_info_hash(params) {
        Ok(ih) => ih,
        Err(e) => return e,
    };
    let keep_data = params.get("keep_data").and_then(|v| v.as_bool()).unwrap_or(true);
    match mgr.remove_torrent(&ih, keep_data) {
        Ok(()) => json!({"ok": true}),
        Err(e) => json!({"error": e}),
    }
}

fn start_torrent(params: &Value, mgr: &Arc<TorrentManager>) -> Value {
    let ih = match get_info_hash(params) { Ok(ih) => ih, Err(e) => return e };
    match mgr.start_torrent(&ih) {
        Ok(()) => json!({"ok": true}),
        Err(e) => json!({"error": e}),
    }
}

fn stop_torrent(params: &Value, mgr: &Arc<TorrentManager>) -> Value {
    let ih = match get_info_hash(params) { Ok(ih) => ih, Err(e) => return e };
    match mgr.stop_torrent(&ih) {
        Ok(()) => json!({"ok": true}),
        Err(e) => json!({"error": e}),
    }
}

fn set_serving_suspended(params: &Value, mgr: &Arc<TorrentManager>) -> Value {
    let ih = match get_info_hash(params) { Ok(ih) => ih, Err(e) => return e };
    let suspended = params.get("suspended").and_then(|v| v.as_bool()).unwrap_or(false);
    match mgr.set_serving_suspended(&ih, suspended) {
        Ok(()) => json!({"ok": true}),
        Err(e) => json!({"error": e}),
    }
}

fn set_save_path(params: &Value, mgr: &Arc<TorrentManager>) -> Value {
    let ih = match get_info_hash(params) { Ok(ih) => ih, Err(e) => return e };
    let new_path = match params.get("save_path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return json!({"error": "missing save_path"}),
    };
    match mgr.set_save_path(&ih, new_path) {
        Ok(()) => json!({"ok": true}),
        Err(e) => json!({"error": e}),
    }
}

/// Hand out a torrent's durable state so another engine can adopt it.
///
/// The two halves of a move are kept as separate calls on purpose: the two
/// engines are separate processes with separate databases, so the orchestrator
/// (Hydra) is the only thing that can see both. It exports here, imports over
/// there, and only then removes the original.
fn export_state(params: &Value, mgr: &Arc<TorrentManager>) -> Value {
    let ih = match get_info_hash(params) { Ok(ih) => ih, Err(e) => return e };
    match mgr.export_state(&ih) {
        Some(rd) => serde_json::to_value(&rd).unwrap_or_else(|e| json!({"error": e.to_string()})),
        None => json!({"error": "torrent not found"}),
    }
}

/// Adopt a torrent exported from another engine, progression included.
fn import_state(params: &Value, mgr: &Arc<TorrentManager>) -> Value {
    let rd: crate::torrent::fastresume::ResumeData = match serde_json::from_value(params.clone()) {
        Ok(rd) => rd,
        Err(e) => return json!({"error": format!("bad state record: {}", e)}),
    };
    match mgr.import_state(&rd) {
        Ok((ih, name)) => json!({"info_hash": hex_encode(&ih), "name": name}),
        Err(e) => json!({"error": e}),
    }
}

fn verify_torrent(params: &Value, mgr: &Arc<TorrentManager>) -> Value {
    // Hash-check data on disk and repopulate the picker (async, background).
    let ih = match get_info_hash(params) { Ok(ih) => ih, Err(e) => return e };
    match mgr.recheck(&ih) {
        Ok(()) => json!({"ok": true, "checking": true}),
        Err(e) => json!({"error": e}),
    }
}

fn get_status(params: &Value, mgr: &Arc<TorrentManager>) -> Value {
    let ih = match get_info_hash(params) { Ok(ih) => ih, Err(e) => return e };
    let t = match mgr.get(&ih) {
        Some(t) => t,
        None => return json!({"error": "torrent not found"}),
    };
    torrent_to_json(&t)
}

/// `slim: true` returns the eight fields the scheduling loops read, instead of
/// the thirty-two the UI needs.
///
/// Three loops on the Go side (announce reconcile, verify batching, download
/// slots) poll this every 10 to 30 seconds and look at a handful of fields. At
/// 196k torrents the full listing is the single biggest thing either process
/// allocates, and most of it is thrown away by those three callers: strings for
/// the name, the save path, the current tracker and the announce error, plus
/// three mutex acquisitions per torrent to read them. The slim projection
/// touches none of that.
///
/// Absent or false, the response is byte-identical to what it always was.
fn list_torrents(params: &Value, mgr: &Arc<TorrentManager>) -> Value {
    let slim = params.get("slim").and_then(|v| v.as_bool()).unwrap_or(false);
    let all = mgr.all();
    let torrents: Vec<Value> = if slim {
        all.iter().map(|t| torrent_to_json_slim(t)).collect()
    } else {
        all.iter().map(|t| torrent_to_json(t)).collect()
    };
    json!({"torrents": torrents, "count": torrents.len()})
}

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

pub fn torrent_core(t: &Arc<crate::torrent::meta::TorrentState>) -> TorrentCore {
    let status_u8 = t.status.load(Ordering::Relaxed);
    let is_paused = t.is_paused.load(Ordering::Relaxed);
    let state = if status_u8 == TorrentStatus::Error as u8 {
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
    };
    // Real progress from picker's have count (was a 0.0/1.0 placeholder).
    let (num_have, progress) = if status_u8 == TorrentStatus::Seeding as u8 {
        (t.meta.num_pieces(), 1.0_f64)
    } else if let Some(picker) = t.picker.get() {
        let n = picker.lock().unwrap().num_have();
        let total = t.meta.num_pieces();
        let p = if total > 0 { n as f64 / total as f64 } else { 0.0 };
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
    TorrentCore { state, progress, total_done, is_paused, status_u8 }
}

fn get_peers(params: &Value, mgr: &Arc<TorrentManager>) -> Value {
    let ih = match get_info_hash(params) { Ok(ih) => ih, Err(e) => return e };
    let t = match mgr.get(&ih) {
        Some(t) => t,
        None => return json!({"error": "torrent not found"}),
    };
    json!({"peers": peers_json(&t)})
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
    let peers: Vec<Value> = t.peer_stats.iter().map(|entry| {
        let p = entry.value();
        let dur = now.duration_since(p.connected_at).unwrap_or_default().as_secs();
        let interested = p.interested.load(Ordering::Relaxed);
        let choked = p.choked.load(Ordering::Relaxed);
        let is_seed = p.is_seed.load(Ordering::Relaxed);
        let num_pieces = p.num_pieces_have.load(Ordering::Relaxed);
        let mut flags = String::new();
        if interested { flags.push('i'); }
        if !choked { flags.push('U'); }
        if p.is_encrypted { flags.push('E'); }
        if p.fast_ext { flags.push('F'); }
        if is_seed { flags.push('S'); }
        let progress = if total_pieces > 0 {
            num_pieces as f64 / total_pieces as f64
        } else { 0.0 };
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
    }).collect();
    Value::Array(peers)
}

fn get_session_stats(mgr: &Arc<TorrentManager>) -> Value {
    let all = mgr.all();
    let mut total_ul: i64 = 0;
    let mut total_dl: i64 = 0;
    for t in &all {
        total_ul += t.total_uploaded.load(Ordering::Relaxed) as i64;
        total_dl += t.total_downloaded.load(Ordering::Relaxed) as i64;
    }
    // unseeded_peers fallback to active_peers since is_seed tracking disabled
    // (would require per-peer state to compute properly).
    // This makes the UI ratio > 100% possible but at least it's non-zero.
    let mut total_active_peers = 0i64;
    for t in &all {
        total_active_peers += t.peers_connected.load(Ordering::Relaxed) as i64;
    }
    let total_unseeded_peers = total_active_peers;
    json!({
        "total_upload": total_ul,
        "total_download": total_dl,
        "upload_rate": mgr.upload_rate.get(),
        "download_rate": mgr.download_rate.get(),
        "num_torrents": all.len(),
        "unseeded_peers": total_unseeded_peers,
    })
}

/// Replace a torrent's tracker list wholesale, in tiers.
///
/// Whole-list replacement is the only primitive: add, remove and edit are all
/// read-modify-write on the caller's side, so there is exactly one path that
/// can change what we announce to, and it is idempotent. Applying the same
/// list twice is a no-op rather than a silent duplicate.
///
/// Empty URLs and empty tiers are dropped, since a tier with nothing in it
/// makes the announce loop skip a level for no reason. An empty list overall
/// IS allowed: it means "announce to nobody", which is a legitimate state for
/// a torrent kept only for DHT or for local peers.
fn set_trackers(params: &Value, mgr: &Arc<TorrentManager>) -> Value {
    let ih = match get_info_hash(params) { Ok(ih) => ih, Err(e) => return e };
    let t = match mgr.get(&ih) {
        Some(t) => t,
        None => return json!({"error": "torrent not found"}),
    };
    let raw = match params.get("trackers").and_then(|v| v.as_array()) {
        Some(a) => a,
        None => return json!({"error": "trackers must be an array of tiers"}),
    };
    let mut tiers: Vec<Vec<String>> = Vec::with_capacity(raw.len());
    for tier in raw {
        let urls = match tier.as_array() {
            Some(u) => u,
            None => return json!({"error": "each tier must be an array of urls"}),
        };
        let cleaned: Vec<String> = urls
            .iter()
            .filter_map(|u| u.as_str())
            .map(|u| u.trim().to_string())
            .filter(|u| !u.is_empty())
            .collect();
        if !cleaned.is_empty() {
            tiers.push(cleaned);
        }
    }
    let count = tiers.iter().map(|t| t.len()).sum::<usize>();
    // Through the manager, so the resume record is written in the same breath:
    // a list that only lives in memory is undone by the next restart.
    let _ = t;
    if let Err(e) = mgr.set_trackers(&ih, tiers.clone()) {
        return json!({"error": e});
    }
    info!("[rpc] set_trackers {} -> {} tiers, {} urls",
          crate::torrent::hex_encode(&ih), tiers.len(), count);
    // Hand back what actually stuck, not what was asked for: the caller writes
    // this same list to durable storage, and the two must not drift.
    json!({"trackers": tiers, "count": count})
}

fn get_trackers(params: &Value, mgr: &Arc<TorrentManager>) -> Value {
    let ih = match get_info_hash(params) { Ok(ih) => ih, Err(e) => return e };
    let t = match mgr.get(&ih) {
        Some(t) => t,
        None => return json!({"error": "torrent not found"}),
    };
    // Typhon tracks a single last-announce state per torrent (not per tracker).
    // Surface it on each tracker entry so the UI detail view stops disagreeing
    // with the list view. Exact for the common 1-tracker case, best-effort on
    // multi-tracker torrents (we don't track which URL failed last).
    let last_error = t.last_announce_error.lock().map(|g| g.clone()).unwrap_or_default();
    let ok = last_error.is_empty();
    let seeders = t.scrape_seeders.load(Ordering::Relaxed) as i64;
    let leechers = t.scrape_leechers.load(Ordering::Relaxed) as i64;
    // NOTE: the Go control plane owns every announce, so the
    // loop that fills these is inert in the normal deployment and the Go side
    // overwrites both fields from its own announce observations. They are kept
    // correct for the case where the internal loop IS enabled, and as the
    // "never / unknown" default the Go override replaces.
    // Seconds since / until, not absolute stamps: the UI shows durations, and
    // sending stamps would make it depend on the browser clock matching ours.
    // -1 means "never" / "not known", which the UI renders as a dash; 0 is a
    // real answer meaning "due now" and must stay distinguishable from it.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let last_at = t.last_announce_at.load(Ordering::Relaxed);
    let next_at = t.next_announce_at.load(Ordering::Relaxed);
    let last_announce = if last_at > 0 { (now - last_at).max(0) } else { -1 };
    let next_announce = if next_at > 0 { (next_at - now).max(0) } else { -1 };
    let live: Vec<Vec<String>> = t.live_trackers.read().clone();
    // One entry per URL, not per tier. Reporting only `urls.first()` hid every
    // fallback URL a tier holds, and a caller doing read-modify-write on this
    // list would have written the hidden ones out of existence.
    let mut trackers: Vec<Value> = Vec::new();
    for (tier, urls) in live.iter().enumerate() {
        for url in urls {
            let err_str = if ok { "Success".to_string() } else { last_error.clone() };
            let msg = if ok { String::new() } else { last_error.clone() };
            trackers.push(json!({
                "url": url,
                "tier": tier,
                "verified": ok,
                "endpoints": [{
                    "last_error": err_str,
                    "message": msg,
                    "last_announce": last_announce,
                    "next_announce": next_announce,
                    "scrape_complete": seeders,
                    "scrape_incomplete": leechers,
                }],
            }));
        }
    }
    json!({"trackers": trackers})
}

fn get_files(params: &Value, mgr: &Arc<TorrentManager>) -> Value {
    let ih = match get_info_hash(params) { Ok(ih) => ih, Err(e) => return e };
    let t = match mgr.get(&ih) {
        Some(t) => t,
        None => return json!({"error": "torrent not found"}),
    };
    let files: Vec<Value> = t.meta.files.iter().map(|f| {
        json!({"path": f.path.to_string_lossy(), "size": f.length})
    }).collect();
    json!({"files": files})
}

/// Engine-side optimisation flags, toggled at runtime by POST /api/opt/flags.
/// Same rationale as the Go-side registry: each flag gates ONE change so an A/B
/// ladder can measure it in isolation, and a restart to do that would cost real
/// tracker credit.
fn set_opt_flag(params: &Value, torrent_mgr: &Arc<TorrentManager>) -> Value {
    let name = params.get("flag").and_then(|v| v.as_str()).unwrap_or("");
    match name {
        "session_pinning" => {
            let on = params.get("on").and_then(|v| v.as_bool()).unwrap_or(false);
            crate::peer::set_session_pinning(on);
            json!({"ok": true, "flags": opt_flags_map(torrent_mgr)})
        }
        "block_mse" => {
            let on = params.get("on").and_then(|v| v.as_bool()).unwrap_or(false);
            torrent_mgr.policy().set_block_mse(on);
            json!({"ok": true, "flags": opt_flags_map(torrent_mgr)})
        }
        "session_runtimes" => {
            let n = params.get("value").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            if n == 0 {
                return json!({"error": "session_runtimes must be >= 1"});
            }
            if !crate::peer::set_session_runtimes(n) {
                return json!({"error": "runtime pool already built; the size is fixed until restart"});
            }
            json!({"ok": true, "flags": opt_flags_map(torrent_mgr)})
        }
        _ => json!({"error": format!("unknown engine flag: {}", name)}),
    }
}

fn opt_flags_map(torrent_mgr: &Arc<TorrentManager>) -> Value {
    json!({
        "session_pinning": crate::peer::session_pinning(),
        "block_mse": torrent_mgr.policy().block_mse(),
        "session_runtimes": crate::peer::session_runtimes_n(),
    })
}

fn get_opt_flags(torrent_mgr: &Arc<TorrentManager>) -> Value {
    json!({"flags": opt_flags_map(torrent_mgr)})
}

/// Piece availability as seen from the swarm. Only download-mode torrents have
/// a picker: a seed_mode torrent carries no bitfield at all, which is exactly
/// what keeps 100k torrents cheap, so there is nothing to report for it and we
/// say so rather than inventing a number.
fn get_availability(params: &Value, mgr: &Arc<TorrentManager>) -> Value {
    let ih = match get_info_hash(params) { Ok(ih) => ih, Err(e) => return e };
    let t = match mgr.get(&ih) {
        Some(t) => t,
        None => return json!({"error": "torrent not found"}),
    };
    let num_pieces = t.meta.num_pieces();
    let picker = match t.picker.get() {
        Some(p) => p,
        None => return json!({"has_piece_map": false, "num_pieces": num_pieces}),
    };
    let (min, max, sum) = picker.lock().unwrap().availability_stats();
    let avg = if num_pieces > 0 { sum as f64 / num_pieces as f64 } else { 0.0 };
    json!({
        "has_piece_map": true,
        "num_pieces": num_pieces,
        "min_availability": min,
        "max_availability": max,
        "avg_availability": avg,
    })
}

fn get_diagnostics(mgr: &Arc<TorrentManager>, config: &EngineConfig) -> Value {
    let all = mgr.all();
    let mut total_peers = 0usize;
    let mut total_interested = 0usize;
    let mut total_uploading = 0usize;
    let mut pex_discovered = 0u64;

    for t in &all {
        let peers = t.peers_connected.load(Ordering::Relaxed);
        let interested = t.peers_interested.load(Ordering::Relaxed);
        total_peers += peers;
        total_interested += interested;
        if t.total_uploaded.load(Ordering::Relaxed) > 0 {
            total_uploading += 1;
        }
        pex_discovered += t.pex_peers_discovered.load(Ordering::Relaxed);
    }

    // NOTE: counters must be built via an explicit map rather than `json!{}` — the
    // macro recurses once per key, which overflows the macro recursion limit
    // past ~30 keys.
    let mut counters = serde_json::Map::new();
    macro_rules! put_u { ($k:expr, $v:expr) => { counters.insert($k.to_string(), json!($v)); } }
    put_u!("num_peers_connected", total_peers as u64);
    put_u!("dial_attempted", crate::tracker::DIAL_ATTEMPTED.load(Ordering::Relaxed));
    put_u!("dial_enqueued", crate::tracker::DIAL_ENQUEUED.load(Ordering::Relaxed));
    put_u!("dial_enqueue_dropped", crate::tracker::DIAL_ENQUEUE_DROPPED.load(Ordering::Relaxed));
    put_u!("dial_plain_ok", crate::tracker::DIAL_PLAIN_OK.load(Ordering::Relaxed));
    put_u!("dial_plain_fail", crate::tracker::DIAL_PLAIN_FAIL.load(Ordering::Relaxed));
    put_u!("dial_mse_attempted", crate::tracker::DIAL_MSE_ATTEMPTED.load(Ordering::Relaxed));
    put_u!("dial_mse_ok", crate::tracker::DIAL_MSE_OK.load(Ordering::Relaxed));
    put_u!("dial_mse_fail", crate::tracker::DIAL_MSE_FAIL.load(Ordering::Relaxed));
    put_u!("inbound_accepted", crate::peer::INBOUND_ACCEPTED.load(Ordering::Relaxed));
    put_u!("dial_hs_timed_out", crate::tracker::DIAL_HS_TIMED_OUT.load(Ordering::Relaxed));
    put_u!("seed_seed_dropped", crate::peer::SEED_SEED_DROPPED.load(Ordering::Relaxed));
    put_u!("mse_inbound_refused", crate::tracker::MSE_INBOUND_REFUSED.load(Ordering::Relaxed));
    put_u!("mse_outbound_skipped", crate::tracker::MSE_OUTBOUND_SKIPPED.load(Ordering::Relaxed));
    put_u!("mse_sessions_dropped", crate::tracker::MSE_SESSIONS_DROPPED.load(Ordering::Relaxed));
    put_u!("dial_tcp_ok", crate::tracker::DIAL_TCP_OK.load(Ordering::Relaxed));
    put_u!("dial_tcp_fail", crate::tracker::DIAL_TCP_FAIL.load(Ordering::Relaxed));
    put_u!("dial_utp_ok", crate::tracker::DIAL_UTP_OK.load(Ordering::Relaxed));
    put_u!("dial_utp_fail", crate::tracker::DIAL_UTP_FAIL.load(Ordering::Relaxed));
    put_u!("dial_utp_fail_timeout", crate::tracker::DIAL_UTP_FAIL_TIMEOUT.load(Ordering::Relaxed));
    put_u!("dial_utp_fail_error", crate::tracker::DIAL_UTP_FAIL_ERROR.load(Ordering::Relaxed));
    put_u!("dial_utp_err_too_many", crate::tracker::DIAL_UTP_ERR_TOO_MANY.load(Ordering::Relaxed));
    put_u!("dial_utp_err_send_syn", crate::tracker::DIAL_UTP_ERR_SEND_SYN.load(Ordering::Relaxed));
    put_u!("dial_utp_err_dispatcher", crate::tracker::DIAL_UTP_ERR_DISPATCHER.load(Ordering::Relaxed));
    put_u!("dial_utp_err_other", crate::tracker::DIAL_UTP_ERR_OTHER.load(Ordering::Relaxed));
    put_u!("dial_utp_skipped_inflight", crate::tracker::DIAL_UTP_SKIPPED_INFLIGHT.load(Ordering::Relaxed));
    put_u!("dial_skipped_inflight", crate::tracker::DIAL_SKIPPED_INFLIGHT.load(Ordering::Relaxed));
    put_u!("dial_skipped_connected", crate::tracker::DIAL_SKIPPED_CONNECTED.load(Ordering::Relaxed));
    put_u!("dial_handshake_ok", crate::tracker::DIAL_HANDSHAKE_OK.load(Ordering::Relaxed));
    put_u!("dial_handshake_fail", crate::tracker::DIAL_HANDSHAKE_FAIL.load(Ordering::Relaxed));
    put_u!("bt_sent_interested", crate::tracker::BT_SENT_INTERESTED.load(Ordering::Relaxed));
    put_u!("bt_got_unchoke", crate::tracker::BT_GOT_UNCHOKE.load(Ordering::Relaxed));
    put_u!("bt_got_choke", crate::tracker::BT_GOT_CHOKE.load(Ordering::Relaxed));
    put_u!("bt_got_bitfield", crate::tracker::BT_GOT_BITFIELD.load(Ordering::Relaxed));
    put_u!("bt_got_have_all", crate::tracker::BT_GOT_HAVE_ALL.load(Ordering::Relaxed));
    put_u!("bt_got_have_none", crate::tracker::BT_GOT_HAVE_NONE.load(Ordering::Relaxed));
    put_u!("bt_got_have", crate::tracker::BT_GOT_HAVE.load(Ordering::Relaxed));
    put_u!("bt_got_interested", crate::tracker::BT_GOT_INTERESTED.load(Ordering::Relaxed));
    put_u!("bt_got_request", crate::tracker::BT_GOT_REQUEST.load(Ordering::Relaxed));
    put_u!("bt_sent_piece", crate::tracker::BT_SENT_PIECE.load(Ordering::Relaxed));
    put_u!("bt_sent_request", crate::tracker::BT_SENT_REQUEST.load(Ordering::Relaxed));
    put_u!("bt_got_piece", crate::tracker::BT_GOT_PIECE.load(Ordering::Relaxed));
    put_u!("bt_dl_entries_loop", crate::tracker::BT_DL_ENTRIES_LOOP.load(Ordering::Relaxed));
    put_u!("bt_dl_should_interested_false", crate::tracker::BT_DL_SHOULD_INTERESTED_FALSE.load(Ordering::Relaxed));
    put_u!("disk_cache_hit", crate::disk::DISK_CACHE_HIT.load(Ordering::Relaxed));
    put_u!("disk_cache_miss", crate::disk::DISK_CACHE_MISS.load(Ordering::Relaxed));
    put_u!("disk_cache_stale", crate::disk::DISK_CACHE_STALE.load(Ordering::Relaxed));
    put_u!("disk_cache_bypass", crate::disk::DISK_CACHE_BYPASS.load(Ordering::Relaxed));
    put_u!("peers_seeders_connected", crate::tracker::PEERS_SEEDERS_CONNECTED.load(Ordering::Relaxed));
    put_u!("peers_leechers_connected", crate::tracker::PEERS_LEECHERS_CONNECTED.load(Ordering::Relaxed));
    put_u!("leech_lifetime_lt1s", crate::tracker::LEECH_LIFETIME_LT1S.load(Ordering::Relaxed));
    put_u!("leech_lifetime_1_5s", crate::tracker::LEECH_LIFETIME_1_5S.load(Ordering::Relaxed));
    put_u!("leech_lifetime_5_30s", crate::tracker::LEECH_LIFETIME_5_30S.load(Ordering::Relaxed));
    put_u!("leech_lifetime_30_300s", crate::tracker::LEECH_LIFETIME_30_300S.load(Ordering::Relaxed));
    put_u!("leech_lifetime_gt300s", crate::tracker::LEECH_LIFETIME_GT300S.load(Ordering::Relaxed));
    put_u!("leech_never_interested", crate::tracker::LEECH_NEVER_INTERESTED.load(Ordering::Relaxed));
    put_u!("leech_got_interested", crate::tracker::LEECH_GOT_INTERESTED.load(Ordering::Relaxed));
    put_u!("leech_got_request", crate::tracker::LEECH_GOT_REQUEST.load(Ordering::Relaxed));
    put_u!("leech_we_served_piece", crate::tracker::LEECH_WE_SERVED_PIECE.load(Ordering::Relaxed));
    put_u!("seeders_in_total", crate::tracker::SEEDERS_IN_TOTAL.load(Ordering::Relaxed));
    put_u!("seeders_out_total", crate::tracker::SEEDERS_OUT_TOTAL.load(Ordering::Relaxed));
    put_u!("leechers_in_total", crate::tracker::LEECHERS_IN_TOTAL.load(Ordering::Relaxed));
    put_u!("leechers_out_total", crate::tracker::LEECHERS_OUT_TOTAL.load(Ordering::Relaxed));
    put_u!("pex_ext_handshakes_sent", crate::tracker::PEX_EXT_HANDSHAKES_SENT.load(Ordering::Relaxed));
    put_u!("pex_ext_handshakes_recv", crate::tracker::PEX_EXT_HANDSHAKES_RECV.load(Ordering::Relaxed));
    // Never incremented anywhere, and published as zero since they were added.
    // Kept so the shape of the answer does not change.
    put_u!("pex_msgs_sent", 0u64);
    put_u!("pex_msgs_recv", 0u64);
    put_u!("pex_peers_dialed", 0u64);
    put_u!("pex_peers_discovered", pex_discovered);
    // This engine's node, not the process's: an engine without a DHT reports
    // zeroes rather than its neighbour's traffic.
    let (dht_tracked, dht_found, dht_dialed) = match mgr.dht() {
        Some(d) => (d.torrents_tracked(), d.peers_discovered(), d.peers_dialed()),
        None => (0, 0, 0),
    };
    put_u!("dht_torrents_tracked", dht_tracked);
    put_u!("dht_peers_discovered", dht_found);
    put_u!("dht_peers_dialed", dht_dialed);

    json!({
        "peer_analysis": {
            "total_peers": total_peers,
            "peers_interested": total_interested,
            "peers_unchoked_interested": total_interested,
            "peers_choked_interested": 0,
            "peers_actively_uploading": total_uploading,
            "torrents_with_interested_peers": 0,
            "total_pending_send_bytes": 0,
        },
        "settings": {
            "max_uploads_per_torrent": config.max_uploads_per_torrent,
            "max_connections": config.max_connections,
            "max_dials_per_sec": config.max_dials_per_sec,
            "peer_timeout": config.peer_timeout,
            "engine": "typhon",
        },
        // Live view of the dial governor: what the ceiling is, how close we
        // are to it, and how much work each control is shedding. `skipped_*`
        // climbing is the signal that a limit is set too tight.
        // The self-dial filter, which ran blind until 2026-08-29: the counter
        // below already existed and was exposed nowhere, so 7342 self-dials
        // could accumulate without a single signal.
        "self_dial_filter": {
            "pushed_ips": crate::tracker::self_ip_sets().0,
            "own_ips": crate::tracker::self_ip_sets().1,
            "dials_skipped_self": crate::tracker::DIAL_SKIPPED_SELF.load(std::sync::atomic::Ordering::Relaxed),
        },
        "dial_governor": {
            "live_connections": mgr.limiter().live_connections(),
            "max_connections": mgr.limiter().max_connections(),
            // The live rate, which after a hot change is NOT what `settings`
            // above reports: that one still echoes the config file.
            "max_dials_per_sec": mgr.limiter().max_dials_per_sec(),
            "dials_paused": mgr.limiter().dials_paused(),
            "skipped_conn_cap": mgr.limiter().skipped_conn_cap(),
            "skipped_paused": mgr.limiter().skipped_paused(),
            "delayed": mgr.limiter().delayed(),
        },
        "counters": Value::Object(counters),
    })
}

/// tracker_host_of extracts the bare host from a tracker announce URL, e.g.
/// "https://tk.tr4ker.net/announce/KEY" -> "tk.tr4ker.net". Lets the list view
/// label each torrent with its (static) tracker without a per-torrent RPC.
pub fn tracker_host_of(url: &str) -> String {
    let s = url.split("://").nth(1).unwrap_or(url);
    s.split(|c| c == '/' || c == ':').next().unwrap_or("").to_string()
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
    let current_tracker = t.current_tracker.lock()
        .map(|s| s.clone())
        .unwrap_or_default();
    let scrape_seeders = t.scrape_seeders.load(Ordering::Relaxed);
    let scrape_leechers = t.scrape_leechers.load(Ordering::Relaxed);
    let announce_ok = t.last_announce_ok.load(Ordering::Relaxed);
    let tracker_error = t.last_announce_error.lock()
        .map(|s| s.clone())
        .unwrap_or_default();
    let tracker_host = t.live_trackers.read().iter().flatten().next()
        .map(|u| tracker_host_of(u))
        .unwrap_or_default();
    let error_msg = t.error_msg.lock().map(|g| g.clone()).unwrap_or_default();

    json!({
        "info_hash": hex_encode(&t.info_hash),
        "name": t.meta.name,
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

/// Inject a list of peers into a torrent's dial queue. Used by Go orchestrator
/// after a tracker announce to feed Typhon the peer list. Each addr is pushed
/// through `enqueue_dial` (same path as PEX/DHT discoveries) and dial_peer
/// is spawned by the queue consumer.
fn add_peers(params: &Value, mgr: &Arc<TorrentManager>) -> Value {
    let ih = match get_info_hash(params) { Ok(ih) => ih, Err(e) => return e };
    let t = match mgr.get(&ih) {
        Some(t) => t,
        None => return json!({"error": "torrent not found"}),
    };
    let peers_arr = match params.get("peers").and_then(|v| v.as_array()) {
        Some(a) => a,
        None => return json!({"error": "missing peers array"}),
    };
    let mut added = 0u32;
    for p in peers_arr {
        let ip = match p.get("ip").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => continue,
        };
        let port = match p.get("port").and_then(|v| v.as_u64()) {
            Some(p) if p > 0 && p <= u16::MAX as u64 => p as u16,
            _ => continue,
        };
        // Parse the IP on its own, then pair it with the port. Going through
        // "{ip}:{port}" works only for v4: a bare v6 literal would come out as
        // `2001:db8::1:6881`, which is not a socket address, so every v6 peer
        // the orchestrator handed us was dropped here without a word.
        let addr: std::net::SocketAddr = match ip.parse::<std::net::IpAddr>() {
            Ok(parsed) => std::net::SocketAddr::new(parsed, port),
            Err(_) => continue,
        };
        // Skip self-IPs (own VPS / styx netns / tunnel egress) — same filter
        // as inbound rejection in peer/mod.rs to avoid loopback dials.
        if crate::tracker::is_self_ip(addr.ip()) {
            continue;
        }
        crate::tracker::enqueue_dial(addr, t.clone());
        added += 1;
    }
    json!({"added": added})
}

/// Hold or release the startup pause. While held, the dial-queue consumer
/// drops every outbound dial instead of opening it, so a freshly started
/// engine creates no outbound flows until the user says so -- the point being
/// to let a VPN user finish configuring before 20k torrents worth of peers hit
/// the tunnel. Deliberately process-level: it never touches per-torrent paused
/// state, so releasing it cannot resurrect torrents the user paused by hand.
fn set_dials_paused(params: &Value, torrent_mgr: &Arc<TorrentManager>) -> Value {
    let paused = match params.get("paused").and_then(|v| v.as_bool()) {
        Some(p) => p,
        None => return json!({"error": "missing or invalid 'paused' boolean"}),
    };
    torrent_mgr.limiter().set_dials_paused(paused);
    info!("[peer] outbound dials {}", if paused { "PAUSED (startup pause held)" } else { "resumed" });
    json!({"ok": true, "paused": paused})
}

/// Hot-sets the dial governor: rate ceiling and/or live-connection ceiling.
///
/// Both fields are optional so a caller can move one without having to know
/// the other; omitting both is an error rather than a silent no-op, because a
/// request that changes nothing is a bug at the caller and should say so.
fn set_dial_limits(params: &Value, torrent_mgr: &Arc<TorrentManager>) -> Value {
    let rate = params.get("max_dials_per_sec").and_then(|v| v.as_f64());
    let conns = params.get("max_connections").and_then(|v| v.as_u64());
    if rate.is_none() && conns.is_none() {
        return json!({"error": "need at least one of 'max_dials_per_sec' or 'max_connections'"});
    }
    if let Some(r) = rate {
        torrent_mgr.limiter().set_max_dials_per_sec(r);
        info!("[peer] outbound dial rate ceiling set to {}/s (0 = unlimited)", r);
    }
    if let Some(c) = conns {
        torrent_mgr.limiter().set_max_connections(c as usize);
        info!("[peer] live connection ceiling set to {} (0 = unlimited)", c);
    }
    json!({
        "ok": true,
        "max_dials_per_sec": torrent_mgr.limiter().max_dials_per_sec(),
        "max_connections": torrent_mgr.limiter().max_connections(),
    })
}

/// Hot-rebind the engine's TCP peer listener to a new port without a restart.
/// Used by the Go orchestrator when a dynamic upstream port (e.g. gluetun /
/// Proton port-forward) rotates. Torrents and live peer connections are kept.
fn set_listen_port(params: &Value, torrent_mgr: &Arc<TorrentManager>) -> Value {
    let port = match params.get("port").and_then(|v| v.as_u64()) {
        Some(p) if p > 0 && p <= u16::MAX as u64 => p as u16,
        _ => return json!({"error": "invalid or missing port"}),
    };
    if torrent_mgr.request_listen_rebind(port) {
        json!({"ok": true, "port": port})
    } else {
        json!({"error": "listener supervisor not ready"})
    }
}

fn get_info_hash(params: &Value) -> Result<[u8; 20], Value> {
    let hex = params.get("info_hash")
        .and_then(|v| v.as_str())
        .ok_or_else(|| json!({"error": "missing info_hash"}))?;
    hex_decode(hex).map_err(|e| json!({"error": e}))
}

// Replace the tracker self-dial IP set at runtime. Go pushes the current public
// IP(s) here so the self-dial pre-filter never goes stale. params: {"ips":[...]}
fn set_self_ips(params: &Value) -> Value {
    let arr = match params.get("ips").and_then(|v| v.as_array()) {
        Some(a) => a,
        None => return json!({"error": "missing ips"}),
    };
    let ips: Vec<std::net::IpAddr> = arr
        .iter()
        .filter_map(|v| v.as_str())
        .filter_map(|s| s.trim().parse::<std::net::IpAddr>().ok())
        .collect();
    let count = ips.len();
    crate::tracker::set_self_ips(ips);
    json!({"ok": true, "count": count})
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A manager on a throwaway directory. `TorrentManager::new` opens a state
    /// database beside the resume folder, so each test gets its own tree rather
    /// than sharing one and racing on it.
    fn manager(tag: &str) -> (Arc<TorrentManager>, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "hydra-dispatch-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let data = root.join("data");
        let resume = root.join("cfg").join("resume");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&resume).unwrap();
        let mgr = Arc::new(TorrentManager::new(
            data.to_string_lossy().into_owned(),
            resume.to_string_lossy().into_owned(),
            Arc::new(DiskManager::new(16)),
        ));
        (mgr, root)
    }

    /// Every field carries a serde default, so an empty object is a valid
    /// engine configuration -- and a truer fixture than a hand-listed struct,
    /// which would drift the moment a field is added.
    fn cfg() -> EngineConfig {
        serde_json::from_str("{}").expect("every field has a default")
    }

    fn call(mgr: &Arc<TorrentManager>, method: &str, params: Value) -> Value {
        dispatch(method, &params, mgr, &Arc::new(DiskManager::new(16)), &cfg())
    }

    /// A minimal single-file torrent on disk, so `add_torrent` has something
    /// real to parse. Built by hand: the bytes under test are visible here.
    fn write_torrent(dir: &std::path::Path, name: &str, length: u64) -> String {
        let mut info = Vec::new();
        info.extend_from_slice(format!("d6:lengthi{length}e4:name{}:{name}", name.len()).as_bytes());
        info.extend_from_slice(b"12:piece lengthi16384e6:pieces20:");
        info.extend_from_slice(&[0xAB; 20]);
        info.push(b'e');

        // Computed, not counted by hand: a bencode length is the one thing in
        // this format that cannot be eyeballed, and getting it wrong yields a
        // file the parser refuses for a reason that has nothing to do with the
        // test.
        let announce = "https://tracker.example/announce";
        let mut out = Vec::new();
        out.extend_from_slice(
            format!("d8:announce{}:{announce}4:info", announce.len()).as_bytes(),
        );
        out.extend_from_slice(&info);
        out.push(b'e');

        let path = dir.join(format!("{name}.torrent"));
        std::fs::write(&path, &out).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn err_of(v: &Value) -> String {
        v.get("error").and_then(|e| e.as_str()).unwrap_or("").to_string()
    }

    // -----------------------------------------------------------------------
    // Routing
    // -----------------------------------------------------------------------

    #[test]
    fn ping_answers() {
        let (mgr, _d) = manager("ping");
        assert_eq!(call(&mgr, "ping", json!({})), json!({"pong": true}));
    }

    /// A method we do not have is an error, not a panic and not silence. This
    /// is the front door of the engine: a caller that sends a name we retired
    /// has to be told, or it waits on a reply that reads as success.
    #[test]
    fn an_unknown_method_is_refused_by_name() {
        let (mgr, _d) = manager("unknown");
        let v = call(&mgr, "no_such_method", json!({}));
        assert!(!err_of(&v).is_empty(), "an unknown method says so: {v}");
    }

    // -----------------------------------------------------------------------
    // Parameters
    // -----------------------------------------------------------------------

    /// Every handler takes its arguments from untyped JSON. A missing one must
    /// name itself: "error" alone sends whoever called it reading source.
    #[test]
    fn a_missing_argument_names_itself() {
        let (mgr, _d) = manager("missing");
        let v = call(&mgr, "add_torrent", json!({"save_path": "/tmp"}));
        assert!(err_of(&v).contains("torrent_path"), "{v}");

        let v = call(&mgr, "add_torrent", json!({"torrent_path": "/tmp/x.torrent"}));
        assert!(err_of(&v).contains("save_path"), "{v}");
    }

    /// An info hash arrives as forty hex characters from a stranger's JSON.
    /// Anything else is refused rather than padded or truncated into a hash
    /// that means another torrent.
    #[test]
    fn a_malformed_info_hash_is_refused() {
        let (mgr, _d) = manager("badhash");
        for bad in [json!("zz"), json!(""), json!("not-hex-at-all"), json!(42)] {
            let v = call(&mgr, "stop_torrent", json!({"info_hash": bad}));
            assert!(!err_of(&v).is_empty(), "{bad} should be refused: {v}");
        }
    }

    /// Operating on a torrent that is not here is an error for every verb.
    /// Answering `ok` would make a caller believe a stop it never got.
    #[test]
    fn every_verb_refuses_a_torrent_that_is_not_here() {
        let (mgr, _d) = manager("absent");
        let absent = json!({"info_hash": "ab".repeat(20)});
        for method in [
            "stop_torrent",
            "start_torrent",
            "remove_torrent",
            "verify_torrent",
            "set_serving_suspended",
        ] {
            let v = call(&mgr, method, absent.clone());
            assert!(
                !err_of(&v).is_empty(),
                "{method} answered {v} for a torrent that does not exist"
            );
            assert!(v.get("ok").is_none(), "{method} claimed success: {v}");
        }
    }

    // -----------------------------------------------------------------------
    // A torrent's life, through the front door only
    // -----------------------------------------------------------------------

    /// Add, see it listed, stop it, read it back, remove it. Everything the
    /// control plane does to a torrent goes through these calls, and none of
    /// them had ever been run by a test.
    #[test]
    fn a_torrent_can_be_added_listed_stopped_and_removed() {
        let (mgr, dir) = manager("lifecycle");
        let torrent = write_torrent(&dir, "sample", 32768);
        let save = dir.join("data").to_string_lossy().into_owned();

        let added = call(
            &mgr,
            "add_torrent",
            json!({"torrent_path": torrent, "save_path": save, "stopped": true}),
        );
        let ih = added
            .get("info_hash")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("add failed: {added}"))
            .to_string();
        assert_eq!(added.get("name").and_then(|v| v.as_str()), Some("sample"));

        let listed = call(&mgr, "list_torrents", json!({}));
        let body = listed.to_string();
        assert!(body.contains(&ih), "the torrent is in the list: {listed}");

        assert_eq!(call(&mgr, "stop_torrent", json!({"info_hash": ih})), json!({"ok": true}));

        let status = call(&mgr, "get_status", json!({"info_hash": ih}));
        assert!(
            err_of(&status).is_empty(),
            "a torrent that exists has a status: {status}"
        );

        assert!(
            call(&mgr, "remove_torrent", json!({"info_hash": ih, "keep_data": true}))
                .get("ok")
                .is_some()
        );
        // And it is gone: the same call twice must not both succeed.
        let v = call(&mgr, "stop_torrent", json!({"info_hash": ih}));
        assert!(!err_of(&v).is_empty(), "removed, so no longer stoppable: {v}");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A file that is not a torrent is refused with a reason, not accepted as
    /// an empty one.
    #[test]
    fn a_file_that_is_not_a_torrent_is_refused() {
        let (mgr, dir) = manager("notatorrent");
        let path = dir.join("junk.torrent");
        std::fs::write(&path, b"this is not bencode").unwrap();
        let v = call(
            &mgr,
            "add_torrent",
            json!({"torrent_path": path.to_string_lossy(), "save_path": dir.to_string_lossy()}),
        );
        assert!(!err_of(&v).is_empty(), "{v}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_torrent_path_that_does_not_exist_is_refused() {
        let (mgr, _d) = manager("nofile");
        let v = call(
            &mgr,
            "add_torrent",
            json!({"torrent_path": "/nonexistent/nope.torrent", "save_path": "/tmp"}),
        );
        assert!(!err_of(&v).is_empty(), "{v}");
    }

    /// The same torrent twice is one torrent. Accepting it again would give the
    /// tracker two peers for one client and double-count what we serve.
    #[test]
    fn adding_the_same_torrent_twice_is_refused() {
        let (mgr, dir) = manager("dup");
        let torrent = write_torrent(&dir, "twice", 16384);
        let save = dir.join("data").to_string_lossy().into_owned();
        let params = json!({"torrent_path": torrent, "save_path": save, "stopped": true});

        assert!(call(&mgr, "add_torrent", params.clone()).get("info_hash").is_some());
        let again = call(&mgr, "add_torrent", params);
        assert!(!err_of(&again).is_empty(), "the second add says no: {again}");

        std::fs::remove_dir_all(&dir).ok();
    }

    // -----------------------------------------------------------------------
    // Read-only calls
    // -----------------------------------------------------------------------

    /// An empty engine answers all of these rather than failing: a control
    /// plane polls them before anything has been added.
    #[test]
    fn the_read_only_calls_answer_on_an_empty_engine() {
        let (mgr, _d) = manager("empty");
        for method in ["list_torrents", "get_session_stats"] {
            let v = call(&mgr, method, json!({}));
            assert!(err_of(&v).is_empty(), "{method} failed on an empty engine: {v}");
        }
    }
}

#[cfg(test)]
mod verb_tests {
    use super::*;
    use serde_json::json;

    fn manager(tag: &str) -> (Arc<TorrentManager>, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "hydra-verbs-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let data = root.join("data");
        let resume = root.join("cfg").join("resume");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&resume).unwrap();
        let mgr = Arc::new(TorrentManager::new(
            data.to_string_lossy().into_owned(),
            resume.to_string_lossy().into_owned(),
            Arc::new(DiskManager::new(16)),
        ));
        (mgr, root)
    }

    fn cfg() -> EngineConfig {
        serde_json::from_str("{}").expect("every field has a default")
    }

    fn call(mgr: &Arc<TorrentManager>, method: &str, params: Value) -> Value {
        dispatch(method, &params, mgr, &Arc::new(DiskManager::new(16)), &cfg())
    }

    /// `dispatch` answers the payload ITSELF, not a `{"result": ...}` envelope
    /// -- the id and the framing are added by the connection loop. So a reply
    /// is any JSON object; what must never happen is no answer at all.
    fn is_a_reply(v: &Value) -> bool {
        v.is_object()
    }

    /// Every engine-wide verb answers on an engine holding nothing.
    ///
    /// "Nothing to report" is a real answer and the commonest state of a fresh
    /// install; a verb that panics or hangs there takes the control plane with
    /// it, because the dispatch loop is shared by every call on the socket.
    macro_rules! global_verbs {
        ($($test_name:ident => $method:expr, $params:expr);+ $(;)?) => {
            $(
                #[test]
                fn $test_name() {
                    let (mgr, root) = manager(stringify!($test_name));
                    let out = call(&mgr, $method, $params);
                    assert!(is_a_reply(&out), "{} answered {out}", $method);
                    let _ = std::fs::remove_dir_all(root);
                }
            )+
        };
    }

    global_verbs!(
        v_get_session_stats => "get_session_stats", json!({});
        v_get_diagnostics => "get_diagnostics", json!({});
        v_get_opt_flags => "get_opt_flags", json!({});
        v_session_pinning => "session_pinning", json!({});
        v_session_runtimes => "session_runtimes", json!({});
        v_export_state => "export_state", json!({});
        v_set_upload_limit => "set_upload_limit", json!({"limit": 1024});
        v_set_download_limit => "set_download_limit", json!({"limit": 2048});
        v_set_dial_limits => "set_dial_limits", json!({"max_dials_per_sec": 5.0});
        v_set_dials_paused => "set_dials_paused", json!({"paused": true});
        v_set_serving_suspended => "set_serving_suspended", json!({"suspended": false});
        v_set_self_ips => "set_self_ips", json!({"ips": ["93.184.216.34"]});
        v_set_opt_flag => "set_opt_flag", json!({"flag": "no_such_flag", "value": true});
        v_block_mse => "block_mse", json!({"blocked": true});
    );

    /// ⭐ Every per-torrent verb must REFUSE a hash it does not hold, rather
    /// than answer success for work it did not do. A 200 that means "received"
    /// and not "done" is the shape of seven bugs in this repo.
    macro_rules! unknown_hash_verbs {
        ($($test_name:ident => $method:expr);+ $(;)?) => {
            $(
                #[test]
                fn $test_name() {
                    let (mgr, root) = manager(stringify!($test_name));
                    let absent = "0".repeat(40);
                    let out = call(&mgr, $method, json!({"info_hash": absent}));
                    assert!(
                        out.get("error").is_some(),
                        "{} answered success for a torrent that is not here: {out}",
                        $method
                    );
                    let _ = std::fs::remove_dir_all(root);
                }
            )+
        };
    }

    unknown_hash_verbs!(
        u_get_files => "get_files";
        u_get_peers => "get_peers";
        u_get_trackers => "get_trackers";
        u_get_availability => "get_availability";
        u_start_torrent => "start_torrent";
        u_recheck_torrent => "recheck_torrent";
        u_verify_torrent => "verify_torrent";
        u_set_save_path => "set_save_path";
        u_add_peers => "add_peers";
        u_set_trackers => "set_trackers";
    );

    /// ⭐ `get_metadata` on a hash we do not hold is NOT a refusal: "unknown"
    /// is the honest state of a magnet whose dict has not arrived yet, and the
    /// UI polls this to find out. Answering an error would make a pending
    /// resolution indistinguishable from a broken one.
    #[test]
    fn metadata_for_an_unresolved_hash_is_a_state_not_an_error() {
        let (mgr, root) = manager("meta-unknown");
        let out = call(&mgr, "get_metadata", json!({"info_hash": "0".repeat(40)}));
        assert!(out.get("error").is_none(), "{out}");
        assert_eq!(out["state"], json!("unknown"));
        let _ = std::fs::remove_dir_all(root);
    }

    /// `fetch_metadata` starts a background resolution, so it needs a runtime
    /// to spawn onto -- and it must still refuse a hash it does not hold.
    #[tokio::test]
    async fn fetching_metadata_for_a_torrent_that_is_not_here_is_refused() {
        let (mgr, root) = manager("meta-fetch");
        let out = call(&mgr, "fetch_metadata", json!({"info_hash": "0".repeat(40)}));
        assert!(out.is_object(), "{out}");
        let _ = std::fs::remove_dir_all(root);
    }

    /// A malformed info hash is refused for being malformed, whatever the
    /// verb: 39 characters, non-hex, or empty are not "not found", they are
    /// not a hash at all.
    #[test]
    fn a_hash_that_is_not_a_hash_is_refused_by_every_verb() {
        let (mgr, root) = manager("badhash");
        for bad in ["", "xyz", "0".repeat(39).as_str(), "z".repeat(40).as_str()] {
            for method in ["get_files", "start_torrent", "remove_torrent", "get_peers"] {
                let out = call(&mgr, method, serde_json::json!({"info_hash": bad}));
                assert!(
                    out.get("error").is_some(),
                    "{method} accepted {bad:?}: {out}"
                );
            }
        }
        let _ = std::fs::remove_dir_all(root);
    }

    /// A verb whose required argument is missing names it, rather than
    /// defaulting to something and acting on the wrong torrent.
    #[test]
    fn a_missing_info_hash_is_reported_not_defaulted() {
        let (mgr, root) = manager("noarg");
        for method in ["get_files", "start_torrent", "set_save_path", "get_peers"] {
            let out = call(&mgr, method, serde_json::json!({}));
            assert!(out.get("error").is_some(), "{method} accepted no arguments: {out}");
        }
        let _ = std::fs::remove_dir_all(root);
    }

    /// Limits round-trip through the session: setting one and reading the
    /// stats back must not error, and zero means unlimited rather than
    /// "stopped".
    #[test]
    fn a_limit_of_zero_is_accepted_as_unlimited() {
        let (mgr, root) = manager("zerolimit");
        for method in ["set_upload_limit", "set_download_limit"] {
            let out = call(&mgr, method, serde_json::json!({"limit": 0}));
            assert!(is_a_reply(&out), "{method} answered {out}");
            assert!(out.get("error").is_none(), "zero is a valid limit: {out}");
        }
        let _ = std::fs::remove_dir_all(root);
    }

    /// Exporting the state of an empty engine gives something importable.
    /// A round trip that cannot be fed back in is not a backup.
    #[test]
    fn an_exported_empty_state_can_be_imported_back() {
        let (mgr, root) = manager("roundtrip");
        let exported = call(&mgr, "export_state", serde_json::json!({}));
        assert!(is_a_reply(&exported), "{exported}");
        if let Some(result) = exported.get("result") {
            let back = call(&mgr, "import_state", result.clone());
            assert!(is_a_reply(&back), "import answered {back}");
        }
        let _ = std::fs::remove_dir_all(root);
    }

    /// The listen port is engine state; setting it must answer rather than
    /// silently do nothing.
    #[test]
    fn setting_the_listen_port_is_answered() {
        let (mgr, root) = manager("listenport");
        let out = call(&mgr, "set_listen_port", serde_json::json!({"port": 16371}));
        assert!(is_a_reply(&out), "{out}");
        let _ = std::fs::remove_dir_all(root);
    }

    /// `tracker_host_of` is what keys every per-tracker counter and the
    /// breaker; getting it wrong splits one tracker into several.
    #[test]
    fn the_tracker_host_is_the_host_and_nothing_else() {
        assert_eq!(tracker_host_of("https://tracker.example/announce"), "tracker.example");
        assert_eq!(tracker_host_of("http://tracker.example:8080/x"), "tracker.example");
        assert_eq!(tracker_host_of("udp://tracker.example:1337"), "tracker.example");
        assert_eq!(tracker_host_of("tracker.example/announce"), "tracker.example");
        assert_eq!(tracker_host_of(""), "");
    }
}
