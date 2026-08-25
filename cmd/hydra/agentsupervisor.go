package main

import (
	"context"
	"encoding/json"
	"fmt"
	"log/slog"
	"os"
	"path/filepath"
	"reflect"
	"sync"
	"time"

	"github.com/Kheopsian/hydra/internal/agent"
	"github.com/Kheopsian/hydra/internal/agentwire"
	"github.com/Kheopsian/hydra/internal/choking"
	"github.com/Kheopsian/hydra/internal/config"
	"github.com/Kheopsian/hydra/internal/engine"
	"github.com/Kheopsian/hydra/internal/store"
)

// engineSupervisor owns an agent node's engines and its configuration.
//
// A node boots knowing only its identity and serves gRPC immediately, before
// any engine exists: the front has to be able to reach it in order to give it
// the config that brings its engines up. From then on every apply_config is a
// reconcile -- the engines whose session config actually changed are restarted
// and the rest are left alone, because restarting a seeding engine costs a
// re-announce and a re-import for nothing.
//
// The applied config is cached on disk so a node that restarts while its front
// is down comes back seeding on the last configuration it was given, instead of
// sitting idle waiting for a push that may be hours away.
type engineSupervisor struct {
	ctx        context.Context
	dataDir    string
	uploadsDir string
	srv        *agent.Server
	boot       []agentBootEngine
	bootIndex  map[string]int // engine id -> position, for a stable IPC socket

	// reconcileMu is held for the whole of a store reconcile, and by Shutdown
	// while it closes the stores. It is deliberately not mu: a reconcile walks
	// every torrent of every engine, and holding mu for that would stall the
	// announce race gate and every ConfigState the front asks for.
	reconcileMu sync.Mutex

	mu       sync.Mutex
	lives    map[string]*liveEngine
	stores   map[string]*store.AgentStore
	revision uint64
	source   string
	lastErr  map[string]string
	// applied is the configuration currently in force, kept so a failed engine
	// can be retried without waiting for the front to send the same thing again
	// -- which it will not do, since it compares revisions and sees a match.
	applied agentwire.ApplyConfigParams
}

// engineRetryInterval is how often an engine whose last start failed is tried
// again. Failures here are usually transient and external (a data directory
// not mounted yet, an engine binary being replaced mid-upgrade), so the node
// heals itself instead of needing a restart.
const engineRetryInterval = 30 * time.Second

// cacheFileName is where the last applied config is kept, beside the engines'
// own data.
const cacheFileName = "pushed-config.json"

func newEngineSupervisor(ctx context.Context, cfg *config.HydraConfig, boot []agentBootEngine, srv *agent.Server) *engineSupervisor {
	idx := make(map[string]int, len(boot))
	for i, b := range boot {
		idx[b.ID] = i
	}
	return &engineSupervisor{
		ctx:        ctx,
		dataDir:    cfg.Daemon.DataDir,
		uploadsDir: filepath.Join(cfg.Daemon.DataDir, "uploads"),
		srv:        srv,
		boot:       boot,
		bootIndex:  idx,
		lives:      make(map[string]*liveEngine),
		stores:     make(map[string]*store.AgentStore),
		lastErr:    make(map[string]string),
		source:     agentwire.ConfigSourceNone,
	}
}

// ---- agent.ConfigManager ----

// ApplyConfig takes a configuration pushed by the front.
func (s *engineSupervisor) ApplyConfig(p agentwire.ApplyConfigParams) agentwire.ConfigState {
	s.mu.Lock()
	defer s.mu.Unlock()
	// Cached only when the apply worked: a config that brought no engine up is
	// not one this node should boot from next time. Caching it anyway would
	// make readCache succeed on the next boot and shadow the local-file
	// fallback, so a node would replay a config it has already proven it
	// cannot run.
	if s.apply(p, agentwire.ConfigSourceFront) {
		if err := s.writeCache(p); err != nil {
			slog.Warn("agent: could not cache the pushed config", "err", err)
		}
	} else {
		slog.Warn("agent: no engine came up on the pushed config, not caching it",
			"revision", p.Revision)
	}
	return s.state()
}

// ConfigState reports what this node is running.
func (s *engineSupervisor) ConfigState() agentwire.ConfigState {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.state()
}

// state builds the reply. Caller holds mu.
func (s *engineSupervisor) state() agentwire.ConfigState {
	out := agentwire.ConfigState{
		Revision: s.revision,
		Source:   s.source,
		Engines:  make(map[string]agentwire.EngineState, len(s.boot)),
	}
	for _, b := range s.boot {
		es := agentwire.EngineState{Role: b.Role, ListenPort: b.ListenPort, State: agentwire.EngineStatePending}
		if msg := s.lastErr[b.ID]; msg != "" {
			es.State, es.Error = agentwire.EngineStateError, msg
		} else if s.lives[b.ID] != nil {
			es.State = agentwire.EngineStateRunning
		}
		out.Engines[b.ID] = es
	}
	return out
}

// ---- boot ----

// Bootstrap brings the node up from whatever configuration is available before
// the front is heard from: the cache of the last pushed config, or -- for a
// node that still declares its engines in its own TOML -- that file's session
// config. A node with neither stays idle, which is correct: it has no
// configuration, and inventing one would make it announce with defaults nobody
// chose.
func (s *engineSupervisor) Bootstrap(cfg *config.HydraConfig, identitySource string) {
	s.mu.Lock()
	defer s.mu.Unlock()

	if p, ok := s.readCache(); ok {
		slog.Info("agent: starting from the last config the front pushed", "revision", p.Revision)
		// The result is not acted on: an engine that fails here is retried by
		// RetryFailedEngines, and the cache it came from is already on disk.
		_ = s.apply(p, agentwire.ConfigSourceCache)
		return
	}
	if identitySource == "file" {
		if p, ok := s.localConfig(cfg); ok {
			slog.Info("agent: no cached config, starting from this node's own config file", "path", cfg.SourcePath)
			_ = s.apply(p, agentwire.ConfigSourceLocal)
			return
		}
	}
	slog.Info("agent: no configuration yet, engines stay down until the front pushes one",
		"engines", len(s.boot))
}

// localConfig builds an apply payload from the node's own TOML, the way an
// agent was configured before the front owned this. Kept so an existing
// deployment upgrades without its engines going dark on the first boot.
func (s *engineSupervisor) localConfig(cfg *config.HydraConfig) (agentwire.ApplyConfigParams, bool) {
	engines, err := cfg.ResolveEngines()
	if err != nil {
		return agentwire.ApplyConfigParams{}, false
	}
	p := agentwire.ApplyConfigParams{
		Announce: agentwire.AnnounceConfigWire{
			Passkeys:       cfg.AnnouncePasskeys,
			SecondaryStats: cfg.AnnounceSecondaryStats,
			IPModes:        cfg.AnnounceIPModes,
			Clients:        make(map[string]agentwire.ClientSpoofWire, len(cfg.AnnounceClients)),
		},
	}
	for host, c := range cfg.AnnounceClients {
		p.Announce.Clients[host] = agentwire.ClientSpoofWire{PeerIDPrefix: c.PeerIDPrefix, UserAgent: c.UserAgent}
	}
	for _, ec := range engines {
		p.Engines = append(p.Engines, agentwire.AgentEngineConfig{ID: ec.ID, Role: ec.Role, Session: ec.SessionConfig})
	}
	p.Revision = p.ComputeRevision()
	return p, len(p.Engines) > 0
}

// ---- apply ----

// apply reconciles the running engines against a configuration and reports
// whether every declared engine ended up running. Caller holds mu.
func (s *engineSupervisor) apply(p agentwire.ApplyConfigParams, source string) bool {
	applyAnnounceOverrides(p.Announce)

	want := make(map[string]config.SessionConfig, len(p.Engines))
	for _, ec := range p.Engines {
		want[ec.ID] = s.overlayIdentity(ec.ID, ec.Session)
	}

	for _, b := range s.boot {
		session, given := want[b.ID]
		if !given {
			// The front composed nothing for this engine. Leaving it running
			// on a config the front no longer holds would be a node quietly
			// out of the fleet, so it goes down and says why.
			if s.lives[b.ID] != nil {
				slog.Warn("agent: no config for this engine in the push, stopping it", "id", b.ID)
				s.stopEngine(b.ID)
			}
			s.lastErr[b.ID] = "the front sent no configuration for this engine"
			continue
		}
		live := s.lives[b.ID]
		if live != nil && reflect.DeepEqual(live.cfg, session) {
			delete(s.lastErr, b.ID)
			continue // nothing about this engine changed
		}
		if live != nil {
			slog.Info("agent: config changed, restarting engine", "id", b.ID)
			s.stopEngine(b.ID)
		}
		if err := s.startEngine(b, session); err != nil {
			slog.Error("agent: engine failed to start on the pushed config", "id", b.ID, "err", err)
			s.lastErr[b.ID] = err.Error()
			continue
		}
		delete(s.lastErr, b.ID)
	}
	s.revision, s.source, s.applied = p.Revision, source, p
	return len(s.lastErr) == 0
}

// RetryFailedEngines re-attempts the engines whose last start failed, on the
// configuration already in force. Nothing else is touched: a running engine is
// left alone, so this is safe to call on a timer.
func (s *engineSupervisor) RetryFailedEngines() {
	s.mu.Lock()
	defer s.mu.Unlock()
	if len(s.lastErr) == 0 || len(s.applied.Engines) == 0 {
		return
	}
	for _, ec := range s.applied.Engines {
		if s.lastErr[ec.ID] == "" || s.lives[ec.ID] != nil {
			continue
		}
		b, ok := s.bootFor(ec.ID)
		if !ok {
			continue
		}
		slog.Info("agent: retrying an engine that failed to start", "id", ec.ID)
		if err := s.startEngine(b, s.overlayIdentity(ec.ID, ec.Session)); err != nil {
			s.lastErr[ec.ID] = err.Error()
			continue
		}
		delete(s.lastErr, ec.ID)
	}
}

// bootFor returns the boot identity of a declared engine.
func (s *engineSupervisor) bootFor(id string) (agentBootEngine, bool) {
	for _, b := range s.boot {
		if b.ID == id {
			return b, true
		}
	}
	return agentBootEngine{}, false
}

// overlayIdentity puts the node's own boot values back over what the front
// sent. The front composes and omits these two, but a front that sent them
// anyway -- an older one, or a hand-edited [[agent.engine]] block -- must not
// be able to move an agent's listen port: the port is a fact about this
// machine's network, and getting it wrong takes a whole announce cycle to
// undo on every tracker.
func (s *engineSupervisor) overlayIdentity(id string, session config.SessionConfig) config.SessionConfig {
	for _, b := range s.boot {
		if b.ID != id {
			continue
		}
		session.ListenPort = b.ListenPort
		session.EnableIPv6 = b.EnableIPv6
		return session
	}
	return session
}

// applyAnnounceOverrides installs all four per-tracker override families. They
// are read on every announce, so they need no restart -- which is why an edit
// to a tracker spoof reaches a whole fleet without interrupting a single
// torrent.
func applyAnnounceOverrides(a agentwire.AnnounceConfigWire) {
	engine.ResetPasskeyOverrides(a.Passkeys)
	spoofs := make(map[string]engine.ClientSpoof, len(a.Clients))
	for host, c := range a.Clients {
		spoofs[host] = engine.ClientSpoof{PeerIDPrefix: c.PeerIDPrefix, UserAgent: c.UserAgent}
	}
	engine.ResetClientOverrides(spoofs)
	engine.ResetSecondaryStatsOverrides(a.SecondaryStats)
	engine.ResetAnnounceIPModes(a.IPModes)
}

// startEngine spawns one engine and wires it into the node. Caller holds mu.
func (s *engineSupervisor) startEngine(b agentBootEngine, session config.SessionConfig) error {
	eDir := filepath.Join(s.dataDir, b.ID)
	if err := os.MkdirAll(eDir, 0755); err != nil {
		return fmt.Errorf("engine dir: %w", err)
	}
	// Same endpoint selection as the monolith: a hardcoded .sock path here
	// would hand Typhon a unix path on Windows, where its listener is a stub
	// that refuses to bind, so the engine never starts. Ports are per-engine
	// so N engines on one node never collide, and start above the monolith
	// 9501/9502 so an agent can share a machine with a daemon. Keyed by the
	// engine's position in the boot list so a restart reuses its own socket.
	sock := engineSocketPath(s.dataDir, b.ID, agentEngineBasePort+s.bootIndex[b.ID])

	le := &liveEngine{id: b.ID, role: b.Role, cfg: session}
	proc, err := engine.StartSessionEngine(&le.cfg, eDir, sock, b.Role == "race")
	if err != nil {
		return err // already says "start engine process: ..."
	}
	le.proc = proc

	if b.Role == "race" {
		var ck engine.ChokingEngineInterface
		if le.cfg.CustomChoking != nil && le.cfg.CustomChoking.Enabled {
			ck = choking.NewChokingEngine(le.cfg.CustomChoking)
		}
		re := engine.NewRaceEngine(&le.cfg, ck, nil, eDir)
		re.SetClient(proc.Client())
		le.race, le.rich = re, re
	} else {
		he := engine.NewHoardEngine(&le.cfg, eDir)
		he.SetClient(proc.Client())
		le.hoard, le.rich = he, he
	}

	// One store per engine id for the life of the process: it is the durable
	// record of what this engine holds, and it has to outlive the restarts the
	// engine goes through when its config changes.
	st := s.stores[b.ID]
	if st == nil {
		if opened, serr := store.OpenAgent(filepath.Join(eDir, "store.db")); serr != nil {
			slog.Error("agent: open store", "id", b.ID, "err", serr)
		} else {
			st, s.stores[b.ID] = opened, opened
		}
	}
	le.store = st

	if le.race != nil {
		if err := le.race.Start(s.ctx); err != nil {
			proc.Stop()
			return fmt.Errorf("race start: %w", err)
		}
	} else if err := le.hoard.Start(s.ctx); err != nil {
		proc.Stop()
		return fmt.Errorf("hoard start: %w", err)
	}

	ann := engine.NewHoardAnnouncer(proc.Client(), engine.ApplyAnnounceEgress(
		engine.DefaultSingleBinding(le.cfg.ListenPort, le.cfg.EnableIPv6, "hoard", le.cfg.AnnounceRateLimit),
		le.cfg.AnnounceProxy, le.cfg.AnnounceIP, le.cfg.Socks5OutboundHost, le.cfg.BindInterface, "hoard"))
	if le.hoard != nil {
		ann.OnObservation = le.hoard.ObserveAnnounce
		le.hoard.SetBootstrapAnnounce(ann.BootstrapAnnounce)
		le.hoard.SetReAnnounce(ann.ReAnnounce)
		ann.SetRaceGate(s.raceGate)
		ann.SetOffsetFn(le.hoard.AnnounceOffset)
	}
	ann.Start(s.ctx)
	le.ann = ann

	le.stopPump = s.srv.ReplaceEngine(b.ID, proc.Client(), le.rich)
	s.lives[b.ID] = le

	if le.store != nil {
		var imported, errs int
		if le.race != nil {
			imported, errs = le.race.ImportFromStore(le.store, s.uploadsDir)
		} else {
			imported, errs = le.hoard.ImportFromStore(le.store, s.uploadsDir)
		}
		slog.Info("agent: store reload", "id", b.ID, "imported", imported, "errors", errs)
	}
	slog.Info("agent: engine running", "id", b.ID, "role", b.Role, "listen_port", le.cfg.ListenPort)
	return nil
}

// stopEngine tears one engine down, keeping its store open for the engine that
// replaces it. Caller holds mu.
func (s *engineSupervisor) stopEngine(id string) {
	le := s.lives[id]
	if le == nil {
		return
	}
	delete(s.lives, id)
	// The store is reconciled before the engine goes: a restart that dropped
	// the last few adds would silently lose them.
	if le.store != nil {
		reconcileAgentStore(id, le.store, le.metas())
	}
	if le.stopPump != nil {
		le.stopPump()
	}
	s.srv.ReplaceEngine(id, nil, nil)
	if le.ann != nil {
		le.ann.Stop()
	}
	if le.race != nil {
		le.race.Stop()
	}
	if le.hoard != nil {
		le.hoard.Stop()
	}
	le.proc.Stop()
}

// raceGate reports whether any race engine on this node already holds an
// info_hash, so a hoard never announces it too (anti dual-announce). It reads
// the live set on each call rather than a slice captured at boot, because that
// set changes whenever a config push restarts an engine.
func (s *engineSupervisor) raceGate(infoHash string) bool {
	s.mu.Lock()
	races := make([]*engine.RaceEngine, 0, len(s.lives))
	for _, le := range s.lives {
		if le.race != nil {
			races = append(races, le.race)
		}
	}
	s.mu.Unlock()
	for _, r := range races {
		if r.HasTorrent(infoHash) {
			return true
		}
	}
	return false
}

// ---- live engine access for the rest of the process ----

// liveEngines returns the currently running engines.
func (s *engineSupervisor) liveEngines() []*liveEngine {
	s.mu.Lock()
	defer s.mu.Unlock()
	out := make([]*liveEngine, 0, len(s.lives))
	for _, b := range s.boot {
		if le := s.lives[b.ID]; le != nil {
			out = append(out, le)
		}
	}
	return out
}

// Reconcile mirrors every running engine's torrents into its store.
func (s *engineSupervisor) Reconcile() {
	s.reconcileMu.Lock()
	defer s.reconcileMu.Unlock()
	for _, le := range s.liveEngines() {
		reconcileAgentStore(le.id, le.store, le.metas())
	}
}

// Shutdown stops every engine and closes the stores.
func (s *engineSupervisor) Shutdown() {
	s.mu.Lock()
	for _, b := range s.boot {
		s.stopEngine(b.ID)
	}
	s.mu.Unlock()

	// The stores close only once no reconcile is in flight. The reconcile
	// ticker and this run both end on the same ctx.Done, so a tick that had
	// already started would otherwise write into a handle closed underneath
	// it -- a use-after-close on SQLite, at the one moment nobody is watching
	// the log.
	s.reconcileMu.Lock()
	defer s.reconcileMu.Unlock()
	s.mu.Lock()
	defer s.mu.Unlock()
	for id, st := range s.stores {
		st.Close()
		delete(s.stores, id)
	}
}

// ---- cache ----

func (s *engineSupervisor) cachePath() string { return filepath.Join(s.dataDir, cacheFileName) }

// readCache loads the last pushed config, if it still matches this node's
// identity. A cache naming other engines belongs to a node this one was
// re-purposed from, and applying it would start engines the operator no longer
// declares.
func (s *engineSupervisor) readCache() (agentwire.ApplyConfigParams, bool) {
	data, err := os.ReadFile(s.cachePath())
	if err != nil {
		return agentwire.ApplyConfigParams{}, false
	}
	var p agentwire.ApplyConfigParams
	if err := json.Unmarshal(data, &p); err != nil {
		slog.Warn("agent: cached config is unreadable, ignoring it", "path", s.cachePath(), "err", err)
		return agentwire.ApplyConfigParams{}, false
	}
	declared := make(map[string]string, len(s.boot))
	for _, b := range s.boot {
		declared[b.ID] = b.Role
	}
	for _, ec := range p.Engines {
		if role, ok := declared[ec.ID]; !ok || role != ec.Role {
			slog.Warn("agent: cached config is for a different set of engines, ignoring it",
				"path", s.cachePath(), "engine", ec.ID)
			return agentwire.ApplyConfigParams{}, false
		}
	}
	return p, len(p.Engines) > 0
}

func (s *engineSupervisor) writeCache(p agentwire.ApplyConfigParams) error {
	data, err := json.MarshalIndent(p, "", "  ")
	if err != nil {
		return err
	}
	tmp := s.cachePath() + ".tmp"
	if err := os.WriteFile(tmp, data, 0600); err != nil {
		return err
	}
	return os.Rename(tmp, s.cachePath())
}
