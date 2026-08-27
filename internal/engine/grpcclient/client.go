// Package grpcclient is the remote arm of the EngineClient factory: a
// *grpcclient.Client dials a HydraAgent and satisfies engine.EngineClient by
// tunneling each method call as JSON (see internal/agentwire). Structurally it
// is interchangeable with *ltclient.Client — the front's HoardEngine/RaceEngine
// can .SetClient() either one and never know the difference.
package grpcclient

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"os"
	"sync"
	"time"

	"google.golang.org/grpc"
	"google.golang.org/grpc/credentials"
	"google.golang.org/grpc/credentials/insecure"

	"github.com/Kheopsian/hydra/internal/agentpb"
	"github.com/Kheopsian/hydra/internal/agentwire"
	"github.com/Kheopsian/hydra/internal/engine/ltclient"
)

// callTimeout bounds a single unary Call, matching ltclient's own budget so
// slow ops (verify, list on 100k torrents) don't spuriously fail.
const callTimeout = 120 * time.Second

// Config describes how to reach one remote engine session.
type Config struct {
	Addr   string // host:port of the HydraAgent
	Engine string // "race" | "hoard" (agentwire.Engine*)
	Token  string // bearer token (empty = none)
	TLSCa  string // path to CA cert (empty = plaintext transport)
}

// Client dials a HydraAgent and implements engine.EngineClient for one engine.
type Client struct {
	conn   *grpc.ClientConn
	stub   agentpb.HydraAgentClient
	engine string

	eventMu sync.Mutex
	onEvent func(ltclient.Event)

	cancelSub context.CancelFunc
	closed    bool
}

// bearer attaches the shared token to every RPC.
type bearer struct{ token string }

func (b bearer) GetRequestMetadata(context.Context, ...string) (map[string]string, error) {
	return map[string]string{"authorization": "Bearer " + b.token}, nil
}
func (b bearer) RequireTransportSecurity() bool { return false }

// New dials the agent and returns a ready EngineClient. It verifies reachability
// with a Ping so wiring errors surface at construction, not first use.
func New(cfg Config) (*Client, error) {
	opts := []grpc.DialOption{}
	if cfg.TLSCa != "" {
		creds, err := credentials.NewClientTLSFromFile(cfg.TLSCa, "")
		if err != nil {
			return nil, fmt.Errorf("grpcclient: tls ca: %w", err)
		}
		opts = append(opts, grpc.WithTransportCredentials(creds))
	} else {
		opts = append(opts, grpc.WithTransportCredentials(insecure.NewCredentials()))
	}
	if cfg.Token != "" {
		opts = append(opts, grpc.WithPerRPCCredentials(bearer{token: cfg.Token}))
	}
	conn, err := grpc.NewClient(cfg.Addr, opts...)
	if err != nil {
		return nil, fmt.Errorf("grpcclient: dial %s: %w", cfg.Addr, err)
	}
	c := &Client{conn: conn, stub: agentpb.NewHydraAgentClient(conn), engine: cfg.Engine}
	if err := c.Ping(); err != nil {
		conn.Close()
		return nil, fmt.Errorf("grpcclient: ping %s: %w", cfg.Addr, err)
	}
	return c, nil
}

// call is the single tunnel primitive: marshal params, invoke Call, re-raise a
// remote engine error, unmarshal the result into out.
func (c *Client) call(method string, params, out interface{}) error {
	var pb []byte
	if params != nil {
		var err error
		if pb, err = json.Marshal(params); err != nil {
			return fmt.Errorf("grpcclient: marshal params: %w", err)
		}
	}
	ctx, cancel := context.WithTimeout(context.Background(), callTimeout)
	defer cancel()
	rep, err := c.stub.Call(ctx, &agentpb.CallRequest{Engine: c.engine, Method: method, Params: pb})
	if err != nil {
		return err
	}
	if rep.Error != "" {
		return errors.New(rep.Error)
	}
	if out != nil && len(rep.Result) > 0 {
		return json.Unmarshal(rep.Result, out)
	}
	return nil
}

// --- engine.EngineClient implementation ---

func (c *Client) Ping() error { return c.call(agentwire.MethodPing, nil, nil) }

// ListEngines enumerates the engines the remote agent hosts (node-level; the
// engine field of the request is ignored by the agent for this method).
func (c *Client) ListEngines() ([]agentwire.EngineDescriptor, error) {
	var out []agentwire.EngineDescriptor
	err := c.call(agentwire.MethodListEngines, nil, &out)
	return out, err
}

// NodeInfo returns the agent's egress IP + host interfaces (node-level).
func (c *Client) NodeInfo() (agentwire.NodeInfo, error) {
	var out agentwire.NodeInfo
	err := c.call(agentwire.MethodNodeInfo, nil, &out)
	return out, err
}

// GetAnnounceOverrides returns the agent's per-host announce override maps.
func (c *Client) GetAnnounceOverrides() (agentwire.AnnounceOverrides, error) {
	var out agentwire.AnnounceOverrides
	err := c.call(agentwire.MethodGetAnnounceOverrides, nil, &out)
	return out, err
}

// SetAnnounceOverride pushes one announce override to the agent (node-level).
func (c *Client) SetAnnounceOverride(p agentwire.AnnounceOverrideParams) error {
	return c.call(agentwire.MethodSetAnnounceOverride, p, nil)
}

// ApplyConfig pushes this node's whole composed configuration (node-level).
// The agent applies it, restarts only the engines whose engine config actually
// changed, and caches it so it can boot from it while the front is down.
func (c *Client) ApplyConfig(p agentwire.ApplyConfigParams) (agentwire.ConfigState, error) {
	var out agentwire.ConfigState
	err := c.call(agentwire.MethodApplyConfig, p, &out)
	return out, err
}

// GetConfigState reports which config revision the agent is running and how
// each of its engines took it (node-level).
func (c *Client) GetConfigState() (agentwire.ConfigState, error) {
	var out agentwire.ConfigState
	err := c.call(agentwire.MethodGetConfigState, nil, &out)
	return out, err
}

// TrackerSnapshot returns the agent's per-host announce aggregate (node-level).
func (c *Client) TrackerSnapshot() ([]agentwire.TrackerStatWire, error) {
	var out []agentwire.TrackerStatWire
	err := c.call(agentwire.MethodTrackerSnapshot, nil, &out)
	return out, err
}

func (c *Client) Close() error {
	c.eventMu.Lock()
	if c.cancelSub != nil {
		c.cancelSub()
	}
	c.closed = true
	c.eventMu.Unlock()
	return c.conn.Close()
}

func (c *Client) AddTorrent(torrentPath, savePath string, stopped bool) (*ltclient.AddTorrentResult, error) {
	return c.add(torrentPath, savePath, stopped, false, false)
}

func (c *Client) AddTorrentWithOptions(torrentPath, savePath string, stopped, seedMode bool) (*ltclient.AddTorrentResult, error) {
	return c.add(torrentPath, savePath, stopped, seedMode, true)
}

func (c *Client) add(torrentPath, savePath string, stopped, seedMode, withOpts bool) (*ltclient.AddTorrentResult, error) {
	data, err := os.ReadFile(torrentPath)
	if err != nil {
		return nil, fmt.Errorf("grpcclient: read torrent %s: %w", torrentPath, err)
	}
	p := agentwire.AddParams{Torrent: data, SavePath: savePath, Stopped: stopped, SeedMode: seedMode, WithOptions: withOpts}
	var res ltclient.AddTorrentResult
	if err := c.call(agentwire.MethodAddTorrent, p, &res); err != nil {
		return nil, err
	}
	return &res, nil
}

func (c *Client) RemoveTorrent(infoHash string, keepData bool) error {
	return c.call(agentwire.MethodRemoveTorrent, agentwire.RemoveParams{InfoHash: infoHash, KeepData: keepData}, nil)
}

func (c *Client) StartTorrent(infoHash string) error {
	return c.call(agentwire.MethodStartTorrent, agentwire.InfoHashParams{InfoHash: infoHash}, nil)
}

func (c *Client) StopTorrent(infoHash string) error {
	return c.call(agentwire.MethodStopTorrent, agentwire.InfoHashParams{InfoHash: infoHash}, nil)
}

func (c *Client) SetSavePath(infoHash, savePath string) error {
	return c.call(agentwire.MethodSetSavePath, agentwire.SetSavePathParams{InfoHash: infoHash, SavePath: savePath}, nil)
}

func (c *Client) VerifyTorrent(infoHash string) error {
	return c.call(agentwire.MethodVerifyTorrent, agentwire.InfoHashParams{InfoHash: infoHash}, nil)
}

// ExportState and ImportState carry a torrent's identity and progression to
// or from a remote agent. They move no payload bytes and never claim to: the
// record's save_path is taken at face value on the far side, exactly as in the
// local case. That makes them correct on their own only when the data already
// sits on both hosts (a cross-seed, a restore from an existing copy); a move
// that genuinely relocates bytes is a separate job type on top of these.
func (c *Client) ExportState(infoHash string) (*ltclient.ResumeRecord, error) {
	var rec ltclient.ResumeRecord
	if err := c.call(agentwire.MethodExportState, agentwire.InfoHashParams{InfoHash: infoHash}, &rec); err != nil {
		return nil, err
	}
	// Same guard as the local client: an empty record unmarshals happily from
	// a null result, and adopting one would register a torrent with no
	// progression at all rather than fail.
	if rec.InfoHash == "" {
		return nil, fmt.Errorf("grpcclient: agent returned an empty state record for %s", infoHash)
	}
	return &rec, nil
}

func (c *Client) ImportState(rec *ltclient.ResumeRecord) (string, error) {
	if rec == nil {
		return "", errors.New("grpcclient: import_state: nil record")
	}
	var res struct {
		InfoHash string `json:"info_hash"`
		Name     string `json:"name"`
	}
	if err := c.call(agentwire.MethodImportState, rec, &res); err != nil {
		return "", err
	}
	return res.InfoHash, nil
}

// GetTorrentFile fetches the raw .torrent bytes backing a torrent on the
// agent. Deliberately NOT part of engine.EngineClient: it exists only to make
// a record exported from one host adoptable on another, and the local engines
// have no use for it.
func (c *Client) GetTorrentFile(infoHash string) ([]byte, error) {
	var res struct {
		Torrent []byte `json:"torrent"`
	}
	if err := c.call(agentwire.MethodGetTorrentFile, agentwire.InfoHashParams{InfoHash: infoHash}, &res); err != nil {
		return nil, err
	}
	if len(res.Torrent) == 0 {
		return nil, fmt.Errorf("grpcclient: agent returned no .torrent bytes for %s", infoHash)
	}
	return res.Torrent, nil
}

// ImportStateWithFile adopts a record onto this agent, shipping the .torrent
// bytes with it. The agent writes them to its own disk and rewrites the
// record's torrent_path before adopting, which is what makes this work across
// hosts at all -- plain ImportState would hand the far engine a path that only
// means something on the machine the record came from.
//
// It still moves no payload: save_path is taken at face value, so the data
// must already be reachable there. That is the honest boundary of this call.
func (c *Client) ImportStateWithFile(rec *ltclient.ResumeRecord, torrent []byte) (string, error) {
	if rec == nil {
		return "", errors.New("grpcclient: import_state_file: nil record")
	}
	if len(torrent) == 0 {
		return "", errors.New("grpcclient: import_state_file: empty .torrent bytes")
	}
	raw, err := json.Marshal(rec)
	if err != nil {
		return "", fmt.Errorf("grpcclient: marshal record: %w", err)
	}
	var res struct {
		InfoHash string `json:"info_hash"`
	}
	if err := c.call(agentwire.MethodImportStateFile,
		agentwire.ImportStateFileParams{Record: raw, TorrentBlob: torrent}, &res); err != nil {
		return "", err
	}
	return res.InfoHash, nil
}

// SetCategoryLabel labels a torrent on the agent without moving its files.
func (c *Client) SetCategoryLabel(engineID, infoHash, category string) error {
	return c.call(agentwire.MethodSetCategoryLabel,
		agentwire.CategoryLabelParams{Engine: engineID, InfoHash: infoHash, Category: category}, nil)
}

// TorrentCategories returns the agent's category per info hash. Categories are
// Hydra's own layer: they are absent from both the resume record and the
// torrent status, so the list has to ask for them separately.
func (c *Client) TorrentCategories(engineID string) (map[string]string, error) {
	var res map[string]string
	if err := c.call(agentwire.MethodTorrentCategories,
		agentwire.EngineParams{Engine: engineID}, &res); err != nil {
		return nil, err
	}
	return res, nil
}

// DiskFree reports the bytes available at a path on the agent.
func (c *Client) DiskFree(path string) (int64, error) {
	var res struct {
		Free int64 `json:"free"`
	}
	if err := c.call(agentwire.MethodDiskFree, agentwire.PathParams{Path: path}, &res); err != nil {
		return 0, err
	}
	return res.Free, nil
}

// MovePlan asks what relocating a payload would involve, in the agent's own
// filesystem. Nothing is touched.
func (c *Client) MovePlan(engineID, infoHash, targetDir string) (agentwire.MovePlan, error) {
	var res agentwire.MovePlan
	err := c.call(agentwire.MethodMovePlan, agentwire.MovePlanParams{
		Engine: engineID, InfoHash: infoHash, TargetDir: targetDir}, &res)
	return res, err
}

// MovePayload starts one and returns straight away: a cross-filesystem copy
// runs for hours, and holding the call open for it would time out on every
// layer in between.
func (c *Client) MovePayload(p agentwire.MovePayloadParams) (agentwire.MoveStatus, error) {
	var res agentwire.MoveStatus
	err := c.call(agentwire.MethodMovePayload, p, &res)
	return res, err
}

// MoveStatus polls one.
func (c *Client) MoveStatus(infoHash string) (agentwire.MoveStatus, error) {
	var res agentwire.MoveStatus
	err := c.call(agentwire.MethodMoveStatus, agentwire.MoveStatusParams{InfoHash: infoHash}, &res)
	return res, err
}

// ReadPiece fetches one whole piece from the agent holding the payload.
// Cross-host only, like GetTorrentFile: not part of engine.EngineClient.
func (c *Client) ReadPiece(infoHash string, piece int) ([]byte, error) {
	var res struct {
		Data []byte `json:"data"`
	}
	if err := c.call(agentwire.MethodReadPiece,
		agentwire.PieceParams{InfoHash: infoHash, Piece: piece}, &res); err != nil {
		return nil, err
	}
	return res.Data, nil
}

// WritePiece hands one whole piece to the agent that will serve it. The agent
// checks it against the torrent's own hash before writing, so a corrupted
// transfer fails here rather than surfacing as a bad recheck days later.
func (c *Client) WritePiece(infoHash string, piece int, data []byte) error {
	return c.call(agentwire.MethodWritePiece,
		agentwire.PieceParams{InfoHash: infoHash, Piece: piece, Data: data}, nil)
}

func (c *Client) GetStatus(infoHash string) (*ltclient.TorrentStatus, error) {
	var res ltclient.TorrentStatus
	if err := c.call(agentwire.MethodGetStatus, agentwire.InfoHashParams{InfoHash: infoHash}, &res); err != nil {
		return nil, err
	}
	return &res, nil
}

// ListTorrentsTimeout is ListTorrents with a caller-chosen deadline, for list
// aggregation where a slow/dead agent must not stall the UI poll.
func (c *Client) ListTorrentsTimeout(d time.Duration) (*ltclient.ListTorrentsResult, error) {
	ctx, cancel := context.WithTimeout(context.Background(), d)
	defer cancel()
	rep, err := c.stub.Call(ctx, &agentpb.CallRequest{Engine: c.engine, Method: agentwire.MethodListTorrents})
	if err != nil {
		return nil, err
	}
	if rep.Error != "" {
		return nil, errors.New(rep.Error)
	}
	var res ltclient.ListTorrentsResult
	if len(rep.Result) > 0 {
		if err := json.Unmarshal(rep.Result, &res); err != nil {
			return nil, err
		}
	}
	return &res, nil
}

// ListTorrentsSlim answers with the full listing.
//
// The projection exists to spare the local engine the strings and mutexes it
// would touch for 196k torrents; a remote agent holds a small set, and teaching
// the agent wire protocol a new parameter would mean an agent running an older
// binary could not serve it. The slim field set is a subset of the full one, so
// the caller sees exactly what it asked for either way.
func (c *Client) ListTorrentsSlim() (*ltclient.ListTorrentsResult, error) {
	return c.ListTorrents()
}

func (c *Client) ListTorrents() (*ltclient.ListTorrentsResult, error) {
	var res ltclient.ListTorrentsResult
	if err := c.call(agentwire.MethodListTorrents, nil, &res); err != nil {
		return nil, err
	}
	return &res, nil
}

func (c *Client) GetPeers(infoHash string) ([]ltclient.PeerInfo, error) {
	var res []ltclient.PeerInfo
	if err := c.call(agentwire.MethodGetPeers, agentwire.InfoHashParams{InfoHash: infoHash}, &res); err != nil {
		return nil, err
	}
	return res, nil
}

func (c *Client) GetSessionStats() (*ltclient.SessionStats, error) {
	var res ltclient.SessionStats
	if err := c.call(agentwire.MethodGetSessionStat, nil, &res); err != nil {
		return nil, err
	}
	return &res, nil
}

func (c *Client) GetFiles(infoHash string) ([]ltclient.FileInfo, error) {
	var res []ltclient.FileInfo
	if err := c.call(agentwire.MethodGetFiles, agentwire.InfoHashParams{InfoHash: infoHash}, &res); err != nil {
		return nil, err
	}
	return res, nil
}

func (c *Client) SetEngineOptFlag(name string, on bool, value int64) (map[string]interface{}, error) {
	var res map[string]interface{}
	if err := c.call(agentwire.MethodSetEngineOptFlag, agentwire.OptFlagParams{Flag: name, On: on, Value: value}, &res); err != nil {
		return nil, err
	}
	return res, nil
}

func (c *Client) EngineOptFlags() (map[string]interface{}, error) {
	var res map[string]interface{}
	if err := c.call(agentwire.MethodEngineOptFlags, nil, &res); err != nil {
		return nil, err
	}
	return res, nil
}

func (c *Client) GetAvailability(infoHash string) (*ltclient.Availability, error) {
	var res ltclient.Availability
	if err := c.call(agentwire.MethodGetAvailability, agentwire.InfoHashParams{InfoHash: infoHash}, &res); err != nil {
		return nil, err
	}
	return &res, nil
}

func (c *Client) GetTrackers(infoHash string) ([]ltclient.TrackerInfo, error) {
	var res []ltclient.TrackerInfo
	if err := c.call(agentwire.MethodGetTrackers, agentwire.InfoHashParams{InfoHash: infoHash}, &res); err != nil {
		return nil, err
	}
	return res, nil
}

func (c *Client) SetTrackers(infoHash string, tiers [][]string) ([][]string, error) {
	if tiers == nil {
		tiers = [][]string{}
	}
	var res [][]string
	if err := c.call(agentwire.MethodSetTrackers,
		agentwire.SetTrackersParams{InfoHash: infoHash, Trackers: tiers}, &res); err != nil {
		return nil, err
	}
	return res, nil
}

func (c *Client) GetDiagnostics() (*ltclient.DiagnosticStats, error) {
	var res ltclient.DiagnosticStats
	if err := c.call(agentwire.MethodGetDiagnostics, nil, &res); err != nil {
		return nil, err
	}
	return &res, nil
}

func (c *Client) AddPeers(infoHash string, peers []struct {
	IP   string
	Port int
}) error {
	wp := make([]agentwire.Peer, len(peers))
	for i, p := range peers {
		wp[i] = agentwire.Peer{IP: p.IP, Port: p.Port}
	}
	return c.call(agentwire.MethodAddPeers, agentwire.AddPeersParams{InfoHash: infoHash, Peers: wp}, nil)
}

// FetchMetadata asks the agent's engine to start resolving a magnet. The agent
// resolves on its own network, which is the point: it is the node that will
// hold the data and that has the tracker/VPN path.
func (c *Client) FetchMetadata(infoHash string, trackers, peers []string, bindingID *uint32) (*ltclient.FetchMetadataResult, error) {
	p := agentwire.FetchMetadataParams{InfoHash: infoHash, Trackers: trackers, Peers: peers, BindingID: bindingID}
	var out ltclient.FetchMetadataResult
	if err := c.call(agentwire.MethodFetchMetadata, p, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// GetMetadata polls a resolution running on the agent.
func (c *Client) GetMetadata(infoHash string) (*ltclient.GetMetadataResult, error) {
	var out ltclient.GetMetadataResult
	if err := c.call(agentwire.MethodGetMetadata, agentwire.GetMetadataParams{InfoHash: infoHash}, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// ActionRouted runs a rich per-torrent operation on the remote agent's own
// engine (announce/drain/stats stay agent-side).
func (c *Client) ActionRouted(mode, action, infoHash string, deleteFiles bool, category, savePath string) error {
	p := agentwire.ActionRoutedParams{Mode: mode, Action: action, InfoHash: infoHash, DeleteFiles: deleteFiles, Category: category, SavePath: savePath}
	return c.call(agentwire.MethodActionRouted, p, nil)
}

// AddRouted performs a rich placement add on the remote agent (its own engine
// logic runs; announce stays agent-side). Not part of EngineClient — callers
// hold the concrete *Client for this.
func (c *Client) AddRouted(mode, torrentPath, savePath, category string, createFolder *bool, skipRecheck bool) (*ltclient.AddTorrentResult, error) {
	data, err := os.ReadFile(torrentPath)
	if err != nil {
		return nil, fmt.Errorf("grpcclient: read torrent %s: %w", torrentPath, err)
	}
	p := agentwire.AddRoutedParams{Mode: mode, Torrent: data, SavePath: savePath, Category: category,
		CreateFolder: createFolder, SkipRecheck: skipRecheck}
	var res ltclient.AddTorrentResult
	if err := c.call(agentwire.MethodAddRouted, p, &res); err != nil {
		return nil, err
	}
	return &res, nil
}

// SetEventHandler stores the callback; SubscribeEvents opens the stream that
// feeds it. Mirror of ltclient's contract (handler set before subscribing).
func (c *Client) SetEventHandler(handler func(ltclient.Event)) {
	c.eventMu.Lock()
	c.onEvent = handler
	c.eventMu.Unlock()
}

// SubscribeEvents opens the agent's event stream and pumps decoded frames into
// the registered handler until Close (or the stream ends).
func (c *Client) SubscribeEvents() error {
	c.eventMu.Lock()
	if c.closed {
		c.eventMu.Unlock()
		return errors.New("grpcclient: closed")
	}
	ctx, cancel := context.WithCancel(context.Background())
	c.cancelSub = cancel
	c.eventMu.Unlock()

	stream, err := c.stub.Subscribe(ctx, &agentpb.SubscribeRequest{Engine: c.engine})
	if err != nil {
		cancel()
		return err
	}
	go c.pumpEvents(stream)
	return nil
}

func (c *Client) pumpEvents(stream grpc.ServerStreamingClient[agentpb.EventFrame]) {
	for {
		frame, err := stream.Recv()
		if err != nil {
			if err != io.EOF && !c.closed {
				slog.Debug("grpcclient: event stream ended", "engine", c.engine, "err", err)
			}
			return
		}
		var ev ltclient.Event
		if err := json.Unmarshal(frame.Payload, &ev); err != nil {
			continue
		}
		c.eventMu.Lock()
		h := c.onEvent
		c.eventMu.Unlock()
		if h != nil {
			h(ev)
		}
	}
}

// NewWithStub builds a Client over an already-made transport instead of
// dialling one. It exists for the in-process case: an engine running in the
// front's own process is reachable through agent.InProcessStub, and this
// constructor lets it reuse every method on this type -- all the param
// envelopes, all the result decoding -- rather than growing a second,
// hand-written implementation that would drift from the remote one.
//
// conn stays nil, so Close on such a client closes nothing. That is correct:
// there is no connection to release, and the engine it speaks to belongs to the
// process, not to this client.
func NewWithStub(stub agentpb.HydraAgentClient, engine string) *Client {
	return &Client{stub: stub, engine: engine}
}
