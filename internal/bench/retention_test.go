package bench

import (
	"path/filepath"
	"testing"
	"time"
)

func countRows(t *testing.T, b *BenchDB, table string) int {
	t.Helper()
	var n int
	if err := b.conn.QueryRow("SELECT COUNT(*) FROM " + table).Scan(&n); err != nil {
		t.Fatalf("count %s: %v", table, err)
	}
	return n
}

// race_snapshots shared the year-long window with the low-rate tables, so on a
// large instance it grew for months before a single row was pruned. It must
// now age out on its own, shorter window while the other tables keep theirs.
func TestRaceSnapshotsPrunedOnTheirOwnWindow(t *testing.T) {
	b := NewBenchDB(filepath.Join(t.TempDir(), "bench.db"))
	if err := b.Open(); err != nil {
		t.Fatalf("open bench db: %v", err)
	}
	defer b.Close()

	now := float64(time.Now().Unix())
	day := 86400.0

	// One race snapshot per age, in days.
	for _, age := range []float64{0, 3, 10, 40, 400} {
		b.InsertRaceSnapshots([]RaceSnapshot{{Ts: now - age*day, InfoHash: "ih", PeersJSON: "[]"}})
	}
	// A low-rate row at 40 days must survive: its window is a year.
	if _, err := b.conn.Exec("INSERT INTO race_events (ts, info_hash, event) VALUES (?, ?, ?)",
		now-40*day, "ih", "added"); err != nil {
		t.Fatalf("seed race_events: %v", err)
	}

	if got := countRows(t, b, "race_snapshots"); got != 5 {
		t.Fatalf("seeded %d race snapshots, want 5", got)
	}

	// The old code shared one 365-day window with every table. Reproduce it
	// to show the seeded rows really are within reach of a prune, so the
	// assertions below exercise the new window and not an empty table.
	b.SetRetention(RetentionPolicy{RaceSnapshotDays: RetentionDays})
	if deleted := b.PurgeOld(); deleted != 1 {
		t.Fatalf("under the old shared 365-day window PurgeOld deleted %d rows, want 1 (only the 400-day snapshot)", deleted)
	}

	b.SetRetention(RetentionPolicy{}) // zero value = defaults (7 days / 365 days)
	if deleted := b.PurgeOld(); deleted != 2 {
		t.Fatalf("PurgeOld deleted %d rows, want 2 (the 10 and 40 day snapshots)", deleted)
	}
	if got := countRows(t, b, "race_snapshots"); got != 2 {
		t.Errorf("race_snapshots left %d rows, want 2 (0 and 3 days old)", got)
	}
	if got := countRows(t, b, "race_events"); got != 1 {
		t.Errorf("race_events lost its 40-day row to the race snapshot window: %d rows left", got)
	}
}

func TestRetentionPolicyOverrides(t *testing.T) {
	now := time.Unix(1_000_000_000, 0)

	// Zero means default, not "prune everything".
	if c, ok := (RetentionPolicy{}).raceSnapshotCutoff(now); !ok || c != float64(now.Unix())-DefaultRaceSnapshotDays*86400 {
		t.Errorf("zero race window = %v (ok=%v), want the %d-day default", c, ok, DefaultRaceSnapshotDays)
	}
	if c, ok := (RetentionPolicy{}).generalCutoff(now); !ok || c != float64(now.Unix())-RetentionDays*86400 {
		t.Errorf("zero general window = %v (ok=%v), want the %d-day default", c, ok, RetentionDays)
	}
	// Negative disables pruning for that family.
	if _, ok := (RetentionPolicy{RaceSnapshotDays: -1}).raceSnapshotCutoff(now); ok {
		t.Error("negative race window still prunes")
	}
	if _, ok := (RetentionPolicy{GeneralDays: -1}).generalCutoff(now); ok {
		t.Error("negative general window still prunes")
	}
	// An explicit value is honoured.
	if c, _ := (RetentionPolicy{RaceSnapshotDays: 2}).raceSnapshotCutoff(now); c != float64(now.Unix())-2*86400 {
		t.Errorf("explicit race window ignored: %v", c)
	}
}
