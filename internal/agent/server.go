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
	"github.com/Kheopsian/hydra/internal/engine"
	"github.com/Kheopsian/hydra/internal/engine/ltclient"
)

// Server implements agentpb.HydraAgentServer by routing each Call to the named
// local EngineClient and JSON-marshalling the ltclient return value back.
type Server struct {
	agentpb.UnimplementedHydraAgentServer

	engines map[string]engine.EngineClient
	tmpDir  string
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
	if req.Method == agentwire.MethodNodeInfo {
		return s.handleNodeInfo()
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
	// Durable, content-addressed — NOT a temp file. The engine keeps this path
	// as the torrent's TorrentFilePath and the 5-minute store reconcile reads
	// it to capture the blob; a path deleted on return meant every routed add
	// was counted `miss` by the reconcile and never made it into the store, so
	// it carried no category/save_path/tags across a restart. A blob left
	// behind by a failed add is content-addressed, so a retry reuses it as is.
	path, err := engine.MaterializeTorrentBlob(s.uploads(), p.Torrent)
	if err != nil {
		return nil, status.Errorf(codes.Internal, "materialize torrent: %v", err)
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
	e := s.rich[id]
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

var (
	agentPubIP     string
	agentPubIPTime time.Time
	agentPubIPMu   sync.Mutex
)

// agentPublicIP returns this agent's egress IP (its own tunnel's public IP when
// behind a per-agent VPN). Cached 5 min. Empty on failure (keeps last value).
func agentPublicIP() string {
	agentPubIPMu.Lock()
	defer agentPubIPMu.Unlock()
	if agentPubIP != "" && time.Since(agentPubIPTime) < 5*time.Minute {
		return agentPubIP
	}
	cl := &http.Client{Timeout: 8 * time.Second}
	resp, err := cl.Get("https://api.ipify.org/")
	if err != nil {
		return agentPubIP
	}
	defer resp.Body.Close()
	b, err := io.ReadAll(io.LimitReader(resp.Body, 64))
	if err != nil {
		return agentPubIP
	}
	if ip := strings.TrimSpace(string(b)); ip != "" {
		agentPubIP, agentPubIPTime = ip, time.Now()
	}
	return agentPubIP
}

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
	return reply(agentwire.NodeInfo{PublicIP: agentPublicIP(), Interfaces: agentNICs()}, nil)
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
