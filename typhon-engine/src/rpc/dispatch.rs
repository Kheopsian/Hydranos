use std::sync::Arc;
use std::sync::atomic::Ordering;
use serde_json::{json, Value};

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
        "remove_torrent" => remove_torrent(params, torrent_mgr),
        "start_torrent" => start_torrent(params, torrent_mgr),
        "stop_torrent" => stop_torrent(params, torrent_mgr),
        "set_serving_suspended" => set_serving_suspended(params, torrent_mgr),
        "set_save_path" => set_save_path(params, torrent_mgr),
        "verify_torrent" => verify_torrent(params, torrent_mgr),
        "recheck_torrent" => verify_torrent(params, torrent_mgr),
        "get_status" => get_status(params, torrent_mgr),
        "list_torrents" => list_torrents(torrent_mgr),
        "get_peers" => get_peers(params, torrent_mgr),
        "add_peers" => add_peers(params, torrent_mgr),
        "set_upload_limit" => json!({"ok": true}), // TODO
        "set_download_limit" => json!({"ok": true}), // TODO
        "get_session_stats" => get_session_stats(torrent_mgr),
        "get_trackers" => get_trackers(params, torrent_mgr),
        "get_files" => get_files(params, torrent_mgr),
        "get_availability" => get_availability(params, torrent_mgr),
        "set_opt_flag" => set_opt_flag(params),
        "get_opt_flags" => get_opt_flags(),
        "get_diagnostics" => get_diagnostics(torrent_mgr, config),
        "set_listen_port" => set_listen_port(params),
        "set_self_ips" => set_self_ips(params),
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
            if !seed_mode && !stopped && mgr.first_file_exists(&ih) {
                let _ = mgr.recheck(&ih);
            }
            json!({"info_hash": hex_encode(&ih), "name": name})
        }
        Err(e) => json!({"error": e}),
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

fn list_torrents(mgr: &Arc<TorrentManager>) -> Value {
    let all = mgr.all();
    let torrents: Vec<Value> = all.iter().map(|t| torrent_to_json(t)).collect();
    json!({"torrents": torrents, "count": torrents.len()})
}

fn get_peers(params: &Value, mgr: &Arc<TorrentManager>) -> Value {
    let ih = match get_info_hash(params) { Ok(ih) => ih, Err(e) => return e };
    let t = match mgr.get(&ih) {
        Some(t) => t,
        None => return json!({"error": "torrent not found"}),
    };
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
    json!({"peers": peers})
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
    let trackers: Vec<Value> = t.meta.trackers.iter().enumerate().map(|(tier, urls)| {
        let err_str = if ok { "Success".to_string() } else { last_error.clone() };
        let msg = if ok { String::new() } else { last_error.clone() };
        json!({
            "url": urls.first().unwrap_or(&String::new()),
            "tier": tier,
            "verified": ok,
            "endpoints": [{
                "last_error": err_str,
                "message": msg,
                "next_announce": 0,
                "scrape_complete": seeders,
                "scrape_incomplete": leechers,
            }],
        })
    }).collect();
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
fn set_opt_flag(params: &Value) -> Value {
    let name = params.get("flag").and_then(|v| v.as_str()).unwrap_or("");
    match name {
        "session_pinning" => {
            let on = params.get("on").and_then(|v| v.as_bool()).unwrap_or(false);
            crate::peer::set_session_pinning(on);
            json!({"ok": true, "flags": opt_flags_map()})
        }
        "session_runtimes" => {
            let n = params.get("value").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            if n == 0 {
                return json!({"error": "session_runtimes must be >= 1"});
            }
            if !crate::peer::set_session_runtimes(n) {
                return json!({"error": "runtime pool already built; the size is fixed until restart"});
            }
            json!({"ok": true, "flags": opt_flags_map()})
        }
        _ => json!({"error": format!("unknown engine flag: {}", name)}),
    }
}

fn opt_flags_map() -> Value {
    json!({
        "session_pinning": crate::peer::session_pinning(),
        "session_runtimes": crate::peer::session_runtimes_n(),
    })
}

fn get_opt_flags() -> Value {
    json!({"flags": opt_flags_map()})
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

    for t in &all {
        let peers = t.peers_connected.load(Ordering::Relaxed);
        let interested = t.peers_interested.load(Ordering::Relaxed);
        total_peers += peers;
        total_interested += interested;
        if t.total_uploaded.load(Ordering::Relaxed) > 0 {
            total_uploading += 1;
        }
    }

    // NOTE: counters must be built via an explicit map rather than `json!{}` — the
    // macro recurses once per key, which overflows the macro recursion limit
    // past ~30 keys.
    let mut counters = serde_json::Map::new();
    macro_rules! put_u { ($k:expr, $v:expr) => { counters.insert($k.to_string(), json!($v)); } }
    put_u!("num_peers_connected", total_peers as u64);
    put_u!("dial_attempted", crate::tracker::DIAL_ATTEMPTED.load(Ordering::Relaxed));
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
    put_u!("pex_msgs_sent", crate::tracker::PEX_MSGS_SENT.load(Ordering::Relaxed));
    put_u!("pex_msgs_recv", crate::tracker::PEX_MSGS_RECV.load(Ordering::Relaxed));
    put_u!("pex_peers_discovered", crate::tracker::PEX_PEERS_DISCOVERED.load(Ordering::Relaxed));
    put_u!("pex_peers_dialed", crate::tracker::PEX_PEERS_DIALED.load(Ordering::Relaxed));
    put_u!("dht_torrents_tracked", crate::dht::DHT_TORRENTS_TRACKED.load(Ordering::Relaxed));
    put_u!("dht_peers_discovered", crate::dht::DHT_PEERS_DISCOVERED.load(Ordering::Relaxed));
    put_u!("dht_peers_dialed", crate::dht::DHT_PEERS_DIALED.load(Ordering::Relaxed));

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
            "peer_timeout": config.peer_timeout,
            "engine": "typhon",
        },
        "counters": Value::Object(counters),
    })
}

/// tracker_host_of extracts the bare host from a tracker announce URL, e.g.
/// "https://tk.tr4ker.net/announce/KEY" -> "tk.tr4ker.net". Lets the list view
/// label each torrent with its (static) tracker without a per-torrent RPC.
fn tracker_host_of(url: &str) -> String {
    let s = url.split("://").nth(1).unwrap_or(url);
    s.split(|c| c == '/' || c == ':').next().unwrap_or("").to_string()
}

pub fn torrent_to_json(t: &Arc<crate::torrent::meta::TorrentState>) -> Value {
    let status_u8 = t.status.load(Ordering::Relaxed);
    let is_paused = t.is_paused.load(Ordering::Relaxed);
    let state = if is_paused {
        "paused"
    } else {
        match status_u8 {
            0 => "stopped",
            1 => "checking_files",
            2 => "downloading",
            3 => "seeding",
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
        (0, 0.0)
    };
    // Approximate — over-reports by ≤1 piece_length when the last (short)
    // piece is among the have set but we can't tell cheaply. Good enough
    // for a progress display.
    let total_done: u64 = if progress >= 1.0 {
        t.meta.total_size
    } else {
        (num_have as u64 * t.meta.piece_length as u64).min(t.meta.total_size)
    };

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let active_time = (now - t.added_time).max(0);
    let completed = t.completed_time.load(Ordering::Relaxed);
    let seeding_time = if completed > 0 { (now - completed).max(0) } else { 0 };
    let current_tracker = t.current_tracker.lock()
        .map(|s| s.clone())
        .unwrap_or_default();
    let scrape_seeders = t.scrape_seeders.load(Ordering::Relaxed);
    let scrape_leechers = t.scrape_leechers.load(Ordering::Relaxed);
    let announce_ok = t.last_announce_ok.load(Ordering::Relaxed);
    let tracker_error = t.last_announce_error.lock()
        .map(|s| s.clone())
        .unwrap_or_default();
    let tracker_host = t.meta.trackers.iter().flatten().next()
        .map(|u| tracker_host_of(u))
        .unwrap_or_default();

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
        let addr: std::net::SocketAddr = match format!("{}:{}", ip, port).parse() {
            Ok(a) => a,
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

/// Hot-rebind the engine's TCP peer listener to a new port without a restart.
/// Used by the Go orchestrator when a dynamic upstream port (e.g. gluetun /
/// Proton port-forward) rotates. Torrents and live peer connections are kept.
fn set_listen_port(params: &Value) -> Value {
    let port = match params.get("port").and_then(|v| v.as_u64()) {
        Some(p) if p > 0 && p <= u16::MAX as u64 => p as u16,
        _ => return json!({"error": "invalid or missing port"}),
    };
    if crate::peer::request_listen_rebind(port) {
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
