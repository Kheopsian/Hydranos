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

/// The frozen schema, as the production database has it.
pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS torrents (
    info_hash TEXT PRIMARY KEY, session TEXT NOT NULL, torrent BLOB NOT NULL,
    save_path TEXT NOT NULL DEFAULT '', category TEXT NOT NULL DEFAULT '',
    added_time REAL NOT NULL DEFAULT 0, completed_time REAL NOT NULL DEFAULT 0,
    total_uploaded INTEGER NOT NULL DEFAULT 0, total_downloaded INTEGER NOT NULL DEFAULT 0,
    paused INTEGER NOT NULL DEFAULT 0, tags TEXT NOT NULL DEFAULT '',
    content_folder INTEGER NOT NULL DEFAULT -1, pinned INTEGER NOT NULL DEFAULT 0,
    seeding_time INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS counters (key TEXT PRIMARY KEY, ul INTEGER NOT NULL DEFAULT 0, dl INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS tag_registry (name TEXT PRIMARY KEY);
CREATE TABLE IF NOT EXISTS jobs (
    id TEXT PRIMARY KEY, type TEXT NOT NULL, state TEXT NOT NULL,
    info_hash TEXT NOT NULL DEFAULT '', params TEXT NOT NULL DEFAULT '',
    progress_bytes INTEGER NOT NULL DEFAULT 0, total_bytes INTEGER NOT NULL DEFAULT 0,
    error TEXT NOT NULL DEFAULT '', created_at INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL DEFAULT 0);
";

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
                    tags: tags
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect(),
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
        let rows = stmt.query_map([session], |row| {
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
                    tags: tags
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect(),
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

    /// Every distinct tag used by one session's torrents, sorted.
    pub fn tags_of_session(&self, session: &str) -> anyhow::Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT tags FROM torrents WHERE session = ?1 AND tags <> ''")?;
        let mut set = std::collections::BTreeSet::new();
        for row in stmt.query_map([session], |r| r.get::<_, String>(0))? {
            for tag in row?.split(',') {
                let tag = tag.trim();
                if !tag.is_empty() {
                    set.insert(tag.to_string());
                }
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
            .query_row("SELECT tags FROM torrents WHERE info_hash = ?1", [info_hash], |r| r.get(0))
            .unwrap_or_default();
        raw.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    }

    pub fn set_tags(&self, info_hash: &str, tags: &[String]) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE torrents SET tags = ?2 WHERE info_hash = ?1",
            rusqlite::params![info_hash, tags.join(",")],
        )?;
        Ok(())
    }

    pub fn set_paused(&self, info_hash: &str, paused: bool) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE torrents SET paused = ?2 WHERE info_hash = ?1",
            rusqlite::params![info_hash, i64::from(paused)],
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
                "SELECT length(torrent) FROM torrents WHERE info_hash = ?1",
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

    pub fn set_pinned(&self, info_hash: &str, pinned: bool) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE torrents SET pinned = ?2 WHERE info_hash = ?1",
            rusqlite::params![info_hash, i64::from(pinned)],
        )?;
        Ok(())
    }

    pub fn set_category(&self, info_hash: &str, category: &str) -> anyhow::Result<()> {
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
mod tests {
    use super::*;

    fn fresh() -> Store {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE torrents (
                info_hash TEXT PRIMARY KEY, session TEXT NOT NULL, torrent BLOB NOT NULL,
                save_path TEXT NOT NULL DEFAULT '', category TEXT NOT NULL DEFAULT '',
                added_time REAL NOT NULL DEFAULT 0, completed_time REAL NOT NULL DEFAULT 0,
                total_uploaded INTEGER NOT NULL DEFAULT 0,
                total_downloaded INTEGER NOT NULL DEFAULT 0,
                paused INTEGER NOT NULL DEFAULT 0, tags TEXT NOT NULL DEFAULT '',
                content_folder INTEGER NOT NULL DEFAULT -1, pinned INTEGER NOT NULL DEFAULT 0,
                seeding_time INTEGER NOT NULL DEFAULT 0);",
        )
        .unwrap();
        Store { conn }
    }

    #[test]
    fn the_frozen_schema_is_accepted() {
        assert!(fresh().check_schema().is_ok());
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
