// Package health turns Hydra's incident history into standing invariants.
//
// Hydra is richly instrumented for PERFORMANCE (bench.db throughput/peers/ARC)
// but poor at SANITY/INTEGRITY observability ("is it working *correctly*", not
// "*fast*"). Every past bug had to be chased with an ad-hoc Python script
// (redl_check, ghost_scan, verify_pieces) — those scripts ARE the missing
// observability. Rather than one probe per known bug (infinite, always late),
// we encode the conservation laws BitTorrent physics imposes and flag any
// torrent that violates them. One invariant catches a whole *class* of future
// bugs: the fake-seed-wrapper bug and the ghost re-download bug are two
// different causes that violate the SAME invariant ("seeding but serves/holds
// nothing"), so a single invariant would have caught both.
//
// Discipline: only invariants grounded in a real past incident or in BitTorrent
// physics — not speculative probes — and alerting is edge-triggered (fires on a
// high-severity condition appearing/worsening), never a heartbeat.
package health

import (
	"context"
	"fmt"
	"log/slog"
	"net/url"
	"os"
	"sort"
	"strings"
	"sync"
	"time"

	"github.com/Kheopsian/hydra/internal/engine/ltclient"
)

// Invariant identifiers (the class of bug each one catches).
const (
	// InvReDL: dl/size ≈ 1. Downloading far more bytes than the torrent's own
	// size means pieces were re-fetched (corruption loop, ghost re-DL). This is
	// the invariant that would have screamed "80 GB downloaded for 3 GB".
	InvReDL = "redl"
	// InvFakeSeed: "seeding ⇒ I actually hold the data". Advertising the seeding
	// state while not genuinely complete is the fake-seed-wrapper / left=0 class:
	// we announce pieces we cannot serve.
	InvFakeSeed = "fake_seed"
	// InvStarved: a leecher, tracker OK, swarm has seeds, yet 0 peers connected.
	// Exactly the shape of the left=0 announce bug (tracker withheld the peer
	// list because we mislabelled ourselves a seed) — the leech never started.
	InvStarved = "starved"
	// InvFilesMissing: "expected file present". A file we are supposed to hold
	// is gone from disk (surfaced from the hardlink monitor's ghost detection).
	InvFilesMissing = "files_missing"
	// InvDualSeed: the same infohash seeded by BOTH engines. Wasteful — torr9
	// credits UL as MAX per (user,torrent), not the sum, and it splits demand
	// across two peers. This is the concern the 2.7.0 anti-dual-announce work
	// addressed; the invariant flags any leak of it.
	InvDualSeed = "dual_seed"
	// InvTrackerFrozen: a persistent tracker error while the tracker itself is
	// healthy (most other torrents on the same host announce fine). A paused/
	// parked torrent does not re-announce, so a boot-time failure freezes
	// forever — the 13.6k frozen-error incident (2.7.7 slot-manager catch-22).
	// Only flagged once it has survived several scans AND its host is not in a
	// full outage (so a tracker going down does not mass-fire this).
	InvTrackerFrozen = "tracker_error_frozen"
	// InvTrackerOutage: a whole tracker host is erroring (most of its torrents).
	// This is an EXTERNAL outage, not a Hydra integrity bug — collapsed into one
	// entry per host and deliberately kept OUT of the ntfy alert path so a
	// tracker maintenance window doesn't spam the phone.
	InvTrackerOutage = "tracker_outage"
	// InvGhost: an ACTIVE torrent (downloading/parked, exchanging with peers)
	// whose save_path has vanished from disk. This is THE recurrent ghost bug
	// (feedback hydra-ghost-torrents-redl-recurrent): the file is deleted under
	// the torrent, every received piece fails SHA1, it re-requests forever —
	// invisible in total_download (thrown pieces aren't counted), so `redl` does
	// NOT catch it. Only a disk stat does. This is the 80-GB-wasted catcher.
	InvGhost = "ghost"
)

// Severity levels.
const (
	SevHigh = "high"
	SevWarn = "warn"
)

const (
	// A complete torrent that downloaded >20% more than its own size has
	// re-fetched data it already had. Conservative band to avoid flagging the
	// normal few-percent overhead of hash-fail re-requests.
	redlFactor = 1.20
	// Re-download waste above this is a hard anomaly, below it a warning.
	wastedHighBytes = 1 << 30 // 1 GiB
	// Ignore re-download below this per torrent: a 1-2 piece torrent (ebook) that
	// re-requests a single piece trivially reads as 1.2-2x its size — noise. The
	// real re-DL offenders waste hundreds of MiB to GiB. Aggregate efficiency
	// still accounts for the small stuff.
	redlFloorBytes = 50 << 20 // 50 MiB
	// Cap the anomaly list in a single report so a pathological run can't return
	// a multi-megabyte payload. Counts stay exact; the list is flagged truncated.
	maxAnomaliesReturned = 500
	// A condition must survive this many consecutive scans before it counts as
	// "persistent" (escalates severity / becomes alertable). At a 5-min tick,
	// 2 scans ≈ 10 min — long enough to rule out boot-time / transient noise.
	persistScans = 2
	// A tracker host with at least this many torrents, of which this fraction or
	// more are erroring, is treated as an external OUTAGE (one collapsed entry,
	// no ntfy) rather than per-torrent frozen anomalies.
	outageMinTorrents = 20
	outageErrFraction = 0.5

	// Alert trigger thresholds (edge-triggered; see decideAlert).
	alertTrackerFrozen = 50      // systemic frozen-error incident
	alertWastedBytes   = 5 << 30 // 5 GiB re-download storm
	alertEfficiencyLow = 0.80
)

// Anomaly is one invariant violation on one torrent.
type Anomaly struct {
	Type        string `json:"type"`
	Engine      string `json:"engine"` // "hoard" | "race"
	InfoHash    string `json:"info_hash"`
	Name        string `json:"name"`
	Severity    string `json:"severity"`
	Detail      string `json:"detail"`
	WastedBytes int64  `json:"wasted_bytes,omitempty"`
}

// Report is the outcome of one scan.
type Report struct {
	GeneratedAt  int64          `json:"generated_at"`
	DurationMs   int64          `json:"scan_duration_ms"`
	ScannedHoard int            `json:"scanned_hoard"`
	ScannedRace  int            `json:"scanned_race"`
	Counts       map[string]int `json:"counts"`
	WastedBytes  int64          `json:"wasted_bytes"` // ACTIVE re-DL overshoot only, both engines (drives alerts)
	// Historical re-DL: settled seeds whose lifetime download exceeded their size
	// — sunk cost, not actionable, deliberately kept out of wasted_bytes/alerts.
	ReDLHistorical      int              `json:"redl_historical"`
	ReDLHistoricalBytes int64            `json:"redl_historical_bytes"`
	Efficiency          float64          `json:"efficiency"`   // useful/exchanged over both engines, ~1 = healthy
	GhostFiles          int              `json:"ghost_files"`  // media present, source gone (hardlink monitor)
	OrphanFiles         int              `json:"orphan_files"` // DL present, not linked into media
	Anomalies           []Anomaly        `json:"anomalies"`
	Truncated           bool             `json:"anomalies_truncated"`
	Persistent          map[string]int64 `json:"persistent_counters"` // survive restart
	Goroutines          int              `json:"goroutines"`
	GCCPUPct            int              `json:"gc_cpu_pct"`
	Errors              []string         `json:"errors,omitempty"`
}

func (r *Report) add(a Anomaly) {
	r.Counts[a.Type]++
	if a.Type == InvReDL {
		r.WastedBytes += a.WastedBytes
	}
	if len(r.Anomalies) < maxAnomaliesReturned {
		r.Anomalies = append(r.Anomalies, a)
	} else {
		r.Truncated = true
	}
}

// Lister returns the raw per-torrent status list of one engine.
type Lister func() ([]ltclient.TorrentStatus, error)

// SummaryFn returns the hardlink monitor summary map (may be nil-returning).
type SummaryFn func() map[string]interface{}

// Alerter pushes a notification (e.g. ntfy). priority is min/low/default/high/
// urgent; tags is a comma-separated tag list.
type Alerter func(title, message, priority, tags string)

// CounterStore persists cumulative health counters across restarts. Satisfied
// by *bench.BenchDB. Decoupled via interface so health does not import bench.
type CounterStore interface {
	PersistHealth(gauges, maxes, increments map[string]int64)
	GetHealthCounters() map[string]int64
}

// Scanner runs the invariant checks on a tick and caches the latest report.
type Scanner struct {
	hoardList Lister
	raceList  Lister
	summary   SummaryFn
	store     CounterStore
	alert     Alerter

	mu   sync.RWMutex
	last *Report
	// seen tracks how many consecutive scans a "type:infohash" condition has
	// been present, so persistent conditions can escalate and transient ones
	// are ignored.
	seen map[string]int
	// lastAlertSig is the bucketed signature of the last alert sent; a new alert
	// fires only when the signature changes (edge-triggered, no per-tick spam).
	lastAlertSig string
	// runtime GC-CPU windowing for the process-level invariant (checkRuntime).
	prevGCSeconds float64
	prevGCAt      time.Time
}

// NewScanner wires the scanner. summary, store and alert may be nil.
func NewScanner(hoard, race Lister, summary SummaryFn, store CounterStore, alert Alerter) *Scanner {
	return &Scanner{
		hoardList: hoard, raceList: race, summary: summary, store: store, alert: alert,
		seen: map[string]int{},
	}
}

// Run scans once per interval until ctx is cancelled. The first scan fires after
// one interval so the engines have settled (announces converge post-boot).
func (s *Scanner) Run(ctx context.Context, interval time.Duration) {
	t := time.NewTicker(interval)
	defer t.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-t.C:
			s.Scan()
		}
	}
}

// Scan computes invariant violations across both engines, persists the
// cumulative counters, fires an edge-triggered alert if warranted, and caches
// the report. Returns the fresh report.
func (s *Scanner) Scan() *Report {
	start := time.Now()
	rep := &Report{GeneratedAt: start.Unix(), Counts: map[string]int{}}

	// nowSeen accumulates the persistent-condition keys present THIS scan; it
	// replaces s.seen at the end so vanished conditions reset their counter.
	nowSeen := map[string]int{}
	hoardHashes := map[string]string{} // infohash -> name (for dual-seed intersection)

	// Per-tracker-host accounting, to tell an external OUTAGE (most torrents on
	// a host erroring) from genuine per-torrent FROZEN errors (host healthy).
	trackerTotal := map[string]int{}
	trackerErr := map[string]int{}
	type frozenCand struct{ engine, infoHash, name, host, msg string }
	var frozenCands []frozenCand

	var usefulBytes, exchangedBytes int64
	scanEngine := func(name string, list Lister, hashes map[string]string) int {
		if list == nil {
			return 0
		}
		torrents, err := list()
		if err != nil {
			rep.Errors = append(rep.Errors, fmt.Sprintf("%s: %v", name, err))
			return 0
		}
		for i := range torrents {
			t := &torrents[i]
			if hashes != nil {
				hashes[t.InfoHash] = t.Name
			}
			s.inspect(t, name, rep, nowSeen)
			// Tracker error accounting (resolved into outage vs frozen below).
			host := trackerHost(t.CurrentTracker)
			trackerTotal[host]++
			if t.TrackerError {
				trackerErr[host]++
				frozenCands = append(frozenCands, frozenCand{name, t.InfoHash, t.Name, host, t.TrackerErrorMsg})
			}
			if t.TotalDownload > 0 {
				exchangedBytes += t.TotalDownload
				useful := t.TotalDone
				if useful > t.TotalSize {
					useful = t.TotalSize
				}
				usefulBytes += useful
			}
		}
		return len(torrents)
	}

	rep.ScannedHoard = scanEngine("hoard", s.hoardList, hoardHashes)
	// Race torrents also checked for dual-seed against the hoard set.
	raceHashes := map[string]string{}
	rep.ScannedRace = scanEngine("race", s.raceList, raceHashes)

	// Resolve tracker errors: a host past the outage threshold is an EXTERNAL
	// outage (one collapsed entry, not alerted); otherwise each persistent error
	// on a HEALTHY host is a genuine frozen anomaly (the 2.7.7 signature).
	outageHosts := map[string]bool{}
	for host, total := range trackerTotal {
		if total >= outageMinTorrents && float64(trackerErr[host]) >= outageErrFraction*float64(total) {
			outageHosts[host] = true
			rep.add(Anomaly{
				Type: InvTrackerOutage, Engine: "both", Name: host, Severity: SevWarn,
				Detail: fmt.Sprintf("%d/%d torrents on %s erroring — external tracker outage (not alerted)",
					trackerErr[host], total, host),
			})
		}
	}
	for _, fc := range frozenCands {
		if outageHosts[fc.host] {
			continue // external outage, already collapsed
		}
		n := s.bumpSeen(nowSeen, InvTrackerFrozen, fc.infoHash)
		if n >= persistScans {
			rep.add(Anomaly{
				Type: InvTrackerFrozen, Engine: fc.engine, InfoHash: fc.infoHash, Name: fc.name,
				Severity: SevWarn,
				Detail:   fmt.Sprintf("tracker error frozen %d scan(s) on healthy host %s: %s", n, fc.host, fc.msg),
			})
		}
	}

	// INV_DUAL_SEED — same infohash held by both engines.
	for ih, name := range raceHashes {
		if _, ok := hoardHashes[ih]; ok {
			rep.add(Anomaly{
				Type: InvDualSeed, Engine: "both", InfoHash: ih, Name: name,
				Severity: SevWarn,
				Detail:   "infohash held by both hoard and race — UL credited as MAX not sum (2.7.0)",
			})
		}
	}

	// Efficiency: useful (data we keep) / exchanged (data we pulled). Drops well
	// below 1 when the engines re-download pieces they already had.
	if exchangedBytes > 0 {
		rep.Efficiency = float64(usefulBytes) / float64(exchangedBytes)
	} else {
		rep.Efficiency = 1
	}

	// "Expected file present": reuse the hardlink monitor's ghost detection
	// (media present, DL source gone) — hoard only.
	if s.summary != nil {
		if sum := s.summary(); sum != nil {
			rep.GhostFiles = asInt(sum["ghost_files"])
			rep.OrphanFiles = asInt(sum["orphan"])
			if rep.GhostFiles > 0 {
				rep.add(Anomaly{
					Type: InvFilesMissing, Engine: "hoard", Severity: SevWarn,
					Detail: fmt.Sprintf("%d media file(s) whose download source is gone (ghost); %d orphan download(s)",
						rep.GhostFiles, rep.OrphanFiles),
				})
			}
		}
	}

	rep.DurationMs = time.Since(start).Milliseconds()

	s.mu.Lock()
	s.seen = nowSeen
	s.mu.Unlock()

	s.checkRuntime(rep)
	s.persist(rep)
	s.maybeAlert(rep)

	s.mu.Lock()
	s.last = rep
	s.mu.Unlock()

	total := 0
	for _, c := range rep.Counts {
		total += c
	}
	slog.Info("health: scan complete",
		"hoard", rep.ScannedHoard, "race", rep.ScannedRace,
		"anomalies", total, "wasted_bytes", rep.WastedBytes,
		"efficiency", fmt.Sprintf("%.3f", rep.Efficiency), "ms", rep.DurationMs)
	return rep
}

// inspect applies every per-torrent invariant to one torrent. nowSeen records
// persistent-condition keys present this scan.
func (s *Scanner) inspect(t *ltclient.TorrentStatus, engine string, rep *Report, nowSeen map[string]int) {
	// INV_REDL — downloaded far more than its own size (above the noise floor).
	// Split ACTIVE (still pulling bytes now = actionable, alerts, feeds
	// wasted_bytes) from HISTORICAL (a settled seed whose over-download is a
	// sunk-cost scar = informational only, no alert, out of wasted_bytes so 4
	// old scars don't pin the alert band above threshold forever).
	if t.TotalSize > 0 && t.TotalDownload > int64(float64(t.TotalSize)*redlFactor) &&
		t.TotalDownload-t.TotalSize >= redlFloorBytes {
		wasted := t.TotalDownload - t.TotalSize
		if t.DownloadRate > 0 || t.State == "downloading" {
			sev := SevWarn
			if wasted >= wastedHighBytes {
				sev = SevHigh
			}
			rep.add(Anomaly{
				Type: InvReDL, Engine: engine, InfoHash: t.InfoHash, Name: t.Name,
				Severity: sev, WastedBytes: wasted,
				Detail: fmt.Sprintf("ACTIVELY re-downloading: %d B pulled for a %d B torrent (%.2fx)",
					t.TotalDownload, t.TotalSize, float64(t.TotalDownload)/float64(t.TotalSize)),
			})
		} else {
			rep.ReDLHistorical++
			rep.ReDLHistoricalBytes += wasted
		}
	}

	// INV_FAKE_SEED — advertises seeding but is not genuinely complete.
	if t.IsSeeding && (!t.IsFinished || t.Progress < 0.999 ||
		(t.TotalSize > 0 && t.TotalDone < t.TotalSize)) {
		rep.add(Anomaly{
			Type: InvFakeSeed, Engine: engine, InfoHash: t.InfoHash, Name: t.Name,
			Severity: SevHigh,
			Detail: fmt.Sprintf("seeding flag set but incomplete: progress=%.3f done=%d/%d finished=%v",
				t.Progress, t.TotalDone, t.TotalSize, t.IsFinished),
		})
	}

	// INV_STARVED — leeching, tracker OK, swarm has seeds, yet 0 peers connected.
	// Escalates to high only when persistent (a transient starve is normal churn).
	if !t.IsSeeding && !t.IsPaused && t.IsAnnounced && !t.TrackerError &&
		t.NumSeeds > 0 && t.NumPeers == 0 {
		n := s.bumpSeen(nowSeen, InvStarved, t.InfoHash)
		sev := SevWarn
		if n >= persistScans {
			sev = SevHigh
		}
		rep.add(Anomaly{
			Type: InvStarved, Engine: engine, InfoHash: t.InfoHash, Name: t.Name,
			Severity: sev,
			Detail: fmt.Sprintf("leeching, tracker OK, %d swarm seed(s), 0 peers connected for %d scan(s)",
				t.NumSeeds, n),
		})
	}

	// INV_GHOST — an active, exchanging torrent whose content path is gone from
	// disk. THE recurrent ghost bug: pieces fail SHA1 forever, peers upload into
	// the void, invisible in total_download. Scoped to non-seeding torrents that
	// are actually exchanging so we neither stat all 62k nor flag a never-started
	// parked torrent (whose files legitimately don't exist yet). Seeding torrents
	// are deliberately NOT stat-checked here (27k stats/tick = ZFS I/O storm);
	// their integrity is covered by fake_seed instead.
	if !t.IsSeeding && !t.IsFinished && t.SavePath != "" &&
		(t.NumPeers > 0 || t.DownloadRate > 0 || t.TotalDownload > 0) {
		if _, err := os.Stat(t.SavePath); os.IsNotExist(err) {
			n := s.bumpSeen(nowSeen, InvGhost, t.InfoHash)
			sev := SevWarn
			if n >= persistScans {
				sev = SevHigh
			}
			rep.add(Anomaly{
				Type: InvGhost, Engine: engine, InfoHash: t.InfoHash, Name: t.Name,
				Severity: sev,
				Detail: fmt.Sprintf("active (%d peers, dl_rate %d) but save_path absent %d scan(s): %s",
					t.NumPeers, t.DownloadRate, n, t.SavePath),
			})
		}
	}
	// INV_TRACKER_FROZEN is resolved in Scan (needs per-host outage context).
}

// trackerHost extracts the host from a tracker announce URL, for grouping
// errors by tracker. Returns "unknown" when the URL is empty/unparseable.
func trackerHost(tracker string) string {
	if tracker == "" {
		return "unknown"
	}
	if u, err := url.Parse(tracker); err == nil && u.Host != "" {
		return u.Hostname()
	}
	return "unknown"
}

// bumpSeen increments the consecutive-scan counter for a persistent condition
// and returns the new count.
func (s *Scanner) bumpSeen(nowSeen map[string]int, typ, infoHash string) int {
	key := typ + ":" + infoHash
	s.mu.RLock()
	prev := s.seen[key]
	s.mu.RUnlock()
	n := prev + 1
	nowSeen[key] = n
	return n
}

// persist writes cumulative counters to the store (survives restart).
func (s *Scanner) persist(rep *Report) {
	if s.store == nil {
		return
	}
	gauges := map[string]int64{
		"redl_current":            int64(rep.Counts[InvReDL]),
		"fake_seed_current":       int64(rep.Counts[InvFakeSeed]),
		"starved_current":         int64(rep.Counts[InvStarved]),
		"ghost_current":           int64(rep.Counts[InvGhost]),
		"files_missing_current":   int64(rep.Counts[InvFilesMissing]),
		"dual_seed_current":       int64(rep.Counts[InvDualSeed]),
		"tracker_frozen_current":  int64(rep.Counts[InvTrackerFrozen]),
		"tracker_outage_current":  int64(rep.Counts[InvTrackerOutage]),
		"wasted_bytes_current":    rep.WastedBytes,
		"redl_historical_current": int64(rep.ReDLHistorical),
		"redl_historical_bytes":   rep.ReDLHistoricalBytes,
		"efficiency_milli":        int64(rep.Efficiency * 1000),
		"ghost_files_current":     int64(rep.GhostFiles),
	}
	maxes := map[string]int64{
		"wasted_bytes_peak":   rep.WastedBytes,
		"redl_peak":           int64(rep.Counts[InvReDL]),
		"fake_seed_peak":      int64(rep.Counts[InvFakeSeed]),
		"ghost_peak":          int64(rep.Counts[InvGhost]),
		"tracker_frozen_peak": int64(rep.Counts[InvTrackerFrozen]),
	}
	total := 0
	for _, c := range rep.Counts {
		total += c
	}
	increments := map[string]int64{"scans_total": 1, "anomalies_seen_total": int64(total)}
	s.store.PersistHealth(gauges, maxes, increments)
	rep.Persistent = s.store.GetHealthCounters()
}

// maybeAlert fires an edge-triggered notification: only when the bucketed
// signature of high-severity conditions changes (appears or worsens). A healthy
// scan clears the signature, so a recurrence re-alerts.
func (s *Scanner) maybeAlert(rep *Report) {
	if s.alert == nil {
		return
	}
	sig, lines, urgent := s.decideAlert(rep)
	if sig == "" || sig == s.lastAlertSig {
		s.lastAlertSig = sig
		return
	}
	s.lastAlertSig = sig
	priority, tags := "high", "warning"
	if urgent {
		priority, tags = "urgent", "rotating_light"
	}
	s.alert("Hydra: anomalies santé", strings.Join(lines, "\n"), priority, tags)
	slog.Warn("health: alert fired", "signature", sig, "urgent", urgent)
}

// decideAlert returns the bucketed signature, human lines, and urgency for the
// current report. Empty signature = nothing worth alerting.
func (s *Scanner) decideAlert(rep *Report) (string, []string, bool) {
	var sigs, lines []string
	urgent := false
	add := func(sig, line string) { sigs = append(sigs, sig); lines = append(lines, line) }

	// Ghost = the recurrent 80-GB bug; page as soon as one has persisted (high).
	if g := highSeverityCount(rep, InvGhost); g > 0 {
		add(fmt.Sprintf("ghost=%s", bucket(g)), fmt.Sprintf("🔴 %d torrent(s) fantôme(s): actifs mais fichier disparu (re-DL infini)", g))
		urgent = true
	}
	if n := rep.Counts[InvFakeSeed]; n > 0 {
		add(fmt.Sprintf("fake_seed=%s", bucket(n)), fmt.Sprintf("⚠ %d faux-seed (annonce des pièces non servables)", n))
		urgent = true
	}
	if n := rep.Counts[InvTrackerFrozen]; n >= alertTrackerFrozen {
		add(fmt.Sprintf("tracker_frozen=%s", bucket(n)), fmt.Sprintf("⚠ %d erreurs tracker gelées", n))
	}
	if rep.WastedBytes >= alertWastedBytes {
		add(fmt.Sprintf("wasted=%s", wastedBucket(rep.WastedBytes)), fmt.Sprintf("⚠ re-DL storm: %.1f Gio gaspillés", float64(rep.WastedBytes)/float64(1<<30)))
	}
	if rep.Efficiency < alertEfficiencyLow {
		add(fmt.Sprintf("eff=%.1f", rep.Efficiency), fmt.Sprintf("⚠ efficacité %.2f (octets utiles/échangés)", rep.Efficiency))
		if rep.Efficiency < 0.5 {
			urgent = true
		}
	}
	if rep.Goroutines >= goroutineAlert {
		add(fmt.Sprintf("goroutines=%s", bucket(rep.Goroutines)), fmt.Sprintf("🔴 %d goroutines (fuite / pattern par-torrent ?)", rep.Goroutines))
		urgent = true
	} else if rep.Goroutines >= goroutineWarn {
		add(fmt.Sprintf("goroutines=%s", bucket(rep.Goroutines)), fmt.Sprintf("⚠ %d goroutines (croissance à surveiller)", rep.Goroutines))
	}
	if rep.GCCPUPct >= gcCPUWarnPct {
		add(fmt.Sprintf("gccpu=%d", rep.GCCPUPct/5*5), fmt.Sprintf("⚠ GC = %d%% du CPU", rep.GCCPUPct))
	}
	if len(sigs) == 0 {
		return "", nil, false
	}
	sort.Strings(sigs)
	return strings.Join(sigs, "|"), lines, urgent
}

// bucket maps a count to a coarse band so the alert signature only changes on a
// meaningful worsening, not every +1.
func bucket(n int) string {
	switch {
	case n >= 1000:
		return "1k+"
	case n >= 100:
		return "100+"
	case n >= 10:
		return "10+"
	default:
		return "1+"
	}
}

// wastedBucket maps a byte count to a coarse GiB band, so a re-DL that keeps
// growing doesn't re-alert on every extra GiB (the double-alert bug). Only a
// jump into the next band changes the signature.
func wastedBucket(b int64) string {
	g := b >> 30
	switch {
	case g >= 500:
		return "500G+"
	case g >= 250:
		return "250G+"
	case g >= 100:
		return "100G+"
	case g >= 50:
		return "50G+"
	case g >= 25:
		return "25G+"
	case g >= 10:
		return "10G+"
	default:
		return "5G+"
	}
}

// highSeverityCount counts anomalies of a given type at high severity.
func highSeverityCount(rep *Report, typ string) int {
	n := 0
	for _, a := range rep.Anomalies {
		if a.Type == typ && a.Severity == SevHigh {
			n++
		}
	}
	return n
}

// Snapshot returns the latest report as a map for the API handler. If no scan
// has run yet it triggers one synchronously.
func (s *Scanner) Snapshot() map[string]interface{} {
	s.mu.RLock()
	last := s.last
	s.mu.RUnlock()
	if last == nil {
		last = s.Scan()
	}
	return map[string]interface{}{
		"generated_at":          last.GeneratedAt,
		"goroutines":            last.Goroutines,
		"gc_cpu_pct":            last.GCCPUPct,
		"scan_duration_ms":      last.DurationMs,
		"scanned_hoard":         last.ScannedHoard,
		"scanned_race":          last.ScannedRace,
		"counts":                last.Counts,
		"wasted_bytes":          last.WastedBytes,
		"redl_historical":       last.ReDLHistorical,
		"redl_historical_bytes": last.ReDLHistoricalBytes,
		"efficiency":            last.Efficiency,
		"ghost_files":           last.GhostFiles,
		"orphan_files":          last.OrphanFiles,
		"anomalies":             last.Anomalies,
		"anomalies_truncated":   last.Truncated,
		"persistent_counters":   last.Persistent,
		"errors":                last.Errors,
	}
}

func asInt(v interface{}) int {
	switch n := v.(type) {
	case int:
		return n
	case int64:
		return int(n)
	case float64:
		return int(n)
	default:
		return 0
	}
}
