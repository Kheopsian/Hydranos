//! The workflow routes, and the task that runs them.
//!
//! Kept out of api.rs, which is already nine thousand lines. Everything here
//! goes through the same `authorised` gate as the rest of `/api`.

use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use std::sync::Arc;

use crate::api::AppState;
use crate::rules::{self, Workflow};
use crate::rulesrun;

fn refuse() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({"error": "Invalid or missing API key"})),
    )
        .into_response()
}

fn bad(msg: impl std::fmt::Display) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({"error": msg.to_string()})),
    )
        .into_response()
}

/// Serialise a stored workflow back out, body included.
fn to_json(s: &crate::store::StoredWorkflow) -> serde_json::Value {
    let body: serde_json::Value =
        serde_json::from_str(&s.body).unwrap_or(serde_json::Value::Null);
    serde_json::json!({
        "id": s.id,
        "name": s.name,
        "enabled": s.enabled,
        "position": s.position,
        "interval_secs": s.interval_secs,
        "last_run": s.last_run,
        "when": body.get("when").cloned().unwrap_or(serde_json::Value::Null),
        "then": body.get("then").cloned().unwrap_or(serde_json::Value::Null),
        "cap": body.get("cap").cloned().unwrap_or(serde_json::Value::Null),
    })
}

pub async fn list(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    if !crate::api::authorised(&state, &headers, &query) {
        return refuse();
    }
    let rows = {
        let store = state.store.lock().unwrap();
        store.workflows().unwrap_or_default()
    };
    Json(rows.iter().map(to_json).collect::<Vec<_>>()).into_response()
}

/// The field catalogue the editor builds its dropdowns from.
///
/// Served from `rules::FIELDS`, the same constant the compiler validates
/// against. A field cannot appear in the editor and be rejected on save, or
/// exist in the engine and be missing from the editor.
pub async fn fields(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    if !crate::api::authorised(&state, &headers, &query) {
        return refuse();
    }
    let out: Vec<serde_json::Value> = rules::FIELDS
        .iter()
        .map(|(name, kind)| {
            let (kind_name, ops): (&str, &[&str]) = match kind {
                rules::Kind::Text => (
                    "text",
                    &["eq", "ne", "contains", "not_contains", "starts_with", "ends_with", "matches"],
                ),
                rules::Kind::Number => ("number", &["eq", "ne", "gt", "ge", "lt", "le"]),
                rules::Kind::Duration => ("duration", &["gt", "ge", "lt", "le", "eq", "ne"]),
                rules::Kind::Size => ("size", &["gt", "ge", "lt", "le", "eq", "ne"]),
                rules::Kind::Percent => ("percent", &["eq", "ne", "gt", "ge", "lt", "le"]),
                rules::Kind::Bool => ("bool", &["eq", "ne"]),
                rules::Kind::Tags => ("tags", &["has_tag", "not_has_tag"]),
            };
            // The editor shows these; the engine is sent `name`. "not_contains"
            // and "ne" are how the matcher spells it, not how anyone reads it.
            let ops_labelled: Vec<serde_json::Value> = ops
                .iter()
                .map(|o| serde_json::json!({"op": o, "label": op_label(o, *kind)}))
                .collect();
            serde_json::json!({
                "name": name,
                "label": field_label(name),
                "kind": kind_name,
                "operators": ops,
                "operators_labelled": ops_labelled,
                // Where the editor should get the list of possible values, when
                // there is one. Typing a category by hand is how a rule ends up
                // pointing at a category that does not exist, matching nothing,
                // and looking perfectly correct while it does it.
                "choices_from": match *name {
                    "category" => "categories",
                    "tags" => "tags",
                    "tracker_host" => "trackers",
                    "engine" => "engines",
                    _ => "",
                },
                "choices": match (*name, kind) {
                    ("state", _) => serde_json::json!(rules::STATES),
                    (_, rules::Kind::Bool) => serde_json::json!(["true", "false"]),
                    _ => serde_json::Value::Null,
                },
                // What the editor should show under the value box. A duration
                // typed as "2 days" is the commonest way to get a rule that
                // silently never fires.
                "hint": match kind {
                    rules::Kind::Duration => "2d, 36h, 90m, or seconds",
                    rules::Kind::Size => "500GB or 500GiB (they differ)",
                    rules::Kind::Percent => "0 to 100",
                    rules::Kind::Bool => "true or false",
                    rules::Kind::Tags => "one tag name",
                    _ => "",
                },
            })
        })
        .collect();
    Json(serde_json::json!({"fields": out})).into_response()
}

/// A field name as a person reads it.
///
/// The engine's name is the wire format and stays untouched: renaming
/// `completed_age` would break every stored rule. This is the other end.
fn field_label(name: &str) -> String {
    match name {
        "name" => "torrent name",
        "info_hash" => "info hash",
        "category" => "category",
        "tags" => "tags",
        "state" => "state",
        "engine" => "engine",
        "save_path" => "save path",
        "tracker_host" => "tracker",
        "tracker_error" => "has a tracker error",
        "tracker_error_msg" => "tracker error message",
        "torrent_error" => "has a torrent error",
        "user_paused" => "stopped by hand",
        "multi_file" => "has several files",
        "progress" => "progress",
        "ratio" => "ratio",
        "total_size" => "size",
        "total_uploaded" => "uploaded",
        "total_downloaded" => "downloaded",
        "upload_rate" => "upload rate",
        "download_rate" => "download rate",
        "num_peers" => "connected peers",
        "num_seeds" => "connected seeds",
        "swarm_seeds" => "seeds in swarm",
        "swarm_leechers" => "leechers in swarm",
        "added_age" => "time since added",
        "completed_age" => "time since completed",
        // A field added to FIELDS without a label still works; it
        // just reads as the engine spells it.
        other => return other.to_string(),
    }
    .to_string()
}

/// An operator as a person reads it, which depends on what it compares.
///
/// "greater than" is right for a ratio and wrong for an age: `added_age > 2d`
/// means added MORE than two days ago, and reading it as "greater" is how a
/// rule gets written backwards.
fn op_label(op: &str, kind: rules::Kind) -> String {
    match (op, kind) {
        ("eq", rules::Kind::Bool) => "is",
        ("ne", rules::Kind::Bool) => "is not",
        ("eq", _) => "is",
        ("ne", _) => "is not",
        ("contains", _) => "contains",
        ("not_contains", _) => "does not contain",
        ("starts_with", _) => "starts with",
        ("ends_with", _) => "ends with",
        ("matches", _) => "matches regex",
        ("has_tag", _) => "has tag",
        ("not_has_tag", _) => "does not have tag",
        ("gt", rules::Kind::Duration) => "is older than",
        ("ge", rules::Kind::Duration) => "is at least",
        ("lt", rules::Kind::Duration) => "is newer than",
        ("le", rules::Kind::Duration) => "is at most",
        ("gt", _) => "is more than",
        ("ge", _) => "is at least",
        ("lt", _) => "is less than",
        ("le", _) => "is at most",
        other => return other.0.to_string(),
    }
    .to_string()
}

/// Parse and validate a workflow from a request body.
fn parse(body: &str) -> Result<Workflow, String> {
    let mut w: Workflow = serde_json::from_str(body).map_err(|e| e.to_string())?;
    if w.name.trim().is_empty() {
        return Err("a workflow needs a name".into());
    }
    if w.id.trim().is_empty() {
        // Time-based and unique enough for a handful of rules; the store's
        // primary key is what actually enforces it.
        w.id = format!("wf{}", crate::store::now_secs());
    }
    w.interval_secs = w.interval_secs.max(rules::MIN_INTERVAL_SECS);
    if w.cap == 0 {
        w.cap = rules::DEFAULT_CAP;
    }
    // Compiled before it is stored. A rule that cannot compile is a rule that
    // would fail silently every interval forever, and the operator would find
    // out by noticing nothing happened.
    rules::compile_workflow(&w).map_err(|e| e.to_string())?;
    Ok(w)
}

pub async fn save(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    if !crate::api::authorised(&state, &headers, &query) {
        return refuse();
    }
    let w = match parse(&body) {
        Ok(w) => w,
        Err(e) => return bad(e),
    };
    let stored = crate::store::StoredWorkflow {
        id: w.id.clone(),
        name: w.name.clone(),
        body: serde_json::to_string(&w).unwrap_or_default(),
        enabled: w.enabled,
        position: w.position,
        interval_secs: w.interval_secs,
        last_run: 0,
    };
    {
        let store = state.store.lock().unwrap();
        if let Err(e) = store.put_workflow(&stored) {
            return bad(e);
        }
    }
    Json(to_json(&stored)).into_response()
}

pub async fn remove(
    State(state): State<AppState>,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    if !crate::api::authorised(&state, &headers, &query) {
        return refuse();
    }
    let gone = {
        let store = state.store.lock().unwrap();
        store.delete_workflow(&id).unwrap_or(false)
    };
    if !gone {
        // An honest 404, not an ok that deleted nothing.
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "no such workflow"})),
        )
            .into_response();
    }
    Json(serde_json::json!({"status": "ok"})).into_response()
}

/// What this workflow WOULD do, right now, changing nothing.
///
/// Takes a whole workflow in the body rather than an id, so an unsaved draft
/// can be previewed. Runs `rulesrun::evaluate` -- the same function the pass
/// runs -- because a preview computed differently is a preview of something
/// else.
pub async fn preview(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    if !crate::api::authorised(&state, &headers, &query) {
        return refuse();
    }
    let w = match parse(&body) {
        Ok(w) => w,
        Err(e) => return bad(e),
    };

    let facts = gather_all(&state);
    let (matches, report) = match rulesrun::evaluate(&w, &facts) {
        Ok(x) => x,
        Err(e) => return bad(e),
    };
    let sample: Vec<serde_json::Value> = matches
        .iter()
        .take(200)
        .map(|m| {
            serde_json::json!({
                "info_hash": m.info_hash,
                "name": m.name,
                "engine": m.engine,
                "total_size": m.total_size,
            })
        })
        .collect();
    Json(serde_json::json!({
        "matched": report.matched,
        "would_apply": matches.len(),
        "skipped": report.skipped,
        "capped": report.capped,
        "freed_bytes": report.freed_bytes,
        "sample": sample,
    }))
    .into_response()
}

/// Facts for every engine this node runs.
fn gather_all(state: &AppState) -> Vec<rules::Facts> {
    let ids: Vec<String> = state
        .engines
        .engines()
        .iter()
        .map(|e| e.id.clone())
        .collect();
    let store = state.store.lock().unwrap();
    let mut out = Vec::new();
    for id in ids {
        out.extend(rulesrun::gather(&state.engines, &store, &id));
    }
    out
}

/// Run one workflow now. `?dry=1` decides, and it is not the default.
pub async fn run_now(
    State(state): State<AppState>,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    if !crate::api::authorised(&state, &headers, &query) {
        return refuse();
    }
    let dry = query.contains("dry=1");
    let stored = {
        let store = state.store.lock().unwrap();
        store.workflow(&id).ok().flatten()
    };
    let Some(stored) = stored else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "no such workflow"})),
        )
            .into_response();
    };
    let w: Workflow = match serde_json::from_str(&stored.body) {
        Ok(w) => w,
        Err(e) => return bad(e),
    };
    let report = run_one(&state, &w, dry);
    Json(report).into_response()
}

/// One pass of one workflow. The single path both the timer and the button use.
pub fn run_one(state: &AppState, w: &Workflow, dry: bool) -> serde_json::Value {
    let facts = gather_all(state);
    let (matches, mut report) = match rulesrun::evaluate(w, &facts) {
        Ok(x) => x,
        Err(e) => {
            return serde_json::json!({"error": e});
        }
    };

    if dry {
        if matches.is_empty() {
            let store = state.store.lock().unwrap();
            let _ = store.log_workflow_activity(&crate::store::ActivityEntry {
                at: crate::store::now_secs(),
                workflow_id: w.id.clone(),
                workflow_name: w.name.clone(),
                action: "dry_run".into(),
                outcome: "dry_run_no_match".into(),
                ..Default::default()
            });
        }
        return serde_json::json!({
            "dry_run": true,
            "matched": report.matched,
            "would_apply": matches.len(),
            "skipped": report.skipped,
            "capped": report.capped,
        });
    }

    // The pause hook takes the same route a human click does, so a workflow
    // cannot pause more or less thoroughly than a person can.
    let hook = |engine: &str, hash: &str, paused: bool| {
        crate::api::apply_pause_to_engine(state, engine, hash, paused);
    };

    for m in &matches {
        let action_name = m
            .actions
            .iter()
            .map(|a| match a {
                rules::Action::Pause => "pause",
                rules::Action::Resume => "resume",
                rules::Action::SetCategory { .. } => "category",
                rules::Action::AddTags { .. } => "add_tags",
                rules::Action::RemoveTags { .. } => "remove_tags",
                rules::Action::Delete { .. } => "delete",
            })
            .collect::<Vec<_>>()
            .join("+");

        match rulesrun::apply(&state.engines, &state.store, w, m, &hook) {
            Ok(()) => {
                report.applied += 1;
                let store = state.store.lock().unwrap();
                rulesrun::log(&store, w, m, &action_name, "applied", "");
            }
            Err(e) => {
                report.failed += 1;
                let store = state.store.lock().unwrap();
                rulesrun::log(&store, w, m, &action_name, "failed", &e);
            }
        }
    }

    {
        let store = state.store.lock().unwrap();
        let _ = store.mark_workflow_run(&w.id, crate::store::now_secs());
    }
    serde_json::json!({
        "matched": report.matched,
        "applied": report.applied,
        "skipped": report.skipped,
        "failed": report.failed,
        "capped": report.capped,
    })
}

pub async fn activity(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    if !crate::api::authorised(&state, &headers, &query) {
        return refuse();
    }
    let rows = {
        let store = state.store.lock().unwrap();
        store.workflow_activity(500).unwrap_or_default()
    };
    let out: Vec<serde_json::Value> = rows
        .iter()
        .map(|e| {
            serde_json::json!({
                "at": e.at,
                "workflow_name": e.workflow_name,
                "info_hash": e.info_hash,
                "torrent_name": e.torrent_name,
                "action": e.action,
                "outcome": e.outcome,
                "detail": e.detail,
            })
        })
        .collect();
    Json(serde_json::json!({"activity": out})).into_response()
}

/// The timer. One tick a minute; each workflow fires on its own interval.
///
/// A minute rather than qui's twenty seconds: the floor for a rule is sixty
/// seconds anyway, so a faster tick would only wake up to decide it has
/// nothing to do -- on a catalogue where deciding means a store scan.
pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        // Long enough for the catalogue to be loaded. Firing a workflow against
        // a half-loaded engine would let a "no seeders" rule match everything.
        tokio::time::sleep(std::time::Duration::from_secs(120)).await;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            let now = crate::store::now_secs();
            let due: Vec<crate::store::StoredWorkflow> = {
                let store = state.store.lock().unwrap();
                let _ = store.prune_workflow_activity(now - 7 * 86400);
                store
                    .workflows()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|w| rulesrun::is_due(w, now))
                    .collect()
            };
            for stored in due {
                let Ok(w) = serde_json::from_str::<Workflow>(&stored.body) else {
                    tracing::warn!(workflow = %stored.name, "workflow body will not parse, skipped");
                    continue;
                };
                let report = run_one(&state, &w, false);
                // Silence when nothing happened: a scheduled rule that matches
                // nothing is the normal case and must not fill the log.
                if report.get("applied").and_then(|v| v.as_u64()).unwrap_or(0) > 0
                    || report.get("failed").and_then(|v| v.as_u64()).unwrap_or(0) > 0
                {
                    tracing::info!(workflow = %w.name, report = %report, "workflow ran");
                }
            }
        }
    });
}

/// Mount the workflow routes.
pub fn routes() -> axum::Router<AppState> {
    use axum::routing::{delete, get, post};
    axum::Router::new()
        .route("/api/workflows", get(list).post(save))
        .route("/api/workflows/fields", get(fields))
        .route("/api/workflows/preview", post(preview))
        .route("/api/workflows/activity", get(activity))
        .route("/api/workflows/:id/run", post(run_now))
        .route("/api/workflows/:id", delete(remove))
}

/// Kept so the module owns its Arc import even when the runner changes shape.
pub type Shared = Arc<std::sync::Mutex<crate::store::Store>>;
