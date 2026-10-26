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

use rusqlite::{params, Connection};
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
    /// Seed time in HOURS. See `fingerprint_of`: seconds here would mark every
    /// seeding torrent dirty at every sweep.
    pub seed_hours: i64,
    /// This torrent's own rate caps: setting one must reach the record.
    pub up_limit: u64,
    pub down_limit: u64,
    /// Parked because pieces it held were found missing: lifting the park
    /// must reach the record as surely as setting it.
    pub data_lost: bool,
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
    trackers         TEXT NOT NULL DEFAULT '',
    seed_secs        INTEGER NOT NULL DEFAULT 0,
    up_limit         INTEGER NOT NULL DEFAULT 0,
    down_limit       INTEGER NOT NULL DEFAULT 0,
    download_order   TEXT NOT NULL DEFAULT '',
    bitfield_sum     TEXT NOT NULL DEFAULT '',
    file_renames     TEXT NOT NULL DEFAULT '',
    data_lost        TEXT NOT NULL DEFAULT ''
);
";

/// Rows per commit in `put_batch`.
const PUT_BATCH_SLICE: usize = 2000;

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
        // The schema above only builds a NEW database. Every existing one --
        // including a 300k-row production file -- keeps the old layout, so the
        // column has to be added explicitly. A second run fails with "duplicate
        // column name", which is the success case here and is why the error is
        // dropped rather than propagated.
        let _ = conn.execute(
            "ALTER TABLE torrent_state ADD COLUMN seed_secs INTEGER NOT NULL DEFAULT 0",
            [],
        );
        // Per-torrent rate caps (bytes/s, 0 = none), added the same way. An
        // older binary reading this file ignores the columns; a rollback loses
        // the caps and nothing else.
        let _ = conn.execute(
            "ALTER TABLE torrent_state ADD COLUMN up_limit INTEGER NOT NULL DEFAULT 0",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE torrent_state ADD COLUMN down_limit INTEGER NOT NULL DEFAULT 0",
            [],
        );
        // Download order (file priorities, sequential, first/last piece) as
        // JSON, '' for the default. Same migration, same rollback story.
        let _ = conn.execute(
            "ALTER TABLE torrent_state ADD COLUMN download_order TEXT NOT NULL DEFAULT ''",
            [],
        );
        // A checksum of the bitfield, written with it. '' = none (a row from
        // before the column, or written by an older build): such a row is
        // read as it always was. The trigger is what keeps an older build
        // that rolls back onto this file from poisoning it: it rewrites a
        // bitfield without a sum, and the sum it leaves behind would no
        // longer match.
        let _ = conn.execute(
            "ALTER TABLE torrent_state ADD COLUMN bitfield_sum TEXT NOT NULL DEFAULT ''",
            [],
        );
        // Files renamed after the add, as JSON {index: path}, '' for none.
        // Same migration, same rollback story: an older build ignores it.
        let _ = conn.execute(
            "ALTER TABLE torrent_state ADD COLUMN file_renames TEXT NOT NULL DEFAULT ''",
            [],
        );
        // Why the torrent is parked for data found missing, '' when it is not.
        // Same migration; an older build ignores it, and a recheck clears it.
        let _ = conn.execute(
            "ALTER TABLE torrent_state ADD COLUMN data_lost TEXT NOT NULL DEFAULT ''",
            [],
        );
        let _ = conn.execute_batch(
            "CREATE TRIGGER IF NOT EXISTS bitfield_sum_reset
             AFTER UPDATE OF bitfield ON torrent_state
             WHEN NEW.bitfield_sum = OLD.bitfield_sum AND NEW.bitfield <> OLD.bitfield
             BEGIN
                 UPDATE torrent_state SET bitfield_sum = '' WHERE info_hash = NEW.info_hash;
             END;",
        );
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn count(&self) -> usize {
        self.try_count().unwrap_or(0)
    }

    /// `count`, telling an empty table from one that could not be read.
    pub fn try_count(&self) -> Result<usize, rusqlite::Error> {
        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(e) => e.into_inner(),
        };
        conn.query_row("SELECT COUNT(*) FROM torrent_state", [], |r| {
            r.get::<_, i64>(0)
        })
        .map(|n| n as usize)
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

    /// Upsert many records, a few thousand per transaction. This is the
    /// whole point of the module: the caller hands over only the rows whose
    /// fingerprint moved, and a sweep costs a handful of commits instead of
    /// one file per torrent.
    ///
    /// Not one transaction for everything: the shutdown flush rewrites every
    /// row, and a process stopped before a single commit of 300k rows lost all
    /// of them. In slices, what is committed stays committed, and a removal
    /// waiting for the connection gets it between two slices.
    pub fn put_batch(&self, records: &[ResumeData]) -> Result<usize, rusqlite::Error> {
        for slice in records.chunks(PUT_BATCH_SLICE) {
            let mut conn = match self.conn.lock() {
                Ok(c) => c,
                Err(e) => e.into_inner(),
            };
            let tx = conn.transaction()?;
            for rd in slice {
                put_in(&tx, rd)?;
            }
            tx.commit()?;
        }
        Ok(records.len())
    }

    pub fn remove(&self, info_hash_hex: &str) -> Result<(), rusqlite::Error> {
        self.remove_timed(info_hash_hex).0
    }

    /// `remove`, with the time spent WAITING for the connection apart from
    /// the time spent holding it. The DELETE itself measures under 1 ms; if a
    /// removal is slow here, it is queued behind whoever else holds the
    /// connection (the resume flush), and only the split says which.
    pub fn remove_timed(
        &self,
        info_hash_hex: &str,
    ) -> (
        Result<(), rusqlite::Error>,
        std::time::Duration,
        std::time::Duration,
    ) {
        let asked = std::time::Instant::now();
        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(e) => e.into_inner(),
        };
        let got = std::time::Instant::now();
        let r = conn
            .execute(
                "DELETE FROM torrent_state WHERE info_hash = ?1",
                params![info_hash_hex],
            )
            .map(|_| ());
        drop(conn);
        (r, got - asked, got.elapsed())
    }

    /// Read every record back. Shape-compatible with `fastresume::load_all`
    /// so the startup path does not care which backend it came from.
    pub fn load_all(&self) -> Vec<ResumeData> {
        self.load_all_reporting().0
    }

    /// `load_all`, plus the info hash of every row that could not be read.
    /// Such a torrent is not loaded, and is not gone either: whoever keeps
    /// other copies of its state must not take its absence for a removal.
    pub fn load_all_reporting(&self) -> (Vec<ResumeData>, Vec<String>) {
        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(e) => e.into_inner(),
        };
        let mut stmt = match conn.prepare(
            "SELECT info_hash, save_path, seed_mode, paused,
                    total_uploaded, total_downloaded, added_time, completed_time,
                    bitfield, trackers, seed_secs, up_limit, down_limit, download_order,
                    bitfield_sum, file_renames, data_lost
             FROM torrent_state",
        ) {
            Ok(s) => s,
            Err(e) => {
                warn!("[statedb] prepare load_all failed: {}", e);
                return (Vec::new(), Vec::new());
            }
        };
        let rows = stmt.query_map([], |row| {
            let key: Option<String> = row.get(0).ok();
            Ok(read_row(row).map_err(|e| (key, e)))
        });
        fn read_row(row: &rusqlite::Row) -> Result<ResumeData, rusqlite::Error> {
            let trackers_json: String = row.get(9)?;
            // A bitfield that does not match the sum written with it was
            // altered after the fact. The torrent still loads -- a torrent
            // that vanishes is worse -- but without the claim: the marker
            // reads as no bitfield, and the loader asks the disk instead.
            let mut bitfield: String = row.get(8)?;
            let sum: String = row.get(14).unwrap_or_default();
            if !sum.is_empty() && sum != bitfield_sum(&bitfield) {
                warn!(
                    "[statedb] the bitfield of {} does not match its checksum",
                    row.get::<_, String>(0).unwrap_or_default()
                );
                bitfield = super::fastresume::BITFIELD_DISCARDED.to_string();
            }
            Ok(ResumeData {
                info_hash: row.get(0)?,
                save_path: row.get(1)?,
                seed_mode: row.get::<_, i64>(2)? != 0,
                paused: row.get::<_, i64>(3)? != 0,
                total_uploaded: row.get::<_, i64>(4)? as u64,
                total_downloaded: row.get::<_, i64>(5)? as u64,
                added_time: row.get(6)?,
                completed_time: row.get(7)?,
                bitfield,
                trackers: decode_trackers(&trackers_json),
                seed_secs: row.get(10).unwrap_or(0),
                up_limit: row.get::<_, i64>(11).unwrap_or(0).max(0) as u64,
                down_limit: row.get::<_, i64>(12).unwrap_or(0).max(0) as u64,
                download_order: decode_order(&row.get::<_, String>(13).unwrap_or_default()),
                file_renames: decode_renames(&row.get::<_, String>(15).unwrap_or_default()),
                data_lost: row.get(16).unwrap_or_default(),
            })
        }
        let rows = match rows {
            Ok(r) => r,
            Err(e) => {
                warn!("[statedb] query load_all failed: {}", e);
                return (Vec::new(), Vec::new());
            }
        };
        let mut out = Vec::new();
        let mut unreadable = Vec::new();
        for r in rows {
            match r {
                Ok(Ok(rd)) => out.push(rd),
                Ok(Err((key, e))) => {
                    warn!("[statedb] bad row {}: {}", key.as_deref().unwrap_or("?"), e);
                    unreadable.extend(key);
                }
                Err(e) => warn!("[statedb] bad row: {}", e),
            }
        }
        (out, unreadable)
    }

    /// One-shot import of a legacy `resume/` directory.
    ///
    /// Deliberately non-destructive: the JSON files are left exactly where they
    /// are. Rolling back to a build that predates this module is then just a
    /// matter of running it -- it finds its directory untouched. Cleaning up is
    /// a separate, deliberate act once the operator is satisfied.
    ///
    /// Returns how many were imported, and the name of every file that could
    /// not be read: those torrents are not imported, and not gone.
    pub fn import_legacy(&self, resume_dir: &str) -> (usize, Vec<String>) {
        let (records, unreadable) = super::fastresume::load_all_reporting(resume_dir);
        if records.is_empty() {
            return (0, unreadable);
        }
        match self.put_batch(&records) {
            Ok(n) => {
                info!(
                    "[statedb] imported {} legacy resume records (JSON files left in place)",
                    n
                );
                (n, unreadable)
            }
            Err(e) => {
                warn!("[statedb] legacy import failed: {}", e);
                (0, unreadable)
            }
        }
    }
}

/// Checksum of a bitfield's text (FNV-1a, 64 bits): it catches a digit that
/// rotted or was edited, it is not meant to stop anyone who can rewrite the
/// row. '' for no bitfield.
fn bitfield_sum(bitfield: &str) -> String {
    if bitfield.is_empty() {
        return String::new();
    }
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bitfield.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

fn put_in(conn: &Connection, rd: &ResumeData) -> Result<(), rusqlite::Error> {
    conn.execute(
        // torrent_path is NOT written any more. The column survives in the
        // schema (NOT NULL DEFAULT '') so an older binary can still read this
        // file, but nothing reads it back: the metainfo comes from the store,
        // keyed by info-hash. Dropping the column is a migration of its own.
        "INSERT INTO torrent_state
            (info_hash, save_path, seed_mode, paused,
             total_uploaded, total_downloaded, added_time, completed_time,
             bitfield, trackers, seed_secs, up_limit, down_limit, download_order,
             bitfield_sum, file_renames, data_lost)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
         ON CONFLICT(info_hash) DO UPDATE SET
            save_path=excluded.save_path,
            seed_mode=excluded.seed_mode,
            paused=excluded.paused,
            total_uploaded=excluded.total_uploaded,
            total_downloaded=excluded.total_downloaded,
            added_time=excluded.added_time,
            completed_time=excluded.completed_time,
            bitfield=excluded.bitfield,
            trackers=excluded.trackers,
            seed_secs=excluded.seed_secs,
            up_limit=excluded.up_limit,
            down_limit=excluded.down_limit,
            download_order=excluded.download_order,
            bitfield_sum=excluded.bitfield_sum,
            file_renames=excluded.file_renames,
            data_lost=excluded.data_lost",
        params![
            rd.info_hash,
            rd.save_path,
            rd.seed_mode as i64,
            rd.paused as i64,
            rd.total_uploaded as i64,
            rd.total_downloaded as i64,
            rd.added_time,
            rd.completed_time,
            rd.bitfield,
            encode_trackers(&rd.trackers),
            rd.seed_secs,
            rd.up_limit.min(i64::MAX as u64) as i64,
            rd.down_limit.min(i64::MAX as u64) as i64,
            encode_order(&rd.download_order),
            bitfield_sum(&rd.bitfield),
            encode_renames(&rd.file_renames),
            rd.data_lost,
        ],
    )?;
    Ok(())
}

/// '' for the default, so the column costs nothing on the rows that have none.
fn encode_order(o: &Option<super::piece_picker::DownloadOrder>) -> String {
    match o {
        Some(o) if !o.is_default() => serde_json::to_string(o).unwrap_or_default(),
        _ => String::new(),
    }
}

/// A value that does not parse reads as the default: a lost order costs a
/// download its preferred sequence, never a piece.
fn decode_order(s: &str) -> Option<super::piece_picker::DownloadOrder> {
    if s.is_empty() {
        return None;
    }
    serde_json::from_str::<super::piece_picker::DownloadOrder>(s)
        .ok()
        .filter(|o| !o.is_default())
}

/// '' for no rename, so the column costs nothing on the rows that have none.
fn encode_renames(r: &std::collections::BTreeMap<usize, String>) -> String {
    if r.is_empty() {
        return String::new();
    }
    serde_json::to_string(r).unwrap_or_default()
}

/// A value that does not parse reads as no rename: the torrent then looks for
/// its files under the names its .torrent gives, which a recheck reports.
fn decode_renames(s: &str) -> std::collections::BTreeMap<usize, String> {
    if s.is_empty() {
        return Default::default();
    }
    serde_json::from_str(s).unwrap_or_else(|e| {
        warn!("[statedb] the renamed files of a row could not be read ({e}); none applied");
        Default::default()
    })
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
            save_path: "/data/x".into(),
            seed_mode: true,
            paused: false,
            total_uploaded: up,
            total_downloaded: 7,
            added_time: 100,
            completed_time: 200,
            bitfield: "ff00".into(),
            trackers: vec![vec!["http://t/announce".into()]],
            seed_secs: 0,
            up_limit: 51_200,
            down_limit: 0,
            download_order: Some(crate::torrent::piece_picker::DownloadOrder {
                sequential: true,
                first_last: false,
                file_prio: vec![1, 7],
            }),
            file_renames: [(1usize, "sub/x.mkv".to_string())].into_iter().collect(),
            data_lost: String::new(),
        }
    }

    fn tmpdb(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "typhon-statedb-test-{}-{}.db",
            name,
            std::process::id()
        ));
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
        assert_eq!(r.save_path, "/data/x");
        assert!(r.seed_mode);
        assert!(!r.paused);
        assert_eq!(r.total_uploaded, 42);
        assert_eq!(r.total_downloaded, 7);
        assert_eq!(r.added_time, 100);
        assert_eq!(r.completed_time, 200);
        assert_eq!(r.bitfield, "ff00");
        assert_eq!(r.trackers, vec![vec!["http://t/announce".to_string()]]);
        assert_eq!(
            (r.up_limit, r.down_limit),
            (51_200, 0),
            "the rate caps come back"
        );
        assert_eq!(
            r.download_order,
            rec("aa", 42).download_order,
            "the download order comes back"
        );
        assert_eq!(
            r.file_renames,
            rec("aa", 42).file_renames,
            "the renamed files come back"
        );
        std::fs::remove_file(&path).ok();
    }

    /// A database from before the column opens, keeps its rows, and reads
    /// them as having no renamed file.
    #[test]
    fn a_database_from_before_file_renames_opens_without_loss() {
        let path = tmpdb("renames-old");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE torrent_state (
                    info_hash TEXT PRIMARY KEY, torrent_path TEXT NOT NULL DEFAULT '',
                    save_path TEXT NOT NULL DEFAULT '', seed_mode INTEGER NOT NULL DEFAULT 0,
                    paused INTEGER NOT NULL DEFAULT 0, total_uploaded INTEGER NOT NULL DEFAULT 0,
                    total_downloaded INTEGER NOT NULL DEFAULT 0, added_time INTEGER NOT NULL DEFAULT 0,
                    completed_time INTEGER NOT NULL DEFAULT 0, bitfield TEXT NOT NULL DEFAULT '',
                    trackers TEXT NOT NULL DEFAULT '');
                 INSERT INTO torrent_state (info_hash, save_path, bitfield) VALUES ('old', '/d', 'f0');",
            )
            .unwrap();
        }
        let db = StateDb::open(&path).unwrap();
        let all = db.load_all();
        assert_eq!(all.len(), 1);
        assert_eq!(
            (all[0].info_hash.as_str(), all[0].bitfield.as_str()),
            ("old", "f0")
        );
        assert!(all[0].file_renames.is_empty());
        db.put(&rec("new", 1)).unwrap();
        assert_eq!(
            db.load_all()
                .iter()
                .find(|r| r.info_hash == "new")
                .unwrap()
                .file_renames
                .len(),
            1
        );
        std::fs::remove_file(&path).ok();
    }

    /// The default order is stored as nothing and read back as `None`.
    #[test]
    fn default_download_order_is_stored_empty() {
        let path = tmpdb("order-default");
        let db = StateDb::open(&path).unwrap();
        let mut r = rec("cc", 1);
        r.download_order = Some(Default::default());
        db.put(&r).unwrap();
        let conn = db.conn.lock().unwrap();
        let raw: String = conn
            .query_row(
                "SELECT download_order FROM torrent_state WHERE info_hash='cc'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        drop(conn);
        assert_eq!(raw, "");
        assert_eq!(db.load_all()[0].download_order, None);
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
        let recs: Vec<ResumeData> = (0..500)
            .map(|i| rec(&format!("{:040x}", i), i as u64))
            .collect();
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

    fn raw(path: &std::path::Path, sql: &str) {
        Connection::open(path).unwrap().execute_batch(sql).unwrap();
    }

    fn column(path: &std::path::Path, hash: &str, col: &str) -> String {
        Connection::open(path)
            .unwrap()
            .query_row(
                &format!("SELECT {col} FROM torrent_state WHERE info_hash='{hash}'"),
                [],
                |r| r.get::<_, String>(0),
            )
            .unwrap_or_default()
    }

    /// ⭐ A bitfield that changed under the database (a digit rotted, a hand
    /// edit) is not believed: the row still loads -- the torrent is not made
    /// to disappear -- but without the claim, so the disk gets to say what is
    /// there.
    #[test]
    fn a_bitfield_that_does_not_match_its_sum_is_not_trusted() {
        let path = tmpdb("sum-untrusted");
        let db = StateDb::open(&path).unwrap();
        db.put(&rec("aa", 1)).unwrap();
        // A write that bypasses the engine and any trigger it has installed.
        raw(
            &path,
            "DROP TRIGGER IF EXISTS bitfield_sum_reset;
                    UPDATE torrent_state SET bitfield='ffff' WHERE info_hash='aa'",
        );
        let (records, unreadable) = db.load_all_reporting();
        assert!(unreadable.is_empty(), "the row is readable");
        assert_eq!(records.len(), 1, "and the torrent is still loaded");
        assert_ne!(
            records[0].bitfield, "ffff",
            "but not on the strength of a bitfield nobody vouches for"
        );
        assert_eq!(db.count(), 1);
        std::fs::remove_file(&path).ok();
    }

    /// The triggers that protect a row from an older build must not fire on
    /// this build's own writes: written twice, the sum is still there, still
    /// right, and still catches a change.
    #[test]
    fn the_sum_survives_this_builds_own_rewrites() {
        let path = tmpdb("sum-twice");
        let db = StateDb::open(&path).unwrap();
        db.put(&rec("aa", 1)).unwrap();
        let mut r = rec("aa", 2);
        r.bitfield = "f0f0".into();
        db.put(&r).unwrap();
        db.put(&r).unwrap();
        let sum = column(&path, "aa", "bitfield_sum");
        assert!(!sum.is_empty(), "the sum is kept across writes");
        assert_eq!(
            db.load_all()[0].bitfield,
            "f0f0",
            "and agrees with the bitfield"
        );
        raw(
            &path,
            "DROP TRIGGER IF EXISTS bitfield_sum_reset;
                    UPDATE torrent_state SET bitfield='ffff' WHERE info_hash='aa'",
        );
        assert_ne!(
            db.load_all()[0].bitfield,
            "ffff",
            "so it still catches a change"
        );
        std::fs::remove_file(&path).ok();
    }

    /// A record written before the sum existed has none, and is read as
    /// before; the next write gives it one.
    #[test]
    fn a_database_from_before_the_sum_is_read_without_error() {
        let path = tmpdb("sum-old");
        raw(
            &path,
            "CREATE TABLE torrent_state (
                info_hash TEXT PRIMARY KEY, torrent_path TEXT NOT NULL DEFAULT '',
                save_path TEXT NOT NULL DEFAULT '', seed_mode INTEGER NOT NULL DEFAULT 0,
                paused INTEGER NOT NULL DEFAULT 0, total_uploaded INTEGER NOT NULL DEFAULT 0,
                total_downloaded INTEGER NOT NULL DEFAULT 0, added_time INTEGER NOT NULL DEFAULT 0,
                completed_time INTEGER NOT NULL DEFAULT 0, bitfield TEXT NOT NULL DEFAULT '',
                trackers TEXT NOT NULL DEFAULT '');
             INSERT INTO torrent_state (info_hash, bitfield) VALUES ('old', 'f0');",
        );
        let db = StateDb::open(&path).unwrap();
        let (records, unreadable) = db.load_all_reporting();
        assert!(unreadable.is_empty());
        assert_eq!(records[0].bitfield, "f0");
        db.put(&rec("old", 2)).unwrap();
        assert_eq!(db.load_all()[0].bitfield, "ff00");
        std::fs::remove_file(&path).ok();
    }

    /// An older binary that rolls back onto this file writes bitfields without
    /// sums; the file must stay readable once the new binary is back.
    #[test]
    fn a_bitfield_rewritten_by_an_older_build_is_still_read() {
        let path = tmpdb("sum-rollback");
        let db = StateDb::open(&path).unwrap();
        db.put(&rec("aa", 1)).unwrap();
        raw(
            &path,
            "UPDATE torrent_state SET bitfield='f0f0' WHERE info_hash='aa'",
        );
        let (records, unreadable) = db.load_all_reporting();
        assert!(unreadable.is_empty());
        assert_eq!(records[0].bitfield, "f0f0");
        std::fs::remove_file(&path).ok();
    }
}
