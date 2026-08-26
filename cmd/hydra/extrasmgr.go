package main

import (
	"context"
	"fmt"
	"log/slog"
	"reflect"
	"sync"
	"time"

	"github.com/Kheopsian/hydra/internal/agent"
	"github.com/Kheopsian/hydra/internal/agentwire"
	"github.com/Kheopsian/hydra/internal/api"
	"github.com/Kheopsian/hydra/internal/config"
	"github.com/Kheopsian/hydra/internal/engine"
	"github.com/Kheopsian/hydra/internal/engine/grpcclient"
)

// extrasManager owns the engines this node runs beyond its primary race+hoard,
// and it owns them for the whole life of the process rather than only at boot.
//
// That is the point of this type. Adding an engine used to mean writing
// engines.json and telling the user to restart: the config manager that landed
// with the hot config apply can only restart an engine it was handed at boot
// (its slots are fixed, an unknown id is skipped), so a new one had nothing to
// bring it into existence. Here a spawn, a registration and a teardown are the
// same three steps whether they run at boot or on a POST.
type extrasManager struct {
	ctx      context.Context
	cfg      *config.HydraConfig
	raceGate func(string) bool
	// srv is the agent server backing every extra engine's cold path. It is
	// built even with no engines: it is what a later hot add attaches to, and
	// it listens on nothing, so an empty one costs a struct.
	srv *agent.Server
	api *api.Server

	mu    sync.Mutex
	lives map[string]*liveEngine
	order []string // creation order, so the declared list is stable
	// revision/source/lastErr are what this node reports back about the config
	// it is running, the same three things a remote agent reports.
	revision uint64
	source   string
	lastErr  map[string]string
}

// startAnnouncer builds this engine's tracker announcer from the settings it is
// running RIGHT NOW, and hands it to the engine.
//
// It lives on the manager rather than beside the spawn because a settings
// change has to rebuild it: the bindings -- interface, proxy, announced IP --
// are computed once, so an announcer kept across a reload would keep announcing
// through the tunnel the engine just left.
func (m *extrasManager) startAnnouncer(le *liveEngine) {
	ann := engine.NewHoardAnnouncer(le.ref, engine.ApplyAnnounceEgress(
		engine.DefaultSingleBinding(le.cfg.ListenPort, le.cfg.EnableIPv6, "hoard", le.cfg.AnnounceRateLimit),
		le.cfg.AnnounceProxy, le.cfg.AnnounceIP, le.cfg.Socks5OutboundHost, le.cfg.BindInterface, "hoard"))
	if le.hoard != nil {
		ann.OnObservation = le.hoard.ObserveAnnounce
		le.hoard.SetBootstrapAnnounce(ann.BootstrapAnnounce)
		le.hoard.SetReAnnounce(ann.ReAnnounce)
		ann.SetRaceGate(m.raceGate)
		ann.SetOffsetFn(le.hoard.AnnounceOffset)
	}
	ann.Start(m.ctx)
	le.ann = ann
}

func (m *extrasManager) engineDir(id string) string    { return engineDirFor(m.cfg, id) }
func (m *extrasManager) engineSocket(id string) string { return engineSocketFor(m.cfg, id) }

func newExtrasManager(ctx context.Context, cfg *config.HydraConfig, raceGate func(string) bool, front *api.Server) *extrasManager {
	srv := agent.NewServer(nil, cfg.Daemon.DataDir, "")
	// The extras used to be SERVED on a loopback port and dialled back by the
	// front as one agent called "local-shards" -- N engines behind a single
	// name, which is exactly the shape "one agent per engine" removed. The
	// server is built but never listened on: it is only the cold-path backend
	// for the in-process clients registered below, one agent per engine.
	srv.SetOwnEvents(true)
	m := &extrasManager{ctx: ctx, cfg: cfg, raceGate: raceGate, srv: srv, api: front,
		lives: map[string]*liveEngine{}, source: agentwire.ConfigSourceLocal, lastErr: map[string]string{}}
	// These engines take a pushed config like every other agent now. Left
	// without a manager, their server answered apply_config with "this node
	// configures itself locally" -- and nothing did, so an edit reached every
	// other node in seconds and stopped here, with the Agents page reporting
	// them online and current.
	srv.SetConfigManager(m)
	go m.reconcileLoop()
	go func() {
		<-ctx.Done()
		m.stopAll()
	}()
	return m
}

// AgentServer is the cold-path backend for the extra engines.
func (m *extrasManager) AgentServer() *agent.Server { return m.srv }

// AddEngine starts an engine and registers it, without a restart. It is the
// api.EngineHost half of this type.
func (m *extrasManager) AddEngine(ec config.EngineConfig) error { return m.spawn(ec) }

// RemoveEngine stops an engine and unregisters it, without a restart.
func (m *extrasManager) RemoveEngine(id string) error {
	m.mu.Lock()
	le := m.lives[id]
	if le == nil {
		m.mu.Unlock()
		return fmt.Errorf("engine %q does not run here", id)
	}
	delete(m.lives, id)
	for i, v := range m.order {
		if v == id {
			m.order = append(m.order[:i], m.order[i+1:]...)
			break
		}
	}
	// Unpublish BEFORE stopping. The other order leaves the front holding a
	// client to a process that is on its way out, and a list refresh landing in
	// that window reads a dying engine -- an error in the UI for something that
	// was asked for and is going fine.
	if m.api != nil {
		m.api.RemoveRemoteAgent(api.LocalAgentNameFor(id))
	}
	m.srv.ReplaceEngine(id, nil, nil)
	m.declareLocked()
	m.mu.Unlock()

	le.stop()
	slog.Info("extra engine: stopped and unregistered", "engine", id)
	return nil
}

// spawn starts one engine and publishes it: to the extras' agent server (cold
// path) and to the front as its own agent named local-<id>.
func (m *extrasManager) spawn(ec config.EngineConfig) error {
	m.mu.Lock()
	if _, dup := m.lives[ec.ID]; dup {
		m.mu.Unlock()
		return fmt.Errorf("engine %q already runs here", ec.ID)
	}
	m.mu.Unlock()

	le, err := startOneExtraEngine(m.ctx, m.cfg, ec)
	if err != nil {
		return err
	}
	m.startAnnouncer(le)

	m.mu.Lock()
	if _, dup := m.lives[ec.ID]; dup { // lost a race with a concurrent add
		m.mu.Unlock()
		le.stop()
		return fmt.Errorf("engine %q already runs here", ec.ID)
	}
	// The ref, not the process's client: a client dies with its process, and
	// the ref is what a later config apply swaps. Handing the raw client out
	// would leave a holder writing into a socket that closed on the next
	// settings change, silently.
	le.stopPump = m.srv.ReplaceEngine(le.id, le.ref, le.rich)
	m.srv.AddRichEngine(le.id, le.rich)
	m.lives[le.id] = le
	m.order = append(m.order, le.id)
	m.declareLocked()
	m.mu.Unlock()

	if m.api != nil {
		cold := grpcclient.NewWithStub(agent.InProcessStub(m.srv), le.id)
		name := api.LocalAgentNameFor(le.id)
		if err := m.api.AddLocalAgent(name, le.id, le.role,
			api.NewLocalAgentClient(le.id, le.ref, cold)); err != nil {
			// The engine runs but nothing can address it, which is worse than
			// not having it: it would seed invisibly and reappear as a
			// duplicate on the next add. Take it back down.
			slog.Error("extra engine: registration failed, stopping it again",
				"engine", le.id, "error", err)
			_ = m.RemoveEngine(le.id)
			return fmt.Errorf("register %s: %w", le.id, err)
		}
		slog.Info("extra engine registered as its own agent", "engine", le.id, "role", le.role, "agent", name)
	}
	return nil
}

// declareLocked republishes the engine list the agent server answers
// list_engines with, and whether any of them wants IPv6. Caller holds mu.
func (m *extrasManager) declareLocked() {
	descs := make([]agentwire.EngineDescriptor, 0, len(m.order))
	v6 := false
	for _, id := range m.order {
		le := m.lives[id]
		if le == nil {
			continue
		}
		descs = append(descs, agentwire.EngineDescriptor{ID: le.id, Role: le.role})
		v6 = v6 || le.cfg.EnableIPv6
	}
	m.srv.DeclareEngines(descs)
	m.srv.SetIPv6Wanted(v6)
}

// snapshot copies the live set, so the periodic work below does not hold the
// lock across a store reconcile of a whole engine.
func (m *extrasManager) snapshot() []*liveEngine {
	m.mu.Lock()
	defer m.mu.Unlock()
	out := make([]*liveEngine, 0, len(m.order))
	for _, id := range m.order {
		if le := m.lives[id]; le != nil {
			out = append(out, le)
		}
	}
	return out
}

func (m *extrasManager) reconcileLoop() {
	t := time.NewTicker(5 * time.Minute)
	defer t.Stop()
	for {
		select {
		case <-m.ctx.Done():
			return
		case <-t.C:
			for _, le := range m.snapshot() {
				reconcileAgentStore(le.id, le.store, le.metas())
			}
		}
	}
}

func (m *extrasManager) stopAll() {
	for _, le := range m.snapshot() {
		le.stop()
	}
}

// engineClients exposes the live engines, for anything that has to reach them
// by id without going through an agent.
func (m *extrasManager) engineClients() map[string]engine.EngineClient {
	out := map[string]engine.EngineClient{}
	for _, le := range m.snapshot() {
		out[le.id] = le.ref
	}
	return out
}

// ---- taking a pushed configuration ----
//
// The front owns the fleet's configuration and pushes it to every agent. These
// engines are agents like any other since 3.138.0, but their server was left
// without a config manager, so apply_config answered "this node configures
// itself locally" -- and nothing did. An edit to a setting reached every other
// node in seconds and stopped at these, silently, with the Agents page
// reporting them online and current.

// ApplyConfig restarts the engines whose settings actually changed.
func (m *extrasManager) ApplyConfig(p agentwire.ApplyConfigParams) agentwire.ConfigState {
	for _, want := range p.Engines {
		m.mu.Lock()
		le := m.lives[want.ID]
		m.mu.Unlock()
		if le == nil {
			continue // not ours; the primaries have their own manager
		}
		next := want.Session
		// ComposeSession zeroes these two on the way out because on a remote
		// node they belong to the agent. This process IS the agent, so it keeps
		// what it is running: applying the pushed pair verbatim would set the
		// listen port to zero and take the engine off the swarm.
		next.ListenPort = le.cfg.ListenPort
		next.EnableIPv6 = le.cfg.EnableIPv6

		if reflect.DeepEqual(le.cfg, next) {
			m.setErr(le.id, "")
			continue
		}
		if err := m.restart(le, next); err != nil {
			m.setErr(le.id, err.Error())
			slog.Error("config apply: extra engine did not come back, it stays down until the next attempt",
				"engine", le.id, "error", err)
			continue
		}
		m.setErr(le.id, "")
		slog.Info("config apply: extra engine restarted on the new settings", "engine", le.id)
	}
	m.mu.Lock()
	m.revision, m.source = p.Revision, agentwire.ConfigSourceFront
	m.mu.Unlock()
	return m.ConfigState()
}

// restart replaces the engine's process on new settings and rebuilds what was
// derived from the old ones.
//
// The announcer is rebuilt, not reused. Its bindings -- interface, proxy,
// announced IP -- are computed once from the settings it was built with, so an
// engine that moved to another tunnel would keep announcing through the old
// one: the peers would follow the new interface and the tracker would keep
// being told the old address. That is a leak, and a silent one.
func (m *extrasManager) restart(le *liveEngine, next config.SessionConfig) error {
	if le.ann != nil {
		le.ann.Stop()
		le.ann = nil
	}
	old := le.proc
	le.cfg = next
	proc, err := engine.StartSessionEngine(&le.cfg, m.engineDir(le.id), m.engineSocket(le.id), le.role == "race")
	if err != nil {
		return err
	}
	le.proc = proc
	// The ref is the whole reason a restart is survivable: a client dies with
	// its process and ltclient never redials, so every holder -- the Go engine,
	// the agent server, the announcer -- follows the swap instead of writing
	// into a socket that closed.
	if prev := le.ref.Swap(proc.Client()); prev != nil {
		_ = prev.Close()
	}
	if old != nil {
		old.Stop()
	}
	m.startAnnouncer(le)
	return nil
}

func (m *extrasManager) setErr(id, msg string) {
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.lastErr == nil {
		m.lastErr = map[string]string{}
	}
	if msg == "" {
		delete(m.lastErr, id)
		return
	}
	m.lastErr[id] = msg
}

// ConfigState reports what these engines are running.
func (m *extrasManager) ConfigState() agentwire.ConfigState {
	m.mu.Lock()
	defer m.mu.Unlock()
	out := agentwire.ConfigState{
		Revision: m.revision,
		Source:   m.source,
		Engines:  make(map[string]agentwire.EngineState, len(m.lives)),
	}
	for id, le := range m.lives {
		es := agentwire.EngineState{Role: le.role, ListenPort: le.cfg.ListenPort, State: agentwire.EngineStateRunning}
		if msg := m.lastErr[id]; msg != "" {
			es.State, es.Error = agentwire.EngineStateError, msg
		}
		out.Engines[id] = es
	}
	return out
}

// Engines reports the live set for the API. It is the running truth, not a
// file: an engine that failed to start is absent here, and one added by hand to
// the config appears as soon as it runs.
func (m *extrasManager) Engines() []api.RunningEngine {
	out := make([]api.RunningEngine, 0, len(m.order))
	for _, le := range m.snapshot() {
		out = append(out, api.RunningEngine{ID: le.id, Role: le.role,
			ListenPort: le.cfg.ListenPort, BindInterface: le.cfg.BindInterface})
	}
	return out
}
