//! RSS feeds and automatic download rules, qBittorrent's way.
//!
//! A feed is fetched on its own interval (30 minutes by default, never less
//! than 5), its items kept as articles (the newest 200 per feed by default),
//! and each new unread article is offered to the rules in priority order; the
//! first rule that wants it adds it. What a rule took is recorded by every
//! identity of the article (guid and torrent link) and never taken again,
//! whatever the feed does with it afterwards.
//!
//! Three properties matter more than the parity:
//!
//! - **The add is the API's add.** A `.torrent` goes through
//!   `api::add_save_path` and `api::add_torrent_bytes`, a magnet through
//!   `magnets::request_with`, exactly as `POST /api/torrents/add` and the
//!   qBittorrent shim do: the category picks the engine and the folder, the
//!   path claim and dedup run, an add with no save path is refused. Each add
//!   and each refusal is written to the workflow activity trail with its
//!   reason, under `RSS: <rule>`.
//! - **A private feed leaves by the engine's way out.** Its URL carries the
//!   passkey, and the tracker sees the address it is fetched from next to
//!   the one the engine announces from. So the feed and its `.torrent`s are
//!   fetched by the route of the engine the torrent will go to (`Exits`):
//!   its tunnel, interface or proxy, its announce proxy when it has one. An
//!   engine the kill switch keeps off the network fetches nothing, and the
//!   feed says why.
//! - **A URL is never logged whole.** `mask_url` drops the query, any
//!   credentials and any path segment that looks like a key.
//!
//! `[rss] enabled = false` stops the worker (feeds stay, nothing is fetched or
//! added). With no feed it costs one read of an empty table every 30 s.

pub mod matcher;
pub mod parse;
mod routes;

pub use routes::routes;

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use axum::http::StatusCode;
use serde_json::Value;

use crate::api::AppState;
use crate::store::{RssArticleRow, RssFeedRow, RssTaken};
use matcher::Rule;
use typhon_engine::egress::Route;

/// qBittorrent's item path separator: `Folder\Feed`.
pub const SEP: char = '\\';
pub const DEFAULT_REFRESH_SECS: i64 = 30 * 60;
pub const MIN_REFRESH_SECS: i64 = 5 * 60;
pub const DEFAULT_MAX_ARTICLES: i64 = 200;
/// A feed larger than this is not a feed anyone means to poll.
pub const MAX_FEED_BYTES: usize = 5 << 20;
pub const MAX_TORRENT_BYTES: usize = 10 << 20;
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);
/// How often the worker looks for a feed that is due.
const TICK: Duration = Duration::from_secs(30);
/// Feeds fetched at once by the worker.
const PARALLEL_FETCHES: usize = 4;

/// `[rss]`, read from the sections the config does not type
/// (`Config::rest`): the section is new and owned here.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub enabled: bool,
    pub max_articles: i64,
    pub refresh_secs: i64,
}

pub fn settings(cfg: &crate::config::Config) -> Settings {
    let t = cfg.rest.get("rss").and_then(|v| v.as_table());
    let int = |k: &str| t.and_then(|t| t.get(k)).and_then(|v| v.as_integer());
    Settings {
        enabled: t.and_then(|t| t.get("enabled")).and_then(|v| v.as_bool()).unwrap_or(true),
        max_articles: int("max_articles").unwrap_or(DEFAULT_MAX_ARTICLES).clamp(1, 10_000),
        refresh_secs: int("refresh_minutes").map(|m| m * 60).unwrap_or(DEFAULT_REFRESH_SECS).max(MIN_REFRESH_SECS),
    }
}

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

/// Why a feed or rule operation was refused, with the status the native API
/// answers. The shim answers 409 for all but `Bad`, as qBittorrent does.
#[derive(Debug, Clone, PartialEq)]
pub enum Refusal {
    Bad(String),
    NotFound(String),
    Conflict(String),
    Failed(String),
}

impl Refusal {
    pub fn message(&self) -> &str {
        match self {
            Refusal::Bad(m) | Refusal::NotFound(m) | Refusal::Conflict(m) | Refusal::Failed(m) => m,
        }
    }

    pub fn status(&self) -> StatusCode {
        match self {
            Refusal::Bad(_) => StatusCode::BAD_REQUEST,
            Refusal::NotFound(_) => StatusCode::NOT_FOUND,
            Refusal::Conflict(_) => StatusCode::CONFLICT,
            Refusal::Failed(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

fn failed(e: impl std::fmt::Display) -> Refusal {
    Refusal::Failed(format!("store: {e}"))
}

// ---------------------------------------------------------------------------
// URLs
// ---------------------------------------------------------------------------

/// A URL as it may appear in a log or an error: scheme, host and path, the
/// query replaced by `?***`, credentials dropped, and any path segment that
/// looks like a key (16+ letters and digits, some digits) replaced by `***`.
/// A magnet keeps its info hash only: its `tr=` can carry a passkey too.
pub fn mask_url(url: &str) -> String {
    let u = url.trim();
    if u.get(..7).is_some_and(|p| p.eq_ignore_ascii_case("magnet:")) {
        let xt = u.split(['?', '&']).find(|p| p.starts_with("xt="));
        return match xt {
            Some(xt) => format!("magnet:?{xt}&***"),
            None => "magnet:?***".into(),
        };
    }
    let Some((scheme, rest)) = u.split_once("://") else {
        return "<url>".into();
    };
    let (before, had_query) = match rest.find(['?', '#']) {
        Some(i) => (&rest[..i], true),
        None => (rest, false),
    };
    let (authority, path) = match before.find('/') {
        Some(i) => (&before[..i], &before[i..]),
        None => (before, ""),
    };
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let secret = |s: &str| {
        s.len() >= 16
            && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            && s.chars().any(|c| c.is_ascii_digit())
    };
    let path: Vec<&str> = path.split('/').map(|seg| if secret(seg) { "***" } else { seg }).collect();
    format!("{scheme}://{host}{}{}", path.join("/"), if had_query { "?***" } else { "" })
}

pub(crate) fn is_magnet(u: &str) -> bool {
    u.len() > 8 && u.get(..8).is_some_and(|p| p.eq_ignore_ascii_case("magnet:?"))
}

fn is_http(u: &str) -> bool {
    let l = u.to_ascii_lowercase();
    l.starts_with("http://") || l.starts_with("https://")
}

// ---------------------------------------------------------------------------
// The way out
// ---------------------------------------------------------------------------

/// Every engine's way out, read once for a pass.
pub struct Exits {
    plan: crate::killswitch::Plan,
    proxy: Route,
}

impl Exits {
    pub fn new(state: &AppState) -> Exits {
        let cfg = state.cfg();
        Exits { plan: crate::killswitch::plan(&cfg), proxy: crate::killswitch::proxy_route(&cfg) }
    }

    /// The route the engine's announces take, and its description; why not
    /// when the kill switch keeps it off the network.
    ///
    /// Built from `[daemon] egress = "engine:<id>"`'s own resolution, so the
    /// tunnel, interface or SOCKS5 proxy are the engine's, and then given the
    /// engine's `announce_proxy` when it has one (the announces' route, which
    /// may differ from the peers'), else `TYPHON_ANNOUNCE_PROXY` as the
    /// tracker client falls back to it.
    pub fn of(&self, state: &AppState, engine: &str) -> Result<(Route, String), String> {
        if let Some(why) = state.engines.get(engine).and_then(|e| e.blocked.get()) {
            return Err(format!("engine {engine} is blocked by the kill switch ({why})"));
        }
        let (mut route, mut via) = if self.plan.verdict(engine).is_some() {
            let exits: Vec<_> = self.plan.engines.iter().map(|v| v.exit.clone()).collect();
            let r = typhon_engine::egress::resolve(
                &self.plan.mode,
                self.plan.armed,
                &format!("engine:{engine}"),
                &self.proxy,
                &exits,
            );
            if r.kind == "refused" {
                return Err(r.route.refusal);
            }
            (r.route, r.via)
        } else if self.plan.armed {
            return Err(format!("engine {engine} is not one this node runs, and the kill switch is on"));
        } else {
            (Route::default(), "direct (the host's default route)".to_string())
        };
        // The running engine's, which is what its announces use.
        let announce_proxy = state
            .engines
            .get(engine)
            .map(|e| e.session.announce_proxy.trim().to_string())
            .unwrap_or_default();
        if !announce_proxy.is_empty() {
            via = format!(
                "the announce proxy of engine {engine} ({})",
                typhon_engine::tracker::http::redact_proxy(&announce_proxy)
            );
            route.proxy = announce_proxy;
        } else if route.proxy.is_empty() {
            if let Some(env) = typhon_engine::tracker::http::env_announce_proxy() {
                via = format!("TYPHON_ANNOUNCE_PROXY ({})", typhon_engine::tracker::http::redact_proxy(env));
                route.proxy = env.to_string();
            }
        }
        Ok((route, via))
    }
}

/// The engine a feed is fetched for: the one its first enabled rule adds to,
/// else the one an add without a category goes to. A feed shared by rules
/// for two engines is fetched by the first one's way out; the tracker sees
/// that address anyway, announcing for those torrents.
pub fn feed_engine(state: &AppState, feed_url: &str, rules: &[Rule]) -> String {
    let rule = rules.iter().find(|r| r.enabled && r.affected_feeds.iter().any(|u| u == feed_url));
    crate::api::placement(state, rule.map(|r| r.category.as_str()).unwrap_or(""), "").0
}

/// GET `url` by `route`, at most `max` bytes, with the client's User-Agent.
/// Errors name the URL masked, never whole.
pub async fn fetch(route: &Route, url: &str, max: usize) -> Result<Vec<u8>, String> {
    let shown = mask_url(url);
    let client = typhon_engine::egress::client_for(route)?;
    let mut resp = client
        .get(url)
        .timeout(FETCH_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("{shown}: {}", e.without_url()))?;
    if !resp.status().is_success() {
        return Err(format!("{shown} answered {}", resp.status()));
    }
    if resp.content_length().is_some_and(|n| n as usize > max) {
        return Err(format!("{shown} is larger than {max} bytes"));
    }
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| format!("{shown}: {}", e.without_url()))? {
        if out.len() + chunk.len() > max {
            return Err(format!("{shown} is larger than {max} bytes"));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// A path's last component: the feed's or folder's name.
pub fn name_of(path: &str) -> &str {
    path.rsplit(SEP).next().unwrap_or(path)
}

/// `a\b\c` → `a\b`; empty at the top.
pub fn parent_of(path: &str) -> &str {
    path.rfind(SEP).map(|i| &path[..i]).unwrap_or("")
}

fn under(path: &str, folder: &str) -> bool {
    folder.is_empty() || (path.len() > folder.len() && path.starts_with(folder) && path[folder.len()..].starts_with(SEP))
}

/// Segments trimmed; an empty one refused.
fn clean_path(p: &str) -> Result<String, Refusal> {
    let parts: Vec<&str> = p.split(SEP).map(str::trim).collect();
    if parts.iter().any(|s| s.is_empty()) {
        return Err(Refusal::Bad(format!("{p:?} is not a valid item path (empty name)")));
    }
    Ok(parts.join(&SEP.to_string()))
}

fn put_parents(store: &crate::store::Store, path: &str) -> Result<(), Refusal> {
    let mut p = parent_of(path);
    while !p.is_empty() {
        store.rss_folder_put(p).map_err(failed)?;
        p = parent_of(p);
    }
    Ok(())
}

fn item_exists(feeds: &[RssFeedRow], folders: &[String], path: &str) -> bool {
    feeds.iter().any(|f| f.path == path) || folders.iter().any(|f| f == path)
}

fn new_uid() -> String {
    use rand::Rng;
    let b: [u8; 16] = rand::thread_rng().gen();
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{{{}-{}-{}-{}-{}}}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

// ---------------------------------------------------------------------------
// Feeds
// ---------------------------------------------------------------------------

pub fn feeds(state: &AppState) -> Vec<RssFeedRow> {
    state.store.lock().ok().and_then(|s| s.rss_feeds().ok()).unwrap_or_default()
}

pub fn folders(state: &AppState) -> Vec<String> {
    state.store.lock().ok().and_then(|s| s.rss_folders().ok()).unwrap_or_default()
}

/// Add a feed. `path` is its place and name (`Folder\Name`); empty = the
/// URL's host, never the whole URL (it would show the passkey wherever the
/// name is shown). Missing folders are created.
pub fn add_feed(state: &AppState, url: &str, path: &str, refresh_secs: Option<i64>) -> Result<RssFeedRow, Refusal> {
    let url = url.trim();
    if !is_http(url) {
        return Err(Refusal::Bad("a feed URL must be http(s)".into()));
    }
    let s = settings(&state.cfg());
    let store = state.store.lock().map_err(|_| Refusal::Failed("store lock".into()))?;
    let feeds = store.rss_feeds().map_err(failed)?;
    let folders = store.rss_folders().map_err(failed)?;
    if let Some(f) = feeds.iter().find(|f| f.url == url) {
        return Err(Refusal::Conflict(format!("this feed URL is already added, as {:?}", f.path)));
    }
    let path = if path.trim().is_empty() {
        let host = mask_url(url).split("://").nth(1).unwrap_or("feed").split('/').next().unwrap_or("feed").to_string();
        let mut name = host.clone();
        let mut n = 2;
        while item_exists(&feeds, &folders, &name) {
            name = format!("{host} ({n})");
            n += 1;
        }
        name
    } else {
        clean_path(path)?
    };
    if item_exists(&feeds, &folders, &path) {
        return Err(Refusal::Conflict(format!("an item already exists at {path:?}")));
    }
    if feeds.iter().any(|f| under(&path, &f.path)) {
        return Err(Refusal::Conflict(format!("{path:?} would be inside a feed")));
    }
    put_parents(&store, &path)?;
    let mut row = RssFeedRow {
        uid: new_uid(),
        path,
        url: url.to_string(),
        refresh_secs: refresh_secs.unwrap_or(s.refresh_secs).max(MIN_REFRESH_SECS),
        created_at: crate::store::now_secs(),
        ..Default::default()
    };
    row.id = store.rss_feed_insert(&row).map_err(failed)?;
    drop(store);
    tracing::info!(feed = %row.path, url = %mask_url(&row.url), "rss: feed added");
    wake().notify_one();
    Ok(row)
}

/// A folder. Its parents are created; an existing one is a conflict.
pub fn add_folder(state: &AppState, path: &str) -> Result<(), Refusal> {
    let path = clean_path(path)?;
    let store = state.store.lock().map_err(|_| Refusal::Failed("store lock".into()))?;
    let feeds = store.rss_feeds().map_err(failed)?;
    let folders = store.rss_folders().map_err(failed)?;
    if item_exists(&feeds, &folders, &path) {
        return Err(Refusal::Conflict(format!("an item already exists at {path:?}")));
    }
    if feeds.iter().any(|f| under(&path, &f.path)) {
        return Err(Refusal::Conflict(format!("{path:?} would be inside a feed")));
    }
    put_parents(&store, &path)?;
    store.rss_folder_put(&path).map_err(failed)
}

/// Remove a feed, or a folder with everything under it. How many feeds went.
pub fn remove_item(state: &AppState, path: &str) -> Result<usize, Refusal> {
    let path = path.trim();
    if path.is_empty() {
        return Err(Refusal::Bad("the root folder cannot be removed".into()));
    }
    let store = state.store.lock().map_err(|_| Refusal::Failed("store lock".into()))?;
    let feeds = store.rss_feeds().map_err(failed)?;
    let folders = store.rss_folders().map_err(failed)?;
    if let Some(f) = feeds.iter().find(|f| f.path == path) {
        store.rss_feed_delete(f.id).map_err(failed)?;
        tracing::info!(feed = %f.path, "rss: feed removed");
        return Ok(1);
    }
    if !folders.iter().any(|f| f == path) {
        return Err(Refusal::NotFound(format!("no feed or folder at {path:?}")));
    }
    let mut n = 0;
    for f in feeds.iter().filter(|f| under(&f.path, path)) {
        store.rss_feed_delete(f.id).map_err(failed)?;
        n += 1;
    }
    for d in folders.iter().filter(|d| *d == path || under(d, path)) {
        store.rss_folder_delete(d).map_err(failed)?;
    }
    tracing::info!(folder = %path, feeds = n, "rss: folder removed");
    Ok(n)
}

/// Move or rename a feed or a folder (qBittorrent's `moveItem`).
pub fn move_item(state: &AppState, from: &str, to: &str) -> Result<(), Refusal> {
    let from = from.trim();
    let to = clean_path(to)?;
    if from == to {
        return Ok(());
    }
    let store = state.store.lock().map_err(|_| Refusal::Failed("store lock".into()))?;
    let feeds = store.rss_feeds().map_err(failed)?;
    let folders = store.rss_folders().map_err(failed)?;
    if item_exists(&feeds, &folders, &to) {
        return Err(Refusal::Conflict(format!("an item already exists at {to:?}")));
    }
    if under(&to, from) {
        return Err(Refusal::Conflict("a folder cannot be moved into itself".into()));
    }
    if feeds.iter().any(|f| under(&to, &f.path)) {
        return Err(Refusal::Conflict(format!("{to:?} would be inside a feed")));
    }
    if let Some(f) = feeds.iter().find(|f| f.path == from) {
        put_parents(&store, &to)?;
        let mut f = f.clone();
        f.path = to;
        store.rss_feed_update(&f).map_err(failed)?;
        return Ok(());
    }
    if !folders.iter().any(|d| d == from) {
        return Err(Refusal::NotFound(format!("no feed or folder at {from:?}")));
    }
    let moved = |p: &str| format!("{to}{}", &p[from.len()..]);
    put_parents(&store, &to)?;
    for d in folders.iter().filter(|d| *d == from || under(d, from)) {
        store.rss_folder_delete(d).map_err(failed)?;
        store.rss_folder_put(&moved(d)).map_err(failed)?;
    }
    for f in feeds.iter().filter(|f| under(&f.path, from)) {
        let mut f = f.clone();
        f.path = moved(&f.path);
        store.rss_feed_update(&f).map_err(failed)?;
    }
    Ok(())
}

/// Change a feed's URL; the rules that named the old one follow it.
pub fn set_feed_url(state: &AppState, path: &str, url: &str) -> Result<RssFeedRow, Refusal> {
    let url = url.trim();
    if !is_http(url) {
        return Err(Refusal::Bad("a feed URL must be http(s)".into()));
    }
    let store = state.store.lock().map_err(|_| Refusal::Failed("store lock".into()))?;
    let feeds = store.rss_feeds().map_err(failed)?;
    let mut f = feeds.iter().find(|f| f.path == path.trim()).cloned().ok_or_else(|| Refusal::NotFound(format!("no feed at {path:?}")))?;
    if f.url == url {
        return Ok(f);
    }
    if feeds.iter().any(|o| o.url == url) {
        return Err(Refusal::Conflict("this feed URL is already added".into()));
    }
    let old = std::mem::replace(&mut f.url, url.to_string());
    f.last_refresh = 0;
    store.rss_feed_update(&f).map_err(failed)?;
    for (name, doc) in store.rss_rules().map_err(failed)? {
        let Ok(v) = serde_json::from_str::<Value>(&doc) else { continue };
        let Ok(mut r) = Rule::from_json(&name, &v) else { continue };
        if r.affected_feeds.iter().any(|u| *u == old) {
            for u in r.affected_feeds.iter_mut().filter(|u| **u == old) {
                *u = url.to_string();
            }
            store.rss_rule_put(&name, &r.to_json().to_string()).map_err(failed)?;
        }
    }
    drop(store);
    wake().notify_one();
    Ok(f)
}

/// The native edit: any of path, URL and interval.
pub fn update_feed(state: &AppState, id: i64, path: Option<&str>, url: Option<&str>, refresh_secs: Option<i64>) -> Result<RssFeedRow, Refusal> {
    let find = |state: &AppState| feeds(state).into_iter().find(|f| f.id == id).ok_or_else(|| Refusal::NotFound(format!("no feed {id}")));
    let f = find(state)?;
    if let Some(p) = path.filter(|p| !p.trim().is_empty()) {
        move_item(state, &f.path, p)?;
    }
    if let Some(u) = url.filter(|u| !u.trim().is_empty()) {
        let f = find(state)?;
        set_feed_url(state, &f.path, u)?;
    }
    if let Some(secs) = refresh_secs {
        let mut f = find(state)?;
        f.refresh_secs = secs.max(MIN_REFRESH_SECS);
        state.store.lock().map_err(|_| Refusal::Failed("store lock".into()))?.rss_feed_update(&f).map_err(failed)?;
    }
    find(state)
}

/// The feeds at or under a path; empty = every feed.
pub fn feeds_at(state: &AppState, path: &str) -> Result<Vec<RssFeedRow>, Refusal> {
    let path = path.trim();
    let all = feeds(state);
    if path.is_empty() {
        return Ok(all);
    }
    if let Some(f) = all.iter().find(|f| f.path == path) {
        return Ok(vec![f.clone()]);
    }
    if !folders(state).iter().any(|d| d == path) {
        return Err(Refusal::NotFound(format!("no feed or folder at {path:?}")));
    }
    Ok(all.into_iter().filter(|f| under(&f.path, path)).collect())
}

/// Mark read: one article of a feed, or everything at or under `path`.
pub fn mark_read(state: &AppState, path: &str, article: Option<&str>) -> Result<usize, Refusal> {
    let targets = feeds_at(state, path)?;
    let store = state.store.lock().map_err(|_| Refusal::Failed("store lock".into()))?;
    let mut n = 0;
    for f in &targets {
        n += store.rss_mark_read(f.id, article.filter(|a| !a.is_empty())).map_err(failed)?;
    }
    Ok(n)
}

/// Feeds being fetched right now (qBittorrent's `isLoading`), by store and
/// id: two stores in one process (the tests) number their feeds alike.
fn loading() -> &'static Mutex<HashSet<(usize, i64)>> {
    static L: OnceLock<Mutex<HashSet<(usize, i64)>>> = OnceLock::new();
    L.get_or_init(Default::default)
}

fn loading_key(state: &AppState, id: i64) -> (usize, i64) {
    (std::sync::Arc::as_ptr(&state.store) as usize, id)
}

pub fn is_loading(state: &AppState, id: i64) -> bool {
    loading().lock().map(|l| l.contains(&loading_key(state, id))).unwrap_or(false)
}

struct Loading((usize, i64));

impl Drop for Loading {
    fn drop(&mut self) {
        if let Ok(mut l) = loading().lock() {
            l.remove(&self.0);
        }
    }
}

/// Why a fetch or an add is refused while RSS is switched off.
pub(crate) const DISABLED: &str = "RSS is disabled ([rss] enabled = false): nothing is fetched or added";

/// Fetch one feed now, store what it holds, and run the rules on it. The
/// feed's row as it stands after; a failed fetch is in its `last_error`,
/// not an `Err` (that is for a feed that does not exist).
pub async fn refresh_feed(state: &AppState, id: i64) -> Result<RssFeedRow, Refusal> {
    let feed = feeds(state).into_iter().find(|f| f.id == id).ok_or_else(|| Refusal::NotFound(format!("no feed {id}")))?;
    // `enabled = false` means nothing is fetched: a refresh by hand, or the
    // one a new feed asks for, fetched all the same.
    if !settings(&state.cfg()).enabled {
        return Err(Refusal::Conflict(DISABLED.into()));
    }
    let _guard = {
        let mut l = loading().lock().map_err(|_| Refusal::Failed("lock".into()))?;
        let key = loading_key(state, id);
        if !l.insert(key) {
            // Already on its way: the caller gets the row as it is.
            return Ok(feed);
        }
        Loading(key)
    };
    let cfg = state.cfg();
    let s = settings(&cfg);
    let rules = load_rules(state);
    let engine = feed_engine(state, &feed.url, &rules);
    let exits = Exits::new(state);
    let now = crate::store::now_secs();
    let fetched = match exits.of(state, &engine) {
        Err(why) => Err(format!("not fetched: the way out of engine {engine} is closed ({why})")),
        Ok((route, _)) => match fetch(&route, &feed.url, MAX_FEED_BYTES).await {
            Ok(bytes) => parse::parse(&bytes).map_err(|e| format!("{}: {e}", mask_url(&feed.url))),
            Err(e) => Err(e),
        },
    };
    let mut fresh = 0;
    {
        let store = state.store.lock().map_err(|_| Refusal::Failed("store lock".into()))?;
        // Removed while it was fetched: nothing to store.
        if !store.rss_feeds().map_err(failed)?.iter().any(|f| f.id == id) {
            return Err(Refusal::NotFound(format!("no feed {id}")));
        }
        match &fetched {
            Ok(parsed) => {
                let mut seen = HashSet::new();
                let rows: Vec<RssArticleRow> = parsed
                    .articles
                    .iter()
                    .filter(|a| seen.insert(a.guid.clone()))
                    .map(|a| RssArticleRow {
                        feed_id: id,
                        guid: a.guid.clone(),
                        title: a.title.clone(),
                        torrent_url: a.torrent_url.clone(),
                        link: a.link.clone(),
                        description: a.description.clone(),
                        author: a.author.clone(),
                        category: a.category.clone(),
                        date: if a.date > 0 { a.date } else { now },
                        size: a.size,
                        is_read: false,
                        seen_at: now,
                    })
                    .collect();
                fresh = store.rss_articles_merge(id, &rows, s.max_articles).map_err(failed)?.len();
                store.rss_feed_refreshed(id, &parsed.title, now, "").map_err(failed)?;
            }
            Err(e) => store.rss_feed_refreshed(id, "", now, e).map_err(failed)?,
        }
    }
    match &fetched {
        Ok(p) => tracing::debug!(feed = %feed.path, articles = p.articles.len(), new = fresh, "rss: refreshed"),
        Err(e) => tracing::warn!(feed = %feed.path, error = %e, "rss: refresh failed"),
    }
    // After every good fetch, not only one that brought something new: an
    // article whose add failed for a network reason is still unread and
    // untaken, and this is the "next pass" it was promised.
    if fetched.is_ok() && s.enabled {
        run_rules(state, Some(id)).await;
    }
    feeds(state).into_iter().find(|f| f.id == id).ok_or_else(|| Refusal::NotFound(format!("no feed {id}")))
}

/// Fetch these feeds in the background (qBittorrent's `refreshItem`).
pub fn refresh_soon(state: &AppState, ids: Vec<i64>) {
    for id in ids {
        let st = state.clone();
        tokio::spawn(async move {
            let _ = refresh_feed(&st, id).await;
        });
    }
}

// ---------------------------------------------------------------------------
// Rules
// ---------------------------------------------------------------------------

/// Every stored rule, in the order they are offered an article: priority,
/// then name. A document that no longer reads is skipped with a warning.
pub fn load_rules(state: &AppState) -> Vec<Rule> {
    let docs = state.store.lock().ok().and_then(|s| s.rss_rules().ok()).unwrap_or_default();
    let mut rules: Vec<Rule> = docs
        .iter()
        .filter_map(|(name, doc)| {
            let parsed = serde_json::from_str::<Value>(doc).map_err(|e| e.to_string()).and_then(|v| Rule::from_json(name, &v));
            match parsed {
                Ok(r) => Some(r),
                Err(e) => {
                    tracing::warn!(rule = %name, "rss: stored rule skipped: {e}");
                    None
                }
            }
        })
        .collect();
    rules.sort_by(|a, b| a.priority.cmp(&b.priority).then_with(|| a.name.cmp(&b.name)));
    rules
}

/// Create or replace a rule (qBittorrent's `setRule`). A document without
/// `lastMatch` / `previouslyMatchedEpisodes` keeps the stored ones: an
/// editor that does not show them must not reset `ignoreDays` and the smart
/// filter by saving.
pub fn put_rule(state: &AppState, name: &str, def: &Value) -> Result<Rule, Refusal> {
    let name = name.trim();
    if name.is_empty() {
        return Err(Refusal::Bad("a rule needs a name".into()));
    }
    let mut rule = Rule::from_json(name, def).map_err(Refusal::Bad)?;
    let store = state.store.lock().map_err(|_| Refusal::Failed("store lock".into()))?;
    let old = store
        .rss_rules()
        .map_err(failed)?
        .into_iter()
        .find(|(n, _)| n == name)
        .and_then(|(_, d)| serde_json::from_str::<Value>(&d).ok())
        .and_then(|v| Rule::from_json(name, &v).ok());
    if let Some(old) = old {
        if def.get("lastMatch").is_none() {
            rule.last_match = old.last_match;
        }
        if def.get("previouslyMatchedEpisodes").is_none() {
            rule.previously_matched = old.previously_matched;
        }
        // Tags only exist in `torrentParams`, which the RSS tab's form (and a
        // 4.x document) does not send: a save from it emptied them.
        if def.get("torrentParams").and_then(|t| t.get("tags")).is_none() {
            rule.tags = old.tags;
        }
    }
    store.rss_rule_put(name, &rule.to_json().to_string()).map_err(failed)?;
    drop(store);
    rules_changed();
    Ok(rule)
}

pub fn rename_rule(state: &AppState, old: &str, new: &str) -> Result<(), Refusal> {
    let new = new.trim();
    if new.is_empty() {
        return Err(Refusal::Bad("a rule needs a name".into()));
    }
    let store = state.store.lock().map_err(|_| Refusal::Failed("store lock".into()))?;
    let rules = store.rss_rules().map_err(failed)?;
    if !rules.iter().any(|(n, _)| n == old) {
        return Err(Refusal::NotFound(format!("no rule {old:?}")));
    }
    if old != new && rules.iter().any(|(n, _)| n == new) {
        return Err(Refusal::Conflict(format!("a rule named {new:?} already exists")));
    }
    store.rss_rule_rename(old, new).map_err(failed)?;
    Ok(())
}

pub fn remove_rule(state: &AppState, name: &str) -> Result<(), Refusal> {
    let store = state.store.lock().map_err(|_| Refusal::Failed("store lock".into()))?;
    if store.rss_rule_delete(name).map_err(failed)? {
        Ok(())
    } else {
        Err(Refusal::NotFound(format!("no rule {name:?}")))
    }
}

/// A rule took an article: remember when and which episode, read back from
/// the store so an edit saved during the pass is kept.
fn bump_rule(state: &AppState, name: &str, at: i64, episode: &str) {
    let Ok(store) = state.store.lock() else { return };
    let Some((_, doc)) = store.rss_rules().ok().and_then(|r| r.into_iter().find(|(n, _)| n == name)) else { return };
    let Ok(mut r) = serde_json::from_str::<Value>(&doc).map_err(|e| e.to_string()).and_then(|v| Rule::from_json(name, &v)) else {
        return;
    };
    r.last_match = at;
    if r.smart_filter && !episode.is_empty() && !r.previously_matched.iter().any(|e| e == episode) {
        r.previously_matched.push(episode.to_string());
    }
    let _ = store.rss_rule_put(name, &r.to_json().to_string());
}

/// The articles a rule would take, feed by feed (qBittorrent's
/// `matchingArticles`, which ignores `enabled`): with what took each one
/// already, if anything did.
pub fn matching(state: &AppState, rule: &Rule) -> Vec<(RssFeedRow, Vec<(RssArticleRow, Option<RssTaken>)>)> {
    let Ok(store) = state.store.lock() else { return Vec::new() };
    let feeds = store.rss_feeds().unwrap_or_default();
    let mut out = Vec::new();
    for f in feeds.into_iter().filter(|f| rule.affected_feeds.iter().any(|u| *u == f.url)) {
        let arts: Vec<_> = store
            .rss_articles(f.id)
            .unwrap_or_default()
            .into_iter()
            .filter(|a| rule.matches_article(&a.title, a.date).is_ok())
            .map(|a| {
                let taken = store.rss_taken(&[&a.guid, &a.torrent_url]).ok().flatten();
                (a, taken)
            })
            .collect();
        out.push((f, arts));
    }
    out
}

fn rules_dirty() -> &'static AtomicBool {
    static D: AtomicBool = AtomicBool::new(false);
    &D
}

/// A rule changed: offer the unread articles of every feed to the rules
/// again at the worker's next turn, without fetching anything.
pub fn rules_changed() {
    rules_dirty().store(true, Ordering::SeqCst);
    wake().notify_one();
}

// ---------------------------------------------------------------------------
// Adding
// ---------------------------------------------------------------------------

/// An article was added (or its torrent was already there).
#[derive(Debug, Clone, PartialEq)]
pub struct Added {
    pub info_hash: String,
    pub already: bool,
}

/// Why an article was not added. `Transient` is tried again at the next
/// pass (a network failure, a closed way out, an answer that was not a
/// torrent); `Refused` is the add path's verdict and is final.
#[derive(Debug, Clone, PartialEq)]
pub enum AddError {
    Transient(String),
    Refused(String),
}

impl AddError {
    pub fn message(&self) -> &str {
        match self {
            AddError::Transient(m) | AddError::Refused(m) => m,
        }
    }
}

/// Add one article by the API's own add: a magnet through the magnet queue,
/// a `.torrent` fetched by the target engine's way out, then placed by its
/// category like any other add.
pub async fn add_article(
    state: &AppState,
    exits: &Exits,
    a: &RssArticleRow,
    category: &str,
    save_path: &str,
    tags: &str,
    paused: bool,
) -> Result<Added, AddError> {
    let url = a.torrent_url.trim().to_string();
    let hide = |e: String| if url.is_empty() { e } else { e.replace(url.as_str(), &mask_url(&url)) };
    let (st, cat, sp, tg) = (state.clone(), category.to_string(), save_path.to_string(), tags.to_string());
    if is_magnet(&url) {
        let u = url.clone();
        let res = tokio::task::spawn_blocking(move || crate::magnets::request_with(&st, &u, &cat, &sp, &tg, paused, false, ""))
            .await
            .unwrap_or_else(|e| Err(e.to_string()));
        return match res {
            Ok(hash) => Ok(Added { info_hash: hash, already: false }),
            Err(e) if e.contains("already added") => {
                let hash = crate::magnets::parse(&url).map(|m| crate::magnets::hex(&m.info_hash)).unwrap_or_default();
                Ok(Added { info_hash: hash, already: true })
            }
            Err(e) => Err(AddError::Refused(hide(e))),
        };
    }
    if !is_http(&url) {
        return Err(AddError::Refused(if url.is_empty() {
            "the article has no torrent link".into()
        } else {
            format!("not a link this client can add: {}", mask_url(&url))
        }));
    }
    let engine = crate::api::placement(state, category, "").0;
    let (route, _) = exits
        .of(state, &engine)
        .map_err(|why| AddError::Transient(format!("not fetched: the way out of engine {engine} is closed ({why})")))?;
    let bytes = fetch(&route, &url, MAX_TORRENT_BYTES).await.map_err(AddError::Transient)?;
    if let Err(e) = typhon_engine::torrent::metainfo::parse_torrent_bytes(&bytes) {
        return Err(AddError::Transient(format!("{} is not a torrent: {e}", mask_url(&url))));
    }
    let hash = typhon_engine::torrent::metainfo::parse_torrent_bytes(&bytes)
        .map(|m| typhon_engine::torrent::hex_encode(&m.info_hash))
        .unwrap_or_default();
    let res = tokio::task::spawn_blocking(move || {
        let subfolder = st.cfg().daemon.create_torrent_folder;
        let sp = crate::api::add_save_path(&st, &bytes, &cat, "", &sp, subfolder)?;
        crate::api::add_torrent_bytes(&st, &bytes, &cat, &sp, &tg, paused, false, "")
    })
    .await
    .unwrap_or_else(|e| Err(e.to_string()));
    match res {
        Ok((h, _)) => Ok(Added { info_hash: h, already: false }),
        Err(e) if e.contains("already added") => Ok(Added { info_hash: hash, already: true }),
        Err(e) => Err(AddError::Refused(hide(e))),
    }
}

/// One line in the workflow activity trail, which the Workflows tab shows.
fn activity(state: &AppState, rule: &str, hash: &str, title: &str, outcome: &str, detail: &str) {
    let Ok(store) = state.store.lock() else { return };
    let _ = store.log_workflow_activity(&crate::store::ActivityEntry {
        at: crate::store::now_secs(),
        workflow_id: format!("rss:{rule}"),
        workflow_name: format!("RSS: {rule}"),
        info_hash: hash.to_string(),
        torrent_name: title.to_string(),
        action: "rss_add".into(),
        outcome: outcome.to_string(),
        detail: detail.to_string(),
    });
}

/// Record the outcome of one article: taken (unless transient), read,
/// logged, and in the activity trail.
fn record(state: &AppState, rule: &str, a: &RssArticleRow, res: &Result<Added, AddError>) {
    let keys = [a.guid.as_str(), a.torrent_url.as_str()];
    let now = crate::store::now_secs();
    let (taken, outcome, detail, hash) = match res {
        Ok(ad) if ad.already => (Some("already"), "skipped", "already in this client".to_string(), ad.info_hash.as_str()),
        Ok(ad) => (Some("added"), "applied", String::new(), ad.info_hash.as_str()),
        Err(AddError::Refused(e)) => (Some("refused"), "failed", e.clone(), ""),
        Err(AddError::Transient(e)) => (None, "failed", format!("{e}; tried again at the next pass"), ""),
    };
    match res {
        Ok(_) => tracing::info!(rule = %rule, article = %a.title, hash = %hash, outcome = taken.unwrap_or(""), "rss: article added"),
        Err(e) => tracing::warn!(rule = %rule, article = %a.title, reason = %e.message(), "rss: article not added"),
    }
    if let Some(t) = taken {
        if let Ok(store) = state.store.lock() {
            let outcome = if t == "refused" { format!("refused: {detail}") } else { t.to_string() };
            let _ = store.rss_take(&keys, &RssTaken { rule: rule.to_string(), at: now, info_hash: hash.to_string(), outcome });
            let _ = store.rss_mark_read(a.feed_id, Some(&a.guid));
        }
    }
    activity(state, rule, hash, &a.title, outcome, &detail);
}

/// Add one article by hand (the RSS tab's Download), with a category of the
/// operator's choice. Recorded as taken, so no rule adds it again.
pub async fn download(state: &AppState, feed_id: i64, guid: &str, category: &str, save_path: &str, paused: bool) -> Result<Added, AddError> {
    let a = state
        .store
        .lock()
        .ok()
        .and_then(|s| s.rss_articles(feed_id).ok())
        .and_then(|arts| arts.into_iter().find(|a| a.guid == guid))
        .ok_or_else(|| AddError::Refused(format!("no article {guid:?} in feed {feed_id}")))?;
    let exits = Exits::new(state);
    let res = add_article(state, &exits, &a, category, save_path, "", paused).await;
    record(state, "(manual)", &a, &res);
    res
}

/// One pass of one at a time: two refreshes ending together must not both
/// take the same article.
fn pass_lock() -> &'static tokio::sync::Mutex<()> {
    static L: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    L.get_or_init(Default::default)
}

/// Offer the unread articles nothing took yet, oldest first, to the rules.
/// `only` limits the pass to one feed. How many articles were added.
pub async fn run_rules(state: &AppState, only: Option<i64>) -> usize {
    let _pass = pass_lock().lock().await;
    let mut rules = load_rules(state);
    if rules.iter().all(|r| !r.enabled) {
        return 0;
    }
    let feeds: Vec<RssFeedRow> = feeds(state).into_iter().filter(|f| only.map_or(true, |id| f.id == id)).collect();
    let exits = Exits::new(state);
    let mut added = 0;
    for f in &feeds {
        let arts = state.store.lock().ok().and_then(|s| s.rss_articles(f.id).ok()).unwrap_or_default();
        for a in arts.iter().rev().filter(|a| !a.is_read) {
            let taken = state.store.lock().ok().and_then(|s| s.rss_taken(&[&a.guid, &a.torrent_url]).ok().flatten());
            if taken.is_some() {
                continue;
            }
            let Some((i, episode)) = rules.iter().enumerate().find_map(|(i, r)| r.decide(&f.url, &a.title, a.date).ok().map(|k| (i, k)))
            else {
                continue;
            };
            let r = rules[i].clone();
            let tags = r.tags.join(",");
            let res = add_article(state, &exits, a, &r.category, &r.save_path, &tags, r.add_paused.unwrap_or(false)).await;
            record(state, &r.name, a, &res);
            if res.is_ok() {
                added += 1;
                let now = crate::store::now_secs();
                bump_rule(state, &r.name, now, &episode);
                // The rule as it now stands, for the rest of this pass.
                rules[i].last_match = now;
                if r.smart_filter && !episode.is_empty() {
                    rules[i].previously_matched.push(episode);
                }
            }
        }
    }
    added
}

// ---------------------------------------------------------------------------
// The worker
// ---------------------------------------------------------------------------

fn wake() -> &'static tokio::sync::Notify {
    static N: OnceLock<tokio::sync::Notify> = OnceLock::new();
    N.get_or_init(tokio::sync::Notify::new)
}

/// Feeds due now: never fetched, or their interval elapsed.
fn due(state: &AppState, feeds: &[RssFeedRow], now: i64) -> Vec<i64> {
    feeds
        .iter()
        .filter(|f| f.last_refresh + f.refresh_secs.max(MIN_REFRESH_SECS) <= now && !is_loading(state, f.id))
        .map(|f| f.id)
        .collect()
}

/// One turn: fetch what is due (a few at a time), then, if a rule changed,
/// offer every feed's unread articles to the rules again.
pub async fn tick(state: &AppState) {
    use futures::StreamExt;
    if !settings(&state.cfg()).enabled {
        return;
    }
    let all = feeds(state);
    if all.is_empty() {
        rules_dirty().store(false, Ordering::SeqCst);
        return;
    }
    let ids = due(state, &all, crate::store::now_secs());
    futures::stream::iter(ids)
        .for_each_concurrent(PARALLEL_FETCHES, |id| async move {
            let _ = refresh_feed(state, id).await;
        })
        .await;
    if rules_dirty().swap(false, Ordering::SeqCst) {
        run_rules(state, None).await;
    }
}

/// The worker: a turn every 30 seconds, or at once when a feed is added or
/// a rule changes.
pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(TICK) => {}
                _ = wake().notified() => {}
            }
            tick(&state).await;
        }
    });
}

#[cfg(test)]
mod tests;
