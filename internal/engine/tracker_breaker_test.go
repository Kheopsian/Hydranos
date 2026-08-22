package engine

import (
	"testing"
	"time"
)

func TestBreakerTripsAfterConsecutiveFailuresAndRecovers(t *testing.T) {
	b := newTrackerBreaker()
	t0 := time.Unix(1_700_000_000, 0)

	// Below the threshold the host stays in service.
	for i := 0; i < breakerFailThreshold-1; i++ {
		b.record("dead.invalid", false, t0)
		if !b.allows("dead.invalid", t0) {
			t.Fatalf("tripped after %d failures, threshold is %d", i+1, breakerFailThreshold)
		}
	}

	b.record("dead.invalid", false, t0)
	if b.allows("dead.invalid", t0) {
		t.Fatalf("still announcing after %d consecutive failures", breakerFailThreshold)
	}
	// Another host is unaffected.
	if !b.allows("live.invalid", t0) {
		t.Error("one dead host took an unrelated host down with it")
	}

	// Still down just before the cooldown expires, back just after.
	if b.allows("dead.invalid", t0.Add(breakerCooldown-time.Second)) {
		t.Error("cooldown ended early")
	}
	if !b.allows("dead.invalid", t0.Add(breakerCooldown+time.Second)) {
		t.Error("host never came back after the cooldown")
	}
}

func TestBreakerSuccessClearsTheStreak(t *testing.T) {
	b := newTrackerBreaker()
	t0 := time.Unix(1_700_000_000, 0)

	// An intermittent tracker must not accumulate its way to a trip: four
	// failures, one success, four more failures is not an outage.
	for i := 0; i < breakerFailThreshold-1; i++ {
		b.record("flaky.invalid", false, t0)
	}
	b.record("flaky.invalid", true, t0)
	for i := 0; i < breakerFailThreshold-1; i++ {
		b.record("flaky.invalid", false, t0)
	}
	if !b.allows("flaky.invalid", t0) {
		t.Fatal("a success in the middle of the streak did not reset the count")
	}
}

func TestBreakerNilIsPermissive(t *testing.T) {
	// An engine built without a breaker (tests, front-only) must still announce.
	var b *trackerBreaker
	if !b.allows("any.invalid", time.Now()) {
		t.Fatal("nil breaker blocked an announce")
	}
	b.record("any.invalid", false, time.Now()) // must not panic
}
