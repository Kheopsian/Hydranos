package api

import (
	"context"
	"log/slog"
	"sort"
	"sync"
	"time"

	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"

	"github.com/Kheopsian/hydra/internal/agentwire"
	"github.com/Kheopsian/hydra/internal/config"
	"github.com/Kheopsian/hydra/internal/engine"
)

// The front owns its agents' configuration. An agent boots knowing only its
// own identity -- engine id, role, listen port, IPv6 -- and everything else is
// composed here from the [race]/[hoard] fleet profiles, the per-engine
// [[agent.engine]] overrides and the live announce overrides, then pushed with
// apply_config.
//
// The push is what makes the model work, so it cannot be a one-shot at boot:
// an agent that restarts comes back on whatever it cached, and one that was
// down when the front started was never reached at all. A reconciler re-dials
// and re-pushes on a timer, and compares the revision the agent reports
// against the one the front would send so a steady fleet stays quiet.

// agentReconcileInterval is how often the front re-dials missing agents and
// re-checks the config revision of the connected ones.
const agentReconcileInterval = 15 * time.Second

// composeAgentConfig builds the configuration one agent should be running.
func (s *Server) composeAgentConfig(cfg *config.HydraConfig, agentName string, descs []agentwire.EngineDescriptor) (agentwire.ApplyConfigParams, error) {
	engines := make([]agentwire.AgentEngineConfig, 0, len(descs))
	for _, d := range descs {
		session, err := cfg.ComposeSession(agentName, d.ID, d.Role)
		if err != nil {
			return agentwire.ApplyConfigParams{}, err
		}
		engines = append(engines, agentwire.AgentEngineConfig{ID: d.ID, Role: d.Role, Session: session})
	}
	// ComputeRevision hashes the marshalled payload, and json preserves slice
	// order: without a stable sort here, two identical configurations could
	// hash differently just because list_engines answered in another order.
	sort.Slice(engines, func(i, j int) bool { return engines[i].ID < engines[j].ID })

	p := agentwire.ApplyConfigParams{Engines: engines, Announce: liveAnnounceConfig()}
	p.Revision = p.ComputeRevision()
	return p, nil
}

// liveAnnounceConfig reads the four announce override families from the engine
// layer rather than from the config struct. That layer is the front's live
// truth: the API handlers apply an override there immediately and mirror it to
// the TOML afterwards, so reading the file would miss an override whose write
// failed, and reading the boot-time struct would miss every runtime change.
func liveAnnounceConfig() agentwire.AnnounceConfigWire {
	clients := engine.GetClientOverrides()
	wire := make(map[string]agentwire.ClientSpoofWire, len(clients))
	for host, c := range clients {
		wire[host] = agentwire.ClientSpoofWire{PeerIDPrefix: c.PeerIDPrefix, UserAgent: c.UserAgent}
	}
	return agentwire.AnnounceConfigWire{
		Passkeys:       engine.GetPasskeyOverrides(),
		Clients:        wire,
		SecondaryStats: engine.GetSecondaryStatsOverrides(),
		IPModes:        engine.GetAnnounceIPModes(),
	}
}

// InitAnnounceOverrides seeds the engine's override maps from a config. Every
// mode needs this, including front-only: the front never announces itself, but
// it is the source of what its agents announce with, and a front that did not
// load its own [announce_*] tables would push empty ones over them.
func InitAnnounceOverrides(cfg *config.HydraConfig) {
	engine.InitPasskeyOverrides(cfg.AnnouncePasskeys)
	spoofs := make(map[string]engine.ClientSpoof, len(cfg.AnnounceClients))
	for host, c := range cfg.AnnounceClients {
		spoofs[host] = engine.ClientSpoof{PeerIDPrefix: c.PeerIDPrefix, UserAgent: c.UserAgent}
	}
	engine.InitClientOverrides(spoofs)
	engine.InitSecondaryStatsOverrides(cfg.AnnounceSecondaryStats)
	engine.InitAnnounceIPModes(cfg.AnnounceIPModes)
}

// liveConfig re-reads the config file so a compose sees the settings editor's
// latest write. It falls back to the boot config when the file is unreadable:
// pushing a slightly stale config beats pushing nothing.
func (s *Server) liveConfig() *config.HydraConfig {
	if cfg, err := config.Reload(s.settingsFilePath()); err == nil {
		return cfg
	}
	return s.config
}

// agentConfigError explains why an [[agent]] block cannot be dialed, or "".
// Skipping an unusable block in silence is indistinguishable from a front that
// dialed nothing: no agents, no error, an empty dashboard.
func agentConfigError(ag config.AgentConfig) string {
	switch {
	case ag.Name == "" && ag.Addr == "":
		return "both name and addr are missing"
	case ag.Name == "":
		return "name is missing"
	case ag.Addr == "":
		return "addr is missing"
	}
	return ""
}

// configStateCache holds the last ConfigState each agent reported, for
// /api/agents. Keyed by agent name.
type configStateCache struct {
	mu sync.RWMutex
	m  map[string]agentwire.ConfigState
}

func (c *configStateCache) set(name string, st agentwire.ConfigState) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.m == nil {
		c.m = map[string]agentwire.ConfigState{}
	}
	c.m[name] = st
}

func (c *configStateCache) get(name string) (agentwire.ConfigState, bool) {
	c.mu.RLock()
	defer c.mu.RUnlock()
	st, ok := c.m[name]
	return st, ok
}

func (c *configStateCache) drop(name string) {
	c.mu.Lock()
	defer c.mu.Unlock()
	delete(c.m, name)
}

// StartAgentReconciler dials every configured agent, pushes it its config, and
// keeps doing both on a timer. It replaces the one-shot boot dial: the first
// pass runs synchronously so the boot log still says which agents came up.
func (s *Server) StartAgentReconciler(ctx context.Context) {
	s.reconcileAgents()
	go func() {
		t := time.NewTicker(agentReconcileInterval)
		defer t.Stop()
		for {
			select {
			case <-ctx.Done():
				return
			case <-t.C:
				s.reconcileAgents()
			}
		}
	}()
}

// reconcileAgents runs one pass: dial what is missing, push config where it
// differs. Every failure is logged and skipped -- a dead agent must never
// block boot or stall the other agents' reconcile.
func (s *Server) reconcileAgents() {
	cfg := s.liveConfig()
	for name, a := range s.desiredRemoteAgents(cfg) {
		ra := s.remoteAgentByName(name)
		// A registered agent that answers no Ping is re-dialed, not merely
		// re-pushed: the node it points at may have been replaced, and a
		// client held from before that is a connection to nothing.
		if ra == nil || !remoteAgentOnline(ra) {
			if err := s.AddRemoteAgent(name, a.Addr, a.Token, a.TLSCa); err != nil {
				slog.Debug("agent dial failed", "name", name, "addr", a.Addr, "err", err)
				s.agentConfigState.drop(name)
				continue
			}
			slog.Info("remote agent registered", "name", name, "addr", a.Addr)
			ra = s.remoteAgentByName(name)
			if ra == nil {
				continue
			}
		} else if s.engineSetChanged(ra, a) {
			// The node came back declaring different engines -- its identity
			// was edited. Re-dial so the front holds one client per engine
			// that now exists, then compose for that set rather than for the
			// one this front happens to remember.
			slog.Info("agent re-declared its engines, re-dialing", "name", name)
			if err := s.AddRemoteAgent(name, a.Addr, a.Token, a.TLSCa); err != nil {
				slog.Warn("agent re-dial failed", "name", name, "err", err)
				continue
			}
			if ra = s.remoteAgentByName(name); ra == nil {
				continue
			}
		}
		s.syncAgentConfig(cfg, ra)
	}
}

// engineSetChanged reports whether the agent now declares engines other than
// the ones this front is connected to. Unreachable or unchanged reads false:
// the reconciler must not churn clients over a blip.
func (s *Server) engineSetChanged(ra *remoteAgent, a agentStore) bool {
	cl := ra.anyClient()
	if cl == nil {
		return false
	}
	descs, err := cl.ListEngines()
	if err != nil || len(descs) == 0 {
		return false
	}
	if len(descs) != len(ra.engines) {
		return true
	}
	known := make(map[string]string, len(ra.engines))
	for _, e := range ra.engines {
		known[e.id] = e.role
	}
	for _, d := range descs {
		if role, ok := known[d.ID]; !ok || role != d.Role {
			return true
		}
	}
	return false
}

// syncAgentConfig pushes the composed config to one agent unless it already
// reports running exactly it. Reports whether the agent ended up holding the
// front's configuration, so a caller can tell the user how much of the fleet
// took an edit.
func (s *Server) syncAgentConfig(cfg *config.HydraConfig, ra *remoteAgent) bool {
	cl := ra.anyClient()
	if cl == nil {
		return false
	}
	descs := make([]agentwire.EngineDescriptor, 0, len(ra.engines))
	for _, e := range ra.engines {
		descs = append(descs, agentwire.EngineDescriptor{ID: e.id, Role: e.role})
	}
	want, err := s.composeAgentConfig(cfg, ra.name, descs)
	if err != nil {
		slog.Warn("agent config compose failed", "agent", ra.name, "err", err)
		return false
	}

	have, gerr := cl.GetConfigState()
	switch {
	case gerr == nil:
		s.agentConfigState.set(ra.name, have)
		if have.Revision == want.Revision && have.Source == agentwire.ConfigSourceFront {
			return true
		}
	case isUnimplemented(gerr):
		// Pre-apply_config agent: the only config channel it has is the old
		// per-override push, which covers two of the four families.
		s.pushLegacyAnnounce(ra, want.Announce)
		return false
	case isUnreachable(gerr):
		// Expected and temporary. Logging a warning every 15s for a node that
		// is simply down would bury the failures worth reading, but the state
		// has to stop claiming the node is on the front's config.
		s.agentConfigState.drop(ra.name)
		slog.Debug("agent unreachable, config push deferred", "agent", ra.name, "err", gerr)
		return false
	}

	st, aerr := cl.ApplyConfig(want)
	if aerr != nil {
		if isUnimplemented(aerr) {
			s.pushLegacyAnnounce(ra, want.Announce)
			return false
		}
		slog.Warn("agent config push failed", "agent", ra.name, "revision", want.Revision, "err", aerr)
		return false
	}
	s.agentConfigState.set(ra.name, st)
	slog.Info("agent config pushed", "agent", ra.name, "revision", want.Revision, "engines", len(want.Engines))
	for id, es := range st.Engines {
		if es.State == agentwire.EngineStateError {
			slog.Warn("agent engine failed to apply config", "agent", ra.name, "engine", id, "err", es.Error)
		}
	}
	return true
}

// pushLegacyAnnounce feeds an agent too old for apply_config through the
// per-override channel it does understand. Secondary-stats and ip-modes have
// no equivalent there and stay on that agent's own config.
func (s *Server) pushLegacyAnnounce(ra *remoteAgent, ann agentwire.AnnounceConfigWire) {
	cl := ra.anyClient()
	if cl == nil {
		return
	}
	for host, passkey := range ann.Passkeys {
		_ = cl.SetAnnounceOverride(agentwire.AnnounceOverrideParams{Kind: "passkey", Host: host, Passkey: passkey})
	}
	for host, spoof := range ann.Clients {
		_ = cl.SetAnnounceOverride(agentwire.AnnounceOverrideParams{
			Kind: "client", Host: host, PeerIDPrefix: spoof.PeerIDPrefix, UserAgent: spoof.UserAgent})
	}
	s.agentConfigState.set(ra.name, agentwire.ConfigState{Source: agentwire.ConfigSourceLocal})
	slog.Warn("agent predates apply_config: pushed announce overrides only, its session config stays local",
		"agent", ra.name, "addr", ra.addr)
}

// PushConfigToAgents recomposes and pushes to every connected agent, returning
// (pushed, failed). Called after a settings change so an edit lands on the
// fleet immediately instead of waiting for the next reconcile tick. An
// unreachable agent counts as failed, not fatal: the reconciler retries, and
// the agent's own cache keeps it on the last config it was given.
func (s *Server) PushConfigToAgents() (int, int) {
	cfg := s.liveConfig()
	pushed, failed := 0, 0
	for _, ra := range s.agentsSnapshot() {
		if s.syncAgentConfig(cfg, ra) {
			pushed++
		} else {
			failed++
		}
	}
	return pushed, failed
}

// pushConfigToAgentsAsync schedules a push and returns how many agents it will
// reach. The config editors use this rather than PushConfigToAgents: an edit to
// a session field restarts engines on every node it touches, and making the
// browser hold the save request open for that is how a save looks hung.
func (s *Server) pushConfigToAgentsAsync() int {
	n := len(s.agentsSnapshot())
	if n == 0 {
		return 0
	}
	go s.PushConfigToAgents()
	return n
}

// isUnimplemented reports whether an agent answered "I do not know this
// method", which is how an older agent binary declines a new call.
func isUnimplemented(err error) bool {
	return status.Code(err) == codes.Unimplemented
}

// isUnreachable reports whether the agent could not be talked to at all, as
// opposed to having answered with a refusal.
func isUnreachable(err error) bool {
	switch status.Code(err) {
	case codes.Unavailable, codes.DeadlineExceeded, codes.Canceled:
		return true
	}
	return false
}

// AgentConfigState returns the last config state an agent reported, for the
// agents view.
func (s *Server) AgentConfigState(name string) (agentwire.ConfigState, bool) {
	return s.agentConfigState.get(name)
}
