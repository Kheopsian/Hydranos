package drain

import (
	"context"
	"log/slog"
	"sync"
	"syscall"
	"time"

	"github.com/Kheopsian/hydra/internal/config"
)

// ---------------------------------------------------------------------------
// RaceEngineForDrain is the minimal interface drain needs from the race engine.
// ---------------------------------------------------------------------------

// RaceEngineForDrain provides access to race torrents for drain purposes.
type RaceEngineForDrain interface {
	GetAllStatus() []map[string]interface{}
	RemoveTorrent(infoHash string, deleteFiles bool) error
}

// ---------------------------------------------------------------------------
// RaceDrain
// ---------------------------------------------------------------------------

// RaceDrain monitors NVMe disk usage and automatically removes the oldest race
// torrents when usage exceeds the high watermark, until it drops below the low
// watermark.
type RaceDrain struct {
	cfg  config.RaceDrainConfig
	race RaceEngineForDrain

	mu        sync.Mutex
	running   bool
	stats     map[string]interface{}
	lastDrain float64
	history   []map[string]interface{}
}

// NewRaceDrain creates a new drain instance.
func NewRaceDrain(cfg config.RaceDrainConfig, race RaceEngineForDrain) *RaceDrain {
	return &RaceDrain{
		cfg:  cfg,
		race: race,
		stats: map[string]interface{}{
			"checks":           0,
			"drains_triggered": 0,
			"torrents_removed": 0,
			"bytes_freed":      int64(0),
		},
	}
}

// Start launches the background check goroutine.
func (d *RaceDrain) Start(ctx context.Context) {
	if !d.cfg.Enabled {
		slog.Info("drain: disabled")
		return
	}
	d.running = true

	go func() {
		// Initial delay
		select {
		case <-ctx.Done():
			return
		case <-time.After(10 * time.Second):
		}

		ticker := time.NewTicker(time.Duration(d.cfg.CheckIntervalSeconds) * time.Second)
		defer ticker.Stop()

		for {
			select {
			case <-ctx.Done():
				d.running = false
				return
			case <-ticker.C:
				d.doCheck()
			}
		}
	}()

	slog.Info("drain: started",
		"check_interval", d.cfg.CheckIntervalSeconds,
		"high", d.cfg.HighWatermarkPct,
		"low", d.cfg.LowWatermarkPct,
	)
}

// ---------------------------------------------------------------------------
// Public API — implements RaceDrainService interface
// ---------------------------------------------------------------------------

// GetStatus returns current drain status and disk usage.
func (d *RaceDrain) GetStatus() map[string]interface{} {
	used, total, pct := d.getDiskUsage()

	d.mu.Lock()
	statsCopy := make(map[string]interface{})
	for k, v := range d.stats {
		statsCopy[k] = v
	}
	lastDrain := d.lastDrain
	d.mu.Unlock()

	return map[string]interface{}{
		"enabled":         d.cfg.Enabled,
		"running":         d.running,
		"disk_used_pct":   roundPct(pct),
		"disk_used":       used,
		"disk_total":      total,
		"high_watermark":  d.cfg.HighWatermarkPct,
		"low_watermark":   d.cfg.LowWatermarkPct,
		"check_interval":  d.cfg.CheckIntervalSeconds,
		"min_age_minutes": d.cfg.MinAgeMinutes,
		"last_drain":      lastDrain,
		"stats":           statsCopy,
	}
}

// GetHistory returns the drain event history (most recent first).
func (d *RaceDrain) GetHistory() []map[string]interface{} {
	d.mu.Lock()
	defer d.mu.Unlock()
	out := make([]map[string]interface{}, len(d.history))
	// Reverse order
	for i, h := range d.history {
		out[len(d.history)-1-i] = h
	}
	return out
}

// DrainNow forces an immediate drain check.
func (d *RaceDrain) DrainNow() map[string]interface{} {
	result := d.doCheck()
	if result == nil {
		return map[string]interface{}{"status": "no_drain_needed"}
	}
	return result
}

// ---------------------------------------------------------------------------
// Internal
// ---------------------------------------------------------------------------

func (d *RaceDrain) getDiskUsage() (used, total int64, pct float64) {
	var stat syscall.Statfs_t
	if err := syscall.Statfs(d.cfg.RacePath, &stat); err != nil {
		return 0, 0, 0
	}
	totalBytes := int64(stat.Blocks) * stat.Bsize
	freeBytes := int64(stat.Bavail) * stat.Bsize
	usedBytes := totalBytes - freeBytes
	if totalBytes > 0 {
		pct = float64(usedBytes) / float64(totalBytes) * 100
	}
	return usedBytes, totalBytes, pct
}

func (d *RaceDrain) doCheck() map[string]interface{} {
	d.mu.Lock()
	d.stats["checks"] = d.stats["checks"].(int) + 1
	d.mu.Unlock()

	used, total, pct := d.getDiskUsage()

	if pct < float64(d.cfg.HighWatermarkPct) {
		return nil
	}

	slog.Warn("drain: disk usage above high watermark, starting drain",
		"pct", roundPct(pct),
		"high", d.cfg.HighWatermarkPct,
		"low", d.cfg.LowWatermarkPct,
	)

	targetUsed := float64(total) * float64(d.cfg.LowWatermarkPct) / 100
	toFree := float64(used) - targetUsed
	var freed int64
	var removed []map[string]interface{}

	torrents := d.getRaceTorrentsSorted()

	for _, t := range torrents {
		if float64(freed) >= toFree {
			break
		}

		infoHash, _ := t["info_hash"].(string)
		name, _ := t["name"].(string)
		if name == "" {
			name = infoHash[:16]
		}
		size := toInt64Val(t["total_size"])

		if err := d.race.RemoveTorrent(infoHash, true); err != nil {
			slog.Warn("drain: failed to remove torrent", "name", name, "error", err)
			continue
		}

		freed += size
		removed = append(removed, map[string]interface{}{
			"name": name,
			"size": size,
		})
		slog.Info("drain: removed torrent", "name", name, "size_gb", float64(size)/1e9)
	}

	d.mu.Lock()
	d.stats["drains_triggered"] = d.stats["drains_triggered"].(int) + 1
	d.stats["torrents_removed"] = d.stats["torrents_removed"].(int) + len(removed)
	d.stats["bytes_freed"] = d.stats["bytes_freed"].(int64) + freed
	d.lastDrain = float64(time.Now().Unix())
	d.mu.Unlock()

	_, _, newPct := d.getDiskUsage()

	result := map[string]interface{}{
		"timestamp":     time.Now().Unix(),
		"before_pct":    roundPct(pct),
		"after_pct":     roundPct(newPct),
		"freed":         freed,
		"removed_count": len(removed),
		"removed":       removed,
	}

	d.mu.Lock()
	d.history = append(d.history, result)
	if len(d.history) > 20 {
		d.history = d.history[len(d.history)-20:]
	}
	d.mu.Unlock()

	slog.Info("drain: complete",
		"before_pct", roundPct(pct),
		"after_pct", roundPct(newPct),
		"removed", len(removed),
		"freed_gb", float64(freed)/1e9,
	)

	return result
}

func (d *RaceDrain) getRaceTorrentsSorted() []map[string]interface{} {
	torrents := d.race.GetAllStatus()
	now := float64(time.Now().Unix())
	minAge := float64(d.cfg.MinAgeMinutes) * 60

	var eligible []map[string]interface{}
	for _, t := range torrents {
		addedTime := toFloat64Val(t["added_time"])
		if addedTime <= 0 {
			addedTime = now
		}
		age := now - addedTime
		if age < minAge {
			continue
		}
		eligible = append(eligible, t)
	}

	// Sort by added_time ascending (oldest first)
	for i := 1; i < len(eligible); i++ {
		key := eligible[i]
		keyTime := toFloat64Val(key["added_time"])
		j := i - 1
		for j >= 0 && toFloat64Val(eligible[j]["added_time"]) > keyTime {
			eligible[j+1] = eligible[j]
			j--
		}
		eligible[j+1] = key
	}

	return eligible
}

func roundPct(v float64) float64 {
	return float64(int64(v*10+0.5)) / 10
}

func toFloat64Val(v interface{}) float64 {
	switch n := v.(type) {
	case float64:
		return n
	case float32:
		return float64(n)
	case int:
		return float64(n)
	case int64:
		return float64(n)
	default:
		return 0
	}
}

func toInt64Val(v interface{}) int64 {
	switch n := v.(type) {
	case int64:
		return n
	case int:
		return int64(n)
	case float64:
		return int64(n)
	default:
		return 0
	}
}
