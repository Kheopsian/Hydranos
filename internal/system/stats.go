package system

import (
	"bufio"
	"os"
	"strconv"
	"strings"
	"sync"
	"time"
)

const (
	arcPath  = "/proc/spl/kstat/zfs/arcstats"
	statPath = "/proc/stat"
)

var (
	mu      sync.Mutex
	prevArc map[string]int64
	prevCPU map[string]int64
	prevTS  float64
)

// Collect reads /proc for ZFS ARC stats and CPU iowait, returning a snapshot.
// Deltas (per-second rates) are computed from the previous call.
func Collect() map[string]interface{} {
	mu.Lock()
	defer mu.Unlock()

	now := float64(time.Now().UnixMilli()) / 1000.0
	dt := now - prevTS
	if dt <= 0 {
		dt = 5.0
	}

	arc := readArcStats()
	cpu := readCPUStats()

	result := map[string]interface{}{
		"available": len(arc) > 0,
	}

	// ARC absolutes
	result["arc_size_bytes"] = arcGet(arc, "size")
	result["arc_c_max_bytes"] = arcGet(arc, "c_max")

	// Cumulative hit rate
	hits := arcGet(arc, "hits")
	misses := arcGet(arc, "misses")
	total := hits + misses
	if total > 0 {
		result["arc_hit_rate_pct"] = round3(float64(hits) / float64(total) * 100)
	} else {
		result["arc_hit_rate_pct"] = 0.0
	}

	// Demand data hit rate
	ddHits := arcGet(arc, "demand_data_hits")
	ddMisses := arcGet(arc, "demand_data_misses")
	ddTotal := ddHits + ddMisses
	if ddTotal > 0 {
		result["arc_demand_hit_rate_pct"] = round3(float64(ddHits) / float64(ddTotal) * 100)
	} else {
		result["arc_demand_hit_rate_pct"] = 0.0
	}

	// Deltas
	if prevArc != nil {
		delta := func(key string) float64 {
			cur := arcGet(arc, key)
			prev := arcGet64(prevArc, key)
			d := float64(cur - prev)
			if d < 0 {
				d = 0
			}
			return d / dt
		}

		result["arc_miss_per_sec"] = round1(delta("misses"))
		result["arc_demand_data_miss_per_sec"] = round1(delta("demand_data_misses"))
		result["arc_ghost_hits_per_sec"] = round1(delta("mfu_ghost_hits") + delta("mru_ghost_hits"))

		dDDH := arcGet(arc, "demand_data_hits") - arcGet64(prevArc, "demand_data_hits")
		dDDM := arcGet(arc, "demand_data_misses") - arcGet64(prevArc, "demand_data_misses")
		dDDT := dDDH + dDDM
		if dDDT > 0 {
			result["arc_demand_hit_rate_delta_pct"] = round2(float64(dDDH) / float64(dDDT) * 100)
		} else {
			result["arc_demand_hit_rate_delta_pct"] = result["arc_demand_hit_rate_pct"]
		}
	} else {
		result["arc_miss_per_sec"] = 0.0
		result["arc_demand_data_miss_per_sec"] = 0.0
		result["arc_ghost_hits_per_sec"] = 0.0
		result["arc_demand_hit_rate_delta_pct"] = result["arc_demand_hit_rate_pct"]
	}

	// IOWait
	if prevCPU != nil && len(cpu) > 0 {
		dIOWait := cpuGet(cpu, "iowait") - cpuGet64(prevCPU, "iowait")
		var dTotal int64
		for k := range cpu {
			dTotal += cpuGet(cpu, k) - cpuGet64(prevCPU, k)
		}
		if dTotal > 0 {
			result["iowait_pct"] = round2(float64(dIOWait) / float64(dTotal) * 100)
		} else {
			result["iowait_pct"] = 0.0
		}
	} else {
		result["iowait_pct"] = 0.0
	}

	// Open FDs
	result["open_fds"] = float64(countFDs())

	// Save for next call
	prevArc = intToInt64Map(arc)
	prevCPU = intToInt64Map(cpu)
	prevTS = now

	return result
}

// countFDs returns the number of open file descriptors for this process.
func countFDs() int64 {
	entries, err := os.ReadDir("/proc/self/fd")
	if err != nil {
		return 0
	}
	return int64(len(entries))
}

// ---------------------------------------------------------------------------
// Readers
// ---------------------------------------------------------------------------

func readArcStats() map[string]int64 {
	stats := map[string]int64{}
	f, err := os.Open(arcPath)
	if err != nil {
		return stats
	}
	defer f.Close()

	scanner := bufio.NewScanner(f)
	for scanner.Scan() {
		parts := strings.Fields(scanner.Text())
		if len(parts) == 3 {
			v, err := strconv.ParseInt(parts[2], 10, 64)
			if err == nil {
				stats[parts[0]] = v
			}
		}
	}
	return stats
}

func readCPUStats() map[string]int64 {
	f, err := os.Open(statPath)
	if err != nil {
		return nil
	}
	defer f.Close()

	scanner := bufio.NewScanner(f)
	keys := []string{"user", "nice", "system", "idle", "iowait", "irq", "softirq"}
	for scanner.Scan() {
		line := scanner.Text()
		if strings.HasPrefix(line, "cpu ") {
			parts := strings.Fields(line)
			result := map[string]int64{}
			for i, k := range keys {
				if i+1 < len(parts) {
					v, _ := strconv.ParseInt(parts[i+1], 10, 64)
					result[k] = v
				}
			}
			return result
		}
	}
	return nil
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

func arcGet(m map[string]int64, key string) int64 {
	if v, ok := m[key]; ok {
		return v
	}
	return 0
}

func arcGet64(m map[string]int64, key string) int64 {
	if v, ok := m[key]; ok {
		return v
	}
	return 0
}

func cpuGet(m map[string]int64, key string) int64 {
	if v, ok := m[key]; ok {
		return v
	}
	return 0
}

func cpuGet64(m map[string]int64, key string) int64 {
	if v, ok := m[key]; ok {
		return v
	}
	return 0
}

func intToInt64Map(m map[string]int64) map[string]int64 {
	if m == nil {
		return nil
	}
	out := make(map[string]int64, len(m))
	for k, v := range m {
		out[k] = v
	}
	return out
}

func round1(v float64) float64 {
	return float64(int64(v*10+0.5)) / 10
}

func round2(v float64) float64 {
	return float64(int64(v*100+0.5)) / 100
}

func round3(v float64) float64 {
	return float64(int64(v*1000+0.5)) / 1000
}
