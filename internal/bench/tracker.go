package bench

import (
	"database/sql"
	"fmt"
)

// trackerSamplesDDL stores a per-tracker, per-engine rollup at each bench tick.
// One row per (engine, tracker) per sample; tracker cardinality is tiny so the
// table stays small. cum_* are lifetime totals (sum of each torrent's
// total_upload/total_download), so MAX-MIN over a range = exact delta.
const trackerSamplesDDL = `
CREATE TABLE IF NOT EXISTS tracker_samples (
    ts             REAL NOT NULL,
    engine         TEXT NOT NULL,
    tracker        TEXT NOT NULL,
    upload_rate    REAL DEFAULT 0,
    download_rate  REAL DEFAULT 0,
    peers          REAL DEFAULT 0,
    active         REAL DEFAULT 0,
    torrents       REAL DEFAULT 0,
    cum_uploaded   INTEGER DEFAULT 0,
    cum_downloaded INTEGER DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_tracker_samples_ts ON tracker_samples(ts);
CREATE INDEX IF NOT EXISTS idx_tracker_samples_trk ON tracker_samples(tracker);
`

// TrackerSample is one (engine, tracker) rollup row for a single tick.
type TrackerSample struct {
	Ts            float64
	Engine        string
	Tracker       string
	UploadRate    int64
	DownloadRate  int64
	Peers         int
	Active        int
	Torrents      int
	CumUploaded   int64
	CumDownloaded int64
}

// InsertTrackerSamples writes a batch of per-tracker rollups in one transaction.
func (b *BenchDB) InsertTrackerSamples(samples []TrackerSample) {
	if len(samples) == 0 {
		return
	}
	b.mu.Lock()
	defer b.mu.Unlock()
	if b.conn == nil {
		return
	}
	tx, err := b.conn.Begin()
	if err != nil {
		return
	}
	stmt, err := tx.Prepare("INSERT INTO tracker_samples (ts, engine, tracker, upload_rate, download_rate, peers, active, torrents, cum_uploaded, cum_downloaded) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)")
	if err != nil {
		tx.Rollback()
		return
	}
	defer stmt.Close()
	for _, s := range samples {
		stmt.Exec(s.Ts, s.Engine, s.Tracker, float64(s.UploadRate), float64(s.DownloadRate),
			float64(s.Peers), float64(s.Active), float64(s.Torrents), s.CumUploaded, s.CumDownloaded)
	}
	tx.Commit()
}

// GetTrackerCurrent returns the latest sample for every (engine, tracker) pair.
// The front-end groups these by tracker to show the hoard/race split.
func (b *BenchDB) GetTrackerCurrent() []map[string]interface{} {
	b.mu.Lock()
	defer b.mu.Unlock()
	if b.conn == nil {
		return nil
	}
	rows, err := b.conn.Query(`
        SELECT t.engine, t.tracker, t.upload_rate, t.download_rate, t.peers,
               t.active, t.torrents, t.cum_uploaded, t.cum_downloaded, t.ts
        FROM tracker_samples t
        JOIN (SELECT engine, tracker, MAX(ts) AS mts FROM tracker_samples GROUP BY engine, tracker) m
          ON t.engine = m.engine AND t.tracker = m.tracker AND t.ts = m.mts
        ORDER BY t.tracker, t.engine`)
	if err != nil {
		return nil
	}
	defer rows.Close()

	var result []map[string]interface{}
	for rows.Next() {
		var engine, tracker string
		var ur, dr, peers, active, torrents, ts float64
		var cumUL, cumDL int64
		if err := rows.Scan(&engine, &tracker, &ur, &dr, &peers, &active, &torrents, &cumUL, &cumDL, &ts); err != nil {
			continue
		}
		result = append(result, map[string]interface{}{
			"engine": engine, "tracker": tracker, "upload_rate": ur, "download_rate": dr,
			"peers": peers, "active": active, "torrents": torrents,
			"cum_uploaded": cumUL, "cum_downloaded": cumDL, "ts": ts,
		})
	}
	return result
}

// GetTrackerRange returns a bucketed time-series for one tracker, split per
// engine, for charting. rates are averaged in each bucket; cum_* take the max.
func (b *BenchDB) GetTrackerRange(start, end, step int, tracker string) []map[string]interface{} {
	if step <= 0 {
		step = autoStep(end - start)
	}
	sqlStr := fmt.Sprintf(`
        SELECT CAST(ts / %d AS INTEGER) * %d AS bts, engine,
               AVG(upload_rate) AS upload_rate, AVG(download_rate) AS download_rate,
               AVG(peers) AS peers, AVG(active) AS active, AVG(torrents) AS torrents,
               MAX(cum_uploaded) AS cum_uploaded, MAX(cum_downloaded) AS cum_downloaded
        FROM tracker_samples
        WHERE ts >= ? AND ts <= ? AND tracker = ?
        GROUP BY CAST(ts / %d AS INTEGER), engine
        ORDER BY bts`, step, step, step)

	b.mu.Lock()
	rows, err := b.conn.Query(sqlStr, start, end, tracker)
	b.mu.Unlock()
	if err != nil {
		return nil
	}
	defer rows.Close()

	var result []map[string]interface{}
	for rows.Next() {
		var engine string
		var bts, ur, dr, peers, active, torrents float64
		var cumUL, cumDL sql.NullInt64
		if err := rows.Scan(&bts, &engine, &ur, &dr, &peers, &active, &torrents, &cumUL, &cumDL); err != nil {
			continue
		}
		result = append(result, map[string]interface{}{
			"ts": bts, "engine": engine, "upload_rate": ur, "download_rate": dr,
			"peers": peers, "active": active, "torrents": torrents,
			"cum_uploaded": cumUL.Int64, "cum_downloaded": cumDL.Int64,
		})
	}
	return result
}
