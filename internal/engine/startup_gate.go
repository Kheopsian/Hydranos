package engine

// Startup pause.
//
// A large library behind a VPN knocks the tunnel over in the first minutes
// after boot: every torrent is due an announce at once, each announce asks for
// numwant=200 peers, and every peer that comes back is dialed. Users hit by
// this need to adjust their limits, but the wave leaves before they can reach
// the settings. `start_paused` holds outbound traffic until they say go.
//
// The hold is deliberately process-level and never touches per-torrent state.
// Writing "paused" onto every torrent at boot would be simpler, but it would
// also erase the difference between "the process is holding" and "the user
// paused this torrent on purpose" -- releasing would then resume torrents the
// user had stopped by hand, destroying that intent with no way to recover it.
// A gate in front of the two chokepoints (announces here, dials in the Rust
// engine's dial queue) achieves the same silence without owning any state that
// belongs to the user.
//
// A held gate is loud on purpose: the boot log says so, the API exposes it and
// the UI carries a permanent banner. A silently muted engine looks exactly
// like a broken one.

import (
	"errors"
	"log/slog"
	"sort"
	"sync"

	"github.com/Kheopsian/hydra/internal/engine/ltclient"
)

// ErrStartupPaused is returned by announce paths while the gate is held.
var ErrStartupPaused = errors.New("tracker announce: startup pause held, announce skipped")

type startupGate struct {
	scope string

	mu   sync.RWMutex
	held bool
}

var (
	startupGatesMu sync.Mutex
	startupGates   = map[string]*startupGate{}
)

// startupGateFor returns the shared gate for an engine scope ("race",
// "hoard"), creating it on first use. Gates are always created, held or not,
// so the API can report a scope that exists but is running.
func startupGateFor(scope string) *startupGate {
	startupGatesMu.Lock()
	defer startupGatesMu.Unlock()
	g := startupGates[scope]
	if g == nil {
		g = &startupGate{scope: scope}
		startupGates[scope] = g
	}
	return g
}

// HoldStartupPause arms the gate for a scope. Called at boot when the session
// has start_paused set, before any announcer starts.
func HoldStartupPause(scope string) {
	g := startupGateFor(scope)
	g.mu.Lock()
	g.held = true
	g.mu.Unlock()
	slog.Warn("startup pause HELD: no announces or peer dials will leave this engine until released",
		"engine", scope, "release", "POST /api/startup-pause/release")
}

// ReleaseStartupPause lifts the gate. Returns false when the scope was not
// held, so the caller can tell a real release from a no-op.
func ReleaseStartupPause(scope string) bool {
	g := startupGateFor(scope)
	g.mu.Lock()
	was := g.held
	g.held = false
	g.mu.Unlock()
	if was {
		slog.Info("startup pause released, outbound traffic resuming", "engine", scope)
	}
	return was
}

// StartupPauseHeld reports whether a scope is currently held.
func StartupPauseHeld(scope string) bool {
	g := startupGateFor(scope)
	g.mu.RLock()
	defer g.mu.RUnlock()
	return g.held
}

// HeldStartupScopes lists the scopes currently held, sorted for a stable API
// response. Empty slice = nothing held.
func HeldStartupScopes() []string {
	startupGatesMu.Lock()
	defer startupGatesMu.Unlock()
	out := []string{}
	for scope, g := range startupGates {
		g.mu.RLock()
		if g.held {
			out = append(out, scope)
		}
		g.mu.RUnlock()
	}
	sort.Strings(out)
	return out
}

// SetEngineDialsPaused holds or releases a local engine's outbound dial queue.
// No-op for a remote (non-ltclient) engine client: a fronted agent runs its own
// gate from its own config, and reaching across to hold it from here would put
// one node's startup policy in charge of another's.
func SetEngineDialsPaused(client EngineClient, paused bool) error {
	lt, ok := client.(*ltclient.Client)
	if !ok {
		return nil
	}
	return lt.SetDialsPaused(paused)
}

// blocked reports whether this gate is holding. Nil receiver = never holds,
// matching the announceLimiter convention.
func (g *startupGate) blocked() bool {
	if g == nil {
		return false
	}
	g.mu.RLock()
	defer g.mu.RUnlock()
	return g.held
}
