package engine

import (
	"fmt"
	"testing"

	"github.com/Kheopsian/hydra/internal/config"
	"github.com/Kheopsian/hydra/internal/engine/ltclient"
)

// stubSlotClient is a minimal EngineClient that keeps an in-memory torrent list
// and applies start/stop to it, so enforceDownloadSlots can be run tick after
// tick against a stable world.
type stubSlotClient struct {
	torrents map[string]*ltclient.TorrentStatus
	order    []string
	starts   int
	stops    int
}

func (c *stubSlotClient) ListTorrents() (*ltclient.ListTorrentsResult, error) {
	out := make([]ltclient.TorrentStatus, 0, len(c.order))
	for _, ih := range c.order {
		out = append(out, *c.torrents[ih])
	}
	return &ltclient.ListTorrentsResult{Torrents: out}, nil
}

func (c *stubSlotClient) StartTorrent(infoHash string) error {
	t, ok := c.torrents[infoHash]
	if !ok {
		return fmt.Errorf("unknown torrent %s", infoHash)
	}
	if t.State != "downloading" {
		c.starts++
	}
	t.State, t.IsPaused = "downloading", false
	return nil
}

func (c *stubSlotClient) StopTorrent(infoHash string) error {
	t, ok := c.torrents[infoHash]
	if !ok {
		return fmt.Errorf("unknown torrent %s", infoHash)
	}
	if t.State == "downloading" {
		c.stops++
	}
	t.State, t.IsPaused = "stopped", true
	return nil
}

// Unused by the slot manager.
func (c *stubSlotClient) Ping() error                            { return nil }
func (c *stubSlotClient) Close() error                           { return nil }
func (c *stubSlotClient) SetEventHandler(func(ltclient.Event))   {}
func (c *stubSlotClient) SubscribeEvents() error                 { return nil }
func (c *stubSlotClient) RemoveTorrent(string, bool) error       { return nil }
func (c *stubSlotClient) SetSavePath(string, string) error       { return nil }
func (c *stubSlotClient) VerifyTorrent(string) error             { return nil }
func (c *stubSlotClient) AddTorrent(string, string, bool) (*ltclient.AddTorrentResult, error) {
	return nil, nil
}
func (c *stubSlotClient) AddTorrentWithOptions(string, string, bool, bool) (*ltclient.AddTorrentResult, error) {
	return nil, nil
}
func (c *stubSlotClient) GetStatus(string) (*ltclient.TorrentStatus, error)   { return nil, nil }
func (c *stubSlotClient) GetPeers(string) ([]ltclient.PeerInfo, error)        { return nil, nil }
func (c *stubSlotClient) GetSessionStats() (*ltclient.SessionStats, error)    { return nil, nil }
func (c *stubSlotClient) GetFiles(string) ([]ltclient.FileInfo, error)        { return nil, nil }
func (c *stubSlotClient) GetTrackers(string) ([]ltclient.TrackerInfo, error)  { return nil, nil }
func (c *stubSlotClient) GetDiagnostics() (*ltclient.DiagnosticStats, error)  { return nil, nil }
func (c *stubSlotClient) AddPeers(string, []struct {
	IP   string
	Port int
}) error {
	return nil
}

func newSlotTestEngine(t *testing.T, maxSlots, torrents int) (*HoardEngine, *stubSlotClient) {
	t.Helper()
	e := NewHoardEngine(&config.SessionConfig{ActiveDownloads: maxSlots}, t.TempDir())
	c := &stubSlotClient{torrents: make(map[string]*ltclient.TorrentStatus)}
	for i := 0; i < torrents; i++ {
		ih := fmt.Sprintf("%040x", i)
		c.torrents[ih] = &ltclient.TorrentStatus{
			InfoHash:  ih,
			State:     "stopped",
			IsPaused:  true,
			Progress:  0.1,
			TotalSize: 1 << 30,
			TotalDone: 1 << 20,
		}
		c.order = append(c.order, ih)
	}
	e.SetClient(c)
	return e, c
}

// A slot pool must not reshuffle between consecutive ticks. Prod ran a pool of
// 2000 slots over 20k incomplete torrents and swapped up to 1800 of them every
// 30s: the seed-rank sort was unstable and almost every torrent was tied at
// zero scrape seeds, so the "top N" was redrawn on every tick and no download
// ever survived long enough to connect.
func TestEnforceDownloadSlotsIsStableAcrossTicks(t *testing.T) {
	const maxSlots = 500
	e, c := newSlotTestEngine(t, maxSlots, 5000)

	e.enforceDownloadSlots()
	if got := e.GetDownloadSlotStatus().ActiveSlots; got != maxSlots {
		t.Fatalf("first tick: filled %d slots, want %d", got, maxSlots)
	}
	firstSet := activeSet(c)

	// Nothing about the world changed, so nothing about the selection should.
	for tick := 2; tick <= 5; tick++ {
		c.starts, c.stops = 0, 0
		e.enforceDownloadSlots()
		st := e.GetDownloadSlotStatus()
		if c.starts != 0 || c.stops != 0 {
			t.Errorf("tick %d churned: %d started, %d stopped (want 0/0); slot stats %+v",
				tick, c.starts, c.stops, st)
		}
		if got := len(activeSet(c)); got != maxSlots {
			t.Errorf("tick %d: %d active, want %d", tick, got, maxSlots)
		}
	}
	for ih := range firstSet {
		if c.torrents[ih].State != "downloading" {
			t.Fatalf("torrent %s lost its slot without cause", ih)
			break
		}
	}
}

// Pinned torrents hold a slot regardless of rank, and the pool still fills.
func TestEnforceDownloadSlotsHonoursPins(t *testing.T) {
	const maxSlots = 10
	e, c := newSlotTestEngine(t, maxSlots, 500)

	// Last by info_hash order — it would never make the seed-rank cut.
	pinned := c.order[len(c.order)-1]
	e.PinTorrent(pinned)

	e.enforceDownloadSlots()
	if c.torrents[pinned].State != "downloading" {
		t.Errorf("pinned torrent %s did not get a slot", pinned)
	}
	if got := len(activeSet(c)); got != maxSlots {
		t.Errorf("filled %d slots, want %d", got, maxSlots)
	}
}

// maxSlots = 0 parks everything; a negative value disables the manager.
func TestEnforceDownloadSlotsZeroAndDisabled(t *testing.T) {
	e, c := newSlotTestEngine(t, 5, 50)
	e.enforceDownloadSlots()
	if got := len(activeSet(c)); got != 5 {
		t.Fatalf("filled %d slots, want 5", got)
	}

	e.SetDownloadSlotsOverride(0)
	e.enforceDownloadSlots()
	if got := len(activeSet(c)); got != 0 {
		t.Errorf("maxSlots=0: %d still active, want 0", got)
	}

	e.SetDownloadSlotsOverride(-1) // -1 means "no override", falls back to config
	e.ClearDownloadSlotsOverride()
	e.enforceDownloadSlots()
	if got := len(activeSet(c)); got != 5 {
		t.Errorf("after clearing override: %d active, want 5", got)
	}
}

func activeSet(c *stubSlotClient) map[string]bool {
	out := make(map[string]bool)
	for ih, t := range c.torrents {
		if t.State == "downloading" {
			out[ih] = true
		}
	}
	return out
}
