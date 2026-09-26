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
                // Per FIELD first: the confusing part of these is what the
                // number means, not how to type it.
                "hint": match *name {
                    "link_count" => "counts every hardlink, ours included: two cross-seeds of each other both say 2",
                    "external_links" => "0 means only Hydranos points at these files, so deleting them loses nothing",
                    "freeable_bytes" => "only files nothing else points at; deleting a shared one frees nothing",
                    "data_missing" => "the torrent is seeding data it cannot read",
                    "seeding_time" => "counted since the torrent completed, pauses included",
                    "ratio" => "uploaded divided by downloaded; 0 when nothing was downloaded",
                    _ => match kind {
                        rules::Kind::Duration => "2d, 36h, 90m, or seconds",
                        rules::Kind::Size => "500GB or 500GiB (they differ)",
                        rules::Kind::Percent => "0 to 100",
                        rules::Kind::Bool => "true or false",
                        rules::Kind::Tags => "one tag name",
                        _ => "",
                    },
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
        "seeding_time" => "time spent seeding",
        "free_space" => "free space where it is stored",
        // ⚠️ "name" is how POSIX counts this, and it is the wrong word here:
        // in a torrent client it reads as the torrent's name, so the label
        // suggested a string comparison when the question is about the bytes.
        // "hardlink" is the word for what these actually are.
        "link_count" => "hardlinks to its files",
        "external_links" => "hardlinks from outside Hydranos",
        "freeable_bytes" => "space deleting would really free",
        "data_missing" => "its files are missing from disk",
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

    let links = link_map(&state, rules::needs_link_scan(&w.when));
    let facts = gather_all_with(&state, &links);
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

/// The one scan shared by every workflow pass on this node.
static LINK_CACHE: std::sync::LazyLock<crate::linkindex::Cache> =
    std::sync::LazyLock::new(crate::linkindex::Cache::default);

/// Facts for every engine this node runs.
fn gather_all(state: &AppState) -> Vec<rules::Facts> {
    gather_all_with(state, &std::collections::HashMap::new())
}

/// The scan, or an empty map when no rule asked for it.
///
/// Returned rather than kept private, because the delete guard has to check
/// against the very counting the pass decided on.
fn link_map(
    state: &AppState,
    want: bool,
) -> std::collections::HashMap<String, crate::linkindex::LinkFacts> {
    if !want {
        return std::collections::HashMap::new();
    }
    LINK_CACHE.get_or_build(crate::store::now_secs(), || {
        // ⚠️ The lock lives and dies inside this block. What follows it is
        // minutes of `stat` whose cost depends on the ARC, and holding the
        // store across that would freeze the daemon for the duration.
        let plan = {
            let store = state.store.lock().unwrap();
            rulesrun::plan_scan(&state.engines, &store)
        };
        rulesrun::run_scan(plan)
    })
}

fn gather_all_with(
    state: &AppState,
    links: &std::collections::HashMap<String, crate::linkindex::LinkFacts>,
) -> Vec<rules::Facts> {
    let ids: Vec<String> = state
        .engines
        .engines()
        .iter()
        .map(|e| e.id.clone())
        .collect();
    let store = state.store.lock().unwrap();
    let mut out = Vec::new();
    for id in ids {
        out.extend(rulesrun::gather(&state.engines, &store, &id, links));
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
    // One scan for the whole run: the pass decides on it, and the delete guard
    // re-measures against it.
    let links = link_map(state, rules::needs_link_scan(&w.when));
    let facts = gather_all_with(state, &links);
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
    // Same reasoning for delete, and it matters more: this is the path that
    // folds a removed torrent's lifetime bytes into the durable counters.
    let delete_hook = |engine: &str, hash: &str, with_files: bool| {
        crate::api::remove_one_torrent(state, hash, &[engine.to_string()], engine, with_files)
            .map(|_| ())
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

        match rulesrun::apply(
            &state.engines,
            &state.store,
            w,
            m,
            &hook,
            &delete_hook,
            links.get(&m.info_hash),
        ) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::testing::{body_json, keyed, state_from, TestState};

    const KEY: &str = "0123456789abcdef0123456789abcdef";

    fn st(tag: &str) -> TestState {
        state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"))
    }

    fn wf_json(name: &str) -> String {
        serde_json::json!({
            "id": "",
            "name": name,
            "enabled": true,
            "interval_secs": 3600,
            "cap": 10,
            "when": {"kind": "all", "of": [
                {"kind": "cond", "field": "ratio", "op": "gt", "value": "2"}
            ]},
            "then": [{"type": "pause"}]
        })
        .to_string()
    }

    /// Every route here rides the same gate as the rest of `/api`. A workflow
    /// can stop and delete torrents, so an unauthenticated caller reaching it
    /// is worse than a read leak.
    #[tokio::test]
    async fn the_routes_refuse_a_caller_with_no_key() {
        let s = st("rules-refuse");
        for resp in [
            list(State(s.state.clone()), RawQuery(None), HeaderMap::new()).await,
            fields(State(s.state.clone()), RawQuery(None), HeaderMap::new()).await,
        ] {
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        }
    }

    #[tokio::test]
    async fn a_fresh_install_lists_no_workflows() {
        let s = st("rules-empty");
        let resp = list(State(s.state.clone()), RawQuery(None), keyed(KEY)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body.as_array().map(|a| a.len()), Some(0));
    }

    /// The editor's dropdowns are built from `rules::FIELDS`, the same
    /// constant the compiler validates against: a field cannot be offered in
    /// the editor and rejected on save.
    #[tokio::test]
    async fn every_offered_field_is_a_field_the_engine_knows() {
        let s = st("rules-fields");
        let resp = fields(State(s.state.clone()), RawQuery(None), keyed(KEY)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let offered = body["fields"].as_array().expect("a fields array");
        assert_eq!(offered.len(), rules::FIELDS.len(), "the catalogue is served whole");

        let known: Vec<&str> = rules::FIELDS.iter().map(|(n, _)| *n).collect();
        for f in offered {
            let name = f["name"].as_str().expect("a field has a name");
            assert!(known.contains(&name), "{name} is offered but unknown to the engine");
            assert!(!f["label"].as_str().unwrap_or_default().is_empty(), "{name} has a label");
            assert!(
                !f["operators"].as_array().expect("operators").is_empty(),
                "{name} offers at least one operator"
            );
            assert_eq!(
                f["operators"].as_array().unwrap().len(),
                f["operators_labelled"].as_array().expect("labelled operators").len(),
                "{name}: every operator has a label"
            );
        }
    }

    /// A field with no label of its own still reads as the engine spells it,
    /// rather than coming back blank.
    #[test]
    fn an_unlabelled_field_falls_back_to_its_own_name() {
        assert_eq!(field_label("no_such_field_yet"), "no_such_field_yet");
        assert_eq!(field_label("tracker_host"), "tracker");
        assert_eq!(field_label("completed_age"), "time since completed");
    }

    /// ⭐ "greater than" is right for a ratio and WRONG for an age:
    /// `added_age > 2d` means added MORE than two days ago. Reading it as
    /// "greater" is how a rule gets written backwards.
    #[test]
    fn a_duration_comparison_reads_as_age_not_as_magnitude() {
        assert_eq!(op_label("gt", rules::Kind::Duration), "is older than");
        assert_eq!(op_label("lt", rules::Kind::Duration), "is newer than");
        assert_eq!(op_label("gt", rules::Kind::Number), "is more than");
        assert_eq!(op_label("lt", rules::Kind::Number), "is less than");
    }

    #[test]
    fn an_unknown_operator_reads_as_itself() {
        assert_eq!(op_label("no_such_op", rules::Kind::Text), "no_such_op");
    }

    #[test]
    fn a_workflow_needs_a_name() {
        let body = serde_json::json!({
            "id": "", "name": "   ",
            "when": {"kind": "all", "of": []},
            "then": []
        })
        .to_string();
        let err = parse(&body).expect_err("a nameless workflow is refused");
        assert!(err.contains("name"), "the reason names the problem: {err}");
    }

    #[test]
    fn a_workflow_with_no_id_is_given_one() {
        let w = parse(&wf_json("ratio reached")).expect("a valid workflow");
        assert!(!w.id.trim().is_empty(), "an id was generated");
        assert!(w.id.starts_with("wf"));
    }

    /// An interval below the floor would have the runner scan the whole
    /// library far more often than the work it does could ever justify.
    #[test]
    fn an_interval_below_the_floor_is_raised_to_it() {
        let mut v: serde_json::Value = serde_json::from_str(&wf_json("too eager")).unwrap();
        v["interval_secs"] = serde_json::json!(1);
        let w = parse(&v.to_string()).expect("a valid workflow");
        assert_eq!(w.interval_secs, rules::MIN_INTERVAL_SECS);
    }

    /// A cap of zero is not "act on nothing"; it is the unset value, and the
    /// default is what bounds a first run from touching the whole library.
    #[test]
    fn a_cap_of_zero_takes_the_default_rather_than_acting_on_nothing() {
        let mut v: serde_json::Value = serde_json::from_str(&wf_json("uncapped")).unwrap();
        v["cap"] = serde_json::json!(0);
        let w = parse(&v.to_string()).expect("a valid workflow");
        assert_eq!(w.cap, rules::DEFAULT_CAP);
        assert!(w.cap > 0);
    }

    /// ⭐ Compiled BEFORE it is stored. A rule that cannot compile would fail
    /// silently every interval forever, and the operator would find out by
    /// noticing that nothing happened.
    #[test]
    fn a_rule_that_cannot_compile_is_refused_at_save_time() {
        let mut v: serde_json::Value = serde_json::from_str(&wf_json("bad field")).unwrap();
        v["when"] = serde_json::json!({"kind": "all", "of": [
            {"kind": "cond", "field": "no_such_field", "op": "eq", "value": "x"}
        ]});
        assert!(parse(&v.to_string()).is_err(), "an unknown field is refused");

        let mut v2: serde_json::Value = serde_json::from_str(&wf_json("bad regex")).unwrap();
        v2["when"] = serde_json::json!({"kind": "all", "of": [
            {"kind": "cond", "field": "name", "op": "matches", "value": "([unclosed"}
        ]});
        assert!(parse(&v2.to_string()).is_err(), "an uncompilable regex is refused");
    }

    #[test]
    fn a_body_that_is_not_json_is_refused_without_panicking() {
        assert!(parse("not json").is_err());
        assert!(parse("").is_err());
    }

    /// A workflow that round-trips through the store comes back with the same
    /// clauses: `to_json` re-splits the stored body, and losing `when`/`then`
    /// there would leave the editor showing an empty rule that still runs.
    #[tokio::test]
    async fn a_saved_workflow_comes_back_with_its_clauses() {
        let s = st("rules-roundtrip");
        let resp = save(
            State(s.state.clone()),
            RawQuery(None),
            keyed(KEY),
            wf_json("ratio reached"),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK, "the save was accepted");

        let listed = body_json(list(State(s.state.clone()), RawQuery(None), keyed(KEY)).await).await;
        let rows = listed.as_array().expect("an array");
        assert_eq!(rows.len(), 1, "exactly the workflow that was saved");
        assert_eq!(rows[0]["name"], serde_json::json!("ratio reached"));
        assert!(!rows[0]["when"].is_null(), "the when clause survived the round trip");
        assert!(!rows[0]["then"].is_null(), "the then clause survived the round trip");
        assert_eq!(rows[0]["enabled"], serde_json::json!(true));
    }

    #[tokio::test]
    async fn saving_an_invalid_workflow_is_a_bad_request_not_a_panic() {
        let s = st("rules-badsave");
        let resp = save(State(s.state.clone()), RawQuery(None), keyed(KEY), "{".into()).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}

#[cfg(test)]
mod handler_tests {
    use super::*;
    use crate::api::testing::{body_json, keyed, state_from, TestState};

    const KEY: &str = "0123456789abcdef0123456789abcdef";

    fn st(tag: &str) -> TestState {
        state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"))
    }

    fn wf(name: &str) -> String {
        serde_json::json!({
            "id": "", "name": name, "enabled": true, "interval_secs": 3600, "cap": 10,
            "when": {"kind": "all", "of": [
                {"kind": "cond", "field": "ratio", "op": "gt", "value": "2"}
            ]},
            "then": [{"type": "pause"}]
        })
        .to_string()
    }

    /// Every route here can stop and delete torrents. None of them may answer
    /// an unauthenticated caller.
    #[tokio::test]
    async fn every_workflow_route_refuses_a_caller_with_no_key() {
        let s = st("rules2-auth");
        let no_key = HeaderMap::new();
        assert_eq!(
            remove(State(s.state.clone()), Path("wf1".into()), RawQuery(None), no_key.clone())
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            run_now(State(s.state.clone()), Path("wf1".into()), RawQuery(None), no_key.clone())
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            preview(State(s.state.clone()), RawQuery(None), no_key.clone(), wf("x")).await.status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            activity(State(s.state.clone()), RawQuery(None), no_key).await.status(),
            StatusCode::UNAUTHORIZED
        );
    }

    /// ⭐⭐ A preview must not DO anything. It is the one way to find out what
    /// a rule would touch before trusting it, and a preview with side effects
    /// is worse than no preview at all.
    #[tokio::test]
    async fn a_preview_reports_without_storing_the_workflow() {
        let s = st("rules2-preview");
        let resp = preview(State(s.state.clone()), RawQuery(None), keyed(KEY), wf("dry run")).await;
        assert!(resp.status().is_success(), "got {:?}", resp.status());
        let _ = body_json(resp).await;

        let listed = body_json(list(State(s.state.clone()), RawQuery(None), keyed(KEY)).await).await;
        assert_eq!(
            listed.as_array().map(|a| a.len()),
            Some(0),
            "a preview stores nothing: {listed}"
        );
    }

    /// A preview of a rule that cannot compile is a 400, not a stored rule
    /// and not a panic.
    #[tokio::test]
    async fn a_preview_of_an_uncompilable_rule_is_refused() {
        let s = st("rules2-badpreview");
        let mut v: serde_json::Value = serde_json::from_str(&wf("bad")).unwrap();
        v["when"] = serde_json::json!({"kind": "all", "of": [
            {"kind": "cond", "field": "no_such_field", "op": "eq", "value": "x"}
        ]});
        let resp =
            preview(State(s.state.clone()), RawQuery(None), keyed(KEY), v.to_string()).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// Running a workflow that does not exist is a refusal, never a silent
    /// "ok" for a pass that never happened.
    #[tokio::test]
    async fn running_a_workflow_that_does_not_exist_is_refused() {
        let s = st("rules2-runmissing");
        let resp = run_now(
            State(s.state.clone()),
            Path("no-such-workflow".into()),
            RawQuery(None),
            keyed(KEY),
        )
        .await;
        assert!(
            !resp.status().is_success(),
            "a workflow that is not here cannot have run: {:?}",
            resp.status()
        );
    }

    /// Deleting one that does not exist says so rather than reporting success.
    #[tokio::test]
    async fn deleting_a_workflow_that_does_not_exist_is_reported() {
        let s = st("rules2-delmissing");
        let resp = remove(
            State(s.state.clone()),
            Path("no-such-workflow".into()),
            RawQuery(None),
            keyed(KEY),
        )
        .await;
        assert!(!resp.status().is_success(), "got {:?}", resp.status());
    }

    /// The full life of a rule: saved, listed, run on an empty library, and
    /// removed.
    #[tokio::test]
    async fn a_workflow_can_be_saved_run_and_removed() {
        let s = st("rules2-life");
        let saved = save(State(s.state.clone()), RawQuery(None), keyed(KEY), wf("ratio")).await;
        assert!(saved.status().is_success());
        let body = body_json(saved).await;
        let id = body["id"].as_str().expect("the saved workflow has an id").to_string();

        let ran = run_now(
            State(s.state.clone()),
            Path(id.clone()),
            RawQuery(None),
            keyed(KEY),
        )
        .await;
        assert!(ran.status().is_success(), "a pass on an empty library still runs: {:?}", ran.status());

        let gone = remove(State(s.state.clone()), Path(id), RawQuery(None), keyed(KEY)).await;
        assert!(gone.status().is_success());

        let listed = body_json(list(State(s.state.clone()), RawQuery(None), keyed(KEY)).await).await;
        assert_eq!(listed.as_array().map(|a| a.len()), Some(0));
    }

    /// The activity trail answers on a fresh install -- empty, not absent.
    #[tokio::test]
    async fn the_activity_trail_is_empty_on_a_fresh_install() {
        let s = st("rules2-activity");
        let body =
            body_json(activity(State(s.state.clone()), RawQuery(None), keyed(KEY)).await).await;
        let empty = body.as_array().map(|a| a.is_empty()).unwrap_or(false)
            || body.get("activity").and_then(|a| a.as_array()).map(|a| a.is_empty()).unwrap_or(false);
        assert!(empty, "got {body}");
    }

    /// ⭐ A new workflow is OFF. It is the only default that cannot cause
    /// damage while its author is still typing.
    #[test]
    fn a_workflow_with_no_enabled_flag_is_off() {
        let body = serde_json::json!({
            "id": "", "name": "half typed",
            "when": {"kind": "all", "of": [
                {"kind": "cond", "field": "ratio", "op": "gt", "value": "2"}
            ]},
            "then": [{"type": "pause"}]
        })
        .to_string();
        let w = parse(&body).expect("a complete workflow is valid");
        assert!(!w.enabled, "a rule nobody switched on must not run");
    }

    /// ⭐⭐ A workflow with NO condition is refused: an empty `when` matches
    /// every torrent in the library. Paired with a `delete` action that is the
    /// whole seedbox, from a rule its author had not finished typing.
    #[test]
    fn a_workflow_with_no_condition_is_refused() {
        let body = serde_json::json!({
            "id": "", "name": "matches everything",
            "when": {"kind": "all", "of": []},
            "then": [{"type": "pause"}]
        })
        .to_string();
        let err = parse(&body).expect_err("an unconditioned workflow is refused");
        assert!(err.contains("condition"), "the reason names the problem: {err}");
    }

    /// ⭐ A workflow with no action at all is refused rather than stored: it
    /// would run on its interval forever and do nothing, and the operator
    /// would find out by noticing that nothing happened.
    #[test]
    fn a_workflow_with_no_action_is_refused() {
        let body = serde_json::json!({
            "id": "", "name": "does nothing",
            "when": {"kind": "all", "of": []},
            "then": []
        })
        .to_string();
        let err = parse(&body).expect_err("an actionless workflow is refused");
        assert!(err.contains("action"), "the reason names the problem: {err}");
    }
}
