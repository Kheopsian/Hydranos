package engine

import (
	"context"
	"encoding/json"
	"fmt"
	"log/slog"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/Kheopsian/hydra/internal/config"
	"github.com/Kheopsian/hydra/internal/engine/ltclient"
)

var _ = json.Unmarshal // ensure import used

// ---------------------------------------------------------------------------
// RaceEngine — manages a libtorrent session via hydra-engine IPC,
// tuned for aggressive racing.
// ---------------------------------------------------------------------------

const (
	raceStatsInterval         = 2 * time.Second
	racePeerIntelInterval     = 30 * time.Second
	raceTrackerWatchInterval  = 30 * time.Second
	raceDelayedReannounceWait = 5 * time.Second
)

// ChokingEngineInterface is the contract between the race engine and the choking subsystem.
type ChokingEngineInterface interface {
	RegisterTorrent(infoHash string)
	UnregisterTorrent(infoHash string)
	Start(ctx context.Context) error
	Stop()
	GetStats() map[string]interface{}
}

// CacheManagerInterface is the contract for the predictive piece cache.
type CacheManagerInterface interface {
	OnTorrentAdded(infoHash string, numPieces int)
	OnTorrentRemoved(infoHash string)
	PreloadPieces(infoHash string, pieceIndices []int)
}

// SwarmData tracks per-torrent swarm intelligence.
type SwarmData struct {
	Seeds    int       `json:"seeds"`
	Leechers int       `json:"leechers"`
	LastSeen time.Time `json:"last_seen"`
}

// RaceEngine wraps an ltclient connection with race-specific logic.
type RaceEngine struct {
	config  *config.SessionConfig
	dataDir string

	livePort atomic.Int64 // runtime listen-port override (0 = use config.ListenPort)

	// Engine client — satisfied by either Typhon or rqbit.
	client EngineClient

	// Raw ltclient.Event fan-out for a dedicated agent (gRPC Subscribe).
	rawEvents *EventHub

	// Torrent bookkeeping.
	mu       sync.RWMutex
	torrents map[string]*TorrentInfo

	// Race-specific tracking.
	swarmData       map[string]*SwarmData
	trackerFails    map[string]int
	breaker         *trackerBreaker
	firstPeerTime   map[string]time.Time
	firstUploadTime map[string]time.Time
	addedTime       map[string]time.Time
	prevUpload      map[string]int64

	lastAnnounceTime map[string]time.Time

	// Announcer for every race announce. Cached rather than rebuilt per call:
	// building one mints a fresh peer_id, and a keepalive tick that mints a new
	// identity every 30s shows up at the tracker as a stream of distinct peers.
	// Rebuilt only when the live listen port moves under us (gluetun lease).
	annMu   sync.Mutex
	ann     *trackerAnnouncer
	annPort int

	// Cached stats.
	cachedStatsMu     sync.RWMutex
	cachedStats       map[string]*TorrentStats
	cachedTorrentList []TorrentStats

	// Cached session stats.
	cachedSessionStatsMu sync.RWMutex
	cachedSessionStats   *ltclient.SessionStats

	// Session-level counters.
	sessionGrabbed atomic.Int64

	// Callbacks.
	onComplete func(stats TorrentStats)
	onEvent    func(event string, stats TorrentStats)
	onRemove   func(infoHash string)

	// Callbacks for stats absorption.
	onBeforeRemove func(infoHash string, ul, dl int64)

	// Pluggable subsystems.
	choking      ChokingEngineInterface
	cacheManager CacheManagerInterface

	// Lifecycle.
	running bool
	ctx     context.Context
	cancel  context.CancelFunc
}

// NewRaceEngine creates a new race engine.
func NewRaceEngine(cfg *config.SessionConfig, choking ChokingEngineInterface, cacheManager CacheManagerInterface, dataDir string) *RaceEngine {
	return &RaceEngine{
		config:           cfg,
		dataDir:          dataDir,
		torrents:         make(map[string]*TorrentInfo),
		swarmData:        make(map[string]*SwarmData),
		trackerFails:     make(map[string]int),
		breaker:          newTrackerBreaker(),
		firstPeerTime:    make(map[string]time.Time),
		firstUploadTime:  make(map[string]time.Time),
		addedTime:        make(map[string]time.Time),
		prevUpload:       make(map[string]int64),
		lastAnnounceTime: make(map[string]time.Time),
		cachedStats:      make(map[string]*TorrentStats),
		choking:          choking,
		cacheManager:     cacheManager,
		rawEvents:        NewEventHub(128),
	}
}

func (e *RaceEngine) SetOnComplete(fn func(stats TorrentStats))                { e.onComplete = fn }
func (e *RaceEngine) SetOnEvent(fn func(event string, stats TorrentStats))     { e.onEvent = fn }
func (e *RaceEngine) SetOnRemove(fn func(infoHash string))                     { e.onRemove = fn }
func (e *RaceEngine) SetOnBeforeRemove(fn func(infoHash string, ul, dl int64)) { e.onBeforeRemove = fn }

// SetClient injects the engine client.
func (e *RaceEngine) RawEventHub() *EventHub { return e.rawEvents }

func (e *RaceEngine) SetClient(client EngineClient) {
	e.client = client
}

// Start launches background goroutines. Client must already be connected.
func (e *RaceEngine) Start(ctx context.Context) error {
	if e.client == nil {
		return fmt.Errorf("race: ltclient not set")
	}
	e.ctx, e.cancel = context.WithCancel(ctx)

	slog.Info("race: starting engine",
		"listen_port", e.config.ListenPort,
		"data_dir", e.dataDir,
	)

	// Event handler for completion events from the C++ engine.
	e.client.SetEventHandler(func(ev ltclient.Event) {
		if ev.Type == "torrent_completed" {
			var data ltclient.TorrentCompletedData
			if err := json.Unmarshal(ev.Data, &data); err == nil {
				now := time.Now()
				e.mu.Lock()
				info := e.torrents[data.InfoHash]
				alreadyCompleted := info != nil && !info.CompletedTime.IsZero()
				if info != nil && info.CompletedTime.IsZero() {
					info.CompletedTime = now
				}
				e.mu.Unlock()

				// Fire completed event (only for fresh completions, not resumed).
				if info != nil && !alreadyCompleted && e.onEvent != nil {
					if s, err := e.client.GetStatus(data.InfoHash); err == nil {
						stats := LtStatusToTorrentStats(*s, info.Category, info.SavePath, info.AddedTime, now)
						go e.onEvent("completed", stats)
					}
				}
			}
		}
		// Raw ltclient.Event fan-out for a dedicated agent (gRPC Subscribe).
		if e.rawEvents != nil && e.rawEvents.NumSubs() > 0 {
			if b, err := json.Marshal(ev); err == nil {
				e.rawEvents.Publish(b)
			}
		}
	})

	// Index resumed torrents and start them (race has few torrents, no stagger needed).
	e.indexResumedTorrents()
	e.startAllResumed()
	e.resumeAnnounceLoops()

	// Start choking engine.
	if e.choking != nil {
		if err := e.choking.Start(e.ctx); err != nil {
			return fmt.Errorf("race: start choking engine: %w", err)
		}
	}

	e.running = true

	go e.statsRefreshLoop()
	go e.updatePeerIntel()
	go e.trackerWatchdog()

	slog.Info("race: engine started", "resumed_torrents", len(e.torrents))
	return nil
}

// ListStatuses returns the raw per-torrent status list from the engine. Used by
// the health scanner to check conservation invariants (re-DL, fake-seed, …).
func (e *RaceEngine) ListStatuses() ([]ltclient.TorrentStatus, error) {
	if e.client == nil {
		return nil, fmt.Errorf("race engine client not connected")
	}
	result, err := e.client.ListTorrents()
	if err != nil {
		return nil, err
	}
	return result.Torrents, nil
}

func (e *RaceEngine) indexResumedTorrents() {
	result, err := e.client.ListTorrents()
	if err != nil {
		slog.Error("race: list torrents failed", "error", err)
		return
	}

	e.mu.Lock()
	defer e.mu.Unlock()

	for _, s := range result.Torrents {
		ih := s.InfoHash
		if _, exists := e.torrents[ih]; exists {
			continue
		}

		now := time.Unix(s.AddedTime, 0)
		info := &TorrentInfo{
			InfoHash:  ih,
			Name:      s.Name,
			SavePath:  s.SavePath,
			AddedTime: now,
		}

		if s.IsFinished {
			info.CompletedTime = now // placeholder
		}

		e.torrents[ih] = info
		e.addedTime[ih] = now

	}
}

// resumeAnnounceLoops re-arms the aggressive announce loop for every torrent
// that came back unfinished.
//
// raceAnnounceLoop is started by the add path and stops at completion, and the
// keepalive below only walks FINISHED torrents. Between the two, a torrent that
// was still downloading when the process went down came back announcing to
// nobody: the tracker never heard from it again, so it got no peers, so it
// never finished -- for as long as the engine ran. Restarting the loop here is
// what the add path would have done, and it is idempotent: the first announce
// that sees the torrent complete stops it again.
func (e *RaceEngine) resumeAnnounceLoops() {
	targets := e.resumeAnnounceTargets()
	for _, t := range targets {
		go e.raceAnnounceLoop(t.infoHash, t.trackerURLs, t.totalSize)
	}
	if len(targets) > 0 {
		slog.Info("race: re-armed the announce loop for resumed downloads", "count", len(targets))
	}
}

// announceTarget is one resumed torrent that needs its loop back.
type announceTarget struct {
	infoHash    string
	trackerURLs []string
	totalSize   int64
}

// resumeAnnounceTargets picks the torrents resumeAnnounceLoops arms. A paused
// one stays quiet (the operator stopped it), an errored one has nothing to
// serve, and a finished one belongs to the keepalive instead.
func (e *RaceEngine) resumeAnnounceTargets() []announceTarget {
	res, err := e.client.ListTorrents()
	if err != nil {
		return nil
	}
	var out []announceTarget
	for _, s := range res.Torrents {
		if s.IsFinished || s.IsPaused || s.State == "error" {
			continue
		}
		urls := e.supportedTrackerURLs(s.InfoHash)
		if len(urls) == 0 {
			continue
		}
		out = append(out, announceTarget{infoHash: s.InfoHash, trackerURLs: urls, totalSize: s.TotalSize})
	}
	return out
}

// supportedTrackerURLs is every tracker the engine holds for a torrent that we
// can actually speak to. A .torrent may declare schemes we have no client for
// (tcp://, wss://); announcing to those only produces a failure per pass.
func (e *RaceEngine) supportedTrackerURLs(infoHash string) []string {
	trks, err := e.client.GetTrackers(infoHash)
	if err != nil {
		return nil
	}
	var urls []string
	for _, t := range trks {
		if isSupportedTrackerScheme(t.URL) {
			urls = append(urls, t.URL)
		}
	}
	return urls
}

// startAllResumed starts all resumed torrents (race = few torrents, no stagger needed).
func (e *RaceEngine) startAllResumed() {
	e.mu.RLock()
	hashes := make([]string, 0, len(e.torrents))
	for ih := range e.torrents {
		hashes = append(hashes, ih)
	}
	e.mu.RUnlock()

	started := 0
	for _, ih := range hashes {
		if err := e.autoStart(ih); err == nil {
			started++
		}
	}
	slog.Info("race: started all resumed torrents", "count", started)
}

func (e *RaceEngine) Stop() {
	if !e.running {
		return
	}
	slog.Info("race: stopping engine")
	e.cancel()
	e.running = false
	if e.choking != nil {
		e.choking.Stop()
	}
	slog.Info("race: engine stopped")
}

// ---------------------------------------------------------------------------
// Torrent management
// ---------------------------------------------------------------------------

func (e *RaceEngine) AddTorrent(torrentPath, magnetURI, savePath string, trackers []string, category string) (string, error) {
	return e.addTorrentInternal(torrentPath, magnetURI, savePath, trackers, category, false)
}

// AddTorrentSeedMode adds a .torrent with seed_mode=true (skip_checking): the
// engine trusts the on-disk payload at savePath and seeds it immediately
// instead of re-downloading. savePath must be the exact content directory.
func (e *RaceEngine) AddTorrentSeedMode(torrentPath, savePath, category string) (string, error) {
	return e.addTorrentInternal(torrentPath, "", savePath, nil, category, true)
}

func (e *RaceEngine) addTorrentInternal(torrentPath, magnetURI, savePath string, trackers []string, category string, seedMode bool) (string, error) {
	if !e.running {
		return "", fmt.Errorf("race: engine not running")
	}

	if torrentPath == "" && magnetURI == "" {
		return "", fmt.Errorf("race: torrent_path or magnet_uri required")
	}

	var infoHash string

	if torrentPath != "" {
		torrentBytes, rerr := os.ReadFile(torrentPath)
		if rerr != nil {
			return "", fmt.Errorf("race: read torrent: %w", rerr)
		}
		ih, perr := infoHashFromTorrentFile(torrentBytes)
		if perr != nil {
			return "", fmt.Errorf("race: parse info_hash: %w", perr)
		}
		infoHash = ih

		// Ensure save_path exists.
		if savePath != "" {
			os.MkdirAll(savePath, 0755)
		}

		// Add to engine via IPC.
		result, err := e.client.AddTorrentWithOptions(torrentPath, savePath, false, seedMode)
		if err != nil {
			return "", fmt.Errorf("race: add torrent: %w", err)
		}
		_ = result
	} else {
		// Magnet URI not supported in current IPC — would need add_magnet command
		return "", fmt.Errorf("race: magnet URIs not yet supported with libtorrent engine")
	}

	now := time.Now()

	info := &TorrentInfo{
		InfoHash:        infoHash,
		Name:            nameFromTorrentFileOrEmpty(torrentPath),
		SavePath:        savePath,
		Category:        category,
		AddedTime:       now,
		TorrentFilePath: torrentPath,
	}

	e.mu.Lock()
	e.torrents[infoHash] = info
	e.addedTime[infoHash] = now
	e.mu.Unlock()

	if e.choking != nil {
		e.choking.RegisterTorrent(infoHash)
	}

	e.sessionGrabbed.Add(1)

	// Fire "added" event.
	if e.onEvent != nil {
		go e.onEvent("added", TorrentStats{
			InfoHash:  infoHash,
			Name:      info.Name,
			Category:  category,
			SavePath:  savePath,
			AddedTime: now.Unix(),
		})
	}

	slog.Info("race: torrent added", "info_hash", infoHash)

	// Start aggressive announce loop — bypass libtorrent's announce scheduler.
	go func() {
		torrentBytes, err := os.ReadFile(torrentPath)
		if err != nil {
			return
		}
		trackerURLs := trackerURLsFromTorrentFile(torrentBytes)
		var totalSize int64
		// Get size from engine status.
		if s, err := e.client.GetStatus(infoHash); err == nil {
			totalSize = s.TotalSize
		}
		e.raceAnnounceLoop(infoHash, trackerURLs, totalSize)
	}()

	return infoHash, nil
}

func nameFromTorrentFileOrEmpty(path string) string {
	if path == "" {
		return ""
	}
	data, err := os.ReadFile(path)
	if err != nil {
		return filepath.Base(path)
	}
	if name := nameFromTorrentFile(data); name != "" {
		return name
	}
	return filepath.Base(path)
}

func (e *RaceEngine) HasTorrent(infoHash string) bool {
	e.mu.RLock()
	defer e.mu.RUnlock()
	_, ok := e.torrents[infoHash]
	return ok
}

// GetTorrentFileList returns the file list for a torrent as path/size pairs.
// Paths are BEP-3 relative: relative to the info.name directory for a
// multi-file torrent, and equal to info.name for a single-file one.
func (e *RaceEngine) GetTorrentFileList(infoHash string) []map[string]interface{} {
	files, err := e.client.GetFiles(infoHash)
	if err != nil || len(files) == 0 {
		return nil
	}
	out := make([]map[string]interface{}, 0, len(files))
	for _, f := range files {
		out = append(out, map[string]interface{}{"path": f.Path, "size": f.Size})
	}
	return out
}

// TorrentFilePath returns the .torrent this engine reloads from at startup.
// The resume record holds no tracker URLs, so that file -- not the store row --
// is what decides the tracker list after a restart.
func (e *RaceEngine) TorrentFilePath(infoHash string) (string, bool) {
	e.mu.RLock()
	defer e.mu.RUnlock()
	info, ok := e.torrents[infoHash]
	if !ok || info == nil {
		return "", false
	}
	return info.TorrentFilePath, info.TorrentFilePath != ""
}

// GetTrackerTiers returns the tracker list this torrent is announcing to now,
// which is not necessarily what its .torrent said: the list is editable at
// runtime.
func (e *RaceEngine) GetTrackerTiers(infoHash string) ([][]string, error) {
	infos, err := e.client.GetTrackers(infoHash)
	if err != nil {
		return nil, err
	}
	return tiersFromTrackerInfo(infos), nil
}

// SetTrackerTiers replaces the whole list and returns what the engine kept.
// Callers persist the returned value, not the requested one.
func (e *RaceEngine) SetTrackerTiers(infoHash string, tiers [][]string) ([][]string, error) {
	applied, err := e.client.SetTrackers(infoHash, tiers)
	if err != nil {
		return nil, err
	}
	if applied == nil {
		applied = [][]string{}
	}
	slog.Info("race: tracker list replaced", "info_hash", infoHash, "tiers", len(applied))
	return applied, nil
}

// AddTrackerToTorrent appends one tracker as a new tier. Kept because the
// route that calls it predates the general edit path; both now go through the
// same primitive, so neither can quietly do nothing.
func (e *RaceEngine) AddTrackerToTorrent(infoHash, url string) error {
	current, err := e.GetTrackerTiers(infoHash)
	if err != nil {
		return err
	}
	for _, tier := range current {
		for _, u := range tier {
			if u == url {
				return nil // already there: adding it twice would announce twice
			}
		}
	}
	_, err = e.SetTrackerTiers(infoHash, append(current, []string{url}))
	return err
}

func (e *RaceEngine) SessionGrabbed() int64 {
	return e.sessionGrabbed.Load()
}

func (e *RaceEngine) RemoveTorrent(infoHash string, deleteFiles bool) error {
	// Absorb stats before removal so global totals stay correct. Fallback
	// on cachedStats when GetStatus fails (drain races where Typhon already
	// dropped the torrent internally) — sinon session_uploaded saigne.
	if e.onBeforeRemove != nil {
		var ul, dl int64
		if s, err := e.client.GetStatus(infoHash); err == nil {
			ul, dl = s.TotalUpload, s.TotalDownload
		} else {
			e.cachedStatsMu.RLock()
			if cached, ok := e.cachedStats[infoHash]; ok && cached != nil {
				ul, dl = cached.TotalUpload, cached.TotalDownload
			}
			e.cachedStatsMu.RUnlock()
			slog.Debug("race: GetStatus failed at remove, using cachedStats fallback",
				"info_hash", infoHash, "err", err, "ul", ul, "dl", dl)
		}
		e.onBeforeRemove(infoHash, ul, dl)
	}

	e.mu.Lock()
	info, exists := e.torrents[infoHash]
	if !exists {
		e.mu.Unlock()
		slog.Warn("race: torrent not found for removal", "info_hash", infoHash)
		return nil
	}

	savePath := info.SavePath
	name := info.Name
	delete(e.torrents, infoHash)
	delete(e.swarmData, infoHash)
	delete(e.trackerFails, infoHash)
	delete(e.firstPeerTime, infoHash)
	delete(e.firstUploadTime, infoHash)
	delete(e.prevUpload, infoHash)
	delete(e.addedTime, infoHash)
	e.mu.Unlock()

	if e.onRemove != nil {
		go e.onRemove(infoHash)
	}
	if e.choking != nil {
		e.choking.UnregisterTorrent(infoHash)
	}
	if e.cacheManager != nil {
		e.cacheManager.OnTorrentRemoved(infoHash)
	}

	keepData := !deleteFiles
	var aggErr error
	if err := e.client.RemoveTorrent(infoHash, keepData); err != nil {
		slog.Error("race: typhon remove failed", "info_hash", infoHash, "error", err)
		aggErr = err
	}

	// Delete real data if requested. Verbose logging — c'est le path qui laisse
	// des orphelins quand Typhon delete partiel + Lstat skip silencieux.
	if deleteFiles {
		realPath := filepath.Join(savePath, name)
		var lstatErr error
		var lstatIsDir bool
		if fi, err := os.Lstat(realPath); err == nil {
			lstatIsDir = fi.IsDir()
		} else {
			lstatErr = err
		}
		var removeErr error
		if lstatErr == nil {
			if err := os.RemoveAll(realPath); err != nil {
				removeErr = err
				aggErr = err
			}
		}
		residualCount := -1
		if entries, err := os.ReadDir(savePath); err == nil {
			residualCount = len(entries)
		}
		slog.Info("race: delete data attempt",
			"info_hash", infoHash,
			"save_path", savePath,
			"name", name,
			"real_path", realPath,
			"lstat_err", lstatErr,
			"lstat_is_dir", lstatIsDir,
			"remove_err", removeErr,
			"residual_entries_in_save_path", residualCount,
		)
		if lstatErr != nil {
			slog.Warn("race: delete skipped, path absent côté Go (Typhon a peut-être tout fait, ou path mismatch save_path/name)",
				"info_hash", infoHash, "real_path", realPath, "lstat_err", lstatErr)
		}
	}

	e.cachedStatsMu.Lock()
	delete(e.cachedStats, infoHash)
	e.cachedStatsMu.Unlock()

	slog.Info("race: torrent removed", "info_hash", infoHash, "delete_files", deleteFiles, "err", aggErr)
	return aggErr
}

// ---------------------------------------------------------------------------
// Settings & data access
// ---------------------------------------------------------------------------

// announcer returns this engine's tracker announcer, built from the session
// config so it carries announce_proxy / announce_ip like the hoard's does.
// Keyed on the port actually bound right now, not the configured one: under a
// rotating VPN lease those differ, and announcing the stale one publishes a
// port nobody answers on.
func (e *RaceEngine) announcer() *trackerAnnouncer {
	port := e.ListenPort()
	e.annMu.Lock()
	defer e.annMu.Unlock()
	if e.ann == nil || e.annPort != port {
		e.ann = newTrackerAnnouncerForSession(port, e.config, "race")
		e.annPort = port
	}
	return e.ann
}

// LivePort exposes the runtime listen-port override atomic (see hoard).
func (e *RaceEngine) LivePort() *atomic.Int64 { return &e.livePort }

// ListenPort is the port the engine is actually bound to right now (see hoard).
func (e *RaceEngine) ListenPort() int {
	if v := e.livePort.Load(); v > 0 {
		return int(v)
	}
	return e.config.ListenPort
}

// SetListenPort hot-rebinds the engine peer listener + updates the announce
// port, with no restart. No-op for a remote (non-ltclient) engine client.
// SetSelfIPs pushes our current public IP(s) to the engine self-dial filter
// (dynamic; no-op for a remote non-ltclient engine).
func (e *RaceEngine) SetSelfIPs(ips []string) {
	lt, ok := e.client.(*ltclient.Client)
	if !ok {
		return
	}
	if err := lt.SetSelfIPs(ips); err != nil {
		slog.Warn("race: set_self_ips failed", "err", err)
	}
}

func (e *RaceEngine) SetListenPort(port int) error {
	if port <= 0 || port > 65535 {
		return fmt.Errorf("race: listen port %d out of range (1-65535)", port)
	}
	lt, ok := e.client.(*ltclient.Client)
	if !ok {
		return fmt.Errorf("race: listen-port rebind unsupported on this engine client")
	}
	if err := lt.SetListenPort(port); err != nil {
		return fmt.Errorf("race: engine listen-port rebind failed: %w", err)
	}
	e.config.ListenPort = port
	e.livePort.Store(int64(port))
	slog.Info("race: listen port hot-swapped", "port", port)
	return nil
}

func (e *RaceEngine) ApplySettings(settings map[string]interface{}) {
	slog.Warn("race: hot-reload not supported with libtorrent engine; restart required")
}

func (e *RaceEngine) GetSessionSettings() map[string]interface{} {
	return map[string]interface{}{
		"listen_port":       e.config.ListenPort,
		"max_connections":   e.config.MaxConnections,
		"upload_rate_limit": e.config.UploadRateLimit,
	}
}

func (e *RaceEngine) GetChokingStats() map[string]interface{} {
	if e.choking == nil {
		return nil
	}
	return e.choking.GetStats()
}

func (e *RaceEngine) RestoreMetadata(infoHash, category, savePath, torrentFilePath string, completedTime time.Time) {
	e.mu.Lock()
	defer e.mu.Unlock()
	info, exists := e.torrents[infoHash]
	if !exists {
		return
	}
	if category != "" {
		info.Category = category
	}
	if savePath != "" {
		info.SavePath = savePath
	}
	if torrentFilePath != "" {
		info.TorrentFilePath = torrentFilePath
	}
	if !completedTime.IsZero() {
		info.CompletedTime = completedTime
	}
}

func (e *RaceEngine) TorrentCount() int {
	e.mu.RLock()
	defer e.mu.RUnlock()
	return len(e.torrents)
}

func (e *RaceEngine) GetTorrentMetas() map[string]*TorrentMeta {
	e.mu.RLock()
	defer e.mu.RUnlock()
	metas := make(map[string]*TorrentMeta, len(e.torrents))
	for ih, info := range e.torrents {
		metas[ih] = &TorrentMeta{
			SavePath:        info.SavePath,
			TorrentFilePath: info.TorrentFilePath,
			Category:        info.Category,
			CompletedTime:   info.CompletedTime,
			UserPaused:      info.UserPaused,
			Tags:            info.Tags,
		}
	}
	return metas
}

func (e *RaceEngine) GetTorrentList() []TorrentStats {
	e.cachedStatsMu.RLock()
	defer e.cachedStatsMu.RUnlock()
	if e.cachedTorrentList != nil {
		return e.cachedTorrentList
	}
	return []TorrentStats{}
}

func (e *RaceEngine) GetSessionTotals() (ul, dl int64) {
	e.cachedStatsMu.RLock()
	defer e.cachedStatsMu.RUnlock()
	for i := range e.cachedTorrentList {
		ul += e.cachedTorrentList[i].TotalUpload
		dl += e.cachedTorrentList[i].TotalDownload
	}
	return
}

func (e *RaceEngine) GetPeersForTorrent(infoHash string) []PeerInfo {
	ltPeers, err := e.client.GetPeers(infoHash)
	if err != nil {
		return nil
	}
	peers := make([]PeerInfo, 0, len(ltPeers))
	for _, p := range ltPeers {
		peers = append(peers, LtPeerToPeerInfo(p))
	}
	return peers
}

func (e *RaceEngine) GetTorrentDetail(infoHash string) map[string]interface{} {
	e.mu.RLock()
	info, exists := e.torrents[infoHash]
	swarm := e.swarmData[infoHash]
	addedAt, hasAdded := e.addedTime[infoHash]
	firstPeer, hasFirstPeer := e.firstPeerTime[infoHash]
	e.mu.RUnlock()
	if !exists {
		return nil
	}

	s, err := e.client.GetStatus(infoHash)
	if err != nil {
		return (&TorrentDetail{
			TorrentStats: TorrentStats{InfoHash: infoHash, Name: info.Name, SavePath: info.SavePath, Category: info.Category},
			Peers:        []PeerInfo{}, Trackers: []TrackerInfo{},
		}).ToMap()
	}

	stats := LtStatusToTorrentStats(*s, info.Category, info.SavePath, info.AddedTime, info.CompletedTime)

	ltPeers, _ := e.client.GetPeers(infoHash)
	peers := make([]PeerInfo, 0, len(ltPeers))
	for _, p := range ltPeers {
		peers = append(peers, LtPeerToPeerInfo(p))
	}

	detail := &TorrentDetail{
		TorrentStats: stats,
		Peers:        peers,
		NumPieces:    s.NumPieces,
		PieceLength:  s.PieceLength,
	}

	if swarm != nil {
		detail.SwarmSeeds = swarm.Seeds
		detail.SwarmLeechers = swarm.Leechers
	}

	if hasAdded && hasFirstPeer {
		ttfp := firstPeer.Sub(addedAt).Seconds()
		detail.TimeToFirstPeer = &ttfp
	}

	m := detail.ToMap()

	// The engine's tracker URLs, with every endpoint reporting what our own
	// announce loop observed -- see TrackerRows for why the engine's own
	// tracker state must not be passed through.
	if trackers, err := e.client.GetTrackers(infoHash); err == nil {
		m["trackers"] = TrackerRowsWithObs(infoHash, trackers)
	}

	return m
}

// AggregateStats returns live aggregates computed from the push-event cache.
// Pattern identique à HoardEngine — remplace la boucle buggée dans handleStatus
// qui traitait GetAllStatus() (map aggrégé) comme une liste par-torrent.
func (e *RaceEngine) AggregateStats() map[string]interface{} {
	e.cachedStatsMu.RLock()
	torrentsWithPeers := 0
	activeDL := 0
	activeSeeds := 0
	totalPeers := 0
	var totalUL, totalDL int64
	for _, ts := range e.cachedStats {
		totalPeers += ts.NumPeers
		totalUL += ts.UploadRate
		totalDL += ts.DownloadRate
		if ts.NumPeers > 0 {
			torrentsWithPeers++
		}
		switch ts.State {
		case "downloading":
			activeDL++
		case "seeding":
			activeSeeds++
		}
	}
	e.cachedStatsMu.RUnlock()
	e.mu.RLock()
	torrents := len(e.torrents)
	e.mu.RUnlock()
	return map[string]interface{}{
		"torrents":            torrents,
		"total_peers":         totalPeers,
		"torrents_with_peers": torrentsWithPeers,
		"total_upload_rate":   totalUL,
		"total_download_rate": totalDL,
		"active_downloads":    activeDL,
		"active_seeds":        activeSeeds,
	}
}

// InboundAccepted returns how many peers have opened a connection to this
// engine since it started, excluding our own addresses. See the hoard version.
func (e *RaceEngine) InboundAccepted() (int64, error) {
	d, err := e.client.GetDiagnostics()
	if err != nil {
		return 0, err
	}
	return d.Counters["inbound_accepted"], nil
}

// SampleServedInfoHash returns the info hash of one torrent this engine holds,
// or "" when it holds none. See the hoard version: it exists so the
// connectivity check can prove the thing answering on our port is this client.
func (e *RaceEngine) SampleServedInfoHash() string {
	e.mu.RLock()
	defer e.mu.RUnlock()
	for ih := range e.torrents {
		return ih
	}
	return ""
}

func (e *RaceEngine) GetAllStatus() map[string]interface{} {
	e.mu.RLock()
	count := len(e.torrents)
	e.mu.RUnlock()

	status := map[string]interface{}{
		"engine":      "race",
		"running":     e.running,
		"torrents":    count,
		"listen_port": e.config.ListenPort,
	}

	if e.choking != nil {
		status["choking"] = e.choking.GetStats()
	}

	e.cachedSessionStatsMu.RLock()
	if e.cachedSessionStats != nil {
		ss := e.cachedSessionStats
		status["peers"] = ss.NumTorrents // approximate
		status["upload_rate"] = ss.UploadRate
		status["download_rate"] = ss.DownloadRate
		status["total_uploaded"] = ss.TotalUpload
		status["total_downloaded"] = ss.TotalDownload
	}
	e.cachedSessionStatsMu.RUnlock()

	return status
}

// ---------------------------------------------------------------------------
// Background goroutines
// ---------------------------------------------------------------------------

func (e *RaceEngine) statsRefreshLoop() {
	ticker := time.NewTicker(raceStatsInterval)
	defer ticker.Stop()
	for {
		select {
		case <-e.ctx.Done():
			return
		case <-ticker.C:
			e.refreshStats()
		}
	}
}

func (e *RaceEngine) updatePeerIntel() {
	ticker := time.NewTicker(racePeerIntelInterval)
	defer ticker.Stop()
	for {
		select {
		case <-e.ctx.Done():
			return
		case <-ticker.C:
			e.scanPeerIntel()
		}
	}
}

func (e *RaceEngine) trackerWatchdog() {
	ticker := time.NewTicker(raceTrackerWatchInterval)
	defer ticker.Stop()
	for {
		select {
		case <-e.ctx.Done():
			return
		case <-ticker.C:
			e.checkTrackerHealth()
		}
	}
}

func (e *RaceEngine) refreshStats() {
	ss, err := e.client.GetSessionStats()
	if err == nil {
		e.cachedSessionStatsMu.Lock()
		e.cachedSessionStats = ss
		e.cachedSessionStatsMu.Unlock()
	}

	result, err := e.client.ListTorrents()
	if err != nil {
		return
	}

	newStats := make(map[string]*TorrentStats, len(result.Torrents))

	for _, s := range result.Torrents {
		ih := s.InfoHash

		e.mu.RLock()
		info := e.torrents[ih]
		swarm := e.swarmData[ih]
		e.mu.RUnlock()

		var category, savePath string
		var addedTime, completedTime time.Time
		if info != nil {
			category = info.Category
			savePath = info.SavePath
			addedTime = info.AddedTime
			completedTime = info.CompletedTime
		} else {
			savePath = s.SavePath
			addedTime = time.Unix(s.AddedTime, 0)
		}

		stats := LtStatusToTorrentStats(s, category, savePath, addedTime, completedTime)

		// Track completion.
		if stats.Progress >= 1.0 && completedTime.IsZero() && info != nil {
			now := time.Now()
			e.mu.Lock()
			info.CompletedTime = now
			e.mu.Unlock()
			stats.CompletedTime = now.Unix()
			if e.onComplete != nil {
				go e.onComplete(stats)
			}
			if e.onEvent != nil {
				go e.onEvent("completed", stats)
			}
		}

		if swarm != nil {
			stats.SwarmSeeds = swarm.Seeds
			stats.SwarmLeechers = swarm.Leechers
		}

		// First peer detection.
		if stats.NumPeers > 0 {
			e.mu.Lock()
			if _, hasFP := e.firstPeerTime[ih]; !hasFP {
				e.firstPeerTime[ih] = time.Now()
				if e.onEvent != nil {
					go e.onEvent("first_peer", stats)
				}
			}
			e.mu.Unlock()

		}

		// First upload.
		if stats.TotalUpload > 0 {
			e.mu.Lock()
			if _, hasFU := e.firstUploadTime[ih]; !hasFU {
				e.firstUploadTime[ih] = time.Now()
				if e.onEvent != nil {
					go e.onEvent("first_upload", stats)
				}
			}
			e.mu.Unlock()
		}

		newStats[ih] = &stats
	}

	accrueSeedTime(newStats)

	e.cachedStatsMu.Lock()
	e.cachedStats = newStats
	list := make([]TorrentStats, 0, len(newStats))
	for _, s := range newStats {
		list = append(list, *s)
	}
	e.cachedTorrentList = list
	e.cachedStatsMu.Unlock()
}

func (e *RaceEngine) scanPeerIntel() {
	// No-op: with internal_announce=off, list_seeds/list_peers from libtorrent
	// are always 0 and would clobber the real swarm counts written by
	// tracker_announce.go after each successful announce.
}

// raceSeedReannounce* keep COMPLETED race torrents in the private-tracker swarm.
// Their aggressive announce loop (raceAnnounceLoop) stops at completion and
// startAllResumed does NOT restart it — so without this, a finished torrent stops
// announcing, the tracker drops us after its interval, and after a process
// restart we never re-announce at all and silently stop seeding (we still have
// the files). This watchdog body re-announces finished torrents periodically.
const (
	raceSeedReannounceInterval = 25 * time.Minute // re-announce a finished torrent at most this often
	raceSeedReannouncePerTick  = 20               // stagger: cap re-announces per watchdog tick (30s)
)

func (e *RaceEngine) checkTrackerHealth() {
	res, err := e.client.ListTorrents()
	if err != nil {
		return
	}
	now := time.Now()
	announcer := e.announcer()
	done := 0
	for _, s := range res.Torrents {
		if done >= raceSeedReannouncePerTick {
			break
		}
		// Only finished torrents — active racers are handled by raceAnnounceLoop.
		if !s.IsFinished {
			continue
		}
		// A stopped torrent stays quiet. The hoard announcer has always filtered
		// these out; this loop never did, and only got away with it because a
		// seed-mode torrent used to report is_finished=false until it ran.
		if s.IsPaused {
			continue
		}
		// Its data is gone, so there is nothing to serve: staying in the swarm
		// only advertises pieces every peer will be rejected for.
		if s.State == "error" {
			continue
		}
		ih := s.InfoHash
		e.mu.RLock()
		_, tracked := e.torrents[ih]
		last := e.lastAnnounceTime[ih]
		e.mu.RUnlock()
		if !tracked {
			continue
		}
		if !last.IsZero() && now.Sub(last) < raceSeedReannounceInterval {
			continue
		}

		// Every tracker this torrent carries, not just the first: a torrent
		// cross-seeded to a second tracker was announced only to whichever
		// tracker happened to come first in the engine's list, and dropped out
		// of every other swarm as its announce interval expired.
		trackerURLs := e.supportedTrackerURLs(ih)
		if len(trackerURLs) == 0 {
			continue
		}

		// Stamp the attempt now (even on failure) so a dead torrent isn't retried
		// every tick — it gets one shot per interval like everyone else.
		e.mu.Lock()
		e.lastAnnounceTime[ih] = now
		e.mu.Unlock()

		announced := false
		for _, trackerURL := range trackerURLs {
			host := overrideHost(trackerURL)
			if !e.breaker.allows(host, now) {
				continue
			}
			// Periodic seeding announce (event=""): reports the real cumulative
			// uploaded/downloaded so ratio is credited, and refreshes swarmData.
			result, aerr := announcer.announce(trackerURL, ih, s.TotalUpload, s.TotalDownload, 0, "")
			if aerr != nil || result == nil || result.FailureReason != "" {
				// A failure reason is the tracker answering, so it is up: only
				// a transport error counts against the breaker.
				e.breaker.record(host, aerr == nil && result != nil, now)
				reason := "no answer"
				if aerr != nil {
					reason = aerr.Error()
				} else if result != nil {
					reason = result.FailureReason
				}
				recordTrackerObs(ih, trackerURL, TrackerObservation{ErrorMsg: reason})
				if announceFailureIsWorthLogging(host) {
					slog.Warn("race: seed announce failed",
						"info_hash", ih[:minStr(len(ih), 8)], "tracker", trackerURL, "error", reason)
				}
				continue
			}
			e.breaker.record(host, true, now)
			announced = true
			obs := TrackerObservation{OK: true, Seeds: result.Complete, Leechers: result.Incomplete, LastAt: now}
			if result.Interval > 0 {
				obs.NextAt = now.Add(time.Duration(result.Interval) * time.Second)
			}
			recordTrackerObs(ih, trackerURL, obs)

			// swarmData holds one entry per torrent, so the last tracker to
			// answer wins. Peers are additive across trackers, counts are not.
			e.mu.Lock()
			if e.swarmData[ih] == nil {
				e.swarmData[ih] = &SwarmData{}
			}
			e.swarmData[ih].Seeds = result.Complete
			e.swarmData[ih].Leechers = result.Incomplete
			e.swarmData[ih].LastSeen = now
			e.mu.Unlock()

			if len(result.Peers) > 0 {
				peers := make([]struct {
					IP   string
					Port int
				}, len(result.Peers))
				for i, p := range result.Peers {
					peers[i] = struct {
						IP   string
						Port int
					}{p.IP, p.Port}
				}
				e.client.AddPeers(ih, peers)
			}
		}
		if !announced {
			continue
		}
		done++
	}
	if done > 0 {
		slog.Info("race: watchdog re-announced finished torrents (seed keepalive)", "count", done)
	}
}

// Unused but kept for API compatibility.
var _ = strings.HasSuffix

// StartTorrent resumes a torrent (agent RPC passthrough to the engine client).
func (e *RaceEngine) StartTorrent(infoHash string) error {
	if e.client == nil {
		return fmt.Errorf("engine client not ready")
	}
	return e.client.StartTorrent(infoHash)
}

// StopTorrent pauses a torrent (agent RPC passthrough to the engine client).
func (e *RaceEngine) StopTorrent(infoHash string) error {
	if e.client == nil {
		return fmt.Errorf("engine client not ready")
	}
	return e.client.StopTorrent(infoHash)
}

// GraduationInfo returns the fields the graduation mover needs for one torrent.
func (e *RaceEngine) GraduationInfo(infoHash string) (savePath, torrentFilePath, name, category string, ok bool) {
	e.mu.RLock()
	defer e.mu.RUnlock()
	info, exists := e.torrents[infoHash]
	if !exists {
		return "", "", "", "", false
	}
	return info.SavePath, info.TorrentFilePath, info.Name, info.Category, true
}

// ClearCategoryLabel drops the given category label from every race torrent
// that carries it, mirroring the hoard engine. Without it, deleting a category
// left race torrents pointing at a label that no longer exists — the chip stayed
// in the sidebar, which is built from the torrents' own categories.
func (e *RaceEngine) ClearCategoryLabel(category string) int {
	if category == "" {
		return 0
	}
	e.mu.Lock()
	hits := make([]string, 0)
	for ih, info := range e.torrents {
		if info.Category == category {
			info.Category = ""
			hits = append(hits, ih)
		}
	}
	e.mu.Unlock()
	if len(hits) == 0 {
		return 0
	}
	e.cachedStatsMu.Lock()
	for _, ih := range hits {
		if st, ok := e.cachedStats[ih]; ok && st != nil {
			st.Category = ""
		}
	}
	e.cachedStatsMu.Unlock()
	return len(hits)
}

// GetTorrentAvailability reports how many copies of each piece the swarm holds.
// Nil when the engine does not know: a seed-mode torrent has no piece map at
// all, which the caller must present as "unknown", not as zero.
func (e *RaceEngine) GetTorrentAvailability(infoHash string) map[string]interface{} {
	a, err := e.client.GetAvailability(infoHash)
	if err != nil || a == nil || !a.HasPieceMap {
		return nil
	}
	return map[string]interface{}{
		"min":        a.MinAvailability,
		"max":        a.MaxAvailability,
		"avg":        a.AvgAvailability,
		"num_pieces": a.NumPieces,
	}
}

// SetEngineOptFlag toggles one engine-side optimisation without a restart.
func (e *RaceEngine) SetEngineOptFlag(name string, on bool, value int64) (map[string]interface{}, error) {
	return e.client.SetEngineOptFlag(name, on, value)
}

// EngineOptFlags reports the engine-side flag state.
func (e *RaceEngine) EngineOptFlags() (map[string]interface{}, error) {
	return e.client.EngineOptFlags()
}

// FetchMetadata forwards a magnet resolution request to the engine client.
// Resolution runs wherever the client points, so on a remote agent it happens
// on the agent's own network.
func (e *RaceEngine) FetchMetadata(infoHash string, trackers, peers []string, bindingID *uint32) (*ltclient.FetchMetadataResult, error) {
	if e.client == nil {
		return nil, fmt.Errorf("race: engine client not available")
	}
	return e.client.FetchMetadata(infoHash, trackers, peers, bindingID)
}

// GetMetadata polls a resolution started by FetchMetadata.
func (e *RaceEngine) GetMetadata(infoHash string) (*ltclient.GetMetadataResult, error) {
	if e.client == nil {
		return nil, fmt.Errorf("race: engine client not available")
	}
	return e.client.GetMetadata(infoHash)
}
