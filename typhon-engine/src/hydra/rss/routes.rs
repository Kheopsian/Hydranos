//! `/api/rss/*` and qBittorrent's `/api/v2/rss/*`.
//!
//! The shim answers qBittorrent 4.6 / 5.x's shapes: `items` nested by folder
//! with a feed's `uid` / `url` (and with `withData`, its `title`,
//! `lastBuildDate`, `isLoading`, `hasError` and `articles`), `rules` keyed by
//! name, `matchingArticles` as feed name → titles. Parameters are read from
//! the query string or the form body, like every other shim route. Refusals
//! are 409 with the reason in plain text, as qBittorrent answers, and 400
//! when a parameter is missing.

use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};

use super::{matcher::Rule, parse::rfc2822, Refusal, SEP};
use crate::api::AppState;
use crate::store::{RssArticleRow, RssFeedRow, RssTaken};

pub fn routes() -> axum::Router<AppState> {
    use axum::routing::{any, get, post, put};
    axum::Router::new()
        .route("/api/rss", get(overview))
        .route("/api/rss/feeds", get(list_feeds).post(create_feed))
        .route("/api/rss/feeds/:id", axum::routing::patch(edit_feed).delete(delete_feed))
        .route("/api/rss/feeds/:id/refresh", post(refresh_one))
        .route("/api/rss/feeds/:id/articles", get(articles))
        .route("/api/rss/feeds/:id/read", post(read))
        .route("/api/rss/feeds/:id/download", post(download_one))
        .route("/api/rss/refresh", post(refresh_all))
        .route("/api/rss/folders", post(create_folder))
        .route("/api/rss/rules", get(list_rules))
        .route("/api/rss/rules/:name", put(save_rule).delete(delete_rule))
        .route("/api/rss/rules/:name/rename", post(rename_rule))
        .route("/api/rss/rules/:name/matching", get(rule_matching))
        .route("/api/rss/preview", post(preview))
        .route("/api/v2/rss/addFolder", post(qb_add_folder))
        .route("/api/v2/rss/addFeed", post(qb_add_feed))
        .route("/api/v2/rss/setFeedURL", post(qb_set_feed_url))
        .route("/api/v2/rss/removeItem", post(qb_remove_item))
        .route("/api/v2/rss/moveItem", post(qb_move_item))
        .route("/api/v2/rss/refreshItem", post(qb_refresh_item))
        .route("/api/v2/rss/markAsRead", post(qb_mark_as_read))
        .route("/api/v2/rss/items", any(qb_items))
        .route("/api/v2/rss/setRule", post(qb_set_rule))
        .route("/api/v2/rss/renameRule", post(qb_rename_rule))
        .route("/api/v2/rss/removeRule", post(qb_remove_rule))
        .route("/api/v2/rss/rules", any(qb_rules))
        .route("/api/v2/rss/matchingArticles", any(qb_matching))
}

fn unauthorised() -> Response {
    (StatusCode::UNAUTHORIZED, Json(json!({"error": "Invalid or missing API key"}))).into_response()
}

macro_rules! guard {
    ($state:expr, $headers:expr, $query:expr) => {
        if !crate::api::authorised(&$state, &$headers, &$query) {
            return unauthorised();
        }
    };
}

fn refuse(r: Refusal) -> Response {
    (r.status(), Json(json!({"error": r.message()}))).into_response()
}

/// One parameter from the query string, else the form body; empty = absent.
fn param(query: &str, body: &str, name: &str) -> Option<String> {
    for src in [query, body] {
        for pair in src.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                if crate::api::percent_decode(k) == name {
                    let v = crate::api::percent_decode(v);
                    if !v.is_empty() {
                        return Some(v);
                    }
                }
            }
        }
    }
    None
}

fn body_json(body: &str) -> Result<Value, Response> {
    if body.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(body).map_err(|_| refuse(Refusal::Bad("the body is not JSON".into())))
}

// ---------------------------------------------------------------------------
// Native
// ---------------------------------------------------------------------------

fn taken_json(t: &Option<RssTaken>) -> Value {
    match t {
        Some(t) => json!({"rule": t.rule, "at": t.at, "info_hash": t.info_hash, "outcome": t.outcome}),
        None => Value::Null,
    }
}

fn article_json(a: &RssArticleRow, taken: &Option<RssTaken>) -> Value {
    json!({
        "guid": a.guid,
        "title": a.title,
        "torrent_url": a.torrent_url,
        "link": a.link,
        "description": a.description,
        "author": a.author,
        "category": a.category,
        "date": a.date,
        "size": a.size,
        "read": a.is_read,
        "taken": taken_json(taken),
    })
}

fn feeds_json(state: &AppState) -> Vec<Value> {
    let feeds = super::feeds(state);
    if feeds.is_empty() {
        return Vec::new();
    }
    let counts = state.store.lock().ok().and_then(|s| s.rss_article_counts().ok()).unwrap_or_default();
    let rules = super::load_rules(state);
    let exits = super::Exits::new(state);
    feeds
        .iter()
        .map(|f| {
            let (n, unread) = counts.get(&f.id).copied().unwrap_or((0, 0));
            let engine = super::feed_engine(state, &f.url, &rules);
            let (via, blocked) = match exits.of(state, &engine) {
                Ok((_, via)) => (via, Value::Null),
                Err(why) => (String::new(), Value::String(why)),
            };
            json!({
                "id": f.id,
                "uid": f.uid,
                "path": f.path,
                "name": super::name_of(&f.path),
                "url": f.url,
                "url_masked": super::mask_url(&f.url),
                "title": f.title,
                "refresh_secs": f.refresh_secs,
                "last_refresh": f.last_refresh,
                "next_refresh": if f.last_refresh == 0 { 0 } else { f.last_refresh + f.refresh_secs },
                "last_error": f.last_error,
                "loading": super::is_loading(state, f.id),
                "articles": n,
                "unread": unread,
                "engine": engine,
                "via": via,
                "blocked": blocked,
            })
        })
        .collect()
}

fn rule_json(r: &Rule) -> Value {
    let mut v = r.to_json();
    v["name"] = Value::String(r.name.clone());
    v
}

async fn overview(State(state): State<AppState>, RawQuery(query): RawQuery, headers: HeaderMap) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let s = super::settings(&state.cfg());
    Json(json!({
        "enabled": s.enabled,
        "max_articles": s.max_articles,
        "default_refresh_secs": s.refresh_secs,
        "min_refresh_secs": super::MIN_REFRESH_SECS,
        "feeds": feeds_json(&state),
        "folders": super::folders(&state),
        "rules": super::load_rules(&state).iter().map(rule_json).collect::<Vec<_>>(),
    }))
    .into_response()
}

async fn list_feeds(State(state): State<AppState>, RawQuery(query): RawQuery, headers: HeaderMap) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    Json(feeds_json(&state)).into_response()
}

fn feed_by_id(state: &AppState, id: i64) -> Option<Value> {
    feeds_json(state).into_iter().find(|f| f["id"] == id)
}

async fn create_feed(State(state): State<AppState>, RawQuery(query): RawQuery, headers: HeaderMap, body: String) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let v = match body_json(&body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    let path = if s("path").is_empty() { s("name") } else { s("path") };
    match super::add_feed(&state, &s("url"), &path, v.get("refresh_secs").and_then(Value::as_i64)) {
        Ok(f) => (StatusCode::CREATED, Json(feed_by_id(&state, f.id).unwrap_or(Value::Null))).into_response(),
        Err(r) => refuse(r),
    }
}

async fn edit_feed(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let v = match body_json(&body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let path = v.get("path").or_else(|| v.get("name")).and_then(Value::as_str);
    match super::update_feed(&state, id, path, v.get("url").and_then(Value::as_str), v.get("refresh_secs").and_then(Value::as_i64)) {
        Ok(_) => Json(feed_by_id(&state, id).unwrap_or(Value::Null)).into_response(),
        Err(r) => refuse(r),
    }
}

async fn delete_feed(State(state): State<AppState>, Path(id): Path<i64>, RawQuery(query): RawQuery, headers: HeaderMap) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let Some(f) = super::feeds(&state).into_iter().find(|f| f.id == id) else {
        return refuse(Refusal::NotFound(format!("no feed {id}")));
    };
    match super::remove_item(&state, &f.path) {
        Ok(_) => Json(json!({"removed": f.path})).into_response(),
        Err(r) => refuse(r),
    }
}

/// Fetch one feed now and answer once it is done: the row says how it went.
async fn refresh_one(State(state): State<AppState>, Path(id): Path<i64>, RawQuery(query): RawQuery, headers: HeaderMap) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    match super::refresh_feed(&state, id).await {
        Ok(_) => Json(feed_by_id(&state, id).unwrap_or(Value::Null)).into_response(),
        Err(r) => refuse(r),
    }
}

async fn refresh_all(State(state): State<AppState>, RawQuery(query): RawQuery, headers: HeaderMap) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let ids: Vec<i64> = super::feeds(&state).iter().map(|f| f.id).collect();
    let n = ids.len();
    super::refresh_soon(&state, ids);
    (StatusCode::ACCEPTED, Json(json!({"refreshing": n}))).into_response()
}

async fn articles(State(state): State<AppState>, Path(id): Path<i64>, RawQuery(query): RawQuery, headers: HeaderMap) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    if !super::feeds(&state).iter().any(|f| f.id == id) {
        return refuse(Refusal::NotFound(format!("no feed {id}")));
    }
    let unread_only = param(&query, "", "unread").is_some_and(|v| v == "1" || v == "true");
    let Ok(store) = state.store.lock() else {
        return refuse(Refusal::Failed("store lock".into()));
    };
    let out: Vec<Value> = store
        .rss_articles(id)
        .unwrap_or_default()
        .iter()
        .filter(|a| !unread_only || !a.is_read)
        .map(|a| {
            let t = store.rss_taken(&[&a.guid, &a.torrent_url]).ok().flatten();
            article_json(a, &t)
        })
        .collect();
    Json(out).into_response()
}

async fn read(State(state): State<AppState>, Path(id): Path<i64>, RawQuery(query): RawQuery, headers: HeaderMap, body: String) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let v = match body_json(&body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let Some(f) = super::feeds(&state).into_iter().find(|f| f.id == id) else {
        return refuse(Refusal::NotFound(format!("no feed {id}")));
    };
    match super::mark_read(&state, &f.path, v.get("guid").and_then(Value::as_str)) {
        Ok(n) => Json(json!({"marked": n})).into_response(),
        Err(r) => refuse(r),
    }
}

/// Add one article by hand, with a category (and optionally a folder).
async fn download_one(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let v = match body_json(&body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().trim().to_string();
    let guid = s("guid");
    if guid.is_empty() {
        return refuse(Refusal::Bad("guid is required".into()));
    }
    let paused = v.get("paused").and_then(Value::as_bool).unwrap_or(false);
    match super::download(&state, id, &guid, &s("category"), &s("save_path"), paused).await {
        Ok(a) => Json(json!({"info_hash": a.info_hash, "already": a.already})).into_response(),
        Err(super::AddError::Transient(e)) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
        Err(super::AddError::Refused(e)) => refuse(Refusal::Bad(e)),
    }
}

async fn create_folder(State(state): State<AppState>, RawQuery(query): RawQuery, headers: HeaderMap, body: String) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let v = match body_json(&body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    match super::add_folder(&state, v.get("path").and_then(Value::as_str).unwrap_or_default()) {
        Ok(()) => (StatusCode::CREATED, Json(json!({"folders": super::folders(&state)}))).into_response(),
        Err(r) => refuse(r),
    }
}

async fn list_rules(State(state): State<AppState>, RawQuery(query): RawQuery, headers: HeaderMap) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    Json(super::load_rules(&state).iter().map(rule_json).collect::<Vec<_>>()).into_response()
}

async fn save_rule(
    State(state): State<AppState>,
    Path(name): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let v = match body_json(&body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    match super::put_rule(&state, &name, &v) {
        Ok(r) => Json(rule_json(&r)).into_response(),
        Err(r) => refuse(r),
    }
}

async fn delete_rule(State(state): State<AppState>, Path(name): Path<String>, RawQuery(query): RawQuery, headers: HeaderMap) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    match super::remove_rule(&state, &name) {
        Ok(()) => Json(json!({"removed": name})).into_response(),
        Err(r) => refuse(r),
    }
}

async fn rename_rule(
    State(state): State<AppState>,
    Path(name): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let v = match body_json(&body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let new = v.get("name").or_else(|| v.get("new_name")).and_then(Value::as_str).unwrap_or_default();
    match super::rename_rule(&state, &name, new) {
        Ok(()) => Json(json!({"name": new.trim()})).into_response(),
        Err(r) => refuse(r),
    }
}

fn matching_json(state: &AppState, rule: &Rule) -> Value {
    let found = super::matching(state, rule);
    let count: usize = found.iter().map(|(_, a)| a.len()).sum();
    let would: usize = found.iter().flat_map(|(_, a)| a).filter(|(a, t)| t.is_none() && !a.is_read).count();
    json!({
        "count": count,
        "would_add": would,
        "feeds": found.iter().map(|(f, arts)| json!({
            "id": f.id,
            "path": f.path,
            "name": super::name_of(&f.path),
            "articles": arts.iter().map(|(a, t)| article_json(a, t)).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

/// What a saved rule would take. Nothing is added.
async fn rule_matching(State(state): State<AppState>, Path(name): Path<String>, RawQuery(query): RawQuery, headers: HeaderMap) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    match super::load_rules(&state).into_iter().find(|r| r.name == name) {
        Some(r) => Json(matching_json(&state, &r)).into_response(),
        None => refuse(Refusal::NotFound(format!("no rule {name:?}"))),
    }
}

/// What a draft rule (the editor's, unsaved) would take. Nothing is added.
async fn preview(State(state): State<AppState>, RawQuery(query): RawQuery, headers: HeaderMap, body: String) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let v = match body_json(&body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    match Rule::from_json(v.get("name").and_then(Value::as_str).unwrap_or("preview"), &v) {
        Ok(r) => Json(matching_json(&state, &r)).into_response(),
        Err(e) => refuse(Refusal::Bad(e)),
    }
}

// ---------------------------------------------------------------------------
// qBittorrent shim
// ---------------------------------------------------------------------------

fn qb_ok() -> Response {
    (StatusCode::OK, "").into_response()
}

fn qb_missing(what: &str) -> Response {
    (StatusCode::BAD_REQUEST, format!("Missing parameter: {what}")).into_response()
}

/// qBittorrent answers 409 with the reason for every refusal of an item or a
/// rule; a malformed value is a 400.
fn qb_refuse(r: Refusal) -> Response {
    let code = if matches!(r, Refusal::Bad(_)) { StatusCode::BAD_REQUEST } else { StatusCode::CONFLICT };
    (code, r.message().to_string()).into_response()
}

fn qb_done<T>(r: Result<T, Refusal>) -> Response {
    match r {
        Ok(_) => qb_ok(),
        Err(r) => qb_refuse(r),
    }
}

async fn qb_add_folder(State(state): State<AppState>, RawQuery(query): RawQuery, headers: HeaderMap, body: String) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let Some(path) = param(&query, &body, "path") else { return qb_missing("path") };
    qb_done(super::add_folder(&state, &path))
}

async fn qb_add_feed(State(state): State<AppState>, RawQuery(query): RawQuery, headers: HeaderMap, body: String) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let Some(url) = param(&query, &body, "url") else { return qb_missing("url") };
    let path = param(&query, &body, "path").unwrap_or_default();
    // 5.x's optional refresh interval, in seconds.
    let every = param(&query, &body, "refreshInterval").and_then(|v| v.trim().parse::<i64>().ok()).filter(|v| *v > 0);
    qb_done(super::add_feed(&state, &url, &path, every))
}

async fn qb_set_feed_url(State(state): State<AppState>, RawQuery(query): RawQuery, headers: HeaderMap, body: String) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let Some(path) = param(&query, &body, "path") else { return qb_missing("path") };
    let Some(url) = param(&query, &body, "url") else { return qb_missing("url") };
    qb_done(super::set_feed_url(&state, &path, &url))
}

async fn qb_remove_item(State(state): State<AppState>, RawQuery(query): RawQuery, headers: HeaderMap, body: String) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let Some(path) = param(&query, &body, "path") else { return qb_missing("path") };
    qb_done(super::remove_item(&state, &path))
}

async fn qb_move_item(State(state): State<AppState>, RawQuery(query): RawQuery, headers: HeaderMap, body: String) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let Some(from) = param(&query, &body, "itemPath") else { return qb_missing("itemPath") };
    let Some(to) = param(&query, &body, "destPath") else { return qb_missing("destPath") };
    qb_done(super::move_item(&state, &from, &to))
}

async fn qb_refresh_item(State(state): State<AppState>, RawQuery(query): RawQuery, headers: HeaderMap, body: String) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let Some(path) = param(&query, &body, "itemPath") else { return qb_missing("itemPath") };
    match super::feeds_at(&state, &path) {
        Ok(feeds) => {
            super::refresh_soon(&state, feeds.iter().map(|f| f.id).collect());
            qb_ok()
        }
        Err(r) => qb_refuse(r),
    }
}

async fn qb_mark_as_read(State(state): State<AppState>, RawQuery(query): RawQuery, headers: HeaderMap, body: String) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let Some(path) = param(&query, &body, "itemPath") else { return qb_missing("itemPath") };
    let article = param(&query, &body, "articleId");
    qb_done(super::mark_read(&state, &path, article.as_deref()))
}

fn qb_article(a: &RssArticleRow) -> Value {
    json!({
        "id": a.guid,
        "date": rfc2822(a.date),
        "title": a.title,
        "author": a.author,
        "category": a.category,
        "description": a.description,
        "torrentURL": a.torrent_url,
        "link": a.link,
        "isRead": a.is_read,
    })
}

fn qb_feed(state: &AppState, f: &RssFeedRow, with_data: bool) -> Value {
    let mut v = json!({"uid": f.uid, "url": f.url});
    if with_data {
        let arts = state.store.lock().ok().and_then(|s| s.rss_articles(f.id).ok()).unwrap_or_default();
        v["title"] = Value::String(f.title.clone());
        v["lastBuildDate"] = Value::String(if f.last_refresh > 0 { rfc2822(f.last_refresh) } else { String::new() });
        v["isLoading"] = Value::Bool(super::is_loading(state, f.id));
        v["hasError"] = Value::Bool(!f.last_error.is_empty());
        v["refreshInterval"] = json!(f.refresh_secs);
        v["articles"] = Value::Array(arts.iter().map(qb_article).collect());
    }
    v
}

/// Put `leaf` at `path` in the tree, creating the folders on the way.
fn insert_at(root: &mut Map<String, Value>, path: &str, leaf: Value) {
    let mut node = root;
    let parts: Vec<&str> = path.split(SEP).collect();
    for (i, part) in parts.iter().enumerate() {
        if i + 1 == parts.len() {
            node.entry(part.to_string()).or_insert(leaf);
            return;
        }
        let next = node.entry(part.to_string()).or_insert_with(|| Value::Object(Map::new()));
        if !next.is_object() {
            return;
        }
        node = next.as_object_mut().unwrap();
    }
}

async fn qb_items(State(state): State<AppState>, RawQuery(query): RawQuery, headers: HeaderMap, body: String) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let with_data = param(&query, &body, "withData").is_some_and(|v| v == "true" || v == "1");
    let mut root = Map::new();
    for d in super::folders(&state) {
        insert_at(&mut root, &d, Value::Object(Map::new()));
    }
    for f in super::feeds(&state) {
        insert_at(&mut root, &f.path, qb_feed(&state, &f, with_data));
    }
    Json(Value::Object(root)).into_response()
}

async fn qb_set_rule(State(state): State<AppState>, RawQuery(query): RawQuery, headers: HeaderMap, body: String) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let Some(name) = param(&query, &body, "ruleName") else { return qb_missing("ruleName") };
    let Some(def) = param(&query, &body, "ruleDef") else { return qb_missing("ruleDef") };
    let Ok(def) = serde_json::from_str::<Value>(&def) else {
        return (StatusCode::BAD_REQUEST, "ruleDef is not JSON").into_response();
    };
    qb_done(super::put_rule(&state, &name, &def))
}

async fn qb_rename_rule(State(state): State<AppState>, RawQuery(query): RawQuery, headers: HeaderMap, body: String) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let Some(name) = param(&query, &body, "ruleName") else { return qb_missing("ruleName") };
    let Some(new) = param(&query, &body, "newRuleName") else { return qb_missing("newRuleName") };
    qb_done(super::rename_rule(&state, &name, &new))
}

async fn qb_remove_rule(State(state): State<AppState>, RawQuery(query): RawQuery, headers: HeaderMap, body: String) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let Some(name) = param(&query, &body, "ruleName") else { return qb_missing("ruleName") };
    qb_done(super::remove_rule(&state, &name))
}

async fn qb_rules(State(state): State<AppState>, RawQuery(query): RawQuery, headers: HeaderMap) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let out: Map<String, Value> = super::load_rules(&state).iter().map(|r| (r.name.clone(), r.to_json())).collect();
    Json(Value::Object(out)).into_response()
}

async fn qb_matching(State(state): State<AppState>, RawQuery(query): RawQuery, headers: HeaderMap, body: String) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let Some(name) = param(&query, &body, "ruleName") else { return qb_missing("ruleName") };
    let Some(rule) = super::load_rules(&state).into_iter().find(|r| r.name == name) else {
        // qBittorrent answers an empty object for a rule it does not have.
        return Json(json!({})).into_response();
    };
    let mut out = Map::new();
    for (f, arts) in super::matching(&state, &rule) {
        if !arts.is_empty() {
            out.insert(
                super::name_of(&f.path).to_string(),
                Value::Array(arts.iter().map(|(a, _)| Value::String(a.title.clone())).collect()),
            );
        }
    }
    Json(Value::Object(out)).into_response()
}
