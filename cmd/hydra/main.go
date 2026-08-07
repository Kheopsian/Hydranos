package main

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	_ "net/http/pprof"
	"os"
	"os/signal"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"time"

	hydraroot "github.com/Kheopsian/hydra"
	"github.com/Kheopsian/hydra/internal/agent"
	"github.com/Kheopsian/hydra/internal/agentwire"
	"github.com/Kheopsian/hydra/internal/api"
	"github.com/Kheopsian/hydra/internal/bench"
	"github.com/Kheopsian/hydra/internal/choking"
	"github.com/Kheopsian/hydra/internal/cleanup"
	"github.com/Kheopsian/hydra/internal/config"
	"github.com/Kheopsian/hydra/internal/drain"
	"github.com/Kheopsian/hydra/internal/engine"
	"github.com/Kheopsian/hydra/internal/engine/ltclient"
	"github.com/Kheopsian/hydra/internal/health"
	"github.com/Kheopsian/hydra/internal/logs"
	"github.com/Kheopsian/hydra/internal/metrics"
	"github.com/Kheopsian/hydra/internal/notify"
	"github.com/Kheopsian/hydra/internal/state"
	"github.com/Kheopsian/hydra/internal/store"
	"github.com/Kheopsian/hydra/internal/system"
	"github.com/Kheopsian/hydra/internal/tagstore"
	version_pkg "github.com/Kheopsian/hydra/internal/version"

	"golang.org/x/crypto/bcrypt"
)

var version = version_pkg.Version

// engineSocketPath returns the IPC endpoint for a session engine. Default
// (Linux) is a Unix domain socket under the data dir. Set HYDRA_ENGINE_TCP
// (any non-empty value) to use a TCP loopback endpoint instead — required
// on Windows/macOS, which have no Unix domain socket in this IPC path, and
// handy for testing the TCP transport on Linux.
func engineSocketPath(dataDir, name string, tcpPort int) string {
	if defaultEngineTCP || os.Getenv("HYDRA_ENGINE_TCP") != "" {
		return fmt.Sprintf("tcp://127.0.0.1:%d", tcpPort)
	}
	return filepath.Join(dataDir, name+".sock")
}

// resolveConfigPath picks the config file. With --config given explicitly we
// use it as-is. Otherwise try, in order: the compiled default path, a
// default.toml next to the executable, then default.toml in the working dir.
// If none exist, write a fresh default.toml next to the executable (with a
// relative data_dir) so a freshly-unzipped, double-clicked install just runs.
func resolveConfigPath(def string, explicit bool) string {
	if explicit {
		return def
	}
	exeDir := ""
	if exe, err := os.Executable(); err == nil {
		exeDir = filepath.Dir(exe)
	}
	candidates := []string{def}
	if exeDir != "" {
		candidates = append(candidates, filepath.Join(exeDir, "default.toml"))
	}
	candidates = append(candidates, "default.toml")
	for _, c := range candidates {
		if st, err := os.Stat(c); err == nil && !st.IsDir() {
			return c
		}
	}
	// Nothing found — write a fresh default next to the executable.
	target := "default.toml"
	if exeDir != "" {
		target = filepath.Join(exeDir, "default.toml")
	}
	doc := hydraroot.DefaultConfigTOML
	if d, err := config.SetTOMLValue(doc, "daemon", "data_dir", `"data"`); err == nil {
		doc = d
	}
	if defaultAPIHost != "" {
		if d, err := config.SetTOMLValue(doc, "daemon", "api_host", `"`+defaultAPIHost+`"`); err == nil {
			doc = d
		}
	}
	if err := os.WriteFile(target, []byte(doc), 0644); err != nil {
		slog.Warn("no config found and could not write a default; using compiled default path", "target", target, "err", err)
		return def
	}
	slog.Info("no config found — wrote a fresh default", "path", target)
	return target
}

func main() {
	// `hydra hash-password <pw>` : imprime un hash bcrypt a coller dans [auth]
	// password_hash (bootstrap du login, aucune connexion requise).
	if len(os.Args) > 1 && os.Args[1] == "hash-password" {
		if len(os.Args) < 3 {
			fmt.Fprintln(os.Stderr, "usage: hydra hash-password <password>")
			os.Exit(2)
		}
		h, err := bcrypt.GenerateFromPassword([]byte(os.Args[2]), bcrypt.DefaultCost)
		if err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
		fmt.Println(string(h))
		return
	}

	// `hydra reset-password <newpass> [config]` : hash <newpass> and write it into
	// [auth] password_hash of the config in one step, so a locked-out admin can
	// recover without hand-editing TOML (the generated first-run password is never
	// stored in cleartext). Config path defaults to ${HYDRA_CONFIG_DIR}/default.toml
	// (else /config/default.toml), matching the daemon's own resolution.
	if len(os.Args) > 1 && os.Args[1] == "reset-password" {
		if len(os.Args) < 3 {
			fmt.Fprintln(os.Stderr, "usage: hydra reset-password <newpassword> [config-path]")
			os.Exit(2)
		}
		cfgPath := "/config/default.toml"
		if len(os.Args) >= 4 {
			cfgPath = os.Args[3]
		} else if d := os.Getenv("HYDRA_CONFIG_DIR"); d != "" {
			cfgPath = filepath.Join(d, "default.toml")
		}
		h, err := bcrypt.GenerateFromPassword([]byte(os.Args[2]), bcrypt.DefaultCost)
		if err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
		data, err := os.ReadFile(cfgPath)
		if err != nil {
			fmt.Fprintln(os.Stderr, "read config", cfgPath, ":", err)
			os.Exit(1)
		}
		doc, err := config.SetTOMLValue(string(data), "auth", "password_hash", fmt.Sprintf("%q", string(h)))
		if err != nil {
			fmt.Fprintln(os.Stderr, "set password_hash:", err)
			os.Exit(1)
		}
		if err := os.WriteFile(cfgPath, []byte(doc), 0644); err != nil {
			fmt.Fprintln(os.Stderr, "write config", cfgPath, ":", err)
			os.Exit(1)
		}
		fmt.Printf("admin password updated in %s - restart Hydra to apply\n", cfgPath)
		return
	}

	// `hydra set-listen-port <engine-socket> <port>` : push a new BT listen port

	// to a running engine over its Unix socket (hot rebind, no restart). Meant to
	// be run by gluetun's VPN_PORT_FORWARDING_UP_COMMAND inside an agent container,
	// where there is no api.Server to POST to. Works for any engine socket.
	if len(os.Args) > 1 && os.Args[1] == "set-listen-port" {
		if len(os.Args) < 4 {
			fmt.Fprintln(os.Stderr, "usage: hydra set-listen-port <engine-socket> <port>")
			os.Exit(2)
		}
		sock := os.Args[2]
		port, err := strconv.Atoi(os.Args[3])
		if err != nil || port <= 0 || port > 65535 {
			fmt.Fprintln(os.Stderr, "invalid port:", os.Args[3])
			os.Exit(2)
		}
		c, err := ltclient.Connect(sock)
		if err != nil {
			fmt.Fprintln(os.Stderr, "connect", sock, ":", err)
			os.Exit(1)
		}
		defer c.Close()
		if err := c.SetListenPort(port); err != nil {
			fmt.Fprintln(os.Stderr, "set-listen-port:", err)
			os.Exit(1)
		}
		fmt.Printf("listen port set to %d on %s\n", port, sock)
		return
	}

	// pprof profiler (localhost only) — added 2026-07-22 to diagnose Go CPU usage.
	// Port 6060 is NOT usable in prod: hydra runs --network host and CrowdSec
	// already owns 6060 there, so the bind failed silently and prod had no
	// profiler for two weeks. Default to 6061; HYDRA_PPROF_ADDR overrides, and
	// the failure is now logged loudly enough to notice.
	go func() {
		addr := os.Getenv("HYDRA_PPROF_ADDR")
		if addr == "" {
			addr = "127.0.0.1:6061"
		}
		slog.Info("pprof listening", "addr", addr)
		if err := http.ListenAndServe(addr, nil); err != nil {
			slog.Error("pprof server could not listen (profiler unavailable)", "addr", addr, "err", err)
		}
	}()

	configPath := flag.String("config", "/config/default.toml", "Path to TOML config file")
	showVersion := flag.Bool("version", false, "Show version and exit")
	frontOnly := flag.Bool("front-only", false, "run as a controller node: no local engine, drive remote [[agent]]s only")
	agentAddr := flag.String("agent-addr", "", "if set, also serve the HydraAgent gRPC data-plane on this addr (e.g. :9090)")
	agentToken := flag.String("agent-token", "", "shared bearer token required by the HydraAgent gRPC API (empty = no auth)")
	agentTLSCert := flag.String("agent-tls-cert", "", "TLS cert file for the HydraAgent gRPC API (with --agent-tls-key)")
	agentTLSKey := flag.String("agent-tls-key", "", "TLS key file for the HydraAgent gRPC API")
	agentOnly := flag.Bool("agent-only", false, "run as a dedicated agent: engines + gRPC data-plane, no api.Server, owns events")
	listenPortHook := flag.Int("listen-port-hook", 0, "agent-only: serve a loopback-only (127.0.0.1) HTTP POST /listen-port hook on this port so a co-netns gluetun UP_COMMAND can push the forwarded BT port; 0 = disabled")
	bootFromStore := flag.Bool("boot-from-store", true, "load torrents from the SQLite store (content-addressed, durable); state.json runs as an overlay/fallback. Default on since v2.9.x; disable with --boot-from-store=false")
	flag.Parse()

	if *showVersion {
		fmt.Println("hydra", version)
		os.Exit(0)
	}

	// Structured logging funnels into the in-process hub (ring buffer for the
	// UI "Logs" tab + hydra.log mirror). The console stays clean: only ERROR
	// surfaces there, plus the explicit human startup banner below.
	logHub := logs.Default
	logger := slog.New(logs.NewMultiHandler(
		logs.NewSlogHandler(logHub, slog.LevelInfo),
		slog.NewTextHandler(os.Stderr, &slog.HandlerOptions{Level: slog.LevelError}),
	))
	slog.SetDefault(logger)
	logs.PrintHeader(version)

	// Raise file descriptor limit for the Go process + children.
	raiseNofileLimit(1000000)

	slog.Info("============================================================")
	slog.Info("  HYDRA TORRENT DAEMON — Typhon engine")
	slog.Info("============================================================")
	slog.Info("Hydra starting", "version", version)
	api.Version = version

	// Zero-setup start: if the user did not pass --config, look for a
	// default.toml next to the executable (and CWD); if none exists, write a
	// fresh one there so a freshly-unzipped, double-clicked install just runs.
	// Docker/systemd pass --config explicitly and keep their exact path.
	configExplicit := false
	flag.Visit(func(f *flag.Flag) {
		if f.Name == "config" {
			configExplicit = true
		}
	})
	*configPath = resolveConfigPath(*configPath, configExplicit)
	logHub.SetMirrorFileBeside(*configPath, "hydra.log")

	cfg, err := config.Load(*configPath)
	if err != nil {
		slog.Error("Failed to load config", "path", *configPath, "error", err)
		os.Exit(1)
	}
	// Cle API auto-generee au 1er demarrage UNIQUEMENT si vide (une install fraiche
	// = api_key="" dans le template) et persistee -> cle unique sans config manuelle.
	// On ne touche PAS une cle existante meme "change-me-in-production" : ecraser une
	// cle en place casse les clients (autobrr/arr) qui l'utilisent deja.
	if cfg.Daemon.APIKey == "" {
		b := make([]byte, 24)
		if _, e := rand.Read(b); e == nil {
			cfg.Daemon.APIKey = hex.EncodeToString(b)
			if data, e2 := os.ReadFile(*configPath); e2 == nil {
				if doc, e3 := config.SetTOMLValue(string(data), "daemon", "api_key", fmt.Sprintf("%q", cfg.Daemon.APIKey)); e3 == nil {
					if e4 := os.WriteFile(*configPath, []byte(doc), 0644); e4 == nil {
						slog.Info("generated a random API key and saved it to config")
					} else {
						slog.Warn("generated API key not persisted (ephemeral this run)", "err", e4)
					}
				} else {
					slog.Warn("generated API key not persisted (no api_key line?)", "err", e3)
				}
			}
		} else {
			slog.Error("failed to generate API key", "err", e)
		}
	}
	// Mot de passe admin auto-genere au 1er demarrage UNIQUEMENT si vide, sur le
	// modele de l'api_key : hash bcrypt persiste dans [auth] password_hash, et le
	// plaintext logge UNE fois (au boot suivant password_hash != "" -> pas de
	// regeneration). Sans ca une install fraiche (password_hash="") ne peut pas se
	// connecter a l'UI (/api/login renvoie 503 "auth not configured").
	// Captured for the startup banner + admin-credentials.txt (never logged).
	var bootUser, bootPass string
	var bootNewPass bool
	if cfg.Auth.PasswordHash == "" {
		pb := make([]byte, 9)
		if _, e := rand.Read(pb); e == nil {
			pw := hex.EncodeToString(pb) // 18 chars hex, copy-paste safe
			if h, eh := bcrypt.GenerateFromPassword([]byte(pw), bcrypt.DefaultCost); eh == nil {
				cfg.Auth.PasswordHash = string(h)
				if data, e2 := os.ReadFile(*configPath); e2 == nil {
					if doc, e3 := config.SetTOMLValue(string(data), "auth", "password_hash", fmt.Sprintf("%q", string(h))); e3 == nil {
						if e4 := os.WriteFile(*configPath, []byte(doc), 0644); e4 == nil {
							bootUser, bootPass, bootNewPass = cfg.Auth.Username, pw, true
							slog.Info("generated a temporary admin password (shown in the console banner + admin-credentials.txt)")
						} else {
							bootUser, bootPass, bootNewPass = cfg.Auth.Username, pw, true
							slog.Warn("generated admin password not persisted (ephemeral this run)", "err", e4)
						}
					} else {
						bootUser, bootPass, bootNewPass = cfg.Auth.Username, pw, true
						slog.Warn("generated admin password not persisted (no password_hash line?)", "err", e3)
					}
				}
			} else {
				slog.Error("failed to hash generated admin password", "err", eh)
			}
		} else {
			slog.Error("failed to generate admin password", "err", e)
		}
	}
	// ---- Resolve the engines this node runs (Option A). A legacy default.toml
	// with [race]/[hoard] yields exactly those two; [[engine]] blocks override.
	// Install the shared SOCKS5 exit BEFORE anything can call getPublicIP
	// (status snapshots, agents). Otherwise the first public-IP lookup races
	// the proxy install, goes out direct, and caches the home WAN IP in the
	// header for 5 minutes at launch.
	api.SetSocks5Proxy(api.NewSOCKS5DialerFromConfig(cfg.Proxy))

	// Portable data_dir: an empty or relative path resolves next to the
	// executable, so a zip that runs from anywhere keeps its data beside it.
	// Absolute paths (Docker /config, bare-metal /var/lib/hydra) are untouched.
	if cfg.Daemon.DataDir == "" {
		cfg.Daemon.DataDir = "data"
	}
	if !filepath.IsAbs(cfg.Daemon.DataDir) {
		base := "."
		if exe, e := os.Executable(); e == nil {
			base = filepath.Dir(exe)
		}
		cfg.Daemon.DataDir = filepath.Join(base, cfg.Daemon.DataDir)
	}

	engineCfgs, engErr := cfg.ResolveEngines()
	if engErr != nil {
		slog.Error("invalid engine config", "error", engErr)
		os.Exit(1)
	}
	if extras, xerr := config.LoadExtraEngines(cfg.Daemon.DataDir); xerr != nil {
		slog.Warn("extra engines: load failed", "error", xerr)
	} else if len(extras) > 0 {
		merged := append(append([]config.EngineConfig{}, engineCfgs...), extras...)
		if verr := config.ValidateEngines(merged); verr != nil {
			slog.Error("extra engines: invalid, ignoring engines.json", "error", verr)
		} else {
			engineCfgs = merged
			slog.Info("extra engines loaded", "count", len(extras))
		}
	}
	var raceCfg, hoardCfg *config.SessionConfig
	for i := range engineCfgs {
		ec := &engineCfgs[i]
		if ec.Role == "race" && raceCfg == nil {
			raceCfg = &ec.SessionConfig
		}
		if ec.Role == "hoard" && hoardCfg == nil {
			hoardCfg = &ec.SessionConfig
		}
	}
	if raceCfg == nil || hoardCfg == nil {
		slog.Error("monolith requires at least one race and one hoard engine")
		os.Exit(1)
	}
	slog.Info("Config loaded",
		"api", fmt.Sprintf("%s:%d", cfg.Daemon.APIHost, cfg.Daemon.APIPort),
		"race_port", raceCfg.ListenPort,
		"hoard_port", hoardCfg.ListenPort,
		"data_dir", cfg.Daemon.DataDir,
	)

	// Discord notifications
	var notifier *notify.Notifier
	if cfg.Notify.Enabled && cfg.Notify.WebhookURL != "" {
		notifier = notify.New(cfg.Notify.WebhookURL)
	} else {
		notifier = notify.New("")
	}

	// State manager
	stateMgr, err := state.NewManager(cfg.Daemon.DataDir)
	if err != nil {
		slog.Error("Failed to init state manager", "error", err)
		os.Exit(1)
	}

	// ---- SQLite torrent store (shadow; durable identity, see internal/store) ----
	// Best-effort: a store failure must never block the daemon. Backfilled once
	// asynchronously from state.json, then kept current via saveState's SyncSession.
	// storeReady gates sync until the initial backfill completes so a saveState
	// tick never runs the heavy first import inline.
	var torStore *store.Store
	var storeReady atomic.Bool
	if *frontOnly || *agentOnly {
		// controller / agent mode: no monolith shadow store (agents own their
		// per-agent DBs; a shadow hydra.db here would be a parasitic dangling file).
	} else if ts, terr := store.Open(filepath.Join(cfg.Daemon.DataDir, "hydra.db")); terr != nil {
		slog.Error("store: open failed — shadow persistence disabled", "error", terr)
	} else {
		torStore = ts
		defer torStore.Close()
		// Hand the store to the API layer before anything reads a counter, and
		// fold in the JSON sidecars while nothing else is writing them. Both
		// must happen before initBaselinePersistence, which is what decides
		// whether the store or the files are authoritative.
		api.SetStore(torStore)
		if rep := store.MigrateSidecars(cfg.Daemon.DataDir, torStore); !rep.Empty() || len(rep.Errors) > 0 {
			if len(rep.Errors) > 0 {
				slog.Error("store: sidecar import had errors — originals kept in place",
					"report", rep.String(), "errors", rep.Errors)
			} else {
				slog.Info("store: imported JSON sidecars (originals renamed .migrated)", "report", rep.String())
			}
		}
		go func() {
			if n, _ := torStore.Count(store.Hoard); n == 0 {
				if r, ierr := torStore.ImportLegacy(filepath.Join(cfg.Daemon.DataDir, "state.json")); ierr != nil {
					slog.Warn("store: initial backfill failed", "error", ierr)
				} else {
					slog.Info("store: initial backfill done",
						"imported", r.Imported, "missing_file", r.MissingFile, "errors", r.Errors)
				}
			}
			storeReady.Store(true)
		}()
	}

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	if *frontOnly {
		runFrontOnly(ctx, cfg)
		return
	}
	if *agentOnly {
		runAgentOnly(ctx, cfg, *agentAddr, *agentToken, *agentTLSCert, *agentTLSKey, *listenPortHook)
		return
	}

	sigCh := make(chan os.Signal, 1)
	signal.Notify(sigCh, syscall.SIGTERM, syscall.SIGINT)

	// ---- Start C++ engine processes ----
	raceDataDir := filepath.Join(cfg.Daemon.DataDir, "race")
	_ = os.MkdirAll(raceDataDir, 0755)
	hoardDataDir := filepath.Join(cfg.Daemon.DataDir, "hoard")
	_ = os.MkdirAll(hoardDataDir, 0755)

	raceSocketPath := engineSocketPath(cfg.Daemon.DataDir, "race", 9501)
	hoardSocketPath := engineSocketPath(cfg.Daemon.DataDir, "hoard", 9502)

	slog.Info("Starting race engine process...")
	raceProc, err := engine.StartSessionEngine(raceCfg, raceDataDir, raceSocketPath, true)
	if err != nil {
		slog.Error("Failed to start race engine process", "error", err)
		os.Exit(1)
	}
	slog.Info("Race engine process started")

	slog.Info("Starting hoard engine process...")
	hoardProc, err := engine.StartSessionEngine(hoardCfg, hoardDataDir, hoardSocketPath, false)
	if err != nil {
		slog.Error("Failed to start hoard engine process", "error", err)
		raceProc.Stop()
		os.Exit(1)
	}
	slog.Info("Hoard engine process started")

	// ---- Create Go engines (wired to IPC clients) ----
	var chokingEngine engine.ChokingEngineInterface
	if raceCfg.CustomChoking != nil && raceCfg.CustomChoking.Enabled {
		chokingEngine = choking.NewChokingEngine(raceCfg.CustomChoking)
	}

	engine.InitPasskeyOverrides(cfg.AnnouncePasskeys)
	clientSpoofs := make(map[string]engine.ClientSpoof, len(cfg.AnnounceClients))
	for host, c := range cfg.AnnounceClients {
		clientSpoofs[host] = engine.ClientSpoof{PeerIDPrefix: c.PeerIDPrefix, UserAgent: c.UserAgent}
	}
	engine.InitClientOverrides(clientSpoofs)
	engine.InitSecondaryStatsOverrides(cfg.AnnounceSecondaryStats)
	raceEngine := engine.NewRaceEngine(raceCfg, chokingEngine, nil, raceDataDir)
	raceEngine.SetClient(raceProc.Client())

	hoardEngine := engine.NewHoardEngine(hoardCfg, hoardDataDir)
	hoardEngine.CreateTorrentFolder = cfg.Daemon.CreateTorrentFolder
	hoardEngine.SetClient(hoardProc.Client())

	// ---- Optional: HydraAgent gRPC data-plane (pivot 25/07) ----
	// Additive: only when --agent-addr is set. Exposes this machine's engine
	// sessions as EngineClient mirrors so a remote front can drive them. Off by
	// default = zero impact on the monolith. ownEvents stays false: the
	// in-process engines already own SetEventHandler on these live clients.
	if *agentAddr != "" {
		if *agentToken == "" {
			slog.Warn("HydraAgent served WITHOUT a token (--agent-token) — do not expose off-LAN")
		}
		agentSrv := agent.NewServer(map[string]engine.EngineClient{
			agentwire.EngineRace:  raceProc.Client(),
			agentwire.EngineHoard: hoardProc.Client(),
		}, cfg.Daemon.DataDir, *agentToken)
		agentSrv.SetTLS(*agentTLSCert, *agentTLSKey)
		agentSrv.SetRichEngines(raceEngine, hoardEngine)
		go func() {
			slog.Info("HydraAgent gRPC serving", "addr", *agentAddr)
			if err := agentSrv.Serve(ctx, *agentAddr); err != nil {
				slog.Error("HydraAgent gRPC server exited", "err", err)
			}
		}()
	}

	// Wire stats absorption: any removal path (API, drain, cleanup) absorbs stats.
	raceEngine.SetOnBeforeRemove(func(infoHash string, ul, dl int64) {
		// Doublon dual-seed : si le hoard tient le même infohash, on lui lègue
		// l'UL/DL du race comme OFFSET D'ANNONCE -> le hoard reprend l'annonce
		// sans trou de cumulé côté tracker. Le global reste préservé par
		// AbsorbStats (l'offset ne touche QUE le cumulé annoncé, pas le global).
		// [anti-dual removed 2026-08-01] multi-seed is legitimate; no announce
		// offset handoff. Both engines announce independently.
		// Unconditional: the torrent's row must go now, not at the next
		// five-minute sync. A restart inside that window used to re-import a
		// torrent the user had just deleted, and it came back from the dead
		// (issue #13). One transaction, so the carry-overs and the row move
		// together and a crash can neither double-count nor lose the bytes.
		api.AbsorbOnRemove("race", raceEngine.TrackerHostFor(infoHash), infoHash, ul, dl)
	})
	hoardEngine.SetOnBeforeRemove(func(infoHash string, ul, dl int64) {
		api.AbsorbOnRemove("hoard", hoardEngine.TrackerHostFor(infoHash), infoHash, ul, dl)
	})

	// ---- Secondary modules ----
	arrCleanup := cleanup.NewArrCleanup(
		&hoardCleanupAdapter{engine: hoardEngine},
		cfg.ArrCleanup,
	)
	raceDrain := drain.NewRaceDrain(
		cfg.RaceDrain,
		&raceDrainAdapter{engine: raceEngine},
	)
	gradr := &graduator{race: raceEngine, hoard: hoardEngine, dataDir: cfg.Daemon.DataDir, active: map[string]*gradProgress{}}
	raceDrain.SetGraduator(gradr)
	benchDBPath := filepath.Join(cfg.Daemon.DataDir, "bench.db")
	benchDB := bench.NewBenchDB(benchDBPath)
	if err := benchDB.Open(); err != nil {
		slog.Warn("Failed to open bench DB", "error", err)
	}

	raceEngine.SetOnEvent(func(event string, stats engine.TorrentStats) {
		now := float64(time.Now().Unix())
		var timeSinceAdd float64
		if stats.AddedTime > 0 {
			timeSinceAdd = now - float64(stats.AddedTime)
		}
		var dlTime float64
		if event == "completed" && stats.AddedTime > 0 {
			dlTime = float64(stats.CompletedTime) - float64(stats.AddedTime)
		}
		ev := bench.RaceEvent{
			Ts:            now,
			InfoHash:      stats.InfoHash,
			Event:         event,
			Name:          stats.Name,
			Size:          stats.TotalSize,
			DownloadTime:  dlTime,
			UploadTotal:   stats.TotalUpload,
			DownloadTotal: stats.TotalDownload,
			UploadRate:    float64(stats.UploadRate),
			DownloadRate:  float64(stats.DownloadRate),
			Peers:         stats.NumPeers,
			Seeds:         stats.NumSeeds,
			SwarmSeeds:    stats.SwarmSeeds,
			SwarmLeechers: stats.SwarmLeechers,
			Category:      stats.Category,
			TimeSinceAdd:  timeSinceAdd,
			Uploader:      stats.Uploader,
			InjectedPeers: stats.InjectedPeers,
		}
		benchDB.InsertRaceEvent(ev)
	})

	raceEngine.SetOnRemove(func(infoHash string) {
		benchDB.PurgeRaceData(infoHash)
	})

	metricsCollector := metrics.NewMetricsCollector(
		&raceMetricsAdapter{engine: raceEngine},
		&hoardMetricsAdapter{engine: hoardEngine},
	)

	// ---- API Server ----
	apiServer := api.NewServer(cfg)
	apiServer.SetEngines(
		&raceAPIAdapter{engine: raceEngine},
		&hoardAPIAdapter{engine: hoardEngine},
	)
	apiServer.SetStateManager(stateMgr)
	// ---- Remote agents ([[agent]]) for multi-home category placement ----
	// Dialed pull-only; a dead agent is logged + skipped, never blocks boot.
	for _, ag := range cfg.Agents {
		if ag.Name == "" || ag.Addr == "" {
			continue
		}
		if err := apiServer.AddRemoteAgent(ag.Name, ag.Addr, ag.Token, ag.TLSCa); err != nil {
			slog.Warn("remote agent dial failed", "name", ag.Name, "addr", ag.Addr, "err", err)
			continue
		}
		slog.Info("remote agent registered", "name", ag.Name, "addr", ag.Addr)
	}
	apiServer.LoadPersistedAgents()
	apiServer.SetRaceDrain(raceDrain)
	apiServer.SetGraduationReporter(gradr)
	apiServer.SetArrCleanup(arrCleanup)
	apiServer.SetBenchDB(&benchAPIAdapter{db: benchDB})
	apiServer.SetSaveStateCallback(func() {
		saveState(stateMgr, raceEngine, hoardEngine, torStore, &storeReady)
	})

	// ---- Health invariant scanner ----
	// Convert past incidents into standing conservation-law checks (re-DL,
	// fake-seed, starved leech, missing files) exposed at /api/health/anomalies.
	// 5-min tick: ListTorrents is a heavy IPC payload at 24k+ torrents, and a
	// first scan one interval after boot lets announces converge (less noise).
	// Edge-triggered ntfy alerts on high-severity anomalies (topic overridable
	// via HYDRA_NTFY_TOPIC; empty disables push). nil sender = safe no-op.
	ntfyTopic := os.Getenv("HYDRA_NTFY_TOPIC")
	healthNtfy := notify.NewNtfy(ntfyTopic)
	healthScanner := health.NewScanner(
		hoardEngine.ListStatuses,
		raceEngine.ListStatuses,
		nil,
		benchDB,
		healthNtfy.Send,
	)
	apiServer.SetHealthReporter(healthScanner)
	go healthScanner.Run(ctx, 5*time.Minute)

	// Engine watchdog: liveness + per-engine RSS ceiling. On a dead/zombie
	// engine or runaway RSS it snapshots memory composition then triggers a
	// graceful self-restart (Docker --restart reboots a clean pair). Closes the
	// 2026-07-09 gap where the race engine OOM'd to 85GB and stayed dead 3h with
	// no alert. Limits: race 24GiB (~12x its ~2GB normal), hoard 48GiB (~2x its
	// ~24GB) on a 125GB box.
	engine.StartEngineWatchdog(ctx, healthNtfy.Send, cfg.Daemon.DataDir, raceProc, hoardProc, 24<<30, 48<<30)

	go func() {
		if err := apiServer.Run(); err != nil {
			slog.Error("API server error", "error", err)
		}
	}()
	slog.Info("API server started",
		"addr", fmt.Sprintf("http://%s:%d", cfg.Daemon.APIHost, cfg.Daemon.APIPort),
	)

	// ---- Estimate total torrents ----
	savedState, err := stateMgr.Load()
	if err != nil {
		slog.Warn("Failed to load state", "error", err)
	}
	expectedTotal := 0
	if savedState != nil {
		expectedTotal = len(savedState.Race) + len(savedState.HoardActive)
	}
	if expectedTotal == 0 {
		expectedTotal = 1
	}
	api.SetStartupTotal(expectedTotal)

	// ---- Start race engine ----
	if err := raceEngine.Start(ctx); err != nil {
		slog.Error("Failed to start race engine", "error", err)
		os.Exit(1)
	}
	slog.Info("Race Engine: started")
	api.SetStartupProgress(raceEngine.TorrentCount())

	// ---- Start hoard engine ----
	hoardLoadStart := time.Now()
	hoardDone := make(chan error, 1)
	go func() {
		hoardDone <- hoardEngine.Start(ctx)
	}()

	pollTicker := time.NewTicker(1 * time.Second)
	hoardStarted := false
	raceCount := raceEngine.TorrentCount()
	for !hoardStarted {
		select {
		case err := <-hoardDone:
			if err != nil {
				slog.Error("Failed to start hoard engine", "error", err)
				os.Exit(1)
			}
			hoardStarted = true
		case <-pollTicker.C:
			hoardCount := hoardEngine.TorrentCount()
			if hoardCount > 0 {
				api.SetStartupProgress(raceCount + hoardCount)
			} else {
				elapsed := time.Since(hoardLoadStart).Seconds()
				hoardTotal := expectedTotal - raceCount
				simPct := elapsed / 50.0
				if simPct > 0.98 {
					simPct = 0.98
				}
				simCount := int(float64(hoardTotal) * simPct)
				api.SetStartupProgress(raceCount + simCount)
			}
		}
	}
	pollTicker.Stop()
	api.SetStartupProgress(expectedTotal)
	slog.Info("Hoard Engine: started", "torrents", hoardEngine.TorrentCount())

	// ---- Per-disk seed-slot regulation (HDD quiet mode) ----
	// Opt-in via [hoard.disk_slots]; off (nil) by default -> no-op.
	if ds := cfg.Hoard.DiskSlots; ds != nil && ds.Enabled {
		slotMgr := engine.NewDiskSlotManager(*ds, func(ih string, suspended bool) {
			if err := hoardEngine.SetServingSuspended(ih, suspended); err != nil {
				slog.Warn("disk-slots: suspend call failed", "ih", ih, "suspended", suspended, "err", err)
			}
		})
		go func() {
			tk := time.NewTicker(slotMgr.CycleInterval())
			defer tk.Stop()
			slog.Info("disk-slots: regulation enabled", "disks", len(ds.Disks))
			for {
				select {
				case <-ctx.Done():
					return
				case <-tk.C:
					sts, err := hoardEngine.ListStatuses()
					if err != nil {
						continue
					}
					snap := make([]engine.SlotTorrent, 0, len(sts))
					for _, s := range sts {
						snap = append(snap, engine.SlotTorrent{
							InfoHash:       s.InfoHash,
							SavePath:       s.SavePath,
							UploadRate:     int64(s.UploadRate),
							ScrapeSeeders:  s.NumSeeds,
							ScrapeLeechers: s.ListPeers,
							Seeding:        s.IsSeeding && !s.IsPaused,
						})
					}
					slotMgr.Tick(snap, time.Now())
				}
			}
		}()
	}

	// ---- Start Go-canonical tracker announcers ----
	// Typhon's internal announce loop is disabled via DisableInternalAnnounce
	// in BuildHoardConfig/BuildRaceConfig. Go now owns all tracker announces,
	// which prepares the multi-binding (multi WG tunnel) extension where each
	// binding announces with its own peer_id and source IP.
	// Build the announce-side bindings: when Proton is enabled, use the
	// tunnel-derived bindings (one per WG tunnel, distinct peer_id, source
	// IP, public IP for tracker). Otherwise fall back to the legacy
	// single-binding for the FOU/wstunnel path.
	hoardAnnounceBindings := engine.DefaultSingleBinding(hoardCfg.ListenPort)
	raceAnnounceBindings := engine.DefaultSingleBinding(raceCfg.ListenPort)
	hoardAnnouncer := engine.NewHoardAnnouncer(hoardProc.Client(), hoardAnnounceBindings)
	hoardAnnouncer.OnObservation = hoardEngine.ObserveAnnounce
	hoardAnnouncer.SetLivePort(hoardEngine.LivePort())
	// On-add bootstrap announce: a freshly-added download announces once
	// immediately so its swarm_seeds reaches the slot-manager cache before the
	// first slot decision (else seeds=0 -> parked -> never announced catch-22).
	hoardEngine.SetBootstrapAnnounce(hoardAnnouncer.BootstrapAnnounce)
	hoardEngine.SetReAnnounce(hoardAnnouncer.ReAnnounce)
	// Anti dual-annonce : le hoard n'annonce PAS un infohash que le race tient
	// (le race est seul annonceur tant qu'il l'a) + offset de continuité au
	// handoff race->hoard. Le race lui-même n'est pas gaté (toujours annonceur).
	// [anti-dual removed 2026-08-01] no race-gate / no announce-offset:
	// race and hoard both announce + seed the same infohash (legit multi-seed).
	hoardAnnouncer.Start(ctx)
	raceAnnouncer := engine.NewHoardAnnouncer(raceProc.Client(), raceAnnounceBindings)
	raceAnnouncer.SetLivePort(raceEngine.LivePort())
	raceAnnouncer.Start(ctx)

	// ---- Option A: extra engines beyond primary race+hoard (sharding) ----
	extraEngines, shardAddr, shardToken := startExtraEngines(ctx, cfg, engineCfgs, raceCfg, hoardCfg, raceEngine.HasTorrent)
	if shardAddr != "" {
		go func() {
			for i := 0; i < 12; i++ {
				time.Sleep(300 * time.Millisecond)
				if err := apiServer.AddRemoteAgent("local-shards", shardAddr, shardToken, ""); err == nil {
					slog.Info("local shard engines registered as agent", "addr", shardAddr, "count", len(extraEngines))
					return
				}
			}
			slog.Warn("failed to register local shard engines")
		}()
	}
	if len(extraEngines) > 0 {
		go func() {
			<-ctx.Done()
			for _, le := range extraEngines {
				if le.ann != nil {
					le.ann.Stop()
				}
				if le.store != nil {
					reconcileAgentStore(le.id, le.store, le.metas())
					le.store.Close()
				}
				le.proc.Stop()
			}
		}()
	}

	go func() {
		hoardEngine.WaitStaggerDone()
		apiServer.CaptureSessionOffset()
	}()

	// ---- boot-from-store (SQLite, content-addressed) ----
	// Reversible read-path flip: load durable identity/metainfo from the store
	// instead of trusting state.json + uploads/ (whose reused filenames caused
	// silent torrent loss). The state.json block below still runs as a metadata
	// overlay / gap-fill. Flag defaults off -> monolith behaviour unchanged.
	if *bootFromStore && torStore != nil {
		up := filepath.Join(cfg.Daemon.DataDir, "uploads")
		rN, rE := raceEngine.ImportFromStoreSession(torStore, store.Race, up)
		hN, hE := hoardEngine.ImportFromStoreSession(torStore, store.Hoard, up)
		slog.Info("boot-from-store: loaded torrents from SQLite store",
			"race", rN, "race_errors", rE, "hoard", hN, "hoard_errors", hE)
	}

	// ---- Restore metadata from state.json ----
	if savedState != nil {
		hoardCount := hoardEngine.TorrentCount()
		raceCount := raceEngine.TorrentCount()

		if hoardCount == 0 && len(savedState.HoardActive) > 0 {
			slog.Info("=== MIGRATION: importing hoard torrents from state.json ===",
				"count", len(savedState.HoardActive))
			hoardMetas := make(map[string]*engine.TorrentMeta, len(savedState.HoardActive))
			for _, meta := range savedState.HoardActive {
				hoardMetas[meta.TorrentFilePath] = &engine.TorrentMeta{
					SavePath:        meta.SavePath,
					TorrentFilePath: meta.TorrentFilePath,
					Category:        meta.Category,
					ContentFolder:   meta.ContentFolder,
				}
			}
			hoardEngine.ImportFromState(hoardMetas)
		} else if hoardCount > 0 {
			for ih, meta := range savedState.HoardActive {
				ct := time.Unix(int64(meta.CompletedTime), 0)
				if meta.CompletedTime == 0 {
					ct = time.Time{}
				}
				hoardEngine.RestoreMetadata(ih, meta.Category, meta.SavePath, meta.TorrentFilePath, ct)
				hoardEngine.SetContentFolder(ih, meta.ContentFolder)
				// Gap-fill (mirror of the race block): if a torrent lives in
				// state.json but not in the engine (e.g. present in state.json but
				// missing from the store), re-add it as long as its .torrent file
				// still exists. Belt-and-suspenders alongside --boot-from-store.
				if !hoardEngine.HasTorrent(ih) && meta.TorrentFilePath != "" {
					if _, serr := os.Stat(meta.TorrentFilePath); serr == nil {
						if _, aerr := hoardEngine.AddTorrentSeedMode(meta.TorrentFilePath, meta.SavePath, meta.Category); aerr == nil {
							hoardEngine.RestoreMetadata(ih, meta.Category, meta.SavePath, meta.TorrentFilePath, ct)
							hoardEngine.SetContentFolder(ih, meta.ContentFolder)
						}
					}
				}
			}
		}

		if raceCount == 0 && len(savedState.Race) > 0 {
			slog.Info("=== MIGRATION: importing race torrents from state.json ===",
				"count", len(savedState.Race))
			for _, meta := range savedState.Race {
				if _, serr := os.Stat(meta.TorrentFilePath); serr != nil {
					continue
				}
				_, aerr := raceEngine.AddTorrent(meta.TorrentFilePath, "", meta.SavePath, nil, meta.Category)
				if aerr != nil {
					slog.Warn("race: migration error", "torrent", meta.TorrentFilePath, "error", aerr)
				}
			}
		} else if raceCount > 0 {
			for ih, meta := range savedState.Race {
				ct := time.Unix(int64(meta.CompletedTime), 0)
				if meta.CompletedTime == 0 {
					ct = time.Time{}
				}
				raceEngine.RestoreMetadata(ih, meta.Category, meta.SavePath, meta.TorrentFilePath, ct)
				if !raceEngine.HasTorrent(ih) && meta.TorrentFilePath != "" {
					if _, serr := os.Stat(meta.TorrentFilePath); serr == nil {
						if _, aerr := raceEngine.AddTorrent(meta.TorrentFilePath, "", meta.SavePath, nil, meta.Category); aerr == nil {
							raceEngine.RestoreMetadata(ih, meta.Category, meta.SavePath, meta.TorrentFilePath, ct)
						}
					}
				}
			}
		}
	}

	// ---- Restore per-torrent tags. They live on the torrent row now, so they
	// cannot outlive the torrent; the tags.json overlay is only read on a node
	// with no store (an upgrading installation had its file imported into the
	// store at Open and renamed aside). ----
	if torStore != nil {
		if tt, terr := torStore.AllTags(); terr != nil {
			slog.Error("tags: restore from store failed", "error", terr)
		} else if len(tt) > 0 {
			hoardEngine.RestoreTags(tt)
			slog.Info("tags: restored from store", "torrents", len(tt))
		}
	} else if tt := tagstore.Load(cfg.Daemon.DataDir); len(tt) > 0 {
		hoardEngine.RestoreTags(tt)
		slog.Info("tags: restored from overlay", "torrents", len(tt))
	}

	// ---- Re-apply the user's pause intent. The engines have just reloaded
	// everything in a running state, so without this a paused torrent would
	// quietly start seeding again on every restart — the exact failure the
	// feature exists to prevent. ----
	if torStore != nil {
		if paused, perr := torStore.PausedSet(); perr != nil {
			slog.Error("pause: restoring intent failed", "error", perr)
		} else if len(paused) > 0 {
			h := hoardEngine.RestoreUserPaused(paused)
			r := raceEngine.RestoreUserPaused(paused)
			slog.Info("pause: restored user intent", "hoard", h, "race", r, "stored", len(paused))
		}
	}

	// ---- Start secondary modules ----
	raceDrain.Start(ctx)

	api.SetStartupReady(true)

	// ---- Background loops ----
	// Keep the engine self-dial filter fresh: push our observed public IP to
	// both engines so we never waste a connect dialing ourselves, even when the
	// ISP lease changes (correctness is still guaranteed by the peer_id check).
	go func() {
		push := func() {
			ips := api.PublicIPs()
			if len(ips) == 0 {
				return
			}
			raceEngine.SetSelfIPs(ips)
			hoardEngine.SetSelfIPs(ips)
		}
		push()
		tk := time.NewTicker(2 * time.Minute)
		defer tk.Stop()
		for {
			select {
			case <-ctx.Done():
				return
			case <-tk.C:
				push()
			}
		}
	}()

	go func() {
		ticker := time.NewTicker(5 * time.Second)
		defer ticker.Stop()
		for {
			select {
			case <-ctx.Done():
				return
			case <-ticker.C:
				snap := collectBenchSnapshot(raceEngine, hoardEngine)
				benchDB.Insert(snap)
				benchDB.InsertTrackerSamples(collectTrackerSamples(raceEngine, hoardEngine))

				raceTorrents := raceEngine.GetTorrentList()
				now := float64(time.Now().Unix())
				var snapshots []bench.RaceSnapshot
				activeHashes := make(map[string]struct{}, len(raceTorrents))
				for _, t := range raceTorrents {
					isActive := t.DownloadRate > 0 || t.UploadRate > 0 || t.NumPeers > 0 || t.Progress < 1.0
					if !isActive {
						continue
					}
					activeHashes[t.InfoHash] = struct{}{}
					peersJSON := "[]"
					if t.Progress < 1.0 && t.NumPeers > 0 {
						peers := raceEngine.GetPeersForTorrent(t.InfoHash)
						if len(peers) > 0 {
							peerRates.enrichRates(t.InfoHash, peers, now)
							peersJSON = collectPeersJSON(peers)
						}
					}
					snapshots = append(snapshots, bench.RaceSnapshot{
						Ts: now, InfoHash: t.InfoHash,
						Progress: t.Progress, UploadRate: float64(t.UploadRate),
						DownloadRate: float64(t.DownloadRate),
						TotalUpload:  t.TotalUpload, TotalDownload: t.TotalDownload,
						Peers: t.NumPeers, Seeds: t.NumSeeds,
						SwarmSeeds: t.SwarmSeeds, SwarmLeechers: t.SwarmLeechers,
						Ratio: t.Ratio, PeersJSON: peersJSON,
					})
				}
				if len(snapshots) > 0 {
					benchDB.InsertRaceSnapshots(snapshots)
				}
				peerRates.purgeInactive(activeHashes)
			}
		}
	}()

	go func() {
		ticker := time.NewTicker(1 * time.Hour)
		defer ticker.Stop()
		for {
			select {
			case <-ctx.Done():
				return
			case <-ticker.C:
				benchDB.PurgeOld()
			}
		}
	}()

	go func() {
		ticker := time.NewTicker(5 * time.Minute)
		defer ticker.Stop()
		for {
			select {
			case <-ctx.Done():
				return
			case <-ticker.C:
				saveState(stateMgr, raceEngine, hoardEngine, torStore, &storeReady)
			}
		}
	}()

	api.SetLoadSampler(func() (float64, float64) {
		var ul, dl float64
		if raceEngine != nil {
			st := raceEngine.GetAllStatus()
			ul += toFloat(st["upload_rate"])
			dl += toFloat(st["download_rate"])
		}
		if hoardEngine != nil {
			st := hoardEngine.GetAllStatus()
			ul += toFloat(st["upload_rate"])
		}
		return ul, dl
	})
	api.StartVpnSpeedtestLoop(ctx, cfg.VpnSpeedtest, &benchAPIAdapter{db: benchDB})

	api.SetStartupReady(true)
	slog.Info("============================================================")
	slog.Info("  All systems GO — Typhon engine")
	slog.Info("============================================================")

	if bootNewPass {
		logs.WriteAdminCredentials(*configPath, bootUser, bootPass)
	}
	logs.PrintReady(cfg.Daemon.APIHost, cfg.Daemon.APIPort, cfg.Auth.Username, bootPass, bootNewPass)

	_ = notifier
	_ = metricsCollector
	_ = system.Collect

	sig := <-sigCh
	slog.Info("Received signal, shutting down", "signal", sig)
	cancel()

	slog.Info("Saving state...")
	saveState(stateMgr, raceEngine, hoardEngine, torStore, &storeReady)

	benchDB.Close()

	hoardAnnouncer.Stop()
	raceAnnouncer.Stop()
	raceEngine.Stop()
	hoardEngine.Stop()

	// Stop engine processes.
	slog.Info("Stopping engine processes...")
	raceProc.Stop()
	hoardProc.Stop()

	slog.Info("Hydra stopped")
}

// ---------------------------------------------------------------------------
// State persistence
// ---------------------------------------------------------------------------

func saveState(stateMgr *state.Manager, race *engine.RaceEngine, hoard *engine.HoardEngine, torStore *store.Store, ready *atomic.Bool) {
	raceMetas := race.GetTorrentMetas()
	hoardMetas := hoard.GetTorrentMetas()

	raceState := make(map[string]*state.TorrentMeta, len(raceMetas))
	for ih, m := range raceMetas {
		raceState[ih] = &state.TorrentMeta{
			SavePath:        m.SavePath,
			TorrentFilePath: m.TorrentFilePath,
			Category:        m.Category,
			CompletedTime:   float64(timeToUnix(m.CompletedTime)),
			ContentFolder:   m.ContentFolder,
		}
	}

	hoardState := make(map[string]*state.TorrentMeta, len(hoardMetas))
	for ih, m := range hoardMetas {
		hoardState[ih] = &state.TorrentMeta{
			SavePath:        m.SavePath,
			TorrentFilePath: m.TorrentFilePath,
			Category:        m.Category,
			CompletedTime:   float64(timeToUnix(m.CompletedTime)),
			ContentFolder:   m.ContentFolder,
		}
	}

	if err := stateMgr.Save(raceState, hoardState); err != nil {
		slog.Error("Failed to save state", "error", err)
	}

	// Shadow-sync the SQLite store (best-effort; never affects state.json).
	if torStore != nil && ready != nil && ready.Load() {
		syncStore(torStore, raceMetas, hoardMetas)
	}
}

func syncStore(st *store.Store, raceMetas, hoardMetas map[string]*engine.TorrentMeta) {
	items := make([]store.SyncItem, 0, len(raceMetas)+len(hoardMetas))
	add := func(sess store.Session, metas map[string]*engine.TorrentMeta) {
		for ih, m := range metas {
			items = append(items, store.SyncItem{
				InfoHash:        ih,
				Session:         sess,
				Paused:          m.UserPaused,
				Tags:            m.Tags,
				SavePath:        m.SavePath,
				Category:        m.Category,
				TorrentFilePath: m.TorrentFilePath,
				CompletedTime:   float64(timeToUnix(m.CompletedTime)),
			})
		}
	}
	add(store.Race, raceMetas)
	add(store.Hoard, hoardMetas)
	if r, err := st.SyncAll(items); err != nil {
		slog.Warn("store: sync failed", "error", err)
	} else if r.Inserted+r.Deleted+r.Missing+r.Conflicts > 0 {
		slog.Info("store: synced",
			"ins", r.Inserted, "upd", r.Updated, "del", r.Deleted, "miss", r.Missing, "conflicts", r.Conflicts)
	}
}

func timeToUnix(t time.Time) int64 {
	if t.IsZero() {
		return 0
	}
	return t.Unix()
}

// ---------------------------------------------------------------------------
// Benchmark snapshot collection
// ---------------------------------------------------------------------------

func collectBenchSnapshot(race *engine.RaceEngine, hoard *engine.HoardEngine) map[string]interface{} {
	snap := map[string]interface{}{
		"ts": float64(time.Now().Unix()),
	}

	raceStatus := race.GetAllStatus()
	if v, ok := raceStatus["torrents"]; ok {
		snap["race_torrents"] = v
	}
	snap["race_upload_rate"] = toFloat(raceStatus["upload_rate"])
	snap["race_download_rate"] = toFloat(raceStatus["download_rate"])
	snap["race_peers"] = toFloat(raceStatus["peers"])

	raceList := race.GetTorrentList()
	var raceUploading int
	var shareSum float64
	var shareCount int
	for _, t := range raceList {
		if t.UploadRate > 0 {
			raceUploading++
		}
		if t.Progress >= 1.0 && t.SwarmSeeds > 1 && t.Ratio > 0 {
			shareSum += t.Ratio / float64(t.SwarmSeeds-1)
			shareCount++
		}
	}
	snap["race_uploading"] = float64(raceUploading)
	if shareCount > 0 {
		snap["race_avg_share"] = shareSum / float64(shareCount)
	}

	hoardStatus := hoard.GetAllStatus()
	if v, ok := hoardStatus["total_torrents"]; ok {
		snap["hoard_active"] = v
	}
	snap["hoard_upload_rate"] = toFloat(hoardStatus["active_upload_rate"])
	snap["hoard_peers"] = toFloat(hoardStatus["active_peers"])
	snap["hoard_with_peers"] = toFloat(hoardStatus["torrents_with_peers"])
	snap["hoard_uploading"] = toFloat(hoardStatus["torrents_uploading"])

	// Ground-truth cumulative counters (int64 bytes). MAX-MIN delta on
	// a time range yields exact uploaded bytes, free of rate-integration error.
	hoardUL, _ := api.GetHoardSessionDelta()
	snap["hoard_session_uploaded"] = hoardUL
	raceUL, _ := api.GetRaceSessionDelta()
	snap["race_session_uploaded"] = raceUL
	// Monotone lifetime cumulative (baseline+session) — MAX-MIN over a range
	// = exact UL/DL bytes, immune to restarts and torrent removals.
	globalUL, globalDL := api.GetGlobalTotals()
	snap["global_uploaded"] = globalUL
	snap["global_downloaded"] = globalDL

	sysStats := system.Collect()
	for _, key := range []string{"iowait_pct", "arc_size_bytes", "arc_hit_rate_pct", "arc_demand_hit_rate_pct", "arc_miss_per_sec", "arc_demand_data_miss_per_sec", "arc_ghost_hits_per_sec", "open_fds"} {
		if v, ok := sysStats[key]; ok {
			snap[key] = v
		}
	}

	return snap
}

// collectTrackerSamples rolls up both engines by tracker host for the bench
// tick — one row per (engine, tracker). Cheap: each engine folds its cached
// stats under a read lock, no full-list copy.
func collectTrackerSamples(race *engine.RaceEngine, hoard *engine.HoardEngine) []bench.TrackerSample {
	ts := float64(time.Now().Unix())
	baseline := api.GetTrackerBaseline() // engine\x00tracker -> {UL,DL}: monotone carry-over of removed torrents
	var out []bench.TrackerSample
	seen := make(map[string]bool)
	add := func(eng string, aggs map[string]*engine.TrackerAgg) {
		for _, a := range aggs {
			key := eng + "\x00" + a.Tracker
			seen[key] = true
			b := baseline[key]
			out = append(out, bench.TrackerSample{
				Ts: ts, Engine: eng, Tracker: a.Tracker,
				UploadRate: a.UploadRate, DownloadRate: a.DownloadRate,
				Peers: a.Peers, Active: a.Active, Torrents: a.Torrents,
				CumUploaded: a.CumUploaded + b[0], CumDownloaded: a.CumDownloaded + b[1],
			})
		}
	}
	add("race", race.AggregateByTracker())
	add("hoard", hoard.AggregateByTracker())
	// Trackers whose torrents are all gone still surface their carried-over total.
	for key, b := range baseline {
		if seen[key] {
			continue
		}
		eng, trk := key, ""
		if i := strings.IndexByte(key, '\x00'); i >= 0 {
			eng, trk = key[:i], key[i+1:]
		}
		out = append(out, bench.TrackerSample{
			Ts: ts, Engine: eng, Tracker: trk,
			CumUploaded: b[0], CumDownloaded: b[1],
		})
	}
	return out
}

// peerRatePrev holds the previous snapshot's cumulative totals for a single
// peer addr, used to compute dl/ul rate via delta.
type peerRatePrev struct {
	totalDL, totalUL int64
	ts               float64
}

// peerRateCache stores per-(infoHash, addr) cumulative totals across ticks,
// enabling rate reconstruction when the engine exposes totals but not rates
// (current state of Typhon per-peer stats).
type peerRateCache struct {
	mu   sync.Mutex
	data map[string]map[string]peerRatePrev
}

var peerRates = &peerRateCache{data: make(map[string]map[string]peerRatePrev)}

// enrichRates mutates peers[i].DownSpeed/UpSpeed in-place with (total - prev) / Δt
// and updates the cache. Called at each 5s snapshot tick.
func (c *peerRateCache) enrichRates(infoHash string, peers []engine.PeerInfo, now float64) {
	c.mu.Lock()
	defer c.mu.Unlock()
	tmap, ok := c.data[infoHash]
	if !ok {
		tmap = make(map[string]peerRatePrev)
		c.data[infoHash] = tmap
	}
	seen := make(map[string]struct{}, len(peers))
	for i := range peers {
		p := &peers[i]
		key := p.IP + ":" + p.Port
		seen[key] = struct{}{}
		if prev, hasPrev := tmap[key]; hasPrev && now > prev.ts {
			dt := now - prev.ts
			if dt > 0 {
				if d := p.TotalDownload - prev.totalDL; d > 0 {
					p.DownSpeed = int64(float64(d) / dt)
				}
				if d := p.TotalUpload - prev.totalUL; d > 0 {
					p.UpSpeed = int64(float64(d) / dt)
				}
			}
		}
		tmap[key] = peerRatePrev{p.TotalDownload, p.TotalUpload, now}
	}
	// Drop addrs no longer connected on this torrent.
	for k := range tmap {
		if _, ok := seen[k]; !ok {
			delete(tmap, k)
		}
	}
}

// purgeInactive drops cache entries for torrents not in the active set.
func (c *peerRateCache) purgeInactive(active map[string]struct{}) {
	c.mu.Lock()
	defer c.mu.Unlock()
	for h := range c.data {
		if _, ok := active[h]; !ok {
			delete(c.data, h)
		}
	}
}

func collectPeersJSON(peers []engine.PeerInfo) string {
	type peerSnap struct {
		IP       string  `json:"ip"`
		Port     string  `json:"port"`
		Client   string  `json:"client"`
		DLSpeed  int64   `json:"dl_speed"`
		ULSpeed  int64   `json:"ul_speed"`
		Progress float64 `json:"progress"`
		Flags    string  `json:"flags"`
	}

	sort.Slice(peers, func(i, j int) bool {
		return peers[i].DownSpeed > peers[j].DownSpeed
	})

	seen := make(map[string]bool)
	var result []peerSnap
	for i, p := range peers {
		if i >= 10 {
			break
		}
		key := p.IP + ":" + p.Port
		seen[key] = true
		flags := ""
		for j, f := range p.Flags {
			if j > 0 {
				flags += " "
			}
			flags += f
		}
		result = append(result, peerSnap{
			IP: p.IP, Port: p.Port, Client: p.Client,
			DLSpeed: p.DownSpeed, ULSpeed: p.UpSpeed,
			Progress: p.Progress, Flags: flags,
		})
	}
	for _, p := range peers {
		key := p.IP + ":" + p.Port
		if seen[key] || p.Progress < 0.8 {
			continue
		}
		flags := ""
		for j, f := range p.Flags {
			if j > 0 {
				flags += " "
			}
			flags += f
		}
		result = append(result, peerSnap{
			IP: p.IP, Port: p.Port, Client: p.Client,
			DLSpeed: p.DownSpeed, ULSpeed: p.UpSpeed,
			Progress: p.Progress, Flags: flags,
		})
	}

	data, err := json.Marshal(result)
	if err != nil {
		return "[]"
	}
	return string(data)
}

func toFloat(v interface{}) float64 {
	switch n := v.(type) {
	case int:
		return float64(n)
	case int64:
		return float64(n)
	case float64:
		return n
	default:
		return 0
	}
}

func formatBytesHuman(b int64) string {
	const unit = 1024
	if b < unit {
		return fmt.Sprintf("%d B", b)
	}
	div, exp := int64(unit), 0
	for n := b / unit; n >= unit; n /= unit {
		div *= unit
		exp++
	}
	return fmt.Sprintf("%.1f %ciB", float64(b)/float64(div), "KMGTPE"[exp])
}

// ===========================================================================
// Adapter types (same as before — bridge engine types to API interfaces)
// ===========================================================================

type raceAPIAdapter struct{ engine *engine.RaceEngine }

func (a *raceAPIAdapter) GetAllStatus() []map[string]interface{} {
	list := a.engine.GetTorrentList()
	result := make([]map[string]interface{}, 0, len(list))
	for _, s := range list {
		result = append(result, torrentStatsToMap(&s))
	}
	return result
}
func (a *raceAPIAdapter) GetAllStatusJSON() []json.RawMessage {
	list := a.engine.GetTorrentList()
	sort.Slice(list, func(i, j int) bool { return list[i].AddedTime > list[j].AddedTime })
	out := make([]json.RawMessage, 0, len(list))
	for i := range list {
		if b, err := json.Marshal(&list[i]); err == nil {
			out = append(out, b)
		}
	}
	return out
}
func (a *raceAPIAdapter) GetSessionTotals() (int64, int64) { return a.engine.GetSessionTotals() }

func (a *raceAPIAdapter) ClearCategoryLabel(category string) int {
	return a.engine.ClearCategoryLabel(category)
}
func (a *raceAPIAdapter) SetUserPaused(ih string, paused bool) error {
	return a.engine.SetUserPaused(ih, paused)
}
func (a *raceAPIAdapter) GetTorrentDetail(infoHash string) map[string]interface{} {
	return a.engine.GetTorrentDetail(infoHash)
}
func (a *raceAPIAdapter) GetTorrentFileList(infoHash string) []map[string]interface{} {
	return a.engine.GetTorrentFileList(infoHash)
}
func (a *raceAPIAdapter) GetTorrentStatus(infoHash string) map[string]interface{} {
	return a.GetTorrentDetail(infoHash)
}
func (a *raceAPIAdapter) AddTorrent(torrentPath, magnetURI, savePath string, trackers []string, category string) (string, error) {
	return a.engine.AddTorrent(torrentPath, magnetURI, savePath, trackers, category)
}
func (a *raceAPIAdapter) AddTorrentSeedMode(torrentPath, savePath, category string) (string, error) {
	return a.engine.AddTorrentSeedMode(torrentPath, savePath, category)
}
func (a *raceAPIAdapter) RemoveTorrent(infoHash string, deleteFiles bool) error {
	return a.engine.RemoveTorrent(infoHash, deleteFiles)
}
func (a *raceAPIAdapter) ReannnounceTorrent(infoHash string) bool { return true }
func (a *raceAPIAdapter) AddTrackerToTorrent(infoHash, url string) error {
	return a.engine.AddTrackerToTorrent(infoHash, url)
}
func (a *raceAPIAdapter) GetChokingStats() map[string]interface{} {
	return a.engine.GetChokingStats()
}
func (a *raceAPIAdapter) GetSessionSettings() map[string]interface{} {
	return a.engine.GetSessionSettings()
}
func (a *raceAPIAdapter) ApplySettings(settings map[string]interface{}) map[string]interface{} {
	a.engine.ApplySettings(settings)
	return a.engine.GetSessionSettings()
}
func (a *raceAPIAdapter) SetListenPort(port int) {
	a.engine.SetListenPort(port)
}
func (a *raceAPIAdapter) HasTorrent(infoHash string) bool {
	return a.engine.GetTorrentDetail(infoHash) != nil
}
func (a *raceAPIAdapter) SessionGrabbed() int64 { return a.engine.SessionGrabbed() }
func (a *raceAPIAdapter) AggregateStats() map[string]interface{} {
	return a.engine.AggregateStats()
}

type hoardAPIAdapter struct{ engine *engine.HoardEngine }

func (a *hoardAPIAdapter) GetAllStatus() map[string]interface{} { return a.engine.GetAllStatus() }
func (a *hoardAPIAdapter) EventHub() *engine.EventHub           { return a.engine.EventHub() }
func (a *hoardAPIAdapter) GetTorrentList() []map[string]interface{} {
	list := a.engine.GetTorrentList()
	result := make([]map[string]interface{}, 0, len(list))
	for _, s := range list {
		result = append(result, torrentStatsToMap(&s))
	}
	return result
}
func (a *hoardAPIAdapter) GetTorrentListJSON() []json.RawMessage {
	list := a.engine.GetTorrentList()
	sort.Slice(list, func(i, j int) bool { return list[i].AddedTime > list[j].AddedTime })
	out := make([]json.RawMessage, 0, len(list))
	for i := range list {
		if b, err := json.Marshal(&list[i]); err == nil {
			out = append(out, b)
		}
	}
	return out
}
func (a *hoardAPIAdapter) GetSessionTotals() (int64, int64) { return a.engine.GetSessionTotals() }
func (a *hoardAPIAdapter) GetTorrentDetail(infoHash string) map[string]interface{} {
	return a.engine.GetTorrentDetail(infoHash)
}
func (a *hoardAPIAdapter) GetTorrentFileList(infoHash string) []map[string]interface{} {
	return a.engine.GetTorrentFileList(infoHash)
}
func (a *hoardAPIAdapter) AddTorrentSeedMode(torrentPath, savePath, category string) (string, error) {
	return a.engine.AddTorrentSeedMode(torrentPath, savePath, category)
}
func (a *hoardAPIAdapter) AddTorrent(torrentPath, savePath, category string) (string, error) {
	return a.engine.AddTorrent(torrentPath, savePath, category)
}
func (a *hoardAPIAdapter) RemoveTorrent(infoHash string, deleteFiles bool) error {
	a.engine.RemoveTorrent(infoHash, deleteFiles)
	return nil
}
func (a *hoardAPIAdapter) ReannnounceTorrent(infoHash string) bool { return true }
func (a *hoardAPIAdapter) AddTrackerToTorrent(infoHash, url string) error {
	return a.engine.AddTrackerToTorrent(infoHash, url)
}
func (a *hoardAPIAdapter) SetListenPort(port int) { a.engine.SetListenPort(port) }
func (a *hoardAPIAdapter) SetAddedTime(infoHash string, t time.Time) {
	a.engine.SetAddedTime(infoHash, t)
}
func (a *hoardAPIAdapter) HasTorrent(infoHash string) bool {
	return a.engine.GetTorrentDetail(infoHash) != nil
}
func (a *hoardAPIAdapter) PauseAll() int { return a.engine.PauseAll() }
func (a *hoardAPIAdapter) SetUserPaused(ih string, paused bool) error {
	return a.engine.SetUserPaused(ih, paused)
}
func (a *hoardAPIAdapter) MarkAllUserPaused(paused bool) int {
	return a.engine.MarkAllUserPaused(paused)
}
func (a *hoardAPIAdapter) ResumeAll() int             { return a.engine.ResumeAll() }
func (a *hoardAPIAdapter) RestartStuckVerifying() int { return a.engine.RestartStuckVerifying() }
func (a *hoardAPIAdapter) VerifyDownloading() int     { return a.engine.VerifyDownloading() }
func (a *hoardAPIAdapter) VerifyTorrent(infoHash string) error {
	return a.engine.VerifyTorrent(infoHash)
}
func (a *hoardAPIAdapter) SetTorrentCategory(infoHash, newCategory, newSavePath string) error {
	return a.engine.SetTorrentCategory(infoHash, newCategory, newSavePath)
}
func (a *hoardAPIAdapter) SetContentFolder(infoHash string, cf *bool) {
	a.engine.SetContentFolder(infoHash, cf)
}
func (a *hoardAPIAdapter) SetCategoryLabel(infoHash, category string) error {
	return a.engine.SetCategoryLabel(infoHash, category)
}
func (a *hoardAPIAdapter) ClearCategoryLabel(category string) int {
	return a.engine.ClearCategoryLabel(category)
}
func (a *hoardAPIAdapter) GetTags(infoHash string) []string { return a.engine.GetTags(infoHash) }
func (a *hoardAPIAdapter) GetAllTags() map[string][]string  { return a.engine.GetAllTags() }
func (a *hoardAPIAdapter) SetTags(infoHash string, tags []string) error {
	return a.engine.SetTags(infoHash, tags)
}
func (a *hoardAPIAdapter) AddTags(infoHash string, tags []string) error {
	return a.engine.AddTags(infoHash, tags)
}
func (a *hoardAPIAdapter) RemoveTags(infoHash string, tags []string) error {
	return a.engine.RemoveTags(infoHash, tags)
}
func (a *hoardAPIAdapter) GetDownloadSlotStatus() engine.DownloadSlotStats {
	return a.engine.GetDownloadSlotStatus()
}
func (a *hoardAPIAdapter) SetDownloadSlotsOverride(max int) { a.engine.SetDownloadSlotsOverride(max) }
func (a *hoardAPIAdapter) ClearDownloadSlotsOverride()      { a.engine.ClearDownloadSlotsOverride() }
func (a *hoardAPIAdapter) PinTorrent(infoHash string)       { a.engine.PinTorrent(infoHash) }
func (a *hoardAPIAdapter) UnpinTorrent(infoHash string)     { a.engine.UnpinTorrent(infoHash) }
func (a *hoardAPIAdapter) PinnedList() []string             { return a.engine.PinnedList() }

type raceDrainAdapter struct{ engine *engine.RaceEngine }

func (a *raceDrainAdapter) GetAllStatus() []map[string]interface{} {
	list := a.engine.GetTorrentList()
	result := make([]map[string]interface{}, 0, len(list))
	for _, s := range list {
		result = append(result, torrentStatsToMap(&s))
	}
	return result
}
func (a *raceDrainAdapter) RemoveTorrent(infoHash string, deleteFiles bool) error {
	return a.engine.RemoveTorrent(infoHash, deleteFiles)
}

type hoardCleanupAdapter struct{ engine *engine.HoardEngine }

func (a *hoardCleanupAdapter) GetTorrentList() []map[string]interface{} {
	list := a.engine.GetTorrentList()
	result := make([]map[string]interface{}, 0, len(list))
	for _, s := range list {
		result = append(result, torrentStatsToMap(&s))
	}
	return result
}
func (a *hoardCleanupAdapter) RemoveTorrent(infoHash string, deleteFiles bool) error {
	a.engine.RemoveTorrent(infoHash, deleteFiles)
	return nil
}

type hoardAdapter struct{ engine *engine.HoardEngine }

func (a *hoardAdapter) GetTorrentList() []map[string]interface{} {
	list := a.engine.GetTorrentList()
	result := make([]map[string]interface{}, 0, len(list))
	for _, s := range list {
		result = append(result, torrentStatsToMap(&s))
	}
	return result
}
func (a *hoardAdapter) GetTorrentFiles(infoHash string) []string {
	return a.engine.GetTorrentFiles(infoHash)
}
func (a *hoardAdapter) GetTorrentSavePath(infoHash string) string {
	d := a.engine.GetTorrentDetail(infoHash)
	if d == nil {
		return ""
	}
	if sp, ok := d["save_path"].(string); ok {
		return sp
	}
	return ""
}
func (a *hoardAdapter) GetCategories() map[string]string { return a.engine.GetCategories() }
func (a *hoardAdapter) GetTorrentName(infoHash string) string {
	d := a.engine.GetTorrentDetail(infoHash)
	if d == nil {
		return ""
	}
	if name, ok := d["name"].(string); ok {
		return name
	}
	return ""
}
func (a *hoardAdapter) RemoveTorrent(infoHash string, deleteFiles bool) error {
	a.engine.RemoveTorrent(infoHash, deleteFiles)
	return nil
}

type raceMetricsAdapter struct{ engine *engine.RaceEngine }

func (a *raceMetricsAdapter) GetAllStatus() []map[string]interface{} {
	list := a.engine.GetTorrentList()
	result := make([]map[string]interface{}, 0, len(list))
	for _, s := range list {
		result = append(result, torrentStatsToMap(&s))
	}
	return result
}
func (a *raceMetricsAdapter) GetChokingStats() map[string]interface{} {
	return a.engine.GetChokingStats()
}

type hoardMetricsAdapter struct{ engine *engine.HoardEngine }

func (a *hoardMetricsAdapter) GetAllStatus() map[string]interface{} { return a.engine.GetAllStatus() }

type benchAPIAdapter struct{ db *bench.BenchDB }

func (a *benchAPIAdapter) GetCurrent() map[string]interface{} { return a.db.GetCurrent() }
func (a *benchAPIAdapter) GetRecords() map[string]interface{} { return a.db.GetRecords() }
func (a *benchAPIAdapter) GetRange(start, end, step int) []map[string]interface{} {
	return a.db.GetRange(start, end, step)
}
func (a *benchAPIAdapter) GetComparison(start, mid, end int) map[string]interface{} {
	return a.db.GetComparison(start, mid, end)
}
func (a *benchAPIAdapter) InsertVpn(ts, ulMbps, dlMbps, ulTorrentMbps, dlTorrentMbps float64) {
	a.db.InsertVpn(ts, ulMbps, dlMbps, ulTorrentMbps, dlTorrentMbps)
}
func (a *benchAPIAdapter) GetVpnLatest() map[string]interface{} { return a.db.GetVpnLatest() }
func (a *benchAPIAdapter) GetVpnRange(start, end float64) []map[string]interface{} {
	return a.db.GetVpnRange(start, end)
}
func (a *benchAPIAdapter) GetRaceEvents(start, end float64) []bench.RaceEvent {
	return a.db.GetRaceEvents(start, end)
}
func (a *benchAPIAdapter) GetRaceEventsForTorrent(infoHash string) []bench.RaceEvent {
	return a.db.GetRaceEventsForTorrent(infoHash)
}
func (a *benchAPIAdapter) GetRaceSnapshots(infoHash string) []bench.RaceSnapshot {
	return a.db.GetRaceSnapshots(infoHash)
}
func (a *benchAPIAdapter) GetTrackerCurrent() []map[string]interface{} {
	return a.db.GetTrackerCurrent()
}
func (a *benchAPIAdapter) GetTrackerRange(start, end, step int, tracker string) []map[string]interface{} {
	return a.db.GetTrackerRange(start, end, step, tracker)
}

func torrentStatsToMap(s *engine.TorrentStats) map[string]interface{} {
	return map[string]interface{}{
		"info_hash": s.InfoHash, "name": s.Name, "state": s.State,
		"progress": s.Progress, "upload_rate": s.UploadRate,
		"download_rate": s.DownloadRate, "total_upload": s.TotalUpload,
		"total_download": s.TotalDownload, "num_peers": s.NumPeers,
		"num_seeds": s.NumSeeds, "total_size": s.TotalSize,
		"ratio": s.Ratio, "save_path": s.SavePath,
		"engine_save_path": s.EngineSavePath, "multi_file": s.MultiFile,
		"category": s.Category, "added_time": s.AddedTime,
		"completed_time": s.CompletedTime, "swarm_seeds": s.SwarmSeeds,
		"swarm_leechers": s.SwarmLeechers, "tracker_error": s.TrackerError,
		"tracker_error_msg": s.TrackerErrorMsg, "torrent_error": s.TorrentError,
		"torrent_error_msg": s.TorrentErrorMsg, "uploader": s.Uploader,
		"injected_peers": s.InjectedPeers, "injection_hit": s.InjectionHit,
		"content_folder": s.ContentFolder, "tags": s.Tags, "user_paused": s.UserPaused,
		"tracker_host": s.TrackerHost,
	}
}

// runFrontOnly serves a controller node: no local Typhon engine, only the API
// + dialed remote agents. Category placement routes adds to those agents. Read
// endpoints use empty engine stubs (list aggregation across agents is a later
// step). api_key/[auth] are already resolved on cfg before this point.
func runFrontOnly(ctx context.Context, cfg *config.HydraConfig) {
	slog.Info("starting in FRONT-ONLY mode (no local engine)")
	apiServer := api.NewServer(cfg)
	apiServer.SetFrontOnly(true)
	apiServer.SetEngines(api.NewEmptyRaceEngine(), api.NewEmptyHoardEngine())
	for _, ag := range cfg.Agents {
		if ag.Name == "" || ag.Addr == "" {
			continue
		}
		if err := apiServer.AddRemoteAgent(ag.Name, ag.Addr, ag.Token, ag.TLSCa); err != nil {
			slog.Warn("remote agent dial failed", "name", ag.Name, "addr", ag.Addr, "err", err)
			continue
		}
		slog.Info("remote agent registered", "name", ag.Name, "addr", ag.Addr)
	}
	apiServer.LoadPersistedAgents()
	go func() {
		if err := apiServer.Run(); err != nil {
			slog.Error("API server error", "error", err)
		}
	}()
	slog.Info("front-only API started", "addr", fmt.Sprintf("http://%s:%d", cfg.Daemon.APIHost, cfg.Daemon.APIPort))
	<-ctx.Done()
}

// ---------------------------------------------------------------------------
// Graduation: move a race torrent to the hoard engine via the category link.
// ---------------------------------------------------------------------------

type gradProgress struct {
	Name    string
	Copied  int64 // atomic
	Total   int64
	Started int64
}

type graduator struct {
	race    *engine.RaceEngine
	hoard   *engine.HoardEngine
	dataDir string
	mu      sync.Mutex
	active  map[string]*gradProgress
}

// GraduationsSnapshot lists the graduations currently copying (for the UI).
func (g *graduator) GraduationsSnapshot() []map[string]interface{} {
	g.mu.Lock()
	defer g.mu.Unlock()
	out := []map[string]interface{}{}
	for ih, p := range g.active {
		copied := atomic.LoadInt64(&p.Copied)
		pct := 0.0
		if p.Total > 0 {
			pct = float64(copied) / float64(p.Total) * 100
		}
		out = append(out, map[string]interface{}{
			"info_hash": ih, "name": p.Name, "copied": copied,
			"total": p.Total, "pct": pct, "started": p.Started,
		})
	}
	return out
}

type countingWriter struct {
	w io.Writer
	n *int64
}

func (c *countingWriter) Write(p []byte) (int, error) {
	m, err := c.w.Write(p)
	atomic.AddInt64(c.n, int64(m))
	return m, err
}

type gradCat struct {
	SavePath   string `json:"save_path"`
	Mode       string `json:"mode"`
	GraduateTo string `json:"graduate_to"`
}

func readGradCategories(dataDir string) map[string]gradCat {
	out := map[string]gradCat{}
	data, err := os.ReadFile(filepath.Join(dataDir, "categories.json"))
	if err != nil {
		return out
	}
	var m map[string]gradCat
	if json.Unmarshal(data, &m) != nil {
		return out
	}
	return m
}

func treeSize(p string) (int64, error) {
	var total int64
	err := filepath.Walk(p, func(_ string, info os.FileInfo, err error) error {
		if err != nil {
			return err
		}
		if !info.IsDir() {
			total += info.Size()
		}
		return nil
	})
	return total, err
}

func copyFile(src, dst string, counter *int64) error {
	if err := os.MkdirAll(filepath.Dir(dst), 0o755); err != nil {
		return err
	}
	in, err := os.Open(src)
	if err != nil {
		return err
	}
	defer in.Close()
	out, err := os.Create(dst)
	if err != nil {
		return err
	}
	var w io.Writer = out
	if counter != nil {
		w = &countingWriter{w: out, n: counter}
	}
	if _, err := io.Copy(w, in); err != nil {
		out.Close()
		return err
	}
	if err := out.Sync(); err != nil {
		out.Close()
		return err
	}
	return out.Close()
}

func copyPath(src, dst string, counter *int64) error {
	info, err := os.Stat(src)
	if err != nil {
		return err
	}
	if !info.IsDir() {
		return copyFile(src, dst, counter)
	}
	if err := os.MkdirAll(dst, 0o755); err != nil {
		return err
	}
	entries, err := os.ReadDir(src)
	if err != nil {
		return err
	}
	for _, e := range entries {
		if err := copyPath(filepath.Join(src, e.Name()), filepath.Join(dst, e.Name()), counter); err != nil {
			return err
		}
	}
	return nil
}

// Graduate moves one race torrent to its linked hoard category. Safe order:
// copy NVMe->ZFS, verify size, register in hoard (hash-verifies), deregister
// from race WITHOUT deleting (files already copied; AbsorbStats keeps the
// global totals), then remove the NVMe source copy. Any failure aborts before
// the source is touched. The hoard announces fresh from zero, so no tracker
// over-credit (per-torrent display counter carry is a separate follow-up).
func (g *graduator) Graduate(infoHash string) (bool, error) {
	savePath, torrentFile, name, cat, ok := g.race.GraduationInfo(infoHash)
	if !ok {
		return false, fmt.Errorf("race torrent %s not found", infoHash)
	}
	cats := readGradCategories(g.dataDir)
	rc, ok := cats[cat]
	if !ok || rc.GraduateTo == "" {
		return false, nil // no link -> skip, never delete
	}
	hc, ok := cats[rc.GraduateTo]
	if !ok || hc.Mode != "hoard" || hc.SavePath == "" {
		return false, fmt.Errorf("invalid graduate_to %q (must be a hoard category with a save_path)", rc.GraduateTo)
	}
	if name == "" || torrentFile == "" {
		return false, fmt.Errorf("missing name or torrent file for %s", infoHash)
	}
	src := filepath.Join(savePath, name)
	if _, err := os.Stat(src); err != nil {
		return false, fmt.Errorf("source content not found at %s: %w", src, err)
	}
	dst := filepath.Join(hc.SavePath, name)
	if _, err := os.Stat(dst); err == nil {
		return false, fmt.Errorf("destination already exists: %s", dst)
	}
	g.mu.Lock()
	if _, busy := g.active[infoHash]; busy {
		g.mu.Unlock()
		return false, nil // already graduating this one
	}
	total, _ := treeSize(src)
	prog := &gradProgress{Name: name, Total: total, Started: time.Now().Unix()}
	g.active[infoHash] = prog
	g.mu.Unlock()
	defer func() {
		g.mu.Lock()
		delete(g.active, infoHash)
		g.mu.Unlock()
	}()
	if err := copyPath(src, dst, &prog.Copied); err != nil {
		os.RemoveAll(dst)
		return false, fmt.Errorf("copy %s -> %s: %w", src, dst, err)
	}
	ss, _ := treeSize(src)
	ds, _ := treeSize(dst)
	if ss != ds {
		os.RemoveAll(dst)
		return false, fmt.Errorf("size mismatch after copy (src=%d dst=%d)", ss, ds)
	}
	if _, err := g.hoard.AddTorrentSeedMode(torrentFile, hc.SavePath, rc.GraduateTo); err != nil {
		os.RemoveAll(dst)
		return false, fmt.Errorf("hoard add-seed failed: %w", err)
	}
	if err := g.race.RemoveTorrent(infoHash, false); err != nil {
		slog.Warn("graduate: race remove failed after hoard add", "info_hash", infoHash, "error", err)
	}
	os.RemoveAll(src)
	slog.Info("graduate: race->hoard", "name", name, "category", rc.GraduateTo, "dest", hc.SavePath)
	return true, nil
}
