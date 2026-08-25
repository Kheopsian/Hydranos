package engine

import (
	"sync/atomic"

	"github.com/Kheopsian/hydra/internal/engine/ltclient"
)

// EngineRef is a stable handle on an engine's client, whichever process is
// currently behind it.
//
// It exists because a client does NOT survive its process. ltclient dials its
// two unix sockets once in Connect and never redials: callVia writes to the
// stored conn and only checks a local `closed` flag that nothing sets when the
// far end dies. EngineProcess holds exactly one client, created with it, so
// restarting an engine produces a new client and leaves every previously handed
// out copy pointing at a closed socket -- silently. The holder still believes
// it is connected; the writes simply go nowhere.
//
// That matters because nineteen places take a copy: the engine objects, the
// agent server's engine map, the local agent registration, the qBittorrent
// shim, the start-paused pass, an adopt closure, and -- worst -- the tracker
// announcers, which would go on announcing into a dead socket while the engine
// runs perfectly beside them. No error, no log, and the trackers stop hearing
// from us.
//
// So holders take a ref instead of a client, and a reload swaps what is inside
// it. Everyone follows, because nobody kept a copy.
type EngineRef struct {
	cur atomic.Pointer[EngineClient]
}

// NewEngineRef returns a ref already pointing at c.
func NewEngineRef(c EngineClient) *EngineRef {
	r := &EngineRef{}
	r.Swap(c)
	return r
}

// Swap installs the client of a new generation. Returns the previous one so the
// caller can close it: the ref deliberately does not, because whoever started
// the process owns its lifetime.
func (r *EngineRef) Swap(c EngineClient) EngineClient {
	old := r.get()
	r.cur.Store(&c)
	return old
}

// get returns the current client, or nil before the first Swap.
func (r *EngineRef) get() EngineClient {
	if p := r.cur.Load(); p != nil {
		return *p
	}
	return nil
}

// Current exposes the live client for the few callers that genuinely need the
// concrete value rather than the behaviour -- passing it to something that will
// hold it only for the duration of one call.
func (r *EngineRef) Current() EngineClient { return r.get() }

// errNoClient is returned rather than panicking when a ref is used before its
// first client is installed. A nil dereference here would take the daemon down
// during a reload window; an error is something a caller can report.
type refNotReady struct{}

func (refNotReady) Error() string { return "engine: no client installed on this ref yet" }

// ---- EngineClient, delegated ----

func (r *EngineRef) Ping() error {
	c := r.get()
	if c == nil {
		return refNotReady{}
	}
	return c.Ping()
}
func (r *EngineRef) Close() error {
	c := r.get()
	if c == nil {
		return nil
	}
	return c.Close()
}
func (r *EngineRef) SetEventHandler(h func(ltclient.Event)) {
	if c := r.get(); c != nil {
		c.SetEventHandler(h)
	}
}
func (r *EngineRef) SubscribeEvents() error {
	c := r.get()
	if c == nil {
		return refNotReady{}
	}
	return c.SubscribeEvents()
}
func (r *EngineRef) AddTorrent(torrentPath, savePath string, stopped bool) (*ltclient.AddTorrentResult, error) {
	c := r.get()
	if c == nil {
		return nil, refNotReady{}
	}
	return c.AddTorrent(torrentPath, savePath, stopped)
}
func (r *EngineRef) AddTorrentWithOptions(torrentPath, savePath string, stopped, seedMode bool) (*ltclient.AddTorrentResult, error) {
	c := r.get()
	if c == nil {
		return nil, refNotReady{}
	}
	return c.AddTorrentWithOptions(torrentPath, savePath, stopped, seedMode)
}
func (r *EngineRef) FetchMetadata(infoHash string, trackers, peers []string, bindingID *uint32) (*ltclient.FetchMetadataResult, error) {
	c := r.get()
	if c == nil {
		return nil, refNotReady{}
	}
	return c.FetchMetadata(infoHash, trackers, peers, bindingID)
}
func (r *EngineRef) GetMetadata(infoHash string) (*ltclient.GetMetadataResult, error) {
	c := r.get()
	if c == nil {
		return nil, refNotReady{}
	}
	return c.GetMetadata(infoHash)
}
func (r *EngineRef) RemoveTorrent(infoHash string, keepData bool) error {
	c := r.get()
	if c == nil {
		return refNotReady{}
	}
	return c.RemoveTorrent(infoHash, keepData)
}
func (r *EngineRef) StartTorrent(infoHash string) error {
	c := r.get()
	if c == nil {
		return refNotReady{}
	}
	return c.StartTorrent(infoHash)
}
func (r *EngineRef) StopTorrent(infoHash string) error {
	c := r.get()
	if c == nil {
		return refNotReady{}
	}
	return c.StopTorrent(infoHash)
}
func (r *EngineRef) SetSavePath(infoHash, savePath string) error {
	c := r.get()
	if c == nil {
		return refNotReady{}
	}
	return c.SetSavePath(infoHash, savePath)
}
func (r *EngineRef) VerifyTorrent(infoHash string) error {
	c := r.get()
	if c == nil {
		return refNotReady{}
	}
	return c.VerifyTorrent(infoHash)
}
func (r *EngineRef) ExportState(infoHash string) (*ltclient.ResumeRecord, error) {
	c := r.get()
	if c == nil {
		return nil, refNotReady{}
	}
	return c.ExportState(infoHash)
}
func (r *EngineRef) ImportState(rec *ltclient.ResumeRecord) (string, error) {
	c := r.get()
	if c == nil {
		return "", refNotReady{}
	}
	return c.ImportState(rec)
}
func (r *EngineRef) GetStatus(infoHash string) (*ltclient.TorrentStatus, error) {
	c := r.get()
	if c == nil {
		return nil, refNotReady{}
	}
	return c.GetStatus(infoHash)
}
func (r *EngineRef) ListTorrents() (*ltclient.ListTorrentsResult, error) {
	c := r.get()
	if c == nil {
		return nil, refNotReady{}
	}
	return c.ListTorrents()
}
func (r *EngineRef) ListTorrentsSlim() (*ltclient.ListTorrentsResult, error) {
	c := r.get()
	if c == nil {
		return nil, refNotReady{}
	}
	return c.ListTorrentsSlim()
}
func (r *EngineRef) GetPeers(infoHash string) ([]ltclient.PeerInfo, error) {
	c := r.get()
	if c == nil {
		return nil, refNotReady{}
	}
	return c.GetPeers(infoHash)
}
func (r *EngineRef) GetSessionStats() (*ltclient.SessionStats, error) {
	c := r.get()
	if c == nil {
		return nil, refNotReady{}
	}
	return c.GetSessionStats()
}
func (r *EngineRef) GetFiles(infoHash string) ([]ltclient.FileInfo, error) {
	c := r.get()
	if c == nil {
		return nil, refNotReady{}
	}
	return c.GetFiles(infoHash)
}
func (r *EngineRef) GetAvailability(infoHash string) (*ltclient.Availability, error) {
	c := r.get()
	if c == nil {
		return nil, refNotReady{}
	}
	return c.GetAvailability(infoHash)
}
func (r *EngineRef) SetEngineOptFlag(name string, on bool, value int64) (map[string]interface{}, error) {
	c := r.get()
	if c == nil {
		return nil, refNotReady{}
	}
	return c.SetEngineOptFlag(name, on, value)
}
func (r *EngineRef) EngineOptFlags() (map[string]interface{}, error) {
	c := r.get()
	if c == nil {
		return nil, refNotReady{}
	}
	return c.EngineOptFlags()
}
func (r *EngineRef) GetTrackers(infoHash string) ([]ltclient.TrackerInfo, error) {
	c := r.get()
	if c == nil {
		return nil, refNotReady{}
	}
	return c.GetTrackers(infoHash)
}
func (r *EngineRef) SetTrackers(infoHash string, tiers [][]string) ([][]string, error) {
	c := r.get()
	if c == nil {
		return nil, refNotReady{}
	}
	return c.SetTrackers(infoHash, tiers)
}
func (r *EngineRef) GetDiagnostics() (*ltclient.DiagnosticStats, error) {
	c := r.get()
	if c == nil {
		return nil, refNotReady{}
	}
	return c.GetDiagnostics()
}
func (r *EngineRef) AddPeers(infoHash string, peers []struct {
	IP   string
	Port int
}) error {
	c := r.get()
	if c == nil {
		return refNotReady{}
	}
	return c.AddPeers(infoHash, peers)
}

// Compile-time proof the ref is substitutable for the thing it wraps. Without
// it a method added to EngineClient would be silently missing here and every
// holder would fall back to the concrete client again.
var _ EngineClient = (*EngineRef)(nil)
