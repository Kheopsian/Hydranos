package main

import (
	"context"
	"encoding/json"
	"fmt"
	"log/slog"
	"net"
	"net/http"
	"os"
	"os/signal"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"syscall"
	"time"

	"github.com/Kheopsian/hydra/internal/agent"
	"github.com/Kheopsian/hydra/internal/agentwire"
	"github.com/Kheopsian/hydra/internal/config"
	"github.com/Kheopsian/hydra/internal/engine"
	"github.com/Kheopsian/hydra/internal/store"
)

// agentEngineBasePort is the first TCP loopback IPC port handed to an
// agent-only node's engines when the unix-socket transport is unavailable
// (Windows) or unusable (data_dir on a network share).
const agentEngineBasePort = 9510

// liveEngine is one running engine of an agent node (Option A: a node hosts an
// arbitrary set of engines, each with an id + role). cfg is the session config
// it was started on, kept so a later push can be compared against it.
type liveEngine struct {
	id   string
	role string
	cfg  config.SessionConfig
	proc *engine.EngineProcess
	// ref is the stable handle everything else takes. proc changes on every
	// restart and its client dies with it; the ref survives, so a holder does
	// not have to be found and updated one by one.
	ref      *engine.EngineRef
	race     *engine.RaceEngine  // set when role == race
	hoard    *engine.HoardEngine // set when role == hoard
	rich     engine.RichEngine
	store    *store.AgentStore
	ann      *engine.HoardAnnouncer
	stopPump func() // detaches this generation's events from the id's stable hub
}

func (le *liveEngine) metas() map[string]*engine.TorrentMeta {
	if le.race != nil {
		return le.race.GetTorrentMetas()
	}
	return le.hoard.GetTorrentMetas()
}

// runAgentOnly serves a dedicated data-plane agent node: it owns its Typhon
// engines and their event handlers (ownEvents=true), persists each engine to
// its own per-agent store, and exposes the HydraAgent gRPC API — no api.Server.
// Egress is unchanged: the SOCKS5 proxy lives in each engine's Typhon config
// applied at StartSessionEngine.
//
// The node knows only its own identity at boot (engine id, role, listen port,
// IPv6) and takes the rest from its front, so the ORDER here matters: gRPC
// comes up first, before any engine, because the front has to be able to reach
// this node in order to give it the configuration its engines need. See
// engineSupervisor in agentsupervisor.go.
func runAgentOnly(parent context.Context, cfg *config.HydraConfig, boot []agentBootEngine, identitySource, addr, token, tlsCert, tlsKey string, listenPortHook int, healthAddr string) {
	if addr == "" {
		slog.Error("agent-only mode requires --agent-addr or $" + envAgentAddr)
		os.Exit(1)
	}
	if err := validateAgentBoot(boot); err != nil {
		slog.Error("agent-only: invalid engine identity", "error", err)
		os.Exit(1)
	}
	logAgentBoot(boot, identitySource)
	warnIgnoredAgentSections(cfg, identitySource)
	slog.Info("starting in AGENT-ONLY mode", "addr", addr, "engines", len(boot))

	ctx, cancel := context.WithCancel(parent)
	defer cancel()
	sigCh := make(chan os.Signal, 1)
	signal.Notify(sigCh, syscall.SIGTERM, syscall.SIGINT)
	go func() {
		<-sigCh
		slog.Info("agent-only: signal received, shutting down")
		cancel()
	}()

	// ---- HydraAgent gRPC (owns events), before any engine ----
	if token == "" {
		slog.Warn("agent-only served WITHOUT a token: set --agent-token, $" + agentwire.TokenEnv + " or [daemon] agent_token before exposing it off-LAN")
	}
	agentSrv := agent.NewServer(nil, cfg.Daemon.DataDir, token)
	agentSrv.SetUploadsDir(filepath.Join(cfg.Daemon.DataDir, "uploads"))
	agentSrv.SetTLS(tlsCert, tlsKey)
	agentSrv.SetOwnEvents(true)
	descs := make([]agentwire.EngineDescriptor, 0, len(boot))
	v6 := false
	for _, b := range boot {
		descs = append(descs, b.descriptor())
		v6 = v6 || b.EnableIPv6
	}
	agentSrv.DeclareEngines(descs)
	// Whether this node wants a v6 egress is part of its identity, not of what
	// the front pushes: it is read from the boot values, which are also the
	// ones overlaid onto every config that arrives.
	agentSrv.SetIPv6Wanted(v6)

	sup := newEngineSupervisor(ctx, cfg, boot, agentSrv)
	agentSrv.SetConfigManager(sup)

	go func() {
		slog.Info("HydraAgent (agent-only) serving", "addr", addr)
		if serr := agentSrv.Serve(ctx, addr); serr != nil {
			slog.Error("agent-only: gRPC server exited", "error", serr)
		}
	}()

	// ---- Bring the engines up from the cache or the local file ----
	sup.Bootstrap(cfg, identitySource)

	// ---- Periodic store reconcile ----
	go func() {
		t := time.NewTicker(5 * time.Minute)
		defer t.Stop()
		for {
			select {
			case <-ctx.Done():
				return
			case <-t.C:
				sup.Reconcile()
			}
		}
	}()

	// ---- Retry engines that failed to start ----
	// The front will not re-push: it compares revisions, sees a match and
	// stays quiet. So a node that could not bring an engine up has to keep
	// trying on its own, or it waits for a config change that may never come.
	go func() {
		t := time.NewTicker(engineRetryInterval)
		defer t.Stop()
		for {
			select {
			case <-ctx.Done():
				return
			case <-t.C:
				sup.RetryFailedEngines()
			}
		}
	}()

	// ---- Loopback-only listen-port hook (opt-in) ----
	// Lets a gluetun sidecar sharing this netns push the VPN's forwarded port
	// via a plain wget, since agent-only has no api.Server. Bound to 127.0.0.1
	// ONLY: never reachable off the shared netns, never over the VPN tunnel.
	if listenPortHook > 0 {
		startListenPortHook(ctx, listenPortHook, token, sup)
	}

	// ---- Health endpoint for the orchestrator ----
	if a := resolveHealthAddr(healthAddr, cfg); a != "" {
		startHealthEndpoint(ctx, a, sup)
	}

	slog.Info("agent-only: all systems GO")
	<-ctx.Done()

	slog.Info("agent-only: stopping")
	sup.Shutdown()
}

// reconcileAgentStore mirrors one engine's current torrents into its store.
func reconcileAgentStore(name string, st *store.AgentStore, metas map[string]*engine.TorrentMeta) {
	if st == nil {
		return
	}
	items := make([]store.AgentSyncItem, 0, len(metas))
	for ih, m := range metas {
		items = append(items, store.AgentSyncItem{
			InfoHash:        ih,
			SavePath:        m.SavePath,
			Category:        m.Category,
			TorrentFilePath: m.TorrentFilePath,
			CompletedTime:   float64(timeToUnix(m.CompletedTime)),
		})
	}
	if r, err := st.Reconcile(items); err != nil {
		slog.Warn("agent-only: store reconcile failed", "engine", name, "error", err)
	} else if r.Inserted+r.Deleted+r.Missing > 0 {
		slog.Info("agent-only: store reconciled", "engine", name,
			"ins", r.Inserted, "upd", r.Updated, "del", r.Deleted, "miss", r.Missing)
	}
}

// startListenPortHook serves a minimal, loopback-only HTTP endpoint that
// hot-swaps the BT listen port of every engine on this agent node. It exists
// solely so a gluetun container sharing this process's network namespace can
// push its rotated forwarded port (VPN_PORT_FORWARDING_UP_COMMAND -> wget)
// without needing the hydra binary or a gRPC client.
//
// Security: the listener is bound to 127.0.0.1 in HARD code — it is reachable
// only from within the shared netns (gluetun + this agent) and never over the
// VPN tunnel or the LAN. When a token is set (--agent-token) it is required in
// the X-API-Key header, matching the monolith's native /api/*/listen-port.
func startListenPortHook(ctx context.Context, port int, token string, sup *engineSupervisor) {
	mux := http.NewServeMux()
	mux.HandleFunc("/listen-port", func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodPost {
			http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
			return
		}
		if token != "" && r.Header.Get("X-API-Key") != token {
			http.Error(w, "unauthorized", http.StatusUnauthorized)
			return
		}
		var req struct {
			Port int `json:"port"`
		}
		if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
			http.Error(w, err.Error(), http.StatusBadRequest)
			return
		}
		if req.Port <= 0 || req.Port > 65535 {
			http.Error(w, "port out of range (1-65535)", http.StatusBadRequest)
			return
		}
		n, failed := 0, 0
		// Read the live set per request rather than closing over a boot-time
		// slice: a config push restarts engines, and a hook holding the old
		// objects would rebind engines that no longer exist while the running
		// ones kept the port the VPN has stopped forwarding.
		for _, le := range sup.liveEngines() {
			var sp interface{ SetListenPort(int) error }
			if le.race != nil {
				sp = le.race
			} else if le.hoard != nil {
				sp = le.hoard
			}
			if sp == nil {
				continue
			}
			if err := sp.SetListenPort(req.Port); err != nil {
				slog.Warn("agent-only: listen-port rebind failed", "port", req.Port, "err", err)
				failed++
				continue
			}
			n++
		}
		// gluetun retries on a non-2xx, so a total failure must not look like
		// success — otherwise the node sits on a port the VPN no longer
		// forwards and nothing ever corrects it.
		if n == 0 && failed > 0 {
			http.Error(w, "listen-port rebind failed on every agent", http.StatusInternalServerError)
			return
		}
		slog.Info("agent-only: listen-port hook applied", "port", req.Port, "engines", n, "failed", failed)
		w.Header().Set("Content-Type", "application/json")
		fmt.Fprintf(w, `{"status":"ok","port":%d,"engines":%d,"failed":%d}`, req.Port, n, failed)
	})

	// HARD-CODED loopback bind: only the port is configurable, never the host,
	// so the hook cannot be exposed on the published gRPC addr or the LAN.
	addr := "127.0.0.1:" + strconv.Itoa(port)
	srv := &http.Server{Addr: addr, Handler: mux}
	go func() {
		<-ctx.Done()
		_ = srv.Close()
	}()
	go func() {
		slog.Info("agent-only: listen-port hook serving (loopback only)", "addr", addr)
		if token == "" {
			slog.Warn("agent-only: listen-port hook has NO token: loopback-only, but set --agent-token, $" +
				agentwire.TokenEnv + " or [daemon] agent_token for defense-in-depth")
		}
		if err := srv.ListenAndServe(); err != nil && err != http.ErrServerClosed {
			slog.Error("agent-only: listen-port hook exited", "error", err)
		}
	}()
}

// resolveHealthAddr defaults to the config's API host:port: agent-only runs no
// api.Server, so that port is free and is the one already published.
func resolveHealthAddr(flagVal string, cfg *config.HydraConfig) string {
	if strings.EqualFold(flagVal, "off") || strings.EqualFold(flagVal, "none") {
		return ""
	}
	if flagVal != "" {
		return flagVal
	}
	host := cfg.Daemon.APIHost
	if host == "" {
		host = "0.0.0.0"
	}
	if cfg.Daemon.APIPort <= 0 {
		return ""
	}
	return net.JoinHostPort(host, strconv.Itoa(cfg.Daemon.APIPort))
}

// A wedged engine must fail the probe, not hold it open until its own timeout.
const enginePingTimeout = 3 * time.Second

// startHealthEndpoint serves GET /health, the only HTTP an agent-only node has.
// Unauthenticated like the monolith's: a container probe carries no token.
func startHealthEndpoint(ctx context.Context, addr string, sup *engineSupervisor) {
	mux := http.NewServeMux()
	mux.HandleFunc("/health", agentHealthHandler(sup)) // everything else 404s: this is a probe, not the REST API
	srv := &http.Server{Addr: addr, Handler: mux}
	go func() {
		<-ctx.Done()
		_ = srv.Close()
	}()
	go func() {
		slog.Info("agent-only: health endpoint serving", "addr", addr, "path", "/health")
		if err := srv.ListenAndServe(); err != nil && err != http.ErrServerClosed {
			slog.Error("agent-only: health endpoint exited", "error", err)
		}
	}()
}

var agentStart = time.Now()

type engineHealth struct {
	ID    string `json:"id"`
	Role  string `json:"role"`
	OK    bool   `json:"ok"`
	Error string `json:"error,omitempty"`
}

// A dead engine still leaves gRPC answering, so healthy means every engine
// pings -- not merely that this process exists.
//
// The set probed is the one this node DECLARES, not the one it happens to be
// running: an engine waiting for a configuration, or one whose last start
// failed, is a node that seeds nothing, and reporting only the live engines
// would call that healthy because the list came out empty.
func agentHealthHandler(sup *engineSupervisor) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodGet && r.Method != http.MethodHead {
			http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
			return
		}
		st := sup.ConfigState()
		lives := make(map[string]*liveEngine, len(st.Engines))
		for _, le := range sup.liveEngines() {
			lives[le.id] = le
		}
		engines := make([]engineHealth, 0, len(st.Engines))
		healthy := true
		for id, es := range st.Engines {
			eh := engineHealth{ID: id, Role: es.Role, OK: true}
			switch le := lives[id]; {
			case es.State == agentwire.EngineStateError:
				eh.OK, eh.Error = false, es.Error
			case le == nil:
				eh.OK, eh.Error = false, "no configuration yet: waiting for the front to push one"
			default:
				if err := pingEngine(le); err != nil {
					eh.OK, eh.Error = false, err.Error()
				}
			}
			healthy = healthy && eh.OK
			engines = append(engines, eh)
		}
		// ConfigState keys a map, and a probe that reorders its own output
		// between two calls reads like something changed.
		sort.Slice(engines, func(i, j int) bool { return engines[i].ID < engines[j].ID })
		status, code := "healthy", http.StatusOK
		if !healthy {
			status, code = "unhealthy", http.StatusServiceUnavailable
		}
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(code)
		_ = json.NewEncoder(w).Encode(map[string]interface{}{
			"status":  status,
			"mode":    "agent-only",
			"version": version,
			"uptime":  time.Since(agentStart).Seconds(),
			"engines": engines,
		})
	}
}

// pingEngine probes one engine. Ping takes no context, so a hung one is left to
// its own goroutine instead of holding the response.
func pingEngine(le *liveEngine) error {
	if le.proc == nil || le.proc.Client() == nil {
		return fmt.Errorf("agent not started")
	}
	done := make(chan error, 1)
	go func() { done <- le.proc.Client().Ping() }()
	select {
	case err := <-done:
		return err
	case <-time.After(enginePingTimeout):
		return fmt.Errorf("ping timed out after %s", enginePingTimeout)
	}
}
