package engine

import (
	"testing"
	"time"
)

// recorder captures the manager's suspend/resume calls; susp[ih] is the last
// state pushed for a torrent (absent = never touched).
type recorder struct{ susp map[string]bool }

func newRecorder() *recorder                 { return &recorder{susp: map[string]bool{}} }
func (r *recorder) fn(ih string, s bool)     { r.susp[ih] = s }
func (r *recorder) suspended(ih string) bool { v, ok := r.susp[ih]; return ok && v }

func baseCfg() DiskSlotsConfig {
	return DiskSlotsConfig{
		Enabled:            true,
		DefaultMaxActive:   2,
		SuperSeedThreshold: 3,
		Disks: []DiskEntry{
			{Key: "D:", Type: "hdd", MaxActive: 2, WakeBelow: 2},
			{Key: "C:", Type: "ssd"},
		},
	}
}

func tor(ih, drive string, ul int64, seeders, leechers int) SlotTorrent {
	return SlotTorrent{
		InfoHash: ih, SavePath: drive + `\Media\x`,
		UploadRate: ul, ScrapeSeeders: seeders, ScrapeLeechers: leechers,
		Seeding: true,
	}
}

func TestHardCapSuspendsRankedAndProtectsSuperSeed(t *testing.T) {
	r := newRecorder()
	m := NewDiskSlotManager(baseCfg(), r.fn)
	now := time.Now()

	// 4 active on D: (cap 2). C is a super-seed (seeders<=3) → protected.
	// Borda over {A,B,D}: A(s100,ul10)=0, D(s80,ul20)=2, B(s50,ul100)=4.
	// nToSuspend = 2 → suspend A and D.
	m.Tick([]SlotTorrent{
		tor("A", "D:", 10, 100, 0),
		tor("B", "D:", 100, 50, 0),
		tor("C", "D:", 5, 2, 0), // super-seed, protected
		tor("D", "D:", 20, 80, 0),
	}, now)

	if !r.suspended("A") || !r.suspended("D") {
		t.Fatalf("expected A and D suspended, got %v", r.susp)
	}
	if r.suspended("B") {
		t.Errorf("B should not be suspended (least dispensable of the pool)")
	}
	if r.suspended("C") {
		t.Errorf("C is a super-seed and must never be suspended")
	}
}

func TestNoSuspendAtOrUnderCap(t *testing.T) {
	r := newRecorder()
	m := NewDiskSlotManager(baseCfg(), r.fn)
	m.Tick([]SlotTorrent{
		tor("A", "D:", 10, 100, 0),
		tor("B", "D:", 20, 80, 0),
	}, time.Now())
	if len(r.susp) != 0 {
		t.Fatalf("no suspend expected at cap, got %v", r.susp)
	}
}

func TestSSDNeverSuspends(t *testing.T) {
	r := newRecorder()
	m := NewDiskSlotManager(baseCfg(), r.fn)
	var ts []SlotTorrent
	for i, c := range "abcde" {
		ts = append(ts, tor(string(c), "C:", int64(10+i), 50, 0))
	}
	m.Tick(ts, time.Now())
	if len(r.susp) != 0 {
		t.Fatalf("SSD must never be capped, got %v", r.susp)
	}
}

func TestUnlistedDiskUntouched(t *testing.T) {
	r := newRecorder()
	m := NewDiskSlotManager(baseCfg(), r.fn)
	var ts []SlotTorrent
	for i, c := range "abcde" {
		ts = append(ts, tor(string(c), "E:", int64(10+i), 50, 0)) // E: not listed
	}
	m.Tick(ts, time.Now())
	if len(r.susp) != 0 {
		t.Fatalf("unlisted disk must be untouched, got %v", r.susp)
	}
}

func TestWakeResumesMostDemandedWhenBelowThreshold(t *testing.T) {
	r := newRecorder()
	m := NewDiskSlotManager(baseCfg(), r.fn)
	t0 := time.Now()

	// Tick 1: over cap → suspends A and D.
	m.Tick([]SlotTorrent{
		tor("A", "D:", 10, 100, 0),
		tor("B", "D:", 100, 50, 0),
		tor("C", "D:", 5, 2, 0),
		tor("D", "D:", 20, 80, 0),
	}, t0)
	if !r.suspended("A") || !r.suspended("D") {
		t.Fatalf("setup: expected A,D suspended, got %v", r.susp)
	}

	// Tick 2: A,D suspended (ul 0), B idle (ul 0), only C active → 1 < wakeBelow(2).
	// Waiting {A(l0/s100), D(l50/s80)} → D has the higher demand score → resume D.
	m.Tick([]SlotTorrent{
		tor("A", "D:", 0, 100, 0),
		tor("B", "D:", 0, 50, 0),
		tor("C", "D:", 5, 2, 0),
		tor("D", "D:", 0, 80, 50),
	}, t0.Add(time.Second))

	if r.suspended("D") {
		t.Errorf("D (most demanded) should have been resumed, got %v", r.susp)
	}
	if !r.suspended("A") {
		t.Errorf("A should still be suspended (lower demand), got %v", r.susp)
	}
}

func TestHysteresisNoWakeAtThreshold(t *testing.T) {
	cfg := baseCfg()
	cfg.Disks[0].MaxActive = 2
	cfg.Disks[0].WakeBelow = 2
	r := newRecorder()
	m := NewDiskSlotManager(cfg, r.fn)
	t0 := time.Now()

	// Tick 1: 3 active over cap 2 → suspend 1 (least critical A). Remaining active B,D = 2.
	m.Tick([]SlotTorrent{
		tor("A", "D:", 10, 100, 0),
		tor("B", "D:", 100, 50, 0),
		tor("D", "D:", 20, 80, 0),
	}, t0)
	if !r.suspended("A") {
		t.Fatalf("setup: expected A suspended, got %v", r.susp)
	}

	// Tick 2: active = {B,D} = 2, which is NOT < wakeBelow(2) → no wake despite A waiting.
	m.Tick([]SlotTorrent{
		tor("A", "D:", 0, 100, 90), // high demand but hysteresis blocks the wake
		tor("B", "D:", 100, 50, 0),
		tor("D", "D:", 20, 80, 0),
	}, t0.Add(time.Second))

	if !r.suspended("A") {
		t.Errorf("A must stay suspended at the hysteresis boundary (active==wakeBelow), got %v", r.susp)
	}
}
