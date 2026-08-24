package engine

import (
	"net/http"
	"net/http/httptest"
	"testing"
	"time"
)

// newStatsAnnouncer builds a minimal announcer bound to its own scope, so each
// test reads counters nobody else touched.
func newStatsAnnouncer(scope string) *trackerAnnouncer {
	return &trackerAnnouncer{
		httpClient: &http.Client{Timeout: 2 * time.Second},
		peerID:     "-HY3550-abcdefghijkl",
		port:       6881,
		gate:       startupGateFor(scope),
		scope:      scope,
	}
}

const testInfoHash = "0123456789abcdef0123456789abcdef01234567"

// A successful announce counts once as sent and never as failed — this is the
// number the bench graph differences into announces/second.
func TestAnnounceCounterSuccess(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Write([]byte("d8:completei1e10:incompletei0e8:intervali60e5:peers0:e"))
	}))
	defer srv.Close()

	const scope = "stats-ok"
	ta := newStatsAnnouncer(scope)
	if _, err := ta.announce(srv.URL, testInfoHash, 1, 2, 0, ""); err != nil {
		t.Fatalf("announce: %v", err)
	}
	sent, failed, limited := AnnounceStats(scope)
	if sent != 1 || failed != 0 || limited != 0 {
		t.Fatalf("sent/failed/limited = %d/%d/%d, want 1/0/0", sent, failed, limited)
	}
}

// A tracker that refuses still had an announce leave the process: it counts as
// sent AND failed, so a graph of failures is a subset of the cadence, not a
// separate series that could exceed it.
func TestAnnounceCounterFailure(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		http.Error(w, "nope", http.StatusForbidden)
	}))
	defer srv.Close()

	const scope = "stats-fail"
	ta := newStatsAnnouncer(scope)
	if _, err := ta.announce(srv.URL, testInfoHash, 0, 0, 0, ""); err == nil {
		t.Fatal("expected an error from a 403 tracker")
	}
	sent, failed, _ := AnnounceStats(scope)
	if sent != 1 || failed != 1 {
		t.Fatalf("sent/failed = %d/%d, want 1/1", sent, failed)
	}
}

// The startup pause holds the announce before anything leaves, so it must not
// show up in the cadence at all — otherwise a paused engine would graph a
// healthy announce rate while announcing nothing.
func TestAnnounceCounterGatedNotCounted(t *testing.T) {
	const scope = "stats-gated"
	ta := newStatsAnnouncer(scope)
	HoldStartupPause(scope)
	defer ReleaseStartupPause(scope)

	if _, err := ta.announce("http://127.0.0.1:1/announce", testInfoHash, 0, 0, 0, ""); err != ErrStartupPaused {
		t.Fatalf("err = %v, want ErrStartupPaused", err)
	}
	sent, failed, limited := AnnounceStats(scope)
	if sent != 0 || failed != 0 || limited != 0 {
		t.Fatalf("sent/failed/limited = %d/%d/%d, want 0/0/0", sent, failed, limited)
	}
}

// An announce dropped by announce_rate_limit counts as limited only: nothing
// went on the wire, so it must not inflate the cadence.
func TestAnnounceCounterRateLimited(t *testing.T) {
	const scope = "stats-limited"
	ta := newStatsAnnouncer(scope)
	// A bucket with no credit and a rate too low to ever refill within the
	// wait cap: the very first announce is dropped.
	ta.limiter = &announceLimiter{scope: scope, rate: 1e-9, burst: 1, tokens: 0, last: time.Now()}

	if _, err := ta.announce("http://127.0.0.1:1/announce", testInfoHash, 0, 0, 0, ""); err != errAnnounceRateLimited {
		t.Fatalf("err = %v, want errAnnounceRateLimited", err)
	}
	sent, failed, limited := AnnounceStats(scope)
	if sent != 0 || failed != 0 || limited != 1 {
		t.Fatalf("sent/failed/limited = %d/%d/%d, want 0/0/1", sent, failed, limited)
	}
}
