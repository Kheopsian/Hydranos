package engine

// Per-disk seed-slot manager (HDD anti-thrash / quiet mode).
//
// Goal: keep a spinning disk quiet by bounding how many torrents actively
// serve pieces from it at once (fewer concurrent read regions = less head
// seeking = less noise). Faithful port of Pandicorn's qbit-manager hard tier,
// but the lever is serving-suspend (force-choke, seedtime preserved) instead
// of a full pause, and the "waiting queue" is tracked here in Go rather than
// via a client tag.
//
// Opt-in: disabled unless [hoard.disk_slots] enabled=true, and only disks
// explicitly listed are regulated (an unlisted disk is never touched). On
// Windows the disk key is the drive letter. Linux path-prefix groups and pool
// resolvers are later additions.
//
// The I/O% "soft" tier and the readahead window from the original design are
// intentionally out of V1 (see project memory): the hard count cap is what
// actually delivers quiet; a per-disk read elevator (Rust) is the companion
// (V1.1).

import (
	"log/slog"
	"sort"
	"strings"
	"sync"
	"time"

	"github.com/Kheopsian/hydra/internal/config"
)

// The user-facing config lives in package config (so SessionConfig can embed
// it); alias it here to keep this file's references unqualified.
type (
	DiskSlotsConfig = config.DiskSlotsConfig
	DiskEntry       = config.DiskEntry
)

func entryIsSSD(e DiskEntry) bool { return strings.EqualFold(e.Type, "ssd") }

func diskSlotsWithDefaults(c DiskSlotsConfig) DiskSlotsConfig {
	d := c
	if d.DefaultMaxActive <= 0 {
		d.DefaultMaxActive = 10
	}
	if d.SuperSeedThreshold <= 0 {
		d.SuperSeedThreshold = 3
	}
	if d.CycleSeconds <= 0 {
		d.CycleSeconds = 30
	}
	if d.PauseCooldownSec <= 0 {
		d.PauseCooldownSec = 90
	}
	if d.WakeCooldownSec <= 0 {
		d.WakeCooldownSec = 60
	}
	if d.WarmupSec <= 0 {
		d.WarmupSec = 240
	}
	if d.WakeAgingBonusPerMin <= 0 {
		d.WakeAgingBonusPerMin = 0.05
	}
	return d
}

// SlotTorrent is the per-torrent snapshot the manager reasons over. The caller
// builds it from the hoard engine status (num_seeds=scrape_seeders,
// list_peers=scrape_leechers, upload_rate, save_path, is_seeding && !is_paused).
type SlotTorrent struct {
	InfoHash       string
	SavePath       string
	UploadRate     int64
	ScrapeSeeders  int
	ScrapeLeechers int
	// Seeding is true when the torrent is a completed seeder eligible for
	// regulation (not downloading, not manually stopped by the user).
	Seeding bool
}

// diskConf is the resolved per-disk regulation parameters.
type diskConf struct {
	regulated bool
	ssd       bool
	maxActive int
	wakeBelow int
}

// DiskSlotManager holds the regulation state across cycles.
type DiskSlotManager struct {
	cfg   DiskSlotsConfig
	disks map[string]DiskEntry // keyed by canonical drive key

	suspendFn func(infoHash string, suspended bool)

	mu           sync.Mutex
	suspended    map[string]bool
	warmupUntil  map[string]time.Time
	waitingSince map[string]time.Time
	lastPauseAt  map[string]time.Time
	lastWakeAt   map[string]time.Time
}

// NewDiskSlotManager builds a manager. suspendFn(ih, true) suspends serving,
// (ih, false) resumes; it must be idempotent.
func NewDiskSlotManager(cfg DiskSlotsConfig, suspendFn func(string, bool)) *DiskSlotManager {
	c := diskSlotsWithDefaults(cfg)
	disks := make(map[string]DiskEntry, len(c.Disks))
	for _, e := range c.Disks {
		disks[driveKey(e.Key)] = e
	}
	return &DiskSlotManager{
		cfg:          c,
		disks:        disks,
		suspendFn:    suspendFn,
		suspended:    map[string]bool{},
		warmupUntil:  map[string]time.Time{},
		waitingSince: map[string]time.Time{},
		lastPauseAt:  map[string]time.Time{},
		lastWakeAt:   map[string]time.Time{},
	}
}

// CycleInterval is how often the caller should invoke Tick.
func (m *DiskSlotManager) CycleInterval() time.Duration {
	return time.Duration(m.cfg.CycleSeconds) * time.Second
}

// driveKey extracts a Windows drive-letter key ("D:") from a path or a bare
// key, upper-cased (drive letters are case-insensitive). Implemented without
// filepath.VolumeName so it behaves identically regardless of the build OS
// (tests run on Linux; the binary that uses this runs on Windows). Paths with
// no drive letter (Linux) return "" — path-prefix / pool grouping is a later
// addition.
func driveKey(s string) string {
	if len(s) >= 2 && s[1] == ':' {
		c := s[0]
		if (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') {
			return strings.ToUpper(s[:2])
		}
	}
	return ""
}

// resolve returns the regulation config for a drive key, or regulated=false if
// the disk is not listed (unlisted disks are never touched).
func (m *DiskSlotManager) resolve(drive string) diskConf {
	e, ok := m.disks[drive]
	if !ok {
		return diskConf{regulated: false}
	}
	if entryIsSSD(e) {
		return diskConf{regulated: true, ssd: true}
	}
	maxActive := e.MaxActive
	if maxActive <= 0 {
		maxActive = m.cfg.DefaultMaxActive
	}
	wakeBelow := e.WakeBelow
	if wakeBelow <= 0 {
		wakeBelow = maxActive / 2
		if wakeBelow < 1 {
			wakeBelow = 1
		}
	}
	if wakeBelow > maxActive {
		wakeBelow = maxActive
	}
	return diskConf{regulated: true, ssd: false, maxActive: maxActive, wakeBelow: wakeBelow}
}

// Tick runs one regulation cycle. now is injected for testability.
func (m *DiskSlotManager) Tick(torrents []SlotTorrent, now time.Time) {
	m.mu.Lock()
	defer m.mu.Unlock()

	present := make(map[string]bool, len(torrents))
	for i := range torrents {
		present[torrents[i].InfoHash] = true
	}
	for h, until := range m.warmupUntil {
		if !until.After(now) || !present[h] {
			delete(m.warmupUntil, h)
		}
	}
	for h := range m.waitingSince {
		if !present[h] {
			delete(m.waitingSince, h)
		}
	}
	for h := range m.suspended {
		if !present[h] {
			delete(m.suspended, h)
		}
	}

	byDrive := map[string][]SlotTorrent{}
	for _, t := range torrents {
		if !t.Seeding {
			continue
		}
		byDrive[driveKey(t.SavePath)] = append(byDrive[driveKey(t.SavePath)], t)
	}

	for drive, dts := range byDrive {
		dc := m.resolve(drive)
		if !dc.regulated {
			continue // unlisted disk: never touched
		}
		if dc.ssd {
			m.handleSSD(drive, dts)
			continue
		}
		m.handleHDD(drive, dts, dc, now)
	}
}

// handleSSD: no cap. Resume anything we suspended on this disk.
func (m *DiskSlotManager) handleSSD(drive string, dts []SlotTorrent) {
	for _, t := range dts {
		if m.suspended[t.InfoHash] {
			m.doResume(t.InfoHash)
			delete(m.waitingSince, t.InfoHash)
			slog.Info("disk-slots: SSD resume", "drive", drive, "ih", short(t.InfoHash))
		}
	}
}

func (m *DiskSlotManager) handleHDD(drive string, dts []SlotTorrent, dc diskConf, now time.Time) {
	var active, waiting []SlotTorrent
	for _, t := range dts {
		if m.suspended[t.InfoHash] {
			waiting = append(waiting, t)
			m.waitingSince[t.InfoHash] = orSet(m.waitingSince[t.InfoHash], now)
			continue
		}
		if t.UploadRate > 0 {
			active = append(active, t)
		}
	}

	anyWarmup := false
	for _, t := range active {
		if _, ok := m.warmupUntil[t.InfoHash]; ok {
			anyWarmup = true
			break
		}
	}

	// HARD cap: too many active uploaders → suspend down to the ceiling.
	// Safety tier: never deferred by warm-up or cooldown.
	if len(active) > dc.maxActive {
		m.doSuspend(drive, active, dc.maxActive, now)
		return
	}

	// WAKE: room to spare and disk quiet → resume the most-demanded waiting
	// torrent. Hysteresis (wakeBelow < maxActive) + cooldown + no warm-up
	// keep it from oscillating.
	if len(active) < dc.wakeBelow && !anyWarmup && len(waiting) > 0 {
		if last, ok := m.lastWakeAt[drive]; ok &&
			now.Sub(last) < time.Duration(m.cfg.WakeCooldownSec)*time.Second {
			return
		}
		m.doWake(drive, waiting, now)
	}
}

func (m *DiskSlotManager) doSuspend(drive string, active []SlotTorrent, maxActive int, now time.Time) {
	pool := make([]SlotTorrent, 0, len(active))
	for _, t := range active {
		if t.ScrapeSeeders <= m.cfg.SuperSeedThreshold {
			continue // protect rare torrents
		}
		pool = append(pool, t)
	}
	nToSuspend := len(active) - maxActive
	if nToSuspend <= 0 || len(pool) == 0 {
		return
	}
	ranked := rankPauseCandidates(pool)
	done := 0
	for _, t := range ranked {
		if done >= nToSuspend {
			break
		}
		m.doSuspendOne(t.InfoHash)
		m.waitingSince[t.InfoHash] = orSet(m.waitingSince[t.InfoHash], now)
		delete(m.warmupUntil, t.InfoHash)
		slog.Info("disk-slots: suspend",
			"drive", drive, "ih", short(t.InfoHash),
			"seeders", t.ScrapeSeeders, "ul", t.UploadRate,
			"active", len(active), "max", maxActive)
		done++
	}
	if done > 0 {
		m.lastPauseAt[drive] = now
	}
}

func (m *DiskSlotManager) doWake(drive string, waiting []SlotTorrent, now time.Time) {
	best := waiting[0]
	bestScore := m.wakeScore(best, now)
	for _, t := range waiting[1:] {
		if s := m.wakeScore(t, now); s > bestScore {
			best, bestScore = t, s
		}
	}
	m.doResume(best.InfoHash)
	m.warmupUntil[best.InfoHash] = now.Add(time.Duration(m.cfg.WarmupSec) * time.Second)
	delete(m.waitingSince, best.InfoHash)
	m.lastWakeAt[drive] = now
	slog.Info("disk-slots: wake",
		"drive", drive, "ih", short(best.InfoHash),
		"leechers", best.ScrapeLeechers, "seeders", best.ScrapeSeeders,
		"score", bestScore)
}

func (m *DiskSlotManager) wakeScore(t SlotTorrent, now time.Time) float64 {
	base := float64(t.ScrapeLeechers) / float64(t.ScrapeSeeders+1)
	if since, ok := m.waitingSince[t.InfoHash]; ok {
		base += now.Sub(since).Minutes() * m.cfg.WakeAgingBonusPerMin
	}
	return base
}

func (m *DiskSlotManager) doSuspendOne(ih string) {
	m.suspended[ih] = true
	if m.suspendFn != nil {
		m.suspendFn(ih, true)
	}
}

func (m *DiskSlotManager) doResume(ih string) {
	delete(m.suspended, ih)
	if m.suspendFn != nil {
		m.suspendFn(ih, false)
	}
}

// rankPauseCandidates orders torrents least-critical first using a Borda count:
// rank by sources (scrape_seeders desc) + rank by upload speed (asc). A torrent
// both well-sourced AND slow bubbles to the front; a low-source (in-demand)
// torrent stays protected even if it's a bit slow.
func rankPauseCandidates(ts []SlotTorrent) []SlotTorrent {
	bySources := append([]SlotTorrent(nil), ts...)
	sort.SliceStable(bySources, func(i, j int) bool {
		return bySources[i].ScrapeSeeders > bySources[j].ScrapeSeeders
	})
	bySpeed := append([]SlotTorrent(nil), ts...)
	sort.SliceStable(bySpeed, func(i, j int) bool {
		return bySpeed[i].UploadRate < bySpeed[j].UploadRate
	})
	rankSrc := make(map[string]int, len(ts))
	rankSpd := make(map[string]int, len(ts))
	for i, t := range bySources {
		rankSrc[t.InfoHash] = i
	}
	for i, t := range bySpeed {
		rankSpd[t.InfoHash] = i
	}
	ranked := append([]SlotTorrent(nil), ts...)
	sort.SliceStable(ranked, func(i, j int) bool {
		return rankSrc[ranked[i].InfoHash]+rankSpd[ranked[i].InfoHash] <
			rankSrc[ranked[j].InfoHash]+rankSpd[ranked[j].InfoHash]
	})
	return ranked
}

func orSet(existing, now time.Time) time.Time {
	if existing.IsZero() {
		return now
	}
	return existing
}

func short(ih string) string {
	if len(ih) > 8 {
		return ih[:8]
	}
	return ih
}
