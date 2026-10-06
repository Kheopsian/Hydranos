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
    hoard_announce_fail_rate REAL DEFAULT 0,
    race_announce_needed REAL DEFAULT 0,
    race_announce_late REAL DEFAULT 0,
    race_announce_lag_p50 REAL DEFAULT 0,
    race_announce_lag_p90 REAL DEFAULT 0,
    race_announce_concurrency REAL DEFAULT 0,
    race_announce_latency_ms REAL DEFAULT 0,
    race_announce_throttled_pct REAL DEFAULT 0,
    race_announce_in_flight REAL DEFAULT 0,
    hoard_announce_needed REAL DEFAULT 0,
    hoard_announce_late REAL DEFAULT 0,
    hoard_announce_lag_p50 REAL DEFAULT 0,
    hoard_announce_lag_p90 REAL DEFAULT 0,
    hoard_announce_concurrency REAL DEFAULT 0,
    hoard_announce_latency_ms REAL DEFAULT 0,
    hoard_announce_throttled_pct REAL DEFAULT 0,
    hoard_announce_in_flight REAL DEFAULT 0,
    hoard_download_rate REAL DEFAULT 0,
    extra_upload_rate REAL DEFAULT 0,
    extra_download_rate REAL DEFAULT 0,
    extra_peers REAL DEFAULT 0,
    extra_uploading REAL DEFAULT 0);
CREATE INDEX IF NOT EXISTS idx_bench_ts ON bench_samples(ts);
CREATE TABLE IF NOT EXISTS engine_samples (
    ts REAL NOT NULL, engine TEXT NOT NULL,
    upload_rate REAL DEFAULT 0, download_rate REAL DEFAULT 0,
    peers REAL DEFAULT 0, uploading REAL DEFAULT 0, torrents REAL DEFAULT 0);
CREATE INDEX IF NOT EXISTS idx_engine_samples_ts ON engine_samples(ts);
CREATE TABLE IF NOT EXISTS tracker_samples (
    ts REAL NOT NULL, engine TEXT NOT NULL, tracker TEXT NOT NULL,
    upload_rate REAL DEFAULT 0, download_rate REAL DEFAULT 0,
    peers REAL DEFAULT 0, active REAL DEFAULT 0, torrents REAL DEFAULT 0,
    cum_uploaded INTEGER DEFAULT 0, cum_downloaded INTEGER DEFAULT 0);
CREATE INDEX IF NOT EXISTS idx_tracker_samples_ts ON tracker_samples(ts);
CREATE INDEX IF NOT EXISTS idx_tracker_samples_trk_ts ON tracker_samples(tracker, ts);
DROP INDEX IF EXISTS idx_tracker_samples_trk;
CREATE TABLE IF NOT EXISTS bench_meta (key TEXT PRIMARY KEY, value REAL);
";

/// Tracker samples younger than this stay as recorded; older ones are folded
/// into one row per `TRACKER_BUCKET_SECS` per (engine, tracker).
pub const TRACKER_RAW_KEEP_SECS: f64 = 48.0 * 3600.0;
pub const TRACKER_BUCKET_SECS: f64 = 300.0;
/// History folded per call. Six hours of 30 s samples on ~20 series is ~15k
/// rows; the first pass on a node with months of 5 s samples reads ~75k.
const TRACKER_COMPACT_SLICE_SECS: f64 = 6.0 * 3600.0;
const TRACKER_COMPACTED_KEY: &str = "tracker_samples_compacted_until";

/// The points a tracker chart is drawn from, at most: a wider window is
/// averaged into buckets rather than shipped whole (17k rows and 3 MB for one
/// day of one tracker at 5 s).
pub const TRACKER_CHART_POINTS: f64 = 300.0;

/// The bucket a tracker window is averaged over.
///
/// Whole multiples of 30 s, the sampling interval; and whole multiples of the
/// compacted bucket once the window reaches into compacted history, or one
/// bucket in two would be empty there.
pub fn tracker_bucket_secs(start: f64, end: f64, now: f64) -> f64 {
    let span = (end - start).max(0.0);
    let mut b = ((span / TRACKER_CHART_POINTS) / 30.0).ceil().max(1.0) * 30.0;
    if start < now - TRACKER_RAW_KEEP_SECS {
        b = (b / TRACKER_BUCKET_SECS).ceil().max(1.0) * TRACKER_BUCKET_SECS;
    }
    b
}

const TRACKER_RANGE_SQL: &str = "
    SELECT CAST(ts / ?4 AS INTEGER) * ?4 AS b, engine, tracker,
           AVG(upload_rate), AVG(download_rate), AVG(peers), AVG(active),
           AVG(torrents), MAX(cum_uploaded), MAX(cum_downloaded)
      FROM tracker_samples
     WHERE tracker = ?1 AND ts >= ?2 AND ts <= ?3
     GROUP BY b, engine
     ORDER BY b, engine";

/// The columns of `bench_samples`, in the order the graphs read them.
///
/// Written out rather than `SELECT *` so a future migration adding a column
/// cannot silently shift what each position means.
/// Window a rate record is averaged over, and the samples it needs to count.
/// The sampler ticks every 5 s, so a full minute holds twelve.
const RATE_WINDOW_SECS: i64 = 60;
const RATE_WINDOW_MIN_SAMPLES: i64 = 6;

pub const BENCH_COLUMNS: &str = "ts, race_upload_rate, race_download_rate, race_peers, \
     race_torrents, hoard_upload_rate, hoard_peers, hoard_active, hoard_with_peers, \
     hoard_uploading, iowait_pct, arc_size_bytes, arc_hit_rate_pct, \
     arc_demand_hit_rate_pct, arc_miss_per_sec, arc_demand_miss_per_sec, \
     arc_ghost_hits_per_sec, race_uploading, race_avg_share, open_fds, \
     hoard_session_uploaded, race_session_uploaded, global_uploaded, \
     global_downloaded, race_announce_rate, hoard_announce_rate, \
     race_announce_fail_rate, hoard_announce_fail_rate, \
     race_announce_needed, race_announce_late, race_announce_lag_p50, race_announce_lag_p90, race_announce_concurrency, race_announce_latency_ms, \
     hoard_announce_needed, hoard_announce_late, hoard_announce_lag_p50, hoard_announce_lag_p90, hoard_announce_concurrency, hoard_announce_latency_ms, \
     race_announce_throttled_pct, hoard_announce_throttled_pct, \
     race_announce_in_flight, hoard_announce_in_flight, \
     hoard_download_rate, extra_upload_rate, extra_download_rate, extra_peers, extra_uploading";

/// Columns added to `bench_samples` after databases already existed in the
/// field. `CREATE TABLE IF NOT EXISTS` does not touch a table that is there,
/// so an existing bench.db would lack them and every insert naming them would
/// fail -- the sampler would stop recording anything at all.
const ADDED_COLUMNS: &[&str] = &["race_announce_needed", "race_announce_late", "race_announce_lag_p50", "race_announce_lag_p90", "race_announce_concurrency", "race_announce_latency_ms", "race_announce_throttled_pct", "race_announce_in_flight", "hoard_announce_needed", "hoard_announce_late", "hoard_announce_lag_p50", "hoard_announce_lag_p90", "hoard_announce_concurrency", "hoard_announce_latency_ms", "hoard_announce_throttled_pct", "hoard_announce_in_flight",
    // 4.4: the engines beyond race and hoard, summed, and hoard's download.
    "hoard_download_rate", "extra_upload_rate", "extra_download_rate", "extra_peers", "extra_uploading"];

fn add_missing_columns(conn: &Connection) -> anyhow::Result<()> {
    let have: std::collections::HashSet<String> = {
        let mut stmt = conn.prepare("PRAGMA table_info(bench_samples)")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
        rows.filter_map(|r| r.ok()).collect()
    };
    for c in ADDED_COLUMNS {
        if !have.contains(*c) {
            conn.execute_batch(&format!("ALTER TABLE bench_samples ADD COLUMN {c} REAL DEFAULT 0"))?;
        }
    }
    Ok(())
}

/// One extra engine's figures at one sample.
#[derive(Debug, Clone, Default)]
pub struct EngineSample {
    pub engine: String,
    pub upload_rate: f64,
    pub download_rate: f64,
    pub peers: f64,
    pub uploading: f64,
    pub torrents: f64,
}

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
    /// What was true of each torrent last tick.
    seen: std::collections::HashMap<String, Seen>,
    /// When this process started watching. Anything already complete at its
    /// first sighting predates us.
    started_at: f64,
}

/// The little that has to be remembered between two ticks.
///
/// `first_peer` and `first_upload` are firsts: without a flag they would fire
/// on every tick for the whole life of the torrent.
#[derive(Debug, Clone, Copy, Default)]
struct Seen {
    progress: f64,
    had_peer: bool,
    had_upload: bool,
    /// The announce cache's stamp for this torrent last tick. It is set by
    /// `cache.record()` on every SUCCESSFUL announce and nowhere else, so a new
    /// value is an announce that happened -- not a guess from swarm counts,
    /// which can answer the same numbers twice.
    last_announce: Option<std::time::Instant>,
}

impl RaceRecorder {
    pub fn new(started_at: f64) -> Self {
        Self { seen: std::collections::HashMap::new(), started_at }
    }

    /// One sighting. Returns every moment this tick crossed.
    ///
    /// Several can land on the same tick -- a torrent that finds its first peer
    /// and its first byte of upload between two looks -- so this answers a list.
    /// Each entry is (event, ts, download_time).
    pub fn sight(
        &mut self,
        info_hash: &str,
        added_time: f64,
        progress: f64,
        peers: i64,
        upload_total: i64,
        announced_at: Option<std::time::Instant>,
        now: f64,
    ) -> Vec<(String, f64, f64)> {
        let complete = progress >= 1.0;
        let mut out = Vec::new();
        let previous = self.seen.get(info_hash).copied();
        let mut state = previous.unwrap_or(Seen { progress, ..Default::default() });

        match previous {
            None => {
                // First sighting. A torrent added before we started is not news
                // -- it would date every old torrent to this boot.
                if added_time >= self.started_at && !complete {
                    out.push(("added".to_string(), now, 0.0));
                }
            }
            Some(prev) => {
                if complete && prev.progress < 1.0 {
                    let download_time = if added_time > 0.0 { now - added_time } else { 0.0 };
                    out.push(("completed".to_string(), now, download_time));
                }
            }
        }

        // Firsts, reported once each. Only for torrents this process saw
        // arrive: an old torrent meeting its first peer of the day is not the
        // first peer of the race, and dating it here would be a lie.
        let witnessed = added_time >= self.started_at;
        if witnessed && !state.had_peer && peers > 0 {
            out.push(("first_peer".to_string(), now, 0.0));
        }
        if witnessed && !state.had_upload && upload_total > 0 {
            out.push(("first_upload".to_string(), now, 0.0));
        }

        // An announce is a moment for any race, old or new: unlike the firsts
        // above it says what the tracker answered just now, which is the whole
        // point of having it on the timeline.
        if let Some(at) = announced_at {
            let is_new = match state.last_announce {
                None => previous.is_some(),
                Some(before) => at > before,
            };
            if is_new {
                out.push(("announce".to_string(), now, 0.0));
            }
            state.last_announce = Some(at);
        }

        state.progress = progress;
        state.had_peer |= peers > 0;
        state.had_upload |= upload_total > 0;
        self.seen.insert(info_hash.to_string(), state);
        out
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

/// Put bench.db in WAL mode, unless it lives on a network share.
///
/// In the rollback journal a reader holds the whole file: the records pass and
/// the chart reads, on their own read-only handles, made the sampler's commit
/// wait behind them -- with the writer's mutex held, so every other reader of
/// that mutex queued too. Same reasoning and same exception as the store, cf
/// `Store::prefer_wal`.
fn prefer_wal(conn: &Connection, path: &Path) {
    if path.parent().map_or(false, crate::platform::is_network_fs) {
        return;
    }
    match conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get::<_, String>(0)) {
        Ok(mode) if mode.eq_ignore_ascii_case("wal") => {
            if let Err(e) = conn.execute_batch("PRAGMA synchronous=NORMAL;") {
                tracing::warn!("bench.db in WAL but not tuned: {e}");
            }
        }
        Ok(mode) => tracing::warn!(mode = %mode, "bench.db stays in its journal mode"),
        Err(e) => tracing::warn!("bench.db stays in its journal mode: {e}"),
    }
}

impl BenchDb {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )?;
        prefer_wal(&conn, path);
        conn.execute_batch(SCHEMA)?;
        add_missing_columns(&conn)?;
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

    /// One row per engine beyond `race` and `hoard`, beside the sample.
    ///
    /// Their own table rather than columns: `bench_samples` has a column per
    /// figure per engine NAME, and an `[[engine]]` block's name is whatever
    /// the operator chose.
    pub fn record_engine_samples(&self, ts: f64, rows: &[EngineSample]) -> anyhow::Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO engine_samples (ts, engine, upload_rate, download_rate, peers, uploading, torrents) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for r in rows {
                stmt.execute(rusqlite::params![ts, r.engine, r.upload_rate, r.download_rate, r.peers, r.uploading, r.torrents])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Every extra engine's rows between two instants, averaged per `bucket`
    /// seconds, oldest first: engine -> rows.
    pub fn engine_samples_in_range(
        &self,
        start: f64,
        end: f64,
        bucket: f64,
    ) -> anyhow::Result<std::collections::BTreeMap<String, Vec<serde_json::Value>>> {
        let mut stmt = self.conn.prepare(
            "SELECT engine, CAST(ts / ?3 AS INTEGER) * ?3 AS b, AVG(upload_rate), AVG(download_rate), \
                    AVG(peers), AVG(uploading), AVG(torrents) \
               FROM engine_samples WHERE ts >= ?1 AND ts <= ?2 \
              GROUP BY engine, b ORDER BY engine, b",
        )?;
        let rows = stmt.query_map(rusqlite::params![start, end, bucket.max(1.0)], |r| {
            let engine: String = r.get(0)?;
            let n = |i: usize| crate::row::num_json(r.get::<_, f64>(i).unwrap_or(0.0));
            Ok((
                engine,
                serde_json::json!({
                    "ts": n(1), "upload_rate": n(2), "download_rate": n(3),
                    "peers": n(4), "uploading": n(5), "torrents": n(6),
                }),
            ))
        })?;
        let mut out: std::collections::BTreeMap<String, Vec<serde_json::Value>> = Default::default();
        for (engine, row) in rows.filter_map(|r| r.ok()) {
            out.entry(engine).or_default().push(row);
        }
        Ok(out)
    }

    /// Append one tracker pass, in one transaction.
    ///
    /// One commit per row was ~20 fsyncs every pass in the rollback journal,
    /// each taken with the writer's mutex held.
    pub fn record_tracker_samples(
        &self,
        ts: f64,
        rows: &[crate::benchsampler::TrackerRow],
    ) -> anyhow::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO tracker_samples
                     (ts, engine, tracker, upload_rate, download_rate, peers, active,
                      torrents, cum_uploaded, cum_downloaded)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            )?;
            for r in rows {
                stmt.execute(rusqlite::params![
                    ts, r.engine, r.tracker, r.upload_rate as f64, r.download_rate as f64,
                    r.peers as f64, r.active as f64, r.torrents as f64,
                    r.cum_uploaded, r.cum_downloaded,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Fold one slice of tracker history older than `TRACKER_RAW_KEEP_SECS`
    /// into 5-minute rows. Returns whether older history is still waiting.
    ///
    /// Gauges are averaged over the bucket, cumulative counters take its
    /// maximum (they only grow), and the row is stamped at the bucket start.
    /// Everything before the stored watermark is already folded, so a slice is
    /// never folded twice, and the swap is one transaction: a crash leaves
    /// either the raw rows or their fold, never both and never neither.
    ///
    /// Bounded to one slice per call because the first pass after upgrading
    /// meets two months of 5 s samples (14M rows on the production node), and
    /// this runs with the writer's mutex held.
    pub fn compact_tracker_samples(&self, now: f64) -> anyhow::Result<bool> {
        let cutoff = ((now - TRACKER_RAW_KEEP_SECS) / TRACKER_BUCKET_SECS).floor()
            * TRACKER_BUCKET_SECS;
        let mark: Option<f64> = self
            .conn
            .query_row(
                "SELECT value FROM bench_meta WHERE key = ?1",
                [TRACKER_COMPACTED_KEY],
                |r| r.get(0),
            )
            .ok();
        let from = match mark {
            Some(m) => m,
            None => {
                let oldest: Option<f64> = self
                    .conn
                    .query_row("SELECT MIN(ts) FROM tracker_samples", [], |r| r.get(0))?;
                match oldest {
                    Some(t) => (t / TRACKER_BUCKET_SECS).floor() * TRACKER_BUCKET_SECS,
                    None => cutoff,
                }
            }
        };
        if from >= cutoff {
            if mark.is_none() {
                self.set_compacted_until(&self.conn, cutoff)?;
            }
            return Ok(false);
        }
        let to = (from + TRACKER_COMPACT_SLICE_SECS).min(cutoff);

        let tx = self.conn.unchecked_transaction()?;
        let last_raw: i64 =
            tx.query_row("SELECT IFNULL(MAX(rowid), 0) FROM tracker_samples", [], |r| r.get(0))?;
        tx.execute(
            "INSERT INTO tracker_samples
                 (ts, engine, tracker, upload_rate, download_rate, peers, active,
                  torrents, cum_uploaded, cum_downloaded)
             SELECT CAST(ts / ?3 AS INTEGER) * ?3, engine, tracker,
                    AVG(upload_rate), AVG(download_rate), AVG(peers), AVG(active),
                    AVG(torrents), MAX(cum_uploaded), MAX(cum_downloaded)
               FROM tracker_samples
              WHERE ts >= ?1 AND ts < ?2
              GROUP BY 1, engine, tracker",
            rusqlite::params![from, to, TRACKER_BUCKET_SECS],
        )?;
        tx.execute(
            "DELETE FROM tracker_samples WHERE ts >= ?1 AND ts < ?2 AND rowid <= ?3",
            rusqlite::params![from, to, last_raw],
        )?;
        self.set_compacted_until(&tx, to)?;
        tx.commit()?;
        Ok(to < cutoff)
    }

    fn set_compacted_until(&self, conn: &Connection, ts: f64) -> anyhow::Result<()> {
        conn.execute(
            "INSERT INTO bench_meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![TRACKER_COMPACTED_KEY, ts],
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

    /// One tracker's samples between two instants, averaged per `bucket`
    /// seconds and stamped at the bucket start, oldest first.
    ///
    /// Served by the `(tracker, ts)` index. With `tracker` alone the window was
    /// filtered row by row over the tracker's whole history: 1.5M rows read to
    /// return the 17k of a day on the production node, 2 s warm and 13 s cold.
    pub fn tracker_samples_in_range(
        &self,
        tracker: &str,
        start: f64,
        end: f64,
        bucket: f64,
    ) -> anyhow::Result<Vec<serde_json::Value>> {
        let mut stmt = self.conn.prepare(TRACKER_RANGE_SQL)?;
        let rows = stmt.query_map(rusqlite::params![tracker, start, end, bucket.max(1.0)], |row| {
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
        // A rate record is held over a minute, not read off one sample. The
        // engine counts a byte when it hands it to the kernel, and the send
        // buffers of thousands of sockets absorb a burst for a few seconds: on
        // single 5 s samples upload once peaked at 9.28 Gbps on an 8 Gbps line,
        // a rate no wire carried. The best minute of that same week was 7.86.
        // A minute needs half of its samples, so a lone sample cannot stand in
        // for one.
        let sustained = |expr: &str| -> Option<(f64, f64)> {
            let sql = format!(
                "SELECT MIN(ts), AVG({expr}) v FROM bench_samples WHERE ({expr}) IS NOT NULL \
                 GROUP BY CAST(ts / {RATE_WINDOW_SECS} AS INTEGER) \
                 HAVING COUNT(*) >= {RATE_WINDOW_MIN_SAMPLES} ORDER BY v DESC LIMIT 1"
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
        if let Some((ts, v)) = sustained("race_upload_rate + hoard_upload_rate + extra_upload_rate") {
            records.push(rec("Peak upload", v * 8.0 / 1e9, "Gbps", ts, true));
        }
        if let Some((ts, v)) = sustained("race_download_rate + hoard_download_rate + extra_download_rate") {
            records.push(rec("Peak download", v * 8.0 / 1e9, "Gbps", ts, false));
        }
        if let Some((ts, v)) = peak("race_peers + hoard_peers + extra_peers") {
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

    /// #92: an `[[engine]]` block is charted from its own rows, and counted
    /// in the Records through the `extra_*` sums.
    #[test]
    fn extra_engines_are_recorded_and_counted_in_the_records() {
        let db = BenchDb::open_in_memory().unwrap();
        let row = |e: &str, up: f64| EngineSample { engine: e.into(), upload_rate: up, peers: 3.0, torrents: 1.0, ..Default::default() };
        for i in 0..12 {
            let ts = 1_000.0 + i as f64 * 5.0;
            db.record_sample(&serde_json::json!({
                "ts": ts, "race_upload_rate": 10.0, "hoard_upload_rate": 10.0,
                "extra_upload_rate": 1_000.0, "extra_peers": 6.0,
            }))
            .unwrap();
            db.record_engine_samples(ts, &[row("vpn1", 600.0), row("vpn2", 400.0)]).unwrap();
        }
        let by_engine = db.engine_samples_in_range(0.0, 5_000.0, 60.0).unwrap();
        assert_eq!(by_engine.keys().collect::<Vec<_>>(), ["vpn1", "vpn2"]);
        assert_eq!(by_engine["vpn1"][0]["upload_rate"], 600.0);

        let rec = db.records_payload().unwrap();
        let peak = rec["records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["label"] == "Peak upload")
            .unwrap()["value"]
            .as_f64()
            .unwrap();
        // (10 + 10 + 1000) B/s in Gbps, rounded to two places by the payload.
        assert!((peak - (1020.0 * 8.0 / 1e9)).abs() < 0.01, "{peak}");
        let peers = rec["records"].as_array().unwrap().iter().find(|r| r["label"] == "Peak swarm peers").unwrap()["value"].clone();
        assert_eq!(peers, 6.0, "race_peers + hoard_peers + extra_peers");
    }

    /// A bench.db written by an older release lacks the announce-health
    /// columns. Opening it must add them, or the first insert fails and the
    /// sampler records nothing from then on.
    #[test]
    fn an_old_database_gains_the_new_columns() {
        let dir = std::env::temp_dir().join(format!("hydra-benchdb-migrate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bench.db");
        {
            let c = Connection::open(&path).unwrap();
            // The table exactly as 4.2 created it: every column but the new ones.
            let old: Vec<String> = BENCH_COLUMNS
                .split(',')
                .map(|c| c.trim())
                .filter(|c| !ADDED_COLUMNS.contains(c))
                .map(|c| if c == "ts" { "ts REAL NOT NULL".to_string() } else { format!("{c} REAL DEFAULT 0") })
                .collect();
            c.execute_batch(&format!("CREATE TABLE bench_samples ({})", old.join(", "))).unwrap();
        }
        let db = BenchDb::open(&path).expect("opens and migrates");
        db.record_sample(&serde_json::json!({"ts": 1.0, "hoard_announce_late": 42.0})).expect("insert with new columns");
        let v = db.samples_in_range(0.0, 2.0).unwrap();
        assert_eq!(v[0]["hoard_announce_late"], 42);
        std::fs::remove_dir_all(&dir).ok();
    }

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
mod race_recorder_tests {
    use super::*;
    use std::time::{Duration, Instant};

    const H: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const START: f64 = 1_000.0;

    fn kinds(v: &[(String, f64, f64)]) -> Vec<&str> {
        v.iter().map(|(k, _, _)| k.as_str()).collect()
    }

    /// The module header claimed these decisions were unit-tested. They were
    /// not: `RaceRecorder::new` appeared in no test until 2026-09-16.
    #[test]
    fn a_torrent_added_after_we_started_is_announced_as_added() {
        let mut r = RaceRecorder::new(START);
        let out = r.sight(H, START + 10.0, 0.0, 0, 0, None, START + 11.0);
        assert_eq!(kinds(&out), ["added"]);
    }

    /// A torrent that predates this process must not be dated to this boot.
    #[test]
    fn an_older_torrent_is_not_news() {
        let mut r = RaceRecorder::new(START);
        let out = r.sight(H, START - 500.0, 0.3, 4, 99, None, START + 1.0);
        assert!(out.is_empty(), "got {:?}", kinds(&out));
    }

    #[test]
    fn crossing_into_complete_reports_the_download_time() {
        let mut r = RaceRecorder::new(START);
        r.sight(H, START + 10.0, 0.5, 1, 0, None, START + 11.0);
        let out = r.sight(H, START + 10.0, 1.0, 1, 0, None, START + 70.0);
        assert_eq!(kinds(&out), ["completed"]);
        assert_eq!(out[0].2, 60.0, "now - added_time");
    }

    /// Already complete when first seen: we did not witness it finishing.
    #[test]
    fn a_torrent_already_complete_publishes_no_completion() {
        let mut r = RaceRecorder::new(START);
        let out = r.sight(H, START + 10.0, 1.0, 0, 0, None, START + 11.0);
        assert!(out.is_empty(), "got {:?}", kinds(&out));
    }

    #[test]
    fn the_first_peer_and_the_first_upload_fire_once_each() {
        let mut r = RaceRecorder::new(START);
        assert_eq!(kinds(&r.sight(H, START + 1.0, 0.0, 0, 0, None, START + 2.0)), ["added"]);
        assert_eq!(kinds(&r.sight(H, START + 1.0, 0.1, 3, 0, None, START + 7.0)), ["first_peer"]);
        // Still peers, still no upload: nothing new to say.
        assert!(r.sight(H, START + 1.0, 0.2, 5, 0, None, START + 12.0).is_empty());
        assert_eq!(kinds(&r.sight(H, START + 1.0, 0.3, 5, 128, None, START + 17.0)), ["first_upload"]);
        assert!(r.sight(H, START + 1.0, 0.4, 5, 900, None, START + 22.0).is_empty());
    }

    /// Both firsts can land between two looks. The tick answers a list, not one.
    #[test]
    fn one_tick_can_cross_several_moments() {
        let mut r = RaceRecorder::new(START);
        r.sight(H, START + 1.0, 0.0, 0, 0, None, START + 2.0);
        let out = r.sight(H, START + 1.0, 1.0, 6, 4096, None, START + 7.0);
        assert_eq!(kinds(&out), ["completed", "first_peer", "first_upload"]);
    }

    /// The announce stamp is set by the runner on every SUCCESSFUL announce.
    /// A new value is an announce; the same value is the same announce.
    #[test]
    fn an_announce_is_reported_when_its_stamp_moves() {
        let mut r = RaceRecorder::new(START);
        let t1 = Instant::now();
        // First sighting: the stamp is only remembered, never reported -- we
        // cannot tell a fresh announce from one that happened before we looked.
        assert_eq!(kinds(&r.sight(H, START + 1.0, 0.0, 0, 0, Some(t1), START + 2.0)), ["added"]);
        assert!(r.sight(H, START + 1.0, 0.1, 0, 0, Some(t1), START + 7.0).is_empty());

        let t2 = t1 + Duration::from_secs(1800);
        assert_eq!(kinds(&r.sight(H, START + 1.0, 0.2, 0, 0, Some(t2), START + 12.0)), ["announce"]);
        assert!(r.sight(H, START + 1.0, 0.3, 0, 0, Some(t2), START + 17.0).is_empty());
    }

    /// An old torrent still reports its announces: unlike the firsts, an
    /// announce says what the tracker answered just now.
    #[test]
    fn an_older_torrent_still_reports_announces() {
        let mut r = RaceRecorder::new(START);
        let t1 = Instant::now();
        assert!(r.sight(H, START - 500.0, 0.9, 2, 1, Some(t1), START + 2.0).is_empty());
        let t2 = t1 + Duration::from_secs(60);
        assert_eq!(kinds(&r.sight(H, START - 500.0, 0.9, 2, 1, Some(t2), START + 7.0)), ["announce"]);
    }

    #[test]
    fn pruning_forgets_torrents_that_left() {
        let mut r = RaceRecorder::new(START);
        r.sight(H, START + 1.0, 0.5, 1, 1, None, START + 2.0);
        r.prune(&std::collections::HashSet::new());
        // Forgotten, so the next sighting is a first one again -- and that means
        // the firsts fire again too, which is right: as far as the recorder can
        // tell, this is a torrent arriving.
        assert_eq!(
            kinds(&r.sight(H, START + 1.0, 0.5, 1, 1, None, START + 9.0)),
            ["added", "first_peer", "first_upload"]
        );
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

    fn trow(engine: &str, tracker: &str, upload_rate: i64, cum_uploaded: i64) -> crate::benchsampler::TrackerRow {
        crate::benchsampler::TrackerRow {
            engine: engine.into(),
            tracker: tracker.into(),
            upload_rate,
            peers: 5,
            active: 3,
            torrents: 40,
            cum_uploaded,
            cum_downloaded: 100,
            ..Default::default()
        }
    }

    #[test]
    fn a_tracker_sample_round_trips() {
        let d = db();
        d.record_tracker_samples(100.0, &[trow("race", "tracker.example", 700, 900)]).unwrap();
        let got = d.tracker_samples_in_range("tracker.example", 0.0, 1000.0, 30.0).unwrap();
        assert_eq!(got.len(), 1, "got {got:?}");
        assert_eq!(got[0]["upload_rate"], 700, "the rate is stored, not a constant 0: {got:?}");
        assert_eq!(got[0]["cum_uploaded"], 900);
        assert_eq!(got[0]["peers"], 5);
    }

    /// ⭐ Two engines announcing to the SAME tracker are two series. Collapsing
    /// them would credit one engine's peers to the other.
    #[test]
    fn two_engines_on_one_tracker_stay_two_series() {
        let d = db();
        d.record_tracker_samples(
            100.0,
            &[trow("race", "tracker.example", 1, 900), trow("hoard", "tracker.example", 2, 800)],
        )
        .unwrap();
        let got = d.tracker_samples_in_range("tracker.example", 0.0, 1000.0, 30.0).unwrap();
        assert_eq!(got.len(), 2, "got {got:?}");
    }

    /// A day of one tracker is 300 points, not the 17k rows it was recorded
    /// as: gauges averaged over the bucket, counters at their highest.
    #[test]
    fn a_window_is_averaged_into_buckets_stamped_at_their_start() {
        let d = db();
        for (i, rate) in [100, 200, 300, 400].iter().enumerate() {
            d.record_tracker_samples(600.0 + 30.0 * i as f64, &[trow("hoard", "t.example", *rate, 1000 + i as i64)])
                .unwrap();
        }
        let got = d.tracker_samples_in_range("t.example", 0.0, 10_000.0, 60.0).unwrap();
        assert_eq!(got.len(), 2, "four 30 s samples in two 60 s buckets: {got:?}");
        assert_eq!(got[0]["ts"], 600);
        assert_eq!(got[0]["upload_rate"], 150);
        assert_eq!(got[0]["cum_uploaded"], 1001);
        assert_eq!(got[1]["ts"], 660);
        assert_eq!(got[1]["upload_rate"], 350);
    }

    #[test]
    fn a_chart_asks_for_a_bounded_number_of_points() {
        let now = 10_000_000.0;
        // One hour of recent samples: every 30 s sample, nothing averaged.
        assert_eq!(super::tracker_bucket_secs(now - 3600.0, now, now), 30.0);
        // One day: ~300 points.
        let day = super::tracker_bucket_secs(now - 86_400.0, now, now);
        assert_eq!(day, 300.0);
        assert!(86_400.0 / day <= super::TRACKER_CHART_POINTS);
        // Reaching into compacted history: a whole number of 5-minute rows per
        // bucket, or one bucket in two would be empty.
        let back = super::tracker_bucket_secs(now - 3.0 * 86_400.0, now - 2.9 * 86_400.0, now);
        assert_eq!(back % super::TRACKER_BUCKET_SECS, 0.0, "got {back}");
    }

    /// History past the raw window is folded to 5-minute rows, the recent
    /// samples are left alone, and a second pass changes nothing.
    #[test]
    fn old_tracker_history_is_folded_once_and_recent_samples_kept() {
        let d = db();
        let now = 1_000_000.0 * super::TRACKER_BUCKET_SECS;
        let old = now - super::TRACKER_RAW_KEEP_SECS - 3600.0;
        // Ten 30 s samples in one old 5-minute bucket, rising counters.
        for i in 0..10 {
            d.record_tracker_samples(old + 30.0 * i as f64, &[trow("hoard", "t.example", 100 * i, 1000 + i)])
                .unwrap();
        }
        d.record_tracker_samples(now - 60.0, &[trow("hoard", "t.example", 7, 5000)]).unwrap();

        while d.compact_tracker_samples(now).unwrap() {}
        let count = |d: &BenchDb| -> i64 {
            d.conn.query_row("SELECT COUNT(*) FROM tracker_samples", [], |r| r.get(0)).unwrap()
        };
        assert_eq!(count(&d), 2, "one folded row and the recent sample");
        let got = d.tracker_samples_in_range("t.example", 0.0, now, 1.0).unwrap();
        assert_eq!(got[0]["ts"], old as i64, "stamped at the bucket start");
        assert_eq!(got[0]["upload_rate"], 450, "the mean of 0..900");
        assert_eq!(got[0]["cum_uploaded"], 1009, "the counter's highest, never an average");
        assert_eq!(got[1]["upload_rate"], 7, "recent samples stay as recorded");

        // Nothing left to fold: another pass is a no-op.
        assert!(!d.compact_tracker_samples(now).unwrap());
        assert_eq!(count(&d), 2);
        // An hour later, the old bucket is not folded a second time.
        assert!(!d.compact_tracker_samples(now + 3600.0).unwrap());
        assert_eq!(count(&d), 2);
    }

    /// Two months of history are folded a slice per call, so no single call
    /// holds the writer for the whole catch-up.
    #[test]
    fn a_long_backlog_is_folded_a_slice_at_a_time() {
        let d = db();
        let now = 1_000_000.0 * super::TRACKER_BUCKET_SECS;
        let start = now - 30.0 * 86_400.0;
        let mut t = start;
        while t < now - super::TRACKER_RAW_KEEP_SECS {
            d.record_tracker_samples(t, &[trow("hoard", "t.example", 1, 1)]).unwrap();
            t += 3600.0;
        }
        let mut calls = 0;
        while d.compact_tracker_samples(now).unwrap() {
            calls += 1;
        }
        assert!(calls > 50, "28 days in 6 h slices is over a hundred calls, got {calls}");
    }

    #[test]
    fn the_latest_tracker_samples_are_empty_on_a_fresh_database() {
        let d = db();
        assert!(d.tracker_samples_latest().unwrap().is_empty());
    }

    /// The chart asks for one day of one tracker. The index has to bound BOTH,
    /// or SQLite walks the tracker's whole history to keep one day of it --
    /// invisible on a test database, 13 s on two months of production samples.
    #[test]
    fn a_tracker_window_is_bounded_by_the_index_not_filtered_row_by_row() {
        let d = db();
        let plan: Vec<String> = d
            .conn
            .prepare(&format!("EXPLAIN QUERY PLAN {}", super::TRACKER_RANGE_SQL))
            .unwrap()
            .query_map(rusqlite::params!["t.example", 0.0, 1.0, 30.0], |r| r.get::<_, String>(3))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(
            plan.iter().any(|p| p.contains("(tracker=? AND ts>? AND ts<?)")),
            "the window must be an index range, got {plan:?}"
        );
    }

    /// A file on local disk is put in WAL, so the chart's read-only handle and
    /// the sampler's commits stop waiting on each other.
    #[test]
    fn bench_db_opens_in_wal() {
        let dir = std::env::temp_dir().join(format!("benchwal-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let d = BenchDb::open(&dir.join("bench.db")).unwrap();
        let mode: String = d.conn.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
        drop(d);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The Records card must render on a library that has done nothing yet.
    #[test]
    fn the_records_payload_answers_on_an_empty_database() {
        let d = db();
        let payload = d.records_payload().expect("an empty database still has a payload");
        assert!(payload.is_object(), "got {payload}");
    }

    fn peak_upload_gbps(d: &BenchDb) -> f64 {
        let payload = d.records_payload().unwrap();
        payload["records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["label"] == "Peak upload")
            .and_then(|r| r["value"].as_f64())
            .expect("a Peak upload record")
    }

    /// ⭐ A rate record is the best MINUTE, not the best sample. One 5 s spike
    /// is the kernel's send buffers filling, not the line: it once read 9.28
    /// Gbps on an 8 Gbps link.
    #[test]
    fn a_rate_record_is_the_best_minute_not_the_best_sample() {
        let d = db();
        // Minute 0: a steady 900 MB/s, twelve samples.
        for i in 0..12 {
            d.record_sample(&sample(f64::from(i) * 5.0, 900_000_000.0)).unwrap();
        }
        // Minute 1: one 1.2 GB/s spike in an otherwise quiet minute.
        for i in 0..12 {
            let rate = if i == 6 { 1_200_000_000.0 } else { 100_000_000.0 };
            d.record_sample(&sample(60.0 + f64::from(i) * 5.0, rate)).unwrap();
        }
        // Minute 2: a lone sample, too thin to stand for a minute.
        d.record_sample(&sample(125.0, 1_500_000_000.0)).unwrap();
        assert_eq!(peak_upload_gbps(&d), 7.2);
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
