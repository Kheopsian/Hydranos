package engine

import (
	"context"
	"encoding/json"
	"fmt"
	"log/slog"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/Kheopsian/hydra/internal/config"
	"github.com/Kheopsian/hydra/internal/engine/ltclient"
)

var _ = json.Unmarshal // ensure import used

// ---------------------------------------------------------------------------
// HoardEngine — manages a libtorrent session via hydra-engine IPC,
// optimised for massive long-term seeding (16k+ torrents).
// ---------------------------------------------------------------------------

const (
	// Cadence of the periodic full-snapshot polling (list_torrents +
	// diagnostics). Lowered from 2s/10s to 60s/60s once push events
	// (stats_snapshot) became the authoritative live source 2026-04-19.
	// Polling now plays the role of a drift-correction backstop (static
	// metadata, scrape results) — the 13k-torrent JSON serialization
	// cost (82 ms/call on prod) happens 30× less often.
	hoardStatsInterval       = 60 * time.Second
	hoardTorrentListInterval = 60 * time.Second
)

// HoardEngine wraps an ltclient connection with Hydra-specific bookkeeping.
type HoardEngine struct {
	config              *config.SessionConfig
	CreateTorrentFolder bool // daemon-global: wrap single-file torrents in a per-name folder (qBit-style; off by default)
	dataDir             string

	livePort      atomic.Int64 // runtime listen-port override (0 = use config.ListenPort)
	expectedTotal atomic.Int64 // store COUNT during boot-from-store import (0=idle) -> total_torrents shows real total, not a climbing count

	// Engine client — satisfied by either *ltclient.Client (Typhon) or
	// *rqbitclient.Client (rqbit). Set via SetClient after the engine
	// process is spawned by main.go.
	client EngineClient

	// Torrent bookkeeping (info_hash hex -> metadata).
	mu       sync.RWMutex
	torrents map[string]*TorrentInfo

	// Torrents parked by the download slot manager.
	slotParkedMu sync.RWMutex
	slotParked   map[string]bool

	// Per-torrent download progress tracking for activity-based parking.
	slotProgressMu sync.Mutex
	slotProgress   map[string]*slotProgressInfo
	lastSlotLog    time.Time

	// Manually pinned torrents: always hold a download slot (exempt from the
	// swarm-seed ranking) and never activity-demoted. For deliberate source
	// grabs (BDMV, rarities) we want regardless of swarm health. Persisted to
	// <dataDir>/hoard_pinned.json so pins survive restarts/redeploys.
	pinnedMu sync.RWMutex
	pinned   map[string]bool

	// Runtime override for active_downloads.
	dlSlotsOverride   int
	dlSlotsOverrideMu sync.Mutex

	// Cached slot stats.
	lastSlotStatsMu sync.RWMutex
	lastSlotStats   DownloadSlotStats

	// Cached data served to the API.
	cachedStatsMu     sync.RWMutex
	cachedStats       map[string]*TorrentStats
	cachedTorrentList []TorrentStats

	// Cached session stats.
	cachedSessionStatsMu sync.RWMutex
	cachedSessionStats   *ltclient.SessionStats

	// Cached diagnostics.
	cachedDiagnosticsMu sync.RWMutex
	cachedDiagnostics   *ltclient.DiagnosticStats

	// Push event fan-out to in-process SSE subscribers. Populated after
	// SubscribeEvents (Rust -> Go) so the same wire-format frames can be
	// re-broadcast to browser clients via /api/events.
	events    *EventHub
	rawEvents *EventHub

	// Callbacks
	onBeforeRemove func(infoHash string, ul, dl int64)

	// bootstrapAnnounce fires one immediate announce on add so a fresh
	// download's swarm-seed count reaches the slot-manager cache right away
	// (breaks the seeds=0 -> parked -> announce-loop-skips-parked catch-22 that
	// leaves seeded torrents stuck at 0 peers). Wired to
	// HoardAnnouncer.BootstrapAnnounce in main.go.
	bootstrapAnnounce func(infoHash string, totalSize int64)

	// reAnnounce fires one immediate SEEDER announce (real left, 0 for a
	// complete torrent) for an already-active torrent whose in-place move
	// dropped it from the tracker swarm — SetTorrentCategory does
	// stop->rename->start and the stop emits event=stopped. Without it the
	// torrent shows 0 seeders until the next slot-gated periodic announce.
	// Wired to HoardAnnouncer.ReAnnounce in main.go.
	reAnnounce func(infoHash string, totalSize int64)

	// stoppedAnnounce tells the trackers we are leaving, once, when the user
	// stops a torrent. Without it a stop is silent and the tracker keeps us in
	// the swarm until our entry goes stale. Wired to
	// HoardAnnouncer.StoppedAnnounce in main.go.
	stoppedAnnounce func(infoHash string, totalSize int64)

	// Announce offset (continuité handoff race->hoard) : UL/DL hérités d'un
	// doublon race purgé. Ajouté UNIQUEMENT au cumulé ANNONCÉ au tracker (pas
	// aux compteurs globaux : ceux-là sont préservés par AbsorbStats côté race).
	annOffMu sync.Mutex
	annOffUL map[string]int64
	annOffDL map[string]int64

	// Lifecycle
	running     bool
	staggerDone chan struct{}
	ctx         context.Context
	cancel      context.CancelFunc
}

// SetOnBeforeRemove registers a callback fired before a torrent is removed.
// Used to absorb UL/DL stats into the baseline.
// HasTorrent reports whether the hoard engine currently holds this infohash.
func (e *HoardEngine) HasTorrent(infoHash string) bool {
	e.mu.RLock()
	defer e.mu.RUnlock()
	_, ok := e.torrents[infoHash]
	return ok
}

// SetBootstrapAnnounce wires the one-shot on-add announce (see field doc).
func (e *HoardEngine) SetBootstrapAnnounce(fn func(infoHash string, totalSize int64)) {
	e.bootstrapAnnounce = fn
}

// SetReAnnounce wires the one-shot post-move seeder re-announce (see field doc).
func (e *HoardEngine) SetReAnnounce(fn func(infoHash string, totalSize int64)) {
	e.reAnnounce = fn
}

// SetStoppedAnnounce wires the one-shot leaving-the-swarm announce (see field doc).
func (e *HoardEngine) SetStoppedAnnounce(fn func(infoHash string, totalSize int64)) {
	e.stoppedAnnounce = fn
}

// AddAnnounceOffset ajoute un offset UL/DL hérité (handoff d'un doublon race
// purgé) appliqué UNIQUEMENT au cumulé annoncé au tracker — pas aux compteurs
// globaux (préservés par AbsorbStats). Rend la transition race->hoard continue.
func (e *HoardEngine) AddAnnounceOffset(infoHash string, ul, dl int64) {
	if ul <= 0 && dl <= 0 {
		return
	}
	e.annOffMu.Lock()
	e.annOffUL[infoHash] += ul
	e.annOffDL[infoHash] += dl
	totUL, totDL := e.annOffUL[infoHash], e.annOffDL[infoHash]
	e.annOffMu.Unlock()
	short := infoHash
	if len(short) > 8 {
		short = short[:8]
	}
	slog.Info("hoard: announce offset inherited from race dup",
		"info_hash", short, "added_ul", ul, "added_dl", dl, "total_ul", totUL, "total_dl", totDL)
}

// AnnounceOffset renvoie l'offset UL/DL à ajouter au cumulé annoncé.
func (e *HoardEngine) AnnounceOffset(infoHash string) (int64, int64) {
	e.annOffMu.Lock()
	defer e.annOffMu.Unlock()
	return e.annOffUL[infoHash], e.annOffDL[infoHash]
}

func (e *HoardEngine) SetOnBeforeRemove(fn func(infoHash string, ul, dl int64)) {
	e.onBeforeRemove = fn
}

// NewHoardEngine creates a new engine with the given session config.
func NewHoardEngine(cfg *config.SessionConfig, dataDir string) *HoardEngine {
	e := &HoardEngine{
		config:          cfg,
		dataDir:         dataDir,
		torrents:        make(map[string]*TorrentInfo),
		slotParked:      make(map[string]bool),
		slotProgress:    make(map[string]*slotProgressInfo),
		dlSlotsOverride: -1,
		cachedStats:     make(map[string]*TorrentStats),
		annOffUL:        make(map[string]int64),
		annOffDL:        make(map[string]int64),
		staggerDone:     make(chan struct{}),
		events:          NewEventHub(128),
		rawEvents:       NewEventHub(128),
		pinned:          make(map[string]bool),
	}
	e.loadPinned()
	return e
}

// EventHub exposes the internal push-event fan-out so HTTP handlers
// (e.g. SSE) can subscribe to live updates from Typhon.
func (e *HoardEngine) EventHub() *EventHub { return e.events }

func (e *HoardEngine) RawEventHub() *EventHub { return e.rawEvents }

// SetClient injects the engine client (set by main.go after engine process starts).
// SetServingSuspended toggles disk-serving suspension for one torrent (HDD
// quiet-mode lever): keeps peers + announces, serves no piece Requests (zero
// disk I/O). Used by the per-disk seed-slot manager.
func (e *HoardEngine) SetServingSuspended(infoHash string, suspended bool) error {
	lt, ok := e.client.(*ltclient.Client)
	if !ok {
		return fmt.Errorf("serving-suspend unsupported on this engine client")
	}
	return lt.SetServingSuspended(infoHash, suspended)
}

func (e *HoardEngine) SetClient(client EngineClient) {
	e.client = client
}

// Start launches background goroutines.
// The ltclient must already be connected (via SetClient).
func (e *HoardEngine) Start(ctx context.Context) error {
	if e.client == nil {
		return fmt.Errorf("hoard: ltclient not set")
	}
	e.ctx, e.cancel = context.WithCancel(ctx)

	slog.Info("hoard: starting engine",
		"listen_port", e.config.ListenPort,
		"data_dir", e.dataDir,
	)

	// Set event handler for push-based stats + lifecycle events.
	// Typhon pushes stats_snapshot every ~1s (delta-filtered) instead of
	// Go polling list_torrents every 2s. See typhon-engine/src/rpc/events.rs.
	e.client.SetEventHandler(func(ev ltclient.Event) {
		switch ev.Type {
		case "torrent_completed":
			var data ltclient.TorrentCompletedData
			if err := json.Unmarshal(ev.Data, &data); err == nil {
				e.mu.Lock()
				if info := e.torrents[data.InfoHash]; info != nil && info.CompletedTime.IsZero() {
					info.CompletedTime = time.Now()
				}
				e.mu.Unlock()
				slog.Info("hoard: torrent completed", "info_hash", data.InfoHash[:minStr(len(data.InfoHash), 16)])
			}
		case "torrent_added":
			var data ltclient.TorrentAddedData
			if err := json.Unmarshal(ev.Data, &data); err == nil {
				e.mu.Lock()
				if _, exists := e.torrents[data.InfoHash]; !exists {
					e.torrents[data.InfoHash] = &TorrentInfo{
						InfoHash:  data.InfoHash,
						Name:      data.Name,
						SavePath:  data.SavePath,
						AddedTime: time.Now(),
					}
				}
				// AddTorrent() populates the canonical Category before Typhon
				// emits this event; capture it so the seeded cachedStats row
				// carries the category immediately (else the row shows "none"
				// until the 60s refreshStats tick).
				addedCategory := e.torrents[data.InfoHash].Category
				addedCF := e.torrents[data.InfoHash].ContentFolder
				e.mu.Unlock()
				// Seed the stats cache with the static metadata from the add
				// event NOW. GetTorrentList() and the SSE list hydration read
				// cachedStats; without this a freshly-added torrent surfaces with
				// an empty name (the UI falls back to its info-hash) until the 60s
				// refreshStats backfills it — the "add → refresh shows the id →
				// refresh again shows the name" bug. Separate lock (not nested in
				// e.mu) to avoid any ordering hazard.
				state := "downloading"
				if data.SeedMode {
					state = "seeding"
				}
				e.cachedStatsMu.Lock()
				st, ok := e.cachedStats[data.InfoHash]
				if !ok {
					st = &TorrentStats{InfoHash: data.InfoHash}
					e.cachedStats[data.InfoHash] = st
				}
				if st.Name == "" {
					st.Name = data.Name
				}
				if st.SavePath == "" {
					st.SavePath = data.SavePath
				}
				if st.TotalSize == 0 {
					st.TotalSize = data.TotalSize
				}
				if st.State == "" {
					st.State = state
				}
				if st.AddedTime == 0 {
					st.AddedTime = time.Now().Unix()
				}
				if st.Category == "" {
					st.Category = addedCategory
				}
				if st.ContentFolder == nil {
					st.ContentFolder = addedCF
				}
				e.cachedStatsMu.Unlock()
			}
		case "torrent_removed":
			var data ltclient.TorrentRemovedData
			if err := json.Unmarshal(ev.Data, &data); err == nil {
				e.mu.Lock()
				delete(e.torrents, data.InfoHash)
				e.mu.Unlock()
				e.cachedStatsMu.Lock()
				delete(e.cachedStats, data.InfoHash)
				e.cachedStatsMu.Unlock()
				forgetTrackerObs(data.InfoHash)
			}
		case "stats_snapshot":
			var data ltclient.StatsSnapshotData
			if err := json.Unmarshal(ev.Data, &data); err != nil {
				return
			}
			// Apply delta to cachedStats. Only dynamic fields — static
			// metadata (name, save_path, swarm scrape) is owned by the
			// periodic refreshStats() loop (now running at 60s as a backstop
			// for anything not in the event stream). Category is the
			// exception: a torrent can transition checking→seeding here well
			// before the first refresh, so we backfill it from the canonical
			// e.torrents map to avoid the row surfacing as "none" meanwhile.
			e.mu.RLock()
			snapCategories := make(map[string]string, len(data.Torrents))
			for _, m := range data.Torrents {
				if info := e.torrents[m.InfoHash]; info != nil {
					snapCategories[m.InfoHash] = info.Category
				}
			}
			e.mu.RUnlock()
			e.cachedStatsMu.Lock()
			for _, m := range data.Torrents {
				st, exists := e.cachedStats[m.InfoHash]
				if !exists {
					// First time we see this torrent (before periodic refresh
					// populated the metadata). Minimal entry; refreshStats
					// will enrich later.
					st = &TorrentStats{InfoHash: m.InfoHash}
					e.cachedStats[m.InfoHash] = st
				}
				if st.Category == "" {
					st.Category = snapCategories[m.InfoHash]
				}
				st.UploadRate = m.UploadRate
				st.DownloadRate = m.DownloadRate
				st.TotalUpload = m.TotalUploaded
				st.TotalDownload = m.TotalDownloaded
				st.NumPeers = m.PeersConnected
				// Map typhon status enum → libtorrent state string.
				switch m.Status {
				case 0:
					// Halted: the intent flag says whether that was us or a
					// scheduler.
					st.State = haltedState(st.UserPaused)
				case 1:
					st.State = "checking_files"
				case 2:
					st.State = "downloading"
				case 3:
					st.State = "seeding"
				}
				if st.TotalSize > 0 && st.TotalDone > 0 {
					st.Ratio = float64(st.TotalUpload) / float64(st.TotalDone)
				}
			}
			e.cachedStatsMu.Unlock()
		}

		// Fan-out the same wire-format event to in-process SSE clients
		// (browser UI via /api/events). Drop silently if no subscribers.
		if e.events != nil && e.events.NumSubs() > 0 {
			if raw, err := json.Marshal(map[string]interface{}{
				"event": ev.Type,
				"data":  ev.Data,
			}); err == nil {
				e.events.Publish(raw)
			}
		}
		// Raw ltclient.Event fan-out for a dedicated agent (gRPC Subscribe): the
		// front's grpcclient decodes these back into ltclient.Event, so the shape
		// is json.Marshal(ev), NOT the browser {event,data} above.
		if e.rawEvents != nil && e.rawEvents.NumSubs() > 0 {
			if b, err := json.Marshal(ev); err == nil {
				e.rawEvents.Publish(b)
			}
		}
	})

	// Activate push stream (no-op on non-typhon engines).
	if err := e.client.SubscribeEvents(); err != nil {
		slog.Warn("hoard: subscribe_events failed — falling back on polling only", "error", err)
	} else {
		slog.Info("hoard: subscribed to push event stream (stats_snapshot / lifecycle)")
	}

	// Index torrents loaded from resume data by the engine.
	e.indexResumedTorrents()

	// Stagger-start all torrents.
	go e.staggerStart()

	e.running = true

	go e.statsRefreshLoop()
	go e.torrentListRefreshLoop()
	go e.verifyThrottle()

	if e.config.ActiveDownloads >= 0 {
		go e.downloadSlotManager()
	}

	slog.Info("hoard: engine started", "resumed_torrents", len(e.torrents))
	return nil
}

// ListStatuses returns the raw per-torrent status list from the engine. Used by
// the health scanner to check conservation invariants (re-DL, fake-seed, …).
func (e *HoardEngine) ListStatuses() ([]ltclient.TorrentStatus, error) {
	if e.client == nil {
		return nil, fmt.Errorf("hoard engine client not connected")
	}
	result, err := e.client.ListTorrents()
	if err != nil {
		return nil, err
	}
	return result.Torrents, nil
}

// indexResumedTorrents queries the engine for loaded torrents and builds tracking maps.
func (e *HoardEngine) indexResumedTorrents() {
	result, err := e.client.ListTorrents()
	if err != nil {
		slog.Error("hoard: list torrents failed", "error", err)
		return
	}

	e.mu.Lock()
	defer e.mu.Unlock()

	for _, s := range result.Torrents {
		ih := s.InfoHash
		if _, exists := e.torrents[ih]; exists {
			continue
		}
		e.torrents[ih] = &TorrentInfo{
			InfoHash:  ih,
			Name:      s.Name,
			SavePath:  s.SavePath,
			AddedTime: time.Unix(s.AddedTime, 0),
		}
	}
}

// staggerStart starts all resumed torrents in controlled batches.
func (e *HoardEngine) staggerStart() {
	defer close(e.staggerDone)

	result, err := e.client.ListTorrents()
	if err != nil {
		slog.Error("hoard: stagger list failed", "error", err)
		return
	}

	total := len(result.Torrents)
	if total == 0 {
		return
	}

	// Start is ~free (2 atomic stores in the engine, no verify — see
	// torrent/mod.rs start_torrent), so we activate everything fast: the box
	// serves inbound peers immediately (they find us via our pre-restart
	// tracker announce, still valid ~30min). The announce ramp stays paced by
	// the scheduler (hoardMaxNewPerCycle=500/10s = 50/s, tracker-safe) and just
	// trails in the background without blocking seeding.
	const batchSize = 2000
	const batchDelay = 100 * time.Millisecond

	started := 0
	for i, s := range result.Torrents {
		if err := e.autoStart(s.InfoHash); err != nil {
			slog.Warn("hoard: stagger start failed", "ih", s.InfoHash[:minStr(len(s.InfoHash), 8)], "error", err)
			continue
		}
		started++

		if (i+1)%batchSize == 0 && i+1 < total {
			slog.Info("hoard: stagger start progress",
				"started", started, "total", total,
				"pct", started*100/total,
			)
			select {
			case <-e.ctx.Done():
				return
			case <-time.After(batchDelay):
			}
		}
	}
	slog.Info("hoard: stagger start complete", "started", started, "total", total)
}

// WaitStaggerDone blocks until the stagger start has completed.
func (e *HoardEngine) WaitStaggerDone() {
	<-e.staggerDone
}

// IsStaggerComplete returns true once all hoard torrents have been started.
func (e *HoardEngine) IsStaggerComplete() bool {
	select {
	case <-e.staggerDone:
		return true
	default:
		return false
	}
}

// Stop gracefully shuts down background goroutines.
func (e *HoardEngine) Stop() {
	if !e.running {
		return
	}
	slog.Info("hoard: stopping engine")
	e.cancel()
	e.running = false
	// The engine process is stopped separately by main.go.
	slog.Info("hoard: engine stopped")
}

// ---------------------------------------------------------------------------
// Torrent management
// ---------------------------------------------------------------------------

// AddTorrent loads a .torrent file into the hoard session.
func (e *HoardEngine) AddTorrent(torrentPath, savePath, category string) (string, error) {
	return e.addTorrentInternal(torrentPath, savePath, category, false)
}

// AddTorrentSeedMode loads a .torrent file with seed_mode=true: the engine
// trusts the on-disk payload, skips the hash check, and begins seeding
// immediately. Used by the qBit shim when callers (cross-seed) request
// `skip_checking=true` — the qBit-equivalent of seed_mode.
func (e *HoardEngine) AddTorrentSeedMode(torrentPath, savePath, category string) (string, error) {
	return e.addTorrentSeedMode(torrentPath, savePath, category)
}

func (e *HoardEngine) addTorrentInternal(torrentPath, savePath, category string, silent bool) (string, error) {
	return e.addTorrentInternalWithOpts(torrentPath, savePath, category, silent, false)
}

func (e *HoardEngine) addTorrentSeedMode(torrentPath, savePath, category string) (string, error) {
	return e.addTorrentInternalWithOpts(torrentPath, savePath, category, true, true)
}

func (e *HoardEngine) addTorrentInternalWithOpts(torrentPath, savePath, category string, silent, seedMode bool) (string, error) {
	if !e.running {
		return "", fmt.Errorf("hoard: engine not running")
	}

	torrentBytes, err := os.ReadFile(torrentPath)
	if err != nil {
		return "", fmt.Errorf("hoard: read torrent file: %w", err)
	}

	infoHash, err := infoHashFromTorrentFile(torrentBytes)
	if err != nil {
		return "", fmt.Errorf("hoard: parse info_hash: %w", err)
	}

	// Build save_path with torrent name subfolder.
	multiFile := isMultiFileTorrent(torrentBytes)
	savePathIsContentRoot := false
	if savePath != "" {
		if name := nameFromTorrentFile(torrentBytes); name != "" {
			cleanName := name
			switch strings.ToLower(filepath.Ext(name)) {
			case ".mkv", ".mp4", ".avi", ".wmv", ".flv", ".mov", ".m4v",
				".ts", ".flac", ".mp3", ".epub", ".cbz", ".cbr", ".pdf",
				".iso", ".img", ".nfo", ".srt", ".sub", ".idx", ".rar", ".zip":
				cleanName = strings.TrimSuffix(name, filepath.Ext(name))
			}
			if !seedMode && (multiFile || e.CreateTorrentFolder) && !strings.HasSuffix(savePath, cleanName) && !strings.HasSuffix(savePath, name) {
				savePath = filepath.Join(savePath, cleanName)
			}
			base := filepath.Base(savePath)
			savePathIsContentRoot = base == name || base == cleanName
		}
	}

	// A multi-file torrent gets its own info.name subdir created by the engine
	// inside the engine save_path, so the engine save_path must be the PARENT
	// of the content root.
	//
	// Only strip a level when save_path really IS the content root (the join
	// above just made it one, or the caller passed it that way). A seed-mode
	// add — a cross-seed injection — passes the directory that *contains* the
	// content root and skips the join, so stripping there points the engine one
	// directory too high and it looks for the data where it does not exist.
	engineSavePath := savePath
	if multiFile && savePath != "" && savePathIsContentRoot {
		engineSavePath = filepath.Dir(savePath)
	}

	// Ensure save_path directory exists.
	if engineSavePath != "" {
		os.MkdirAll(engineSavePath, 0755)
	}

	// Add to engine via IPC.
	result, err := e.client.AddTorrentWithOptions(torrentPath, engineSavePath, false, seedMode)
	if err != nil {
		return "", fmt.Errorf("hoard: add torrent: %w", err)
	}

	var cf *bool
	if !seedMode {
		v := multiFile || e.CreateTorrentFolder
		cf = &v
	}
	info := &TorrentInfo{
		InfoHash:        infoHash,
		Name:            result.Name,
		SavePath:        savePath,
		Category:        category,
		AddedTime:       time.Now(),
		TorrentFilePath: torrentPath,
		ContentFolder:   cf,
	}

	e.mu.Lock()
	e.torrents[infoHash] = info
	e.mu.Unlock()

	// Seed the path shape into the stats cache now. Both fields are otherwise
	// only filled by the 60s refreshStats tick, and until then the qBit shim
	// falls back to the Go save_path — which for a multi-file torrent is the
	// content root, so `save_path + name` would double the release directory
	// for any client polling in that window.
	e.cachedStatsMu.Lock()
	st, ok := e.cachedStats[infoHash]
	if !ok {
		st = &TorrentStats{InfoHash: infoHash}
		e.cachedStats[infoHash] = st
	}
	st.MultiFile = multiFile
	st.EngineSavePath = engineSavePath
	e.cachedStatsMu.Unlock()

	if !silent {
		slog.Info("hoard: torrent added", "info_hash", infoHash[:minStr(len(infoHash), 16)])
	}
	// Fresh download: fire one immediate announce so swarm_seeds lands in the
	// slot-manager cache now, letting it rank the torrent correctly instead of
	// parking it at seeds=0 (a parked torrent is never announced -> catch-22).
	if !seedMode && e.bootstrapAnnounce != nil {
		go e.bootstrapAnnounce(infoHash, 0)
	}
	return infoHash, nil
}

// ImportFromState bulk-imports torrents from state.json (migration path).
func (e *HoardEngine) ImportFromState(entries map[string]*TorrentMeta) (imported, skipped, errors int) {
	total := len(entries)
	slog.Info("hoard: importing from state.json", "total", total)

	i := 0
	for _, meta := range entries {
		i++
		if i%200 == 0 {
			slog.Info("hoard: import progress", "done", i, "total", total, "imported", imported, "errors", errors)
			time.Sleep(2 * time.Second)
		}

		if _, err := os.Stat(meta.TorrentFilePath); err != nil {
			skipped++
			continue
		}

		ihRestore, err := e.addTorrentSeedMode(meta.TorrentFilePath, meta.SavePath, meta.Category)
		if err == nil && meta.ContentFolder != nil {
			e.mu.Lock()
			if inf := e.torrents[ihRestore]; inf != nil {
				inf.ContentFolder = meta.ContentFolder
			}
			e.mu.Unlock()
		}
		if err != nil {
			if strings.Contains(err.Error(), "duplicate") {
				if data, rerr := os.ReadFile(meta.TorrentFilePath); rerr == nil {
					if ih, perr := infoHashFromTorrentFile(data); perr == nil {
						e.RestoreMetadata(ih, meta.Category, meta.SavePath, meta.TorrentFilePath, meta.CompletedTime)
						if meta.ContentFolder != nil {
							e.mu.Lock()
							if inf := e.torrents[ih]; inf != nil {
								inf.ContentFolder = meta.ContentFolder
							}
							e.mu.Unlock()
						}
						imported++
						continue
					}
				}
			}
			errors++
			if errors <= 5 {
				slog.Warn("hoard: import error", "torrent", meta.TorrentFilePath, "error", err)
			}
			continue
		}
		imported++
	}

	slog.Info("hoard: import complete", "imported", imported, "skipped", skipped, "errors", errors)
	return
}

// RemoveTorrent removes a torrent from the session.
func (e *HoardEngine) RemoveTorrent(infoHash string, deleteFiles bool) {
	// Absorb stats before removal so global totals stay correct. Fallback
	// on cachedStats when GetStatus fails (race with Typhon-side drops) —
	// sinon session_uploaded saigne au remove.
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
			slog.Debug("hoard: GetStatus failed at remove, using cachedStats fallback",
				"info_hash", infoHash, "err", err, "ul", ul, "dl", dl)
		}
		e.onBeforeRemove(infoHash, ul, dl)
	}

	e.mu.Lock()
	_, exists := e.torrents[infoHash]
	if !exists {
		e.mu.Unlock()
		slog.Warn("hoard: torrent not found for removal", "info_hash", infoHash)
		return
	}
	delete(e.torrents, infoHash)
	e.mu.Unlock()

	keepData := !deleteFiles
	if err := e.client.RemoveTorrent(infoHash, keepData); err != nil {
		slog.Error("hoard: error removing torrent", "info_hash", infoHash, "error", err)
	}

	e.cachedStatsMu.Lock()
	delete(e.cachedStats, infoHash)
	e.cachedStatsMu.Unlock()
	forgetTrackerObs(infoHash)

	slog.Info("hoard: torrent removed", "info_hash", infoHash, "delete_files", deleteFiles)
}

// AddTrackerToTorrent is not directly supported via current IPC — no-op for now.
func (e *HoardEngine) AddTrackerToTorrent(infoHash, url string) error {
	// TODO: add set_trackers command to hydra-engine
	slog.Warn("hoard: AddTrackerToTorrent not yet implemented in libtorrent engine")
	return nil
}

// LivePort exposes the runtime listen-port override atomic so the hoard
// announcer sends the current port to trackers after a hot rebind.
func (e *HoardEngine) LivePort() *atomic.Int64 { return &e.livePort }

// SetListenPort hot-rebinds the engine peer listener + updates the announce
// port, with no restart. No-op for a remote (non-ltclient) engine client.
// SetSelfIPs pushes our current public IP(s) to the engine self-dial filter
// (dynamic; no-op for a remote non-ltclient engine).
func (e *HoardEngine) SetSelfIPs(ips []string) {
	lt, ok := e.client.(*ltclient.Client)
	if !ok {
		return
	}
	if err := lt.SetSelfIPs(ips); err != nil {
		slog.Warn("hoard: set_self_ips failed", "err", err)
	}
}

func (e *HoardEngine) SetListenPort(port int) {
	if port <= 0 || port > 65535 {
		slog.Warn("hoard: SetListenPort out of range", "port", port)
		return
	}
	lt, ok := e.client.(*ltclient.Client)
	if !ok {
		slog.Warn("hoard: SetListenPort unsupported on this client", "port", port)
		return
	}
	if err := lt.SetListenPort(port); err != nil {
		slog.Warn("hoard: engine listen-port rebind failed", "port", port, "err", err)
		return
	}
	e.config.ListenPort = port
	e.livePort.Store(int64(port))
	slog.Info("hoard: listen port hot-swapped", "port", port)
}

// PauseAll stops all torrents in the session.
func (e *HoardEngine) PauseAll() int {
	result, err := e.client.ListTorrents()
	if err != nil {
		return 0
	}
	count := 0
	for _, s := range result.Torrents {
		if !s.IsPaused {
			if err := e.client.StopTorrent(s.InfoHash); err == nil {
				count++
			}
		}
	}
	slog.Info("hoard: paused all torrents", "count", count)
	return count
}

// ResumeAll starts all torrents in the session.
func (e *HoardEngine) ResumeAll() int {
	result, err := e.client.ListTorrents()
	if err != nil {
		return 0
	}
	count := 0
	for _, s := range result.Torrents {
		if s.IsPaused {
			if err := e.autoStart(s.InfoHash); err == nil {
				count++
			}
		}
	}
	slog.Info("hoard: resumed all torrents", "count", count)
	return count
}

// GetTorrentULDL returns the cumulative uploaded/downloaded bytes for a torrent.
func (e *HoardEngine) GetTorrentULDL(infoHash string) (ul, dl int64) {
	s, err := e.client.GetStatus(infoHash)
	if err != nil {
		return 0, 0
	}
	return s.TotalUpload, s.TotalDownload
}

// RestoreMetadata updates category, save_path etc. for a torrent loaded from resume data.
func (e *HoardEngine) RestoreMetadata(infoHash, category, savePath, torrentFilePath string, completedTime time.Time) {
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
	if !completedTime.IsZero() && info.CompletedTime.IsZero() {
		info.CompletedTime = completedTime
	}
}

// GetCategories returns a map of info_hash -> category.
func (e *HoardEngine) GetCategories() map[string]string {
	e.mu.RLock()
	defer e.mu.RUnlock()
	cats := make(map[string]string, len(e.torrents))
	for ih, info := range e.torrents {
		if info.Category != "" {
			cats[ih] = info.Category
		}
	}
	return cats
}

// ---------------------------------------------------------------------------
// Data access (thread-safe, served from cache)
// ---------------------------------------------------------------------------

func (e *HoardEngine) TorrentCount() int {
	e.mu.RLock()
	defer e.mu.RUnlock()
	return len(e.torrents)
}

func (e *HoardEngine) SetContentFolder(infoHash string, cf *bool) {
	if cf == nil {
		return
	}
	e.mu.Lock()
	defer e.mu.Unlock()
	if inf := e.torrents[infoHash]; inf != nil {
		inf.ContentFolder = cf
	}
}

func (e *HoardEngine) GetTorrentMetas() map[string]*TorrentMeta {
	e.mu.RLock()
	defer e.mu.RUnlock()
	metas := make(map[string]*TorrentMeta, len(e.torrents))
	for ih, info := range e.torrents {
		metas[ih] = &TorrentMeta{
			SavePath:        info.SavePath,
			TorrentFilePath: info.TorrentFilePath,
			Category:        info.Category,
			CompletedTime:   info.CompletedTime,
			ContentFolder:   info.ContentFolder,
			UserPaused:      info.UserPaused,
			Tags:            info.Tags,
		}
	}
	return metas
}

func (e *HoardEngine) GetTorrentList() []TorrentStats {
	// Build directly from cachedStats on each call so push events
	// (stats_snapshot updates cachedStats in-place) are immediately
	// visible via the HTTP API — no cachedTorrentList staleness.
	// O(N) copy per call is acceptable: ~650 KB at 13k torrents,
	// HTTP polls ~1/s from the UI.
	e.cachedStatsMu.RLock()
	defer e.cachedStatsMu.RUnlock()
	list := make([]TorrentStats, 0, len(e.cachedStats))
	for _, s := range e.cachedStats {
		list = append(list, *s)
	}
	return list
}

// GetSessionTotals sums UL/DL over the typed stats cache directly — no map
// materialization (called ~1Hz for /api/status session stats).
func (e *HoardEngine) GetSessionTotals() (ul, dl int64) {
	e.cachedStatsMu.RLock()
	defer e.cachedStatsMu.RUnlock()
	for _, s := range e.cachedStats {
		ul += s.TotalUpload
		dl += s.TotalDownload
	}
	return
}

func (e *HoardEngine) GetTorrentDetail(infoHash string) map[string]interface{} {
	e.mu.RLock()
	info, exists := e.torrents[infoHash]
	e.mu.RUnlock()
	if !exists {
		return nil
	}

	s, err := e.client.GetStatus(infoHash)
	if err != nil {
		return (&TorrentDetail{
			TorrentStats: TorrentStats{
				InfoHash: infoHash, Name: info.Name,
				SavePath: info.SavePath, Category: info.Category,
			},
			Peers: []PeerInfo{}, Trackers: []TrackerInfo{},
		}).ToMap()
	}

	stats := ltStatusToTorrentStats(*s, info.Category, info.SavePath, info.AddedTime, info.CompletedTime)

	// Get peers.
	ltPeers, _ := e.client.GetPeers(infoHash)
	peers := make([]PeerInfo, 0, len(ltPeers))
	for _, p := range ltPeers {
		peers = append(peers, ltPeerToPeerInfo(p))
	}

	detail := &TorrentDetail{
		TorrentStats: stats,
		Peers:        peers,
		NumPieces:    s.NumPieces,
		PieceLength:  s.PieceLength,
		SeedingTime:  s.SeedingTime,
		ActiveTime:   s.ActiveTime,
	}

	m := detail.ToMap()

	// Inject raw tracker data from engine, but overwrite each endpoint's
	// last_error/message/scrape_* using the cachedTrackerObs we filled
	// from the Go-canonical announce loop. Typhon's tracker state is stale
	// because internal_announce is off — see feedback_announce_propagation_pattern.md.
	if trackers, err := e.client.GetTrackers(infoHash); err == nil {
		obsByURL := trackerObsFor(infoHash)
		trackerMaps := make([]map[string]interface{}, 0, len(trackers))
		for _, t := range trackers {
			tm := map[string]interface{}{
				"url":      t.URL,
				"tier":     t.Tier,
				"verified": t.Verified,
			}
			var endpoints []map[string]interface{}
			if len(t.Endpoints) > 0 {
				_ = json.Unmarshal(t.Endpoints, &endpoints)
			}
			if obs, ok := obsByURL[t.URL]; ok {
				if len(endpoints) == 0 {
					endpoints = []map[string]interface{}{{}}
				}
				for i := range endpoints {
					if obs.OK {
						endpoints[i]["last_error"] = "Success"
						endpoints[i]["message"] = ""
						endpoints[i]["scrape_complete"] = obs.Seeds
						endpoints[i]["scrape_incomplete"] = obs.Leechers
					} else {
						endpoints[i]["last_error"] = obs.ErrorMsg
						endpoints[i]["message"] = obs.ErrorMsg
					}
					if !obs.NextAt.IsZero() {
						secs := int64(time.Until(obs.NextAt).Seconds())
						if secs < 0 {
							secs = 0
						}
						endpoints[i]["next_announce"] = secs
					}
				}
			}
			tm["endpoints"] = endpoints
			trackerMaps = append(trackerMaps, tm)
		}
		m["trackers"] = trackerMaps
	}

	// Merge top-level swarm/tracker_error fields from cachedStats.
	e.cachedStatsMu.RLock()
	if cs, ok := e.cachedStats[infoHash]; ok {
		m["tracker_error"] = cs.TrackerError
		m["tracker_error_msg"] = cs.TrackerErrorMsg
		m["swarm_seeds"] = cs.SwarmSeeds
		m["swarm_leechers"] = cs.SwarmLeechers
		m["is_announced"] = cs.IsAnnounced
	}
	e.cachedStatsMu.RUnlock()

	return m
}

func (e *HoardEngine) GetAllStatus() map[string]interface{} {
	e.mu.RLock()
	count := len(e.torrents)
	// During a boot-from-store import, report the store's real total (known
	// up-front) instead of the count that climbs as e.torrents fills.
	if exp := int(e.expectedTotal.Load()); exp > count {
		count = exp
	}
	e.mu.RUnlock()

	// Aggregate from cachedStats (live via push events, updated ~1s) instead
	// of cachedTorrentList (rebuilt every 60s = stale for UL/DL). Also sums
	// upload/download rates + session totals on the fly so /api/status returns
	// sub-second-fresh numbers to the UI without round-tripping through the
	// 60s backstop poll.
	e.cachedStatsMu.RLock()
	torrentsWithPeers := 0
	torrentsUploading := 0
	torrentsAnnounced := 0
	totalPeers := 0
	totalSwarmLeechers := 0
	var totalUlRate, totalDlRate, totalSessionUl, totalSessionDl int64
	for _, ts := range e.cachedStats {
		totalPeers += ts.NumPeers
		totalSwarmLeechers += ts.SwarmLeechers
		totalUlRate += ts.UploadRate
		totalDlRate += ts.DownloadRate
		totalSessionUl += ts.TotalUpload
		totalSessionDl += ts.TotalDownload
		if ts.NumPeers > 0 {
			torrentsWithPeers++
		}
		if ts.UploadRate > 0 {
			torrentsUploading++
		}
		if ts.IsAnnounced {
			torrentsAnnounced++
		}
	}
	e.cachedStatsMu.RUnlock()

	status := map[string]interface{}{
		"engine":              "hoard",
		"running":             e.running,
		"total_torrents":      count,
		"listen_port":         e.config.ListenPort,
		"torrents_with_peers": torrentsWithPeers,
		"torrents_uploading":  torrentsUploading,
		"torrents_announced":  torrentsAnnounced,
		"swarm_leechers":      totalSwarmLeechers,
		"stagger_complete":    e.IsStaggerComplete(),
	}

	// Use live aggregates from cachedStats. Fall back on the 60s-stale
	// cachedSessionStats only for unseeded_peers which we don't compute
	// per-torrent yet.
	status["active_peers"] = totalPeers
	status["active_upload_rate"] = totalUlRate
	status["active_download_rate"] = totalDlRate
	status["session_uploaded"] = totalSessionUl
	status["session_downloaded"] = totalSessionDl
	e.cachedSessionStatsMu.RLock()
	if e.cachedSessionStats != nil {
		status["unseeded_peers"] = e.cachedSessionStats.UnseededPeers
	}
	e.cachedSessionStatsMu.RUnlock()

	e.cachedDiagnosticsMu.RLock()
	if e.cachedDiagnostics != nil {
		status["diagnostics"] = e.cachedDiagnostics
	}
	e.cachedDiagnosticsMu.RUnlock()

	return status
}

// ---------------------------------------------------------------------------
// Background goroutines
// ---------------------------------------------------------------------------

func (e *HoardEngine) statsRefreshLoop() {
	ticker := time.NewTicker(hoardStatsInterval)
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

func (e *HoardEngine) torrentListRefreshLoop() {
	ticker := time.NewTicker(hoardTorrentListInterval)
	defer ticker.Stop()
	for {
		select {
		case <-e.ctx.Done():
			return
		case <-ticker.C:
			e.refreshTorrentList()
		}
	}
}

func (e *HoardEngine) refreshStats() {
	// Session stats.
	ss, err := e.client.GetSessionStats()
	if err == nil {
		e.cachedSessionStatsMu.Lock()
		e.cachedSessionStats = ss
		e.cachedSessionStatsMu.Unlock()
	}

	// Diagnostics (every refresh cycle).
	diag, diagErr := e.client.GetDiagnostics()
	if diagErr == nil {
		e.cachedDiagnosticsMu.Lock()
		e.cachedDiagnostics = diag
		e.cachedDiagnosticsMu.Unlock()
	}

	// Per-torrent stats.
	result, err := e.client.ListTorrents()
	if err != nil {
		return
	}

	newStats := make(map[string]*TorrentStats, len(result.Torrents))
	for _, s := range result.Torrents {
		ih := s.InfoHash

		e.mu.RLock()
		info := e.torrents[ih]
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

		stats := ltStatusToTorrentStats(s, category, savePath, addedTime, completedTime)
		if info != nil {
			stats.ContentFolder = info.ContentFolder
			stats.Tags = info.Tags
			stats.UserPaused = info.UserPaused
			// The state was derived before the intent was known -- redo it, or
			// a stopped torrent reads "queued" again after the next refresh.
			stats.State = DeriveState(stats.State, stats.UserPaused)
		}

		if stats.Progress >= 1.0 && completedTime.IsZero() && info != nil {
			now := time.Now()
			e.mu.Lock()
			info.CompletedTime = now
			e.mu.Unlock()
			stats.CompletedTime = now.Unix()
		}

		newStats[ih] = &stats
	}

	e.cachedStatsMu.Lock()
	// Preserve announce-derived fields (populated by HoardAnnouncer.ObserveAnnounce)
	// across the periodic refresh — Typhon's internal announce loop is disabled
	// so ltStatusToTorrentStats returns zero for these and would clobber our
	// Go-canonical values otherwise.
	for ih, ns := range newStats {
		if old, ok := e.cachedStats[ih]; ok {
			ns.IsAnnounced = old.IsAnnounced
			ns.SwarmSeeds = old.SwarmSeeds
			ns.SwarmLeechers = old.SwarmLeechers
			ns.TrackerError = old.TrackerError
			ns.TrackerErrorMsg = old.TrackerErrorMsg
			if ns.Category == "" {
				ns.Category = old.Category
			}
			if ns.Tags == nil {
				ns.Tags = old.Tags
			}
		}
	}
	e.cachedStats = newStats
	e.cachedStatsMu.Unlock()
}

func (e *HoardEngine) refreshTorrentList() {
	e.cachedStatsMu.RLock()
	list := make([]TorrentStats, 0, len(e.cachedStats))
	for _, s := range e.cachedStats {
		list = append(list, *s)
	}
	e.cachedStatsMu.RUnlock()

	e.cachedStatsMu.Lock()
	e.cachedTorrentList = list
	e.cachedStatsMu.Unlock()
}

// GetTorrentFiles returns the file list for a torrent.
func (e *HoardEngine) GetTorrentFiles(infoHash string) []string {
	files, err := e.client.GetFiles(infoHash)
	if err != nil || len(files) == 0 {
		// Fallback: walk save_path
		e.mu.RLock()
		info, exists := e.torrents[infoHash]
		e.mu.RUnlock()
		if !exists || info.SavePath == "" {
			return nil
		}
		var result []string
		filepath.Walk(info.SavePath, func(path string, fi os.FileInfo, err error) error {
			if err != nil || fi.IsDir() {
				return nil
			}
			rel, rerr := filepath.Rel(info.SavePath, path)
			if rerr == nil {
				result = append(result, rel)
			}
			return nil
		})
		return result
	}

	result := make([]string, len(files))
	for i, f := range files {
		result[i] = f.Path
	}
	return result
}

// GetTorrentFileList returns the file list for a torrent as path/size pairs.
// Paths are BEP-3 relative: relative to the info.name directory for a
// multi-file torrent, and equal to info.name for a single-file one.
func (e *HoardEngine) GetTorrentFileList(infoHash string) []map[string]interface{} {
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

// GetTorrentAvailability reports how many copies of each piece the swarm holds.
// Nil when the engine does not know: a seed-mode torrent has no piece map at
// all, which the caller must present as "unknown", not as zero.
func (e *HoardEngine) GetTorrentAvailability(infoHash string) map[string]interface{} {
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

// ---------------------------------------------------------------------------
// Verify throttle
// ---------------------------------------------------------------------------

const (
	maxConcurrentVerify    = 5
	verifyThrottleInterval = 10 * time.Second
)

func (e *HoardEngine) verifyThrottle() {
	e.WaitStaggerDone()
	select {
	case <-e.ctx.Done():
		return
	case <-time.After(5 * time.Second):
	}

	e.stopAllVerifying()

	ticker := time.NewTicker(verifyThrottleInterval)
	defer ticker.Stop()
	for {
		select {
		case <-e.ctx.Done():
			return
		case <-ticker.C:
			remaining := e.manageVerifyBatches()
			if remaining == 0 {
				slog.Info("hoard: verify throttle — all verified, exiting")
				return
			}
		}
	}
}

func (e *HoardEngine) stopAllVerifying() {
	result, err := e.client.ListTorrents()
	if err != nil {
		return
	}
	count := 0
	for _, s := range result.Torrents {
		if s.State == "checking_files" {
			e.client.StopTorrent(s.InfoHash)
			count++
		}
	}
	if count > 0 {
		slog.Info("hoard: verify throttle — stopped all verifiers", "count", count)
	}
}

func (e *HoardEngine) manageVerifyBatches() int {
	result, err := e.client.ListTorrents()
	if err != nil {
		return 0
	}

	var activeVerify int
	var parked []string // info_hashes of parked torrents

	for _, s := range result.Torrents {
		switch s.State {
		case "checking_files":
			activeVerify++
		case "paused":
			e.slotParkedMu.RLock()
			isSlotParked := e.slotParked[s.InfoHash]
			e.slotParkedMu.RUnlock()
			if isSlotParked {
				continue
			}
			// Needs verify: paused, no progress, has content
			if s.Progress == 0 && s.TotalSize > 0 {
				parked = append(parked, s.InfoHash)
			}
		}
	}

	slots := maxConcurrentVerify - activeVerify
	if slots <= 0 || len(parked) == 0 {
		if len(parked) > 0 {
			slog.Info("hoard: verify throttle — waiting", "active", activeVerify, "parked", len(parked))
		}
		return len(parked)
	}
	if slots > len(parked) {
		slots = len(parked)
	}

	slog.Info("hoard: verify throttle — starting batch", "starting", slots, "active", activeVerify, "remaining", len(parked)-slots)
	for i := 0; i < slots; i++ {
		e.autoStart(parked[i])
	}
	return len(parked) - slots
}

func (e *HoardEngine) RestartStuckVerifying() int {
	result, err := e.client.ListTorrents()
	if err != nil {
		return 0
	}
	count := 0
	for _, s := range result.Torrents {
		if s.State == "checking_files" {
			e.client.StopTorrent(s.InfoHash)
			count++
		}
	}
	if count > 0 {
		slog.Info("hoard: restart-stuck — stopped verifying", "count", count)
	}
	return count
}

func (e *HoardEngine) VerifyTorrent(infoHash string) error {
	return e.client.VerifyTorrent(infoHash)
}

func (e *HoardEngine) VerifyDownloading() int {
	result, err := e.client.ListTorrents()
	if err != nil {
		return 0
	}
	count := 0
	for _, s := range result.Torrents {
		if s.State == "downloading" && s.DownloadRate == 0 {
			e.client.VerifyTorrent(s.InfoHash)
			count++
		}
	}
	slog.Info("hoard: verify-downloading triggered", "count", count)
	return count
}

// ---------------------------------------------------------------------------
// Download Slot Manager (same logic as before, adapted for ltclient)
// ---------------------------------------------------------------------------

const (
	downloadSlotInterval       = 30 * time.Second
	progressEvalWindow         = 90 * time.Second
	progressMinBytes     int64 = 256 * 1024

	// A torrent that just got a slot keeps it for at least this long before the
	// ranking is allowed to evict it. Without this, the probe quota handed out
	// slots that lasted exactly one tick: the probe sort is oldest-LastTry-first
	// and taking a slot stamps LastTry=now, so any never-tried torrent outranked
	// the torrent that had just started. 30s of residency is not enough to
	// connect to a swarm, so nothing in the probe pool could ever finish — and
	// the progress check then punished them for the stall it had caused.
	slotMinResidency = 5 * time.Minute

	// Per-tick ceiling on evictions, as a fraction of maxSlots. Stop/start drops
	// every peer connection and restarts the ramp from zero, so a slot pool that
	// turns over faster than it connects downloads nothing. Observed in prod
	// before the sticky rule below: 1800 of 2000 slots swapped per 30s tick.
	// This is a backstop — if the ranking ever churns again, it churns slowly.
	slotMaxEvictFrac = 20

	// Bytes/s below which an active torrent counts as making no progress. Kept
	// consistent with progressMinBytes/progressEvalWindow (~2.9 KB/s), and used
	// as an escape hatch: total_done is quantised to piece_length engine-side
	// (typhon dispatch.rs torrent_to_json), so a genuine download spread across
	// several large partial pieces can report delta=0 over a whole window.
	progressMinRate = 3000
)

var slotCooldownLevels = []time.Duration{
	1 * time.Minute,
	5 * time.Minute,
	15 * time.Minute,
	1 * time.Hour,
}

type slotProgressInfo struct {
	unparkAt      time.Time
	bytesAtUnpark int64
	cooldownUntil time.Time
	backoffLevel  int
}

func (e *HoardEngine) GetEffectiveMaxDownloads() int {
	e.dlSlotsOverrideMu.Lock()
	defer e.dlSlotsOverrideMu.Unlock()
	if e.dlSlotsOverride >= 0 {
		return e.dlSlotsOverride
	}
	return e.config.ActiveDownloads
}

func (e *HoardEngine) SetDownloadSlotsOverride(max int) {
	e.dlSlotsOverrideMu.Lock()
	e.dlSlotsOverride = max
	e.dlSlotsOverrideMu.Unlock()
	slog.Info("hoard: download slots override set", "max", max)
}

func (e *HoardEngine) ClearDownloadSlotsOverride() {
	e.dlSlotsOverrideMu.Lock()
	e.dlSlotsOverride = -1
	e.dlSlotsOverrideMu.Unlock()
	slog.Info("hoard: download slots override cleared")
}

// PinTorrent marks a torrent as pinned (see the `pinned` field doc). Idempotent.
func (e *HoardEngine) PinTorrent(infoHash string) {
	e.pinnedMu.Lock()
	if e.pinned[infoHash] {
		e.pinnedMu.Unlock()
		return
	}
	e.pinned[infoHash] = true
	e.pinnedMu.Unlock()
	e.savePinned()
	// Drop any progress/cooldown state so the next enforce tick can hand it a
	// slot immediately instead of waiting out a backoff from before the pin.
	e.slotProgressMu.Lock()
	delete(e.slotProgress, infoHash)
	e.slotProgressMu.Unlock()
	short := infoHash
	if len(short) > 8 {
		short = short[:8]
	}
	slog.Info("hoard: torrent pinned", "info_hash", short)
}

// UnpinTorrent removes a pin. Idempotent.
func (e *HoardEngine) UnpinTorrent(infoHash string) {
	e.pinnedMu.Lock()
	if !e.pinned[infoHash] {
		e.pinnedMu.Unlock()
		return
	}
	delete(e.pinned, infoHash)
	e.pinnedMu.Unlock()
	e.savePinned()
	short := infoHash
	if len(short) > 8 {
		short = short[:8]
	}
	slog.Info("hoard: torrent unpinned", "info_hash", short)
}

// IsPinned reports whether a torrent is pinned.
func (e *HoardEngine) IsPinned(infoHash string) bool {
	e.pinnedMu.RLock()
	defer e.pinnedMu.RUnlock()
	return e.pinned[infoHash]
}

// PinnedList returns the info_hashes of all pinned torrents.
func (e *HoardEngine) PinnedList() []string {
	e.pinnedMu.RLock()
	defer e.pinnedMu.RUnlock()
	out := make([]string, 0, len(e.pinned))
	for ih := range e.pinned {
		out = append(out, ih)
	}
	return out
}

func (e *HoardEngine) pinnedPath() string {
	return filepath.Join(e.dataDir, "hoard_pinned.json")
}

// loadPinned restores pins from disk (best-effort; called at construction).
func (e *HoardEngine) loadPinned() {
	data, err := os.ReadFile(e.pinnedPath())
	if err != nil {
		return
	}
	var list []string
	if err := json.Unmarshal(data, &list); err != nil {
		slog.Warn("hoard: hoard_pinned.json parse failed", "err", err)
		return
	}
	for _, ih := range list {
		e.pinned[ih] = true
	}
	if len(list) > 0 {
		slog.Info("hoard: restored pinned torrents", "count", len(list))
	}
}

// savePinned writes pins to disk atomically (best-effort).
func (e *HoardEngine) savePinned() {
	e.pinnedMu.RLock()
	list := make([]string, 0, len(e.pinned))
	for ih := range e.pinned {
		list = append(list, ih)
	}
	e.pinnedMu.RUnlock()
	data, err := json.Marshal(list)
	if err != nil {
		return
	}
	tmp := e.pinnedPath() + ".tmp"
	if err := os.WriteFile(tmp, data, 0644); err != nil {
		slog.Warn("hoard: hoard_pinned.json write failed", "err", err)
		return
	}
	_ = os.Rename(tmp, e.pinnedPath())
}

func (e *HoardEngine) GetDownloadSlotStatus() DownloadSlotStats {
	e.lastSlotStatsMu.RLock()
	defer e.lastSlotStatsMu.RUnlock()
	return e.lastSlotStats
}

func (e *HoardEngine) downloadSlotManager() {
	e.WaitStaggerDone()
	select {
	case <-e.ctx.Done():
		return
	case <-time.After(30 * time.Second):
	}

	slog.Info("hoard: download slot manager started", "max", e.GetEffectiveMaxDownloads())

	ticker := time.NewTicker(downloadSlotInterval)
	defer ticker.Stop()
	for {
		select {
		case <-e.ctx.Done():
			return
		case <-ticker.C:
			e.enforceDownloadSlots()
		}
	}
}

func (e *HoardEngine) enforceDownloadSlots() {
	maxSlots := e.GetEffectiveMaxDownloads()
	if maxSlots < 0 {
		return
	}

	result, err := e.client.ListTorrents()
	if err != nil {
		return
	}

	now := time.Now()

	// Snapshot tracker-scrape seed counts from the announce cache. The
	// engine list_torrents RPC carries no swarm data, and ListSeeds is
	// connected-only (0 on parked torrents), which made slot priority
	// effectively random. cachedStats is fed by ObserveAnnounce.
	swarmSeeds := make(map[string]int)
	e.cachedStatsMu.RLock()
	for ih, cs := range e.cachedStats {
		swarmSeeds[ih] = cs.SwarmSeeds
	}
	e.cachedStatsMu.RUnlock()

	// Classify torrents
	type dlTorrent struct {
		InfoHash string
		Seeds    int
		Active   bool
		Bytes    int64
		Rate     int
	}

	var incomplete []dlTorrent
	var actives []string
	byHash := make(map[string]int, len(result.Torrents))

	for _, s := range result.Torrents {
		if s.IsFinished || s.Progress >= 1.0 {
			continue
		}
		isActive := !s.IsPaused && s.State == "downloading"
		byHash[s.InfoHash] = len(incomplete)
		incomplete = append(incomplete, dlTorrent{
			InfoHash: s.InfoHash,
			Seeds:    swarmSeeds[s.InfoHash], // tracker-scrape seeds from announce cache
			Active:   isActive,
			Bytes:    s.TotalDone,
			Rate:     s.DownloadRate,
		})
		if isActive {
			actives = append(actives, s.InfoHash)
		}
	}

	// Phase 1: Progress check for active torrents.
	//
	// This also produces `sticky`: the torrents that hold a slot and are not
	// failing the progress rule. Phase 2 hands those their slot back before it
	// ranks anything, which is the whole point — the ranking below sorts on
	// tracker-scrape seed counts and has no idea whether a torrent is currently
	// downloading. Without the sticky pass, a torrent pulling 8 MB/s competed
	// on equal footing with 18k parked ones, and lost whenever the (heavily
	// tied) seed sort reshuffled.
	sticky := make(map[string]bool, len(actives))
	demoted := make(map[string]bool)
	e.slotProgressMu.Lock()
	var activityDemoted int
	for _, ih := range actives {
		// Pinned torrents are exempt from activity-based demotion.
		if e.IsPinned(ih) {
			sticky[ih] = true
			continue
		}
		idx, ok := byHash[ih]
		if !ok {
			continue
		}
		t := incomplete[idx]
		pi, exists := e.slotProgress[ih]
		if !exists {
			e.slotProgress[ih] = &slotProgressInfo{
				unparkAt:      now,
				bytesAtUnpark: t.Bytes,
			}
			sticky[ih] = true
			continue
		}
		if now.Before(pi.unparkAt.Add(progressEvalWindow)) {
			// Still inside its evaluation window: too early to judge, so it
			// keeps the slot. A torrent needs time to announce and connect.
			sticky[ih] = true
			continue
		}
		delta := t.Bytes - pi.bytesAtUnpark
		if delta < progressMinBytes && t.Rate < progressMinRate {
			// No progress — demote with cooldown
			level := pi.backoffLevel
			if level >= len(slotCooldownLevels) {
				level = len(slotCooldownLevels) - 1
			}
			pi.cooldownUntil = now.Add(slotCooldownLevels[level])
			pi.backoffLevel++
			e.client.StopTorrent(ih)
			e.slotParkedMu.Lock()
			e.slotParked[ih] = true
			e.slotParkedMu.Unlock()
			demoted[ih] = true
			activityDemoted++
		} else {
			// Good progress — reset window, decay backoff
			pi.unparkAt = now
			pi.bytesAtUnpark = t.Bytes
			if pi.backoffLevel > 0 {
				pi.backoffLevel--
			}
			sticky[ih] = true
		}
	}
	e.slotProgressMu.Unlock()

	// Phase 2: Build target set (top maxSlots by seeds desc, excluding cooldowns)
	if maxSlots == 0 {
		// Stop all
		for _, ih := range actives {
			e.client.StopTorrent(ih)
			e.slotParkedMu.Lock()
			e.slotParked[ih] = true
			e.slotParkedMu.Unlock()
		}
		return
	}

	// Filter eligible. Carry lastTry (when the torrent last held a slot) so
	// the probe quota below can rotate fairly: zero time = never given a slot.
	type candidate struct {
		InfoHash string
		Seeds    int
		LastTry  time.Time
	}
	var eligible []candidate
	e.slotProgressMu.Lock()
	for _, t := range incomplete {
		pi := e.slotProgress[t.InfoHash]
		if pi != nil && now.Before(pi.cooldownUntil) {
			continue
		}
		var lastTry time.Time
		if pi != nil {
			lastTry = pi.unparkAt
		}
		eligible = append(eligible, candidate{t.InfoHash, t.Seeds, lastTry})
	}
	e.slotProgressMu.Unlock()

	// Sort by seeds descending (most seeded = fastest to complete), info_hash
	// breaking ties. The tiebreak is not cosmetic: most of the pool sits at the
	// same seed count (0 for anything the announce cache has never seen), and
	// an unstable sort over a huge tie group reshuffles the "top N" on every
	// tick — which turned the slot pool into a lottery redrawn every 30s.
	sort.SliceStable(eligible, func(i, j int) bool {
		if eligible[i].Seeds != eligible[j].Seeds {
			return eligible[i].Seeds > eligible[j].Seeds
		}
		return eligible[i].InfoHash < eligible[j].InfoHash
	})

	targetSet := make(map[string]bool)

	// Pinned torrents always hold a slot — ahead of the seed-rank quota and
	// regardless of cooldown. They count against maxSlots.
	for _, t := range incomplete {
		if e.IsPinned(t.InfoHash) {
			targetSet[t.InfoHash] = true
		}
	}

	// Sticky: a torrent that already holds a slot and is not failing the
	// progress rule keeps it, ahead of both quotas. Also honour a minimum
	// residency for torrents that only just got a slot, so the probe rotation
	// below cannot evict them before they have had a chance to connect.
	e.slotProgressMu.Lock()
	for _, t := range incomplete {
		if len(targetSet) >= maxSlots {
			break
		}
		if demoted[t.InfoHash] {
			continue
		}
		young := false
		if pi := e.slotProgress[t.InfoHash]; pi != nil && t.Active {
			young = now.Before(pi.unparkAt.Add(slotMinResidency))
		}
		if sticky[t.InfoHash] || young {
			targetSet[t.InfoHash] = true
		}
	}
	e.slotProgressMu.Unlock()

	// Main quota: the most-seeded eligible torrents (fastest to finish).
	// Probe quota: reserve a slice for the longest-waiting torrents so the
	// seed-sort can't starve those that never managed an initial announce
	// (their swarm_seeds stays 0 in the announce cache, sorting them last
	// forever — a catch-22 that froze ~13k Sharewood adds on a dial-timeout).
	probeQuota := maxSlots / 5
	mainQuota := maxSlots - probeQuota
	for i := 0; i < len(eligible) && len(targetSet) < mainQuota; i++ {
		targetSet[eligible[i].InfoHash] = true
	}
	if probeQuota > 0 {
		remaining := make([]candidate, 0, len(eligible))
		for _, c := range eligible {
			if !targetSet[c.InfoHash] {
				remaining = append(remaining, c)
			}
		}
		// Oldest LastTry first (zero = never tried = highest probe priority).
		sort.Slice(remaining, func(i, j int) bool {
			return remaining[i].LastTry.Before(remaining[j].LastTry)
		})
		for i := 0; i < len(remaining) && len(targetSet) < maxSlots; i++ {
			targetSet[remaining[i].InfoHash] = true
		}
	}

	// Phase 2b: bound per-tick turnover. Demotions are exempt (the progress rule
	// already justified those); this only caps rank-driven eviction, so a future
	// ranking bug degrades throughput instead of collapsing it.
	maxEvict := maxSlots / slotMaxEvictFrac
	if maxEvict < 1 {
		maxEvict = 1
	}
	var evictees, newcomers []dlTorrent
	for _, t := range incomplete {
		switch {
		case targetSet[t.InfoHash] && !t.Active && !e.IsPinned(t.InfoHash):
			newcomers = append(newcomers, t)
		case !targetSet[t.InfoHash] && t.Active && !demoted[t.InfoHash]:
			evictees = append(evictees, t)
		}
	}
	// A reprieve is a SWAP, never a widening: each evictee kept costs one
	// newcomer, so the pool size is unchanged and the cap can only slow churn
	// down, never overshoot maxSlots. Capping the reprieve at len(newcomers) is
	// what enforces that — and it matters most right after boot, where stagger
	// start has every incomplete torrent running, there are no newcomers at all,
	// and the pool has to shed thousands of slots in one tick to reach maxSlots.
	if reprieve := len(evictees) - maxEvict; reprieve > 0 && len(newcomers) > 0 {
		if reprieve > len(newcomers) {
			reprieve = len(newcomers)
		}
		// Keep the most-seeded evictees, drop the least-seeded newcomers.
		sort.SliceStable(evictees, func(i, j int) bool { return evictees[i].Seeds > evictees[j].Seeds })
		sort.SliceStable(newcomers, func(i, j int) bool { return newcomers[i].Seeds < newcomers[j].Seeds })
		for i := 0; i < reprieve; i++ {
			targetSet[evictees[i].InfoHash] = true
			delete(targetSet, newcomers[i].InfoHash)
		}
	}

	// Phase 3: Transitions
	started, stopped := 0, 0
	for _, t := range incomplete {
		shouldBeActive := targetSet[t.InfoHash]
		if shouldBeActive && !t.Active {
			e.autoStart(t.InfoHash)
			e.slotParkedMu.Lock()
			delete(e.slotParked, t.InfoHash)
			e.slotParkedMu.Unlock()
			// Init progress tracking
			e.slotProgressMu.Lock()
			e.slotProgress[t.InfoHash] = &slotProgressInfo{
				unparkAt:      now,
				bytesAtUnpark: t.Bytes,
			}
			e.slotProgressMu.Unlock()
			started++
		} else if !shouldBeActive && t.Active {
			e.client.StopTorrent(t.InfoHash)
			e.slotParkedMu.Lock()
			e.slotParked[t.InfoHash] = true
			e.slotParkedMu.Unlock()
			stopped++
		}
	}

	// Update stats
	activeAfter := 0
	for _, t := range incomplete {
		if targetSet[t.InfoHash] {
			activeAfter++
		}
	}
	cooldownCount := 0
	e.slotProgressMu.Lock()
	for _, pi := range e.slotProgress {
		if now.Before(pi.cooldownUntil) {
			cooldownCount++
		}
	}
	e.slotProgressMu.Unlock()

	e.lastSlotStatsMu.Lock()
	e.lastSlotStats = DownloadSlotStats{
		MaxSlots:        maxSlots,
		ActiveSlots:     activeAfter,
		TotalIncomplete: len(incomplete),
		ActivityDemoted: activityDemoted,
		Cooldown:        cooldownCount,
		Started:         started,
		Stopped:         stopped,
	}
	e.lastSlotStatsMu.Unlock()

	if started > 0 || stopped > 0 || activityDemoted > 0 || now.Sub(e.lastSlotLog) > 5*time.Minute {
		slog.Info("hoard: slot manager",
			"total_incomplete", len(incomplete),
			"active_after", activeAfter,
			"started", started, "stopped", stopped,
			"demoted", activityDemoted, "cooldown", cooldownCount)
		e.lastSlotLog = now
	}
}

// ---------------------------------------------------------------------------
// SetCategoryLabel re-tags a torrent's category WITHOUT moving its files on
// disk (save_path is unchanged). Use SetTorrentCategory to also relocate the
// data under the new category's save_path.
func (e *HoardEngine) SetCategoryLabel(infoHash, category string) error {
	e.mu.Lock()
	info, ok := e.torrents[infoHash]
	if !ok {
		e.mu.Unlock()
		return fmt.Errorf("torrent not found")
	}
	info.Category = category
	e.mu.Unlock()
	e.cachedStatsMu.Lock()
	if st, ok := e.cachedStats[infoHash]; ok {
		st.Category = category
	}
	e.cachedStatsMu.Unlock()
	return nil
}

// ClearCategoryLabel drops the given category label from every torrent that
// carries it (no file move) and returns how many were cleared. Used when a
// category is deleted so torrents do not keep a dangling label; they become
// uncategorized, like qBittorrent's delete-category behaviour. Persisted with
// the next store snapshot (mutates e.torrents, which the snapshot reads).
func (e *HoardEngine) ClearCategoryLabel(category string) int {
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
		if st, ok := e.cachedStats[ih]; ok {
			st.Category = ""
		}
	}
	e.cachedStatsMu.Unlock()
	return len(hits)
}

// ---------------------------------------------------------------------------
// Tags — qBittorrent-style multi-labels. Go-side only (Typhon never sees them),
// mirrored into cachedStats for immediate effect and persisted via tags.json.
// ---------------------------------------------------------------------------

// GetTags returns a copy of a torrent's tags (nil if none / not found).
func (e *HoardEngine) GetTags(infoHash string) []string {
	e.mu.RLock()
	defer e.mu.RUnlock()
	if info, ok := e.torrents[infoHash]; ok && len(info.Tags) > 0 {
		out := make([]string, len(info.Tags))
		copy(out, info.Tags)
		return out
	}
	return nil
}

// GetAllTags returns info_hash -> tags for every tagged torrent (backs the
// tags.json overlay and the /api/tags list).
func (e *HoardEngine) GetAllTags() map[string][]string {
	e.mu.RLock()
	defer e.mu.RUnlock()
	out := make(map[string][]string)
	for ih, info := range e.torrents {
		if len(info.Tags) > 0 {
			cp := make([]string, len(info.Tags))
			copy(cp, info.Tags)
			out[ih] = cp
		}
	}
	return out
}

func (e *HoardEngine) applyTags(infoHash string, tags []string) error {
	e.mu.Lock()
	info, ok := e.torrents[infoHash]
	if !ok {
		e.mu.Unlock()
		return fmt.Errorf("torrent not found")
	}
	info.Tags = tags
	e.mu.Unlock()
	e.cachedStatsMu.Lock()
	if st, ok := e.cachedStats[infoHash]; ok {
		st.Tags = tags
	}
	e.cachedStatsMu.Unlock()
	return nil
}

// SetTags replaces a torrent's tag set (nil/empty clears).
func (e *HoardEngine) SetTags(infoHash string, tags []string) error {
	return e.applyTags(infoHash, normalizeTags(tags))
}

// AddTags unions tags into the torrent's set.
func (e *HoardEngine) AddTags(infoHash string, tags []string) error {
	e.mu.RLock()
	var cur []string
	if info, ok := e.torrents[infoHash]; ok {
		cur = info.Tags
	}
	e.mu.RUnlock()
	return e.applyTags(infoHash, normalizeTags(append(append([]string{}, cur...), tags...)))
}

// RemoveTags removes the given tags (empty list clears all).
func (e *HoardEngine) RemoveTags(infoHash string, tags []string) error {
	if len(tags) == 0 {
		return e.applyTags(infoHash, nil)
	}
	drop := make(map[string]bool)
	for _, t := range normalizeTags(tags) {
		drop[t] = true
	}
	e.mu.RLock()
	var cur []string
	if info, ok := e.torrents[infoHash]; ok {
		cur = info.Tags
	}
	e.mu.RUnlock()
	var kept []string
	for _, t := range cur {
		if !drop[t] {
			kept = append(kept, t)
		}
	}
	return e.applyTags(infoHash, kept)
}

// RestoreTags applies persisted tags at boot (from the tags.json overlay).
func (e *HoardEngine) RestoreTags(byHash map[string][]string) {
	e.mu.Lock()
	defer e.mu.Unlock()
	for ih, tags := range byHash {
		if info, ok := e.torrents[ih]; ok && len(tags) > 0 {
			info.Tags = normalizeTags(tags)
		}
	}
}

// SetAddedTime overrides a torrent's recorded add time (both the canonical
// TorrentInfo and the stats cache). Used by the qBittorrent import to preserve
// the original add date instead of the moment of import.
func (e *HoardEngine) SetAddedTime(infoHash string, t time.Time) {
	e.mu.Lock()
	if info, ok := e.torrents[infoHash]; ok {
		info.AddedTime = t
	}
	e.mu.Unlock()
	e.cachedStatsMu.Lock()
	if st, ok := e.cachedStats[infoHash]; ok {
		st.AddedTime = t.Unix()
	}
	e.cachedStatsMu.Unlock()
}

// SetTorrentCategory — move a torrent between categories at runtime.
//
// Stops the torrent, renames its data directory to the target category's
// save_path (same-filesystem only; refuses cross-fs to preserve Sonarr/
// Radarr hardlinks via inode), tells Typhon about the new save_path, then
// restarts. Mirrors Category + SavePath into the Go-side metadata so the
// next state.json snapshot persists the new layout.
// ---------------------------------------------------------------------------

// SetTorrentCategory changes a hoard torrent's category and moves its files
// under the target category's save_path. The arg newCategorySavePath is the
// raw `save_path` from categories.json (the *category* dir, not the torrent
// root). Hydra's convention: info.SavePath is the torrent's on-disk root
// (`<category>/<torrent_name>`), while Typhon's engine save_path is either
// `filepath.Dir(info.SavePath)` for multi-file torrents or equal to it for
// single-file torrents. Same-fs only; refuses cross-fs to preserve hardlinks.
func (e *HoardEngine) SetTorrentCategory(infoHash, newCategory, newCategorySavePath string) error {
	if infoHash == "" {
		return fmt.Errorf("empty info_hash")
	}
	if newCategorySavePath == "" {
		return fmt.Errorf("empty save_path for target category")
	}

	// Snapshot Go-side metadata.
	e.mu.RLock()
	info, ok := e.torrents[infoHash]
	if !ok {
		e.mu.RUnlock()
		return fmt.Errorf("torrent not found")
	}
	oldOnDisk := info.SavePath
	oldCategory := info.Category
	torrentName := info.Name
	wrapped := info.ContentFolder == nil || *info.ContentFolder
	e.mu.RUnlock()

	if oldOnDisk == "" {
		return fmt.Errorf("torrent has no save_path")
	}

	// Ask the engine for its current save_path. If it differs from the
	// on-disk root we hold, this is a multi-file torrent (Typhon points at
	// the parent, then joins `name`); otherwise it's single-file.
	st, err := e.client.GetStatus(infoHash)
	if err != nil {
		return fmt.Errorf("get torrent status: %w", err)
	}
	oldEngineSavePath := st.SavePath
	isMultiFile := oldEngineSavePath != oldOnDisk

	// Loose single-file (create_torrent_folder off): the payload is a bare file
	// living directly in the category dir (info.SavePath == category dir), with
	// no per-torrent folder to rename. Move the file itself. (B1)
	if !isMultiFile && !wrapped {
		if torrentName == "" {
			return fmt.Errorf("cannot move loose single-file torrent: unknown file name")
		}
		oldFile := filepath.Join(oldOnDisk, torrentName)
		newFile := filepath.Join(newCategorySavePath, torrentName)
		if oldFile == newFile && (newCategory == "" || newCategory == oldCategory) {
			return nil
		}
		if err := hoardCheckSameFs(oldOnDisk, newCategorySavePath); err != nil {
			return err
		}
		if err := os.MkdirAll(newCategorySavePath, 0o755); err != nil {
			return fmt.Errorf("mkdir %s: %w", newCategorySavePath, err)
		}
		if _, err := os.Stat(newFile); err == nil {
			return fmt.Errorf("destination already exists: %s", newFile)
		}
		if err := e.client.StopTorrent(infoHash); err != nil {
			return fmt.Errorf("stop torrent: %w", err)
		}
		time.Sleep(150 * time.Millisecond)
		if err := os.Rename(oldFile, newFile); err != nil {
			_ = e.autoStart(infoHash)
			return fmt.Errorf("rename %s -> %s: %w", oldFile, newFile, err)
		}
		if err := e.client.SetSavePath(infoHash, newCategorySavePath); err != nil {
			_ = os.Rename(newFile, oldFile)
			_ = e.autoStart(infoHash)
			return fmt.Errorf("set engine save_path: %w", err)
		}
		e.mu.Lock()
		if inf, ok := e.torrents[infoHash]; ok {
			inf.SavePath = newCategorySavePath
			if newCategory != "" {
				inf.Category = newCategory
			}
		}
		e.mu.Unlock()
		e.cachedStatsMu.Lock()
		if st, ok := e.cachedStats[infoHash]; ok {
			st.SavePath = newCategorySavePath
			if newCategory != "" {
				st.Category = newCategory
			}
		}
		e.cachedStatsMu.Unlock()
		if err := e.autoStart(infoHash); err != nil {
			slog.Warn("hoard: restart after category change failed", "info_hash", infoHash, "error", err)
		}
		if e.reAnnounce != nil {
			go e.reAnnounce(infoHash, st.TotalSize)
		}
		slog.Info("hoard: changed category (loose single-file)", "info_hash", infoHash,
			"from_category", oldCategory, "to_category", newCategory, "from_file", oldFile, "to_file", newFile)
		return nil
	}

	rootName := filepath.Base(oldOnDisk)
	newOnDisk := filepath.Join(newCategorySavePath, rootName)

	if oldOnDisk == newOnDisk && (newCategory == "" || newCategory == oldCategory) {
		return nil // nothing to do
	}

	if err := hoardCheckSameFs(oldOnDisk, newCategorySavePath); err != nil {
		return err
	}

	if err := os.MkdirAll(newCategorySavePath, 0o755); err != nil {
		return fmt.Errorf("mkdir %s: %w", newCategorySavePath, err)
	}
	if _, err := os.Stat(newOnDisk); err == nil {
		return fmt.Errorf("destination already exists: %s", newOnDisk)
	}

	if err := e.client.StopTorrent(infoHash); err != nil {
		return fmt.Errorf("stop torrent: %w", err)
	}
	// Brief drain so any in-flight write completes against the old path
	// before the rename swaps the dir.
	time.Sleep(150 * time.Millisecond)

	if err := os.Rename(oldOnDisk, newOnDisk); err != nil {
		_ = e.autoStart(infoHash)
		return fmt.Errorf("rename %s -> %s: %w", oldOnDisk, newOnDisk, err)
	}

	// Compute the new engine save_path. Multi-file: the *category* dir.
	// Single-file: the torrent root itself.
	newEngineSavePath := newCategorySavePath
	if !isMultiFile {
		newEngineSavePath = newOnDisk
	}

	if err := e.client.SetSavePath(infoHash, newEngineSavePath); err != nil {
		_ = os.Rename(newOnDisk, oldOnDisk)
		_ = e.autoStart(infoHash)
		return fmt.Errorf("set engine save_path: %w", err)
	}

	e.mu.Lock()
	if info, ok := e.torrents[infoHash]; ok {
		info.SavePath = newOnDisk
		if newCategory != "" {
			info.Category = newCategory
		}
	}
	e.mu.Unlock()
	e.cachedStatsMu.Lock()
	if st, ok := e.cachedStats[infoHash]; ok {
		st.SavePath = newOnDisk
		if newCategory != "" {
			st.Category = newCategory
		}
	}
	e.cachedStatsMu.Unlock()

	if err := e.autoStart(infoHash); err != nil {
		slog.Warn("hoard: restart after category change failed",
			"info_hash", infoHash, "error", err)
	}

	// The Stop above emitted event=stopped, dropping us from the tracker swarm.
	// Force an immediate seeder re-announce so we don't sit at 0 seeders until
	// the next slot-gated periodic announce. (Bug: a category change on a live
	// seed made torr9 show 0 seeders / N leechers until re-add.)
	if e.reAnnounce != nil {
		go e.reAnnounce(infoHash, st.TotalSize)
	}

	slog.Info("hoard: changed category",
		"info_hash", infoHash,
		"from_category", oldCategory, "to_category", newCategory,
		"from_on_disk", oldOnDisk, "to_on_disk", newOnDisk,
		"engine_save_path", newEngineSavePath, "multi_file", isMultiFile)
	return nil
}

// hoardCheckSameFs returns an error if `a` and `b` (or their first existing
// ancestor for `b`) live on different filesystems. Used by SetTorrentCategory
// to refuse cross-fs moves that would break Sonarr/Radarr hardlinks (inodes
// are not preserved across filesystems).
func hoardCheckSameFs(a, b string) error {
	devA, err := hoardStatDev(a)
	if err != nil {
		return fmt.Errorf("stat %s: %w", a, err)
	}
	devB, err := hoardStatDevWithAncestors(b)
	if err != nil {
		return fmt.Errorf("stat %s: %w", b, err)
	}
	if devA != devB {
		return fmt.Errorf("cross-filesystem move would break hardlinks: %s (dev %d) -> %s (dev %d)",
			a, devA, b, devB)
	}
	return nil
}

func hoardStatDev(p string) (uint64, error) {
	return statDev(p)
}

// hoardStatDevWithAncestors walks up until it finds an existing directory and
// returns its device id. Handles the common case where the new save_path
// directory itself doesn't exist yet but its parent does.
func hoardStatDevWithAncestors(p string) (uint64, error) {
	cur := p
	for {
		if dev, err := hoardStatDev(cur); err == nil {
			return dev, nil
		}
		parent := filepath.Dir(cur)
		if parent == cur {
			return 0, fmt.Errorf("no existing ancestor for %s", p)
		}
		cur = parent
	}
}

// StartTorrent resumes a torrent (agent RPC passthrough to the engine client).
func (e *HoardEngine) StartTorrent(infoHash string) error {
	if e.client == nil {
		return fmt.Errorf("engine client not ready")
	}
	return e.client.StartTorrent(infoHash)
}

// StopTorrent pauses a torrent (agent RPC passthrough to the engine client).
func (e *HoardEngine) StopTorrent(infoHash string) error {
	if e.client == nil {
		return fmt.Errorf("engine client not ready")
	}
	return e.client.StopTorrent(infoHash)
}

// SetEngineOptFlag toggles one engine-side optimisation without a restart.
func (e *HoardEngine) SetEngineOptFlag(name string, on bool, value int64) (map[string]interface{}, error) {
	return e.client.SetEngineOptFlag(name, on, value)
}

// EngineOptFlags reports the engine-side flag state.
func (e *HoardEngine) EngineOptFlags() (map[string]interface{}, error) {
	return e.client.EngineOptFlags()
}
