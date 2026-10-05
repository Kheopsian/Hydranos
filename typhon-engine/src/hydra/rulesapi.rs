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
        // Absent from a body saved before events existed: those all ran on
        // a timer, and say so.
        "trigger": body.get("trigger").cloned().unwrap_or_else(|| serde_json::json!("schedule")),
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
                    "seeding_time" => "time actually spent seeding since the torrent completed; stopped time is not counted",
                    "ratio" => "uploaded divided by downloaded, or by the data held when under 1% of it was downloaded (a cross-seed); 0 when nothing is held or downloaded",
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
        "num_seeds" => "seeds the tracker reports",
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

/// A fresh workflow id.
///
/// ⚠️ It used to be `wf<seconds>`, and the store's primary key does not
/// "enforce" anything on an upsert: two workflows created in the same second
/// got the same id, and the second silently REPLACED the first. A script
/// creating a few rules in a row kept only the last. Nanoseconds plus a
/// process counter cannot meet twice.
fn new_workflow_id() -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("wf{nanos:x}{n:x}")
}

/// Parse and validate a workflow from a request body.
fn parse(body: &str) -> Result<Workflow, String> {
    let mut w: Workflow = serde_json::from_str(body).map_err(|e| e.to_string())?;
    if w.name.trim().is_empty() {
        return Err("a workflow needs a name".into());
    }
    if w.id.trim().is_empty() {
        w.id = new_workflow_id();
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

    if w.trigger.is_event() {
        let run = move || match w.trigger {
            rules::Trigger::Added => preview_added(&state, &w),
            _ => preview_completion(&state, &w),
        };
        return match tokio::task::spawn_blocking(run).await {
            Ok(Ok(v)) => Json(v).into_response(),
            Ok(Err(e)) => bad(e),
            Err(e) => bad(e),
        };
    }

    let decided = tokio::task::spawn_blocking(move || decide(&state, &w)).await;
    let Decision { matches, report, rechecked, no_longer, .. } = match decided {
        Ok(Ok(d)) => d,
        Ok(Err(e)) => return bad(e),
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
        "rechecked": rechecked,
        "no_longer_true": no_longer,
        "sample": sample,
    }))
    .into_response()
}

/// What a completion workflow would do to the downloads under way now.
///
/// The catalogue as it stands is the wrong sample: a torrent that finished
/// last month never fires the event again. The torrents that WILL fire it are
/// the ones still downloading, so the preview is run on those -- the filter
/// and the convergence check exactly as the event will run them, minus the
/// cap, which an event of one torrent never reaches.
fn preview_completion(state: &AppState, w: &Workflow) -> Result<serde_json::Value, String> {
    let mut facts = gather_all(state, rulesrun::Want::of(w));
    facts.retain(|f| f.progress < 100.0);
    let unbounded = Workflow { cap: usize::MAX, ..w.clone() };
    let (matches, report) = rulesrun::evaluate(&unbounded, &facts)?;
    let sample: Vec<serde_json::Value> = matches
        .iter()
        .take(200)
        .map(|m| serde_json::json!({"info_hash": m.info_hash, "name": m.name, "engine": m.engine, "total_size": m.total_size}))
        .collect();
    Ok(serde_json::json!({
        "trigger": "completed",
        "downloading": facts.len(),
        "matched": report.matched,
        "would_apply": matches.len(),
        "skipped": report.skipped,
        "capped": false,
        "freed_bytes": report.freed_bytes,
        "rechecked": 0,
        "no_longer_true": 0,
        "sample": sample,
    }))
}

/// Preview an "on add" workflow on the torrents added in the last day.
///
/// The torrents that will fire it do not exist yet; the latest arrivals are
/// the closest stand-in for what keeps arriving, judged by the filter and the
/// convergence check exactly as the event will judge each new one.
fn preview_added(state: &AppState, w: &Workflow) -> Result<serde_json::Value, String> {
    let mut facts = gather_all(state, rulesrun::Want::of(w));
    facts.retain(|f| f.added_age < RECENT_ADD_SECS);
    let unbounded = Workflow { cap: usize::MAX, ..w.clone() };
    let (matches, report) = rulesrun::evaluate(&unbounded, &facts)?;
    let sample: Vec<serde_json::Value> = matches
        .iter()
        .take(200)
        .map(|m| serde_json::json!({"info_hash": m.info_hash, "name": m.name, "engine": m.engine, "total_size": m.total_size}))
        .collect();
    Ok(serde_json::json!({
        "trigger": "added",
        "recent": facts.len(),
        "matched": report.matched,
        "would_apply": matches.len(),
        "skipped": report.skipped,
        "capped": false,
        "freed_bytes": report.freed_bytes,
        "rechecked": 0,
        "no_longer_true": 0,
        "sample": sample,
    }))
}

/// The window `preview_added` samples: a day of arrivals.
const RECENT_ADD_SECS: f64 = 86400.0;

/// Facts for every engine this node runs.
fn gather_all(state: &AppState, want: rulesrun::Want) -> Vec<rules::Facts> {
    gather_all_with(state, &std::collections::HashMap::new(), want)
}

/// What one pass decided, and the link facts it decided on.
struct Decision {
    matches: Vec<rulesrun::Match>,
    report: rulesrun::PassReport,
    /// Returned rather than kept private: the delete guard has to check
    /// against the very measurement the pass decided on.
    links: std::collections::HashMap<String, crate::linkindex::LinkFacts>,
    /// Candidates measured again before acting, and those of them the fresh
    /// measurement no longer justified.
    rechecked: usize,
    no_longer: usize,
}

/// Decide what a workflow would do. The single path the timer, the run
/// button and the preview all take.
///
/// ⭐ A rule on hardlinks reads the LINK INDEX, which the background scanner
/// keeps (`linkscan`), and never stats the catalogue itself: at a million
/// torrents that was millions of `statx` on the request that asked, and a
/// dry-run that answered after the person had given up. Then the torrents it
/// would act on -- a few hundred at most, `cap` bounds them -- are measured
/// again NOW, written back, and the rule is evaluated once more on that. A
/// candidate whose files changed since the index saw them drops out here,
/// before anything is done to it.
fn decide(state: &AppState, w: &Workflow) -> Result<Decision, String> {
    if !rules::needs_link_scan(&w.when) {
        let facts = gather_all(state, rulesrun::Want::of(w));
        let (matches, report) = rulesrun::evaluate(w, &facts)?;
        return Ok(Decision {
            matches,
            report,
            links: Default::default(),
            rechecked: 0,
            no_longer: 0,
        });
    }

    // On the read connection: a pass must not hold the writer while it reads
    // a million rows.
    let (stored, rows) = {
        let store = state.store.read().map_err(|_| "store lock")?;
        let stored = rulesrun::stored_facts(&state.engines, &store);
        let rows = store.link_index_stats().map_err(|e| e.to_string())?;
        (stored, rows)
    };
    let cat = rulesrun::catalogue_from(&state.engines, &stored);
    drop(stored);
    // The same computation the list's "Hardlinks" column is built from, cf
    // `rulesrun::links_from_store`.
    let rulesrun::StoredLinks { mut entries, origin, mut measured_at, links } =
        rulesrun::links_from_store(&cat, &rows);
    drop(rows);
    let mut facts = gather_all_with(state, &links, rulesrun::Want::of(w));
    let (first, _) = rulesrun::evaluate(w, &facts)?;

    let want: std::collections::HashSet<String> =
        first.iter().map(|m| m.info_hash.clone()).collect();
    let picked: Vec<usize> = (0..entries.len()).filter(|&i| want.contains(&entries[i].0)).collect();
    let plan = picked
        .iter()
        .map(|&i| (entries[i].0.clone(), entries[i].1.iter().map(|(p, _)| p.clone()).collect()))
        .collect();
    let fresh = rulesrun::stat_plan_with(plan, rulesrun::scan_threads());
    let now = crate::store::now_secs();
    let mut rows = Vec::with_capacity(picked.len());
    for (&i, e) in picked.iter().zip(fresh) {
        let stats: Vec<_> = e.1.iter().map(|(_, st)| *st).collect();
        rows.push(rulesrun::link_row(&cat[origin[i]], &stats, now));
        entries[i] = e;
        measured_at[i] = now;
    }
    if !rows.is_empty() {
        let store = state.store.lock().map_err(|_| "store lock")?;
        store.put_link_rows(&rows).map_err(|e| e.to_string())?;
    }

    // Only the candidates' facts change: the others were not re-measured, and
    // letting them in now would act on what nobody just looked at.
    let links = crate::linkindex::compute(&entries);
    // What this pass decided on is the freshest whole-catalogue answer there
    // is: the list shows it from now on, so a torrent the pass just tagged
    // "noHL" does not sit beside a column still reading the old count.
    state
        .engines
        .publish_link_summary(rulesrun::link_summary(&cat, &origin, &measured_at, &links));
    rulesrun::patch_link_facts(&mut facts, &links, &want);
    let (second, report) = rulesrun::evaluate(w, &facts)?;
    let matches: Vec<_> = second.into_iter().filter(|m| want.contains(&m.info_hash)).collect();
    Ok(Decision {
        no_longer: first.len().saturating_sub(matches.len()),
        rechecked: picked.len(),
        matches,
        report,
        links,
    })
}

fn gather_all_with(
    state: &AppState,
    links: &std::collections::HashMap<String, crate::linkindex::LinkFacts>,
    want: rulesrun::Want,
) -> Vec<rules::Facts> {
    let ids: Vec<String> = state
        .engines
        .engines()
        .iter()
        .map(|e| e.id.clone())
        .collect();
    // ⚠️ The READ connection. This only reads, and it walks the whole
    // catalogue: on the writer it held every add, tag and pause for twenty
    // seconds and more at a million torrents.
    let store = state.store.read().unwrap();
    let mut out = Vec::new();
    for id in ids {
        out.extend(rulesrun::gather(&state.engines, &store, &id, links, want));
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
    if w.trigger != rules::Trigger::Schedule {
        // Running it "now" would mean inventing the event, and the only
        // honest candidates -- torrents that finished at some point -- are
        // exactly what an event workflow exists NOT to act on.
        return bad(match w.trigger {
            rules::Trigger::Added => "this workflow runs when a torrent is added; use Preview to see what it would do to the last day's arrivals",
            _ => "this workflow runs when a download completes; use Preview to see which downloads it would act on",
        });
    }
    // Off the async runtime: a pass reads the link index and stats its
    // candidates, which is disk wait a request thread must not sit in.
    match tokio::task::spawn_blocking(move || run_one(&state, &w, dry)).await {
        Ok(report) => Json(report).into_response(),
        Err(e) => bad(e),
    }
}

/// One pass of one workflow. The single path both the timer and the button use.
pub fn run_one(state: &AppState, w: &Workflow, dry: bool) -> serde_json::Value {
    let Decision { matches, mut report, links, rechecked, no_longer } = match decide(state, w) {
        Ok(d) => d,
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
            "rechecked": rechecked,
            "no_longer_true": no_longer,
        });
    }

    apply_matches(state, w, &matches, &links, &mut report);

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
        "rechecked": rechecked,
        "no_longer_true": no_longer,
    })
}

/// Phase two for a set of matches: carry each out, log each outcome.
///
/// The one place actions are done, whether a timer, a button or an event
/// decided on them.
fn apply_matches(
    state: &AppState,
    w: &Workflow,
    matches: &[rulesrun::Match],
    links: &std::collections::HashMap<String, crate::linkindex::LinkFacts>,
    report: &mut rulesrun::PassReport,
) {
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
    // Moves and tracker edits too: the job "Set location..." queues, the edit
    // the tracker editor makes.
    let move_hook = |engine: &str, hash: &str, to: &str, allow: bool| {
        crate::api::queue_location_move(state, engine, hash, to, allow)
    };
    let trackers_hook = |_engine: &str, hash: &str, urls: &[String]| crate::api::add_trackers_to(state, hash, urls);

    for m in matches {
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
                rules::Action::Webhook { .. } => "webhook",
                rules::Action::SetLocation { .. } => "set_location",
                rules::Action::AddTrackers { .. } => "add_trackers",
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
            &move_hook,
            &trackers_hook,
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
}

/// Where the link index stands: how much of the catalogue it has measured,
/// how old the oldest measurement is, and how many torrents it found with
/// their files gone.
pub async fn links_status(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    if !crate::api::authorised(&state, &headers, &query) {
        return refuse();
    }
    let counts = {
        let store = state.store.read().unwrap();
        store.link_index_counts().unwrap_or_default()
    };
    use std::sync::atomic::Ordering::Relaxed;
    let p = &crate::linkscan::PROGRESS;
    Json(serde_json::json!({
        "catalogue": p.catalogue.load(Relaxed),
        "measured": counts.measured,
        "files": counts.files,
        "data_missing": counts.data_missing,
        "partly_missing": counts.partly_missing,
        "oldest_measurement": counts.oldest,
        "refresh_secs": crate::linkscan::REFRESH_SECS,
        "sweep": {
            "total": p.sweep_total.load(Relaxed),
            "done": p.sweep_done.load(Relaxed),
            "started": p.sweep_started.load(Relaxed),
            "last_batch_at": p.last_batch_at.load(Relaxed),
            "files_per_sec": p.files_per_sec.load(Relaxed),
        },
    }))
    .into_response()
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
            tick(&state, crate::store::now_secs()).await;
        }
    });
}

/// One tick of the timer: the waiting events, then the scheduled workflows
/// that are due. Apart from the loop so a test can run exactly what a tick
/// runs, at the time it chooses.
async fn tick(state: &AppState, now: i64) {
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
    // Whatever the completion listener could not finish: events
    // written before a restart, or left waiting for their engine.
    {
        let st = state.clone();
        let _ = tokio::task::spawn_blocking(move || run_events_at(&st, now)).await;
    }
    for stored in due {
        let Ok(w) = serde_json::from_str::<Workflow>(&stored.body) else {
            tracing::warn!(workflow = %stored.name, "workflow body will not parse, skipped");
            continue;
        };
        // An event workflow is due whenever its event happens, never
        // on the clock.
        if w.trigger != rules::Trigger::Schedule {
            continue;
        }
        // Claimed BEFORE it runs. Marked only at the end, a pass that
        // takes longer than the tick -- or never reaches the end --
        // stays due, and the timer starts it again every minute on top
        // of the one still running.
        if let Ok(store) = state.store.lock() {
            let _ = store.mark_workflow_run(&w.id, now);
        }
        let st = state.clone();
        let name = w.name.clone();
        let Ok(report) = tokio::task::spawn_blocking(move || run_one(&st, &w, false)).await else {
            tracing::warn!(workflow = %name, "workflow pass panicked");
            continue;
        };
        // Silence when nothing happened: a scheduled rule that matches
        // nothing is the normal case and must not fill the log.
        if report.get("applied").and_then(|v| v.as_u64()).unwrap_or(0) > 0
            || report.get("failed").and_then(|v| v.as_u64()).unwrap_or(0) > 0
        {
            tracing::info!(workflow = %name, report = %report, "workflow ran");
        }
    }
}

/// How long an event waits for its torrent to show up in its engine.
///
/// It is normally there already -- the engine is what raised the event. The
/// wait is for a restart, where the rows are read back before the catalogue
/// is loaded. An hour is far past any load, and short enough that a torrent
/// removed in the meantime does not keep a row alive.
const EVENT_PATIENCE_SECS: i64 = 3600;

/// One at a time. The listener and the timer both drain the queue, and two
/// drains reading the same rows would run the same completion twice.
static EVENT_DRAIN: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Listen for finished downloads, write each down, then run the event
/// workflows on it.
///
/// Written to the store FIRST: once the row exists, a crash, a restart or a
/// failed pass loses nothing -- the timer picks it back up within a minute.
pub fn spawn_events(
    state: AppState,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<(String, [u8; 20])>,
) {
    // Adds are written to the store by the add itself (`record_added`); this
    // only has to be told there is something to drain. A `Notify` keeps one
    // permit, so a thousand adds in a burst wake it once or twice, not a
    // thousand times.
    let woken = state.clone();
    tokio::spawn(async move {
        loop {
            EVENT_WAKE.notified().await;
            let st = woken.clone();
            let _ = tokio::task::spawn_blocking(move || run_events(&st)).await;
        }
    });
    tokio::spawn(async move {
        while let Some(first) = rx.recv().await {
            // A race finishing a burst of torrents is one store write, not
            // one per torrent.
            let mut batch = vec![first];
            while let Ok(more) = rx.try_recv() {
                batch.push(more);
            }
            let st = state.clone();
            let _ = tokio::task::spawn_blocking(move || record_completions(&st, &batch)).await;
        }
    });
}

static EVENT_WAKE: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// Note a torrent just added, for the "on add" workflows. Returns whether
/// there is anything to run; nothing is written when no enabled workflow
/// listens, so a node without one pays a single small query per add.
///
/// Written to the store first, like a completion, and for the same reason: a
/// restart between the add and the run loses nothing.
pub fn record_added(state: &AppState, session: &str, info_hash: &str) {
    let Ok(store) = state.store.lock() else { return };
    let listening = store.workflows().unwrap_or_default().iter().any(|s| {
        s.enabled
            && serde_json::from_str::<Workflow>(&s.body).is_ok_and(|w| w.trigger == rules::Trigger::Added)
    });
    if !listening {
        return;
    }
    if let Err(e) = store.push_workflow_event("added", session, info_hash) {
        tracing::warn!(error = %e, info_hash = %info_hash, "add could not be recorded for workflows");
        return;
    }
    drop(store);
    EVENT_WAKE.notify_one();
}

/// Write finished downloads down, then run the event workflows on them.
pub fn record_completions(state: &AppState, batch: &[(String, [u8; 20])]) {
    {
        let Ok(store) = state.store.lock() else { return };
        for (session, ih) in batch {
            let hash: String = ih.iter().map(|b| format!("{b:02x}")).collect();
            if let Err(e) = store.push_workflow_event("completed", session, &hash) {
                tracing::warn!(error = %e, info_hash = %hash, "completion could not be recorded for workflows");
            }
        }
    }
    run_events(state);
}

/// Run the event workflows on every waiting event. Returns how many events
/// were dealt with (and removed).
pub fn run_events(state: &AppState) -> usize {
    run_events_at(state, crate::store::now_secs())
}

/// `run_events` as of `now`, which decides how long an event has waited.
fn run_events_at(state: &AppState, now: i64) -> usize {
    let _one = EVENT_DRAIN.lock().unwrap_or_else(|p| p.into_inner());
    let (events, workflows) = {
        let Ok(store) = state.store.lock() else { return 0 };
        let events = store.workflow_events(1000).unwrap_or_default();
        if events.is_empty() {
            return 0;
        }
        let workflows: Vec<Workflow> = store
            .workflows()
            .unwrap_or_default()
            .into_iter()
            .filter(|s| s.enabled)
            .filter_map(|s| serde_json::from_str::<Workflow>(&s.body).ok())
            .filter(|w| w.trigger.is_event())
            .collect();
        (events, workflows)
    };
    let mut done = 0;
    for ev in events {
        // No workflow to hand it to, or an event nothing here handles: the
        // row is cleared all the same, or it would wait forever.
        let listening: Vec<&Workflow> =
            workflows.iter().filter(|w| w.trigger.event_name() == ev.event).collect();
        let facts = if !listening.is_empty() {
            let Ok(store) = state.store.read() else { break };
            let f = rulesrun::gather_one(&state.engines, &store, &ev.session, &ev.info_hash);
            if f.is_none() && now - ev.at < EVENT_PATIENCE_SECS {
                continue;
            }
            f
        } else {
            None
        };
        if let Some(f) = facts {
            for w in listening {
                match rulesrun::evaluate(w, std::slice::from_ref(&f)) {
                    Ok((matches, mut report)) => {
                        apply_matches(state, w, &matches, &Default::default(), &mut report)
                    }
                    Err(e) => tracing::warn!(workflow = %w.name, error = %e, "event workflow will not compile, skipped"),
                }
            }
        }
        if let Ok(store) = state.store.lock() {
            let _ = store.drop_workflow_event(ev.id);
        }
        done += 1;
    }
    done
}

/// Mount the workflow routes.
pub fn routes() -> axum::Router<AppState> {
    use axum::routing::{delete, get, post};
    axum::Router::new()
        .route("/api/workflows", get(list).post(save))
        .route("/api/workflows/fields", get(fields))
        .route("/api/workflows/preview", post(preview))
        .route("/api/workflows/activity", get(activity))
        .route("/api/workflows/links", get(links_status))
        .route("/api/workflows/:id/run", post(run_now))
        .route("/api/workflows/:id", delete(remove))
}

/// Kept so the module owns its Arc import even when the runner changes shape.
pub type Shared = Arc<crate::store::StoreLock>;

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

    /// Two workflows created back to back are two workflows. With a
    /// seconds-based id the second replaced the first on save.
    #[test]
    fn two_workflows_created_in_the_same_second_get_different_ids() {
        let a = parse(&wf_json("one")).unwrap();
        let b = parse(&wf_json("two")).unwrap();
        assert_ne!(a.id, b.id);
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

#[cfg(test)]
mod event_tests {
    use super::*;
    use crate::api::testing::{body_json, keyed, state_from, TestState};

    const KEY: &str = "0123456789abcdef0123456789abcdef";

    fn st(tag: &str) -> TestState {
        state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"))
    }

    fn torrent_bytes(name: &str) -> Vec<u8> {
        let mut info = Vec::new();
        info.extend_from_slice(format!("d6:lengthi16384e4:name{}:{name}", name.len()).as_bytes());
        info.extend_from_slice(b"12:piece lengthi16384e6:pieces20:");
        let mut piece = [0xCDu8; 20];
        piece[0] = name.as_bytes()[0];
        piece[1] = name.len() as u8;
        info.extend_from_slice(&piece);
        info.push(b'e');
        let mut out = Vec::new();
        out.extend_from_slice(b"d4:info");
        out.extend_from_slice(&info);
        out.push(b'e');
        out
    }

    /// A download under way: added, not seeded, nothing on disk.
    fn add(s: &TestState, name: &str) -> String {
        crate::api::add_torrent_bytes(&s.state, &torrent_bytes(name), "", "/tmp", "", true, false, "hoard")
            .unwrap_or_else(|e| panic!("add {name}: {e}"))
            .0
    }

    async fn save_wf(s: &TestState, trigger: &str, tag: &str) -> String {
        // A timer with no condition is refused (the whole catalogue); one that
        // would match anything still has to say so.
        let when = if trigger != "schedule" {
            serde_json::json!({"kind": "all", "of": []})
        } else {
            serde_json::json!({"kind": "all", "of": [{"kind": "cond", "field": "ratio", "op": "ge", "value": "0"}]})
        };
        let body = serde_json::json!({
            "id": "", "name": format!("{trigger} {tag}"), "enabled": true, "trigger": trigger,
            "when": when,
            "then": [{"type": "add_tags", "tags": [tag]}],
        });
        let r = save(State(s.state.clone()), RawQuery(None), keyed(KEY), body.to_string()).await;
        let status = r.status();
        let v = body_json(r).await;
        assert!(status.is_success(), "saved: {v}");
        v["id"].as_str().unwrap().to_string()
    }

    fn tags(s: &TestState, hash: &str) -> Vec<String> {
        s.state.store.lock().unwrap().tags_of(hash)
    }

    fn waiting(s: &TestState) -> usize {
        s.state.store.lock().unwrap().workflow_events(100).unwrap().len()
    }

    /// ⭐ "On add": the add itself queues the event, the drain runs the add
    /// workflows on that torrent alone, and neither the completion workflow
    /// nor a torrent added before the workflow existed is touched.
    #[tokio::test]
    async fn an_added_torrent_runs_the_add_workflows_once() {
        let s = st("wf-ev-added");
        let before = add(&s, "before");
        assert_eq!(waiting(&s), 0, "no add workflow yet: nothing is written");
        save_wf(&s, "added", "new").await;
        save_wf(&s, "completed", "done").await;

        let hash = add(&s, "arrival");
        assert_eq!(waiting(&s), 1, "the add is queued before anything runs");
        assert_eq!(run_events(&s.state), 1);
        assert_eq!(tags(&s, &hash), vec!["new".to_string()], "only the add workflow ran");
        assert!(tags(&s, &before).is_empty(), "added before: not an event, untouched");
        assert_eq!(waiting(&s), 0);
        assert_eq!(run_events(&s.state), 0, "nothing to replay");
    }

    /// Taking a library over is not a stream of arrivals: the import wizard's
    /// adds fire no "on add" workflow.
    #[tokio::test]
    async fn an_imported_torrent_fires_no_add_workflow() {
        let s = st("wf-ev-import");
        save_wf(&s, "added", "new").await;
        crate::api::add_torrent_bytes_as(&s.state, &torrent_bytes("imported"), "", "/tmp", "", true, false, "hoard")
            .expect("added");
        assert_eq!(waiting(&s), 0);
    }

    /// The preview of an add workflow judges the last day's arrivals, and
    /// running one "now" is refused like any event workflow.
    #[tokio::test]
    async fn an_add_workflow_previews_recent_arrivals_and_cannot_be_run_now() {
        let s = st("wf-ev-added-preview");
        add(&s, "recent");
        let body = serde_json::json!({
            "id": "", "name": "p", "trigger": "added",
            "when": {"kind": "all", "of": []},
            "then": [{"type": "add_tags", "tags": ["x"]}],
        });
        let v = body_json(preview(State(s.state.clone()), RawQuery(None), keyed(KEY), body.to_string()).await).await;
        assert_eq!(v["trigger"], "added", "got {v}");
        assert_eq!((v["recent"].as_u64(), v["would_apply"].as_u64()), (Some(1), Some(1)), "got {v}");

        let id = save_wf(&s, "added", "x").await;
        let r = run_now(State(s.state.clone()), Path(id), RawQuery(None), keyed(KEY)).await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        assert!(body_json(r).await["error"].as_str().unwrap().contains("torrent is added"));
    }

    fn torrent_bytes_private(name: &str) -> Vec<u8> {
        let mut t = torrent_bytes(name);
        // Inside the info dict, after `pieces` (keys stay sorted).
        let at = t.len() - 2;
        t.splice(at..at, b"7:privatei1e".iter().copied());
        t
    }

    async fn save_wf_then(s: &TestState, trigger: &str, then: serde_json::Value) -> String {
        let body = serde_json::json!({
            "id": "", "name": "wf", "enabled": true, "trigger": trigger,
            "when": {"kind": "all", "of": []}, "then": then,
        });
        let r = save(State(s.state.clone()), RawQuery(None), keyed(KEY), body.to_string()).await;
        let status = r.status();
        let v = body_json(r).await;
        assert!(status.is_success(), "saved: {v}");
        v["id"].as_str().unwrap().to_string()
    }

    fn trackers_of(s: &TestState, hash: &str) -> Vec<String> {
        let ih = crate::store::hex20(hash).unwrap();
        let e = s.state.engines.engines().iter().find(|e| e.id == "hoard").unwrap();
        e.manager.get(&ih).unwrap().live_trackers.read().iter().flatten().cloned().collect()
    }

    /// "Add trackers" on arrival adds to a public torrent, and NEVER to a
    /// private one, whatever the conditions say.
    #[tokio::test]
    async fn trackers_are_added_on_arrival_but_never_to_a_private_torrent() {
        let s = st("wf-addtrk");
        save_wf_then(&s, "added", serde_json::json!([
            {"type": "add_trackers", "urls": ["udp://open.example:1337/announce"]}
        ])).await;
        let public = add(&s, "public");
        let private = crate::api::add_torrent_bytes(&s.state, &torrent_bytes_private("private"), "", "/tmp", "", true, false, "hoard")
            .expect("added").0;
        run_events(&s.state);
        assert!(trackers_of(&s, &public).contains(&"udp://open.example:1337/announce".to_string()));
        assert!(trackers_of(&s, &private).is_empty(), "a private torrent keeps its own trackers only");
    }

    /// A refused URL never reaches a torrent: the workflow does not save.
    #[tokio::test]
    async fn add_trackers_refuses_what_is_not_an_announce_url() {
        let s = st("wf-addtrk-bad");
        for then in [
            serde_json::json!([{"type": "add_trackers", "urls": ["ftp://x/announce"]}]),
            serde_json::json!([{"type": "add_trackers"}]),
            serde_json::json!([{"type": "set_location", "to": "relative/dir"}]),
            serde_json::json!([{"type": "set_location", "to": "/data/../etc"}]),
        ] {
            let body = serde_json::json!({"id": "", "name": "x", "enabled": true, "trigger": "added",
                "when": {"kind": "all", "of": []}, "then": then});
            let r = save(State(s.state.clone()), RawQuery(None), keyed(KEY), body.to_string()).await;
            assert_eq!(r.status(), StatusCode::BAD_REQUEST, "accepted {body}");
        }
    }

    /// "Move files to a folder" when a download completes queues the same
    /// data move "Set location..." does, and only once: a torrent already
    /// there is left alone.
    #[tokio::test]
    async fn a_completed_download_is_moved_to_its_folder() {
        let s = st("wf-move");
        let root = std::env::temp_dir().join(format!("wf-move-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (src, dst) = (root.join("temp"), root.join("final"));
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(src.join("moved"), vec![0u8; 16384]).unwrap();
        let hash = crate::api::add_torrent_bytes(&s.state, &torrent_bytes("moved"), "", src.to_str().unwrap(), "", true, true, "hoard")
            .expect("added").0;
        save_wf_then(&s, "completed", serde_json::json!([
            {"type": "set_location", "to": dst.to_str().unwrap()}
        ])).await;
        s.state.store.lock().unwrap().push_workflow_event("completed", "hoard", &hash).unwrap();
        run_events(&s.state);
        let jobs = s.state.store.lock().unwrap().list_jobs(10).unwrap();
        let moves: Vec<_> = jobs.iter().filter(|j| j.info_hash == hash).collect();
        assert_eq!(moves.len(), 1, "one move queued: {jobs:?}");
        assert!(moves[0].params.contains("final"), "towards the folder asked: {:?}", moves[0].params);
        let act = s.state.store.lock().unwrap().workflow_activity(10).unwrap();
        assert_eq!((act[0].action.as_str(), act[0].outcome.as_str()), ("set_location", "applied"), "{act:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ⭐ The whole road: the engine says a download finished, through the
    /// hook `EngineHost` installs; the completion workflow acts on that
    /// torrent, the scheduled one does not, and the event is gone after.
    #[tokio::test]
    async fn a_finished_download_runs_the_completion_workflows_once() {
        let s = st("wf-ev-road");
        let hash = add(&s, "finished");
        let other = add(&s, "stilldl");
        save_wf(&s, "completed", "done").await;
        save_wf(&s, "schedule", "timer").await;

        let mut rx = s.state.engines.take_completions().expect("the stream is there to take");
        let ih = crate::store::hex20(&hash).unwrap();
        let engine = s.state.engines.engines().iter().find(|e| e.id == "hoard").unwrap();
        engine.manager.on_completed(&ih);
        let got = rx.try_recv().expect("the hook reported the completion");
        assert_eq!(got, ("hoard".to_string(), ih));

        record_completions(&s.state, &[got]);
        assert_eq!(tags(&s, &hash), vec!["done".to_string()], "only the completion workflow ran");
        assert!(tags(&s, &other).is_empty(), "a torrent that did not finish is untouched");
        assert_eq!(waiting(&s), 0, "the event is dealt with and removed");

        let act = s.state.store.lock().unwrap().workflow_activity(10).unwrap();
        assert_eq!(act.len(), 1);
        assert_eq!((act[0].action.as_str(), act[0].outcome.as_str()), ("add_tags", "applied"));

        // Nothing is left to replay: another drain does nothing.
        assert_eq!(run_events(&s.state), 0);
        assert_eq!(s.state.store.lock().unwrap().workflow_activity(10).unwrap().len(), 1);
    }

    /// With no completion workflow the queue must not grow: every event is
    /// cleared, not kept for a workflow that may never exist.
    #[tokio::test]
    async fn without_a_completion_workflow_events_are_cleared() {
        let s = st("wf-ev-none");
        let hash = add(&s, "lonely");
        save_wf(&s, "schedule", "timer").await;
        record_completions(&s.state, &[("hoard".into(), crate::store::hex20(&hash).unwrap())]);
        assert_eq!(waiting(&s), 0);
        assert!(tags(&s, &hash).is_empty());
    }

    /// A completion the engine cannot place yet -- read back after a restart,
    /// before the catalogue is loaded -- waits rather than being thrown away.
    #[tokio::test]
    async fn an_event_for_a_torrent_not_loaded_yet_waits() {
        let s = st("wf-ev-wait");
        save_wf(&s, "completed", "done").await;
        record_completions(&s.state, &[("hoard".into(), [0x42; 20])]);
        assert_eq!(waiting(&s), 1, "kept for the next drain");
    }

    /// An event whose torrent never shows up is not kept forever: an hour,
    /// then it goes. Before the hour it waits, however many drains pass.
    #[tokio::test]
    async fn an_event_that_never_finds_its_torrent_is_dropped_after_an_hour() {
        let s = st("wf-ev-expire");
        save_wf(&s, "completed", "done").await;
        record_completions(&s.state, &[("hoard".into(), [0x42; 20])]);
        let now = crate::store::now_secs();
        assert_eq!(run_events_at(&s.state, now + EVENT_PATIENCE_SECS - 60), 0, "still inside the hour");
        assert_eq!(waiting(&s), 1);
        assert_eq!(run_events_at(&s.state, now + EVENT_PATIENCE_SECS + 1), 1, "past it: dealt with");
        assert_eq!(waiting(&s), 0);
        assert!(
            s.state.store.lock().unwrap().workflow_activity(10).unwrap().is_empty(),
            "and nothing was done in its name"
        );
    }

    /// ⭐ A restart between the completion and the workflow: the row is in
    /// the store, no listener will ever hear of it again. The timer's tick is
    /// what picks it up.
    #[tokio::test]
    async fn the_timer_picks_up_an_event_left_by_a_restart() {
        let s = st("wf-ev-tick");
        let hash = add(&s, "restarted");
        save_wf(&s, "completed", "done").await;
        // Written by the previous process, which stopped before acting on it.
        s.state.store.lock().unwrap().push_workflow_event("completed", "hoard", &hash).unwrap();
        tick(&s.state, crate::store::now_secs()).await;
        assert_eq!(tags(&s, &hash), vec!["done".to_string()]);
        assert_eq!(waiting(&s), 0);
    }

    /// A receiver in a test: records each POST body and answers `status`.
    async fn hook_receiver(status: u16) -> (String, std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>) {
        let got = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = got.clone();
        let app = axum::Router::new().route(
            "/hook/SECRET",
            axum::routing::post(move |body: String| {
                let log = log.clone();
                async move {
                    log.lock().unwrap().push(serde_json::from_str(&body).unwrap_or(serde_json::Value::Null));
                    axum::http::StatusCode::from_u16(status).unwrap()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}/hook/SECRET"), got)
    }

    async fn save_hook_wf(s: &TestState, url: &str) {
        let body = serde_json::json!({
            "id": "", "name": "tell me", "enabled": true, "trigger": "completed",
            "when": {"kind": "all", "of": []},
            "then": [{"type": "add_tags", "tags": ["done"]}, {"type": "webhook", "url": url}],
        });
        let r = save(State(s.state.clone()), RawQuery(None), keyed(KEY), body.to_string()).await;
        assert!(r.status().is_success());
    }

    /// ⭐ A finished download reaches the webhook: one POST, with the torrent
    /// and the line Discord shows, after the tag.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_finished_download_is_posted_to_the_webhook() {
        let s = st("wf-ev-hook");
        let hash = add(&s, "hooked");
        let (url, got) = hook_receiver(204).await;
        save_hook_wf(&s, &url).await;
        let st2 = s.state.clone();
        let ih = crate::store::hex20(&hash).unwrap();
        tokio::task::spawn_blocking(move || record_completions(&st2, &[("hoard".into(), ih)])).await.unwrap();

        let posts = got.lock().unwrap().clone();
        assert_eq!(posts.len(), 1, "one call");
        assert_eq!(posts[0]["event"], "completed");
        assert_eq!(posts[0]["torrent"]["info_hash"], hash);
        assert_eq!(posts[0]["torrent"]["tags"], serde_json::json!([]), "the facts the rule matched on, before its own tag");
        assert!(posts[0]["content"].as_str().unwrap().contains("hooked"));
        let act = s.state.store.lock().unwrap().workflow_activity(10).unwrap();
        assert_eq!((act[0].action.as_str(), act[0].outcome.as_str()), ("add_tags+webhook", "applied"));
    }

    /// A receiver that refuses is a failed action, said in the activity log
    /// -- and the log never shows the URL, which for Discord is the secret.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_refused_webhook_is_a_failure_that_never_shows_its_url() {
        let s = st("wf-ev-hook404");
        let hash = add(&s, "refused");
        let (url, got) = hook_receiver(404).await;
        save_hook_wf(&s, &url).await;
        let st2 = s.state.clone();
        let ih = crate::store::hex20(&hash).unwrap();
        tokio::task::spawn_blocking(move || record_completions(&st2, &[("hoard".into(), ih)])).await.unwrap();

        assert_eq!(got.lock().unwrap().len(), 1, "a 4xx is not retried");
        let act = s.state.store.lock().unwrap().workflow_activity(10).unwrap();
        assert_eq!(act[0].outcome, "failed");
        assert!(act[0].detail.contains("404"), "{}", act[0].detail);
        assert!(!act[0].detail.contains("SECRET"), "the URL stays out of the log: {}", act[0].detail);
    }

    /// A disabled completion workflow is the same as none.
    #[tokio::test]
    async fn a_disabled_completion_workflow_does_nothing() {
        let s = st("wf-ev-off");
        let hash = add(&s, "offwf");
        let id = save_wf(&s, "completed", "done").await;
        {
            let store = s.state.store.lock().unwrap();
            let mut w = store.workflow(&id).unwrap().unwrap();
            w.enabled = false;
            store.put_workflow(&w).unwrap();
        }
        record_completions(&s.state, &[("hoard".into(), crate::store::hex20(&hash).unwrap())]);
        assert!(tags(&s, &hash).is_empty());
        assert_eq!(waiting(&s), 0);
    }

    /// "Run now" on a completion workflow would have to invent the event.
    #[tokio::test]
    async fn a_completion_workflow_cannot_be_run_by_hand() {
        let s = st("wf-ev-run");
        add(&s, "x");
        let id = save_wf(&s, "completed", "done").await;
        let r = run_now(State(s.state.clone()), Path(id), RawQuery(None), keyed(KEY)).await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    }

    /// Its preview is the downloads under way -- the torrents that WILL fire
    /// it -- and says so, rather than the catalogue that already finished.
    #[tokio::test]
    async fn a_completion_preview_looks_at_the_downloads_under_way() {
        let s = st("wf-ev-preview");
        add(&s, "a");
        add(&s, "b");
        let body = serde_json::json!({
            "id": "", "name": "p", "trigger": "completed",
            "when": {"kind": "all", "of": []},
            "then": [{"type": "add_tags", "tags": ["done"]}],
        });
        let r = preview(State(s.state.clone()), RawQuery(None), keyed(KEY), body.to_string()).await;
        let v = body_json(r).await;
        assert_eq!(v["trigger"], "completed");
        assert_eq!(v["downloading"], 2);
        assert_eq!(v["would_apply"], 2);
    }

    /// The listing says which trigger a workflow has, and one saved before
    /// triggers existed reads as the timer it always was.
    #[tokio::test]
    async fn the_listing_carries_the_trigger() {
        let s = st("wf-ev-list");
        save_wf(&s, "completed", "done").await;
        let rows = body_json(list(State(s.state.clone()), RawQuery(None), keyed(KEY)).await).await;
        assert_eq!(rows[0]["trigger"], "completed");

        let old = crate::store::StoredWorkflow {
            id: "old".into(),
            name: "old".into(),
            body: r#"{"name":"old","when":{"kind":"cond","field":"ratio","op":"ge","value":"2"},"then":[{"type":"pause"}]}"#.into(),
            enabled: false,
            position: 9,
            interval_secs: 900,
            last_run: 0,
        };
        s.state.store.lock().unwrap().put_workflow(&old).unwrap();
        let rows = body_json(list(State(s.state.clone()), RawQuery(None), keyed(KEY)).await).await;
        let old = rows.as_array().unwrap().iter().find(|r| r["id"] == "old").unwrap();
        assert_eq!(old["trigger"], "schedule");
    }
}

/// One ratio and one hardlink count, whoever asks.
#[cfg(test)]
mod one_number_tests {
    use super::*;
    use crate::api::testing::{state_from, TestState};

    const KEY: &str = "0123456789abcdef0123456789abcdef";

    fn st(tag: &str) -> TestState {
        state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"))
    }

    /// A single-file torrent named `name`. The info hash varies with the
    /// name's first byte and length, so the names below are chosen distinct.
    fn torrent_bytes(name: &str) -> Vec<u8> {
        let mut info = Vec::new();
        info.extend_from_slice(format!("d6:lengthi16384e4:name{}:{name}", name.len()).as_bytes());
        info.extend_from_slice(b"12:piece lengthi16384e6:pieces20:");
        let mut piece = [0xEFu8; 20];
        piece[0] = name.as_bytes()[0];
        piece[1] = name.len() as u8;
        info.extend_from_slice(&piece);
        info.push(b'e');
        let mut out = Vec::new();
        out.extend_from_slice(b"d4:info");
        out.extend_from_slice(&info);
        out.push(b'e');
        out
    }

    /// Seeded (the data is trusted whole, as for a cross-seed) and stopped:
    /// no network, and `total_done` is the full size.
    fn add(s: &TestState, name: &str, save_path: &str) -> String {
        crate::api::add_torrent_bytes(&s.state, &torrent_bytes(name), "", save_path, "", true, true, "hoard")
            .unwrap_or_else(|e| panic!("add {name}: {e}"))
            .0
    }

    fn set_transfer(s: &TestState, hash: &str, up: u64, down: u64) {
        let t = s.state.engines.get("hoard").unwrap().manager
            .get(&crate::store::hex20(hash).unwrap())
            .expect("the engine holds it");
        t.total_uploaded.store(up, std::sync::atomic::Ordering::Relaxed);
        t.total_downloaded.store(down, std::sync::atomic::Ordering::Relaxed);
    }

    fn by_hash(rows: &serde_json::Value, key: &str) -> std::collections::HashMap<String, serde_json::Value> {
        rows.as_array()
            .expect("rows")
            .iter()
            .map(|r| (r[key].as_str().unwrap().to_string(), r.clone()))
            .collect()
    }

    /// ⭐ 02/10/2026: a cross-seed (downloaded 0, holds N, uploaded 3N) read
    /// 3.2 in the table while the sort, the filters and the workflows saw 0.
    /// Every reader now gets 3 from the same function.
    #[tokio::test]
    async fn a_cross_seed_has_one_ratio_in_the_row_the_sort_and_the_workflows() {
        let s = st("one-ratio");
        let n: u64 = 16384; // the torrents' size, so what each one holds
        let xseed = add(&s, "xseed", "/tmp");
        let two = add(&s, "twofold", "/tmp");
        let one = add(&s, "o", "/tmp");
        set_transfer(&s, &xseed, 3 * n, 0);
        set_transfer(&s, &two, 2 * n, n);
        set_transfer(&s, &one, n, n);

        // The row.
        let page = crate::api::engine_page_value(&s.state, "hoard", "sort=ratio&order=desc").await;
        let rows = by_hash(&page["rows"], "info_hash");
        assert_eq!(rows[&xseed]["ratio"], serde_json::json!(3));
        assert_eq!(rows[&two]["ratio"], serde_json::json!(2), "a normal download is unchanged");
        assert_eq!(rows[&one]["ratio"], serde_json::json!(1));

        // The sort key: 3 puts it first. Under the old key it was 0, last.
        let order: Vec<&str> = page["rows"].as_array().unwrap().iter()
            .map(|r| r["info_hash"].as_str().unwrap()).collect();
        assert_eq!(order, vec![xseed.as_str(), two.as_str(), one.as_str()]);

        // The workflow facts.
        let facts = gather_all(&s.state, Default::default());
        let ratio_of = |h: &str| facts.iter().find(|f| f.info_hash == h).expect("facts").ratio;
        assert_eq!(ratio_of(&xseed), 3.0);
        assert_eq!(ratio_of(&two), 2.0);
        assert_eq!(ratio_of(&one), 1.0);

        // And the qBittorrent API, since 4.4: it answered 0 for every torrent
        // until then (the 3.x key spelling it reproduced, see qbitrow.rs).
        // *arr acts on this number, so it is the same one as everywhere else.
        let qbit = serde_json::Value::Array(crate::api::engine_qbit_rows(&s.state, "hoard", 0, None, None));
        let qbit = by_hash(&qbit, "hash");
        assert_eq!(qbit[&xseed]["ratio"], serde_json::json!(3));
        assert_eq!(qbit[&two]["ratio"], serde_json::json!(2));
    }

    /// ⭐ The "Hardlinks" column and a workflow condition on `external_links`
    /// read ONE number. Real files, a real cross-seed hardlink and a real
    /// outside one, measured and stored as the scanner does; then the column
    /// (from the published summary) is compared torrent by torrent with what
    /// `decide` evaluates the condition on.
    #[tokio::test]
    async fn the_hardlink_column_and_the_workflow_condition_agree() {
        let s = st("links-agree");
        let root = s.dir.join("data");
        std::fs::create_dir_all(&root).unwrap();
        let p = |n: &str| root.join(n);
        std::fs::write(p("ours.bin"), b"x").unwrap();
        std::fs::hard_link(p("ours.bin"), p("cross.bin")).unwrap(); // a cross-seed of ours
        std::fs::write(p("lib.bin"), b"y").unwrap();
        std::fs::hard_link(p("lib.bin"), p("library-copy.mkv")).unwrap(); // the media library
        std::fs::write(p("never.bin"), b"z").unwrap();

        let root_s = root.to_str().unwrap();
        let ours = add(&s, "ours.bin", root_s);
        let cross = add(&s, "cross.bin", root_s);
        let lib = add(&s, "lib.bin", root_s);

        // Measured and stored the way `linkscan::sweep` does it.
        let measured_at = 1_700_000_123;
        {
            let stored = {
                let store = s.state.store.read().unwrap();
                rulesrun::stored_facts(&s.state.engines, &store)
            };
            let cat = rulesrun::catalogue_from(&s.state.engines, &stored);
            assert_eq!(cat.len(), 3);
            let plan = cat.iter().map(|c| (c.info_hash.clone(), c.paths.clone())).collect();
            let measured = rulesrun::stat_plan_with(plan, 2);
            let rows: Vec<_> = cat.iter().zip(&measured)
                .map(|(c, (_, fs))| {
                    let stats: Vec<_> = fs.iter().map(|(_, st)| *st).collect();
                    rulesrun::link_row(c, &stats, measured_at)
                })
                .collect();
            s.state.store.lock().unwrap().put_link_rows(&rows).unwrap();
        }
        // Added after the sweep: no measurement at all.
        let never = add(&s, "never.bin", root_s);

        assert_eq!(crate::linkscan::refresh_summary(&s.state.engines, &s.state.store), Some(3));

        let page = crate::api::engine_page_value(&s.state, "hoard", "sort=external_links&order=asc").await;
        let order: Vec<&str> = page["rows"].as_array().unwrap().iter()
            .map(|r| r["info_hash"].as_str().unwrap()).collect();
        assert_eq!(order.last(), Some(&never.as_str()), "unmeasured sorts last ascending");
        assert_eq!(order[2], lib.as_str());
        let desc = crate::api::engine_page_value(&s.state, "hoard", "sort=external_links&order=desc").await;
        let order_desc: Vec<&str> = desc["rows"].as_array().unwrap().iter()
            .map(|r| r["info_hash"].as_str().unwrap()).collect();
        assert_eq!(order_desc[0], lib.as_str());
        assert_eq!(order_desc.last(), Some(&never.as_str()), "and last descending too");

        let col = by_hash(&page["rows"], "info_hash");
        assert_eq!(col[&ours]["external_links"], serde_json::json!(0), "a cross-seed of ours is not an outsider");
        assert_eq!(col[&cross]["external_links"], serde_json::json!(0));
        assert_eq!(col[&lib]["external_links"], serde_json::json!(1), "the library holds a name");
        assert_eq!(col[&never]["external_links"], serde_json::Value::Null, "never measured is not 0");
        assert_eq!(col[&ours]["links_checked_at"], serde_json::json!(measured_at));
        assert_eq!(col[&never]["links_checked_at"], serde_json::Value::Null);

        let w = parse(&serde_json::json!({
            "id": "", "name": "noHL", "enabled": true, "interval_secs": 3600, "cap": 10,
            "when": {"kind": "cond", "field": "external_links", "op": "eq", "value": "0"},
            "then": [{"type": "add_tags", "tags": ["noHL"]}],
        }).to_string()).expect("a valid workflow");
        let d = decide(&s.state, &w).expect("decided");

        // Torrent by torrent: the column's value is the workflow's value.
        for (hash, row) in &col {
            let wf = d.links.get(hash).map(|f| serde_json::json!(f.external_links));
            assert_eq!(
                row["external_links"],
                wf.unwrap_or(serde_json::Value::Null),
                "{hash}: column and workflow disagree"
            );
        }
        // And the condition matches exactly the rows the column shows at 0.
        let mut matched: Vec<&str> = d.matches.iter().map(|m| m.info_hash.as_str()).collect();
        let mut zero: Vec<&str> = col.iter()
            .filter(|(_, r)| r["external_links"] == serde_json::json!(0))
            .map(|(h, _)| h.as_str()).collect();
        matched.sort();
        zero.sort();
        assert_eq!(matched, zero);

        // The pass re-measured its candidates and published what it decided
        // on: the column still agrees, now dated by the re-measurement.
        let after = crate::api::engine_page_value(&s.state, "hoard", "").await;
        let after = by_hash(&after["rows"], "info_hash");
        for (hash, row) in &after {
            let wf = d.links.get(hash).map(|f| serde_json::json!(f.external_links));
            assert_eq!(row["external_links"], wf.unwrap_or(serde_json::Value::Null), "{hash} after the pass");
        }
        assert!(after[&ours]["links_checked_at"].as_i64().unwrap() > measured_at);
        assert_eq!(after[&lib]["links_checked_at"], serde_json::json!(measured_at), "not a candidate, not re-measured");
    }
}
