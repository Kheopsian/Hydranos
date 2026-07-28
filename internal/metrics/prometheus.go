package metrics

import (
	"fmt"
	"strings"
	"time"
)

// ---------------------------------------------------------------------------
// Engine interfaces (minimal subset needed for metrics)
// ---------------------------------------------------------------------------

// RaceStatsProvider provides race engine stats for metrics collection.
type RaceStatsProvider interface {
	GetAllStatus() []map[string]interface{}
	GetChokingStats() map[string]interface{}
}

// HoardStatsProvider provides hoard engine stats for metrics collection.
type HoardStatsProvider interface {
	GetAllStatus() map[string]interface{}
}

// ---------------------------------------------------------------------------
// MetricsCollector
// ---------------------------------------------------------------------------

// MetricsCollector gathers stats from engines and formats them as Prometheus
// text exposition format. No prometheus_client dependency needed.
type MetricsCollector struct {
	race      RaceStatsProvider
	hoard     HoardStatsProvider
	startTime time.Time
}

// NewMetricsCollector creates a collector wired to the given engines.
func NewMetricsCollector(race RaceStatsProvider, hoard HoardStatsProvider) *MetricsCollector {
	return &MetricsCollector{
		race:      race,
		hoard:     hoard,
		startTime: time.Now(),
	}
}

// CollectPrometheus returns all metrics in Prometheus text format.
func (m *MetricsCollector) CollectPrometheus() string {
	var b strings.Builder

	// Uptime
	uptime := time.Since(m.startTime).Seconds()
	fmt.Fprintf(&b, "hydra_uptime_seconds %.0f\n", uptime)

	// Race engine
	if m.race != nil {
		raceTorrents := m.race.GetAllStatus()
		fmt.Fprintf(&b, "hydra_race_torrents_total %d\n", len(raceTorrents))

		var totalUp, totalDown float64
		var totalPeers int64
		for _, t := range raceTorrents {
			totalUp += toFloat64(t["upload_rate"])
			totalDown += toFloat64(t["download_rate"])
			totalPeers += toInt64(t["num_peers"])
		}
		fmt.Fprintf(&b, "hydra_race_upload_rate_bytes %.0f\n", totalUp)
		fmt.Fprintf(&b, "hydra_race_download_rate_bytes %.0f\n", totalDown)
		fmt.Fprintf(&b, "hydra_race_peers_total %d\n", totalPeers)

		// Choking stats
		choking := m.race.GetChokingStats()
		for torrentID, v := range choking {
			if stats, ok := v.(map[string]interface{}); ok {
				numUnchoked := toInt64(stats["num_unchoked"])
				fmt.Fprintf(&b, "hydra_race_choking_unchoked{torrent=\"%s\"} %d\n", torrentID, numUnchoked)
			}
		}
	}

	// Hoard engine
	if m.hoard != nil {
		hoardStats := m.hoard.GetAllStatus()
		fmt.Fprintf(&b, "hydra_hoard_torrents_total %d\n", toInt64(hoardStats["total_torrents"]))
		fmt.Fprintf(&b, "hydra_hoard_upload_rate_bytes %.0f\n", toFloat64(hoardStats["active_upload_rate"]))
		fmt.Fprintf(&b, "hydra_hoard_peers_total %d\n", toInt64(hoardStats["active_peers"]))
		fmt.Fprintf(&b, "hydra_hoard_torrents_with_peers %d\n", toInt64(hoardStats["torrents_with_peers"]))
		fmt.Fprintf(&b, "hydra_hoard_torrents_uploading %d\n", toInt64(hoardStats["torrents_uploading"]))
	}

	return b.String()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

func toFloat64(v interface{}) float64 {
	switch n := v.(type) {
	case float64:
		return n
	case float32:
		return float64(n)
	case int:
		return float64(n)
	case int64:
		return float64(n)
	case int32:
		return float64(n)
	default:
		return 0
	}
}

func toInt64(v interface{}) int64 {
	switch n := v.(type) {
	case int64:
		return n
	case int:
		return int64(n)
	case int32:
		return int64(n)
	case float64:
		return int64(n)
	case float32:
		return int64(n)
	default:
		return 0
	}
}
