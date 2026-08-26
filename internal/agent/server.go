// Package agent exposes one machine's engine sessions (race/hoard) over the
// HydraAgent gRPC contract, which is a thin JSON mirror of engine.EngineClient
// (see internal/agentwire). A remote control plane (front) dials in and drives
// each session's EngineClient as if it were local. Additive: only served when
// --agent-addr is set, so it has zero impact on the monolith otherwise.
package agent

import (
	"context"
	"crypto/sha256"
	"crypto/subtle"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"

	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/credentials"
	"google.golang.org/grpc/metadata"
	"google.golang.org/grpc/status"

	"github.com/Kheopsian/hydra/internal/agentpb"
	"github.com/Kheopsian/hydra/internal/agentwire"
	"github.com/Kheopsian/hydra/internal/drain"
	"github.com/Kheopsian/hydra/internal/engine"
	"github.com/Kheopsian/hydra/internal/engine/ltclient"
)

// Server implements agentpb.HydraAgentServer by routing each Call to the named
// local EngineClient and JSON-marshalling the ltclient return value back.
type Server struct {
	agentpb.UnimplementedHydraAgentServer

	// enginesMu guards engines, rich, declared and hubs. A dedicated agent
	// takes its configuration from the front, so its engines are not a fixed
	// set decided at construction: they appear when the first config arrives
	// and are swapped out whenever a pushed change needs a Typhon restart,
	// while the gRPC server keeps serving throughout.
	enginesMu sync.RWMutex
	engines   map[string]engine.EngineClient
	// declared is the boot identity: the engines this node WILL host, known
	// before any of them runs. list_engines answers from it so a front can
	// compose a config for a node whose engines are still down.
	declared []agentwire.EngineDescriptor
	// hubs are per-engine-id event fan-outs that outlive an engine restart.
	// Subscribing straight to a RichEngine's own hub would leave every front
	// subscription attached to a dead engine's hub after a config change --
	// silent, and only noticed as a UI that stopped updating.
	hubs map[string]*engine.EventHub
	// configManager owns this node's configuration (nil in additive mode).
	configManager ConfigManager
	tmpDir        string
	// uploadsDir is where a routed add lands its .torrent for good. Unlike the
	// thin add path (whose caller keeps the blob), a routed add hands the file
	// to this node's OWN engine, which stores the path as its TorrentFilePath
	// and hands it to the store reconcile later — so it has to outlive the RPC.
	uploadsDir string
	token      string
	tlsCert    string
	tlsKey     string

	// ownEvents gates the Subscribe stream. SetEventHandler is single-slot on a
	// live ltclient, so an agent sharing the monolith's clients (additive mode)
	// MUST NOT hijack it — that would steal events from the in-process engines.
	// Only a dedicated agent process (front runs the engine logic elsewhere)
	// sets ownEvents=true and may own the event handler.
	ownEvents bool
	subMu     sync.Mutex
	subs      map[string]map[int]chan []byte // engine -> subID -> frame channel
	subSeq    int
	wired     map[string]bool // engine -> event handler installed

	// Rich engines for routed add/action, addressed by engine-id (Option A:
	// a node hosts an arbitrary set of engines). Empty in additive mode. Each
	// value is role-typed via RichEngine.Role().
	rich map[string]engine.RichEngine

	ipv6Wanted bool // enable_ipv6 on any engine here; set at boot

	// moves tracks payload relocations running on this node (agentmove.go).
	// They outlive the call that starts them: a cross-filesystem copy runs for
	// hours, so the front polls instead of waiting.
	moves moveTracker
}

// NewServer builds an agent over the given named EngineClients (e.g.
// {"race": ..., "hoard": ...}). tmpDir holds shipped .torrent bytes on add.
// token (if non-empty) is the required bearer token.
func NewServer(engines map[string]engine.EngineClient, tmpDir, token string) *Server {
	if engines == nil {
		engines = make(map[string]engine.EngineClient)
	}
	return &Server{
		engines: engines,
		tmpDir:  tmpDir,
		token:   token,
		subs:    make(map[string]map[int]chan []byte),
		wired:   make(map[string]bool),
		rich:    make(map[string]engine.RichEngine),
		hubs:    make(map[string]*engine.EventHub),
	}
}

// DeclareEngines records the engines this node is going to host, before any of
// them is running. It is what list_engines reports while the node waits for its
// first config: without it a front would see a node with no engines, compose an
// empty config for it, and the node would wait forever for the config it needs
// in order to have the engines that would have made the front send one.
func (s *Server) DeclareEngines(d []agentwire.EngineDescriptor) {
	s.enginesMu.Lock()
	defer s.enginesMu.Unlock()
	s.declared = append(s.declared[:0], d...)
}

// ReplaceEngine installs (or swaps) one engine under an id. Passing a nil
// client removes it. The engine's raw events are pumped into this id's stable
// hub until the returned stop function is called, which the caller does before
// replacing the engine again.
func (s *Server) ReplaceEngine(id string, c engine.EngineClient, r engine.RichEngine) (stopPump func()) {
	s.enginesMu.Lock()
	if c == nil {
		delete(s.engines, id)
		delete(s.rich, id)
		s.enginesMu.Unlock()
		return func() {}
	}
	s.engines[id] = c
	if r != nil {
		s.rich[id] = r
	} else {
		delete(s.rich, id)
	}
	hub := s.hubLocked(id)
	s.enginesMu.Unlock()

	if r == nil {
		return func() {}
	}
	return pumpHub(r.RawEventHub(), hub)
}

// pumpHub forwards one engine generation's events into the id's stable hub.
func pumpHub(from, to *engine.EventHub) func() {
	if from == nil || to == nil {
		return func() {}
	}
	subID, ch := from.Subscribe()
	done := make(chan struct{})
	go func() {
		for {
			select {
			case <-done:
				return
			case frame, ok := <-ch:
				if !ok {
					return
				}
				to.Publish(frame)
			}
		}
	}()
	return func() {
		close(done)
		from.Unsubscribe(subID)
	}
}

// hubLocked returns the stable hub for an engine id, creating it on first use.
// Caller holds enginesMu.
func (s *Server) hubLocked(id string) *engine.EventHub {
	if s.hubs == nil {
		s.hubs = make(map[string]*engine.EventHub)
	}
	h := s.hubs[id]
	if h == nil {
		h = engine.NewEventHub(256)
		s.hubs[id] = h
	}
	return h
}

// SetUploadsDir sets where routed adds keep their .torrent. Defaults to
// <tmpDir>/uploads, which is the same directory the agent's store import
// materialises blobs into.
func (s *Server) SetUploadsDir(dir string) { s.uploadsDir = dir }

func (s *Server) uploads() string {
	if s.uploadsDir != "" {
		return s.uploadsDir
	}
	return filepath.Join(s.tmpDir, "uploads")
}

// SetTLS enables TLS with the given cert/key files (empty = plaintext).
func (s *Server) SetTLS(certFile, keyFile string) { s.tlsCert, s.tlsKey = certFile, keyFile }

// SetOwnEvents opts the agent into owning the engine event handler. Safe only
// when nothing else consumes these EngineClients' events (dedicated agent).
func (s *Server) SetOwnEvents(v bool) { s.ownEvents = v }

// SetIPv6Wanted opts node_info into reporting the v6 egress. The engine configs
// live in the process that builds them, not here, hence the setter.
func (s *Server) SetIPv6Wanted(v bool) { s.ipv6Wanted = v }

// AddRichEngine registers a role-typed local engine under an id for routed
// add/action/subscribe. Ids are arbitrary ("race", "hoard", "hoard2", ...).
func (s *Server) AddRichEngine(id string, r engine.RichEngine) {
	s.enginesMu.Lock()
	defer s.enginesMu.Unlock()
	if s.rich == nil {
		s.rich = make(map[string]engine.RichEngine)
	}
	s.rich[id] = r
}

// SetRichEngines is a back-compat shim for the fixed race+hoard pair. Nil
// concrete engines are skipped so a typed-nil never lands in the map.
func (s *Server) SetRichEngines(race *engine.RaceEngine, hoard *engine.HoardEngine) {
	if race != nil {
		s.AddRichEngine(agentwire.EngineRace, race)
	}
	if hoard != nil {
		s.AddRichEngine(agentwire.EngineHoard, hoard)
	}
}

// Serve starts the gRPC server on addr and blocks until ctx is cancelled.
func (s *Server) Serve(ctx context.Context, addr string) error {
	lis, err := net.Listen("tcp", addr)
	if err != nil {
		return fmt.Errorf("agent: listen %s: %w", addr, err)
	}
	opts := []grpc.ServerOption{
		grpc.ChainUnaryInterceptor(s.authUnary),
		grpc.ChainStreamInterceptor(s.authStream),
	}
	if s.tlsCert != "" && s.tlsKey != "" {
		creds, err := credentials.NewServerTLSFromFile(s.tlsCert, s.tlsKey)
		if err != nil {
			return fmt.Errorf("agent: tls: %w", err)
		}
		opts = append(opts, grpc.Creds(creds))
	}
	gs := grpc.NewServer(opts...)
	agentpb.RegisterHydraAgentServer(gs, s)
	go func() {
		<-ctx.Done()
		gs.GracefulStop()
	}()
	return gs.Serve(lis)
}

// --- auth (bearer token, constant-time) ---

func (s *Server) checkToken(ctx context.Context) error {
	if s.token == "" {
		return nil
	}
	md, _ := metadata.FromIncomingContext(ctx)
	vals := md.Get("authorization")
	want := "Bearer " + s.token
	if len(vals) == 1 && subtle.ConstantTimeCompare([]byte(vals[0]), []byte(want)) == 1 {
		return nil
	}
	return status.Error(codes.Unauthenticated, "missing or invalid agent token")
}

func (s *Server) authUnary(ctx context.Context, req interface{}, _ *grpc.UnaryServerInfo, handler grpc.UnaryHandler) (interface{}, error) {
	if err := s.checkToken(ctx); err != nil {
		return nil, err
	}
	return handler(ctx, req)
}

func (s *Server) authStream(srv interface{}, ss grpc.ServerStream, _ *grpc.StreamServerInfo, handler grpc.StreamHandler) error {
	if err := s.checkToken(ss.Context()); err != nil {
		return err
	}
	return handler(srv, ss)
}

// --- Call dispatch ---

// rawHubFor returns the stable event hub for an engine id, or nil when this
// node has no rich engine matching (additive mode). The hub is the agent's own
// rather than the engine's, so a subscription survives an engine restart.
func (s *Server) rawHubFor(id string) *engine.EventHub {
	s.enginesMu.Lock()
	defer s.enginesMu.Unlock()
	e := s.resolveRichLocked(id)
	if e == nil {
		return nil
	}
	// Resolve by role too: the hub must be keyed by the id the engine is
	// registered under, not by the selector the caller happened to use.
	for regID, re := range s.rich {
		if re == e {
			return s.hubLocked(regID)
		}
	}
	return nil
}

func (s *Server) client(name string) (engine.EngineClient, error) {
	s.enginesMu.RLock()
	c, ok := s.engines[name]
	s.enginesMu.RUnlock()
	if !ok || c == nil {
		// Declared but not started: the node knows this engine exists and is
		// waiting for a config to bring it up. Saying so beats "unknown
		// engine", which reads like a misconfigured front.
		if s.isDeclared(name) {
			return nil, status.Errorf(codes.Unavailable, "engine %q is declared but not configured yet", name)
		}
		return nil, status.Errorf(codes.NotFound, "unknown engine %q", name)
	}
	return c, nil
}

// isDeclared reports whether an id belongs to this node's boot identity.
func (s *Server) isDeclared(id string) bool {
	s.enginesMu.RLock()
	defer s.enginesMu.RUnlock()
	for _, d := range s.declared {
		if d.ID == id {
			return true
		}
	}
	return false
}

// ConfigManager is the node's configuration owner: it applies what a front
// pushes and reports what the node is running. The agent server only routes
// the two calls to it, because starting and restarting engines is the process
// supervisor's job, not the wire layer's (see cmd/hydra/agentsupervisor.go).
type ConfigManager interface {
	ApplyConfig(agentwire.ApplyConfigParams) agentwire.ConfigState
	ConfigState() agentwire.ConfigState
}

// SetConfigManager wires the node's config owner. Left nil (the monolith's
// additive agent, which configures itself from its own file), apply_config is
// declined as unimplemented and the front falls back to the older per-override
// announce push.
func (s *Server) SetConfigManager(m ConfigManager) {
	s.enginesMu.Lock()
	defer s.enginesMu.Unlock()
	s.configManager = m
}

func (s *Server) configMgr() ConfigManager {
	s.enginesMu.RLock()
	defer s.enginesMu.RUnlock()
	return s.configManager
}

func (s *Server) handleApplyConfig(p agentwire.ApplyConfigParams) (*agentpb.CallReply, error) {
	m := s.configMgr()
	if m == nil {
		return nil, status.Error(codes.Unimplemented, "this node configures itself locally and takes no pushed config")
	}
	return reply(m.ApplyConfig(p), nil)
}

func (s *Server) handleGetConfigState() (*agentpb.CallReply, error) {
	m := s.configMgr()
	if m == nil {
		return nil, status.Error(codes.Unimplemented, "this node configures itself locally and takes no pushed config")
	}
	return reply(m.ConfigState(), nil)
}

// reply wraps a (result-value, error) into a CallReply. A nil value yields an
// empty result body; a non-nil err is surfaced in CallReply.error (the client
// re-raises it) rather than as a gRPC status, so transport vs. engine errors
// stay distinguishable.
func reply(v interface{}, err error) (*agentpb.CallReply, error) {
	if err != nil {
		return &agentpb.CallReply{Error: err.Error()}, nil
	}
	if v == nil {
		return &agentpb.CallReply{}, nil
	}
	b, mErr := json.Marshal(v)
	if mErr != nil {
		return nil, status.Errorf(codes.Internal, "marshal result: %v", mErr)
	}
	return &agentpb.CallReply{Result: b}, nil
}

func (s *Server) Call(ctx context.Context, req *agentpb.CallRequest) (*agentpb.CallReply, error) {
	if req.Method == agentwire.MethodListEngines {
		return s.handleListEngines()
	}
	if req.Method == agentwire.MethodNodeInfo {
		return s.handleNodeInfo()
	}
	if req.Method == agentwire.MethodApplyConfig {
		var p agentwire.ApplyConfigParams
		if len(req.Params) > 0 {
			if err := json.Unmarshal(req.Params, &p); err != nil {
				return nil, err
			}
		}
		return s.handleApplyConfig(p)
	}
	if req.Method == agentwire.MethodGetConfigState {
		return s.handleGetConfigState()
	}
	if req.Method == agentwire.MethodGetAnnounceOverrides {
		return s.handleGetAnnounceOverrides()
	}
	if req.Method == agentwire.MethodSetAnnounceOverride {
		var p agentwire.AnnounceOverrideParams
		if len(req.Params) > 0 {
			if err := json.Unmarshal(req.Params, &p); err != nil {
				return nil, err
			}
		}
		return s.handleSetAnnounceOverride(p)
	}
	if req.Method == agentwire.MethodTrackerSnapshot {
		return reply(engine.TrackerSnapshot(), nil)
	}
	if req.Method == agentwire.MethodPing {
		// Node-level liveness, handled before engine resolution so a discovery
		// ping (engine id not yet known / absent on this node) still succeeds.
		return reply(nil, nil)
	}
	c, err := s.client(req.Engine)
	if err != nil {
		return nil, err
	}
	unmarshal := func(dst interface{}) error {
		if len(req.Params) == 0 {
			return nil
		}
		return json.Unmarshal(req.Params, dst)
	}

	switch req.Method {
	case agentwire.MethodPing:
		return reply(nil, c.Ping())

	case agentwire.MethodListTorrents:
		return reply(c.ListTorrents())

	case agentwire.MethodGetSessionStat:
		return reply(c.GetSessionStats())

	case agentwire.MethodGetDiagnostics:
		return reply(c.GetDiagnostics())

	case agentwire.MethodGetStatus:
		var p agentwire.InfoHashParams
		if err := unmarshal(&p); err != nil {
			return nil, badParams(err)
		}
		return reply(c.GetStatus(p.InfoHash))

	case agentwire.MethodGetPeers:
		var p agentwire.InfoHashParams
		if err := unmarshal(&p); err != nil {
			return nil, badParams(err)
		}
		return reply(c.GetPeers(p.InfoHash))

	case agentwire.MethodGetFiles:
		var p agentwire.InfoHashParams
		if err := unmarshal(&p); err != nil {
			return nil, badParams(err)
		}
		return reply(c.GetFiles(p.InfoHash))

	case agentwire.MethodGetAvailability:
		var p agentwire.InfoHashParams
		if err := unmarshal(&p); err != nil {
			return nil, badParams(err)
		}
		return reply(c.GetAvailability(p.InfoHash))

	case agentwire.MethodSetEngineOptFlag:
		var p agentwire.OptFlagParams
		if err := unmarshal(&p); err != nil {
			return nil, badParams(err)
		}
		return reply(c.SetEngineOptFlag(p.Flag, p.On, p.Value))

	case agentwire.MethodEngineOptFlags:
		return reply(c.EngineOptFlags())

	case agentwire.MethodGetTrackers:
		var p agentwire.InfoHashParams
		if err := unmarshal(&p); err != nil {
			return nil, badParams(err)
		}
		// Answered with this node's own announce observations folded in. The
		// engine's tracker state is a placeholder that reads as "Success" for a
		// tracker it has never contacted (see engine.TrackerRows), and the
		// front has no way to tell one from the other -- so every tracker on
		// every agent-held torrent showed a green OK, including the ones we
		// were failing to reach.
		trks, err := c.GetTrackers(p.InfoHash)
		if err != nil {
			return reply(trks, err)
		}
		return reply(engine.EncodeTrackerRows(trks, engine.TrackerRowsWithObs(p.InfoHash, trks)), nil)

	case agentwire.MethodStartTorrent:
		var p agentwire.InfoHashParams
		if err := unmarshal(&p); err != nil {
			return nil, badParams(err)
		}
		return reply(nil, c.StartTorrent(p.InfoHash))

	case agentwire.MethodStopTorrent:
		var p agentwire.InfoHashParams
		if err := unmarshal(&p); err != nil {
			return nil, badParams(err)
		}
		return reply(nil, c.StopTorrent(p.InfoHash))

	case agentwire.MethodVerifyTorrent:
		var p agentwire.InfoHashParams
		if err := unmarshal(&p); err != nil {
			return nil, badParams(err)
		}
		return reply(nil, c.VerifyTorrent(p.InfoHash))

	case agentwire.MethodExportState:
		var p agentwire.InfoHashParams
		if err := unmarshal(&p); err != nil {
			return nil, badParams(err)
		}
		return reply(c.ExportState(p.InfoHash))

	case agentwire.MethodImportState:
		// The params are the record itself, not an envelope around it.
		var rec ltclient.ResumeRecord
		if err := unmarshal(&rec); err != nil {
			return nil, badParams(err)
		}
		if rec.InfoHash == "" {
			return nil, badParams(fmt.Errorf("import_state: empty info_hash"))
		}
		ih, iErr := c.ImportState(&rec)
		if iErr != nil {
			return reply(nil, iErr)
		}
		return reply(map[string]string{"info_hash": ih}, nil)

	case agentwire.MethodGetTorrentFile:
		var p agentwire.InfoHashParams
		if err := unmarshal(&p); err != nil {
			return nil, badParams(err)
		}
		rec, rErr := c.ExportState(p.InfoHash)
		if rErr != nil {
			return reply(nil, rErr)
		}
		blob, bErr := os.ReadFile(rec.TorrentPath)
		if bErr != nil {
			return reply(nil, fmt.Errorf("get_torrent_file: %w", bErr))
		}
		return reply(map[string][]byte{"torrent": blob}, nil)

	case agentwire.MethodImportStateFile:
		var p agentwire.ImportStateFileParams
		if err := unmarshal(&p); err != nil {
			return nil, badParams(err)
		}
		var rec ltclient.ResumeRecord
		if err := json.Unmarshal(p.Record, &rec); err != nil {
			return nil, badParams(fmt.Errorf("import_state_file: record: %w", err))
		}
		if rec.InfoHash == "" {
			return nil, badParams(fmt.Errorf("import_state_file: empty info_hash"))
		}
		if len(p.TorrentBlob) == 0 {
			return nil, badParams(fmt.Errorf("import_state_file: empty .torrent bytes"))
		}
		// The incoming torrent_path is a path on the SOURCE host. Keep our own
		// copy and point the record at it, exactly as handleAdd does for a
		// shipped .torrent -- an adopted torrent whose file lives in a temp
		// that gets swept would fail to reload on the next restart, so this
		// lands beside the agent's other durable state, keyed by info_hash.
		dst := filepath.Join(s.tmpDir, "agent-import-"+rec.InfoHash+".torrent")
		if err := os.WriteFile(dst, p.TorrentBlob, 0644); err != nil {
			return reply(nil, fmt.Errorf("import_state_file: write torrent: %w", err))
		}
		rec.TorrentPath = dst
		ih, iErr := c.ImportState(&rec)
		if iErr != nil {
			// Do not leave the file behind on a refused adopt.
			_ = os.Remove(dst)
			return reply(nil, iErr)
		}
		return reply(map[string]string{"info_hash": ih}, nil)

	case agentwire.MethodSetCategoryLabel:
		var p agentwire.CategoryLabelParams
		if err := unmarshal(&p); err != nil {
			return nil, badParams(err)
		}
		e := s.resolveRich(engineOrHoard(p.Engine))
		if e == nil {
			return nil, status.Errorf(codes.Unavailable, "engine %q not wired on agent", p.Engine)
		}
		lab, ok := e.(interface {
			SetCategoryLabel(infoHash, category string) error
		})
		if !ok {
			return reply(nil, fmt.Errorf("engine %q cannot carry a category label", p.Engine))
		}
		return reply(nil, lab.SetCategoryLabel(p.InfoHash, p.Category))

	case agentwire.MethodTorrentCategories:
		var p agentwire.EngineParams
		if err := unmarshal(&p); err != nil {
			return nil, badParams(err)
		}
		e := s.resolveRich(engineOrHoard(p.Engine))
		if e == nil {
			return reply(map[string]string{}, nil)
		}
		mp, ok := e.(interface {
			GetTorrentMetas() map[string]*engine.TorrentMeta
		})
		if !ok {
			return reply(map[string]string{}, nil)
		}
		out := map[string]string{}
		for ih, m := range mp.GetTorrentMetas() {
			if m != nil && m.Category != "" {
				out[ih] = m.Category
			}
		}
		return reply(out, nil)

	case agentwire.MethodDiskFree:
		var p agentwire.PathParams
		if err := unmarshal(&p); err != nil {
			return nil, badParams(err)
		}
		total, free, dErr := drain.TotalFree(p.Path)
		if dErr != nil {
			return reply(nil, fmt.Errorf("disk_free %s: %w", p.Path, dErr))
		}
		return reply(map[string]int64{"total": total, "free": free}, nil)

	case agentwire.MethodReadPiece:
		var p agentwire.PieceParams
		if err := unmarshal(&p); err != nil {
			return nil, badParams(err)
		}
		pc, pErr := s.pieceContextFor(c, p.InfoHash)
		if pErr != nil {
			return reply(nil, pErr)
		}
		data, rErr := pc.ReadPiece(p.Piece)
		if rErr != nil {
			return reply(nil, rErr)
		}
		return reply(map[string][]byte{"data": data}, nil)

	case agentwire.MethodWritePiece:
		var p agentwire.PieceParams
		if err := unmarshal(&p); err != nil {
			return nil, badParams(err)
		}
		pc, pErr := s.pieceContextFor(c, p.InfoHash)
		if pErr != nil {
			return reply(nil, pErr)
		}
		if wErr := pc.WritePiece(p.Piece, p.Data); wErr != nil {
			return reply(nil, wErr)
		}
		return reply(map[string]int{"piece": p.Piece}, nil)

	case agentwire.MethodMovePlan:
		var p agentwire.MovePlanParams
		if err := unmarshal(&p); err != nil {
			return nil, badParams(err)
		}
		plan, mErr := s.handleMovePlan(p)
		return reply(plan, mErr)

	case agentwire.MethodMovePayload:
		var p agentwire.MovePayloadParams
		if err := unmarshal(&p); err != nil {
			return nil, badParams(err)
		}
		st, mErr := s.handleMovePayload(p)
		return reply(st, mErr)

	case agentwire.MethodMoveStatus:
		var p agentwire.MoveStatusParams
		if err := unmarshal(&p); err != nil {
			return nil, badParams(err)
		}
		return reply(s.handleMoveStatus(p), nil)

	case agentwire.MethodSetSavePath:
		var p agentwire.SetSavePathParams
		if err := unmarshal(&p); err != nil {
			return nil, badParams(err)
		}
		return reply(nil, c.SetSavePath(p.InfoHash, p.SavePath))

	case agentwire.MethodRemoveTorrent:
		var p agentwire.RemoveParams
		if err := unmarshal(&p); err != nil {
			return nil, badParams(err)
		}
		return reply(nil, c.RemoveTorrent(p.InfoHash, p.KeepData))

	case agentwire.MethodFetchMetadata:
		var p agentwire.FetchMetadataParams
		if err := unmarshal(&p); err != nil {
			return nil, badParams(err)
		}
		return reply(c.FetchMetadata(p.InfoHash, p.Trackers, p.Peers, p.BindingID))
	case agentwire.MethodGetMetadata:
		var p agentwire.GetMetadataParams
		if err := unmarshal(&p); err != nil {
			return nil, badParams(err)
		}
		return reply(c.GetMetadata(p.InfoHash))
	case agentwire.MethodAddPeers:
		var p agentwire.AddPeersParams
		if err := unmarshal(&p); err != nil {
			return nil, badParams(err)
		}
		peers := make([]struct {
			IP   string
			Port int
		}, len(p.Peers))
		for i, pr := range p.Peers {
			peers[i].IP, peers[i].Port = pr.IP, pr.Port
		}
		return reply(nil, c.AddPeers(p.InfoHash, peers))

	case agentwire.MethodAddTorrent:
		var p agentwire.AddParams
		if err := unmarshal(&p); err != nil {
			return nil, badParams(err)
		}
		return s.handleAdd(c, p)

	case agentwire.MethodActionRouted:
		var p agentwire.ActionRoutedParams
		if err := unmarshal(&p); err != nil {
			return nil, badParams(err)
		}
		return s.handleActionRouted(p)

	case agentwire.MethodAddRouted:
		var p agentwire.AddRoutedParams
		if err := unmarshal(&p); err != nil {
			return nil, badParams(err)
		}
		return s.handleAddRouted(p)

	default:
		return nil, status.Errorf(codes.Unimplemented, "unknown method %q", req.Method)
	}
}

func badParams(err error) error {
	return status.Errorf(codes.InvalidArgument, "bad params: %v", err)
}

// keepTorrentBlob writes shipped .torrent bytes somewhere the engine can still
// find them later, and returns that path.
//
// It must NOT be a temp file the caller deletes. The engine keeps the path it
// was handed as the torrent's durable torrent_file_path: it is what a restart
// re-parses, and what export_state hands to another engine. A .torrent removed
// as soon as the add returned left every torrent added to an agent working
// until the next restart and gone after it, with nothing in the logs.
//
// The name is content-addressed rather than random so that re-adding the same
// .torrent reuses one file instead of growing a new one on every call.
func (s *Server) keepTorrentBlob(prefix string, blob []byte) (string, error) {
	sum := sha256.Sum256(blob)
	path := filepath.Join(s.tmpDir, prefix+"-"+hex.EncodeToString(sum[:8])+".torrent")
	if _, err := os.Stat(path); err == nil {
		return path, nil
	}
	// Write-then-rename: a crash mid-write must not leave a truncated
	// .torrent behind under a name that later looks complete.
	tmp, err := os.CreateTemp(s.tmpDir, prefix+"-*.tmp")
	if err != nil {
		return "", fmt.Errorf("temp file: %w", err)
	}
	if _, err := tmp.Write(blob); err != nil {
		tmp.Close()
		os.Remove(tmp.Name())
		return "", fmt.Errorf("write torrent: %w", err)
	}
	if err := tmp.Close(); err != nil {
		os.Remove(tmp.Name())
		return "", fmt.Errorf("close torrent: %w", err)
	}
	if err := os.Rename(tmp.Name(), path); err != nil {
		os.Remove(tmp.Name())
		return "", fmt.Errorf("rename torrent: %w", err)
	}
	return path, nil
}

// handleAdd materialises the shipped .torrent bytes under the agent's data
// dir, then calls the local AddTorrent variant.
func (s *Server) handleAdd(c engine.EngineClient, p agentwire.AddParams) (*agentpb.CallReply, error) {
	if len(p.Torrent) == 0 {
		return nil, status.Error(codes.InvalidArgument, "add_torrent: empty torrent bytes")
	}
	path, err := s.keepTorrentBlob("agent-add", p.Torrent)
	if err != nil {
		return nil, status.Errorf(codes.Internal, "%v", err)
	}
	if p.WithOptions {
		return reply(c.AddTorrentWithOptions(path, p.SavePath, p.Stopped, p.SeedMode))
	}
	return reply(c.AddTorrent(path, p.SavePath, p.Stopped))
}

// handleListEngines enumerates the engines this agent hosts. It answers from
// the declared identity when there is one, so a node that has not been given a
// config yet still tells the front what to compose for.
func (s *Server) handleListEngines() (*agentpb.CallReply, error) {
	s.enginesMu.RLock()
	defer s.enginesMu.RUnlock()
	if len(s.declared) > 0 {
		out := make([]agentwire.EngineDescriptor, len(s.declared))
		copy(out, s.declared)
		return reply(out, nil)
	}
	out := make([]agentwire.EngineDescriptor, 0, len(s.rich))
	for id, e := range s.rich {
		out = append(out, agentwire.EngineDescriptor{ID: id, Role: string(e.Role())})
	}
	return reply(out, nil)
}

// resolveRich looks an engine up by id, then falls back to the first engine
// whose ROLE matches. Config engine ids are arbitrary ("race-0") while callers
// address engines by role ("race"), so an id-only lookup misses.
func (s *Server) resolveRich(id string) engine.RichEngine {
	s.enginesMu.RLock()
	defer s.enginesMu.RUnlock()
	return s.resolveRichLocked(id)
}

// resolveRichLocked is resolveRich for callers already holding enginesMu.
func (s *Server) resolveRichLocked(id string) engine.RichEngine {
	if e := s.rich[id]; e != nil {
		return e
	}
	for _, e := range s.rich {
		if string(e.Role()) == id {
			return e
		}
	}
	return nil
}

// handleAddRouted materialises the shipped .torrent and adds it through the
// agent's OWN rich engine (race or hoard), so category/announce/drain run here.
func (s *Server) handleAddRouted(p agentwire.AddRoutedParams) (*agentpb.CallReply, error) {
	if len(p.Torrent) == 0 {
		return nil, status.Error(codes.InvalidArgument, "add_routed: empty torrent bytes")
	}
	id := p.Mode
	if id == "" {
		id = agentwire.EngineRace
	}
	e := s.resolveRich(id)
	if e == nil {
		return nil, status.Errorf(codes.Unavailable, "engine %q not wired on agent", id)
	}
	path, err := s.keepTorrentBlob("agent-routed", p.Torrent)
	if err != nil {
		return nil, status.Errorf(codes.Internal, "%v", err)
	}
	ih, err := e.AddRouted(path, "", p.SavePath, nil, p.Category)
	return reply(&ltclient.AddTorrentResult{InfoHash: ih}, err)
}

// handleActionRouted runs a rich per-torrent op through the agent's own engine.
func (s *Server) handleActionRouted(p agentwire.ActionRoutedParams) (*agentpb.CallReply, error) {
	if p.Action == "reannounce" {
		// No-op at the API layer (mirrors the local adapter); engine-agnostic.
		return reply(nil, nil)
	}
	id := p.Mode
	if id == "" {
		id = agentwire.EngineHoard
	}
	e := s.resolveRich(id)
	if e == nil {
		return nil, status.Errorf(codes.Unavailable, "engine %q not wired on agent", id)
	}
	switch p.Action {
	case "pause":
		return reply(nil, e.SetUserPaused(p.InfoHash, true))
	case "resume":
		return reply(nil, e.SetUserPaused(p.InfoHash, false))
	case "remove":
		return reply(nil, e.RemoveRouted(p.InfoHash, p.DeleteFiles))
	case "verify":
		return reply(nil, e.VerifyTorrent(p.InfoHash))
	case "setcategory":
		return reply(nil, e.SetTorrentCategory(p.InfoHash, p.Category, p.SavePath))
	default:
		return nil, status.Errorf(codes.Unimplemented, "unknown action %q", p.Action)
	}
}

// --- Subscribe (event stream) ---

func (s *Server) Subscribe(req *agentpb.SubscribeRequest, stream grpc.ServerStreamingServer[agentpb.EventFrame]) error {
	if !s.ownEvents {
		return status.Error(codes.Unavailable, "agent does not own the event handler (additive mode)")
	}
	// A dedicated agent with a rich engine reads that engine's raw event hub
	// rather than installing a competing single-slot SetEventHandler.
	if hub := s.rawHubFor(req.Engine); hub != nil {
		id, ch := hub.Subscribe()
		defer hub.Unsubscribe(id)
		ctx := stream.Context()
		for {
			select {
			case <-ctx.Done():
				return nil
			case frame, ok := <-ch:
				if !ok {
					return nil
				}
				if err := stream.Send(&agentpb.EventFrame{Payload: frame}); err != nil {
					return err
				}
			}
		}
	}
	c, err := s.client(req.Engine)
	if err != nil {
		return err
	}
	id, ch := s.addSub(req.Engine, c)
	defer s.removeSub(req.Engine, id)
	ctx := stream.Context()
	for {
		select {
		case <-ctx.Done():
			return nil
		case frame, ok := <-ch:
			if !ok {
				return nil
			}
			if err := stream.Send(&agentpb.EventFrame{Payload: frame}); err != nil {
				return err
			}
		}
	}
}

// addSub registers a subscriber channel for an engine, installing the fan-out
// event handler on first use.
func (s *Server) addSub(name string, c engine.EngineClient) (int, chan []byte) {
	s.subMu.Lock()
	defer s.subMu.Unlock()
	if s.subs[name] == nil {
		s.subs[name] = make(map[int]chan []byte)
	}
	s.subSeq++
	id := s.subSeq
	ch := make(chan []byte, 256)
	s.subs[name][id] = ch
	if !s.wired[name] {
		s.wired[name] = true
		engineName := name
		c.SetEventHandler(func(ev ltclient.Event) {
			b, err := json.Marshal(ev)
			if err != nil {
				return
			}
			s.broadcast(engineName, b)
		})
		// Best-effort: opt into push events. Harmless if already subscribed.
		_ = c.SubscribeEvents()
	}
	return id, ch
}

func (s *Server) removeSub(name string, id int) {
	s.subMu.Lock()
	defer s.subMu.Unlock()
	if m := s.subs[name]; m != nil {
		if ch, ok := m[id]; ok {
			close(ch)
			delete(m, id)
		}
	}
}

func (s *Server) broadcast(name string, frame []byte) {
	s.subMu.Lock()
	defer s.subMu.Unlock()
	for _, ch := range s.subs[name] {
		select {
		case ch <- frame:
		default: // slow consumer: drop rather than block the engine reader
		}
	}
}

// ---- Node-level info (exit IP + interfaces) ----

type pubIPCache struct {
	mu      sync.Mutex
	ip      string
	at      time.Time
	lastTry time.Time
	url     string
}

// Retry throttle after a FAILED lookup: without it a v6-enabled node on a host
// with no v6 pays the 8s timeout on every node_info, and the agents table asks
// one node after another.
const pubIPRetryAfter = 60 * time.Second

var (
	agentPubIP   = &pubIPCache{url: "https://api.ipify.org/"}
	agentPubIPv6 = &pubIPCache{url: "https://api6.ipify.org/"}
)

// get refreshes at most every 5 min, and keeps the last known value on failure
// rather than blanking a display that was correct a moment ago.
func (c *pubIPCache) get() string {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.ip != "" && time.Since(c.at) < 5*time.Minute {
		return c.ip
	}
	if !c.lastTry.IsZero() && time.Since(c.lastTry) < pubIPRetryAfter {
		return c.ip
	}
	c.lastTry = time.Now()
	cl := &http.Client{Timeout: 8 * time.Second}
	resp, err := cl.Get(c.url)
	if err != nil {
		return c.ip
	}
	defer resp.Body.Close()
	b, err := io.ReadAll(io.LimitReader(resp.Body, 64))
	if err != nil {
		return c.ip
	}
	if ip := strings.TrimSpace(string(b)); ip != "" {
		c.ip, c.at = ip, time.Now()
	}
	return c.ip
}

// agentPublicIP is this agent's egress IP -- its own tunnel's, behind a VPN.
func agentPublicIP() string { return agentPubIP.get() }

// agentPublicIPv6 hits an AAAA-only endpoint: a v4-only host returns "".
func agentPublicIPv6() string { return agentPubIPv6.get() }

func agentNICs() []agentwire.NICInfo {
	out := []agentwire.NICInfo{}
	ifaces, err := net.Interfaces()
	if err != nil {
		return out
	}
	for _, ifc := range ifaces {
		if ifc.Flags&net.FlagLoopback != 0 {
			continue
		}
		up := ifc.Flags&net.FlagUp != 0
		addrs, _ := ifc.Addrs()
		for _, a := range addrs {
			if ipnet, ok := a.(*net.IPNet); ok {
				if ip4 := ipnet.IP.To4(); ip4 != nil {
					out = append(out, agentwire.NICInfo{Name: ifc.Name, IP: ip4.String(), Up: up})
				}
			}
		}
	}
	return out
}

func (s *Server) handleNodeInfo() (*agentpb.CallReply, error) {
	ni := agentwire.NodeInfo{
		PublicIP:   agentPublicIP(),
		IPv6Wanted: s.ipv6Wanted,
		Interfaces: agentNICs(),
	}
	// Off, the AAAA-only endpoint is unreachable: don't pay its timeout.
	if s.ipv6Wanted {
		ni.PublicIPv6 = agentPublicIPv6()
	}
	return reply(ni, nil)
}

// handleGetAnnounceOverrides returns this agent's per-host passkey + client
// spoof override maps (node-level; the announce overrides are process-global).
func (s *Server) handleGetAnnounceOverrides() (*agentpb.CallReply, error) {
	cl := engine.GetClientOverrides()
	out := agentwire.AnnounceOverrides{
		Passkeys: engine.GetPasskeyOverrides(),
		Clients:  make(map[string]agentwire.ClientSpoofWire, len(cl)),
	}
	for h, c := range cl {
		out.Clients[h] = agentwire.ClientSpoofWire{PeerIDPrefix: c.PeerIDPrefix, UserAgent: c.UserAgent}
	}
	return reply(out, nil)
}

// handleSetAnnounceOverride sets (or clears, on empty value) one announce
// override on this agent, mirroring the local POST /api/announce/* handlers.
func (s *Server) handleSetAnnounceOverride(p agentwire.AnnounceOverrideParams) (*agentpb.CallReply, error) {
	switch p.Kind {
	case "passkey":
		engine.SetPasskeyOverride(p.Host, p.Passkey)
	case "client":
		engine.SetClientOverride(p.Host, p.PeerIDPrefix, p.UserAgent)
	default:
		return reply(nil, fmt.Errorf("unknown announce override kind %q", p.Kind))
	}
	return reply(nil, nil)
}

// engineOrHoard defaults an unspecified engine id to the hoard, which is the
// one a category label is about in every current caller.
func engineOrHoard(id string) string {
	if id == "" {
		return agentwire.EngineHoard
	}
	return id
}
