package api

import (
	"encoding/json"
	"fmt"
	"github.com/Kheopsian/hydra/internal/engine/ltclient"
	"html/template"
	"io/fs"
	"log/slog"
	"net/http"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	hydraroot "github.com/Kheopsian/hydra"
	"github.com/Kheopsian/hydra/internal/agentwire"
	"github.com/Kheopsian/hydra/internal/bench"
	"github.com/Kheopsian/hydra/internal/config"
	"github.com/Kheopsian/hydra/internal/engine"
	"github.com/Kheopsian/hydra/internal/engine/grpcclient"
	"github.com/Kheopsian/hydra/internal/jobs"
	"github.com/Kheopsian/hydra/internal/state"
	"github.com/gin-gonic/gin"

	"github.com/Kheopsian/hydra/internal/logs"
)

// ---------------------------------------------------------------------------
// Package-level version & startup progress
// ---------------------------------------------------------------------------

// Version is set from main at build time or init.
var Version = "dev"

var (
	startupMu       sync.RWMutex
	startupTotal    int32
	startupProgress int32
	startupReady    atomic.Bool
)

// SetStartupTotal sets the number of startup steps for progress tracking.
func SetStartupTotal(n int) {
	atomic.StoreInt32(&startupTotal, int32(n))
}

// SetStartupProgress sets the current startup step number.
func SetStartupProgress(n int) {
	atomic.StoreInt32(&startupProgress, int32(n))
}

// SetStartupReady marks the daemon as fully started.
func SetStartupReady(ready bool) {
	startupReady.Store(ready)
}

// ---------------------------------------------------------------------------
// Engine interfaces
// ---------------------------------------------------------------------------

// RaceEngine abstracts the race (speed-oriented) torrent engine.
type RaceEngine interface {
	// Handing this torrent to the other engine, progression included.
	Role() engine.Role
	ExportTorrentState(infoHash string) (*ltclient.ResumeRecord, error)
	AdoptTorrent(rec *ltclient.ResumeRecord, category string) error
	ReleaseTorrent(infoHash string) error

	// SampleServedInfoHash returns one info hash this engine holds, for the
	// connectivity check's handshake probe. "" when it holds none.
	SampleServedInfoHash() string
	SetUserPaused(infoHash string, paused bool) error
	MatchHashes(f engine.TorrentFilter, exclude map[string]bool) []string
	GetAllStatus() []map[string]interface{}
	GetTorrentDetail(infoHash string) map[string]interface{}
	GetTorrentFileList(infoHash string) []map[string]interface{}
	GetTorrentAvailability(infoHash string) map[string]interface{}
	SetEngineOptFlag(name string, on bool, value int64) (map[string]interface{}, error)
	EngineOptFlags() (map[string]interface{}, error)
	// InboundAccepted counts peers that connected to us: proof of reachability.
	InboundAccepted() (int64, error)
	GetTorrentStatus(infoHash string) map[string]interface{}
	AddTorrent(torrentPath, magnetURI, savePath string, trackers []string, category string) (string, error)
	FetchMetadata(infoHash string, trackers, peers []string, bindingID *uint32) (*ltclient.FetchMetadataResult, error)
	GetMetadata(infoHash string) (*ltclient.GetMetadataResult, error)
	AddTorrentSeedMode(torrentPath, savePath, category string) (string, error)
	RemoveTorrent(infoHash string, deleteFiles bool) error
	ReannnounceTorrent(infoHash string) bool
	AddTrackerToTorrent(infoHash, url string) error
	GetTrackerTiers(infoHash string) ([][]string, error)
	SetTrackerTiers(infoHash string, tiers [][]string) ([][]string, error)
	TorrentFilePath(infoHash string) (string, bool)
	GetChokingStats() map[string]interface{}
	GetSessionSettings() map[string]interface{}
	ApplySettings(settings map[string]interface{}) map[string]interface{}
	SetListenPort(port int) error
	ListenPort() int
	HasTorrent(infoHash string) bool
	SessionGrabbed() int64
	AggregateStats() map[string]interface{}
	GetAllStatusJSON() []json.RawMessage
	GetSessionTotals() (int64, int64)
	ClearCategoryLabel(category string) int
}

// HoardEngine abstracts the hoard (long-term seeding) torrent engine.
type HoardEngine interface {
	// SampleServedInfoHash returns one info hash this engine holds, for the
	// connectivity check's handshake probe. "" when it holds none.
	SampleServedInfoHash() string
	GetAllStatus() map[string]interface{}
	GetTorrentList() []map[string]interface{}
	// GetTorrentListInCategory returns only the rows in one category. The
	// *arr stack polls the qBittorrent shim once per category, and every one
	// of those polls was building a row for the whole catalogue before
	// discarding all but its own: at 196k torrents the five categories they
	// ask for hold 4.4k torrents, so 97.7% of the work was thrown away.
	GetTorrentListInCategory(category string) []map[string]interface{}
	GetTorrentListJSON() []json.RawMessage
	GetSessionTotals() (int64, int64)
	GetTorrentDetail(infoHash string) map[string]interface{}
	GetTorrentFileList(infoHash string) []map[string]interface{}
	GetTorrentAvailability(infoHash string) map[string]interface{}
	SetEngineOptFlag(name string, on bool, value int64) (map[string]interface{}, error)
	EngineOptFlags() (map[string]interface{}, error)
	// InboundAccepted counts peers that connected to us: proof of reachability.
	InboundAccepted() (int64, error)
	AddTorrent(torrentPath, savePath, category string) (string, error)
	FetchMetadata(infoHash string, trackers, peers []string, bindingID *uint32) (*ltclient.FetchMetadataResult, error)
	GetMetadata(infoHash string) (*ltclient.GetMetadataResult, error)
	AddTorrentSeedMode(torrentPath, savePath, category string) (string, error)
	RemoveTorrent(infoHash string, deleteFiles bool) error
	ReannnounceTorrent(infoHash string) bool
	AddTrackerToTorrent(infoHash, url string) error
	GetTrackerTiers(infoHash string) ([][]string, error)
	SetTrackerTiers(infoHash string, tiers [][]string) ([][]string, error)
	TorrentFilePath(infoHash string) (string, bool)
	SetListenPort(port int) error
	ListenPort() int
	HasTorrent(infoHash string) bool
	PauseAll() int
	ResumeAll() int
	SetUserPaused(infoHash string, paused bool) error
	MarkAllUserPaused(paused bool) int
	MatchHashes(f engine.TorrentFilter, exclude map[string]bool) []string
	RestartStuckVerifying() int
	VerifyDownloading() int
	VerifyTorrent(infoHash string) error
	SetTorrentCategory(infoHash, newCategory, newSavePath string) error
	// Handing this torrent to the other engine, progression included. The
	// category's mode decides which engine should hold a torrent, so a
	// category change can imply a move.
	Role() engine.Role
	ExportTorrentState(infoHash string) (*ltclient.ResumeRecord, error)
	AdoptTorrent(rec *ltclient.ResumeRecord, category string) error
	ReleaseTorrent(infoHash string) error
	SetCategoryLabel(infoHash, category string) error
	ClearCategoryLabel(category string) int
	GetTags(infoHash string) []string
	GetAllTags() map[string][]string
	SetTags(infoHash string, tags []string) error
	AddTags(infoHash string, tags []string) error
	RemoveTags(infoHash string, tags []string) error
	SetAddedTime(infoHash string, t time.Time)
	SetCompletedTime(infoHash string, t time.Time)
	SetContentFolder(infoHash string, cf *bool)
	// Download slot manager runtime control.
	GetDownloadSlotStatus() engine.DownloadSlotStats
	SetDownloadSlotsOverride(max int)
	ClearDownloadSlotsOverride()
	// Per-torrent pin (always-hold-a-slot, exempt from seed-rank + demote).
	PinTorrent(infoHash string)
	UnpinTorrent(infoHash string)
	PinnedList() []string

	// Push event fan-out (nil on engines that don't support push).
	// Used by /api/events (SSE) to stream Typhon updates to the browser.
	EventHub() *engine.EventHub
}

// RaceDrainService abstracts the NVMe drain subsystem.
// GraduationReporter surfaces in-flight race->hoard copies for the UI.
type GraduationReporter interface {
	GraduationsSnapshot() []map[string]interface{}
}

type RaceDrainService interface {
	GetStatus() map[string]interface{}
	GetHistory() []map[string]interface{}
	DrainNow() map[string]interface{}
}

// ArrCleanupService abstracts the Sonarr/Radarr cleanup subsystem.
type ArrCleanupService interface {
	Scan() map[string]interface{}
	Execute(params map[string]interface{}) map[string]interface{}
}

// BenchDB abstracts the benchmark database.
type BenchDB interface {
	GetCurrent() map[string]interface{}
	GetRecords() map[string]interface{}
	GetRange(start, end, step int) []map[string]interface{}
	GetComparison(start, mid, end int) map[string]interface{}
	InsertVpn(ts, ulMbps, dlMbps, ulTorrentMbps, dlTorrentMbps float64)
	GetVpnLatest() map[string]interface{}
	GetVpnRange(start, end float64) []map[string]interface{}
	GetRaceEvents(start, end float64) []bench.RaceEvent
	GetRaceEventsForTorrent(infoHash string) []bench.RaceEvent
	GetRaceSnapshots(infoHash string) []bench.RaceSnapshot
	GetTrackerCurrent() []map[string]interface{}
	GetTrackerRange(start, end, step int, tracker string) []map[string]interface{}
	// The write half, used by the race timeline recorder: on a controller node
	// this package is what samples the agents, so it writes as well as reads.
	InsertRaceEvent(ev bench.RaceEvent)
	InsertRaceSnapshots(snapshots []bench.RaceSnapshot)
	PurgeOld() int64
}

// HealthReporter exposes the latest invariant-anomaly report. Satisfied by
// *health.Scanner; injected by main.go so this package doesn't import health.
type HealthReporter interface {
	Snapshot() map[string]interface{}
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

// Server is the main HTTP API server for Hydra.
type Server struct {
	router      *gin.Engine
	config      *config.HydraConfig
	raceEngine  RaceEngine
	hoardEngine HoardEngine
	// jobs runs the work that outlives a request -- payload moves today,
	// whatever a rules engine schedules later. nil on a front-only node.
	jobs           *jobs.Manager
	stateManager   *state.Manager
	raceDrain      RaceDrainService
	gradReporter   GraduationReporter
	arrCleanup     ArrCleanupService
	benchDB        BenchDB
	fleet          fleetStats // agents' share of the overview totals
	healthReporter HealthReporter
	// saveStateFn flushes state.json on demand. Wired by main.go after
	// NewServer; called e.g. right after a category move so the new
	// save_path survives a crash before the periodic 5-min save.
	saveStateFn func()

	// startupPauseRelease lifts the startup pause on every engine and returns
	// the scopes it actually released. Wired by main.go, which owns the engine
	// clients needed to resume their dial queues. Nil = nothing to release.
	startupPauseRelease func() []string

	// remoteAgents are dialed HydraAgents for multi-home category placement.
	// The built-in "local" agent is s.raceEngine/s.hoardEngine, not listed here.
	remoteAgents []*remoteAgent

	// frontOnly hides the local agent (controller node with no local engine).
	frontOnly bool

	// agentsMu guards remoteAgents for runtime add/remove via the Agents menu.
	agentsMu sync.RWMutex

	// agentConfigState is the last config revision each agent reported back,
	// so the agents view can show which nodes are running what (agentconfig.go).
	agentConfigState configStateCache

	// reconnect backs incremental SSE reconnects (delta since a cursor).
	reconnect *reconnectState

	// agentRows caches the remote agents' hoard torrents (see agentrows.go).
	agentRows agentRowCache

	// sseClients counts the browsers on /api/events. The hub's own NumSubs
	// cannot answer that: the reconnect watcher holds a subscription for the
	// life of the process, so it never reads zero.
	sseClients atomic.Int32
}

// NewServer creates a new API server with all routes registered.
func NewServer(cfg *config.HydraConfig) *Server {
	gin.SetMode(gin.ReleaseMode)
	gin.DefaultWriter = logs.Default.NewWriter("gin", "INFO")
	gin.DefaultErrorWriter = logs.Default.NewWriter("gin", "ERROR")
	router := gin.New()
	router.Use(gin.Recovery())
	router.Use(gin.LoggerWithConfig(gin.LoggerConfig{
		Output: logs.Default.NewWriter("gin", "INFO"),
		SkipPaths: []string{"/health", "/api/startup", "/api/status", "/api/events",
			"/api/hoard/stats", "/api/port-forward", "/api/public-ip",
			"/api/categories", "/api/agents", "/api/logs", "/api/logs/stream"},
	}))

	s := &Server{
		router: router,
		config: cfg,
	}

	// Load HTML templates from the embedded FS (self-contained binary).
	if tmpl, err := template.ParseFS(hydraroot.WebAssets, "web/templates/*.html"); err == nil {
		router.SetHTMLTemplate(tmpl)
	} else {
		slog.Warn("Failed to load templates", "error", err)
	}

	// Static files, served from the embedded FS.
	if staticFS, err := fs.Sub(hydraroot.WebAssets, "web/static"); err == nil {
		router.Use(func(c *gin.Context) {
			if strings.HasPrefix(c.Request.URL.Path, "/static/") {
				c.Header("Cache-Control", "no-cache")
			}
			c.Next()
		})
		router.StaticFS("/static", http.FS(staticFS))
	} else {
		slog.Warn("Failed to mount static assets", "error", err)
	}

	// Changelog: the UI fetches /changelog.md; serve the embedded root file
	// (single source, no /static duplication and no wildcard route conflict).
	router.GET("/changelog.md", func(c *gin.Context) {
		data, err := hydraroot.WebAssets.ReadFile("CHANGELOG.md")
		if err != nil {
			c.String(http.StatusNotFound, "changelog unavailable")
			return
		}
		c.Data(http.StatusOK, "text/markdown; charset=utf-8", data)
	})

	// Register routes
	s.registerHydraRoutes()
	s.registerQbitRoutes()

	return s
}

// remoteAgent is a dialed HydraAgent. One control client per agent suffices for
// Ping (online) + AddRouted (rich placement add); the read-side aggregation is
// a later step.
// remoteEngine is one engine of a remote agent node, dialled by engine-id.
type remoteEngine struct {
	id   string
	role string
	// client is the interface, not *grpcclient.Client, so that an engine
	// running in THIS process can be registered exactly like a remote one.
	// That is the whole of "1 agent = 1 engine": an addressing change, with no
	// caller able to tell the two apart. See agentclient.go for why a local
	// implementation must not go through the wire.
	client AgentClient
}

type remoteAgent struct {
	name    string
	addr    string
	engines []remoteEngine
	// local marks an agent whose engines run in this process. The only thing
	// that still depends on it is what the UI is told ("local" vs "grpc"), and
	// the fact that there is no address to show. Everything else -- resolving
	// an engine, listing, placement, actions -- goes through the same code as a
	// remote agent, which is the entire point.
	local bool
}

// anyClient returns any connected engine client. Routed add/action dispatch
// on the agent by the engine-id param, not by which client carries the call.
func (ra *remoteAgent) anyClient() AgentClient {
	if len(ra.engines) > 0 {
		return ra.engines[0].client
	}
	return nil
}

// resolveEngine maps an engine selector -- an engine id ("race-0") OR a role
// ("race") -- to the client and the REAL engine id to put on the wire. The
// agent indexes its rich engines by config id, so sending a role straight
// through misses whenever the id is not literally "race"/"hoard". Returns a
// nil client when the agent hosts nothing matching.
func (ra *remoteAgent) resolveEngine(sel string) (AgentClient, string) {
	for _, e := range ra.engines {
		if e.id == sel {
			return e.client, e.id
		}
	}
	for _, e := range ra.engines {
		if e.role == sel {
			return e.client, e.id
		}
	}
	if sel == "" {
		if c := ra.anyClient(); c != nil {
			return c, ra.engines[0].id
		}
	}
	return nil, sel
}

// byRole returns the agent's engines of a given role.
func (ra *remoteAgent) byRole(role string) []remoteEngine {
	var out []remoteEngine
	for _, e := range ra.engines {
		if e.role == role {
			out = append(out, e)
		}
	}
	return out
}

// AddRemoteAgent dials a remote HydraAgent and registers it for placement. A
// failed dial returns an error (caller logs + skips) so a dead agent never
// blocks boot.
func (s *Server) AddRemoteAgent(name, addr, token, tlsCa string) error {
	// Control client (engine field ignored by ListEngines) to discover engines.
	ctrl, err := grpcclient.New(grpcclient.Config{Addr: addr, Engine: agentwire.EngineRace, Token: token, TLSCa: tlsCa})
	if err != nil {
		return err
	}
	descs, lerr := ctrl.ListEngines()
	ctrl.Close()
	if lerr != nil || len(descs) == 0 {
		// Old agent binary / nothing reported: assume the fixed race+hoard pair.
		descs = []agentwire.EngineDescriptor{{ID: agentwire.EngineRace, Role: "race"}, {ID: agentwire.EngineHoard, Role: "hoard"}}
	}
	var engines []remoteEngine
	for _, d := range descs {
		cl, derr := grpcclient.New(grpcclient.Config{Addr: addr, Engine: d.ID, Token: token, TLSCa: tlsCa})
		if derr != nil {
			for _, e := range engines {
				e.client.Close()
			}
			return derr
		}
		engines = append(engines, remoteEngine{id: d.ID, role: d.Role, client: cl})
	}
	s.agentsMu.Lock()
	s.removeRemoteAgentLocked(name) // replace an existing same-name agent (re-dial)
	s.remoteAgents = append(s.remoteAgents, &remoteAgent{name: name, addr: addr, engines: engines})
	s.agentsMu.Unlock()
	return nil
}

// AddLocalAgent registers an engine of THIS process under an agent name, so it
// is addressed exactly like one on another machine. No dialling, no token, no
// discovery round-trip: the caller already holds the engine, and asking it over
// a socket what engines it has would be asking ourselves.
//
// Repeated calls with the same name ADD engines to that agent rather than
// replacing it, which is how a node ends up hosting more than one. The remote
// path cannot do this -- it learns an agent's engines in one ListEngines -- so
// it replaces instead. Getting that backwards here would mean registering the
// hoard engine silently dropped the race one.
func (s *Server) AddLocalAgent(name, id, role string, cl AgentClient) error {
	if name == "" || id == "" || cl == nil {
		return fmt.Errorf("local agent needs a name, an engine id and a client")
	}
	s.agentsMu.Lock()
	defer s.agentsMu.Unlock()
	for _, ra := range s.remoteAgents {
		if ra.name != name {
			continue
		}
		for i, e := range ra.engines {
			if e.id == id {
				ra.engines[i] = remoteEngine{id: id, role: role, client: cl}
				return nil
			}
		}
		ra.engines = append(ra.engines, remoteEngine{id: id, role: role, client: cl})
		return nil
	}
	// addr stays empty: there is no address, and inventing a loopback one would
	// show up in the UI as a node reachable at a port nothing listens on.
	s.remoteAgents = append(s.remoteAgents, &remoteAgent{
		name: name, local: true, engines: []remoteEngine{{id: id, role: role, client: cl}},
	})
	return nil
}

// remoteAgentByName returns the dialed agent or nil.
func (s *Server) remoteAgentByName(name string) *remoteAgent {
	s.agentsMu.RLock()
	defer s.agentsMu.RUnlock()
	for _, ra := range s.remoteAgents {
		if ra.name == name {
			return ra
		}
	}
	return nil
}

// SetEngines injects the engine dependencies after creation.
func (s *Server) SetEngines(race RaceEngine, hoard HoardEngine) {
	s.raceEngine = race
	s.hoardEngine = hoard

	// Per-engine total functions for offset capture and session delta.
	raceTotalFunc = func() (int64, int64) {
		if s.raceEngine != nil {
			return s.raceEngine.GetSessionTotals()
		}
		return 0, 0
	}
	hoardTotalFunc = func() (int64, int64) {
		if s.hoardEngine != nil {
			return s.hoardEngine.GetSessionTotals()
		}
		return 0, 0
	}

	// Combined total for baseline.
	RegisterSessionTotalFunc(func() (int64, int64) {
		rUL, rDL := raceTotalFunc()
		hUL, hDL := hoardTotalFunc()
		return rUL + hUL, rDL + hDL
	})
}

// CaptureSessionOffset snapshots each engine's cumulative totals at boot, so
// the session delta counts only what moved since. Must be called AFTER both
// engines have finished loading their torrents, or the offset lands too low and
// the session delta is inflated by everything they load afterwards.
//
// Only per-engine offsets exist: they are the ones absorbTorrentStats knows how
// to decrement on removal. Do not reintroduce a combined one.
func (s *Server) CaptureSessionOffset() {
	if sessionTotalFunc == nil {
		slog.Warn("baseline: cannot capture offset, sessionTotalFunc not set")
		return
	}
	ul, dl := sessionTotalFunc()

	if raceTotalFunc != nil {
		rUL, rDL := raceTotalFunc()
		atomic.StoreInt64(&sessionOffsetRaceUL, rUL)
		atomic.StoreInt64(&sessionOffsetRaceDL, rDL)
		slog.Info("baseline: race offset captured", "offset_ul", rUL, "offset_dl", rDL)
	}
	if hoardTotalFunc != nil {
		hUL, hDL := hoardTotalFunc()
		atomic.StoreInt64(&sessionOffsetHoardUL, hUL)
		atomic.StoreInt64(&sessionOffsetHoardDL, hDL)
		slog.Info("baseline: hoard offset captured", "offset_ul", hUL, "offset_dl", hDL)
	}

	slog.Info("baseline: session offset captured", "offset_ul", ul, "offset_dl", dl)
}

// SetStateManager injects the state manager.
func (s *Server) SetStateManager(sm *state.Manager) {
	s.stateManager = sm
}

// SetSaveStateCallback wires a function that synchronously persists
// state.json. Used by handlers that mutate torrent metadata (e.g.
// category move) so the change survives a crash before the next tick.
func (s *Server) SetSaveStateCallback(fn func()) {
	s.saveStateFn = fn
}

// SetStartupPauseRelease injects the callback that lifts the startup pause.
func (s *Server) SetStartupPauseRelease(fn func() []string) {
	s.startupPauseRelease = fn
}

// SetRaceDrain injects the RaceDrain service.
func (s *Server) SetGraduationReporter(g GraduationReporter) { s.gradReporter = g }

func (s *Server) SetRaceDrain(rd RaceDrainService) {
	s.raceDrain = rd
}

// SetArrCleanup injects the arr cleanup service.
func (s *Server) SetArrCleanup(ac ArrCleanupService) {
	s.arrCleanup = ac
}

// SetBenchDB injects the benchmark database.
func (s *Server) SetBenchDB(b BenchDB) {
	s.benchDB = b
}

// SetHealthReporter injects the invariant-anomaly reporter.
func (s *Server) SetHealthReporter(hr HealthReporter) {
	s.healthReporter = hr
}

// Run starts the HTTP server on the configured host:port.
func (s *Server) Run() error {
	StartTunnelPoller()
	s.startSnapshotPusher()
	s.reconnect = newReconnectState()
	s.startReconnectWatcher()
	s.startAgentRowPusher()
	s.startAgentRaceStatsSampler()
	addr := fmt.Sprintf("%s:%d", s.config.Daemon.APIHost, s.config.Daemon.APIPort)
	slog.Info("Starting Hydra API server", "addr", addr, "version", Version)
	return s.router.Run(addr)
}

// Router returns the underlying gin.Engine (useful for testing).
func (s *Server) Router() http.Handler {
	return s.router
}

// SetJobManager wires the background job runner. Called once at startup.
// LocalAgentName is how the node running the front refers to itself, matching
// the "local" default already used by category placement.
const LocalAgentName = "local"

func (s *Server) SetJobManager(m *jobs.Manager) { s.jobs = m }

// RemoteAgentEngineClient resolves a named agent's engine to its live client.
// engine matches an engine id first, then a role, so both "hoard" as an id and
// "hoard" as a role resolve the way a caller would expect.
//
// The returned client is the shared, long-lived one: callers must not close it.
func (s *Server) RemoteAgentEngineClient(agent, engine string) (AgentClient, error) {
	ra := s.remoteAgentByName(agent)
	if ra == nil {
		return nil, fmt.Errorf("no agent named %q is registered", agent)
	}
	s.agentsMu.RLock()
	defer s.agentsMu.RUnlock()
	for _, e := range ra.engines {
		if e.id == engine {
			return e.client, nil
		}
	}
	for _, e := range ra.engines {
		if e.role == engine {
			return e.client, nil
		}
	}
	if c := ra.anyClient(); c != nil && engine == "" {
		return c, nil
	}
	return nil, fmt.Errorf("agent %q has no engine %q", agent, engine)
}
