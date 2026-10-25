//! The durable store: the same hydra.db the Go binary reads and writes.
//!
//! The schema is frozen for the whole port. Not because it is beyond criticism,
//! but because freezing it is what makes 4.0.0 reversible: an operator switches
//! image, and if anything is wrong switches back, with the same database
//! underneath and nothing to migrate. A schema improvement smuggled in along the
//! way would turn that rollback into a restore from backup.
//!
//! The columns below were read off the live production database rather than off
//! the Go source, and the two were checked against each other: a table built in
//! one CREATE and a table grown by years of ALTER can disagree on column order
//! even when the code says otherwise. Here they agree exactly, appended columns
//! included.

use rusqlite::{Connection, OpenFlags};
use std::path::Path;

/// Columns of `torrents`, in the order the production database has them.
pub const TORRENT_COLUMNS: &[&str] = &[
    "info_hash",
    "session",
    "torrent",
    "save_path",
    "category",
    "added_time",
    "completed_time",
    "total_uploaded",
    "total_downloaded",
    "paused",
    "tags",
    "content_folder",
    "pinned",
    "seeding_time",
];

/// A row of the `jobs` table.
#[derive(Debug, Clone)]
pub struct Job {
    pub id: String,
    /// `type` in SQL and in JSON; `kind` here because type is a Rust keyword.
    pub kind: String,
    pub state: String,
    pub info_hash: String,
    pub params: String,
    pub progress_bytes: i64,
    pub total_bytes: i64,
    pub error: String,
    pub created_at: i64,
    pub updated_at: i64,
}

/// One Hydra in the fleet, other than this one.
///
/// A node is an ENTIRE Hydra reached over its normal HTTP API -- not an agent
/// speaking a private protocol. That is the whole point of the model: every
/// route the fleet needs is a route this build already serves and already
/// tests, so a remote capability cannot rot separately from the local one.
#[derive(Debug, Clone, Default)]
pub struct Node {
    pub name: String,
    /// Origin only, no trailing slash: `http://10.0.0.5:8199`.
    pub url: String,
    /// The remote's own API key. It stays here and is injected server-side by
    /// the relay, so it never reaches a browser and never sits in a URL.
    pub api_key: String,
    pub enabled: bool,
    pub added_at: i64,
}

/// One torrent's hardlink measurement, as the link scanner writes it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LinkRow {
    pub info_hash: String,
    pub session: String,
    /// The save path the files were resolved under. A row measured under
    /// another one describes other files, and is ignored.
    pub save_path: String,
    pub measured_at: i64,
    pub files: i64,
    pub missing: i64,
    /// `linkindex::pack` of the per-file measurement, in file order.
    pub stats: Vec<u8>,
}

/// What the scanner needs to know about a row to decide whether it is due,
/// without reading its measurement.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LinkRowMeta {
    pub save_path: String,
    pub measured_at: i64,
    pub files: i64,
}

/// How far the link index has got, for the status line.
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize)]
pub struct LinkIndexCounts {
    pub measured: i64,
    pub files: i64,
    /// Torrents none of whose files could be read.
    pub data_missing: i64,
    /// Torrents some, but not all, of whose files could be read.
    pub partly_missing: i64,
    pub oldest: i64,
}

/// The store's half of a torrent's facts, for a workflow pass.
#[derive(Debug, Clone, Default)]
pub struct WorkflowFacts {
    pub category: String,
    pub save_path: String,
    pub added_time: f64,
    // Loaded with the row; the rules do not read it yet.
    #[allow(dead_code)]
    pub completed_time: f64,
    // Loaded with the row; the rules do not read it yet.
    #[allow(dead_code)]
    pub seeding_time: i64,
    pub tags: Vec<String>,
    pub paused: bool,
}

/// A workflow as the database holds it: metadata in columns, rule in JSON.
#[derive(Debug, Clone, Default)]
pub struct StoredWorkflow {
    pub id: String,
    pub name: String,
    /// The serialised `rules::Workflow`. Opaque here on purpose -- the store
    /// does not need to understand a condition tree to keep one.
    pub body: String,
    pub enabled: bool,
    pub position: i64,
    pub interval_secs: i64,
    pub last_run: i64,
}

/// A magnet waiting for its metadata. See `magnets`.
#[derive(Debug, Clone, PartialEq)]
pub struct MagnetRow {
    pub info_hash: String,
    pub uri: String,
    pub name: String,
    pub engine: String,
    pub category: String,
    pub save_path: String,
    pub tags: String,
    pub paused: bool,
    /// Trust the data already at the save path once the metadata is in
    /// (qBit's skip_checking). 4.3 dropped it for magnets.
    pub seed_mode: bool,
    /// What the add asked for beyond the columns above, to apply once the
    /// torrent exists: `magnets::AddOptions` as JSON, empty for none.
    pub options: String,
    pub added_at: i64,
    pub attempts: i64,
    /// Not before this time (seconds) is the next resolution started.
    pub next_try: i64,
    /// `resolving` or `failed`.
    pub state: String,
    pub error: String,
}

/// Something that happened to a torrent, waiting for the event workflows.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowEvent {
    pub id: i64,
    pub at: i64,
    /// `completed` for now: the only event there is.
    pub event: String,
    pub session: String,
    pub info_hash: String,
}

/// One line of what a workflow did, or refused to do.
///
/// The failures matter more than the successes: "why did my rule not fire" is
/// the question this table exists to answer, so a refusal is recorded with its
/// reason rather than dropped.
#[derive(Debug, Clone, Default)]
pub struct ActivityEntry {
    pub at: i64,
    pub workflow_id: String,
    pub workflow_name: String,
    pub info_hash: String,
    pub torrent_name: String,
    pub action: String,
    /// `applied`, `skipped`, `failed`, `preview`, or `dry_run_no_match`.
    pub outcome: String,
    pub detail: String,
}

/// One edit applied to many copies at once, cf `Store::bulk_edit`.
#[derive(Debug, Clone, Copy)]
pub enum BulkEdit<'a> {
    Paused(bool),
    /// Pinning holds a download slot in ONE engine, so it is per copy;
    /// unpinning clears every copy, as the single-torrent route does.
    Pinned(bool),
    Category(&'a str),
    Tags {
        tags: &'a [String],
        add: bool,
    },
}

/// One torrent as an export sees it: its `.torrent` and what the store adds.
#[derive(Debug, Clone, Default)]
pub struct ExportRow {
    pub info_hash: String,
    pub torrent: Vec<u8>,
    pub category: String,
    pub tags: Vec<String>,
    pub save_path: String,
    pub added_time: f64,
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Bring the dates of one torrent's tags in line with an edit of its tag list.
///
/// A tag in `after` and not in `before` is new: dated `now` (replacing any
/// stale row). A tag in both is left alone, row or no row -- a tag without a
/// row reads as the migration instant, and writing `now` for it here would
/// make an old tag young. A tag in `before` and not in `after` loses its row,
/// unless `others` (what the torrent's other copies wear) still has it.
fn retag_since(
    conn: &Connection,
    info_hash: &str,
    before: &[String],
    after: &[String],
    others: &[String],
    now: i64,
) -> rusqlite::Result<()> {
    for t in after {
        if !before.contains(t) && !others.contains(t) {
            conn.prepare_cached(
                "INSERT OR REPLACE INTO tag_since (info_hash, tag, since) VALUES (?1, ?2, ?3)",
            )?
            .execute(rusqlite::params![info_hash, t, now])?;
        }
    }
    for t in before {
        if !after.contains(t) && !others.contains(t) {
            conn.prepare_cached("DELETE FROM tag_since WHERE info_hash = ?1 AND tag = ?2")?
                .execute(rusqlite::params![info_hash, t])?;
        }
    }
    Ok(())
}

/// `meta` key holding the instant the `tag_since` table was first created on
/// this database. A tag worn by a torrent without a `tag_since` row is read as
/// set at that instant: "at least since then".
pub const TAG_SINCE_MIGRATED_KEY: &str = "tag_since_migrated_at";

/// The frozen schema, as the production database has it.
pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS torrents (
    info_hash TEXT NOT NULL, session TEXT NOT NULL, torrent BLOB NOT NULL,
    save_path TEXT NOT NULL DEFAULT '', category TEXT NOT NULL DEFAULT '',
    added_time REAL NOT NULL DEFAULT 0, completed_time REAL NOT NULL DEFAULT 0,
    total_uploaded INTEGER NOT NULL DEFAULT 0, total_downloaded INTEGER NOT NULL DEFAULT 0,
    paused INTEGER NOT NULL DEFAULT 0, tags TEXT NOT NULL DEFAULT '',
    content_folder INTEGER NOT NULL DEFAULT -1, pinned INTEGER NOT NULL DEFAULT 0,
    seeding_time INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (info_hash, session));
CREATE TABLE IF NOT EXISTS counters (key TEXT PRIMARY KEY, ul INTEGER NOT NULL DEFAULT 0, dl INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS tag_registry (name TEXT PRIMARY KEY);
CREATE TABLE IF NOT EXISTS tag_since (
    info_hash TEXT NOT NULL, tag TEXT NOT NULL, since INTEGER NOT NULL,
    PRIMARY KEY (info_hash, tag));
CREATE TABLE IF NOT EXISTS drain_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT, at INTEGER NOT NULL, volume TEXT NOT NULL,
    before_pct REAL NOT NULL DEFAULT 0, after_pct REAL NOT NULL DEFAULT 0,
    deleted INTEGER NOT NULL DEFAULT 0, graduated INTEGER NOT NULL DEFAULT 0,
    stuck INTEGER NOT NULL DEFAULT 0, freed INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS jobs (
    id TEXT PRIMARY KEY, type TEXT NOT NULL, state TEXT NOT NULL,
    info_hash TEXT NOT NULL DEFAULT '', params TEXT NOT NULL DEFAULT '',
    progress_bytes INTEGER NOT NULL DEFAULT 0, total_bytes INTEGER NOT NULL DEFAULT 0,
    error TEXT NOT NULL DEFAULT '', created_at INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL DEFAULT 0);
";

/// Split a `tags` column into tag names.
///
/// The column is comma-separated, EXCEPT that some rows were written with a
/// JSON array literal in it -- `["cross-seed"]` -- by an earlier importer. Read
/// literally those become a tag whose name includes the brackets and quotes,
/// which is what put `["cross-seed"]`, `["upload"]` and `cross-seed` side by
/// side in the tag chips as three different tags. Normalising on READ fixes
/// every consumer at once and leaves the stored data untouched.
fn split_tags(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();
    if trimmed.starts_with('[') {
        if let Ok(serde_json::Value::Array(items)) =
            serde_json::from_str::<serde_json::Value>(trimmed)
        {
            return items
                .iter()
                .filter_map(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
        }
    }
    // A piece that is itself a JSON list is flattened: `tags_of` once read the
    // JSON form as one comma-separated tag and wrote it back with the new one
    // appended, which left 479 rows reading `["cross-seed"],noHL` (01/10/2026)
    // -- a tag literally named `["cross-seed"]` that no filter could select.
    let mut out = Vec::new();
    for piece in trimmed.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if piece.starts_with('[') {
            if let Ok(serde_json::Value::Array(items)) =
                serde_json::from_str::<serde_json::Value>(piece)
            {
                out.extend(
                    items
                        .iter()
                        .filter_map(|v| v.as_str())
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string),
                );
                continue;
            }
        }
        out.push(piece.to_string());
    }
    out.dedup();
    out
}

/// One torrent's share of `SlimFacts`: eight bytes, no allocation.
#[derive(Clone, Copy, Default)]
pub struct SlimFact {
    /// Index into `SlimFacts::categories`; 0 means uncategorised.
    pub category_id: u16,
    /// One bit per index into `SlimFacts::tags`; 0 means untagged.
    pub tag_bits: u64,
    pub user_paused: bool,
}

/// Hashes an info hash by folding its bytes, instead of SipHash.
///
/// Info hashes are SHA-1 outputs: already uniform, and nobody can choose one
/// to collide in a table without breaking SHA-1 first. SipHash's protection is
/// wasted on them, and it was 15 % of a list request -- one lookup per torrent.
#[derive(Default, Clone, Copy)]
pub struct InfoHashHasher(u64);

impl std::hash::Hasher for InfoHashHasher {
    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(8) {
            let mut word = [0u8; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            self.0 = (self.0.rotate_left(5) ^ u64::from_le_bytes(word))
                .wrapping_mul(0x517c_c1b7_2722_0a95);
        }
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

pub type InfoHashMap<V> =
    std::collections::HashMap<[u8; 20], V, std::hash::BuildHasherDefault<InfoHashHasher>>;

/// The whole session's slim facts, with the text interned once.
#[derive(Default, Clone)]
pub struct SlimFacts {
    pub by_hash: InfoHashMap<SlimFact>,
    pub categories: Vec<String>,
    pub tags: Vec<String>,
    cat_ids: std::collections::HashMap<String, u16>,
    tag_ids: std::collections::HashMap<String, u16>,
}

impl SlimFacts {
    fn empty() -> Self {
        // Index 0 is "no category" / "no tags", so the common case stores a
        // zero and never touches the intern tables.
        SlimFacts {
            categories: vec![String::new()],
            ..Default::default()
        }
    }

    /// One row of the torrents table as the list pass sees it.
    fn fact_of(&mut self, category: String, tags_raw: &str, paused: i64) -> SlimFact {
        let category_id = self.intern_category(category);
        let tag_bits = self.tag_bits_of(split_tags(tags_raw));
        SlimFact {
            category_id,
            tag_bits,
            user_paused: paused != 0,
        }
    }

    fn intern_category(&mut self, category: String) -> u16 {
        if category.is_empty() {
            0
        } else if let Some(id) = self.cat_ids.get(&category) {
            *id
        } else {
            let id = self.categories.len() as u16;
            self.categories.push(category.clone());
            self.cat_ids.insert(category, id);
            id
        }
    }

    fn tag_bits_of(&mut self, tags: Vec<String>) -> u64 {
        let mut tag_bits: u64 = 0;
        for tag in tags {
            let id = if let Some(id) = self.tag_ids.get(&tag) {
                *id
            } else {
                // 64 distinct tags is the ceiling of the bitset. Beyond it
                // the extra tags stop being counted rather than corrupting
                // the ones already there.
                if self.tags.len() >= 64 {
                    continue;
                }
                let id = self.tags.len() as u16;
                self.tags.push(tag.clone());
                self.tag_ids.insert(tag, id);
                id
            };
            tag_bits |= 1u64 << id;
        }
        tag_bits
    }

    pub fn get(&self, hash: &[u8; 20]) -> SlimFact {
        self.by_hash.get(hash).copied().unwrap_or_default()
    }

    pub fn category(&self, id: u16) -> &str {
        self.categories
            .get(id as usize)
            .map(String::as_str)
            .unwrap_or("")
    }

    /// The id of a category by name, or None when the library has none such --
    /// which makes a filter on it match nothing, as it should.
    pub fn category_id(&self, name: &str) -> Option<u16> {
        self.categories
            .iter()
            .position(|c| c == name)
            .map(|i| i as u16)
    }

    pub fn tag_bit(&self, name: &str) -> Option<u64> {
        self.tags.iter().position(|t| t == name).map(|i| 1u64 << i)
    }
}

/// Non-empty and nothing but lowercase hex digits.
fn is_hex(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A 40-character hex info hash as its 20 raw bytes.
pub(crate) fn hex20(hex: &str) -> Option<[u8; 20]> {
    if hex.len() != 40 {
        return None;
    }
    let bytes = hex.as_bytes();
    let mut out = [0u8; 20];
    for (i, slot) in out.iter_mut().enumerate() {
        let hi = (bytes[i * 2] as char).to_digit(16)?;
        let lo = (bytes[i * 2 + 1] as char).to_digit(16)?;
        *slot = (hi * 16 + lo) as u8;
    }
    Some(out)
}

/// A store lock held longer than this is logged, with the line that took it.
const LOCK_WARN: std::time::Duration = std::time::Duration::from_millis(200);

/// The store behind its lock, and a second, read-only connection beside it.
///
/// Every route and worker shares one connection, so one slow statement holds
/// up all the others -- which is how the seed-time sync made every add, tag and
/// list page wait 0.6 s for twelve minutes an hour, found only by probing
/// from outside. `lock()` now reports any hold longer than `LOCK_WARN` with
/// the caller's file and line, so the next one names itself.
///
/// `read()` is for long reads. In WAL a reader does not block the writer, but
/// only on a connection of its own: through the shared one it still queues
/// behind every write and holds every write up.
pub struct StoreLock {
    inner: std::sync::Mutex<Store>,
    reader: Option<std::sync::Mutex<Store>>,
    /// When the connections were last reopened (or tried to be), so a store
    /// that cannot be reopened is retried at a bounded pace.
    last_heal: std::sync::Mutex<Option<std::time::Instant>>,
    /// What the last ping of the connections found, and when. Pinged apart
    /// from any request (the `store-heal` thread, or `refreshed` when nothing
    /// ran it recently): `probe`, which `/health` reads, only looks here and
    /// at the holders, so it never touches the file.
    last_ping: std::sync::Mutex<Option<(std::time::Instant, Result<(), String>)>>,
    /// A refresh is running (the one at a time that may be stuck on the mount).
    refreshing: std::sync::atomic::AtomicBool,
    /// Counts the refreshes that ended, for the requests waiting for one.
    refreshed: tokio::sync::watch::Sender<u64>,
    /// Who holds each connection, since when: what the watchdog and `/health`
    /// read while the holder is still inside its query.
    inner_hold: HoldSlot,
    reader_hold: HoldSlot,
    /// Shared with the connections' `ping`, which sleeps this long first.
    /// Read without taking any connection, so a test can set it while a
    /// ping is stuck. For tests; 0 = off.
    #[cfg(test)]
    ping_stall_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

#[cfg(test)]
thread_local! {
    /// Make `Store::ping` on this thread sleep this long first, the way a
    /// query on a hung mount does. For tests; 0 = off.
    pub static PING_STALL_MS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// The current holder of one connection.
struct Hold {
    since: std::time::Instant,
    at: &'static std::panic::Location<'static>,
    warned: bool,
}
type HoldSlot = std::sync::Mutex<Option<Hold>>;

/// A connection held this long is reported to the log while it is still held.
pub const LOCK_STUCK_WARN: std::time::Duration = std::time::Duration::from_secs(10);

/// `/health` calls the store unavailable once its only connection has been
/// held this long by one request.
const UNAVAILABLE_AFTER: std::time::Duration = std::time::Duration::from_secs(5);

/// A ping result older than this is asked again before `/health` reads it.
const PING_FRESH: std::time::Duration = std::time::Duration::from_secs(2);

/// How long a request waits for the refresh it started: the store answers or
/// the holder shows, and past this the request answers with what it knows.
const REFRESH_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

/// The least time between two attempts to reopen the store's connections
/// when a ping fails.
const HEAL_EVERY: std::time::Duration = std::time::Duration::from_secs(5);

/// How the store answers a probe that never waits for it.
#[derive(Debug, Clone, PartialEq)]
pub enum Probe {
    Ok,
    /// Someone holds the connection the probe wanted; not a failure.
    Busy,
    /// Held by one request for longer than any healthy statement takes.
    Blocked(std::time::Duration),
    Error(String),
}

/// What pinging one connection found.
enum Beat {
    Alive,
    /// Somebody holds it: not pinged.
    Held,
    Broken(String),
}

/// A held store. Derefs to `Store`; logs on release if it was held too long.
pub struct StoreGuard<'a> {
    guard: std::sync::MutexGuard<'a, Store>,
    at: &'static std::panic::Location<'static>,
    waited: std::time::Duration,
    since: std::time::Instant,
    slot: &'a HoldSlot,
}

impl StoreLock {
    #[cfg(test)]
    pub fn new(store: Store) -> Self {
        Self::with_reader(store, None)
    }

    pub fn with_reader(store: Store, reader: Option<Store>) -> Self {
        #[cfg(test)]
        let (mut store, mut reader) = (store, reader);
        #[cfg(test)]
        let ping_stall_ms = {
            let shared = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            store.ping_stall_ms = shared.clone();
            if let Some(r) = reader.as_mut() {
                r.ping_stall_ms = shared.clone();
            }
            shared
        };
        StoreLock {
            #[cfg(test)]
            ping_stall_ms,
            inner: std::sync::Mutex::new(store),
            reader: reader.map(std::sync::Mutex::new),
            last_heal: Default::default(),
            last_ping: Default::default(),
            refreshing: Default::default(),
            refreshed: tokio::sync::watch::channel(0).0,
            inner_hold: Default::default(),
            reader_hold: Default::default(),
        }
    }

    /// What the store is known to do, without touching it: who holds the
    /// connection and since when, and what the last ping (made elsewhere,
    /// see `heal`) found. A connection somebody holds is `Busy`, never an
    /// error, until one request has held the only connection for
    /// `UNAVAILABLE_AFTER`.
    ///
    /// Never pings and never takes a connection: on a frozen mount the ping is
    /// what hangs, and a probe that made it hung with it.
    pub fn probe(&self) -> Probe {
        if self.inner.is_poisoned() || self.reader.as_ref().is_some_and(|r| r.is_poisoned()) {
            return Probe::Error(
                "the store's lock is poisoned: a thread panicked holding it".into(),
            );
        }
        // With a read connection beside it (a local WAL store) a long read is
        // ordinary and the writer is not what is probed; with the shared
        // connection alone, one request holding it for seconds is everything
        // waiting behind it.
        let alone = self.reader.is_none();
        let held = Self::held_for(if alone {
            &self.inner_hold
        } else {
            &self.reader_hold
        });
        if alone {
            if let Some(d) = held {
                return if d >= UNAVAILABLE_AFTER {
                    Probe::Blocked(d)
                } else {
                    Probe::Busy
                };
            }
        }
        let last = self
            .last_ping
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        match last {
            Some((_, Err(e))) => Probe::Error(e),
            Some((_, Ok(()))) if held.is_none() => Probe::Ok,
            // Held, or not pinged yet: nothing is known to be wrong.
            _ => Probe::Busy,
        }
    }

    /// Make sure the last ping is recent before `/health` reads it, without
    /// ever pinging on the caller's own path: a stale result starts a refresh
    /// on a thread of its own (one at a time) and this waits for it, in the
    /// async sense, for `REFRESH_WAIT` at most. A ping stuck on a frozen mount
    /// outlasts that, and the holder it leaves is what `probe` then reports.
    pub async fn refreshed(self: &std::sync::Arc<Self>) {
        let recent = self
            .last_ping
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|(at, _)| at.elapsed() < PING_FRESH);
        if recent {
            return;
        }
        let mut ended = self.refreshed.subscribe();
        self.heal();
        let _ = tokio::time::timeout(REFRESH_WAIT, ended.changed()).await;
    }

    fn held_for(slot: &HoldSlot) -> Option<std::time::Duration> {
        slot.lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|h| h.since.elapsed())
    }

    /// Log, once per hold, each connection that has been held for `after` and
    /// is still held: the holder's line, while it is stuck and not after. A
    /// query hung on a dead mount never gets to release the guard that would
    /// have reported it. Returns how many holds were reported now.
    pub fn watch_holds(&self, after: std::time::Duration) -> usize {
        let mut n = 0;
        for (which, slot) in [("writer", &self.inner_hold), ("reader", &self.reader_hold)] {
            let mut g = slot.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(h) = g.as_mut() {
                if !h.warned && h.since.elapsed() >= after {
                    h.warned = true;
                    n += 1;
                    tracing::warn!(
                        target: "hydranos::store_lock",
                        connection = which,
                        held_secs = h.since.elapsed().as_secs(),
                        at = %h.at,
                        "store held and still held: every request that needs it waits (a query stuck on a hung mount?)"
                    );
                }
            }
        }
        n
    }

    /// Ask for the connections to be pinged, and reopened if they no longer
    /// answer, on a thread of its own, and return at once. For a request that
    /// met a store error: the ping and the reopening are what hang on a frozen
    /// mount, and they are not made on the request's path. One at a time.
    pub fn heal(self: &std::sync::Arc<Self>) {
        if !self.claim_refresh() {
            return;
        }
        let me = self.clone();
        let spawned = std::thread::Builder::new()
            .name("store-refresh".into())
            .spawn(move || me.run_claimed(true));
        if let Err(e) = spawned {
            tracing::warn!("no store refresh: {e}");
            self.refreshing
                .store(false, std::sync::atomic::Ordering::Release);
        }
    }

    fn claim_refresh(&self) -> bool {
        self.refreshing
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
    }

    /// Run the refresh `claim_refresh` granted, and say it ended.
    fn run_claimed(&self, paced: bool) {
        struct Ends<'a>(&'a StoreLock);
        impl Drop for Ends<'_> {
            fn drop(&mut self) {
                self.0
                    .refreshing
                    .store(false, std::sync::atomic::Ordering::Release);
                self.0.refreshed.send_modify(|n| *n += 1);
            }
        }
        let _ends = Ends(self);
        self.refresh(paced);
    }

    /// Close and reopen the connections that no longer answer, then check
    /// that they do. Returns whether every connection answers afterwards.
    ///
    /// For the day a network mount reconnects: the connections opened before
    /// keep descriptors on the old session and fail every query with
    /// "malformed" or "not a database" while the file is intact. A fresh
    /// connection to the same path works. The file is only ever opened, never
    /// repaired. A connection someone holds is left to them. The result is
    /// what `probe` reports. Without pacing, for tests.
    #[track_caller]
    #[cfg(test)]
    pub fn heal_now(&self) -> bool {
        self.refresh(false)
    }

    /// Ping each connection, reopen the ones that fail (paced: at most once
    /// every `HEAL_EVERY`), and keep what was found.
    #[track_caller]
    fn refresh(&self, paced: bool) -> bool {
        let may_reopen = || {
            if !paced {
                return true;
            }
            let mut last = self.last_heal.lock().unwrap_or_else(|e| e.into_inner());
            if last.is_some_and(|t| t.elapsed() < HEAL_EVERY) {
                return false;
            }
            *last = Some(std::time::Instant::now());
            true
        };
        let mut beats = vec![Self::heal_one(
            &self.inner,
            &self.inner_hold,
            "writer",
            &may_reopen,
        )];
        if let Some(r) = &self.reader {
            beats.push(Self::heal_one(r, &self.reader_hold, "reader", &may_reopen));
        }
        let broken = beats.iter().find_map(|b| match b {
            Beat::Broken(e) => Some(e.clone()),
            _ => None,
        });
        let held = beats.iter().any(|b| matches!(b, Beat::Held));
        // A connection somebody holds was not pinged: its holder is what
        // `probe` says about it, and nothing new is known.
        if broken.is_some() || !held {
            let found = broken.clone().map_or(Ok(()), Err);
            *self.last_ping.lock().unwrap_or_else(|e| e.into_inner()) =
                Some((std::time::Instant::now(), found));
        }
        broken.is_none()
    }

    #[track_caller]
    fn heal_one(
        m: &std::sync::Mutex<Store>,
        slot: &HoldSlot,
        which: &str,
        may_reopen: &dyn Fn() -> bool,
    ) -> Beat {
        // Held through a guard, so a ping or a reopening stuck on a frozen
        // mount shows in `/health` and in the watchdog like any request.
        let mut s = match Self::try_take(m, slot, std::panic::Location::caller()) {
            Ok(s) => s,
            Err(std::sync::TryLockError::WouldBlock) => return Beat::Held,
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Beat::Broken(
                    "the store's lock is poisoned: a thread panicked holding it".into(),
                )
            }
        };
        let Err(why) = s.ping() else {
            return Beat::Alive;
        };
        if !may_reopen() {
            return Beat::Broken(why);
        }
        tracing::warn!(connection = which, error = %why, "store connection broken, reopening it");
        match s
            .reopen()
            .and_then(|()| s.ping().map_err(anyhow::Error::msg))
        {
            Ok(()) => {
                tracing::info!(connection = which, "store connection reopened");
                Beat::Alive
            }
            Err(e) => {
                tracing::warn!(connection = which, error = %e, "store connection still broken after reopening");
                Beat::Broken(e.to_string())
            }
        }
    }

    #[track_caller]
    pub fn lock(&self) -> std::sync::LockResult<StoreGuard<'_>> {
        Self::take(
            &self.inner,
            &self.inner_hold,
            std::panic::Location::caller(),
        )
    }

    /// The read-only connection when there is one, the shared one otherwise.
    #[track_caller]
    pub fn read(&self) -> std::sync::LockResult<StoreGuard<'_>> {
        let at = std::panic::Location::caller();
        match &self.reader {
            Some(reader) => Self::take(reader, &self.reader_hold, at),
            None => Self::take(&self.inner, &self.inner_hold, at),
        }
    }

    /// Break the writer's connection and the reader's (see
    /// `Store::break_connection_for_tests`). For tests; no effect otherwise.
    #[cfg(test)]
    pub fn break_connections_for_tests(&self) {
        self.inner.lock().unwrap().break_connection_for_tests();
        if let Some(r) = &self.reader {
            r.lock().unwrap().break_connection_for_tests();
        }
    }

    /// Make the `ping` of this store's connections sleep `ms` first, whichever
    /// thread pings (`PING_STALL_MS` is the pinging thread's own, and a
    /// request's thread is not known beforehand). For tests; 0 = off.
    #[cfg(test)]
    pub fn stall_pings_for_tests(&self, ms: u64) {
        self.ping_stall_ms
            .store(ms, std::sync::atomic::Ordering::Relaxed);
    }

    /// The read connection if it is free right now, `None` if someone holds
    /// it. For a health probe: a probe that queued behind a long read would
    /// report the store as broken when it is only busy, and pile up blocked
    /// threads while it waited.
    #[track_caller]
    // No caller today. Kept, not deleted: clearing lints is not the moment to drop code.
    #[allow(dead_code)]
    pub fn try_read(&self) -> Option<Result<StoreGuard<'_>, ()>> {
        let m = self.reader.as_ref().unwrap_or(&self.inner);
        let at = std::panic::Location::caller();
        let slot = if self.reader.is_some() {
            &self.reader_hold
        } else {
            &self.inner_hold
        };
        let wrap = |guard| {
            Self::held(StoreGuard {
                guard,
                at,
                waited: std::time::Duration::ZERO,
                since: std::time::Instant::now(),
                slot,
            })
        };
        match m.try_lock() {
            Ok(g) => Some(Ok(wrap(g))),
            Err(std::sync::TryLockError::WouldBlock) => None,
            Err(std::sync::TryLockError::Poisoned(_)) => Some(Err(())),
        }
    }

    /// Record the holder of a connection that was just locked.
    fn held(g: StoreGuard<'_>) -> StoreGuard<'_> {
        *g.slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(Hold {
            since: g.since,
            at: g.at,
            warned: false,
        });
        g
    }

    /// `take` without waiting: the connection if it is free now, its holder
    /// recorded like any other.
    fn try_take<'a>(
        m: &'a std::sync::Mutex<Store>,
        slot: &'a HoldSlot,
        at: &'static std::panic::Location<'static>,
    ) -> Result<StoreGuard<'a>, std::sync::TryLockError<()>> {
        let wrap = |guard| {
            Self::held(StoreGuard {
                guard,
                at,
                waited: std::time::Duration::ZERO,
                since: std::time::Instant::now(),
                slot,
            })
        };
        match m.try_lock() {
            Ok(g) => Ok(wrap(g)),
            Err(std::sync::TryLockError::WouldBlock) => Err(std::sync::TryLockError::WouldBlock),
            Err(std::sync::TryLockError::Poisoned(_)) => Err(std::sync::TryLockError::Poisoned(
                std::sync::PoisonError::new(()),
            )),
        }
    }

    fn take<'a>(
        m: &'a std::sync::Mutex<Store>,
        slot: &'a HoldSlot,
        at: &'static std::panic::Location<'static>,
    ) -> std::sync::LockResult<StoreGuard<'a>> {
        let asked = std::time::Instant::now();
        let wrap = |guard| {
            Self::held(StoreGuard {
                guard,
                at,
                waited: asked.elapsed(),
                since: std::time::Instant::now(),
                slot,
            })
        };
        let got = match m.try_lock() {
            Ok(g) => Ok(g),
            Err(std::sync::TryLockError::Poisoned(p)) => Err(p),
            // Held: the wait may last as long as a query stuck on a frozen
            // mount. On a runtime worker it hands the worker's other tasks
            // to a new thread first, or every request that needs the store
            // takes a worker down with it and nothing else runs -- /health
            // included. Free, the lock costs what it always did.
            Err(std::sync::TryLockError::WouldBlock) => match tokio::runtime::Handle::try_current()
            {
                Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
                    tokio::task::block_in_place(|| m.lock())
                }
                _ => m.lock(),
            },
        };
        match got {
            Ok(g) => Ok(wrap(g)),
            Err(p) => Err(std::sync::PoisonError::new(wrap(p.into_inner()))),
        }
    }
}

impl StoreGuard<'_> {
    /// How long this guard waited for the lock before it was granted.
    pub fn waited(&self) -> std::time::Duration {
        self.waited
    }
}

impl std::ops::Deref for StoreGuard<'_> {
    type Target = Store;
    fn deref(&self) -> &Store {
        &self.guard
    }
}

impl std::ops::DerefMut for StoreGuard<'_> {
    fn deref_mut(&mut self) -> &mut Store {
        &mut self.guard
    }
}

impl Drop for StoreGuard<'_> {
    fn drop(&mut self) {
        *self.slot.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let held = self.since.elapsed();
        if held >= LOCK_WARN {
            tracing::warn!(
                target: "hydranos::store_lock",
                held_ms = held.as_millis() as u64,
                waited_ms = self.waited.as_millis() as u64,
                at = %self.at,
                "store held long"
            );
        }
    }
}

/// A background write the store refused: said at ERROR, never swallowed.
/// For the places that have nobody to answer (a job, a worker); a route
/// answers 5xx instead.
pub fn log_store<T, E: std::fmt::Display>(what: &str, r: Result<T, E>) -> Option<T> {
    match r {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::error!(what, error = %e, "the store refused a write: the change is not saved");
            None
        }
    }
}

/// Look after the shared store. Every 2 s, report a connection held for
/// `LOCK_STUCK_WARN` while it is still held (`StoreLock::watch_holds`); every
/// 2 s, ping the connections and reopen the ones that do not answer (at the
/// `HEAL_EVERY` pace; `StoreLock::probe` reads what that found), so a store
/// comes back after its mount did even when nothing is asking it anything,
/// and `/health` never pings it itself.
///
/// Two threads: a reopening check stuck on a frozen mount must not stop the
/// watchdog that reports it.
pub fn spawn_watch(lock: std::sync::Arc<StoreLock>) {
    let watcher = lock.clone();
    let spawned = std::thread::Builder::new()
        .name("store-watch".into())
        .spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(2));
            watcher.watch_holds(LOCK_STUCK_WARN);
        });
    if let Err(e) = spawned {
        tracing::warn!("no store watcher: {e}");
    }
    let spawned = std::thread::Builder::new()
        .name("store-heal".into())
        .spawn(move || loop {
            std::thread::sleep(PING_FRESH);
            if lock.claim_refresh() {
                lock.run_claimed(true);
            }
        });
    if let Err(e) = spawned {
        tracing::warn!("no store reopening check: {e}");
    }
}

/// Copy the WAL back into the database once a second, on a connection of its
/// own, so no write ever pays for it.
///
/// Left to SQLite, the checkpoint runs inside whichever commit crosses the
/// threshold, and that commit carries every fsync: measured at 438 ms at p99
/// for a writer on the production copy. PASSIVE never waits for a writer or a
/// reader; what it cannot copy this second it copies the next.
pub fn spawn_checkpointer(path: &Path) {
    let path = path.to_path_buf();
    let spawned = std::thread::Builder::new()
        .name("store-checkpoint".into())
        .spawn(move || {
            let conn = match Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_WRITE) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("no checkpointer, SQLite's own will do: {e}");
                    return;
                }
            };
            let mut failing = false;
            loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
                match checkpoint(&conn) {
                    Ok(()) => failing = false,
                    Err(e) if !failing => {
                        failing = true;
                        tracing::warn!("checkpoint: {e}");
                    }
                    Err(_) => {}
                }
            }
        });
    if let Err(e) = spawned {
        tracing::warn!("no checkpointer, SQLite's own will do: {e}");
    }
}

fn checkpoint(conn: &Connection) -> rusqlite::Result<()> {
    conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()))
}

/// Rows written since the last list request above which the list facts are
/// read whole again rather than row by row.
const SLIM_REREAD_MAX: usize = 100_000;

pub struct Store {
    conn: Connection,
    /// Where this connection was opened, to open it again. `None` in memory.
    path: Option<std::path::PathBuf>,
    read_only: bool,
    /// `slim_facts` per session, kept current by `slim_facts_current`.
    slim: std::collections::HashMap<String, std::sync::Arc<SlimFacts>>,
    slim_version: i64,
    tracks_changes: bool,
    /// When this connection was opened: what a tag without a date reads as on
    /// a store that never recorded its migration instant.
    opened_at: i64,
    /// Make this store's `ping` sleep this many ms first, from whichever
    /// thread pings it. For tests; 0 = off.
    #[cfg(test)]
    ping_stall_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Copies (`info_hash/session`) this process wrote a row for, and when.
    /// The add path writes the row BEFORE the engine holds the torrent, so for
    /// a moment a new row looks exactly like an orphan; the reconcile asks here
    /// before calling it one. In memory on purpose: `added_time` is no witness,
    /// an import carries the date of the client it comes from.
    fresh: std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
}

impl Store {
    /// Open the store. `read_only` is what the parity bench uses: pointing the
    /// candidate at a copy of production and having it write there would make
    /// the comparison unrepeatable from the second run onwards.
    pub fn open(path: &Path, read_only: bool) -> anyhow::Result<Self> {
        let flags = if read_only {
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI
        } else {
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_URI
        };
        if !read_only && crate::privfiles::enabled() {
            crate::privfiles::precreate(path);
        }
        let conn = Connection::open_with_flags(path, flags)?;
        let mut store = Self::bare(conn);
        store.path = Some(path.to_path_buf());
        store.read_only = read_only;
        // 3.x applies its CREATE TABLE IF NOT EXISTS on every open, so a fresh
        // install comes up with an empty but complete database rather than
        // refusing to start. Reproduced here; read-only opens skip it, since a
        // bench pointed at a copy of production must not write to it.
        if !read_only {
            store.ensure_schema()?;
            store.track_changes();
            store.prefer_wal(path);
            if crate::privfiles::enabled() {
                // The -wal and -shm exist by now; the next ones SQLite makes
                // take the database file's mode.
                for p in crate::privfiles::store_files(path) {
                    crate::privfiles::tighten(&p);
                }
            }
            // 64 MB of page cache for the writer, against SQLite's 2 MB
            // default. A flag change moves an entry in `idx_torrents_cover`
            // (it carries paused, tags and category), and with 2 MB those
            // index pages are read back from the OS for every few rows:
            // measured on the bench, 50 000 paused flags took 1.69 s, 1.47 s
            // of it in the kernel; with 64 MB, 0.59 s.
            if let Err(e) = store.conn.execute_batch("PRAGMA cache_size=-65536;") {
                tracing::warn!("store page cache left at the default: {e}");
            }
        }
        Ok(store)
    }

    /// Put the file in WAL mode, unless it lives on a network share.
    ///
    /// Measured on a copy of production (1M torrents, 6 GB): a tag write
    /// 1.4 ms in the rollback journal, 0.08 ms in WAL with synchronous=NORMAL;
    /// and a writer waiting behind a one-second read, 1.2 s at p99 in the
    /// rollback journal, where WAL lets it through. NORMAL gives up only the
    /// last few seconds of commits to a power cut (never to a crash of the
    /// daemon, and never corruption), which a seedbox can afford.
    ///
    /// A share cannot hold a WAL database -- the shared memory it needs does
    /// not cross SMB or NFS, cf `walrepair` -- so there the file keeps the
    /// rollback journal. The mode is stored in the file; `synchronous` is not,
    /// and is set on every open.
    fn prefer_wal(&self, path: &Path) {
        if path.parent().is_some_and(crate::platform::is_network_fs) {
            return;
        }
        match self
            .conn
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get::<_, String>(0))
        {
            Ok(mode) if mode.eq_ignore_ascii_case("wal") => {
                // The checkpointer thread keeps the WAL short; SQLite's own
                // automatic checkpoint, which runs INSIDE the commit that
                // trips it, is only a backstop at 64 MB.
                if let Err(e) = self.conn.execute_batch(
                    "PRAGMA synchronous=NORMAL;
                     PRAGMA wal_autocheckpoint=16384;
                     PRAGMA journal_size_limit=134217728;",
                ) {
                    tracing::warn!("store in WAL but not tuned: {e}");
                }
            }
            Ok(mode) => tracing::warn!(mode = %mode, "store stays in its journal mode"),
            Err(e) => tracing::warn!("store stays in its journal mode: {e}"),
        }
    }

    /// The journal mode SQLite reports for this file.
    /// Whether the database answers a query at all.
    pub fn ping(&self) -> Result<(), String> {
        #[cfg(test)]
        {
            let ms = PING_STALL_MS.with(|c| c.get());
            if ms > 0 {
                std::thread::sleep(std::time::Duration::from_millis(ms));
            }
            let ms = self
                .ping_stall_ms
                .load(std::sync::atomic::Ordering::Relaxed);
            if ms > 0 {
                std::thread::sleep(std::time::Duration::from_millis(ms));
            }
        }
        self.conn
            .query_row("SELECT count(*) FROM sqlite_master", [], |r| {
                r.get::<_, i64>(0)
            })
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Close this connection and open a new one on the same file, the way
    /// `open` did. What the store holds in memory about rows is dropped (it is
    /// read again on the next list). The file is never repaired.
    pub fn reopen(&mut self) -> anyhow::Result<()> {
        let Some(path) = self.path.clone() else {
            anyhow::bail!("an in-memory store has nothing to reopen");
        };
        // Never `open`: it creates the file and its tables. A mount that
        // dropped can leave an empty folder where the database was, and a
        // fresh store there would answer every ping while the real one is
        // hidden under the mount once it is back.
        if !path.is_file() {
            anyhow::bail!(
                "{} is not there: not creating a new store in its place",
                path.display()
            );
        }
        let flags = if self.read_only {
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI
        } else {
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI
        };
        let mut new = Self::bare(Connection::open_with_flags(&path, flags)?);
        new.path = Some(path.clone());
        new.read_only = self.read_only;
        let tables: i64 = new.conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name IN ('torrents', 'meta')",
            [],
            |r| r.get(0),
        )?;
        if tables < 2 {
            anyhow::bail!(
                "{} holds no store (its tables are missing): not reopened",
                path.display()
            );
        }
        if !self.read_only {
            new.track_changes();
            new.prefer_wal(&path);
            if let Err(e) = new.conn.execute_batch("PRAGMA cache_size=-65536;") {
                tracing::warn!("store page cache left at the default: {e}");
            }
        }
        self.conn = new.conn;
        self.tracks_changes = new.tracks_changes;
        self.slim.clear();
        self.slim_version = -1;
        Ok(())
    }

    pub fn journal_mode(&self) -> String {
        self.conn
            .query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))
            .unwrap_or_default()
            .to_lowercase()
    }

    fn bare(conn: Connection) -> Self {
        Store {
            conn,
            path: None,
            read_only: false,
            slim: Default::default(),
            slim_version: -1,
            tracks_changes: false,
            opened_at: now_secs(),
            #[cfg(test)]
            ping_stall_ms: Default::default(),
            fresh: Default::default(),
        }
    }

    /// Create anything missing. Every statement is IF NOT EXISTS, so this is a
    /// no-op against a database that already has the tables -- including one
    /// written by 3.x, which is the whole point.
    pub fn ensure_schema(&self) -> anyhow::Result<()> {
        // Asked BEFORE the schema runs, which creates the table.
        let had_tag_since = self
            .conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='tag_since'",
                [],
                |_| Ok(()),
            )
            .is_ok();
        self.conn.execute_batch(SCHEMA)?;
        self.ensure_cover_index()?;
        self.ensure_nodes_table()?;
        self.ensure_enrol_table()?;
        self.ensure_workflows_table()?;
        self.ensure_content_index()?;
        self.ensure_link_index()?;
        self.ensure_share_limits_table()?;
        self.ensure_rss_tables()?;
        // After the tables exist, and before anything reads them.
        self.migrate_composite_key()?;
        if !had_tag_since {
            // No row is written for the tags already worn: a library has
            // millions of them. One instant stands for all, cf `tag_since`.
            self.conn.execute(
                "INSERT OR IGNORE INTO meta (key, value) VALUES (?1, ?2)",
                rusqlite::params![TAG_SINCE_MIGRATED_KEY, now_secs().to_string()],
            )?;
        }
        Ok(())
    }

    /// An index that carries the columns the list reads.
    ///
    /// The `torrents` table holds the .torrent BLOB beside the metadata, so it
    /// is 4.7 GB at 300k torrents. Reading nine small columns from it means
    /// walking pages that are mostly torrent files: measured at 21 seconds for
    /// one session, 16.7 of them in the kernel. An index holding those columns
    /// answers from itself and never opens the table -- the same query drops to
    /// 0.5 seconds.
    ///
    /// Additive, and invisible to 3.x: a rollback reads the same database and
    /// simply never uses this index. Built once, in about five seconds.
    fn ensure_cover_index(&self) -> anyhow::Result<()> {
        self.conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_torrents_cover
             ON torrents(session, info_hash, category, save_path, added_time,
                         completed_time, seeding_time, tags, paused, content_folder);",
        )?;
        // `pinned` is deliberately NOT in the index above, and asking for the
        // pinned list therefore fell back to the table -- 4.7 GB of .torrent
        // BLOBs walked to read one flag per row, six seconds to answer with an
        // empty list. A PARTIAL index holds only the rows that are pinned,
        // which is a handful and usually none, so it costs almost nothing and
        // answers from itself.
        self.conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_torrents_pinned
             ON torrents(session, info_hash) WHERE pinned <> 0;",
        )?;
        // Which rows hang off a path: what an add asks to learn whether its
        // files are already another torrent's (`pathclaim.rs`). Without it
        // the answer was a walk of every torrent of every engine, measured at
        // ~190 ms per add on a million-torrent library. NOCASE so the same
        // index answers for NTFS, where two names differing by case are one
        // file; on other systems it only adds candidates, which the caller
        // checks against the real paths. Covering, so the table and its BLOBs
        // are never read.
        self.conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_torrents_save_path
             ON torrents(save_path COLLATE NOCASE, session, info_hash);",
        )?;
        Ok(())
    }

    /// (info_hash, session) of every row whose `save_path` is one of
    /// `exact`, or lies under `root` (a path starting with `root` and the
    /// separator). Compared without case, through `idx_torrents_save_path`.
    pub fn rows_by_save_path(
        &self,
        exact: &[String],
        root: &str,
        sep: char,
    ) -> anyhow::Result<Vec<(String, String)>> {
        let mut out: Vec<(String, String)> = Vec::new();
        {
            let mut q = self.conn.prepare_cached(
                "SELECT info_hash, session FROM torrents WHERE save_path = ?1 COLLATE NOCASE",
            )?;
            for p in exact {
                let rows = q.query_map([p], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?;
                for r in rows {
                    out.push(r?);
                }
            }
        }
        // Everything under `root`: from `root/` up to, not including, the
        // character after the separator.
        let lo = format!("{root}{sep}");
        let hi = format!("{root}{}", char::from_u32(sep as u32 + 1).unwrap_or(sep));
        let mut q = self.conn.prepare_cached(
            "SELECT info_hash, session FROM torrents
             WHERE save_path >= ?1 COLLATE NOCASE AND save_path < ?2 COLLATE NOCASE",
        )?;
        let rows = q.query_map([lo, hi], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        for r in rows {
            out.push(r?);
        }
        out.sort();
        out.dedup();
        Ok(out)
    }

    /// The fleet registry.
    ///
    /// Deliberately in the store and NOT in the TOML. Declaring a remote node
    /// used to mean hand-editing a config file over SSH, which is the single
    /// thing that made the old agent model unusable in practice. State that
    /// the UI creates belongs where the UI can write it.
    ///
    /// Additive and invisible to any older build, exactly like the covering
    /// index: a rollback opens the same database and never reads this table.
    fn ensure_nodes_table(&self) -> anyhow::Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS nodes (
                 name TEXT PRIMARY KEY,
                 url TEXT NOT NULL,
                 api_key TEXT NOT NULL DEFAULT '',
                 enabled INTEGER NOT NULL DEFAULT 1,
                 added_at INTEGER NOT NULL DEFAULT 0);",
        )?;
        Ok(())
    }

    /// Workflows, and the log of what they did.
    ///
    /// The rule BODY is one opaque JSON column. Only what the daemon has to sort
    /// or filter on gets a column of its own -- `enabled`, `position`,
    /// `interval_secs`, `last_run`. Modelling a condition tree in SQL would buy
    /// nothing and cost a migration every time an operator is added.
    ///
    /// Additive like the nodes table, so a rollback to a build without
    /// workflows reads the same database and simply never looks here.
    fn ensure_workflows_table(&self) -> anyhow::Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS workflows (
                 id TEXT PRIMARY KEY,
                 name TEXT NOT NULL,
                 body TEXT NOT NULL,
                 enabled INTEGER NOT NULL DEFAULT 0,
                 position INTEGER NOT NULL DEFAULT 0,
                 interval_secs INTEGER NOT NULL DEFAULT 900,
                 last_run INTEGER NOT NULL DEFAULT 0,
                 created_at INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE IF NOT EXISTS workflow_activity (
                 at INTEGER NOT NULL,
                 workflow_id TEXT NOT NULL DEFAULT '',
                 workflow_name TEXT NOT NULL DEFAULT '',
                 info_hash TEXT NOT NULL DEFAULT '',
                 torrent_name TEXT NOT NULL DEFAULT '',
                 action TEXT NOT NULL DEFAULT '',
                 outcome TEXT NOT NULL DEFAULT '',
                 detail TEXT NOT NULL DEFAULT '');
             CREATE INDEX IF NOT EXISTS idx_workflow_activity_at
                 ON workflow_activity(at DESC);
             CREATE TABLE IF NOT EXISTS app_settings (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS ip_bans (
                 ip TEXT PRIMARY KEY,
                 reason TEXT NOT NULL DEFAULT '',
                 added_at INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE IF NOT EXISTS magnets (
                 info_hash TEXT PRIMARY KEY,
                 uri TEXT NOT NULL,
                 name TEXT NOT NULL DEFAULT '',
                 engine TEXT NOT NULL,
                 category TEXT NOT NULL DEFAULT '',
                 save_path TEXT NOT NULL DEFAULT '',
                 tags TEXT NOT NULL DEFAULT '',
                 paused INTEGER NOT NULL DEFAULT 0,
                 added_at INTEGER NOT NULL DEFAULT 0,
                 attempts INTEGER NOT NULL DEFAULT 0,
                 next_try INTEGER NOT NULL DEFAULT 0,
                 state TEXT NOT NULL DEFAULT 'resolving',
                 error TEXT NOT NULL DEFAULT '',
                 seed_mode INTEGER NOT NULL DEFAULT 0,
                 options TEXT NOT NULL DEFAULT '');
             CREATE TABLE IF NOT EXISTS workflow_events (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 at INTEGER NOT NULL,
                 event TEXT NOT NULL,
                 session TEXT NOT NULL,
                 info_hash TEXT NOT NULL);",
        )?;
        // A 4.3 store has the magnets table without seed_mode.
        let has_seed_mode = self
            .conn
            .prepare("SELECT 1 FROM pragma_table_info('magnets') WHERE name = 'seed_mode'")?
            .exists([])?;
        if !has_seed_mode {
            self.conn.execute_batch(
                "ALTER TABLE magnets ADD COLUMN seed_mode INTEGER NOT NULL DEFAULT 0;",
            )?;
        }
        // Nor does a 4.4.0-rc store have its options.
        let has_options = self
            .conn
            .prepare("SELECT 1 FROM pragma_table_info('magnets') WHERE name = 'options'")?
            .exists([])?;
        if !has_options {
            self.conn.execute_batch(
                "ALTER TABLE magnets ADD COLUMN options TEXT NOT NULL DEFAULT '';",
            )?;
        }
        Ok(())
    }

    /// Per-torrent share limits, qBittorrent's `setShareLimits`.
    ///
    /// SPARSE, one row per torrent that has a limit of its own, keyed by hash
    /// and not by copy (a qBit client names a hash, never an engine). A
    /// torrent that follows its engine has no row, so the share-limit worker
    /// and the listings read a table of a few rows, not one per torrent of a
    /// million-torrent library. -2 = follow the engine, -1 = no limit.
    ///
    /// Additive: a rollback opens the same database and never looks here.
    fn ensure_share_limits_table(&self) -> anyhow::Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS share_limits (
                 info_hash TEXT PRIMARY KEY,
                 ratio REAL NOT NULL DEFAULT -2,
                 seeding_minutes INTEGER NOT NULL DEFAULT -2,
                 inactive_minutes INTEGER NOT NULL DEFAULT -2);",
        )?;
        Ok(())
    }

    /// Every torrent's own share limits, by hash. A store without the table
    /// (a read-only bench copy) answers an error the callers read as "none".
    pub fn share_limits_all(
        &self,
    ) -> anyhow::Result<std::collections::HashMap<String, crate::sharelimits::TorrentShareLimits>>
    {
        let mut stmt = self.conn.prepare(
            "SELECT info_hash, ratio, seeding_minutes, inactive_minutes FROM share_limits",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                crate::sharelimits::TorrentShareLimits {
                    ratio: r.get(1)?,
                    seeding_minutes: r.get(2)?,
                    inactive_minutes: r.get(3)?,
                },
            ))
        })?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    /// One torrent's own share limits; all -2 when it has none.
    pub fn share_limits_of(&self, info_hash: &str) -> crate::sharelimits::TorrentShareLimits {
        self.conn
            .query_row(
                "SELECT ratio, seeding_minutes, inactive_minutes FROM share_limits WHERE info_hash = ?1",
                [info_hash],
                |r| {
                    Ok(crate::sharelimits::TorrentShareLimits {
                        ratio: r.get(0)?,
                        seeding_minutes: r.get(1)?,
                        inactive_minutes: r.get(2)?,
                    })
                },
            )
            .unwrap_or_default()
    }

    /// Set share limits on a set of torrents, in ONE transaction (`hashes=all`
    /// over a large library is one call). `None` keeps a field as it is; a
    /// row back to all -2 is deleted, so the table stays as sparse as the
    /// limits are.
    pub fn set_share_limits_batch(
        &self,
        hashes: &[String],
        ratio: Option<f64>,
        seeding_minutes: Option<i64>,
        inactive_minutes: Option<i64>,
    ) -> Result<usize, rusqlite::Error> {
        if hashes.is_empty() {
            return Ok(0);
        }
        let tx = self.conn.unchecked_transaction()?;
        let mut n = 0usize;
        {
            let mut upsert = tx.prepare_cached(
                "INSERT INTO share_limits (info_hash, ratio, seeding_minutes, inactive_minutes)
                 VALUES (?1, COALESCE(?2, -2), COALESCE(?3, -2), COALESCE(?4, -2))
                 ON CONFLICT(info_hash) DO UPDATE SET
                     ratio = COALESCE(?2, ratio),
                     seeding_minutes = COALESCE(?3, seeding_minutes),
                     inactive_minutes = COALESCE(?4, inactive_minutes)",
            )?;
            let mut prune = tx.prepare_cached(
                "DELETE FROM share_limits WHERE info_hash = ?1
                 AND ratio = -2 AND seeding_minutes = -2 AND inactive_minutes = -2",
            )?;
            for hash in hashes {
                n += upsert.execute(rusqlite::params![
                    hash,
                    ratio,
                    seeding_minutes,
                    inactive_minutes
                ])?;
                prune.execute([hash])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// A setting the UI writes (JSON). Kept here rather than in the TOML,
    /// whose editor refuses keys the file does not already have: a feature
    /// added in a release would be unreachable on every existing install.
    pub fn setting(&self, key: &str) -> anyhow::Result<Option<String>> {
        use rusqlite::OptionalExtension;
        Ok(self
            .conn
            .query_row(
                "SELECT value FROM app_settings WHERE key = ?1",
                rusqlite::params![key],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn put_setting(&self, key: &str, value: &str) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO app_settings (key, value) VALUES (?1, ?2)",
            rusqlite::params![key, value],
        )?;
        Ok(())
    }

    pub fn put_ban(&self, ip: &str, reason: &str) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO ip_bans (ip, reason, added_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![ip, reason, now_secs()],
        )?;
        Ok(())
    }

    /// `(ip or range, reason, added_at)`.
    pub fn bans(&self) -> anyhow::Result<Vec<(String, String, i64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT ip, reason, added_at FROM ip_bans ORDER BY added_at")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    pub fn delete_ban(&self, ip: &str) -> anyhow::Result<bool> {
        Ok(self
            .conn
            .execute("DELETE FROM ip_bans WHERE ip = ?1", rusqlite::params![ip])?
            > 0)
    }

    pub fn put_magnet(&self, m: &MagnetRow) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO magnets
                 (info_hash, uri, name, engine, category, save_path, tags, paused,
                  added_at, attempts, next_try, state, error, seed_mode, options)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
            rusqlite::params![
                m.info_hash,
                m.uri,
                m.name,
                m.engine,
                m.category,
                m.save_path,
                m.tags,
                i64::from(m.paused),
                m.added_at,
                m.attempts,
                m.next_try,
                m.state,
                m.error,
                i64::from(m.seed_mode),
                m.options
            ],
        )?;
        Ok(())
    }

    pub fn magnets(&self) -> anyhow::Result<Vec<MagnetRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT info_hash, uri, name, engine, category, save_path, tags, paused,
                    added_at, attempts, next_try, state, error, seed_mode, options
             FROM magnets ORDER BY added_at",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(MagnetRow {
                info_hash: r.get(0)?,
                uri: r.get(1)?,
                name: r.get(2)?,
                engine: r.get(3)?,
                category: r.get(4)?,
                save_path: r.get(5)?,
                tags: r.get(6)?,
                paused: r.get::<_, i64>(7)? != 0,
                added_at: r.get(8)?,
                attempts: r.get(9)?,
                next_try: r.get(10)?,
                state: r.get(11)?,
                error: r.get(12)?,
                seed_mode: r.get::<_, i64>(13)? != 0,
                options: r.get(14)?,
            })
        })?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    pub fn magnet(&self, info_hash: &str) -> anyhow::Result<Option<MagnetRow>> {
        Ok(self
            .magnets()?
            .into_iter()
            .find(|m| m.info_hash == info_hash))
    }

    pub fn delete_magnet(&self, info_hash: &str) -> anyhow::Result<bool> {
        Ok(self.conn.execute(
            "DELETE FROM magnets WHERE info_hash = ?1",
            rusqlite::params![info_hash],
        )? > 0)
    }

    /// Write down that something happened, before anything acts on it.
    ///
    /// ⭐ The event exists once, in memory, at the moment a download finishes.
    /// A restart, a crash or a busy store between that moment and the
    /// workflow running would lose it for good -- there is no second
    /// completion to wait for. A row survives all three; it is deleted only
    /// once the workflows have seen it.
    pub fn push_workflow_event(
        &self,
        event: &str,
        session: &str,
        info_hash: &str,
    ) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT INTO workflow_events (at, event, session, info_hash) VALUES (?1,?2,?3,?4)",
            rusqlite::params![now_secs(), event, session, info_hash],
        )?;
        Ok(())
    }

    /// The oldest waiting events first: they happened first.
    pub fn workflow_events(&self, limit: i64) -> anyhow::Result<Vec<WorkflowEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, at, event, session, info_hash FROM workflow_events ORDER BY id LIMIT ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![limit], |r| {
            Ok(WorkflowEvent {
                id: r.get(0)?,
                at: r.get(1)?,
                event: r.get(2)?,
                session: r.get(3)?,
                info_hash: r.get(4)?,
            })
        })?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    pub fn drop_workflow_event(&self, id: i64) -> anyhow::Result<()> {
        self.conn.execute(
            "DELETE FROM workflow_events WHERE id = ?1",
            rusqlite::params![id],
        )?;
        Ok(())
    }

    /// `workflow_facts` for one copy: the event path, where reading the whole
    /// session to answer about one torrent would be a million rows for one.
    pub fn workflow_facts_of(
        &self,
        session: &str,
        info_hash: &str,
    ) -> anyhow::Result<Option<WorkflowFacts>> {
        use rusqlite::OptionalExtension;
        Ok(self
            .conn
            .query_row(
                "SELECT category, save_path, added_time, completed_time, seeding_time, tags, paused
                 FROM torrents WHERE session = ?1 AND info_hash = ?2",
                rusqlite::params![session, info_hash],
                |r| {
                    Ok(WorkflowFacts {
                        category: r.get(0)?,
                        save_path: r.get(1)?,
                        added_time: r.get(2)?,
                        completed_time: r.get(3)?,
                        seeding_time: r.get(4)?,
                        tags: split_tags(&r.get::<_, String>(5)?),
                        paused: r.get::<_, i64>(6)? != 0,
                    })
                },
            )
            .optional()?)
    }

    /// Everything the store knows about one session's torrents, for a pass.
    ///
    /// Every column named here is in `idx_torrents_cover`, so this is an
    /// index-only scan and never opens the 4.7 GB table -- the same reason the
    /// listing is half a second instead of twenty-one.
    ///
    /// Keyed by info hash so the caller can join it to the engine's own view
    /// without a second query per torrent.
    pub fn workflow_facts(
        &self,
        session: &str,
    ) -> anyhow::Result<std::collections::HashMap<String, WorkflowFacts>> {
        let mut stmt = self.conn.prepare(
            "SELECT info_hash, category, save_path, added_time, completed_time,
                    seeding_time, tags, paused
             FROM torrents WHERE session = ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![session], |r| {
            Ok((
                r.get::<_, String>(0)?,
                WorkflowFacts {
                    category: r.get(1)?,
                    save_path: r.get(2)?,
                    added_time: r.get(3)?,
                    completed_time: r.get(4)?,
                    seeding_time: r.get(5)?,
                    tags: split_tags(&r.get::<_, String>(6)?),
                    paused: r.get::<_, i64>(7)? != 0,
                },
            ))
        })?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    pub fn workflows(&self) -> anyhow::Result<Vec<StoredWorkflow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, body, enabled, position, interval_secs, last_run
             FROM workflows ORDER BY position, created_at",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(StoredWorkflow {
                id: r.get(0)?,
                name: r.get(1)?,
                body: r.get(2)?,
                enabled: r.get::<_, i64>(3)? != 0,
                position: r.get(4)?,
                interval_secs: r.get(5)?,
                last_run: r.get(6)?,
            })
        })?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    pub fn workflow(&self, id: &str) -> anyhow::Result<Option<StoredWorkflow>> {
        Ok(self.workflows()?.into_iter().find(|w| w.id == id))
    }

    pub fn put_workflow(&self, w: &StoredWorkflow) -> anyhow::Result<()> {
        let now = now_secs();
        self.conn.execute(
            "INSERT INTO workflows (id, name, body, enabled, position, interval_secs, created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(id) DO UPDATE SET
                 name = ?2, body = ?3, enabled = ?4, position = ?5, interval_secs = ?6",
            rusqlite::params![
                w.id,
                w.name,
                w.body,
                i64::from(w.enabled),
                w.position,
                w.interval_secs,
                now
            ],
        )?;
        Ok(())
    }

    pub fn delete_workflow(&self, id: &str) -> anyhow::Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM workflows WHERE id = ?1", rusqlite::params![id])?;
        Ok(n > 0)
    }

    /// Stamp a workflow as having run, so its own interval is measured from
    /// when it last ran and not from when the daemon started.
    pub fn mark_workflow_run(&self, id: &str, at: i64) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE workflows SET last_run = ?2 WHERE id = ?1",
            rusqlite::params![id, at],
        )?;
        Ok(())
    }

    pub fn log_workflow_activity(&self, e: &ActivityEntry) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT INTO workflow_activity
                 (at, workflow_id, workflow_name, info_hash, torrent_name, action, outcome, detail)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            rusqlite::params![
                e.at,
                e.workflow_id,
                e.workflow_name,
                e.info_hash,
                e.torrent_name,
                e.action,
                e.outcome,
                e.detail
            ],
        )?;
        Ok(())
    }

    pub fn workflow_activity(&self, limit: i64) -> anyhow::Result<Vec<ActivityEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT at, workflow_id, workflow_name, info_hash, torrent_name, action, outcome, detail
             FROM workflow_activity ORDER BY at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![limit], |r| {
            Ok(ActivityEntry {
                at: r.get(0)?,
                workflow_id: r.get(1)?,
                workflow_name: r.get(2)?,
                info_hash: r.get(3)?,
                torrent_name: r.get(4)?,
                action: r.get(5)?,
                outcome: r.get(6)?,
                detail: r.get(7)?,
            })
        })?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    /// Seven days, the window qui keeps. An unbounded log of every action on a
    /// 300k catalogue is a database that grows without anybody deciding to.
    pub fn prune_workflow_activity(&self, older_than: i64) -> anyhow::Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM workflow_activity WHERE at < ?1",
            rusqlite::params![older_than],
        )?)
    }

    /// The .torrent itself, as it was added.
    ///
    /// Needed to hand a torrent to another node: the receiving Hydra has to be
    /// given the metainfo before it can be told where to fetch the data from.
    /// Reading it here rather than re-encoding from the parsed metadata keeps
    /// the info dict byte-identical, and therefore the info hash with it.
    pub fn torrent_blob(&self, info_hash: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let mut stmt = self
            .conn
            .prepare("SELECT torrent FROM torrents WHERE info_hash = ?1 LIMIT 1")?;
        let mut rows = stmt.query([info_hash.to_lowercase()])?;
        match rows.next()? {
            Some(r) => Ok(Some(r.get(0)?)),
            None => Ok(None),
        }
    }

    /// What an export needs for a batch of hashes, one row per hash.
    ///
    /// A hash held by two sessions (race and hoard) carries the same
    /// `.torrent`; the first row read wins. Hashes the store does not hold are
    /// simply absent from the answer -- the caller knows which it asked for.
    pub fn export_rows(&self, hashes: &[String]) -> anyhow::Result<Vec<ExportRow>> {
        if hashes.is_empty() {
            return Ok(Vec::new());
        }
        let marks = vec!["?"; hashes.len()].join(",");
        let mut stmt = self.conn.prepare(&format!(
            "SELECT info_hash, torrent, category, tags, save_path, added_time
             FROM torrents WHERE info_hash IN ({marks})"
        ))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(hashes.iter()), |r| {
            Ok(ExportRow {
                info_hash: r.get(0)?,
                torrent: r.get(1)?,
                category: r.get(2)?,
                tags: split_tags(&r.get::<_, String>(3)?),
                save_path: r.get(4)?,
                added_time: r.get(5)?,
            })
        })?;
        let mut out: Vec<ExportRow> = Vec::with_capacity(hashes.len());
        let mut seen = std::collections::HashSet::new();
        for row in rows {
            let row = row?;
            if seen.insert(row.info_hash.clone()) {
                out.push(row);
            }
        }
        Ok(out)
    }

    /// Apply one edit to every `(info_hash, session)` in `targets`, in ONE
    /// transaction, and bring the list facts up to date in place.
    ///
    /// The per-torrent routes each commit on their own and leave the list to
    /// re-read every row they touched on its next request, under this same
    /// lock: 15 000 tagged rows cost the next list 210 ms, and past 100 000
    /// the list reads the whole library again (1.1 s at a million). Here the
    /// new values are known, so the cached facts are patched directly and the
    /// rows this call marked dirty are dropped from the queue.
    ///
    /// Returns the number of rows that changed. Callers bound `targets` (cf
    /// `selection::BULK_TX`) so one call never holds the store for long.
    pub fn bulk_edit(
        &mut self,
        targets: &[(String, String)],
        edit: BulkEdit,
    ) -> anyhow::Result<usize> {
        if targets.is_empty() {
            return Ok(0);
        }
        let mark: i64 = if self.tracks_changes {
            self.conn.query_row(
                "SELECT coalesce(max(rowid), 0) FROM temp.slim_dirty",
                [],
                |r| r.get(0),
            )?
        } else {
            0
        };
        // The new tag list of each row that changed, for the facts below.
        let mut new_tags: Vec<(usize, Vec<String>)> = Vec::new();
        let mut changed = 0usize;
        {
            let tx = self.conn.unchecked_transaction()?;
            match edit {
                BulkEdit::Paused(p) => {
                    let mut st = tx.prepare_cached(
                        "UPDATE torrents SET paused = ?3 WHERE info_hash = ?1 AND session = ?2 AND paused != ?3",
                    )?;
                    for (h, s) in targets {
                        changed += st.execute(rusqlite::params![h, s, i64::from(p)])?;
                    }
                }
                BulkEdit::Pinned(true) => {
                    let mut st = tx.prepare_cached(
                        "UPDATE torrents SET pinned = 1 WHERE info_hash = ?1 AND session = ?2 AND pinned != 1",
                    )?;
                    for (h, s) in targets {
                        changed += st.execute(rusqlite::params![h, s])?;
                    }
                }
                BulkEdit::Pinned(false) => {
                    let mut st = tx.prepare_cached(
                        "UPDATE torrents SET pinned = 0 WHERE info_hash = ?1 AND pinned != 0",
                    )?;
                    for (h, _) in targets {
                        changed += st.execute(rusqlite::params![h])?;
                    }
                }
                BulkEdit::Category(c) => {
                    let mut st = tx.prepare_cached(
                        "UPDATE torrents SET category = ?3 WHERE info_hash = ?1 AND session = ?2 AND category != ?3",
                    )?;
                    for (h, s) in targets {
                        changed += st.execute(rusqlite::params![h, s, c])?;
                    }
                }
                BulkEdit::Tags { tags, add } => {
                    if add {
                        let mut reg = tx.prepare_cached(
                            "INSERT OR IGNORE INTO tag_registry (name) VALUES (?1)",
                        )?;
                        for t in tags {
                            reg.execute([t])?;
                        }
                    }
                    let mut get = tx.prepare_cached(
                        "SELECT session, tags FROM torrents WHERE info_hash = ?1",
                    )?;
                    let mut put = tx.prepare_cached(
                        "UPDATE torrents SET tags = ?3 WHERE info_hash = ?1 AND session = ?2",
                    )?;
                    let now = now_secs();
                    for (i, (h, s)) in targets.iter().enumerate() {
                        // One read for the torrent's copies: the dates are
                        // per torrent, so a tag another copy still wears
                        // keeps its date.
                        let copies: Vec<(String, String)> = get
                            .query_map(rusqlite::params![h], |r| Ok((r.get(0)?, r.get(1)?)))?
                            .collect::<Result<_, _>>()?;
                        let Some(raw) = copies.iter().find(|(c, _)| c == s).map(|(_, t)| t) else {
                            continue;
                        };
                        let mut list = split_tags(raw);
                        let before = list.clone();
                        if add {
                            for t in tags {
                                if !list.contains(t) {
                                    list.push(t.clone());
                                }
                            }
                        } else {
                            list.retain(|x| !tags.contains(x));
                        }
                        if list != before {
                            changed += put.execute(rusqlite::params![h, s, list.join(",")])?;
                            let others: Vec<String> = copies
                                .iter()
                                .filter(|(c, _)| c != s)
                                .flat_map(|(_, t)| split_tags(t))
                                .collect();
                            retag_since(&tx, h, &before, &list, &others, now)?;
                            new_tags.push((i, list));
                        }
                    }
                }
            }
            tx.commit()?;
        }

        if self.tracks_changes {
            self.conn
                .execute("DELETE FROM temp.slim_dirty WHERE rowid > ?1", [mark])?;
            // Patch the cached facts of every session this call wrote to.
            let tags_by_target: std::collections::HashMap<usize, Vec<String>> =
                new_tags.into_iter().collect();
            let sessions: std::collections::HashSet<&str> =
                targets.iter().map(|(_, s)| s.as_str()).collect();
            for session in sessions {
                let Some(cached) = self.slim.get_mut(session) else {
                    continue;
                };
                let facts = std::sync::Arc::make_mut(cached);
                for (i, (h, s)) in targets.iter().enumerate() {
                    if s != session {
                        continue;
                    }
                    let Some(key) = hex20(h) else { continue };
                    let Some(mut fact) = facts.by_hash.get(&key).copied() else {
                        continue;
                    };
                    match edit {
                        BulkEdit::Paused(p) => fact.user_paused = p,
                        BulkEdit::Pinned(_) => continue,
                        BulkEdit::Category(c) => {
                            fact.category_id = facts.intern_category(c.to_string())
                        }
                        BulkEdit::Tags { .. } => match tags_by_target.get(&i) {
                            Some(list) => fact.tag_bits = facts.tag_bits_of(list.clone()),
                            None => continue,
                        },
                    }
                    facts.by_hash.insert(key, fact);
                }
            }
        }
        Ok(changed)
    }

    /// Swap the connection for one that fails every query, the way a
    /// connection whose descriptors belong to a vanished network session does
    /// ("file is not a database" / "database disk image is malformed"), while
    /// the database file itself stays intact. For tests; no effect otherwise.
    #[cfg(test)]
    pub fn break_connection_for_tests(&mut self) {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let junk = std::env::temp_dir().join(format!(
            "store-junk-{}-{}.db",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::write(&junk, vec![0x5Au8; 8192]).expect("junk file");
        self.conn = Connection::open(&junk).expect("open junk");
    }

    /// Rows waiting in the list's re-read queue, for tests.
    #[cfg(test)]
    pub fn dirty_count_for_tests(&self) -> i64 {
        self.conn
            .query_row("SELECT count(*) FROM temp.slim_dirty", [], |r| r.get(0))
            .unwrap_or(-1)
    }

    /// Run raw SQL, for tests that need a row the API would never write.
    #[cfg(test)]
    pub fn execute_batch_for_tests(&self, sql: &str) {
        self.conn.execute_batch(sql).expect("test SQL");
    }

    /// One-time enrolment tokens.
    ///
    /// A node enrols itself: the operator never hands this Hydra a credential
    /// for another machine, and this Hydra never opens a session on one. The
    /// token is the whole authority, so it is single use, short lived, and the
    /// only thing that can be replayed if it leaks -- once, within its window,
    /// to register a node the operator will see in the list.
    fn ensure_enrol_table(&self) -> anyhow::Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS enrol_tokens (
                 token TEXT PRIMARY KEY,
                 created_at INTEGER NOT NULL DEFAULT 0,
                 expires_at INTEGER NOT NULL DEFAULT 0,
                 used_at INTEGER NOT NULL DEFAULT 0);",
        )?;
        Ok(())
    }

    pub fn create_enrol_token(&self, ttl_secs: i64) -> anyhow::Result<(String, i64)> {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        let token: String = (0..32)
            .map(|_| {
                const HEX: &[u8] = b"0123456789abcdef";
                HEX[rng.gen_range(0..16)] as char
            })
            .collect();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let expires = now + ttl_secs;
        self.conn.execute(
            "INSERT INTO enrol_tokens (token, created_at, expires_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![token, now, expires],
        )?;
        Ok((token, expires))
    }

    /// Could this token be spent right now? Reads only; spending is
    /// `consume_enrol_token`, which stays the one that decides.
    pub fn enrol_token_is_live(&self, token: &str) -> anyhow::Result<bool> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM enrol_tokens WHERE token = ?1 AND used_at = 0 AND expires_at > ?2",
            rusqlite::params![token, now],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Spend a token, or say why it cannot be spent.
    ///
    /// The UPDATE carries the conditions rather than a read-then-write: two
    /// nodes racing on the same token would both pass a check done separately,
    /// and both would register.
    pub fn consume_enrol_token(&self, token: &str) -> anyhow::Result<bool> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let n = self.conn.execute(
            "UPDATE enrol_tokens SET used_at = ?2
             WHERE token = ?1 AND used_at = 0 AND expires_at > ?2",
            rusqlite::params![token, now],
        )?;
        Ok(n > 0)
    }

    pub fn nodes(&self) -> anyhow::Result<Vec<Node>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name, url, api_key, enabled, added_at FROM nodes ORDER BY name")?;
        let rows = stmt.query_map([], |r| {
            Ok(Node {
                name: r.get(0)?,
                url: r.get(1)?,
                api_key: r.get(2)?,
                enabled: r.get::<_, i64>(3)? != 0,
                added_at: r.get(4)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn node(&self, name: &str) -> anyhow::Result<Option<Node>> {
        Ok(self.nodes()?.into_iter().find(|n| n.name == name))
    }

    pub fn put_node(&self, n: &Node) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT INTO nodes (name, url, api_key, enabled, added_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(name) DO UPDATE SET
                 url = excluded.url,
                 api_key = excluded.api_key,
                 enabled = excluded.enabled",
            rusqlite::params![
                n.name,
                n.url,
                n.api_key,
                if n.enabled { 1 } else { 0 },
                n.added_at
            ],
        )?;
        Ok(())
    }

    /// Returns whether a row actually went away.
    ///
    /// The caller needs this to answer honestly. The route it replaces,
    /// `delete_agent`, returned `{"status":"ok"}` unconditionally while doing
    /// nothing at all -- so the UI struck the entry off and it came back on the
    /// next reload, with no error anywhere to explain it.
    pub fn delete_node(&self, name: &str) -> anyhow::Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM nodes WHERE name = ?1", [name])?;
        Ok(n > 0)
    }

    /// Rename a node, and every waiting handoff that names it.
    ///
    /// One transaction: a handoff still waiting on `old` would otherwise find
    /// no node on its next poll and fail as if the node had been removed.
    /// Refuses (Ok(false)) when `old` does not exist or `new` is taken -- a
    /// rename must never merge two nodes.
    pub fn rename_node(&self, old: &str, new: &str) -> anyhow::Result<bool> {
        let tx = self.conn.unchecked_transaction()?;
        let taken: i64 =
            tx.query_row("SELECT COUNT(*) FROM nodes WHERE name = ?1", [new], |r| {
                r.get(0)
            })?;
        if taken > 0 {
            return Ok(false);
        }
        let n = tx.execute(
            "UPDATE nodes SET name = ?2 WHERE name = ?1",
            rusqlite::params![old, new],
        )?;
        if n == 0 {
            return Ok(false);
        }
        let waiting: Vec<(String, String)> = {
            let mut stmt = tx.prepare("SELECT id, params FROM jobs WHERE state = 'waiting'")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.filter_map(|r| r.ok()).collect()
        };
        for (id, params) in waiting {
            let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&params) else {
                continue;
            };
            if v.get("node").and_then(|x| x.as_str()) != Some(old) {
                continue;
            }
            v["node"] = serde_json::Value::String(new.to_string());
            tx.execute(
                "UPDATE jobs SET params = ?2 WHERE id = ?1",
                rusqlite::params![id, v.to_string()],
            )?;
        }
        tx.commit()?;
        Ok(true)
    }

    /// Re-key `torrents` on `(info_hash, session)`.
    ///
    /// One torrent, one row was the wrong shape: a torrent lives in an ENGINE,
    /// and a node running one engine per tunnel has a real reason to seed the
    /// same content from several of them at once. Three tunnels are three
    /// separate egress paths, so three copies are three times the upload when
    /// the tunnel is what saturates -- and they cost nothing extra on disk,
    /// because they are the same files.
    ///
    /// SQLite cannot alter a primary key, so this rebuilds the table. It runs
    /// inside a transaction: either the new table is complete or the old one is
    /// still there, and a failure cannot leave a half-copied catalogue.
    ///
    /// ⚠ This is the change that ends the 3.x rollback. 3.x reads this same
    /// file and assumes one row per info hash; two rows would show it the same
    /// torrent twice. The V4 lineage is the rollback path from here on.
    fn migrate_composite_key(&self) -> anyhow::Result<()> {
        let sql: String = self
            .conn
            .query_row(
                "SELECT COALESCE(sql, '') FROM sqlite_master WHERE type='table' AND name='torrents'",
                [],
                |r| r.get(0),
            )
            .unwrap_or_default();
        if sql.is_empty() || sql.contains("PRIMARY KEY (info_hash, session)") {
            return Ok(());
        }

        let rows: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM torrents", [], |r| r.get(0))
            .unwrap_or(0);
        tracing::warn!(
            rows,
            "re-keying the torrents table on (info_hash, session); this rewrites it and \
             ends the 3.x rollback"
        );
        let started = std::time::Instant::now();

        self.conn.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE torrents_v2 (
                 info_hash TEXT NOT NULL, session TEXT NOT NULL, torrent BLOB NOT NULL,
                 save_path TEXT NOT NULL DEFAULT '', category TEXT NOT NULL DEFAULT '',
                 added_time REAL NOT NULL DEFAULT 0, completed_time REAL NOT NULL DEFAULT 0,
                 total_uploaded INTEGER NOT NULL DEFAULT 0, total_downloaded INTEGER NOT NULL DEFAULT 0,
                 paused INTEGER NOT NULL DEFAULT 0, tags TEXT NOT NULL DEFAULT '',
                 content_folder INTEGER NOT NULL DEFAULT -1, pinned INTEGER NOT NULL DEFAULT 0,
                 seeding_time INTEGER NOT NULL DEFAULT 0,
                 PRIMARY KEY (info_hash, session));
             INSERT INTO torrents_v2
                 SELECT info_hash, session, torrent, save_path, category, added_time,
                        completed_time, total_uploaded, total_downloaded, paused, tags,
                        content_folder, pinned, seeding_time
                 FROM torrents;
             DROP TABLE torrents;
             ALTER TABLE torrents_v2 RENAME TO torrents;
             COMMIT;",
        )?;
        // The covering index went with the old table.
        self.ensure_cover_index()?;
        tracing::warn!(
            rows,
            seconds = started.elapsed().as_secs(),
            "torrents table re-keyed"
        );
        Ok(())
    }

    /// Fail loudly if the database is not the shape this build expects.
    ///
    /// A missing column would otherwise surface as a wrong value in one field of
    /// one endpoint, which is exactly the kind of difference that survives a
    /// review and reaches production.
    /// An empty store with the frozen schema applied, for tests.
    #[cfg(test)]
    pub fn open_in_memory() -> anyhow::Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        let mut store = Self::bare(conn);
        store.track_changes();
        Ok(store)
    }

    pub fn check_schema(&self) -> anyhow::Result<()> {
        let mut stmt = self.conn.prepare("PRAGMA table_info(torrents)")?;
        let found: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<_, _>>()?;

        if found.is_empty() {
            anyhow::bail!("the torrents table does not exist in this database");
        }
        let missing: Vec<&str> = TORRENT_COLUMNS
            .iter()
            .copied()
            .filter(|c| !found.iter().any(|f| f == c))
            .collect();
        if !missing.is_empty() {
            anyhow::bail!(
                "torrents is missing column(s) {:?}; found {:?}",
                missing,
                found
            );
        }
        Ok(())
    }

    /// A document stored in the `meta` table.
    ///
    /// The store is the primary source: categories and provenance live here
    /// first and only fall back to their legacy JSON file when the row is
    /// absent, which is what an install upgraded from an older layout looks
    /// like. Returning None for a missing row rather than an error is what
    /// makes that fallback expressible.
    pub fn meta_doc(&self, key: &str) -> Option<String> {
        self.conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .ok()
    }

    /// `meta_doc`, but a store that cannot be read is an error and not an
    /// absent row. A writer that edits the document must tell the two apart:
    /// taking "unreadable" for "empty" and saving the edit would replace the
    /// whole document with one entry.
    pub fn meta_doc_checked(&self, key: &str) -> anyhow::Result<Option<String>> {
        use rusqlite::OptionalExtension;
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    /// One job by id.
    /// Queue a job. Returns its id.
    ///
    /// The `jobs` table has been in the schema, served by three routes and
    /// drawn by a whole tab since the V4 port, and nothing ever inserted a
    /// row. These are the writes it was missing.
    pub fn create_job(
        &self,
        kind: &str,
        info_hash: &str,
        params: &str,
        total_bytes: i64,
    ) -> anyhow::Result<String> {
        let now = now_secs();
        let id = format!(
            "job{}{}",
            now,
            info_hash.chars().take(6).collect::<String>()
        );
        self.conn.execute(
            "INSERT INTO jobs (id, type, state, info_hash, params, progress_bytes,
                               total_bytes, error, created_at, updated_at)
             VALUES (?1, ?2, 'queued', ?3, ?4, 0, ?5, '', ?6, ?6)
             ON CONFLICT(id) DO NOTHING",
            rusqlite::params![id, kind, info_hash, params, total_bytes, now],
        )?;
        Ok(id)
    }

    /// Is there already a job of this kind in flight for this torrent?
    ///
    /// Without this a drain that runs every minute queues the same graduation
    /// sixty times while the first copy is still going.
    pub fn job_pending_for(&self, kind: &str, info_hash: &str) -> bool {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM jobs
                 WHERE type = ?1 AND info_hash = ?2 AND state IN ('queued','running','cancelling','waiting')",
                rusqlite::params![kind, info_hash],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0
    }

    /// Take the oldest queued job of any kind and mark it running.
    ///
    /// One statement, so two runners cannot claim the same row.
    pub fn claim_next_job(&self) -> Option<Job> {
        let now = now_secs();
        let id: String = self
            .conn
            .query_row(
                "UPDATE jobs SET state = 'running', updated_at = ?1
                 WHERE id = (SELECT id FROM jobs WHERE state = 'queued'
                             ORDER BY created_at LIMIT 1)
                 RETURNING id",
                rusqlite::params![now],
                |r| r.get(0),
            )
            .ok()?;
        self.job(&id)
    }

    pub fn job_progress(&self, id: &str, done: i64) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE jobs SET progress_bytes = ?2, updated_at = ?3 WHERE id = ?1",
            rusqlite::params![id, done, now_secs()],
        )?;
        Ok(())
    }

    /// Cancel a job: a queued one is cancelled at once; a running one is
    /// marked `cancelling` and its runner stops at its next check. Returns the
    /// state it was found in (`None` when there is no such job).
    pub fn cancel_job(&self, id: &str) -> Option<String> {
        let before = self.job(id)?.state;
        let now = now_secs();
        match before.as_str() {
            // A waiting job has no runner to tell: the watcher that polls it
            // only ever settles a row still `waiting` (`settle_waiting_job`),
            // so cancelling it here is the whole of the stop.
            "queued" | "waiting" => {
                let _ = self.conn.execute(
                    "UPDATE jobs SET state = 'cancelled', error = 'cancelled', updated_at = ?2 WHERE id = ?1 AND state IN ('queued', 'waiting')",
                    rusqlite::params![id, now],
                );
            }
            "running" => {
                let _ = self.conn.execute(
                    "UPDATE jobs SET state = 'cancelling', updated_at = ?2 WHERE id = ?1 AND state = 'running'",
                    rusqlite::params![id, now],
                );
            }
            _ => {}
        }
        Some(before)
    }

    /// Whether a running job has been asked to stop.
    pub fn job_cancelling(&self, id: &str) -> bool {
        self.job(id).is_some_and(|j| j.state == "cancelling")
    }

    /// Queue a job that WAITS rather than works: state `waiting`.
    ///
    /// The runner (`jobsrun`) claims `queued` rows one at a time, and a wait of
    /// hours on another machine must not hold the only slot every graduation
    /// queues behind. A waiting row is polled by its own watcher, survives a
    /// restart like any row, and is never requeued: there is nothing half done
    /// to redo.
    pub fn create_waiting_job(
        &self,
        kind: &str,
        info_hash: &str,
        params: &str,
        total_bytes: i64,
    ) -> anyhow::Result<String> {
        let id = self.create_job(kind, info_hash, params, total_bytes)?;
        self.conn.execute(
            "UPDATE jobs SET state = 'waiting' WHERE id = ?1 AND state = 'queued'",
            [&id],
        )?;
        Ok(id)
    }

    /// Every job of a kind still waiting, oldest first.
    pub fn waiting_jobs(&self, kind: &str) -> Vec<Job> {
        let Ok(mut stmt) = self.conn.prepare(
            "SELECT id FROM jobs WHERE type = ?1 AND state = 'waiting' ORDER BY created_at",
        ) else {
            return Vec::new();
        };
        let ids: Vec<String> = stmt
            .query_map([kind], |r| r.get(0))
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default();
        ids.iter().filter_map(|id| self.job(id)).collect()
    }

    /// End a waiting job, only if it is STILL waiting. Returns whether it was.
    ///
    /// Conditional on purpose: a cancel that lands between the watcher's poll
    /// and its verdict must win. The watcher acts (drops a local copy) only
    /// after this answered true, so a cancelled handoff never deletes.
    pub fn settle_waiting_job(&self, id: &str, state: &str, error: &str) -> bool {
        self.conn
            .execute(
                "UPDATE jobs SET state = ?2, error = ?3, updated_at = ?4 WHERE id = ?1 AND state = 'waiting'",
                rusqlite::params![id, state, error, now_secs()],
            )
            .unwrap_or(0)
            > 0
    }

    /// Replace a job's params (a waiting job's node was renamed, its deadline
    /// moved).
    pub fn set_job_params(&self, id: &str, params: &str) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE jobs SET params = ?2, updated_at = ?3 WHERE id = ?1",
            rusqlite::params![id, params, now_secs()],
        )?;
        Ok(())
    }

    pub fn job_finish(&self, id: &str, error: &str) -> anyhow::Result<()> {
        let state = if error.is_empty() {
            "done"
        } else if error.starts_with("cancelled") {
            "cancelled"
        } else {
            "failed"
        };
        self.conn.execute(
            "UPDATE jobs SET state = ?2, error = ?3, updated_at = ?4 WHERE id = ?1",
            rusqlite::params![id, state, error, now_secs()],
        )?;
        Ok(())
    }

    /// Put jobs that were running when the process died back in the queue.
    ///
    /// A job left "running" belongs to a process that no longer exists, so it
    /// will never finish and never fail: it would sit in the tab forever. A
    /// half-copied file is not a state to trust, so a file's copy is re-done
    /// -- `copy_then_delete` only unlinks the source once the copy is
    /// complete, so the source is still there. A graduation or move that had
    /// already taken its torrent out of the engine finishes from the record
    /// it carries.
    pub fn requeue_running_jobs(&self) -> usize {
        self.conn
            .execute(
                "UPDATE jobs SET state = 'queued', progress_bytes = 0, updated_at = ?1
                 WHERE state IN ('running', 'cancelling')",
                rusqlite::params![now_secs()],
            )
            .unwrap_or(0)
    }

    pub fn job(&self, id: &str) -> Option<Job> {
        self.conn
            .query_row(
                "SELECT id, type, state, info_hash, params, progress_bytes, total_bytes,
                        error, created_at, updated_at
                 FROM jobs WHERE id = ?1",
                [id],
                |row| {
                    Ok(Job {
                        id: row.get(0)?,
                        kind: row.get(1)?,
                        state: row.get(2)?,
                        info_hash: row.get(3)?,
                        params: row.get(4)?,
                        progress_bytes: row.get(5)?,
                        total_bytes: row.get(6)?,
                        error: row.get(7)?,
                        created_at: row.get(8)?,
                        updated_at: row.get(9)?,
                    })
                },
            )
            .ok()
    }

    /// Background jobs, newest first, as the API reports them.
    /// Write down one drain pass.
    ///
    /// A drain that leaves no trace cannot be argued with later: the operator
    /// sees a torrent gone and has nothing to read. This is the row that says
    /// which disk, when, and what it cost.
    #[allow(clippy::too_many_arguments)]
    pub fn record_drain(
        &self,
        at: i64,
        volume: &str,
        before_pct: f64,
        after_pct: f64,
        deleted: i64,
        graduated: i64,
        stuck: i64,
        freed: i64,
    ) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT INTO drain_history (at, volume, before_pct, after_pct, deleted, graduated, stuck, freed)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![at, volume, before_pct, after_pct, deleted, graduated, stuck, freed],
        )?;
        Ok(())
    }

    /// A fill percentage as a human reads it.
    ///
    /// `statvfs` arithmetic yields the full float expansion -- 95.12606489907212
    /// -- and the drain history printed every digit. Two decimals is already
    /// more precision than a disk gauge carries.
    fn pct2(v: f64) -> f64 {
        (v * 100.0).round() / 100.0
    }

    pub fn drain_history(&self, limit: i64) -> anyhow::Result<Vec<serde_json::Value>> {
        let mut stmt = self.conn.prepare(
            "SELECT at, volume, before_pct, after_pct, deleted, graduated, stuck, freed
             FROM drain_history ORDER BY at DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map([limit], |row| {
                let deleted: i64 = row.get(4)?;
                let graduated: i64 = row.get(5)?;
                Ok(serde_json::json!({
                    "timestamp": row.get::<_, i64>(0)?,
                    "volume": row.get::<_, String>(1)?,
                    // Two decimals. A disk that went from 95.12606489907212%
                    // to 89.2658601277033% is a fill level, not a measurement
                    // worth fourteen digits: the extra ones are float noise and
                    // they make the history column unreadable.
                    "before_pct": crate::row::num_json(Self::pct2(row.get::<_, f64>(2)?)),
                    "after_pct": crate::row::num_json(Self::pct2(row.get::<_, f64>(3)?)),
                    "deleted": deleted,
                    "graduated": graduated,
                    "removed_count": deleted + graduated,
                    "stuck": row.get::<_, i64>(6)?,
                    "freed": row.get::<_, i64>(7)?,
                }))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn list_jobs(&self, limit: i64) -> anyhow::Result<Vec<Job>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, type, state, info_hash, params, progress_bytes, total_bytes,
                    error, created_at, updated_at
             FROM jobs ORDER BY created_at DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map([limit], |row| {
                Ok(Job {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    state: row.get(2)?,
                    info_hash: row.get(3)?,
                    params: row.get(4)?,
                    progress_bytes: row.get(5)?,
                    total_bytes: row.get(6)?,
                    error: row.get(7)?,
                    created_at: row.get(8)?,
                    updated_at: row.get(9)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// What the store knows about every torrent of one session, by info hash.
    ///
    /// Fetched in one query rather than one per row: at 243k torrents the
    /// per-row version is 243k round trips to answer a single listing, which
    /// is the shape of problem this port exists to remove, not to reproduce.
    pub fn facts_for_session(
        &self,
        session: &str,
    ) -> anyhow::Result<std::collections::HashMap<String, crate::row::StoreFacts>> {
        let mut stmt = self.conn.prepare(
            "SELECT info_hash, category, save_path, added_time, completed_time,
                    seeding_time, tags, paused, content_folder
             FROM torrents WHERE session = ?1",
        )?;
        let mut out = std::collections::HashMap::new();
        let rows = stmt.query_map([session], |r| {
            let info_hash: String = r.get(0)?;
            let tags: String = r.get(6)?;
            let content_folder: i64 = r.get(8)?;
            Ok((
                info_hash,
                crate::row::StoreFacts {
                    category: r.get(1)?,
                    save_path: r.get(2)?,
                    // added_time/completed_time are REAL seconds in the schema;
                    // the API publishes whole seconds.
                    added_time: r.get::<_, f64>(3)? as i64,
                    completed_time: r.get::<_, f64>(4)? as i64,
                    seeding_time: r.get(5)?,
                    tags: split_tags(&tags),
                    user_paused: r.get::<_, i64>(7)? != 0,
                    // -1 is "unset" in the column, and unset must stay absent
                    // from the JSON rather than becoming false.
                    content_folder: match content_folder {
                        -1 => None,
                        0 => Some(false),
                        _ => Some(true),
                    },
                },
            ))
        })?;
        for row in rows {
            let (k, v) = row?;
            out.insert(k, v);
        }
        Ok(out)
    }

    /// What the store knows about every torrent of one session.
    ///
    /// Read in one query and handed to the row builder as a map: doing it per
    /// torrent would be 486 statements to answer one listing, which is the kind
    /// of thing that only shows up as "the UI got slow" at 243k.
    /// The same facts, for a named set of torrents.
    ///
    /// One query per batch instead of one for the whole session. The total I/O
    /// is the same -- 300k rows have to come off a 4.7 GB database either way,
    /// and that read is 21 seconds of it -- but the page paints from the first
    /// batch instead of after the last. Lookups go through the primary key
    /// rather than the session index, which also spares the row fetch.
    /// ⚠⚠ THE SESSION IS PART OF THE KEY. A hash alone names a CONTENT, not a
    /// copy.
    ///
    /// This looked up `WHERE info_hash IN (...)` and keyed the result by hash
    /// alone, so for a torrent held by two engines the last row SQLite happened
    /// to return won. On 2026-09-16 five torrents seeded by both hoard and race
    /// showed up in the Hoard table carrying the race copy's category ("Race")
    /// and its save path ("/race/torrents") -- while the facet chips, which go
    /// through `slim_facts(engine_id)`, did not count them. One response
    /// contradicting itself.
    pub fn facts_for_hashes(
        &self,
        hashes: &[String],
        session: &str,
    ) -> anyhow::Result<std::collections::HashMap<String, crate::row::StoreFacts>> {
        let mut out = std::collections::HashMap::with_capacity(hashes.len());
        if hashes.is_empty() {
            return Ok(out);
        }
        // Placeholders rather than an interpolated list: an info hash comes
        // from a torrent file, and a query built by concatenation is one that
        // can be steered by its input.
        let holes = std::iter::repeat_n("?", hashes.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT info_hash, category, save_path, added_time, completed_time,
                    seeding_time, tags, paused, content_folder
             FROM torrents WHERE session = ?1 AND info_hash IN ({holes})"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(hashes.len() + 1);
        params.push(&session as &dyn rusqlite::ToSql);
        params.extend(hashes.iter().map(|h| h as &dyn rusqlite::ToSql));
        let rows = stmt.query_map(params.as_slice(), |row| {
            let info_hash: String = row.get(0)?;
            let tags: String = row.get(6)?;
            let content_folder: i64 = row.get(8)?;
            Ok((
                info_hash,
                crate::row::StoreFacts {
                    category: row.get(1)?,
                    save_path: row.get(2)?,
                    added_time: row.get::<_, f64>(3)? as i64,
                    completed_time: row.get::<_, f64>(4)? as i64,
                    seeding_time: row.get(5)?,
                    tags: split_tags(&tags),
                    user_paused: row.get::<_, i64>(7)? != 0,
                    // -1 is the column's "unset" default, and unset must stay
                    // absent from the JSON rather than become false.
                    content_folder: if content_folder < 0 {
                        None
                    } else {
                        Some(content_folder != 0)
                    },
                },
            ))
        })?;
        for row in rows {
            let (hash, facts) = row?;
            out.insert(hash, facts);
        }
        Ok(out)
    }

    /// The three facts the list pass needs about every torrent, interned.
    ///
    /// `facts_by_session` builds a `StoreFacts` per torrent -- three Strings and
    /// a Vec each, keyed by a 40-character hex String. At 300k that is roughly
    /// 270 MB of transient allocation to answer one page of 500 rows, measured
    /// against a control run. Here the key is the raw 20-byte info hash and the
    /// two text fields are interned into small tables, because a library has a
    /// couple of dozen categories and a handful of tags however many torrents it
    /// holds. Same information, a few MB instead.
    pub fn slim_facts(&self, session: &str) -> anyhow::Result<SlimFacts> {
        let mut stmt = self
            .conn
            .prepare("SELECT info_hash, category, tags, paused FROM torrents WHERE session = ?1")?;
        let mut out = SlimFacts::empty();
        let mut rows = stmt.query([session])?;
        while let Some(row) = rows.next()? {
            let hash: String = row.get(0)?;
            let Some(key) = hex20(&hash) else { continue };
            let category: String = row.get(1)?;
            let tags_raw: String = row.get(2)?;
            let paused: i64 = row.get(3)?;
            let fact = out.fact_of(category, &tags_raw, paused);
            out.by_hash.insert(key, fact);
        }
        Ok(out)
    }

    /// `slim_facts`, kept in memory and brought up to date from the rows
    /// written since the last call.
    ///
    /// Reading the whole session took 1.1 s at a million torrents, on every
    /// list request, holding the store's lock -- every other store write
    /// waited behind every search. A cache invalidated on any write was tried
    /// before and never served: the store takes a write every few seconds
    /// (Calewood adds torrents all day). So the triggers set up in
    /// `track_changes` note WHICH rows changed, and only those are read again.
    ///
    /// Falls back to a full read when it cannot vouch for the copy: no
    /// triggers on this connection (a read-only open), another connection
    /// wrote to the file (`data_version` moved), or so many rows changed that
    /// reading them one by one would cost more than reading them all.
    pub fn slim_facts_current(
        &mut self,
        session: &str,
    ) -> anyhow::Result<std::sync::Arc<SlimFacts>> {
        if !self.tracks_changes {
            return Ok(std::sync::Arc::new(self.slim_facts(session)?));
        }
        let version: i64 = self
            .conn
            .query_row("PRAGMA data_version", [], |r| r.get(0))?;
        if version != self.slim_version {
            self.slim.clear();
            self.slim_version = version;
        }
        let dirty: Vec<(String, String)> = {
            let mut stmt = self
                .conn
                .prepare_cached("SELECT session, info_hash FROM temp.slim_dirty LIMIT ?1")?;
            let rows =
                stmt.query_map([SLIM_REREAD_MAX as i64 + 1], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<Result<_, _>>()?
        };
        if !dirty.is_empty() {
            self.conn.execute("DELETE FROM temp.slim_dirty", [])?;
        }
        if dirty.len() > SLIM_REREAD_MAX {
            self.slim.clear();
        } else if !self.slim.is_empty() {
            let mut stmt = self.conn.prepare_cached(
                "SELECT category, tags, paused FROM torrents WHERE session = ?1 AND info_hash = ?2",
            )?;
            for (sess, hash) in &dirty {
                let Some(cached) = self.slim.get_mut(sess) else {
                    continue;
                };
                let Some(key) = hex20(hash) else { continue };
                let facts = std::sync::Arc::make_mut(cached);
                let row: Option<(String, String, i64)> = stmt
                    .query_row([sess.as_str(), hash.as_str()], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                    })
                    .map(Some)
                    .or_else(|e| match e {
                        rusqlite::Error::QueryReturnedNoRows => Ok(None),
                        e => Err(e),
                    })?;
                match row {
                    Some((category, tags_raw, paused)) => {
                        let fact = facts.fact_of(category, &tags_raw, paused);
                        facts.by_hash.insert(key, fact);
                    }
                    None => {
                        facts.by_hash.remove(&key);
                    }
                }
            }
        }
        if let Some(f) = self.slim.get(session) {
            return Ok(f.clone());
        }
        let f = std::sync::Arc::new(self.slim_facts(session)?);
        self.slim.insert(session.to_string(), f.clone());
        Ok(f)
    }

    /// Note every row of `torrents` that is written, for `slim_facts_current`.
    ///
    /// TEMP objects: they live with this connection, in its temp schema, and
    /// never reach the file -- nothing about the database changes, and a
    /// read-only open, where they cannot be created, simply goes without.
    /// Only the columns the list pass reads count as a change; a write to any
    /// other column leaves the copy valid.
    fn track_changes(&mut self) {
        let ddl = "
            CREATE TEMP TABLE IF NOT EXISTS slim_dirty(session TEXT NOT NULL, info_hash TEXT NOT NULL);
            CREATE TEMP TRIGGER IF NOT EXISTS slim_dirty_ins AFTER INSERT ON main.torrents
                BEGIN INSERT INTO slim_dirty VALUES (NEW.session, NEW.info_hash); END;
            CREATE TEMP TRIGGER IF NOT EXISTS slim_dirty_del AFTER DELETE ON main.torrents
                BEGIN INSERT INTO slim_dirty VALUES (OLD.session, OLD.info_hash); END;
            CREATE TEMP TRIGGER IF NOT EXISTS slim_dirty_upd
                AFTER UPDATE OF session, info_hash, category, tags, paused ON main.torrents
                BEGIN
                    INSERT INTO slim_dirty VALUES (OLD.session, OLD.info_hash);
                    INSERT INTO slim_dirty VALUES (NEW.session, NEW.info_hash);
                END;";
        self.tracks_changes = match self.conn.execute_batch(ddl) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!("list facts will be read in full on every request: {e}");
                false
            }
        };
    }

    /// SQLite's own tally of rows written on this connection.
    ///
    /// Any INSERT, UPDATE or DELETE moves it, so a cache keyed on this value
    /// cannot go stale through a write somebody forgot to annotate -- which is
    /// the failure mode that makes hand-maintained cache versions untrustworthy
    /// on a store with sixty write methods. It counts writes to every table, so
    /// a job row moving invalidates a torrent cache needlessly; that costs one
    /// rebuild, where the other direction costs a wrong answer.
    ///
    /// It does NOT see writes made on another connection, which is why callers
    /// pair it with a TTL.
    #[cfg(test)]
    pub fn write_mark(&self) -> u64 {
        self.conn.total_changes()
    }

    /// The facts of ONE category, and the hashes that carry it.
    ///
    /// The whole-session query builds a StoreFacts for every torrent in the
    /// library -- three Strings and a Vec each -- and the qBit shim then throws
    /// away all but the category *arr asked about: 300k built, 1972 kept. This
    /// asks the index for the category directly, so the work is proportional to
    /// what the client wanted.
    ///
    /// The covering index already leads with `session`; `category` sits inside
    /// it, so the scan is over that session's slice of the index and stops at
    /// the rows that match.
    pub fn facts_in_category(
        &self,
        session: &str,
        category: &str,
    ) -> anyhow::Result<std::collections::HashMap<String, crate::row::StoreFacts>> {
        let mut stmt = self.conn.prepare(
            "SELECT info_hash, category, save_path, added_time, completed_time,
                    seeding_time, tags, paused, content_folder
             FROM torrents WHERE session = ?1 AND category = ?2",
        )?;
        let mut out = std::collections::HashMap::new();
        let rows = stmt.query_map([session, category], Self::fact_row)?;
        for row in rows {
            let (hash, facts) = row?;
            out.insert(hash, facts);
        }
        Ok(out)
    }

    /// One row of the facts query, shared by the whole-session and the
    /// per-category form so the two can never drift into reading the same
    /// columns differently.
    fn fact_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(String, crate::row::StoreFacts)> {
        let info_hash: String = row.get(0)?;
        let tags: String = row.get(6)?;
        let content_folder: i64 = row.get(8)?;
        Ok((
            info_hash,
            crate::row::StoreFacts {
                category: row.get(1)?,
                save_path: row.get(2)?,
                // added_time / completed_time are REAL seconds in the
                // schema; the API publishes whole seconds.
                added_time: row.get::<_, f64>(3)? as i64,
                completed_time: row.get::<_, f64>(4)? as i64,
                seeding_time: row.get(5)?,
                tags: split_tags(&tags),
                user_paused: row.get::<_, i64>(7)? != 0,
                // -1 is the column's "unset" default, and unset must stay
                // absent from the JSON rather than become false.
                content_folder: if content_folder < 0 {
                    None
                } else {
                    Some(content_folder != 0)
                },
            },
        ))
    }

    pub fn facts_by_session(
        &self,
        session: &str,
    ) -> anyhow::Result<std::collections::HashMap<String, crate::row::StoreFacts>> {
        let mut stmt = self.conn.prepare(
            "SELECT info_hash, category, save_path, added_time, completed_time,
                    seeding_time, tags, paused, content_folder
             FROM torrents WHERE session = ?1",
        )?;
        let mut out = std::collections::HashMap::new();
        let rows = stmt.query_map([session], Self::fact_row)?;
        for row in rows {
            let (hash, facts) = row?;
            out.insert(hash, facts);
        }
        Ok(out)
    }

    /// The lifetime carry-over counters, by key.
    ///
    /// The "global" row is the baseline every total is measured from: it holds
    /// what this library had transferred before the engines currently running
    /// were started, which is why a restart does not reset the headline figure.
    pub fn counter(&self, key: &str) -> (i64, i64) {
        self.conn
            .query_row("SELECT ul, dl FROM counters WHERE key = ?1", [key], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap_or((0, 0))
    }

    /// Write a lifetime counter.
    pub fn set_counter(&self, key: &str, ul: i64, dl: i64) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT INTO counters (key, ul, dl) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET ul = excluded.ul, dl = excluded.dl",
            rusqlite::params![key, ul, dl],
        )?;
        Ok(())
    }

    /// The counters key for one (engine, tracker) pair.
    ///
    /// The separators are NUL bytes, matching what `tracker_counters` reads
    /// back. Building this key with spaces would create a second row beside the
    /// real one, invisible to the reader that only knows the NUL form.
    pub fn tracker_counter_key(engine: &str, host: &str) -> String {
        format!("tracker\0{engine}\0{host}")
    }

    /// Fold a removed torrent's lifetime bytes into the carry-over counters and
    /// drop its row, in ONE transaction.
    ///
    /// This is the whole reason the counters exist. Every total the interface
    /// publishes is a sum over the torrents currently LOADED, so removing one
    /// takes its lifetime bytes out of that sum; the carry-over is what puts
    /// them back. Without this call a delete silently rewrites history --
    /// measured on prod 2026-09-14, the drain alone erased 7.03 TB of lifetime
    /// upload in a day, from the all-time figure as well as the day's.
    ///
    /// The fold and the delete must commit together. As two statements, a crash
    /// between them either double-counts the torrent at the next boot or drops
    /// its bytes for good -- and lifetime upload is the one number here that
    /// cannot be recomputed from anything else.
    ///
    /// `session` names which copy to drop; `None` means every copy, which is
    /// what an unqualified "remove this torrent" has always meant.
    pub fn delete_absorb(
        &self,
        info_hash: &str,
        session: Option<&str>,
        keys: &[String],
        ul: i64,
        dl: i64,
    ) -> anyhow::Result<bool> {
        let tx = self.conn.unchecked_transaction()?;
        // A torrent that never moved a byte still has to lose its row, so only
        // the folding is conditional.
        if ul > 0 || dl > 0 {
            for key in keys {
                tx.execute(
                    "INSERT INTO counters (key, ul, dl) VALUES (?1, ?2, ?3)
                     ON CONFLICT(key) DO UPDATE SET ul = ul + excluded.ul, dl = dl + excluded.dl",
                    rusqlite::params![key, ul, dl],
                )?;
            }
        }
        let n = match session {
            Some(s) => tx.execute(
                "DELETE FROM torrents WHERE info_hash = ?1 AND session = ?2",
                rusqlite::params![info_hash, s],
            )?,
            None => tx.execute("DELETE FROM torrents WHERE info_hash = ?1", [info_hash])?,
        };
        // The dates are the torrent's: they go with its last copy.
        tx.execute(
            "DELETE FROM tag_since WHERE info_hash = ?1
             AND NOT EXISTS (SELECT 1 FROM torrents WHERE info_hash = ?1)",
            [info_hash],
        )?;
        tx.commit()?;
        Ok(n > 0)
    }

    /// Every distinct tag used by one session's torrents, sorted.
    pub fn tags_of_session(&self, session: &str) -> anyhow::Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT tags FROM torrents WHERE session = ?1 AND tags <> ''")?;
        let mut set = std::collections::BTreeSet::new();
        for row in stmt.query_map([session], |r| r.get::<_, String>(0))? {
            for tag in split_tags(&row?) {
                set.insert(tag);
            }
        }
        Ok(set.into_iter().collect())
    }

    /// Info hashes pinned by the user, sorted.
    pub fn pinned(&self, session: &str) -> anyhow::Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT info_hash FROM torrents WHERE session = ?1 AND pinned <> 0 ORDER BY info_hash",
        )?;
        let rows: Vec<String> = stmt
            .query_map([session], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Every registered tag name, sorted.
    ///
    /// The registry, not the torrents: a tag the user created and has not
    /// applied yet still has to appear, or it vanishes from the UI the moment
    /// the last torrent carrying it is removed.
    pub fn registered_tags(&self) -> anyhow::Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name FROM tag_registry ORDER BY name")?;
        let rows: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Per-tracker cumulative counters, as (engine, tracker, ul, dl).
    ///
    /// ⚠ The key fields are separated by NUL bytes, not spaces:
    /// `tracker\0hoard\0tk.tr4ker.net`. A dump prints them as if they were
    /// spaces, so `LIKE 'tracker %'` looks right and matches nothing at all --
    /// the endpoint then answers an empty list while the data is sitting there.
    pub fn tracker_counters(&self) -> anyhow::Result<Vec<(String, String, i64, i64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT key, ul, dl FROM counters WHERE key LIKE 'tracker' || char(0) || '%' ORDER BY key")?;
        let rows: Vec<(String, i64, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|(key, ul, dl)| {
                let rest = key.strip_prefix("tracker\0")?;
                let (engine, host) = rest.split_once('\0')?;
                Some((engine.to_string(), host.to_string(), ul, dl))
            })
            .collect())
    }

    // -- mutations ---------------------------------------------------------
    //
    // Everything the write endpoints change goes through here, so the SQL that
    // touches the shared database sits in one file rather than in the handlers.

    pub fn put_meta(&self, key: &str, value: &str) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [key, value],
        )?;
        Ok(())
    }

    /// Register tag names. Unknown ones are created, known ones left alone.
    pub fn register_tags(&self, tags: &[String]) -> anyhow::Result<()> {
        for tag in tags {
            self.conn.execute(
                "INSERT OR IGNORE INTO tag_registry (name) VALUES (?1)",
                [tag],
            )?;
        }
        Ok(())
    }

    /// Take these tags off every torrent wearing them. Returns how many rows
    /// changed.
    pub fn strip_tags(&self, tags: &[String]) -> anyhow::Result<usize> {
        let mut changed = 0usize;
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut find =
                tx.prepare("SELECT rowid, tags FROM torrents WHERE tags LIKE '%' || ?1 || '%'")?;
            let mut put = tx.prepare("UPDATE torrents SET tags = ?2 WHERE rowid = ?1")?;
            for tag in tags {
                let rows: Vec<(i64, String)> = find
                    .query_map([tag], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .filter_map(Result::ok)
                    .collect();
                for (rowid, raw) in rows {
                    let current = split_tags(&raw);
                    if !current.iter().any(|t| t == tag) {
                        continue;
                    }
                    let kept: Vec<String> = current.into_iter().filter(|t| t != tag).collect();
                    put.execute(rusqlite::params![rowid, kept.join(",")])?;
                    changed += 1;
                }
                // Every date of the tag, whoever wore it.
                tx.execute("DELETE FROM tag_since WHERE tag = ?1", [tag])?;
            }
        }
        tx.commit()?;
        Ok(changed)
    }

    pub fn unregister_tags(&self, tags: &[String]) -> anyhow::Result<()> {
        for tag in tags {
            self.conn
                .execute("DELETE FROM tag_registry WHERE name = ?1", [tag])?;
        }
        Ok(())
    }

    /// Tags of one torrent, read the way every other reader reads them.
    ///
    /// ⚠ Through `split_tags`, never a bare comma split: rows written by the
    /// qBit shim hold a JSON list (`["cross-seed"]`, 95k of them in prod). A
    /// comma split returned that whole string as ONE tag, and every add or
    /// remove wrote it back -- the workflow tagging noHL on 01/10/2026 turned
    /// 479 cross-seeds into a tag named `["cross-seed"]`.
    pub fn tags_of(&self, info_hash: &str) -> Vec<String> {
        let raw: String = self
            .conn
            .query_row(
                "SELECT tags FROM torrents WHERE info_hash = ?1 LIMIT 1",
                [info_hash],
                |r| r.get(0),
            )
            .unwrap_or_default();
        split_tags(&raw)
    }

    /// Push the engines' seed counters into the column the UI and the rules
    /// engine both read.
    ///
    /// `torrents.seeding_time` was declared, indexed and read in six places,
    /// and written by nothing: it answered 0 for every torrent since the
    /// column existed. A rule saying "seeded for 48 hours" was therefore
    /// always false, and the detail panel always said zero.
    ///
    /// One transaction for the batch: 300k single-statement commits would be
    /// 300k fsyncs.
    pub fn update_seeding_times(&self, rows: &[(String, i64)]) -> Result<usize, rusqlite::Error> {
        if rows.is_empty() {
            return Ok(0);
        }
        let tx = self.conn.unchecked_transaction()?;
        let mut n = 0usize;
        {
            let mut stmt =
                tx.prepare_cached("UPDATE torrents SET seeding_time = ?2 WHERE info_hash = ?1")?;
            for (hash, secs) in rows {
                n += stmt.execute(rusqlite::params![hash, secs])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// Replace a torrent's tags. The date of each tag (`tag_since`) follows in
    /// the same transaction: a new tag is dated now, a kept one keeps its date
    /// (or the lack of one, which reads as the migration instant), a removed
    /// one loses its row.
    pub fn set_tags(&self, info_hash: &str, tags: &[String]) -> anyhow::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let joined = tags.join(",");
        let before = tx
            .query_row(
                "SELECT tags FROM torrents WHERE info_hash = ?1 LIMIT 1",
                [info_hash],
                |r| r.get::<_, String>(0),
            )
            .map(|raw| split_tags(&raw))
            .unwrap_or_default();
        let n = tx.execute(
            "UPDATE torrents SET tags = ?2 WHERE info_hash = ?1",
            rusqlite::params![info_hash, joined],
        )?;
        // No row, no dates: a row for a torrent the store does not hold
        // would be an orphan.
        if n > 0 {
            retag_since(
                &tx,
                info_hash,
                &before,
                &split_tags(&joined),
                &[],
                now_secs(),
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The raw `tag_since` rows of one torrent, sorted, for tests: unlike
    /// `tag_since` it does not fill in the migration instant.
    #[cfg(test)]
    pub fn tag_since_rows(&self, info_hash: &str) -> Vec<(String, i64)> {
        let mut st = self
            .conn
            .prepare("SELECT tag, since FROM tag_since WHERE info_hash = ?1 ORDER BY tag")
            .unwrap();
        st.query_map([info_hash], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    /// When each tag of one torrent was set, as seconds since the epoch. A tag
    /// the torrent wears without a date was there when dates began to be kept:
    /// it reads as the migration instant ("at least since").
    pub fn tag_since(
        &self,
        info_hash: &str,
    ) -> anyhow::Result<std::collections::HashMap<String, i64>> {
        Ok(self
            .tag_since_bulk(&[info_hash])?
            .remove(info_hash)
            .unwrap_or_default())
    }

    /// `tag_since` for many torrents: a read of the tags and a read of the
    /// dates per 900 hashes, not one per torrent. Torrents without tags are
    /// absent from the answer. Only tags the torrent wears now are reported.
    pub fn tag_since_bulk(
        &self,
        info_hashes: &[&str],
    ) -> anyhow::Result<std::collections::HashMap<String, std::collections::HashMap<String, i64>>>
    {
        use std::collections::HashMap;
        let mut out: HashMap<String, HashMap<String, i64>> = HashMap::new();
        if info_hashes.is_empty() {
            return Ok(out);
        }
        let floor: i64 = self
            .meta_doc(TAG_SINCE_MIGRATED_KEY)
            .and_then(|v| v.parse().ok())
            .unwrap_or(self.opened_at);
        for chunk in info_hashes.chunks(900) {
            let marks = vec!["?"; chunk.len()].join(",");
            let mut worn: HashMap<String, Vec<String>> = HashMap::new();
            let mut stmt = self.conn.prepare(&format!(
                "SELECT info_hash, tags FROM torrents WHERE tags <> '' AND info_hash IN ({marks})"
            ))?;
            let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (h, raw) = row?;
                worn.entry(h).or_default().extend(split_tags(&raw));
            }
            if worn.is_empty() {
                continue;
            }
            // A store opened read-only from before this table has none: every
            // tag then reads as the floor.
            let mut dated: HashMap<(String, String), i64> = HashMap::new();
            if let Ok(mut stmt) = self.conn.prepare(&format!(
                "SELECT info_hash, tag, since FROM tag_since WHERE info_hash IN ({marks})"
            )) {
                let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                })?;
                for row in rows {
                    let (h, t, at) = row?;
                    dated.insert((h, t), at);
                }
            }
            for (h, tags) in worn {
                let map = out.entry(h.clone()).or_default();
                for t in tags {
                    let at = dated.get(&(h.clone(), t.clone())).copied().unwrap_or(floor);
                    map.insert(t, at);
                }
            }
        }
        Ok(out)
    }

    /// Per COPY: a torrent seeded from two engines can be paused in one and
    /// running in the other. Pause describes an execution, not the content.
    pub fn set_paused(&self, info_hash: &str, session: &str, paused: bool) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE torrents SET paused = ?3 WHERE info_hash = ?1 AND session = ?2",
            rusqlite::params![info_hash, session, i64::from(paused)],
        )?;
        Ok(())
    }

    /// Pause or resume every torrent of one session. Returns how many rows moved.
    pub fn set_paused_all(&self, session: &str, paused: bool) -> anyhow::Result<usize> {
        let changed = self.conn.execute(
            "UPDATE torrents SET paused = ?2 WHERE session = ?1",
            rusqlite::params![session, i64::from(paused)],
        )?;
        Ok(changed)
    }

    /// The info hashes of one session the operator has paused.
    ///
    /// Read by the download slot manager on every pass. The intent lives here
    /// and nowhere else, so a scheduler that does not ask cannot tell "the
    /// human stopped this" from "I parked this myself" -- and will happily
    /// restart the first, which is the bug this exists to prevent.
    pub fn paused_hashes(&self, session: &str) -> anyhow::Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT info_hash FROM torrents WHERE session = ?1 AND paused <> 0")?;
        let rows = stmt.query_map(rusqlite::params![session], |r| r.get::<_, String>(0))?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    /// Every info hash of one session, sorted.
    pub fn all_hashes(&self, session: &str) -> anyhow::Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT info_hash FROM torrents WHERE session = ?1 ORDER BY info_hash")?;
        let rows: Vec<String> = stmt
            .query_map([session], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Does the store hold the .torrent for this hash?
    ///
    /// The tracker editor asks before touching anything: an edit it cannot
    /// persist would be live until the next restart and then silently revert,
    /// which is worse than refusing.
    pub fn has_torrent_blob(&self, info_hash: &str) -> bool {
        self.conn
            .query_row(
                "SELECT length(torrent) FROM torrents WHERE info_hash = ?1 LIMIT 1",
                [info_hash],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n > 0)
            .unwrap_or(false)
    }

    /// Remove a torrent row. Returns whether it was there.
    pub fn delete_torrent(&self, info_hash: &str) -> anyhow::Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM torrents WHERE info_hash = ?1", [info_hash])?;
        // Its share limits go with it: a re-add starts on the engine's.
        let _ = self
            .conn
            .execute("DELETE FROM share_limits WHERE info_hash = ?1", [info_hash]);
        self.conn
            .execute("DELETE FROM tag_since WHERE info_hash = ?1", [info_hash])?;
        Ok(n > 0)
    }

    /// EVERY copy. For callers that name a torrent and not an engine -- the
    /// qBit shim, which Sonarr and Radarr speak, has no notion of engines and
    /// means "stop this torrent" whichever engines hold it.
    pub fn set_paused_everywhere(&self, info_hash: &str, paused: bool) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE torrents SET paused = ?2 WHERE info_hash = ?1",
            rusqlite::params![info_hash, if paused { 1 } else { 0 }],
        )?;
        Ok(())
    }

    /// The same intent for a whole SET, in ONE transaction.
    ///
    /// The per-hash version above is right for a context menu and wrong for a
    /// bulk action: on 2026-09-16 a bulk start walked 293k hashes calling it in
    /// a loop, which is 293k autocommits and 293k fsyncs. Measured on the
    /// production box that ran at ~17 rows/s, so the loop held the store lock
    /// for about half an hour and no other request was served in the meantime.
    /// Same shape as `update_seeding_times`, for the same reason.
    pub fn set_paused_everywhere_batch(
        &self,
        hashes: &[String],
        paused: bool,
    ) -> Result<usize, rusqlite::Error> {
        if hashes.is_empty() {
            return Ok(0);
        }
        let tx = self.conn.unchecked_transaction()?;
        let mut n = 0usize;
        {
            let mut stmt =
                tx.prepare_cached("UPDATE torrents SET paused = ?2 WHERE info_hash = ?1")?;
            for hash in hashes {
                n += stmt.execute(rusqlite::params![hash, i64::from(paused)])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// Per COPY, for a whole set. Same reason as above.
    pub fn set_paused_batch(
        &self,
        hashes: &[String],
        session: &str,
        paused: bool,
    ) -> Result<usize, rusqlite::Error> {
        if hashes.is_empty() {
            return Ok(0);
        }
        let tx = self.conn.unchecked_transaction()?;
        let mut n = 0usize;
        {
            let mut stmt = tx.prepare_cached(
                "UPDATE torrents SET paused = ?3 WHERE info_hash = ?1 AND session = ?2",
            )?;
            for hash in hashes {
                n += stmt.execute(rusqlite::params![hash, session, i64::from(paused)])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// EVERY copy, for the same reason.
    pub fn set_pinned_everywhere(&self, info_hash: &str, pinned: bool) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE torrents SET pinned = ?2 WHERE info_hash = ?1",
            rusqlite::params![info_hash, if pinned { 1 } else { 0 }],
        )?;
        Ok(())
    }

    /// Per COPY: a download slot is held by one engine, not by the torrent.
    pub fn set_pinned(&self, info_hash: &str, session: &str, pinned: bool) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE torrents SET pinned = ?3 WHERE info_hash = ?1 AND session = ?2",
            rusqlite::params![info_hash, session, i64::from(pinned)],
        )?;
        Ok(())
    }

    /// Record a newly added torrent, metadata and file together.
    ///
    /// The BLOB is the .torrent itself: this table is what a rebuild reads, and
    /// a row without it is a torrent the node can list but never re-add. The
    /// insert is `OR IGNORE` because the engine has already refused a duplicate
    /// by the time we get here -- racing two adds of the same hash should leave
    /// the first row alone rather than overwrite its category and added_time.
    #[allow(clippy::too_many_arguments)]
    /// Returns whether a row was CREATED, which is not the same as success.
    ///
    /// `INSERT OR IGNORE` answers Ok either way, and swallowing that difference
    /// is how a re-add came to destroy the torrent it duplicated: the caller
    /// cleaned up "its" row on failure, but on a duplicate the row it deleted
    /// belonged to the copy already running (2026-09-24, 20 torrents left
    /// running in the engine with nothing in the store -- no lookup, no
    /// category, and every piece refused for want of a metainfo to hash
    /// against). A caller that undoes its own work has to be told what its own
    /// work was.
    pub fn insert_torrent(
        &self,
        info_hash: &str,
        session: &str,
        torrent: &[u8],
        save_path: &str,
        category: &str,
        added_time: f64,
        paused: bool,
        tags: &str,
    ) -> anyhow::Result<bool> {
        let tx = self.conn.unchecked_transaction()?;
        let n = tx.execute(
            "INSERT OR IGNORE INTO torrents
                 (info_hash, session, torrent, save_path, category, added_time, paused, tags)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            rusqlite::params![
                info_hash,
                session,
                torrent,
                save_path,
                category,
                added_time,
                if paused { 1 } else { 0 },
                tags
            ],
        )?;
        if n > 0 && !tags.is_empty() {
            // Tags a torrent arrives with are set now, whatever date the
            // client it comes from gave the torrent. A second copy of a
            // torrent already tagged adds nothing to its dates.
            let mut others = Vec::new();
            let mut sib = tx.prepare_cached(
                "SELECT tags FROM torrents WHERE info_hash = ?1 AND session <> ?2",
            )?;
            for raw in sib.query_map(rusqlite::params![info_hash, session], |r| {
                r.get::<_, String>(0)
            })? {
                others.extend(split_tags(&raw?));
            }
            drop(sib);
            retag_since(&tx, info_hash, &[], &split_tags(tags), &others, now_secs())?;
        }
        tx.commit()?;
        if n > 0 {
            self.mark_fresh(info_hash, session);
        }
        Ok(n > 0)
    }

    fn mark_fresh(&self, info_hash: &str, session: &str) {
        let mut fresh = self.fresh.lock().unwrap_or_else(|e| e.into_inner());
        fresh.insert(format!("{info_hash}/{session}"), std::time::Instant::now());
    }

    /// `delete_copy`, unless this process wrote that row less than `grace` ago:
    /// for the store reconcile, which must not take a torrent still on its way
    /// into the engine for one that left it. A row lost that way strands a live
    /// torrent with no metainfo, and every piece it receives is refused.
    pub fn delete_settled_copy(
        &self,
        info_hash: &str,
        session: &str,
        grace: std::time::Duration,
    ) -> anyhow::Result<bool> {
        {
            let mut fresh = self.fresh.lock().unwrap_or_else(|e| e.into_inner());
            fresh.retain(|_, at| at.elapsed() < grace);
            if fresh.contains_key(&format!("{info_hash}/{session}")) {
                return Ok(false);
            }
        }
        // A torrent being moved or graduated is out of its engine on purpose,
        // for as long as its files take to copy, and its row still names the
        // engine it left. That row holds the only metainfo the target engine
        // will adopt it with.
        if self.job_in_flight_for(info_hash) {
            return Ok(false);
        }
        self.delete_copy(info_hash, session)
    }

    /// Any job still queued or running for this torrent.
    fn job_in_flight_for(&self, info_hash: &str) -> bool {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM jobs
                 WHERE info_hash = ?1 AND state IN ('queued','running','cancelling','waiting')",
                rusqlite::params![info_hash],
                |r| r.get::<_, i64>(0),
            )
            // Unreadable: keep the row. A reconcile that waits loses nothing.
            .map_or(true, |n| n > 0)
    }

    /// Re-home a torrent to another engine of this node.
    ///
    /// `session` is what every per-engine query filters on, so this one column
    /// decides which list a torrent appears in. `insert_torrent` is an
    /// INSERT OR IGNORE and would leave it pointing at the old engine, which is
    /// how a moved torrent ends up running in one engine and listed under
    /// another.
    /// Move ONE copy from one engine to another.
    ///
    /// Takes the source: with several copies of a torrent, "set its session"
    /// has no single meaning, and updating them all would silently collapse
    /// three copies into one.
    pub fn set_session(&self, info_hash: &str, from: &str, to: &str) -> anyhow::Result<()> {
        let n = self.conn.execute(
            "UPDATE torrents SET session = ?3 WHERE info_hash = ?1 AND session = ?2",
            rusqlite::params![info_hash, from, to],
        )?;
        // A row re-homed ahead of the engine that will hold it is as new there
        // as an inserted one.
        if n > 0 {
            self.mark_fresh(info_hash, to);
        }
        Ok(())
    }

    /// Drop ONE copy, leaving the others.
    pub fn delete_copy(&self, info_hash: &str, session: &str) -> anyhow::Result<bool> {
        let n = self.conn.execute(
            "DELETE FROM torrents WHERE info_hash = ?1 AND session = ?2",
            rusqlite::params![info_hash, session],
        )?;
        // The limits are the torrent's, not the copy's: they go with the last.
        let _ = self.conn.execute(
            "DELETE FROM share_limits WHERE info_hash = ?1
             AND NOT EXISTS (SELECT 1 FROM torrents WHERE info_hash = ?1)",
            [info_hash],
        );
        self.conn.execute(
            "DELETE FROM tag_since WHERE info_hash = ?1
             AND NOT EXISTS (SELECT 1 FROM torrents WHERE info_hash = ?1)",
            [info_hash],
        )?;
        Ok(n > 0)
    }

    /// Which engines hold this torrent.
    pub fn sessions_of(&self, info_hash: &str) -> Vec<String> {
        let Ok(mut stmt) = self
            .conn
            .prepare("SELECT session FROM torrents WHERE info_hash = ?1 ORDER BY session")
        else {
            return Vec::new();
        };
        let Ok(rows) = stmt.query_map([info_hash], |r| r.get::<_, String>(0)) else {
            return Vec::new();
        };
        rows.filter_map(|r| r.ok()).collect()
    }

    /// Where this torrent's data now lives.
    ///
    /// A graduation moves the payload; without this the row keeps pointing at
    /// the directory the bytes left, and the next restart looks for them there.
    pub fn set_save_path(&self, info_hash: &str, save_path: &str) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE torrents SET save_path = ?2 WHERE info_hash = ?1",
            rusqlite::params![info_hash, save_path],
        )?;
        Ok(())
    }

    /// This COPY's category, or `None` when this session does not hold it.
    ///
    /// ⚠⚠ The session is not optional. A category is a property of the copy,
    /// not of the content: the same torrent is `Race` in the race engine and
    /// `series` in the hoard. Asked by hash alone this returned whichever row
    /// SQLite reached first, and the drain took its decisions on it -- which
    /// category may graduate, and where to. Same root cause as the Hoard table
    /// showing "Race" on 2026-09-16, but this one moves data rather than pixels.
    pub fn category_of(&self, info_hash: &str, session: &str) -> Option<String> {
        self.conn
            .query_row(
                "SELECT category FROM torrents WHERE info_hash = ?1 AND session = ?2",
                rusqlite::params![info_hash, session],
                |r| r.get::<_, String>(0),
            )
            .ok()
    }

    /// Per COPY, like pause and pin.
    ///
    /// A category is not a label on the content: it decides where the payload
    /// lives and what the drain may do with it. The same torrent is `Race` in
    /// the race engine, seeded from /race, and `series` in the hoard, seeded
    /// from /data/downloads -- production holds five of those. Writing both at
    /// once would move one copy's rules onto the other.
    pub fn set_category_in(
        &self,
        info_hash: &str,
        session: &str,
        category: &str,
    ) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE torrents SET category = ?3 WHERE info_hash = ?1 AND session = ?2",
            rusqlite::params![info_hash, session, category],
        )?;
        Ok(())
    }

    /// EVERY copy. For the qBit shim ONLY.
    ///
    /// Sonarr, Radarr and the rest speak a protocol with no notion of engines:
    /// they name a torrent and a category, and there is no third field to carry
    /// which copy they mean. Every other caller knows its engine and must use
    /// `set_category_in`; this one is named for what it does so the choice is
    /// visible at the call site rather than hidden in a WHERE clause.
    /// Give every torrent labelled `old` the label `new` (a category rename;
    /// `new` empty clears the label). Returns how many rows changed.
    pub fn relabel_category(&self, old: &str, new: &str) -> anyhow::Result<usize> {
        Ok(self.conn.execute(
            "UPDATE torrents SET category = ?2 WHERE category = ?1",
            rusqlite::params![old, new],
        )?)
    }

    /// Which labels the torrents wear, with where they run and live:
    /// `(category, session, save_path, torrents)`, empty label excluded.
    pub fn category_usage(&self) -> anyhow::Result<Vec<(String, String, String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT category, session, save_path, COUNT(*) FROM torrents
             WHERE category != '' GROUP BY category, session, save_path",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    pub fn set_category_everywhere(&self, info_hash: &str, category: &str) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE torrents SET category = ?2 WHERE info_hash = ?1",
            rusqlite::params![info_hash, category],
        )?;
        Ok(())
    }

    /// Resolve a hash prefix WITHIN one session.
    ///
    /// The hoard routes refuse a race torrent and vice versa, so the session is
    /// part of the lookup rather than a check bolted on afterwards.
    pub fn resolve_hash_in(&self, session: &str, prefix: &str) -> Option<String> {
        let prefix = prefix.to_lowercase();
        if is_hex(&prefix) {
            // ⚠ By range, not LIKE. `info_hash LIKE ?2 || '%'` cannot use an
            // index (the pattern is an expression, and LIKE folds case), so
            // every write route resolved its hash by scanning the whole
            // session under the store's lock: 215-430 ms per tag, pause or
            // category at a million torrents, found by the lock log. Stored
            // hashes are lowercase hex, and every string that starts with a
            // hex prefix p sorts in [p, p || 'g').
            return self
                .conn
                .query_row(
                    "SELECT info_hash FROM torrents WHERE session = ?1 AND info_hash >= ?2 AND info_hash < ?2 || 'g' LIMIT 1",
                    rusqlite::params![session, prefix],
                    |r| r.get(0),
                )
                .ok();
        }
        self.conn
            .query_row(
                "SELECT info_hash FROM torrents WHERE session = ?1 AND info_hash LIKE ?2 || '%' LIMIT 1",
                rusqlite::params![session, prefix],
                |r| r.get(0),
            )
            .ok()
    }

    /// Info hashes whose value starts with the given prefix.
    ///
    /// qBittorrent clients routinely send a shortened hash, and refusing them
    /// would break the very callers the shim exists for.
    pub fn resolve_hash(&self, prefix: &str) -> Option<String> {
        let prefix = prefix.to_lowercase();
        if is_hex(&prefix) {
            // By range, for the reason given in `resolve_hash_in`.
            return self
                .conn
                .query_row(
                    "SELECT info_hash FROM torrents WHERE info_hash >= ?1 AND info_hash < ?1 || 'g' LIMIT 1",
                    [&prefix],
                    |r| r.get(0),
                )
                .ok();
        }
        self.conn
            .query_row(
                "SELECT info_hash FROM torrents WHERE info_hash LIKE ?1 || '%' LIMIT 1",
                [&prefix],
                |r| r.get(0),
            )
            .ok()
    }

    /// The content index: which torrents hold which payload.
    ///
    /// A table of its own rather than a column on `torrents`, because
    /// `torrents` is 4.5 GB in production and an ALTER there rewrites the lot
    /// for a field that only one feature reads. Additive and invisible to an
    /// older build, like the nodes and workflows tables.
    ///
    /// The key is (info_hash, session): the same payload legitimately appears
    /// in two engines, and collapsing those into one row would make the second
    /// engine's copy invisible to the lookup.
    fn ensure_content_index(&self) -> anyhow::Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS content_index (
                 info_hash TEXT NOT NULL,
                 session TEXT NOT NULL,
                 content_key TEXT NOT NULL,
                 PRIMARY KEY (info_hash, session));
             CREATE INDEX IF NOT EXISTS idx_content_index_key
                 ON content_index(content_key);",
        )?;
        Ok(())
    }

    /// The link index: what the background scanner measured of each torrent's
    /// files, so a workflow pass reads its hardlink facts instead of stat-ing
    /// the whole catalogue itself.
    ///
    /// ⭐ One row per torrent copy, the files packed in a BLOB. At a million
    /// torrents a row per file would be three million keys to rewrite and read
    /// back; the paths themselves are not stored, since they follow from the
    /// torrent's metadata and `save_path`. Additive and invisible to an older
    /// build, like every table added since 4.0.
    fn ensure_link_index(&self) -> anyhow::Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS link_index (
                 info_hash TEXT NOT NULL,
                 session TEXT NOT NULL,
                 save_path TEXT NOT NULL,
                 measured_at INTEGER NOT NULL,
                 files INTEGER NOT NULL,
                 missing INTEGER NOT NULL,
                 stats BLOB NOT NULL,
                 PRIMARY KEY (info_hash, session));",
        )?;
        Ok(())
    }

    /// Every row's bookkeeping, without the measurements: what the scanner
    /// reads to decide what is due.
    pub fn link_index_meta(
        &self,
    ) -> anyhow::Result<std::collections::HashMap<(String, String), LinkRowMeta>> {
        let mut q = self
            .conn
            .prepare("SELECT info_hash, session, save_path, measured_at, files FROM link_index")?;
        let rows = q.query_map([], |r| {
            Ok((
                (r.get::<_, String>(0)?, r.get::<_, String>(1)?),
                LinkRowMeta {
                    save_path: r.get(2)?,
                    measured_at: r.get(3)?,
                    files: r.get(4)?,
                },
            ))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Every row's measurement, handed over one at a time: (info_hash,
    /// session, the save path it was taken under, when, the packed stats).
    ///
    /// `measured_at` rides along so the list's "Checked" column is read in the
    /// same query as the measurement it dates, not from a second read that a
    /// batch written in between would make disagree.
    ///
    /// ⚠️ A callback and not a map, on purpose: collected into a HashMap of
    /// owned strings and BLOBs, the table was 0.7 GB at a million torrents,
    /// built again by every summary refresh and every workflow pass. The caller
    /// keeps the rows it can use, and nothing else is ever allocated.
    pub fn for_each_link_row(
        &self,
        mut f: impl FnMut(&str, &str, &str, i64, &[u8]),
    ) -> anyhow::Result<()> {
        let mut q = self
            .conn
            .prepare("SELECT info_hash, session, save_path, measured_at, stats FROM link_index")?;
        let mut rows = q.query([])?;
        while let Some(r) = rows.next()? {
            f(
                r.get_ref(0)?.as_str()?,
                r.get_ref(1)?.as_str()?,
                r.get_ref(2)?.as_str()?,
                r.get(3)?,
                r.get_ref(4)?.as_blob()?,
            );
        }
        Ok(())
    }

    /// The torrents the store holds for one session, as raw hashes.
    ///
    /// All the link index asks of the store about the catalogue is whether a
    /// copy is known; `workflow_facts` answers that too, with every column of
    /// a million rows (half a gigabyte) behind a yes or no. Index-only, like
    /// `workflow_facts`: `session, info_hash` lead `idx_torrents_cover`.
    pub fn session_hashes(
        &self,
        session: &str,
    ) -> anyhow::Result<std::collections::HashSet<[u8; 20]>> {
        let mut q = self
            .conn
            .prepare("SELECT info_hash FROM torrents WHERE session = ?1")?;
        let mut rows = q.query(rusqlite::params![session])?;
        let mut out = std::collections::HashSet::new();
        while let Some(r) = rows.next()? {
            if let Some(h) = r.get_ref(0)?.as_str().ok().and_then(hex20) {
                out.insert(h);
            }
        }
        Ok(out)
    }

    /// Write a batch of measurements in one transaction.
    pub fn put_link_rows(&self, rows: &[LinkRow]) -> anyhow::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut q = tx.prepare(
                "INSERT OR REPLACE INTO link_index
                     (info_hash, session, save_path, measured_at, files, missing, stats)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
            )?;
            for r in rows {
                q.execute(rusqlite::params![
                    r.info_hash,
                    r.session,
                    r.save_path,
                    r.measured_at,
                    r.files,
                    r.missing,
                    r.stats
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Forget the rows of torrents that are no longer held.
    pub fn drop_link_rows(&self, keys: &[(String, String)]) -> anyhow::Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        let mut n = 0;
        {
            let mut q =
                tx.prepare("DELETE FROM link_index WHERE info_hash = ?1 AND session = ?2")?;
            for (h, s) in keys {
                n += q.execute(rusqlite::params![h, s])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    pub fn link_index_counts(&self) -> anyhow::Result<LinkIndexCounts> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(files),0),
                    COALESCE(SUM(files > 0 AND missing = files),0),
                    COALESCE(SUM(missing > 0 AND missing < files),0),
                    COALESCE(MIN(measured_at),0)
               FROM link_index",
            [],
            |r| {
                Ok(LinkIndexCounts {
                    measured: r.get(0)?,
                    files: r.get(1)?,
                    data_missing: r.get(2)?,
                    partly_missing: r.get(3)?,
                    oldest: r.get(4)?,
                })
            },
        )?)
    }

    /// Where a torrent's data is meant to live, as the store recorded it.
    #[cfg(test)]
    pub fn save_path_of(&self, info_hash: &str) -> Option<String> {
        self.conn
            .query_row(
                "SELECT save_path FROM torrents WHERE info_hash = ?1",
                [info_hash],
                |r| r.get::<_, String>(0),
            )
            .ok()
    }

    pub fn put_content_key(
        &self,
        info_hash: &str,
        session: &str,
        content_key: &str,
    ) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO content_index (info_hash, session, content_key)
             VALUES (?1,?2,?3)",
            rusqlite::params![info_hash, session, content_key],
        )?;
        Ok(())
    }

    // No caller today. Kept, not deleted: clearing lints is not the moment to drop code.
    #[allow(dead_code)]
    pub fn drop_content_key(&self, info_hash: &str) -> anyhow::Result<()> {
        self.conn.execute(
            "DELETE FROM content_index WHERE info_hash = ?1",
            [info_hash],
        )?;
        Ok(())
    }

    /// Torrents already held whose payload matches `content_key`.
    ///
    /// Joined against `torrents` so a stale index row -- one whose torrent has
    /// since been deleted -- cannot be returned as a linkable source.
    pub fn content_matches(
        &self,
        content_key: &str,
        exclude_info_hash: &str,
    ) -> anyhow::Result<Vec<(String, String, String)>> {
        let mut q = self.conn.prepare(
            "SELECT t.info_hash, t.session, t.save_path
               FROM content_index c
               JOIN torrents t ON t.info_hash = c.info_hash AND t.session = c.session
              WHERE c.content_key = ?1 AND c.info_hash <> ?2",
        )?;
        let rows = q
            .query_map(rusqlite::params![content_key, exclude_info_hash], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Index every torrent that has no content key yet.
    ///
    /// Resumable by construction: it only looks at rows missing from
    /// `content_index`, so an interrupted pass costs nothing and a completed
    /// one is a no-op. Measured at 45 s for 301 221 torrents.
    ///
    /// Bounded by `limit` because the caller holds the store mutex for the
    /// whole call: a single 45 s pass would freeze every API handler behind
    /// it. The boot pass loops in batches and lets go in between.
    pub fn backfill_content_index(&self, limit: usize) -> anyhow::Result<usize> {
        let mut q = self.conn.prepare(
            "SELECT t.info_hash, t.session, t.torrent
               FROM torrents t
               LEFT JOIN content_index c
                 ON c.info_hash = t.info_hash AND c.session = t.session
              WHERE c.info_hash IS NULL
              LIMIT ?1",
        )?;
        let pending = q
            .query_map([limit as i64], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut done = 0;
        for (ih, sess, blob) in pending {
            if let Some(key) = crate::dedup::content_key(&blob) {
                self.put_content_key(&ih, &sess, &key)?;
                done += 1;
            }
        }
        Ok(done)
    }

    /// Groups of torrents that hold the same payload, with their save paths.
    ///
    /// The caller decides what counts as waste: a group whose members share a
    /// location already shares its bytes, and only differing locations cost
    /// disk.
    pub fn content_duplicate_groups(&self) -> anyhow::Result<Vec<Vec<(String, String, String)>>> {
        let mut q = self.conn.prepare(
            "SELECT c.content_key, t.info_hash, t.session, t.save_path
               FROM content_index c
               JOIN torrents t ON t.info_hash = c.info_hash AND t.session = c.session
              WHERE c.content_key IN (
                    SELECT content_key FROM content_index
                     GROUP BY content_key HAVING COUNT(DISTINCT info_hash) > 1)
              ORDER BY c.content_key",
        )?;
        let rows = q
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut out: Vec<Vec<(String, String, String)>> = Vec::new();
        let mut cur = String::new();
        for (key, ih, sess, sp) in rows {
            if key != cur {
                cur = key;
                out.push(Vec::new());
            }
            out.last_mut().unwrap().push((ih, sess, sp));
        }
        Ok(out)
    }

    #[cfg(test)]
    pub fn count_torrents(&self) -> anyhow::Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM torrents", [], |r| r.get(0))?)
    }

    /// Torrent counts per session, which is what the status endpoints report.
    #[cfg(test)]
    pub fn count_by_session(&self) -> anyhow::Result<Vec<(String, i64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT session, COUNT(*) FROM torrents GROUP BY session ORDER BY session")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

/// One RSS feed (`rss.rs`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RssFeedRow {
    pub id: i64,
    /// qBittorrent's `uid`, kept stable across renames.
    pub uid: String,
    /// Its place in the tree, qBittorrent's way: `Folder\Name`.
    pub path: String,
    pub url: String,
    pub refresh_secs: i64,
    /// The channel's own title, as last read.
    pub title: String,
    pub last_refresh: i64,
    /// Why the last refresh failed; empty when it did not.
    pub last_error: String,
    pub created_at: i64,
}

/// One article of a feed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RssArticleRow {
    pub feed_id: i64,
    pub guid: String,
    pub title: String,
    pub torrent_url: String,
    pub link: String,
    pub description: String,
    pub author: String,
    pub category: String,
    /// The feed's date, else when it was first seen.
    pub date: i64,
    pub size: i64,
    pub is_read: bool,
    pub seen_at: i64,
}

/// What took an article, so it is never taken twice.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RssTaken {
    pub rule: String,
    pub at: i64,
    pub info_hash: String,
    /// `added`, `already` (the torrent was there) or `refused: <why>`.
    pub outcome: String,
}

impl Store {
    /// RSS feeds, their articles, the automatic download rules and what the
    /// rules took.
    ///
    /// Additive and invisible to an older build, like every table since 4.0.
    /// `rss_taken` is keyed by every identity of an article (guid and torrent
    /// link): a feed that re-publishes an item under a new guid, or the same
    /// link in two feeds, is still recognised. Rows are small and one per
    /// download, so they are kept.
    fn ensure_rss_tables(&self) -> anyhow::Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS rss_feeds (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 uid TEXT NOT NULL DEFAULT '',
                 path TEXT NOT NULL UNIQUE,
                 url TEXT NOT NULL UNIQUE,
                 refresh_secs INTEGER NOT NULL DEFAULT 1800,
                 title TEXT NOT NULL DEFAULT '',
                 last_refresh INTEGER NOT NULL DEFAULT 0,
                 last_error TEXT NOT NULL DEFAULT '',
                 created_at INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE IF NOT EXISTS rss_folders (path TEXT PRIMARY KEY);
             CREATE TABLE IF NOT EXISTS rss_articles (
                 feed_id INTEGER NOT NULL,
                 guid TEXT NOT NULL,
                 title TEXT NOT NULL DEFAULT '',
                 torrent_url TEXT NOT NULL DEFAULT '',
                 link TEXT NOT NULL DEFAULT '',
                 description TEXT NOT NULL DEFAULT '',
                 author TEXT NOT NULL DEFAULT '',
                 category TEXT NOT NULL DEFAULT '',
                 date INTEGER NOT NULL DEFAULT 0,
                 size INTEGER NOT NULL DEFAULT 0,
                 is_read INTEGER NOT NULL DEFAULT 0,
                 seen_at INTEGER NOT NULL DEFAULT 0,
                 PRIMARY KEY (feed_id, guid));
             CREATE TABLE IF NOT EXISTS rss_rules (
                 name TEXT PRIMARY KEY,
                 def TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS rss_taken (
                 key TEXT PRIMARY KEY,
                 rule TEXT NOT NULL DEFAULT '',
                 at INTEGER NOT NULL DEFAULT 0,
                 info_hash TEXT NOT NULL DEFAULT '',
                 outcome TEXT NOT NULL DEFAULT '');",
        )?;
        Ok(())
    }

    pub fn rss_feeds(&self) -> anyhow::Result<Vec<RssFeedRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, uid, path, url, refresh_secs, title, last_refresh, last_error, created_at
             FROM rss_feeds ORDER BY path",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(RssFeedRow {
                id: r.get(0)?,
                uid: r.get(1)?,
                path: r.get(2)?,
                url: r.get(3)?,
                refresh_secs: r.get(4)?,
                title: r.get(5)?,
                last_refresh: r.get(6)?,
                last_error: r.get(7)?,
                created_at: r.get(8)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// A new feed; its id. A path or URL already taken is an error.
    pub fn rss_feed_insert(&self, f: &RssFeedRow) -> anyhow::Result<i64> {
        self.conn.execute(
            "INSERT INTO rss_feeds (uid, path, url, refresh_secs, title, last_refresh, last_error, created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            rusqlite::params![f.uid, f.path, f.url, f.refresh_secs, f.title, f.last_refresh, f.last_error, f.created_at],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn rss_feed_update(&self, f: &RssFeedRow) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE rss_feeds SET path=?2, url=?3, refresh_secs=?4, title=?5, last_refresh=?6, last_error=?7
             WHERE id=?1",
            rusqlite::params![f.id, f.path, f.url, f.refresh_secs, f.title, f.last_refresh, f.last_error],
        )?;
        Ok(())
    }

    /// What a refresh found, and only that: a rename or a new URL saved
    /// while the feed was being fetched is not undone by the fetch ending.
    pub fn rss_feed_refreshed(
        &self,
        id: i64,
        title: &str,
        at: i64,
        error: &str,
    ) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE rss_feeds SET title=CASE WHEN ?2='' THEN title ELSE ?2 END, last_refresh=?3, last_error=?4 WHERE id=?1",
            rusqlite::params![id, title, at, error],
        )?;
        Ok(())
    }

    /// The feed and its articles. What its rules took stays taken.
    pub fn rss_feed_delete(&self, id: i64) -> anyhow::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM rss_articles WHERE feed_id=?1", [id])?;
        tx.execute("DELETE FROM rss_feeds WHERE id=?1", [id])?;
        tx.commit()?;
        Ok(())
    }

    pub fn rss_folders(&self) -> anyhow::Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM rss_folders ORDER BY path")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn rss_folder_put(&self, path: &str) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO rss_folders (path) VALUES (?1)",
            [path],
        )?;
        Ok(())
    }

    pub fn rss_folder_delete(&self, path: &str) -> anyhow::Result<()> {
        self.conn
            .execute("DELETE FROM rss_folders WHERE path=?1", [path])?;
        Ok(())
    }

    /// A feed's articles, newest first.
    pub fn rss_articles(&self, feed_id: i64) -> anyhow::Result<Vec<RssArticleRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT feed_id, guid, title, torrent_url, link, description, author, category, date, size, is_read, seen_at
             FROM rss_articles WHERE feed_id=?1 ORDER BY date DESC, seen_at DESC, guid",
        )?;
        let rows = stmt.query_map([feed_id], |r| {
            Ok(RssArticleRow {
                feed_id: r.get(0)?,
                guid: r.get(1)?,
                title: r.get(2)?,
                torrent_url: r.get(3)?,
                link: r.get(4)?,
                description: r.get(5)?,
                author: r.get(6)?,
                category: r.get(7)?,
                date: r.get(8)?,
                size: r.get(9)?,
                is_read: r.get::<_, i64>(10)? != 0,
                seen_at: r.get(11)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Articles of one refresh: the new ones go in unread, the known ones keep
    /// their read flag, and only the newest `keep` stay. The guids that were
    /// new, in the order given.
    pub fn rss_articles_merge(
        &self,
        feed_id: i64,
        articles: &[RssArticleRow],
        keep: i64,
    ) -> anyhow::Result<Vec<String>> {
        let tx = self.conn.unchecked_transaction()?;
        let mut fresh = Vec::new();
        {
            let mut ins = tx.prepare(
                "INSERT OR IGNORE INTO rss_articles
                     (feed_id, guid, title, torrent_url, link, description, author, category, date, size, is_read, seen_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,0,?11)",
            )?;
            for a in articles {
                let n = ins.execute(rusqlite::params![
                    feed_id,
                    a.guid,
                    a.title,
                    a.torrent_url,
                    a.link,
                    a.description,
                    a.author,
                    a.category,
                    a.date,
                    a.size,
                    a.seen_at
                ])?;
                if n > 0 {
                    fresh.push(a.guid.clone());
                }
            }
        }
        tx.execute(
            "DELETE FROM rss_articles WHERE feed_id=?1 AND guid NOT IN
                 (SELECT guid FROM rss_articles WHERE feed_id=?1 ORDER BY date DESC, seen_at DESC, guid LIMIT ?2)",
            rusqlite::params![feed_id, keep.max(1)],
        )?;
        tx.commit()?;
        Ok(fresh)
    }

    /// Per feed: (articles, unread). One grouped read for the whole list.
    pub fn rss_article_counts(&self) -> anyhow::Result<std::collections::HashMap<i64, (i64, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT feed_id, COUNT(*), SUM(CASE WHEN is_read=0 THEN 1 ELSE 0 END) FROM rss_articles GROUP BY feed_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                (r.get::<_, i64>(1)?, r.get::<_, i64>(2)?),
            ))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Mark one article read (`guid`), or all of a feed's. How many changed.
    pub fn rss_mark_read(&self, feed_id: i64, guid: Option<&str>) -> anyhow::Result<usize> {
        Ok(match guid {
            Some(g) => self.conn.execute(
                "UPDATE rss_articles SET is_read=1 WHERE feed_id=?1 AND guid=?2 AND is_read=0",
                rusqlite::params![feed_id, g],
            )?,
            None => self.conn.execute(
                "UPDATE rss_articles SET is_read=1 WHERE feed_id=?1 AND is_read=0",
                [feed_id],
            )?,
        })
    }

    /// Every rule: name and qBittorrent document, by name.
    pub fn rss_rules(&self) -> anyhow::Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name, def FROM rss_rules ORDER BY name")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn rss_rule_put(&self, name: &str, def: &str) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT INTO rss_rules (name, def) VALUES (?1, ?2) ON CONFLICT(name) DO UPDATE SET def=excluded.def",
            rusqlite::params![name, def],
        )?;
        Ok(())
    }

    /// False when there is no such rule; an error when the new name is taken.
    pub fn rss_rule_rename(&self, old: &str, new: &str) -> anyhow::Result<bool> {
        Ok(self.conn.execute(
            "UPDATE rss_rules SET name=?2 WHERE name=?1",
            rusqlite::params![old, new],
        )? > 0)
    }

    pub fn rss_rule_delete(&self, name: &str) -> anyhow::Result<bool> {
        Ok(self
            .conn
            .execute("DELETE FROM rss_rules WHERE name=?1", [name])?
            > 0)
    }

    /// What took the first of these keys that was taken, if any.
    pub fn rss_taken(&self, keys: &[&str]) -> anyhow::Result<Option<RssTaken>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT rule, at, info_hash, outcome FROM rss_taken WHERE key=?1")?;
        for k in keys.iter().filter(|k| !k.is_empty()) {
            let row = stmt.query_row([k], |r| {
                Ok(RssTaken {
                    rule: r.get(0)?,
                    at: r.get(1)?,
                    info_hash: r.get(2)?,
                    outcome: r.get(3)?,
                })
            });
            match row {
                Ok(t) => return Ok(Some(t)),
                Err(rusqlite::Error::QueryReturnedNoRows) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(None)
    }

    /// Record every key of an article as taken. A key already there keeps
    /// its first record.
    pub fn rss_take(&self, keys: &[&str], t: &RssTaken) -> anyhow::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        for k in keys.iter().filter(|k| !k.is_empty()) {
            tx.execute(
                "INSERT OR IGNORE INTO rss_taken (key, rule, at, info_hash, outcome) VALUES (?1,?2,?3,?4,?5)",
                rusqlite::params![k, t.rule, t.at, t.info_hash, t.outcome],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod rss_store_tests {
    use super::*;

    fn feed(path: &str, url: &str) -> RssFeedRow {
        RssFeedRow {
            uid: "{u}".into(),
            path: path.into(),
            url: url.into(),
            refresh_secs: 1800,
            ..Default::default()
        }
    }

    fn art(guid: &str, date: i64) -> RssArticleRow {
        RssArticleRow {
            guid: guid.into(),
            title: guid.into(),
            date,
            seen_at: 1,
            ..Default::default()
        }
    }

    /// Everything RSS keeps is still there after the store is closed and
    /// opened again: feeds, folders, articles and their read flag, rules and
    /// what the rules took.
    #[test]
    fn rss_state_survives_a_reopen() {
        let dir = std::env::temp_dir().join(format!("hydra-rss-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hydra.db");
        {
            let s = Store::open(&path, false).unwrap();
            let id = s
                .rss_feed_insert(&feed("Folder\\Tracker", "https://t.example/rss?passkey=x"))
                .unwrap();
            s.rss_folder_put("Folder").unwrap();
            assert_eq!(
                s.rss_articles_merge(id, &[art("a", 10), art("b", 20)], 200)
                    .unwrap(),
                vec!["a", "b"]
            );
            s.rss_mark_read(id, Some("a")).unwrap();
            s.rss_rule_put("tv", "{\"enabled\":true}").unwrap();
            s.rss_take(
                &["a", "https://t.example/a.torrent"],
                &RssTaken {
                    rule: "tv".into(),
                    at: 5,
                    info_hash: "h".into(),
                    outcome: "added".into(),
                },
            )
            .unwrap();
        }
        let s = Store::open(&path, false).unwrap();
        let feeds = s.rss_feeds().unwrap();
        assert_eq!(feeds.len(), 1);
        assert_eq!(feeds[0].path, "Folder\\Tracker");
        assert_eq!(s.rss_folders().unwrap(), vec!["Folder"]);
        let arts = s.rss_articles(feeds[0].id).unwrap();
        assert_eq!(
            arts.iter()
                .map(|a| (a.guid.as_str(), a.is_read))
                .collect::<Vec<_>>(),
            vec![("b", false), ("a", true)]
        );
        assert_eq!(
            s.rss_rules().unwrap(),
            vec![("tv".to_string(), "{\"enabled\":true}".to_string())]
        );
        let t = s
            .rss_taken(&["nope", "https://t.example/a.torrent"])
            .unwrap()
            .expect("taken by its link");
        assert_eq!((t.rule.as_str(), t.outcome.as_str()), ("tv", "added"));
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_refresh_keeps_the_read_flag_and_only_the_newest_articles() {
        let s = Store::open_in_memory().unwrap();
        s.ensure_schema().unwrap();
        let id = s.rss_feed_insert(&feed("F", "u")).unwrap();
        s.rss_articles_merge(id, &[art("old", 1), art("mid", 2)], 2)
            .unwrap();
        s.rss_mark_read(id, None).unwrap();
        let fresh = s
            .rss_articles_merge(id, &[art("mid", 2), art("new", 3)], 2)
            .unwrap();
        assert_eq!(fresh, vec!["new"], "a known guid is not new");
        let arts = s.rss_articles(id).unwrap();
        assert_eq!(
            arts.iter()
                .map(|a| (a.guid.as_str(), a.is_read))
                .collect::<Vec<_>>(),
            vec![("new", false), ("mid", true)]
        );
        assert!(
            s.rss_feed_insert(&feed("F", "other")).is_err(),
            "a path is unique"
        );
        assert!(
            s.rss_feed_insert(&feed("G", "u")).is_err(),
            "a URL is unique"
        );
        s.rss_feed_delete(id).unwrap();
        assert!(s.rss_articles(id).unwrap().is_empty());
    }
}

#[cfg(test)]
mod composite_key_tests {
    use super::*;

    fn blob() -> Vec<u8> {
        b"d4:infod6:lengthi1eee".to_vec()
    }

    /// A second add of a torrent already held must NOT look like a first one.
    ///
    /// The caller cleans up "its" row when the engine refuses, and the refusal
    /// it meets in practice is "already added". Told the insert succeeded, it
    /// deletes the row of the copy still running: 2026-09-24, 20 torrents left
    /// in the engine with no store row, no lookup by infohash, no category, and
    /// every piece refused for want of a metainfo.
    #[test]
    fn insert_torrent_reports_only_the_row_it_created() {
        let s = Store::open_in_memory().unwrap();
        assert!(
            s.insert_torrent("aa", "hoard", &blob(), "/data", "movies", 1.0, false, "")
                .unwrap(),
            "the first add creates the row"
        );
        assert!(
            !s.insert_torrent("aa", "hoard", &blob(), "/data", "movies", 1.0, false, "")
                .unwrap(),
            "a duplicate add creates nothing, and must say so"
        );
        // A DIFFERENT engine is a different copy, so that one IS a creation.
        assert!(
            s.insert_torrent("aa", "vpn1", &blob(), "/data", "movies", 1.0, false, "")
                .unwrap(),
            "a second engine holding the same torrent is its own row"
        );
    }

    /// The migration must keep every row and change only the key.
    ///
    /// Run against a store created with the OLD schema, which is what a
    /// production database is until this build opens it.
    #[test]
    fn the_rebuild_keeps_every_row_and_re_keys_the_table() {
        let s = Store::open_in_memory().unwrap();
        s.insert_torrent("aa", "hoard", &blob(), "/data", "movies", 1.0, false, "x")
            .unwrap();
        s.insert_torrent("bb", "race", &blob(), "/race", "", 2.0, true, "")
            .unwrap();

        s.migrate_composite_key().unwrap();

        let sql: String = s
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='torrents'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(sql.contains("PRIMARY KEY (info_hash, session)"), "{sql}");

        let n: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM torrents", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2, "a row was lost in the rebuild");
        // Columns came across, not just the keys.
        let cat: String = s
            .conn
            .query_row(
                "SELECT category FROM torrents WHERE info_hash='aa'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cat, "movies");
        assert_eq!(s.torrent_blob("aa").unwrap().unwrap(), blob());
    }

    /// Running it twice must be a no-op, because it runs at every boot.
    #[test]
    fn migrating_an_already_migrated_table_does_nothing() {
        let s = Store::open_in_memory().unwrap();
        s.insert_torrent("aa", "hoard", &blob(), "/data", "", 1.0, false, "")
            .unwrap();
        s.migrate_composite_key().unwrap();
        s.migrate_composite_key().unwrap();
        let n: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM torrents", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    /// The point of the whole change: one torrent, two engines.
    #[test]
    fn one_torrent_can_live_in_two_engines() {
        let s = Store::open_in_memory().unwrap();
        s.migrate_composite_key().unwrap();
        s.insert_torrent("aa", "hoard", &blob(), "/data", "movies", 1.0, false, "")
            .unwrap();
        s.insert_torrent("aa", "vpn1", &blob(), "/data", "movies", 1.0, false, "")
            .unwrap();
        assert_eq!(s.sessions_of("aa"), vec!["hoard", "vpn1"]);
    }

    /// Pause describes an execution, so it must not leak between copies: a
    /// torrent held back on one tunnel keeps seeding on the other.
    #[test]
    fn pausing_one_copy_leaves_the_other_running() {
        let s = Store::open_in_memory().unwrap();
        s.migrate_composite_key().unwrap();
        s.insert_torrent("aa", "hoard", &blob(), "/data", "", 1.0, false, "")
            .unwrap();
        s.insert_torrent("aa", "vpn1", &blob(), "/data", "", 1.0, false, "")
            .unwrap();

        s.set_paused("aa", "hoard", true).unwrap();
        let paused = |sess: &str| -> i64 {
            s.conn
                .query_row(
                    "SELECT paused FROM torrents WHERE info_hash='aa' AND session=?1",
                    [sess],
                    |r| r.get(0),
                )
                .unwrap()
        };
        assert_eq!(paused("hoard"), 1);
        assert_eq!(paused("vpn1"), 0, "the other copy was paused too");

        // And the shim's torrent-wide form reaches both.
        s.set_paused_everywhere("aa", true).unwrap();
        assert_eq!(paused("vpn1"), 1);
    }

    #[test]
    fn deleting_one_copy_leaves_the_other() {
        let s = Store::open_in_memory().unwrap();
        s.migrate_composite_key().unwrap();
        s.insert_torrent("aa", "hoard", &blob(), "/data", "", 1.0, false, "")
            .unwrap();
        s.insert_torrent("aa", "vpn1", &blob(), "/data", "", 1.0, false, "")
            .unwrap();
        assert!(s.delete_copy("aa", "hoard").unwrap());
        assert_eq!(s.sessions_of("aa"), vec!["vpn1"]);
        assert!(!s.delete_copy("aa", "hoard").unwrap(), "already gone");
    }

    /// A move takes the source, or three copies would collapse into one.
    #[test]
    fn moving_a_copy_moves_only_that_copy() {
        let s = Store::open_in_memory().unwrap();
        s.migrate_composite_key().unwrap();
        s.insert_torrent("aa", "hoard", &blob(), "/data", "", 1.0, false, "")
            .unwrap();
        s.insert_torrent("aa", "vpn1", &blob(), "/data", "", 1.0, false, "")
            .unwrap();
        s.set_session("aa", "hoard", "vpn2").unwrap();
        assert_eq!(s.sessions_of("aa"), vec!["vpn1", "vpn2"]);
    }
}

#[cfg(test)]
mod enrol_tests {
    use super::*;

    fn store() -> Store {
        let s = Store::open_in_memory().unwrap();
        s.ensure_enrol_table().unwrap();
        s.ensure_nodes_table().unwrap();
        s
    }

    /// A token is authority to join the fleet, so spending it twice must be
    /// impossible: a token left in a shell scrollback would otherwise enrol a
    /// second machine nobody asked for.
    #[test]
    fn a_token_can_only_be_spent_once() {
        let s = store();
        let (token, _) = s.create_enrol_token(1800).unwrap();
        assert!(
            s.consume_enrol_token(&token).unwrap(),
            "first use must work"
        );
        assert!(
            !s.consume_enrol_token(&token).unwrap(),
            "second use must not"
        );
    }

    #[test]
    fn an_expired_token_is_refused() {
        let s = store();
        // Minted already stale: the window is what limits a leaked token, so
        // the check has to be on the clock and not on a flag someone forgot.
        let (token, _) = s.create_enrol_token(-1).unwrap();
        assert!(!s.consume_enrol_token(&token).unwrap());
    }

    #[test]
    fn an_unknown_token_is_refused() {
        let s = store();
        assert!(!s.consume_enrol_token("0000000000000000").unwrap());
    }

    #[test]
    fn two_tokens_are_not_the_same_token() {
        let s = store();
        let (a, _) = s.create_enrol_token(60).unwrap();
        let (b, _) = s.create_enrol_token(60).unwrap();
        assert_ne!(a, b);
        assert_eq!(
            a.len(),
            32,
            "short enough to paste, long enough not to guess"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The write mark is what the facts cache trusts to know it is stale.
    ///
    /// Every write has to move it, whichever method made it -- that is the
    /// whole point of taking SQLite's own tally instead of a counter this file
    /// would have to remember to bump in sixty places. Pin the property here:
    /// an insert, an update and a delete each move it, a read never does.
    #[test]
    fn every_write_moves_the_mark_and_no_read_does() {
        let store = fresh();
        let a = "a".repeat(40);

        let start = store.write_mark();
        store
            .insert_torrent(&a, "hoard", b"x", "", "", 0.0, false, "")
            .unwrap();
        let after_insert = store.write_mark();
        assert!(after_insert > start, "an insert must move the mark");

        store.set_category_in(&a, "hoard", "movies").unwrap();
        let after_update = store.write_mark();
        assert!(after_update > after_insert, "an update must move the mark");

        // Reads are what the cache does between writes; if they moved the mark
        // it would rebuild on every request and buy nothing.
        let _ = store.slim_facts("hoard").unwrap();
        let _ = store.facts_by_session("hoard").unwrap();
        let _ = store.facts_in_category("hoard", "movies").unwrap();
        assert_eq!(store.write_mark(), after_update, "a read must not move it");

        store.delete_torrent(&a).unwrap();
        assert!(
            store.write_mark() > after_update,
            "a delete must move the mark"
        );
    }

    /// The bug this whole mechanism exists to prevent.
    ///
    /// The published totals are a live sum over the loaded torrents plus these
    /// counters. Drop a row without folding its bytes in and the figure goes
    /// DOWN -- which is how deleting torrents erased 7 TB of lifetime upload in
    /// a day on prod, from the all-time total as well as the day's.
    #[test]
    fn a_removed_torrent_leaves_its_bytes_in_the_counters() {
        let store = fresh();
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        store
            .insert_torrent(&a, "hoard", b"x", "", "", 0.0, false, "")
            .unwrap();
        let keys = vec![
            "global".to_string(),
            Store::tracker_counter_key("hoard", "tk.example.net"),
        ];

        assert!(store.delete_absorb(&a, None, &keys, 1000, 100).unwrap());
        assert!(!store.has_torrent_blob(&a), "the row must be gone");
        for key in &keys {
            assert_eq!(store.counter(key), (1000, 100), "counter {key}");
        }

        // A second removal ACCUMULATES rather than overwrites. `set_counter`
        // would have passed the first assertion and silently lost this one --
        // that is the difference between a carry-over and a gauge.
        store
            .insert_torrent(&b, "hoard", b"y", "", "", 0.0, false, "")
            .unwrap();
        assert!(store.delete_absorb(&b, None, &keys, 5, 5).unwrap());
        for key in &keys {
            assert_eq!(
                store.counter(key),
                (1005, 105),
                "counter {key} after the second"
            );
        }
    }

    /// A torrent that never moved a byte still has to lose its row.
    #[test]
    fn a_removal_with_no_bytes_still_drops_the_row() {
        let store = fresh();
        let a = "a".repeat(40);
        store
            .insert_torrent(&a, "hoard", b"x", "", "", 0.0, false, "")
            .unwrap();
        assert!(store
            .delete_absorb(&a, None, &["global".to_string()], 0, 0)
            .unwrap());
        assert!(!store.has_torrent_blob(&a));
        assert_eq!(
            store.counter("global"),
            (0, 0),
            "nothing to fold, nothing folded"
        );
    }

    /// Naming a session drops THAT copy only: two engines seeding one payload
    /// are two rows, and removing one must not take the other's row with it.
    #[test]
    fn absorbing_one_copy_leaves_the_other() {
        // On the MIGRATED key: one row per (hash, session) is what makes two
        // copies possible in the first place, and `fresh()` is still on the
        // pre-migration schema a new install starts from.
        let store = Store::open_in_memory().unwrap();
        store.migrate_composite_key().unwrap();
        let a = "a".repeat(40);
        store
            .insert_torrent(&a, "hoard", b"x", "", "", 0.0, false, "")
            .unwrap();
        store
            .insert_torrent(&a, "race", b"x", "", "", 0.0, false, "")
            .unwrap();

        assert!(store
            .delete_absorb(&a, Some("hoard"), &["global".to_string()], 7, 3)
            .unwrap());
        assert_eq!(store.sessions_of(&a), vec!["race".to_string()]);
        assert_eq!(store.counter("global"), (7, 3));
    }

    /// The per-category query is the whole-session one, narrowed -- the qBit
    /// shim swaps between them by which the client asked for, so a difference
    /// in the facts would be a difference in what *arr sees.
    #[test]
    fn one_category_reads_exactly_what_the_whole_session_would() {
        let store = fresh();
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        store
            .insert_torrent(&a, "hoard", b"x", "/data/one", "movies", 11.0, false, "")
            .unwrap();
        store
            .insert_torrent(&b, "hoard", b"x", "/data/two", "series", 22.0, true, "")
            .unwrap();

        let whole = store.facts_by_session("hoard").unwrap();
        let narrowed = store.facts_in_category("hoard", "movies").unwrap();

        assert_eq!(narrowed.len(), 1, "only the category asked for");
        let from_narrow = narrowed.get(&a).expect("the movies torrent");
        let from_whole = whole.get(&a).expect("the movies torrent");
        assert_eq!(from_narrow.category, from_whole.category);
        assert_eq!(from_narrow.save_path, from_whole.save_path);
        assert_eq!(from_narrow.added_time, from_whole.added_time);
        assert_eq!(from_narrow.user_paused, from_whole.user_paused);
        assert_eq!(from_narrow.tags, from_whole.tags);
    }

    /// What the download slot manager asks on every pass.
    ///
    /// It must see the operator's pauses and only those: a scheduler that
    /// cannot tell "the human stopped this" from "I parked this myself"
    /// restarts the first, which is exactly how a paused torrent kept
    /// downloading at 8 MB/s while the interface said stopped.
    #[test]
    fn paused_hashes_names_the_stopped_of_that_session_only() {
        let store = fresh();
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let c = "c".repeat(40);
        store
            .insert_torrent(&a, "hoard", b"x", "", "", 0.0, true, "")
            .unwrap();
        store
            .insert_torrent(&b, "hoard", b"x", "", "", 0.0, false, "")
            .unwrap();
        // Same decision, different engine: asking for hoard must not return it.
        store
            .insert_torrent(&c, "race", b"x", "", "", 0.0, true, "")
            .unwrap();

        let mut paused = store.paused_hashes("hoard").unwrap();
        paused.sort();
        assert_eq!(paused, vec![a.clone()], "only the paused hoard torrent");

        assert_eq!(store.paused_hashes("race").unwrap(), vec![c]);

        // And it follows the intent rather than caching it.
        store.set_paused(&b, "hoard", true).unwrap();
        store.set_paused(&a, "hoard", false).unwrap();
        assert_eq!(store.paused_hashes("hoard").unwrap(), vec![b]);
    }

    /// A store on the REAL schema, not a hand-copied subset of it.
    ///
    /// This used to declare `torrents` inline and nothing else, so every test
    /// ran against a database missing `counters`, `meta`, `tag_registry` and
    /// the rest. Anything reading those swallowed "no such table" through an
    /// `unwrap_or` and passed -- a second schema definition that drifts from
    /// the first tests the drift, not the code.
    fn fresh() -> Store {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        let mut store = Store::bare(conn);
        store.track_changes();
        store
    }

    #[test]
    fn the_frozen_schema_is_accepted() {
        assert!(fresh().check_schema().is_ok());
    }

    /// ⭐ A FRESH database must be born with the key it will end up with.
    ///
    /// `SCHEMA` declared `info_hash TEXT PRIMARY KEY` while the production
    /// database has been re-keyed to `(info_hash, session)` by
    /// `migrate_composite_key`. A new install therefore started life unable to
    /// hold two copies of a torrent, and -- worse -- every test written on the
    /// bare constant silently tested a shape production does not have. Two of
    /// mine failed on it on 2026-09-16 with a UNIQUE constraint violation, on a
    /// case that is perfectly legal in production.
    #[test]
    fn a_new_database_is_keyed_on_the_copy_like_a_migrated_one() {
        let s = fresh();
        let sql: String = s
            .conn
            .query_row(
                "SELECT COALESCE(sql,'') FROM sqlite_master WHERE type='table' AND name='torrents'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            sql.contains("PRIMARY KEY (info_hash, session)"),
            "a fresh schema must carry the composite key: {sql}"
        );
        // Exactly the string `migrate_composite_key` looks for, so the
        // migration is a no-op here instead of rewriting a brand new table.
        assert!(s.migrate_composite_key().is_ok());

        // And the shape holds: two engines, one content, two rows.
        s.conn
            .execute_batch(
                "INSERT INTO torrents (info_hash, session, torrent) VALUES
                   ('aa','hoard',x''), ('aa','race',x'');",
            )
            .expect("a fresh database must accept one row per copy");
    }

    // Proven by breaking it: the guard is only worth having if it actually
    // refuses a database that drifted, so drop a column and check it complains.
    #[test]
    fn a_missing_column_is_refused() {
        let store = fresh();
        store
            .conn
            .execute_batch("ALTER TABLE torrents DROP COLUMN seeding_time;")
            .unwrap();
        let err = store.check_schema().unwrap_err().to_string();
        assert!(err.contains("seeding_time"), "unhelpful error: {err}");
    }

    #[test]
    fn counts_group_by_session() {
        let store = fresh();
        store
            .conn
            .execute_batch(
                "INSERT INTO torrents (info_hash, session, torrent) VALUES
                   ('a','hoard',x''), ('b','hoard',x''), ('c','race',x'');",
            )
            .unwrap();
        assert_eq!(store.count_torrents().unwrap(), 3);
        assert_eq!(
            store.count_by_session().unwrap(),
            vec![("hoard".to_string(), 2), ("race".to_string(), 1)]
        );
    }
}

/// The batched pause/resume added after the 2026-09-16 bulk-start incident.
///
/// ⚠ These use `ensure_schema()`, not the raw `SCHEMA` constant the module
/// above uses: `SCHEMA` still declares `info_hash TEXT PRIMARY KEY`, one row per
/// torrent, and the migration to `(info_hash, session)` is what gives a torrent
/// one row per COPY. Tests written on the bare constant cannot hold two copies
/// of the same hash at all, which is precisely the case that matters here.
#[cfg(test)]
mod paused_batch_tests {
    use super::*;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    /// 'A' is held by two engines, 'B' by one: three rows, two hashes.
    fn three_copies() -> Store {
        let s = Store::open_in_memory().expect("in-memory store");
        s.ensure_schema().expect("schema");
        for (hash, session) in [(A, "hoard"), (B, "hoard"), (A, "race")] {
            s.insert_torrent(
                hash,
                session,
                b"d4:infod4:name4:teseee",
                "/data",
                "cat",
                1.0,
                false,
                "",
            )
            .expect("insert");
        }
        s
    }

    fn paused_of(store: &Store, hash: &str, session: &str) -> i64 {
        store
            .conn
            .query_row(
                "SELECT paused FROM torrents WHERE info_hash = ?1 AND session = ?2",
                rusqlite::params![hash, session],
                |r| r.get(0),
            )
            .unwrap()
    }

    #[test]
    fn the_batch_pauses_every_copy_and_counts_rows_not_hashes() {
        let store = three_copies();
        let n = store
            .set_paused_everywhere_batch(&[A.to_string(), B.to_string()], true)
            .unwrap();
        assert_eq!(n, 3, "both copies of A plus B");
        assert_eq!(paused_of(&store, A, "hoard"), 1);
        assert_eq!(paused_of(&store, A, "race"), 1);
        assert_eq!(paused_of(&store, B, "hoard"), 1);
    }

    #[test]
    fn the_per_session_batch_leaves_the_other_copy_alone() {
        let store = three_copies();
        let n = store
            .set_paused_batch(&[A.to_string()], "hoard", true)
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(paused_of(&store, A, "hoard"), 1);
        // The race copy of the same content keeps running: pause describes an
        // execution, not the content.
        assert_eq!(paused_of(&store, A, "race"), 0);
    }

    // An empty set is a no-op, NOT "everything". The whole 2026-09-16 incident
    // was one layer reading empty as a wildcard; the store must never do it.
    #[test]
    fn an_empty_batch_touches_nothing() {
        let store = three_copies();
        assert_eq!(store.set_paused_everywhere_batch(&[], true).unwrap(), 0);
        assert_eq!(store.set_paused_batch(&[], "hoard", true).unwrap(), 0);
        assert_eq!(paused_of(&store, A, "hoard"), 0);
        assert_eq!(paused_of(&store, B, "hoard"), 0);
    }

    // The batch is one transaction: a hash nobody holds updates zero rows and
    // must not abort the ones around it.
    #[test]
    fn an_unknown_hash_does_not_sink_the_batch() {
        let store = three_copies();
        let nope = "cccccccccccccccccccccccccccccccccccccccc".to_string();
        let n = store
            .set_paused_everywhere_batch(&[A.to_string(), nope, B.to_string()], true)
            .unwrap();
        assert_eq!(
            n, 3,
            "two copies of A plus B; the unknown hash matches nothing"
        );
        assert_eq!(paused_of(&store, B, "hoard"), 1);
    }

    /// ⭐ THE REGRESSION OF 2026-09-16: the Hoard table showed "Race".
    ///
    /// A torrent seeded by both engines has two rows. Looked up by hash alone,
    /// whichever row SQLite returned last won, and the Hoard page painted the
    /// race copy's category and save path onto the hoard row.
    #[test]
    fn facts_are_read_per_copy_not_per_content() {
        let s = Store::open_in_memory().expect("in-memory store");
        s.ensure_schema().expect("schema");
        s.insert_torrent(
            A,
            "hoard",
            b"d4:infod4:name4:teseee",
            "/data/tv",
            "series",
            1.0,
            false,
            "",
        )
        .expect("hoard copy");
        s.insert_torrent(
            A,
            "race",
            b"d4:infod4:name4:teseee",
            "/race/torrents",
            "Race",
            1.0,
            false,
            "",
        )
        .expect("race copy");

        let hashes = vec![A.to_string()];
        let hoard = s.facts_for_hashes(&hashes, "hoard").expect("hoard facts");
        let race = s.facts_for_hashes(&hashes, "race").expect("race facts");

        let h = hoard.get(A).expect("the hoard copy is found");
        let r = race.get(A).expect("the race copy is found");
        assert_eq!(
            h.category, "series",
            "the hoard row must not wear the race category"
        );
        assert_eq!(h.save_path, "/data/tv");
        assert_eq!(r.category, "Race", "and the race row keeps its own");
        assert_eq!(r.save_path, "/race/torrents");
    }

    /// A session that holds nothing answers nothing, rather than borrowing the
    /// other engine's copy.
    #[test]
    fn a_session_without_the_torrent_gets_no_facts() {
        let s = Store::open_in_memory().expect("in-memory store");
        s.ensure_schema().expect("schema");
        s.insert_torrent(
            A,
            "race",
            b"d4:infod4:name4:teseee",
            "/race/torrents",
            "Race",
            1.0,
            false,
            "",
        )
        .expect("race copy");
        let facts = s
            .facts_for_hashes(&[A.to_string()], "hoard")
            .expect("facts");
        assert!(
            facts.is_empty(),
            "hoard holds no copy, so it has no facts to show"
        );
    }

    #[test]
    fn the_batch_resumes_as_well_as_it_pauses() {
        let store = three_copies();
        let both = [A.to_string(), B.to_string()];
        store.set_paused_everywhere_batch(&both, true).unwrap();
        assert_eq!(store.set_paused_everywhere_batch(&both, false).unwrap(), 3);
        assert_eq!(paused_of(&store, A, "hoard"), 0);
        assert_eq!(paused_of(&store, A, "race"), 0);
        assert_eq!(paused_of(&store, B, "hoard"), 0);
    }
}

#[cfg(test)]
mod absorb_and_copies_tests {
    use super::*;

    fn store() -> Store {
        let s = Store::open_in_memory().expect("in-memory store");
        s.ensure_schema().expect("schema");
        s
    }

    const H: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const H2: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn add(s: &Store, hash: &str, session: &str) {
        s.insert_torrent(
            hash,
            session,
            b"d4:infod4:name4:teseee",
            "/data",
            "cat",
            1.0,
            false,
            "",
        )
        .expect("insert");
    }

    /// ⭐⭐ THE regression of 14/09: removing a torrent used to take its bytes
    /// out of the lifetime total, because the total was summed over the
    /// torrents still loaded. The fold into the carry-over and the deletion of
    /// the row must happen in ONE transaction, or the bytes are simply lost.
    #[test]
    fn deleting_a_torrent_folds_its_bytes_into_the_carry_over() {
        let s = store();
        add(&s, H, "race");
        let keys = vec!["global".to_string(), "race:tracker.example".to_string()];

        assert_eq!(s.counter("global"), (0, 0));
        assert!(s
            .delete_absorb(H, Some("race"), &keys, 5_000, 1_200)
            .unwrap());

        assert_eq!(
            s.counter("global"),
            (5_000, 1_200),
            "the bytes outlived the torrent"
        );
        assert_eq!(s.counter("race:tracker.example"), (5_000, 1_200));
        assert!(s.sessions_of(H).is_empty(), "and the row is gone");
    }

    /// Absorbing twice adds; it never overwrites. A second deletion that reset
    /// the counter would throw away every earlier torrent's contribution.
    #[test]
    fn absorbing_accumulates_rather_than_replaces() {
        let s = store();
        let keys = vec!["global".to_string()];
        add(&s, H, "race");
        s.delete_absorb(H, Some("race"), &keys, 100, 10).unwrap();
        add(&s, H2, "race");
        s.delete_absorb(H2, Some("race"), &keys, 400, 40).unwrap();
        assert_eq!(s.counter("global"), (500, 50));
    }

    /// A torrent that never moved a byte still has to lose its row: only the
    /// folding is conditional, not the deletion.
    #[test]
    fn a_torrent_that_moved_nothing_is_still_deleted() {
        let s = store();
        add(&s, H, "race");
        assert!(s
            .delete_absorb(H, Some("race"), &["global".to_string()], 0, 0)
            .unwrap());
        assert!(s.sessions_of(H).is_empty());
        assert_eq!(
            s.counter("global"),
            (0, 0),
            "no row was written for zero bytes"
        );
    }

    /// Deleting a copy from ONE engine must leave the other engine's copy
    /// alone -- and must not fold the bytes twice.
    #[test]
    fn deleting_one_copy_leaves_the_other_engines_copy() {
        let s = store();
        add(&s, H, "race");
        add(&s, H, "hoard");
        assert_eq!(
            s.sessions_of(H),
            vec!["hoard".to_string(), "race".to_string()]
        );

        s.delete_absorb(H, Some("race"), &["global".to_string()], 7, 3)
            .unwrap();
        assert_eq!(
            s.sessions_of(H),
            vec!["hoard".to_string()],
            "hoard still holds it"
        );
        assert_eq!(s.counter("global"), (7, 3));
    }

    /// No session means every copy: the torrent is leaving the node.
    #[test]
    fn absorbing_without_a_session_removes_every_copy() {
        let s = store();
        add(&s, H, "race");
        add(&s, H, "hoard");
        assert!(s
            .delete_absorb(H, None, &["global".to_string()], 1, 1)
            .unwrap());
        assert!(s.sessions_of(H).is_empty());
    }

    /// Deleting something that is not there is `false`, not an error and not a
    /// counter write: a retried delete must not double-count the bytes.
    #[test]
    fn absorbing_a_torrent_that_is_not_here_reports_false() {
        let s = store();
        assert!(!s
            .delete_absorb(H, Some("race"), &["global".to_string()], 9, 9)
            .unwrap());
    }

    /// ⭐ `insert_torrent` is INSERT OR IGNORE, so it would leave the session
    /// pointing at the old engine. That is how a moved torrent ends up running
    /// in one engine and listed under another.
    #[test]
    fn re_homing_a_torrent_moves_the_copy_it_was_given() {
        let s = store();
        add(&s, H, "race");
        s.set_session(H, "race", "hoard").unwrap();
        assert_eq!(s.sessions_of(H), vec!["hoard".to_string()]);
    }

    #[test]
    fn re_homing_from_an_engine_that_does_not_hold_it_changes_nothing() {
        let s = store();
        add(&s, H, "race");
        s.set_session(H, "hoard", "other").unwrap();
        assert_eq!(
            s.sessions_of(H),
            vec!["race".to_string()],
            "the race copy is untouched"
        );
    }

    #[test]
    fn dropping_one_copy_reports_whether_there_was_one() {
        let s = store();
        add(&s, H, "race");
        assert!(s.delete_copy(H, "race").unwrap());
        assert!(
            !s.delete_copy(H, "race").unwrap(),
            "the second call has nothing to drop"
        );
    }

    /// 13/10/2026: a torrent added during a reconcile pass had its row taken
    /// for an orphan -- written, not yet in the engine -- and kept running with
    /// no metainfo, refusing every piece. A row this process just wrote waits
    /// out the grace; past it, it goes like any other.
    #[test]
    fn the_reconcile_leaves_a_row_written_moments_ago() {
        let s = store();
        add(&s, H, "hoard");
        let grace = std::time::Duration::from_secs(600);
        assert!(
            !s.delete_settled_copy(H, "hoard", grace).unwrap(),
            "just written: kept"
        );
        assert_eq!(s.sessions_of(H), vec!["hoard".to_string()]);
        assert!(
            s.delete_settled_copy(H, "hoard", std::time::Duration::ZERO)
                .unwrap(),
            "settled: dropped"
        );
        assert!(s.sessions_of(H).is_empty());
    }

    /// ⭐ A graduation takes the torrent out of its engine before copying its
    /// files, minutes or hours of it, and the row keeps naming the engine it
    /// left until the end. The reconcile took that row for an orphan and
    /// deleted it with the metainfo the target engine was about to adopt.
    #[test]
    fn the_reconcile_leaves_a_row_whose_torrent_is_being_moved() {
        let s = store();
        add(&s, H, "race");
        s.create_job("graduate", H, "{}", 100).unwrap();
        let running = s.claim_next_job().expect("claimed");
        assert!(
            !s.delete_settled_copy(H, "race", std::time::Duration::ZERO)
                .unwrap(),
            "moving: kept"
        );
        assert!(
            s.torrent_blob(H).unwrap().is_some(),
            "the metainfo is still there"
        );

        s.job_finish(&running.id, "").unwrap();
        assert!(
            s.delete_settled_copy(H, "race", std::time::Duration::ZERO)
                .unwrap(),
            "done: dropped"
        );
    }

    #[test]
    fn a_counter_that_was_never_written_reads_as_zero_not_as_missing() {
        let s = store();
        assert_eq!(s.counter("never-written"), (0, 0));
    }

    #[test]
    fn setting_a_counter_replaces_it() {
        let s = store();
        s.set_counter("k", 10, 20).unwrap();
        s.set_counter("k", 3, 4).unwrap();
        assert_eq!(
            s.counter("k"),
            (3, 4),
            "set replaces, unlike absorb which adds"
        );
    }

    /// The key carries the engine as well as the host: two engines seeding to
    /// the same tracker keep separate obligations, and collapsing them would
    /// credit one engine's upload to the other.
    #[test]
    fn a_tracker_counter_key_separates_engines_on_the_same_host() {
        let a = Store::tracker_counter_key("race", "tracker.example");
        let b = Store::tracker_counter_key("hoard", "tracker.example");
        assert_ne!(a, b);
        assert!(a.contains("race") && a.contains("tracker.example"));
    }

    #[test]
    fn a_torrents_blob_comes_back_as_it_went_in() {
        let s = store();
        add(&s, H, "race");
        assert!(s.has_torrent_blob(H));
        assert_eq!(
            s.torrent_blob(H).unwrap().as_deref(),
            Some(&b"d4:infod4:name4:teseee"[..])
        );
        assert!(!s.has_torrent_blob(H2));
        assert!(s.torrent_blob(H2).unwrap().is_none());
    }

    #[test]
    fn tags_round_trip_and_an_empty_list_clears_them() {
        let s = store();
        add(&s, H, "race");
        s.set_tags(H, &["fr".into(), "anime".into()]).unwrap();
        let mut got = s.tags_of(H);
        got.sort();
        assert_eq!(got, vec!["anime".to_string(), "fr".to_string()]);
        s.set_tags(H, &[]).unwrap();
        assert!(s.tags_of(H).is_empty());
    }

    #[test]
    fn tags_stored_as_json_read_as_tags_and_survive_an_edit() {
        let s = store();
        add(&s, H, "race");
        s.conn
            .execute(
                "UPDATE torrents SET tags = '[\"cross-seed\"]' WHERE info_hash = ?1",
                [H],
            )
            .unwrap();
        assert_eq!(s.tags_of(H), vec!["cross-seed".to_string()]);
        // What a tag action does: read, append, write back.
        let mut tags = s.tags_of(H);
        tags.push("noHL".into());
        s.set_tags(H, &tags).unwrap();
        assert_eq!(
            s.tags_of(H),
            vec!["cross-seed".to_string(), "noHL".to_string()]
        );
        // And the rows the old read already damaged come back whole.
        s.conn
            .execute(
                "UPDATE torrents SET tags = '[\"cross-seed\"],noHL' WHERE info_hash = ?1",
                [H],
            )
            .unwrap();
        assert_eq!(
            s.tags_of(H),
            vec!["cross-seed".to_string(), "noHL".to_string()]
        );
    }

    #[test]
    fn a_pause_is_remembered_per_engine() {
        let s = store();
        add(&s, H, "race");
        add(&s, H, "hoard");
        s.set_paused(H, "race", true).unwrap();
        assert_eq!(s.paused_hashes("race").unwrap(), vec![H.to_string()]);
        assert!(
            s.paused_hashes("hoard").unwrap().is_empty(),
            "the hoard copy still runs"
        );
    }

    #[test]
    fn pausing_everywhere_reaches_every_copy() {
        let s = store();
        add(&s, H, "race");
        add(&s, H, "hoard");
        s.set_paused_everywhere(H, true).unwrap();
        assert_eq!(s.paused_hashes("race").unwrap().len(), 1);
        assert_eq!(s.paused_hashes("hoard").unwrap().len(), 1);
    }

    #[test]
    fn a_save_path_can_be_rewritten_after_a_move() {
        let s = store();
        add(&s, H, "race");
        s.set_save_path(H, "/data/moved").unwrap();
        assert_eq!(
            s.all_hashes("race").unwrap(),
            vec![H.to_string()],
            "the torrent is still listed under its engine after the move"
        );
        assert_eq!(
            s.category_of(H, "race").as_deref(),
            Some("cat"),
            "and keeps its category"
        );
    }

    /// ⭐ A write must not relabel the other engine's copy either.
    ///
    /// `set_category` updated every row for the hash. Renaming the hoard copy
    /// `movies` from the interface also made the race copy `movies`, which
    /// hands it another category's save path and graduation rules.
    #[test]
    fn setting_a_category_touches_only_that_copy() {
        let s = store();
        add(&s, H, "race");
        add(&s, H, "hoard");
        s.set_category_in(H, "hoard", "movies").unwrap();
        assert_eq!(s.category_of(H, "hoard").as_deref(), Some("movies"));
        assert_eq!(
            s.category_of(H, "race").as_deref(),
            Some("cat"),
            "the race copy is untouched"
        );
    }

    /// The qBit shim has no engine to name, so its write stays torrent-wide --
    /// on purpose, and under a name that says so.
    #[test]
    fn the_shim_writes_every_copy_by_design() {
        let s = store();
        add(&s, H, "race");
        add(&s, H, "hoard");
        s.set_category_everywhere(H, "movies").unwrap();
        assert_eq!(s.category_of(H, "hoard").as_deref(), Some("movies"));
        assert_eq!(s.category_of(H, "race").as_deref(), Some("movies"));
    }

    /// ⭐ The drain reads this to decide what may graduate and where to. Asked
    /// by hash alone it answered for whichever copy SQLite reached first, so a
    /// torrent seeded by both engines could be judged on the other one's rules.
    #[test]
    fn a_category_belongs_to_the_copy_not_to_the_content() {
        let s = store();
        add(&s, H, "race");
        add(&s, H, "hoard");
        // Written per copy on purpose: `set_category` still updates EVERY copy
        // (the qBit shim has no notion of engines), so it cannot set up this
        // case. That write is the next one to look at.
        s.conn
            .execute(
                "UPDATE torrents SET category = 'series' WHERE info_hash = ?1 AND session = 'hoard'",
                rusqlite::params![H],
            )
            .unwrap();

        assert_eq!(s.category_of(H, "race").as_deref(), Some("cat"));
        assert_eq!(s.category_of(H, "hoard").as_deref(), Some("series"));
        assert_eq!(
            s.category_of(H, "vpn1"),
            None,
            "an engine that does not hold it must not borrow another copy's category"
        );
    }

    #[test]
    fn the_write_mark_moves_when_something_is_written() {
        let s = store();
        let before = s.write_mark();
        add(&s, H, "race");
        assert_ne!(
            s.write_mark(),
            before,
            "the facts cache must see this as stale"
        );
    }
}

#[cfg(test)]
mod jobs_nodes_drain_tests {
    use super::*;

    fn store() -> Store {
        let s = Store::open_in_memory().expect("in-memory store");
        s.ensure_schema().expect("schema");
        s
    }

    const H: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const H2: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    /// ⭐ Without this gate a drain that runs every minute queues the same
    /// graduation sixty times while the first copy is still going.
    #[test]
    fn a_job_already_in_flight_is_not_queued_again() {
        let s = store();
        assert!(!s.job_pending_for("graduate", H), "nothing is pending yet");
        s.create_job("graduate", H, "{}", 100).unwrap();
        assert!(
            s.job_pending_for("graduate", H),
            "the queued job is pending"
        );
    }

    /// The gate is per KIND and per TORRENT: a move must not be blocked by a
    /// graduation, and another torrent's job is not this torrent's.
    #[test]
    fn the_pending_gate_is_per_kind_and_per_torrent() {
        let s = store();
        s.create_job("graduate", H, "{}", 100).unwrap();
        assert!(
            !s.job_pending_for("move", H),
            "a different kind is not pending"
        );
        assert!(
            !s.job_pending_for("graduate", H2),
            "another torrent is not this one"
        );
    }

    /// A queued job is cancelled outright and never claimed; a running one
    /// is marked and finishes as cancelled when its runner gives up.
    #[test]
    fn a_job_can_be_cancelled_queued_or_running() {
        let s = store();
        let queued = s.create_job("graduate", H, "{}", 100).unwrap();
        assert_eq!(s.cancel_job(&queued).as_deref(), Some("queued"));
        assert_eq!(s.job(&queued).unwrap().state, "cancelled");
        assert!(s.claim_next_job().is_none(), "a cancelled job is not run");

        let running = s.create_job("graduate", H2, "{}", 100).unwrap();
        s.claim_next_job().unwrap();
        assert_eq!(s.cancel_job(&running).as_deref(), Some("running"));
        assert!(s.job_cancelling(&running));
        s.job_finish(&running, "cancelled; rolled back").unwrap();
        assert_eq!(s.job(&running).unwrap().state, "cancelled");
        assert!(s.cancel_job("nope").is_none());
    }

    /// A finished job stops being pending, or the torrent could never be
    /// graduated a second time.
    #[test]
    fn a_finished_job_stops_blocking_the_next_one() {
        let s = store();
        let id = s.create_job("graduate", H, "{}", 100).unwrap();
        let claimed = s.claim_next_job().expect("the queued job is claimable");
        assert_eq!(claimed.id, id);
        s.job_finish(&id, "").unwrap();
        assert!(
            !s.job_pending_for("graduate", H),
            "a finished job is not in flight"
        );
    }

    /// A job is claimed ONCE. Two workers claiming the same job would do the
    /// same copy twice.
    #[test]
    fn a_job_is_claimed_only_once() {
        let s = store();
        s.create_job("move", H, "{}", 10).unwrap();
        assert!(s.claim_next_job().is_some());
        assert!(s.claim_next_job().is_none(), "the queue is empty now");
    }

    #[test]
    fn claiming_from_an_empty_queue_is_none_not_an_error() {
        let s = store();
        assert!(s.claim_next_job().is_none());
    }

    #[test]
    fn progress_and_completion_are_readable_back() {
        let s = store();
        let id = s.create_job("move", H, "{}", 1000).unwrap();
        s.claim_next_job().unwrap();
        s.job_progress(&id, 400).unwrap();
        let j = s.job(&id).expect("the job is there");
        assert_eq!(j.progress_bytes, 400);
        assert_eq!(j.total_bytes, 1000);

        s.job_finish(&id, "disk full").unwrap();
        let done = s.job(&id).expect("still there after finishing");
        assert_eq!(done.error, "disk full", "a failure keeps its reason");
    }

    #[test]
    fn a_job_that_does_not_exist_is_none() {
        let s = store();
        assert!(s.job("no-such-job").is_none());
    }

    /// ⭐ A job left `running` by a crash must go back to the queue at boot,
    /// or the work it was doing is never picked up again and never reported.
    #[test]
    fn a_job_left_running_by_a_crash_is_requeued_at_boot() {
        let s = store();
        s.create_job("move", H, "{}", 10).unwrap();
        s.claim_next_job().expect("claimed, now running");
        assert!(s.claim_next_job().is_none(), "nothing left queued");

        assert_eq!(s.requeue_running_jobs(), 1);
        assert!(
            s.claim_next_job().is_some(),
            "it is claimable again after the requeue"
        );
    }

    #[test]
    fn requeueing_with_nothing_running_changes_nothing() {
        let s = store();
        assert_eq!(s.requeue_running_jobs(), 0);
    }

    #[test]
    fn the_job_listing_is_bounded_by_its_limit() {
        let s = store();
        for i in 0..5 {
            s.create_job("move", &format!("{i}{}", &H[1..]), "{}", 1)
                .unwrap();
        }
        assert_eq!(s.list_jobs(3).unwrap().len(), 3);
        assert!(s.list_jobs(100).unwrap().len() >= 5);
    }

    fn node(name: &str, url: &str) -> Node {
        Node {
            name: name.into(),
            url: url.into(),
            api_key: "remote-key".into(),
            enabled: true,
            added_at: 1_700_000_000,
        }
    }

    /// ⭐ The remote's key stays in the store and is injected server-side by
    /// the relay: it must survive a round trip, because losing it silently
    /// turns every fleet call into a 401.
    #[test]
    fn a_node_round_trips_with_its_key() {
        let s = store();
        s.put_node(&node("heracles", "http://10.0.0.5:8199"))
            .unwrap();
        let back = s.node("heracles").unwrap().expect("the node is stored");
        assert_eq!(back.url, "http://10.0.0.5:8199");
        assert_eq!(back.api_key, "remote-key");
        assert!(back.enabled);
    }

    #[test]
    fn putting_a_node_twice_updates_it_rather_than_duplicating_it() {
        let s = store();
        s.put_node(&node("heracles", "http://10.0.0.5:8199"))
            .unwrap();
        s.put_node(&node("heracles", "http://10.0.0.9:8199"))
            .unwrap();
        let all = s.nodes().unwrap();
        assert_eq!(all.len(), 1, "one name is one node");
        assert_eq!(all[0].url, "http://10.0.0.9:8199", "the newer address wins");
    }

    #[test]
    fn deleting_a_node_reports_whether_there_was_one() {
        let s = store();
        s.put_node(&node("heracles", "http://10.0.0.5:8199"))
            .unwrap();
        assert!(s.delete_node("heracles").unwrap());
        assert!(
            !s.delete_node("heracles").unwrap(),
            "the second call has nothing to delete"
        );
        assert!(s.nodes().unwrap().is_empty());
    }

    #[test]
    fn an_unknown_node_is_none_not_an_error() {
        let s = store();
        assert!(s.node("nobody").unwrap().is_none());
    }

    /// ⭐ A waiting job is never claimed by the runner: a six-hour wait on
    /// another machine must not hold the slot every graduation queues behind.
    #[test]
    fn a_waiting_job_is_not_claimed_by_the_runner_and_survives_a_requeue() {
        let s = store();
        let id = s
            .create_waiting_job("handoff", H, r#"{"node":"n1"}"#, 10)
            .unwrap();
        assert_eq!(s.job(&id).unwrap().state, "waiting");
        assert!(s.claim_next_job().is_none(), "the runner leaves it alone");
        assert_eq!(s.requeue_running_jobs(), 0, "a restart does not requeue it");
        assert_eq!(s.job(&id).unwrap().state, "waiting");
        assert_eq!(s.waiting_jobs("handoff").len(), 1);
        assert!(
            s.job_pending_for("handoff", H),
            "a second handoff of the same hash is refused"
        );
    }

    /// ⭐ A cancel between the watcher's poll and its verdict wins: the
    /// watcher only drops the local copy after settling a row still waiting.
    #[test]
    fn a_cancelled_waiting_job_cannot_be_settled_afterwards() {
        let s = store();
        let id = s.create_waiting_job("handoff", H, "{}", 0).unwrap();
        assert_eq!(s.cancel_job(&id).as_deref(), Some("waiting"));
        assert_eq!(s.job(&id).unwrap().state, "cancelled");
        assert!(
            !s.settle_waiting_job(&id, "done", ""),
            "too late: it was cancelled"
        );
        assert_eq!(s.job(&id).unwrap().state, "cancelled");

        let id2 = s
            .create_waiting_job("handoff", &format!("b{}", &H[1..]), "{}", 0)
            .unwrap();
        assert!(s.settle_waiting_job(&id2, "done", ""));
        assert!(!s.settle_waiting_job(&id2, "failed", "x"), "settled once");
        assert_eq!(s.job(&id2).unwrap().state, "done");
    }

    /// A rename carries the waiting handoffs with it, and never merges two
    /// nodes.
    #[test]
    fn renaming_a_node_moves_its_waiting_handoffs_and_refuses_a_taken_name() {
        let s = store();
        s.put_node(&node("alpha", "http://10.0.0.5:8199")).unwrap();
        s.put_node(&node("beta", "http://10.0.0.6:8199")).unwrap();
        let id = s
            .create_waiting_job("handoff", H, r#"{"node":"alpha","engine":"vpn1"}"#, 0)
            .unwrap();

        assert!(!s.rename_node("alpha", "beta").unwrap(), "beta is taken");
        assert!(!s.rename_node("nobody", "gamma").unwrap(), "no such node");
        assert!(s.rename_node("alpha", "gamma").unwrap());
        assert!(s.node("alpha").unwrap().is_none());
        assert_eq!(s.node("gamma").unwrap().unwrap().api_key, "remote-key");
        let p: serde_json::Value = serde_json::from_str(&s.job(&id).unwrap().params).unwrap();
        assert_eq!(p["node"], "gamma");
        assert_eq!(p["engine"], "vpn1", "the rest of the params is untouched");
    }

    /// ⭐ An enrolment token is ONE-TIME. A token that could be spent twice
    /// would let a second machine register under the operator's single
    /// intention.
    #[test]
    fn an_enrolment_token_is_spent_exactly_once() {
        let s = store();
        let (token, _expiry) = s.create_enrol_token(3600).unwrap();
        assert!(
            s.consume_enrol_token(&token).unwrap(),
            "the first use works"
        );
        assert!(
            !s.consume_enrol_token(&token).unwrap(),
            "the second use does not"
        );
    }

    #[test]
    fn a_token_that_was_never_minted_cannot_be_spent() {
        let s = store();
        assert!(!s.consume_enrol_token("never-minted").unwrap());
    }

    /// ⭐ An expired token is refused. A ttl that is not enforced is not a ttl.
    #[test]
    fn an_expired_token_is_refused() {
        let s = store();
        let (token, _) = s.create_enrol_token(-1).unwrap();
        assert!(
            !s.consume_enrol_token(&token).unwrap(),
            "already past its expiry"
        );
    }

    #[test]
    fn a_drain_pass_is_recorded_and_comes_back_newest_first() {
        let s = store();
        s.record_drain(100, "/mnt/race", 95.0, 80.0, 3, 1, 0, 1 << 30)
            .unwrap();
        s.record_drain(200, "/mnt/race", 92.0, 79.0, 2, 0, 1, 1 << 29)
            .unwrap();
        let hist = s.drain_history(10).unwrap();
        assert_eq!(hist.len(), 2);
        // The SQL column is `at`; the key served to the UI is `timestamp`.
        assert_eq!(hist[0]["timestamp"].as_i64(), Some(200), "newest first");
        assert_eq!(hist[0]["volume"], serde_json::json!("/mnt/race"));
    }

    /// ⭐ `removed_count` is deleted PLUS graduated: a graduated torrent left
    /// the volume just as surely as a deleted one, and counting only the
    /// deletions understates what the pass actually freed.
    #[test]
    fn a_drain_pass_counts_graduations_as_removals_too() {
        let s = store();
        s.record_drain(100, "/mnt/race", 95.0, 80.0, 3, 2, 1, 1 << 30)
            .unwrap();
        let hist = s.drain_history(1).unwrap();
        assert_eq!(hist[0]["deleted"].as_i64(), Some(3));
        assert_eq!(hist[0]["graduated"].as_i64(), Some(2));
        assert_eq!(
            hist[0]["removed_count"].as_i64(),
            Some(5),
            "3 deleted + 2 graduated"
        );
        assert_eq!(hist[0]["stuck"].as_i64(), Some(1));
    }

    #[test]
    fn the_drain_history_is_bounded_by_its_limit() {
        let s = store();
        for at in 0..5 {
            s.record_drain(at, "/mnt/race", 90.0, 80.0, 1, 0, 0, 1)
                .unwrap();
        }
        assert_eq!(s.drain_history(2).unwrap().len(), 2);
    }

    #[test]
    fn an_empty_drain_history_is_a_list_not_a_null() {
        let s = store();
        assert!(s.drain_history(10).unwrap().is_empty());
    }

    /// Tags are registered so the UI can offer them before any torrent wears
    /// one; unregistering removes them again.
    #[test]
    fn tags_can_be_registered_and_unregistered() {
        let s = store();
        s.register_tags(&["fr".into(), "anime".into()]).unwrap();
        let mut got = s.registered_tags().unwrap();
        got.sort();
        assert_eq!(got, vec!["anime".to_string(), "fr".to_string()]);

        s.unregister_tags(&["fr".into()]).unwrap();
        assert_eq!(s.registered_tags().unwrap(), vec!["anime".to_string()]);
    }

    #[test]
    fn registering_the_same_tag_twice_does_not_duplicate_it() {
        let s = store();
        s.register_tags(&["fr".into()]).unwrap();
        s.register_tags(&["fr".into()]).unwrap();
        assert_eq!(s.registered_tags().unwrap().len(), 1);
    }

    /// Workflow activity is the audit trail: what a rule did, to what, and
    /// whether it worked.
    #[test]
    fn workflow_activity_is_recorded_and_read_back_newest_first() {
        let s = store();
        for at in [100i64, 200] {
            s.log_workflow_activity(&ActivityEntry {
                at,
                workflow_id: "wf1".into(),
                workflow_name: "ratio reached".into(),
                info_hash: H.into(),
                torrent_name: "something".into(),
                action: "pause".into(),
                outcome: "applied".into(),
                detail: String::new(),
            })
            .unwrap();
        }
        let rows = s.workflow_activity(10).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].at, 200, "newest first");
        assert_eq!(rows[0].outcome, "applied");
    }

    /// The trail is pruned by age, and pruning must not take the recent
    /// entries with it.
    #[test]
    fn pruning_the_trail_keeps_what_is_newer_than_the_cutoff() {
        let s = store();
        for at in [100i64, 500] {
            s.log_workflow_activity(&ActivityEntry {
                at,
                workflow_id: "wf1".into(),
                workflow_name: "w".into(),
                info_hash: H.into(),
                torrent_name: "t".into(),
                action: "pause".into(),
                outcome: "applied".into(),
                detail: String::new(),
            })
            .unwrap();
        }
        assert_eq!(
            s.prune_workflow_activity(300).unwrap(),
            1,
            "only the old one goes"
        );
        let rows = s.workflow_activity(10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].at, 500);
    }

    /// A workflow round-trips, and deleting it reports whether there was one.
    #[test]
    fn a_workflow_round_trips_and_can_be_deleted() {
        let s = store();
        let w = StoredWorkflow {
            id: "wf1".into(),
            name: "ratio reached".into(),
            body: r#"{"when":null,"then":[]}"#.into(),
            enabled: true,
            position: 0,
            interval_secs: 900,
            last_run: 0,
        };
        s.put_workflow(&w).unwrap();
        let back = s.workflow("wf1").unwrap().expect("stored");
        assert_eq!(back.name, "ratio reached");
        assert_eq!(s.workflows().unwrap().len(), 1);

        s.mark_workflow_run("wf1", 12345).unwrap();
        assert_eq!(s.workflow("wf1").unwrap().unwrap().last_run, 12345);

        assert!(s.delete_workflow("wf1").unwrap());
        assert!(!s.delete_workflow("wf1").unwrap());
    }

    /// Seeding times are written in bulk, and the count says how many rows the
    /// write actually touched -- a silent zero is how a sync looks when it is
    /// addressing rows that are not there.
    #[test]
    fn a_bulk_seeding_time_write_reports_what_it_touched() {
        let s = store();
        s.insert_torrent(
            H,
            "race",
            b"d4:infod4:name1:aee",
            "/data",
            "",
            1.0,
            false,
            "",
        )
        .unwrap();
        let touched = s.update_seeding_times(&[(H.to_string(), 3600)]).unwrap();
        assert_eq!(touched, 1);

        let none = s.update_seeding_times(&[(H2.to_string(), 100)]).unwrap();
        assert_eq!(none, 0, "a hash the store does not hold touches nothing");
    }

    #[test]
    fn a_meta_document_round_trips_and_is_absent_until_written() {
        let s = store();
        assert!(s.meta_doc("nothing-here").is_none());
        s.put_meta("k", "{\"a\":1}").unwrap();
        assert_eq!(s.meta_doc("k").as_deref(), Some("{\"a\":1}"));
    }
}

#[cfg(test)]
mod slim_current_tests {
    use super::*;

    /// A SlimFacts by what it says, not by how it numbered its names: ids are
    /// assigned in the order names were first seen, which differs between a
    /// full read and one kept up to date row by row.
    fn canon(f: &SlimFacts) -> std::collections::BTreeMap<[u8; 20], (String, Vec<String>, bool)> {
        f.by_hash
            .iter()
            .map(|(k, v)| {
                let tags = (0..64)
                    .filter(|i| v.tag_bits & (1u64 << i) != 0)
                    .map(|i| f.tags[i].clone())
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect();
                (
                    *k,
                    (f.category(v.category_id).to_string(), tags, v.user_paused),
                )
            })
            .collect()
    }

    fn hash(n: u8) -> String {
        format!("{:02x}", n).repeat(20)
    }

    fn add(s: &Store, n: u8, session: &str, category: &str, tags: &str) {
        s.conn
            .execute(
                "INSERT INTO torrents (info_hash, session, torrent, category, tags) VALUES (?1, ?2, x'00', ?3, ?4)",
                rusqlite::params![hash(n), session, category, tags],
            )
            .unwrap();
    }

    fn agrees(s: &mut Store, session: &str) {
        let kept = s.slim_facts_current(session).unwrap();
        let read = s.slim_facts(session).unwrap();
        assert_eq!(
            canon(&kept),
            canon(&read),
            "the kept copy says what a full read says"
        );
    }

    /// ⭐ Every kind of write the store makes reaches the kept copy: an add, a
    /// category, tags, a pause, a delete, a move between engines -- and a
    /// write to a column the list does not read changes nothing.
    #[test]
    fn the_kept_copy_follows_every_write() {
        let mut s = Store::open_in_memory().unwrap();
        for n in 1..=20 {
            add(
                &s,
                n,
                "hoard",
                if n % 2 == 0 { "Books" } else { "" },
                if n % 3 == 0 { "a,b" } else { "" },
            );
        }
        add(&s, 21, "race", "Race", "");
        agrees(&mut s, "hoard");
        agrees(&mut s, "race");

        add(&s, 30, "hoard", "New", "fresh");
        agrees(&mut s, "hoard");
        s.set_category_everywhere(&hash(2), "Films").unwrap();
        agrees(&mut s, "hoard");
        s.set_tags(&hash(3), &["z".to_string()]).unwrap();
        agrees(&mut s, "hoard");
        s.set_paused(&hash(4), "hoard", true).unwrap();
        agrees(&mut s, "hoard");
        s.delete_torrent(&hash(5)).unwrap();
        agrees(&mut s, "hoard");
        s.conn
            .execute(
                "UPDATE torrents SET session = 'race' WHERE info_hash = ?1",
                [hash(6)],
            )
            .unwrap();
        agrees(&mut s, "hoard");
        agrees(&mut s, "race");
        s.conn
            .execute(
                "UPDATE torrents SET total_uploaded = 99 WHERE info_hash = ?1",
                [hash(7)],
            )
            .unwrap();
        let before = s
            .conn
            .query_row("SELECT count(*) FROM temp.slim_dirty", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap();
        assert_eq!(before, 0, "a column the list does not read is not a change");
        agrees(&mut s, "hoard");
    }

    /// The copy is served, not re-read: with no write in between, a second
    /// call hands back the same allocation.
    #[test]
    fn with_no_write_the_copy_is_served_as_is() {
        let mut s = Store::open_in_memory().unwrap();
        add(&s, 1, "hoard", "Books", "");
        let a = s.slim_facts_current("hoard").unwrap();
        let b = s.slim_facts_current("hoard").unwrap();
        assert!(std::sync::Arc::ptr_eq(&a, &b));
    }

    /// A request still holding the previous copy keeps what it read; the
    /// update goes to a new one.
    #[test]
    fn a_copy_in_use_is_not_changed_under_its_reader() {
        let mut s = Store::open_in_memory().unwrap();
        add(&s, 1, "hoard", "Books", "");
        let held = s.slim_facts_current("hoard").unwrap();
        s.set_category_everywhere(&hash(1), "Films").unwrap();
        let now = s.slim_facts_current("hoard").unwrap();
        assert_eq!(canon(&held).values().next().unwrap().0, "Books");
        assert_eq!(canon(&now).values().next().unwrap().0, "Films");
    }

    /// ⭐ A write made through ANOTHER connection fires no trigger here. The
    /// file's data_version sees it, and the copy is read again in full.
    #[test]
    fn a_write_from_another_connection_is_not_missed() {
        let dir = std::env::temp_dir().join(format!("slimcur-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("hydra.db");
        let _ = std::fs::remove_file(&path);
        let mut s = Store::open(&path, false).unwrap();
        add(&s, 1, "hoard", "Books", "");
        agrees(&mut s, "hoard");
        let other = Connection::open(&path).unwrap();
        other
            .execute(
                "UPDATE torrents SET category = 'Films' WHERE info_hash = ?1",
                [hash(1)],
            )
            .unwrap();
        let kept = s.slim_facts_current("hoard").unwrap();
        assert_eq!(canon(&kept).values().next().unwrap().0, "Films");
        drop(other);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A store opened read-only cannot set up its triggers and reads in full
    /// every time -- slower, never wrong.
    #[test]
    fn a_read_only_store_reads_in_full() {
        let dir = std::env::temp_dir().join(format!("slimro-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("hydra.db");
        let _ = std::fs::remove_file(&path);
        {
            let s = Store::open(&path, false).unwrap();
            add(&s, 1, "hoard", "Books", "");
        }
        let mut ro = Store::open(&path, true).unwrap();
        assert!(!ro.tracks_changes);
        let f = ro.slim_facts_current("hoard").unwrap();
        assert_eq!(f.by_hash.len(), 1);
        drop(ro);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod wal_tests {
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("walt-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("hydra.db")
    }

    /// A store on a local disk opens in WAL, with synchronous=NORMAL.
    #[test]
    fn a_local_store_opens_in_wal() {
        let path = tmp("mode");
        let s = Store::open(&path, false).unwrap();
        assert_eq!(s.journal_mode(), "wal");
        let sync: i64 = s
            .conn
            .query_row("PRAGMA synchronous", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sync, 1, "NORMAL");
        drop(s);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// ⭐ The read connection sees every committed write, and a long read on
    /// it does not hold the writer: the writer's lock is free while it runs.
    #[test]
    fn the_read_connection_sees_writes_and_holds_nothing() {
        let path = tmp("reader");
        let writer = Store::open(&path, false).unwrap();
        let reader = Store::open(&path, true).unwrap();
        let lock = StoreLock::with_reader(writer, Some(reader));
        lock.lock()
            .unwrap()
            .conn
            .execute(
                "INSERT INTO torrents (info_hash, session, torrent, tags) VALUES (?1, 'hoard', x'00', 'a')",
                ["aa".repeat(20)],
            )
            .unwrap();
        let held = lock.read().unwrap();
        assert_eq!(
            held.tags_of_session("hoard").unwrap(),
            vec!["a".to_string()]
        );
        // The shared connection is not the one being read on.
        assert!(
            lock.inner.try_lock().is_ok(),
            "a read must not take the writer's lock"
        );
        drop(held);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// The watchdog reports a hold while it lasts, once, and not a short one.
    #[test]
    fn the_watchdog_reports_a_hold_while_it_lasts_once() {
        let lock = StoreLock::new(Store::open_in_memory().unwrap());
        let after = std::time::Duration::from_millis(300);
        assert_eq!(lock.watch_holds(after), 0, "nothing held");
        let held = lock.lock().unwrap();
        assert_eq!(lock.watch_holds(after), 0, "held for an instant");
        std::thread::sleep(std::time::Duration::from_millis(400));
        assert_eq!(lock.watch_holds(after), 1, "reported while still held");
        assert_eq!(lock.watch_holds(after), 0, "once per hold");
        drop(held);
        assert_eq!(lock.watch_holds(after), 0, "released");
        let held = lock.lock().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(400));
        assert_eq!(
            lock.watch_holds(after),
            1,
            "the next hold is reported again"
        );
        drop(held);
    }

    /// Without a read connection, read() is the shared lock -- never a panic,
    /// never a second view.
    #[test]
    fn without_a_reader_read_is_the_shared_lock() {
        let lock = StoreLock::new(Store::open_in_memory().unwrap());
        let held = lock.read().unwrap();
        assert!(lock.inner.try_lock().is_err());
        drop(held);
    }

    /// The checkpoint copies the WAL back and empties it for the next writer.
    #[test]
    fn a_checkpoint_empties_the_wal() {
        let path = tmp("ckpt");
        let s = Store::open(&path, false).unwrap();
        s.conn
            .execute_batch("PRAGMA wal_autocheckpoint=0;")
            .unwrap();
        for i in 0..200 {
            s.conn
                .execute(
                    "INSERT INTO torrents (info_hash, session, torrent) VALUES (?1, 'hoard', x'00')",
                    [format!("{:040x}", i)],
                )
                .unwrap();
        }
        let other = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
        checkpoint(&other).unwrap();
        let (_, log, done): (i64, i64, i64) = other
            .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!(log, done, "everything in the WAL is back in the database");
        drop((s, other));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}

#[cfg(test)]
mod resolve_tests {
    use super::*;

    fn add(s: &Store, hash: &str, session: &str) {
        s.conn
            .execute(
                "INSERT INTO torrents (info_hash, session, torrent) VALUES (?1, ?2, x'00')",
                [hash, session],
            )
            .unwrap();
    }

    /// A full hash, a prefix, upper case, the other session, nothing: the
    /// same answers the LIKE gave.
    #[test]
    fn a_hash_or_a_prefix_resolves_as_before() {
        let s = Store::open_in_memory().unwrap();
        let a = format!("ab{}", "0".repeat(38));
        let b = format!("abc{}", "1".repeat(37));
        let f = format!("f{}", "e".repeat(39));
        add(&s, &a, "hoard");
        add(&s, &b, "hoard");
        add(&s, &f, "race");
        assert_eq!(s.resolve_hash_in("hoard", &a).as_deref(), Some(a.as_str()));
        assert_eq!(
            s.resolve_hash_in("hoard", &a.to_uppercase()).as_deref(),
            Some(a.as_str())
        );
        assert_eq!(
            s.resolve_hash_in("hoard", "abc").as_deref(),
            Some(b.as_str())
        );
        assert!(s.resolve_hash_in("hoard", "ab").is_some());
        assert_eq!(
            s.resolve_hash_in("hoard", "ff"),
            None,
            "the race copy is not the hoard's"
        );
        assert_eq!(s.resolve_hash_in("race", "fe").as_deref(), Some(f.as_str()));
        assert_eq!(s.resolve_hash_in("hoard", "ac"), None);
        assert_eq!(
            s.resolve_hash(&f.to_uppercase()).as_deref(),
            Some(f.as_str())
        );
        assert_eq!(s.resolve_hash("0"), None);
    }

    /// ⭐ The lookup is an index search, not a scan: the query plan says so.
    #[test]
    fn resolving_a_hash_uses_the_index() {
        let s = Store::open_in_memory().unwrap();
        s.ensure_schema().unwrap();
        let plan: Vec<String> = s
            .conn
            .prepare("EXPLAIN QUERY PLAN SELECT info_hash FROM torrents WHERE session = ?1 AND info_hash >= ?2 AND info_hash < ?2 || 'g' LIMIT 1")
            .unwrap()
            .query_map(["hoard", "ab"], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let plan = plan.join(" | ");
        assert!(
            plan.contains("SEARCH") && plan.contains("info_hash>?"),
            "{plan}"
        );
    }
}

#[cfg(test)]
mod link_index_tests {
    use super::*;

    fn store() -> Store {
        let s = Store::open_in_memory().unwrap();
        s.ensure_link_index().unwrap();
        s
    }

    fn row(hash: &str, session: &str, measured_at: i64, files: i64, missing: i64) -> LinkRow {
        LinkRow {
            info_hash: hash.into(),
            session: session.into(),
            save_path: "/data/x".into(),
            measured_at,
            files,
            missing,
            stats: vec![7; files as usize],
        }
    }

    #[test]
    fn a_measurement_is_stored_per_copy_and_replaced_not_duplicated() {
        let s = store();
        s.put_link_rows(&[row("a", "hoard", 10, 2, 0), row("a", "race", 11, 2, 0)])
            .unwrap();
        s.put_link_rows(&[row("a", "hoard", 20, 3, 1)]).unwrap();
        let meta = s.link_index_meta().unwrap();
        assert_eq!(meta.len(), 2, "one row per (hash, session)");
        assert_eq!(meta[&("a".into(), "hoard".into())].measured_at, 20);
        assert_eq!(meta[&("a".into(), "hoard".into())].files, 3);
        let mut stats = std::collections::HashMap::new();
        s.for_each_link_row(|h, sess, sp, at, blob| {
            stats.insert(
                (h.to_string(), sess.to_string()),
                (sp.to_string(), at, blob.to_vec()),
            );
        })
        .unwrap();
        assert_eq!(stats.len(), 2);
        assert_eq!(
            stats[&("a".into(), "race".into())],
            ("/data/x".to_string(), 11, vec![7, 7])
        );
    }

    #[test]
    fn the_counts_tell_missing_from_partly_missing() {
        let s = store();
        s.put_link_rows(&[
            row("whole", "hoard", 30, 2, 0),
            row("gone", "hoard", 10, 2, 2),
            row("holes", "hoard", 20, 3, 1),
            row("empty", "hoard", 40, 0, 0),
        ])
        .unwrap();
        let c = s.link_index_counts().unwrap();
        assert_eq!(c.measured, 4);
        assert_eq!(c.files, 7);
        assert_eq!(c.data_missing, 1, "only the torrent with nothing readable");
        assert_eq!(c.partly_missing, 1);
        assert_eq!(c.oldest, 10);
    }

    #[test]
    fn dropped_rows_are_gone_and_only_those() {
        let s = store();
        s.put_link_rows(&[row("a", "hoard", 1, 1, 0), row("b", "hoard", 1, 1, 0)])
            .unwrap();
        assert_eq!(
            s.drop_link_rows(&[("a".into(), "hoard".into()), ("zz".into(), "hoard".into())])
                .unwrap(),
            1
        );
        let meta = s.link_index_meta().unwrap();
        assert!(meta.contains_key(&("b".into(), "hoard".into())));
        assert_eq!(meta.len(), 1);
    }
}

#[cfg(test)]
mod tag_since_tests {
    use super::*;

    /// The frozen schema as a store from before `tag_since` had it.
    fn old_schema() -> String {
        let table = "CREATE TABLE IF NOT EXISTS tag_since (
    info_hash TEXT NOT NULL, tag TEXT NOT NULL, since INTEGER NOT NULL,
    PRIMARY KEY (info_hash, tag));";
        assert!(
            SCHEMA.contains(table),
            "the fixture must really drop the table"
        );
        SCHEMA.replace(table, "")
    }

    fn hash(n: u8) -> String {
        format!("{:02x}", n).repeat(20)
    }

    fn tags(l: &[&str]) -> Vec<String> {
        l.iter().map(|t| t.to_string()).collect()
    }

    fn add(s: &Store, n: u8, session: &str, tags: &str) {
        assert!(s
            .insert_torrent(&hash(n), session, b"x", "", "", 0.0, false, tags)
            .unwrap());
    }

    /// Dates are only readable apart from the migration instant if that
    /// instant is far from "now".
    fn migrated_long_ago(s: &Store) {
        s.put_meta(TAG_SINCE_MIGRATED_KEY, "1000").unwrap();
    }

    fn near_now(at: i64) -> bool {
        (now_secs() - at).abs() < 30
    }

    fn names(rows: &[(String, i64)]) -> Vec<&str> {
        rows.iter().map(|r| r.0.as_str()).collect()
    }

    #[test]
    fn a_new_tag_is_dated_now() {
        let s = Store::open_in_memory().unwrap();
        migrated_long_ago(&s);
        add(&s, 1, "hoard", "");
        s.set_tags(&hash(1), &tags(&["a"])).unwrap();
        let rows = s.tag_since_rows(&hash(1));
        assert_eq!(names(&rows), ["a"]);
        assert!(near_now(rows[0].1), "{rows:?}");
        assert_eq!(s.tag_since(&hash(1)).unwrap()["a"], rows[0].1);
    }

    #[test]
    fn a_removed_tag_loses_its_row() {
        let s = Store::open_in_memory().unwrap();
        add(&s, 1, "hoard", "");
        s.set_tags(&hash(1), &tags(&["a", "b"])).unwrap();
        s.set_tags(&hash(1), &tags(&["b"])).unwrap();
        assert_eq!(names(&s.tag_since_rows(&hash(1))), ["b"]);
        s.set_tags(&hash(1), &[]).unwrap();
        assert!(s.tag_since_rows(&hash(1)).is_empty());
        assert!(s.tag_since(&hash(1)).unwrap().is_empty());
    }

    #[test]
    fn a_kept_tag_keeps_its_date() {
        let s = Store::open_in_memory().unwrap();
        add(&s, 1, "hoard", "");
        s.set_tags(&hash(1), &tags(&["a"])).unwrap();
        s.conn
            .execute("UPDATE tag_since SET since = 5000 WHERE tag = 'a'", [])
            .unwrap();
        s.set_tags(&hash(1), &tags(&["a", "b"])).unwrap();
        let got = s.tag_since(&hash(1)).unwrap();
        assert_eq!(got["a"], 5000, "a tag that stays is not re-dated");
        assert!(near_now(got["b"]));
    }

    /// A tag worn before the dates were kept has no row, and a set_tags that
    /// keeps it must not give it one: that would turn "at least since the
    /// migration" into "since now", and an age rule would never fire.
    #[test]
    fn a_kept_old_tag_stays_without_a_row_and_reads_as_the_migration() {
        let s = Store::open_in_memory().unwrap();
        migrated_long_ago(&s);
        add(&s, 1, "hoard", "");
        s.conn
            .execute("UPDATE torrents SET tags = 'old'", [])
            .unwrap();
        s.set_tags(&hash(1), &tags(&["old", "new"])).unwrap();
        assert_eq!(names(&s.tag_since_rows(&hash(1))), ["new"]);
        let got = s.tag_since(&hash(1)).unwrap();
        assert_eq!(got["old"], 1000);
        assert!(near_now(got["new"]));
    }

    #[test]
    fn tags_set_on_a_torrent_the_store_does_not_hold_leave_no_row() {
        let s = Store::open_in_memory().unwrap();
        s.set_tags(&hash(9), &tags(&["ghost"])).unwrap();
        assert!(s.tag_since_rows(&hash(9)).is_empty());
    }

    #[test]
    fn a_torrent_added_with_tags_dates_them_now() {
        let s = Store::open_in_memory().unwrap();
        migrated_long_ago(&s);
        add(&s, 1, "hoard", "x,y");
        let got = s.tag_since(&hash(1)).unwrap();
        assert!(near_now(got["x"]) && near_now(got["y"]), "{got:?}");
        // A second copy arriving with the same tag does not re-date it.
        s.conn
            .execute("UPDATE tag_since SET since = 7000", [])
            .unwrap();
        add(&s, 1, "race", "x");
        assert_eq!(s.tag_since(&hash(1)).unwrap()["x"], 7000);
    }

    /// Opening a store that has tags and no table writes no row for them:
    /// they read as the instant of the migration.
    #[test]
    fn opening_an_existing_store_dates_its_tags_at_the_migration_without_rows() {
        let dir = std::env::temp_dir().join(format!("tagsince-mig-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("hydra.db");
        let _ = std::fs::remove_file(&path);
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch(&old_schema()).unwrap();
            assert!(
                c.prepare("SELECT 1 FROM tag_since").is_err(),
                "the fixture has no tag_since"
            );
            for n in 1..=3u8 {
                c.execute(
                    "INSERT INTO torrents (info_hash, session, torrent, tags) VALUES (?1, 'hoard', x'00', 'a,b')",
                    [hash(n)],
                )
                .unwrap();
            }
        }
        let before = now_secs();
        let s = Store::open(&path, false).unwrap();
        let after = now_secs();
        let rows: i64 = s
            .conn
            .query_row("SELECT count(*) FROM tag_since", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0, "no row is written for the tags already worn");
        let instant: i64 = s
            .meta_doc(TAG_SINCE_MIGRATED_KEY)
            .expect("the instant is recorded")
            .parse()
            .unwrap();
        assert!((before..=after).contains(&instant));
        for n in 1..=3u8 {
            let got = s.tag_since(&hash(n)).unwrap();
            assert_eq!(got.len(), 2);
            assert!(got.values().all(|&v| v == instant), "{got:?}");
        }
        // A second open does not move the instant.
        drop(s);
        let s = Store::open(&path, false).unwrap();
        assert_eq!(
            s.meta_doc(TAG_SINCE_MIGRATED_KEY).unwrap(),
            instant.to_string()
        );
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_tag_added_after_the_migration_has_its_own_date() {
        let s = Store::open_in_memory().unwrap();
        migrated_long_ago(&s);
        add(&s, 1, "hoard", "");
        s.conn
            .execute("UPDATE torrents SET tags = 'old'", [])
            .unwrap();
        s.set_tags(&hash(1), &tags(&["old", "fresh"])).unwrap();
        let got = s.tag_since(&hash(1)).unwrap();
        assert_eq!(got["old"], 1000);
        assert!(got["fresh"] > 1000);
    }

    #[test]
    fn removing_a_torrent_leaves_no_row_whichever_path_removes_it() {
        let s = Store::open_in_memory().unwrap();
        let rows = |n: u8| s.tag_since_rows(&hash(n)).len();
        for n in 1..=3u8 {
            add(&s, n, "hoard", "");
            s.set_tags(&hash(n), &tags(&["a", "b"])).unwrap();
        }
        assert_eq!(rows(1), 2);
        assert!(s.delete_torrent(&hash(1)).unwrap());
        assert_eq!(rows(1), 0, "delete_torrent");
        assert!(s.delete_copy(&hash(2), "hoard").unwrap());
        assert_eq!(rows(2), 0, "delete_copy of the last copy");
        assert!(s.delete_absorb(&hash(3), None, &[], 0, 0).unwrap());
        assert_eq!(rows(3), 0, "delete_absorb");
        let orphans: i64 = s
            .conn
            .query_row(
                "SELECT count(*) FROM tag_since
                 WHERE info_hash NOT IN (SELECT info_hash FROM torrents)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(orphans, 0);
    }

    #[test]
    fn the_dates_outlive_all_but_the_last_copy() {
        let s = Store::open_in_memory().unwrap();
        add(&s, 1, "hoard", "");
        add(&s, 1, "race", "");
        s.set_tags(&hash(1), &tags(&["a"])).unwrap();
        assert!(s.delete_copy(&hash(1), "race").unwrap());
        assert_eq!(s.tag_since_rows(&hash(1)).len(), 1, "a copy is left");
        assert!(s.delete_absorb(&hash(1), Some("hoard"), &[], 0, 0).unwrap());
        assert!(s.tag_since_rows(&hash(1)).is_empty());
    }

    #[test]
    fn a_bulk_edit_dates_what_it_adds_and_drops_what_it_removes() {
        let mut s = Store::open_in_memory().unwrap();
        migrated_long_ago(&s);
        add(&s, 1, "hoard", "");
        s.conn
            .execute("UPDATE torrents SET tags = 'old'", [])
            .unwrap();
        add(&s, 2, "hoard", "");
        let targets = vec![
            (hash(1), "hoard".to_string()),
            (hash(2), "hoard".to_string()),
        ];
        let n = tags(&["new"]);
        let edit = |add| BulkEdit::Tags { tags: &n, add };
        s.bulk_edit(&targets, edit(true)).unwrap();
        let one = s.tag_since(&hash(1)).unwrap();
        assert_eq!(one["old"], 1000, "the old tag is not touched");
        assert!(near_now(one["new"]));
        assert!(near_now(s.tag_since(&hash(2)).unwrap()["new"]));
        s.bulk_edit(&targets, edit(false)).unwrap();
        assert!(s.tag_since_rows(&hash(1)).is_empty());
        assert!(s.tag_since_rows(&hash(2)).is_empty());
        assert_eq!(s.tag_since(&hash(1)).unwrap().len(), 1, "`old` still worn");
    }

    #[test]
    fn a_bulk_removal_on_one_copy_keeps_the_date_while_another_copy_wears_the_tag() {
        let mut s = Store::open_in_memory().unwrap();
        add(&s, 1, "hoard", "a");
        add(&s, 1, "race", "a");
        s.conn
            .execute(
                "INSERT OR REPLACE INTO tag_since VALUES (?1, 'a', 4000)",
                [hash(1)],
            )
            .unwrap();
        let a = tags(&["a"]);
        s.bulk_edit(
            &[(hash(1), "hoard".to_string())],
            BulkEdit::Tags {
                tags: &a,
                add: false,
            },
        )
        .unwrap();
        assert_eq!(s.tag_since(&hash(1)).unwrap()["a"], 4000);
    }

    #[test]
    fn deleting_a_tag_everywhere_drops_its_dates() {
        let s = Store::open_in_memory().unwrap();
        add(&s, 1, "hoard", "");
        add(&s, 2, "hoard", "");
        s.set_tags(&hash(1), &tags(&["a", "b"])).unwrap();
        s.set_tags(&hash(2), &tags(&["a"])).unwrap();
        s.strip_tags(&tags(&["a"])).unwrap();
        assert_eq!(s.tag_since_rows(&hash(1)).len(), 1);
        assert!(s.tag_since_rows(&hash(2)).is_empty());
    }

    #[test]
    fn the_bulk_read_answers_every_torrent_and_only_tagged_ones() {
        let s = Store::open_in_memory().unwrap();
        migrated_long_ago(&s);
        add(&s, 1, "hoard", "");
        s.conn
            .execute(
                "UPDATE torrents SET tags = 'old' WHERE info_hash = ?1",
                [hash(1)],
            )
            .unwrap();
        add(&s, 2, "hoard", "");
        add(&s, 3, "hoard", "");
        s.set_tags(&hash(3), &tags(&["x"])).unwrap();
        let (h1, h2, h3) = (hash(1), hash(2), hash(3));
        let got = s
            .tag_since_bulk(&[h1.as_str(), h2.as_str(), h3.as_str(), "nope"])
            .unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[&h1]["old"], 1000);
        assert!(near_now(got[&h3]["x"]));
        assert!(s.tag_since_bulk(&[]).unwrap().is_empty());
    }

    /// Measures, asserts no bound:
    /// `cargo test --lib measure_migration -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn measure_migration_and_bulk_read_at_100k() {
        let dir = std::env::temp_dir().join(format!("tagsince-100k-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("hydra.db");
        let _ = std::fs::remove_file(&path);
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch(&old_schema()).unwrap();
            c.execute_batch("BEGIN").unwrap();
            {
                let mut st = c
                    .prepare("INSERT INTO torrents (info_hash, session, torrent, tags) VALUES (?1, 'hoard', x'00', ?2)")
                    .unwrap();
                for n in 0..100_000u32 {
                    st.execute(rusqlite::params![
                        format!("{n:040x}"),
                        format!("t{},u{},v{}", n % 7, n % 13, n % 101)
                    ])
                    .unwrap();
                }
            }
            c.execute_batch("COMMIT").unwrap();
        }
        let t = std::time::Instant::now();
        let mut s = Store::open(&path, false).unwrap();
        println!("open+migrate 100k torrents: {:?}", t.elapsed());
        // What the same open costs with nothing to migrate: the rest of the
        // open (here, the covering index a fixture without one builds) is not
        // the table's.
        let t = std::time::Instant::now();
        drop(Store::open(&path, false).unwrap());
        println!("second open, nothing to migrate: {:?}", t.elapsed());
        // The migration alone: the table dropped from a store that has every
        // other index, as production does.
        Connection::open(&path)
            .unwrap()
            .execute_batch(&format!(
                "DROP TABLE tag_since; DELETE FROM meta WHERE key = '{TAG_SINCE_MIGRATED_KEY}';"
            ))
            .unwrap();
        let t = std::time::Instant::now();
        drop(Store::open(&path, false).unwrap());
        println!("open that migrates tag_since only: {:?}", t.elapsed());
        let hashes: Vec<String> = (0..100_000u32).map(|n| format!("{n:040x}")).collect();
        let refs: Vec<&str> = hashes.iter().map(String::as_str).collect();
        let t = std::time::Instant::now();
        let got = s.tag_since_bulk(&refs).unwrap();
        println!(
            "tag_since_bulk 100k torrents: {:?} ({} answered)",
            t.elapsed(),
            got.len()
        );
        let targets: Vec<(String, String)> = hashes[..10_000]
            .iter()
            .map(|x| (x.clone(), "hoard".into()))
            .collect();
        let n = vec!["fresh".to_string()];
        let t = std::time::Instant::now();
        s.bulk_edit(
            &targets,
            BulkEdit::Tags {
                tags: &n,
                add: true,
            },
        )
        .unwrap();
        println!("bulk_edit add one tag on 10000 torrents: {:?}", t.elapsed());
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
