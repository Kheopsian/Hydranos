package engine

import (
	"context"
	"testing"
	"time"
)

// A zero/negative rate must yield no limiter at all, so the announce path stays
// exactly as it was before the setting existed.
func TestAnnounceLimiterDisabled(t *testing.T) {
	if l := announceLimiterFor("test-off#0", 0); l != nil {
		t.Fatalf("rate 0 should give no limiter, got %+v", l)
	}
	var nilLim *announceLimiter
	if err := nilLim.wait(context.Background()); err != nil {
		t.Fatalf("nil limiter must be a no-op, got %v", err)
	}
}

// The bucket holds one second of credit, so a burst up to the rate goes through
// instantly and the next announce waits for a token to refill.
func TestAnnounceLimiterSpreadsBurst(t *testing.T) {
	l := &announceLimiter{scope: "test", rate: 10, burst: 10, tokens: 10}
	start := time.Now()
	for i := 0; i < 10; i++ {
		if err := l.wait(context.Background()); err != nil {
			t.Fatalf("burst announce %d refused: %v", i, err)
		}
	}
	if d := time.Since(start); d > 50*time.Millisecond {
		t.Fatalf("burst within the bucket should not block, waited %v", d)
	}
	if err := l.wait(context.Background()); err != nil {
		t.Fatalf("11th announce refused: %v", err)
	}
	if d := time.Since(start); d < 90*time.Millisecond {
		t.Fatalf("11th announce should wait ~1/rate, total %v", d)
	}
}

// A cancelled context releases the waiter instead of pinning it for the cap.
func TestAnnounceLimiterContextCancel(t *testing.T) {
	l := &announceLimiter{scope: "test", rate: 0.01, burst: 1, tokens: 0, last: time.Now()}
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Millisecond)
	defer cancel()
	start := time.Now()
	if err := l.wait(ctx); err == nil {
		t.Fatal("expected the cancelled context to abort the wait")
	}
	if d := time.Since(start); d > time.Second {
		t.Fatalf("wait should end with the context, took %v", d)
	}
}

// Past announceWaitCap the announce is refused rather than held forever, so a
// rate too low for the swarm degrades into re-planned announces.
func TestAnnounceLimiterRefusesBeyondCap(t *testing.T) {
	// 1 announce per 10 minutes: the wait for a token exceeds the 120s cap.
	l := &announceLimiter{scope: "test", rate: 1.0 / 600, burst: 1, tokens: 0, last: time.Now()}
	if err := l.wait(context.Background()); err != errAnnounceRateLimited {
		t.Fatalf("expected errAnnounceRateLimited, got %v", err)
	}
}
