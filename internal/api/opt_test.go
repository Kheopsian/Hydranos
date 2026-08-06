package api

import (
	"sort"
	"testing"
)

func mkList(n int) []map[string]interface{} {
	out := make([]map[string]interface{}, 0, n)
	for i := 0; i < n; i++ {
		out = append(out, map[string]interface{}{"hash": string(rune(0x61 + (n - i))), "i": i})
	}
	return out
}

// Off, every request rebuilds: that is the baseline rung.
func TestQbitSnapshotDisabledAlwaysBuilds(t *testing.T) {
	optQbitSnapshot.Store(false)
	InvalidateQbitSnapshot()
	calls := 0
	build := func() []map[string]interface{} { calls++; return mkList(3) }
	for i := 0; i < 3; i++ {
		qbitSnapshot(build)
	}
	if calls != 3 {
		t.Fatalf("built %d times, want 3", calls)
	}
}

// On, the listing is built once inside the TTL.
func TestQbitSnapshotSharesWithinTTL(t *testing.T) {
	optQbitSnapshot.Store(true)
	defer optQbitSnapshot.Store(false)
	InvalidateQbitSnapshot()
	calls := 0
	build := func() []map[string]interface{} { calls++; return mkList(5) }
	for i := 0; i < 4; i++ {
		qbitSnapshot(build)
	}
	if calls != 1 {
		t.Fatalf("built %d times, want 1", calls)
	}
}

// Each caller sorts its result in place. If they shared one slice, the second
// caller would see the first one is ordering - and two concurrent requests
// would race on the same backing array.
func TestQbitSnapshotReturnsIndependentSlices(t *testing.T) {
	optQbitSnapshot.Store(true)
	defer optQbitSnapshot.Store(false)
	InvalidateQbitSnapshot()
	build := func() []map[string]interface{} { return mkList(6) }

	a := qbitSnapshot(build)
	b := qbitSnapshot(build)
	sort.Slice(a, func(i, j int) bool {
		return a[i]["i"].(int) < a[j]["i"].(int)
	})
	for i := range b {
		if b[i]["i"].(int) != i {
			t.Fatalf("sorting one caller reordered another at %d", i)
		}
	}
}

// The memo must be a pass-through when the flag is off.
func TestTotalsMemoOffIsPassThrough(t *testing.T) {
	optTotalsCache.Store(false)
	var m totalsMemo
	n := 0
	f := func() (int64, int64) { n++; return int64(n), int64(n * 2) }
	for i := 1; i <= 3; i++ {
		ul, dl := m.get(f)
		if ul != int64(i) || dl != int64(i*2) {
			t.Fatalf("call %d returned %d/%d", i, ul, dl)
		}
	}
}

func TestTotalsMemoOnCachesWithinTTL(t *testing.T) {
	optTotalsCache.Store(true)
	defer optTotalsCache.Store(false)
	var m totalsMemo
	n := 0
	f := func() (int64, int64) { n++; return int64(n), 0 }
	for i := 0; i < 5; i++ {
		if ul, _ := m.get(f); ul != 1 {
			t.Fatalf("call %d returned %d, want the memoised 1", i, ul)
		}
	}
	if n != 1 {
		t.Fatalf("underlying function ran %d times, want 1", n)
	}
}
