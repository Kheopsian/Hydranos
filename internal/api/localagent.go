package api

import (
	"time"

	"github.com/Kheopsian/hydra/internal/agentwire"
	"github.com/Kheopsian/hydra/internal/engine"
	"github.com/Kheopsian/hydra/internal/engine/ltclient"
)

// localAgentClient makes an engine running in THIS process reachable under the
// same interface as one on another machine. It is what lets the local engines
// stop being a special case ("local" hardcoded in the placement, hidden from
// /api/agents, a loopback agent for shards) and simply be agents.
//
// It is deliberately a HYBRID, and the split is not arbitrary:
//
//   - Everything on the hot path -- listing, stats, per-torrent reads -- goes
//     STRAIGHT to the in-process engine client. Measured at 198k torrents, one
//     list through the agent wire costs 1.68 s and 954 MB against 376 ns and no
//     allocation in process, because the wire JSON-encodes every reply. Routing
//     those through a socket would undo the whole 2026-08 allocation work.
//
//   - The agent-level operations -- node info, config, routed add/action, disk
//     free -- go through the process's own agent server. They are cold (called
//     per user action, not per refresh) and that code is already carrying the
//     shard traffic in production. Re-implementing eleven of them by hand here
//     would be eleven chances to behave subtly differently from a remote agent,
//     which is precisely the divergence this whole change exists to remove.
//
// So: performance where it is measured to matter, proven code everywhere else.
type localAgentClient struct {
	// eng is the in-process client for this engine (ltclient over the engine's
	// own unix socket, exactly what the monolith uses today).
	eng engine.EngineClient
	// agent is this process's own agent server, reached the way the shards
	// already reach it. Cold calls only.
	agent AgentClient
	// id is the engine id this client stands for, needed by the routed calls
	// that take one explicitly.
	id string
}

func newLocalAgentClient(id string, eng engine.EngineClient, agent AgentClient) *localAgentClient {
	return &localAgentClient{eng: eng, agent: agent, id: id}
}

// NewLocalAgentClient exposes the constructor to the process that owns the
// engines. Assembly lives there, not here, so this package never has to import
// internal/agent: the caller builds the cold-path client from
// agent.InProcessStub + grpcclient.NewWithStub and hands it in.
func NewLocalAgentClient(id string, eng engine.EngineClient, cold AgentClient) AgentClient {
	return newLocalAgentClient(id, eng, cold)
}

// ---- hot path: straight to the engine, no encoding ----

func (l *localAgentClient) ListTorrents() (*ltclient.ListTorrentsResult, error) {
	return l.eng.ListTorrents()
}
func (l *localAgentClient) ListTorrentsSlim() (*ltclient.ListTorrentsResult, error) {
	return l.eng.ListTorrentsSlim()
}

// ListTorrentsTimeout ignores the deadline on purpose: it exists to bound a
// NETWORK call, and there is no network here. Honouring it would mean wrapping
// an in-process call in a goroutine and a timer per list -- cost and complexity
// to enforce a limit that cannot be hit.
func (l *localAgentClient) ListTorrentsTimeout(time.Duration) (*ltclient.ListTorrentsResult, error) {
	return l.eng.ListTorrents()
}

func (l *localAgentClient) GetSessionStats() (*ltclient.SessionStats, error) {
	return l.eng.GetSessionStats()
}
func (l *localAgentClient) GetStatus(h string) (*ltclient.TorrentStatus, error) {
	return l.eng.GetStatus(h)
}
func (l *localAgentClient) GetPeers(h string) ([]ltclient.PeerInfo, error) { return l.eng.GetPeers(h) }
func (l *localAgentClient) GetFiles(h string) ([]ltclient.FileInfo, error) { return l.eng.GetFiles(h) }
func (l *localAgentClient) GetTrackers(h string) ([]ltclient.TrackerInfo, error) {
	return l.eng.GetTrackers(h)
}
func (l *localAgentClient) GetAvailability(h string) (*ltclient.Availability, error) {
	return l.eng.GetAvailability(h)
}
func (l *localAgentClient) GetMetadata(h string) (*ltclient.GetMetadataResult, error) {
	return l.eng.GetMetadata(h)
}
func (l *localAgentClient) FetchMetadata(h string, tr, pr []string, b *uint32) (*ltclient.FetchMetadataResult, error) {
	return l.eng.FetchMetadata(h, tr, pr, b)
}
func (l *localAgentClient) ExportState(h string) (*ltclient.ResumeRecord, error) {
	return l.eng.ExportState(h)
}
func (l *localAgentClient) StartTorrent(h string) error { return l.eng.StartTorrent(h) }
func (l *localAgentClient) StopTorrent(h string) error  { return l.eng.StopTorrent(h) }
func (l *localAgentClient) VerifyTorrent(h string) error {
	return l.eng.VerifyTorrent(h)
}
func (l *localAgentClient) RemoveTorrent(h string, keepData bool) error {
	return l.eng.RemoveTorrent(h, keepData)
}
func (l *localAgentClient) SetTrackers(h string, tiers [][]string) ([][]string, error) {
	return l.eng.SetTrackers(h, tiers)
}
func (l *localAgentClient) Ping() error { return l.eng.Ping() }

// Close is a no-op. The engine client is the process's own, shared by every
// caller; closing it because one agent entry went away would tear the monolith
// off its engine. Connection lifetime belongs to whoever dialled -- the same
// rule the job interfaces already state.
func (l *localAgentClient) Close() error { return nil }

// ---- cold path: the process's own agent server ----

func (l *localAgentClient) NodeInfo() (agentwire.NodeInfo, error) { return l.agent.NodeInfo() }
func (l *localAgentClient) ListEngines() ([]agentwire.EngineDescriptor, error) {
	return l.agent.ListEngines()
}
func (l *localAgentClient) ApplyConfig(p agentwire.ApplyConfigParams) (agentwire.ConfigState, error) {
	return l.agent.ApplyConfig(p)
}
func (l *localAgentClient) GetConfigState() (agentwire.ConfigState, error) {
	return l.agent.GetConfigState()
}
func (l *localAgentClient) SetAnnounceOverride(p agentwire.AnnounceOverrideParams) error {
	return l.agent.SetAnnounceOverride(p)
}
func (l *localAgentClient) DiskFree(path string) (int64, error) { return l.agent.DiskFree(path) }

// The move verbs go through this process's own agent server, like every other
// cold call. That is what makes an engine of this machine behave exactly like
// one on another: the same code plans it, moves it and reports it.
func (l *localAgentClient) MovePlan(engineID, infoHash, targetDir string) (agentwire.MovePlan, error) {
	if engineID == "" {
		engineID = l.id
	}
	return l.agent.MovePlan(engineID, infoHash, targetDir)
}

func (l *localAgentClient) MovePayload(p agentwire.MovePayloadParams) (agentwire.MoveStatus, error) {
	if p.Engine == "" {
		p.Engine = l.id
	}
	return l.agent.MovePayload(p)
}

func (l *localAgentClient) MoveStatus(infoHash string) (agentwire.MoveStatus, error) {
	return l.agent.MoveStatus(infoHash)
}
func (l *localAgentClient) TrackerSnapshot() ([]agentwire.TrackerStatWire, error) {
	return l.agent.TrackerSnapshot()
}
func (l *localAgentClient) TorrentCategories(engineID string) (map[string]string, error) {
	return l.agent.TorrentCategories(l.engineOr(engineID))
}
func (l *localAgentClient) SetCategoryLabel(engineID, h, category string) error {
	return l.agent.SetCategoryLabel(l.engineOr(engineID), h, category)
}
func (l *localAgentClient) ActionRouted(mode, action, h string, del bool, cat, save string) error {
	return l.agent.ActionRouted(mode, action, h, del, cat, save)
}
func (l *localAgentClient) AddRouted(mode, torrentPath, savePath, category string) (*ltclient.AddTorrentResult, error) {
	return l.agent.AddRouted(mode, torrentPath, savePath, category)
}
func (l *localAgentClient) GetTorrentFile(h string) ([]byte, error) {
	return l.agent.GetTorrentFile(h)
}
func (l *localAgentClient) ReadPiece(h string, piece int) ([]byte, error) {
	return l.agent.ReadPiece(h, piece)
}
func (l *localAgentClient) WritePiece(h string, piece int, data []byte) error {
	return l.agent.WritePiece(h, piece, data)
}
func (l *localAgentClient) ImportStateWithFile(rec *ltclient.ResumeRecord, torrent []byte) (string, error) {
	return l.agent.ImportStateWithFile(rec, torrent)
}

// engineOr defaults an empty engine selector to the one this client stands for.
// A remote client is built per engine and the caller may legitimately pass "";
// forwarding that blank to the agent would let it pick whichever engine it
// listed first, so a category read for the race engine could answer with the
// hoard's.
func (l *localAgentClient) engineOr(sel string) string {
	if sel == "" {
		return l.id
	}
	return sel
}

var _ AgentClient = (*localAgentClient)(nil)
