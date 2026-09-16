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
CREATE TABLE IF NOT EXISTS bench_samples (
    ts REAL NOT NULL,
    race_upload_rate REAL DEFAULT 0, race_download_rate REAL DEFAULT 0,
    race_peers REAL DEFAULT 0, race_torrents REAL DEFAULT 0,
    hoard_upload_rate REAL DEFAULT 0, hoard_peers REAL DEFAULT 0,
    hoard_active REAL DEFAULT 0, hoard_with_peers REAL DEFAULT 0,
    hoard_uploading REAL DEFAULT 0, iowait_pct REAL DEFAULT 0,
    arc_size_bytes REAL DEFAULT 0, arc_hit_rate_pct REAL DEFAULT 0,
    arc_demand_hit_rate_pct REAL DEFAULT 0, arc_miss_per_sec REAL DEFAULT 0,
    arc_demand_miss_per_sec REAL DEFAULT 0, arc_ghost_hits_per_sec REAL DEFAULT 0,
    race_uploading REAL DEFAULT 0, race_avg_share REAL DEFAULT 0,
    open_fds REAL DEFAULT 0, hoard_session_uploaded INTEGER DEFAULT 0,
    race_session_uploaded INTEGER DEFAULT 0, global_uploaded INTEGER DEFAULT 0,
    global_downloaded INTEGER DEFAULT 0, race_announce_rate REAL DEFAULT 0,
    hoard_announce_rate REAL DEFAULT 0, race_announce_fail_rate REAL DEFAULT 0,
    hoard_announce_fail_rate REAL DEFAULT 0);
CREATE INDEX IF NOT EXISTS idx_bench_ts ON bench_samples(ts);
CREATE TABLE IF NOT EXISTS tracker_samples (
    ts REAL NOT NULL, engine TEXT NOT NULL, tracker TEXT NOT NULL,
    upload_rate REAL DEFAULT 0, download_rate REAL DEFAULT 0,
    peers REAL DEFAULT 0, active REAL DEFAULT 0, torrents REAL DEFAULT 0,
    cum_uploaded INTEGER DEFAULT 0, cum_downloaded INTEGER DEFAULT 0);
CREATE INDEX IF NOT EXISTS idx_tracker_samples_ts ON tracker_samples(ts);
CREATE INDEX IF NOT EXISTS idx_tracker_samples_trk ON tracker_samples(tracker);
";

/// The columns of `bench_samples`, in the order the graphs read them.
///
/// Written out rather than `SELECT *` so a future migration adding a column
/// cannot silently shift what each position means.
pub const BENCH_COLUMNS: &str = "ts, race_upload_rate, race_download_rate, race_peers, \
     race_torrents, hoard_upload_rate, hoard_peers, hoard_active, hoard_with_peers, \
     hoard_uploading, iowait_pct, arc_size_bytes, arc_hit_rate_pct, \
     arc_demand_hit_rate_pct, arc_miss_per_sec, arc_demand_miss_per_sec, \
     arc_ghost_hits_per_sec, race_uploading, race_avg_share, open_fds, \
     hoard_session_uploaded, race_session_uploaded, global_uploaded, \
     global_downloaded, race_announce_rate, hoard_announce_rate, \
     race_announce_fail_rate, hoard_announce_fail_rate";

/// One recorded moment in a torrent's life.
#[derive(Debug, Clone, Default, serde::Serialize)]
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

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// "Jan 2", the short form the Records card puts under each label.
fn day_date(ts: f64) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (_, m, d) = civil_from_days((ts as i64).div_euclid(86_400));
    format!("{} {}", MONTHS[(m as usize).saturating_sub(1).min(11)], d)
}

/// "2026-09-08", the form the milestone rows and the ETA use.
fn iso_date(ts: f64) -> String {
    let (y, m, d) = civil_from_days((ts as i64).div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Howard Hinnant's civil_from_days, the standard branch-free conversion.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The gap between two milestones, in the words the card uses.
fn human_dur(sec: f64) -> String {
    let d = sec / 86_400.0;
    if d >= 365.0 {
        let y = d / 365.0;
        format!("~{:.1} year{}", y, if y < 2.0 { "" } else { "s" })
    } else if d >= 60.0 {
        format!("~{:.0} months", d / 30.44)
    } else {
        format!("~{d:.0} days")
    }
}

/// One `tracker_samples` row as the tracker stats table and chart read it.
fn tracker_row_json(row: &rusqlite::Row) -> serde_json::Value {
    let f = |i: usize| -> f64 { row.get(i).unwrap_or(0.0) };
    let s = |i: usize| -> String { row.get(i).unwrap_or_default() };
    let n = |i: usize| -> i64 { row.get(i).unwrap_or(0) };
    serde_json::json!({
        "ts": crate::row::num_json(f(0)),
        "engine": s(1),
        "tracker": s(2),
        "upload_rate": crate::row::num_json(f(3)),
        "download_rate": crate::row::num_json(f(4)),
        "peers": crate::row::num_json(f(5)),
        "active": crate::row::num_json(f(6)),
        "torrents": crate::row::num_json(f(7)),
        "cum_uploaded": n(8),
        "cum_downloaded": n(9),
    })
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
#[derive(Debug, Clone, Default, serde::Serialize)]
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

    /// A second, read-only handle on the same file.
    ///
    /// The records pass reads 1.7M rows and takes seconds. Running it on the
    /// writer's connection would hold that mutex for the whole scan, and the
    /// sampler writing every five seconds would queue behind it.
    pub fn open_read_only(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        Ok(Self { conn })
    }

    pub fn open_in_memory() -> anyhow::Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// One sample of a race in flight.
    ///
    /// `race_snapshots` was created, indexed, read by `snapshots_for` and
    /// written by nothing: the V4 port carried the table and the reader across
    /// and left the writer behind, so every race timeline answered with an
    /// empty graph. The events survived -- 2585 of them on the production node
    /// -- which is why the panel looked broken rather than empty.
    pub fn record_snapshot(&self, s: &RaceSnapshot) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT INTO race_snapshots (ts, info_hash, progress, upload_rate, download_rate,
                 total_upload, total_download, peers, seeds, swarm_seeds, swarm_leechers,
                 ratio, peers_json)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            rusqlite::params![
                s.ts,
                s.info_hash,
                s.progress,
                s.upload_rate,
                s.download_rate,
                s.total_upload,
                s.total_download,
                s.peers,
                s.seeds,
                s.swarm_seeds,
                s.swarm_leechers,
                s.ratio,
                s.peers_json,
            ],
        )?;
        Ok(())
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

    /// Append one performance sample.
    ///
    /// The sampler is the only writer; 3.x wrote a row every 5s and the graphs
    /// assume that spacing. The values arrive as a JSON object keyed by column
    /// name so the caller does not have to keep 28 positional arguments in the
    /// same order as the schema.
    pub fn record_sample(&self, sample: &serde_json::Value) -> anyhow::Result<()> {
        let cols: Vec<&str> = BENCH_COLUMNS.split(',').map(|c| c.trim()).collect();
        let placeholders: Vec<String> =
            (1..=cols.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "INSERT INTO bench_samples ({}) VALUES ({})",
            cols.join(", "),
            placeholders.join(", ")
        );
        let values: Vec<f64> = cols
            .iter()
            .map(|c| sample.get(*c).and_then(|v| v.as_f64()).unwrap_or(0.0))
            .collect();
        let params: Vec<&dyn rusqlite::ToSql> =
            values.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
        self.conn.execute(&sql, params.as_slice())?;
        Ok(())
    }

    /// Performance samples between two instants, oldest first.
    ///
    /// Returned as JSON objects keyed by column so the route can hand them to
    /// the graphs unchanged.
    pub fn samples_in_range(&self, start: f64, end: f64) -> anyhow::Result<Vec<serde_json::Value>> {
        let cols: Vec<&str> = BENCH_COLUMNS.split(',').map(|c| c.trim()).collect();
        let sql = format!(
            "SELECT {BENCH_COLUMNS} FROM bench_samples WHERE ts >= ?1 AND ts <= ?2 ORDER BY ts"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params![start, end], |row| {
            let mut out = serde_json::Map::new();
            for (i, name) in cols.iter().enumerate() {
                let v: f64 = row.get(i).unwrap_or(0.0);
                out.insert((*name).to_string(), crate::row::num_json(v));
            }
            Ok(serde_json::Value::Object(out))
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Append one per-tracker sample.
    pub fn record_tracker_sample(
        &self,
        ts: f64,
        engine: &str,
        tracker: &str,
        peers: f64,
        active: f64,
        torrents: f64,
        cum_uploaded: i64,
        cum_downloaded: i64,
    ) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT INTO tracker_samples
                 (ts, engine, tracker, upload_rate, download_rate, peers, active,
                  torrents, cum_uploaded, cum_downloaded)
             VALUES (?1,?2,?3,0,0,?4,?5,?6,?7,?8)",
            rusqlite::params![ts, engine, tracker, peers, active, torrents,
                              cum_uploaded, cum_downloaded],
        )?;
        Ok(())
    }

    /// The most recent sample for each tracker, for the tracker stats table.
    pub fn tracker_samples_latest(&self) -> anyhow::Result<Vec<serde_json::Value>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.ts, s.engine, s.tracker, s.upload_rate, s.download_rate, s.peers,
                    s.active, s.torrents, s.cum_uploaded, s.cum_downloaded
               FROM tracker_samples s
               JOIN (SELECT tracker, MAX(ts) AS ts FROM tracker_samples GROUP BY tracker) m
                 ON m.tracker = s.tracker AND m.ts = s.ts
              ORDER BY s.tracker",
        )?;
        let rows = stmt.query_map([], |row| Ok(tracker_row_json(row)))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// One tracker's samples between two instants, oldest first.
    pub fn tracker_samples_in_range(
        &self,
        tracker: &str,
        start: f64,
        end: f64,
    ) -> anyhow::Result<Vec<serde_json::Value>> {
        let mut stmt = self.conn.prepare(
            "SELECT ts, engine, tracker, upload_rate, download_rate, peers, active,
                    torrents, cum_uploaded, cum_downloaded
               FROM tracker_samples
              WHERE tracker = ?1 AND ts >= ?2 AND ts <= ?3
              ORDER BY ts",
        )?;
        let rows = stmt.query_map(rusqlite::params![tracker, start, end], |row| {
            Ok(tracker_row_json(row))
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Everything the Records card and the milestone list render.
    ///
    /// Mirrors what 3.x computed, including the part that is not obvious: the
    /// lifetime counter has been carried across clients and its lineage shows
    /// up as a jump of more than 100 TiB between two consecutive samples. Every
    /// figure derived from a delta is measured only after the LAST such jump,
    /// or a single lineage change would be published as the best upload day
    /// this node ever had.
    pub fn records_payload(&self) -> anyhow::Result<serde_json::Value> {
        const PIB: f64 = 1024.0 * 1024.0 * 1024.0 * 1024.0 * 1024.0;
        const TIB: f64 = 1024.0 * 1024.0 * 1024.0 * 1024.0;
        const JUMP_CAP: f64 = 100.0 * TIB;

        // The first clean sample: the one after the last lineage jump.
        let (mut t_clean, mut first_ts) = (0.0f64, 0.0f64);
        {
            let mut stmt = self
                .conn
                .prepare("SELECT ts, global_uploaded FROM bench_samples ORDER BY ts")?;
            let mut rows = stmt.query([])?;
            let mut prev: Option<f64> = None;
            let mut have_first = false;
            while let Some(row) = rows.next()? {
                let ts: f64 = row.get(0).unwrap_or(0.0);
                let v: f64 = row.get(1).unwrap_or(0.0);
                if !have_first {
                    first_ts = ts;
                    have_first = true;
                }
                if let Some(p) = prev {
                    if v - p > JUMP_CAP {
                        t_clean = ts;
                    }
                }
                prev = Some(v);
            }
        }
        if t_clean == 0.0 {
            t_clean = first_ts;
        }

        let now: f64 = self
            .conn
            .query_row("SELECT MAX(ts) FROM bench_samples", [], |r| r.get(0))
            .unwrap_or(0.0);

        let peak = |expr: &str| -> Option<(f64, f64)> {
            let sql = format!(
                "SELECT ts, ({expr}) v FROM bench_samples WHERE ({expr}) IS NOT NULL \
                 ORDER BY v DESC LIMIT 1"
            );
            self.conn
                .query_row(&sql, [], |r| Ok((r.get(0).unwrap_or(0.0), r.get(1).unwrap_or(0.0))))
                .ok()
        };
        let rec = |label: &str, value: f64, unit: &str, ts: f64, hi: bool| {
            serde_json::json!({
                "label": label,
                "value": crate::row::num_json(round2(value)),
                "unit": unit,
                "date": day_date(ts),
                "hi": hi,
            })
        };

        let mut records = Vec::new();
        if let Some((ts, v)) = peak("race_upload_rate + hoard_upload_rate") {
            records.push(rec("Peak upload", v * 8.0 / 1e9, "Gbps", ts, true));
        }
        if let Some((ts, v)) = peak("race_download_rate") {
            records.push(rec("Peak download", v * 8.0 / 1e9, "Gbps", ts, false));
        }
        if let Some((ts, v)) = peak("race_peers + hoard_peers") {
            records.push(rec("Peak swarm peers", v.round(), "", ts, false));
        }
        if let Ok((ts, delta)) = self.conn.query_row(
            "SELECT MAX(ts) ts, MAX(global_uploaded)-MIN(global_uploaded) delta \
               FROM bench_samples WHERE ts>=?1 \
              GROUP BY CAST(ts/86400 AS INT) ORDER BY delta DESC LIMIT 1",
            rusqlite::params![t_clean],
            |r| Ok((r.get::<_, f64>(0).unwrap_or(0.0), r.get::<_, f64>(1).unwrap_or(0.0))),
        ) {
            records.push(rec("Best upload day", delta / TIB, "TiB", ts, true));
        }
        if let Some((ts, v)) = peak("hoard_uploading + race_uploading") {
            records.push(rec("Peak live seeds", v.round(), "", ts, false));
        }
        if let Ok((ts, ul)) = self.conn.query_row(
            "SELECT ts, ul_mbps FROM vpn_speedtest ORDER BY ul_mbps DESC LIMIT 1",
            [],
            |r| Ok((r.get::<_, f64>(0).unwrap_or(0.0), r.get::<_, f64>(1).unwrap_or(0.0))),
        ) {
            records.push(rec("Best line test", ul / 1000.0, "Gbps", ts, false));
        }

        // Milestones. A petabyte the counter was ALREADY past when the clean
        // period opened was not witnessed here; it is marked unobserved and the
        // card credits it to the previous client rather than to Hydra.
        let g_max: f64 = self
            .conn
            .query_row("SELECT MAX(global_uploaded) FROM bench_samples", [], |r| r.get(0))
            .unwrap_or(0.0);
        let g_min_clean: f64 = self
            .conn
            .query_row(
                "SELECT MIN(global_uploaded) FROM bench_samples WHERE ts>=?1",
                rusqlite::params![t_clean],
                |r| r.get(0),
            )
            .unwrap_or(0.0);

        let mut milestones: Vec<serde_json::Value> = Vec::new();
        let mut observed_ts: Vec<Option<f64>> = Vec::new();
        let mut k = 1i64;
        while (k as f64) * PIB <= g_max {
            let thr = (k as f64) * PIB;
            let mut m = serde_json::Map::new();
            m.insert("pib".into(), k.into());
            if g_min_clean < thr {
                let mts: f64 = self
                    .conn
                    .query_row(
                        "SELECT MIN(ts) FROM bench_samples WHERE global_uploaded>=?1 AND ts>=?2",
                        rusqlite::params![thr, t_clean],
                        |r| r.get(0),
                    )
                    .unwrap_or(0.0);
                m.insert("observed".into(), true.into());
                m.insert("ts".into(), crate::row::num_json(mts));
                m.insert("date".into(), iso_date(mts).into());
                observed_ts.push(Some(mts));
            } else {
                m.insert("observed".into(), false.into());
                observed_ts.push(None);
            }
            milestones.push(serde_json::Value::Object(m));
            k += 1;
        }
        for i in 1..milestones.len() {
            if let (Some(cur), Some(prev)) = (observed_ts[i], observed_ts[i - 1]) {
                milestones[i]["since_prev"] = human_dur(cur - prev).into();
            }
        }

        // Projection from the last seven days of movement.
        let next_pib = (g_max / PIB).floor() as i64 + 1;
        let mut out = serde_json::json!({
            "records": records,
            "milestones": milestones,
            "current_pib": crate::row::num_json((g_max / PIB * 1000.0).round() / 1000.0),
            "next_pib": next_pib,
        });
        if let Ok((w0, w1, wt0, wt1)) = self.conn.query_row(
            "SELECT MIN(global_uploaded),MAX(global_uploaded),MIN(ts),MAX(ts) \
               FROM bench_samples WHERE ts>=?1",
            rusqlite::params![now - 7.0 * 86400.0],
            |r| {
                Ok((
                    r.get::<_, f64>(0).unwrap_or(0.0),
                    r.get::<_, f64>(1).unwrap_or(0.0),
                    r.get::<_, f64>(2).unwrap_or(0.0),
                    r.get::<_, f64>(3).unwrap_or(0.0),
                ))
            },
        ) {
            if wt1 > wt0 {
                let rate = (w1 - w0) / (wt1 - wt0);
                if rate > 0.0 {
                    let togo = (next_pib as f64) * PIB - g_max;
                    out["rate_tib_day"] =
                        crate::row::num_json(round2(rate * 86400.0 / TIB));
                    out["next_eta_days"] =
                        crate::row::num_json(round2(togo / rate / 86400.0));
                    out["next_eta_date"] = iso_date(now + togo / rate).into();
                }
            }
        }
        Ok(out)
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

#[cfg(test)]
mod sample_tests {
    use super::*;

    fn db() -> BenchDb {
        BenchDb::open_in_memory().expect("an in-memory bench database")
    }

    /// A sample carries whatever columns the recorder had; the ones it did not
    /// measure come back as zero rather than making the row unreadable.
    fn sample(ts: f64, race_upload_rate: f64) -> serde_json::Value {
        serde_json::json!({
            "ts": ts,
            "race_upload_rate": race_upload_rate,
            "race_peers": 10.0,
            "hoard_upload_rate": 1.0,
        })
    }

    #[test]
    fn a_sample_comes_back_out_of_the_window_it_falls_in() {
        let d = db();
        d.record_sample(&sample(100.0, 500.0)).unwrap();
        let got = d.samples_in_range(0.0, 1000.0).unwrap();
        assert_eq!(got.len(), 1);
        // `num_json` emits a whole number as an integer, so compare the VALUE
        // rather than the JSON shape: 100 and 100.0 are the same sample.
        assert_eq!(got[0]["ts"].as_f64(), Some(100.0));
        assert_eq!(got[0]["race_upload_rate"].as_f64(), Some(500.0));
    }

    /// The window is INCLUSIVE at both ends. An exclusive bound drops the
    /// sample sitting exactly on the edge, which is the one a graph is
    /// scrolled to.
    #[test]
    fn the_window_includes_both_of_its_bounds() {
        let d = db();
        d.record_sample(&sample(100.0, 1.0)).unwrap();
        d.record_sample(&sample(200.0, 2.0)).unwrap();
        assert_eq!(d.samples_in_range(100.0, 200.0).unwrap().len(), 2);
        assert_eq!(d.samples_in_range(101.0, 199.0).unwrap().len(), 0);
    }

    #[test]
    fn samples_come_back_oldest_first_so_a_graph_reads_left_to_right() {
        let d = db();
        for ts in [300.0, 100.0, 200.0] {
            d.record_sample(&sample(ts, 1.0)).unwrap();
        }
        let got = d.samples_in_range(0.0, 1000.0).unwrap();
        let order: Vec<f64> = got.iter().filter_map(|s| s["ts"].as_f64()).collect();
        assert_eq!(order, vec![100.0, 200.0, 300.0]);
    }

    /// A column the sample never carried is zero, not absent: the graph reads
    /// every key on every point.
    #[test]
    fn a_column_the_sample_never_carried_reads_as_zero() {
        let d = db();
        d.record_sample(&sample(100.0, 1.0)).unwrap();
        let got = d.samples_in_range(0.0, 1000.0).unwrap();
        assert_eq!(got[0]["global_uploaded"].as_f64(), Some(0.0));
    }

    #[test]
    fn an_empty_window_is_an_empty_list_not_an_error() {
        let d = db();
        assert!(d.samples_in_range(0.0, 10.0).unwrap().is_empty());
    }

    /// The timeline is observability: losing it must never cost the seedbox,
    /// so a window that is backwards answers empty rather than failing.
    #[test]
    fn a_backwards_window_answers_empty_rather_than_failing() {
        let d = db();
        d.record_sample(&sample(100.0, 1.0)).unwrap();
        assert!(d.samples_in_range(500.0, 10.0).unwrap().is_empty());
    }

    #[test]
    fn a_tracker_sample_round_trips() {
        let d = db();
        d.record_tracker_sample(100.0, "race", "tracker.example", 5.0, 3.0, 40.0, 900, 100)
            .unwrap();
        let got = d.tracker_samples_in_range("tracker.example", 0.0, 1000.0).unwrap();
        assert_eq!(got.len(), 1, "got {got:?}");
    }

    /// ⭐ Two engines announcing to the SAME tracker are two series. Collapsing
    /// them would credit one engine's peers to the other.
    #[test]
    fn two_engines_on_one_tracker_stay_two_series() {
        let d = db();
        d.record_tracker_sample(100.0, "race", "tracker.example", 5.0, 3.0, 40.0, 900, 100)
            .unwrap();
        d.record_tracker_sample(100.0, "hoard", "tracker.example", 7.0, 4.0, 50.0, 800, 200)
            .unwrap();
        let got = d.tracker_samples_in_range("tracker.example", 0.0, 1000.0).unwrap();
        assert_eq!(got.len(), 2, "got {got:?}");
    }

    #[test]
    fn the_latest_tracker_samples_are_empty_on_a_fresh_database() {
        let d = db();
        assert!(d.tracker_samples_latest().unwrap().is_empty());
    }

    /// The Records card must render on a library that has done nothing yet.
    #[test]
    fn the_records_payload_answers_on_an_empty_database() {
        let d = db();
        let payload = d.records_payload().expect("an empty database still has a payload");
        assert!(payload.is_object(), "got {payload}");
    }

    /// A read-only handle is how a refresh opens the file beside the writer.
    /// Opening one on a path that does not exist is an error, not a panic and
    /// not a silently created database.
    #[test]
    fn a_read_only_handle_on_a_missing_file_is_an_error() {
        let missing = std::env::temp_dir().join("typhon-no-such-bench-4a1f.db");
        let _ = std::fs::remove_file(&missing);
        assert!(BenchDb::open_read_only(&missing).is_err());
        assert!(!missing.exists(), "a read-only open must not create the file");
    }

    /// Opening for writing creates the file and its schema.
    #[test]
    fn opening_for_writing_creates_the_database() {
        let path = std::env::temp_dir().join(format!(
            "typhon-bench-{}-{:?}.db",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);
        {
            let d = BenchDb::open(&path).expect("open creates");
            d.record_sample(&sample(1.0, 1.0)).unwrap();
        }
        assert!(path.exists());
        let ro = BenchDb::open_read_only(&path).expect("now it can be read");
        assert_eq!(ro.samples_in_range(0.0, 10.0).unwrap().len(), 1);
        let _ = std::fs::remove_file(&path);
    }
}
