package api

import (
	"fmt"
	"os"
	"strconv"
	"strings"
	"sync"
	"time"
)

// TunnelStats tracks per-tunnel network interface statistics.
type TunnelStats struct {
	mu       sync.Mutex
	tunnels  []tunnelInfo
	lastPoll time.Time
	lastRX   []uint64
	lastTX   []uint64
	rateRX   []float64 // bytes/sec
	rateTX   []float64
}

type tunnelInfo struct {
	Iface string // e.g. "fou0"
	IP    string // e.g. "203.0.113.10"
}

// FOU tunnels are no longer the primary path for peer traffic (incoming
// flows now arrive via the VPS haproxy → the router rdr v6 → the seedbox host PROXY v2
// chain since 2026-04-21). The tunnel iface counters remain read for dial
// outgoing stats but are hidden from the public /api/status payload to avoid
// misleading the UI header.
var globalTunnelStats = &TunnelStats{
	tunnels: []tunnelInfo{},
}

func init() {
	n := len(globalTunnelStats.tunnels)
	globalTunnelStats.lastRX = make([]uint64, n)
	globalTunnelStats.lastTX = make([]uint64, n)
	globalTunnelStats.rateRX = make([]float64, n)
	globalTunnelStats.rateTX = make([]float64, n)
}

func readSysCounter(iface, counter string) (uint64, error) {
	path := fmt.Sprintf("/sys/class/net/%s/statistics/%s", iface, counter)
	data, err := os.ReadFile(path)
	if err != nil {
		return 0, err
	}
	return strconv.ParseUint(strings.TrimSpace(string(data)), 10, 64)
}

// Poll reads current counters and computes rates.
func (ts *TunnelStats) Poll() {
	ts.mu.Lock()
	defer ts.mu.Unlock()

	now := time.Now()
	dt := now.Sub(ts.lastPoll).Seconds()

	for i, t := range ts.tunnels {
		rx, err1 := readSysCounter(t.Iface, "rx_bytes")
		tx, err2 := readSysCounter(t.Iface, "tx_bytes")
		if err1 != nil || err2 != nil {
			continue
		}
		if dt > 0 && ts.lastPoll != (time.Time{}) {
			if rx >= ts.lastRX[i] {
				ts.rateRX[i] = float64(rx-ts.lastRX[i]) / dt
			}
			if tx >= ts.lastTX[i] {
				ts.rateTX[i] = float64(tx-ts.lastTX[i]) / dt
			}
		}
		ts.lastRX[i] = rx
		ts.lastTX[i] = tx
	}
	ts.lastPoll = now
}

// Snapshot returns current tunnel stats for the API.
func (ts *TunnelStats) Snapshot() []map[string]interface{} {
	ts.mu.Lock()
	defer ts.mu.Unlock()

	// Total rate across all tunnels
	var totalTX float64
	for _, r := range ts.rateTX {
		totalTX += r
	}

	result := make([]map[string]interface{}, len(ts.tunnels))
	for i, t := range ts.tunnels {
		pct := 0.0
		if totalTX > 0 {
			pct = ts.rateTX[i] / totalTX * 100
		}
		result[i] = map[string]interface{}{
			"iface":   t.Iface,
			"ip":      t.IP,
			"tx_rate": int64(ts.rateTX[i]), // bytes/sec (upload from Hydra perspective)
			"rx_rate": int64(ts.rateRX[i]), // bytes/sec (download)
			"tx_pct":  int(pct + 0.5),
		}
	}
	return result
}

// StartTunnelPoller launches a goroutine polling tunnel stats every 3 seconds.
func StartTunnelPoller() {
	go func() {
		for {
			globalTunnelStats.Poll()
			time.Sleep(3 * time.Second)
		}
	}()
}

// GetTunnelSnapshot returns the latest tunnel stats.
func GetTunnelSnapshot() []map[string]interface{} {
	return globalTunnelStats.Snapshot()
}
