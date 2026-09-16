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

/// The store's half of a torrent's facts, for a workflow pass.
#[derive(Debug, Clone, Default)]
pub struct WorkflowFacts {
    pub category: String,
    pub save_path: String,
    pub added_time: f64,
    pub completed_time: f64,
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

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

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
        if let Ok(serde_json::Value::Array(items)) = serde_json::from_str::<serde_json::Value>(trimmed) {
            return items
                .iter()
                .filter_map(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
        }
    }
    trimmed
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
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

/// The whole session's slim facts, with the text interned once.
#[derive(Default)]
pub struct SlimFacts {
    pub by_hash: std::collections::HashMap<[u8; 20], SlimFact>,
    pub categories: Vec<String>,
    pub tags: Vec<String>,
}

impl SlimFacts {
    pub fn get(&self, hash: &[u8; 20]) -> SlimFact {
        self.by_hash.get(hash).copied().unwrap_or_default()
    }

    pub fn category(&self, id: u16) -> &str {
        self.categories.get(id as usize).map(String::as_str).unwrap_or("")
    }

    /// The id of a category by name, or None when the library has none such --
    /// which makes a filter on it match nothing, as it should.
    pub fn category_id(&self, name: &str) -> Option<u16> {
        self.categories.iter().position(|c| c == name).map(|i| i as u16)
    }

    pub fn tag_bit(&self, name: &str) -> Option<u64> {
        self.tags.iter().position(|t| t == name).map(|i| 1u64 << i)
    }
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

pub struct Store {
    conn: Connection,
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
        let conn = Connection::open_with_flags(path, flags)?;
        let store = Self { conn };
        // 3.x applies its CREATE TABLE IF NOT EXISTS on every open, so a fresh
        // install comes up with an empty but complete database rather than
        // refusing to start. Reproduced here; read-only opens skip it, since a
        // bench pointed at a copy of production must not write to it.
        if !read_only {
            store.ensure_schema()?;
        }
        Ok(store)
    }

    /// Create anything missing. Every statement is IF NOT EXISTS, so this is a
    /// no-op against a database that already has the tables -- including one
    /// written by 3.x, which is the whole point.
    pub fn ensure_schema(&self) -> anyhow::Result<()> {
        self.conn.execute_batch(SCHEMA)?;
        self.ensure_cover_index()?;
        self.ensure_nodes_table()?;
        self.ensure_enrol_table()?;
        self.ensure_workflows_table()?;
        self.ensure_content_index()?;
        // After the tables exist, and before anything reads them.
        self.migrate_composite_key()?;
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
        Ok(())
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
                 ON workflow_activity(at DESC);",
        )?;
        Ok(())
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
        let mut stmt = self.conn.prepare(
            "SELECT name, url, api_key, enabled, added_at FROM nodes ORDER BY name",
        )?;
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
        tracing::warn!(rows, seconds = started.elapsed().as_secs(), "torrents table re-keyed");
        Ok(())
    }

    /// Fail loudly if the database is not the shape this build expects.
    ///
    /// A missing column would otherwise surface as a wrong value in one field of
    /// one endpoint, which is exactly the kind of difference that survives a
    /// review and reaches production.
    /// An empty store with the frozen schema applied, for tests.
    pub fn open_in_memory() -> anyhow::Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
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
        let id = format!("job{}{}", now, &info_hash.chars().take(6).collect::<String>());
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
                 WHERE type = ?1 AND info_hash = ?2 AND state IN ('queued','running')",
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

    pub fn job_finish(&self, id: &str, error: &str) -> anyhow::Result<()> {
        let state = if error.is_empty() { "done" } else { "failed" };
        self.conn.execute(
            "UPDATE jobs SET state = ?2, error = ?3, updated_at = ?4 WHERE id = ?1",
            rusqlite::params![id, state, error, now_secs()],
        )?;
        Ok(())
    }

    /// Put jobs that were running when the process died back in the queue.
    ///
    /// A job left "running" belongs to a process that no longer exists, so it
    /// will never finish and never fail: it would sit in the tab forever. The
    /// work is re-done rather than resumed -- a half-copied file is not a
    /// state this can trust, and `copy_then_delete` only unlinks the source
    /// once the copy is complete, so the source is still there.
    pub fn requeue_running_jobs(&self) -> usize {
        self.conn
            .execute(
                "UPDATE jobs SET state = 'queued', progress_bytes = 0, updated_at = ?1
                 WHERE state = 'running'",
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
        let holes = std::iter::repeat("?").take(hashes.len()).collect::<Vec<_>>().join(",");
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
        let mut out = SlimFacts::default();
        // Index 0 is "no category" / "no tags", so the common case stores a
        // zero and never touches the intern tables.
        out.categories.push(String::new());
        let mut cat_ids: std::collections::HashMap<String, u16> = Default::default();
        let mut tag_ids: std::collections::HashMap<String, u16> = Default::default();

        let mut rows = stmt.query([session])?;
        while let Some(row) = rows.next()? {
            let hash: String = row.get(0)?;
            let Some(key) = hex20(&hash) else { continue };
            let category: String = row.get(1)?;
            let tags_raw: String = row.get(2)?;
            let paused: i64 = row.get(3)?;

            let category_id = if category.is_empty() {
                0
            } else if let Some(id) = cat_ids.get(&category) {
                *id
            } else {
                let id = out.categories.len() as u16;
                out.categories.push(category.clone());
                cat_ids.insert(category, id);
                id
            };

            let mut tag_bits: u64 = 0;
            for tag in split_tags(&tags_raw) {
                let id = if let Some(id) = tag_ids.get(&tag) {
                    *id
                } else {
                    // 64 distinct tags is the ceiling of the bitset. Beyond it
                    // the extra tags stop being counted rather than corrupting
                    // the ones already there.
                    if out.tags.len() >= 64 {
                        continue;
                    }
                    let id = out.tags.len() as u16;
                    out.tags.push(tag.clone());
                    tag_ids.insert(tag, id);
                    id
                };
                tag_bits |= 1u64 << id;
            }

            out.by_hash.insert(
                key,
                SlimFact { category_id, tag_bits, user_paused: paused != 0 },
            );
        }
        Ok(out)
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
    fn fact_row(
        row: &rusqlite::Row<'_>,
    ) -> rusqlite::Result<(String, crate::row::StoreFacts)> {
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
        let mut stmt = self.conn.prepare("SELECT name FROM tag_registry ORDER BY name")?;
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

    pub fn unregister_tags(&self, tags: &[String]) -> anyhow::Result<()> {
        for tag in tags {
            self.conn
                .execute("DELETE FROM tag_registry WHERE name = ?1", [tag])?;
        }
        Ok(())
    }

    /// Tags of one torrent, as stored: a comma-separated list.
    pub fn tags_of(&self, info_hash: &str) -> Vec<String> {
        let raw: String = self
            .conn
            .query_row("SELECT tags FROM torrents WHERE info_hash = ?1 LIMIT 1", [info_hash], |r| r.get(0))
            .unwrap_or_default();
        raw.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
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
            let mut stmt = tx.prepare_cached(
                "UPDATE torrents SET seeding_time = ?2 WHERE info_hash = ?1",
            )?;
            for (hash, secs) in rows {
                n += stmt.execute(rusqlite::params![hash, secs])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    pub fn set_tags(&self, info_hash: &str, tags: &[String]) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE torrents SET tags = ?2 WHERE info_hash = ?1",
            rusqlite::params![info_hash, tags.join(",")],
        )?;
        Ok(())
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
        let mut stmt = self.conn.prepare(
            "SELECT info_hash FROM torrents WHERE session = ?1 AND paused <> 0",
        )?;
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
    ) -> anyhow::Result<()> {
        self.conn.execute(
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
        Ok(())
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
        self.conn.execute(
            "UPDATE torrents SET session = ?3 WHERE info_hash = ?1 AND session = ?2",
            rusqlite::params![info_hash, from, to],
        )?;
        Ok(())
    }

    /// Drop ONE copy, leaving the others.
    pub fn delete_copy(&self, info_hash: &str, session: &str) -> anyhow::Result<bool> {
        let n = self.conn.execute(
            "DELETE FROM torrents WHERE info_hash = ?1 AND session = ?2",
            rusqlite::params![info_hash, session],
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
        self.conn
            .query_row(
                "SELECT info_hash FROM torrents WHERE session = ?1 AND info_hash LIKE ?2 || '%' LIMIT 1",
                rusqlite::params![session, prefix.to_lowercase()],
                |r| r.get(0),
            )
            .ok()
    }

    /// Info hashes whose value starts with the given prefix.
    ///
    /// qBittorrent clients routinely send a shortened hash, and refusing them
    /// would break the very callers the shim exists for.
    pub fn resolve_hash(&self, prefix: &str) -> Option<String> {
        self.conn
            .query_row(
                "SELECT info_hash FROM torrents WHERE info_hash LIKE ?1 || '%' LIMIT 1",
                [prefix.to_lowercase()],
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

    /// Where a torrent's data is meant to live, as the store recorded it.
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

    pub fn drop_content_key(&self, info_hash: &str) -> anyhow::Result<()> {
        self.conn
            .execute("DELETE FROM content_index WHERE info_hash = ?1", [info_hash])?;
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

    pub fn count_torrents(&self) -> anyhow::Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM torrents", [], |r| r.get(0))?)
    }

    /// Torrent counts per session, which is what the status endpoints report.
    pub fn count_by_session(&self) -> anyhow::Result<Vec<(String, i64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT session, COUNT(*) FROM torrents GROUP BY session ORDER BY session")?;
        let rows = stmt
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod composite_key_tests {
    use super::*;

    fn blob() -> Vec<u8> { b"d4:infod6:lengthi1eee".to_vec() }

    /// The migration must keep every row and change only the key.
    ///
    /// Run against a store created with the OLD schema, which is what a
    /// production database is until this build opens it.
    #[test]
    fn the_rebuild_keeps_every_row_and_re_keys_the_table() {
        let s = Store::open_in_memory().unwrap();
        s.insert_torrent("aa", "hoard", &blob(), "/data", "movies", 1.0, false, "x").unwrap();
        s.insert_torrent("bb", "race", &blob(), "/race", "", 2.0, true, "").unwrap();

        s.migrate_composite_key().unwrap();

        let sql: String = s.conn.query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='torrents'", [], |r| r.get(0),
        ).unwrap();
        assert!(sql.contains("PRIMARY KEY (info_hash, session)"), "{sql}");

        let n: i64 = s.conn.query_row("SELECT COUNT(*) FROM torrents", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 2, "a row was lost in the rebuild");
        // Columns came across, not just the keys.
        let cat: String = s.conn.query_row(
            "SELECT category FROM torrents WHERE info_hash='aa'", [], |r| r.get(0)).unwrap();
        assert_eq!(cat, "movies");
        assert_eq!(s.torrent_blob("aa").unwrap().unwrap(), blob());
    }

    /// Running it twice must be a no-op, because it runs at every boot.
    #[test]
    fn migrating_an_already_migrated_table_does_nothing() {
        let s = Store::open_in_memory().unwrap();
        s.insert_torrent("aa", "hoard", &blob(), "/data", "", 1.0, false, "").unwrap();
        s.migrate_composite_key().unwrap();
        s.migrate_composite_key().unwrap();
        let n: i64 = s.conn.query_row("SELECT COUNT(*) FROM torrents", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
    }

    /// The point of the whole change: one torrent, two engines.
    #[test]
    fn one_torrent_can_live_in_two_engines() {
        let s = Store::open_in_memory().unwrap();
        s.migrate_composite_key().unwrap();
        s.insert_torrent("aa", "hoard", &blob(), "/data", "movies", 1.0, false, "").unwrap();
        s.insert_torrent("aa", "vpn1", &blob(), "/data", "movies", 1.0, false, "").unwrap();
        assert_eq!(s.sessions_of("aa"), vec!["hoard", "vpn1"]);
    }

    /// Pause describes an execution, so it must not leak between copies: a
    /// torrent held back on one tunnel keeps seeding on the other.
    #[test]
    fn pausing_one_copy_leaves_the_other_running() {
        let s = Store::open_in_memory().unwrap();
        s.migrate_composite_key().unwrap();
        s.insert_torrent("aa", "hoard", &blob(), "/data", "", 1.0, false, "").unwrap();
        s.insert_torrent("aa", "vpn1", &blob(), "/data", "", 1.0, false, "").unwrap();

        s.set_paused("aa", "hoard", true).unwrap();
        let paused = |sess: &str| -> i64 {
            s.conn.query_row(
                "SELECT paused FROM torrents WHERE info_hash='aa' AND session=?1",
                [sess], |r| r.get(0)).unwrap()
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
        s.insert_torrent("aa", "hoard", &blob(), "/data", "", 1.0, false, "").unwrap();
        s.insert_torrent("aa", "vpn1", &blob(), "/data", "", 1.0, false, "").unwrap();
        assert!(s.delete_copy("aa", "hoard").unwrap());
        assert_eq!(s.sessions_of("aa"), vec!["vpn1"]);
        assert!(!s.delete_copy("aa", "hoard").unwrap(), "already gone");
    }

    /// A move takes the source, or three copies would collapse into one.
    #[test]
    fn moving_a_copy_moves_only_that_copy() {
        let s = Store::open_in_memory().unwrap();
        s.migrate_composite_key().unwrap();
        s.insert_torrent("aa", "hoard", &blob(), "/data", "", 1.0, false, "").unwrap();
        s.insert_torrent("aa", "vpn1", &blob(), "/data", "", 1.0, false, "").unwrap();
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
        assert!(s.consume_enrol_token(&token).unwrap(), "first use must work");
        assert!(!s.consume_enrol_token(&token).unwrap(), "second use must not");
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
        assert_eq!(a.len(), 32, "short enough to paste, long enough not to guess");
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
        store.insert_torrent(&a, "hoard", b"x", "", "", 0.0, false, "").unwrap();
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
        assert!(store.write_mark() > after_update, "a delete must move the mark");
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
        store.insert_torrent(&a, "hoard", b"x", "", "", 0.0, false, "").unwrap();
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
        store.insert_torrent(&b, "hoard", b"y", "", "", 0.0, false, "").unwrap();
        assert!(store.delete_absorb(&b, None, &keys, 5, 5).unwrap());
        for key in &keys {
            assert_eq!(store.counter(key), (1005, 105), "counter {key} after the second");
        }
    }

    /// A torrent that never moved a byte still has to lose its row.
    #[test]
    fn a_removal_with_no_bytes_still_drops_the_row() {
        let store = fresh();
        let a = "a".repeat(40);
        store.insert_torrent(&a, "hoard", b"x", "", "", 0.0, false, "").unwrap();
        assert!(store.delete_absorb(&a, None, &["global".to_string()], 0, 0).unwrap());
        assert!(!store.has_torrent_blob(&a));
        assert_eq!(store.counter("global"), (0, 0), "nothing to fold, nothing folded");
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
        store.insert_torrent(&a, "hoard", b"x", "", "", 0.0, false, "").unwrap();
        store.insert_torrent(&a, "race", b"x", "", "", 0.0, false, "").unwrap();

        assert!(store.delete_absorb(&a, Some("hoard"), &["global".to_string()], 7, 3).unwrap());
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
        store.insert_torrent(&a, "hoard", b"x", "/data/one", "movies", 11.0, false, "").unwrap();
        store.insert_torrent(&b, "hoard", b"x", "/data/two", "series", 22.0, true, "").unwrap();

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
        store.insert_torrent(&a, "hoard", b"x", "", "", 0.0, true, "").unwrap();
        store.insert_torrent(&b, "hoard", b"x", "", "", 0.0, false, "").unwrap();
        // Same decision, different engine: asking for hoard must not return it.
        store.insert_torrent(&c, "race", b"x", "", "", 0.0, true, "").unwrap();

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
        Store { conn }
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
            s.insert_torrent(hash, session, b"d4:infod4:name4:teseee", "/data", "cat", 1.0, false, "")
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
        let n = store.set_paused_batch(&[A.to_string()], "hoard", true).unwrap();
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
        assert_eq!(n, 3, "two copies of A plus B; the unknown hash matches nothing");
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
        s.insert_torrent(A, "hoard", b"d4:infod4:name4:teseee", "/data/tv", "series", 1.0, false, "")
            .expect("hoard copy");
        s.insert_torrent(A, "race", b"d4:infod4:name4:teseee", "/race/torrents", "Race", 1.0, false, "")
            .expect("race copy");

        let hashes = vec![A.to_string()];
        let hoard = s.facts_for_hashes(&hashes, "hoard").expect("hoard facts");
        let race = s.facts_for_hashes(&hashes, "race").expect("race facts");

        let h = hoard.get(A).expect("the hoard copy is found");
        let r = race.get(A).expect("the race copy is found");
        assert_eq!(h.category, "series", "the hoard row must not wear the race category");
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
        s.insert_torrent(A, "race", b"d4:infod4:name4:teseee", "/race/torrents", "Race", 1.0, false, "")
            .expect("race copy");
        let facts = s.facts_for_hashes(&[A.to_string()], "hoard").expect("facts");
        assert!(facts.is_empty(), "hoard holds no copy, so it has no facts to show");
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
        s.insert_torrent(hash, session, b"d4:infod4:name4:teseee", "/data", "cat", 1.0, false, "")
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
        assert!(s.delete_absorb(H, Some("race"), &keys, 5_000, 1_200).unwrap());

        assert_eq!(s.counter("global"), (5_000, 1_200), "the bytes outlived the torrent");
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
        assert!(s.delete_absorb(H, Some("race"), &["global".to_string()], 0, 0).unwrap());
        assert!(s.sessions_of(H).is_empty());
        assert_eq!(s.counter("global"), (0, 0), "no row was written for zero bytes");
    }

    /// Deleting a copy from ONE engine must leave the other engine's copy
    /// alone -- and must not fold the bytes twice.
    #[test]
    fn deleting_one_copy_leaves_the_other_engines_copy() {
        let s = store();
        add(&s, H, "race");
        add(&s, H, "hoard");
        assert_eq!(s.sessions_of(H), vec!["hoard".to_string(), "race".to_string()]);

        s.delete_absorb(H, Some("race"), &["global".to_string()], 7, 3).unwrap();
        assert_eq!(s.sessions_of(H), vec!["hoard".to_string()], "hoard still holds it");
        assert_eq!(s.counter("global"), (7, 3));
    }

    /// No session means every copy: the torrent is leaving the node.
    #[test]
    fn absorbing_without_a_session_removes_every_copy() {
        let s = store();
        add(&s, H, "race");
        add(&s, H, "hoard");
        assert!(s.delete_absorb(H, None, &["global".to_string()], 1, 1).unwrap());
        assert!(s.sessions_of(H).is_empty());
    }

    /// Deleting something that is not there is `false`, not an error and not a
    /// counter write: a retried delete must not double-count the bytes.
    #[test]
    fn absorbing_a_torrent_that_is_not_here_reports_false() {
        let s = store();
        assert!(!s.delete_absorb(H, Some("race"), &["global".to_string()], 9, 9).unwrap());
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
        assert_eq!(s.sessions_of(H), vec!["race".to_string()], "the race copy is untouched");
    }

    #[test]
    fn dropping_one_copy_reports_whether_there_was_one() {
        let s = store();
        add(&s, H, "race");
        assert!(s.delete_copy(H, "race").unwrap());
        assert!(!s.delete_copy(H, "race").unwrap(), "the second call has nothing to drop");
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
        assert_eq!(s.counter("k"), (3, 4), "set replaces, unlike absorb which adds");
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
        assert_eq!(s.torrent_blob(H).unwrap().as_deref(), Some(&b"d4:infod4:name4:teseee"[..]));
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
    fn a_pause_is_remembered_per_engine() {
        let s = store();
        add(&s, H, "race");
        add(&s, H, "hoard");
        s.set_paused(H, "race", true).unwrap();
        assert_eq!(s.paused_hashes("race").unwrap(), vec![H.to_string()]);
        assert!(s.paused_hashes("hoard").unwrap().is_empty(), "the hoard copy still runs");
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
        assert_eq!(s.all_hashes("race").unwrap(), vec![H.to_string()],
            "the torrent is still listed under its engine after the move");
        assert_eq!(s.category_of(H, "race").as_deref(), Some("cat"), "and keeps its category");
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
        assert_eq!(s.category_of(H, "race").as_deref(), Some("cat"), "the race copy is untouched");
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
        assert_ne!(s.write_mark(), before, "the facts cache must see this as stale");
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
        assert!(s.job_pending_for("graduate", H), "the queued job is pending");
    }

    /// The gate is per KIND and per TORRENT: a move must not be blocked by a
    /// graduation, and another torrent's job is not this torrent's.
    #[test]
    fn the_pending_gate_is_per_kind_and_per_torrent() {
        let s = store();
        s.create_job("graduate", H, "{}", 100).unwrap();
        assert!(!s.job_pending_for("move", H), "a different kind is not pending");
        assert!(!s.job_pending_for("graduate", H2), "another torrent is not this one");
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
        assert!(!s.job_pending_for("graduate", H), "a finished job is not in flight");
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
        assert!(s.claim_next_job().is_some(), "it is claimable again after the requeue");
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
            s.create_job("move", &format!("{i}{}", &H[1..]), "{}", 1).unwrap();
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
        s.put_node(&node("heracles", "http://10.0.0.5:8199")).unwrap();
        let back = s.node("heracles").unwrap().expect("the node is stored");
        assert_eq!(back.url, "http://10.0.0.5:8199");
        assert_eq!(back.api_key, "remote-key");
        assert!(back.enabled);
    }

    #[test]
    fn putting_a_node_twice_updates_it_rather_than_duplicating_it() {
        let s = store();
        s.put_node(&node("heracles", "http://10.0.0.5:8199")).unwrap();
        s.put_node(&node("heracles", "http://10.0.0.9:8199")).unwrap();
        let all = s.nodes().unwrap();
        assert_eq!(all.len(), 1, "one name is one node");
        assert_eq!(all[0].url, "http://10.0.0.9:8199", "the newer address wins");
    }

    #[test]
    fn deleting_a_node_reports_whether_there_was_one() {
        let s = store();
        s.put_node(&node("heracles", "http://10.0.0.5:8199")).unwrap();
        assert!(s.delete_node("heracles").unwrap());
        assert!(!s.delete_node("heracles").unwrap(), "the second call has nothing to delete");
        assert!(s.nodes().unwrap().is_empty());
    }

    #[test]
    fn an_unknown_node_is_none_not_an_error() {
        let s = store();
        assert!(s.node("nobody").unwrap().is_none());
    }

    /// ⭐ An enrolment token is ONE-TIME. A token that could be spent twice
    /// would let a second machine register under the operator's single
    /// intention.
    #[test]
    fn an_enrolment_token_is_spent_exactly_once() {
        let s = store();
        let (token, _expiry) = s.create_enrol_token(3600).unwrap();
        assert!(s.consume_enrol_token(&token).unwrap(), "the first use works");
        assert!(!s.consume_enrol_token(&token).unwrap(), "the second use does not");
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
        assert!(!s.consume_enrol_token(&token).unwrap(), "already past its expiry");
    }

    #[test]
    fn a_drain_pass_is_recorded_and_comes_back_newest_first() {
        let s = store();
        s.record_drain(100, "/mnt/race", 95.0, 80.0, 3, 1, 0, 1 << 30).unwrap();
        s.record_drain(200, "/mnt/race", 92.0, 79.0, 2, 0, 1, 1 << 29).unwrap();
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
        s.record_drain(100, "/mnt/race", 95.0, 80.0, 3, 2, 1, 1 << 30).unwrap();
        let hist = s.drain_history(1).unwrap();
        assert_eq!(hist[0]["deleted"].as_i64(), Some(3));
        assert_eq!(hist[0]["graduated"].as_i64(), Some(2));
        assert_eq!(hist[0]["removed_count"].as_i64(), Some(5), "3 deleted + 2 graduated");
        assert_eq!(hist[0]["stuck"].as_i64(), Some(1));
    }

    #[test]
    fn the_drain_history_is_bounded_by_its_limit() {
        let s = store();
        for at in 0..5 {
            s.record_drain(at, "/mnt/race", 90.0, 80.0, 1, 0, 0, 1).unwrap();
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
        assert_eq!(s.prune_workflow_activity(300).unwrap(), 1, "only the old one goes");
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
        s.insert_torrent(H, "race", b"d4:infod4:name1:aee", "/data", "", 1.0, false, "")
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
