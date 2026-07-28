package choking

import (
	"context"
	"log/slog"
	"sort"
	"sync"
	"time"

	"github.com/Kheopsian/hydra/internal/config"
)

// ---------------------------------------------------------------------------
// Data structures
// ---------------------------------------------------------------------------

// PeerScore holds the computed scoring breakdown for a single peer.
type PeerScore struct {
	IP                 string  `json:"ip"`
	Port               string  `json:"port"`
	UploadRate         float64 `json:"upload_rate"`
	DownloadRate       float64 `json:"download_rate"`
	NumPieces          int     `json:"num_pieces"`
	TotalPieces        int     `json:"total_pieces"`
	ConnectionDuration float64 `json:"connection_duration"`
	RarityScore        float64 `json:"rarity_score"`
	SpeedScore         float64 `json:"speed_score"`
	IntelScore         float64 `json:"intel_score"`
	FinalScore         float64 `json:"final_score"`
}

// TorrentChokingState holds the choking state for a single torrent.
type TorrentChokingState struct {
	InfoHash      string          `json:"info_hash"`
	PeerScores    []PeerScore     `json:"peer_scores"`
	UnchokedPeers map[string]bool `json:"unchoked_peers"` // ip:port -> unchoked
	LastTick      time.Time       `json:"last_tick"`
}

// ---------------------------------------------------------------------------
// ChokingEngine
// ---------------------------------------------------------------------------

// ChokingEngine implements the rarity_captive choking strategy.
// It scores peers by rarity (how few pieces they have), upload speed,
// and historical intel, then unchokes the top N to maximise ratio.
type ChokingEngine struct {
	config *config.RaceChokingConfig

	mu       sync.RWMutex
	torrents map[string]*TorrentChokingState // info_hash -> state

	// Stats counters.
	totalTicks    int64
	totalUnchokes int64
	totalChokes   int64

	running bool
	ctx     context.Context
	cancel  context.CancelFunc
}

// NewChokingEngine creates a new choking engine.
func NewChokingEngine(cfg *config.RaceChokingConfig) *ChokingEngine {
	return &ChokingEngine{
		config:   cfg,
		torrents: make(map[string]*TorrentChokingState),
	}
}

// RegisterTorrent starts tracking a torrent for choking decisions.
func (ce *ChokingEngine) RegisterTorrent(infoHash string) {
	ce.mu.Lock()
	defer ce.mu.Unlock()

	ce.torrents[infoHash] = &TorrentChokingState{
		InfoHash:      infoHash,
		UnchokedPeers: make(map[string]bool),
		LastTick:      time.Now(),
	}

	slog.Debug("choking: registered torrent", "info_hash", infoHash)
}

// UnregisterTorrent stops tracking a torrent.
func (ce *ChokingEngine) UnregisterTorrent(infoHash string) {
	ce.mu.Lock()
	defer ce.mu.Unlock()

	delete(ce.torrents, infoHash)
	slog.Debug("choking: unregistered torrent", "info_hash", infoHash)
}

// Start launches the choking tick loop.
func (ce *ChokingEngine) Start(ctx context.Context) error {
	ce.ctx, ce.cancel = context.WithCancel(ctx)
	ce.running = true

	tickInterval := time.Duration(ce.config.TickIntervalSeconds * float64(time.Second))
	if tickInterval <= 0 {
		tickInterval = 2 * time.Second
	}

	slog.Info("choking: engine started",
		"strategy", ce.config.Strategy,
		"max_unchoked", ce.config.MaxUnchoked,
		"tick_interval", tickInterval,
		"rarity_weight", ce.config.RarityWeight,
		"speed_weight", ce.config.SpeedWeight,
	)

	go ce.tickLoop(tickInterval)

	return nil
}

// Stop shuts down the choking engine.
func (ce *ChokingEngine) Stop() {
	if !ce.running {
		return
	}
	slog.Info("choking: engine stopping")

	ce.cancel()
	ce.running = false

	slog.Info("choking: engine stopped",
		"total_ticks", ce.totalTicks,
		"total_unchokes", ce.totalUnchokes,
		"total_chokes", ce.totalChokes,
	)
}

// GetStats returns a snapshot of the choking engine's internal state.
func (ce *ChokingEngine) GetStats() map[string]interface{} {
	ce.mu.RLock()
	defer ce.mu.RUnlock()

	totalPeers := 0
	totalUnchoked := 0
	for _, ts := range ce.torrents {
		totalPeers += len(ts.PeerScores)
		for _, unchoked := range ts.UnchokedPeers {
			if unchoked {
				totalUnchoked++
			}
		}
	}

	return map[string]interface{}{
		"strategy":       ce.config.Strategy,
		"max_unchoked":   ce.config.MaxUnchoked,
		"torrents":       len(ce.torrents),
		"total_peers":    totalPeers,
		"total_unchoked": totalUnchoked,
		"total_ticks":    ce.totalTicks,
		"running":        ce.running,
	}
}

// ---------------------------------------------------------------------------
// Tick loop
// ---------------------------------------------------------------------------

func (ce *ChokingEngine) tickLoop(interval time.Duration) {
	ticker := time.NewTicker(interval)
	defer ticker.Stop()

	for {
		select {
		case <-ce.ctx.Done():
			return
		case <-ticker.C:
			ce.tick()
		}
	}
}

func (ce *ChokingEngine) tick() {
	ce.mu.Lock()
	defer ce.mu.Unlock()

	ce.totalTicks++

	for _, ts := range ce.torrents {
		ce.tickTorrent(ts)
		ts.LastTick = time.Now()
	}
}

func (ce *ChokingEngine) tickTorrent(ts *TorrentChokingState) {
	// TODO: Pull live peer list from rain torrent handle.
	// For now, we operate on whatever PeerScores have been set.
	if len(ts.PeerScores) == 0 {
		return
	}

	// Find max speed for normalisation.
	var maxSpeed float64
	for i := range ts.PeerScores {
		if ts.PeerScores[i].UploadRate > maxSpeed {
			maxSpeed = ts.PeerScores[i].UploadRate
		}
	}

	// Score each peer.
	for i := range ts.PeerScores {
		p := &ts.PeerScores[i]

		p.RarityScore = ComputeRarityScore(
			peerCompletion(p.NumPieces, p.TotalPieces),
			p.ConnectionDuration,
		)
		p.SpeedScore = ComputeSpeedScore(p.UploadRate, maxSpeed)

		p.FinalScore = ComputeFinalScore(
			p.RarityScore,
			p.SpeedScore,
			p.IntelScore,
			ce.config.RarityWeight,
			ce.config.SpeedWeight,
		)
	}

	// Sort by final score descending.
	sort.Slice(ts.PeerScores, func(i, j int) bool {
		return ts.PeerScores[i].FinalScore > ts.PeerScores[j].FinalScore
	})

	// Determine unchoke set.
	maxUnchoked := ce.config.MaxUnchoked
	if maxUnchoked <= 0 {
		maxUnchoked = 30
	}

	newUnchoked := make(map[string]bool, maxUnchoked)
	for i, p := range ts.PeerScores {
		key := p.IP + ":" + p.Port
		if i < maxUnchoked {
			newUnchoked[key] = true
		}
	}

	// Count choke/unchoke transitions.
	for key := range newUnchoked {
		if !ts.UnchokedPeers[key] {
			ce.totalUnchokes++
			// TODO: Actually unchoke the peer via rain API.
		}
	}
	for key := range ts.UnchokedPeers {
		if !newUnchoked[key] {
			ce.totalChokes++
			// TODO: Actually choke the peer via rain API.
		}
	}

	ts.UnchokedPeers = newUnchoked
}

// peerCompletion returns the fraction of pieces a peer has (0.0–1.0).
func peerCompletion(numPieces, totalPieces int) float64 {
	if totalPieces <= 0 {
		return 0
	}
	return float64(numPieces) / float64(totalPieces)
}
