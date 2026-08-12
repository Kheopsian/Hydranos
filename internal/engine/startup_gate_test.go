package engine

import (
	"sync"
	"testing"
)

// The gate must default to open. A fresh scope that reported "held" would
// silence an engine nobody asked to silence.
func TestStartupGateDefaultsOpen(t *testing.T) {
	if StartupPauseHeld("scope-default") {
		t.Fatal("a scope that was never held must not report held")
	}
	if got := HeldStartupScopes(); len(got) != 0 {
		t.Fatalf("expected no held scopes, got %v", got)
	}
}

func TestStartupGateHoldAndRelease(t *testing.T) {
	const scope = "scope-hold"
	HoldStartupPause(scope)
	if !StartupPauseHeld(scope) {
		t.Fatal("scope should be held after HoldStartupPause")
	}
	held := HeldStartupScopes()
	if len(held) != 1 || held[0] != scope {
		t.Fatalf("expected [%s] held, got %v", scope, held)
	}

	// First release reports that it did something; the second is a no-op. The
	// UI double-click and a stale banner both land on the second case, and
	// neither should surface as an error.
	if !ReleaseStartupPause(scope) {
		t.Fatal("releasing a held scope should report true")
	}
	if ReleaseStartupPause(scope) {
		t.Fatal("releasing an already-released scope should report false")
	}
	if StartupPauseHeld(scope) {
		t.Fatal("scope should be open after release")
	}
}

// Holding one engine must not hold the other: start_paused is per engine, and
// a user pausing hoard alone still expects race to run.
func TestStartupGateScopesAreIndependent(t *testing.T) {
	HoldStartupPause("scope-a")
	defer ReleaseStartupPause("scope-a")
	if StartupPauseHeld("scope-b") {
		t.Fatal("holding one scope must not hold another")
	}
}

// The announce path consults the gate through a nil-able pointer; a nil gate
// is the "no gate configured" case and must never block.
func TestNilGateNeverBlocks(t *testing.T) {
	var g *startupGate
	if g.blocked() {
		t.Fatal("a nil gate must not block")
	}
}

func TestStartupGateConcurrentAccess(t *testing.T) {
	const scope = "scope-race"
	var wg sync.WaitGroup
	for i := 0; i < 50; i++ {
		wg.Add(2)
		go func() { defer wg.Done(); HoldStartupPause(scope) }()
		go func() { defer wg.Done(); _ = StartupPauseHeld(scope) }()
	}
	wg.Wait()
	if !StartupPauseHeld(scope) {
		t.Fatal("scope should be held after concurrent holds")
	}
	ReleaseStartupPause(scope)
}

// A held gate must stop announces before they reach the rate limiter: the
// pause is about nothing leaving, not about leaving slowly.
func TestAnnouncerGateBlocksBeforeLimiter(t *testing.T) {
	const scope = "scope-announcer"
	ta := &trackerAnnouncer{gate: startupGateFor(scope)}

	if ta.gate.blocked() {
		t.Fatal("announcer gate should start open")
	}
	HoldStartupPause(scope)
	if !ta.gate.blocked() {
		t.Fatal("announcer gate should block while the scope is held")
	}
	ReleaseStartupPause(scope)
	if ta.gate.blocked() {
		t.Fatal("announcer gate should reopen after release")
	}
}
