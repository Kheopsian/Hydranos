package api

import (
	"bufio"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
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

// GetHoardSessionDelta returns hoard UL/DL transferred since boot.
func GetHoardSessionDelta() (ul, dl int64) {
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
	bl := struct {
		TotalUploaded   int64 `json:"total_uploaded"`
		TotalDownloaded int64 `json:"total_downloaded"`
	}{
		TotalUploaded:   atomic.LoadInt64(&baselineUploaded),
		TotalDownloaded: atomic.LoadInt64(&baselineDownloaded),
	}
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

// getRainTotals returns current cumulative UL/DL from Rain (Bolt).
func getRainTotals() (ul, dl int64) {
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
	Name      string            `json:"name"`
	SavePath  string            `json:"save_path"`
	Mode      string            `json:"mode"`
	Agents    map[string]string `json:"agents,omitempty"`    // per-agent save_path override; empty = flat SavePath (local agent)
	Placement []string          `json:"placement,omitempty"` // agent names hosting new torrents; empty = ["local"]
	Strategy  string            `json:"strategy,omitempty"`  // pick among Placement: all|least_torrents|least_load (default all)
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
	SavePath  string            `json:"save_path"`
	Mode      string            `json:"mode"`
	Agents    map[string]string `json:"agents,omitempty"`
	Placement []string          `json:"placement,omitempty"`
	Strategy  string            `json:"strategy,omitempty"`
}

func categoriesFile(dataDir string) string {
	return filepath.Join(dataDir, "categories.json")
}

func loadCategories(dataDir string) []category {
	data, err := os.ReadFile(categoriesFile(dataDir))
	if err != nil {
		return []category{}
	}
	// On-disk format is a map: {"name": {"save_path": "...", "mode": "..."}}
	var catMap map[string]categoryJSON
	if json.Unmarshal(data, &catMap) != nil {
		return []category{}
	}
	cats := make([]category, 0, len(catMap))
	for name, cj := range catMap {
		cats = append(cats, category{Name: name, SavePath: cj.SavePath, Mode: cj.Mode, Agents: cj.Agents, Placement: cj.Placement, Strategy: cj.Strategy})
	}
	sort.Slice(cats, func(i, j int) bool { return cats[i].Name < cats[j].Name })
	return cats
}

func saveCategories(dataDir string, cats []category) error {
	catMap := make(map[string]categoryJSON, len(cats))
	for _, cat := range cats {
		catMap[cat.Name] = categoryJSON{SavePath: cat.SavePath, Mode: cat.Mode, Agents: cat.Agents, Placement: cat.Placement, Strategy: cat.Strategy}
	}
	data, err := json.MarshalIndent(catMap, "", "  ")
	if err != nil {
		return err
	}
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

		// Per-tracker announce passkey override (hot-swap, no restart)
		api.GET("/announce/passkeys", s.handleGetPasskeys)
		api.POST("/announce/passkeys", s.handleSetPasskey)
		// Per-tracker client spoof (peer_id prefix + UA) to pass client whitelists
		api.GET("/announce/clients", s.handleGetClients)
		api.POST("/announce/clients", s.handleSetClient)

		api.GET("/announce/secondary-stats", s.handleGetSecondaryStats)
		api.POST("/announce/secondary-stats", s.handleSetSecondaryStats)

		// Global status
		api.GET("/status", s.handleStatus)
		api.GET("/update-check", s.handleUpdateCheck)

		// Server-Sent Events stream (Typhon push → browser).
		// Emits the same wire-format as Rust: {"event":"stats_snapshot","data":{...}}.
		api.GET("/events", s.handleSSE)

		// qBittorrent migration (onboarding import)
		api.POST("/import/qbit/preview", s.handleQbitPreview)
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
			race.POST("/listen-port", s.handleRaceSetListenPort)
		}

		// Hoard engine
		hoard := api.Group("/hoard")
		{
			hoard.GET("/stats", s.handleHoardStats)
			hoard.GET("/torrents", s.handleHoardTorrents)
			hoard.GET("/torrents/:info_hash", s.handleHoardTorrentDetail)
			hoard.POST("/pause-all", s.handleHoardPauseAll)
			hoard.POST("/resume-all", s.handleHoardResumeAll)
			hoard.POST("/listen-port", s.handleHoardSetListenPort)
			hoard.POST("/restart-stuck", s.handleHoardRestartStuck)
			hoard.POST("/verify-downloading", s.handleHoardVerifyDownloading)
			hoard.POST("/torrents/:info_hash/verify", s.handleHoardVerifyTorrent)
			hoard.POST("/torrents/:info_hash/category", s.handleHoardSetCategory)
			hoard.GET("/download-slots", s.handleHoardDownloadSlotsGet)
			hoard.POST("/download-slots", s.handleHoardDownloadSlotsSet)
			hoard.DELETE("/download-slots", s.handleHoardDownloadSlotsClear)
			hoard.POST("/torrents/:info_hash/pin", s.handleHoardPin)
			hoard.POST("/torrents/:info_hash/unpin", s.handleHoardUnpin)
			hoard.GET("/pinned", s.handleHoardPinnedList)
		}

		// Categories
		api.GET("/categories", s.handleCategoriesGet)
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

// localAdd performs the rich local add for a mode (today's monolith path).
func (s *Server) localAdd(mode, torrentPath, magnetURI, savePath string, trackers []string, category string) (string, error) {
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
	c.JSON(http.StatusOK, gin.H{"status": "ok", "paused": count})
}

func (s *Server) handleHoardResumeAll(c *gin.Context) {
	if s.hoardEngine == nil {
		c.JSON(http.StatusServiceUnavailable, gin.H{"error": "hoard engine not available"})
		return
	}
	count := s.hoardEngine.ResumeAll()
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

	if !found {
		c.JSON(http.StatusNotFound, gin.H{"error": "category not found"})
		return
	}

	if err := saveCategories(s.config.Daemon.DataDir, newCats); err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	c.JSON(http.StatusOK, gin.H{"status": "ok"})
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
		c.JSON(http.StatusBadRequest, gin.H{"error": "iperf3_server non configuré dans [vpn_speedtest]"})
		return
	}
	result, err := runIperf3(cfg.Iperf3Server, cfg.Iperf3Port, cfg.DurationSecs)
	if err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	if s.benchDB != nil {
		s.benchDB.InsertVpn(result["ts"].(float64), result["ul_mbps"].(float64), result["dl_mbps"].(float64))
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
	racePort := s.config.Race.ListenPort
	hoardPort := s.config.Hoard.ListenPort

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
	if cachedPublicIP != "" && time.Since(cachedPublicIPTime) < 5*time.Minute {
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
	if cachedPublicIPv6 != "" && time.Since(cachedPublicIPv6Time) < 5*time.Minute {
		return cachedPublicIPv6
	}
	ip := fetchPublicIP("https://api6.ipify.org/")
	if ip != "" {
		cachedPublicIPv6 = ip
		cachedPublicIPv6Time = time.Now()
	}
	return cachedPublicIPv6
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
	if ul > 0 || dl > 0 {
		saveBaseline()
	}
}

// handleGetPasskeys returns the current per-tracker announce passkey overrides.
func (s *Server) handleGetPasskeys(c *gin.Context) {
	c.JSON(http.StatusOK, engine.GetPasskeyOverrides())
}

// handleSetPasskey sets (passkey="" clears) the announce passkey for trackers
// whose URL contains host. Hot — applies on the next announce, no restart.
// setListenPort hot-swaps an engine peer listen port at runtime (no restart).
func (s *Server) setListenPort(c *gin.Context, setter interface{ SetListenPort(int) }) {
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
	setter.SetListenPort(req.Port)
	c.JSON(http.StatusOK, gin.H{"status": "ok", "port": req.Port})
}

func (s *Server) handleHoardSetListenPort(c *gin.Context) {
	s.setListenPort(c, s.hoardEngine)
}

func (s *Server) handleRaceSetListenPort(c *gin.Context) {
	s.setListenPort(c, s.raceEngine)
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
	c.JSON(http.StatusOK, gin.H{"status": "ok", "passkeys": engine.GetPasskeyOverrides()})
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
	c.JSON(http.StatusOK, gin.H{"status": "ok", "clients": engine.GetClientOverrides()})
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
	c.JSON(http.StatusOK, gin.H{"status": "ok", "secondary_stats": engine.GetSecondaryStatsOverrides()})
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
	if time.Since(updateCheckAt) < 6*time.Hour && updateCheckLatest != "" {
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
	best := ""
	var bestV [3]int
	for _, t := range tags {
		if !strings.HasPrefix(t.Name, "v") {
			continue
		}
		v := normVer(t.Name)
		if best == "" || verLess(bestV, v) {
			best = t.Name
			bestV = v
		}
	}
	if best != "" {
		updateCheckLatest = best
		updateCheckURL = "https://github.com/Kheopsian/Hydra/releases/tag/" + best
	}
	return updateCheckLatest, updateCheckURL
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
