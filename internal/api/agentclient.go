package api

import (
	"time"

	"github.com/Kheopsian/hydra/internal/agentwire"
	"github.com/Kheopsian/hydra/internal/engine/grpcclient"
	"github.com/Kheopsian/hydra/internal/engine/ltclient"
	"github.com/Kheopsian/hydra/internal/jobs"
)

// AgentClient is everything the API asks of one engine reached through an
// agent. It is deliberately the WHOLE surface rather than a convenient subset:
// the point is that a caller can no longer tell a local engine from a remote
// one, and a subset would force the local case back into a branch somewhere.
//
// Why this exists: the registry used to hold a concrete *grpcclient.Client, so
// "the local engines" could not be agents and were instead a special case --
// LocalAgentName, `if agent == "local"` in the placement, a hidden entry in
// /api/agents, and a loopback gRPC agent ("local-shards") for anything beyond
// the first two. Three ways to address the same thing.
//
// Note what this interface is NOT: a transport decision. Measured at 198k
// torrents, routing a local list through the agent WIRE costs 1.68 s and 954 MB
// per call, against 376 ns and zero allocation in process, because the wire
// JSON-encodes every reply (internal/agent/server.go reply()). So a local agent
// must implement this interface by calling the engine directly, never by
// talking to itself over a socket. "1 agent = 1 engine" is an addressing
// change; the hot path does not move.
type AgentClient interface {
	// Agent-level: about the node, not a torrent.
	Ping() error
	ListEngines() ([]agentwire.EngineDescriptor, error)
	NodeInfo() (agentwire.NodeInfo, error)
	ApplyConfig(p agentwire.ApplyConfigParams) (agentwire.ConfigState, error)
	GetConfigState() (agentwire.ConfigState, error)
	SetAnnounceOverride(p agentwire.AnnounceOverrideParams) error
	DiskFree(path string) (int64, error)

	// Relocating a payload where it lives. Only the node holding the files can
	// plan or move them: planning from here measured an agent's paths against
	// this host's, which is why a category change never moved an agent's data.
	MovePlan(engineID, infoHash, targetDir string) (agentwire.MovePlan, error)
	MovePayload(p agentwire.MovePayloadParams) (agentwire.MoveStatus, error)
	MoveStatus(infoHash string) (agentwire.MoveStatus, error)
	Close() error

	// Listing and stats.
	ListTorrents() (*ltclient.ListTorrentsResult, error)
	ListTorrentsSlim() (*ltclient.ListTorrentsResult, error)
	ListTorrentsTimeout(d time.Duration) (*ltclient.ListTorrentsResult, error)
	GetSessionStats() (*ltclient.SessionStats, error)
	TorrentCategories(engineID string) (map[string]string, error)

	// Per-torrent reads.
	GetStatus(infoHash string) (*ltclient.TorrentStatus, error)
	GetPeers(infoHash string) ([]ltclient.PeerInfo, error)
	GetFiles(infoHash string) ([]ltclient.FileInfo, error)
	GetTrackers(infoHash string) ([]ltclient.TrackerInfo, error)
	GetAvailability(infoHash string) (*ltclient.Availability, error)
	GetMetadata(infoHash string) (*ltclient.GetMetadataResult, error)
	FetchMetadata(infoHash string, trackers, peers []string, bindingID *uint32) (*ltclient.FetchMetadataResult, error)

	TrackerSnapshot() ([]agentwire.TrackerStatWire, error)

	// Mutations.
	StartTorrent(infoHash string) error
	StopTorrent(infoHash string) error
	SetTrackers(infoHash string, tiers [][]string) ([][]string, error)
	SetCategoryLabel(engineID, infoHash, category string) error
	ActionRouted(mode, action, infoHash string, deleteFiles bool, category, savePath string) error
	AddRouted(mode, torrentPath, savePath, category string, createFolder *bool, skipRecheck bool) (*ltclient.AddTorrentResult, error)
	RemoveTorrent(infoHash string, keepData bool) error

	// Piece transfer. Present so that jobs.PieceSource and jobs.PieceSink are
	// satisfied by anything reachable as an agent -- which is what makes a move
	// between two engines of the SAME node expressible once the local engines
	// are agents. That move is inexpressible today: RemoteMoveParams names a
	// source and a target AGENT, and a monolith has exactly one.
	ExportState(infoHash string) (*ltclient.ResumeRecord, error)
	ImportStateWithFile(rec *ltclient.ResumeRecord, torrent []byte) (string, error)
	GetTorrentFile(infoHash string) ([]byte, error)
	ReadPiece(infoHash string, piece int) ([]byte, error)
	WritePiece(infoHash string, piece int, data []byte) error
	VerifyTorrent(infoHash string) error
}

// The job interfaces must stay satisfiable by an agent, or a move loses the
// ability to name one. Asserted here rather than discovered at a call site.
var (
	_ jobs.PieceSource = (AgentClient)(nil)
	_ jobs.PieceSink   = (AgentClient)(nil)
)

// The gRPC client is the only implementation for now. This assertion is the
// whole safety of the change: if a method drifts, the build breaks here rather
// than at some call site months later.
var _ AgentClient = (*grpcclient.Client)(nil)
