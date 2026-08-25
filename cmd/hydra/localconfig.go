package main

import (
	"log/slog"
	"reflect"
	"sync"

	"github.com/Kheopsian/hydra/internal/agentwire"
	"github.com/Kheopsian/hydra/internal/config"
	"github.com/Kheopsian/hydra/internal/engine"
)

// localEngineSlot is one primary engine of this node, as the config manager
// needs to see it: the settings it runs, the stable handle everything else
// talks through, and the process currently behind that handle.
type localEngineSlot struct {
	id      string
	role    string
	isRace  bool
	dataDir string
	socket  string
	cfg     *config.SessionConfig
	ref     *engine.EngineRef
	proc    *engine.EngineProcess
}

// localConfigManager applies a pushed configuration to THIS node's engines.
//
// The monolith declined apply_config until now -- SetConfigManager was left nil
// with the reasoning that it "configures itself from its own file". That held
// while the monolith and an agent were different things. Since its engines
// became agents they are not, and the asymmetry was visible: a settings change
// reached every remote node in seconds while this one waited for a restart, with
// nothing on screen saying the fleet was running two different configurations.
type localConfigManager struct {
	mu       sync.Mutex
	slots    map[string]*localEngineSlot
	revision uint64
	source   string
	lastErr  map[string]string
}

func newLocalConfigManager(slots ...*localEngineSlot) *localConfigManager {
	m := &localConfigManager{
		slots:   make(map[string]*localEngineSlot, len(slots)),
		source:  agentwire.ConfigSourceLocal,
		lastErr: map[string]string{},
	}
	for _, s := range slots {
		m.slots[s.id] = s
	}
	return m
}

// ApplyConfig restarts the engines whose settings actually changed.
func (m *localConfigManager) ApplyConfig(p agentwire.ApplyConfigParams) agentwire.ConfigState {
	m.mu.Lock()
	defer m.mu.Unlock()

	for _, want := range p.Engines {
		slot := m.slots[want.ID]
		if slot == nil {
			continue // not ours; the shards have their own manager
		}
		next := want.Session
		// ComposeSession zeroes these two on the way out, because on a remote
		// node they belong to the agent -- it is the side that knows which port
		// its VPN forwards and which interfaces it has. Here the front IS the
		// agent, so the rule inverts: keep what we are running, or the first
		// reload would silently drop this node's listen port to zero.
		next.ListenPort = slot.cfg.ListenPort
		next.EnableIPv6 = slot.cfg.EnableIPv6

		if sameSession(*slot.cfg, next) {
			delete(m.lastErr, slot.id)
			continue
		}
		if err := m.restart(slot, next); err != nil {
			m.lastErr[slot.id] = err.Error()
			slog.Error("config apply: engine did not come back, it stays down until the next attempt",
				"engine", slot.id, "error", err)
			continue
		}
		delete(m.lastErr, slot.id)
		slog.Info("config apply: engine restarted on the new settings", "engine", slot.id)
	}
	m.revision, m.source = p.Revision, agentwire.ConfigSourceFront
	return m.state()
}

// restart stops the process, starts one on the new settings, and republishes
// its client through the ref.
//
// The ref swap is the whole point. A client does not survive its process --
// ltclient dials once and never redials -- so without it every holder, the
// tracker announcers included, would keep writing into a closed socket while
// the new process ran perfectly beside them, silently.
func (m *localConfigManager) restart(slot *localEngineSlot, next config.SessionConfig) error {
	old := slot.proc
	*slot.cfg = next
	proc, err := engine.StartSessionEngine(slot.cfg, slot.dataDir, slot.socket, slot.isRace)
	if err != nil {
		return err
	}
	slot.proc = proc
	if prev := slot.ref.Swap(proc.Client()); prev != nil {
		_ = prev.Close()
	}
	if old != nil {
		old.Stop()
	}
	return nil
}

// ProcFor returns the process currently behind an engine, for anything that
// must follow a replacement rather than hold the one it was handed at boot --
// the watchdog above all, which otherwise sees a deliberately retired pid and
// restarts the daemon.
func (m *localConfigManager) ProcFor(id string) *engine.EngineProcess {
	m.mu.Lock()
	defer m.mu.Unlock()
	if s := m.slots[id]; s != nil {
		return s.proc
	}
	return nil
}

// ConfigState reports what this node is running.
func (m *localConfigManager) ConfigState() agentwire.ConfigState {
	m.mu.Lock()
	defer m.mu.Unlock()
	return m.state()
}

// state builds the reply. Caller holds mu.
func (m *localConfigManager) state() agentwire.ConfigState {
	out := agentwire.ConfigState{
		Revision: m.revision,
		Source:   m.source,
		Engines:  make(map[string]agentwire.EngineState, len(m.slots)),
	}
	for id, s := range m.slots {
		es := agentwire.EngineState{Role: s.role, ListenPort: s.cfg.ListenPort, State: agentwire.EngineStateRunning}
		if msg := m.lastErr[id]; msg != "" {
			es.State, es.Error = agentwire.EngineStateError, msg
		}
		out.Engines[id] = es
	}
	return out
}

// sameSession decides whether a push is worth a restart.
//
// Compared field by field through the struct's own equality rather than a hash
// of the marshalled form: a restart drops every peer connection this engine
// holds, so answering "changed" to a difference that is only in encoding order
// would cost the swarm for nothing.
func sameSession(a, b config.SessionConfig) bool {
	return reflect.DeepEqual(a, b)
}
