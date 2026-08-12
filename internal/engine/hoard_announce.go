package engine

import (
	"context"
	"fmt"
	"log/slog"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/Kheopsian/hydra/internal/engine/ltclient"
)

// ---------------------------------------------------------------------------
// HoardAnnouncer — Go-canonical tracker announce loop for the hoard engine.
// ---------------------------------------------------------------------------
//
// Scans the engine's torrent list every cycleInterval, spawns a per-torrent
// goroutine that announces to all configured trackers (tier 0 first, fallback
// tiers if the primary fails), aggregates peer lists, and injects peers back
// into the engine via add_peers.
//
// Single-binding for now: announcers is a slice of length 1. Multi-binding
// extends this slice to N (one per WireGuard tunnel egress) so each binding
// announces with its own peer_id and source IP — see project_proton_wg_multi_tunnel.

const (
	hoardCycleInterval      = 10 * time.Second
	hoardMaxNewPerCycle     = 500
	hoardDefaultInterval    = 30 * time.Minute
	hoardMinInterval        = 60 * time.Second // floor for tracker-supplied interval
	hoardInitialBootDelay   = 5 * time.Second
	hoardTrackerCallTimeout = 10 * time.Second
)

// TrackerObservation captures the result of one announce against one tracker
// URL. Stored per (info_hash, url) so the detail panel can show real per-
// tracker state (Typhon's own state is stale because internal_announce is off).
type TrackerObservation struct {
	OK       bool
	Seeds    int
	Leechers int
	ErrorMsg string    // empty when OK
	NextAt   time.Time // expected next announce; zero == unknown / immediate
}

// AnnounceObservation summarises a single torrent-announce cycle so the
// engine can mirror tracker-derived state into its cachedStats and the UI
// sees swarm scrape numbers + tracker errors. Typhon's internal announce
// loop is disabled (DisableInternalAnnounce=true) so this is the only
// channel that keeps SwarmSeeds/SwarmLeechers/TrackerError flowing.
type AnnounceObservation struct {
	// Cross-tracker aggregate for the torrent. Seeds/Leechers are the max
	// scrape across tiers that responded; OK is true if any tier did.
	Seeds    int
	Leechers int
	OK       bool
	ErrorMsg string

	// Per-tracker breakdown — one entry per HTTP(S) tracker URL we tried
	// this cycle. Used to override the per-endpoint "last_error"/"message"
	// fields in GetTorrentDetail's trackers[] passthrough.
	Trackers map[string]TrackerObservation
}

// HoardAnnouncer owns the periodic tracker announce loop for hoard torrents.
// It is engine-canonical: when active, Typhon's internal announce loop is
// disabled via EngineConfig.disable_internal_announce.
type HoardAnnouncer struct {
	announcers []*trackerAnnouncer // one per binding (start with 1)
	client     EngineClient
	logPrefix  string

	// OnObservation, if set, is invoked once per torrent-announce cycle
	// with the aggregated swarm scrape + last error so the engine can fold
	// it into cachedStats. Wired in main.go to HoardEngine.ObserveAnnounce.
	OnObservation func(infoHash string, obs AnnounceObservation)

	// raceHas, si défini, gate l'annonce : on N'ANNONCE PAS un infohash que le
	// moteur race possède (anti dual-annonce ; le race est seul annonceur tant
	// qu'il tient le torrent). Laissé nil sur l'announcer race lui-même.
	raceHas func(infoHash string) bool

	// userStopped reports a deliberate user stop, so one-shot announces stay
	// away from torrents nobody wants in the swarm.
	userStopped func(infoHash string) bool
	// offsetFn, si défini, renvoie un offset UL/DL ajouté au cumulé annoncé
	// (continuité du handoff race->hoard). nil = pas d'offset.
	offsetFn func(infoHash string) (int64, int64)

	mu      sync.Mutex
	running map[string]*announceTask // info_hash -> task

	ctx    context.Context
	cancel context.CancelFunc
}

// SetUserStoppedGate wires the check that keeps one-shot announces away from a
// torrent the user has stopped. Without it an import that lands 3000 stopped
// torrents would announce every one of them as "started" before the stop lands.
func (h *HoardAnnouncer) SetUserStoppedGate(fn func(infoHash string) bool) { h.userStopped = fn }

// SetRaceGate active le gating d'annonce : les infohash pour lesquels has(ih)
// est vrai ne sont PAS annoncés par cet announcer (le moteur race les annonce).
func (h *HoardAnnouncer) SetRaceGate(has func(infoHash string) bool) { h.raceHas = has }

// SetOffsetFn câble la source d'offset UL/DL ajouté au cumulé annoncé.
func (h *HoardAnnouncer) SetOffsetFn(fn func(infoHash string) (int64, int64)) { h.offsetFn = fn }

// SetLivePort points every binding announcer at a live port atomic so a
// runtime listen-port change (engine SetListenPort) is reflected in the
// &port= we send to trackers. A nil pointer or a zero value keeps the
// binding static port.
func (h *HoardAnnouncer) SetLivePort(p *atomic.Int64) {
	for _, a := range h.announcers {
		a.livePort = p
	}
}

type announceTask struct {
	stop chan struct{}
}

// NewHoardAnnouncer constructs a hoard announcer wired to the given engine
// client. `bindings` is a per-tunnel description of network identities;
// each entry produces one trackerAnnouncer with its own peer_id, public IP,
// and source-bound HTTP transport. Pass a single binding for the legacy
// single-tunnel behavior (helpers in binding.go).
func NewHoardAnnouncer(client EngineClient, bindings []Binding) *HoardAnnouncer {
	announcers := make([]*trackerAnnouncer, 0, len(bindings))
	for _, b := range bindings {
		announcers = append(announcers, newTrackerAnnouncerForBinding(b))
	}
	return &HoardAnnouncer{
		announcers: announcers,
		client:     client,
		logPrefix:  "hoard_announce",
		running:    make(map[string]*announceTask),
	}
}

// Start launches the orchestrator goroutine. Blocks for hoardInitialBootDelay
// before the first scan to let Typhon finish loading torrents at startup.
func (h *HoardAnnouncer) Start(ctx context.Context) {
	if len(h.announcers) == 0 {
		slog.Warn("hoard_announce: no announcers configured, loop disabled")
		return
	}
	h.ctx, h.cancel = context.WithCancel(ctx)
	go h.startScheduler()
	slog.Info("hoard_announce: started", "bindings", len(h.announcers))
}

// Stop cancels all in-flight per-torrent announce loops. Safe to call multiple
// times.
func (h *HoardAnnouncer) Stop() {
	if h.cancel != nil {
		h.cancel()
	}
}

func (h *HoardAnnouncer) orchestratorLoop() {
	// Initial boot delay: same heuristic Typhon used (let session settle).
	select {
	case <-h.ctx.Done():
		return
	case <-time.After(hoardInitialBootDelay):
	}

	ticker := time.NewTicker(hoardCycleInterval)
	defer ticker.Stop()

	// Run one scan immediately, then every cycleInterval.
	h.scanAndStart()
	for {
		select {
		case <-h.ctx.Done():
			return
		case <-ticker.C:
			h.scanAndStart()
		}
	}
}

// startupHeld reports whether this announcer's engine is in its startup pause.
// Every announcer here shares one engine scope, so the first one answers for
// all of them.
func (h *HoardAnnouncer) startupHeld() bool {
	if len(h.announcers) == 0 {
		return false
	}
	return h.announcers[0].gate.blocked()
}

// scanAndStart enumerates active torrents and starts an announce loop for
// each one not already covered. Capped at hoardMaxNewPerCycle to avoid a
// thundering herd on the tracker at first boot (~13k torrents).
func (h *HoardAnnouncer) scanAndStart() {
	res, err := h.client.ListTorrents()
	if err != nil {
		slog.Debug("hoard_announce: ListTorrents failed", "error", err)
		return
	}

	started := 0
	h.mu.Lock()
	for i := range res.Torrents {
		if started >= hoardMaxNewPerCycle {
			break
		}
		t := &res.Torrents[i]
		if !shouldAnnounce(t) {
			continue
		}
		// Gating : si le moteur race possède ce torrent, on le laisse annoncer
		// (un seul peer annonceur par infohash -> cumulé propre, crédit max).
		if h.raceHas != nil && h.raceHas(t.InfoHash) {
			continue
		}
		if _, exists := h.running[t.InfoHash]; exists {
			continue
		}
		stop := make(chan struct{})
		h.running[t.InfoHash] = &announceTask{stop: stop}
		started++
		go h.torrentAnnounceLoop(t.InfoHash, t.TotalSize, stop)
	}
	total := len(h.running)
	h.mu.Unlock()
	if started > 0 {
		slog.Info("hoard_announce: started new loops",
			"new", started, "active", total, "total_torrents", len(res.Torrents))
	}
}

// shouldAnnounce returns true for torrents the announcer should track.
// Mirrors Typhon's filter: seeding or downloading, not paused.
func shouldAnnounce(t *ltclient.TorrentStatus) bool {
	if t.IsPaused {
		return false
	}
	switch t.State {
	case "Seeding", "Downloading", "seeding", "downloading":
		return true
	}
	return false
}

// torrentAnnounceLoop is the per-torrent announce goroutine. Lives until the
// torrent is removed, the announcer is stopped, or status check fails.
func (h *HoardAnnouncer) torrentAnnounceLoop(infoHash string, totalSize int64, stop chan struct{}) {
	defer func() {
		h.mu.Lock()
		delete(h.running, infoHash)
		h.mu.Unlock()
	}()

	short := infoHash
	if len(short) > 8 {
		short = short[:8]
	}

	interval := hoardDefaultInterval
	firstAnnounce := true

	for {
		// Fetch fresh status to compute `left` and stop condition.
		status, err := h.client.GetStatus(infoHash)
		if err != nil {
			slog.Debug("hoard_announce: GetStatus failed, exiting loop",
				"info_hash", short, "error", err)
			return
		}
		// Torrent removed or stopped: bail.
		if !shouldAnnounce(status) {
			slog.Debug("hoard_announce: torrent no longer announceable, exiting",
				"info_hash", short, "state", status.State, "paused", status.IsPaused)
			return
		}
		// Le moteur race a (re)pris ce torrent : on cède l'annonce (un seul
		// peer annonceur). Le scan reprendra l'annonce hoard quand le race purge.
		if h.raceHas != nil && h.raceHas(infoHash) {
			slog.Debug("hoard_announce: race owns torrent, ceding announce",
				"info_hash", short)
			return
		}

		var left int64
		if status.IsSeeding {
			left = 0
		} else if totalSize > 0 {
			left = totalSize - status.TotalDone
			if left < 0 {
				left = 0
			}
		} else if status.TotalSize > 0 {
			left = status.TotalSize - status.TotalDone
		}
		// Never announce a leecher with left=0: trackers treat left=0 as "we are
		// a seed" and withhold the peer list (confirmed on MAM — seeds returned
		// but 0 peers). If the remaining bytes can't be derived yet (fresh add,
		// size not known), send a non-zero sentinel so we get peers.
		if !status.IsSeeding && left <= 0 {
			left = 1
		}

		event := ""
		if firstAnnounce {
			event = "started"
		}

		// Pull tracker URLs (cheap RPC; trackers can change at runtime via
		// add-tracker route, so we re-read each cycle rather than caching).
		trackers, err := h.client.GetTrackers(infoHash)
		if err != nil {
			slog.Debug("hoard_announce: GetTrackers failed",
				"info_hash", short, "error", err)
			// Backoff and retry — torrent may be mid-add.
			if !sleepOrStop(stop, h.ctx, hoardMinInterval) {
				return
			}
			continue
		}

		// Offset hérité d'un doublon race purgé : ajouté au cumulé ANNONCÉ
		// uniquement (continuité du handoff ; les compteurs globaux ne le
		// voient pas — préservés par AbsorbStats côté race).
		ulOff, dlOff := int64(0), int64(0)
		if h.offsetFn != nil {
			ulOff, dlOff = h.offsetFn(infoHash)
		}
		newInterval, obs := h.announceAllTiers(infoHash, totalSize, left, status.TotalUpload+ulOff, status.TotalDownload+dlOff, event, trackers)
		if obs.OK {
			firstAnnounce = false
			if newInterval > 0 {
				interval = newInterval
			}
		}
		// Stamp the next-announce ETA on every per-tracker observation so the
		// UI's `next` column shows a real countdown instead of 0/"Now".
		nextAt := time.Now().Add(interval)
		for url, to := range obs.Trackers {
			to.NextAt = nextAt
			obs.Trackers[url] = to
		}
		if h.OnObservation != nil {
			h.OnObservation(infoHash, obs)
		}

		if !sleepOrStop(stop, h.ctx, interval) {
			return
		}
	}
}

// BootstrapAnnounce fires a SINGLE immediate announce for a freshly-added
// torrent so its swarm-seed count reaches the announce cache (and thus the
// download slot manager) right away. Without it a new download starts at
// swarm_seeds=0, the slot manager ranks it last and parks it, and — because
// the periodic announce loop skips parked torrents — it never announces:
// a catch-22 that leaves well-seeded torrents stuck at 0 peers. Peers found
// here are injected too. Called (in a goroutine) from the hoard AddTorrent path.
func (h *HoardAnnouncer) BootstrapAnnounce(infoHash string, totalSize int64) {
	// Fresh-download bootstrap: force a non-zero left so the tracker returns the
	// peer list (left=0 = "seed" = no peers).
	h.announceOnce(infoHash, totalSize, true, "bootstrap")
}

// ReAnnounce fires one immediate announce for an ALREADY-active torrent using
// its real completion state, so a complete torrent re-registers as a *seeder*
// (left=0), not a leecher. Used after an in-place operation that dropped us from
// the tracker swarm — e.g. SetTorrentCategory does stop->rename->start and the
// stop emits event=stopped. Without it the torrent sits at 0 seeders until the
// next slot-gated periodic announce. Unlike BootstrapAnnounce it does NOT force
// left>=1 (that would re-register a seeder as a leecher).
func (h *HoardAnnouncer) ReAnnounce(infoHash string, totalSize int64) {
	h.announceOnce(infoHash, totalSize, false, "reannounce")
}

// StoppedAnnounce tells the trackers we are leaving the swarm, once, right
// when the user stops the torrent.
//
// Without it stopping is silent: we simply go quiet, and the tracker keeps
// listing us as an active peer until the entry expires on its side. On a
// private tracker that counts seedtime, "quietly gone" and "still here" are not
// the same thing.
//
// Deliberately best-effort and fire-and-forget: it must not delay the stop, and
// a tracker that is down is not a reason to refuse to stop a torrent. No peer
// injection either -- we are leaving, peers are of no use to us.
func (h *HoardAnnouncer) StoppedAnnounce(infoHash string, totalSize int64) {
	short := infoHash
	if len(short) > 8 {
		short = short[:8]
	}
	// Race owns the announce for dual-seeded torrents -- it sends its own.
	if h.raceHas != nil && h.raceHas(infoHash) {
		return
	}
	status, err := h.client.GetStatus(infoHash)
	if err != nil {
		slog.Debug("hoard_announce: stopped GetStatus failed", "info_hash", short, "error", err)
		return
	}
	if totalSize <= 0 {
		totalSize = status.TotalSize
	}
	trackers, err := h.client.GetTrackers(infoHash)
	if err != nil || len(trackers) == 0 {
		return
	}
	ulOff, dlOff := int64(0), int64(0)
	if h.offsetFn != nil {
		ulOff, dlOff = h.offsetFn(infoHash)
	}
	left := totalSize - status.TotalDone
	if left < 0 {
		left = 0
	}
	h.announceAllTiers(infoHash, totalSize, left,
		status.TotalUpload+ulOff, status.TotalDownload+dlOff, "stopped", trackers)
	slog.Info("hoard_announce: stopped announce", "info_hash", short)
}

// announceOnce performs a single "started" announce across all tiers. When
// forceLeftForPeers is true and the torrent is already complete, left is bumped
// to 1 so the tracker returns peers (leecher bootstrap); otherwise the real left
// is used (0 for a seeder).
func (h *HoardAnnouncer) announceOnce(infoHash string, totalSize int64, forceLeftForPeers bool, tag string) {
	short := infoHash
	if len(short) > 8 {
		short = short[:8]
	}
	// Race owns the announce for dual-seeded torrents — don't double-announce.
	if h.raceHas != nil && h.raceHas(infoHash) {
		return
	}
	// The user stopped this one: announcing it would undo the stop.
	if h.userStopped != nil && h.userStopped(infoHash) {
		return
	}
	status, err := h.client.GetStatus(infoHash)
	if err != nil {
		slog.Debug("hoard_announce: "+tag+" GetStatus failed", "info_hash", short, "error", err)
		return
	}
	if totalSize <= 0 {
		totalSize = status.TotalSize
	}
	trackers, err := h.client.GetTrackers(infoHash)
	if err != nil || len(trackers) == 0 {
		slog.Debug("hoard_announce: "+tag+" no trackers", "info_hash", short, "error", err)
		return
	}
	ulOff, dlOff := int64(0), int64(0)
	if h.offsetFn != nil {
		ulOff, dlOff = h.offsetFn(infoHash)
	}
	left := totalSize - status.TotalDone
	if left <= 0 && forceLeftForPeers {
		left = 1
	}
	if left < 0 {
		left = 0
	}
	interval, obs := h.announceAllTiers(infoHash, totalSize, left, status.TotalUpload+ulOff, status.TotalDownload+dlOff, "started", trackers)
	nextAt := time.Now().Add(interval)
	for url, to := range obs.Trackers {
		to.NextAt = nextAt
		obs.Trackers[url] = to
	}
	if h.OnObservation != nil {
		h.OnObservation(infoHash, obs)
	}
	slog.Info("hoard_announce: "+tag+" announce", "info_hash", short, "seeds", obs.Seeds, "ok", obs.OK)
}

// announceAllTiers walks the tracker tiers in order, announcing to each
// tier's URLs through every binding. Returns the longest interval reported
// across successful announces and the aggregated observation (per-tracker
// breakdown included).
func (h *HoardAnnouncer) announceAllTiers(infoHash string, totalSize, left, uploaded, downloaded int64, event string, trackers []ltclient.TrackerInfo) (time.Duration, AnnounceObservation) {
	// Group by tier — the engine returns a flat list with tier ints; we want
	// to try lower tiers first and only fallback if everything in that tier
	// fails (BEP-12 "multitracker" semantics, simplified).
	tiers := groupTrackersByTier(trackers)
	obs := AnnounceObservation{Trackers: make(map[string]TrackerObservation)}
	maxInterval := 0

	for _, tier := range tiers {
		anyOK := false
		for _, url := range tier {
			if !isSupportedTrackerScheme(url) {
				continue
			}
			peers, intervalSec, seeds, leechers, errMsg, ok := h.announceFromAllBindings(url, infoHash, uploaded, downloaded, left, event)
			if !ok {
				obs.Trackers[url] = TrackerObservation{OK: false, ErrorMsg: errMsg}
				if errMsg != "" {
					obs.ErrorMsg = errMsg
				}
				continue
			}
			obs.Trackers[url] = TrackerObservation{
				OK:       true,
				Seeds:    seeds,
				Leechers: leechers,
			}
			anyOK = true
			obs.OK = true
			if seeds > obs.Seeds {
				obs.Seeds = seeds
			}
			if leechers > obs.Leechers {
				obs.Leechers = leechers
			}
			if intervalSec > maxInterval {
				maxInterval = intervalSec
			}
			if strings.Contains(url, "myanonamouse") {
				slog.Info("hoard_announce: MAM announce parsed",
					"info_hash", infoHash[:minStr(len(infoHash), 8)],
					"peers", len(peers), "seeds", seeds, "leechers", leechers)
			}
			if len(peers) > 0 {
				h.injectPeers(infoHash, peers)
			}
		}
		if anyOK {
			break // per BEP-12, stop at first tier with any successful announce
		}
	}

	// Fold this cycle's per-tracker results into the per-host registry that
	// backs the Trackers UI tab (built from live announces = hot set only).
	for turl, to := range obs.Trackers {
		trackerReg.recordAnnounce(trackerHostOf(turl), infoHash, to.OK, to.ErrorMsg)
	}

	// Floor the interval so a misbehaving tracker can't pin us at 1s loops.
	if maxInterval > 0 {
		d := time.Duration(maxInterval) * time.Second
		if d < hoardMinInterval {
			d = hoardMinInterval
		}
		_ = totalSize
		return d, obs
	}
	return 0, obs
}

// announceFromAllBindings runs the announce on every binding (announcer)
// in parallel and aggregates peers + swarm scrape across bindings. Each
// binding presents its own peer_id to the tracker — for single-binding this
// is a no-op fan-out. Returns last error string when no binding succeeded.
func (h *HoardAnnouncer) announceFromAllBindings(trackerURL, infoHash string, uploaded, downloaded, left int64, event string) ([]TrackerPeer, int, int, int, string, bool) {
	type result struct {
		peers    []TrackerPeer
		interval int
		seeds    int
		leechers int
		errMsg   string
		ok       bool
	}

	results := make(chan result, len(h.announcers))
	for _, ann := range h.announcers {
		ann := ann
		go func() {
			res, err := ann.announce(trackerURL, infoHash, uploaded, downloaded, left, event)
			if err != nil {
				results <- result{ok: false, errMsg: err.Error()}
				return
			}
			if res == nil {
				results <- result{ok: false, errMsg: "nil tracker response"}
				return
			}
			if res.FailureReason != "" {
				slog.Debug("hoard_announce: tracker failure",
					"info_hash", infoHash[:minStr(len(infoHash), 8)],
					"tracker", trackerURL, "reason", res.FailureReason)
				results <- result{ok: false, errMsg: res.FailureReason}
				return
			}
			results <- result{
				peers:    res.Peers,
				interval: res.Interval,
				seeds:    res.Complete,
				leechers: res.Incomplete,
				ok:       true,
			}
		}()
	}

	merged := make([]TrackerPeer, 0)
	seen := make(map[string]struct{})
	maxInterval := 0
	maxSeeds, maxLeechers := 0, 0
	anyOK := false
	var lastErr string
	for i := 0; i < len(h.announcers); i++ {
		r := <-results
		if !r.ok {
			if r.errMsg != "" {
				lastErr = r.errMsg
			}
			continue
		}
		anyOK = true
		if r.interval > maxInterval {
			maxInterval = r.interval
		}
		if r.seeds > maxSeeds {
			maxSeeds = r.seeds
		}
		if r.leechers > maxLeechers {
			maxLeechers = r.leechers
		}
		for _, p := range r.peers {
			key := fmt.Sprintf("%s:%d", p.IP, p.Port)
			if _, ok := seen[key]; ok {
				continue
			}
			seen[key] = struct{}{}
			merged = append(merged, p)
		}
	}
	if anyOK {
		lastErr = ""
	}
	return merged, maxInterval, maxSeeds, maxLeechers, lastErr, anyOK
}

// injectPeers feeds the aggregated peer list to the engine via add_peers.
// Errors are logged but not fatal — tracker peers are a best-effort feed
// (DHT/PEX cover the gap).
func (h *HoardAnnouncer) injectPeers(infoHash string, peers []TrackerPeer) {
	if len(peers) == 0 {
		return
	}
	pl := make([]struct {
		IP   string
		Port int
	}, len(peers))
	for i, p := range peers {
		pl[i] = struct {
			IP   string
			Port int
		}{IP: p.IP, Port: p.Port}
	}
	if err := h.client.AddPeers(infoHash, pl); err != nil {
		slog.Warn("hoard_announce: add_peers FAILED",
			"info_hash", infoHash[:minStr(len(infoHash), 8)],
			"peers", len(peers), "error", err)
	}
}

// groupTrackersByTier sorts the tracker list into per-tier groups, ordered
// from lowest tier (highest priority) upwards. Trackers with the same tier
// number share a tier; we treat each tier as a fallback group per BEP-12.
func groupTrackersByTier(trackers []ltclient.TrackerInfo) [][]string {
	if len(trackers) == 0 {
		return nil
	}
	tierMap := make(map[int][]string)
	for _, t := range trackers {
		tierMap[t.Tier] = append(tierMap[t.Tier], t.URL)
	}
	// Sort keys ascending.
	keys := make([]int, 0, len(tierMap))
	for k := range tierMap {
		keys = append(keys, k)
	}
	// Insertion sort — tier counts are tiny in practice (1-3 typically).
	for i := 1; i < len(keys); i++ {
		for j := i; j > 0 && keys[j-1] > keys[j]; j-- {
			keys[j-1], keys[j] = keys[j], keys[j-1]
		}
	}
	out := make([][]string, 0, len(keys))
	for _, k := range keys {
		out = append(out, tierMap[k])
	}
	return out
}

// sleepOrStop waits for `d`, the loop's stop signal, or the parent context
// cancellation. Returns true if the wait completed normally (loop should
// continue), false if cancelled.
func sleepOrStop(stop chan struct{}, ctx context.Context, d time.Duration) bool {
	select {
	case <-stop:
		return false
	case <-ctx.Done():
		return false
	case <-time.After(d):
		return true
	}
}
