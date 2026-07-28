// Package agent exposes one machine's engine sessions (race/hoard) over the
// HydraAgent gRPC contract, which is a thin JSON mirror of engine.EngineClient
// (see internal/agentwire). A remote control plane (front) dials in and drives
// each session's EngineClient as if it were local. Additive: only served when
// --agent-addr is set, so it has zero impact on the monolith otherwise.
package agent

import (
	"context"
	"crypto/subtle"
	"encoding/json"
	"fmt"
	"net"
	"os"
	"sync"

	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/credentials"
	"google.golang.org/grpc/metadata"
	"google.golang.org/grpc/status"

	"github.com/Kheopsian/hydra/internal/agentpb"
	"github.com/Kheopsian/hydra/internal/agentwire"
	"github.com/Kheopsian/hydra/internal/engine"
	"github.com/Kheopsian/hydra/internal/engine/ltclient"
)

// Server implements agentpb.HydraAgentServer by routing each Call to the named
// local EngineClient and JSON-marshalling the ltclient return value back.
type Server struct {
	agentpb.UnimplementedHydraAgentServer

	engines map[string]engine.EngineClient
	tmpDir  string
	token   string
	tlsCert string
	tlsKey  string

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
}

// NewServer builds an agent over the given named EngineClients (e.g.
// {"race": ..., "hoard": ...}). tmpDir holds shipped .torrent bytes on add.
// token (if non-empty) is the required bearer token.
func NewServer(engines map[string]engine.EngineClient, tmpDir, token string) *Server {
	return &Server{
		engines: engines,
		tmpDir:  tmpDir,
		token:   token,
		subs:    make(map[string]map[int]chan []byte),
		wired:   make(map[string]bool),
		rich:    make(map[string]engine.RichEngine),
	}
}

// SetTLS enables TLS with the given cert/key files (empty = plaintext).
func (s *Server) SetTLS(certFile, keyFile string) { s.tlsCert, s.tlsKey = certFile, keyFile }

// SetOwnEvents opts the agent into owning the engine event handler. Safe only
// when nothing else consumes these EngineClients' events (dedicated agent).
func (s *Server) SetOwnEvents(v bool) { s.ownEvents = v }

// AddRichEngine registers a role-typed local engine under an id for routed
// add/action/subscribe. Ids are arbitrary ("race", "hoard", "hoard2", ...).
func (s *Server) AddRichEngine(id string, r engine.RichEngine) {
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

// rawHubFor returns the raw ltclient.Event hub of the rich engine registered
// under id, or nil if none (additive mode).
func (s *Server) rawHubFor(id string) *engine.EventHub {
	if e := s.rich[id]; e != nil {
		return e.RawEventHub()
	}
	return nil
}

func (s *Server) client(name string) (engine.EngineClient, error) {
	c, ok := s.engines[name]
	if !ok || c == nil {
		return nil, status.Errorf(codes.NotFound, "unknown engine %q", name)
	}
	return c, nil
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

	case agentwire.MethodGetTrackers:
		var p agentwire.InfoHashParams
		if err := unmarshal(&p); err != nil {
			return nil, badParams(err)
		}
		return reply(c.GetTrackers(p.InfoHash))

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

// handleAdd materialises the shipped .torrent bytes to a temp file, then calls
// the local AddTorrent variant.
func (s *Server) handleAdd(c engine.EngineClient, p agentwire.AddParams) (*agentpb.CallReply, error) {
	if len(p.Torrent) == 0 {
		return nil, status.Error(codes.InvalidArgument, "add_torrent: empty torrent bytes")
	}
	f, err := os.CreateTemp(s.tmpDir, "agent-add-*.torrent")
	if err != nil {
		return nil, status.Errorf(codes.Internal, "temp file: %v", err)
	}
	defer os.Remove(f.Name())
	if _, err := f.Write(p.Torrent); err != nil {
		f.Close()
		return nil, status.Errorf(codes.Internal, "write temp: %v", err)
	}
	f.Close()
	if p.WithOptions {
		return reply(c.AddTorrentWithOptions(f.Name(), p.SavePath, p.Stopped, p.SeedMode))
	}
	return reply(c.AddTorrent(f.Name(), p.SavePath, p.Stopped))
}

// handleAddRouted materialises the shipped .torrent and adds it through the
// agent's OWN rich engine (race or hoard), so category/announce/drain run here.
// handleListEngines enumerates the role-typed engines this agent hosts.
func (s *Server) handleListEngines() (*agentpb.CallReply, error) {
	out := make([]agentwire.EngineDescriptor, 0, len(s.rich))
	for id, e := range s.rich {
		out = append(out, agentwire.EngineDescriptor{ID: id, Role: string(e.Role())})
	}
	return reply(out, nil)
}

func (s *Server) handleAddRouted(p agentwire.AddRoutedParams) (*agentpb.CallReply, error) {
	if len(p.Torrent) == 0 {
		return nil, status.Error(codes.InvalidArgument, "add_routed: empty torrent bytes")
	}
	id := p.Mode
	if id == "" {
		id = agentwire.EngineRace
	}
	e := s.rich[id]
	if e == nil {
		return nil, status.Errorf(codes.Unavailable, "engine %q not wired on agent", id)
	}
	f, err := os.CreateTemp(s.tmpDir, "agent-routed-*.torrent")
	if err != nil {
		return nil, status.Errorf(codes.Internal, "temp file: %v", err)
	}
	defer os.Remove(f.Name())
	if _, err := f.Write(p.Torrent); err != nil {
		f.Close()
		return nil, status.Errorf(codes.Internal, "write temp: %v", err)
	}
	f.Close()
	ih, err := e.AddRouted(f.Name(), "", p.SavePath, nil, p.Category)
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
	e := s.rich[id]
	if e == nil {
		return nil, status.Errorf(codes.Unavailable, "engine %q not wired on agent", id)
	}
	switch p.Action {
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
