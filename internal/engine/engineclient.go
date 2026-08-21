package engine

import "github.com/Kheopsian/hydra/internal/engine/ltclient"

// EngineClient is the minimal surface both engine implementations must
// satisfy. Keeping this interface narrow means we don't need to change
// call sites when swapping engines — hoard.go/race.go use only these
// methods against e.client.
//
// Both `*ltclient.Client` (Typhon via Unix-socket JSON-RPC) and
// `*rqbitclient.Client` (rqbit via HTTP REST) satisfy this interface
// by sharing the same method signatures. Return types come from the
// ltclient package; rqbitclient re-exports them for compat.
// tiersFromTrackerInfo regroups the engine's flat tracker list back into tiers.
// The engine reports one entry per URL with its tier index; the index is the
// grouping key, so a tier holding two URLs comes back as one tier of two and a
// read-modify-write round trip keeps the fallbacks it started with.
func tiersFromTrackerInfo(infos []ltclient.TrackerInfo) [][]string {
	if len(infos) == 0 {
		return [][]string{}
	}
	maxTier := 0
	for _, in := range infos {
		if in.Tier > maxTier {
			maxTier = in.Tier
		}
	}
	buckets := make([][]string, maxTier+1)
	for _, in := range infos {
		if in.URL == "" || in.Tier < 0 {
			continue
		}
		buckets[in.Tier] = append(buckets[in.Tier], in.URL)
	}
	out := make([][]string, 0, len(buckets))
	for _, b := range buckets {
		if len(b) > 0 {
			out = append(out, b)
		}
	}
	return out
}

type EngineClient interface {
	// Lifecycle
	Ping() error
	Close() error
	SetEventHandler(handler func(ltclient.Event))
	// SubscribeEvents opts in to push-based event stream (typhon only;
	// rqbit has a compat no-op). Must be called after SetEventHandler.
	SubscribeEvents() error

	// Torrent lifecycle
	AddTorrent(torrentPath, savePath string, stopped bool) (*ltclient.AddTorrentResult, error)
	AddTorrentWithOptions(torrentPath, savePath string, stopped, seedMode bool) (*ltclient.AddTorrentResult, error)
	// FetchMetadata starts resolving a magnet's info dict from the swarm and
	// returns immediately; GetMetadata polls it. Resolution runs wherever the
	// engine lives, so a remote agent resolves on its own network.
	FetchMetadata(infoHash string, trackers, peers []string, bindingID *uint32) (*ltclient.FetchMetadataResult, error)
	GetMetadata(infoHash string) (*ltclient.GetMetadataResult, error)
	RemoveTorrent(infoHash string, keepData bool) error
	StartTorrent(infoHash string) error
	StopTorrent(infoHash string) error
	// SetSavePath swaps the engine's in-memory save_path for a torrent and
	// flushes fastresume. Files must already have been moved on disk.
	SetSavePath(infoHash, savePath string) error
	VerifyTorrent(infoHash string) error
	// ExportState/ImportState carry a torrent's durable state between two
	// engines. The record is the same one the engine persists, so a torrent
	// that changes engine is indistinguishable from one that restarted.
	ExportState(infoHash string) (*ltclient.ResumeRecord, error)
	ImportState(rec *ltclient.ResumeRecord) (string, error)

	// Queries
	GetStatus(infoHash string) (*ltclient.TorrentStatus, error)
	ListTorrents() (*ltclient.ListTorrentsResult, error)
	// ListTorrentsSlim returns the same rows with only the fields the
	// scheduling loops read populated: info hash, state, progress, total size,
	// bytes done, download rate, paused and finished. Everything else is zero.
	// Callers must have been checked against that list. An implementation is
	// always free to answer with the full listing -- the slim set is a subset,
	// so a caller cannot tell the difference.
	ListTorrentsSlim() (*ltclient.ListTorrentsResult, error)
	GetPeers(infoHash string) ([]ltclient.PeerInfo, error)
	GetSessionStats() (*ltclient.SessionStats, error)
	GetFiles(infoHash string) ([]ltclient.FileInfo, error)
	GetAvailability(infoHash string) (*ltclient.Availability, error)
	SetEngineOptFlag(name string, on bool, value int64) (map[string]interface{}, error)
	EngineOptFlags() (map[string]interface{}, error)
	GetTrackers(infoHash string) ([]ltclient.TrackerInfo, error)
	SetTrackers(infoHash string, tiers [][]string) ([][]string, error)
	GetDiagnostics() (*ltclient.DiagnosticStats, error)

	// Peer injection (patched rqbit endpoint; native in Typhon).
	AddPeers(infoHash string, peers []struct {
		IP   string
		Port int
	}) error
}

// Engine IDs recognised by StartEngineProcess.
const (
	EngineTyphon = "typhon"
)
