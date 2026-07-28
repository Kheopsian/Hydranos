package health

import (
	"fmt"
	"runtime"
	"runtime/metrics"
	"time"
)

// Process-level runtime invariant (added 2026-07-22 after the goroutine-per-
// torrent CPU incident: hydra-go had grown ~63k announce goroutines and the GC
// scan of their stacks burned ~7 cores, invisible for 18 days of uptime).
// Unlike the per-torrent physics invariants, these are conservation laws on the
// PROCESS: the goroutine count must stay O(1) in torrent count, and GC must not
// dominate the CPU. Same discipline: grounded in a real incident, edge-triggered.
const InvRuntimeBloat = "runtime_bloat"

const (
	// Post-scheduler steady state is ~2k goroutines, DECOUPLED from torrent
	// count. A climb past these ceilings means a leak or a re-introduced
	// per-entity goroutine pattern (the broken announce model peaked ~63k).
	goroutineWarn  = 15000
	goroutineAlert = 30000
	// GC eating this share of TOTAL CPU capacity = allocation/scan pathology.
	// The incident sat ~15% (scanobject); this is the edge, the goroutine
	// ceiling fires far earlier as the primary signal.
	gcCPUWarnPct = 15
)

// checkRuntime evaluates the process-level runtime invariants once per scan and
// writes the gauges (+ any threshold-crossing anomaly) into rep. Cheap: a
// couple of runtime calls, no iteration.
func (s *Scanner) checkRuntime(rep *Report) {
	ng := runtime.NumGoroutine()
	rep.Goroutines = ng

	// Windowed GC CPU fraction. runtime.MemStats.GCCPUFraction is cumulative
	// since boot (useless after long uptime), so we window /cpu/classes/gc
	// ourselves: delta of GC cpu-seconds over the wall window, normalised by
	// GOMAXPROCS = fraction of total available CPU spent in GC.
	var gcPct int
	samples := []metrics.Sample{{Name: "/cpu/classes/gc/total:cpu-seconds"}}
	metrics.Read(samples)
	if samples[0].Value.Kind() == metrics.KindFloat64 {
		gcNow := samples[0].Value.Float64()
		now := time.Now()
		if !s.prevGCAt.IsZero() {
			dt := now.Sub(s.prevGCAt).Seconds()
			procs := float64(runtime.GOMAXPROCS(0))
			if dt > 0 && procs > 0 {
				frac := (gcNow - s.prevGCSeconds) / (dt * procs)
				if frac < 0 {
					frac = 0
				}
				gcPct = int(frac*100 + 0.5)
			}
		}
		s.prevGCSeconds = gcNow
		s.prevGCAt = now
	}
	rep.GCCPUPct = gcPct

	if ng >= goroutineWarn {
		sev := SevWarn
		if ng >= goroutineAlert {
			sev = SevHigh
		}
		rep.add(Anomaly{
			Type:     InvRuntimeBloat,
			Engine:   "runtime",
			Severity: sev,
			Detail:   fmt.Sprintf("%d goroutines (attendu O(1) en nb de torrents ; fuite / pattern par-torrent ?)", ng),
		})
	}
	if gcPct >= gcCPUWarnPct {
		rep.add(Anomaly{
			Type:     InvRuntimeBloat,
			Engine:   "runtime",
			Severity: SevWarn,
			Detail:   fmt.Sprintf("GC = %d%% du CPU (pression allocation/scan)", gcPct),
		})
	}
}
