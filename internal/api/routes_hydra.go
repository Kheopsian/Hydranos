package api

import (
	"bufio"
	"encoding/json"
	"fmt"
	"io"
	"log/slog"
	"net"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"sort"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/Kheopsian/hydra/internal/agentwire"
	"github.com/Kheopsian/hydra/internal/bench"
	"github.com/Kheopsian/hydra/internal/config"
	"github.com/Kheopsian/hydra/internal/engine"
	"github.com/Kheopsian/hydra/internal/engine/ltclient"
	"github.com/Kheopsian/hydra/internal/store"
	"github.com/Kheopsian/hydra/internal/tagstore"
	"github.com/gin-gonic/gin"
)

// ---------------------------------------------------------------------------
// Baseline persistence
// ---------------------------------------------------------------------------

var (
	baselineUploaded   int64
	baselineDownloaded int64
	baselineFile       string

	// sessionOffset captures Rain BoltDB cumulative totals at boot time,
	// so we can compute session = current_rain - offset.
	sessionOffsetUL int64
	sessionOffsetDL int64

	// Per-engine offsets for accurate session stats in /api/status.
	sessionOffsetRaceUL  int64
	sessionOffsetRaceDL  int64
	sessionOffsetHoardUL int64
	sessionOffsetHoardDL int64

	// Day baseline : snapshot du session cumulé à la dernière bascule minuit
	// Europe/Paris. day_uploaded = sessionUL - dayBaselineUL.
	dayBaselineUL   int64
	dayBaselineDL   int64
	dayBaselineDate string
	dayMu           sync.Mutex
	parisLoc        *time.Location
)

// sessionTotalFunc returns (totalUL, totalDL) summed from all engines.
// These are Rain's BoltDB cumulative totals (never reset).
var sessionTotalFunc func() (int64, int64)

// Per-engine total funcs for capturing per-engine offsets.
var raceTotalFunc func() (int64, int64)
var hoardTotalFunc func() (int64, int64)

// RegisterSessionTotalFunc allows the server to provide a callback
// that reads current cumulative UL/DL from Rain engines.
func RegisterSessionTotalFunc(f func() (int64, int64)) {
	sessionTotalFunc = f
}

// GetRaceSessionDelta returns race UL/DL transferred since boot.
func GetRaceSessionDelta() (ul, dl int64) {
	if raceTotalFunc == nil {
		return 0, 0
	}
	rawUL, rawDL := raceTotalFunc()
	ul = rawUL - atomic.LoadInt64(&sessionOffsetRaceUL)
	dl = rawDL - atomic.LoadInt64(&sessionOffsetRaceDL)
	if ul < 0 {
		ul = 0
	}
	if dl < 0 {
		dl = 0
	}
	return
}

// GetHoardSessionDelta returns hoard UL/DL transferred since boot. Memoised
// for a second when totals_cache is on: the SSE pusher asks on every tick and
// the underlying call rescans the torrent set.
func GetHoardSessionDelta() (ul, dl int64) {
	return memoHoardDelta.get(getHoardSessionDeltaUncached)
}

func getHoardSessionDeltaUncached() (ul, dl int64) {
	if hoardTotalFunc == nil {
		return 0, 0
	}
	rawUL, rawDL := hoardTotalFunc()
	ul = rawUL - atomic.LoadInt64(&sessionOffsetHoardUL)
	dl = rawDL - atomic.LoadInt64(&sessionOffsetHoardDL)
	if ul < 0 {
		ul = 0
	}
	if dl < 0 {
		dl = 0
	}
	return
}

// GetDayDelta returns UL/DL transferred since last midnight (Europe/Paris).
// Resets automatically at midnight ; resets aussi si le session cumulé régresse
// (restart process détecté). Un restart en cours de journée perd le cumul
// pré-restart — acceptable vu que session_uploaded repart à 0 au boot.
func GetDayDelta() (ul, dl int64) {
	raceUL, raceDL := GetRaceSessionDelta()
	hoardUL, hoardDL := GetHoardSessionDelta()
	totalUL := raceUL + hoardUL
	totalDL := raceDL + hoardDL

	if parisLoc == nil {
		parisLoc, _ = time.LoadLocation("Europe/Paris")
	}
	now := time.Now()
	if parisLoc != nil {
		now = now.In(parisLoc)
	}
	today := now.Format("2006-01-02")

	dayMu.Lock()
	defer dayMu.Unlock()

	if dayBaselineDate != today {
		dayBaselineDate = today
		atomic.StoreInt64(&dayBaselineUL, totalUL)
		atomic.StoreInt64(&dayBaselineDL, totalDL)
	}
	blUL := atomic.LoadInt64(&dayBaselineUL)
	blDL := atomic.LoadInt64(&dayBaselineDL)
	if totalUL < blUL || totalDL < blDL {
		atomic.StoreInt64(&dayBaselineUL, 0)
		atomic.StoreInt64(&dayBaselineDL, 0)
		blUL, blDL = 0, 0
	}
	ul = totalUL - blUL
	dl = totalDL - blDL
	if ul < 0 {
		ul = 0
	}
	if dl < 0 {
		dl = 0
	}
	return
}

func initBaselinePersistence(dataDir string) {
	baselineFile = filepath.Join(dataDir, "baseline.json")
	loadBaseline()

	// Background ticker: rolls dayBaseline at midnight even when no
	// client is hitting /api/status. Without it, day_uploaded /
	// day_downloaded stay stuck on the previous day's baseline until
	// the first GET /api/status of the new day — visible if a browser
	// tab is opened mid-afternoon (the operator's case 2026-05-08, day_ul
	// reset only when the dashboard was opened at 16:46).
	go func() {
		t := time.NewTicker(time.Minute)
		defer t.Stop()
		for range t.C {
			_, _ = GetDayDelta() // side effect: rolls baseline at midnight Paris
		}
	}()
}

func loadBaseline() {
	// The store is authoritative once there is one; baseline.json is only read
	// on a node that has no database (front-only). An installation upgrading
	// from the JSON era has already had its file imported into the store by
	// store.MigrateSidecars before this runs.
	if loadCountersFromStore() {
		return
	}

	var bl struct {
		TotalUploaded   int64 `json:"total_uploaded"`
		TotalDownloaded int64 `json:"total_downloaded"`
	}

	if data, err := os.ReadFile(baselineFile); err == nil {
		if json.Unmarshal(data, &bl) == nil {
			atomic.StoreInt64(&baselineUploaded, bl.TotalUploaded)
			atomic.StoreInt64(&baselineDownloaded, bl.TotalDownloaded)
		}
	}
}

func saveBaseline() {
	ul := atomic.LoadInt64(&baselineUploaded)
	dl := atomic.LoadInt64(&baselineDownloaded)

	if st := durable(); st != nil {
		if err := st.CounterSet(store.CounterGlobal, ul, dl); err != nil {
			logCounterErr("save global baseline", err)
		}
		return
	}

	bl := struct {
		TotalUploaded   int64 `json:"total_uploaded"`
		TotalDownloaded int64 `json:"total_downloaded"`
	}{TotalUploaded: ul, TotalDownloaded: dl}
	data, err := json.MarshalIndent(bl, "", "  ")
	if err != nil {
		return
	}
	tmp := baselineFile + ".tmp"
	if err := os.WriteFile(tmp, data, 0644); err != nil {
		return
	}
	os.Rename(tmp, baselineFile)
}

// getRainTotals returns current cumulative UL/DL from Rain (Bolt). Memoised
// for a second under totals_cache; getGlobalTotals rides on this one too.
func getRainTotals() (ul, dl int64) {
	return memoRain.get(getRainTotalsUncached)
}

func getRainTotalsUncached() (ul, dl int64) {
	if sessionTotalFunc != nil {
		return sessionTotalFunc()
	}
	return 0, 0
}

// getSessionDelta returns UL/DL transferred since boot.
func getSessionDelta() (ul, dl int64) {
	rawUL, rawDL := getRainTotals()
	ul = rawUL - atomic.LoadInt64(&sessionOffsetUL)
	dl = rawDL - atomic.LoadInt64(&sessionOffsetDL)
	if ul < 0 {
		ul = 0
	}
	if dl < 0 {
		dl = 0
	}
	return
}

// getGlobalTotals returns all-time UL/DL (baseline + Rain Bolt).
// GetGlobalTotals exposes the monotone lifetime cumulative UL/DL (baseline +
// live session) for the bench sampler. Never resets on restart or torrent
// removal, so MAX-MIN over a time range yields exact bytes.
func GetGlobalTotals() (ul, dl int64) { return getGlobalTotals() }

func getGlobalTotals() (ul, dl int64) {
	rainUL, rainDL := getRainTotals()
	return atomic.LoadInt64(&baselineUploaded) + rainUL, atomic.LoadInt64(&baselineDownloaded) + rainDL
}

// absorbTorrentStats adds a torrent's cumulative UL/DL to the baseline
// before it is removed, so all-time global totals remain correct.
func (s *Server) absorbTorrentStats(infoHash string) {
	var ul, dl int64
	var matched string // "hoard" | "race"

	// Check hoard engine torrent list
	if s.hoardEngine != nil {
		for _, t := range s.hoardEngine.GetTorrentList() {
			if h, _ := t["info_hash"].(string); h == infoHash {
				if v, ok := t["total_upload"].(int64); ok {
					ul = v
				}
				if v, ok := t["total_download"].(int64); ok {
					dl = v
				}
				matched = "hoard"
				break
			}
		}
	}

	// Check race engine torrent list
	if matched == "" && s.raceEngine != nil {
		for _, t := range s.raceEngine.GetAllStatus() {
			if h, _ := t["info_hash"].(string); h == infoHash {
				if v, ok := t["total_upload"].(int64); ok {
					ul = v
				}
				if v, ok := t["total_download"].(int64); ok {
					dl = v
				}
				matched = "race"
				break
			}
		}
	}

	if ul > 0 || dl > 0 {
		atomic.AddInt64(&baselineUploaded, ul)
		atomic.AddInt64(&baselineDownloaded, dl)
		// Compenser la baisse de SUM(t.total_uploaded) côté engine, sinon
		// session_uploaded (et day_uploaded) saigne à chaque remove.
		switch matched {
		case "hoard":
			atomic.AddInt64(&sessionOffsetHoardUL, -ul)
			atomic.AddInt64(&sessionOffsetHoardDL, -dl)
		case "race":
			atomic.AddInt64(&sessionOffsetRaceUL, -ul)
			atomic.AddInt64(&sessionOffsetRaceDL, -dl)
		}
		saveBaseline()
	}
}

// ---------------------------------------------------------------------------
// Categories persistence
// ---------------------------------------------------------------------------

type category struct {
	Name       string            `json:"name"`
	SavePath   string            `json:"save_path"`
	Mode       string            `json:"mode"`
	GraduateTo string            `json:"graduate_to,omitempty"` // race only: target hoard category for graduation
	Agents     map[string]string `json:"agents,omitempty"`      // per-agent save_path override; empty = flat SavePath (local agent)
	Placement  []string          `json:"placement,omitempty"`   // agent names hosting new torrents; empty = ["local"]
	Strategy   string            `json:"strategy,omitempty"`    // pick among Placement: all|least_torrents|least_load (default all)
}

// SavePathFor returns the save_path for the given agent, falling back to the flat
// SavePath (local/default agent). Multi-agent overrides live in Agents (Phase C).
func (cat category) SavePathFor(agent string) string {
	if pth, ok := cat.Agents[agent]; ok && pth != "" {
		return pth
	}
	return cat.SavePath
}

// categoryJSON is the on-disk format: {"save_path": "...", "mode": "..."}.
type categoryJSON struct {
	SavePath   string            `json:"save_path"`
	Mode       string            `json:"mode"`
	GraduateTo string            `json:"graduate_to,omitempty"`
	Agents     map[string]string `json:"agents,omitempty"`
	Placement  []string          `json:"placement,omitempty"`
	Strategy   string            `json:"strategy,omitempty"`
}

func categoriesFile(dataDir string) string {
	return filepath.Join(dataDir, "categories.json")
}

func loadCategories(dataDir string) []category {
	data := []byte(metaDoc(store.MetaCategories))
	if len(data) == 0 {
		var err error
		if data, err = os.ReadFile(categoriesFile(dataDir)); err != nil {
			return []category{}
		}
	}
	// On-disk format is a map: {"name": {"save_path": "...", "mode": "..."}}
	var catMap map[string]categoryJSON
	if json.Unmarshal(data, &catMap) != nil {
		return []category{}
	}
	cats := make([]category, 0, len(catMap))
	for name, cj := range catMap {
		cats = append(cats, category{Name: name, SavePath: cj.SavePath, Mode: cj.Mode, GraduateTo: cj.GraduateTo, Agents: cj.Agents, Placement: cj.Placement, Strategy: cj.Strategy})
	}
	sort.Slice(cats, func(i, j int) bool { return cats[i].Name < cats[j].Name })
	return cats
}

func saveCategories(dataDir string, cats []category) error {
	catMap := make(map[string]categoryJSON, len(cats))
	for _, cat := range cats {
		catMap[cat.Name] = categoryJSON{SavePath: cat.SavePath, Mode: cat.Mode, GraduateTo: cat.GraduateTo, Agents: cat.Agents, Placement: cat.Placement, Strategy: cat.Strategy}
	}
	data, err := json.MarshalIndent(catMap, "", "  ")
	if err != nil {
		return err
	}
	if setMetaDoc(store.MetaCategories, string(data)) {
		return nil
	}
	// No store (front-only): keep the file behaviour.
	tmp := categoriesFile(dataDir) + ".tmp"
	if err := os.WriteFile(tmp, data, 0644); err != nil {
		return err
	}
	return os.Rename(tmp, categoriesFile(dataDir))
}

// agentInfo is the front's view of one engine host (data plane). The built-in
// "local" agent is this process's own race/hoard engines; remote [[agent]]
// entries join the registry as the api.Server retrofit lands (multi-home add).
// engineInfo is one role-typed engine hosted by an agent (Option A: a node
// hosts an arbitrary set of engines addressed by id).
type engineInfo struct {
	ID     string `json:"id"`
	Role   string `json:"role"`
	Online bool   `json:"online"`
}

type agentInfo struct {
	Name       string              `json:"name"`
	Kind       string              `json:"kind"` // "local" | "grpc"
	Online     bool                `json:"online"`
	Addr       string              `json:"addr,omitempty"`
	Engines    []engineInfo        `json:"engines,omitempty"`
	ExitIP     string              `json:"exit_ip,omitempty"`
	Interfaces []agentwire.NICInfo `json:"interfaces,omitempty"`
}

// handleAgentsGet lists the agents a category's placement can target. v1 exposes
// only "local"; the categories UI reads this to populate placement checkboxes
// and per-agent save-path rows.
func (s *Server) handleAgentsGet(c *gin.Context) {
	var agents []agentInfo
	if !s.frontOnly {
		var engs []engineInfo
		if s.raceEngine != nil {
			engs = append(engs, engineInfo{ID: "race", Role: "race", Online: true})
		}
		if s.hoardEngine != nil {
			engs = append(engs, engineInfo{ID: "hoard", Role: "hoard", Online: true})
		}
		agents = append(agents, agentInfo{Name: "local", Kind: "local", Online: len(engs) > 0, Engines: engs, ExitIP: getPublicIP(), Interfaces: localNICs()})
	}
	for _, ra := range s.agentsSnapshot() {
		var engs []engineInfo
		online := false
		for _, e := range ra.engines {
			up := e.client != nil && e.client.Ping() == nil
			online = online || up
			engs = append(engs, engineInfo{ID: e.id, Role: e.role, Online: up})
		}
		var exitIP string
		var ifaces []agentwire.NICInfo
		if len(ra.engines) > 0 && ra.engines[0].client != nil {
			if ni, nerr := ra.engines[0].client.NodeInfo(); nerr == nil {
				exitIP, ifaces = ni.PublicIP, ni.Interfaces
			}
		}
		agents = append(agents, agentInfo{Name: ra.name, Kind: "grpc", Addr: ra.addr, Online: online, Engines: engs, ExitIP: exitIP, Interfaces: ifaces})
	}
	c.JSON(http.StatusOK, agents)
}

// ---------------------------------------------------------------------------
// Route registration
// ---------------------------------------------------------------------------

func (s *Server) registerHydraRoutes() {
	// Initialize baseline persistence
	initBaselinePersistence(s.config.Daemon.DataDir)
	initTrackerBaseline(s.config.Daemon.DataDir)

	// Public routes (no auth)
	s.router.GET("/health", s.handleHealth)
	s.router.GET("/metrics", s.handleMetrics)
	s.router.GET("/api/startup", s.handleStartup)
	s.router.GET("/", s.handleIndex)
	s.router.POST("/api/login", s.handleLogin) // public: verifie creds -> renvoie l API key

	// Authenticated API routes
	api := s.router.Group("/api", s.apiKeyAuth())
	{
		// Torrent management
		api.POST("/torrents", s.handleAddTorrent)
		api.POST("/torrents/upload", s.handleUploadTorrent)
		api.DELETE("/torrents/:info_hash", s.handleRemoveTorrent)
		api.POST("/torrents/:info_hash/reannounce", s.handleReannounce)
		api.POST("/torrents/:info_hash/add-tracker", s.handleAddTracker)
		api.GET("/torrents/:info_hash/files", s.handleTorrentFiles)

		// Per-tracker announce passkey override (hot-swap, no restart)
		api.GET("/announce/passkeys", s.handleGetPasskeys)
		api.POST("/announce/passkeys", s.handleSetPasskey)
		// Per-tracker client spoof (peer_id prefix + UA) to pass client whitelists
		api.GET("/announce/clients", s.handleGetClients)
		api.POST("/announce/clients", s.handleSetClient)

		api.GET("/opt/flags", s.handleGetOptFlags)
		api.POST("/opt/flags", s.handleSetOptFlag)

		api.GET("/announce/secondary-stats", s.handleGetSecondaryStats)
		api.POST("/announce/secondary-stats", s.handleSetSecondaryStats)

		// Per-host tracker aggregate (announce health + override state)
		api.GET("/trackers", s.handleGetTrackers)

		// Global status
		api.GET("/status", s.handleStatus)
		api.GET("/update-check", s.handleUpdateCheck)

		// Server-Sent Events stream (Typhon push → browser).
		// Emits the same wire-format as Rust: {"event":"stats_snapshot","data":{...}}.
		api.GET("/events", s.handleSSE)

		// In-process log hub (UI Logs tab)
		api.GET("/logs", s.handleLogs)
		api.GET("/logs/stream", s.handleLogsStream)

		// qBittorrent migration (onboarding import)
		api.POST("/import/qbit/preview", s.handleQbitPreview)
		api.POST("/import/transmission/preview", s.handleTransmissionPreview)
		api.POST("/import/transmission/upload", s.handleTransmissionUpload)
		api.POST("/import/transmission/start", s.handleTransmissionStart)
		api.POST("/import/qbit/start", s.handleQbitStart)
		api.GET("/import/qbit/events", s.handleQbitEvents)
		api.GET("/import/qbit/status", s.handleQbitStatus)
		api.GET("/provenance", s.handleProvenance)

		// Race engine
		race := api.Group("/race")
		{
			race.GET("/torrents", s.handleRaceTorrents)
			race.GET("/torrents/:info_hash", s.handleRaceTorrentDetail)
			race.GET("/choking", s.handleRaceChoking)
			race.GET("/settings", s.handleRaceSettingsGet)
			race.POST("/settings", s.handleRaceSettingsPost)
			race.GET("/timeline/:info_hash", s.handleRaceTimeline)
			race.POST("/torrents/:info_hash/purge", s.handleRacePurge)
			race.POST("/torrents/:info_hash/pause", s.handleRacePause)
			race.POST("/torrents/:info_hash/resume", s.handleRaceResume)
			race.POST("/pause", s.handleRacePauseBulk)
			race.POST("/torrents/bulk", s.handleRaceBulk)
			race.POST("/listen-port", s.handleRaceSetListenPort)
		}

		// Hoard engine
		hoard := api.Group("/hoard")
		{
			hoard.GET("/stats", s.handleHoardStats)
			hoard.GET("/torrents", s.handleHoardTorrents)
			hoard.GET("/torrents/:info_hash", s.handleHoardTorrentDetail)
			hoard.POST("/torrents/:info_hash/pause", s.handleHoardPause)
			hoard.POST("/torrents/:info_hash/resume", s.handleHoardResume)
			hoard.POST("/pause", s.handleHoardPauseBulk)
			// Bulk stop/start driven by the on-screen filter (see routes_bulk.go).
			hoard.POST("/torrents/bulk", s.handleHoardBulk)
			hoard.POST("/pause-all", s.handleHoardPauseAll)
			hoard.POST("/resume-all", s.handleHoardResumeAll)
			hoard.POST("/listen-port", s.handleHoardSetListenPort)
			hoard.POST("/restart-stuck", s.handleHoardRestartStuck)
			hoard.POST("/verify-downloading", s.handleHoardVerifyDownloading)
			hoard.POST("/torrents/:info_hash/verify", s.handleHoardVerifyTorrent)
			hoard.POST("/torrents/:info_hash/category", s.handleHoardSetCategory)
			hoard.POST("/torrents/:info_hash/tags", s.handleHoardSetTags)
			hoard.GET("/download-slots", s.handleHoardDownloadSlotsGet)
			hoard.POST("/download-slots", s.handleHoardDownloadSlotsSet)
			hoard.DELETE("/download-slots", s.handleHoardDownloadSlotsClear)
			hoard.POST("/torrents/:info_hash/pin", s.handleHoardPin)
			hoard.POST("/torrents/:info_hash/unpin", s.handleHoardUnpin)
			hoard.GET("/pinned", s.handleHoardPinnedList)
		}

		// Categories
		api.GET("/categories", s.handleCategoriesGet)
		api.GET("/tags", s.handleGetTags)
		api.GET("/agents", s.handleAgentsGet)
		api.POST("/agents", s.handleAgentCreate)
		api.POST("/agents/test", s.handleAgentTest)
		api.PUT("/agents/:name", s.handleAgentUpdate)
		api.GET("/agents/removed", s.handleAgentsRemovedGet)
		api.POST("/agents/restore/:name", s.handleAgentRestore)
		api.DELETE("/agents/:name", s.handleAgentDelete)
		api.GET("/engines", s.handleEnginesGet)
		api.POST("/engines", s.handleEnginesPost)
		api.DELETE("/engines/:id", s.handleEnginesDelete)
		api.POST("/restart", s.handleRestart)
		api.GET("/categories/orphans", s.handleCategoriesOrphans)
		api.POST("/categories", s.handleCategoryCreate)
		api.PUT("/categories/:name", s.handleCategoryUpdate)
		api.DELETE("/categories/:name", s.handleCategoryDelete)

		// Filesystem
		api.GET("/fs/browse", s.handleFSBrowse)

		// Peers

		// Drain
		drain := api.Group("/drain")
		{
			drain.GET("/status", s.handleDrainStatus)
			drain.GET("/history", s.handleDrainHistory)
			drain.POST("/now", s.handleDrainNow)
			drain.GET("/graduations", s.handleGraduations)
		}

		// Arr cleanup
		arr := api.Group("/arr-cleanup")
		{
			arr.GET("/scan", s.handleArrCleanupScan)
			arr.POST("/execute", s.handleArrCleanupExecute)
		}

		// Benchmark
		bench := api.Group("/benchmark")
		{
			bench.GET("/current", s.handleBenchCurrent)
			bench.GET("/records", s.handleBenchRecords)
			bench.GET("/range", s.handleBenchRange)
			bench.GET("/compare", s.handleBenchCompare)
			bench.GET("/race-events", s.handleRaceEvents)
			bench.GET("/race-snapshots/:info_hash", s.handleRaceSnapshots)
			bench.GET("/trackers/current", s.handleTrackerStatsCurrent)
			bench.GET("/trackers/range", s.handleTrackerStatsRange)
		}

		// Health / invariant anomalies (integrity, not performance)
		api.GET("/health/anomalies", s.handleHealthAnomalies)

		// VPN speedtest
		vpn := api.Group("/vpn-speedtest")
		{
			vpn.GET("/latest", s.handleVPNSpeedtestLatest)
			vpn.GET("/history", s.handleVPNSpeedtestHistory)
			vpn.POST("/run", s.handleVPNSpeedtestRun)
		}

		// Baseline
		api.GET("/stats/baseline", s.handleBaselineGet)
		api.POST("/stats/baseline", s.handleBaselinePost)

		// Config
		api.GET("/settings", s.handleSettingsGet)
		api.POST("/settings", s.handleSettingsPost)
		api.POST("/settings/restart", s.handleSettingsRestart)
		api.POST("/auth/password", s.handleSetPassword)

		// Port forward
		api.GET("/port-forward", s.handlePortForwardStatus)
		api.GET("/network/interfaces", s.handleNetworkInterfaces)

		// System
		api.GET("/public-ip", s.handlePublicIP)
	}
}

// ===========================================================================
// Handlers — System & Health
// ===========================================================================

func (s *Server) handleHealth(c *gin.Context) {
	c.JSON(http.StatusOK, gin.H{
		"status":  "healthy",
		"version": Version,
		"uptime":  time.Since(startTime).Seconds(),
	})
}

var startTime = time.Now()

func (s *Server) handleMetrics(c *gin.Context) {
	var lines []string

	lines = append(lines, fmt.Sprintf("hydra_up 1"))
	lines = append(lines, fmt.Sprintf("hydra_uptime_seconds %.0f", time.Since(startTime).Seconds()))

	// Race engine metrics
	if s.raceEngine != nil {
		torrents := s.raceEngine.GetAllStatus()
		lines = append(lines, fmt.Sprintf("hydra_race_torrents_total %d", len(torrents)))
		var dlRate, ulRate int64
		for _, t := range torrents {
			if dr, ok := t["download_rate"].(int64); ok {
				dlRate += dr
			}
			if ur, ok := t["upload_rate"].(int64); ok {
				ulRate += ur
			}
		}
		lines = append(lines, fmt.Sprintf("hydra_race_download_bytes_per_second %d", dlRate))
		lines = append(lines, fmt.Sprintf("hydra_race_upload_bytes_per_second %d", ulRate))
	}

	// Hoard engine metrics
	if s.hoardEngine != nil {
		stats := s.hoardEngine.GetAllStatus()
		if count, ok := stats["torrent_count"]; ok {
			lines = append(lines, fmt.Sprintf("hydra_hoard_torrents_total %v", count))
		}
	}

	// Baseline metrics
	lines = append(lines, fmt.Sprintf("hydra_baseline_uploaded_bytes %d", atomic.LoadInt64(&baselineUploaded)))
	lines = append(lines, fmt.Sprintf("hydra_baseline_downloaded_bytes %d", atomic.LoadInt64(&baselineDownloaded)))

	c.Data(http.StatusOK, "text/plain; charset=utf-8", []byte(strings.Join(lines, "\n")+"\n"))
}

func (s *Server) handleStartup(c *gin.Context) {
	c.JSON(http.StatusOK, gin.H{
		"ready":    startupReady.Load(),
		"total":    atomic.LoadInt32(&startupTotal),
		"restored": atomic.LoadInt32(&startupProgress),
	})
}

func (s *Server) handleIndex(c *gin.Context) {
	c.Header("Cache-Control", "no-cache")
	c.HTML(http.StatusOK, "index.html", gin.H{
		"Version": Version,
	})
}

// ===========================================================================
// Handlers — Torrent Management
// ===========================================================================

// resolveCategory fills empty savePath/mode from the named category's on-disk
// config (categories.json) so category->save_path routing is authoritative on
// the server for every client (UI, native API, qBit shim), not just when the
// frontend pre-fills the path.
func (s *Server) resolveCategory(category, savePath, mode string) (string, string) {
	if category == "" {
		return savePath, mode
	}
	for _, cat := range loadCategories(s.config.Daemon.DataDir) {
		if strings.EqualFold(cat.Name, category) {
			// The category is authoritative for the engine role: picking a
			// category must route there even if the caller also sent a default
			// mode (the web form always sends one). The save path is only filled
			// when the caller left it empty, so an explicit path (cross-seed) wins.
			if cat.Mode != "" {
				mode = cat.Mode
			}
			if savePath == "" {
				savePath = cat.SavePath
			}
			break
		}
	}
	return savePath, mode
}

// categoryByName returns the full category by name (case-insensitive), or nil.
func (s *Server) categoryByName(name string) *category {
	if name == "" {
		return nil
	}
	for _, c := range loadCategories(s.config.Daemon.DataDir) {
		if strings.EqualFold(c.Name, name) {
			cc := c
			return &cc
		}
	}
	return nil
}

// agentTorrentCount is the placement metric for the least_torrents strategy.
// Unknown/unreachable agents return maxInt so they are never chosen.
func (s *Server) agentTorrentCount(name string) int {
	const maxInt = int(^uint(0) >> 1)
	if name == "local" {
		n := 0
		if s.raceEngine != nil {
			n += len(s.raceEngine.GetAllStatus())
		}
		if s.hoardEngine != nil {
			n += len(s.hoardEngine.GetTorrentList())
		}
		return n
	}
	ra := s.remoteAgentByName(name)
	if ra == nil {
		return maxInt
	}
	n := 0
	for _, e := range ra.engines {
		if lst, err := e.client.ListTorrentsTimeout(4 * time.Second); err == nil && lst != nil {
			n += lst.Count
		}
	}
	return n
}

// ltStatusToRow maps a remote agent's ltclient.TorrentStatus into the same
// JSON row shape the UI reads for local torrents, tagged with its origin agent.
func ltStatusToRow(t ltclient.TorrentStatus, agent string) map[string]interface{} {
	ratio := 0.0
	if t.TotalDownload > 0 {
		ratio = float64(t.TotalUpload) / float64(t.TotalDownload)
	}
	return map[string]interface{}{
		"info_hash": t.InfoHash, "name": t.Name, "state": t.State, "progress": t.Progress,
		"total_size": t.TotalSize, "total_upload": t.TotalUpload, "total_download": t.TotalDownload,
		"upload_rate": t.UploadRate, "download_rate": t.DownloadRate,
		"num_peers": t.NumPeers, "num_seeds": t.NumSeeds,
		"swarm_seeds": t.ListSeeds, "swarm_leechers": t.ListPeers,
		"save_path": t.SavePath, "added_time": t.AddedTime, "completed_time": t.CompletedTime,
		"ratio": ratio, "tracker_error": t.TrackerError, "tracker_error_msg": t.TrackerErrorMsg,
		"torrent_error": false, "torrent_error_msg": "", "injected_peers": 0,
		"injection_hit": false, "uploader": "", "category": "", "agent": agent,
	}
}

// addTargets applies a category's placement + strategy to decide which agents a
// new torrent lands on. Empty category or empty placement => ["local"] (the
// unchanged monolith behavior).
func (s *Server) addTargets(catName string) []string {
	cat := s.categoryByName(catName)
	if cat == nil || len(cat.Placement) == 0 {
		return []string{s.defaultAgent()}
	}
	switch cat.Strategy {
	case "least_torrents":
		best, bestN := cat.Placement[0], int(^uint(0)>>1)
		for _, t := range cat.Placement {
			if n := s.agentTorrentCount(t); n < bestN {
				bestN, best = n, t
			}
		}
		return []string{best}
	default: // "" | "all" => fan-out (multi-home)
		return cat.Placement
	}
}

// ensureSavePathWritable fails an add up front when the destination cannot be
// written. Without it the torrent is accepted, sits in "downloading" with no
// error, and only fails whenever the first piece happens to land -- which can
// be never if the swarm is quiet. This is the usual shape of a PUID/PGID
// mistake: the daemon runs as a user that cannot write the payload directory.
func ensureSavePathWritable(savePath string) error {
	if strings.TrimSpace(savePath) == "" {
		return nil
	}
	if err := os.MkdirAll(savePath, 0o755); err != nil {
		return fmt.Errorf("save path %q cannot be created (running as uid %d): %w", savePath, os.Getuid(), err)
	}
	probe, err := os.CreateTemp(savePath, ".hydra-write-probe-*")
	if err != nil {
		return fmt.Errorf("save path %q is not writable (running as uid %d): %w", savePath, os.Getuid(), err)
	}
	name := probe.Name()
	probe.Close()
	os.Remove(name)
	return nil
}

// localAdd performs the rich local add for a mode (today's monolith path).
func (s *Server) localAdd(mode, torrentPath, magnetURI, savePath string, trackers []string, category string) (string, error) {
	if err := ensureSavePathWritable(savePath); err != nil {
		return "", err
	}
	switch mode {
	case "race":
		if s.raceEngine == nil {
			return "", fmt.Errorf("race engine not available")
		}
		return s.raceEngine.AddTorrent(torrentPath, magnetURI, savePath, trackers, category)
	case "hoard":
		if s.hoardEngine == nil {
			return "", fmt.Errorf("hoard engine not available")
		}
		return s.hoardEngine.AddTorrent(torrentPath, savePath, category)
	default:
		return "", fmt.Errorf("invalid mode %q", mode)
	}
}

// routeAdd dispatches one add to a single target agent (local rich engine, or a
// remote agent's rich AddRouted). Remote requires a .torrent path (no magnet).
func (s *Server) routeAdd(target, mode, torrentPath, magnetURI, savePath, category string, trackers []string, cat *category) (string, error) {
	// A magnet has no metadata yet, so there is nothing to place: resolve it
	// first, then come back through here with a real .torrent. This is the only
	// magnet-aware branch in the add path -- race, hoard and remote agents all
	// keep taking the same road they already took.
	if torrentPath == "" && magnetURI != "" {
		return s.startMagnetResolve(target, mode, magnetURI, savePath, category, trackers, cat)
	}
	if target == "local" {
		return s.localAdd(mode, torrentPath, magnetURI, savePath, trackers, category)
	}
	ra := s.remoteAgentByName(target)
	if ra == nil {
		return "", fmt.Errorf("unknown agent %q", target)
	}
	if torrentPath == "" {
		return "", fmt.Errorf("remote agent %q requires a .torrent path (magnet unsupported)", target)
	}
	sp := savePath
	if cat != nil {
		sp = cat.SavePathFor(target)
	}
	var r *ltclient.AddTorrentResult
	r, err := ra.anyClient().AddRouted(mode, torrentPath, sp, category)
	if err != nil {
		return "", err
	}
	if r == nil {
		return "", nil
	}
	return r.InfoHash, nil
}

// raceDiskFull reports whether a new race torrent should be rejected because the
// NVMe is (near) full. It first triggers an emergency drain, then rechecks, so a
// burst of grabs is only refused when even draining cannot make room. Returns
// (true, message) to reject with 507. No-op unless add_block_enabled.
func (s *Server) raceDiskFull() (bool, string) {
	if s.raceDrain == nil || !s.config.RaceDrain.AddBlockEnabled {
		return false, ""
	}
	over := func() (bool, float64, int64) {
		st := s.raceDrain.GetStatus()
		pct, _ := st["disk_used_pct"].(float64)
		used, _ := st["disk_used"].(int64)
		total, _ := st["disk_total"].(int64)
		free := total - used
		reserve := int64(s.config.RaceDrain.ReserveFreeGB) * 1_000_000_000
		high := float64(s.config.RaceDrain.HighWatermarkPct)
		bad := (high > 0 && pct >= high) || (reserve > 0 && free < reserve)
		return bad, pct, free
	}
	if bad, _, _ := over(); !bad {
		return false, ""
	}
	s.raceDrain.DrainNow()
	if bad, pct, free := over(); bad {
		return true, fmt.Sprintf("race NVMe full: %.0f%% used, %.1f GB free — add rejected (drain could not free enough)", pct, float64(free)/1e9)
	}
	return false, ""
}

// raceDrainOnAddIfFull triggers a background emergency drain when a new race add
// lands on a (near) full NVMe. It never blocks the add — missing a grab is worse
// than a transient disk-full that the drain resolves. No-op if the drain is off.
func (s *Server) raceDrainOnAddIfFull() {
	if s.raceDrain == nil || !s.config.RaceDrain.Enabled {
		return
	}
	st := s.raceDrain.GetStatus()
	pct, _ := st["disk_used_pct"].(float64)
	high := float64(s.config.RaceDrain.HighWatermarkPct)
	if high > 0 && pct >= high {
		go s.raceDrain.DrainNow()
	}
}

func (s *Server) handleAddTorrent(c *gin.Context) {
	var req struct {
		TorrentPath string   `json:"torrent_path"`
		MagnetURI   string   `json:"magnet_uri"`
		SavePath    string   `json:"save_path"`
		Mode        string   `json:"mode"`
		Category    string   `json:"category"`
		Trackers    []string `json:"trackers"`
	}
	if err := c.ShouldBindJSON(&req); err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
		return
	}

	req.SavePath, req.Mode = s.resolveCategory(req.Category, req.SavePath, req.Mode)
	mode := req.Mode
	if mode == "" {
		mode = "race"
	}

	if mode == "race" {
		s.raceDrainOnAddIfFull()
	}

	cat := s.categoryByName(req.Category)
	targets := s.addTargets(req.Category)

	type addResult struct {
		Agent    string `json:"agent"`
		InfoHash string `json:"info_hash,omitempty"`
		Error    string `json:"error,omitempty"`
	}
	var results []addResult
	var firstHash string
	var firstErr error
	for _, t := range targets {
		ih, aerr := s.routeAdd(t, mode, req.TorrentPath, req.MagnetURI, req.SavePath, req.Category, req.Trackers, cat)
		r := addResult{Agent: t, InfoHash: ih}
		if aerr != nil {
			r.Error = aerr.Error()
			if firstErr == nil {
				firstErr = aerr
			}
		}
		if ih != "" && firstHash == "" {
			firstHash = ih
		}
		results = append(results, r)
	}
	if firstHash == "" && firstErr != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": firstErr.Error(), "targets": results})
		return
	}
	c.JSON(http.StatusOK, gin.H{
		"status":    "ok",
		"info_hash": firstHash,
		"mode":      mode,
		"targets":   results,
	})
}

func (s *Server) handleUploadTorrent(c *gin.Context) {
	file, header, err := c.Request.FormFile("file")
	if err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": "no torrent file in request"})
		return
	}
	defer file.Close()

	// Save uploaded file to temp location
	tmpDir := filepath.Join(s.config.Daemon.DataDir, "uploads")
	os.MkdirAll(tmpDir, 0755)
	tmpPath := filepath.Join(tmpDir, header.Filename)

	out, err := os.Create(tmpPath)
	if err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": "failed to save upload"})
		return
	}
	defer out.Close()

	if _, err := io.Copy(out, file); err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": "failed to write upload"})
		return
	}
	out.Close()

	savePath := c.PostForm("save_path")
	mode := c.PostForm("mode")
	category := c.PostForm("category")
	savePath, mode = s.resolveCategory(category, savePath, mode)
	if mode == "" {
		mode = "race"
	}
	if savePath == "" {
		savePath = "/data"
	}

	cat := s.categoryByName(category)
	var infoHash string
	err = nil
	for _, t := range s.addTargets(category) {
		ih, aerr := s.routeAdd(t, mode, tmpPath, "", savePath, category, nil, cat)
		if aerr != nil && err == nil {
			err = aerr
		}
		if ih != "" && infoHash == "" {
			infoHash = ih
		}
	}
	if infoHash != "" {
		err = nil // at least one target succeeded
	}

	if err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}

	c.JSON(http.StatusOK, gin.H{
		"status":    "ok",
		"info_hash": infoHash,
		"mode":      mode,
	})
}

func (s *Server) handleRemoveTorrent(c *gin.Context) {
	infoHash := strings.ToLower(c.Param("info_hash"))
	deleteFilesStr := c.DefaultQuery("delete_files", "false")
	deleteFiles := deleteFilesStr == "true" || deleteFilesStr == "1"

	removed := false

	if s.raceEngine != nil && s.raceEngine.HasTorrent(infoHash) {
		// Stats absorption gérée par le callback OnBeforeRemove de l'engine.
		if err := s.raceEngine.RemoveTorrent(infoHash, deleteFiles); err != nil {
			c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
			return
		}
		removed = true
	}

	if s.hoardEngine != nil && s.hoardEngine.HasTorrent(infoHash) {
		// Stats absorption gérée par le callback OnBeforeRemove de l'engine.
		if err := s.hoardEngine.RemoveTorrent(infoHash, deleteFiles); err != nil {
			c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
			return
		}
		removed = true
	}

	if !removed {
		if ra, mode, ok := s.findRemoteOwner(infoHash); ok {
			if err := ra.anyClient().ActionRouted(mode, "remove", infoHash, deleteFiles, "", ""); err != nil {
				c.JSON(http.StatusBadGateway, gin.H{"error": err.Error()})
				return
			}
			c.JSON(http.StatusOK, gin.H{"status": "ok", "agent": ra.name})
			return
		}
		c.JSON(http.StatusNotFound, gin.H{"error": "torrent not found"})
		return
	}

	// Remove from persistent state
	if s.stateManager != nil {
		s.stateManager.RemoveTorrent(infoHash)
	}

	c.JSON(http.StatusOK, gin.H{"status": "ok"})
}

func (s *Server) handleReannounce(c *gin.Context) {
	infoHash := strings.ToLower(c.Param("info_hash"))

	reannounced := false
	if s.raceEngine != nil && s.raceEngine.HasTorrent(infoHash) {
		reannounced = s.raceEngine.ReannnounceTorrent(infoHash)
	} else if s.hoardEngine != nil && s.hoardEngine.HasTorrent(infoHash) {
		reannounced = s.hoardEngine.ReannnounceTorrent(infoHash)
	}

	if !reannounced {
		if ra, mode, ok := s.findRemoteOwner(infoHash); ok {
			if err := ra.anyClient().ActionRouted(mode, "reannounce", infoHash, false, "", ""); err != nil {
				c.JSON(http.StatusBadGateway, gin.H{"error": err.Error()})
				return
			}
			c.JSON(http.StatusOK, gin.H{"status": "ok", "agent": ra.name})
			return
		}
		c.JSON(http.StatusNotFound, gin.H{"error": "torrent not found or reannounce failed"})
		return
	}

	c.JSON(http.StatusOK, gin.H{"status": "ok"})
}

func (s *Server) handleAddTracker(c *gin.Context) {
	infoHash := strings.ToLower(c.Param("info_hash"))

	var body struct {
		URL string `json:"url" binding:"required"`
	}
	if err := c.ShouldBindJSON(&body); err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": "url is required"})
		return
	}

	var err error
	if s.raceEngine != nil && s.raceEngine.HasTorrent(infoHash) {
		err = s.raceEngine.AddTrackerToTorrent(infoHash, body.URL)
	} else if s.hoardEngine != nil && s.hoardEngine.HasTorrent(infoHash) {
		err = s.hoardEngine.AddTrackerToTorrent(infoHash, body.URL)
	} else {
		c.JSON(http.StatusNotFound, gin.H{"error": "torrent not found"})
		return
	}

	if err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}

	c.JSON(http.StatusOK, gin.H{"status": "ok"})
}

// ===========================================================================
// Handlers — Status & Monitoring
// ===========================================================================

// statusPayload builds the same map served by /api/status. Extracted so the
// SSE snapshot pusher can publish it without going through HTTP.
func (s *Server) statusPayload() gin.H {
	result := gin.H{
		"version": Version,
		"uptime":  time.Since(startTime).Seconds(),
	}

	if s.raceEngine != nil {
		agg := s.raceEngine.AggregateStats()
		raceSessionUL, raceSessionDL := GetRaceSessionDelta()
		var sessionRatio float64
		if raceSessionDL > 0 {
			sessionRatio = float64(raceSessionUL) / float64(raceSessionDL)
		}
		agg["session_uploaded"] = raceSessionUL
		agg["session_downloaded"] = raceSessionDL
		agg["session_grabbed"] = s.raceEngine.SessionGrabbed()
		agg["session_ratio"] = sessionRatio
		result["race"] = agg
	}

	if s.hoardEngine != nil {
		hoardStatus := s.hoardEngine.GetAllStatus()
		if sc, _ := hoardStatus["stagger_complete"].(bool); sc {
			hoardSessionUL, hoardSessionDL := GetHoardSessionDelta()
			hoardStatus["session_uploaded"] = hoardSessionUL
			hoardStatus["session_downloaded"] = hoardSessionDL
		}
		result["hoard"] = hoardStatus
	}

	sessionUL, sessionDL := getSessionDelta()
	globalUL, globalDL := getGlobalTotals()
	result["baseline"] = gin.H{
		"total_uploaded":     atomic.LoadInt64(&baselineUploaded),
		"total_downloaded":   atomic.LoadInt64(&baselineDownloaded),
		"session_uploaded":   sessionUL,
		"session_downloaded": sessionDL,
		"global_uploaded":    globalUL,
		"global_downloaded":  globalDL,
	}

	dayUL, dayDL := GetDayDelta()
	result["day_uploaded"] = dayUL
	result["day_downloaded"] = dayDL

	result["tunnels"] = GetTunnelSnapshot()
	// Cursor for incremental SSE reconnects: the client echoes this back as ?since=.
	result["server_ts"] = time.Now().Unix()
	return result
}

func (s *Server) handleStatus(c *gin.Context) {
	c.JSON(http.StatusOK, s.statusPayload())
}

// ===========================================================================
// Handlers — Race Engine
// ===========================================================================

func (s *Server) handleRaceTorrents(c *gin.Context) {
	out := []interface{}{}
	if s.raceEngine != nil {
		for _, row := range s.raceEngine.GetAllStatus() {
			row["agent"] = "local"
			out = append(out, row)
		}
	}
	for _, ra := range s.agentsSnapshot() {
		for _, e := range ra.byRole("race") {
			lst, err := e.client.ListTorrentsTimeout(4 * time.Second)
			if err != nil || lst == nil {
				continue
			}
			for _, t := range lst.Torrents {
				out = append(out, ltStatusToRow(t, ra.name))
			}
		}
	}
	c.JSON(http.StatusOK, out)
}

func (s *Server) handleRaceTorrentDetail(c *gin.Context) {
	if s.raceEngine == nil {
		c.JSON(http.StatusNotFound, gin.H{"error": "race engine not available"})
		return
	}
	infoHash := strings.ToLower(c.Param("info_hash"))
	detail := s.raceEngine.GetTorrentDetail(infoHash)
	if detail == nil {
		c.JSON(http.StatusNotFound, gin.H{"error": "torrent not found"})
		return
	}
	c.JSON(http.StatusOK, detail)
}

// handleRacePurge retire un torrent du SEUL moteur race (pas du hoard) — comme
// le drain. Déclenche OnBeforeRemove(race) -> AbsorbStats + (si dup) transfert
// d'offset d'annonce vers le hoard. Outil ops + test du handoff race->hoard.
func (s *Server) handleRacePurge(c *gin.Context) {
	if s.raceEngine == nil {
		c.JSON(http.StatusNotFound, gin.H{"error": "race engine not available"})
		return
	}
	infoHash := strings.ToLower(c.Param("info_hash"))
	if !s.raceEngine.HasTorrent(infoHash) {
		c.JSON(http.StatusNotFound, gin.H{"error": "torrent not in race"})
		return
	}
	deleteFiles := c.DefaultQuery("delete_files", "false") == "true"
	if err := s.raceEngine.RemoveTorrent(infoHash, deleteFiles); err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	c.JSON(http.StatusOK, gin.H{"status": "ok", "purged_from": "race", "delete_files": deleteFiles})
}

func (s *Server) handleRaceChoking(c *gin.Context) {
	if s.raceEngine == nil {
		c.JSON(http.StatusOK, gin.H{"enabled": false})
		return
	}
	c.JSON(http.StatusOK, s.raceEngine.GetChokingStats())
}

func (s *Server) handleRaceSettingsGet(c *gin.Context) {
	if s.raceEngine == nil {
		c.JSON(http.StatusOK, gin.H{})
		return
	}
	c.JSON(http.StatusOK, s.raceEngine.GetSessionSettings())
}

func (s *Server) handleRaceSettingsPost(c *gin.Context) {
	if s.raceEngine == nil {
		c.JSON(http.StatusServiceUnavailable, gin.H{"error": "race engine not available"})
		return
	}
	var settings map[string]interface{}
	if err := c.ShouldBindJSON(&settings); err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
		return
	}
	result := s.raceEngine.ApplySettings(settings)
	c.JSON(http.StatusOK, result)
}

// ===========================================================================
// Handlers — Hoard Engine
// ===========================================================================

// hoardStatsPayload mirrors /api/hoard/stats. Extracted for the SSE pusher.
func (s *Server) hoardStatsPayload() gin.H {
	if s.hoardEngine == nil {
		return gin.H{"torrent_count": 0}
	}
	status := s.hoardEngine.GetAllStatus()
	if sc, _ := status["stagger_complete"].(bool); sc {
		hoardSessionUL, hoardSessionDL := GetHoardSessionDelta()
		status["session_uploaded"] = hoardSessionUL
		status["session_downloaded"] = hoardSessionDL
	}
	return status
}

func (s *Server) handleHoardStats(c *gin.Context) {
	c.JSON(http.StatusOK, s.hoardStatsPayload())
}

func (s *Server) handleHoardTorrents(c *gin.Context) {
	out := []interface{}{}
	if s.hoardEngine != nil {
		for _, t := range s.hoardEngine.GetTorrentList() {
			out = append(out, t)
		}
	}
	for _, ra := range s.agentsSnapshot() {
		for _, e := range ra.byRole("hoard") {
			lst, err := e.client.ListTorrentsTimeout(4 * time.Second)
			if err != nil || lst == nil {
				continue
			}
			for _, t := range lst.Torrents {
				out = append(out, ltStatusToRow(t, ra.name))
			}
		}
	}
	c.JSON(http.StatusOK, out)
}

func (s *Server) handleHoardTorrentDetail(c *gin.Context) {
	if s.hoardEngine == nil {
		c.JSON(http.StatusNotFound, gin.H{"error": "hoard engine not available"})
		return
	}
	infoHash := strings.ToLower(c.Param("info_hash"))
	detail := s.hoardEngine.GetTorrentDetail(infoHash)
	if detail == nil {
		c.JSON(http.StatusNotFound, gin.H{"error": "torrent not found"})
		return
	}
	c.JSON(http.StatusOK, detail)
}

func (s *Server) handleHoardPauseAll(c *gin.Context) {
	if s.hoardEngine == nil {
		c.JSON(http.StatusServiceUnavailable, gin.H{"error": "hoard engine not available"})
		return
	}
	count := s.hoardEngine.PauseAll()
	// The intent is the user's, so it is recorded like any other pause.
	s.hoardEngine.MarkAllUserPaused(true)
	persistPausedSession(store.Hoard, true)
	c.JSON(http.StatusOK, gin.H{"status": "ok", "paused": count})
}

func (s *Server) handleHoardResumeAll(c *gin.Context) {
	if s.hoardEngine == nil {
		c.JSON(http.StatusServiceUnavailable, gin.H{"error": "hoard engine not available"})
		return
	}
	count := s.hoardEngine.ResumeAll()
	s.hoardEngine.MarkAllUserPaused(false)
	persistPausedSession(store.Hoard, false)
	c.JSON(http.StatusOK, gin.H{"status": "ok", "resumed": count})
}

func (s *Server) handleHoardRestartStuck(c *gin.Context) {
	if s.hoardEngine == nil {
		c.JSON(http.StatusServiceUnavailable, gin.H{"error": "hoard engine not available"})
		return
	}
	count := s.hoardEngine.RestartStuckVerifying()
	c.JSON(http.StatusOK, gin.H{"status": "ok", "restarted": count})
}

func (s *Server) handleHoardVerifyTorrent(c *gin.Context) {
	hash := c.Param("info_hash")
	if s.hoardEngine != nil && s.hoardEngine.HasTorrent(hash) {
		if err := s.hoardEngine.VerifyTorrent(hash); err != nil {
			c.JSON(http.StatusNotFound, gin.H{"error": err.Error()})
			return
		}
		c.JSON(http.StatusOK, gin.H{"status": "ok", "info_hash": hash})
		return
	}
	if ra, mode, ok := s.findRemoteOwner(hash); ok {
		if err := ra.anyClient().ActionRouted(mode, "verify", hash, false, "", ""); err != nil {
			c.JSON(http.StatusBadGateway, gin.H{"error": err.Error()})
			return
		}
		c.JSON(http.StatusOK, gin.H{"status": "ok", "info_hash": hash, "agent": ra.name})
		return
	}
	c.JSON(http.StatusNotFound, gin.H{"error": "torrent not found"})
}

// handleHoardSetCategory changes a hoard torrent's category at runtime.
// Stops the torrent, renames its data directory under the target category's
// save_path (same-fs only, to keep Sonarr/Radarr hardlinks intact via inode
// preservation), tells Typhon, mirrors Go-side metadata, and restarts.
// Refuses cross-filesystem moves with 409 Conflict.
func (s *Server) handleHoardSetCategory(c *gin.Context) {
	hash := c.Param("info_hash")
	var body struct {
		Category  string `json:"category"`
		MoveFiles bool   `json:"move_files"`
	}
	if err := c.ShouldBindJSON(&body); err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": "invalid body: " + err.Error()})
		return
	}
	if body.Category == "" {
		c.JSON(http.StatusBadRequest, gin.H{"error": "category required"})
		return
	}

	// Resolve target save_path from categories.json.
	cats := loadCategories(s.config.Daemon.DataDir)
	var targetSavePath string
	found := false
	for _, cat := range cats {
		if cat.Name == body.Category {
			targetSavePath = cat.SavePath
			found = true
			break
		}
	}
	if !found {
		c.JSON(http.StatusNotFound, gin.H{"error": "category not found: " + body.Category})
		return
	}
	if body.MoveFiles && targetSavePath == "" {
		c.JSON(http.StatusBadRequest, gin.H{"error": "target category has empty save_path"})
		return
	}

	if s.hoardEngine != nil && s.hoardEngine.HasTorrent(hash) {
		var serr error
		if body.MoveFiles {
			serr = s.hoardEngine.SetTorrentCategory(hash, body.Category, targetSavePath)
		} else {
			serr = s.hoardEngine.SetCategoryLabel(hash, body.Category)
		}
		if err := serr; err != nil {
			msg := err.Error()
			if strings.Contains(msg, "cross-filesystem") {
				c.JSON(http.StatusConflict, gin.H{"error": msg})
				return
			}
			if strings.Contains(msg, "not found") {
				c.JSON(http.StatusNotFound, gin.H{"error": msg})
				return
			}
			c.JSON(http.StatusInternalServerError, gin.H{"error": msg})
			return
		}
		if s.saveStateFn != nil {
			s.saveStateFn()
		}
		c.JSON(http.StatusOK, gin.H{"status": "ok", "info_hash": hash, "category": body.Category, "save_path": targetSavePath})
		return
	}
	if ra, mode, ok := s.findRemoteOwner(hash); ok {
		if err := ra.anyClient().ActionRouted(mode, "setcategory", hash, false, body.Category, targetSavePath); err != nil {
			c.JSON(http.StatusBadGateway, gin.H{"error": err.Error()})
			return
		}
		c.JSON(http.StatusOK, gin.H{"status": "ok", "info_hash": hash, "category": body.Category, "save_path": targetSavePath, "agent": ra.name})
		return
	}
	c.JSON(http.StatusNotFound, gin.H{"error": "torrent not found"})
}

func (s *Server) handleHoardVerifyDownloading(c *gin.Context) {
	if s.hoardEngine == nil {
		c.JSON(http.StatusServiceUnavailable, gin.H{"error": "hoard engine not available"})
		return
	}
	count := s.hoardEngine.VerifyDownloading()
	c.JSON(http.StatusOK, gin.H{"status": "ok", "verified": count})
}

// ===========================================================================
// Handlers — Download Slots
// ===========================================================================

func (s *Server) handleHoardDownloadSlotsGet(c *gin.Context) {
	if s.hoardEngine == nil {
		c.JSON(http.StatusServiceUnavailable, gin.H{"error": "hoard engine not available"})
		return
	}
	c.JSON(http.StatusOK, s.hoardEngine.GetDownloadSlotStatus())
}

func (s *Server) handleHoardDownloadSlotsSet(c *gin.Context) {
	if s.hoardEngine == nil {
		c.JSON(http.StatusServiceUnavailable, gin.H{"error": "hoard engine not available"})
		return
	}
	var req struct {
		Max int `json:"max"`
	}
	if err := c.ShouldBindJSON(&req); err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": "body must be {\"max\": N}"})
		return
	}
	if req.Max < 0 {
		c.JSON(http.StatusBadRequest, gin.H{"error": "max must be >= 0"})
		return
	}
	s.hoardEngine.SetDownloadSlotsOverride(req.Max)
	c.JSON(http.StatusOK, s.hoardEngine.GetDownloadSlotStatus())
}

func (s *Server) handleHoardDownloadSlotsClear(c *gin.Context) {
	if s.hoardEngine == nil {
		c.JSON(http.StatusServiceUnavailable, gin.H{"error": "hoard engine not available"})
		return
	}
	s.hoardEngine.ClearDownloadSlotsOverride()
	c.JSON(http.StatusOK, s.hoardEngine.GetDownloadSlotStatus())
}

// handleHoardPin pins a torrent so the slot manager always keeps it active,
// regardless of swarm-seed rank, and never activity-demotes it. For deliberate
// source grabs (BDMV, rarities) we want regardless of swarm health.
func (s *Server) handleHoardPin(c *gin.Context) {
	if s.hoardEngine == nil {
		c.JSON(http.StatusServiceUnavailable, gin.H{"error": "hoard engine not available"})
		return
	}
	hash := c.Param("info_hash")
	if !s.hoardEngine.HasTorrent(hash) {
		c.JSON(http.StatusNotFound, gin.H{"error": "torrent not in hoard: " + hash})
		return
	}
	s.hoardEngine.PinTorrent(hash)
	c.JSON(http.StatusOK, gin.H{"status": "ok", "info_hash": hash, "pinned": true})
}

// handleHoardUnpin removes a pin.
func (s *Server) handleHoardUnpin(c *gin.Context) {
	if s.hoardEngine == nil {
		c.JSON(http.StatusServiceUnavailable, gin.H{"error": "hoard engine not available"})
		return
	}
	hash := c.Param("info_hash")
	s.hoardEngine.UnpinTorrent(hash)
	c.JSON(http.StatusOK, gin.H{"status": "ok", "info_hash": hash, "pinned": false})
}

// handleHoardPinnedList returns all pinned info_hashes.
func (s *Server) handleHoardPinnedList(c *gin.Context) {
	if s.hoardEngine == nil {
		c.JSON(http.StatusServiceUnavailable, gin.H{"error": "hoard engine not available"})
		return
	}
	c.JSON(http.StatusOK, gin.H{"pinned": s.hoardEngine.PinnedList()})
}

// ===========================================================================
// Handlers — Categories
// ===========================================================================

func (s *Server) handleCategoriesGet(c *gin.Context) {
	cats := loadCategories(s.config.Daemon.DataDir)
	c.JSON(http.StatusOK, cats)
}

// handleCategoriesOrphans lists category labels worn by torrents that match no
// configured category. They are invisible in the categories screen otherwise,
// which is where the only delete button lives.
func (s *Server) handleCategoriesOrphans(c *gin.Context) {
	known := map[string]bool{}
	for _, cat := range loadCategories(s.config.Daemon.DataDir) {
		known[cat.Name] = true
	}
	out := []gin.H{}
	st := durable()
	if st == nil {
		c.JSON(http.StatusOK, out)
		return
	}
	counts, err := st.CategoryCounts()
	if err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	for _, cc := range counts {
		if !known[cc.Name] {
			out = append(out, gin.H{"name": cc.Name, "torrents": cc.Count})
		}
	}
	c.JSON(http.StatusOK, out)
}

func (s *Server) handleCategoryCreate(c *gin.Context) {
	var cat category
	if err := c.ShouldBindJSON(&cat); err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
		return
	}
	if cat.Name == "" {
		c.JSON(http.StatusBadRequest, gin.H{"error": "name is required"})
		return
	}
	if cat.Mode == "" {
		cat.Mode = "hoard"
	}

	cats := loadCategories(s.config.Daemon.DataDir)
	for _, existing := range cats {
		if existing.Name == cat.Name {
			c.JSON(http.StatusConflict, gin.H{"error": "category already exists"})
			return
		}
	}

	cats = append(cats, cat)
	if err := saveCategories(s.config.Daemon.DataDir, cats); err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	c.JSON(http.StatusCreated, cat)
}

func (s *Server) handleCategoryUpdate(c *gin.Context) {
	name := c.Param("name")
	var update category
	if err := c.ShouldBindJSON(&update); err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
		return
	}

	cats := loadCategories(s.config.Daemon.DataDir)
	found := false
	for i, cat := range cats {
		if cat.Name == name {
			if update.SavePath != "" {
				cats[i].SavePath = update.SavePath
			}
			if update.Mode != "" {
				cats[i].Mode = update.Mode
			}
			if update.Agents != nil {
				cats[i].Agents = update.Agents
			}
			if update.Placement != nil {
				cats[i].Placement = update.Placement
			}
			if update.Strategy != "" {
				cats[i].Strategy = update.Strategy
			}
			cats[i].GraduateTo = update.GraduateTo
			found = true
			break
		}
	}

	if !found {
		c.JSON(http.StatusNotFound, gin.H{"error": "category not found"})
		return
	}

	if err := saveCategories(s.config.Daemon.DataDir, cats); err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	c.JSON(http.StatusOK, gin.H{"status": "ok"})
}

func (s *Server) handleCategoryDelete(c *gin.Context) {
	name := c.Param("name")
	cats := loadCategories(s.config.Daemon.DataDir)
	newCats := make([]category, 0, len(cats))
	found := false
	for _, cat := range cats {
		if cat.Name == name {
			found = true
			continue
		}
		newCats = append(newCats, cat)
	}

	// A category deleted before labels were cleared durably left torrents
	// wearing a name that is no longer in the list. Refusing here would strand
	// those labels forever: this route is the only thing that clears one. So a
	// missing entry is not the error — a name nothing carries at all is.
	if found {
		if err := saveCategories(s.config.Daemon.DataDir, newCats); err != nil {
			c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
			return
		}
	}
	// Deleting a category must not leave torrents pointing at a dangling label:
	// clear it from every hoard torrent that carried it (no file move), like
	// qBittorrent (the torrents become uncategorized).
	cleared := 0
	if s.hoardEngine != nil {
		cleared = s.hoardEngine.ClearCategoryLabel(name)
	}
	if s.raceEngine != nil {
		cleared += s.raceEngine.ClearCategoryLabel(name)
	}
	// The engines hold the label in memory only. Clearing it there makes the
	// category disappear from the UI right away, but the store still carries it,
	// so the next boot reloads every one of those torrents with the deleted
	// category and the chips come back (issue #7).
	var clearedStored int64
	if st := durable(); st != nil {
		n, err := st.ClearCategory(name)
		if err != nil {
			slog.Warn("category deleted but its label survives in the store",
				"category", name, "error", err)
		}
		clearedStored = n
	}
	if !found && cleared == 0 && clearedStored == 0 {
		c.JSON(http.StatusNotFound, gin.H{"error": "category not found"})
		return
	}
	c.JSON(http.StatusOK, gin.H{"status": "ok", "cleared": cleared,
		"cleared_stored": clearedStored, "was_orphan": !found})
}

// ===========================================================================
// Handlers — Filesystem
// ===========================================================================

func (s *Server) handleFSBrowse(c *gin.Context) {
	browsePath := c.DefaultQuery("path", "/")

	entries, err := os.ReadDir(browsePath)
	if err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
		return
	}

	type dirEntry struct {
		Name  string `json:"name"`
		Path  string `json:"path"`
		IsDir bool   `json:"is_dir"`
		Size  int64  `json:"size,omitempty"`
	}

	dirs := make([]string, 0, len(entries))
	for _, e := range entries {
		if e.IsDir() {
			dirs = append(dirs, e.Name())
		}
	}
	sort.Strings(dirs)

	c.JSON(http.StatusOK, gin.H{
		"path": browsePath,
		"dirs": dirs,
	})
}

// ===========================================================================
// Handlers — Drain
// ===========================================================================

func (s *Server) handleGraduations(c *gin.Context) {
	if s.gradReporter == nil {
		c.JSON(http.StatusOK, []interface{}{})
		return
	}
	c.JSON(http.StatusOK, s.gradReporter.GraduationsSnapshot())
}

func (s *Server) handleDrainStatus(c *gin.Context) {
	if s.raceDrain == nil {
		// TODO: implement race drain
		c.JSON(http.StatusOK, gin.H{"enabled": false})
		return
	}
	c.JSON(http.StatusOK, s.raceDrain.GetStatus())
}

func (s *Server) handleDrainHistory(c *gin.Context) {
	if s.raceDrain == nil {
		c.JSON(http.StatusOK, []interface{}{})
		return
	}
	c.JSON(http.StatusOK, s.raceDrain.GetHistory())
}

func (s *Server) handleDrainNow(c *gin.Context) {
	if s.raceDrain == nil {
		c.JSON(http.StatusServiceUnavailable, gin.H{"error": "race drain not available"})
		return
	}
	c.JSON(http.StatusOK, s.raceDrain.DrainNow())
}

// ===========================================================================
// Handlers — Arr Cleanup
// ===========================================================================

func (s *Server) handleArrCleanupScan(c *gin.Context) {
	if s.arrCleanup == nil {
		// TODO: implement arr cleanup
		c.JSON(http.StatusOK, gin.H{"enabled": false, "results": []interface{}{}})
		return
	}
	c.JSON(http.StatusOK, s.arrCleanup.Scan())
}

func (s *Server) handleArrCleanupExecute(c *gin.Context) {
	if s.arrCleanup == nil {
		c.JSON(http.StatusServiceUnavailable, gin.H{"error": "arr cleanup not available"})
		return
	}
	var params map[string]interface{}
	if err := c.ShouldBindJSON(&params); err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
		return
	}
	c.JSON(http.StatusOK, s.arrCleanup.Execute(params))
}

// ===========================================================================
// Handlers — Benchmark
// ===========================================================================

func (s *Server) handleBenchRecords(c *gin.Context) {
	if s.benchDB == nil {
		c.JSON(http.StatusOK, gin.H{})
		return
	}
	c.JSON(http.StatusOK, s.benchDB.GetRecords())
}

func (s *Server) handleBenchCurrent(c *gin.Context) {
	if s.benchDB == nil {
		// TODO: implement bench db
		c.JSON(http.StatusOK, gin.H{"enabled": false})
		return
	}
	c.JSON(http.StatusOK, s.benchDB.GetCurrent())
}

func (s *Server) handleBenchRange(c *gin.Context) {
	if s.benchDB == nil {
		c.JSON(http.StatusOK, []interface{}{})
		return
	}
	startF, _ := strconv.ParseFloat(c.Query("start"), 64)
	endF, _ := strconv.ParseFloat(c.Query("end"), 64)
	step, _ := strconv.Atoi(c.Query("step"))
	result := s.benchDB.GetRange(int(startF), int(endF), step)
	if result == nil {
		c.JSON(http.StatusOK, []interface{}{})
		return
	}
	c.JSON(http.StatusOK, result)
}

func (s *Server) handleBenchCompare(c *gin.Context) {
	if s.benchDB == nil {
		c.JSON(http.StatusOK, gin.H{})
		return
	}
	startF, _ := strconv.ParseFloat(c.Query("start"), 64)
	midF, _ := strconv.ParseFloat(c.Query("mid"), 64)
	endF, _ := strconv.ParseFloat(c.Query("end"), 64)
	c.JSON(http.StatusOK, s.benchDB.GetComparison(int(startF), int(midF), int(endF)))
}

func (s *Server) handleTrackerStatsCurrent(c *gin.Context) {
	if s.benchDB == nil {
		c.JSON(http.StatusOK, []interface{}{})
		return
	}
	res := s.benchDB.GetTrackerCurrent()
	if res == nil {
		c.JSON(http.StatusOK, []interface{}{})
		return
	}
	c.JSON(http.StatusOK, res)
}

func (s *Server) handleTrackerStatsRange(c *gin.Context) {
	if s.benchDB == nil {
		c.JSON(http.StatusOK, []interface{}{})
		return
	}
	startF, _ := strconv.ParseFloat(c.Query("start"), 64)
	endF, _ := strconv.ParseFloat(c.Query("end"), 64)
	step, _ := strconv.Atoi(c.Query("step"))
	tracker := c.Query("tracker")
	res := s.benchDB.GetTrackerRange(int(startF), int(endF), step, tracker)
	if res == nil {
		c.JSON(http.StatusOK, []interface{}{})
		return
	}
	c.JSON(http.StatusOK, res)
}

func (s *Server) handleRaceEvents(c *gin.Context) {
	if s.benchDB == nil {
		c.JSON(http.StatusOK, []bench.RaceEvent{})
		return
	}
	ih := c.Query("info_hash")
	if ih != "" {
		c.JSON(http.StatusOK, s.benchDB.GetRaceEventsForTorrent(ih))
		return
	}
	startF, _ := strconv.ParseFloat(c.Query("start"), 64)
	endF, _ := strconv.ParseFloat(c.Query("end"), 64)
	if endF == 0 {
		endF = float64(time.Now().Unix())
	}
	if startF == 0 {
		startF = endF - 86400 // Default: last 24h
	}
	c.JSON(http.StatusOK, s.benchDB.GetRaceEvents(startF, endF))
}

func (s *Server) handleRaceSnapshots(c *gin.Context) {
	if s.benchDB == nil {
		c.JSON(http.StatusOK, []bench.RaceSnapshot{})
		return
	}
	infoHash := c.Param("info_hash")
	c.JSON(http.StatusOK, s.benchDB.GetRaceSnapshots(infoHash))
}

// ===========================================================================
// Handlers — Race Timeline
// ===========================================================================

func (s *Server) handleRaceTimeline(c *gin.Context) {
	infoHash := strings.ToLower(c.Param("info_hash"))
	if infoHash == "" {
		c.JSON(http.StatusBadRequest, gin.H{"error": "info_hash required"})
		return
	}

	var events []bench.RaceEvent
	var snapshots []bench.RaceSnapshot
	if s.benchDB != nil {
		events = s.benchDB.GetRaceEventsForTorrent(infoHash)
		snapshots = s.benchDB.GetRaceSnapshots(infoHash)
	}
	if events == nil {
		events = []bench.RaceEvent{}
	}
	if snapshots == nil {
		snapshots = []bench.RaceSnapshot{}
	}

	// Get current torrent state for auto-refresh decision
	var state string
	if s.raceEngine != nil {
		detail := s.raceEngine.GetTorrentDetail(infoHash)
		if detail != nil {
			if st, ok := detail["state"].(string); ok {
				state = st
			}
		}
	}

	c.JSON(http.StatusOK, gin.H{
		"info_hash": infoHash,
		"state":     state,
		"events":    events,
		"snapshots": snapshots,
	})
}

// ===========================================================================
// Handlers — VPN Speedtest
// ===========================================================================

func (s *Server) handleVPNSpeedtestLatest(c *gin.Context) {
	if s.benchDB == nil {
		c.JSON(http.StatusOK, gin.H{"enabled": s.config.VpnSpeedtest.Enabled})
		return
	}
	result := s.benchDB.GetVpnLatest()
	if result == nil {
		c.JSON(http.StatusOK, gin.H{"enabled": s.config.VpnSpeedtest.Enabled})
		return
	}
	result["enabled"] = s.config.VpnSpeedtest.Enabled
	c.JSON(http.StatusOK, result)
}

func (s *Server) handleVPNSpeedtestHistory(c *gin.Context) {
	if s.benchDB == nil {
		c.JSON(http.StatusOK, []interface{}{})
		return
	}
	hoursStr := c.DefaultQuery("hours", "24")
	hours, _ := strconv.ParseFloat(hoursStr, 64)
	if hours <= 0 {
		hours = 24
	}
	now := float64(time.Now().Unix())
	start := now - hours*3600
	result := s.benchDB.GetVpnRange(start, now)
	if result == nil {
		c.JSON(http.StatusOK, []interface{}{})
		return
	}
	c.JSON(http.StatusOK, result)
}

func (s *Server) handleVPNSpeedtestRun(c *gin.Context) {
	cfg := s.config.VpnSpeedtest
	if !cfg.Enabled || cfg.Iperf3Server == "" {
		c.JSON(http.StatusBadRequest, gin.H{"error": "iperf3_server not configured in [vpn_speedtest]"})
		return
	}
	result, err := runIperf3(cfg.Iperf3Server, cfg.Iperf3Port, cfg.DurationSecs)
	if err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	if s.benchDB != nil {
		s.benchDB.InsertVpn(result["ts"].(float64), result["ul_mbps"].(float64), result["dl_mbps"].(float64),
			result["ul_torrent_mbps"].(float64), result["dl_torrent_mbps"].(float64))
	}
	c.JSON(http.StatusOK, result)
}

// ===========================================================================
// Handlers — Baseline
// ===========================================================================

func (s *Server) handleBaselineGet(c *gin.Context) {
	sessionUL, sessionDL := getSessionDelta()
	globalUL, globalDL := getGlobalTotals()

	c.JSON(http.StatusOK, gin.H{
		"baseline_uploaded":   atomic.LoadInt64(&baselineUploaded),
		"baseline_downloaded": atomic.LoadInt64(&baselineDownloaded),
		"session_uploaded":    sessionUL,
		"session_downloaded":  sessionDL,
		"global_uploaded":     globalUL,
		"global_downloaded":   globalDL,
	})
}

func (s *Server) handleBaselinePost(c *gin.Context) {
	var req struct {
		TotalUploaded   int64 `json:"total_uploaded"`
		TotalDownloaded int64 `json:"total_downloaded"`
	}
	if err := c.ShouldBindJSON(&req); err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
		return
	}

	atomic.StoreInt64(&baselineUploaded, req.TotalUploaded)
	atomic.StoreInt64(&baselineDownloaded, req.TotalDownloaded)
	saveBaseline()

	c.JSON(http.StatusOK, gin.H{
		"status":           "ok",
		"total_uploaded":   req.TotalUploaded,
		"total_downloaded": req.TotalDownloaded,
	})
}

// ===========================================================================
// Handlers — Config
// ===========================================================================

// ===========================================================================
// Handlers — Port Forward
// ===========================================================================

func (s *Server) handlePortForwardStatus(c *gin.Context) {
	// The effective port, not the boot-time config: after a hot rebind the
	// two diverge, and reporting the config value here meant the UI showed a
	// dead port and the socket health check below probed the wrong one.
	racePort := s.config.Race.ListenPort
	if s.raceEngine != nil {
		if p := s.raceEngine.ListenPort(); p > 0 {
			racePort = p
		}
	}
	hoardPort := s.config.Hoard.ListenPort
	if s.hoardEngine != nil {
		if p := s.hoardEngine.ListenPort(); p > 0 {
			hoardPort = p
		}
	}

	var racePeers, hoardPeers int
	if s.raceEngine != nil {
		for _, t := range s.raceEngine.GetAllStatus() {
			if v, ok := t["num_peers"].(int); ok {
				racePeers += v
			} else if v, ok := t["num_peers"].(int64); ok {
				racePeers += int(v)
			}
		}
	}
	if s.hoardEngine != nil {
		for _, t := range s.hoardEngine.GetTorrentList() {
			if v, ok := t["num_peers"].(int); ok {
				hoardPeers += v
			} else if v, ok := t["num_peers"].(int64); ok {
				hoardPeers += int(v)
			}
		}
	}

	// Check listen socket health
	raceSockets := checkListenSockets(racePort)
	hoardSockets := checkListenSockets(hoardPort)

	raceHealthy := true
	for _, ls := range raceSockets {
		if ls.Stale {
			raceHealthy = false
			break
		}
	}
	hoardHealthy := true
	for _, ls := range hoardSockets {
		if ls.Stale {
			hoardHealthy = false
			break
		}
	}

	c.JSON(http.StatusOK, gin.H{
		"race_port":         racePort,
		"race_peers":        racePeers,
		"race_connectable":  racePeers > 0,
		"hoard_port":        hoardPort,
		"hoard_peers":       hoardPeers,
		"hoard_connectable": hoardPeers > 0,
		"all_connectable":   racePeers > 0 && hoardPeers > 0,
		"listen_healthy":    raceHealthy && hoardHealthy,
		"race_sockets":      raceSockets,
		"hoard_sockets":     hoardSockets,
		"public_ip":         getPublicIP(),
	})
}

// listenSocketStatus describes a single listen socket's health.
type listenSocketStatus struct {
	IP             string `json:"ip"`
	Port           int    `json:"port"`
	BoundInterface string `json:"bound_interface"`
	Stale          bool   `json:"stale"`
}

// checkListenSockets parses ss output to detect stale interface bindings.
// Uses net.Interfaces() which respects the container's network namespace,
// unlike /sys/class/net which shows host interfaces.
func checkListenSockets(port int) []listenSocketStatus {
	out, err := exec.Command("ss", "-tlnH", "sport", "=", fmt.Sprintf(":%d", port)).Output()
	if err != nil {
		return nil
	}

	// Build map of current interface indices and names using net.Interfaces()
	// This correctly reflects the network namespace (styx netns).
	currentIndices := map[string]bool{} // "1584" → true
	currentNames := map[string]bool{}   // "fou0" → true
	if ifaces, err := net.Interfaces(); err == nil {
		for _, iface := range ifaces {
			currentIndices[strconv.Itoa(iface.Index)] = true
			currentNames[iface.Name] = true
		}
	}

	var results []listenSocketStatus
	scanner := bufio.NewScanner(strings.NewReader(string(out)))
	for scanner.Scan() {
		line := scanner.Text()
		fields := strings.Fields(line)
		if len(fields) < 4 {
			continue
		}
		localAddr := fields[3]

		// "10.0.0.2%if1474:16172" → ip, boundIface, port
		colonIdx := strings.LastIndex(localAddr, ":")
		if colonIdx < 0 {
			continue
		}
		ipPart := localAddr[:colonIdx]

		var ip, boundIface string
		if pctIdx := strings.Index(ipPart, "%"); pctIdx >= 0 {
			ip = ipPart[:pctIdx]
			boundIface = ipPart[pctIdx+1:]
		} else {
			ip = ipPart
		}

		stale := false
		if strings.HasPrefix(boundIface, "if") {
			// Numeric index (e.g. "if1584") — check index exists in netns
			idx := strings.TrimPrefix(boundIface, "if")
			if !currentIndices[idx] {
				stale = true
			}
		} else if boundIface != "" {
			// Named interface (e.g. "fou1") — check name exists in netns
			if !currentNames[boundIface] {
				stale = true
			}
		}

		results = append(results, listenSocketStatus{
			IP:             ip,
			Port:           port,
			BoundInterface: boundIface,
			Stale:          stale,
		})
	}
	return results
}

var cachedPublicIP string
var cachedPublicIPTime time.Time

func getPublicIP() string {
	if cachedPublicIP != "" && time.Since(cachedPublicIPTime) < 90*time.Second {
		return cachedPublicIP
	}
	ip := fetchPublicIP("https://api.ipify.org/")
	if ip != "" {
		cachedPublicIP = ip
		cachedPublicIPTime = time.Now()
	}
	return cachedPublicIP
}

var cachedPublicIPv6 string
var cachedPublicIPv6Time time.Time

// getPublicIPv6 hits an AAAA-only endpoint so the SOCKS5 proxy is forced
// to dial in v6 — returns our v6 egress (gost VPS in the current stack).
// Cached 5 min like the v4 variant.
func getPublicIPv6() string {
	if cachedPublicIPv6 != "" && time.Since(cachedPublicIPv6Time) < 90*time.Second {
		return cachedPublicIPv6
	}
	ip := fetchPublicIP("https://api6.ipify.org/")
	if ip != "" {
		cachedPublicIPv6 = ip
		cachedPublicIPv6Time = time.Now()
	}
	return cachedPublicIPv6
}

// PublicIPs returns our current public IPv4 (non-empty) for pushing to the
// engine self-dial filter. v4 only: inbound is v4 and the v6 lookup can stall.
func PublicIPs() []string {
	var out []string
	if v4 := getPublicIP(); v4 != "" {
		out = append(out, v4)
	}
	return out
}

// fetchPublicIP is the shared HTTP-via-SOCKS5 dance used by both v4 and v6
// public-ip lookups. Returns "" on any failure (caller keeps its cached
// value rather than blanking the UI).
func fetchPublicIP(url string) string {
	transport := &http.Transport{}
	if d := getSocks5Proxy(); d != nil {
		transport.DialContext = d.DialContext
	}
	client := &http.Client{Timeout: 10 * time.Second, Transport: transport}
	resp, err := client.Get(url)
	if err != nil {
		return ""
	}
	defer resp.Body.Close()
	body := make([]byte, 64)
	n, _ := resp.Body.Read(body)
	return strings.TrimSpace(string(body[:n]))
}

// ===========================================================================
// Handlers — System
// ===========================================================================

func (s *Server) handlePublicIP(c *gin.Context) {
	if c.Query("refresh") == "1" {
		// Force a fresh lookup (e.g. user just switched Proton server): expire
		// both caches so getPublicIP{,v6} re-hit the echo service right now.
		cachedPublicIPTime = time.Time{}
		cachedPublicIPv6Time = time.Time{}
	}
	c.JSON(http.StatusOK, gin.H{
		"ip":    getPublicIP(),
		"ip_v6": getPublicIPv6(),
	})
}

// AbsorbStats adds UL/DL to the baseline from any removal path (engine
// callback). Engine = "hoard" | "race" — utilisé pour compenser
// sessionOffset[Hoard|Race][UL|DL] et que session_uploaded/day_uploaded ne
// saignent pas au retrait du torrent (sa contribution lifetime sort de
// SUM(t.total_uploaded) côté Typhon).
func AbsorbStats(engine string, ul, dl int64) {
	absorbGlobalMem(engine, ul, dl)
	if ul > 0 || dl > 0 {
		saveBaseline()
	}
}

// absorbGlobalMem is the in-memory half of AbsorbStats, split out so the
// atomic removal path (AbsorbOnRemove) can update what readers see and then
// persist in one transaction instead of two.
func absorbGlobalMem(engine string, ul, dl int64) {
	if ul > 0 {
		atomic.AddInt64(&baselineUploaded, ul)
	}
	if dl > 0 {
		atomic.AddInt64(&baselineDownloaded, dl)
	}
	switch engine {
	case "hoard":
		atomic.AddInt64(&sessionOffsetHoardUL, -ul)
		atomic.AddInt64(&sessionOffsetHoardDL, -dl)
	case "race":
		atomic.AddInt64(&sessionOffsetRaceUL, -ul)
		atomic.AddInt64(&sessionOffsetRaceDL, -dl)
	}
}

// fanoutAnnounceOverride pushes an announce override to every connected remote
// agent so a global setting (client spoof / passkey) stays consistent across
// the fleet. Best-effort, returns (pushed, failed): an unreachable agent counts
// as failed, not fatal (it re-seeds from its own toml on restart, and re-saving
// re-pushes). Read stays local -- the front is the source of truth.
func (s *Server) fanoutAnnounceOverride(p agentwire.AnnounceOverrideParams) (int, int) {
	pushed, failed := 0, 0
	for _, ra := range s.agentsSnapshot() {
		cl := ra.anyClient()
		if cl == nil {
			continue
		}
		if err := cl.SetAnnounceOverride(p); err != nil {
			failed++
		} else {
			pushed++
		}
	}
	return pushed, failed
}

// handleHoardSetTags sets/adds/removes a hoard torrent's tags (qBittorrent-style
// labels). Body: {"tags":[...], "op":"set"|"add"|"remove"} (default set). Tags
// take effect immediately (cachedStats) and are persisted to the tags.json
// overlay so they survive restart.
func (s *Server) handleHoardSetTags(c *gin.Context) {
	hash := c.Param("info_hash")
	var body struct {
		Tags []string `json:"tags"`
		Op   string   `json:"op"`
	}
	if err := c.ShouldBindJSON(&body); err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
		return
	}
	if s.hoardEngine == nil || !s.hoardEngine.HasTorrent(hash) {
		c.JSON(http.StatusNotFound, gin.H{"error": "torrent not found"})
		return
	}
	var err error
	switch body.Op {
	case "add":
		err = s.hoardEngine.AddTags(hash, body.Tags)
	case "remove":
		err = s.hoardEngine.RemoveTags(hash, body.Tags)
	default:
		err = s.hoardEngine.SetTags(hash, body.Tags)
	}
	if err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	s.persistTagsFor(hash)
	c.JSON(http.StatusOK, gin.H{"status": "ok", "info_hash": hash, "tags": s.hoardEngine.GetTags(hash)})
}

// persistTagsFor writes the current tag assignments of the named torrents. With
// a store that is one transaction touching only the rows that changed; a
// front-only node has no database and falls back to rewriting the whole overlay.
func (s *Server) persistTagsFor(hashes ...string) {
	if s.hoardEngine == nil || len(hashes) == 0 {
		return
	}
	st := durable()
	if st == nil {
		_ = tagstore.Save(s.config.Daemon.DataDir, s.hoardEngine.GetAllTags())
		return
	}
	m := make(map[string][]string, len(hashes))
	for _, h := range hashes {
		m[h] = s.hoardEngine.GetTags(h) // empty list clears the row
	}
	if _, err := st.SetTagsBulk(m); err != nil {
		slog.Error("tags: persist failed", "count", len(m), "err", err)
	}
}

// persistTagsAll reconciles every stored tag assignment with the engine. Used on
// the paths that cannot name what changed — deleting a tag across the whole
// library, or adding torrents whose hashes the caller did not collect.
func (s *Server) persistTagsAll() {
	if s.hoardEngine == nil {
		return
	}
	st := durable()
	if st == nil {
		_ = tagstore.Save(s.config.Daemon.DataDir, s.hoardEngine.GetAllTags())
		return
	}
	if _, err := st.ReplaceAllTags(s.hoardEngine.GetAllTags()); err != nil {
		slog.Error("tags: full persist failed", "err", err)
	}
}

// handleGetTags returns the sorted set of distinct tags currently in use.
func (s *Server) handleGetTags(c *gin.Context) {
	set := map[string]bool{}
	if s.hoardEngine != nil {
		for _, tags := range s.hoardEngine.GetAllTags() {
			for _, t := range tags {
				set[t] = true
			}
		}
	}
	out := make([]string, 0, len(set))
	for t := range set {
		out = append(out, t)
	}
	sort.Strings(out)
	c.JSON(http.StatusOK, out)
}

// handleGetPasskeys returns the current per-tracker announce passkey overrides.
func (s *Server) handleGetPasskeys(c *gin.Context) {
	c.JSON(http.StatusOK, engine.GetPasskeyOverrides())
}

// handleSetPasskey sets (passkey="" clears) the announce passkey for trackers
// whose URL contains host. Hot — applies on the next announce, no restart.
// setListenPort hot-swaps an engine peer listen port at runtime (no restart).
func (s *Server) setListenPort(c *gin.Context, role string, setter interface{ SetListenPort(int) error }) {
	var req struct {
		Port int `json:"port"`
	}
	if err := c.ShouldBindJSON(&req); err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
		return
	}
	if req.Port <= 0 || req.Port > 65535 {
		c.JSON(http.StatusBadRequest, gin.H{"error": "port out of range (1-65535)"})
		return
	}
	if setter == nil {
		c.JSON(http.StatusServiceUnavailable, gin.H{"error": "engine unavailable"})
		return
	}
	// A failed rebind used to still answer 200 ok, so a caller had no way to
	// tell a live port from a wish.
	if err := setter.SetListenPort(req.Port); err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	// Persisted after the rebind succeeded — never record a port we failed to
	// bind. A write failure leaves the port live but forgotten at restart, so
	// it is reported rather than swallowed.
	persisted := true
	if err := s.persistListenPort(role, req.Port); err != nil {
		slog.Warn("listen port: rebound but not persisted", "role", role, "port", req.Port, "err", err)
		persisted = false
	}
	c.JSON(http.StatusOK, gin.H{"status": "ok", "port": req.Port, "persisted": persisted})
}

func (s *Server) handleHoardSetListenPort(c *gin.Context) {
	s.setListenPort(c, "hoard", s.hoardEngine)
}

func (s *Server) handleRaceSetListenPort(c *gin.Context) {
	s.setListenPort(c, "race", s.raceEngine)
}

func (s *Server) handleSetPasskey(c *gin.Context) {
	var req struct {
		Host    string `json:"host"`
		Passkey string `json:"passkey"`
	}
	if err := c.ShouldBindJSON(&req); err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
		return
	}
	engine.SetPasskeyOverride(req.Host, req.Passkey)
	pushed, failed := s.fanoutAnnounceOverride(agentwire.AnnounceOverrideParams{Kind: "passkey", Host: req.Host, Passkey: req.Passkey})
	persisted := persistedFlag(s.persistPasskey(req.Host, req.Passkey), "passkey", req.Host)
	c.JSON(http.StatusOK, gin.H{"status": "ok", "passkeys": engine.GetPasskeyOverrides(), "agents_pushed": pushed, "agents_failed": failed, "persisted": persisted})
}

// handleGetClients returns the current per-tracker client spoof overrides.
func (s *Server) handleGetClients(c *gin.Context) {
	c.JSON(http.StatusOK, engine.GetClientOverrides())
}

// handleSetClient sets (peer_id_prefix="" clears) the spoofed client identity
// for trackers whose URL contains host. Hot — applies on the next announce.
func (s *Server) handleSetClient(c *gin.Context) {
	var req struct {
		Host         string `json:"host"`
		PeerIDPrefix string `json:"peer_id_prefix"`
		UserAgent    string `json:"user_agent"`
	}
	if err := c.ShouldBindJSON(&req); err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
		return
	}
	engine.SetClientOverride(req.Host, req.PeerIDPrefix, req.UserAgent)
	pushed, failed := s.fanoutAnnounceOverride(agentwire.AnnounceOverrideParams{Kind: "client", Host: req.Host, PeerIDPrefix: req.PeerIDPrefix, UserAgent: req.UserAgent})
	persisted := persistedFlag(s.persistClientSpoof(req.Host, req.PeerIDPrefix, req.UserAgent), "client", req.Host)
	c.JSON(http.StatusOK, gin.H{"status": "ok", "clients": engine.GetClientOverrides(), "agents_pushed": pushed, "agents_failed": failed, "persisted": persisted})
}

// handleGetOptFlags reports the state of the hot-swappable IPC optimisation
// flags (see internal/engine/ltclient/opt.go).
func (s *Server) handleGetOptFlags(c *gin.Context) {
	flags := map[string]bool{}
	for k, v := range ltclient.OptFlags() {
		flags[k] = v
	}
	for k, v := range OptFlags() {
		flags[k] = v
	}
	// The engines are separate processes with their own flags, so they are
	// reported per engine rather than folded into the Go-side map.
	engineFlags := gin.H{}
	if s.raceEngine != nil {
		if f, err := s.raceEngine.EngineOptFlags(); err == nil {
			engineFlags["race"] = f
		}
	}
	if s.hoardEngine != nil {
		if f, err := s.hoardEngine.EngineOptFlags(); err == nil {
			engineFlags["hoard"] = f
		}
	}
	c.JSON(http.StatusOK, gin.H{
		"flags":             flags,
		"engine_flags":      engineFlags,
		"gogc":              GOGC(),
		"list_cache_ttl_ms": ltclient.ListCacheTTL() / 1e6,
	})
}

// handleSetOptFlag toggles one optimisation at runtime, so a profiling ladder
// can measure each one in isolation without a restart (a restart resets the
// per-torrent upload counters, which costs real tracker credit).
func (s *Server) handleSetOptFlag(c *gin.Context) {
	var req struct {
		Flag  string `json:"flag"`
		On    bool   `json:"on"`
		Value int64  `json:"value"`
	}
	if err := c.ShouldBindJSON(&req); err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
		return
	}
	// Two knobs carry a value rather than a boolean.
	switch req.Flag {
	case "gogc":
		if req.Value < 10 {
			c.JSON(http.StatusBadRequest, gin.H{"error": "gogc must be >= 10"})
			return
		}
		prev := SetGOGC(int(req.Value))
		slog.Info("GOGC set", "percent", req.Value, "previous", prev)
		c.JSON(http.StatusOK, gin.H{"status": "ok", "gogc": GOGC(), "previous": prev})
		return
	case "list_cache_ttl_ms":
		if !ltclient.SetListCacheTTL(req.Value * 1e6) {
			c.JSON(http.StatusBadRequest, gin.H{"error": "ttl must be >= 0"})
			return
		}
		slog.Info("list cache TTL set", "ms", req.Value)
		c.JSON(http.StatusOK, gin.H{"status": "ok", "list_cache_ttl_ms": ltclient.ListCacheTTL() / 1e6})
		return
	}
	// Engine-side flags live in the Rust process, so they are forwarded to
	// both engines. An engine that refuses (an unknown name, or a pool size
	// change after the pool is built) is an error, not a silent no-op: a
	// measurement that quietly did not take is worse than one that failed.
	if isEngineOptFlag(req.Flag) {
		applied := gin.H{}
		for name, eng := range s.engineTargets() {
			f, err := eng.SetEngineOptFlag(req.Flag, req.On, req.Value)
			if err != nil {
				c.JSON(http.StatusBadRequest, gin.H{"error": name + ": " + err.Error()})
				return
			}
			applied[name] = f
		}
		if len(applied) == 0 {
			c.JSON(http.StatusBadRequest, gin.H{"error": "no local engine to set " + req.Flag + " on"})
			return
		}
		slog.Info("engine opt flag set", "flag", req.Flag, "on", req.On, "value", req.Value)
		c.JSON(http.StatusOK, gin.H{"status": "ok", "engine_flags": applied})
		return
	}
	if !ltclient.SetOptFlag(req.Flag, req.On) && !SetOptFlag(req.Flag, req.On) {
		c.JSON(http.StatusBadRequest, gin.H{"error": "unknown flag: " + req.Flag})
		return
	}
	slog.Info("opt flag set", "flag", req.Flag, "on", req.On)
	c.JSON(http.StatusOK, gin.H{"status": "ok", "flags": ltclient.OptFlags(), "http_flags": OptFlags()})
}

// isEngineOptFlag reports whether a flag name belongs to the Rust engines
// rather than to either Go-side registry.
func isEngineOptFlag(name string) bool {
	switch name {
	case "session_pinning", "session_runtimes":
		return true
	}
	return false
}

// engineTargets returns the local engines an engine-side flag applies to.
func (s *Server) engineTargets() map[string]interface {
	SetEngineOptFlag(string, bool, int64) (map[string]interface{}, error)
} {
	out := map[string]interface {
		SetEngineOptFlag(string, bool, int64) (map[string]interface{}, error)
	}{}
	if s.raceEngine != nil {
		out["race"] = s.raceEngine
	}
	if s.hoardEngine != nil {
		out["hoard"] = s.hoardEngine
	}
	return out
}

// handleGetTrackers returns the per-host tracker aggregate joined with the
// client-spoof and passkey override state, backing the Trackers tab. It MERGES
// the local announce registry with every connected remote agent's, so a
// front-only node (no local engine) still sees and manages the whole fleet's
// trackers. Overrides are global (fanned out to all agents), hence one row per
// host; the override columns come from the local maps (the front's source of
// truth). sources lists which nodes announce to each host.
func (s *Server) handleGetTrackers(c *gin.Context) {
	merged := map[string]*engine.TrackerStat{}
	sources := map[string][]string{}
	fold := func(stats []engine.TrackerStat, node string) {
		for i := range stats {
			st := stats[i]
			if m := merged[st.Host]; m != nil {
				m.Torrents += st.Torrents
				m.Announces += st.Announces
				m.Errors += st.Errors
				if st.OK {
					m.OK = true
				}
				if m.LastError == "" && st.LastError != "" {
					m.LastError = st.LastError
				}
				if st.LastAnnounce.After(m.LastAnnounce) {
					m.LastAnnounce = st.LastAnnounce
				}
				sources[st.Host] = append(sources[st.Host], node)
				continue
			}
			cp := st
			merged[st.Host] = &cp
			sources[st.Host] = []string{node}
		}
	}
	fold(engine.TrackerSnapshot(), "local")
	for _, ra := range s.agentsSnapshot() {
		cl := ra.anyClient()
		if cl == nil {
			continue
		}
		ws, err := cl.TrackerSnapshot()
		if err != nil {
			continue
		}
		stats := make([]engine.TrackerStat, len(ws))
		for i, w := range ws {
			stats[i] = engine.TrackerStat{Host: w.Host, Torrents: w.Torrents, OK: w.OK, LastError: w.LastError, LastAnnounce: w.LastAnnounce, Announces: w.Announces, Errors: w.Errors}
		}
		fold(stats, ra.name)
	}
	type trackerRow struct {
		engine.TrackerStat
		Spoofed      bool     `json:"spoofed"`
		PeerIDPrefix string   `json:"peer_id_prefix,omitempty"`
		UserAgent    string   `json:"user_agent,omitempty"`
		PasskeySet   bool     `json:"passkey_set"`
		Sources      []string `json:"sources"`
	}
	rows := make([]trackerRow, 0, len(merged))
	for host, m := range merged {
		row := trackerRow{TrackerStat: *m, Sources: sources[host]}
		if sp, ok := engine.ClientSpoofForHost(host); ok {
			row.Spoofed = true
			row.PeerIDPrefix = sp.PeerIDPrefix
			row.UserAgent = sp.UserAgent
		}
		row.PasskeySet = engine.PasskeyOverrideForHost(host)
		rows = append(rows, row)
	}
	sort.Slice(rows, func(i, j int) bool {
		if rows[i].Torrents != rows[j].Torrents {
			return rows[i].Torrents > rows[j].Torrents
		}
		return rows[i].Host < rows[j].Host
	})
	c.JSON(http.StatusOK, rows)
}

// handleGetSecondaryStats returns the current per-tracker secondary-announce
// stats modes (host -> "zero"|"off"; absent = "clone" default).
func (s *Server) handleGetSecondaryStats(c *gin.Context) {
	c.JSON(http.StatusOK, engine.GetSecondaryStatsOverrides())
}

// handleSetSecondaryStats sets the secondary-announce stats mode for trackers
// whose URL contains host. mode: "zero" (up=0/down=0), "off" (skip secondary),
// "clone"/"" (default, clears). Hot — applies on the next announce.
func (s *Server) handleSetSecondaryStats(c *gin.Context) {
	var req struct {
		Host string `json:"host"`
		Mode string `json:"mode"`
	}
	if err := c.ShouldBindJSON(&req); err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
		return
	}
	engine.SetSecondaryStatsOverride(req.Host, req.Mode)
	persisted := persistedFlag(s.persistSecondaryStats(req.Host, req.Mode), "secondary_stats", req.Host)
	c.JSON(http.StatusOK, gin.H{"status": "ok", "secondary_stats": engine.GetSecondaryStatsOverrides(), "persisted": persisted})
}

// ---------------------------------------------------------------------------
// Handlers — Settings (whole-config editor; writes default.toml in place)
// ---------------------------------------------------------------------------

// localNICs enumerates this host's non-loopback IPv4 interfaces.
func localNICs() []agentwire.NICInfo {
	out := []agentwire.NICInfo{}
	ifaces, err := net.Interfaces()
	if err != nil {
		return out
	}
	for _, ifc := range ifaces {
		if ifc.Flags&net.FlagLoopback != 0 {
			continue
		}
		up := ifc.Flags&net.FlagUp != 0
		addrs, _ := ifc.Addrs()
		for _, a := range addrs {
			if ipnet, ok := a.(*net.IPNet); ok {
				if ip4 := ipnet.IP.To4(); ip4 != nil {
					out = append(out, agentwire.NICInfo{Name: ifc.Name, IP: ip4.String(), Up: up})
				}
			}
		}
	}
	return out
}

// handleNetworkInterfaces lists the host's non-loopback IPv4 interfaces so the
// Configuration UI can offer them (name + ip) for engine binding.
func (s *Server) handleNetworkInterfaces(c *gin.Context) {
	ifaces, err := net.Interfaces()
	if err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	type nic struct {
		Name string `json:"name"`
		IP   string `json:"ip"`
		Up   bool   `json:"up"`
	}
	out := []nic{}
	for _, ifc := range ifaces {
		if ifc.Flags&net.FlagLoopback != 0 {
			continue
		}
		up := ifc.Flags&net.FlagUp != 0
		addrs, _ := ifc.Addrs()
		for _, a := range addrs {
			if ipnet, ok := a.(*net.IPNet); ok {
				if ip4 := ipnet.IP.To4(); ip4 != nil {
					out = append(out, nic{Name: ifc.Name, IP: ip4.String(), Up: up})
				}
			}
		}
	}
	c.JSON(http.StatusOK, gin.H{"interfaces": out})
}

// settingsFilePath returns the daemon TOML config path. Convention:
// <data_dir>/default.toml (matches the -config default and the container mount).
func (s *Server) settingsFilePath() string {
	if s.config.SourcePath != "" {
		return s.config.SourcePath
	}
	return filepath.Join(s.config.Daemon.DataDir, "default.toml")
}

// configWriteMu serializes the read-modify-write cycles on the daemon TOML, so
// two saves landing together cannot lose one another's edit.
var configWriteMu sync.Mutex

// editConfigFile applies edit to the daemon TOML and writes the result back
// atomically, refusing any document that would no longer parse or no longer
// decode into the typed config. Same guards, backup and tmp+rename as the
// settings editor: the tracker editor must not be able to corrupt a file the
// settings editor protects.
func (s *Server) editConfigFile(edit func(string) (string, error)) error {
	configWriteMu.Lock()
	defer configWriteMu.Unlock()
	path := s.settingsFilePath()
	data, err := os.ReadFile(path)
	if err != nil {
		return err
	}
	doc, err := edit(string(data))
	if err != nil {
		return err
	}
	if _, err := config.ParseTOMLMap([]byte(doc)); err != nil {
		return fmt.Errorf("edited config no longer parses: %w", err)
	}
	if err := config.ValidateTyped([]byte(doc)); err != nil {
		return fmt.Errorf("edit would break the config schema: %w", err)
	}
	_ = os.WriteFile(path+".bak-settings", data, 0644)
	tmp := path + ".tmp"
	if err := os.WriteFile(tmp, []byte(doc), 0644); err != nil {
		return err
	}
	return os.Rename(tmp, path)
}

// persistClientSpoof mirrors a hot client-spoof override into [announce_clients]
// so it survives a restart. Without this the override lives only in the engine's
// map and the next boot rebuilds that map from the config file alone — the
// override silently disappears (issue #3).
func (s *Server) persistClientSpoof(host, peerIDPrefix, userAgent string) error {
	section := "announce_clients." + config.QuoteTOMLKey(host)
	return s.editConfigFile(func(doc string) (string, error) {
		if strings.TrimSpace(peerIDPrefix) == "" {
			return config.DeleteTOMLTable(doc, section), nil
		}
		return config.SetTOMLTable(doc, section, [][2]string{
			{"peer_id_prefix", strconv.Quote(peerIDPrefix)},
			{"user_agent", strconv.Quote(userAgent)},
		})
	})
}

// persistPasskey mirrors a hot passkey override into [announce_passkeys].
func (s *Server) persistPasskey(host, passkey string) error {
	return s.editConfigFile(func(doc string) (string, error) {
		key := config.QuoteTOMLKey(host)
		if strings.TrimSpace(passkey) == "" {
			return config.PruneEmptyTable(config.DeleteTOMLKey(doc, "announce_passkeys", key), "announce_passkeys"), nil
		}
		return config.SetTOMLTable(doc, "announce_passkeys",
			[][2]string{{key, strconv.Quote(passkey)}})
	})
}

// persistSecondaryStats mirrors a hot secondary-stats mode into
// [announce_secondary_stats]. "clone" is the default, so it is stored as an
// absence rather than a value.
func (s *Server) persistSecondaryStats(host, mode string) error {
	return s.editConfigFile(func(doc string) (string, error) {
		key := config.QuoteTOMLKey(host)
		m := strings.TrimSpace(mode)
		if m == "" || m == "clone" {
			return config.PruneEmptyTable(config.DeleteTOMLKey(doc, "announce_secondary_stats", key), "announce_secondary_stats"), nil
		}
		return config.SetTOMLTable(doc, "announce_secondary_stats",
			[][2]string{{key, strconv.Quote(m)}})
	})
}

// persistedFlag runs a persistence attempt and reports it to the caller instead
// of failing the request: the hot change already applied, and a read-only config
// file should not make the override look rejected. The UI shows the difference.
func persistedFlag(err error, kind, host string) bool {
	if err != nil {
		slog.Warn("announce override applied but not persisted", "kind", kind, "host", host, "error", err)
		return false
	}
	return true
}

func (s *Server) handleSettingsGet(c *gin.Context) {
	data, err := os.ReadFile(s.settingsFilePath())
	if err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	m, err := config.ParseTOMLMap(data)
	if err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	c.JSON(http.StatusOK, m)
}

func tomlScalar(v interface{}) (string, error) {
	switch x := v.(type) {
	case bool:
		if x {
			return "true", nil
		}
		return "false", nil
	case string:
		return fmt.Sprintf("%q", x), nil
	case float64:
		if x == float64(int64(x)) {
			return strconv.FormatInt(int64(x), 10), nil
		}
		return strconv.FormatFloat(x, 'g', -1, 64), nil
	default:
		return "", fmt.Errorf("unsupported value type %T (scalars only)", v)
	}
}

func (s *Server) handleSettingsPost(c *gin.Context) {
	var req struct {
		Changes []struct {
			Section string      `json:"section"`
			Key     string      `json:"key"`
			Value   interface{} `json:"value"`
		} `json:"changes"`
	}
	if err := c.ShouldBindJSON(&req); err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
		return
	}
	if len(req.Changes) == 0 {
		c.JSON(http.StatusBadRequest, gin.H{"error": "no changes"})
		return
	}
	configWriteMu.Lock()
	defer configWriteMu.Unlock()
	path := s.settingsFilePath()
	data, err := os.ReadFile(path)
	if err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	doc := string(data)
	for _, ch := range req.Changes {
		tv, err := tomlScalar(ch.Value)
		if err != nil {
			c.JSON(http.StatusBadRequest, gin.H{"error": fmt.Sprintf("[%s] %s: %v", ch.Section, ch.Key, err)})
			return
		}
		doc, err = config.SetTOMLValue(doc, ch.Section, ch.Key, tv)
		if err != nil {
			c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
			return
		}
	}
	// Never commit a config that no longer parses.
	if _, err := config.ParseTOMLMap([]byte(doc)); err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": "edited config no longer parses: " + err.Error()})
		return
	}
	// Strict typed guard: a scalar written where an array/map is expected stays
	// valid generic TOML (ParseTOMLMap passes) but bricks the daemon at boot.
	// Reject before writing so a settings save can never corrupt the schema.
	if err := config.ValidateTyped([]byte(doc)); err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": "edit would break the config schema (wrong type for a field?): " + err.Error()})
		return
	}
	_ = os.WriteFile(path+".bak-settings", data, 0644)
	tmp := path + ".tmp"
	if err := os.WriteFile(tmp, []byte(doc), 0644); err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	if err := os.Rename(tmp, path); err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	// Engine/session params are not hot-reloadable -> restart to apply. We persist
	// now; the UI surfaces "restart required" + an "Apply & restart" button.
	c.JSON(http.StatusOK, gin.H{"status": "ok", "changed": len(req.Changes), "restart_required": true})
}

func (s *Server) handleSettingsRestart(c *gin.Context) {
	c.JSON(http.StatusOK, gin.H{"status": "restarting"})
	go func() {
		time.Sleep(300 * time.Millisecond)
		selfTerminate()
	}()
}

// ─── Update check (badge next to the version) ──────────────────────────────
var (
	updateCheckMu     sync.Mutex
	updateCheckAt     time.Time
	updateCheckLatest string
	updateCheckURL    string
)

// handleUpdateCheck reports whether a newer GitHub release exists. The lookup
// is server-side (avoids browser CORS / GitHub per-IP rate limits) and cached
// 6h. Opt out with [daemon] update_check_disabled = true. Best-effort: any
// network error just yields update_available=false.
func (s *Server) handleUpdateCheck(c *gin.Context) {
	if s.config.Daemon.UpdateCheckDisabled {
		c.JSON(http.StatusOK, gin.H{"enabled": false})
		return
	}
	latest, url := fetchLatestRelease()
	avail := latest != "" && verLess(normVer(Version), normVer(latest))
	c.JSON(http.StatusOK, gin.H{
		"enabled":          true,
		"current":          Version,
		"latest":           latest,
		"update_available": avail,
		"url":              url,
	})
}

func fetchLatestRelease() (string, string) {
	updateCheckMu.Lock()
	defer updateCheckMu.Unlock()
	if !updateCheckAt.IsZero() && time.Since(updateCheckAt) < 6*time.Hour {
		return updateCheckLatest, updateCheckURL
	}
	// Use the tags API (works from day one) rather than releases/latest, which
	// 404s until a GitHub *Release* is actually published. Pick the highest
	// semver vX.Y.Z tag.
	req, err := http.NewRequest("GET", "https://api.github.com/repos/Kheopsian/Hydra/tags?per_page=100", nil)
	if err != nil {
		return updateCheckLatest, updateCheckURL
	}
	req.Header.Set("User-Agent", "Hydra/"+Version)
	req.Header.Set("Accept", "application/vnd.github+json")
	resp, err := (&http.Client{Timeout: 5 * time.Second}).Do(req)
	if err != nil {
		return updateCheckLatest, updateCheckURL
	}
	defer resp.Body.Close()
	if resp.StatusCode != 200 {
		updateCheckAt = time.Now()
		return updateCheckLatest, updateCheckURL
	}
	var tags []struct {
		Name string `json:"name"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&tags); err != nil {
		return updateCheckLatest, updateCheckURL
	}
	updateCheckAt = time.Now()
	cur := normVer(Version)
	var cands []string
	for _, t := range tags {
		if strings.HasPrefix(t.Name, "v") && verLess(cur, normVer(t.Name)) {
			cands = append(cands, t.Name)
		}
	}
	sort.Slice(cands, func(i, j int) bool { return verLess(normVer(cands[i]), normVer(cands[j])) })
	// Highest first: pick the newest release that targets THIS platform. A
	// release with no "Platforms:" label affects all platforms (back-compat),
	// so users are never silently starved of a genuinely cross-cutting update.
	for i := len(cands) - 1; i >= 0; i-- {
		plats := fetchReleasePlatforms(cands[i])
		if plats == nil || plats[runtime.GOOS] {
			updateCheckLatest = cands[i]
			updateCheckURL = "https://github.com/Kheopsian/Hydra/releases/tag/" + cands[i]
			return updateCheckLatest, updateCheckURL
		}
	}
	updateCheckLatest = ""
	updateCheckURL = ""
	return updateCheckLatest, updateCheckURL
}

// fetchReleasePlatforms returns the set of platforms a release targets, parsed
// from a "Platforms: linux, windows" line in the release body. nil means the
// release has no label (or is unreachable) -> treat as affecting all platforms.
func fetchReleasePlatforms(tag string) map[string]bool {
	req, err := http.NewRequest("GET", "https://api.github.com/repos/Kheopsian/Hydra/releases/tags/"+tag, nil)
	if err != nil {
		return nil
	}
	req.Header.Set("User-Agent", "Hydra/"+Version)
	req.Header.Set("Accept", "application/vnd.github+json")
	resp, err := (&http.Client{Timeout: 5 * time.Second}).Do(req)
	if err != nil {
		return nil
	}
	defer resp.Body.Close()
	if resp.StatusCode != 200 {
		return nil
	}
	var rel struct {
		Body string `json:"body"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&rel); err != nil {
		return nil
	}
	return parsePlatforms(rel.Body)
}

// parsePlatforms extracts the "Platforms:" label from a release body; nil means
// "all platforms".
func parsePlatforms(body string) map[string]bool {
	for _, line := range strings.Split(body, "\n") {
		low := strings.ToLower(strings.TrimSpace(line))
		if !strings.HasPrefix(low, "platforms:") {
			continue
		}
		set := map[string]bool{}
		for _, p := range strings.Split(low[len("platforms:"):], ",") {
			if p = strings.TrimSpace(p); p != "" {
				set[p] = true
			}
		}
		if len(set) == 0 {
			return nil
		}
		return set
	}
	return nil
}

func normVer(v string) [3]int {
	v = strings.TrimPrefix(strings.TrimSpace(v), "v")
	end := len(v)
	for i := 0; i < len(v); i++ {
		c := v[i]
		if (c < '0' || c > '9') && c != '.' {
			end = i
			break
		}
	}
	parts := strings.Split(v[:end], ".")
	var out [3]int
	for i := 0; i < len(parts) && i < 3; i++ {
		n, _ := strconv.Atoi(parts[i])
		out[i] = n
	}
	return out
}

func verLess(a, b [3]int) bool {
	for i := 0; i < 3; i++ {
		if a[i] != b[i] {
			return a[i] < b[i]
		}
	}
	return false
}

// handleTorrentFiles returns the file list (path + size) of a torrent, looked up
// in whichever engine holds it. The engines already expose this for the qBit
// shim; this is the native-API counterpart the WebUI uses for its Content tab.
func (s *Server) handleTorrentFiles(c *gin.Context) {
	hash := strings.ToLower(c.Param("info_hash"))
	var files []map[string]interface{}
	var avail map[string]interface{}
	switch {
	case s.raceEngine != nil && s.raceEngine.HasTorrent(hash):
		files = s.raceEngine.GetTorrentFileList(hash)
		avail = s.raceEngine.GetTorrentAvailability(hash)
	case s.hoardEngine != nil && s.hoardEngine.HasTorrent(hash):
		files = s.hoardEngine.GetTorrentFileList(hash)
		avail = s.hoardEngine.GetTorrentAvailability(hash)
	default:
		c.JSON(http.StatusNotFound, gin.H{"error": "torrent not found"})
		return
	}
	if files == nil {
		files = []map[string]interface{}{}
	}
	out := gin.H{"files": files}
	if avail != nil {
		out["availability"] = avail
	}
	c.JSON(http.StatusOK, out)
}
