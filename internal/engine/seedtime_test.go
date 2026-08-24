package engine

import (
	"testing"
	"time"
)

// resetSeedClock isolates each test from the package singleton.
func resetSeedClock() {
	seedClockSingleton.mu.Lock()
	seedClockSingleton.entries = map[string]seedEntry{}
	seedClockSingleton.lastPrune = time.Time{}
	seedClockSingleton.mu.Unlock()
}

// backdate rewinds a torrent's last observation, standing in for elapsed time
// without making the test sleep.
func backdate(ih string, secs int64) {
	seedClockSingleton.mu.Lock()
	e := seedClockSingleton.entries[ih]
	e.seen -= secs
	seedClockSingleton.entries[ih] = e
	seedClockSingleton.mu.Unlock()
}

func seeding(ih string) map[string]*TorrentStats {
	return map[string]*TorrentStats{ih: {InfoHash: ih, Progress: 1.0, State: "seeding"}}
}

// The first observation must not credit anything: the process has no idea what
// happened before it started, and crediting it would invent seed time.
func TestSeedTimeFirstSightCreditsNothing(t *testing.T) {
	resetSeedClock()
	accrueSeedTime(seeding("aa"))
	if got := SeedTimeFor("aa"); got != 0 {
		t.Fatalf("seed time = %d, want 0 on first sight", got)
	}
}

// Two observations 60s apart credit exactly the elapsed time, and the total is
// stamped into the stats the callers read.
func TestSeedTimeAccrues(t *testing.T) {
	resetSeedClock()
	accrueSeedTime(seeding("bb"))
	backdate("bb", 60)
	stats := seeding("bb")
	accrueSeedTime(stats)
	if got := SeedTimeFor("bb"); got != 60 {
		t.Fatalf("seed time = %d, want 60", got)
	}
	if got := stats["bb"].SeedingTime; got != 60 {
		t.Fatalf("stamped SeedingTime = %d, want 60", got)
	}
}

// A user stop halts the clock, and the pause is NOT back-filled when the
// torrent starts again -- the whole point of the counter.
func TestSeedTimeStoppedDoesNotAccrue(t *testing.T) {
	resetSeedClock()
	accrueSeedTime(seeding("cc"))
	backdate("cc", 60)
	accrueSeedTime(seeding("cc")) // 60s banked

	stopped := map[string]*TorrentStats{"cc": {InfoHash: "cc", Progress: 1.0, State: StateStopped}}
	accrueSeedTime(stopped)
	backdate("cc", 3600) // an hour goes by, stopped
	accrueSeedTime(seeding("cc"))
	if got := SeedTimeFor("cc"); got != 60 {
		t.Fatalf("seed time = %d, want 60 (the stopped hour must not count)", got)
	}
}

// The race engine reports a user-stopped torrent as StateQueued and carries
// the intent in UserPaused instead. Testing the state alone let the clock run
// straight through a pause; this is that regression, pinned.
func TestSeedTimeUserPausedDoesNotAccrue(t *testing.T) {
	resetSeedClock()
	accrueSeedTime(seeding("pp"))
	backdate("pp", 60)
	accrueSeedTime(seeding("pp")) // 60s banked

	paused := map[string]*TorrentStats{"pp": {InfoHash: "pp", Progress: 1.0, State: StateQueued, UserPaused: true}}
	accrueSeedTime(paused)
	backdate("pp", 300)
	accrueSeedTime(seeding("pp"))
	if got := SeedTimeFor("pp"); got != 60 {
		t.Fatalf("seed time = %d, want 60 (a user pause must stop the clock whatever the state string says)", got)
	}
}

// A scheduler hold (queued) and the disk slot manager's serving-suspend both
// keep the torrent available as far as a tracker is concerned, so the clock
// keeps running -- disk_slots.go promises "seedtime preserved".
func TestSeedTimeQueuedStillAccrues(t *testing.T) {
	resetSeedClock()
	queued := map[string]*TorrentStats{"dd": {InfoHash: "dd", Progress: 1.0, State: StateQueued}}
	accrueSeedTime(queued)
	backdate("dd", 30)
	accrueSeedTime(map[string]*TorrentStats{"dd": {InfoHash: "dd", Progress: 1.0, State: StateQueued}})
	if got := SeedTimeFor("dd"); got != 30 {
		t.Fatalf("seed time = %d, want 30 for a queued seed", got)
	}
}

// An incomplete torrent is not seeding, whatever its state string says.
func TestSeedTimeIncompleteDoesNotAccrue(t *testing.T) {
	resetSeedClock()
	part := func() map[string]*TorrentStats {
		return map[string]*TorrentStats{"ee": {InfoHash: "ee", Progress: 0.5, State: "downloading"}}
	}
	accrueSeedTime(part())
	backdate("ee", 120)
	accrueSeedTime(part())
	if got := SeedTimeFor("ee"); got != 0 {
		t.Fatalf("seed time = %d, want 0 while incomplete", got)
	}
}

// A gap longer than the cap credits the cap, not the gap: a stalled push or a
// suspended host must not hand out seed time nobody observed.
func TestSeedTimeAccrualIsCapped(t *testing.T) {
	resetSeedClock()
	accrueSeedTime(seeding("ff"))
	backdate("ff", 86400) // a day with no observation
	accrueSeedTime(seeding("ff"))
	want := int64(seedAccrualCap / time.Second)
	if got := SeedTimeFor("ff"); got != want {
		t.Fatalf("seed time = %d, want the cap %d", got, want)
	}
}

// Boot re-seeding takes the durable value, and never lowers a counter this
// process has already advanced further.
func TestSeedTimeSeedTakesMax(t *testing.T) {
	resetSeedClock()
	SeedTimeSeed(map[string]int64{"gg": 500})
	if got := SeedTimeFor("gg"); got != 500 {
		t.Fatalf("seed time = %d, want 500 after seeding", got)
	}
	SeedTimeSeed(map[string]int64{"gg": 100})
	if got := SeedTimeFor("gg"); got != 500 {
		t.Fatalf("seed time = %d, want 500 (a lower saved value must not win)", got)
	}
}

// The same info_hash reported by both engines in one tick accrues once, not
// twice -- what makes the race/hoard handoff free.
func TestSeedTimeCountsOncePerTick(t *testing.T) {
	resetSeedClock()
	accrueSeedTime(seeding("hh"))
	backdate("hh", 60)
	accrueSeedTime(seeding("hh")) // race engine
	accrueSeedTime(seeding("hh")) // hoard engine, same tick
	if got := SeedTimeFor("hh"); got != 60 {
		t.Fatalf("seed time = %d, want 60 (no double count)", got)
	}
}
