package bench

import (
	"database/sql"
	"fmt"
	"math"
	"time"
)

// pibBytes = 1 PiB en octets.
const pibBytes = 1 << 50

func round2(v float64) float64 { return math.Round(v*100) / 100 }

func dayDate(ts float64) string { return time.Unix(int64(ts), 0).UTC().Format("Jan 2") }
func isoDate(ts float64) string { return time.Unix(int64(ts), 0).UTC().Format("2006-01-02") }

func humanDur(sec float64) string {
	d := sec / 86400
	switch {
	case d >= 365:
		y := d / 365
		s := "s"
		if y < 2 {
			s = ""
		}
		return fmt.Sprintf("~%.1f year%s", y, s)
	case d >= 60:
		return fmt.Sprintf("~%.0f months", d/30.44)
	default:
		return fmt.Sprintf("~%.0f days", d)
	}
}

// GetRecords calcule les records all-time + jalons PiB depuis bench_samples.
//
// bench.db contient des samples d'AVANT le wiring de la baseline (global_uploaded
// non-monotone avant le fix 2.7.8) : au branchement, global_uploaded saute de la
// session-seule (~0) a baseline+session (~1 PiB de l'ere qBittorrent). On detecte
// ce saut (>100 TiB entre 2 samples consecutifs) et on ne considere comme reelle
// que la periode APRES le dernier saut, pour ne pas polluer "best upload day" ni
// dater faussement les jalons pre-bench.
const recordsTTL = 30 * time.Minute

// GetRecords returns the all-time records aggregate. It never blocks on the
// multi-second computation: it returns the cached value immediately and, when
// the cache is stale or empty, kicks off a single background refresh on the
// read-only connection. Before the first compute finishes it returns an empty
// map; the UI fills in on a later poll. This keeps the endpoint instant even
// though the underlying full scan of bench_samples is slow.
func (b *BenchDB) GetRecords() map[string]interface{} {
	b.recMu.Lock()
	cache := b.recCache
	stale := cache == nil || time.Since(b.recAt) >= recordsTTL
	if stale && !b.recComputing {
		b.recComputing = true
		go b.refreshRecords()
	}
	b.recMu.Unlock()
	if cache != nil {
		return cache
	}
	return map[string]interface{}{}
}

func (b *BenchDB) refreshRecords() {
	var res map[string]interface{}
	if b.roConn != nil {
		res = b.computeRecords(b.roConn)
	} else {
		b.mu.Lock()
		res = b.computeRecords(b.conn)
		b.mu.Unlock()
	}
	b.recMu.Lock()
	if len(res) > 0 {
		b.recCache = res
		b.recAt = time.Now()
	}
	b.recComputing = false
	b.recMu.Unlock()
}

func (b *BenchDB) computeRecords(conn *sql.DB) map[string]interface{} {
	if conn == nil {
		return map[string]interface{}{}
	}

	// --- t_clean : 1er sample propre apres le dernier saut > 100 TiB ---
	const jumpCap = 100.0 * (1 << 40)
	var tClean, firstTs float64
	if rows, err := conn.Query("SELECT ts, global_uploaded FROM bench_samples ORDER BY ts"); err == nil {
		var prevV float64
		havePrev, haveFirst := false, false
		for rows.Next() {
			var ts, v sql.NullFloat64
			if rows.Scan(&ts, &v) != nil {
				continue
			}
			if !haveFirst {
				firstTs, haveFirst = ts.Float64, true
			}
			if havePrev && (v.Float64-prevV) > jumpCap {
				tClean = ts.Float64
			}
			prevV, havePrev = v.Float64, true
		}
		rows.Close()
	}
	if tClean == 0 {
		tClean = firstTs
	}

	var now sql.NullFloat64
	conn.QueryRow("SELECT MAX(ts) FROM bench_samples").Scan(&now)

	// peak renvoie (ts, valeur) du max d'une expression sur bench_samples.
	peak := func(expr string) (float64, float64, bool) {
		var t, v sql.NullFloat64
		q := "SELECT ts, (" + expr + ") v FROM bench_samples WHERE (" + expr + ") IS NOT NULL ORDER BY v DESC LIMIT 1"
		if err := conn.QueryRow(q).Scan(&t, &v); err != nil || !v.Valid {
			return 0, 0, false
		}
		return t.Float64, v.Float64, true
	}

	rec := func(label string, value float64, unit, date string, hi bool) map[string]interface{} {
		return map[string]interface{}{"label": label, "value": value, "unit": unit, "date": date, "hi": hi}
	}
	records := []map[string]interface{}{}

	// Peak upload (Gbps) = (race+hoard upload_rate) * 8 / 1e9
	if ts, v, ok := peak("race_upload_rate + hoard_upload_rate"); ok {
		records = append(records, rec("Peak upload", round2(v*8/1e9), "Gbps", dayDate(ts), true))
	}
	// Peak download (Gbps) = race_download_rate * 8 / 1e9
	if ts, v, ok := peak("race_download_rate"); ok {
		records = append(records, rec("Peak download", round2(v*8/1e9), "Gbps", dayDate(ts), false))
	}
	// Peak swarm peers = race_peers + hoard_peers
	if ts, v, ok := peak("race_peers + hoard_peers"); ok {
		records = append(records, rec("Peak swarm peers", math.Round(v), "", dayDate(ts), false))
	}
	// Best upload day = max(delta global_uploaded) par jour, PERIODE PROPRE
	{
		var t, delta sql.NullFloat64
		err := conn.QueryRow(
			"SELECT MAX(ts) ts, MAX(global_uploaded)-MIN(global_uploaded) delta "+
				"FROM bench_samples WHERE ts>=? GROUP BY CAST(ts/86400 AS INT) ORDER BY delta DESC LIMIT 1",
			tClean).Scan(&t, &delta)
		if err == nil && delta.Valid {
			records = append(records, rec("Best upload day", round2(delta.Float64/(1<<40)), "TiB", dayDate(t.Float64), true))
		}
	}
	// Peak live seeds = hoard_uploading + race_uploading
	if ts, v, ok := peak("hoard_uploading + race_uploading"); ok {
		records = append(records, rec("Peak live seeds", math.Round(v), "", dayDate(ts), false))
	}
	// Best line test = vpn_speedtest max ul_mbps -> Gbps
	{
		var t, ul sql.NullFloat64
		if err := conn.QueryRow("SELECT ts, ul_mbps FROM vpn_speedtest ORDER BY ul_mbps DESC LIMIT 1").Scan(&t, &ul); err == nil && ul.Valid {
			records = append(records, rec("Best line test", round2(ul.Float64/1000), "Gbps", dayDate(t.Float64), false))
		}
	}

	// --- Jalons PiB ---
	var gmax, gminClean sql.NullFloat64
	conn.QueryRow("SELECT MAX(global_uploaded) FROM bench_samples").Scan(&gmax)
	conn.QueryRow("SELECT MIN(global_uploaded) FROM bench_samples WHERE ts>=?", tClean).Scan(&gminClean)
	curPib := gmax.Float64 / pibBytes

	milestones := []map[string]interface{}{}
	for k := 1; float64(k)*pibBytes <= gmax.Float64; k++ {
		thr := float64(k) * pibBytes
		m := map[string]interface{}{"pib": k}
		if gminClean.Float64 < thr { // vu passer sous->sur dans la periode propre = vrai franchissement
			var mts sql.NullFloat64
			conn.QueryRow("SELECT MIN(ts) FROM bench_samples WHERE global_uploaded>=? AND ts>=?", thr, tClean).Scan(&mts)
			m["observed"] = true
			m["ts"] = mts.Float64
			m["date"] = isoDate(mts.Float64)
		} else { // deja au-dessus au debut de la periode propre = pre-bench (ere qBittorrent)
			m["observed"] = false
		}
		milestones = append(milestones, m)
	}
	for i := 1; i < len(milestones); i++ {
		cur, prev := milestones[i], milestones[i-1]
		if cur["observed"] == true && prev["observed"] == true {
			cur["since_prev"] = humanDur(cur["ts"].(float64) - prev["ts"].(float64))
		}
	}

	// Prochain jalon + ETA (debit moyen 7 derniers jours)
	nextPib := int(curPib) + 1
	var w0, w1, wt0, wt1 sql.NullFloat64
	conn.QueryRow(
		"SELECT MIN(global_uploaded),MAX(global_uploaded),MIN(ts),MAX(ts) FROM bench_samples WHERE ts>=?",
		now.Float64-7*86400).Scan(&w0, &w1, &wt0, &wt1)
	rate := 0.0 // bytes/s
	if wt1.Float64 > wt0.Float64 {
		rate = (w1.Float64 - w0.Float64) / (wt1.Float64 - wt0.Float64)
	}
	togo := float64(nextPib)*pibBytes - gmax.Float64
	out := map[string]interface{}{
		"records":     records,
		"milestones":  milestones,
		"current_pib": math.Round(curPib*1000) / 1000,
		"next_pib":    nextPib,
	}
	if rate > 0 {
		out["rate_tib_day"] = round2(rate * 86400 / (1 << 40))
		out["next_eta_days"] = round2(togo / rate / 86400)
		out["next_eta_date"] = isoDate(now.Float64 + togo/rate)
	}
	return out
}
