//! Durable per-torrent state, in SQLite.
//!
//! This replaces the one-JSON-file-per-torrent scheme in `fastresume`. That
//! scheme did not fail because JSON is slow -- reading the records is about 5%
//! of a cold start. It failed on the write side, and on three counts:
//!
//!   1. `save_all_resume` rewrites *every* torrent every five minutes, whether
//!      or not it changed. At production scale that is ~200k file
//!      create/write/rename cycles every 300s, roughly 666 per second, forever,
//!      on a machine that is otherwise idle.
//!   2. Each record is a few hundred bytes but occupies a whole filesystem
//!      block, so a sweep dirties ~780 MiB of copy-on-write blocks that the
//!      hourly ZFS snapshots then pin. The resume directory was the dominant
//!      source of snapshot growth on the appdata pool.
//!   3. A directory of files has no transaction. Durability had to be
//!      hand-rolled with a `.tmp` + `rename(2)`, deletions could leak orphans
//!      (thousands accumulated in production), and a torrent's identity in the
//!      Go-side store could silently disagree with its progression here.
//!
//! One table in one file fixes all three: a sweep is a single transaction over
//! only the rows that actually changed, deletions cannot orphan anything, and
//! moving a torrent between engines becomes a row copy instead of a two-system
//! dance.
//!
//! The engine owns this file exclusively -- Hydra (Go) opens it read-only. That
//! is the same rule the per-agent identity store follows: the database belongs
//! to the agent, and an agent is its engine.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};
use tracing::{info, warn};

use super::fastresume::ResumeData;

/// The subset of a record that decides whether a row needs rewriting.
///
/// Comparing this instead of the whole record is what turns the periodic sweep
/// from "write 200k rows" into "write the few hundred that moved". The verified
/// piece count stands in for the bitfield: the bitfield cannot change without
/// the count changing with it, and comparing one u32 beats memcmp-ing a
/// multi-kilobyte hex string per torrent per sweep.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint {
    pub total_uploaded: u64,
    pub total_downloaded: u64,
    pub completed_time: i64,
    pub num_have: u32,
    pub paused: bool,
    pub seed_mode: bool,
}

pub struct StateDb {
    conn: Mutex<Connection>,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS torrent_state (
    info_hash        TEXT PRIMARY KEY,
    torrent_path     TEXT NOT NULL DEFAULT '',
    save_path        TEXT NOT NULL DEFAULT '',
    seed_mode        INTEGER NOT NULL DEFAULT 0,
    paused           INTEGER NOT NULL DEFAULT 0,
    total_uploaded   INTEGER NOT NULL DEFAULT 0,
    total_downloaded INTEGER NOT NULL DEFAULT 0,
    added_time       INTEGER NOT NULL DEFAULT 0,
    completed_time   INTEGER NOT NULL DEFAULT 0,
    bitfield         TEXT NOT NULL DEFAULT '',
    trackers         TEXT NOT NULL DEFAULT ''
);
";

impl StateDb {
    /// Open (creating if absent) the state database at `path`.
    ///
    /// WAL plus `synchronous=NORMAL` is the right pairing here: a commit is an
    /// append to the log with no fsync of the main database, and the worst a
    /// power cut can cost is the last few seconds of counters -- exactly the
    /// durability the five-minute JSON sweep offered, at a fraction of the I/O.
    /// A torn write cannot happen at all, which is more than the old scheme
    /// could say before the `.tmp` + rename fix.
    pub fn open(path: &Path) -> Result<Self, rusqlite::Error> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn count(&self) -> usize {
        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(e) => e.into_inner(),
        };
        conn.query_row("SELECT COUNT(*) FROM torrent_state", [], |r| r.get::<_, i64>(0))
            .optional()
            .ok()
            .flatten()
            .unwrap_or(0) as usize
    }

    /// Upsert one record. Used on the paths that must be durable immediately
    /// (a torrent is added, a category move rewrites the save path) rather
    /// than waiting for the next sweep.
    pub fn put(&self, rd: &ResumeData) -> Result<(), rusqlite::Error> {
        let mut conn = match self.conn.lock() {
            Ok(c) => c,
            Err(e) => e.into_inner(),
        };
        let tx = conn.transaction()?;
        put_in(&tx, rd)?;
        tx.commit()
    }

    /// Upsert many records in a single transaction. This is the whole point of
    /// the module: the caller hands over only the rows whose fingerprint moved,
    /// and the entire sweep costs one commit instead of one file per torrent.
    pub fn put_batch(&self, records: &[ResumeData]) -> Result<usize, rusqlite::Error> {
        if records.is_empty() {
            return Ok(0);
        }
        let mut conn = match self.conn.lock() {
            Ok(c) => c,
            Err(e) => e.into_inner(),
        };
        let tx = conn.transaction()?;
        for rd in records {
            put_in(&tx, rd)?;
        }
        tx.commit()?;
        Ok(records.len())
    }

    pub fn remove(&self, info_hash_hex: &str) -> Result<(), rusqlite::Error> {
        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(e) => e.into_inner(),
        };
        conn.execute("DELETE FROM torrent_state WHERE info_hash = ?1", params![info_hash_hex])?;
        Ok(())
    }

    /// Read every record back. Shape-compatible with `fastresume::load_all`
    /// so the startup path does not care which backend it came from.
    pub fn load_all(&self) -> Vec<ResumeData> {
        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(e) => e.into_inner(),
        };
        let mut stmt = match conn.prepare(
            "SELECT info_hash, torrent_path, save_path, seed_mode, paused,
                    total_uploaded, total_downloaded, added_time, completed_time,
                    bitfield, trackers
             FROM torrent_state",
        ) {
            Ok(s) => s,
            Err(e) => {
                warn!("[statedb] prepare load_all failed: {}", e);
                return Vec::new();
            }
        };
        let rows = stmt.query_map([], |row| {
            let trackers_json: String = row.get(10)?;
            Ok(ResumeData {
                info_hash: row.get(0)?,
                torrent_path: row.get(1)?,
                save_path: row.get(2)?,
                seed_mode: row.get::<_, i64>(3)? != 0,
                paused: row.get::<_, i64>(4)? != 0,
                total_uploaded: row.get::<_, i64>(5)? as u64,
                total_downloaded: row.get::<_, i64>(6)? as u64,
                added_time: row.get(7)?,
                completed_time: row.get(8)?,
                bitfield: row.get(9)?,
                trackers: decode_trackers(&trackers_json),
            })
        });
        let rows = match rows {
            Ok(r) => r,
            Err(e) => {
                warn!("[statedb] query load_all failed: {}", e);
                return Vec::new();
            }
        };
        let mut out = Vec::new();
        for r in rows {
            match r {
                Ok(rd) => out.push(rd),
                Err(e) => warn!("[statedb] bad row: {}", e),
            }
        }
        out
    }

    /// One-shot import of a legacy `resume/` directory.
    ///
    /// Deliberately non-destructive: the JSON files are left exactly where they
    /// are. Rolling back to a build that predates this module is then just a
    /// matter of running it -- it finds its directory untouched. Cleaning up is
    /// a separate, deliberate act once the operator is satisfied.
    pub fn import_legacy(&self, resume_dir: &str) -> usize {
        let records = super::fastresume::load_all(resume_dir);
        if records.is_empty() {
            return 0;
        }
        match self.put_batch(&records) {
            Ok(n) => {
                info!("[statedb] imported {} legacy resume records (JSON files left in place)", n);
                n
            }
            Err(e) => {
                warn!("[statedb] legacy import failed: {}", e);
                0
            }
        }
    }
}

fn put_in(conn: &Connection, rd: &ResumeData) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT INTO torrent_state
            (info_hash, torrent_path, save_path, seed_mode, paused,
             total_uploaded, total_downloaded, added_time, completed_time,
             bitfield, trackers)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT(info_hash) DO UPDATE SET
            torrent_path=excluded.torrent_path,
            save_path=excluded.save_path,
            seed_mode=excluded.seed_mode,
            paused=excluded.paused,
            total_uploaded=excluded.total_uploaded,
            total_downloaded=excluded.total_downloaded,
            added_time=excluded.added_time,
            completed_time=excluded.completed_time,
            bitfield=excluded.bitfield,
            trackers=excluded.trackers",
        params![
            rd.info_hash,
            rd.torrent_path,
            rd.save_path,
            rd.seed_mode as i64,
            rd.paused as i64,
            rd.total_uploaded as i64,
            rd.total_downloaded as i64,
            rd.added_time,
            rd.completed_time,
            rd.bitfield,
            encode_trackers(&rd.trackers),
        ],
    )?;
    Ok(())
}

fn encode_trackers(t: &[Vec<String>]) -> String {
    if t.is_empty() {
        return String::new();
    }
    serde_json::to_string(t).unwrap_or_default()
}

fn decode_trackers(s: &str) -> Vec<Vec<String>> {
    if s.is_empty() {
        return Vec::new();
    }
    serde_json::from_str(s).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(ih: &str, up: u64) -> ResumeData {
        ResumeData {
            info_hash: ih.to_string(),
            torrent_path: format!("/uploads/{}.torrent", ih),
            save_path: "/data/x".into(),
            seed_mode: true,
            paused: false,
            total_uploaded: up,
            total_downloaded: 7,
            added_time: 100,
            completed_time: 200,
            bitfield: "ff00".into(),
            trackers: vec![vec!["http://t/announce".into()]],
        }
    }

    fn tmpdb(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("typhon-statedb-test-{}-{}.db", name, std::process::id()));
        std::fs::remove_file(&p).ok();
        p
    }

    #[test]
    fn roundtrip_preserves_every_field() {
        let path = tmpdb("roundtrip");
        let db = StateDb::open(&path).unwrap();
        db.put(&rec("aa", 42)).unwrap();
        let all = db.load_all();
        assert_eq!(all.len(), 1);
        let r = &all[0];
        assert_eq!(r.info_hash, "aa");
        assert_eq!(r.torrent_path, "/uploads/aa.torrent");
        assert_eq!(r.save_path, "/data/x");
        assert!(r.seed_mode);
        assert!(!r.paused);
        assert_eq!(r.total_uploaded, 42);
        assert_eq!(r.total_downloaded, 7);
        assert_eq!(r.added_time, 100);
        assert_eq!(r.completed_time, 200);
        assert_eq!(r.bitfield, "ff00");
        assert_eq!(r.trackers, vec![vec!["http://t/announce".to_string()]]);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn upsert_overwrites_rather_than_duplicating() {
        let path = tmpdb("upsert");
        let db = StateDb::open(&path).unwrap();
        db.put(&rec("bb", 1)).unwrap();
        db.put(&rec("bb", 999)).unwrap();
        let all = db.load_all();
        assert_eq!(all.len(), 1, "info_hash is the primary key");
        assert_eq!(all[0].total_uploaded, 999);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn batch_writes_all_rows_in_one_transaction() {
        let path = tmpdb("batch");
        let db = StateDb::open(&path).unwrap();
        let recs: Vec<ResumeData> = (0..500).map(|i| rec(&format!("{:040x}", i), i as u64)).collect();
        assert_eq!(db.put_batch(&recs).unwrap(), 500);
        assert_eq!(db.count(), 500);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn remove_leaves_no_orphan() {
        let path = tmpdb("remove");
        let db = StateDb::open(&path).unwrap();
        db.put(&rec("cc", 1)).unwrap();
        db.remove("cc").unwrap();
        assert_eq!(db.count(), 0);
        assert!(db.load_all().is_empty());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn empty_trackers_survive_the_round_trip() {
        let path = tmpdb("notrackers");
        let db = StateDb::open(&path).unwrap();
        let mut r = rec("dd", 0);
        r.trackers = Vec::new();
        r.bitfield = String::new();
        db.put(&r).unwrap();
        let back = db.load_all();
        assert!(back[0].trackers.is_empty());
        assert!(back[0].bitfield.is_empty());
        std::fs::remove_file(&path).ok();
    }
}
