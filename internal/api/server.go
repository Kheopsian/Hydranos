package api

import (
	"encoding/json"
	"fmt"
	"html/template"
	"io/fs"
	"log/slog"
	"net/http"
	"sync"
	"sync/atomic"
	"time"

	hydraroot "github.com/Kheopsian/hydra"
	"github.com/Kheopsian/hydra/internal/agentwire"
	"github.com/Kheopsian/hydra/internal/bench"
	"github.com/Kheopsian/hydra/internal/config"
	"github.com/Kheopsian/hydra/internal/engine"
	"github.com/Kheopsian/hydra/internal/engine/grpcclient"
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
	SetUserPaused(infoHash string, paused bool) error
	GetAllStatus() []map[string]interface{}
	GetTorrentDetail(infoHash string) map[string]interface{}
	GetTorrentFileList(infoHash string) []map[string]interface{}
	GetTorrentAvailability(infoHash string) map[string]interface{}
	SetEngineOptFlag(name string, on bool, value int64) (map[string]interface{}, error)
	EngineOptFlags() (map[string]interface{}, error)
	GetTorrentStatus(infoHash string) map[string]interface{}
	AddTorrent(torrentPath, magnetURI, savePath string, trackers []string, category string) (string, error)
	AddTorrentSeedMode(torrentPath, savePath, category string) (string, error)
	RemoveTorrent(infoHash string, deleteFiles bool) error
	ReannnounceTorrent(infoHash string) bool
	AddTrackerToTorrent(infoHash, url string) error
	GetChokingStats() map[string]interface{}
	GetSessionSettings() map[string]interface{}
	ApplySettings(settings map[string]interface{}) map[string]interface{}
	SetListenPort(port int)
	HasTorrent(infoHash string) bool
	SessionGrabbed() int64
	AggregateStats() map[string]interface{}
	GetAllStatusJSON() []json.RawMessage
	GetSessionTotals() (int64, int64)
	ClearCategoryLabel(category string) int
}

// HoardEngine abstracts the hoard (long-term seeding) torrent engine.
type HoardEngine interface {
	GetAllStatus() map[string]interface{}
	GetTorrentList() []map[string]interface{}
	GetTorrentListJSON() []json.RawMessage
	GetSessionTotals() (int64, int64)
	GetTorrentDetail(infoHash string) map[string]interface{}
	GetTorrentFileList(infoHash string) []map[string]interface{}
	GetTorrentAvailability(infoHash string) map[string]interface{}
	SetEngineOptFlag(name string, on bool, value int64) (map[string]interface{}, error)
	EngineOptFlags() (map[string]interface{}, error)
	AddTorrent(torrentPath, savePath, category string) (string, error)
	AddTorrentSeedMode(torrentPath, savePath, category string) (string, error)
	RemoveTorrent(infoHash string, deleteFiles bool) error
	ReannnounceTorrent(infoHash string) bool
	AddTrackerToTorrent(infoHash, url string) error
	SetListenPort(port int)
	HasTorrent(infoHash string) bool
	PauseAll() int
	ResumeAll() int
	SetUserPaused(infoHash string, paused bool) error
	MarkAllUserPaused(paused bool) int
	RestartStuckVerifying() int
	VerifyDownloading() int
	VerifyTorrent(infoHash string) error
	SetTorrentCategory(infoHash, newCategory, newSavePath string) error
	SetCategoryLabel(infoHash, category string) error
	ClearCategoryLabel(category string) int
	GetTags(infoHash string) []string
	GetAllTags() map[string][]string
	SetTags(infoHash string, tags []string) error
	AddTags(infoHash string, tags []string) error
	RemoveTags(infoHash string, tags []string) error
	SetAddedTime(infoHash string, t time.Time)
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
	router         *gin.Engine
	config         *config.HydraConfig
	raceEngine     RaceEngine
	hoardEngine    HoardEngine
	stateManager   *state.Manager
	raceDrain      RaceDrainService
	gradReporter   GraduationReporter
	arrCleanup     ArrCleanupService
	benchDB        BenchDB
	healthReporter HealthReporter
	// saveStateFn flushes state.json on demand. Wired by main.go after
	// NewServer; called e.g. right after a category move so the new
	// save_path survives a crash before the periodic 5-min save.
	saveStateFn func()

	// remoteAgents are dialed HydraAgents for multi-home category placement.
	// The built-in "local" agent is s.raceEngine/s.hoardEngine, not listed here.
	remoteAgents []*remoteAgent

	// frontOnly hides the local agent (controller node with no local engine).
	frontOnly bool

	// agentsMu guards remoteAgents for runtime add/remove via the Agents menu.
	agentsMu sync.RWMutex

	// reconnect backs incremental SSE reconnects (delta since a cursor).
	reconnect *reconnectState
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
	id     string
	role   string
	client *grpcclient.Client
}

type remoteAgent struct {
	name    string
	addr    string
	engines []remoteEngine
}

// anyClient returns any connected engine client. Routed add/action dispatch
// on the agent by the engine-id param, not by which client carries the call.
func (ra *remoteAgent) anyClient() *grpcclient.Client {
	if len(ra.engines) > 0 {
		return ra.engines[0].client
	}
	return nil
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

// CaptureSessionOffset snapshots the current Rain BoltDB cumulative totals.
// Must be called AFTER both engines have finished loading torrents from BoltDB.
func (s *Server) CaptureSessionOffset() {
	if sessionTotalFunc == nil {
		slog.Warn("baseline: cannot capture offset, sessionTotalFunc not set")
		return
	}
	ul, dl := sessionTotalFunc()
	atomic.StoreInt64(&sessionOffsetUL, ul)
	atomic.StoreInt64(&sessionOffsetDL, dl)

	// Per-engine offsets for accurate session stats in /api/status.
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
	addr := fmt.Sprintf("%s:%d", s.config.Daemon.APIHost, s.config.Daemon.APIPort)
	slog.Info("Starting Hydra API server", "addr", addr, "version", Version)
	return s.router.Run(addr)
}

// Router returns the underlying gin.Engine (useful for testing).
func (s *Server) Router() http.Handler {
	return s.router
}
