package api

import (
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"strings"
	"sync"
	"time"

	"github.com/Kheopsian/hydra/internal/config"
	"github.com/Kheopsian/hydra/internal/engine"
)

// Gluetun hands out a port forwarded by the VPN provider, and that port is not
// ours to choose: it is assigned per lease and changes. A static listen_port
// behind such a tunnel is wrong the moment the lease rotates, and nothing says
// so — the node simply stops being reachable while looking healthy.
//
// So: ask gluetun what it got, bind that, and follow it.
//
// The important half is the waiting. Announcing before the port is known would
// publish the wrong one to every tracker, and they keep it for a full announce
// cycle, sending peers to a port that answers nobody. The engine's startup gate
// already holds announces AND peer dials, so it is held from boot and released
// only once the port is bound.

const (
	gluetunPollInterval  = 5 * time.Second
	gluetunWatchInterval = 60 * time.Second
	gluetunHTTPTimeout   = 8 * time.Second
	// Give up holding announces after this long. A tunnel that never yields a
	// port would otherwise keep the engine silent forever, which is worse than
	// running on the configured port: the operator can see and fix a wrong
	// port, but not a node that never speaks.
	gluetunWaitTimeout = 10 * time.Minute
)

var errGluetunNoPort = errors.New("gluetun has no forwarded port yet")

// gluetunPort asks the control server for the currently forwarded port.
// Returns errGluetunNoPort when gluetun answers but holds no port, which is the
// normal state while the tunnel is still negotiating one.
func gluetunPort(baseURL, apiKey string) (int, error) {
	base := strings.TrimRight(strings.TrimSpace(baseURL), "/")
	if base == "" {
		base = "http://127.0.0.1:8000"
	}
	req, err := http.NewRequest(http.MethodGet, base+"/v1/portforward", nil)
	if err != nil {
		return 0, err
	}
	if k := strings.TrimSpace(apiKey); k != "" {
		req.Header.Set("X-API-Key", k)
	}
	client := &http.Client{Timeout: gluetunHTTPTimeout}
	resp, err := client.Do(req)
	if err != nil {
		return 0, err
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(io.LimitReader(resp.Body, 4096))
	if resp.StatusCode == http.StatusUnauthorized {
		return 0, fmt.Errorf("gluetun refused the request (401): set gluetun_api_key, and give the role the route GET /v1/portforward")
	}
	if resp.StatusCode != http.StatusOK {
		return 0, fmt.Errorf("gluetun answered HTTP %d: %s", resp.StatusCode, strings.TrimSpace(string(body)))
	}
	var out struct {
		Port int `json:"port"`
	}
	if err := json.Unmarshal(body, &out); err != nil {
		return 0, fmt.Errorf("gluetun answer not understood: %w", err)
	}
	if out.Port <= 0 {
		return 0, errGluetunNoPort
	}
	return out.Port, nil
}

type gluetunSync struct {
	scope  string
	cfg    *config.SessionConfig
	setter interface{ SetListenPort(int) error }
	server *Server

	mu   sync.RWMutex
	port int
	last string
}

var (
	gluetunMu    sync.RWMutex
	gluetunState = map[string]*gluetunSync{}
)

// GluetunStatus reports what the port follower is doing, for the Network tab.
func GluetunStatus(scope string) (port int, detail string, active bool) {
	gluetunMu.RLock()
	defer gluetunMu.RUnlock()
	g := gluetunState[scope]
	if g == nil {
		return 0, "", false
	}
	g.mu.RLock()
	defer g.mu.RUnlock()
	return g.port, g.last, true
}

func (g *gluetunSync) note(format string, args ...interface{}) {
	msg := fmt.Sprintf(format, args...)
	g.mu.Lock()
	g.last = msg
	g.mu.Unlock()
}

// apply rebinds the engine and records the port, exactly as the API route does:
// persist only after a successful bind, so a port we could not take is never
// written down as if it were live.
func (g *gluetunSync) apply(port int) error {
	if g.setter == nil {
		return errors.New("agent unavailable")
	}
	if err := g.setter.SetListenPort(port); err != nil {
		return err
	}
	if err := g.server.persistListenPort(g.scope, port); err != nil {
		slog.Warn("gluetun: port bound but not persisted", "engine", g.scope, "port", port, "err", err)
	}
	g.mu.Lock()
	g.port = port
	g.mu.Unlock()
	return nil
}

func (g *gluetunSync) run() {
	// Phase one: wait for a port, with announces held.
	deadline := time.Now().Add(gluetunWaitTimeout)
	for {
		port, err := gluetunPort(g.cfg.GluetunURL, g.cfg.GluetunAPIKey)
		if err == nil {
			if aerr := g.apply(port); aerr != nil {
				g.note("could not bind the port gluetun gave (%d): %v", port, aerr)
				slog.Error("gluetun: rebind failed", "engine", g.scope, "port", port, "err", aerr)
			} else {
				g.note("listening on the forwarded port %d", port)
				slog.Info("gluetun: bound the forwarded port, releasing the startup hold",
					"engine", g.scope, "port", port)
			}
			break
		}
		if time.Now().After(deadline) {
			// Silence is worse than a wrong port: one is visible and fixable,
			// the other looks like a healthy node that nobody can reach.
			g.note("gave up waiting for a forwarded port: %v", err)
			slog.Warn("gluetun: no forwarded port within the wait window, releasing the hold anyway",
				"engine", g.scope, "waited", gluetunWaitTimeout.String(), "err", err)
			break
		}
		g.note("waiting for gluetun to report a forwarded port: %v", err)
		time.Sleep(gluetunPollInterval)
	}
	engine.ReleaseStartupPause(g.scope)

	// Phase two: follow it. The lease rotates, and a stale port is a node that
	// announces an address answering nobody.
	for {
		time.Sleep(gluetunWatchInterval)
		port, err := gluetunPort(g.cfg.GluetunURL, g.cfg.GluetunAPIKey)
		if err != nil {
			g.note("could not read the forwarded port: %v", err)
			continue
		}
		g.mu.RLock()
		cur := g.port
		g.mu.RUnlock()
		if port == cur {
			continue
		}
		if err := g.apply(port); err != nil {
			g.note("forwarded port changed to %d but the rebind failed: %v", port, err)
			slog.Error("gluetun: rebind after port change failed", "engine", g.scope, "port", port, "err", err)
			continue
		}
		g.note("forwarded port changed to %d, now listening there", port)
		slog.Info("gluetun: forwarded port changed, rebound", "engine", g.scope, "from", cur, "to", port)
	}
}

// HoldForGluetun arms the startup gate for every session set to follow gluetun.
// Called from main before any announcer exists: the gate has to be held before
// the first announce can leave, not after.
func HoldForGluetun(sessions map[string]*config.SessionConfig) {
	for scope, cfg := range sessions {
		if cfg != nil && cfg.GluetunPortForward {
			engine.HoldStartupPause(scope)
			slog.Info("gluetun: holding announces until the forwarded port is known", "engine", scope)
		}
	}
}

// StartGluetunPortSync launches the follower for each session that asked for
// it. Safe to call with sessions that did not: they are skipped.
func (s *Server) StartGluetunPortSync(sessions map[string]*config.SessionConfig) {
	for scope, cfg := range sessions {
		if cfg == nil || !cfg.GluetunPortForward {
			continue
		}
		var setter interface{ SetListenPort(int) error }
		switch scope {
		case "race":
			if s.raceEngine != nil {
				setter = s.raceEngine
			}
		case "hoard":
			if s.hoardEngine != nil {
				setter = s.hoardEngine
			}
		}
		g := &gluetunSync{scope: scope, cfg: cfg, setter: setter, server: s}
		gluetunMu.Lock()
		gluetunState[scope] = g
		gluetunMu.Unlock()
		go g.run()
	}
}
