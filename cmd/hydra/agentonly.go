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
	"strconv"
	"strings"
	"syscall"
	"time"

	"github.com/Kheopsian/hydra/internal/agent"
	"github.com/Kheopsian/hydra/internal/agentwire"
	"github.com/Kheopsian/hydra/internal/choking"
	"github.com/Kheopsian/hydra/internal/config"
	"github.com/Kheopsian/hydra/internal/engine"
	"github.com/Kheopsian/hydra/internal/store"
)

// agentEngineBasePort is the first TCP loopback IPC port handed to an
// agent-only node's engines when the unix-socket transport is unavailable
// (Windows) or unusable (data_dir on a network share).
const agentEngineBasePort = 9510

// liveEngine is one running engine of an agent node (Option A: a node hosts an
// arbitrary set of engines, each with an id + role).
type liveEngine struct {
	id    string
	role  string
	cfg   config.SessionConfig
	proc  *engine.EngineProcess
	race  *engine.RaceEngine  // set when role == race
	hoard *engine.HoardEngine // set when role == hoard
	rich  engine.RichEngine
	store *store.AgentStore
	ann   *engine.HoardAnnouncer
}

func (le *liveEngine) metas() map[string]*engine.TorrentMeta {
	if le.race != nil {
		return le.race.GetTorrentMetas()
	}
	return le.hoard.GetTorrentMetas()
}

// runAgentOnly serves a dedicated data-plane agent node: it owns its Typhon
// engines (an arbitrary set resolved from config) and their event handlers
// (ownEvents=true), persists each engine to its own per-agent store, and exposes
// the HydraAgent gRPC API — no api.Server. Egress is unchanged: the SOCKS5 proxy
// lives in each engine's Typhon config applied at StartSessionEngine.
func runAgentOnly(parent context.Context, cfg *config.HydraConfig, addr, token, tlsCert, tlsKey string, listenPortHook int, healthAddr string) {
	if addr == "" {
		slog.Error("agent-only mode requires --agent-addr")
		os.Exit(1)
	}
	engineCfgs, err := cfg.ResolveEngines()
	if err != nil {
		slog.Error("agent-only: invalid engine config", "error", err)
		os.Exit(1)
	}
	slog.Info("starting in AGENT-ONLY mode", "addr", addr, "engines", len(engineCfgs))

	ctx, cancel := context.WithCancel(parent)
	defer cancel()
	sigCh := make(chan os.Signal, 1)
	signal.Notify(sigCh, syscall.SIGTERM, syscall.SIGINT)
	go func() {
		<-sigCh
		slog.Info("agent-only: signal received, shutting down")
		cancel()
	}()

	engine.InitPasskeyOverrides(cfg.AnnouncePasskeys)
	clientSpoofs := make(map[string]engine.ClientSpoof, len(cfg.AnnounceClients))
	for host, c := range cfg.AnnounceClients {
		clientSpoofs[host] = engine.ClientSpoof{PeerIDPrefix: c.PeerIDPrefix, UserAgent: c.UserAgent}
	}
	engine.InitClientOverrides(clientSpoofs)
	engine.InitSecondaryStatsOverrides(cfg.AnnounceSecondaryStats)

	uploadsDir := filepath.Join(cfg.Daemon.DataDir, "uploads")

	// ---- Spawn + build each engine ----
	var lives []*liveEngine
	var raceEngines []*engine.RaceEngine
	stopAll := func() {
		for _, le := range lives {
			le.proc.Stop()
		}
	}
	for i, ec := range engineCfgs {
		eDir := filepath.Join(cfg.Daemon.DataDir, ec.ID)
		_ = os.MkdirAll(eDir, 0755)
		// Same endpoint selection as the monolith: a hardcoded .sock path here
		// would hand Typhon a unix path on Windows, where its listener is a
		// stub that refuses to bind, so the engine never starts. Ports are
		// per-engine so N engines on one node never collide, and start above
		// the monolith 9501/9502 so an agent can share a machine with a daemon.
		sock := engineSocketPath(cfg.Daemon.DataDir, ec.ID, agentEngineBasePort+i)
		le := &liveEngine{id: ec.ID, role: ec.Role, cfg: ec.SessionConfig}
		proc, perr := engine.StartSessionEngine(&le.cfg, eDir, sock, ec.Role == "race")
		if perr != nil {
			slog.Error("agent-only: start engine", "id", ec.ID, "error", perr)
			stopAll()
			os.Exit(1)
		}
		le.proc = proc
		if ec.Role == "race" {
			var ck engine.ChokingEngineInterface
			if le.cfg.CustomChoking != nil && le.cfg.CustomChoking.Enabled {
				ck = choking.NewChokingEngine(le.cfg.CustomChoking)
			}
			re := engine.NewRaceEngine(&le.cfg, ck, nil, eDir)
			re.SetClient(proc.Client())
			le.race, le.rich = re, re
			raceEngines = append(raceEngines, re)
		} else {
			he := engine.NewHoardEngine(&le.cfg, eDir)
			he.SetClient(proc.Client())
			le.hoard, le.rich = he, he
		}
		if st, serr := store.OpenAgent(filepath.Join(eDir, "store.db")); serr != nil {
			slog.Error("agent-only: open store", "id", ec.ID, "error", serr)
		} else {
			le.store = st
		}
		lives = append(lives, le)
	}

	// Aggregated race gate: a hoard must not announce an info_hash any race
	// engine on this node already holds (anti dual-announce), generalised to N
	// race engines.
	raceGate := func(infoHash string) bool {
		for _, r := range raceEngines {
			if r.HasTorrent(infoHash) {
				return true
			}
		}
		return false
	}

	// ---- Start engines, announcers, reload from store ----
	for _, le := range lives {
		if le.race != nil {
			if serr := le.race.Start(ctx); serr != nil {
				slog.Error("agent-only: race start", "id", le.id, "error", serr)
				stopAll()
				os.Exit(1)
			}
		} else {
			if serr := le.hoard.Start(ctx); serr != nil {
				slog.Error("agent-only: hoard start", "id", le.id, "error", serr)
				stopAll()
				os.Exit(1)
			}
		}
		ann := engine.NewHoardAnnouncer(le.proc.Client(), engine.ApplyAnnounceEgress(
			engine.DefaultSingleBinding(le.cfg.ListenPort, le.cfg.EnableIPv6, "hoard", le.cfg.AnnounceRateLimit),
			le.cfg.AnnounceProxy, le.cfg.AnnounceIP, le.cfg.Socks5OutboundHost, "hoard"))
		if le.hoard != nil {
			ann.OnObservation = le.hoard.ObserveAnnounce
			le.hoard.SetBootstrapAnnounce(ann.BootstrapAnnounce)
			le.hoard.SetReAnnounce(ann.ReAnnounce)
			ann.SetRaceGate(raceGate)
			ann.SetOffsetFn(le.hoard.AnnounceOffset)
		}
		ann.Start(ctx)
		le.ann = ann

		if le.store != nil {
			var imp, errs int
			if le.race != nil {
				imp, errs = le.race.ImportFromStore(le.store, uploadsDir)
			} else {
				imp, errs = le.hoard.ImportFromStore(le.store, uploadsDir)
			}
			slog.Info("agent-only: store reload", "id", le.id, "imported", imp, "errors", errs)
		}
	}

	// ---- Periodic store reconcile ----
	go func() {
		t := time.NewTicker(5 * time.Minute)
		defer t.Stop()
		for {
			select {
			case <-ctx.Done():
				return
			case <-t.C:
				for _, le := range lives {
					reconcileAgentStore(le.id, le.store, le.metas())
				}
			}
		}
	}()

	// ---- HydraAgent gRPC (owns events) ----
	if token == "" {
		slog.Warn("agent-only served WITHOUT a token: set --agent-token, $" + agentwire.TokenEnv + " or [daemon] agent_token before exposing it off-LAN")
	}
	engines := make(map[string]engine.EngineClient, len(lives))
	for _, le := range lives {
		engines[le.id] = le.proc.Client()
	}
	agentSrv := agent.NewServer(engines, cfg.Daemon.DataDir, token)
	agentSrv.SetUploadsDir(uploadsDir)
	agentSrv.SetTLS(tlsCert, tlsKey)
	agentSrv.SetOwnEvents(true)
	for _, le := range lives {
		agentSrv.AddRichEngine(le.id, le.rich)
	}
	go func() {
		slog.Info("HydraAgent (agent-only) serving", "addr", addr)
		if serr := agentSrv.Serve(ctx, addr); serr != nil {
			slog.Error("agent-only: gRPC server exited", "error", serr)
		}
	}()

	// ---- Loopback-only listen-port hook (opt-in) ----
	// Lets a gluetun sidecar sharing this netns push the VPN's forwarded port
	// via a plain wget, since agent-only has no api.Server. Bound to 127.0.0.1
	// ONLY: never reachable off the shared netns, never over the VPN tunnel.
	if listenPortHook > 0 {
		startListenPortHook(ctx, listenPortHook, token, lives)
	}

	// ---- Health endpoint for the orchestrator ----
	if a := resolveHealthAddr(healthAddr, cfg); a != "" {
		startHealthEndpoint(ctx, a, lives)
	}

	slog.Info("agent-only: all systems GO")
	<-ctx.Done()

	// ---- Graceful shutdown ----
	slog.Info("agent-only: stopping")
	for _, le := range lives {
		if le.ann != nil {
			le.ann.Stop()
		}
	}
	for _, le := range lives {
		if le.store != nil {
			reconcileAgentStore(le.id, le.store, le.metas())
			le.store.Close()
		}
		le.proc.Stop()
	}
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
func startListenPortHook(ctx context.Context, port int, token string, lives []*liveEngine) {
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
		for _, le := range lives {
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
			http.Error(w, "listen-port rebind failed on every engine", http.StatusInternalServerError)
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
func startHealthEndpoint(ctx context.Context, addr string, lives []*liveEngine) {
	mux := http.NewServeMux()
	mux.HandleFunc("/health", agentHealthHandler(lives)) // everything else 404s: this is a probe, not the REST API
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
func agentHealthHandler(lives []*liveEngine) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodGet && r.Method != http.MethodHead {
			http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
			return
		}
		engines := make([]engineHealth, 0, len(lives))
		healthy := true
		for _, le := range lives {
			eh := engineHealth{ID: le.id, Role: le.role, OK: true}
			if err := pingEngine(le); err != nil {
				eh.OK, eh.Error, healthy = false, err.Error(), false
			}
			engines = append(engines, eh)
		}
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
		return fmt.Errorf("engine not started")
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
