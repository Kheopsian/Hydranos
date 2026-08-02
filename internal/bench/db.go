package bench

import (
	"database/sql"
	"fmt"
	"log/slog"
	"math"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"

	_ "modernc.org/sqlite"
)

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const (
	RetentionDays = 365
	retentionSecs = RetentionDays * 86400
)

// MetricCols is the ordered list of metric columns in the bench_samples table.
var MetricCols = []string{
	"race_upload_rate",
	"race_download_rate",
	"race_peers",
	"race_torrents",
	"race_uploading",
	"race_avg_share",
	"hoard_upload_rate",
	"hoard_peers",
	"hoard_active",
	"hoard_with_peers",
	"hoard_uploading",
	"iowait_pct",
	"arc_size_bytes",
	"arc_hit_rate_pct",
	"arc_demand_hit_rate_pct",
	"arc_miss_per_sec",
	"arc_demand_miss_per_sec",
	"arc_ghost_hits_per_sec",
	"open_fds",
	"hoard_session_uploaded",
	"race_session_uploaded",
	"global_uploaded",
	"global_downloaded",
}

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

const benchDDL = `
CREATE TABLE IF NOT EXISTS bench_samples (
    ts                      REAL NOT NULL,
    race_upload_rate        REAL DEFAULT 0,
    race_download_rate      REAL DEFAULT 0,
    race_peers              REAL DEFAULT 0,
    race_torrents           REAL DEFAULT 0,
    race_uploading          REAL DEFAULT 0,
    race_avg_share          REAL DEFAULT 0,
    hoard_upload_rate       REAL DEFAULT 0,
    hoard_peers             REAL DEFAULT 0,
    hoard_active            REAL DEFAULT 0,
    hoard_with_peers        REAL DEFAULT 0,
    hoard_uploading         REAL DEFAULT 0,
    iowait_pct              REAL DEFAULT 0,
    arc_size_bytes          REAL DEFAULT 0,
    arc_hit_rate_pct        REAL DEFAULT 0,
    arc_demand_hit_rate_pct REAL DEFAULT 0,
    arc_miss_per_sec        REAL DEFAULT 0,
    arc_demand_miss_per_sec REAL DEFAULT 0,
    arc_ghost_hits_per_sec  REAL DEFAULT 0,
    hoard_session_uploaded  INTEGER DEFAULT 0,
    race_session_uploaded   INTEGER DEFAULT 0,
    global_uploaded         INTEGER DEFAULT 0,
    global_downloaded       INTEGER DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_bench_ts ON bench_samples(ts);
`

// healthCountersDDL is a simple key/value store for cumulative health
// invariant counters (wasted bytes, per-invariant counts, efficiency). Kept in
// bench.db so the numbers survive restarts — regressions of a correction run
// silently for days otherwise.
const healthCountersDDL = `
CREATE TABLE IF NOT EXISTS health_counters (
    key        TEXT PRIMARY KEY,
    value      INTEGER NOT NULL DEFAULT 0,
    updated_at REAL NOT NULL DEFAULT 0
);
`

const vpnDDL = `
CREATE TABLE IF NOT EXISTS vpn_speedtest (
    ts      REAL NOT NULL,
    ul_mbps REAL NOT NULL,
    dl_mbps REAL NOT NULL,
    ul_torrent_mbps REAL DEFAULT 0,
    dl_torrent_mbps REAL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_vpn_ts ON vpn_speedtest(ts);
`

const raceEventsDDL = `
CREATE TABLE IF NOT EXISTS race_events (
    ts              REAL NOT NULL,
    info_hash       TEXT NOT NULL,
    event           TEXT NOT NULL,
    name            TEXT DEFAULT '',
    size            INTEGER DEFAULT 0,
    download_time   REAL DEFAULT 0,
    upload_total    INTEGER DEFAULT 0,
    upload_rate     REAL DEFAULT 0,
    download_rate   REAL DEFAULT 0,
    peers           INTEGER DEFAULT 0,
    seeds           INTEGER DEFAULT 0,
    swarm_seeds     INTEGER DEFAULT 0,
    swarm_leechers  INTEGER DEFAULT 0,
    category        TEXT DEFAULT '',
    time_since_add  REAL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_race_events_ts ON race_events(ts);
CREATE INDEX IF NOT EXISTS idx_race_events_hash ON race_events(info_hash);
`

const raceSnapshotsDDL = `
CREATE TABLE IF NOT EXISTS race_snapshots (
    ts              REAL NOT NULL,
    info_hash       TEXT NOT NULL,
    progress        REAL DEFAULT 0,
    upload_rate     REAL DEFAULT 0,
    download_rate   REAL DEFAULT 0,
    total_upload    INTEGER DEFAULT 0,
    total_download  INTEGER DEFAULT 0,
    peers           INTEGER DEFAULT 0,
    seeds           INTEGER DEFAULT 0,
    swarm_seeds     INTEGER DEFAULT 0,
    swarm_leechers  INTEGER DEFAULT 0,
    ratio           REAL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_race_snap_ts ON race_snapshots(ts);
CREATE INDEX IF NOT EXISTS idx_race_snap_hash ON race_snapshots(info_hash);
`

// ---------------------------------------------------------------------------
// BenchDB
// ---------------------------------------------------------------------------

// BenchDB is a SQLite-backed time-series store for benchmark metrics.
type BenchDB struct {
	mu   sync.Mutex
	conn *sql.DB
	path string

	// roConn is a dedicated read-only connection for heavy, unbounded read
	// aggregates (records) so they never hold the write mutex and stall the
	// 5s sampler. Enabled by WAL on the main connection.
	roConn *sql.DB

	// records cache: the all-time records aggregate is a full scan of
	// bench_samples (O(N)); recompute at most every recordsTTL.
	recMu        sync.Mutex
	recCache     map[string]interface{}
	recAt        time.Time
	recComputing bool
}

// NewBenchDB creates a BenchDB but does not open it yet.
func NewBenchDB(dbPath string) *BenchDB {
	return &BenchDB{path: dbPath}
}

// Open creates/opens the SQLite database and initialises schemas.
func (b *BenchDB) Open() error {
	dir := filepath.Dir(b.path)
	if err := os.MkdirAll(dir, 0755); err != nil {
		return err
	}

	db, err := sql.Open("sqlite", b.path)
	if err != nil {
		return err
	}

	if _, err := db.Exec(benchDDL); err != nil {
		db.Close()
		return err
	}
	if _, err := db.Exec(vpnDDL); err != nil {
		db.Close()
		return err
	}
	if _, err := db.Exec(raceEventsDDL); err != nil {
		db.Close()
		return err
	}
	if _, err := db.Exec(raceSnapshotsDDL); err != nil {
		db.Close()
		return err
	}
	if _, err := db.Exec(healthCountersDDL); err != nil {
		db.Close()
		return err
	}
	if _, err := db.Exec(trackerSamplesDDL); err != nil {
		db.Close()
		return err
	}
	// Migrate: add columns that may not exist in older databases.
	db.Exec("ALTER TABLE bench_samples ADD COLUMN race_uploading REAL DEFAULT 0")
	db.Exec("ALTER TABLE bench_samples ADD COLUMN race_avg_share REAL DEFAULT 0")
	db.Exec("ALTER TABLE race_events ADD COLUMN download_total INTEGER DEFAULT 0")
	db.Exec("ALTER TABLE race_events ADD COLUMN uploader TEXT DEFAULT ''")
	db.Exec("ALTER TABLE race_events ADD COLUMN injected_peers INTEGER DEFAULT 0")
	db.Exec("ALTER TABLE race_snapshots ADD COLUMN peers_json TEXT DEFAULT '[]'")
	db.Exec("ALTER TABLE bench_samples ADD COLUMN open_fds REAL DEFAULT 0")
	db.Exec("ALTER TABLE bench_samples ADD COLUMN hoard_session_uploaded INTEGER DEFAULT 0")
	db.Exec("ALTER TABLE bench_samples ADD COLUMN race_session_uploaded INTEGER DEFAULT 0")
	db.Exec("ALTER TABLE bench_samples ADD COLUMN global_uploaded INTEGER DEFAULT 0")
	db.Exec("ALTER TABLE bench_samples ADD COLUMN global_downloaded INTEGER DEFAULT 0")
	db.Exec("ALTER TABLE vpn_speedtest ADD COLUMN ul_torrent_mbps REAL DEFAULT 0")
	db.Exec("ALTER TABLE vpn_speedtest ADD COLUMN dl_torrent_mbps REAL DEFAULT 0")

	// WAL lets a read-only connection run concurrently with the 5s writer
	// instead of every reader/writer serialising on one connection.
	db.Exec("PRAGMA journal_mode=WAL")
	db.Exec("PRAGMA busy_timeout=5000")
	db.SetMaxOpenConns(1)

	b.conn = db

	// Dedicated read-only connection for heavy aggregate reads (records),
	// so a multi-second full scan never blocks writes or other endpoints.
	if ro, roErr := sql.Open("sqlite", "file:"+b.path+"?mode=ro"); roErr == nil {
		ro.Exec("PRAGMA busy_timeout=5000")
		ro.SetMaxOpenConns(2)
		b.roConn = ro
	} else {
		slog.Warn("bench_db: read-only connection failed; records fall back to the write connection", "err", roErr)
	}

	go b.GetRecords() // warm the records cache off the request path

	slog.Info("bench_db: opened", "path", b.path)
	return nil
}

// Close closes the database.
func (b *BenchDB) Close() error {
	if b.roConn != nil {
		b.roConn.Close()
	}
	if b.conn != nil {
		return b.conn.Close()
	}
	return nil
}

// ---------------------------------------------------------------------------
// Write
// ---------------------------------------------------------------------------

// Insert stores a snapshot. Unknown keys are silently ignored.
func (b *BenchDB) Insert(snap map[string]interface{}) {
	var cols []string
	var vals []interface{}

	ts, ok := snap["ts"]
	if !ok {
		ts = float64(time.Now().Unix())
	}
	cols = append(cols, "ts")
	vals = append(vals, ts)

	for _, c := range MetricCols {
		if v, exists := snap[c]; exists {
			cols = append(cols, c)
			vals = append(vals, v)
		}
	}

	if len(cols) <= 1 { // only ts
		return
	}

	placeholders := make([]string, len(cols))
	for i := range placeholders {
		placeholders[i] = "?"
	}

	sqlStr := fmt.Sprintf("INSERT INTO bench_samples (%s) VALUES (%s)",
		strings.Join(cols, ", "), strings.Join(placeholders, ", "))

	b.mu.Lock()
	defer b.mu.Unlock()
	if b.conn == nil {
		return
	}
	b.conn.Exec(sqlStr, vals...)
}

// PurgeOld removes entries older than 30 days.
func (b *BenchDB) PurgeOld() {
	cutoff := float64(time.Now().Unix()) - retentionSecs
	b.mu.Lock()
	defer b.mu.Unlock()
	if b.conn == nil {
		return
	}
	b.conn.Exec("DELETE FROM bench_samples WHERE ts < ?", cutoff)
	b.conn.Exec("DELETE FROM vpn_speedtest WHERE ts < ?", cutoff)
	b.conn.Exec("DELETE FROM race_events WHERE ts < ?", cutoff)
	b.conn.Exec("DELETE FROM race_snapshots WHERE ts < ?", cutoff)
	b.conn.Exec("DELETE FROM tracker_samples WHERE ts < ?", cutoff)
}

// InsertVpn stores a VPN speed test result.
func (b *BenchDB) InsertVpn(ts, ulMbps, dlMbps, ulTorrentMbps, dlTorrentMbps float64) {
	b.mu.Lock()
	defer b.mu.Unlock()
	if b.conn == nil {
		return
	}
	b.conn.Exec("INSERT INTO vpn_speedtest (ts, ul_mbps, dl_mbps, ul_torrent_mbps, dl_torrent_mbps) VALUES (?, ?, ?, ?, ?)",
		ts, ulMbps, dlMbps, ulTorrentMbps, dlTorrentMbps)
}

// PersistHealth upserts the health invariant counters. gauges overwrite the
// stored value, maxes raise a high-water mark, increments add to a monotone
// total. One transaction so a scan's counters land atomically. Satisfies
// health.CounterStore.
func (b *BenchDB) PersistHealth(gauges, maxes, increments map[string]int64) {
	b.mu.Lock()
	defer b.mu.Unlock()
	if b.conn == nil {
		return
	}
	ts := float64(time.Now().Unix())
	tx, err := b.conn.Begin()
	if err != nil {
		return
	}
	set := `INSERT INTO health_counters(key,value,updated_at) VALUES(?,?,?)
		ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at`
	max := `INSERT INTO health_counters(key,value,updated_at) VALUES(?,?,?)
		ON CONFLICT(key) DO UPDATE SET value=MAX(value,excluded.value), updated_at=excluded.updated_at`
	incr := `INSERT INTO health_counters(key,value,updated_at) VALUES(?,?,?)
		ON CONFLICT(key) DO UPDATE SET value=value+excluded.value, updated_at=excluded.updated_at`
	for k, v := range gauges {
		tx.Exec(set, k, v, ts)
	}
	for k, v := range maxes {
		tx.Exec(max, k, v, ts)
	}
	for k, v := range increments {
		tx.Exec(incr, k, v, ts)
	}
	tx.Commit()
}

// GetHealthCounters returns all persisted health counters. Satisfies
// health.CounterStore.
func (b *BenchDB) GetHealthCounters() map[string]int64 {
	b.mu.Lock()
	defer b.mu.Unlock()
	out := map[string]int64{}
	if b.conn == nil {
		return out
	}
	rows, err := b.conn.Query("SELECT key, value FROM health_counters")
	if err != nil {
		return out
	}
	defer rows.Close()
	for rows.Next() {
		var k string
		var v int64
		if err := rows.Scan(&k, &v); err == nil {
			out[k] = v
		}
	}
	return out
}

// ---------------------------------------------------------------------------
// Read — implement BenchDB interface from server.go
// ---------------------------------------------------------------------------

// GetCurrent returns the latest sample as a map.
func (b *BenchDB) GetCurrent() map[string]interface{} {
	b.mu.Lock()
	defer b.mu.Unlock()
	if b.conn == nil {
		return map[string]interface{}{}
	}

	allCols := append([]string{"ts"}, MetricCols...)
	sqlStr := fmt.Sprintf("SELECT %s FROM bench_samples ORDER BY ts DESC LIMIT 1",
		strings.Join(allCols, ", "))
	row := b.conn.QueryRow(sqlStr)

	values := make([]sql.NullFloat64, len(allCols))
	ptrs := make([]interface{}, len(allCols))
	for i := range values {
		ptrs[i] = &values[i]
	}

	if err := row.Scan(ptrs...); err != nil {
		return map[string]interface{}{}
	}

	result := make(map[string]interface{}, len(allCols))
	for i, col := range allCols {
		if values[i].Valid {
			result[col] = values[i].Float64
		} else {
			result[col] = 0.0
		}
	}
	return result
}

// GetRange returns aggregated samples in [start, end], grouped by step seconds.
// If step <= 0, it is auto-selected based on the span.
func (b *BenchDB) GetRange(start, end, step int) []map[string]interface{} {
	if step <= 0 {
		step = autoStep(end - start)
	}

	avgCols := make([]string, len(MetricCols))
	for i, c := range MetricCols {
		avgCols[i] = fmt.Sprintf("AVG(%s) AS %s", c, c)
	}

	sqlStr := fmt.Sprintf(
		"SELECT CAST(ts / %d AS INTEGER) * %d AS ts, %s "+
			"FROM bench_samples WHERE ts >= ? AND ts <= ? "+
			"GROUP BY CAST(ts / %d AS INTEGER) ORDER BY ts",
		step, step, strings.Join(avgCols, ", "), step,
	)

	b.mu.Lock()
	rows, err := b.conn.Query(sqlStr, start, end)
	b.mu.Unlock()
	if err != nil {
		return nil
	}
	defer rows.Close()

	allCols := append([]string{"ts"}, MetricCols...)
	var result []map[string]interface{}

	for rows.Next() {
		values := make([]sql.NullFloat64, len(allCols))
		ptrs := make([]interface{}, len(allCols))
		for i := range values {
			ptrs[i] = &values[i]
		}
		if err := rows.Scan(ptrs...); err != nil {
			continue
		}

		m := make(map[string]interface{}, len(allCols))
		for i, col := range allCols {
			if values[i].Valid {
				m[col] = values[i].Float64
			} else {
				m[col] = 0.0
			}
		}
		result = append(result, m)
	}
	return result
}

// GetComparison compares two periods: P1=[start, mid] vs P2=[mid, end].
func (b *BenchDB) GetComparison(start, mid, end int) map[string]interface{} {
	agg1 := b.getAggregates(float64(start), float64(mid))
	agg2 := b.getAggregates(float64(mid), float64(end))

	metricsMap := make(map[string]interface{})
	for _, col := range MetricCols {
		a1 := agg1[col]
		a2 := agg2[col]
		avg1 := a1["avg"].(float64)
		avg2 := a2["avg"].(float64)
		var delta float64
		if avg1 != 0 {
			delta = (avg2 - avg1) / math.Abs(avg1) * 100
		}
		metricsMap[col] = map[string]interface{}{
			"p1":            a1,
			"p2":            a2,
			"delta_avg_pct": math.Round(delta*100) / 100,
		}
	}

	return map[string]interface{}{
		"metrics":  metricsMap,
		"p1_count": agg1[MetricCols[0]]["count"],
		"p2_count": agg2[MetricCols[0]]["count"],
	}
}

// GetVpnLatest returns the most recent VPN speed test.
func (b *BenchDB) GetVpnLatest() map[string]interface{} {
	b.mu.Lock()
	defer b.mu.Unlock()
	if b.conn == nil {
		return nil
	}
	var ts, ul, dl, ult, dlt float64
	err := b.conn.QueryRow(
		"SELECT ts, ul_mbps, dl_mbps, ul_torrent_mbps, dl_torrent_mbps FROM vpn_speedtest ORDER BY ts DESC LIMIT 1",
	).Scan(&ts, &ul, &dl, &ult, &dlt)
	if err != nil {
		return nil
	}
	return map[string]interface{}{"ts": ts, "ul_mbps": ul, "dl_mbps": dl, "ul_torrent_mbps": ult, "dl_torrent_mbps": dlt}
}

// GetVpnRange returns VPN speed tests in the given time range.
func (b *BenchDB) GetVpnRange(start, end float64) []map[string]interface{} {
	b.mu.Lock()
	rows, err := b.conn.Query(
		"SELECT ts, ul_mbps, dl_mbps, ul_torrent_mbps, dl_torrent_mbps FROM vpn_speedtest WHERE ts >= ? AND ts <= ? ORDER BY ts",
		start, end)
	b.mu.Unlock()
	if err != nil {
		return nil
	}
	defer rows.Close()

	var result []map[string]interface{}
	for rows.Next() {
		var ts, ul, dl, ult, dlt float64
		if rows.Scan(&ts, &ul, &dl, &ult, &dlt) == nil {
			result = append(result, map[string]interface{}{"ts": ts, "ul_mbps": ul, "dl_mbps": dl, "ul_torrent_mbps": ult, "dl_torrent_mbps": dlt})
		}
	}
	return result
}

// ---------------------------------------------------------------------------
// Race Events
// ---------------------------------------------------------------------------

// RaceEvent represents a per-torrent lifecycle event.
type RaceEvent struct {
	Ts            float64 `json:"ts"`
	InfoHash      string  `json:"info_hash"`
	Event         string  `json:"event"`
	Name          string  `json:"name"`
	Size          int64   `json:"size"`
	DownloadTime  float64 `json:"download_time"`
	UploadTotal   int64   `json:"upload_total"`
	DownloadTotal int64   `json:"download_total"`
	UploadRate    float64 `json:"upload_rate"`
	DownloadRate  float64 `json:"download_rate"`
	Peers         int     `json:"peers"`
	Seeds         int     `json:"seeds"`
	SwarmSeeds    int     `json:"swarm_seeds"`
	SwarmLeechers int     `json:"swarm_leechers"`
	Category      string  `json:"category"`
	TimeSinceAdd  float64 `json:"time_since_add"`
	Uploader      string  `json:"uploader,omitempty"`
	InjectedPeers int     `json:"injected_peers,omitempty"`
}

// InsertRaceEvent stores a race torrent lifecycle event.
func (b *BenchDB) InsertRaceEvent(ev RaceEvent) {
	b.mu.Lock()
	defer b.mu.Unlock()
	if b.conn == nil {
		return
	}
	b.conn.Exec(`INSERT INTO race_events
		(ts, info_hash, event, name, size, download_time, upload_total, download_total, upload_rate,
		 download_rate, peers, seeds, swarm_seeds, swarm_leechers, category, time_since_add, uploader, injected_peers)
		VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
		ev.Ts, ev.InfoHash, ev.Event, ev.Name, ev.Size, ev.DownloadTime,
		ev.UploadTotal, ev.DownloadTotal, ev.UploadRate, ev.DownloadRate,
		ev.Peers, ev.Seeds, ev.SwarmSeeds, ev.SwarmLeechers,
		ev.Category, ev.TimeSinceAdd, ev.Uploader, ev.InjectedPeers,
	)
}

// GetRaceEvents returns race events in the given time range.
func (b *BenchDB) GetRaceEvents(start, end float64) []RaceEvent {
	b.mu.Lock()
	rows, err := b.conn.Query(
		`SELECT ts, info_hash, event, name, size, download_time, upload_total, COALESCE(download_total,0), upload_rate,
		        download_rate, peers, seeds, swarm_seeds, swarm_leechers, category, time_since_add, COALESCE(uploader,''), COALESCE(injected_peers,0)
		 FROM race_events WHERE ts >= ? AND ts <= ? ORDER BY ts`, start, end)
	b.mu.Unlock()
	if err != nil {
		return nil
	}
	defer rows.Close()

	var result []RaceEvent
	for rows.Next() {
		var ev RaceEvent
		if rows.Scan(&ev.Ts, &ev.InfoHash, &ev.Event, &ev.Name, &ev.Size,
			&ev.DownloadTime, &ev.UploadTotal, &ev.DownloadTotal, &ev.UploadRate, &ev.DownloadRate,
			&ev.Peers, &ev.Seeds, &ev.SwarmSeeds, &ev.SwarmLeechers,
			&ev.Category, &ev.TimeSinceAdd, &ev.Uploader, &ev.InjectedPeers) == nil {
			result = append(result, ev)
		}
	}
	return result
}

// GetRaceEventsForTorrent returns all events for a specific torrent.
// GetInjectionData returns injection stats for a torrent from persisted events.
func (b *BenchDB) GetInjectionData(infoHash string) (injectedPeers int, injectionHit bool) {
	b.mu.Lock()
	defer b.mu.Unlock()
	if b.conn == nil {
		return 0, false
	}
	// Get max injected_peers from uploader_injected events
	b.conn.QueryRow(
		"SELECT COALESCE(MAX(injected_peers),0) FROM race_events WHERE info_hash = ? AND event = 'uploader_injected'",
		infoHash,
	).Scan(&injectedPeers)
	// Check if injection_hit event exists
	var hitCount int
	b.conn.QueryRow(
		"SELECT COUNT(*) FROM race_events WHERE info_hash = ? AND event = 'injection_hit'",
		infoHash,
	).Scan(&hitCount)
	injectionHit = hitCount > 0
	return
}

func (b *BenchDB) GetRaceEventsForTorrent(infoHash string) []RaceEvent {
	b.mu.Lock()
	rows, err := b.conn.Query(
		`SELECT ts, info_hash, event, name, size, download_time, upload_total, COALESCE(download_total,0), upload_rate,
		        download_rate, peers, seeds, swarm_seeds, swarm_leechers, category, time_since_add, COALESCE(uploader,''), COALESCE(injected_peers,0)
		 FROM race_events WHERE info_hash = ? ORDER BY ts`, infoHash)
	b.mu.Unlock()
	if err != nil {
		return nil
	}
	defer rows.Close()

	var result []RaceEvent
	for rows.Next() {
		var ev RaceEvent
		if rows.Scan(&ev.Ts, &ev.InfoHash, &ev.Event, &ev.Name, &ev.Size,
			&ev.DownloadTime, &ev.UploadTotal, &ev.DownloadTotal, &ev.UploadRate, &ev.DownloadRate,
			&ev.Peers, &ev.Seeds, &ev.SwarmSeeds, &ev.SwarmLeechers,
			&ev.Category, &ev.TimeSinceAdd, &ev.Uploader, &ev.InjectedPeers) == nil {
			result = append(result, ev)
		}
	}
	return result
}

// ---------------------------------------------------------------------------
// Race Snapshots
// ---------------------------------------------------------------------------

// RaceSnapshot represents a point-in-time state of a race torrent.
type RaceSnapshot struct {
	Ts            float64 `json:"ts"`
	InfoHash      string  `json:"info_hash"`
	Progress      float64 `json:"progress"`
	UploadRate    float64 `json:"upload_rate"`
	DownloadRate  float64 `json:"download_rate"`
	TotalUpload   int64   `json:"total_upload"`
	TotalDownload int64   `json:"total_download"`
	Peers         int     `json:"peers"`
	Seeds         int     `json:"seeds"`
	SwarmSeeds    int     `json:"swarm_seeds"`
	SwarmLeechers int     `json:"swarm_leechers"`
	Ratio         float64 `json:"ratio"`
	PeersJSON     string  `json:"peers_json,omitempty"`
}

// InsertRaceSnapshots stores a batch of race torrent snapshots.
func (b *BenchDB) InsertRaceSnapshots(snapshots []RaceSnapshot) {
	b.mu.Lock()
	defer b.mu.Unlock()
	if b.conn == nil || len(snapshots) == 0 {
		return
	}
	tx, err := b.conn.Begin()
	if err != nil {
		return
	}
	stmt, err := tx.Prepare(`INSERT INTO race_snapshots
		(ts, info_hash, progress, upload_rate, download_rate, total_upload, total_download,
		 peers, seeds, swarm_seeds, swarm_leechers, ratio, peers_json)
		VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`)
	if err != nil {
		tx.Rollback()
		return
	}
	defer stmt.Close()
	for _, s := range snapshots {
		stmt.Exec(s.Ts, s.InfoHash, s.Progress, s.UploadRate, s.DownloadRate,
			s.TotalUpload, s.TotalDownload, s.Peers, s.Seeds,
			s.SwarmSeeds, s.SwarmLeechers, s.Ratio, s.PeersJSON)
	}
	tx.Commit()
}

// GetRaceSnapshots returns snapshots for a specific torrent.
func (b *BenchDB) GetRaceSnapshots(infoHash string) []RaceSnapshot {
	b.mu.Lock()
	rows, err := b.conn.Query(
		`SELECT ts, info_hash, progress, upload_rate, download_rate, total_upload, total_download,
		        peers, seeds, swarm_seeds, swarm_leechers, ratio, COALESCE(peers_json,'[]')
		 FROM race_snapshots WHERE info_hash = ? ORDER BY ts`, infoHash)
	b.mu.Unlock()
	if err != nil {
		return nil
	}
	defer rows.Close()

	var result []RaceSnapshot
	for rows.Next() {
		var s RaceSnapshot
		if rows.Scan(&s.Ts, &s.InfoHash, &s.Progress, &s.UploadRate, &s.DownloadRate,
			&s.TotalUpload, &s.TotalDownload, &s.Peers, &s.Seeds,
			&s.SwarmSeeds, &s.SwarmLeechers, &s.Ratio, &s.PeersJSON) == nil {
			result = append(result, s)
		}
	}
	return result
}

// PurgeRaceData removes snapshots for a torrent (called on drain/remove). Events are kept for history.
func (b *BenchDB) PurgeRaceData(infoHash string) {
	b.mu.Lock()
	defer b.mu.Unlock()
	if b.conn == nil {
		return
	}
	b.conn.Exec("DELETE FROM race_snapshots WHERE info_hash = ?", infoHash)
	// race_events kept for history (cleaned by ts-based retention)
}

// ---------------------------------------------------------------------------
// Internal
// ---------------------------------------------------------------------------

func (b *BenchDB) getAggregates(start, end float64) map[string]map[string]interface{} {
	b.mu.Lock()
	sqlStr := fmt.Sprintf("SELECT %s FROM bench_samples WHERE ts >= ? AND ts <= ?",
		strings.Join(MetricCols, ", "))
	rows, err := b.conn.Query(sqlStr, start, end)
	b.mu.Unlock()

	result := make(map[string]map[string]interface{})

	if err != nil {
		for _, c := range MetricCols {
			result[c] = map[string]interface{}{"avg": 0.0, "max": 0.0, "p95": 0.0, "count": 0}
		}
		return result
	}
	defer rows.Close()

	// Collect all values per column
	columns := make([][]float64, len(MetricCols))
	for i := range columns {
		columns[i] = make([]float64, 0, 64)
	}

	for rows.Next() {
		values := make([]sql.NullFloat64, len(MetricCols))
		ptrs := make([]interface{}, len(MetricCols))
		for i := range values {
			ptrs[i] = &values[i]
		}
		if rows.Scan(ptrs...) != nil {
			continue
		}
		for i, v := range values {
			val := 0.0
			if v.Valid {
				val = v.Float64
			}
			columns[i] = append(columns[i], val)
		}
	}

	for i, col := range MetricCols {
		vals := columns[i]
		n := len(vals)
		if n == 0 {
			result[col] = map[string]interface{}{"avg": 0.0, "max": 0.0, "p95": 0.0, "count": 0}
			continue
		}

		var sum, maxVal float64
		for _, v := range vals {
			sum += v
			if v > maxVal {
				maxVal = v
			}
		}

		// Sort for p95 — simple insertion sort for our use case
		sorted := make([]float64, len(vals))
		copy(sorted, vals)
		for j := 1; j < len(sorted); j++ {
			key := sorted[j]
			k := j - 1
			for k >= 0 && sorted[k] > key {
				sorted[k+1] = sorted[k]
				k--
			}
			sorted[k+1] = key
		}

		p95Idx := int(float64(n) * 0.95)
		if p95Idx >= n {
			p95Idx = n - 1
		}

		result[col] = map[string]interface{}{
			"avg":   sum / float64(n),
			"max":   maxVal,
			"p95":   sorted[p95Idx],
			"count": n,
		}
	}

	return result
}

func autoStep(span int) int {
	switch {
	case span <= 1800:
		return 5
	case span <= 10800:
		return 30
	case span <= 43200:
		return 120
	case span <= 172800:
		return 300
	case span <= 604800:
		return 1800
	default:
		return 3600
	}
}
