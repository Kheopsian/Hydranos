//! The measurement database, and the recorder that fills it.
//!
//! `bench.db` is where the race timeline lives: what was added, what completed,
//! and how long it took. Nothing else in the daemon writes it, and no endpoint
//! can invent it -- an empty timeline and a timeline nobody recorded look
//! identical from the outside, which is why this module exists rather than the
//! endpoints answering `[]` and calling it done.
//!
//! Schema identical to the one 3.x creates, so the two write the same file.

use rusqlite::{Connection, OpenFlags};
use std::path::Path;
use std::sync::{Arc, Mutex};

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS race_events (
    ts REAL NOT NULL, info_hash TEXT NOT NULL, event TEXT NOT NULL,
    name TEXT DEFAULT '', size INTEGER DEFAULT 0, download_time REAL DEFAULT 0,
    upload_total INTEGER DEFAULT 0, upload_rate REAL DEFAULT 0,
    download_rate REAL DEFAULT 0, peers INTEGER DEFAULT 0, seeds INTEGER DEFAULT 0,
    swarm_seeds INTEGER DEFAULT 0, swarm_leechers INTEGER DEFAULT 0,
    category TEXT DEFAULT '', time_since_add REAL DEFAULT 0,
    download_total INTEGER DEFAULT 0, uploader TEXT DEFAULT '',
    injected_peers INTEGER DEFAULT 0);
CREATE INDEX IF NOT EXISTS idx_race_events_ts ON race_events(ts);
CREATE INDEX IF NOT EXISTS idx_race_events_hash ON race_events(info_hash);
CREATE TABLE IF NOT EXISTS race_snapshots (
    ts REAL NOT NULL, info_hash TEXT NOT NULL, progress REAL DEFAULT 0,
    upload_rate REAL DEFAULT 0, download_rate REAL DEFAULT 0,
    total_upload INTEGER DEFAULT 0, total_download INTEGER DEFAULT 0,
    peers INTEGER DEFAULT 0, seeds INTEGER DEFAULT 0,
    swarm_seeds INTEGER DEFAULT 0, swarm_leechers INTEGER DEFAULT 0,
    ratio REAL DEFAULT 0, peers_json TEXT DEFAULT '');
CREATE INDEX IF NOT EXISTS idx_race_snap_ts ON race_snapshots(ts);
CREATE INDEX IF NOT EXISTS idx_race_snap_hash ON race_snapshots(info_hash);
";

/// One recorded moment in a torrent's life.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RaceEvent {
    pub ts: f64,
    pub info_hash: String,
    pub event: String,
    pub name: String,
    pub size: i64,
    pub download_time: f64,
    pub upload_total: i64,
    pub download_total: i64,
    pub upload_rate: f64,
    pub download_rate: f64,
    pub peers: i64,
    pub seeds: i64,
    pub swarm_seeds: i64,
    pub swarm_leechers: i64,
    pub category: String,
    pub time_since_add: f64,
    // Both were added to the table by a migration, after the fact, and both
    // carry `omitempty` in 3.x: an event with no uploader and no injected peer
    // -- which is every event the recorder itself writes -- publishes neither
    // key. Serialising them as ""/0 would add two keys to every object.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub uploader: String,
    #[serde(skip_serializing_if = "is_zero")]
    pub injected_peers: i64,
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

/// Turns repeated sightings of a torrent into the few moments worth recording.
///
/// The loop sees the same torrent every tick; the timeline wants two instants
/// out of that stream: when it appeared, and when it finished. Keeping the last
/// seen progress per hash is what tells a crossing apart from a torrent that
/// was already complete when this process started -- the latter must not
/// publish a completion it did not witness.
pub struct RaceRecorder {
    /// Last progress seen, per info hash.
    seen: std::collections::HashMap<String, f64>,
    /// When this process started watching. Anything already complete at its
    /// first sighting predates us.
    started_at: f64,
}

impl RaceRecorder {
    pub fn new(started_at: f64) -> Self {
        Self { seen: std::collections::HashMap::new(), started_at }
    }

    /// One sighting. Returns the event to record, if this one is a moment.
    pub fn sight(
        &mut self,
        info_hash: &str,
        added_time: f64,
        progress: f64,
        now: f64,
    ) -> Option<(String, f64, f64)> {
        let complete = progress >= 1.0;
        match self.seen.insert(info_hash.to_string(), progress) {
            None => {
                // First sighting. A torrent added before we started is not news
                // -- it would date every old torrent to this boot.
                if added_time >= self.started_at && !complete {
                    Some(("added".to_string(), now, 0.0))
                } else {
                    None
                }
            }
            Some(previous) => {
                if complete && previous < 1.0 {
                    let download_time = if added_time > 0.0 { now - added_time } else { 0.0 };
                    Some(("completed".to_string(), now, download_time))
                } else {
                    None
                }
            }
        }
    }

    /// Forget torrents that left the engine, so the map tracks the library
    /// rather than growing with everything ever seen.
    pub fn prune(&mut self, live: &std::collections::HashSet<String>) {
        self.seen.retain(|hash, _| live.contains(hash));
    }
}

/// A race torrent as it stood at one instant.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RaceSnapshot {
    pub ts: f64,
    pub info_hash: String,
    pub progress: f64,
    pub upload_rate: f64,
    pub download_rate: f64,
    pub total_upload: i64,
    pub total_download: i64,
    pub peers: i64,
    pub seeds: i64,
    pub swarm_seeds: i64,
    pub swarm_leechers: i64,
    pub ratio: f64,
    // Migrated in later and `omitempty`: a snapshot taken before the column
    // existed, or one with no peer detail, publishes no key at all.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub peers_json: String,
}

pub struct BenchDb {
    conn: Connection,
}

impl BenchDb {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    pub fn open_in_memory() -> anyhow::Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    pub fn record(&self, e: &RaceEvent) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT INTO race_events (ts, info_hash, event, name, size, download_time,
                 upload_total, download_total, upload_rate, download_rate, peers, seeds,
                 swarm_seeds, swarm_leechers, category, time_since_add, uploader,
                 injected_peers)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
            rusqlite::params![
                e.ts,
                e.info_hash,
                e.event,
                e.name,
                e.size,
                e.download_time,
                e.upload_total,
                e.download_total,
                e.upload_rate,
                e.download_rate,
                e.peers,
                e.seeds,
                e.swarm_seeds,
                e.swarm_leechers,
                e.category,
                e.time_since_add,
                e.uploader,
                e.injected_peers
            ],
        )?;
        Ok(())
    }

    fn read(&self, sql: &str, params: &[&dyn rusqlite::ToSql]) -> anyhow::Result<Vec<RaceEvent>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows: Vec<RaceEvent> = stmt
            .query_map(params, |r| {
                Ok(RaceEvent {
                    ts: r.get(0)?,
                    info_hash: r.get(1)?,
                    event: r.get(2)?,
                    name: r.get(3)?,
                    size: r.get(4)?,
                    download_time: r.get(5)?,
                    upload_total: r.get(6)?,
                    download_total: r.get(7)?,
                    upload_rate: r.get(8)?,
                    download_rate: r.get(9)?,
                    peers: r.get(10)?,
                    seeds: r.get(11)?,
                    swarm_seeds: r.get(12)?,
                    swarm_leechers: r.get(13)?,
                    category: r.get(14)?,
                    time_since_add: r.get(15)?,
                    uploader: r.get(16)?,
                    injected_peers: r.get(17)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // The two migrated columns are read through COALESCE: a bench.db created by
    // an older 3.x has NULLs there, and a NULL into a non-Option field is a
    // decode error that would empty the whole answer.
    const COLUMNS: &'static str = "ts, info_hash, event, name, size, download_time,
        upload_total, COALESCE(download_total,0), upload_rate, download_rate, peers,
        seeds, swarm_seeds, swarm_leechers, category, time_since_add,
        COALESCE(uploader,''), COALESCE(injected_peers,0)";

    /// The most recent events, newest first.
    ///
    /// ORDER BY is DESC and the LIMIT applies to that order, so a capped read
    /// keeps the newest rows rather than the first ones ever written -- the
    /// opposite mistake yields a panel frozen on the day of installation.
    pub fn events(&self, limit: i64) -> anyhow::Result<Vec<RaceEvent>> {
        self.read(
            &format!(
                "SELECT {} FROM race_events ORDER BY ts DESC LIMIT ?1",
                Self::COLUMNS
            ),
            &[&limit],
        )
    }

    /// Events inside a window, oldest first -- the order the chart draws in.
    pub fn events_in_range(&self, start: f64, end: f64) -> anyhow::Result<Vec<RaceEvent>> {
        self.read(
            &format!(
                "SELECT {} FROM race_events WHERE ts >= ?1 AND ts <= ?2 ORDER BY ts",
                Self::COLUMNS
            ),
            &[&start, &end],
        )
    }

    pub fn events_for(&self, info_hash: &str) -> anyhow::Result<Vec<RaceEvent>> {
        self.read(
            &format!(
                "SELECT {} FROM race_events WHERE info_hash = ?1 ORDER BY ts",
                Self::COLUMNS
            ),
            &[&info_hash],
        )
    }

    /// Snapshots for one torrent, oldest first.
    pub fn snapshots_for(&self, info_hash: &str) -> anyhow::Result<Vec<RaceSnapshot>> {
        let mut stmt = self.conn.prepare(
            "SELECT ts, info_hash, progress, upload_rate, download_rate, total_upload,
                    total_download, peers, seeds, swarm_seeds, swarm_leechers, ratio,
                    COALESCE(peers_json,'')
             FROM race_snapshots WHERE info_hash = ?1 ORDER BY ts",
        )?;
        let rows = stmt
            .query_map([info_hash], |r| {
                Ok(RaceSnapshot {
                    ts: r.get(0)?,
                    info_hash: r.get(1)?,
                    progress: r.get(2)?,
                    upload_rate: r.get(3)?,
                    download_rate: r.get(4)?,
                    total_upload: r.get(5)?,
                    total_download: r.get(6)?,
                    peers: r.get(7)?,
                    seeds: r.get(8)?,
                    swarm_seeds: r.get(9)?,
                    swarm_leechers: r.get(10)?,
                    ratio: r.get(11)?,
                    peers_json: r.get(12)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Forget everything recorded about one torrent.
    ///
    /// 3.x does this when a torrent leaves the engine: an info hash re-added
    /// later is a new torrent, and inheriting its predecessor's timeline would
    /// date its "added" before it existed.
    pub fn purge(&self, info_hash: &str) -> anyhow::Result<()> {
        self.conn
            .execute("DELETE FROM race_events WHERE info_hash = ?1", [info_hash])?;
        self.conn
            .execute("DELETE FROM race_snapshots WHERE info_hash = ?1", [info_hash])?;
        Ok(())
    }
}

pub type Shared = Arc<Mutex<BenchDb>>;

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(hash: &str, kind: &str, ts: f64) -> RaceEvent {
        RaceEvent {
            ts,
            info_hash: hash.into(),
            event: kind.into(),
            name: "n".into(),
            size: 10,
            download_time: 0.0,
            upload_total: 1,
            download_total: 2,
            upload_rate: 0.0,
            download_rate: 0.0,
            peers: 0,
            seeds: 0,
            swarm_seeds: 0,
            swarm_leechers: 0,
            category: "Race".into(),
            time_since_add: 0.0,
            uploader: String::new(),
            injected_peers: 0,
        }
    }

    #[test]
    fn events_round_trip_through_the_database() {
        let db = BenchDb::open_in_memory().unwrap();
        db.record(&ev("abc", "added", 42.0)).unwrap();
        let all = db.events(10).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].info_hash, "abc");
        assert_eq!(all[0].event, "added");
        assert_eq!(db.events_for("abc").unwrap().len(), 1);
        assert!(db.events_for("nope").unwrap().is_empty());
    }

    #[test]
    fn events_come_back_newest_first() {
        let db = BenchDb::open_in_memory().unwrap();
        db.record(&ev("a", "added", 10.0)).unwrap();
        db.record(&ev("b", "added", 30.0)).unwrap();
        db.record(&ev("c", "added", 20.0)).unwrap();
        let got: Vec<f64> = db.events(10).unwrap().iter().map(|e| e.ts).collect();
        assert_eq!(got, vec![30.0, 20.0, 10.0]);
    }

    #[test]
    fn the_limit_keeps_the_newest_not_the_first_written() {
        let db = BenchDb::open_in_memory().unwrap();
        db.record(&ev("a", "added", 10.0)).unwrap();
        db.record(&ev("b", "added", 30.0)).unwrap();
        assert_eq!(db.events(1).unwrap()[0].ts, 30.0);
    }

    /// A window read is inclusive on both ends and ordered oldest first, the
    /// opposite of `events`.
    #[test]
    fn a_window_is_inclusive_and_ordered_oldest_first() {
        let db = BenchDb::open_in_memory().unwrap();
        for ts in [10.0, 20.0, 30.0, 40.0] {
            db.record(&ev("a", "added", ts)).unwrap();
        }
        let got: Vec<f64> = db
            .events_in_range(20.0, 30.0)
            .unwrap()
            .iter()
            .map(|e| e.ts)
            .collect();
        assert_eq!(got, vec![20.0, 30.0]);
    }

    /// 3.x purges a torrent's rows when it leaves the engine, so a re-add does
    /// not inherit the timeline of its predecessor.
    #[test]
    fn purging_a_torrent_leaves_the_others_alone() {
        let db = BenchDb::open_in_memory().unwrap();
        db.record(&ev("gone", "added", 1.0)).unwrap();
        db.record(&ev("stays", "added", 2.0)).unwrap();
        db.purge("gone").unwrap();
        assert!(db.events_for("gone").unwrap().is_empty());
        assert_eq!(db.events_for("stays").unwrap().len(), 1);
    }

    /// The two migrated columns carry `omitempty`: an event the recorder wrote
    /// itself publishes neither key.
    #[test]
    fn empty_migrated_columns_publish_no_key() {
        let json = serde_json::to_string(&ev("a", "added", 1.0)).unwrap();
        assert!(!json.contains("uploader"), "{json}");
        assert!(!json.contains("injected_peers"), "{json}");
    }
}
