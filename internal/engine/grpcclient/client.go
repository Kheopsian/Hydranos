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

func (c *Client) GetTrackers(infoHash string) ([]ltclient.TrackerInfo, error) {
	var res []ltclient.TrackerInfo
	if err := c.call(agentwire.MethodGetTrackers, agentwire.InfoHashParams{InfoHash: infoHash}, &res); err != nil {
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

// ActionRouted runs a rich per-torrent operation on the remote agent's own
// engine (announce/drain/stats stay agent-side).
func (c *Client) ActionRouted(mode, action, infoHash string, deleteFiles bool, category, savePath string) error {
	p := agentwire.ActionRoutedParams{Mode: mode, Action: action, InfoHash: infoHash, DeleteFiles: deleteFiles, Category: category, SavePath: savePath}
	return c.call(agentwire.MethodActionRouted, p, nil)
}

// AddRouted performs a rich placement add on the remote agent (its own engine
// logic runs; announce stays agent-side). Not part of EngineClient — callers
// hold the concrete *Client for this.
func (c *Client) AddRouted(mode, torrentPath, savePath, category string) (*ltclient.AddTorrentResult, error) {
	data, err := os.ReadFile(torrentPath)
	if err != nil {
		return nil, fmt.Errorf("grpcclient: read torrent %s: %w", torrentPath, err)
	}
	p := agentwire.AddRoutedParams{Mode: mode, Torrent: data, SavePath: savePath, Category: category}
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
