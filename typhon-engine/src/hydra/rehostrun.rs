//! Moving every torrent of one tracker host to another: a tracker changed its
//! domain.
//!
//! The Tracker form's host is the key of the per-host settings, so typing a new
//! one there only creates settings for another host. This is the part that
//! rewrites the torrents, as a job: a production catalogue holds hundreds of
//! thousands of torrents on one host, and one persisted resume record each is
//! minutes of work, not a request.
//!
//! The work goes through the single-torrent editor (`api::rehost_torrent`), so
//! a torrent with no stored `.torrent` is refused for the same reason there, a
//! failed save rolls the torrent back, and the reconnect ring hears of it.
//! Nothing is held across batches, and a torrent already moved no longer
//! matches, so a job found running at boot simply starts again.

use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::api::{authorised, guard, unauthorised, AppState};

pub const KIND: &str = "tracker_rehost";

/// What `run` returns when the shutdown began: the row stays `running` and the
/// next start queues it again.
pub const INTERRUPTED: &str = "interrupted by shutdown";

const BATCH: usize = 200;

/// The sections of the config keyed by tracker host.
const HOST_SECTIONS: [&str; 5] = [
    "announce_passkeys",
    "announce_ip_modes",
    "announce_min_seed_hours",
    "announce_muted",
    "announce_hidden",
];

fn host_maps(
    cfg: &crate::config::Config,
) -> [(&'static str, &std::collections::BTreeMap<String, String>); 5] {
    [
        (HOST_SECTIONS[0], &cfg.announce_passkeys),
        (HOST_SECTIONS[1], &cfg.announce_ip_modes),
        (HOST_SECTIONS[2], &cfg.announce_min_seed_hours),
        (HOST_SECTIONS[3], &cfg.announce_muted),
        (HOST_SECTIONS[4], &cfg.announce_hidden),
    ]
}

fn error(code: StatusCode, msg: &str) -> Response {
    (code, Json(json!({ "error": msg }))).into_response()
}

/// Every torrent, per engine, with an announce URL on `from`. Visited in place:
/// no Vec of Arcs for a million torrents.
fn on_host(state: &AppState, from: &str) -> Vec<typhon_engine::torrent::meta::InfoHash> {
    let mut out = Vec::new();
    for engine in state.engines.engines() {
        engine.manager.for_each(|t| {
            if crate::trackeredit::mentions_host(&t.live_trackers.read(), from) {
                out.push(t.info_hash);
            }
        });
    }
    out
}

/// Torrents that still announce to `from` or to a subdomain of it: whatever the
/// settings of `from` still apply to.
fn still_served_by(state: &AppState, from: &str) -> usize {
    use crate::announce::overrides::{host_key_matches, override_host};
    let mut n = 0;
    for engine in state.engines.engines() {
        engine.manager.for_each(|t| {
            let hit = t
                .live_trackers
                .read()
                .iter()
                .flatten()
                .any(|u| host_key_matches(&override_host(u), from));
            n += usize::from(hit);
        });
    }
    n
}

/// The key of `map` that applies to `host`, read the way the code using that
/// section reads it. The passkey and the IP family are resolved at announce
/// time by the most specific key covering the host, a parent domain included
/// (`announce::policy`); the others are looked up by the exact host.
fn applying_key<'a>(
    section: &str,
    map: &'a std::collections::BTreeMap<String, String>,
    host: &str,
) -> Option<&'a str> {
    use crate::announce::overrides::{longest_override_key, override_host};
    if matches!(section, "announce_passkeys" | "announce_ip_modes") {
        longest_override_key(&override_host(host), map.keys().map(|k| k.as_str()))
    } else {
        map.keys()
            .find(|k| k.eq_ignore_ascii_case(host))
            .map(|k| k.as_str())
    }
}

/// Give `to` the settings that apply to `from`, so a rewritten torrent keeps
/// its passkey, IP family, minimum seed, mute and hide -- including a value it
/// only had through a parent domain. A key written for `to` itself is the
/// operator's and is kept; a value `to` would only get through a parent
/// domain is not enough when it differs, since the torrents would announce
/// with it once the old host's key is gone.
fn copy_settings(state: &AppState, from: &str, to_host: &str) -> bool {
    let cfg = state.cfg();
    let mut ok = true;
    for (section, map) in host_maps(&cfg) {
        let Some(value) = applying_key(section, map, from).and_then(|k| map.get(k)) else {
            continue;
        };
        let already = applying_key(section, map, to_host)
            .is_some_and(|k| k.eq_ignore_ascii_case(to_host) || map.get(k) == Some(value));
        if value.is_empty() || already {
            continue;
        }
        ok &= crate::api::set_host_entry(state, section, to_host, value);
    }
    ok
}

fn remove_settings(state: &AppState, from: &str) {
    let cfg = state.cfg();
    for (section, map) in host_maps(&cfg) {
        for key in map.keys().filter(|k| k.eq_ignore_ascii_case(from)) {
            crate::api::set_host_entry(state, section, key, "");
        }
    }
}

#[derive(serde::Deserialize)]
struct Req {
    #[serde(default)]
    from: String,
    #[serde(default)]
    to: String,
    #[serde(default)]
    dry_run: bool,
    /// A format move instead of a host move: the two templates, see
    /// `trackeredit::Format`.
    #[serde(default)]
    from_template: String,
    #[serde(default)]
    to_template: String,
}

/// What a walk of the catalogue finds for a format move.
#[derive(Default)]
struct FormatWalk {
    /// Torrents with at least one URL the move would change.
    torrents: usize,
    /// Torrents on the old host with no URL matching the old format.
    unmatched: usize,
    samples: Vec<(String, String)>,
    hashes: Vec<typhon_engine::torrent::meta::InfoHash>,
}

const SAMPLES: usize = 5;

/// Visited in place, like `on_host`. Samples are masked here, so no caller can
/// print a whole value; `hashes` is only filled when the job needs it.
fn walk_format(
    state: &AppState,
    fmt: &crate::trackeredit::Format,
    want_hashes: bool,
) -> FormatWalk {
    let mut w = FormatWalk::default();
    for engine in state.engines.engines() {
        engine.manager.for_each(|t| {
            let tiers = t.live_trackers.read();
            let (changes, unmatched) = fmt.scan(&tiers);
            if changes > 0 {
                w.torrents += 1;
                if want_hashes {
                    w.hashes.push(t.info_hash);
                }
                if w.samples.len() < SAMPLES {
                    let hit = tiers
                        .iter()
                        .flatten()
                        .find(|u| fmt.convert(u).is_some_and(|n| n != **u))
                        .and_then(|u| fmt.masked(u));
                    w.samples.extend(hit);
                }
            } else if unmatched > 0 {
                w.unmatched += 1;
            }
        });
    }
    w
}

/// `POST /api/trackers/rehost` with `from_template` and `to_template`.
async fn post_reformat(state: AppState, req: Req) -> Response {
    let fmt = match crate::trackeredit::Format::parse(&req.from_template, &req.to_template) {
        Ok(f) => f,
        Err(e) => return error(StatusCode::BAD_REQUEST, &e),
    };
    let walk = {
        let (state, fmt) = (state.clone(), fmt.clone());
        match tokio::task::spawn_blocking(move || walk_format(&state, &fmt, false)).await {
            Ok(w) => w,
            Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        }
    };
    if req.dry_run {
        let samples: Vec<_> = walk
            .samples
            .iter()
            .map(|(before, after)| json!({"before": before, "after": after}))
            .collect();
        return Json(json!({
            "dry_run": true, "mode": "format",
            "torrents": walk.torrents, "unmatched": walk.unmatched,
            "samples": samples,
        }))
        .into_response();
    }
    if walk.torrents == 0 {
        return error(
            StatusCode::BAD_REQUEST,
            "no torrent has an announce URL in that format",
        );
    }
    let busy = || {
        let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
        store.job_in_flight_of_kind(KIND)
    };
    if busy() {
        return error(
            StatusCode::CONFLICT,
            "a tracker move is already running; wait for it or cancel it",
        );
    }
    let (from_host, to_host) = (fmt.old_host().to_string(), fmt.new_host().to_string());
    // Same reason as for a host move: before the job exists. Nothing to copy
    // when the host stays.
    if !from_host.eq_ignore_ascii_case(&to_host) && !copy_settings(&state, &from_host, &to_host) {
        tracing::warn!("tracker move: the settings of the old host could not all be copied");
    }
    let id = {
        let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
        if store.job_in_flight_of_kind(KIND) {
            return error(
                StatusCode::CONFLICT,
                "a tracker move is already running; wait for it or cancel it",
            );
        }
        // Hosts and templates only: a template has holes, not values. The job
        // never stores a URL of a torrent.
        let params = json!({
            "mode": "format",
            "name": format!("{from_host} -> {to_host} (format)"),
            "target": to_host,
            "from": from_host, "to": to_host,
            "from_template": req.from_template.trim(),
            "to_template": req.to_template.trim(),
            "total": walk.torrents, "done": 0, "changed": 0, "skipped": 0, "failed": 0,
            "unmatched": walk.unmatched,
        })
        .to_string();
        match store.create_job(KIND, "", &params, walk.torrents as i64) {
            Ok(id) => id,
            Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        }
    };
    Json(json!({"job_id": id, "torrents": walk.torrents, "unmatched": walk.unmatched}))
        .into_response()
}

/// `GET /api/trackers/rehost/format?host=`: the format of a host's URLs with
/// the passkey as `{passkey}`, to prefill the form; `null` when it cannot be
/// told. Never carries a key.
pub async fn get_format_guess(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let host = crate::api::query_param(&query, "host").unwrap_or_default();
    let host = host.trim().to_string();
    if host.is_empty() {
        return error(StatusCode::BAD_REQUEST, "host required");
    }
    let guess = tokio::task::spawn_blocking(move || {
        let mut found: Option<String> = None;
        for engine in state.engines.engines() {
            engine.manager.for_each(|t| {
                if found.is_some() {
                    return;
                }
                found = t
                    .live_trackers
                    .read()
                    .iter()
                    .flatten()
                    .find(|u| {
                        typhon_engine::rpc::dispatch::tracker_host_str(u)
                            .eq_ignore_ascii_case(&host)
                    })
                    .and_then(|u| crate::trackeredit::guess_format(u));
            });
        }
        found
    })
    .await
    .unwrap_or(None);
    Json(json!({ "format": guess })).into_response()
}

/// `POST /api/trackers/rehost`
pub async fn post_rehost(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let Ok(req) = serde_json::from_str::<Req>(&body) else {
        return error(StatusCode::BAD_REQUEST, "invalid body");
    };
    if !req.from_template.trim().is_empty() || !req.to_template.trim().is_empty() {
        return post_reformat(state, req).await;
    }
    let from = req.from.trim().to_string();
    // The same checks the rewrite makes, before anything is counted or queued.
    if let Err(e) = crate::trackeredit::rehost(&[], &from, &req.to) {
        return error(StatusCode::BAD_REQUEST, &e);
    }
    let (to_host, _) = crate::trackeredit::parse_new_host(&req.to).expect("checked above");
    // A walk of the whole catalogue (about a second at a million torrents):
    // off the async workers.
    let torrents = {
        let (state, from) = (state.clone(), from.clone());
        match tokio::task::spawn_blocking(move || on_host(&state, &from).len()).await {
            Ok(n) => n,
            Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        }
    };
    if req.dry_run {
        return Json(json!({"dry_run": true, "torrents": torrents})).into_response();
    }
    if torrents == 0 {
        return error(StatusCode::BAD_REQUEST, "no torrent announces to that host");
    }
    let busy = || {
        let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
        store.job_in_flight_of_kind(KIND)
    };
    if busy() {
        return error(
            StatusCode::CONFLICT,
            "a tracker move is already running; wait for it or cancel it",
        );
    }
    // Before the job exists: the runner may pick it up at once, and a torrent
    // rewritten before its new host has a passkey or an IP family would
    // announce without them.
    if !copy_settings(&state, &from, &to_host) {
        tracing::warn!("tracker move: the settings of the old host could not all be copied");
    }
    let id = {
        let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
        if store.job_in_flight_of_kind(KIND) {
            return error(
                StatusCode::CONFLICT,
                "a tracker move is already running; wait for it or cancel it",
            );
        }
        let params = json!({
            "name": format!("{from} -> {}", req.to.trim()),
            "target": req.to.trim(),
            "from": from,
            "to": req.to.trim(),
            "total": torrents, "done": 0, "changed": 0, "skipped": 0, "failed": 0,
        })
        .to_string();
        // Created under the same lock as the check: two requests cannot both
        // find the slot free.
        match store.create_job(KIND, "", &params, torrents as i64) {
            Ok(id) => id,
            Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        }
    };
    Json(json!({"job_id": id, "torrents": torrents})).into_response()
}

/// `GET /api/trackers/rehost`: the latest move.
pub async fn get_rehost(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let job = {
        let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
        store.latest_job_of_kind(KIND)
    };
    let Some(job) = job else {
        return Json(json!({"state": "none"})).into_response();
    };
    let p: serde_json::Value = serde_json::from_str(&job.params).unwrap_or_default();
    let num = |k: &str| p.get(k).cloned().unwrap_or(json!(0));
    let mut out = json!({
        "job_id": job.id,
        "state": job.state,
        "from": p.get("from").cloned().unwrap_or(json!("")),
        "to": p.get("to").cloned().unwrap_or(json!("")),
        "total": job.total_bytes,
        "done": job.progress_bytes,
        "changed": num("changed"),
        "skipped": num("skipped"),
        "failed": num("failed"),
        "created_at": job.created_at,
        "updated_at": job.updated_at,
    });
    if !job.error.is_empty() {
        out["error"] = json!(job.error);
    }
    for k in [
        "settings_kept",
        "note",
        "mode",
        "from_template",
        "to_template",
        "unmatched",
    ] {
        if let Some(v) = p.get(k) {
            out[k] = v.clone();
        }
    }
    Json(out).into_response()
}

#[derive(Default, Clone, Copy)]
struct Counts {
    changed: i64,
    skipped: i64,
    failed: i64,
}

fn save_progress(
    state: &AppState,
    job: &crate::store::Job,
    p: &serde_json::Value,
    total: i64,
    done: i64,
    c: Counts,
    extra: &[(&str, serde_json::Value)],
) {
    let mut p = p.clone();
    p["total"] = json!(total);
    p["done"] = json!(done);
    p["changed"] = json!(c.changed);
    p["skipped"] = json!(c.skipped);
    p["failed"] = json!(c.failed);
    for (k, v) in extra {
        p[*k] = v.clone();
    }
    let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
    crate::store::log_store("job_progress", store.job_progress(&job.id, done));
    crate::store::log_store("job_set_total", store.job_set_total(&job.id, total));
    crate::store::log_store(
        "job_set_params",
        store.set_job_params(&job.id, &p.to_string()),
    );
}

/// Run the move. Called by the job runner on a blocking thread.
pub(crate) fn run(state: &AppState, job: &crate::store::Job) -> Result<(), String> {
    let p: serde_json::Value =
        serde_json::from_str(&job.params).map_err(|e| format!("bad job params: {e}"))?;
    let text = |k: &str| p.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let (from, to) = (text("from"), text("to"));
    // A format move keeps the hosts in `from` and `to` for the settings and
    // the report; what it rewrites is told by the templates.
    let format = if text("mode") == "format" {
        Some(crate::trackeredit::Format::parse(
            &text("from_template"),
            &text("to_template"),
        )?)
    } else {
        crate::trackeredit::parse_new_host(&to)?;
        None
    };
    // A resumed job: what an earlier run moved no longer matches, so the
    // total is what it moved plus what is left. Skipped and failed start
    // again from zero on purpose: those torrents are still on `from`, so they
    // are walked (and counted) again.
    let before = p.get("changed").and_then(|v| v.as_i64()).unwrap_or(0);
    let (todo, unmatched) = match &format {
        Some(fmt) => {
            let w = walk_format(state, fmt, true);
            (w.hashes, Some(w.unmatched))
        }
        None => (on_host(state, &from), None),
    };
    let total = before + todo.len() as i64;
    let mut c = Counts {
        changed: before,
        ..Default::default()
    };
    tracing::info!(job = %job.id, torrents = todo.len(), resumed_after = before, "tracker move started");

    let mut last_error = String::new();
    for (n, batch) in todo.chunks(BATCH).enumerate() {
        for ih in batch {
            let hex = typhon_engine::torrent::hex_encode(ih);
            let done = match &format {
                Some(_) => crate::api::reformat_torrent(
                    state,
                    &hex,
                    &text("from_template"),
                    &text("to_template"),
                ),
                None => crate::api::rehost_torrent(state, &hex, &from, &to),
            };
            match done {
                Ok(true) => c.changed += 1,
                // Moved meanwhile by another edit.
                Ok(false) => {}
                // No stored .torrent yet, or gone: left as it was.
                Err((StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND, _)) => c.skipped += 1,
                Err((_, e)) => {
                    c.failed += 1;
                    last_error = e;
                }
            }
        }
        let done = before + ((n + 1) * BATCH).min(todo.len()) as i64;
        save_progress(state, job, &p, total, done, c, &[]);
        let cancelling = {
            let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
            store.job_cancelling(&job.id)
        };
        if cancelling {
            tracing::info!(job = %job.id, changed = c.changed, "tracker move cancelled");
            return Err(format!(
                "cancelled; {} torrents were already moved",
                c.changed
            ));
        }
        if crate::shutdown::stopping() {
            return Err(INTERRUPTED.to_string());
        }
        // Let the API and the engine breathe between two batches.
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    let left = still_served_by(state, &from);
    let (kept, note) = if format.is_some() && from.eq_ignore_ascii_case(&to) {
        // Only the format changed: the host and its settings are the same.
        (
            true,
            "the host did not change; its settings were kept".to_string(),
        )
    } else if left == 0 && c.failed == 0 {
        remove_settings(state, &from);
        (
            false,
            "the settings of the old host were removed".to_string(),
        )
    } else {
        (
            true,
            format!("{left} torrents still announce to the old host; its settings were kept"),
        )
    };
    let mut extra = vec![("settings_kept", json!(kept)), ("note", json!(note))];
    if let Some(n) = unmatched {
        extra.push(("unmatched", json!(n)));
    }
    if !last_error.is_empty() {
        extra.push(("last_error", json!(last_error)));
    }
    save_progress(state, job, &p, total, total, c, &extra);
    tracing::info!(
        job = %job.id, changed = c.changed, skipped = c.skipped, failed = c.failed,
        left, "tracker move finished"
    );
    Ok(())
}
